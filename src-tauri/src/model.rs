use serde::{Deserialize, Serialize};

/// Expectation thresholds (in milliseconds). A sample's color band is the first threshold it
/// falls under: <= good -> green, <= ok -> yellow, <= poor -> orange, <= terrible -> red (which
/// is also the graph ceiling). Kept as the legacy per-tag shape for config migration; the live
/// model is `ThresholdSet` below.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagProfile {
    pub tag: String,
    pub good: f64,
    pub ok: f64,
    pub poor: f64,
    pub terrible: f64,
}

/// One tag (alias) within a threshold set. `color` None means it inherits the set's group color.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alias {
    pub text: String,
    #[serde(default)]
    pub color: Option<String>,
}

/// A named threshold set: shared expectation thresholds + a group color, plus a list of alias
/// tags (each optionally recolored). The `name` is itself a usable tag (colored by `color`). A
/// target's `tag` must match a set's name or one of its aliases. `builtin` sets can't be deleted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdSet {
    pub name: String,
    pub color: String,
    pub good: f64,
    pub ok: f64,
    pub poor: f64,
    pub terrible: f64,
    #[serde(default)]
    pub aliases: Vec<Alias>,
    #[serde(default)]
    pub builtin: bool,
}

/// A single probe destination.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub id: String,
    pub label: String,
    /// Domain name or IP literal.
    pub host: String,
    /// References a `TagProfile.tag`.
    pub tag: String,
    #[serde(default = "default_interval")]
    pub interval_ms: u64,
}

fn default_interval() -> u64 {
    1000
}

/// This app's network identity + role.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub id: String,
    pub name: String,
    /// "client" (default) or "server".
    pub mode: String,
}

impl Default for NodeInfo {
    fn default() -> Self {
        NodeInfo {
            id: String::new(),
            name: String::new(),
            mode: "client".into(),
        }
    }
}

/// A paired peer and the shared secret negotiated at accept time. `role` is the peer's
/// role relative to us: "server" = it directs us; "client" = we direct it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub secret: String,
    pub role: String,
    #[serde(default)]
    pub addr: Option<String>,
}

fn default_theme() -> String {
    "system".into()
}

fn default_aggregate() -> String {
    "worst".into()
}

/// Whole persisted configuration: probe targets + threshold sets + networking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub targets: Vec<Target>,
    /// Threshold sets (the live model). Serialized going forward.
    #[serde(default)]
    pub sets: Vec<ThresholdSet>,
    /// Legacy per-tag profiles — read only to migrate old configs into `sets`, never written.
    #[serde(default, skip_serializing)]
    pub tags: Vec<TagProfile>,
    #[serde(default)]
    pub node: NodeInfo,
    #[serde(default)]
    pub peers: Vec<Peer>,
    /// "system" (default), "light", or "dark".
    #[serde(default = "default_theme")]
    pub theme: String,
    /// How each pixel-bucket reduces the samples it covers: "worst" (default), "trimmed",
    /// "mean", "median", or "best".
    #[serde(default = "default_aggregate")]
    pub aggregate: String,
}

/// One measurement. `rtt_ms == None` means no response (loss) — rendered as a gap.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    /// Epoch milliseconds when the sample was recorded.
    pub ts: u64,
    pub rtt_ms: Option<f64>,
}

/// Pushed to the frontend over the `sample` event as each probe completes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SampleEvent {
    pub target_id: String,
    pub resolved_ip: Option<String>,
    pub sample: Sample,
}

/// One aggregated time slice of history, produced by `get_window`. Each bucket maps to a
/// single pixel column. `val` is the current aggregate's value (the drawn bar); `worst`/`mean`/
/// `best` are the max/avg/min RTT among successful probes in the slice (each None if none
/// succeeded), always provided so the renderer can dot the non-current aggregates. `loss` is the
/// fraction of attempts lost; `count` is attempts; `up` is whether the daemon was running for any
/// part of the slice (false -> render grey).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bucket {
    pub t: u64,
    pub val: Option<f64>,
    pub worst: Option<f64>,
    pub mean: Option<f64>,
    pub best: Option<f64>,
    pub loss: f64,
    pub count: u32,
    pub up: bool,
}

/// Per-sample summary over a window: mean RTT, jitter (std dev of RTT), loss %.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowStats {
    pub avg: Option<f64>,
    pub jitter: Option<f64>,
    pub loss_pct: f64,
    pub count: u32,
}
