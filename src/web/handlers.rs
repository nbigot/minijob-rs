//! HTTP handlers (`web/apijobs.go`, `apiresources.go`, `apisystem.go`, `apiutils.go`).

use super::App;
use super::http::{Req, Resp, json, status_text, text, with_metric};
use crate::apierror::{ApiError, ValidationError, codes};
use crate::model::{JobResponse, JobState, UuidStr, now_ms};
use crate::service::PullRequest;
use crate::service::query::GetJobsRequest;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

fn api_err(e: &ApiError) -> Resp {
    json(e.http, e)
}

#[derive(Serialize)]
struct Success {
    code: u16,
    message: &'static str,
}

const SUCCESS: Success = Success {
    code: 200,
    message: "success",
};

fn success() -> Resp {
    json(200, &SUCCESS)
}

/// `GetUintParameterFromQuery`: `strconv.ParseUint(s, 10, 32)` then range checks.
pub fn uint_param(
    req: &Req,
    name: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, ApiError> {
    let s = req.query(name);
    if s.is_empty() {
        return Ok(default);
    }
    let value = (s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse::<u32>().ok())
        .flatten()
        .map(u64::from)
        .ok_or_else(|| {
            ApiError::new(
                400,
                codes::INVALID_PARAMETER_VALUE,
                format!("invalid value for query parameter: {name}"),
            )
        })?;
    if value < min {
        return Err(ApiError::new(
            400,
            codes::INVALID_PARAMETER_VALUE,
            format!(
                "invalid value for query parameter: {name}, found {s}, minimum allowed value is {min}"
            ),
        ));
    }
    if value > max {
        return Err(ApiError::new(
            400,
            codes::INVALID_PARAMETER_VALUE,
            format!(
                "invalid value for query parameter: {name}, found {s}, maximum allowed value is {max}"
            ),
        ));
    }
    Ok(value)
}

fn invalid_uuid(value: &str) -> ApiError {
    ApiError::new(400, codes::INVALID_JOB_UUID, "invalid job uuid").validation(vec![
        ValidationError {
            failed_field: "jobuuid".into(),
            tag: "parameter".into(),
            value: value.to_string(),
        },
    ])
}

/// `GetJobUUIDFromParameter`.
pub fn uuid_param(req: &Req) -> Result<Uuid, ApiError> {
    let raw = req.param("jobuuid");
    crate::model::uuid::parse_go(raw).map_err(|_| invalid_uuid(raw))
}

/// `GetJobUUIDFromQuery`: absent or empty → `None`.
pub fn uuid_query(req: &Req) -> Result<Option<Uuid>, ApiError> {
    let raw = req.query("jobuuid");
    if raw.is_empty() {
        return Ok(None);
    }
    crate::model::uuid::parse_go(raw)
        .map(Some)
        .map_err(|_| invalid_uuid(raw))
}

macro_rules! tri {
    ($e:expr, $metric:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return with_metric(api_err(&e), $metric),
        }
    };
}

// ------------------------------------------------------------------ utils

pub async fn home(_: Arc<App>, _: Req) -> Resp {
    text(200, "Welcome to MiniJob!")
}

pub async fn ping(_: Arc<App>, _: Req) -> Resp {
    status_text(200)
}

pub async fn healthcheck(app: Arc<App>, _: Req) -> Resp {
    status_text(if app.svc.healthcheck().await {
        200
    } else {
        503
    })
}

pub async fn livez(_: Arc<App>, _: Req) -> Resp {
    status_text(200)
}

pub async fn readyz(app: Arc<App>, _: Req) -> Resp {
    healthcheck_status(&app).await
}

async fn healthcheck_status(app: &App) -> Resp {
    status_text(if app.svc.healthcheck().await {
        200
    } else {
        503
    })
}

