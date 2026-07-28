use crate::config;
use crate::db::Db;
use crate::model::{AppConfig, Bucket, NodeInfo, Peer, Target, ThresholdSet, WindowStats};
use crate::net::Net;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::State;

/// Live configuration + where it persists. Managed (as `Arc`) so net tasks share it.
pub struct ConfigState {
    pub config: Mutex<AppConfig>,
    pub path: PathBuf,
}

fn save(cfg: &ConfigState, c: &AppConfig) -> Result<(), String> {
    config::save(&cfg.path, c).map_err(|e| e.to_string())
}

/// Tell the running `speedd` daemon to reload config.toml after we changed targets/intervals.
/// Best-effort: a no-op (harmless error) if the daemon isn't currently loaded.
fn reload_daemon() {
    let uid = unsafe { libc::getuid() };
    let _ = std::process::Command::new("launchctl")
        .args(["kill", "SIGHUP", &format!("gui/{uid}/org.est.speeddaemon")])
        .status();
}

// ---- poller (speedd LaunchAgent) control -----------------------------------
//
// The GUI owns the per-user LaunchAgent so end users never touch a terminal. Two independent
// controls:
//   - "run poller"     -> load/kickstart (start now) vs bootout (stop now); this session.
//   - "launch at login" -> the plist's RunAtLoad/KeepAlive, which macOS reads when it bootstraps
//                          ~/Library/LaunchAgents at login. Rewriting the file affects the NEXT
//                          login; the currently-loaded job is untouched, so the two don't fight.

const LABEL: &str = "org.est.speeddaemon";

fn svc_uid() -> u32 {
    unsafe { libc::getuid() }
}
fn svc_domain() -> String {
    format!("gui/{}", svc_uid())
}
fn svc_target() -> String {
    format!("gui/{}/{}", svc_uid(), LABEL)
}
fn plist_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// The speedd binary the LaunchAgent runs. It lives at a STABLE path in the data dir — never
/// inside the app bundle — so app updates/moves don't replace a running binary out from under
/// launchd (an OS_REASON_CODESIGNING kill) or leave a dangling agent. Stage 2 will copy the
/// bundled speedd here on install/update; in dev, bin/speedd-ctl puts it here.
fn speedd_path() -> PathBuf {
    config::data_dir().join("speedd")
}

