use serde::{Deserialize, Serialize};

/// Expectation thresholds (in milliseconds) for a tag. A sample's color band is the
/// first threshold it falls under: <= good -> green, <= ok -> yellow, <= poor -> orange,
/// <= terrible -> red, and anything greater stays red (saturated). These also drive the
/// log-height scaling on the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagProfile {
    pub tag: String,
    pub good: f64,
    pub ok: f64,
    pub poor: f64,
    pub terrible: f64,
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

/// Whole persisted configuration: probe targets + tag expectation profiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub targets: Vec<Target>,
    pub tags: Vec<TagProfile>,
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
/// single pixel column. `worst` is the max RTT among successful probes in the slice (None
/// if none succeeded); `loss` is the fraction of attempts lost; `count` is attempts; `up`
/// is whether the daemon was running for any part of the slice (false -> render grey).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bucket {
    pub t: u64,
    pub worst: Option<f64>,
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
