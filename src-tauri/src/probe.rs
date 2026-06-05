use crate::db::Db;
use crate::model::{Sample, SampleEvent, Target};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use surge_ping::{Client, Config, PingIdentifier, PingSequence, Pinger, ICMP};
use tauri::{AppHandle, Emitter};

/// Re-resolve domain names this often so DNS changes (and CDN rotations) are picked up.
const RESOLVE_EVERY: Duration = Duration::from_secs(60);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Stable per-target ICMP identifier so each probe's socket is distinguishable.
fn stable_id(s: &str) -> u16 {
    let mut h: u16 = 0;
    for b in s.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as u16);
    }
    h
}

/// Resolve a host (IP literal or domain) to a single address, preferring IPv4.
async fn resolve(host: &str) -> Option<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip);
    }
    let addrs: Vec<IpAddr> = tokio::net::lookup_host((host, 0))
        .await
        .ok()?
        .map(|s| s.ip())
        .collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .copied()
        .or_else(|| addrs.first().copied())
}

/// Unprivileged ICMP via a SOCK_DGRAM socket — works without root on macOS.
fn build_client(ip: IpAddr) -> std::io::Result<Client> {
    let kind = if ip.is_ipv4() { ICMP::V4 } else { ICMP::V6 };
    let cfg = Config::builder()
        .kind(kind)
        .sock_type_hint(socket2::Type::DGRAM)
        .build();
    Client::new(&cfg)
}

/// Spawn the continuous ping loop for one target on Tauri's async runtime.
pub fn spawn_probe(app: AppHandle, db: Arc<Db>, target: Target, stop: Arc<AtomicBool>) {
    tauri::async_runtime::spawn(async move {
        let id = PingIdentifier(stable_id(&target.id));
        let timeout_ms = target.interval_ms.saturating_sub(50).clamp(200, 5000);
        let payload = [0u8; 56];

        let mut seq: u16 = 0;
        let mut current_ip: Option<IpAddr> = None;
        // (resolved ip, client kept alive for its recv task, pinger)
        let mut probe: Option<(IpAddr, Client, Pinger)> = None;
        let mut last_resolve = Instant::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            // (Re)resolve on first pass and periodically thereafter.
            if current_ip.is_none() || last_resolve.elapsed() >= RESOLVE_EVERY {
                if let Some(ip) = resolve(&target.host).await {
                    if Some(ip) != current_ip {
                        current_ip = Some(ip);
                        probe = None; // address (or family) changed -> rebuild
                    }
                }
                last_resolve = Instant::now();
            }

            // Build the pinger lazily / when the address changes.
            if let Some(ip) = current_ip {
                let need_new = !matches!(&probe, Some((pip, _, _)) if *pip == ip);
                if need_new {
                    match build_client(ip) {
                        Ok(client) => {
                            let mut pinger = client.pinger(ip, id).await;
                            pinger.timeout(Duration::from_millis(timeout_ms));
                            probe = Some((ip, client, pinger));
                        }
                        Err(_) => probe = None,
                    }
                }
            } else {
                probe = None;
            }

            let sample = if let Some((_, _client, pinger)) = probe.as_mut() {
                match pinger.ping(PingSequence(seq), &payload).await {
                    Ok((_packet, dur)) => Sample {
                        ts: now_ms(),
                        rtt_ms: Some(dur.as_secs_f64() * 1000.0),
                    },
                    // Timeout / unreachable / send error all count as loss.
                    Err(_) => Sample {
                        ts: now_ms(),
                        rtt_ms: None,
                    },
                }
            } else {
                // Could not resolve or open a socket — record as loss.
                Sample {
                    ts: now_ms(),
                    rtt_ms: None,
                }
            };

            seq = seq.wrapping_add(1);
            let ip_str = current_ip.map(|i| i.to_string());
            db.insert(&target.id, sample.ts, sample.rtt_ms);
            let _ = app.emit(
                "sample",
                SampleEvent {
                    target_id: target.id.clone(),
                    resolved_ip: ip_str,
                    sample,
                },
            );

            tokio::time::sleep(Duration::from_millis(target.interval_ms)).await;
        }
    });
}
