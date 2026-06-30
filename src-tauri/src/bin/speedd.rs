// speedd — the Speed Daemon poller daemon.
//
// Headless: runs the real prober (icmp::Pinger via the Tauri-free poll loop) in a plain CLI
// process so the OS measures our Wi-Fi RTT correctly — a GUI/AppKit process gets its sends
// deferred ~40ms and inflates every reading. Managed by launchd, or run standalone.
//
//   speedd                 # poll every target in the config, write history.db, run forever
//   SIGHUP                 # reload config.toml and respawn the per-target loops
//   SIGTERM / SIGINT       # exit
//
// Config + history live in the data dir (config::data_dir(), overridable via SPEED_DAEMON_DIR),
// shared with the GUI viewer.

use speed_daemon_lib::{config, db::Db, model::AppConfig, poll};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::signal::unix::{signal, SignalKind};

#[tokio::main]
async fn main() {
    let dir = config::data_dir();
    let cfg_path = dir.join("config.toml");
    let cfg = config::load_or_default(&cfg_path);
    // Materialize defaults on first run so there's a file to edit.
    if let Err(e) = config::save(&cfg_path, &cfg) {
        eprintln!("speedd: warning: could not write {}: {e}", cfg_path.display());
    }

    let db = match Db::open(&dir.join("history.db"), false) {
        Ok(db) => Arc::new(db),
        Err(e) => {
            eprintln!("speedd: fatal: could not open history.db: {e}");
            std::process::exit(1);
        }
    };
    db.prune();

    let mut stops = spawn_targets(&cfg, &db);
    eprintln!(
        "speedd: polling {} target(s); data dir {}",
        cfg.targets.len(),
        dir.display()
    );

    // Uptime heartbeat (so post-restart gaps are detectable) + periodic prune.
    {
        let hb = db.clone();
        tokio::spawn(async move {
            let mut ticks: u64 = 0;
            loop {
                hb.heartbeat();
                ticks += 1;
                if ticks % 720 == 0 {
                    hb.prune();
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    let mut hup = signal(SignalKind::hangup()).expect("install SIGHUP handler");
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");

    // Reload on an explicit SIGHUP (the GUI sends one after edits) OR when config.toml's mtime
    // changes — so a hand-edit, a file sync, or a GUI edit whose signal didn't land all apply
    // within a couple seconds without any manual step.
    let mut last_mtime = file_mtime(&cfg_path);
    let mut watch = tokio::time::interval(Duration::from_secs(2));

    loop {
        tokio::select! {
            _ = hup.recv() => {
                last_mtime = file_mtime(&cfg_path);
                reload_targets(&cfg_path, &db, &mut stops);
            }
            _ = watch.tick() => {
                let m = file_mtime(&cfg_path);
                if m != last_mtime {
                    last_mtime = m;
                    reload_targets(&cfg_path, &db, &mut stops);
                }
            }
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }

    for s in &stops {
        s.store(true, Ordering::Relaxed);
    }
    eprintln!("speedd: shutting down");
}

/// Retire the running per-target threads and start fresh ones from the on-disk config.
fn reload_targets(cfg_path: &Path, db: &Arc<Db>, stops: &mut Vec<Arc<AtomicBool>>) {
    for s in stops.iter() {
        s.store(true, Ordering::Relaxed);
    }
    let cfg = config::load_or_default(cfg_path);
    *stops = spawn_targets(&cfg, db);
    eprintln!("speedd: reloaded config ({} target(s))", cfg.targets.len());
}

fn file_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Start one ping loop per target; return their stop flags so a reload can retire them.
fn spawn_targets(cfg: &AppConfig, db: &Arc<Db>) -> Vec<Arc<AtomicBool>> {
    cfg.targets
        .iter()
        .map(|t| {
            let stop = Arc::new(AtomicBool::new(false));
            poll::spawn(db.clone(), t.clone(), stop.clone());
            stop
        })
        .collect()
}
