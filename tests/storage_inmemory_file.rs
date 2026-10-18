//! In-memory backend with persistence: reads the file written by the Go server
//! (`tests/fixtures/go-inmemory-jobs.json`) and writes the same bytes back.

mod common;

use common::{Server, prepare_config};
use serde_json::Value;
use serde_json::value::RawValue;
use std::collections::BTreeMap;
use std::time::Duration;

const T: &str = "standard/config/config.inmemory.yaml";
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/go-inmemory-jobs.json"
);
/// startAfter of the delayed job in the fixture (one hour after it was written).
const DELAYED_START: &str = "\"startAfter\":1792168961201";
const FAR_FUTURE: &str = "\"startAfter\":4102444800000";

fn raw_jobs(text: &str) -> BTreeMap<String, String> {
    #[derive(serde::Deserialize)]
    struct F<'a> {
        #[serde(borrow)]
        jobs: BTreeMap<String, &'a RawValue>,
    }
    let f: F = serde_json::from_str(text).unwrap();
    f.jobs
        .into_iter()
        .map(|(k, v)| (k, v.get().to_string()))
        .collect()
}

fn by_name(jobs: &Value, name: &str) -> Value {
    jobs.as_array()
        .unwrap()
        .iter()
        .find(|j| j["name"] == name)
        .unwrap_or_else(|| panic!("{name} missing"))
        .clone()
}

/// Starts on a copy of the Go file (the delayed job is moved far into the future so that
/// the test does not depend on the date it runs).
fn start_on_fixture() -> (Server, String) {
    let dir = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = prepare_config(T, dir.path(), port, |c| {
        c.replace(
            "enablePersistentStorage: false",
            "enablePersistentStorage: true",
        )
        .replace(
            "retentionPolicy:\n        enable: true",
            "retentionPolicy:\n        enable: false",
        )
    });
    let go = std::fs::read_to_string(FIXTURE)
        .unwrap()
        .replace(DELAYED_START, FAR_FUTURE);
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    std::fs::write(dir.path().join("data/jobs.json"), &go).unwrap();
    (Server::start_config(dir, port, &config), go)
}

#[test]
fn reads_go_file_and_rewrites_the_same_bytes() {
    let (mut s, go) = start_on_fixture();
    let all = s.get("/api/v1/jobs/all").json()["jobs"].clone();
    assert_eq!(all.as_array().unwrap().len(), 7);
    let go_jobs: BTreeMap<String, Value> = raw_jobs(&go)
        .into_iter()
        .map(|(k, v)| (k, serde_json::from_str(&v).unwrap()))
        .collect();
    // Jobs that nothing changes after the load: same content as written by Go.
    for name in ["queued", "succeeded", "canceled-requeued", "delayed"] {
        let mut j = by_name(&all, name);
        let state = j.as_object_mut().unwrap().remove("state").unwrap();
        assert_eq!(j, go_jobs[j["id"].as_str().unwrap()], "{name}");
        let expected = match name {
            "canceled-requeued" => "queued",
            n => n,
        };
        assert_eq!(state, expected, "{name}");
    }
    let running = by_name(&all, "running");
    assert_eq!(
        running["properties"]["big"],
        serde_json::json!(12345678901234567000u64)
    );
    assert_eq!(
        running["properties"]["s"].as_str().unwrap().chars().next(),
        Some('<')
    );
    // The running job keeps its lock until it times out (its visibility timeout expired
    // long ago, so the scheduler fails it at the first tick and the blocked job proceeds).
    let blocked = by_name(&all, "pending-blocked")["id"]
        .as_str()
        .unwrap()
        .to_string();
    s.wait_state(&blocked, "queued", Duration::from_secs(5));
    // The hidden job is retried right after the load.
    let hidden = by_name(&all, "hidden")["id"].as_str().unwrap().to_string();
    s.wait_state(&hidden, "queued", Duration::from_secs(5));
    assert_eq!(s.shutdown().code(), Some(0));
    let rust = std::fs::read_to_string(s.data_dir().join("jobs.json")).unwrap();
    let (go_raw, rust_raw) = (raw_jobs(&go), raw_jobs(&rust));
    assert_eq!(
        go_raw.keys().collect::<Vec<_>>(),
        rust_raw.keys().collect::<Vec<_>>()
    );
    for name in ["queued", "succeeded", "canceled-requeued", "delayed"] {
        let id = by_name(&all, name)["id"].as_str().unwrap().to_string();
        assert_eq!(rust_raw[&id], go_raw[&id], "bytes of {name}");
    }
    // Keys are sorted like Go's map encoding.
    let ids: Vec<&String> = rust_raw.keys().collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    assert!(rust.starts_with("{\"jobs\":{\""));
}

#[test]
fn created_job_is_scheduled_after_load() {
    // A job saved between CREATE and PENDING: Go never scheduled it (B23).
    let dir = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = prepare_config(T, dir.path(), port, |c| {
        c.replace(
            "enablePersistentStorage: false",
            "enablePersistentStorage: true",
        )
    });
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    std::fs::write(
        dir.path().join("data/jobs.json"),
        r#"{"jobs":{"01a14561-34ff-7e10-bc6b-9f3f1d47ec47":{"id":"01a14561-34ff-7e10-bc6b-9f3f1d47ec47","topic":"c","priority":0,"properties":null,"history":[{"eventType":"CREATE","timestamp":1792165360895}],"lockResources":null,"userAgent":"","requester":"","name":"","sessionId":"","traceId":"","visibilityTimeout":0,"startAfter":0,"callbackUrl":""}}}"#,
    )
    .unwrap();
    let s = Server::start_config(dir, port, &config);
    s.wait_state(
        "01a14561-34ff-7e10-bc6b-9f3f1d47ec47",
        "queued",
        Duration::from_secs(5),
    );
}

#[test]
fn invalid_file_prevents_start() {
    let dir = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = prepare_config(T, dir.path(), port, |c| {
        c.replace(
            "enablePersistentStorage: false",
            "enablePersistentStorage: true",
        )
    });
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    std::fs::write(dir.path().join("data/jobs.json"), "{\"jobs\":").unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, config).unwrap();
    let (st, out, _) = common::run_binary(&["-config", cfg.to_str().unwrap()]);
    assert_eq!(st.code(), Some(1), "{out}");
    assert!(out.contains("Error while loading jobs"));
}
