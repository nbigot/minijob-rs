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
