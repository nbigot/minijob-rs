//! Prometheus collectors, same names, labels and buckets as the Go server
//! (`metrics/prometheusmetrics.go`, `fiberprometheus/fiberprometheus.go`).

use prometheus::{
    CounterVec, Encoder, GaugeVec, HistogramOpts, HistogramVec, Opts, Registry, TextEncoder,
};

pub const BUCKETS_JOB_COMPLETION: [f64; 18] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 60.0, 300.0, 600.0,
    1800.0, 3600.0,
];
pub const BUCKETS_ACTIVE_JOB: [f64; 17] = [
    1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0, 7200.0, 14400.0,
    28800.0, 57600.0, 86400.0,
];
pub const BUCKETS_PERCENTILES: [f64; 9] = [0.05, 0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99, 1.0];
pub const BUCKETS_HTTP: [f64; 17] = [
    0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 20.0, 30.0,
    60.0,
];

/// Job and application collectors.
pub struct Prom {
    pub registry: Registry,
    pub app_events: CounterVec,
    pub job_events: CounterVec,
    pub job_status: GaugeVec,
    pub job_completion: HistogramVec,
    pub job_active: HistogramVec,
    pub job_percentiles: GaugeVec,
    pub http: HttpMetrics,
}

/// `http_*` collectors of the request middleware.
pub struct HttpMetrics {
    pub requests_total: CounterVec,
    pub request_duration: HistogramVec,
    pub in_progress: GaugeVec,
}

fn reg<C: prometheus::core::Collector + Clone + 'static>(r: &Registry, c: C) -> C {
    r.register(Box::new(c.clone()))
        .expect("metric names are unique");
    c
}

impl Prom {
    pub fn new() -> Prom {
        let r = Registry::new();
        let app_events = reg(
            &r,
            CounterVec::new(
                Opts::new("app_events", "Count all events by topic."),
                &["event", "topic"],
            )
            .expect("valid metric"),
        );
        let job_events = reg(
            &r,
            CounterVec::new(
                Opts::new("job_events", "Count all jobs by event and topic."),
                &["event", "topic"],
            )
            .expect("valid metric"),
        );
        let job_status = reg(
            &r,
            GaugeVec::new(
                Opts::new("job_status", "Jobs by status and topic"),
                &["status", "topic"],
            )
            .expect("valid metric"),
        );
        let job_completion = reg(
            &r,
            HistogramVec::new(
                HistogramOpts::new(
                    "job_completion_duration",
                    "Histogram tracking job execution time from start to completion in seconds.",
                )
                .buckets(BUCKETS_JOB_COMPLETION.to_vec()),
                &["topic"],
            )
            .expect("valid metric"),
        );
        let job_active = reg(
            &r,
            HistogramVec::new(
                HistogramOpts::new(
                    "job_active_duration_seconds",
                    "Distribution of active job durations in seconds by topic",
                )
                .buckets(BUCKETS_ACTIVE_JOB.to_vec()),
                &["topic"],
            )
            .expect("valid metric"),
        );
        let job_percentiles = reg(
            &r,
            GaugeVec::new(
                Opts::new(
                    "job_duration_percentiles",
                    "Percentiles of job duration in seconds by topic and by status (active, completed)",
                ),
                &["topic", "status", "percentile"],
            )
            .expect("valid metric"),
        );
        let http = HttpMetrics {
            requests_total: reg(
                &r,
                CounterVec::new(
                    Opts::new(
                        "http_requests_total",
                        "Count all http requests by status code, method and path.",
                    )
                    .const_label("service", "minijob"),
                    &["status_code", "method", "path"],
                )
                .expect("valid metric"),
            ),
            request_duration: reg(
                &r,
                HistogramVec::new(
                    HistogramOpts::new(
                        "http_request_duration_seconds",
                        "Duration of all HTTP requests by status code, method and path.",
                    )
                    .const_label("service", "minijob")
                    .buckets(BUCKETS_HTTP.to_vec()),
                    &["status_code", "method", "path"],
                )
                .expect("valid metric"),
            ),
            in_progress: reg(
                &r,
                GaugeVec::new(
                    Opts::new(
                        "http_requests_in_progress_total",
                        "All the requests in progress",
                    )
                    .const_label("service", "minijob"),
                    &["method"],
                )
                .expect("valid metric"),
            ),
        };
        Prom {
            registry: r,
            app_events,
            job_events,
            job_status,
            job_completion,
            job_active,
            job_percentiles,
            http,
        }
    }

    /// Text exposition format (`promhttp` without OpenMetrics).
    pub fn encode(&self) -> (Vec<u8>, String) {
        let encoder = TextEncoder::new();
        let mut buf = Vec::new();
        let _ = encoder.encode(&self.registry.gather(), &mut buf);
        (buf, encoder.format_type().to_string())
    }

    /// `SetGauges`: the `job_status` gauges of one topic.
    pub fn set_gauges(&self, topic: &str, m: &super::JobMetrics) {
        let pairs: [(&str, u64); 10] = [
            ("Existing", m.jobs_existing),
            ("Created", m.jobs_status_created),
            ("Delayed", m.jobs_status_delayed),
            ("Pending", m.jobs_status_pending),
            ("Queued", m.jobs_status_queued),
            ("Running", m.jobs_status_running),
            ("Succeeded", m.jobs_status_succeeded),
            ("Failed", m.jobs_status_failed),
            ("Hidden", m.jobs_status_hidden),
            ("Canceled", m.jobs_status_canceled),
        ];
        for (status, v) in pairs {
            self.job_status
                .with_label_values(&[status, topic])
                .set(v as f64);
        }
    }

    /// `ResetAllGauges`: every existing `job_status` series set to 0, labels kept. Topics
    /// without series (only pulled) get none. Go also dropped the series of the empty topic.
    pub fn zero_job_status(&self) {
        use prometheus::core::Collector;
        for family in self.job_status.collect() {
            for metric in family.get_metric() {
                let label = |name: &str| {
                    metric
                        .get_label()
                        .iter()
                        .find(|l| l.name() == name)
                        .map(|l| l.value().to_string())
                        .unwrap_or_default()
                };
                self.job_status
                    .with_label_values(&[label("status"), label("topic")])
                    .set(0.0);
            }
        }
    }
}

impl Default for Prom {
    fn default() -> Self {
        Prom::new()
    }
}
