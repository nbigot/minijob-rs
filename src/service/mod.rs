//! The job service (`service/service.go`): job life cycle, scheduling, resource locks,
//! retention, visibility timeouts.
//!
//! All scheduler state lives behind one lock and every transition happens under it, which
//! removes the data races of the Go version (B03, B04). Observers (metrics, event log,
//! callbacks, storage) are notified under the lock without blocking; storage writes are
//! awaited after the lock is released.

pub mod query;
pub mod queue;

use crate::apierror::{ApiError, codes};
use crate::callback::Callbacks;
use crate::config::Config;
use crate::eventlog::EventLog;
use crate::events::ServiceEvent;
use crate::log::{self, Field, Logger};
use crate::metrics::ServiceMetrics;
use crate::model::{Job, JobRequest, JobState, events, now_ms};
use crate::storage::{Backend, Ticket};
use parking_lot::{Mutex, RwLock};
use query::{GetJobsRequest, GetJobsResponse};
use queue::{OrderKey, TimeQueue};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Parameters of `POST /api/v1/job/pull` (`RequestPullJobs`).
#[derive(Clone, Debug, Default)]
pub struct PullRequest {
    pub job_uuid: Option<Uuid>,
    /// `*` = any topic; empty = jobs without topic.
    pub topic: String,
    pub num_jobs: u64,
    /// Seconds.
    pub visibility_timeout: u64,
    /// Long-poll duration in seconds.
    pub wait_time_seconds: u64,
}

#[derive(Default)]
struct State {
    jobs: HashMap<Uuid, Arc<Job>>,
    pending: BTreeSet<OrderKey>,
    queued: BTreeSet<OrderKey>,
    queued_by_topic: HashMap<String, BTreeSet<OrderKey>>,
    running: HashSet<Uuid>,
    retry: TimeQueue,
    delayed: TimeQueue,
    /// resource → owning job
    locks: HashMap<String, Uuid>,
    /// Incremented on every change; the snapshot saver compares it.
    generation: u64,
}

impl State {
    fn index_out(&mut self, state: JobState, key: OrderKey, topic: &str) {
        match state {
            JobState::Queued => {
                self.queued.remove(&key);
                if let Some(set) = self.queued_by_topic.get_mut(topic) {
                    set.remove(&key);
                    if set.is_empty() {
                        self.queued_by_topic.remove(topic);
                    }
                }
            }
            JobState::Running => {
                self.running.remove(&key.id);
            }
            _ => {}
        }
    }

    fn index_in(&mut self, state: JobState, key: OrderKey, topic: &str) {
        match state {
            JobState::Queued => {
                self.queued.insert(key);
                self.queued_by_topic
                    .entry(topic.to_string())
                    .or_default()
                    .insert(key);
            }
            JobState::Running => {
                self.running.insert(key.id);
            }
            _ => {}
        }
    }

    /// Appends an event to a job, keeps the indexes in sync, returns the new snapshot.
    fn event(&mut self, id: Uuid, event_type: &str, ts: i64) -> Arc<Job> {
        let entry = self.jobs.get_mut(&id).expect("job present");
        let old = entry.state();
        let key = OrderKey::of(entry);
        let topic = entry.topic.clone();
        Arc::make_mut(entry).add_event(event_type, ts);
        let snapshot = entry.clone();
        self.index_out(old, key, &topic);
        self.index_in(snapshot.state(), key, &topic);
        self.generation += 1;
        snapshot
    }

    fn insert(&mut self, job: Arc<Job>) {
        let key = OrderKey::of(&job);
        self.index_in(job.state(), key, &job.topic);
        self.jobs.insert(job.id, job);
        self.generation += 1;
    }

    /// Locks the job's resources if they are free or already its own.
    /// Returns the number of locks acquired, or the first unavailable resource.
    fn lock_resources(&mut self, job: &Job) -> Result<i64, String> {
        for r in job.resources() {
            if let Some(owner) = self.locks.get(r)
                && *owner != job.id
            {
                return Err(format!("resource not available: {r}"));
            }
        }
        let mut acquired = 0;
        for r in job.resources() {
            if self.locks.insert(r.clone(), job.id).is_none() {
                acquired += 1;
            }
        }
        Ok(acquired)
    }

    /// Releases the locks owned by the job; returns how many were released.
    fn unlock_resources(&mut self, job: &Job) -> i64 {
        let mut released = 0;
        for r in job.resources() {
            if self.locks.get(r) == Some(&job.id) {
                self.locks.remove(r);
                released += 1;
            }
        }
        released
    }
}

/// Side effects collected under the lock, applied after it is released.
#[derive(Default)]
struct Fx {
    tickets: Vec<Ticket>,
    wake: bool,
}

impl Fx {
    async fn finish(self, svc: &Service) {
        if self.wake {
            svc.queued_notify.notify_waiters();
        }
        for t in self.tickets {
            let _ = t.await;
        }
    }
}

pub struct Service {
    cfg: Arc<Config>,
    version: String,
    logger: Logger,
    state: RwLock<State>,
    started_at: Instant,
    queued_notify: Notify,
    metrics: Arc<ServiceMetrics>,
    event_log: Option<EventLog>,
    callbacks: Callbacks,
    backend: Backend,
    shutdown_tx: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    stopped: AtomicBool,
}

fn not_found(code: i32, id: Uuid) -> ApiError {
    ApiError::new(400, code, "job not found").job(id)
}

fn not_running(code: i32, id: Uuid) -> ApiError {
    ApiError::new(400, code, "job is not running").job(id)
}

