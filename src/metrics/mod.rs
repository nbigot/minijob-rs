//! Job metrics (`metrics/servicemetrics.go`): per-topic counters, topic statistics, locked
//! resources, Prometheus collectors.
//!
//! Corrections compared to Go v1.0.0 (see MIGRATION.md): deletes are counted, counters saturate
//! instead of wrapping, loaded jobs count in `totalJobs`/`activeJobs`, `ResourcesLockedCount`
//! decreases on unlock, `job_active_duration_seconds` is fed seconds, the topic list has no
//! duplicates.

pub mod percentiles;
pub mod prom;

use crate::events::ServiceEvent;
use crate::model::{Job, JobState};
use parking_lot::Mutex;
use prom::{BUCKETS_PERCENTILES, Prom};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// Counters of one topic (`JobMetrics`), serialized in Go's field order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobMetrics {
    pub resources_locked_count: u64,
    pub jobs_counter_deleted: u64,
    pub jobs_existing: u64,
    pub jobs_status_created: u64,
    pub jobs_status_delayed: u64,
    pub jobs_status_pending: u64,
    pub jobs_status_queued: u64,
    pub jobs_status_running: u64,
    pub jobs_status_succeeded: u64,
    pub jobs_status_failed: u64,
    pub jobs_status_hidden: u64,
    pub jobs_status_canceled: u64,
}

impl JobMetrics {
    fn counter(&mut self, s: JobState) -> Option<&mut u64> {
        match s {
            JobState::Created => Some(&mut self.jobs_status_created),
            JobState::Delayed => Some(&mut self.jobs_status_delayed),
            JobState::Pending => Some(&mut self.jobs_status_pending),
            JobState::Queued => Some(&mut self.jobs_status_queued),
            JobState::Running => Some(&mut self.jobs_status_running),
            JobState::Succeeded => Some(&mut self.jobs_status_succeeded),
            JobState::Failed => Some(&mut self.jobs_status_failed),
            JobState::Hidden => Some(&mut self.jobs_status_hidden),
            JobState::Canceled => Some(&mut self.jobs_status_canceled),
            JobState::Deleted | JobState::NoState => None,
        }
    }

    /// `OnJobStateChange`.
    pub fn on_state_change(&mut self, prev: JobState, new: JobState) {
        if prev == new {
            return;
        }
        if prev == JobState::Deleted {
            self.jobs_counter_deleted = self.jobs_counter_deleted.saturating_sub(1);
        } else if let Some(c) = self.counter(prev) {
            *c = c.saturating_sub(1);
        }
        match new {
            JobState::Created => {
                self.jobs_existing += 1;
                self.jobs_status_created += 1;
            }
            JobState::Deleted => {
                self.jobs_existing = self.jobs_existing.saturating_sub(1);
                self.jobs_counter_deleted += 1;
            }
            s => {
                if let Some(c) = self.counter(s) {
                    *c += 1;
                }
            }
        }
    }
}

/// Pre-computed statistics of one topic (`TopicMetrics`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopicMetrics {
    pub topic_name: String,
    pub total_jobs: u64,
    /// 0..100.
    pub percent_jobs: f64,
    pub active_jobs: u64,
    /// 0..1.
    pub success_rate: f64,
    /// Seconds.
    pub average_duration: f64,
}

/// A locked resource (`ResourceMetrics`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceMetrics {
    pub resource_name: String,
    #[serde(serialize_with = "ser_uuid")]
    pub job_uuid: Uuid,
    pub topic: String,
    /// Unix seconds.
    pub start_time: i64,
    /// Seconds.
    pub lock_duration: i64,
}

fn ser_uuid<S: serde::Serializer>(id: &Uuid, s: S) -> Result<S::Ok, S::Error> {
    let mut buf = Uuid::encode_buffer();
    s.serialize_str(id.hyphenated().encode_lower(&mut buf))
}

/// `JobStatsMetrics` (`/api/v1/metrics/jobs/processing`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatsMetrics {
    pub existing_jobs: u64,
    pub created_jobs: u64,
    pub delayed_jobs: u64,
    pub pending_jobs: u64,
    pub queued_jobs: u64,
    pub running_jobs: u64,
    pub succeeded_jobs: u64,
    pub failed_jobs: u64,
    pub hidden_jobs: u64,
    pub canceled_jobs: u64,
    pub deleted_jobs: u64,
}

