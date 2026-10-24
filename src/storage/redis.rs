//! Redis backend (`jobbackendprovider/redisprovider/redisprovider.go`): one string per job,
//! key `job:<uuid>`, value = the job JSON (same bytes as Go).
//!
//! Writes go through one ordered queue and are pipelined; the request that caused a write
//! waits for its first attempt. On failure the API answers as Go does (the job exists in
//! memory) while the write is retried in the background and `/readyz` reports 503 until it
//! succeeds. Loading uses `SCAN` + `MGET` (Go: `KEYS` + `GET`, which failed when a key
//! expired in between).

use super::Ticket;
use super::redis_url::{self, Address, RedisOptions};
use crate::config::Config;
use crate::events::ServiceEvent;
use crate::log::{self, Logger};
use crate::model::Job;
use parking_lot::Mutex;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{
    AsyncCommands, ConnectionAddr, IntoConnectionInfo, ProtocolVersion, RedisConnectionInfo,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{OnceCell, mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

const KEY_PREFIX: &str = "job:";
const MAX_BATCH: usize = 1024;

pub fn key(id: Uuid) -> String {
    format!("{KEY_PREFIX}{id}")
}

#[derive(Clone, Debug, PartialEq)]
enum Op {
    Set {
        key: String,
        value: Vec<u8>,
        ttl: u64,
    },
    Del(String),
    DelAll,
}

impl Op {
    fn key(&self) -> Option<&str> {
        match self {
            Op::Set { key, .. } | Op::Del(key) => Some(key),
            Op::DelAll => None,
        }
    }
}

struct Msg {
    op: Op,
    ack: Option<oneshot::Sender<()>>,
}

pub struct RedisBackend {
    opts: RedisOptions,
    max_jobs: u64,
    finished_ttl: u64,
    pub(super) logger: Logger,
    pub(super) log_verbosity: i64,
    conn: OnceCell<ConnectionManager>,
    tx: Mutex<Option<mpsc::UnboundedSender<Msg>>>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<Msg>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
    writes_ok: Arc<AtomicBool>,
}

impl RedisBackend {
    pub fn new(cfg: &Config, logger: Logger) -> Result<RedisBackend, String> {
        let r = &cfg.backend.redis;
        if r.max_jobs == 0 {
            return Err(format!(
                "invalid value for configuration backend.redis.maxJobs: {}",
                r.max_jobs
            ));
        }
        let opts =
            redis_url::parse(&r.url).map_err(|e| format!("error while parsing redis URL: {e}"))?;
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(RedisBackend {
            opts,
            max_jobs: r.max_jobs,
            finished_ttl: r.finished_job_ttl,
            logger,
            log_verbosity: cfg.backend.log_verbosity,
            conn: OnceCell::new(),
            tx: Mutex::new(Some(tx)),
            rx: Mutex::new(Some(rx)),
            writer: Mutex::new(None),
            writes_ok: Arc::new(AtomicBool::new(true)),
        })
    }

    pub fn max_jobs(&self) -> u64 {
        self.max_jobs
    }

    fn connection_info(&self) -> Result<redis::ConnectionInfo, String> {
        let addr = match self.opts.address.clone() {
            Some(Address::Tcp { host, port }) if self.opts.tls => ConnectionAddr::TcpTls {
                host,
                port,
                insecure: self.opts.skip_verify,
                tls_params: None,
            },
            Some(Address::Tcp { host, port }) => ConnectionAddr::Tcp(host, port),
            Some(Address::Unix(p)) => ConnectionAddr::Unix(p.into()),
            None => return Err("redis: missing address".into()),
        };
        let mut settings = RedisConnectionInfo::default().set_db(self.opts.db);
        if !self.opts.username.is_empty() {
            settings = settings.set_username(&self.opts.username);
        }
        if !self.opts.password.is_empty() {
            settings = settings.set_password(&self.opts.password);
        }
        if self.opts.protocol == 3 {
            settings = settings.set_protocol(ProtocolVersion::RESP3);
        }
        Ok(addr
            .into_connection_info()
            .map_err(|e| e.to_string())?
            .set_redis_settings(settings))
    }

    async fn connect(&self) -> Result<ConnectionManager, String> {
        crate::tls::install_provider();
        let info = self.connection_info()?;
        let client = redis::Client::open(info).map_err(|e| e.to_string())?;
        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(self.opts.dial())
            .set_response_timeout(self.opts.read())
            .set_number_of_retries(self.opts.retries());
        let mut conn = ConnectionManager::new_with_config(client, config)
            .await
            .map_err(|e| go_error(&e))?;
        if !self.opts.client_name.is_empty() {
            let _: Result<(), _> = redis::cmd("CLIENT")
                .arg("SETNAME")
                .arg(&self.opts.client_name)
                .query_async(&mut conn)
                .await;
        }
        Ok(conn)
    }

    /// Connects, starts the writer, loads every `job:*` key.
    pub async fn load_jobs(&self) -> Result<Vec<Job>, String> {
        let conn = match self.conn.get_or_try_init(|| self.connect()).await {
            Ok(c) => c.clone(),
            Err(e) => {
                self.logger
                    .error("Error getting job keys from Redis", &[log::err(&e)]);
                return Err("cannot load jobs".to_string());
            }
        };
        // Go enables keyspace notifications and refuses to start if CONFIG is not allowed
        // (managed Redis). Nothing uses them, so a failure is only a warning.
        let r: redis::RedisResult<()> = redis::cmd("CONFIG")
            .arg("SET")
            .arg("notify-keyspace-events")
            .arg("KEA")
            .query_async(&mut conn.clone())
            .await;
        if let Err(e) = r {
            self.logger.warn(
                "Cannot enable Redis keyspace notifications",
                &[
                    log::s("topic", "backendProvider"),
                    log::s("method", "Init"),
                    log::err(go_error(&e)),
                ],
            );
        }
        let jobs = self.scan_jobs(conn.clone()).await?;
        if let Some(rx) = self.rx.lock().take() {
            let logger = self.logger.clone();
            let ok = self.writes_ok.clone();
            *self.writer.lock() = Some(tokio::spawn(writer(conn, rx, logger, ok)));
        }
        Ok(jobs)
    }

    async fn scan_jobs(&self, mut conn: ConnectionManager) -> Result<Vec<Job>, String> {
        let mut keys: Vec<String> = Vec::new();
        let mut cursor: u64 = 0;
        loop {
            let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg("job:*")
                .arg("COUNT")
                .arg(1000)
                .query_async(&mut conn)
                .await
                .map_err(|e| {
                    self.logger.error(
                        "Error getting job keys from Redis",
                        &[log::err(go_error(&e))],
                    );
                    "cannot load jobs".to_string()
                })?;
            keys.extend(batch);
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
        keys.sort();
        keys.dedup();
        let mut jobs = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(500) {
            let values: Vec<Option<Vec<u8>>> = redis::cmd("MGET")
                .arg(chunk)
                .query_async(&mut conn)
                .await
                .map_err(|e| {
                    self.logger
                        .error("Error getting job from Redis", &[log::err(go_error(&e))]);
                    "cannot load job".to_string()
                })?;
            // A key that expired since SCAN is skipped.
            for v in values.into_iter().flatten() {
                let job = Job::decode(&v).map_err(|e| {
                    self.logger
                        .error("Error load job from JSON", &[log::err(&e)]);
                    "cannot load job".to_string()
                })?;
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    fn push(&self, op: Op) -> Option<Ticket> {
        let (ack, ticket) = oneshot::channel();
        let tx = self.tx.lock();
        tx.as_ref()?.send(Msg { op, ack: Some(ack) }).ok()?;
        Some(ticket)
    }

    /// Saves the job on every event that changes it (`SaveJob`), with the TTL for succeeded
    /// jobs (`SetJobTTL`).
    pub fn on_job(&self, job: &Arc<Job>, ev: ServiceEvent) -> Option<Ticket> {
        let ttl = match ev {
            ServiceEvent::JobLoaded | ServiceEvent::JobDeleted => return None,
            ServiceEvent::JobSucceeded => self.finished_ttl,
            _ => 0,
        };
        self.push(Op::Set {
            key: key(job.id),
            value: job.to_json(),
            ttl,
        })
    }

    pub fn on_job_deleted(&self, id: Uuid) -> Option<Ticket> {
        self.push(Op::Del(key(id)))
    }

    pub fn on_all_deleted(&self) -> Option<Ticket> {
        self.push(Op::DelAll)
    }

    pub async fn healthcheck(&self) -> bool {
        let Some(conn) = self.conn.get() else {
            // Not connected yet: assumed healthy, like Go.
            return true;
        };
        let mut conn = conn.clone();
        let ping: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut conn).await;
        match ping {
            Ok(_) => self.writes_ok.load(Ordering::Relaxed),
            Err(e) => {
                self.logger
                    .error("Error pinging Redis", &[log::err(go_error(&e))]);
                false
            }
        }
    }

    /// Closes the queue and waits (at most 5 s) for the pending writes, then stops the writer
    /// (it must not write old values after a restart of the service).
    pub async fn stop(&self) {
        self.tx.lock().take();
        let handle = self.writer.lock().take();
        if let Some(mut h) = handle
            && tokio::time::timeout(Duration::from_secs(5), &mut h)
                .await
                .is_err()
        {
            h.abort();
            self.logger.error(
                "Pending Redis writes not flushed at shutdown",
                &[log::s("topic", "backendProvider"), log::s("method", "Stop")],
            );
        }
    }
}

/// A Redis error worded as go-redis words it: a server error is the reply as sent
/// (`NOPERM User ...`, `OOM command not allowed ...`), and a pipeline fails with the error of
/// its first failed command (redis-rs: `NoPerm: User ...`, `Pipeline failure: [(Index 0,
/// error: ...)]`).
fn go_error(e: &redis::RedisError) -> String {
    let first = e
        .clone()
        .into_server_errors()
        .and_then(|errors| errors.first().map(|(_, err)| err.clone()));
    match first {
        Some(err) => match err.details() {
            Some(detail) => format!("{} {detail}", err.code()),
            None => err.code().to_string(),
        },
        None => e.to_string(),
    }
}

/// Keeps the last write per key, in order (used when retrying after a failure).
fn coalesce(ops: Vec<Op>) -> Vec<Op> {
    let mut out: Vec<Op> = Vec::new();
    for op in ops {
        match &op {
            Op::DelAll => out.clear(),
            _ => {
                let k = op.key().map(str::to_string);
                out.retain(|o| o.key().map(str::to_string) != k || o == &Op::DelAll);
            }
        }
        out.push(op);
    }
    out
}

async fn execute(conn: &mut ConnectionManager, ops: &[Op]) -> redis::RedisResult<()> {
    let mut pipe = redis::pipe();
    let mut queued = 0;
    for op in ops {
        match op {
            Op::Set { key, value, ttl } => {
                let cmd = pipe.cmd("SET").arg(key).arg(value);
                if *ttl > 0 {
                    cmd.arg("EX").arg(*ttl);
                }
                cmd.ignore();
                queued += 1;
            }
            Op::Del(key) => {
                pipe.cmd("DEL").arg(key).ignore();
                queued += 1;
            }
            Op::DelAll => {
                if queued > 0 {
                    pipe.query_async::<()>(conn).await?;
                    pipe = redis::pipe();
                    queued = 0;
                }
                delete_all(conn).await?;
            }
        }
    }
    if queued > 0 {
        pipe.query_async::<()>(conn).await?;
    }
    Ok(())
}

async fn delete_all(conn: &mut ConnectionManager) -> redis::RedisResult<()> {
    let mut cursor: u64 = 0;
    loop {
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg("job:*")
            .arg("COUNT")
            .arg(1000)
            .query_async(conn)
            .await?;
        if !keys.is_empty() {
            let _: () = conn.del(keys).await?;
        }
        cursor = next;
        if cursor == 0 {
            return Ok(());
        }
    }
}

async fn writer(
    mut conn: ConnectionManager,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    logger: Logger,
    ok: Arc<AtomicBool>,
) {
    let mut retry: Vec<Op> = Vec::new();
    let mut backoff = Duration::from_millis(100);
    loop {
        let mut batch: Vec<Msg> = Vec::new();
        if retry.is_empty() {
            match rx.recv().await {
                Some(m) => batch.push(m),
                None => return,
            }
        }
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(m) => batch.push(m),
                Err(_) => break,
            }
        }
        let mut ops = std::mem::take(&mut retry);
        ops.extend(batch.iter().map(|m| m.op.clone()));
        if ops.is_empty() {
            continue;
        }
        let result = execute(&mut conn, &ops).await;
        // The requests waiting for these writes are released after the first attempt.
        for m in batch {
            if let Some(a) = m.ack {
                let _ = a.send(());
            }
        }
        match result {
            Ok(()) => {
                ok.store(true, Ordering::Relaxed);
                backoff = Duration::from_millis(100);
            }
            Err(e) => {
                logger.error(
                    "Error saving job to Redis",
                    &[
                        log::s("topic", "backendProvider"),
                        log::s("method", "SaveJob"),
                        log::err(go_error(&e)),
                    ],
                );
                // Every failure is retried: Redis unreachable, but also a rejected write
                // (out of memory, read-only replica during a failover); the job stays in
                // memory and `/readyz` reports 503 until the write succeeds.
                ok.store(false, Ordering::Relaxed);
                retry = coalesce(ops);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(k: &str, v: &str) -> Op {
        Op::Set {
            key: k.into(),
            value: v.as_bytes().to_vec(),
            ttl: 0,
        }
    }

    #[test]
    fn coalescing_keeps_last_write_per_key() {
        let ops = vec![
            set("a", "1"),
            set("b", "1"),
            set("a", "2"),
            Op::Del("b".into()),
        ];
        assert_eq!(coalesce(ops), vec![set("a", "2"), Op::Del("b".into())]);
        let ops = vec![set("a", "1"), Op::DelAll, set("b", "1")];
        assert_eq!(coalesce(ops), vec![Op::DelAll, set("b", "1")]);
    }
}