impl Service {
    /// Creates the backend and the observers, loads the stored jobs
    /// (`NewService` + `Service.Init`).
    pub async fn create(
        cfg: Arc<Config>,
        version: &str,
        logger: Logger,
    ) -> Result<Arc<Service>, String> {
        let backend = Backend::create(&cfg).inspect_err(|e| {
            logger.error(
                "Error while instantiate job service",
                &[
                    log::s("topic", "service"),
                    log::s("method", "CreateAndStartService"),
                    log::err(e),
                ],
            );
        })?;
        let metrics = Arc::new(ServiceMetrics::new(cfg.webserver.metrics.enable));
        let init_error = |e: String| {
            logger.error(
                "Error initializing event notifier",
                &[
                    log::s("topic", "service"),
                    log::s("method", "Init"),
                    log::err(&e),
                ],
            );
            e
        };
        let event_log = if cfg.events_logger.enable && !cfg.events_logger.file_path.is_empty() {
            Some(EventLog::open(&cfg.events_logger.file_path, logger.clone()).map_err(init_error)?)
        } else {
            None
        };
        let callbacks = Callbacks::new(logger.clone()).map_err(init_error)?;
        let (shutdown_tx, _) = watch::channel(false);
        let jobs = backend.load_jobs().await.inspect_err(|e| {
            logger.error(
                "Error while loading jobs",
                &[
                    log::s("topic", "service"),
                    log::s("method", "Init"),
                    log::err(e),
                ],
            );
        })?;
        let svc = Arc::new(Service {
            cfg,
            version: version.to_string(),
            logger,
            state: RwLock::new(State::default()),
            started_at: Instant::now(),
            queued_notify: Notify::new(),
            metrics,
            event_log,
            callbacks,
            backend,
            shutdown_tx,
            tasks: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
        });
        svc.restore(jobs);
        svc.logger.info(
            "Job server started",
            &[
                log::s("topic", "service"),
                log::s("method", "CreateAndStartService"),
            ],
        );
        Ok(svc)
    }

