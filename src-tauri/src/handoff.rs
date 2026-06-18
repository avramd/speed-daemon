// Near-zero-downtime instance handoff via two semaphore files in the data dir.
//
// poller.sem   = the active collector's "<pid> <checkin_ms>" (refreshed each tick)
// takeover.sem = a newly-launched instance waiting to take over
//
// An instance booting up tries to atomically claim poller.sem. If a live poller already
// holds it, the newcomer writes takeover.sem and waits. The poller, on its check-in tick,
// sees the takeover, drops poller.sem and quits — at which point the waiter claims it and
// starts collecting. A semaphore whose pid is no longer running is cleaned up by whoever
// notices, so a crash never wedges the system.
//
// Every transition is appended to handoff.log (and echoed to stderr) so a handover can be
// debugged after the fact without reproducing it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::AppHandle;

const TICK: Duration = Duration::from_millis(1000); // poller check-in cadence
const WAIT: Duration = Duration::from_millis(250); // waiter poll cadence

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn pid_alive(pid: u32) -> bool {
    pid != 0 && unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn poller_path(dir: &Path) -> PathBuf {
    dir.join("poller.sem")
}
fn takeover_path(dir: &Path) -> PathBuf {
    dir.join("takeover.sem")
}

fn read_pid(path: &Path) -> Option<u32> {
    let s = fs::read_to_string(path).ok()?;
    s.split_whitespace().next()?.parse().ok()
}

fn write_sem(path: &Path, pid: u32) {
    let _ = fs::write(path, format!("{} {}", pid, now_ms()));
}

/// Append a transition to handoff.log (epoch-ms timestamp + pid) and echo to stderr.
fn log(dir: &Path, msg: &str) {
    let line = format!("{} pid={} {}\n", now_ms(), std::process::id(), msg);
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("handoff.log"))
    {
        let _ = f.write_all(line.as_bytes());
    }
    eprintln!("handoff: {msg}");
}

/// Atomically claim poller.sem (only one creator wins). Returns true if we now own it.
fn claim(path: &Path, pid: u32) -> bool {
    match fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            let _ = write!(f, "{} {}", pid, now_ms());
            true
        }
        Err(_) => false,
    }
}

/// Spawn the handoff coordinator. `start_collecting` runs exactly once, when this instance
/// becomes the active poller.
pub fn spawn(app: AppHandle, dir: PathBuf, start_collecting: impl FnOnce() + Send + 'static) {
    tauri::async_runtime::spawn(async move {
        let me = std::process::id();
        let poller = poller_path(&dir);
        let takeover = takeover_path(&dir);

        // Acquire the poller role, waiting behind a live poller if necessary.
        let mut announced = false;
        loop {
            if claim(&poller, me) {
                break;
            }
            match read_pid(&poller) {
                Some(pid) if pid_alive(pid) => {
                    if !announced {
                        log(&dir, &format!("waiting to take over from pid {pid}"));
                        announced = true;
                    }
                    write_sem(&takeover, me); // (re)assert our takeover marker while waiting
                    tokio::time::sleep(WAIT).await;
                }
                Some(pid) => {
                    log(&dir, &format!("clearing stale poller.sem (dead pid {pid})"));
                    let _ = fs::remove_file(&poller);
                }
                None => {
                    let _ = fs::remove_file(&poller); // vanished between claim and read; retry
                }
            }
        }
        let _ = fs::remove_file(&takeover); // we're the poller now; clear our takeover marker
        log(
            &dir,
            if announced {
                "promoted to active poller (took over)"
            } else {
                "claimed active poller (cold start)"
            },
        );

        start_collecting();

        // Poller loop: check in, and hand off if a live takeover appears.
        loop {
            tokio::time::sleep(TICK).await;
            write_sem(&poller, me);
            match read_pid(&takeover) {
                Some(pid) if pid_alive(pid) => {
                    log(&dir, &format!("handing off to pid {pid}; releasing and quitting"));
                    let _ = fs::remove_file(&poller); // release first, then exit
                    app.exit(0);
                    return;
                }
                Some(pid) => {
                    log(&dir, &format!("clearing stale takeover.sem (dead pid {pid})"));
                    let _ = fs::remove_file(&takeover);
                }
                None => {}
            }
        }
    });
}
