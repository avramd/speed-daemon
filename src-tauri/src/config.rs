use crate::model::{Alias, AppConfig, NodeInfo, TagProfile, ThresholdSet, Target};
use std::path::{Path, PathBuf};
use tauri::Manager;

/// Random lowercase-hex string of `n` bytes (2n chars). Used for node ids and secrets.
pub fn random_hex(n: usize) -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    (0..n).map(|_| format!("{:02x}", rng.random::<u8>())).collect()
}

fn default_name() -> String {
    gethostname::gethostname().to_string_lossy().to_string()
}

/// Fill in a stable identity / default mode the first time (or after an upgrade).
fn ensure_node(cfg: &mut AppConfig) {
    if cfg.node.id.is_empty() {
        cfg.node.id = random_hex(8);
    }
    if cfg.node.name.is_empty() {
        cfg.node.name = default_name();
    }
    if cfg.node.mode.is_empty() {
        cfg.node.mode = "client".into();
    }
}

/// `<data dir>/config.toml`. The data dir is normally the OS app-config dir
/// (~/Library/Application Support/org.est.speeddaemon/), but `SPEED_DAEMON_DIR` overrides
/// it — used so a dev instance keeps its own data and never disturbs the real collector.
pub fn config_path(app: &tauri::AppHandle) -> tauri::Result<PathBuf> {
    if let Ok(dir) = std::env::var("SPEED_DAEMON_DIR") {
        return Ok(PathBuf::from(dir).join("config.toml"));
    }
    Ok(app.path().app_config_dir()?.join("config.toml"))
}

/// The data dir holding `config.toml` and `history.db`, resolved WITHOUT Tauri so `speedd` can
/// find it. `SPEED_DAEMON_DIR` overrides; otherwise it's the macOS app-support dir for our
/// bundle id — the same path Tauri's `app_config_dir()` returns, so the daemon and the GUI
/// share the same files.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SPEED_DAEMON_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("Library/Application Support/org.est.speeddaemon")
}

pub fn load_or_default(path: &Path) -> AppConfig {
    let mut cfg = match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str::<AppConfig>(&s).unwrap_or_else(|_| default_config()),
        Err(_) => default_config(),
    };
    // One-time migration: fold the legacy per-tag profiles into threshold sets.
    if cfg.sets.is_empty() {
        cfg.sets = if cfg.tags.is_empty() {
            built_in_sets()
        } else {
            migrate_tags_to_sets(&cfg.tags)
        };
    }
    cfg.tags.clear(); // the legacy field is never persisted again
    ensure_node(&mut cfg);
    cfg
}

pub fn save(path: &Path, cfg: &AppConfig) -> anyhow::Result<()> {
    // Read-only (dev viewer) mode never writes config — it shares the real instance's files.
    if std::env::var("SPEED_DAEMON_READONLY").is_ok() {
        return Ok(());
    }
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

// Built-in group colors (previously the hashed tag colors we're retiring).
const GREEN: &str = "#5fa85f";
const PINK: &str = "#d2679a";
const CYAN: &str = "#2bb0c4";
const GOLD: &str = "#c9a227";

fn alias(text: &str, color: Option<&str>) -> Alias {
    Alias {
        text: text.into(),
        color: color.map(|c| c.into()),
    }
}

/// The built-in threshold sets (self is added in the self phase). `wifi`/`gateway` share LAN's
/// thresholds but keep their distinct colors as per-alias overrides.
pub fn built_in_sets() -> Vec<ThresholdSet> {
    vec![
        ThresholdSet {
            name: "LAN".into(),
            color: GREEN.into(),
            good: 10.0,
            ok: 20.0,
            poor: 40.0,
            terrible: 100.0,
            aliases: vec![alias("wifi", None), alias("gateway", Some(PINK))],
            builtin: true,
        },
        ThresholdSet {
            name: "ISP".into(),
            color: CYAN.into(),
            good: 25.0,
            ok: 40.0,
            poor: 80.0,
            terrible: 150.0,
            aliases: vec![alias("isp", None)],
            builtin: true,
        },
        ThresholdSet {
            name: "Internet".into(),
            color: GOLD.into(),
            good: 30.0,
            ok: 50.0,
            poor: 100.0,
            terrible: 300.0,
            aliases: vec![alias("internet", None)],
            builtin: true,
        },
    ]
}

/// Fold legacy per-tag profiles into the built-in sets, carrying over any edited thresholds; any
/// tag that isn't one of the four known ones becomes its own custom set.
fn migrate_tags_to_sets(tags: &[TagProfile]) -> Vec<ThresholdSet> {
    let mut sets = built_in_sets();
    let find = |t: &str| tags.iter().find(|p| p.tag == t);
    let apply = |s: &mut ThresholdSet, p: &TagProfile| {
        s.good = p.good;
        s.ok = p.ok;
        s.poor = p.poor;
        s.terrible = p.terrible;
    };
    if let Some(p) = find("wifi").or_else(|| find("gateway")) {
        apply(&mut sets[0], p);
    }
    if let Some(p) = find("isp") {
        apply(&mut sets[1], p);
    }
    if let Some(p) = find("internet") {
        apply(&mut sets[2], p);
    }
    for p in tags {
        if !matches!(p.tag.as_str(), "wifi" | "gateway" | "isp" | "internet") {
            sets.push(ThresholdSet {
                name: p.tag.clone(),
                color: GREEN.into(),
                good: p.good,
                ok: p.ok,
                poor: p.poor,
                terrible: p.terrible,
                aliases: vec![],
                builtin: false,
            });
        }
    }
    sets
}

/// Seed matching the user's described layout. All values are starting points; the user
/// edits hosts and thresholds in-app or directly in the TOML.
pub fn default_config() -> AppConfig {
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

    AppConfig {
        targets,
        sets: built_in_sets(),
        tags: Vec::new(),
        node: NodeInfo::default(),
        peers: Vec::new(),
        theme: "system".into(),
        aggregate: "worst".into(),
    }
}