fn launchctl(args: &[&str]) -> bool {
    std::process::Command::new("launchctl")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn poller_running() -> bool {
    std::process::Command::new("launchctl")
        .args(["print", &svc_target()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("state = running"))
        .unwrap_or(false)
}

/// Whether the service is bootstrapped into launchd (loaded), running or not.
fn poller_loaded() -> bool {
    std::process::Command::new("launchctl")
        .args(["print", &svc_target()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// True if the installed plist auto-starts at login (the `<true/>` after RunAtLoad comes before
/// any `<false/>`).
fn plist_at_login() -> bool {
    let Ok(s) = std::fs::read_to_string(plist_path()) else {
        return false;
    };
    let Some(i) = s.find("RunAtLoad") else {
        return false;
    };
    let rest = &s[i + "RunAtLoad".len()..];
    match (rest.find("<true/>"), rest.find("<false/>")) {
        (Some(_), None) => true,
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

fn write_plist(at_login: bool) -> std::io::Result<()> {
    let path = plist_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let b = |v: bool| if v { "<true/>" } else { "<false/>" };
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
    </array>
    <key>RunAtLoad</key>
    {run}
    <key>KeepAlive</key>
    {keep}
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        bin = speedd_path().display(),
        log = config::data_dir().join("speedd.log").display(),
        run = b(at_login),
        keep = b(at_login),
    );
    std::fs::write(path, content)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PollerStatus {
    running: bool,
    at_login: bool,
    installed: bool,
}

fn poller_state() -> PollerStatus {
    PollerStatus {
        running: poller_running(),
        at_login: plist_path().exists() && plist_at_login(),
        installed: plist_path().exists(),
    }
}

#[tauri::command]
pub async fn poller_status() -> Result<PollerStatus, String> {
    tauri::async_runtime::spawn_blocking(poller_state)
        .await
        .map_err(|e| e.to_string())
}

fn start_poller() {
    if !plist_path().exists() {
        let _ = write_plist(true);
    }
    let plist = plist_path().to_string_lossy().to_string();
    // A bootstrap immediately after a bootout fails with EIO until the old instance finishes
    // unloading — retry briefly until the service is actually loaded.
    for _ in 0..15 {
        if poller_loaded() {
            break;
        }
        let _ = launchctl(&["bootstrap", &svc_domain(), &plist]);
        if poller_loaded() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    // No `-k`: bootstrap already started it via RunAtLoad. A plain kickstart just starts it if the
    // plist has RunAtLoad=false (launch-at-login off); it's a no-op otherwise. (`-k` would kill the
    // fresh instance and restart it — a wasteful double-start.)
    let _ = launchctl(&["kickstart", &svc_target()]);
}

fn stop_poller() {
    let _ = launchctl(&["bootout", &svc_target()]);
}

fn restart_poller() {
    stop_poller();
    for _ in 0..15 {
        if !poller_loaded() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    start_poller();
}

/// The speedd binary Tauri bundles beside the app executable, if present (packaged build).
fn bundled_speedd() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("speedd")))
        .filter(|p| p.exists())
}

fn mtime_secs(p: &Path) -> Option<u64> {
    std::fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Keep the app self-contained: on launch, copy the bundled speedd to the stable data-dir path
/// (atomically, so a running daemon isn't code-sign-killed), first-run install the LaunchAgent,
/// and restart a running daemon when the binary changed. Skips gracefully in dev builds that
/// don't bundle speedd. Runs off the main thread (blocking launchctl / file IO).
pub fn ensure_poller_installed() {
    let stable = speedd_path();
    let stamp = config::data_dir().join("speedd.stamp");
    let mut updated = false;

    if let Some(bundled) = bundled_speedd() {
        // The bundle's speedd mtime is fixed at build time, so it's a stable "version" marker —
        // unlike the copy's mtime, which would change every launch and re-trigger the copy.
        let bmtime = mtime_secs(&bundled);
        let stamped = std::fs::read_to_string(&stamp)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        if !stable.exists() || bmtime.is_none() || stamped != bmtime {
            if let Some(dir) = stable.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = stable.with_extension("new");
            if std::fs::copy(&bundled, &tmp).is_ok() && std::fs::rename(&tmp, &stable).is_ok() {
                updated = true;
                if let Some(m) = bmtime {
                    let _ = std::fs::write(&stamp, m.to_string());
                }
            }
        }
    }

    if !plist_path().exists() {
        // First run: install the agent and start polling (the user can disable it in Settings).
        let _ = write_plist(true);
        start_poller();
    } else if updated && poller_loaded() {
        // App updated the binary and the daemon is running — restart it onto the new speedd.
        restart_poller();
    }
}

#[tauri::command]
pub async fn set_poller_running(run: bool) -> Result<PollerStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if run {
            start_poller();
        } else {
            stop_poller();
        }
        poller_state()
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_poller_at_login(enabled: bool) -> Result<PollerStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // Rewrite the plist only — takes effect at the next login; the loaded job is untouched.
        let _ = write_plist(enabled);
        poller_state()
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_config(cfg: State<Arc<ConfigState>>) -> AppConfig {
    cfg.config.lock().unwrap().clone()
}

// The read commands are async + spawn_blocking: the DB scan runs on a blocking thread (never
// the UI thread, so no beachball) and, via the Db reader pool, per-target queries run in
// parallel instead of serializing on one connection.
#[tauri::command]
pub async fn get_window(
    db: State<'_, Arc<Db>>,
    target_id: String,
    from: u64,
    to: u64,
    buckets: usize,
    agg: String,
) -> Result<Vec<Bucket>, String> {
    let db = db.inner().clone();
    tauri::async_runtime::spawn_blocking(move || db.window(&target_id, from, to, buckets, &agg))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_stats(
    db: State<'_, Arc<Db>>,
    target_id: String,
    from: u64,
    to: u64,
) -> Result<WindowStats, String> {
    let db = db.inner().clone();
    tauri::async_runtime::spawn_blocking(move || db.stats(&target_id, from, to))
        .await
        .map_err(|e| e.to_string())
}

/// The last `limit` raw samples at or before `before` (one Bucket per poll) for the true 1:1
/// view, where each pixel column is a single ping rather than the worst of a time slice.
#[tauri::command]
pub async fn get_samples(
    db: State<'_, Arc<Db>>,
    target_id: String,
    before: u64,
    limit: usize,
) -> Result<Vec<Bucket>, String> {
    let db = db.inner().clone();
    tauri::async_runtime::spawn_blocking(move || db.recent(&target_id, before, limit))
        .await
        .map_err(|e| e.to_string())
}

/// Export the raw samples in [from, to) as CSV to the user's Downloads folder, then reveal it
/// in Finder. Returns the saved file path.
#[tauri::command]
pub async fn export_csv(
    db: State<'_, Arc<Db>>,
    cfg: State<'_, Arc<ConfigState>>,
    from: u64,
    to: u64,
) -> Result<String, String> {
    let db = db.inner().clone();
    let targets = cfg.config.lock().unwrap().targets.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let csv = db.export_csv(from, to, &targets);

        let home = std::env::var("HOME").map_err(|_| "no HOME".to_string())?;
        let downloads = PathBuf::from(&home).join("Downloads");
        let dir = if downloads.is_dir() {
            downloads
        } else {
            PathBuf::from(&home)
        };
        let path = dir.join(format!("speed-daemon_{}-{}.csv", from / 1000, to / 1000));
        std::fs::write(&path, csv).map_err(|e| e.to_string())?;

        // Reveal in Finder (best-effort; the saved path is returned regardless).
        let _ = std::process::Command::new("open").arg("-R").arg(&path).spawn();

        Ok(path.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_bounds(db: State<'_, Arc<Db>>) -> Result<u64, String> {
    let db = db.inner().clone();
    tauri::async_runtime::spawn_blocking(move || db.oldest().unwrap_or(0))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn add_target(cfg: State<Arc<ConfigState>>, target: Target) -> Result<AppConfig, String> {
    let result = {
        let mut guard = cfg.config.lock().unwrap();
        if guard.targets.iter().any(|t| t.id == target.id) {
            return Err(format!("target id '{}' already exists", target.id));
        }
        guard.targets.push(target);
        save(&cfg, &guard)?;
        guard.clone()
    };
    reload_daemon();
    Ok(result)
}

#[tauri::command]
pub fn update_target(cfg: State<Arc<ConfigState>>, target: Target) -> Result<AppConfig, String> {
    let result = {
        let mut guard = cfg.config.lock().unwrap();
        match guard.targets.iter_mut().find(|t| t.id == target.id) {
            Some(slot) => *slot = target,
            None => guard.targets.push(target),
        }
        save(&cfg, &guard)?;
        guard.clone()
    };
    // speedd re-reads every target on SIGHUP, so any host/interval change just takes effect.
    reload_daemon();
    Ok(result)
}

#[tauri::command]
pub fn remove_target(
    cfg: State<Arc<ConfigState>>,
    target_id: String,
) -> Result<AppConfig, String> {
    let result = {
        let mut guard = cfg.config.lock().unwrap();
        guard.targets.retain(|t| t.id != target_id);
        save(&cfg, &guard)?;
        guard.clone()
    };
    reload_daemon();
    Ok(result)
}

#[tauri::command]
pub fn reorder_targets(
    cfg: State<Arc<ConfigState>>,
    order: Vec<String>,
) -> Result<AppConfig, String> {
    let mut guard = cfg.config.lock().unwrap();
    let mut by_id: HashMap<String, Target> =
        guard.targets.drain(..).map(|t| (t.id.clone(), t)).collect();
    let mut ordered: Vec<Target> = Vec::with_capacity(order.len());
    for id in &order {
        if let Some(t) = by_id.remove(id) {
            ordered.push(t);
        }
    }
    ordered.extend(by_id.into_values());
    guard.targets = ordered;
    save(&cfg, &guard)?;
    Ok(guard.clone())
}

#[tauri::command]
pub fn set_sets(
    cfg: State<Arc<ConfigState>>,
    sets: Vec<ThresholdSet>,
) -> Result<AppConfig, String> {
    let mut guard = cfg.config.lock().unwrap();
    guard.sets = sets;
    save(&cfg, &guard)?;
    Ok(guard.clone())
}

#[tauri::command]
pub fn set_theme(cfg: State<Arc<ConfigState>>, theme: String) -> Result<AppConfig, String> {
    let mut guard = cfg.config.lock().unwrap();
    guard.theme = match theme.as_str() {
        "light" | "dark" => theme,
        _ => "system".into(),
    };
    save(&cfg, &guard)?;
    Ok(guard.clone())
}

#[tauri::command]
pub fn set_aggregate(cfg: State<Arc<ConfigState>>, aggregate: String) -> Result<AppConfig, String> {
    let mut guard = cfg.config.lock().unwrap();
    guard.aggregate = match aggregate.as_str() {
        "best" | "trimmed" | "mean" | "median" => aggregate,
        _ => "worst".into(),
    };
    save(&cfg, &guard)?;
    Ok(guard.clone())
}

// ---- networking ----------------------------------------------------------

#[tauri::command]
pub fn get_node(cfg: State<Arc<ConfigState>>) -> NodeInfo {
    cfg.config.lock().unwrap().node.clone()
}

#[tauri::command]
pub fn set_mode(cfg: State<Arc<ConfigState>>, mode: String) -> Result<NodeInfo, String> {
    let mut guard = cfg.config.lock().unwrap();
    guard.node.mode = if mode == "server" { "server" } else { "client" }.into();
    save(&cfg, &guard)?;
    Ok(guard.node.clone())
}

#[tauri::command]
pub fn set_node_name(cfg: State<Arc<ConfigState>>, name: String) -> Result<NodeInfo, String> {
    let mut guard = cfg.config.lock().unwrap();
    guard.node.name = name;
    save(&cfg, &guard)?;
    Ok(guard.node.clone())
}

#[tauri::command]
pub fn get_peers(cfg: State<Arc<ConfigState>>) -> Vec<Peer> {
    cfg.config.lock().unwrap().peers.clone()
}

#[tauri::command]
pub fn net_discovered(net: State<Arc<Net>>) -> Vec<Value> {
    net.discovered_list()
}

#[tauri::command]
pub async fn net_discover_now(net: State<'_, Arc<Net>>) -> Result<(), String> {
    net.discover_now(true).await;
    Ok(())
}

#[tauri::command]
pub fn net_invite(net: State<Arc<Net>>, peer_id: String, message: String) {
    net.invite(peer_id, message);
}

#[tauri::command]
pub fn net_respond_invite(net: State<Arc<Net>>, server_id: String, accept: bool) {
    net.respond_invite(server_id, accept);
}
