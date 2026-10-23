//! Job completion callbacks (`callback/callback.go`).
//!
//! `POST <callbackUrl>` with `{"jobId","status","timestamp","job"}` when a job reaches a final
//! state (succeeded, failed, canceled). Up to 4 attempts, 2 s apart, 30 s timeout each.
//!
//! Corrections compared to Go: the payload is a snapshot taken at the transition (Go read the
//! job later from another goroutine), restarts do not resend callbacks for completed jobs, and
//! at most 256 callbacks run at the same time.

use crate::gojson::quote;
use crate::gourl;
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
    retry_delay: Duration,
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
            retry_delay: RETRY_DELAY,
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
        let url = &job.callback_url;
        let mut last_err = String::new();
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(self.retry_delay).await;
            }
            if let Err(e) = check_url(url) {
                last_err = e;
                continue;
            }
            let req = self
                .client
                .post(url)
                .header("Content-Type", "application/json")
                .header("User-Agent", "MiniJob-Callback/1.0")
                .body(body.clone());
            match req.send().await {
                Err(e) => last_err = request_failed(url, &cause(&e)),
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

/// Go's checks before sending (`http.NewRequest`, then `http.Client.Do`), with their
/// messages. A URL Go refuses is never sent (reqwest's parser would accept `http:///x` as
/// host `x`).
fn check_url(url: &str) -> Result<(), String> {
    let u = gourl::parse(url).map_err(|e| format!("failed to create request: {e}"))?;
    if u.scheme != "http" && u.scheme != "https" {
        let e = format!("unsupported protocol scheme {}", quote(&u.scheme));
        return Err(request_failed(url, &e));
    }
    if u.host.is_empty() {
        return Err(request_failed(url, "http: no Host in request URL"));
    }
    Ok(())
}

/// The `*url.Error` of `http.Client.Do`: `Post "<url>": <cause>`, the password hidden.
fn request_failed(url: &str, cause: &str) -> String {
    format!(
        "HTTP request failed: Post {}: {cause}",
        quote(&strip_password(url))
    )
}

/// Go's `stripPassword`: `user:***@`.
fn strip_password(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some(at) = rest[..authority_end].rfind('@') else {
        return url.to_string();
    };
    match rest[..at].split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:***{}", &rest[at..]),
        None => url.to_string(),
    }
}

/// Why a request failed: the messages of the error's sources (reqwest's own message only
/// repeats the URL); a timeout reads like Go's.
fn cause(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "context deadline exceeded".to_string();
    }
    let mut parts = Vec::new();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        let m = s.to_string();
        // hyper-util's wrapper (`client error (Connect)`) adds nothing.
        if !m.starts_with("client error (") {
            parts.push(m);
        }
        source = s.source();
    }
    if parts.is_empty() {
        e.to_string()
    } else {
        parts.join(": ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_errors_like_go() {
        assert_eq!(
            check_url("http://[::1").unwrap_err(),
            "failed to create request: parse \"http://[::1\": missing ']' in host"
        );
        assert_eq!(
            check_url("ftp://127.0.0.1/x").unwrap_err(),
            "HTTP request failed: Post \"ftp://127.0.0.1/x\": unsupported protocol scheme \"ftp\""
        );
        assert_eq!(
            check_url("x/y").unwrap_err(),
            "HTTP request failed: Post \"x/y\": unsupported protocol scheme \"\""
        );
        assert_eq!(
            check_url("http:///x").unwrap_err(),
            "HTTP request failed: Post \"http:///x\": http: no Host in request URL"
        );
        assert_eq!(
            check_url("HTTP:opaque").unwrap_err(),
            "HTTP request failed: Post \"HTTP:opaque\": http: no Host in request URL"
        );
        assert!(check_url("https://user:pw@example.com:8443/cb?x=1").is_ok());
        assert_eq!(
            strip_password("https://user:p@ss@example.com/a@b"),
            "https://user:***@example.com/a@b"
        );
        assert_eq!(strip_password("http://user@h/"), "http://user@h/");
        assert_eq!(strip_password("http://h/a:b@c"), "http://h/a:b@c");
    }

    #[tokio::test]
    async fn send_errors() {
        let logger = Logger::nop();
        let mut cb = Callbacks::new(logger).unwrap();
        cb.retry_delay = Duration::from_millis(1);
        // A server that answers 503 then closes.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut requests = 0;
            while let Ok((mut s, _)) = listener.accept().await {
                let mut buf = vec![0; 65536];
                let _ = s.read(&mut buf).await;
                requests += 1;
                let _ = s
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                if requests == 4 {
                    break;
                }
            }
            requests
        });
        let job = |url: String| Job {
            callback_url: url,
            ..Default::default()
        };
        let e = cb
            .send(&job(format!("http://{addr}/cb")))
            .await
            .unwrap_err();
        assert_eq!(
            e,
            "failed to send callback after 4 attempts: callback returned status 503"
        );
        assert_eq!(server.await.unwrap(), 4);
        let e = cb
            .send(&job("http://user:secret@nonexistent.invalid/cb".into()))
            .await
            .unwrap_err();
        assert!(
            e.starts_with(
                "failed to send callback after 4 attempts: HTTP request failed: \
                 Post \"http://user:***@nonexistent.invalid/cb\": dns error: "
            ),
            "{e}"
        );
    }
}
