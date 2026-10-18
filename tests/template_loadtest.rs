//! Web API with `config-templates/standard/config/config.loadtest.yaml` under concurrency:
//! every job is pulled and completed exactly once, no deadlock.

mod common;

use common::Server;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const T: &str = "standard/config/config.loadtest.yaml";

#[test]
fn concurrent_producers_and_consumers() {
    let s = Server::start(T);
    const PRODUCERS: usize = 4;
    const JOBS_PER_PRODUCER: usize = 250;
    const CONSUMERS: usize = 4;
    let total = PRODUCERS * JOBS_PER_PRODUCER;
    let created = Arc::new(Mutex::new(HashSet::new()));
    let completed = Arc::new(Mutex::new(Vec::new()));
    let mut threads = Vec::new();
    for p in 0..PRODUCERS {
        let url = s.url("/api/v1/job/");
        let created = created.clone();
        threads.push(std::thread::spawn(move || {
            let c = common::client();
            for i in 0..JOBS_PER_PRODUCER {
                let topic = if i % 2 == 0 {
                    "service.load_test.1"
                } else {
                    "service.load_test.2"
                };
                let body = format!(
                    r#"{{"topic":"{topic}","properties":{{"p":{p},"i":{i}}},"priority":{}}}"#,
                    i % 3
                );
                let r = c
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .unwrap();
                assert_eq!(r.status(), 200);
                let v: Value = r.json().unwrap();
                created
                    .lock()
                    .unwrap()
                    .insert(v["job"]["id"].as_str().unwrap().to_string());
            }
        }));
    }
    for _ in 0..CONSUMERS {
        let base = s.base.clone();
        let completed = completed.clone();
        threads.push(std::thread::spawn(move || {
            let c = common::client();
            let deadline = Instant::now() + Duration::from_secs(120);
            let mut idle = 0;
            while idle < 3 && Instant::now() < deadline {
                let r = c
                    .post(format!(
                        "{base}/api/v1/job/pull?topic=*&numjobs=10&waittimeseconds=1"
                    ))
                    .send()
                    .unwrap();
                if r.status() == 404 {
                    idle += 1;
                    continue;
                }
                idle = 0;
                let v: Value = r.json().unwrap();
                for j in v["jobs"].as_array().unwrap() {
                    let id = j["id"].as_str().unwrap().to_string();
                    let r = c
                        .post(format!("{base}/api/v1/job/{id}/succeed"))
                        .send()
                        .unwrap();
                    assert_eq!(r.status(), 200, "{id} completed twice?");
                    completed.lock().unwrap().push(id);
                }
            }
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    let completed = completed.lock().unwrap();
    let unique: HashSet<&String> = completed.iter().collect();
    assert_eq!(unique.len(), completed.len(), "a job was pulled twice");
    assert_eq!(completed.len(), total);
    assert_eq!(
        *created.lock().unwrap(),
        unique.into_iter().cloned().collect::<HashSet<String>>()
    );
    let stats = s.get("/api/v1/metrics/jobs/processing").json();
    assert_eq!(stats["jobStats"]["succeededJobs"], total);
    assert_eq!(stats["jobStats"]["existingJobs"], total);
    let r = s.get("/api/v1/jobs/?status=succeeded&limit=1").json();
    assert_eq!(r["total"], total);
}

#[test]
fn resources_are_exclusive_under_concurrency() {
    let s = Server::start(T);
    for i in 0..60 {
        s.create(&format!(
            r#"{{"topic":"res","lockResources":["shared","r{}"]}}"#,
            i % 5
        ));
    }
    let running = Arc::new(Mutex::new(HashSet::<String>::new()));
    let done = Arc::new(Mutex::new(0usize));
    let mut threads = Vec::new();
    for _ in 0..6 {
        let base = s.base.clone();
        let running = running.clone();
        let done = done.clone();
        threads.push(std::thread::spawn(move || {
            let c = common::client();
            let deadline = Instant::now() + Duration::from_secs(60);
            while *done.lock().unwrap() < 60 && Instant::now() < deadline {
                let r = c
                    .post(format!(
                        "{base}/api/v1/job/pull?topic=res&waittimeseconds=1"
                    ))
                    .send()
                    .unwrap();
                if r.status() == 404 {
                    continue;
                }
                let v: Value = r.json().unwrap();
                let id = v["jobs"][0]["id"].as_str().unwrap().to_string();
                {
                    let mut set = running.lock().unwrap();
                    assert!(set.is_empty(), "two jobs hold the shared resource");
                    set.insert(id.clone());
                }
                std::thread::sleep(Duration::from_millis(5));
                running.lock().unwrap().remove(&id);
                assert_eq!(
                    c.post(format!("{base}/api/v1/job/{id}/succeed"))
                        .send()
                        .unwrap()
                        .status(),
                    200
                );
                *done.lock().unwrap() += 1;
            }
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(*done.lock().unwrap(), 60);
}
