use crate::model::{AppConfig, TagProfile, Target};
use std::path::{Path, PathBuf};
use tauri::Manager;

/// `<app config dir>/config.toml` — e.g. ~/Library/Application Support/org.est.speeddaemon/
pub fn config_path(app: &tauri::AppHandle) -> tauri::Result<PathBuf> {
    Ok(app.path().app_config_dir()?.join("config.toml"))
}

pub fn load_or_default(path: &Path) -> AppConfig {
    match std::fs::read_to_string(path) {
        Ok(s) => match toml::from_str::<AppConfig>(&s) {
            Ok(mut cfg) => {
                // One-time upgrade: if the tag thresholds are still the original
                // auto-generated defaults, adopt the new defaults (keeping the user's
                // targets). Customized thresholds are left untouched.
                if tags_are_legacy(&cfg.tags) {
                    cfg.tags = default_config().tags;
                }
                cfg
            }
            Err(_) => default_config(),
        },
        Err(_) => default_config(),
    }
}

/// True if `tags` matches the original 6/10/20/40-times-multiplier seed exactly.
fn tags_are_legacy(tags: &[TagProfile]) -> bool {
    let legacy = [
        ("wifi", 6.0, 10.0, 20.0, 40.0),
        ("gateway", 6.0, 10.0, 20.0, 40.0),
        ("isp", 12.0, 20.0, 40.0, 80.0),
        ("internet", 15.0, 25.0, 50.0, 100.0),
    ];
    tags.len() == legacy.len()
        && tags.iter().zip(legacy.iter()).all(|(t, l)| {
            t.tag == l.0 && t.good == l.1 && t.ok == l.2 && t.poor == l.3 && t.terrible == l.4
        })
}

pub fn save(path: &Path, cfg: &AppConfig) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(cfg)?)?;
    Ok(())
}

fn target(id: &str, label: &str, host: &str, tag: &str) -> Target {
    Target {
        id: id.into(),
        label: label.into(),
        host: host.into(),
        tag: tag.into(),
        interval_ms: 1000,
    }
}

// good / ok / bad(poor) / max(terrible) cutoffs in ms.
fn profile(tag: &str, good: f64, ok: f64, bad: f64, max: f64) -> TagProfile {
    TagProfile {
        tag: tag.into(),
        good,
        ok,
        poor: bad,
        terrible: max,
    }
}

/// Seed matching the user's described layout. All values are starting points; the user
/// edits hosts and thresholds in-app or directly in the TOML.
pub fn default_config() -> AppConfig {
    let tags = vec![
        // gateway follows LAN
        profile("wifi", 10.0, 20.0, 40.0, 100.0),
        profile("gateway", 10.0, 20.0, 40.0, 100.0),
        profile("isp", 25.0, 40.0, 80.0, 150.0),
        profile("internet", 30.0, 50.0, 100.0, 300.0),
    ];

    let targets = vec![
        target("wifi-a", "LAN host A", "192.168.1.2", "wifi"),
        target("wifi-b", "LAN host B", "192.168.1.3", "wifi"),
        target("gw-int", "Router (internal)", "192.168.1.1", "gateway"),
        target("isp-dns1", "ISP resolver 1", "75.75.75.75", "isp"),
        target("isp-dns2", "ISP resolver 2", "75.75.76.76", "isp"),
        target("net-cf", "Cloudflare 1.1.1.1", "1.1.1.1", "internet"),
        target("net-google", "Google 8.8.8.8", "8.8.8.8", "internet"),
        target("net-apple", "apple.com", "apple.com", "internet"),
        target("net-ibm", "ibm.com", "ibm.com", "internet"),
    ];

    AppConfig { targets, tags }
}
