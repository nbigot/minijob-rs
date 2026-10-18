//! Result file format, shared with the Go version of the tool (same field names and units).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RunResult {
    pub tool: String,
    /// Language of the load generator that wrote the file ("rust" or "go").
    pub implementation: String,
    pub tool_version: String,
    pub label: String,
    pub url: String,
    pub started: String,
    pub concurrency: usize,
    pub duration_s: u64,
    pub warmup_s: u64,
    pub server: ServerInfo,
    pub scenarios: Vec<ScenarioResult>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ServerInfo {
    pub version: String,
    pub pid: u32,
    /// Resident memory before the first scenario (kB), when the process is visible.
    pub idle_rss_kb: Option<u64>,
    /// Highest resident memory of the process at the end (VmHWM, kB).
    pub peak_rss_kb: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ScenarioResult {
    pub name: String,
    /// HTTP requests sent during the measured window.
    pub requests: u64,
    /// Requests that failed (connection error or unexpected status).
    pub errors: u64,
    pub elapsed_s: f64,
    pub requests_per_s: f64,
    /// Scenario units: jobs for `pull-batch`, cycles for `cycle`, requests otherwise.
    pub items: u64,
    pub items_per_s: f64,
    /// Latency of one item (one request, or one whole cycle for `cycle`).
    pub latency_ms: Latency,
    pub statuses: BTreeMap<String, u64>,
    pub server_cpu_us_per_request: Option<f64>,
    pub server_rss_kb: Option<u64>,
    /// CPU used by the load generator itself, in % of one CPU (Linux): close to the number
    /// of CPUs it may use means the load generator, not the server, was the limit.
    pub client_cpu_percent: Option<f64>,
    pub check: Option<Check>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Latency {
    pub mean: f64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Check {
    pub ok: bool,
    pub detail: String,
}
