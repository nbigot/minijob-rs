//! Scenarios. Each one starts from an empty server (`DELETE /api/v1/jobs/`), so results do
//! not depend on the order or on what ran before.

use crate::client::Conn;
use crate::output::{Check, Latency, RunResult, ScenarioResult, ServerInfo};
use crate::stats::{
    ProcStat, Samples, TICKS_PER_S, cpu_percent, proc_stat, round3, self_cpu_ticks,
};
use bytes::Bytes;
use http::Method;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const SCENARIOS: &[&str] = &[
    "ping",
    "create",
    "pull-batch",
    "get",
    "list",
    "list-filter",
    "cycle",
];

pub struct Opts {
    pub url: String,
    pub addr: String,
    pub host: String,
    pub scenarios: Vec<String>,
    pub concurrency: usize,
    pub duration: u64,
    pub warmup: u64,
    pub preload: usize,
    pub batch_jobs: usize,
    pub batch_size: usize,
    pub pid: u32,
    pub label: String,
    pub wait: u64,
}

/// xorshift64*: cheap, deterministic per worker.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n.max(1)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Ping,
    Create,
    Get,
    List,
    ListFilter,
    Cycle,
}

/// Outcome of one item (one request, or one create/pull/succeed cycle).
struct Outcome {
    /// Last HTTP status, 0 when the connection failed.
    status: u16,
    ok: bool,
    requests: u64,
}

#[derive(Default)]
struct WorkerOut {
    samples: Samples,
    requests: u64,
    errors: u64,
    items: u64,
    statuses: BTreeMap<String, u64>,
    /// Successful items including the warm-up (for the consistency checks).
    ok_total: u64,
}

fn status_key(s: u16) -> String {
    if s == 0 {
        "error".to_string()
    } else {
        s.to_string()
    }
}

fn job_id(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    Some(v.pointer("/job/id")?.as_str()?.to_string())
}

async fn request(conn: &mut Conn, m: Method, path: &str, body: Option<String>) -> (u16, Bytes) {
    match conn.send(m, path, body.map(Bytes::from)).await {
        Ok(r) => (r.status, r.body),
        Err(_) => (0, Bytes::new()),
    }
}

