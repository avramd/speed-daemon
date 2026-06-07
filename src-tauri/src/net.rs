// Client/server distributed monitoring.
//
// Discovery: one shared UDP socket on DISCO_PORT. A server broadcasts {"t":"q"}; clients
// reply {"t":"r", port} with their ephemeral TCP control port. The server connects over
// TCP and runs the pairing handshake (invite -> accept, exchanging a shared secret that
// both sides store and check on every later message). It then sends an `assign` (the hosts
// to check, each with a short id = hash(config+negotiation ts)); the client de-dups against
// its own probes, runs any new ones, and reports id-keyed results every REPORT_SECS.

use crate::commands::ConfigState;
use crate::config;
use crate::db::{now_ms, Db};
use crate::model::{Peer, Target};
use crate::probes::Probes;
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};

const DISCO_PORT: u16 = 51900;
const REPORT_SECS: u64 = 10;
const INVITE_TIMEOUT_SECS: u64 = 300;

struct DiscoPeer {
    name: String,
    addr: SocketAddr, // ip + advertised control port
    last_seen: u64,
}

struct ServerConn {
    tx: mpsc::UnboundedSender<String>,
    secret: String,
    /// assigned id -> (host, tag, label) so reports (id-only) can be enriched for the UI.
    assign: HashMap<u32, (String, String, String)>,
}

pub struct Net {
    app: AppHandle,
    db: Arc<Db>,
    probes: Arc<Probes>,
    cfg: Arc<ConfigState>,
    ctrl_port: Mutex<u16>,
    /// server side: discovered clients (id -> peer)
    discovered: Mutex<HashMap<String, DiscoPeer>>,
    /// client side: pending invites awaiting a user decision (serverId -> decision sender)
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    /// server side: live connections to clients (clientId -> conn)
    conns: Mutex<HashMap<String, ServerConn>>,
}

fn node(cfg: &ConfigState) -> (String, String, String) {
    let g = cfg.config.lock().unwrap();
    (g.node.id.clone(), g.node.name.clone(), g.node.mode.clone())
}

/// Short id for an assigned target: hash of its config plus the negotiation timestamp.
fn assign_id(t: &Target, ts: u64) -> u32 {
    let mut h = DefaultHasher::new();
    t.host.hash(&mut h);
    t.tag.hash(&mut h);
    t.interval_ms.hash(&mut h);
    t.label.hash(&mut h);
    ts.hash(&mut h);
    h.finish() as u32
}

fn send_line(tx: &mpsc::UnboundedSender<String>, v: Value) {
    let _ = tx.send(v.to_string());
}

impl Net {
    pub fn new(app: AppHandle, db: Arc<Db>, probes: Arc<Probes>, cfg: Arc<ConfigState>) -> Arc<Net> {
        Arc::new(Net {
            app,
            db,
            probes,
            cfg,
            ctrl_port: Mutex::new(0),
            discovered: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            conns: Mutex::new(HashMap::new()),
        })
    }