/// `JobActivityMetrics` (`/api/v1/metrics/jobs/activity`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct JobActivityMetrics {
    pub created: u64,
    pub delayed: u64,
    pub pending: u64,
    pub enqueued: u64,
    pub started: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub canceled: u64,
    pub hidden: u64,
    pub deleted: u64,
    pub timeout: u64,
    pub terminated: u64,
}

#[derive(Default)]
struct TopicData {
    jobs: JobMetrics,
    stats: TopicMetrics,
    completed_count: u64,
    cumulative_duration_ms: i64,
}

#[derive(Default)]
struct State {
    /// Insertion order, no duplicates.
    topics: Vec<String>,
    by_topic: HashMap<String, TopicData>,
    resources: HashMap<String, ResourceMetrics>,
    activity: JobActivityMetrics,
}

impl State {
    fn topic(&mut self, topic: &str) -> &mut TopicData {
        if !self.by_topic.contains_key(topic) {
            self.topics.push(topic.to_string());
            self.by_topic.insert(
                topic.to_string(),
                TopicData {
                    stats: TopicMetrics {
                        topic_name: topic.to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
        }
        self.by_topic.get_mut(topic).expect("topic just inserted")
    }
}

fn is_active(s: JobState) -> bool {
    matches!(s, JobState::Pending | JobState::Queued | JobState::Running)
}

/// Metrics of the service. The topic registry is always maintained (it backs
/// `GET /api/v1/topics/`); counters and Prometheus only when `webserver.metrics.enable`.
pub struct ServiceMetrics {
    enabled: bool,
    state: Mutex<State>,
    prom: Option<Arc<Prom>>,
}

impl ServiceMetrics {
    pub fn new(enabled: bool) -> ServiceMetrics {
        ServiceMetrics {
            enabled,
            state: Mutex::new(State::default()),
            prom: enabled.then(|| Arc::new(Prom::new())),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn prom(&self) -> Option<&Arc<Prom>> {
        self.prom.as_ref()
    }

    /// Adds a topic to the registry (`EnsureTopicExists`).
    pub fn ensure_topic(&self, topic: &str) {
        self.state.lock().topic(topic);
    }

    /// `UpdateResourcesLockedCountMetric`, saturating (Go v1.0.0 adds instead of subtracting).
    pub fn update_resources_locked(&self, topic: &str, delta: i64) {
        let mut st = self.state.lock();
        let m = &mut st.topic(topic).jobs;
        m.resources_locked_count = if delta < 0 {
            m.resources_locked_count
                .saturating_sub(delta.unsigned_abs())
        } else {
            m.resources_locked_count + delta as u64
        };
    }

    /// A job event, with the state transition it caused. For deletions the caller passes the
    /// state before the deletion and `JobState::Deleted`.
    pub fn on_job_event(&self, job: &Job, ev: ServiceEvent, prev: JobState, new: JobState) {
        if !self.enabled {
            return;
        }
        let mut st = self.state.lock();
        let duration = job.duration_first_last();
        let data = st.topic(&job.topic);
        data.jobs.on_state_change(prev, new);
        // UpdateTopicMetrics
        if is_active(prev) {
            data.stats.active_jobs = data.stats.active_jobs.saturating_sub(1);
        }
        match new {
            s if is_active(s) => data.stats.active_jobs += 1,
            JobState::Created => data.stats.total_jobs += 1,
            JobState::Deleted => data.stats.total_jobs = data.stats.total_jobs.saturating_sub(1),
            _ => {}
        }
        if new.is_completed() && !prev.is_completed() && duration > 0 {
            data.completed_count += 1;
            data.cumulative_duration_ms += duration;
            let avg_ms = data.cumulative_duration_ms / data.completed_count as i64;
            data.stats.average_duration = avg_ms as f64 / 1000.0;
        }
        let m = &data.jobs;
        let completed = m.jobs_status_succeeded + m.jobs_status_failed + m.jobs_status_canceled;
        data.stats.success_rate = if completed > 0 {
            m.jobs_status_succeeded as f64 / completed as f64
        } else {
            0.0
        };
        let jobs_snapshot = data.jobs.clone();
        // Resources held by the job.
        if job.lock_resources.is_some() {
            match new {
                JobState::Queued | JobState::Running => {
                    let start = job.queued_or_running_timestamp();
                    for r in job.resources() {
                        st.resources.insert(
                            r.clone(),
                            ResourceMetrics {
                                resource_name: r.clone(),
                                job_uuid: job.id,
                                topic: job.topic.clone(),
                                start_time: start,
                                lock_duration: 0,
                            },
                        );
                    }
                }
                JobState::Succeeded
                | JobState::Failed
                | JobState::Canceled
                | JobState::Deleted
                | JobState::Hidden => {
                    for r in job.resources() {
                        if st.resources.get(r).is_some_and(|m| m.job_uuid == job.id) {
                            st.resources.remove(r);
                        }
                    }
                }
                _ => {}
            }
        }
        let a = &mut st.activity;
        match ev {
            ServiceEvent::JobCreated => a.created += 1,
            ServiceEvent::JobDelayed => a.delayed += 1,
            ServiceEvent::JobPending => a.pending += 1,
            ServiceEvent::JobEnqueued => a.enqueued += 1,
            ServiceEvent::JobStarted => a.started += 1,
            ServiceEvent::JobSucceeded => a.succeeded += 1,
            ServiceEvent::JobFailed => a.failed += 1,
            ServiceEvent::JobCanceled => a.canceled += 1,
            ServiceEvent::JobHidden => a.hidden += 1,
            ServiceEvent::JobDeleted => a.deleted += 1,
            ServiceEvent::JobTimeout => a.timeout += 1,
            ServiceEvent::JobTerminated => a.terminated += 1,
            _ => {}
        }
        drop(st);
        if let Some(p) = &self.prom {
            p.job_events
                .with_label_values(&[ev.name(), &job.topic])
                .inc();
            p.set_gauges(&job.topic, &jobs_snapshot);
            if matches!(
                ev,
                ServiceEvent::JobSucceeded | ServiceEvent::JobDeleted | ServiceEvent::JobTerminated
            ) {
                p.job_completion
                    .with_label_values(&[&job.topic])
                    .observe(duration as f64 / 1000.0);
            }
        }
    }

    /// A job restored from storage (`UpdateMetricsFromJobHistory`).
    pub fn on_job_loaded(&self, job: &Job) {
        if !self.enabled {
            return;
        }
        let mut st = self.state.lock();
        let data = st.topic(&job.topic);
        let mut prev = JobState::NoState;
        for e in &job.history {
            let s = JobState::from_event(&e.event_type);
            data.jobs.on_state_change(prev, s);
            prev = s;
        }
        data.stats.total_jobs += 1;
        if is_active(job.state()) {
            data.stats.active_jobs += 1;
        }
        let snapshot = data.jobs.clone();
        if matches!(job.state(), JobState::Queued | JobState::Running) {
            let start = job.queued_or_running_timestamp();
            for r in job.resources() {
                st.resources.insert(
                    r.clone(),
                    ResourceMetrics {
                        resource_name: r.clone(),
                        job_uuid: job.id,
                        topic: job.topic.clone(),
                        start_time: start,
                        lock_duration: 0,
                    },
                );
            }
        }
        drop(st);
        if let Some(p) = &self.prom {
            p.set_gauges(&job.topic, &snapshot);
        }
    }

    /// All jobs deleted (`OnDeleteAllJobs`).
    pub fn on_delete_all(&self) {
        if !self.enabled {
            return;
        }
        let mut st = self.state.lock();
        for data in st.by_topic.values_mut() {
            let deleted = data.jobs.jobs_counter_deleted + data.jobs.jobs_existing;
            data.jobs = JobMetrics {
                jobs_counter_deleted: deleted,
                ..Default::default()
            };
            data.stats.total_jobs = 0;
            data.stats.active_jobs = 0;
            data.stats.success_rate = 0.0;
        }
        st.resources.clear();
        drop(st);
        if let Some(p) = &self.prom {
            p.zero_job_status();
        }
    }

    /// All resource locks released (`POST /api/v1/resources/unlock`).
    pub fn on_unlock_all(&self) {
        let mut st = self.state.lock();
        st.resources.clear();
        for data in st.by_topic.values_mut() {
            data.jobs.resources_locked_count = 0;
        }
    }

    /// One resource lock released by the admin endpoint.
    pub fn on_resource_unlocked(&self, resource: &str, topic: &str) {
        self.state.lock().resources.remove(resource);
        self.update_resources_locked(topic, -1);
    }

    /// No job for a pull request (`NotifyTopicEvent(EmptyPollReceived)`).
    pub fn empty_poll(&self, topic: &str) {
        if !self.enabled {
            return;
        }
        if topic != "*" {
            self.ensure_topic(topic);
        }
        if let Some(p) = &self.prom {
            p.app_events
                .with_label_values(&[ServiceEvent::EmptyPollReceived.name(), topic])
                .inc();
        }
    }

    /// Ten-second statistics (`UpdateJobStatistics`), only when metrics are enabled.
    pub fn update_job_statistics(&self, jobs: &[Arc<Job>], now_ms: i64) {
        let Some(p) = &self.prom else {
            return;
        };
        let topics = self.topics();
        p.job_active.reset();
        for t in &topics {
            p.job_active.with_label_values(&[t]).observe(0.0);
        }
        let mut per_topic: HashMap<&str, (Vec<i64>, Vec<i64>)> = topics
            .iter()
            .map(|t| (t.as_str(), (Vec::new(), Vec::new())))
            .collect();
        for j in jobs {
            if !j.is_completed() {
                let ms = now_ms - j.creation_timestamp();
                p.job_active
                    .with_label_values(&[&j.topic])
                    .observe(ms as f64 / 1000.0);
            }
            if let Some((active, completed)) = per_topic.get_mut(j.topic.as_str()) {
                if j.is_completed() {
                    completed.push(j.duration_first_last());
                } else {
                    active.push(now_ms - j.creation_timestamp());
                }
            }
        }
        p.job_percentiles.reset();
        let labels: Vec<String> = BUCKETS_PERCENTILES
            .iter()
            .map(|b| percentiles::label(*b))
            .collect();
        for (topic, (mut active, mut completed)) in per_topic {
            active.sort_unstable();
            completed.sort_unstable();
            for (status, values) in [("active", &active), ("completed", &completed)] {
                let ps = percentiles::percentiles(values, &BUCKETS_PERCENTILES);
                for (i, label) in labels.iter().enumerate() {
                    p.job_percentiles
                        .with_label_values(&[topic, status, label])
                        .set(ps[i] as f64 / 1000.0);
                }
            }
        }
    }

    pub fn topics(&self) -> Vec<String> {
        self.state.lock().topics.clone()
    }

    /// `GetTopicsStats`: topics in insertion order, with their share of all jobs.
    pub fn topics_stats(&self) -> Vec<TopicMetrics> {
        let st = self.state.lock();
        let mut stats: Vec<TopicMetrics> = st
            .topics
            .iter()
            .filter_map(|t| st.by_topic.get(t).map(|d| d.stats.clone()))
            .collect();
        let total: u64 = stats.iter().map(|s| s.total_jobs).sum();
        for s in &mut stats {
            s.percent_jobs = if total > 0 {
                s.total_jobs as f64 / total as f64 * 100.0
            } else {
                0.0
            };
        }
        stats
    }

    /// `GetResourcesMetrics`, sorted by resource name.
    pub fn resources_metrics(&self, now_secs: i64) -> Vec<ResourceMetrics> {
        let st = self.state.lock();
        let mut list: Vec<ResourceMetrics> = st
            .resources
            .values()
            .map(|r| ResourceMetrics {
                lock_duration: now_secs - r.start_time,
                ..r.clone()
            })
            .collect();
        list.sort_by(|a, b| a.resource_name.cmp(&b.resource_name));
        list
    }

    /// `GetJobMetricsByTopicMap`: topic → one-element list, as in Go.
    pub fn job_metrics_by_topic(&self) -> std::collections::BTreeMap<String, Vec<JobMetrics>> {
        let st = self.state.lock();
        st.by_topic
            .iter()
            .map(|(t, d)| (t.clone(), vec![d.jobs.clone()]))
            .collect()
    }

    /// `GetJobMetricsByStatus`: sums over topics.
    pub fn job_stats(&self) -> JobStatsMetrics {
        let st = self.state.lock();
        let mut s = JobStatsMetrics::default();
        for d in st.by_topic.values() {
            let m = &d.jobs;
            s.existing_jobs += m.jobs_existing;
            s.created_jobs += m.jobs_status_created;
            s.delayed_jobs += m.jobs_status_delayed;
            s.pending_jobs += m.jobs_status_pending;
            s.queued_jobs += m.jobs_status_queued;
            s.running_jobs += m.jobs_status_running;
            s.succeeded_jobs += m.jobs_status_succeeded;
            s.failed_jobs += m.jobs_status_failed;
            s.hidden_jobs += m.jobs_status_hidden;
            s.canceled_jobs += m.jobs_status_canceled;
            s.deleted_jobs += m.jobs_counter_deleted;
        }
        s
    }

    pub fn job_activity(&self) -> JobActivityMetrics {
        self.state.lock().activity.clone()
    }

    #[cfg(test)]
    fn job_metrics(&self, topic: &str) -> JobMetrics {
        self.state.lock().topic(topic).jobs.clone()
    }

    #[cfg(test)]
    fn topic_stats(&self, topic: &str) -> TopicMetrics {
        self.state.lock().topic(topic).stats.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::events;

    fn new_job(topic: &str, created: i64) -> Job {
        let mut j = Job {
            topic: topic.into(),
            ..Default::default()
        };
        j.init(Uuid::now_v7(), created);
        j
    }

    /// Applies an event to the job and notifies the metrics, like the service does.
    fn step(m: &ServiceMetrics, j: &mut Job, ev_type: &str, ts: i64, ev: ServiceEvent) {
        j.add_event(ev_type, ts);
        m.on_job_event(j, ev, j.previous_state(), j.state());
    }

    // Port of TestServiceMetrics_AddTopic / GetMetricByTopic / NewServiceMetrics.
    #[test]
    fn topics_registry() {
        let m = ServiceMetrics::new(true);
        assert!(m.topics().is_empty());
        m.ensure_topic("t");
        m.ensure_topic("t");
        assert_eq!(m.topics(), ["t"]);
        assert_eq!(m.job_metrics("t"), JobMetrics::default());
        assert!(!ServiceMetrics::new(false).enabled());
    }

    // Port of TestServiceMetrics_OnJobStateChange and _AllStates.
    #[test]
    fn state_changes() {
        let mut jm = JobMetrics::default();
        jm.on_state_change(JobState::Created, JobState::Created);
        assert_eq!(jm.jobs_status_created, 0);
        jm.on_state_change(JobState::NoState, JobState::Created);
        assert_eq!((jm.jobs_existing, jm.jobs_status_created), (1, 1));
        jm.on_state_change(JobState::Created, JobState::Running);
        assert_eq!(
            (
                jm.jobs_status_created,
                jm.jobs_status_running,
                jm.jobs_existing
            ),
            (0, 1, 1)
        );
        jm.on_state_change(JobState::Running, JobState::Deleted);
        assert_eq!(
            (
                jm.jobs_status_running,
                jm.jobs_counter_deleted,
                jm.jobs_existing
            ),
            (0, 1, 0)
        );
        // Saturates instead of wrapping around.
        jm.on_state_change(JobState::Hidden, JobState::Queued);
        assert_eq!((jm.jobs_status_hidden, jm.jobs_status_queued), (0, 1));
    }

    // Port of TestServiceMetrics_UpdateResourcesLockedCountMetric. Go v1.0.0 expected
    // 5 + (-2) = 7 (unsigned wrap-around); the port subtracts.
    #[test]
    fn resources_locked_count() {
        let m = ServiceMetrics::new(true);
        m.update_resources_locked("t", 5);
        assert_eq!(m.job_metrics("t").resources_locked_count, 5);
        m.update_resources_locked("t", -2);
        assert_eq!(m.job_metrics("t").resources_locked_count, 3);
        m.update_resources_locked("t", -10);
        assert_eq!(m.job_metrics("t").resources_locked_count, 0);
    }

    // Port of TestServiceMetrics_OnDeleteAllJobs / NotifyEvent.
    #[test]
    fn delete_all() {
        let m = ServiceMetrics::new(true);
        let mut j = new_job("t", 1);
        m.on_job_event(
            &j,
            ServiceEvent::JobCreated,
            JobState::NoState,
            JobState::Created,
        );
        step(&m, &mut j, events::PENDING, 2, ServiceEvent::JobPending);
        let e = new_job("", 3);
        m.on_job_event(
            &e,
            ServiceEvent::JobCreated,
            JobState::NoState,
            JobState::Created,
        );
        m.empty_poll("pulled");
        let series = |status: &str, topic: &str| {
            let text = String::from_utf8(m.prom().unwrap().encode().0).unwrap();
            let name = format!("job_status{{status=\"{status}\",topic=\"{topic}\"}} ");
            text.lines()
                .find_map(|l| l.strip_prefix(&name))
                .map(str::to_string)
        };
        assert_eq!(series("Pending", "t").as_deref(), Some("1"));
        m.on_delete_all();
        let jm = m.job_metrics("t");
        assert_eq!(
            (
                jm.jobs_existing,
                jm.jobs_status_pending,
                jm.jobs_counter_deleted
            ),
            (0, 0, 1)
        );
        assert_eq!(m.topic_stats("t").total_jobs, 0);
        // the Prometheus gauges follow (Go v1.1.2 kept the old values)
        assert_eq!(series("Pending", "t").as_deref(), Some("0"));
        assert_eq!(series("Existing", "t").as_deref(), Some("0"));
        assert_eq!(series("Created", "").as_deref(), Some("0"));
        assert_eq!(series("Existing", "pulled"), None);
    }

    // Port of TestServiceMetrics_NotifyJobEvent and _UpdateMetricsFromJobHistory.
    #[test]
    fn loaded_and_events() {
        let m = ServiceMetrics::new(true);
        let mut j = new_job("t", 10);
        j.add_event(events::PENDING, 10);
        j.add_event(events::ENQUEUE, 10);
        j.add_event(events::START, 10);
        m.on_job_loaded(&j);
        let jm = m.job_metrics("t");
        assert_eq!(
            (
                jm.jobs_existing,
                jm.jobs_status_created,
                jm.jobs_status_running
            ),
            (1, 0, 1)
        );
        assert_eq!(m.topic_stats("t").active_jobs, 1);
        // a loaded job is in the gauges at once (Go v1.1.2: after the next event)
        let text = String::from_utf8(m.prom().unwrap().encode().0).unwrap();
        assert!(text.contains("job_status{status=\"Running\",topic=\"t\"} 1\n"));
        assert!(text.contains("job_status{status=\"Existing\",topic=\"t\"} 1\n"));
        step(
            &m,
            &mut j,
            events::SUCCESS,
            2010,
            ServiceEvent::JobSucceeded,
        );
        let jm = m.job_metrics("t");
        assert_eq!((jm.jobs_status_running, jm.jobs_status_succeeded), (0, 1));
        let s = m.topic_stats("t");
        assert_eq!((s.active_jobs, s.total_jobs), (0, 1));
        assert_eq!(s.average_duration, 2.0);
        assert_eq!(s.success_rate, 1.0);
        let off = ServiceMetrics::new(false);
        off.on_job_event(
            &j,
            ServiceEvent::JobDeleted,
            JobState::Succeeded,
            JobState::Deleted,
        );
        assert_eq!(off.job_metrics("t"), JobMetrics::default());
    }

    // Port of TestServiceMetrics_GetTopicsStats_* and _Integration_TopicMetrics.
    #[test]
    fn topic_stats() {
        let m = ServiceMetrics::new(true);
        assert!(m.topics_stats().is_empty());
        for (topic, n, succeed) in [("a", 3, 2), ("b", 1, 0)] {
            for i in 0..n {
                let mut j = new_job(topic, 0);
                m.on_job_event(
                    &j,
                    ServiceEvent::JobCreated,
                    JobState::NoState,
                    JobState::Created,
                );
                step(&m, &mut j, events::PENDING, 0, ServiceEvent::JobPending);
                step(&m, &mut j, events::ENQUEUE, 0, ServiceEvent::JobEnqueued);
                step(&m, &mut j, events::START, 0, ServiceEvent::JobStarted);
                if i < succeed {
                    step(
                        &m,
                        &mut j,
                        events::SUCCESS,
                        1000,
                        ServiceEvent::JobSucceeded,
                    );
                } else if topic == "a" {
                    step(&m, &mut j, events::FAIL, 500, ServiceEvent::JobFailed);
                    step(
                        &m,
                        &mut j,
                        events::TERMINATE,
                        3000,
                        ServiceEvent::JobTerminated,
                    );
                }
            }
        }
        let stats = m.topics_stats();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].topic_name, "a");
        assert_eq!(stats[0].total_jobs, 3);
        assert_eq!(stats[0].percent_jobs, 75.0);
        assert!((stats[0].success_rate - 2.0 / 3.0).abs() < 1e-12);
        // (1000 + 1000 + 3000) ms / 3 = 1666 ms (integer division, as in Go).
        assert_eq!(stats[0].average_duration, 1.666);
        assert_eq!(stats[1].active_jobs, 1);
        assert_eq!(stats[1].success_rate, 0.0);
    }

    // Port of TestServiceMetrics_NotifyJobEvent_WithResources(JobFailed) and
    // TestServiceMetrics_GetResourcesMetrics.
    #[test]
    fn resources() {
        let m = ServiceMetrics::new(true);
        let mut j = new_job("t", 1000);
        j.lock_resources = Some(vec!["r1".into(), "r2".into()]);
        m.on_job_event(
            &j,
            ServiceEvent::JobCreated,
            JobState::NoState,
            JobState::Created,
        );
        step(&m, &mut j, events::PENDING, 1000, ServiceEvent::JobPending);
        step(&m, &mut j, events::ENQUEUE, 5000, ServiceEvent::JobEnqueued);
        let r = m.resources_metrics(15);
        assert_eq!(r.len(), 2);
        assert_eq!(
            (
                r[0].resource_name.as_str(),
                r[0].start_time,
                r[0].lock_duration
            ),
            ("r1", 5, 10)
        );
        step(&m, &mut j, events::START, 6000, ServiceEvent::JobStarted);
        step(&m, &mut j, events::FAIL, 7000, ServiceEvent::JobFailed);
        assert_eq!(m.resources_metrics(15).len(), 2);
        step(
            &m,
            &mut j,
            events::TERMINATE,
            7000,
            ServiceEvent::JobTerminated,
        );
        assert!(m.resources_metrics(15).is_empty());
    }

    // Port of TestServiceMetrics_GetJobMetricsByStatus / GetJobActivityMetrics /
    // GetJobMetricsByTopicMap.
    #[test]
    fn aggregates() {
        let m = ServiceMetrics::new(true);
        for t in ["a", "b"] {
            let mut j = new_job(t, 0);
            m.on_job_event(
                &j,
                ServiceEvent::JobCreated,
                JobState::NoState,
                JobState::Created,
            );
            step(&m, &mut j, events::PENDING, 0, ServiceEvent::JobPending);
            step(&m, &mut j, events::HIDE, 0, ServiceEvent::JobHidden);
        }
        let s = m.job_stats();
        assert_eq!((s.existing_jobs, s.hidden_jobs, s.pending_jobs), (2, 2, 0));
        let a = m.job_activity();
        assert_eq!((a.created, a.pending, a.hidden), (2, 2, 2));
        let by = m.job_metrics_by_topic();
        assert_eq!(by.len(), 2);
        assert_eq!(by["a"].len(), 1);
        assert_eq!(by["a"][0].jobs_status_hidden, 1);
    }

    // Port of TestServiceMetrics_UpdateJobStatistics / UpdateJobDurationPercentiles: runs
    // without errors and fills the gauges.
    #[test]
    fn statistics() {
        let m = ServiceMetrics::new(true);
        let mut j1 = new_job("t1", 0);
        m.on_job_event(
            &j1,
            ServiceEvent::JobCreated,
            JobState::NoState,
            JobState::Created,
        );
        step(&m, &mut j1, events::PENDING, 0, ServiceEvent::JobPending);
        let mut j2 = new_job("t2", 0);
        m.on_job_event(
            &j2,
            ServiceEvent::JobCreated,
            JobState::NoState,
            JobState::Created,
        );
        step(
            &m,
            &mut j2,
            events::SUCCESS,
            5000,
            ServiceEvent::JobSucceeded,
        );
        // A job whose topic is unknown to the metrics must not break the computation.
        let j3 = new_job("unknown", 0);
        let jobs = vec![Arc::new(j1), Arc::new(j2), Arc::new(j3)];
        m.update_job_statistics(&jobs, 100_000);
        m.update_job_statistics(&jobs, 105_000);
        let (text, _) = m.prom().unwrap().encode();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains(
            "job_duration_percentiles{percentile=\"p50\",status=\"active\",topic=\"t1\"} 105"
        ));
        assert!(text.contains(
            "job_duration_percentiles{percentile=\"p50\",status=\"completed\",topic=\"t2\"} 5"
        ));
        assert!(text.contains("job_active_duration_seconds_count{topic=\"t1\"} 2"));
        ServiceMetrics::new(false).update_job_statistics(&jobs, 0);
    }
}
