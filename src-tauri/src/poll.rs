// The daemon's polling loop: one dedicated OS thread per target, each parked in a blocking
// recv (see `icmp::BlockingPinger`). All in one process. Blocking — not async/kqueue — because
// a kqueue-parked socket gets ~40ms of Wi-Fi power-save deprioritization once the process isn't
// the foreground app, while a thread actively blocked in recvmsg stays in the fast path.
//
// One thread per target (not one shared thread) so a slow or timing-out target can't stall the
// others behind it: each blocks independently on its own socket. The threads are asleep in the
// kernel almost all the time, so a dozen of them is cheap.

use crate::db::Db;
use crate::icmp::BlockingPinger;
use crate::model::Target;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Re-resolve domain names this often so DNS changes (and CDN rotations) are picked up.
const RESOLVE_EVERY: Duration = Duration::from_secs(60);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Resolve a host (IP literal or domain) to a single address, preferring IPv4. Blocking.
fn resolve(host: &str) -> Option<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip);
    }
    let addrs: Vec<IpAddr> = (host, 0u16)
        .to_socket_addrs()
        .ok()?
        .map(|s| s.ip())
        .collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .copied()
        .or_else(|| addrs.first().copied())
}

/// Spawn the continuous ping loop for one target on its own OS thread. `stop` retires the
/// thread (within ~one interval) on a config reload.
pub fn spawn(db: Arc<Db>, target: Target, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        // 1s floor: never poll a destination more than once per wall-clock second, so there's
        // exactly one sample per second per target (no intra-second aggregation to reason about).
        let interval_ms = target.interval_ms.max(1000);
        let interval = Duration::from_millis(interval_ms);
        let timeout = Duration::from_millis(interval_ms.saturating_sub(50).clamp(200, 5000));

        let mut seq: u16 = 0;
        let mut current_ip: Option<IpAddr> = None;
        let mut pinger: Option<(IpAddr, BlockingPinger)> = None;
        let mut last_resolve = Instant::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);
        let mut next_send = Instant::now();

        loop {
            // Wait until this poll is due, then bail promptly if we've been retired.
            let now = Instant::now();
            if next_send > now {
                std::thread::sleep(next_send - now);
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }

            // (Re)resolve on first pass and periodically thereafter.
            if current_ip.is_none() || last_resolve.elapsed() >= RESOLVE_EVERY {
                if let Some(ip) = resolve(&target.host) {
                    if Some(ip) != current_ip {
                        current_ip = Some(ip);
                        pinger = None; // address (or family) changed -> rebuild socket
                    }
                }
                last_resolve = Instant::now();
            }

            // Build the per-target socket lazily / when the address changes.
            if let Some(ip) = current_ip {
                if !matches!(&pinger, Some((pip, _)) if *pip == ip) {
                    pinger = BlockingPinger::new(ip).ok().map(|p| (ip, p));
                }
            } else {
                pinger = None;
            }

            // Schedule the next poll a full interval after THIS send, so two sends are never
            // less than `interval` (>= 1s) apart regardless of resolve/ping jitter.
            next_send = Instant::now() + interval;

            // Stamp the sample with the SEND time, not the reply time: the reply lands `rtt` ms
            // later, and that jitter (often ±150ms) would otherwise bump samples across second
            // boundaries — producing phantom empty/duplicate wall-clock seconds.
            let send_ms = now_ms();
            let rtt = match pinger.as_ref() {
                Some((_, p)) => p.ping(seq, timeout),
                None => None, // couldn't resolve or open a socket -> loss
            };

            seq = seq.wrapping_add(1);
            db.insert(&target.id, send_ms, rtt);
        }
    });
}
