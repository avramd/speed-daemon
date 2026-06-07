use crate::config;
use crate::db::Db;
use crate::model::{AppConfig, Bucket, NodeInfo, Peer, TagProfile, Target, WindowStats};
use crate::net::Net;
use crate::probes::Probes;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, State};

/// Live configuration + where it persists. Managed (as `Arc`) so net tasks share it.
pub struct ConfigState {
    pub config: Mutex<AppConfig>,
    pub path: PathBuf,
}

fn save(cfg: &ConfigState, c: &AppConfig) -> Result<(), String> {
    config::save(&cfg.path, c).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_config(cfg: State<Arc<ConfigState>>) -> AppConfig {
    cfg.config.lock().unwrap().clone()
}

#[tauri::command]
pub fn get_window(
    db: State<Arc<Db>>,
    target_id: String,
    from: u64,
    to: u64,
    buckets: usize,
) -> Vec<Bucket> {
    db.window(&target_id, from, to, buckets)
}

#[tauri::command]
pub fn get_stats(db: State<Arc<Db>>, target_id: String, from: u64, to: u64) -> WindowStats {
    db.stats(&target_id, from, to)
}

#[tauri::command]
pub fn get_bounds(db: State<Arc<Db>>) -> u64 {
    db.oldest().unwrap_or(0)
}

#[tauri::command]
pub fn add_target(
    app: AppHandle,
    db: State<Arc<Db>>,
    probes: State<Arc<Probes>>,
    cfg: State<Arc<ConfigState>>,
    net: State<Arc<Net>>,
    target: Target,
) -> Result<AppConfig, String> {
    let result = {
        let mut guard = cfg.config.lock().unwrap();
        if guard.targets.iter().any(|t| t.id == target.id) {
            return Err(format!("target id '{}' already exists", target.id));
        }
        guard.targets.push(target.clone());
        save(&cfg, &guard)?;
        guard.clone()
    };
    let stop = probes.start(&target.id);
    crate::probe::spawn_probe(app, db.inner().clone(), target, stop);
    net.reassign_all();
    Ok(result)
}

#[tauri::command]
pub fn update_target(
    app: AppHandle,
    db: State<Arc<Db>>,
    probes: State<Arc<Probes>>,
    cfg: State<Arc<ConfigState>>,
    net: State<Arc<Net>>,
    target: Target,
) -> Result<AppConfig, String> {
    let (result, needs_restart) = {
        let mut guard = cfg.config.lock().unwrap();
        let needs_restart = match guard.targets.iter().find(|t| t.id == target.id) {
            Some(old) => old.host != target.host || old.interval_ms != target.interval_ms,
            None => true,
        };
        match guard.targets.iter_mut().find(|t| t.id == target.id) {
            Some(slot) => *slot = target.clone(),
            None => guard.targets.push(target.clone()),
        }
        save(&cfg, &guard)?;
        (guard.clone(), needs_restart)
    };
    if needs_restart {
        probes.stop(&target.id);
        let stop = probes.start(&target.id);
        crate::probe::spawn_probe(app, db.inner().clone(), target, stop);
    }
    net.reassign_all();
    Ok(result)
}

#[tauri::command]
pub fn remove_target(
    probes: State<Arc<Probes>>,
    cfg: State<Arc<ConfigState>>,
    net: State<Arc<Net>>,
    target_id: String,
) -> Result<AppConfig, String> {
    let result = {
        let mut guard = cfg.config.lock().unwrap();
        guard.targets.retain(|t| t.id != target_id);
        save(&cfg, &guard)?;
        guard.clone()
    };
    probes.stop(&target_id);
    net.reassign_all();
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
