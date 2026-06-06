mod commands;
mod config;
mod db;
mod model;
mod probe;
mod probes;

use commands::ConfigState;
use db::Db;
use probes::Probes;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();

            let cfg_path = config::config_path(&handle)?;
            let cfg = config::load_or_default(&cfg_path);
            let _ = config::save(&cfg_path, &cfg);

            // history.db lives next to config.toml in the app config dir.
            let db_path = cfg_path
                .parent()
                .map(|p| p.join("history.db"))
                .unwrap_or_else(|| "history.db".into());
            let db = Arc::new(Db::open(&db_path)?);
            db.prune();

            let probes = Probes::new();
            for t in &cfg.targets {
                let stop = probes.start(&t.id);
                probe::spawn_probe(handle.clone(), db.clone(), t.clone(), stop);
            }

            // Heartbeat (keeps the uptime row fresh) + periodic prune.
            let hb_db = db.clone();
            tauri::async_runtime::spawn(async move {
                let mut ticks: u64 = 0;
                loop {
                    hb_db.heartbeat();
                    ticks += 1;
                    if ticks % 720 == 0 {
                        // ~hourly at 5s cadence
                        hb_db.prune();
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            });

            app.manage(db);
            app.manage(probes);
            app.manage(ConfigState {
                config: Mutex::new(cfg),
                path: cfg_path,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::get_window,
            commands::get_stats,
            commands::get_bounds,
            commands::add_target,
            commands::update_target,
            commands::remove_target,
            commands::reorder_targets,
            commands::set_tags,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
