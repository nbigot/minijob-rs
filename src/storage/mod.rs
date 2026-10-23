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

enum Provider {
    InMemory(InMemory),
    Mock(Mock),
    #[cfg(feature = "redis")]
    Redis(Box<redis::RedisBackend>),
}

pub struct Backend {
    provider: Provider,
    /// The service logger when `jobs.logVerbosity` is 3 or more: Go's service logs each
    /// notification it receives from the provider (`Event received`).
    received: Option<Logger>,
}

/// `jobbackendprovider.Event` as zap reflects it (members in their Go order).
fn event_json(event_type: i64, id: Uuid, resource: Option<&str>) -> String {
    let resource = match resource {
        Some(r) => String::from_utf8(crate::gojson::to_vec_no_html(&r)).unwrap_or_default(),
        None => "null".to_string(),
    };
    format!("{{\"Type\":{event_type},\"JobUUID\":\"{id}\",\"Resource\":{resource}}}")
}

fn json_field(key: &str, json: &str) -> log::Field {
    log::Field {
        key: key.to_string(),
        value: log::FieldValue::Json(json.to_string()),
    }
}

impl Backend {
    /// Creates the backend selected by `backend.type` (`registry.NewJobBackendProvider`), with
    /// Go's checks and error messages; `logger` is the service logger.
    pub fn create(cfg: &Config, logger: &Logger) -> Result<Backend, String> {
        Ok(Backend {
            provider: Provider::create(cfg)?,
            received: (cfg.jobs.log_verbosity > 2).then(|| logger.clone()),
        })
    }
}