    pub fn start(self: &Arc<Net>) {
        let net = self.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = net.run().await {
                eprintln!("net: fatal {e}");
            }
        });
    }

    async fn run(self: Arc<Net>) -> std::io::Result<()> {
        // TCP control listener on an ephemeral port (advertised via discovery).
        let tcp = TcpListener::bind(("0.0.0.0", 0)).await?;
        let port = tcp.local_addr()?.port();
        *self.ctrl_port.lock().unwrap() = port;

        // Shared UDP discovery socket.
        let udp = Arc::new(bind_udp()?);

        // Accept inbound control connections (client role).
        {
            let net = self.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    if let Ok((stream, addr)) = tcp.accept().await {
                        let net = net.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = net.handle_inbound(stream, addr).await;
                        });
                    }
                }
            });
        }

        // UDP listener: reply to queries (client) / record replies (server).
        {
            let net = self.clone();
            let udp = udp.clone();
            tauri::async_runtime::spawn(async move {
                let mut buf = [0u8; 2048];
                loop {
                    let Ok((n, from)) = udp.recv_from(&mut buf).await else {
                        continue;
                    };
                    let Ok(v) = serde_json::from_slice::<Value>(&buf[..n]) else {
                        continue;
                    };
                    net.on_udp(&udp, v, from).await;
                }
            });
        }

        // Server maintenance: broadcast discovery + connect to paired clients.
        {
            let net = self.clone();
            let udp = udp.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    let (id, name, mode) = node(&net.cfg);
                    if mode == "server" {
                        let port = *net.ctrl_port.lock().unwrap();
                        let q = json!({"t":"q","id":id,"name":name,"port":port});
                        broadcast_query(&udp, &q.to_string()).await;
                        net.connect_paired_clients().await;
                    }
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            });
        }

        Ok(())
    }

    async fn on_udp(self: &Arc<Net>, udp: &UdpSocket, v: Value, from: SocketAddr) {
        let (id, name, mode) = node(&self.cfg);
        match v.get("t").and_then(|t| t.as_str()) {
            Some("q") if mode == "client" => {
                // A server is looking for clients; advertise ourselves.
                let port = *self.ctrl_port.lock().unwrap();
                let r = json!({"t":"r","id":id,"name":name,"port":port});
                let _ = udp.send_to(r.to_string().as_bytes(), from).await;
            }
            Some("r") if mode == "server" => {
                let (Some(pid), Some(pname), Some(pport)) = (
                    v.get("id").and_then(|x| x.as_str()),
                    v.get("name").and_then(|x| x.as_str()),
                    v.get("port").and_then(|x| x.as_u64()),
                ) else {
                    return;
                };
                if pid == id {
                    return; // ignore ourselves
                }
                let addr = SocketAddr::new(from.ip(), pport as u16);
                self.discovered.lock().unwrap().insert(
                    pid.to_string(),
                    DiscoPeer {
                        name: pname.to_string(),
                        addr,
                        last_seen: now_ms(),
                    },
                );
                self.emit_discovered();
            }
            _ => {}
        }
    }

    fn emit_discovered(&self) {
        let list: Vec<Value> = self
            .discovered
            .lock()
            .unwrap()
            .iter()
            .map(|(id, p)| json!({"id":id,"name":p.name,"addr":p.addr.to_string(),"lastSeen":p.last_seen}))
            .collect();
        let _ = self.app.emit("net-discovered", json!(list));
    }

    fn peer_by_id(&self, id: &str) -> Option<Peer> {
        self.cfg
            .config
            .lock()
            .unwrap()
            .peers
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    fn upsert_peer(&self, peer: Peer) {
        let mut g = self.cfg.config.lock().unwrap();
        if let Some(slot) = g.peers.iter_mut().find(|p| p.id == peer.id) {
            *slot = peer;
        } else {
            g.peers.push(peer);
        }
        let _ = config::save(&self.cfg.path, &g);
        let peers = g.peers.clone();
        drop(g);
        let _ = self.app.emit("net-peers", json!(peers));
    }

    // ---- discovery / invite commands (server side) ----

    pub fn discovered_list(&self) -> Vec<Value> {
        self.discovered
            .lock()
            .unwrap()
            .iter()
            .map(|(id, p)| json!({"id":id,"name":p.name,"addr":p.addr.to_string(),"lastSeen":p.last_seen}))
            .collect()
    }

    pub async fn discover_now(self: &Arc<Net>, udp_send: bool) {
        if !udp_send {
            return;
        }
        // Best-effort one-off broadcast from a transient socket.
        if let Ok(s) = bind_udp() {
            let (id, name, _) = node(&self.cfg);
            let port = *self.ctrl_port.lock().unwrap();
            let q = json!({"t":"q","id":id,"name":name,"port":port});
            broadcast_query(&s, &q.to_string()).await;
        }
    }

    /// Server: connect to a discovered client and send an invite with a custom message.
    pub fn invite(self: &Arc<Net>, peer_id: String, message: String) {
        let addr = match self.discovered.lock().unwrap().get(&peer_id) {
            Some(p) => p.addr,
            None => return,
        };
        let net = self.clone();
        tauri::async_runtime::spawn(async move {
            let _ = net.server_connect(addr, None, message).await;
        });
    }

    async fn connect_paired_clients(self: &Arc<Net>) {
        let peers: Vec<Peer> = self
            .cfg
            .config
            .lock()
            .unwrap()
            .peers
            .iter()
            .filter(|p| p.role == "client")
            .cloned()
            .collect();
        for p in peers {
            if self.conns.lock().unwrap().contains_key(&p.id) {
                continue; // already connected
            }
            let Some(addr) = p.addr.as_ref().and_then(|a| a.parse::<SocketAddr>().ok()) else {
                continue;
            };
            let net = self.clone();
            tauri::async_runtime::spawn(async move {
                let _ = net.server_connect(addr, Some(p), String::new()).await;
            });
        }
    }

    /// Build the assignment list from the server's own targets.
    fn build_assignment(&self) -> (Vec<Value>, HashMap<u32, (String, String, String)>) {
        let ts = now_ms();
        let targets = self.cfg.config.lock().unwrap().targets.clone();
        let mut list = Vec::new();
        let mut map = HashMap::new();
        for t in &targets {
            let id = assign_id(t, ts);
            list.push(json!({
                "id": id, "host": t.host, "tag": t.tag,
                "label": t.label, "intervalMs": t.interval_ms
            }));
            map.insert(id, (t.host.clone(), t.tag.clone(), t.label.clone()));
        }
        (list, map)
    }

    /// Re-send assignment to all connected clients (call when server targets change).
    pub fn reassign_all(self: &Arc<Net>) {
        let (list, map) = self.build_assignment();
        let mut conns = self.conns.lock().unwrap();
        for conn in conns.values_mut() {
            conn.assign = map.clone();
            send_line(&conn.tx, json!({"t":"assign","secret":conn.secret,"targets":list}));
        }
    }

    /// Server side of a control connection to one client.
    async fn server_connect(
        self: &Arc<Net>,
        addr: SocketAddr,
        existing: Option<Peer>,
        message: String,
    ) -> std::io::Result<()> {
        let stream = TcpStream::connect(addr).await?;
        let (rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tauri::async_runtime::spawn(async move {
            while let Some(s) = rx.recv().await {
                if wr.write_all(s.as_bytes()).await.is_err() || wr.write_all(b"\n").await.is_err() {
                    break;
                }
            }
        });
        let mut lines = BufReader::new(rd).lines();
        let (my_id, my_name, _) = node(&self.cfg);

        let (client_id, client_name, secret) = if let Some(peer) = existing {
            // Reconnect: greet with the stored secret.
            send_line(&tx, json!({"t":"hello","id":my_id,"secret":peer.secret}));
            (peer.id, peer.name, peer.secret)
        } else {
            // New pairing: invite and await the client's accept.
            send_line(
                &tx,
                json!({"t":"invite","id":my_id,"name":my_name,"msg":message}),
            );
            let line = match lines.next_line().await {
                Ok(Some(l)) => l,
                _ => return Ok(()),
            };
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                _ => return Ok(()),
            };
            if v.get("t").and_then(|t| t.as_str()) != Some("accept") {
                let _ = self.app.emit("net-invite-result", json!({"accepted":false}));
                return Ok(());
            }
            let cid = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let cname = v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let secret = v.get("secret").and_then(|x| x.as_str()).unwrap_or("").to_string();
            self.upsert_peer(Peer {
                id: cid.clone(),
                name: cname.clone(),
                secret: secret.clone(),
                role: "client".into(),
                addr: Some(addr.to_string()),
            });
            let _ = self.app.emit("net-invite-result", json!({"accepted":true,"name":cname}));
            (cid, cname, secret)
        };

        // Register the connection and push the initial assignment.
        let (list, map) = self.build_assignment();
        self.conns.lock().unwrap().insert(
            client_id.clone(),
            ServerConn {
                tx: tx.clone(),
                secret: secret.clone(),
                assign: map,
            },
        );
        send_line(&tx, json!({"t":"assign","secret":secret,"targets":list}));

        // Read reports until the connection drops.
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("t").and_then(|t| t.as_str()) == Some("report")
                && v.get("secret").and_then(|x| x.as_str()) == Some(secret.as_str())
            {
                self.on_report(&client_id, &client_name, &v);
            }
        }

        self.conns.lock().unwrap().remove(&client_id);
        Ok(())
    }

    fn on_report(&self, client_id: &str, client_name: &str, v: &Value) {
        let conns = self.conns.lock().unwrap();
        let Some(conn) = conns.get(client_id) else {
            return;
        };
        let mut results = Vec::new();
        if let Some(arr) = v.get("results").and_then(|r| r.as_array()) {
            for r in arr {
                let id = r.get("id").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                let (host, tag, label) = conn
                    .assign
                    .get(&id)
                    .cloned()
                    .unwrap_or_default();
                results.push(json!({
                    "id": id,
                    "host": host, "tag": tag, "label": label,
                    "rttMs": r.get("rttMs").cloned().unwrap_or(Value::Null),
                    "lossPct": r.get("lossPct").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    "intervalMs": r.get("intervalMs").and_then(|x| x.as_u64()).unwrap_or(1000),
                }));
            }
        }
        let _ = self.app.emit(
            "net-remote",
            json!({"fromId":client_id,"fromName":client_name,"ts":now_ms(),"results":results}),
        );
    }

    // ---- client side: inbound connection from a server ----

    async fn handle_inbound(self: &Arc<Net>, stream: TcpStream, addr: SocketAddr) -> std::io::Result<()> {
        let (rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tauri::async_runtime::spawn(async move {
            while let Some(s) = rx.recv().await {
                if wr.write_all(s.as_bytes()).await.is_err() || wr.write_all(b"\n").await.is_err() {
                    break;
                }
            }
        });
        let mut lines = BufReader::new(rd).lines();
        let (my_id, my_name, _) = node(&self.cfg);

        // First message establishes the server identity + trust.
        let first = match lines.next_line().await {
            Ok(Some(l)) => l,
            _ => return Ok(()),
        };
        let v: Value = serde_json::from_str(&first).unwrap_or(Value::Null);
        let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
        let server_id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();

        let secret: String = if t == "hello" {
            // Reconnect from a paired server: known-key check.
            let presented = v.get("secret").and_then(|x| x.as_str()).unwrap_or("");
            match self.peer_by_id(&server_id) {
                Some(p) if p.secret == presented && !presented.is_empty() => p.secret,
                _ => {
                    send_line(&tx, json!({"t":"reject"}));
                    return Ok(());
                }
            }
        } else if t == "invite" {
            let server_name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let msg = v.get("msg").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let (dtx, drx) = oneshot::channel::<bool>();
            self.pending.lock().unwrap().insert(server_id.clone(), dtx);
            let _ = self.app.emit(
                "net-invite",
                json!({"fromId":server_id,"fromName":server_name,"msg":msg}),
            );
            let accepted = matches!(
                tokio::time::timeout(Duration::from_secs(INVITE_TIMEOUT_SECS), drx).await,
                Ok(Ok(true))
            );
            self.pending.lock().unwrap().remove(&server_id);
            if !accepted {
                send_line(&tx, json!({"t":"decline","id":my_id}));
                return Ok(());
            }
            let secret = config::random_hex(16);
            self.upsert_peer(Peer {
                id: server_id.clone(),
                name: server_name,
                secret: secret.clone(),
                role: "server".into(),
                addr: Some(addr.to_string()),
            });
            send_line(
                &tx,
                json!({"t":"accept","id":my_id,"name":my_name,"secret":secret}),
            );
            secret
        } else {
            return Ok(());
        };

        // Paired: handle assignments and stream reports every REPORT_SECS.
        // assigned id -> (local target id, interval)
        let mut assigned: HashMap<u32, (String, u64)> = HashMap::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(REPORT_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break; };
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue; };
                    if v.get("t").and_then(|x| x.as_str()) == Some("assign")
                        && v.get("secret").and_then(|x| x.as_str()) == Some(secret.as_str())
                    {
                        assigned = self.apply_assignment(&v);
                    }
                }
                _ = ticker.tick() => {
                    if assigned.is_empty() { continue; }
                    let mut results = Vec::new();
                    let to = now_ms();
                    let from = to.saturating_sub(REPORT_SECS * 1000);
                    for (aid, (local, interval)) in &assigned {
                        let s = self.db.stats(local, from, to);
                        results.push(json!({
                            "id": aid,
                            "rttMs": s.avg,
                            "lossPct": s.loss_pct,
                            "intervalMs": interval,
                        }));
                    }
                    send_line(&tx, json!({"t":"report","secret":secret,"id":my_id,"name":my_name,"results":results}));
                }
            }
        }
        Ok(())
    }

    /// Merge an assignment into our probing: reuse a probe if we already watch that host
    /// (keeping its cadence), otherwise add and start a new one. Returns id -> (localId, interval).
    fn apply_assignment(self: &Arc<Net>, v: &Value) -> HashMap<u32, (String, u64)> {
        let mut out = HashMap::new();
        let Some(arr) = v.get("targets").and_then(|t| t.as_array()) else {
            return out;
        };
        let mut added = false;
        for t in arr {
            let id = t.get("id").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let host = t.get("host").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let tag = t.get("tag").and_then(|x| x.as_str()).unwrap_or("internet").to_string();
            let label = t.get("label").and_then(|x| x.as_str()).unwrap_or(&host).to_string();
            let interval = t.get("intervalMs").and_then(|x| x.as_u64()).unwrap_or(1000);
            if host.is_empty() {
                continue;
            }

            let mut g = self.cfg.config.lock().unwrap();
            if let Some(existing) = g.targets.iter().find(|x| x.host == host) {
                // Already polling this host — reuse it (and its cadence).
                out.insert(id, (existing.id.clone(), existing.interval_ms));
            } else {
                let local_id = format!("asg-{:08x}", id);
                let target = Target {
                    id: local_id.clone(),
                    label,
                    host,
                    tag,
                    interval_ms: interval,
                };
                g.targets.push(target.clone());
                let _ = config::save(&self.cfg.path, &g);
                drop(g);
                let stop = self.probes.start(&local_id);
                crate::probe::spawn_probe(self.app.clone(), self.db.clone(), target, stop);
                out.insert(id, (local_id, interval));
                added = true;
            }
        }
        if added {
            let _ = self.app.emit("targets-changed", json!({}));
        }
        out
    }

    // ---- client side: respond to an invite (command) ----

    pub fn respond_invite(&self, server_id: String, accept: bool) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&server_id) {
            let _ = tx.send(accept);
        }
    }
}