pub async fn metrics(app: Arc<App>, req: Req) -> Resp {
    let Some(p) = app.svc.metrics().prom() else {
        return status_text(404);
    };
    let (body, _) = p.encode();
    let ctype = "text/plain; version=0.0.4; charset=utf-8; escaping=underscores";
    let gzip = req
        .header("accept-encoding")
        .split(',')
        .any(|e| e.trim().split(';').next() == Some("gzip"));
    if gzip {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        if enc.write_all(&body).is_ok()
            && let Ok(compressed) = enc.finish()
        {
            let mut r = super::http::response(200, ctype, compressed);
            r.headers_mut().insert(
                http::header::CONTENT_ENCODING,
                http::HeaderValue::from_static("gzip"),
            );
            return r;
        }
    }
    super::http::response(200, ctype, body)
}

pub async fn monitor(app: Arc<App>, req: Req) -> Resp {
    app.monitor.handle(&req, app.open_connections())
}

pub async fn docs(_: Arc<App>, req: Req) -> Resp {
    super::swagger::handle(&req)
}

// ------------------------------------------------------------------ jobs

#[derive(Serialize)]
struct JobResult<'a> {
    code: u16,
    message: &'static str,
    job: JobResponse<'a>,
}

#[derive(Serialize)]
struct JobsResult<'a> {
    code: u16,
    message: &'static str,
    jobs: Vec<JobResponse<'a>>,
}

pub async fn get_job(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "GetJob";
    let id = tri!(uuid_param(&req), M);
    match app.svc.get_job(id) {
        Some(j) => with_metric(
            json(
                200,
                &JobResult {
                    code: 200,
                    message: "success",
                    job: JobResponse(&j),
                },
            ),
            M,
        ),
        None => with_metric(
            api_err(&ApiError::new(404, codes::JOB_UUID_NOT_FOUND, "job not found").job(id)),
            M,
        ),
    }
}

/// `Content-Type` must be `application/json` (parameters such as `charset` are accepted,
/// Go compared the whole header).
fn is_json_content_type(v: &str) -> bool {
    v.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
}

/// Fiber `c.Body()`: decodes `Content-Encoding` (gzip, deflate, br). Unsupported encodings
/// give an invalid body, as in Fiber.
fn decoded_body(req: &Req) -> Vec<u8> {
    use std::io::Read;
    let enc = req.header("content-encoding");
    let mut body = req.body.to_vec();
    if enc.is_empty() {
        return body;
    }
    for e in enc
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .rev()
    {
        let mut out = Vec::new();
        let ok = match e.to_ascii_lowercase().as_str() {
            "gzip" | "x-gzip" => flate2::read::GzDecoder::new(&body[..])
                .read_to_end(&mut out)
                .is_ok(),
            "deflate" => flate2::read::ZlibDecoder::new(&body[..])
                .read_to_end(&mut out)
                .is_ok(),
            "br" | "brotli" => brotli_decompressor::Decompressor::new(&body[..], 4096)
                .read_to_end(&mut out)
                .is_ok(),
            "identity" => {
                out = body.clone();
                true
            }
            _ => return b"Unsupported Media Type".to_vec(),
        };
        if !ok {
            return b"unexpected EOF".to_vec();
        }
        body = out;
    }
    body
}

pub async fn create_job(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "CreateJob";
    if !is_json_content_type(req.header("content-type")) {
        return with_metric(
            api_err(&ApiError::new(
                400,
                codes::INVALID_CONTENT_TYPE,
                "invalid content type (must be application/json)",
            )),
            M,
        );
    }
    let payload = decoded_body(&req);
    if let Some(schema) = &app.schema {
        let Ok(doc) = crate::gojson::decode::parse_value(&payload) else {
            return with_metric(
                api_err(&ApiError::new(
                    400,
                    codes::CANT_DESERIALIZE_JSON,
                    "invalid json body format",
                )),
                M,
            );
        };
        let errors = schema.validate(&doc);
        if !errors.is_empty() {
            let list = errors
                .into_iter()
                .map(|e| ValidationError {
                    failed_field: e.property_path,
                    tag: e.message,
                    value: crate::gojson::fmt_v(&e.invalid_value),
                })
                .collect();
            return with_metric(
                api_err(
                    &ApiError::new(400, codes::INVALID_JSON_DATA, "invalid json content")
                        .validation(list),
                ),
                M,
            );
        }
    }
    let job = tri!(app.svc.create_job(&payload).await, M);
    with_metric(
        json(
            200,
            &JobResult {
                code: 200,
                message: "success",
                job: JobResponse(&job),
            },
        ),
        M,
    )
}

