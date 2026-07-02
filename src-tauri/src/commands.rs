use crate::config;
use crate::db::Db;
use crate::model::{AppConfig, Bucket, NodeInfo, Peer, TagProfile, Target, WindowStats};
use crate::net::Net;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
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
pub fn set_tags(cfg: State<Arc<ConfigState>>, tags: Vec<TagProfile>) -> Result<AppConfig, String> {
    let mut guard = cfg.config.lock().unwrap();
    guard.tags = tags;
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