/// Directed-broadcast targets for discovery: `en*` interfaces (Wi-Fi + physical Ethernet)
/// that are up with an IPv4, de-duplicated per subnet so each network is queried only once.
/// Falls back to the limited broadcast address if none are found.
fn discovery_targets() -> Vec<SocketAddr> {
    use std::collections::HashSet;
    use std::net::{IpAddr, Ipv4Addr};
    let mut seen: HashSet<Ipv4Addr> = HashSet::new();
    let mut out = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if iface.is_loopback() || !iface.name.starts_with("en") {
                continue;
            }
            if let if_addrs::IfAddr::V4(v4) = iface.addr {
                let bcast = v4
                    .broadcast
                    .unwrap_or_else(|| Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask)));
                if seen.insert(bcast) {
                    out.push(SocketAddr::new(IpAddr::V4(bcast), DISCO_PORT));
                }
            }
        }
    }
    if out.is_empty() {
        out.push(SocketAddr::from(([255, 255, 255, 255], DISCO_PORT)));
    }
    out
}

async fn broadcast_query(sock: &UdpSocket, q: &str) {
    for target in discovery_targets() {
        let _ = sock.send_to(q.as_bytes(), target).await;
    }
}

/// UDP socket bound to DISCO_PORT with address/port reuse and broadcast enabled.
fn bind_udp() -> std::io::Result<UdpSocket> {
    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    sock.set_reuse_address(true)?;
    let _ = sock.set_reuse_port(true);
    sock.set_broadcast(true)?;
    let addr: SocketAddr = ([0, 0, 0, 0], DISCO_PORT).into();
    sock.bind(&addr.into())?;
    sock.set_nonblocking(true)?;
    UdpSocket::from_std(sock.into())
}