pub async fn clone_job(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "CloneJob";
    let id = tri!(uuid_param(&req), M);
    let job = tri!(app.svc.clone_job(id).await, M);
    with_metric(
        json(
            200,
            &JobResult {
                code: 200,
                message: "success",
                job: JobResponse(&job),
            },
        ),
        M,
    )
}

pub async fn delete_job(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "DeleteJob";
    let id = tri!(uuid_param(&req), M);
    tri!(app.svc.delete_job(id).await, M);
    with_metric(success(), M)
}

fn pull_request(app: &App, req: &Req) -> Result<PullRequest, ApiError> {
    let jobs = &app.cfg.jobs;
    let mut num_jobs = uint_param(req, "numjobs", 1, 1, 100)?;
    let visibility_timeout = uint_param(
        req,
        "visibilitytimeout",
        jobs.default_visibility_timeout,
        1,
        jobs.max_visibility_timeout,
    )?;
    let wait_time_seconds = uint_param(req, "waittimeseconds", 0, 0, 60)?;
    let topic = req.query("topic").to_string();
    let job_uuid = uuid_query(req)?;
    if job_uuid.is_some() {
        num_jobs = 1;
    }
    Ok(PullRequest {
        job_uuid,
        topic,
        num_jobs,
        visibility_timeout,
        wait_time_seconds,
    })
}

pub async fn pull_job(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "PullJob";
    let pr = tri!(pull_request(&app, &req), M);
    let jobs = tri!(app.svc.pull_jobs(&pr).await, M);
    with_metric(
        json(
            200,
            &JobsResult {
                code: 200,
                message: "success",
                jobs: jobs.iter().map(|j| JobResponse(j)).collect(),
            },
        ),
        M,
    )
}

pub async fn succeed_job(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "SetJobAsSuccessful";
    let id = tri!(uuid_param(&req), M);
    tri!(app.svc.succeed(id).await, M);
    with_metric(success(), M)
}

pub async fn cancel_job(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "CancelJob";
    let id = tri!(uuid_param(&req), M);
    tri!(app.svc.cancel(id).await, M);
    with_metric(success(), M)
}

pub async fn fail_job(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "FailJob";
    let id = tri!(uuid_param(&req), M);
    let max = tri!(app.svc.fail(id).await, M);
    let message = if max {
        "job reached max retry"
    } else {
        "success"
    };
    with_metric(json(200, &Success { code: 200, message }), M)
}

pub async fn visibility_timeout(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "ChangeVisibilityTimeoutJob";
    let id = tri!(uuid_param(&req), M);
    let jobs = &app.cfg.jobs;
    let vt = tri!(
        uint_param(
            &req,
            "visibilitytimeout",
            jobs.default_visibility_timeout,
            1,
            jobs.max_visibility_timeout
        ),
        M
    );
    tri!(app.svc.change_visibility(id, vt).await, M);
    with_metric(success(), M)
}

#[derive(Serialize)]
struct GetJobsResult<'a> {
    code: u16,
    message: &'static str,
    jobs: Vec<JobResponse<'a>>,
    total: u64,
    page: u64,
    limit: u64,
    totalpages: u64,
}

