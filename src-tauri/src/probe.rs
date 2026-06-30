use crate::db::Db;
use crate::icmp::Pinger;
use crate::model::{Sample, SampleEvent, Target};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

/// Re-resolve domain names this often so DNS changes (and CDN rotations) are picked up.
const RESOLVE_EVERY: Duration = Duration::from_secs(60);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

/// Spawn the continuous ping loop for one target on Tauri's async runtime.
pub fn spawn_probe(app: AppHandle, db: Arc<Db>, target: Target, stop: Arc<AtomicBool>) {
    tauri::async_runtime::spawn(async move {
        let timeout =
            Duration::from_millis(target.interval_ms.saturating_sub(50).clamp(200, 5000));

        let mut seq: u16 = 0;
        let mut current_ip: Option<IpAddr> = None;
        let mut pinger: Option<(IpAddr, Pinger)> = None;
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
                        pinger = None; // address (or family) changed -> rebuild socket
                    }
                }
                last_resolve = Instant::now();
            }

            // Build the per-target socket lazily / when the address changes.
            if let Some(ip) = current_ip {
                if !matches!(&pinger, Some((pip, _)) if *pip == ip) {
                    pinger = Pinger::new(ip).ok().map(|p| (ip, p));
                }
            } else {
                pinger = None;
            }

            let rtt = match pinger.as_ref() {
                Some((_, p)) => p.ping(seq, timeout).await,
                None => None, // couldn't resolve or open a socket -> loss
            };

            seq = seq.wrapping_add(1);
            let sample = Sample {
                ts: now_ms(),
                rtt_ms: rtt,
            };
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