async fn run_op(
    op: Op,
    o: &Opts,
    conn: &mut Conn,
    worker: usize,
    seq: u64,
    rng: &mut Rng,
    ids: &[String],
) -> Outcome {
    let one = |status: u16| Outcome {
        status,
        ok: status == 200,
        requests: 1,
    };
    match op {
        Op::Ping => one(request(conn, Method::GET, "/ping", None).await.0),
        Op::Create => {
            let body = format!(
                r#"{{"topic":"bench","priority":{},"properties":{{"worker":{worker},"seq":{seq},"data":"0123456789abcdef0123456789abcdef"}}}}"#,
                seq % 10
            );
            one(request(conn, Method::POST, "/api/v1/job/", Some(body))
                .await
                .0)
        }
        Op::Get => {
            let id = &ids[rng.below(ids.len() as u64) as usize];
            one(
                request(conn, Method::GET, &format!("/api/v1/job/{id}"), None)
                    .await
                    .0,
            )
        }
        Op::List => {
            let pages = (o.preload / 100).max(1) as u64;
            let path = format!("/api/v1/jobs/?page={}&limit=100", 1 + rng.below(pages));
            one(request(conn, Method::GET, &path, None).await.0)
        }
        Op::ListFilter => {
            let path = format!(
                "/api/v1/jobs/?status=queued&topic=t{}&sort=priority_desc&limit=20",
                rng.below(10)
            );
            one(request(conn, Method::GET, &path, None).await.0)
        }
        Op::Cycle => {
            let topic = format!("cycle-{worker}");
            let body = format!(r#"{{"topic":"{topic}","properties":{{"seq":{seq}}}}}"#);
            let (s, _) = request(conn, Method::POST, "/api/v1/job/", Some(body)).await;
            let mut requests = 1;
            if s != 200 {
                return Outcome {
                    status: s,
                    ok: false,
                    requests,
                };
            }
            // The job is queued by the server's scheduler; pull until it is there.
            let pull = format!("/api/v1/job/pull?topic={topic}&visibilitytimeout=60");
            let mut id = None;
            for _ in 0..1000 {
                let (s, b) = request(conn, Method::POST, &pull, None).await;
                requests += 1;
                match s {
                    200 => {
                        let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
                        id = v
                            .pointer("/jobs/0/id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        break;
                    }
                    404 => continue,
                    s => {
                        return Outcome {
                            status: s,
                            ok: false,
                            requests,
                        };
                    }
                }
            }
            let Some(id) = id else {
                return Outcome {
                    status: 404,
                    ok: false,
                    requests,
                };
            };
            let path = format!("/api/v1/job/{id}/succeed");
            let (s, _) = request(conn, Method::POST, &path, None).await;
            Outcome {
                status: s,
                ok: s == 200,
                requests: requests + 1,
            }
        }
    }
}

fn cpu_per_request(p0: Option<ProcStat>, p1: Option<ProcStat>, requests: u64) -> Option<f64> {
    let (a, b) = (p0?, p1?);
    if requests == 0 {
        return None;
    }
    let us = (b.cpu_ticks.saturating_sub(a.cpu_ticks)) as f64 * 1e6 / TICKS_PER_S;
    Some(round3(us / requests as f64))
}

fn finish(
    name: &str,
    mut all: WorkerOut,
    elapsed: f64,
    p0: Option<ProcStat>,
    p1: Option<ProcStat>,
    client: (Option<u64>, Option<u64>),
) -> ScenarioResult {
    let latency: Latency = all.samples.summary();
    ScenarioResult {
        name: name.to_string(),
        requests: all.requests,
        errors: all.errors,
        elapsed_s: round3(elapsed),
        requests_per_s: round3(all.requests as f64 / elapsed),
        items: all.items,
        items_per_s: round3(all.items as f64 / elapsed),
        latency_ms: latency,
        statuses: std::mem::take(&mut all.statuses),
        server_cpu_us_per_request: cpu_per_request(p0, p1, all.requests),
        server_rss_kb: p1.map(|p| p.rss_kb),
        client_cpu_percent: cpu_percent(client.0, client.1, elapsed),
        check: None,
    }
}

fn merge(outs: Vec<WorkerOut>) -> WorkerOut {
    let mut all = WorkerOut::default();
    for w in outs {
        all.samples.merge(w.samples);
        all.requests += w.requests;
        all.errors += w.errors;
        all.items += w.items;
        all.ok_total += w.ok_total;
        for (k, v) in w.statuses {
            *all.statuses.entry(k).or_default() += v;
        }
    }
    all
}

/// Closed loop: `concurrency` workers, warm-up then a measured window of `duration`.
async fn timed(o: &Arc<Opts>, name: &str, op: Op, ids: Arc<Vec<String>>) -> (ScenarioResult, u64) {
    let start = Instant::now();
    let rec_start = start + Duration::from_secs(o.warmup);
    let end = rec_start + Duration::from_secs(o.duration);
    let mut handles = Vec::with_capacity(o.concurrency);
    for w in 0..o.concurrency {
        let o = o.clone();
        let ids = ids.clone();
        handles.push(tokio::spawn(async move {
            let mut conn = Conn::new(&o.addr, &o.host);
            let mut rng = Rng::new(w as u64 + 1);
            let mut out = WorkerOut::default();
            let mut seq = 0u64;
            loop {
                let t = Instant::now();
                if t >= end {
                    break;
                }
                let r = run_op(op, &o, &mut conn, w, seq, &mut rng, &ids).await;
                seq += 1;
                if r.ok {
                    out.ok_total += 1;
                }
                if t >= rec_start {
                    out.samples.push(t.elapsed());
                    out.requests += r.requests;
                    if r.ok {
                        out.items += 1;
                    } else {
                        out.errors += 1;
                    }
                    *out.statuses.entry(status_key(r.status)).or_default() += 1;
                }
            }
            out
        }));
    }
    tokio::time::sleep_until(rec_start.into()).await;
    let p0 = proc_stat(o.pid);
    let c0 = self_cpu_ticks();
    tokio::time::sleep_until(end.into()).await;
    let p1 = proc_stat(o.pid);
    let c1 = self_cpu_ticks();
    let mut outs = Vec::new();
    for h in handles {
        outs.push(h.await.expect("worker"));
    }
    let all = merge(outs);
    let ok_total = all.ok_total;
    (
        finish(name, all, o.duration as f64, p0, p1, (c0, c1)),
        ok_total,
    )
}

/// A count read with [`total`], or "unknown".
fn count_text(n: Option<u64>) -> String {
    n.map_or_else(|| "unknown".to_string(), |n| n.to_string())
}

/// Total number of jobs matching a `GET /api/v1/jobs/` query.
async fn total(conn: &mut Conn, query: &str) -> Option<u64> {
    let (s, b) = request(
        conn,
        Method::GET,
        &format!("/api/v1/jobs/?{query}&limit=1"),
        None,
    )
    .await;
    if s != 200 {
        return None;
    }
    let v: Value = serde_json::from_slice(&b).ok()?;
    v.get("total")?.as_u64()
}

async fn wait_total(conn: &mut Conn, query: &str, expected: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if total(conn, query).await == Some(expected) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn reset(o: &Opts) -> Result<(), String> {
    let mut conn = Conn::new(&o.addr, &o.host);
    let (s, _) = request(&mut conn, Method::DELETE, "/api/v1/jobs/", None).await;
    if s != 200 {
        return Err(format!("DELETE /api/v1/jobs/ answered {s}"));
    }
    if !wait_total(&mut conn, "", 0).await {
        return Err("jobs remain after DELETE /api/v1/jobs/".into());
    }
    Ok(())
}

/// Creates `n` jobs (not measured) and returns their ids in creation order per worker.
async fn preload(
    o: &Arc<Opts>,
    n: usize,
    topic: fn(usize) -> String,
) -> Result<Vec<String>, String> {
    let mut handles = Vec::new();
    for w in 0..o.concurrency {
        let o = o.clone();
        handles.push(tokio::spawn(async move {
            let mut conn = Conn::new(&o.addr, &o.host);
            let mut ids = Vec::new();
            let mut i = w;
            while i < n {
                let body = format!(
                    r#"{{"topic":"{}","priority":{},"properties":{{"seq":{i}}}}}"#,
                    topic(i),
                    i % 10
                );
                let (s, b) = request(&mut conn, Method::POST, "/api/v1/job/", Some(body)).await;
                match (s, job_id(&b)) {
                    (200, Some(id)) => ids.push(id),
                    _ => return Err(format!("preload: create answered {s}")),
                }
                i += o.concurrency;
            }
            Ok(ids)
        }));
    }
    let mut ids = Vec::with_capacity(n);
    for h in handles {
        ids.extend(h.await.map_err(|e| e.to_string())??);
    }
    Ok(ids)
}

async fn pull_batch(o: &Arc<Opts>) -> Result<ScenarioResult, String> {
    let ids = preload(o, o.batch_jobs, |_| "batch".to_string()).await?;
    let mut conn = Conn::new(&o.addr, &o.host);
    if !wait_total(&mut conn, "status=queued&topic=batch", ids.len() as u64).await {
        return Err("pull-batch: the preloaded jobs were not all queued after 60 s".into());
    }
    let p0 = proc_stat(o.pid);
    let c0 = self_cpu_ticks();
    let start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..o.concurrency {
        let o = o.clone();
        handles.push(tokio::spawn(async move {
            let mut conn = Conn::new(&o.addr, &o.host);
            let mut out = WorkerOut::default();
            let mut pulled: Vec<String> = Vec::new();
            let path = format!(
                "/api/v1/job/pull?topic=batch&numjobs={}&visibilitytimeout=300",
                o.batch_size
            );
            let mut failures = 0;
            loop {
                let t = Instant::now();
                let (s, b) = request(&mut conn, Method::POST, &path, None).await;
                out.samples.push(t.elapsed());
                out.requests += 1;
                *out.statuses.entry(status_key(s)).or_default() += 1;
                match s {
                    200 => {
                        let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
                        let jobs = v.get("jobs").and_then(Value::as_array);
                        for j in jobs.into_iter().flatten() {
                            if let Some(id) = j.get("id").and_then(Value::as_str) {
                                pulled.push(id.to_string());
                                out.items += 1;
                            }
                        }
                    }
                    // Queue empty: this worker is done.
                    404 => break,
                    _ => {
                        out.errors += 1;
                        failures += 1;
                        if failures >= 10 {
                            break;
                        }
                    }
                }
            }
            (out, pulled)
        }));
    }
    let mut outs = Vec::new();
    let mut pulled = Vec::new();
    for h in handles {
        let (out, p) = h.await.map_err(|e| e.to_string())?;
        outs.push(out);
        pulled.extend(p);
    }
    let elapsed = start.elapsed().as_secs_f64();
    let p1 = proc_stat(o.pid);
    let c1 = self_cpu_ticks();
    let mut r = finish("pull-batch", merge(outs), elapsed, p0, p1, (c0, c1));
    // Every preloaded job must be pulled exactly once.
    let expected: HashSet<&String> = ids.iter().collect();
    let mut seen = HashSet::new();
    let mut duplicates = 0;
    let mut unknown = 0;
    for id in &pulled {
        if !seen.insert(id) {
            duplicates += 1;
        } else if !expected.contains(id) {
            unknown += 1;
        }
    }
    let missing = expected.iter().filter(|id| !seen.contains(**id)).count();
    r.check = Some(Check {
        ok: duplicates == 0 && missing == 0 && unknown == 0,
        detail: format!(
            "{} jobs created, {} pulled, {duplicates} duplicates, {missing} missing, {unknown} unknown",
            ids.len(),
            pulled.len()
        ),
    });
    Ok(r)
}

async fn server_version(o: &Opts) -> String {
    let mut conn = Conn::new(&o.addr, &o.host);
    let (_, b) = request(&mut conn, Method::GET, "/api/v1/system/info", None).await;
    let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
    v.pointer("/systemInfo/version")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

pub fn print_line(r: &ScenarioResult) {
    let cpu = r
        .server_cpu_us_per_request
        .map(|c| format!("{c:>8.1} µs/req"))
        .unwrap_or_else(|| "       - µs/req".into());
    let check = r
        .check
        .as_ref()
        .map(|c| {
            format!(
                "  check {}: {}",
                if c.ok { "OK" } else { "FAILED" },
                c.detail
            )
        })
        .unwrap_or_default();
    println!(
        "{:<12} {:>10.1} req/s {:>10.1} items/s  p50 {:>8.3} ms  p99 {:>8.3} ms  cpu {cpu}  errors {}{check}",
        r.name, r.requests_per_s, r.items_per_s, r.latency_ms.p50, r.latency_ms.p99, r.errors
    );
}

pub async fn run(o: Opts) -> Result<RunResult, String> {
    let o = Arc::new(o);
    let mut conn = Conn::new(&o.addr, &o.host);
    let deadline = Instant::now() + Duration::from_secs(o.wait);
    let mut s = request(&mut conn, Method::GET, "/ping", None).await.0;
    while s != 200 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        s = request(&mut conn, Method::GET, "/ping", None).await.0;
    }
    if s != 200 {
        return Err(format!("{}/ping answered {}", o.url, status_key(s)));
    }
    let version = server_version(&o).await;
    reset(&o).await?;
    let idle = proc_stat(o.pid);
    let mut result = RunResult {
        tool: "minijob-bench".into(),
        implementation: "rust".into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        label: o.label.clone(),
        url: o.url.clone(),
        started: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        concurrency: o.concurrency,
        duration_s: o.duration,
        warmup_s: o.warmup,
        server: ServerInfo {
            version,
            pid: o.pid,
            idle_rss_kb: idle.map(|p| p.rss_kb),
            peak_rss_kb: None,
        },
        scenarios: Vec::new(),
    };
    let none: Arc<Vec<String>> = Arc::new(Vec::new());
    let mut read_ids: Option<Arc<Vec<String>>> = None;
    for name in o.scenarios.clone() {
        let reads = matches!(name.as_str(), "get" | "list" | "list-filter");
        if !reads && read_ids.take().is_some() {
            reset(&o).await?;
        }
        let r = match name.as_str() {
            "ping" => timed(&o, "ping", Op::Ping, none.clone()).await.0,
            "create" => {
                let (mut r, created) = timed(&o, "create", Op::Create, none.clone()).await;
                let stored = total(&mut conn, "").await;
                r.check = Some(Check {
                    ok: stored == Some(created),
                    detail: format!("{created} jobs created, {} stored", count_text(stored)),
                });
                reset(&o).await?;
                r
            }
            "pull-batch" => {
                let r = pull_batch(&o).await?;
                reset(&o).await?;
                r
            }
            "get" | "list" | "list-filter" => {
                if read_ids.is_none() {
                    let ids = preload(&o, o.preload, |i| format!("t{}", i % 10)).await?;
                    if !wait_total(&mut conn, "status=queued", ids.len() as u64).await {
                        return Err("read preload: jobs not all queued after 60 s".into());
                    }
                    read_ids = Some(Arc::new(ids));
                }
                let ids = read_ids.clone().expect("preloaded");
                let op = match name.as_str() {
                    "get" => Op::Get,
                    "list" => Op::List,
                    _ => Op::ListFilter,
                };
                timed(&o, &name, op, ids).await.0
            }
            "cycle" => {
                let (mut r, cycles) = timed(&o, "cycle", Op::Cycle, none.clone()).await;
                let done = total(&mut conn, "status=succeeded").await;
                r.check = Some(Check {
                    ok: done == Some(cycles),
                    detail: format!(
                        "{cycles} cycles completed, {} jobs succeeded",
                        count_text(done)
                    ),
                });
                reset(&o).await?;
                r
            }
            other => return Err(format!("unknown scenario {other}")),
        };
        print_line(&r);
        result.scenarios.push(r);
    }
    if read_ids.is_some() {
        reset(&o).await?;
    }
    result.server.peak_rss_kb = proc_stat(o.pid).map(|p| p.hwm_kb);
    Ok(result)
}