pub async fn get_jobs(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "GetJobs";
    let page = tri!(uint_param(&req, "page", 1, 1, 10000), M);
    let limit = tri!(uint_param(&req, "limit", 10, 1, 1000), M);
    let r = app.svc.get_jobs(&GetJobsRequest {
        page,
        limit,
        status: req.query("status").to_string(),
        topic: req.query("topic").to_string(),
        sort: req.query("sort").to_string(),
        search: req.query("search").to_string(),
    });
    with_metric(
        json(
            200,
            &GetJobsResult {
                code: 200,
                message: "success",
                jobs: r.jobs.iter().map(|j| JobResponse(j)).collect(),
                total: r.total,
                page: r.page,
                limit: r.limit,
                totalpages: r.total_pages,
            },
        ),
        M,
    )
}

pub async fn get_all_jobs(app: Arc<App>, _: Req) -> Resp {
    let jobs = app.svc.all_jobs();
    with_metric(
        json(
            200,
            &JobsResult {
                code: 200,
                message: "success",
                jobs: jobs.iter().map(|j| JobResponse(j)).collect(),
            },
        ),
        "GetAllJobs",
    )
}

#[derive(Serialize)]
struct OldJob {
    jobuuid: String,
    duration: i64,
}

#[derive(Serialize)]
struct OldJobsResult {
    code: u16,
    message: &'static str,
    topics: BTreeMap<String, Vec<OldJob>>,
}

pub async fn get_oldest_jobs(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "GetOldestJobs";
    let limit = tri!(uint_param(&req, "limit", 10, 1, 100), M);
    let min_duration = tri!(uint_param(&req, "duration", 0, 0, 86400 * 365), M);
    let min_ms = min_duration as i64 * 1000;
    let now = now_ms();
    let mut topics: BTreeMap<String, Vec<(i64, Uuid)>> = BTreeMap::new();
    for j in app.svc.all_jobs() {
        if !j.is_completed() {
            let d = now - j.creation_timestamp();
            if d >= min_ms {
                topics.entry(j.topic.clone()).or_default().push((d, j.id));
            }
        }
    }
    let topics = topics
        .into_iter()
        .map(|(t, mut list)| {
            list.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            list.truncate(limit as usize);
            let list = list
                .into_iter()
                .map(|(d, id)| OldJob {
                    jobuuid: id.to_string(),
                    duration: d,
                })
                .collect();
            (t, list)
        })
        .collect();
    with_metric(
        json(
            200,
            &OldJobsResult {
                code: 200,
                message: "success",
                topics,
            },
        ),
        M,
    )
}

#[derive(Serialize)]
struct JobDuration {
    id: String,
    topic: String,
    state: &'static str,
    created: i64,
    duration: u64,
}

#[derive(Serialize)]
struct JobDurationList {
    code: u16,
    message: &'static str,
    jobs: Vec<JobDuration>,
}

/// Queued or running jobs whose age (ms) satisfies `keep`, sorted by duration.
fn duration_list(app: &App, keep: impl Fn(i64) -> bool, descending: bool) -> Vec<JobDuration> {
    let now = now_ms();
    let mut list: Vec<JobDuration> = app
        .svc
        .all_jobs()
        .into_iter()
        .filter(|j| matches!(j.state(), JobState::Running | JobState::Queued))
        .filter_map(|j| {
            let d = now - j.creation_timestamp();
            keep(d).then(|| JobDuration {
                id: j.id.to_string(),
                topic: j.topic.clone(),
                state: j.state().as_str(),
                created: j.creation_timestamp(),
                duration: d as u64,
            })
        })
        .collect();
    if descending {
        list.sort_by_key(|j| std::cmp::Reverse(j.duration));
    } else {
        list.sort_by_key(|j| j.duration);
    }
    list
}

pub async fn get_stalled_jobs(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "GetStalledJobs";
    let minutes = tri!(uint_param(&req, "duration", 60, 1, 10080), M);
    let min = minutes as i64 * 60 * 1000;
    let jobs = duration_list(&app, |d| d >= min, true);
    with_metric(
        json(
            200,
            &JobDurationList {
                code: 200,
                message: "success",
                jobs,
            },
        ),
        M,
    )
}

