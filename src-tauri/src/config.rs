use crate::model::{AppConfig, TagProfile, Target};
use std::path::{Path, PathBuf};
use tauri::Manager;

/// `<app config dir>/config.toml` — e.g. ~/Library/Application Support/org.est.speeddaemon/
pub fn config_path(app: &tauri::AppHandle) -> tauri::Result<PathBuf> {
    Ok(app.path().app_config_dir()?.join("config.toml"))
}

pub fn load_or_default(path: &Path) -> AppConfig {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).unwrap_or_else(|_| default_config()),
        Err(_) => default_config(),
    }
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

fn profile(tag: &str, mult: f64) -> TagProfile {
    TagProfile {
        tag: tag.into(),
        good: 6.0 * mult,
        ok: 10.0 * mult,
        poor: 20.0 * mult,
        terrible: 40.0 * mult,
    }
}

/// Seed matching the user's described layout. All values are starting points; the user
/// edits hosts and thresholds in-app or directly in the TOML.
pub fn default_config() -> AppConfig {
    let tags = vec![
        profile("wifi", 1.0),
        profile("gateway", 1.0),
        profile("isp", 2.0),
        profile("internet", 2.5),
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
