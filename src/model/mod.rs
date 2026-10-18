//! Job model shared by the service, the storage backends and the web API.

mod job;
mod request;
pub mod uuid;

pub use job::{HistoryEvent, Job, JobResponse, JobState, events};
pub(crate) use job::{UuidStr, serialize_uuid_map};
pub use request::JobRequest;

use std::time::{SystemTime, UNIX_EPOCH};

/// Current time in Unix milliseconds (Go `time.Now().UnixMilli()`).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
