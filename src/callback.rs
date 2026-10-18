//! Job completion callbacks (`callback/callback.go`).
//!
//! `POST <callbackUrl>` with `{"jobId","status","timestamp","job"}` when a job reaches a final
//! state (succeeded, failed, canceled). Up to 4 attempts, 2 s apart, 30 s timeout each.
//!
//! Corrections compared to Go: the payload is a snapshot taken at the transition (Go read the
//! job later from another goroutine), restarts do not resend callbacks for completed jobs, and
//! at most 256 callbacks run at the same time.

use crate::log::{self, Logger};
use crate::model::Job;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

const MAX_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONCURRENT: usize = 256;

#[derive(Serialize)]
struct Payload<'a> {
    #[serde(rename = "jobId")]
    job_id: String,
    status: &'a str,
    timestamp: i64,
    job: &'a Job,
}

#[derive(Clone)]
pub struct Callbacks {
    client: reqwest::Client,
    permits: Arc<Semaphore>,
    logger: Logger,
}

impl Callbacks {
    pub fn new(logger: Logger) -> Result<Callbacks, String> {
        crate::tls::install_provider();
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Callbacks {
            client,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            logger,
        })
    }

    /// Sends the callback of `job` in the background.
    pub fn trigger(&self, job: Arc<Job>) {
        if job.callback_url.is_empty() {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let Ok(_permit) = this.permits.clone().acquire_owned().await else {
                return;
            };
            let id = job.id.to_string();
            match this.send(&job).await {
                Ok(()) => this
                    .logger
                    .info("Callback successfully sent", &[log::s("jobId", id)]),
                Err(e) => this
                    .logger
                    .error("Callback failed", &[log::s("jobId", id), log::err(e)]),
            }
        });
    }

    async fn send(&self, job: &Job) -> Result<(), String> {
        let body = crate::gojson::to_vec(&Payload {
            job_id: job.id.to_string(),
            status: job.state().as_str(),
            timestamp: job.last_update(),
            job,
        });
        let mut last_err = String::new();
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            let req = self
                .client
                .post(&job.callback_url)
                .header("Content-Type", "application/json")
                .header("User-Agent", "MiniJob-Callback/1.0")
                .body(body.clone());
            match req.send().await {
                Err(e) if e.is_builder() => last_err = format!("failed to create request: {e}"),
                Err(e) => last_err = format!("HTTP request failed: {e}"),
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if (200..300).contains(&status) {
                        return Ok(());
                    }
                    last_err = format!("callback returned status {status}");
                }
            }
        }
        Err(format!(
            "failed to send callback after {} attempts: {last_err}",
            MAX_RETRIES + 1
        ))
    }
}
