use crate::config;
use crate::db::Db;
use crate::model::{AppConfig, Bucket, TagProfile, Target};
use crate::probes::Probes;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, State};

/// Live configuration + where it persists. Managed as Tauri state.
pub struct ConfigState {
    pub config: Mutex<AppConfig>,
    pub path: PathBuf,
}

fn persist(state: &ConfigState, cfg: &AppConfig) -> Result<(), String> {
    config::save(&state.path, cfg).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_config(cfg: State<ConfigState>) -> AppConfig {
    cfg.config.lock().unwrap().clone()
}

/// Aggregated history for one target over [from, to) into `buckets` pixel columns.
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

/// Earliest instant known to the store (for slider bounds). 0 if empty.
#[tauri::command]
pub fn get_bounds(db: State<Arc<Db>>) -> u64 {
    db.oldest().unwrap_or(0)
}

#[tauri::command]
pub fn add_target(
    app: AppHandle,
    db: State<Arc<Db>>,
    probes: State<Probes>,
    cfg: State<ConfigState>,
    target: Target,
) -> Result<AppConfig, String> {
    let mut guard = cfg.config.lock().unwrap();
    if guard.targets.iter().any(|t| t.id == target.id) {
        return Err(format!("target id '{}' already exists", target.id));
    }
    guard.targets.push(target.clone());
    persist(&cfg, &guard)?;

    let stop = probes.start(&target.id);
    crate::probe::spawn_probe(app, db.inner().clone(), target, stop);
    Ok(guard.clone())
}

#[tauri::command]
pub fn update_target(
    app: AppHandle,
    db: State<Arc<Db>>,
    probes: State<Probes>,
    cfg: State<ConfigState>,
    target: Target,
) -> Result<AppConfig, String> {
    let mut guard = cfg.config.lock().unwrap();

    // Only restart the probe when something it depends on changed; a pure
    // label/tag rename keeps the existing probe (and history is on disk regardless).
    let needs_restart = match guard.targets.iter().find(|t| t.id == target.id) {
        Some(old) => old.host != target.host || old.interval_ms != target.interval_ms,
        None => true,
    };

    match guard.targets.iter_mut().find(|t| t.id == target.id) {
        Some(slot) => *slot = target.clone(),
        None => guard.targets.push(target.clone()),
    }
    persist(&cfg, &guard)?;

    if needs_restart {
        probes.stop(&target.id);
        let stop = probes.start(&target.id);
        crate::probe::spawn_probe(app, db.inner().clone(), target, stop);
    }
    Ok(guard.clone())
}

#[tauri::command]
pub fn remove_target(
    probes: State<Probes>,
    cfg: State<ConfigState>,
    target_id: String,
) -> Result<AppConfig, String> {
    let mut guard = cfg.config.lock().unwrap();
    guard.targets.retain(|t| t.id != target_id);
    persist(&cfg, &guard)?;
    probes.stop(&target_id);
    Ok(guard.clone())
}

/// Reorder targets to match the given list of ids (others appended, unknown ignored).
#[tauri::command]
pub fn reorder_targets(
    cfg: State<ConfigState>,
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
    // Any target not named in `order` keeps its place at the end.
    ordered.extend(by_id.into_values());
    guard.targets = ordered;
    persist(&cfg, &guard)?;
    Ok(guard.clone())
}

/// Update tag expectation profiles. Tags only affect frontend coloring/scaling, so no
/// probe restart is needed.
#[tauri::command]
pub fn set_tags(cfg: State<ConfigState>, tags: Vec<TagProfile>) -> Result<AppConfig, String> {
    let mut guard = cfg.config.lock().unwrap();
    guard.tags = tags;
    persist(&cfg, &guard)?;
    Ok(guard.clone())
}
