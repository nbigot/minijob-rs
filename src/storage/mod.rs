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
/// the `notify event` debug log, for the job events the Go providers handle (not a timeout:
/// the port passes it on to save a visibility change).
fn go_event_type(ev: ServiceEvent) -> Option<i64> {
    Some(match ev {
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
        _ => return None,
    })
}

const EVENT_ALL_JOBS_DELETED: i64 = 12;
const EVENT_ALL_RESOURCES_UNLOCKED: i64 = 13;
const EVENT_RESOURCE_UNLOCKED: i64 = 14;

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

/// The `notify event` debug log of the Go providers (`NotifyChange`), with the members of
/// `jobbackendprovider.Event` in their Go order.
fn debug_event(logger: &Logger, event_type: i64, id: Uuid, resource: Option<&str>) {
    if logger.enabled(log::Level::Debug) {
        let resource = match resource {
            Some(r) => String::from_utf8(crate::gojson::to_vec_no_html(&r)).unwrap_or_default(),
            None => "null".to_string(),
        };
        let event = log::Field {
            key: "event".to_string(),
            value: log::FieldValue::Json(format!(
                "{{\"Type\":{event_type},\"JobUUID\":\"{id}\",\"Resource\":{resource}}}"
            )),
        };
        logger.debug(
            "notify event",
            &[
                log::s("topic", "backendProvider"),
                log::s("method", "NotifyChange"),
                event,
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

    /// The logger of the `notify event` debug lines, when the provider writes them: Go's
    /// inMemory and Redis providers from `backend.logVerbosity` 2, the mock one always (its
    /// verbosity is 4).
    fn notify_logger(&self) -> Option<&Logger> {
        match self {
            Backend::InMemory(m) => (m.log_verbosity >= 2).then_some(&m.logger),
            Backend::Mock(m) => Some(&m.logger),
            #[cfg(feature = "redis")]
            Backend::Redis(r) => (r.log_verbosity >= 2).then_some(&r.logger),
        }
    }

    /// The `notify event` line of a job event; Go's Redis provider only notifies the
    /// resource events.
    fn notify_job(&self, event_type: i64, id: Uuid) {
        #[cfg(feature = "redis")]
        if let Backend::Redis(_) = self {
            return;
        }
        if let Some(logger) = self.notify_logger() {
            debug_event(logger, event_type, id, None);
        }
    }

    /// A job changed (called under the service lock).
    pub fn on_job(&self, job: &Arc<Job>, ev: ServiceEvent) -> Option<Ticket> {
        if let Some(t) = go_event_type(ev) {
            self.notify_job(t, job.id);
        }
        match self {
            #[cfg(feature = "redis")]
            Backend::Redis(r) => r.on_job(job, ev),
            _ => None,
        }
    }

    /// A job was deleted (called under the service lock).
    pub fn on_job_deleted(&self, id: Uuid) -> Option<Ticket> {
        if let Some(t) = go_event_type(ServiceEvent::JobDeleted) {
            self.notify_job(t, id);
        }
        match self {
            #[cfg(feature = "redis")]
            Backend::Redis(r) => r.on_job_deleted(id),
            _ => None,
        }
    }

    /// All jobs were deleted (called under the service lock).
    pub fn on_all_deleted(&self) -> Option<Ticket> {
        self.notify_job(EVENT_ALL_JOBS_DELETED, Uuid::nil());
        match self {
            #[cfg(feature = "redis")]
            Backend::Redis(r) => r.on_all_deleted(),
            _ => None,
        }
    }

    /// A job released one of its lock resources (`OnResourceUnlocked`, called for each
    /// resource of the job, like Go).
    pub fn on_resource_unlocked(&self, id: Uuid, resource: &str) {
        if let Some(logger) = self.notify_logger() {
            debug_event(logger, EVENT_RESOURCE_UNLOCKED, id, Some(resource));
        }
    }

    /// The admin API unlocked all resources (`OnAllResourcesUnlocked`).
    pub fn on_all_resources_unlocked(&self) {
        if let Some(logger) = self.notify_logger() {
            debug_event(logger, EVENT_ALL_RESOURCES_UNLOCKED, Uuid::nil(), None);
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