pub async fn get_recent_jobs(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "GetRecentJobs";
    let minutes = tri!(uint_param(&req, "duration", 10, 1, 10080), M);
    let max = minutes as i64 * 60 * 1000;
    let jobs = duration_list(&app, |d| d <= max, false);
    with_metric(
        json(
            200,
            &JobDurationList {
                code: 200,
                message: "success",
                jobs,
            },
        ),
        M,
    )
}

pub async fn delete_queued_jobs(app: Arc<App>, _: Req) -> Resp {
    const M: &str = "DeleteQueuedJobs";
    tri!(app.svc.delete_queued().await, M);
    with_metric(success(), M)
}

pub async fn delete_all_jobs(app: Arc<App>, _: Req) -> Resp {
    const M: &str = "DeleteAllJobs";
    tri!(app.svc.delete_all().await, M);
    with_metric(success(), M)
}

#[derive(Serialize)]
struct TopicsResult {
    code: u16,
    message: &'static str,
    topics: Vec<String>,
}

pub async fn get_topics(app: Arc<App>, _: Req) -> Resp {
    let topics = app.svc.metrics().topics();
    with_metric(
        json(
            200,
            &TopicsResult {
                code: 200,
                message: "success",
                topics,
            },
        ),
        "GetTopics",
    )
}

// ------------------------------------------------------------------ resources

#[derive(Serialize)]
struct ResourcesResult {
    code: u16,
    message: &'static str,
    resources: BTreeMap<String, UuidStr>,
}

pub async fn get_locked_resources(app: Arc<App>, _: Req) -> Resp {
    let resources = app
        .svc
        .locked_resources()
        .into_iter()
        .map(|(k, v)| (k, UuidStr(v)))
        .collect();
    with_metric(
        json(
            200,
            &ResourcesResult {
                code: 200,
                message: "success",
                resources,
            },
        ),
        "GetLockedResources",
    )
}

pub async fn unlock_all_resources(app: Arc<App>, _: Req) -> Resp {
    app.svc.unlock_all_resources().await;
    with_metric(success(), "UnlockAllResources")
}

/// The resource name comes from `?resourcename=` (Go read only the query) or, when absent,
/// from the path parameter (Go ignored it).
pub async fn unlock_resource(app: Arc<App>, req: Req) -> Resp {
    const M: &str = "UnlockResource";
    let from_query = req.query("resourcename");
    let name = if from_query.is_empty() {
        percent_encoding::percent_decode_str(req.param("resourcename"))
            .decode_utf8_lossy()
            .into_owned()
    } else {
        from_query.to_string()
    };
    tri!(app.svc.unlock_resource(&name).await, M);
    with_metric(success(), M)
}

// ------------------------------------------------------------------ admin, system

pub async fn admin_shutdown(app: Arc<App>, _: Req) -> Resp {
    app.request(super::Control::Shutdown);
    success()
}

pub async fn admin_restart(app: Arc<App>, _: Req) -> Resp {
    app.request(super::Control::Restart);
    success()
}

#[derive(Serialize)]
struct SystemInfo {
    version: String,
    uptime: u64,
    hostname: String,
    environment: String,
}

#[derive(Serialize)]
struct SystemInfoResult {
    code: u16,
    message: &'static str,
    #[serde(rename = "systemInfo")]
    system_info: SystemInfo,
}

pub async fn system_info(app: Arc<App>, _: Req) -> Resp {
    json(
        200,
        &SystemInfoResult {
            code: 200,
            message: "success",
            system_info: SystemInfo {
                version: app.svc.version().to_string(),
                uptime: app.svc.uptime_secs(),
                hostname: app.cfg.webserver.http.address.clone(),
                environment: app.cfg.environment.clone(),
            },
        },
    )
}

