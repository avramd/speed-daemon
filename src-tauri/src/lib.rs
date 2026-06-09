mod commands;
mod config;
mod db;
mod model;
mod net;
mod probe;
mod probes;

use commands::ConfigState;
use db::Db;
use net::Net;
use probes::Probes;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

/// Dev-only (macOS): this file's presence/mtime drives the application-level hide so the
/// dev window stays out of the way (and survives `tauri dev` relaunches), while ⌘-tab /
/// LaunchBar / Dock still unhide it natively. Toggled by `bin/dev-mode hide` / `show`.
#[cfg(all(debug_assertions, target_os = "macos"))]
const DEV_HIDE_FLAG: &str = "/tmp/speed-daemon-dev.hidden";

#[cfg(all(debug_assertions, target_os = "macos"))]
fn flag_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(DEV_HIDE_FLAG)
        .and_then(|m| m.modified())
        .ok()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[allow(unused_mut)] // `mut` is only used by the release-only cfg block below
    let mut builder = tauri::Builder::default();
    // Single-instance only in release builds, so a dev build can run alongside the
    // installed collector (they share the bundle identifier). A second launch of the
    // real app focuses the running one (one collector).
    #[cfg(not(debug_assertions))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }));
    }
    builder
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();

            let cfg_path = config::config_path(&handle)?;
            let cfg = config::load_or_default(&cfg_path);
            let _ = config::save(&cfg_path, &cfg);

            // Read-only viewer (dev): share the real instance's files, but don't poll,
            // write, heartbeat, prune, or run networking — just read the live history.
            let readonly = std::env::var("SPEED_DAEMON_READONLY").is_ok();

            let db_path = cfg_path
                .parent()
                .map(|p| p.join("history.db"))
                .unwrap_or_else(|| "history.db".into());
            let db = Arc::new(Db::open(&db_path, readonly)?);

            let probes = Arc::new(Probes::new());
            if !readonly {
                db.prune();
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
                            hb_db.prune();
                        }
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                });
            }

            let cfg_state = Arc::new(ConfigState {
                config: Mutex::new(cfg),
                path: cfg_path,
            });

            let net = Net::new(handle.clone(), db.clone(), probes.clone(), cfg_state.clone());
            if !readonly {
                net.start();
            }

            app.manage(db);
            app.manage(probes);
            app.manage(cfg_state);
            app.manage(net);

            // Auto-start at login — release builds only, so dev binaries aren't registered.
            #[cfg(not(debug_assertions))]
            {
                use tauri_plugin_autostart::ManagerExt;
                let _ = handle.autolaunch().enable();
            }

            // Tray icon so the app can live in the background with the window closed.
            let show = MenuItem::with_id(&handle, "show", "Show Speed Daemon", true, None::<&str>)?;
            let quit = MenuItem::with_id(&handle, "quit", "Quit Speed Daemon", true, None::<&str>)?;
            let menu = Menu::with_items(&handle, &[&show, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Speed Daemon")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(&handle)?;

            // Closing the window hides it instead of quitting (keep collecting).
            if let Some(win) = handle.get_webview_window("main") {
                #[cfg(debug_assertions)]
                let _ = win.set_title("Speed Daemon (dev)");
                let w = win.clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                });
            }

            // Dev (macOS): application-level hide driven by the hide-flag, edge-triggered on
            // the file's mtime so a native unhide (⌘-tab / LaunchBar / Dock) isn't fought.
            #[cfg(all(debug_assertions, target_os = "macos"))]
            {
                if std::path::Path::new(DEV_HIDE_FLAG).exists() {
                    let _ = handle.hide(); // start hidden if flagged (e.g. relaunch during churn)
                }
                let h = handle.clone();
                let mut last = flag_mtime();
                tauri::async_runtime::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_millis(400)).await;
                        let cur = flag_mtime();
                        match (last, cur) {
                            // flag created or touched -> hide the app
                            (l, Some(m)) if l != Some(m) => {
                                let _ = h.hide();
                            }
                            // flag removed -> show + focus
                            (Some(_), None) => {
                                let _ = h.show();
                                if let Some(w) = h.get_webview_window("main") {
                                    let _ = w.set_focus();
                                }
                            }
                            _ => {}
                        }
                        last = cur;
                    }
                });
            }

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
            commands::set_theme,
            commands::get_node,
            commands::set_mode,
            commands::set_node_name,
            commands::get_peers,
            commands::net_discovered,
            commands::net_discover_now,
            commands::net_invite,
            commands::net_respond_invite,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // macOS: relaunching/clicking the Dock icon re-opens the hidden window
            // (relaunch only activates the running process — no second instance).
            if let tauri::RunEvent::Reopen { .. } = event {
                if let Some(w) = app_handle.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
        });
}