impl Provider {
    fn create(cfg: &Config) -> Result<Provider, String> {
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
            "mock" => Ok(Provider::Mock(Mock { logger })),
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
                Ok(Provider::InMemory(InMemory {
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
            _ => Ok(Provider::Redis(Box::new(redis::RedisBackend::new(
                cfg, logger,
            )?))),
            #[cfg(not(feature = "redis"))]
            _ => unreachable!("checked above"),
        }
    }
}

impl Backend {
    /// `GetType`, shown by `/api/v1/system/resources`.
    pub fn kind(&self) -> &'static str {
        match &self.provider {
            Provider::InMemory(_) => "InMemory",
            Provider::Mock(_) => "Mock",
            #[cfg(feature = "redis")]
            Provider::Redis(_) => "Redis",
        }
    }

    /// Maximum number of jobs the backend accepts (Go only logged it for inMemory and never
    /// checked it for Redis; the port refuses the creation).
    pub fn max_jobs(&self) -> Option<u64> {
        match &self.provider {
            Provider::InMemory(m) => Some(m.max_jobs),
            Provider::Mock(_) => None,
            #[cfg(feature = "redis")]
            Provider::Redis(r) => Some(r.max_jobs()),
        }
    }

    /// Connects and loads the stored jobs (`Init` + `LoadJobs`).
    pub async fn load_jobs(&self) -> Result<Vec<Job>, String> {
        match &self.provider {
            Provider::InMemory(m) => match &m.file {
                Some(f) => f.load().await,
                None => Ok(Vec::new()),
            },
            Provider::Mock(_) => Ok(Vec::new()),
            #[cfg(feature = "redis")]
            Provider::Redis(r) => r.load_jobs().await,
        }
    }

    /// The logger of the `notify event` debug lines, when the provider writes them: Go's
    /// inMemory and Redis providers from `backend.logVerbosity` 2, the mock one always (its
    /// verbosity is 4).
    fn notify_logger(&self) -> Option<&Logger> {
        match &self.provider {
            Provider::InMemory(m) => (m.log_verbosity >= 2).then_some(&m.logger),
            Provider::Mock(m) => Some(&m.logger),
            #[cfg(feature = "redis")]
            Provider::Redis(r) => (r.log_verbosity >= 2).then_some(&r.logger),
        }
    }

    /// A notification of the Go provider (`NotifyChange`): its `notify event` debug line,
    /// then the `Event received` line of the service.
    fn notify(&self, event_type: i64, id: Uuid, resource: Option<&str>) {
        let logger = self
            .notify_logger()
            .filter(|l| l.enabled(log::Level::Debug));
        if logger.is_none() && self.received.is_none() {
            return;
        }
        let event = event_json(event_type, id, resource);
        if let Some(l) = logger {
            l.debug(
                "notify event",
                &[
                    log::s("topic", "backendProvider"),
                    log::s("method", "NotifyChange"),
                    json_field("event", &event),
                ],
            );
        }
        if let Some(l) = &self.received {
            l.info(
                "Event received",
                &[
                    log::s("topic", "job"),
                    log::s("method", "Run"),
                    json_field("eventType", &event),
                ],
            );
        }
    }

    /// The notification of a job event; Go's Redis provider only notifies the resource
    /// events.
    fn notify_job(&self, event_type: i64, id: Uuid) {
        #[cfg(feature = "redis")]
        if let Provider::Redis(_) = &self.provider {
            return;
        }
        self.notify(event_type, id, None);
    }

    /// A job changed (called under the service lock).
    pub fn on_job(&self, job: &Arc<Job>, ev: ServiceEvent) -> Option<Ticket> {
        if let Some(t) = go_event_type(ev) {
            self.notify_job(t, job.id);
        }
        match &self.provider {
            #[cfg(feature = "redis")]
            Provider::Redis(r) => r.on_job(job, ev),
            _ => None,
        }
    }

    /// A job was deleted (called under the service lock).
    pub fn on_job_deleted(&self, id: Uuid) -> Option<Ticket> {
        if let Some(t) = go_event_type(ServiceEvent::JobDeleted) {
            self.notify_job(t, id);
        }
        match &self.provider {
            #[cfg(feature = "redis")]
            Provider::Redis(r) => r.on_job_deleted(id),
            _ => None,
        }
    }

    /// All jobs were deleted (called under the service lock).
    pub fn on_all_deleted(&self) -> Option<Ticket> {
        self.notify_job(EVENT_ALL_JOBS_DELETED, Uuid::nil());
        match &self.provider {
            #[cfg(feature = "redis")]
            Provider::Redis(r) => r.on_all_deleted(),
            _ => None,
        }
    }

    /// A job released one of its lock resources (`OnResourceUnlocked`, called for each
    /// resource of the job, like Go).
    pub fn on_resource_unlocked(&self, id: Uuid, resource: &str) {
        self.notify(EVENT_RESOURCE_UNLOCKED, id, Some(resource));
    }

    /// The admin API unlocked all resources (`OnAllResourcesUnlocked`).
    pub fn on_all_resources_unlocked(&self) {
        self.notify(EVENT_ALL_RESOURCES_UNLOCKED, Uuid::nil(), None);
    }

    /// Period of the snapshot saves, for backends that save snapshots.
    pub fn snapshot_interval(&self) -> Option<Duration> {
        match &self.provider {
            Provider::InMemory(m) if m.file.is_some() => Some(m.write_frequency),
            _ => None,
        }
    }

    /// Saves a snapshot of all jobs.
    pub async fn save_snapshot(&self, jobs: Vec<Arc<Job>>) -> Result<(), String> {
        match &self.provider {
            Provider::InMemory(InMemory { file: Some(f), .. }) => f.save(jobs).await,
            _ => Ok(()),
        }
    }

    pub async fn healthcheck(&self) -> bool {
        match &self.provider {
            #[cfg(feature = "redis")]
            Provider::Redis(r) => r.healthcheck().await,
            _ => true,
        }
    }

    /// Size of the storage file in bytes (`GetDiskUsage`).
    pub fn disk_usage(&self) -> u64 {
        match &self.provider {
            Provider::InMemory(InMemory { file: Some(f), .. }) => f.disk_usage(),
            _ => 0,
        }
    }

    /// Flushes pending writes and closes connections.
    pub async fn stop(&self) {
        #[cfg(feature = "redis")]
        if let Provider::Redis(r) = &self.provider {
            r.stop().await;
        }
    }
}
