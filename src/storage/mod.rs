//! Storage backends (`jobbackendprovider/*`): `inMemory` (optionally persisted to a JSON
//! file), `redis`, `mock`.
//!
//! The service keeps the authoritative state in memory; a backend mirrors it. Notifications
//! are called while the service holds its state lock and never block: Redis writes are queued
//! and the request handler awaits the returned [`Ticket`] after releasing the lock.

mod file;
#[cfg(feature = "redis")]
pub mod redis;
#[cfg(feature = "redis")]
pub mod redis_url;

use crate::config::Config;
use crate::events::ServiceEvent;
use crate::log::{self, Logger};
use crate::model::Job;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;

/// Completion of a queued storage write.
pub type Ticket = oneshot::Receiver<()>;

/// Event type numbers of the Go backend notifications (`jobbackendprovider.Event`), used in
/// the `notify event` debug log.
fn go_event_type(ev: ServiceEvent) -> i64 {
    match ev {
        ServiceEvent::JobCreated => 0,
        ServiceEvent::JobDelayed => 1,
        ServiceEvent::JobPending => 2,
        ServiceEvent::JobEnqueued => 3,
        ServiceEvent::JobDeleted => 4,
        ServiceEvent::JobStarted => 5,
        ServiceEvent::JobSucceeded => 6,
        ServiceEvent::JobCanceled => 7,
        ServiceEvent::JobFailed => 8,
        ServiceEvent::JobHidden => 9,
        ServiceEvent::JobTerminated => 10,
        ServiceEvent::JobTimeout => 11,
        ServiceEvent::JobDeletedAll => 12,
        ServiceEvent::JobUnlockedAllResources => 13,
        _ => 15,
    }
}

/// `inMemory` backend: nothing to do per event; the service saves snapshots periodically.
pub struct InMemory {
    max_jobs: u64,
    write_frequency: Duration,
    file: Option<file::JobsFile>,
    logger: Logger,
    log_verbosity: i64,
}

/// `mock` backend (hard-coded settings of the Go mock provider).
pub struct Mock {
    logger: Logger,
}

pub enum Backend {
    InMemory(InMemory),
    Mock(Mock),
    #[cfg(feature = "redis")]
    Redis(Box<redis::RedisBackend>),
}

fn debug_event(logger: &Logger, ev: ServiceEvent, id: Uuid) {
    if logger.enabled(log::Level::Debug) {
        let event = serde_json::json!({
            "Type": go_event_type(ev),
            "JobUUID": id.to_string(),
            "Resource": null,
        });
        logger.debug(
            "notify event",
            &[
                log::s("topic", "backendProvider"),
                log::s("method", "NotifyChange"),
                log::any("event", &event),
            ],
        );
    }
}

impl Backend {
    /// Creates the backend selected by `backend.type` (`registry.NewJobBackendProvider`), with
    /// Go's checks and error messages.
    pub fn create(cfg: &Config) -> Result<Backend, String> {
        let kind = cfg.backend.kind.as_str();
        if !matches!(kind, "mock" | "inMemory" | "redis") {
            return Err(format!(
                "error creating job backend provider. No such provider type exists: '{kind}'"
            ));
        }
        #[cfg(not(feature = "redis"))]
        if kind == "redis" {
            return Err(
                "error creating job backend provider. No such provider type exists: 'redis' \
                 (this binary was built without the redis feature)"
                    .to_string(),
            );
        }
        let logger = Logger::build(&cfg.backend.logger)?;
        match kind {
            "mock" => Ok(Backend::Mock(Mock { logger })),
            "inMemory" => {
                let m = &cfg.backend.in_memory;
                if m.max_jobs == 0 {
                    return Err(format!(
                        "invalid value for configuration backend.inMemory.maxJobs: {}",
                        m.max_jobs
                    ));
                }
                let secs = match m.write_frequency {
                    0 => 300,
                    n if n < 0 => {
                        return Err(format!(
                            "invalid value for configuration backend.inMemory.writeFrequency: {n}"
                        ));
                    }
                    n => n as u64,
                };
                Ok(Backend::InMemory(InMemory {
                    max_jobs: m.max_jobs,
                    write_frequency: Duration::from_secs(secs),
                    file: m
                        .enable_persistent_storage
                        .then(|| file::JobsFile::new(&m.directory, &m.filename, logger.clone())),
                    logger,
                    log_verbosity: cfg.backend.log_verbosity,
                }))
            }
            #[cfg(feature = "redis")]
            _ => Ok(Backend::Redis(Box::new(redis::RedisBackend::new(
                cfg, logger,
            )?))),
            #[cfg(not(feature = "redis"))]
            _ => unreachable!("checked above"),
        }
    }

