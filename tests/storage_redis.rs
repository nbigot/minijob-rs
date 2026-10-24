//! Redis backend. Runs only when `MINIJOB_TEST_REDIS_URL` is set (e.g.
//! `redis://127.0.0.1:6379`); uses database 9, which it empties.
#![cfg(feature = "redis")]

mod common;

use common::Server;
use redis::Commands;
use serde_json::Value;
use std::time::Duration;

const T: &str = "standard/config/config.inmemory.yaml";
const DB: u32 = 9;
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/go-inmemory-jobs.json"
);

fn base_url() -> Option<String> {
    std::env::var("MINIJOB_TEST_REDIS_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .map(|u| u.trim_end_matches('/').to_string())
}

fn conn(base: &str) -> redis::Connection {
    redis::Client::open(format!("{base}/{DB}"))
        .unwrap()
        .get_connection()
        .unwrap()
}

fn start(base: &str, max_jobs: u64) -> Server {
    let url = format!(
        "{base}/3?db={DB}&dial_timeout=3&read_timeout=6s&max_retries=2&client_name=minijob"
    );
    Server::start_with(T, |c| {
        c.replace("type: \"inMemory\"", "type: \"redis\"")
            .replace("        url: \"\"", &format!("        url: \"{url}\""))
            .replace(
                "        maxJobs: 100000\n        finishedJobTTL: 86400",
                &format!("        maxJobs: {max_jobs}\n        finishedJobTTL: 86400"),
            )
    })
}

#[test]
fn redis_backend() {
    let Some(base) = base_url() else {
        eprintln!("MINIJOB_TEST_REDIS_URL not set, skipped");
        return;
    };
    let _lock = REDIS.lock().unwrap_or_else(|e| e.into_inner());
    let mut r = conn(&base);
    let _: () = redis::cmd("FLUSHDB").query(&mut r).unwrap();

    // Keys written by the Go server are loaded (same JSON as its in-memory file).
    let go = std::fs::read_to_string(FIXTURE).unwrap();
    let fixture: Value = serde_json::from_str(&go).unwrap();
    for (id, job) in fixture["jobs"].as_object().unwrap() {
        let _: () = r.set(format!("job:{id}"), job.to_string()).unwrap();
    }
    let mut s = start(&base, 1000);
    assert_eq!(
        s.get("/api/v1/system/resources").json()["resourceUsage"]["backend"]["value"],
        "Redis"
    );
    assert_eq!(
        s.get("/api/v1/jobs/all").json()["jobs"]
            .as_array()
            .unwrap()
            .len(),
        7
    );
    assert_eq!(s.get("/readyz").status, 200);
    assert_eq!(s.delete("/api/v1/jobs/").status, 200);
    let keys: Vec<String> = r.keys("job:*").unwrap();
    assert!(keys.is_empty(), "{keys:?}");

    // Each change is written before the answer, with the Go JSON format.
    let id = s.create(r#"{"topic":"rd","properties":{"a":1}}"#);
    let stored: String = r.get(format!("job:{id}")).unwrap();
    let mut api = s.job(&id);
    api.as_object_mut().unwrap().remove("state");
    let stored_v: Value = serde_json::from_str(&stored).unwrap();
    // The pending → queued transition may already be stored.
    assert_eq!(stored_v["id"], api["id"]);
    assert!(stored.starts_with(&format!(
        "{{\"id\":\"{id}\",\"topic\":\"rd\",\"priority\":0,\"properties\":{{\"a\":1}},\"history\":["
    )));
    s.wait_state(&id, "queued", Duration::from_secs(3));
    assert_eq!(s.pull("topic=rd"), std::slice::from_ref(&id));
    let stored: Value =
        serde_json::from_str(&r.get::<_, String>(format!("job:{id}")).unwrap()).unwrap();
    assert_eq!(
        stored["history"].as_array().unwrap().last().unwrap()["eventType"],
        "START"
    );
    let ttl: i64 = r.ttl(format!("job:{id}")).unwrap();
    assert_eq!(ttl, -1);
    assert_eq!(s.post(&format!("/api/v1/job/{id}/succeed")).status, 200);
    let ttl: i64 = r.ttl(format!("job:{id}")).unwrap();
    assert!(ttl > 86000 && ttl <= 86400, "finishedJobTTL: {ttl}");

    let other = s.create(r#"{"topic":"rd"}"#);
    assert_eq!(s.delete(&format!("/api/v1/job/{other}")).status, 200);
    assert!(!r.exists::<_, bool>(format!("job:{other}")).unwrap());

    // Restart: jobs come back from Redis.
    let kept = s.create(r#"{"topic":"keep","delay":3600}"#);
    assert_eq!(s.shutdown().code(), Some(0));
    s.spawn();
    assert_eq!(s.state(&kept), "delayed");
    assert_eq!(s.state(&id), "succeeded");
    drop(s);

    // backend.redis.maxJobs is enforced (Go ignored it).
    let _: () = redis::cmd("FLUSHDB").query(&mut r).unwrap();
    let s = start(&base, 2);
    s.create("{}");
    s.create("{}");
    let resp = s.post_json("/api/v1/job/", "{}");
    assert_eq!(resp.status, 503);
    assert_eq!(resp.json()["code"], 1002);
    let _: () = redis::cmd("FLUSHDB").query(&mut r).unwrap();
}

#[test]
fn unreachable_redis_prevents_start() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = common::prepare_config(T, dir.path(), 1, |c| {
        c.replace("type: \"inMemory\"", "type: \"redis\"").replace(
            "        url: \"\"",
            "        url: \"redis://127.0.0.1:1/0?dial_timeout=1&max_retries=-1\"",
        )
    });
    let path = dir.path().join("c.yaml");
    std::fs::write(&path, cfg).unwrap();
    let (st, out, _) = common::run_binary(&["-config", path.to_str().unwrap()]);
    assert_eq!(st.code(), Some(1), "{out}");
    assert!(out.contains("cannot load jobs"), "{out}");
    let cfg = common::prepare_config(T, dir.path(), 1, |c| {
        c.replace("type: \"inMemory\"", "type: \"redis\"")
            .replace("        url: \"\"", "        url: \"redis://h/?unknown=1\"")
    });
    std::fs::write(&path, cfg).unwrap();
    let (st, out, _) = common::run_binary(&["-config", path.to_str().unwrap()]);
    assert_eq!(st.code(), Some(1));
    assert!(
        out.contains("error while parsing redis URL: redis: unexpected option: unknown"),
        "{out}"
    );
}

/// Serializes the tests that change settings of the Redis server.
static REDIS: std::sync::Mutex<()> = std::sync::Mutex::new(());

const USER: &str = "minijob-rs-test";

/// Resets the server settings changed by a test, even when it fails.
struct Restore(String);

impl Drop for Restore {
    fn drop(&mut self) {
        let mut r = conn(&self.0);
        let _: redis::RedisResult<()> = redis::cmd("CONFIG")
            .arg("SET")
            .arg("maxmemory")
            .arg("0")
            .query(&mut r);
        let _: redis::RedisResult<()> = redis::cmd("ACL").arg("DELUSER").arg(USER).query(&mut r);
        let _: redis::RedisResult<()> = redis::cmd("FLUSHDB").query(&mut r);
    }
}

/// The test user, allowed everything but `denied` commands.
fn acl(r: &mut redis::Connection, denied: &[&str]) {
    let _: () = redis::cmd("ACL")
        .arg("SETUSER")
        .arg(USER)
        .arg("reset")
        .arg("on")
        .arg(">pw")
        .arg("~*")
        .arg("+@all")
        .arg(denied.iter().map(|c| format!("-{c}")).collect::<Vec<_>>())
        .query(r)
        .unwrap();
}

fn max_memory(r: &mut redis::Connection, bytes: &str) {
    let _: () = redis::cmd("CONFIG")
        .arg("SET")
        .arg("maxmemory")
        .arg(bytes)
        .arg("maxmemory-policy")
        .arg("noeviction")
        .query(r)
        .unwrap();
}

/// The URL of the test user, with RESP3.
fn user_url(base: &str) -> String {
    let host = base.trim_start_matches("redis://");
    format!("redis://{USER}:pw@{host}/{DB}?protocol=3&client_name=minijob")
}

fn redis_config(url: &str, max_jobs: u64) -> impl FnOnce(String) -> String {
    let url = url.to_string();
    move |c: String| {
        c.replace("type: \"inMemory\"", "type: \"redis\"")
            .replace("        url: \"\"", &format!("        url: \"{url}\""))
            .replace(
                "        maxJobs: 100000\n        finishedJobTTL: 86400",
                &format!("        maxJobs: {max_jobs}\n        finishedJobTTL: 86400"),
            )
    }
}

/// Runs the server until it exits (a start that fails); returns its exit code and output.
fn run_failing(url: &str, max_jobs: u64) -> (Option<i32>, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.yaml");
    let cfg = common::prepare_config(T, dir.path(), 1, redis_config(url, max_jobs));
    std::fs::write(&path, cfg).unwrap();
    let (st, out, _) = common::run_binary(&["-config", path.to_str().unwrap()]);
    (st.code(), out)
}

/// Failures of Redis: at startup (a job that is not JSON, a command refused, `maxJobs: 0`),
/// writes refused (retried in the background, `/readyz` 503 until they succeed, a deletion of
/// every job applied in order), a failing ping, and writes still failing at shutdown.
#[test]
fn redis_failures() {
    let Some(base) = base_url() else {
        eprintln!("MINIJOB_TEST_REDIS_URL not set, skipped");
        return;
    };
    let _lock = REDIS.lock().unwrap_or_else(|e| e.into_inner());
    let _restore = Restore(base.clone());
    let mut r = conn(&base);
    let _: () = redis::cmd("FLUSHDB").query(&mut r).unwrap();
    let plain = format!("{base}/{DB}");

    let _: () = r.set("job:bad", "{").unwrap();
    let (code, out) = run_failing(&plain, 10);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("Error load job from JSON"), "{out}");
    assert!(out.contains("cannot load job"), "{out}");
    let _: () = r.del("job:bad").unwrap();
    acl(&mut r, &["scan"]);
    let (code, out) = run_failing(&user_url(&base), 10);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("Error getting job keys from Redis"), "{out}");
    let (code, out) = run_failing(&plain, 0);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains("invalid value for configuration backend.redis.maxJobs: 0"),
        "{out}"
    );

    // Without CONFIG (managed Redis), a warning only; user, password and RESP3 from the URL.
    acl(&mut r, &["config"]);
    let mut s = Server::start_with(T, redis_config(&user_url(&base), 100));
    assert!(
        s.log().contains(
            r#""message":"Cannot enable Redis keyspace notifications","app":"minijob","backend.provider":"inMemory","topic":"backendProvider","method":"Init","error":"NOPERM User minijob-rs-test has no permissions to run the 'config|set' command""#
        ),
        "{}",
        s.log()
    );
    let first = s.create(r#"{"topic":"f"}"#);
    assert!(r.exists::<_, bool>(format!("job:{first}")).unwrap());

    // Writes refused: the jobs live in memory, /readyz reports 503 until Redis accepts them.
    max_memory(&mut r, "1");
    let lost = s.create(r#"{"topic":"f"}"#);
    assert_eq!(s.get("/readyz").status, 503);
    assert!(!r.exists::<_, bool>(format!("job:{lost}")).unwrap());
    assert_eq!(s.delete("/api/v1/jobs/").status, 200);
    let kept = s.create(r#"{"topic":"f"}"#);
    max_memory(&mut r, "0");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while s.get("/readyz").status != 200 {
        assert!(std::time::Instant::now() < deadline, "{}", s.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let keys: Vec<String> = r.keys("job:*").unwrap();
    assert_eq!(keys, [format!("job:{kept}")]);
    assert!(
        s.log().contains(
            r#""message":"Error saving job to Redis","app":"minijob","backend.provider":"inMemory","topic":"backendProvider","method":"SaveJob","error":"OOM command not allowed when used memory > 'maxmemory'.""#
        ),
        "{}",
        s.log()
    );

    // PING refused: not ready.
    let _: () = redis::cmd("ACL")
        .arg("SETUSER")
        .arg(USER)
        .arg("-ping")
        .query(&mut r)
        .unwrap();
    assert_eq!(s.get("/readyz").status, 503);
    assert!(
        s.log().contains(
            r#""message":"Error pinging Redis","app":"minijob","backend.provider":"inMemory","error":"NOPERM User minijob-rs-test has no permissions to run the 'ping' command""#
        ),
        "{}",
        s.log()
    );
    let _: () = redis::cmd("ACL")
        .arg("SETUSER")
        .arg(USER)
        .arg("+ping")
        .query(&mut r)
        .unwrap();
    assert_eq!(s.get("/readyz").status, 200);

    // Writes still failing at shutdown are abandoned after 5 s.
    max_memory(&mut r, "1");
    s.create(r#"{"topic":"f"}"#);
    assert_eq!(s.shutdown().code(), Some(0));
    assert!(
        s.log().contains(
            r#""message":"Pending Redis writes not flushed at shutdown","app":"minijob","backend.provider":"inMemory","topic":"backendProvider","method":"Stop""#
        ),
        "{}",
        s.log()
    );
}