    /// Restores loaded jobs (`LoadJobs`). The backend is not notified.
    fn restore(&self, jobs: Vec<Job>) {
        let now = now_ms();
        let mut fx = Fx::default();
        let mut st = self.state.write();
        let mut created = Vec::new();
        for job in jobs {
            let job = Arc::new(job);
            match job.state() {
                JobState::Queued | JobState::Running => {
                    // LockResourcesForJobs: the last job wins on duplicates.
                    for r in job.resources() {
                        st.locks.insert(r.clone(), job.id);
                    }
                }
                JobState::Pending => {
                    st.pending.insert(OrderKey::of(&job));
                }
                JobState::Hidden => st.retry.push(job.id, now),
                JobState::Delayed => st.delayed.push(job.id, job.start_after),
                JobState::Created => created.push(job.id),
                _ => {}
            }
            self.metrics.on_job_loaded(&job);
            if let Some(l) = &self.event_log {
                l.job_event(ServiceEvent::JobLoaded, &job);
            }
            self.metrics.ensure_topic(&job.topic);
            st.insert(job);
        }
        // Locks actually held, per topic of the owner: a resource listed twice by a job, or
        // also claimed by a later job, counts once (releases never count it twice).
        let mut locked: HashMap<&str, i64> = HashMap::new();
        for owner in st.locks.values() {
            if let Some(j) = st.jobs.get(owner) {
                *locked.entry(j.topic.as_str()).or_default() += 1;
            }
        }
        for (topic, n) in locked {
            self.metrics.update_resources_locked(topic, n);
        }
        st.generation = 0;
        // A job saved between CREATE and PENDING (Go never schedules it).
        for id in created {
            self.schedule_new(&mut st, id, now, &mut fx);
        }
        drop(st);
        for t in fx.tickets {
            drop(t);
        }
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn logger(&self) -> &Logger {
        &self.logger
    }

    pub fn metrics(&self) -> &Arc<ServiceMetrics> {
        &self.metrics
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// Seconds since the service started.
    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    fn verbose(&self) -> bool {
        self.cfg.jobs.log_verbosity > 1
    }

    fn vlog(&self, msg: &str, method: &str, id: Uuid, extra: &[Field]) {
        if self.verbose() {
            let mut f = vec![
                log::s("topic", "service"),
                log::s("method", method),
                log::s("JobUUID", id.to_string()),
            ];
            f.extend_from_slice(extra);
            self.logger.info(msg, &f);
        }
    }

    fn log_error(&self, msg: &str, method: &str, id: Uuid) {
        self.logger.error(
            msg,
            &[
                log::s("topic", "service"),
                log::s("method", method),
                log::s("JobUUID", id.to_string()),
            ],
        );
    }

    /// Notifies the observers of a transition (under the lock).
    fn emit(&self, fx: &mut Fx, job: &Arc<Job>, ev: ServiceEvent, prev: JobState, new: JobState) {
        self.metrics.on_job_event(job, ev, prev, new);
        if let Some(l) = &self.event_log {
            l.job_event(ev, job);
        }
        if !job.callback_url.is_empty() && new.is_completed() && !prev.is_completed() {
            self.callbacks.trigger(job.clone());
        }
        let ticket = if ev == ServiceEvent::JobDeleted {
            self.backend.on_job_deleted(job.id)
        } else {
            self.backend.on_job(job, ev)
        };
        if let Some(t) = ticket {
            fx.tickets.push(t);
        }
    }

    fn transition(
        &self,
        st: &mut State,
        fx: &mut Fx,
        id: Uuid,
        event_type: &str,
        ev: ServiceEvent,
        now: i64,
    ) -> Arc<Job> {
        let job = st.event(id, event_type, now);
        self.emit(fx, &job, ev, job.previous_state(), job.state());
        job
    }

    fn unlock_job(&self, st: &mut State, job: &Job) {
        if job.resources().is_empty() {
            return;
        }
        let released = st.unlock_resources(job);
        self.metrics.update_resources_locked(&job.topic, -released);
    }

    // ---------------------------------------------------------------- creation

    fn new_job_id(st: &State) -> Uuid {
        loop {
            let id = Uuid::now_v7();
            if !st.jobs.contains_key(&id) {
                return id;
            }
        }
    }

    /// Second half of `FinalizeJobCreation`: delayed or pending, then scheduling.
    fn schedule_new(&self, st: &mut State, id: Uuid, now: i64, fx: &mut Fx) {
        let start_after = st.jobs[&id].start_after;
        if start_after != 0 && start_after > now {
            let job = self.transition(st, fx, id, events::DELAY, ServiceEvent::JobDelayed, now);
            st.delayed.push(id, job.start_after);
        } else {
            let job = self.transition(st, fx, id, events::PENDING, ServiceEvent::JobPending, now);
            st.pending.insert(OrderKey::of(&job));
        }
    }

    fn check_capacity(&self, st: &State) -> Result<(), ApiError> {
        if let Some(max) = self.backend.max_jobs()
            && st.jobs.len() as u64 >= max
        {
            self.logger.error(
                "Cannot create job",
                &[
                    log::s("topic", "service"),
                    log::s("method", "FinalizeJobCreation"),
                    log::err(format!("cannot create job, max limit reached: {max}")),
                ],
            );
            return Err(ApiError::new(
                503,
                codes::CANT_CREATE_JOB,
                "cannot create job",
            ));
        }
        Ok(())
    }

    /// Inserts a new job, returns the snapshot answered to the client.
    fn finalize_creation(
        &self,
        st: &mut State,
        mut job: Job,
        fx: &mut Fx,
    ) -> Result<Arc<Job>, ApiError> {
        self.check_capacity(st)?;
        let now = now_ms();
        let id = Self::new_job_id(st);
        job.init(id, now);
        let job = Arc::new(job);
        st.insert(job.clone());
        self.vlog("Job created", "FinalizeJobCreation", id, &[]);
        if self.cfg.jobs.log_verbosity > 2 {
            let json = String::from_utf8(job.to_json()).unwrap_or_default();
            self.logger.info(
                "New job",
                &[Field {
                    key: "Job".into(),
                    value: log::FieldValue::Json(json),
                }],
            );
        }
        self.emit(
            fx,
            &job,
            ServiceEvent::JobCreated,
            JobState::NoState,
            JobState::Created,
        );
        self.schedule_new(st, id, now_ms(), fx);
        let snapshot = st.jobs[&id].clone();
        self.enqueue_pending(st, fx);
        Ok(snapshot)
    }

    /// `POST /api/v1/job/` (`CreateJob`). The returned job is the state at creation
    /// (pending or delayed).
    pub async fn create_job(&self, payload: &[u8]) -> Result<Arc<Job>, ApiError> {
        let parsed = JobRequest::decode(payload);
        let mut fx = Fx::default();
        let result = {
            let mut st = self.state.write();
            let max = self.cfg.jobs.max_allowed_jobs;
            if max > 0 && st.jobs.len() as u64 >= max {
                self.logger.error(
                    "Cannot create job",
                    &[
                        log::s("topic", "service"),
                        log::s("method", "CreateJob"),
                        log::err("cannot create job, limit reached"),
                    ],
                );
                Err(ApiError::new(
                    503,
                    codes::CANT_CREATE_JOB,
                    "cannot create job",
                ))
            } else {
                match parsed {
                    Err(e) => {
                        self.logger.error(
                            "Cannot create job",
                            &[
                                log::s("topic", "service"),
                                log::s("method", "CreateJob"),
                                log::err("cannot create job from request"),
                            ],
                        );
                        Err(ApiError::new(
                            400,
                            codes::CANT_CREATE_JOB,
                            "cannot create job from request",
                        )
                        .cause(e.to_string()))
                    }
                    Ok(req) => self.finalize_creation(&mut st, req.into_job(now_ms()), &mut fx),
                }
            }
        };
        fx.finish(self).await;
        result
    }

    /// `POST /api/v1/job/{id}/clone` (`CloneJob`; `maxAllowedJobs` is not checked, as in Go).
    pub async fn clone_job(&self, id: Uuid) -> Result<Arc<Job>, ApiError> {
        let mut fx = Fx::default();
        let result = {
            let mut st = self.state.write();
            match st.jobs.get(&id).map(|j| j.clone_for_new()) {
                None => {
                    self.log_error("Cannot clone job", "CloneJob", id);
                    Err(not_found(codes::CANT_CLONE_JOB, id))
                }
                Some(clone) => self.finalize_creation(&mut st, clone, &mut fx),
            }
        };
        fx.finish(self).await;
        result
    }

    // ---------------------------------------------------------------- scheduling

    /// `EnqueueJob`: pending → queued when due and its resources can be locked.
    fn enqueue_job(&self, st: &mut State, id: Uuid, now: i64, fx: &mut Fx) -> bool {
        let job = st.jobs[&id].clone();
        if job.start_after != 0 && job.start_after > now {
            return false;
        }
        let Ok(acquired) = st.lock_resources(&job) else {
            return false;
        };
        // Go always calls this, which also registers the topic.
        self.metrics.update_resources_locked(&job.topic, acquired);
        st.pending.remove(&OrderKey::of(&job));
        self.transition(st, fx, id, events::ENQUEUE, ServiceEvent::JobEnqueued, now);
        self.vlog("Job enqueued", "EnqueueJob", id, &[]);
        fx.wake = true;
        true
    }

    /// `TryEnqueuePendingJobs`: pending jobs in priority order.
    fn enqueue_pending(&self, st: &mut State, fx: &mut Fx) {
        if st.pending.is_empty() {
            return;
        }
        let now = now_ms();
        let candidates: Vec<OrderKey> = st.pending.iter().copied().collect();
        for k in candidates {
            self.enqueue_job(st, k.id, now, fx);
        }
    }

    /// `RetryJob`: back to the queue. Unlike Go, the resources are locked again (they were
    /// released when the job was hidden); returns false if one is taken.
    fn retry_job(&self, st: &mut State, id: Uuid, now: i64, fx: &mut Fx) -> bool {
        let job = st.jobs[&id].clone();
        let Ok(acquired) = st.lock_resources(&job) else {
            return false;
        };
        if acquired > 0 {
            self.metrics.update_resources_locked(&job.topic, acquired);
        }
        self.transition(st, fx, id, events::ENQUEUE, ServiceEvent::JobEnqueued, now);
        self.vlog("Job enqueued", "RetryJob", id, &[]);
        fx.wake = true;
        true
    }

    /// One scheduler tick (1 s): delayed jobs, retries, visibility timeouts, pending jobs.
    pub async fn tick(&self) {
        let mut fx = Fx::default();
        {
            let mut st = self.state.write();
            let now = now_ms();
            // CheckDelayedJobs
            while let Some((id, _)) = st.delayed.pop_due(now) {
                if st.jobs.contains_key(&id) {
                    let job = self.transition(
                        &mut st,
                        &mut fx,
                        id,
                        events::PENDING,
                        ServiceEvent::JobPending,
                        now,
                    );
                    st.pending.insert(OrderKey::of(&job));
                }
            }
            // AttemptJobRetries
            let mut blocked = Vec::new();
            while let Some((id, time)) = st.retry.pop_due(now) {
                if st.jobs.contains_key(&id) && !self.retry_job(&mut st, id, now, &mut fx) {
                    blocked.push((id, time));
                }
            }
            for (id, time) in blocked {
                st.retry.push(id, time);
            }
            // CheckJobsVisibilityTimeout
            let expired: Vec<Uuid> = st
                .running
                .iter()
                .filter(|id| {
                    let j = &st.jobs[*id];
                    now - j.last_update() >= (j.visibility_timeout as i64).saturating_mul(1000)
                })
                .copied()
                .collect();
            for id in expired {
                self.logger.info(
                    "Job is running for too long",
                    &[
                        log::s("method", "CheckJobsVisibilityTimeout"),
                        log::s("JobUUID", id.to_string()),
                    ],
                );
                let _ = self.fail_locked(&mut st, id, &mut fx);
            }
            self.enqueue_pending(&mut st, &mut fx);
        }
        // Background writes are not awaited.
        if fx.wake {
            self.queued_notify.notify_waiters();
        }
    }

    // ---------------------------------------------------------------- pulling

    fn start_job(
        &self,
        st: &mut State,
        id: Uuid,
        vt: u64,
        fx: &mut Fx,
    ) -> Result<Arc<Job>, ApiError> {
        let Some(job) = st.jobs.get(&id) else {
            self.log_error("Cannot start job", "StartJob", id);
            return Err(not_found(codes::CANT_START_JOB, id));
        };
        if job.state() != JobState::Queued {
            self.log_error("Cannot start job", "StartJob", id);
            return Err(ApiError::new(400, codes::CANT_START_JOB, "job already started").job(id));
        }
        st.pending.remove(&OrderKey::of(job));
        let now = now_ms();
        let entry = st.jobs.get_mut(&id).expect("job present");
        Arc::make_mut(entry).visibility_timeout = vt;
        let job = self.transition(st, fx, id, events::START, ServiceEvent::JobStarted, now);
        self.vlog("Job started", "StartJob", id, &[]);
        Ok(job)
    }

    fn take_candidates(
        &self,
        st: &mut State,
        req: &PullRequest,
        fx: &mut Fx,
    ) -> Result<Vec<Arc<Job>>, ApiError> {
        if st.jobs.is_empty() {
            return Err(ApiError::new(
                404,
                codes::CANT_PULL_ANY_JOB,
                "no jobs found in the queue",
            ));
        }
        let set = if req.topic == "*" {
            Some(&st.queued)
        } else {
            st.queued_by_topic.get(&req.topic)
        };
        let ids: Vec<Uuid> = set
            .map(|s| s.iter().take(req.num_jobs as usize).map(|k| k.id).collect())
            .unwrap_or_default();
        if ids.is_empty() {
            return Err(ApiError::new(
                404,
                codes::CANT_PULL_ANY_JOB,
                "no job found that is ready or matches the criteria",
            ));
        }
        let mut started = Vec::with_capacity(ids.len());
        for id in ids {
            match self.start_job(st, id, req.visibility_timeout, fx) {
                Ok(j) => started.push(j),
                Err(e) => self.logger.warn(
                    "Cannot start job",
                    &[
                        log::s("topic", "service"),
                        log::s("method", "PullJobs"),
                        log::s("JobUUID", id.to_string()),
                        log::err(e),
                    ],
                ),
            }
        }
        Ok(started)
    }

    /// `POST /api/v1/job/pull` (`PullJobs`). Long polls wait for a job to be queued instead
    /// of sleeping while holding a global lock (B11), and end at shutdown.
    pub async fn pull_jobs(&self, req: &PullRequest) -> Result<Vec<Arc<Job>>, ApiError> {
        if let Some(id) = req.job_uuid {
            let mut fx = Fx::default();
            let result = {
                let mut st = self.state.write();
                if st.jobs.contains_key(&id) {
                    self.start_job(&mut st, id, req.visibility_timeout, &mut fx)
                        .map(|j| vec![j])
                } else {
                    Err(ApiError::new(
                        400,
                        codes::CANT_PULL_SPECIFIC_JOB,
                        "job not found in the queue",
                    )
                    .job(id))
                }
            };
            fx.finish(self).await;
            return result;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(req.wait_time_seconds);
        let mut shutdown = self.shutdown_tx.subscribe();
        loop {
            let notified = self.queued_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mut fx = Fx::default();
            let result = {
                let mut st = self.state.write();
                self.take_candidates(&mut st, req, &mut fx)
            };
            match result {
                Ok(jobs) if !jobs.is_empty() => {
                    fx.finish(self).await;
                    return Ok(jobs);
                }
                Ok(_) => {
                    fx.finish(self).await;
                    self.metrics.empty_poll(&req.topic);
                    return Err(ApiError::new(
                        404,
                        codes::CANT_PULL_ANY_JOB,
                        "no job found that is ready or matches the criteria",
                    ));
                }
                Err(e) => {
                    let expired = req.wait_time_seconds == 0
                        || tokio::time::Instant::now() >= deadline
                        || *shutdown.borrow();
                    if expired {
                        self.metrics.empty_poll(&req.topic);
                        return Err(e);
                    }
                    tokio::select! {
                        _ = &mut notified => {}
                        _ = tokio::time::sleep_until(deadline) => {}
                        _ = shutdown.changed() => {}
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------- transitions

    /// `POST /api/v1/job/{id}/succeed` (`SetJobAsSuccessful`).
    pub async fn succeed(&self, id: Uuid) -> Result<(), ApiError> {
        let mut fx = Fx::default();
        let result = {
            let mut st = self.state.write();
            match st.jobs.get(&id).cloned() {
                None => {
                    self.log_error("Cannot set job as successful", "SetJobAsSuccessful", id);
                    Err(not_found(codes::CANT_SET_JOB_AS_SUCCESSFUL, id))
                }
                Some(j) if j.state() != JobState::Running => {
                    self.log_error("Cannot set job as successful", "SetJobAsSuccessful", id);
                    Err(not_running(codes::CANT_SET_JOB_AS_SUCCESSFUL, id))
                }
                Some(j) => {
                    self.unlock_job(&mut st, &j);
                    self.transition(
                        &mut st,
                        &mut fx,
                        id,
                        events::SUCCESS,
                        ServiceEvent::JobSucceeded,
                        now_ms(),
                    );
                    self.vlog("Job succeeded", "SetJobAsSuccessful", id, &[]);
                    self.enqueue_pending(&mut st, &mut fx);
                    Ok(())
                }
            }
        };
        fx.finish(self).await;
        result
    }

    /// `POST /api/v1/job/{id}/cancel` (`CancelJob`): back to the queue, resources kept.
    pub async fn cancel(&self, id: Uuid) -> Result<(), ApiError> {
        let mut fx = Fx::default();
        let result = {
            let mut st = self.state.write();
            match st.jobs.get(&id).map(|j| j.state()) {
                None => {
                    self.log_error("Cannot cancel job", "CancelJob", id);
                    Err(not_found(codes::CANT_CANCEL_JOB, id))
                }
                Some(s) if s != JobState::Running => {
                    self.log_error("Cannot cancel job", "CancelJob", id);
                    Err(not_running(codes::CANT_CANCEL_JOB, id))
                }
                Some(_) => {
                    let now = now_ms();
                    self.transition(
                        &mut st,
                        &mut fx,
                        id,
                        events::CANCEL,
                        ServiceEvent::JobCanceled,
                        now,
                    );
                    self.vlog("Job canceled", "CancelJob", id, &[]);
                    self.transition(
                        &mut st,
                        &mut fx,
                        id,
                        events::ENQUEUE,
                        ServiceEvent::JobEnqueued,
                        now_ms(),
                    );
                    self.vlog("Job enqueued", "CancelJob", id, &[]);
                    fx.wake = true;
                    self.enqueue_pending(&mut st, &mut fx);
                    Ok(())
                }
            }
        };
        fx.finish(self).await;
        result
    }

    /// `FailJob` under the lock; `Ok(true)` when the job reached the maximum of retries.
    fn fail_locked(&self, st: &mut State, id: Uuid, fx: &mut Fx) -> Result<bool, ApiError> {
        match st.jobs.get(&id).map(|j| j.state()) {
            None => {
                self.log_error("Cannot set job as failed", "FailJob", id);
                return Err(not_found(codes::CANT_FAIL_JOB, id));
            }
            Some(s) if s != JobState::Running => {
                self.log_error("Cannot set job as failed", "FailJob", id);
                return Err(not_running(codes::CANT_FAIL_JOB, id));
            }
            Some(_) => {}
        }
        self.vlog("Job failed", "FailJob", id, &[]);
        let job = self.transition(st, fx, id, events::FAIL, ServiceEvent::JobFailed, now_ms());
        let failures = job.count_failures();
        let policy = &self.cfg.jobs.retry_policy;
        if failures >= policy.max_retry {
            let job = st.event(id, events::TERMINATE, now_ms());
            self.unlock_job(st, &job);
            if self.verbose() {
                self.logger.info(
                    "Job terminated",
                    &[
                        log::s("topic", "service"),
                        log::s("method", "FailJob"),
                        log::s("reason", "Max retry limit reached"),
                        log::s("JobUUID", id.to_string()),
                    ],
                );
            }
            self.emit(
                fx,
                &job,
                ServiceEvent::JobTerminated,
                job.previous_state(),
                job.state(),
            );
            self.enqueue_pending(st, fx);
            return Ok(true);
        }
        let delay = policy.backoff_strategy.delay_ms(failures);
        // RetryJob: the job normally still owns its resources. If one was unlocked by the
        // admin API and taken by another job, the job waits in the retry queue instead.
        if delay == 0 && self.retry_job(st, id, now_ms(), fx) {
            return Ok(false);
        }
        // RetryJobLater
        let now = now_ms();
        let next = now.saturating_add(delay.min(i64::MAX as u64) as i64);
        if self.verbose() {
            let when = chrono::DateTime::from_timestamp_millis(next)
                .map(|t| t.with_timezone(&chrono::Local).fixed_offset())
                .map(|t| log::gotime::format(&t, "2006-01-02 15:04:05.999999999 -0700 MST"))
                .unwrap_or_default();
            self.logger.info(
                "Job added to the retry queue",
                &[
                    log::s("topic", "service"),
                    log::s("method", "FailJob"),
                    log::s("JobUUID", id.to_string()),
                    log::s("NextRetryTime", when),
                ],
            );
        }
        let job = st.event(id, events::HIDE, now);
        self.unlock_job(st, &job);
        self.emit(
            fx,
            &job,
            ServiceEvent::JobHidden,
            job.previous_state(),
            job.state(),
        );
        st.retry.push(id, next);
        self.enqueue_pending(st, fx);
        Ok(false)
    }

    /// `POST /api/v1/job/{id}/fail` (`FailJob`).
    pub async fn fail(&self, id: Uuid) -> Result<bool, ApiError> {
        let mut fx = Fx::default();
        let result = {
            let mut st = self.state.write();
            self.fail_locked(&mut st, id, &mut fx)
        };
        fx.finish(self).await;
        result
    }

    /// `POST /api/v1/job/{id}/visibilitytimeout` (`ChangeVisibilityTimeoutJob`; any state).
    pub async fn change_visibility(&self, id: Uuid, vt: u64) -> Result<(), ApiError> {
        let mut fx = Fx::default();
        let result = {
            let mut st = self.state.write();
            match st.jobs.get_mut(&id) {
                None => {
                    self.log_error(
                        "Cannot change job visibility timeout",
                        "ChangeVisibilityTimeoutJob",
                        id,
                    );
                    Err(not_found(codes::CANT_CLONE_JOB, id))
                }
                Some(entry) => {
                    Arc::make_mut(entry).visibility_timeout = vt;
                    let job = entry.clone();
                    st.generation += 1;
                    // Go does not persist the change in Redis; the port does, except for
                    // completed jobs (a SET would clear their TTL).
                    if !job.is_completed()
                        && let Some(t) = self.backend.on_job(&job, ServiceEvent::JobTimeout)
                    {
                        fx.tickets.push(t);
                    }
                    Ok(())
                }
            }
        };
        fx.finish(self).await;
        result
    }

    // ---------------------------------------------------------------- deletion

    fn delete_locked(&self, st: &mut State, id: Uuid, fx: &mut Fx) -> bool {
        let Some(job) = st.jobs.get(&id).cloned() else {
            return false;
        };
        self.unlock_job(st, &job);
        let prev = job.state();
        let key = OrderKey::of(&job);
        let entry = st.jobs.get_mut(&id).expect("job present");
        Arc::make_mut(entry).add_event(events::DELETE, now_ms());
        let job = entry.clone();
        st.index_out(prev, key, &job.topic);
        st.retry.remove(id);
        st.delayed.remove(id);
        st.jobs.remove(&id);
        st.pending.remove(&key);
        st.generation += 1;
        self.vlog("Job deleted", "DeleteJob", id, &[]);
        self.emit(fx, &job, ServiceEvent::JobDeleted, prev, JobState::Deleted);
        true
    }

    /// `DELETE /api/v1/job/{id}` (`DeleteJob`).
    pub async fn delete_job(&self, id: Uuid) -> Result<(), ApiError> {
        let mut fx = Fx::default();
        let result = {
            let mut st = self.state.write();
            if self.delete_locked(&mut st, id, &mut fx) {
                self.enqueue_pending(&mut st, &mut fx);
                Ok(())
            } else {
                self.log_error("Cannot delete job", "DeleteJob", id);
                Err(ApiError::new(400, codes::CANT_DELETE_JOB, "job not found").job(id))
            }
        };
        fx.finish(self).await;
        result
    }

    /// `DELETE /api/v1/jobs/queued`: created, delayed, pending and queued jobs.
    pub async fn delete_queued(&self) -> Result<(), ApiError> {
        let mut fx = Fx::default();
        {
            let mut st = self.state.write();
            let ids: Vec<Uuid> = st
                .jobs
                .values()
                .filter(|j| {
                    matches!(
                        j.state(),
                        JobState::Created
                            | JobState::Delayed
                            | JobState::Pending
                            | JobState::Queued
                    )
                })
                .map(|j| j.id)
                .collect();
            for id in ids {
                self.delete_locked(&mut st, id, &mut fx);
            }
            self.enqueue_pending(&mut st, &mut fx);
        }
        fx.finish(self).await;
        Ok(())
    }

    /// `DELETE /api/v1/jobs/` (`DeleteAllJobs`).
    pub async fn delete_all(&self) -> Result<(), ApiError> {
        let mut fx = Fx::default();
        {
            let mut st = self.state.write();
            let generation = st.generation + 1;
            *st = State {
                generation,
                ..State::default()
            };
            self.logger.info("All resources unlocked", &[]);
            self.metrics.on_unlock_all();
            self.metrics.on_delete_all();
            if let Some(l) = &self.event_log {
                l.service_event(ServiceEvent::JobDeletedAll);
            }
            if let Some(t) = self.backend.on_all_deleted() {
                fx.tickets.push(t);
            }
        }
        fx.finish(self).await;
        Ok(())
    }

    /// Retention policy (`DeleteOldCompletedJobs`).
    pub async fn apply_retention(&self) {
        let policy = &self.cfg.jobs.retention_policy;
        if !policy.enable {
            return;
        }
        let mut fx = Fx::default();
        {
            let mut st = self.state.write();
            let mut completed: Vec<(i64, Uuid)> = st
                .jobs
                .values()
                .filter(|j| j.is_completed())
                .map(|j| (j.last_update(), j.id))
                .collect();
            completed.sort();
            let to_delete = if policy.max_jobs >= 0 {
                completed.len().saturating_sub(policy.max_jobs as usize)
            } else {
                0
            };
            let max_age = if policy.max_job_age >= 0 {
                policy.max_job_age.saturating_mul(1000)
            } else {
                i64::MAX
            };
            let now = now_ms();
            for (i, (last, id)) in completed.into_iter().enumerate() {
                if i < to_delete || now - last >= max_age {
                    self.delete_locked(&mut st, id, &mut fx);
                }
            }
        }
        if fx.wake {
            self.queued_notify.notify_waiters();
        }
    }

    // ---------------------------------------------------------------- resources

    /// `GET /api/v1/resources/`.
    pub fn locked_resources(&self) -> BTreeMap<String, Uuid> {
        self.state
            .read()
            .locks
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }

    /// `POST /api/v1/resources/unlock` (safety net).
    pub async fn unlock_all_resources(&self) {
        let mut fx = Fx::default();
        {
            let mut st = self.state.write();
            st.locks.clear();
            self.logger.info("All resources unlocked", &[]);
            self.metrics.on_unlock_all();
            self.enqueue_pending(&mut st, &mut fx);
        }
        fx.finish(self).await;
    }

    /// `POST /api/v1/resource/{name}/unlock` (safety net).
    pub async fn unlock_resource(&self, name: &str) -> Result<(), ApiError> {
        let mut fx = Fx::default();
        let result = {
            let mut st = self.state.write();
            match st.locks.remove(name) {
                None => {
                    self.logger
                        .warn("Resource not found", &[log::s("resource", name)]);
                    Err(ApiError::new(
                        404,
                        codes::RESOURCE_NOT_FOUND,
                        format!("resource not found: {name}"),
                    ))
                }
                Some(owner) => {
                    self.logger
                        .info("Resource unlocked", &[log::s("resource", name)]);
                    let topic = st
                        .jobs
                        .get(&owner)
                        .map(|j| j.topic.clone())
                        .unwrap_or_default();
                    self.metrics.on_resource_unlocked(name, &topic);
                    self.enqueue_pending(&mut st, &mut fx);
                    Ok(())
                }
            }
        };
        fx.finish(self).await;
        result
    }

    // ---------------------------------------------------------------- reads

    pub fn get_job(&self, id: Uuid) -> Option<Arc<Job>> {
        self.state.read().jobs.get(&id).cloned()
    }

    /// All jobs, in id order (Go: random map order).
    pub fn all_jobs(&self) -> Vec<Arc<Job>> {
        let mut jobs: Vec<(Uuid, Arc<Job>)> = self
            .state
            .read()
            .jobs
            .iter()
            .map(|(id, j)| (*id, j.clone()))
            .collect();
        jobs.sort_unstable_by_key(|(id, _)| *id);
        jobs.into_iter().map(|(_, j)| j).collect()
    }

    pub fn jobs_count(&self) -> usize {
        self.state.read().jobs.len()
    }

    pub fn get_jobs(&self, req: &GetJobsRequest) -> GetJobsResponse {
        // Under the read lock: no job is copied or reference-counted except the page.
        let st = self.state.read();
        query::paginate(query::select(st.jobs.values(), req), req)
    }

    pub async fn healthcheck(&self) -> bool {
        self.backend.healthcheck().await
    }

    // ---------------------------------------------------------------- lifecycle

    /// Starts the background tasks (`Service.Run`).
    pub fn start(self: &Arc<Self>) {
        self.logger.info(
            "Service started",
            &[log::s("topic", "service"), log::s("method", "Run")],
        );
        if let Some(l) = &self.event_log {
            l.service_event(ServiceEvent::Ready);
        }
        let mut tasks = self.tasks.lock();
        let svc = self.clone();
        let mut stop = self.shutdown_tx.subscribe();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tokio::select! {
                    _ = tick.tick() => svc.tick().await,
                    _ = stop.changed() => return,
                }
            }
        }));
        if self.cfg.jobs.retention_policy.enable {
            let svc = self.clone();
            let mut stop = self.shutdown_tx.subscribe();
            let secs = self.cfg.jobs.retention_policy.interval.max(60) as u64;
            tasks.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(secs));
                tick.tick().await;
                loop {
                    tokio::select! {
                        _ = tick.tick() => svc.apply_retention().await,
                        _ = stop.changed() => return,
                    }
                }
            }));
        }
        if self.metrics.enabled() {
            let svc = self.clone();
            let mut stop = self.shutdown_tx.subscribe();
            tasks.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(10));
                tick.tick().await;
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            let jobs: Vec<Arc<Job>> = svc.state.read().jobs.values().cloned().collect();
                            svc.metrics.update_job_statistics(&jobs, now_ms());
                        }
                        _ = stop.changed() => return,
                    }
                }
            }));
        }
        if let Some(every) = self.backend.snapshot_interval() {
            let svc = self.clone();
            let mut stop = self.shutdown_tx.subscribe();
            tasks.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(every);
                tick.tick().await;
                let mut saved = 0u64;
                loop {
                    tokio::select! {
                        _ = tick.tick() => saved = svc.save_snapshot(saved).await,
                        _ = stop.changed() => return,
                    }
                }
            }));
        }
    }

    /// Saves the jobs if they changed since generation `saved`; returns the saved generation.
    async fn save_snapshot(&self, saved: u64) -> u64 {
        let (generation, jobs) = {
            let st = self.state.read();
            if st.generation == saved {
                return saved;
            }
            (st.generation, st.jobs.values().cloned().collect::<Vec<_>>())
        };
        match self.backend.save_snapshot(jobs).await {
            Ok(()) => generation,
            Err(_) => saved,
        }
    }

    /// Wakes long polls so that they answer now; called when the server starts to stop.
    pub fn begin_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        self.queued_notify.notify_waiters();
    }

    /// Stops the background tasks, saves, flushes the storage and the event log.
    pub async fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.begin_shutdown();
        let tasks: Vec<JoinHandle<()>> = self.tasks.lock().drain(..).collect();
        for t in tasks {
            let _ = t.await;
        }
        if self.backend.snapshot_interval().is_some() {
            // Final save, always (Go could skip it, B08).
            let jobs: Vec<Arc<Job>> = self.state.read().jobs.values().cloned().collect();
            let _ = self.backend.save_snapshot(jobs).await;
        }
        self.backend.stop().await;
        if let Some(l) = &self.event_log {
            l.close();
        }
        self.logger.info(
            "Service stopped",
            &[log::s("topic", "service"), log::s("method", "Run")],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "
backend:
  type: mock
  logger: {level: error, encoding: json}
jobs:
  defaultVisibilityTimeout: 60
  maxVisibilityTimeout: 600
  retryPolicy: {maxRetry: 1}
  retentionPolicy: {enable: true, interval: 60, maxJobs: RETAIN, maxJobAge: AGE}
logger: {level: error, encoding: json}
";

    async fn service(retain: i64, age: i64) -> Arc<Service> {
        let text = CONFIG
            .replace("RETAIN", &retain.to_string())
            .replace("AGE", &age.to_string());
        let cfg = Arc::new(crate::config::parse(&text).unwrap());
        Service::create(cfg, "test", Logger::nop()).await.unwrap()
    }

    async fn completed_job(svc: &Service, fail: bool) -> Uuid {
        let job = svc.create_job(br#"{"topic":"r"}"#).await.unwrap();
        let req = PullRequest {
            job_uuid: Some(job.id),
            num_jobs: 1,
            visibility_timeout: 60,
            ..Default::default()
        };
        svc.pull_jobs(&req).await.unwrap();
        if fail {
            assert!(svc.fail(job.id).await.unwrap(), "maxRetry 1 terminates");
        } else {
            svc.succeed(job.id).await.unwrap();
        }
        job.id
    }

    #[tokio::test]
    async fn retention_keeps_the_most_recent_completed_jobs() {
        let svc = service(1, -1).await;
        let a = completed_job(&svc, false).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        let b = completed_job(&svc, true).await;
        let running = svc.create_job(b"{}").await.unwrap().id;
        svc.apply_retention().await;
        assert!(svc.get_job(a).is_none());
        assert_eq!(svc.get_job(b).map(|j| j.state()), Some(JobState::Failed));
        assert!(
            svc.get_job(running).is_some(),
            "non-completed jobs are kept"
        );
    }

    #[tokio::test]
    async fn retention_by_age() {
        let svc = service(-1, 0).await;
        completed_job(&svc, false).await;
        completed_job(&svc, false).await;
        let pending = svc.create_job(b"{}").await.unwrap().id;
        svc.apply_retention().await;
        assert_eq!(svc.jobs_count(), 1);
        assert!(svc.get_job(pending).is_some());
        let svc = service(-1, 3600).await;
        completed_job(&svc, false).await;
        svc.apply_retention().await;
        assert_eq!(svc.jobs_count(), 1);
    }

    fn locked_count(svc: &Service, topic: &str) -> u64 {
        svc.metrics().job_metrics_by_topic()[topic][0].resources_locked_count
    }

    #[tokio::test]
    async fn restored_locks_are_counted_once() {
        let svc = service(-1, -1).await;
        let mut running = Job {
            topic: "r".into(),
            lock_resources: Some(vec!["a".into(), "a".into(), "b".into()]),
            ..Default::default()
        };
        running.init(Uuid::now_v7(), now_ms());
        for e in [events::PENDING, events::ENQUEUE, events::START] {
            running.add_event(e, now_ms());
        }
        let id = running.id;
        svc.restore(vec![running]);
        assert_eq!(locked_count(&svc, "r"), 2);
        svc.succeed(id).await.unwrap();
        assert!(svc.locked_resources().is_empty());
        assert_eq!(locked_count(&svc, "r"), 0);
    }

    /// Retry without delay while the job's resource has been unlocked and taken by another
    /// job: the failed job waits in the retry queue instead of staying "running".
    #[tokio::test]
    async fn immediate_retry_waits_for_a_taken_resource() {
        let text = CONFIG
            .replace(
                "retryPolicy: {maxRetry: 1}",
                "retryPolicy: {maxRetry: 5, backoffStrategy: {type: 0, base: 0}}",
            )
            .replace("RETAIN", "-1")
            .replace("AGE", "-1");
        let cfg = Arc::new(crate::config::parse(&text).unwrap());
        let svc = Service::create(cfg, "test", Logger::nop()).await.unwrap();
        let body = br#"{"topic":"t","lockResources":["r"]}"#;
        let a = svc.create_job(body).await.unwrap().id;
        let pull = |id| PullRequest {
            job_uuid: Some(id),
            num_jobs: 1,
            visibility_timeout: 60,
            ..Default::default()
        };
        svc.pull_jobs(&pull(a)).await.unwrap();
        svc.unlock_resource("r").await.unwrap();
        let b = svc.create_job(body).await.unwrap().id;
        assert_eq!(svc.get_job(b).unwrap().state(), JobState::Queued);
        assert!(!svc.fail(a).await.unwrap());
        assert_eq!(svc.get_job(a).unwrap().state(), JobState::Hidden);
        svc.pull_jobs(&pull(b)).await.unwrap();
        svc.succeed(b).await.unwrap();
        svc.tick().await;
        assert_eq!(svc.get_job(a).unwrap().state(), JobState::Queued);
        assert_eq!(svc.locked_resources().get("r"), Some(&a));
    }

    #[tokio::test]
    async fn delayed_jobs_become_pending() {
        let svc = service(-1, -1).await;
        let start = now_ms() + 300;
        let job = svc
            .create_job(format!(r#"{{"startAfter":{start}}}"#).as_bytes())
            .await
            .unwrap();
        assert_eq!(job.state(), JobState::Delayed);
        tokio::time::sleep(Duration::from_millis(400)).await;
        svc.tick().await;
        assert_eq!(svc.get_job(job.id).unwrap().state(), JobState::Queued);
    }
}