pub async fn system_health(app: Arc<App>, _: Req) -> Resp {
    healthcheck_status(&app).await
}

#[derive(Serialize)]
struct LabelValue {
    label: &'static str,
    value: String,
}

#[derive(Serialize)]
struct ResourceUsage {
    memory: LabelValue,
    disk: LabelValue,
    backend: LabelValue,
}

#[derive(Serialize)]
struct SystemResourcesResult {
    code: u16,
    message: &'static str,
    #[serde(rename = "resourceUsage")]
    resource_usage: ResourceUsage,
}

/// Memory is the resident memory of the process (Go reported its heap size).
pub async fn system_resources(app: Arc<App>, _: Req) -> Resp {
    let mb = 1024 * 1024;
    json(
        200,
        &SystemResourcesResult {
            code: 200,
            message: "success",
            resource_usage: ResourceUsage {
                memory: LabelValue {
                    label: "Memory Usage",
                    value: format!("{}MB", app.monitor.process_memory() / mb),
                },
                disk: LabelValue {
                    label: "Disk Usage",
                    value: format!("{}MB", app.svc.backend().disk_usage() / mb),
                },
                backend: LabelValue {
                    label: "Backend Provider",
                    value: app.svc.backend().kind().to_string(),
                },
            },
        },
    )
}

// ------------------------------------------------------------------ metrics routes

#[derive(Serialize)]
struct JobStatsResult {
    code: u16,
    message: &'static str,
    #[serde(rename = "jobStats")]
    job_stats: crate::metrics::JobStatsMetrics,
}

pub async fn jobs_metrics(app: Arc<App>, _: Req) -> Resp {
    with_metric(
        json(
            200,
            &JobStatsResult {
                code: 200,
                message: "success",
                job_stats: app.svc.metrics().job_stats(),
            },
        ),
        "GetJobsMetrics",
    )
}

#[derive(Serialize)]
struct ActivityResult {
    code: u16,
    message: &'static str,
    #[serde(rename = "jobActivity")]
    job_activity: crate::metrics::JobActivityMetrics,
}

pub async fn jobs_activity(app: Arc<App>, _: Req) -> Resp {
    json(
        200,
        &ActivityResult {
            code: 200,
            message: "success",
            job_activity: app.svc.metrics().job_activity(),
        },
    )
}

#[derive(Serialize)]
struct TopicMetricsResult {
    code: u16,
    message: &'static str,
    metrics: BTreeMap<String, Vec<crate::metrics::JobMetrics>>,
}

pub async fn jobs_metrics_topics(app: Arc<App>, _: Req) -> Resp {
    with_metric(
        json(
            200,
            &TopicMetricsResult {
                code: 200,
                message: "success",
                metrics: app.svc.metrics().job_metrics_by_topic(),
            },
        ),
        "GetJobsMetricsTopics",
    )
}

#[derive(Serialize)]
struct ResourcesStatsResult {
    code: u16,
    message: &'static str,
    resources: Vec<crate::metrics::ResourceMetrics>,
}

pub async fn resources_metrics(app: Arc<App>, _: Req) -> Resp {
    let now = now_ms() / 1000;
    with_metric(
        json(
            200,
            &ResourcesStatsResult {
                code: 200,
                message: "success",
                resources: app.svc.metrics().resources_metrics(now),
            },
        ),
        "GetResourcesMetrics",
    )
}

#[derive(Serialize)]
struct TopicsStatsResult {
    code: u16,
    message: &'static str,
    topics: Vec<crate::metrics::TopicMetrics>,
}

pub async fn topics_stats(app: Arc<App>, _: Req) -> Resp {
    with_metric(
        json(
            200,
            &TopicsStatsResult {
                code: 200,
                message: "success",
                topics: app.svc.metrics().topics_stats(),
            },
        ),
        "GetTopicsStats",
    )
}