    /// `GetType`, shown by `/api/v1/system/resources`.
    pub fn kind(&self) -> &'static str {
        match self {
            Backend::InMemory(_) => "InMemory",
            Backend::Mock(_) => "Mock",
            #[cfg(feature = "redis")]
            Backend::Redis(_) => "Redis",
        }
    }

    /// Maximum number of jobs the backend accepts (Go only logged it for inMemory and never
    /// checked it for Redis; the port refuses the creation).
    pub fn max_jobs(&self) -> Option<u64> {
        match self {
            Backend::InMemory(m) => Some(m.max_jobs),
            Backend::Mock(_) => None,
            #[cfg(feature = "redis")]
            Backend::Redis(r) => Some(r.max_jobs()),
        }
    }

    /// Connects and loads the stored jobs (`Init` + `LoadJobs`).
    pub async fn load_jobs(&self) -> Result<Vec<Job>, String> {
        match self {
            Backend::InMemory(m) => match &m.file {
                Some(f) => f.load().await,
                None => Ok(Vec::new()),
            },
            Backend::Mock(_) => Ok(Vec::new()),
            #[cfg(feature = "redis")]
            Backend::Redis(r) => r.load_jobs().await,
        }
    }

    /// A job changed (called under the service lock).
    pub fn on_job(&self, job: &Arc<Job>, ev: ServiceEvent) -> Option<Ticket> {
        match self {
            Backend::InMemory(m) => {
                if m.log_verbosity >= 2 {
                    debug_event(&m.logger, ev, job.id);
                }
                None
            }
            Backend::Mock(m) => {
                debug_event(&m.logger, ev, job.id);
                None
            }
            #[cfg(feature = "redis")]
            Backend::Redis(r) => r.on_job(job, ev),
        }
    }

    /// A job was deleted (called under the service lock).
    pub fn on_job_deleted(&self, id: Uuid) -> Option<Ticket> {
        match self {
            Backend::InMemory(m) => {
                if m.log_verbosity >= 2 {
                    debug_event(&m.logger, ServiceEvent::JobDeleted, id);
                }
                None
            }
            Backend::Mock(m) => {
                debug_event(&m.logger, ServiceEvent::JobDeleted, id);
                None
            }
            #[cfg(feature = "redis")]
            Backend::Redis(r) => r.on_job_deleted(id),
        }
    }

    /// All jobs were deleted (called under the service lock).
    pub fn on_all_deleted(&self) -> Option<Ticket> {
        match self {
            Backend::InMemory(m) => {
                if m.log_verbosity >= 2 {
                    debug_event(&m.logger, ServiceEvent::JobDeletedAll, Uuid::nil());
                }
                None
            }
            Backend::Mock(m) => {
                debug_event(&m.logger, ServiceEvent::JobDeletedAll, Uuid::nil());
                None
            }
            #[cfg(feature = "redis")]
            Backend::Redis(r) => r.on_all_deleted(),
        }
    }

    /// Period of the snapshot saves, for backends that save snapshots.
    pub fn snapshot_interval(&self) -> Option<Duration> {
        match self {
            Backend::InMemory(m) if m.file.is_some() => Some(m.write_frequency),
            _ => None,
        }
    }

    /// Saves a snapshot of all jobs.
    pub async fn save_snapshot(&self, jobs: Vec<Arc<Job>>) -> Result<(), String> {
        match self {
            Backend::InMemory(InMemory { file: Some(f), .. }) => f.save(jobs).await,
            _ => Ok(()),
        }
    }

    pub async fn healthcheck(&self) -> bool {
        match self {
            #[cfg(feature = "redis")]
            Backend::Redis(r) => r.healthcheck().await,
            _ => true,
        }
    }

    /// Size of the storage file in bytes (`GetDiskUsage`).
    pub fn disk_usage(&self) -> u64 {
        match self {
            Backend::InMemory(InMemory { file: Some(f), .. }) => f.disk_usage(),
            _ => 0,
        }
    }

    /// Flushes pending writes and closes connections.
    pub async fn stop(&self) {
        #[cfg(feature = "redis")]
        if let Backend::Redis(r) = self {
            r.stop().await;
        }
    }
}
