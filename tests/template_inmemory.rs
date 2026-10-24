//! Web API with `config-templates/standard/config/config.inmemory.yaml` (no persistence, no
//! schema, metrics/monitor/swagger on, constant retry backoff of 2 ms, 3 retries).

mod common;

use common::{Server, run_binary};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const T: &str = "standard/config/config.inmemory.yaml";
const ZERO: &str = "00000000-0000-0000-0000-000000000000";

fn start() -> Server {
    Server::start(T)
}

#[test]
fn utils_and_fallbacks() {
    let s = start();
    let r = s.get("/");
    assert_eq!((r.status, r.text().as_str()), (200, "Welcome to MiniJob!"));
    assert_eq!(r.header("content-type"), "text/plain; charset=utf-8");
    for p in [
        "/ping",
        "/healthcheck",
        "/livez",
        "/readyz",
        "/api/v1/system/health",
    ] {
        let r = s.get(p);
        assert_eq!((r.status, r.text().as_str()), (200, "OK"), "{p}");
    }
    // Healthcheck middleware: GET only, no 405.
    assert_eq!(s.head("/livez").status, 404);
    let r = s.post("/livez");
    assert_eq!((r.status, r.text().as_str()), (404, "Cannot POST /livez"));
    let r = s.get("/nope");
    assert_eq!((r.status, r.text().as_str()), (404, "Cannot GET /nope"));
    let r = s.get("/api/v1/topics");
    assert_eq!(r.status, 404, "strict routing");
    let r = s.get("/API/v1/topics/");
    assert_eq!(r.status, 404, "case sensitive");
    let r = s.post("/api/v1/jobs/all");
    assert_eq!((r.status, r.text().as_str()), (405, "Method Not Allowed"));
    assert_eq!(r.header("allow"), "GET, HEAD");
    let r = s.send(s.request("OPTIONS", "/api/v1/job/"));
    assert_eq!((r.status, r.header("allow")), (405, "POST"));
    let r = s.send(s.request("FOO", "/ping"));
    assert_eq!((r.status, r.text().as_str()), (400, "Invalid http method"));
    // GET on the pull path reaches the `:jobuuid` route, like in Fiber.
    let r = s.get("/api/v1/job/pull");
    assert_eq!(r.status, 400);
    assert_eq!(
        r.json(),
        json!({"error":"invalid job uuid","code":1000,"jobUUID":ZERO,
               "validationErrors":[{"failedField":"jobuuid","tag":"parameter","value":"pull"}]})
    );
    let r = s.head("/api/v1/topics/");
    assert_eq!((r.status, r.header("content-length")), (200, "44"));
    assert!(r.body.is_empty());
}

#[test]
fn job_lifecycle() {
    let s = start();
    let r = s.post_json(
        "/api/v1/job/",
        r#"{"topic":"t","priority":3,"properties":{"a":1.0,"b":12345678901234567890,"c":1e-7,"h":"<&>"},
            "lockResources":["r1"],"userAgent":"ua","requester":"rq","name":"n","sessionId":"s","traceId":"tr","callbackUrl":""}"#,
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), "application/json");
    let text = r.text();
    let bs = char::from(92);
    let escaped = format!("\"h\":\"{bs}u003c{bs}u0026{bs}u003e\"");
    assert!(
        text.contains(r#""properties":{"a":1,"b":12345678901234567890,"c":1e-7,"#),
        "{text}"
    );
    assert!(text.contains(&escaped), "{text}");
    let body = r.json();
    assert_eq!(
        (body["code"].clone(), body["message"].clone()),
        (json!(200), json!("success"))
    );
    let job = &body["job"];
    let id = job["id"].as_str().unwrap().to_string();
    assert_eq!(job["state"], "pending");
    let keys: Vec<&str> = job
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.len(), 15);
    let events: Vec<&str> = job["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["eventType"].as_str().unwrap())
        .collect();
    assert_eq!(events, ["CREATE", "PENDING"]);
    s.wait_state(&id, "queued", Duration::from_secs(3));
    assert_eq!(s.pull("topic=other"), Vec::<String>::new());
    let pulled = s.pull("topic=t&visibilitytimeout=120");
    assert_eq!(pulled, std::slice::from_ref(&id));
    let j = s.job(&id);
    assert_eq!(
        (j["state"].as_str(), j["visibilityTimeout"].as_u64()),
        (Some("running"), Some(120))
    );
    let r = s.post(&format!("/api/v1/job/{id}/succeed"));
    assert_eq!(r.json(), json!({"code":200,"message":"success"}));
    assert_eq!(s.state(&id), "succeeded");
    let r = s.post(&format!("/api/v1/job/{id}/succeed"));
    assert_eq!(r.status, 400);
    assert_eq!(
        r.json(),
        json!({"error":"job is not running","code":1009,"jobUUID":id})
    );
    // Go-compatible UUID forms.
    let upper = id.to_uppercase();
    assert_eq!(s.get(&format!("/api/v1/job/{upper}")).status, 200);
    assert_eq!(s.get(&format!("/api/v1/job/urn:uuid:{id}")).status, 200);
    assert_eq!(
        s.get(&format!("/api/v1/job/{}", id.replace('-', "")))
            .status,
        200
    );
    let unknown = "0190e951-c29a-70aa-ba59-18be4abe97a2";
    let r = s.get(&format!("/api/v1/job/{unknown}"));
    assert_eq!(r.status, 404);
    assert_eq!(
        r.json(),
        json!({"error":"job not found","code":1001,"jobUUID":unknown})
    );
    let r = s.get("/api/v1/job/x%20y");
    assert_eq!(r.json()["validationErrors"][0]["value"], "x%20y");
}

#[test]
fn create_errors() {
    let s = Server::start_with(T, |c| c.replace("maxAllowedJobs: 0", "maxAllowedJobs: 2"));
    let r = s.send(s.request("POST", "/api/v1/job/").body("{}"));
    assert_eq!(r.status, 400);
    assert_eq!(
        r.json(),
        json!({"error":"invalid content type (must be application/json)","code":1024,"jobUUID":ZERO})
    );
    // Parameters of the media type are accepted (Go compared the whole header).
    let r = s.send(
        s.request("POST", "/api/v1/job/")
            .header("Content-Type", "Application/JSON; charset=utf-8")
            .body("null"),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["job"]["properties"], Value::Null);
    assert_eq!(r.json()["job"]["lockResources"], Value::Null);
    for bad in [
        "{",
        "[1]",
        r#"{"priority":200}"#,
        r#"{"priority":"1"}"#,
        r#"{"properties":[]}"#,
        "",
    ] {
        let r = s.post_json("/api/v1/job/", bad);
        assert_eq!(r.status, 400, "{bad}");
        assert_eq!(
            r.json(),
            json!({"error":"cannot create job from request","code":1002,"jobUUID":ZERO}),
            "{bad}"
        );
    }
    // Go matches field names case-insensitively.
    let r = s.post_json("/api/v1/job/", r#"{"TOPIC":"x","Priority":-128}"#);
    assert_eq!(
        (
            r.json()["job"]["topic"].clone(),
            r.json()["job"]["priority"].clone()
        ),
        (json!("x"), json!(-128))
    );
    let r = s.post_json("/api/v1/job/", "{}");
    assert_eq!(r.status, 503);
    assert_eq!(
        r.json(),
        json!({"error":"cannot create job","code":1002,"jobUUID":ZERO})
    );
    // A clone is a new job: same limit (Go v1.1.2); an unknown job is answered first.
    let id = r#"0190e951-c29a-70aa-ba59-18be4abe97a1"#;
    let r = s.post_json(&format!("/api/v1/job/{id}/clone"), "");
    assert_eq!(
        (r.status, r.json()),
        (
            400,
            json!({"error":"job not found","code":1007,"jobUUID":id})
        )
    );
    let jobs = s.get("/api/v1/jobs/").json();
    let existing = jobs["jobs"][0]["id"].as_str().unwrap().to_string();
    let r = s.post_json(&format!("/api/v1/job/{existing}/clone"), "");
    assert_eq!(
        (r.status, r.json()),
        (
            503,
            json!({"error":"cannot create job","code":1002,"jobUUID":ZERO})
        )
    );
}

#[test]
fn compressed_bodies() {
    use std::io::Write;
    let s = start();
    let body = br#"{"topic":"gz","properties":{"n":1}}"#;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(body).unwrap();
    // `{"topic":"br","properties":{"n":1}}` compressed by the brotli package Fiber uses.
    let br: Vec<u8> = vec![
        27, 34, 0, 0, 4, 106, 114, 228, 199, 73, 250, 134, 109, 65, 18, 20, 182, 114, 146, 124, 4,
        42, 46, 117, 112, 36, 34, 6, 99, 36, 252, 202, 69, 71, 35, 255, 7,
    ];
    for (encoding, data, topic) in [
        ("gzip", gz.finish().unwrap(), "gz"),
        ("br", br.clone(), "br"),
        ("brotli", br, "br"),
    ] {
        let r = s.send(
            s.request("POST", "/api/v1/job/")
                .header("Content-Type", "application/json")
                .header("Content-Encoding", encoding)
                .body(data),
        );
        assert_eq!(r.status, 200, "{encoding}: {}", r.text());
        assert_eq!(r.json()["job"]["topic"], topic, "{encoding}");
    }
}

#[test]
fn query_parameters() {
    let s = start();
    let msg = |q: &str| {
        s.post(&format!("/api/v1/job/pull?{q}")).json()["error"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        msg("numjobs=0"),
        "invalid value for query parameter: numjobs, found 0, minimum allowed value is 1"
    );
    assert_eq!(
        msg("numjobs=101"),
        "invalid value for query parameter: numjobs, found 101, maximum allowed value is 100"
    );
    assert_eq!(
        msg("numjobs=%2B1"),
        "invalid value for query parameter: numjobs"
    );
    assert_eq!(
        msg("numjobs=4294967296"),
        "invalid value for query parameter: numjobs"
    );
    assert_eq!(
        msg("waittimeseconds=61"),
        "invalid value for query parameter: waittimeseconds, found 61, maximum allowed value is 60"
    );
    assert_eq!(
        msg("visibilitytimeout=86401"),
        "invalid value for query parameter: visibilitytimeout, found 86401, maximum allowed value is 86400"
    );
    let r = s.post("/api/v1/job/pull?numjobs=x");
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["code"], 1021);
    let r = s.post("/api/v1/job/pull?jobuuid=bad");
    assert_eq!(
        r.json()["validationErrors"][0],
        json!({"failedField":"jobuuid","tag":"parameter","value":"bad"})
    );
    let r = s.get("/api/v1/jobs/?limit=0");
    assert_eq!(
        r.json()["error"],
        "invalid value for query parameter: limit, found 0, minimum allowed value is 1"
    );
}

#[test]
fn pull_selection() {
    let s = start();
    let r = s.post("/api/v1/job/pull?topic=*");
    assert_eq!(r.status, 404);
    assert_eq!(
        r.json(),
        json!({"error":"no jobs found in the queue","code":1004,"jobUUID":ZERO})
    );
    let low = s.create(r#"{"topic":"a","priority":1}"#);
    let high = s.create(r#"{"topic":"a","priority":9}"#);
    let other = s.create(r#"{"topic":"b"}"#);
    let empty = s.create(r#"{}"#);
    for id in [&low, &high, &other, &empty] {
        s.wait_state(id, "queued", Duration::from_secs(3));
    }
    let r = s.post("/api/v1/job/pull?topic=zzz");
    assert_eq!(
        r.json()["error"],
        "no job found that is ready or matches the criteria"
    );
    // An empty topic only matches jobs without a topic (Go behavior, kept).
    assert_eq!(s.pull(""), std::slice::from_ref(&empty));
    assert_eq!(s.pull("topic=a&numjobs=5"), [high.clone(), low.clone()]);
    assert_eq!(s.pull("topic=*"), std::slice::from_ref(&other));
    let r = s.post(&format!("/api/v1/job/pull?jobuuid={other}"));
    assert_eq!(r.status, 400);
    assert_eq!(
        r.json(),
        json!({"error":"job already started","code":1006,"jobUUID":other})
    );
    let unknown = "0190e951-c29a-70aa-ba59-18be4abe97a2";
    let r = s.post(&format!("/api/v1/job/pull?jobuuid={unknown}"));
    assert_eq!(
        r.json(),
        json!({"error":"job not found in the queue","code":1005,"jobUUID":unknown})
    );
}

#[test]
fn long_poll_wakes_up_and_does_not_block_other_pulls() {
    let s = start();
    let url = s.url("/api/v1/job/pull?topic=lp&waittimeseconds=20");
    let started = Instant::now();
    let poller = std::thread::spawn(move || {
        let c = common::client();
        let r = c.post(url).send().unwrap();
        (r.status().as_u16(), started.elapsed())
    });
    std::thread::sleep(Duration::from_millis(300));
    // Another pull is not blocked by the long poll (Go held a global lock while sleeping).
    let t = Instant::now();
    assert_eq!(s.post("/api/v1/job/pull?topic=other").status, 404);
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    let id = s.create(r#"{"topic":"lp"}"#);
    let (status, waited) = poller.join().unwrap();
    assert_eq!(status, 200);
    assert!(waited < Duration::from_secs(5), "{waited:?}");
    assert_eq!(s.state(&id), "running");
    // A long poll without a job ends after the wait time.
    let t = Instant::now();
    assert_eq!(
        s.post("/api/v1/job/pull?topic=none&waittimeseconds=1")
            .status,
        404
    );
    assert!(t.elapsed() >= Duration::from_millis(900));
}

#[test]
fn fail_retry_and_terminate() {
    let s = start();
    let id = s.create(r#"{"topic":"f"}"#);
    s.wait_state(&id, "queued", Duration::from_secs(3));
    let r = s.post(&format!("/api/v1/job/{id}/fail"));
    assert_eq!(
        r.json(),
        json!({"error":"job is not running","code":1010,"jobUUID":id})
    );
    for attempt in 1..=3 {
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.pull("topic=f").is_empty() {
            assert!(
                Instant::now() < deadline,
                "not re-queued after failure {attempt}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let r = s.post(&format!("/api/v1/job/{id}/fail"));
        let expected = if attempt == 3 {
            "job reached max retry"
        } else {
            "success"
        };
        assert_eq!(r.json(), json!({"code":200,"message":expected}));
    }
    assert_eq!(s.state(&id), "failed");
    let events: Vec<String> = s.job(&id)["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["eventType"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(events.iter().filter(|e| *e == "FAIL").count(), 3);
    assert_eq!(events.last().map(String::as_str), Some("TERMINATE"));
    assert!(events.contains(&"HIDE".to_string()));
}

#[test]
fn cancel_requeues() {
    let s = start();
    let id = s.create(r#"{"topic":"c","lockResources":["rc"]}"#);
    s.wait_state(&id, "queued", Duration::from_secs(3));
    let r = s.post(&format!("/api/v1/job/{id}/cancel"));
    assert_eq!(
        r.json(),
        json!({"error":"job is not running","code":1008,"jobUUID":id})
    );
    assert_eq!(s.pull("topic=c"), std::slice::from_ref(&id));
    assert_eq!(s.post(&format!("/api/v1/job/{id}/cancel")).status, 200);
    let j = s.job(&id);
    assert_eq!(j["state"], "queued");
    let n = j["history"].as_array().unwrap().len();
    assert_eq!(j["history"][n - 2]["eventType"], "CANCEL");
    // The lock is kept while the job is queued again.
    assert_eq!(s.get("/api/v1/resources/").json()["resources"]["rc"], id);
}

#[test]
fn visibility_timeout() {
    let s = start();
    let unknown = "0190e951-c29a-70aa-ba59-18be4abe97a2";
    let r = s.post(&format!(
        "/api/v1/job/{unknown}/visibilitytimeout?visibilitytimeout=5"
    ));
    assert_eq!(
        r.json(),
        json!({"error":"job not found","code":1007,"jobUUID":unknown})
    );
    let id = s.create(r#"{"topic":"v"}"#);
    s.wait_state(&id, "queued", Duration::from_secs(3));
    assert_eq!(
        s.pull("topic=v&visibilitytimeout=1"),
        std::slice::from_ref(&id)
    );
    // The scheduler fails the job after 1 s, then retries it.
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let j = s.job(&id);
        if j["history"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["eventType"] == "FAIL")
        {
            break;
        }
        assert!(Instant::now() < deadline, "no timeout: {j}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let r = s.post(&format!(
        "/api/v1/job/{id}/visibilitytimeout?visibilitytimeout=300"
    ));
    assert_eq!(r.status, 200);
    assert_eq!(s.job(&id)["visibilityTimeout"], 300);
}

#[test]
fn delete_and_clone() {
    let s = start();
    let id =
        s.create(r#"{"topic":"d","properties":{"k":[1,2]},"lockResources":["x"],"name":"orig"}"#);
    let r = s.post(&format!("/api/v1/job/{id}/clone"));
    assert_eq!(r.status, 200);
    let clone = r.json()["job"].clone();
    assert_ne!(clone["id"], id);
    assert_eq!(
        (clone["name"].clone(), clone["properties"].clone()),
        (json!("orig"), json!({"k":[1,2]}))
    );
    assert_eq!(clone["history"].as_array().unwrap().len(), 2);
    let unknown = "0190e951-c29a-70aa-ba59-18be4abe97a2";
    assert_eq!(
        s.post(&format!("/api/v1/job/{unknown}/clone")).json()["code"],
        1007
    );
    let r = s.delete(&format!("/api/v1/job/{unknown}"));
    assert_eq!(
        (r.status, r.json()),
        (
            400,
            json!({"error":"job not found","code":1003,"jobUUID":unknown})
        )
    );
    assert_eq!(s.delete(&format!("/api/v1/job/{id}")).status, 200);
    assert_eq!(s.get(&format!("/api/v1/job/{id}")).status, 404);
    let a = s.create(r#"{"topic":"d"}"#);
    s.wait_state(&a, "queued", Duration::from_secs(3));
    assert_eq!(s.pull(&format!("jobuuid={a}")), std::slice::from_ref(&a));
    s.create(r#"{"topic":"d","delay":3600}"#);
    assert_eq!(
        s.delete("/api/v1/jobs/queued").json(),
        json!({"code":200,"message":"success"})
    );
    let all = s.get("/api/v1/jobs/all").json();
    let ids: Vec<&str> = all["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [a.as_str()], "only the running job is left");
    assert_eq!(s.delete("/api/v1/jobs/").status, 200);
    assert_eq!(s.get("/api/v1/jobs/all").json()["jobs"], json!([]));
}

#[test]
fn resource_locks() {
    let s = start();
    let a = s.create(r#"{"topic":"r","lockResources":["db"]}"#);
    s.wait_state(&a, "queued", Duration::from_secs(3));
    assert_eq!(s.pull("topic=r"), std::slice::from_ref(&a));
    let b = s.create(r#"{"topic":"r","lockResources":["db"]}"#);
    let c = s.create(r#"{"topic":"r","lockResources":["db"]}"#);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        (s.state(&b), s.state(&c)),
        ("pending".into(), "pending".into())
    );
    // Deleting a pending job must not free a lock it does not hold (Go freed it).
    assert_eq!(s.delete(&format!("/api/v1/job/{c}")).status, 200);
    assert_eq!(
        s.get("/api/v1/resources/").json(),
        json!({"code":200,"message":"success","resources":{"db":a}})
    );
    assert_eq!(s.post(&format!("/api/v1/job/{a}/succeed")).status, 200);
    s.wait_state(&b, "queued", Duration::from_secs(3));
    // Duplicated names do not block the job (Go: stuck pending forever).
    let d = s.create(r#"{"topic":"r","lockResources":["x","x"]}"#);
    s.wait_state(&d, "queued", Duration::from_secs(3));
    // Unlock by path parameter (Go read only ?resourcename=).
    assert_eq!(
        s.post("/api/v1/resource/x/unlock").json(),
        json!({"code":200,"message":"success"})
    );
    assert_eq!(
        s.post("/api/v1/resource/ignored/unlock?resourcename=db")
            .status,
        200
    );
    let r = s.post("/api/v1/resource/nothing/unlock");
    assert_eq!(r.status, 404);
    assert_eq!(
        r.json(),
        json!({"error":"resource not found: nothing","code":1030,"jobUUID":ZERO})
    );
    assert_eq!(
        s.post("/api/v1/resources/unlock").json(),
        json!({"code":200,"message":"success"})
    );
    assert_eq!(s.get("/api/v1/resources/").json()["resources"], json!({}));
}

#[test]
fn retried_job_locks_its_resources_again() {
    // 1.5 s before a retry, so that another job can take the resource meanwhile.
    let s = Server::start_with(T, |c| {
        c.replace("            base: 2\n", "            base: 1500\n")
    });
    let a = s.create(r#"{"topic":"rr","lockResources":["db"]}"#);
    s.wait_state(&a, "queued", Duration::from_secs(3));
    assert_eq!(s.pull(&format!("jobuuid={a}")), std::slice::from_ref(&a));
    assert_eq!(s.post(&format!("/api/v1/job/{a}/fail")).status, 200);
    assert_eq!(s.state(&a), "hidden");
    let b = s.create(r#"{"topic":"rr","lockResources":["db"]}"#);
    s.wait_state(&b, "queued", Duration::from_secs(3));
    assert_eq!(s.pull(&format!("jobuuid={b}")), std::slice::from_ref(&b));
    std::thread::sleep(Duration::from_millis(2500));
    // `a` cannot be re-queued while `b` holds the lock (Go re-queued it without the lock).
    assert_eq!(s.state(&a), "hidden");
    assert_eq!(s.post(&format!("/api/v1/job/{b}/succeed")).status, 200);
    s.wait_state(&a, "queued", Duration::from_secs(3));
    assert_eq!(s.get("/api/v1/resources/").json()["resources"]["db"], a);
}

#[test]
fn list_jobs() {
    let s = start();
    let mut ids = Vec::new();
    for (i, topic) in ["x", "y", "x", "y", "x"].iter().enumerate() {
        ids.push(s.create(&format!(r#"{{"topic":"{topic}","priority":{i}}}"#)));
        std::thread::sleep(Duration::from_millis(3));
    }
    let r = s
        .get("/api/v1/jobs/?limit=2&page=2&sort=created_asc")
        .json();
    assert_eq!(
        (
            r["total"].clone(),
            r["page"].clone(),
            r["limit"].clone(),
            r["totalpages"].clone()
        ),
        (json!(5), json!(2), json!(2), json!(3))
    );
    let got: Vec<&str> = r["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap())
        .collect();
    assert_eq!(got, [ids[2].as_str(), ids[3].as_str()]);
    let r = s.get("/api/v1/jobs/?topic=x&sort=priority_desc").json();
    let got: Vec<&str> = r["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap())
        .collect();
    assert_eq!(got, [ids[4].as_str(), ids[2].as_str(), ids[0].as_str()]);
    let r = s
        .get(&format!(
            "/api/v1/jobs/?search={}",
            ids[1][24..].to_uppercase()
        ))
        .json();
    assert_eq!(r["total"], 1);
    assert_eq!(s.get("/api/v1/jobs/?status=unknown").json()["total"], 0);
    assert_eq!(s.get("/api/v1/jobs/?page=9").json()["jobs"], json!([]));
    std::thread::sleep(Duration::from_millis(1200));
    let r = s.get("/api/v1/jobs/?status=QUEUED").json();
    assert_eq!(r["total"], 5);
    let oldest = s.get("/api/v1/jobs/oldest?limit=2").json();
    assert_eq!(oldest["topics"]["x"].as_array().unwrap().len(), 2);
    assert!(
        oldest["topics"]["x"][0]["duration"].as_i64().unwrap()
            >= oldest["topics"]["x"][1]["duration"].as_i64().unwrap()
    );
    let recent = s.get("/api/v1/jobs/recent").json();
    assert_eq!(recent["jobs"].as_array().unwrap().len(), 5);
    let first = &recent["jobs"][0];
    assert_eq!(
        first
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["created", "duration", "id", "state", "topic"]
    );
    assert_eq!(s.get("/api/v1/jobs/stalled").json()["jobs"], json!([]));
    assert_eq!(
        s.get("/api/v1/metrics/jobs/stalled?duration=0").json()["code"],
        1021
    );
    let topics = s.get("/api/v1/topics/").json();
    assert_eq!(topics["topics"], json!(["x", "y"]));
}

#[test]
fn metrics_endpoints() {
    let s = start();
    let a = s.create(r#"{"topic":"m","lockResources":["mr"]}"#);
    let b = s.create(r#"{"topic":"m"}"#);
    s.wait_state(&b, "queued", Duration::from_secs(3));
    assert_eq!(s.pull(&format!("jobuuid={a}")), std::slice::from_ref(&a));
    assert_eq!(s.delete(&format!("/api/v1/job/{b}")).status, 200);
    let p = s.get("/api/v1/metrics/jobs/processing").json();
    assert_eq!(p["jobStats"]["existingJobs"], 1, "deletes are counted: {p}");
    assert_eq!(p["jobStats"]["deletedJobs"], 1);
    assert_eq!(p["jobStats"]["runningJobs"], 1);
    let act = s.get("/api/v1/metrics/jobs/activity").json();
    assert_eq!(
        (
            act["jobActivity"]["created"].clone(),
            act["jobActivity"]["deleted"].clone()
        ),
        (json!(2), json!(1))
    );
    let t = s.get("/api/v1/metrics/jobs/topics").json();
    assert_eq!(t["metrics"]["m"][0]["resourcesLockedCount"], 1);
    let res = s.get("/api/v1/metrics/resources").json();
    assert_eq!(res["resources"][0]["resourceName"], "mr");
    assert_eq!(res["resources"][0]["jobUuid"], a);
    let ts = s.get("/api/v1/metrics/topics").json();
    assert_eq!(ts["topics"][0]["topicName"], "m");
    assert_eq!(ts["topics"][0]["totalJobs"], 1);
    assert_eq!(ts["topics"][0]["percentJobs"], 100);
    let r = s.get("/metrics");
    assert_eq!(
        r.header("content-type"),
        "text/plain; version=0.0.4; charset=utf-8; escaping=underscores"
    );
    let text = r.text();
    for name in [
        "job_events{",
        "job_status{",
        "http_requests_total{",
        "http_request_duration_seconds_bucket{",
        "http_requests_in_progress_total{",
    ] {
        assert!(text.contains(name), "{name} missing");
    }
    assert!(text.contains(r#"http_requests_total{method="POST",path="CreateJob",service="minijob",status_code="200"} 2"#), "{text}");
    // `GET /metrics` is registered before the Prometheus middleware (not in progress), and
    // unknown methods are answered before any middleware (no label).
    assert!(text.contains(r#"http_requests_in_progress_total{method="GET",service="minijob"} 0"#));
    assert_eq!(s.request("PURGE", "/").send().unwrap().status(), 400);
    assert!(!s.get("/metrics").text().contains("PURGE"));
    // The topic of an empty pull is kept as sent (Go kept a reference to a reused buffer).
    s.send(s.request("POST", "/api/v1/job/pull?topic=none"));
    s.get(&format!("/api/v1/job/{a}"));
    assert!(
        s.get("/metrics")
            .text()
            .contains(r#"app_events{event="EmptyPollReceived",topic="none"} 1"#)
    );
    let topics = s.get("/api/v1/topics/").json();
    assert_eq!(topics["topics"], json!(["m", "none"]));
    let r = s.send(
        s.request("GET", "/metrics")
            .header("Accept-Encoding", "gzip"),
    );
    // reqwest without the gzip feature returns the raw bytes.
    assert_eq!(r.header("content-encoding"), "gzip");
    assert_eq!(&r.body[..2], &[0x1f, 0x8b]);
}

#[test]
fn system_monitor_and_docs() {
    let s = start();
    let info = s.get("/api/v1/system/info").json();
    assert_eq!(info["systemInfo"]["environment"], "development-inMemory");
    assert_eq!(
        info["systemInfo"]["hostname"],
        format!("127.0.0.1:{}", s.port)
    );
    assert_eq!(info["systemInfo"]["version"], minijob::VERSION);
    let r = s.get("/api/v1/system/resources").json();
    assert_eq!(
        r["resourceUsage"]["backend"],
        json!({"label":"Backend Provider","value":"InMemory"})
    );
    assert_eq!(
        r["resourceUsage"]["disk"],
        json!({"label":"Disk Usage","value":"0MB"})
    );
    let m = s.get("/monitor");
    assert_eq!(m.header("content-type"), "text/html; charset=utf-8");
    let m = s
        .send(
            s.request("GET", "/monitor")
                .header("Accept", "application/json"),
        )
        .json();
    assert!(m["pid"]["ram"].as_u64().unwrap() > 0);
    assert!(m["os"]["total_ram"].as_u64().unwrap() > 0);
    let r = s.get("/docs/doc.json");
    assert_eq!(r.header("content-type"), "application/json");
    assert_eq!(r.json()["info"]["title"], "MiniJob API");
    assert_eq!(s.get("/debug/pprof/").status, 404);
    if !cfg!(feature = "swagger-ui") {
        assert_eq!(s.get("/docs/index.html").status, 404);
        return;
    }
    let r = s.get("/docs/");
    assert_eq!((r.status, r.header("location")), (301, "/docs/index.html"));
    assert_eq!(s.get("/docs").status, 301);
    let r = s.get("/docs/index.html");
    assert_eq!(r.header("content-type"), "text/html");
    assert!(r.text().contains(r#""url":"/docs/doc.json""#));
    let r = s.send(
        s.request("GET", "/docs/index.html")
            .header("X-Forwarded-Prefix", "/api/"),
    );
    assert!(r.text().contains(r#""url":"/api/docs/doc.json""#));
    let r = s.get("/docs/swagger-ui.css");
    assert_eq!((r.status, r.header("content-type")), (200, "text/css"));
    assert_eq!(
        s.get("/docs/missing.js").text(),
        "Cannot GET /docs/missing.js"
    );
}

/// `webserver.logs.enable`: Fiber's access log on stdout,
/// `[time] status - ip:port - latency method path`.
#[test]
fn access_log_lines() {
    let mut s = Server::start_with(T, |c| {
        c.replace(
            "    logs:\n        enable: false",
            "    logs:\n        enable: true",
        )
    });
    s.get("/ping");
    s.get("/a%20b?x=1");
    s.post("/api/v1/job/pull?topic=x");
    s.head("/ping");
    assert_eq!(s.shutdown().code(), Some(0));
    let log = s.log();
    let lines: Vec<&str> = log.lines().filter(|l| l.starts_with('[')).collect();
    let re = regex::Regex::new(
        r"^\[\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d[+-]\d{4}\] (\d{3}) - 127\.0\.0\.1:\d+ - {0,12}\S+ (\S+) (.*)$",
    )
    .unwrap();
    let parsed: Vec<(String, String, String)> = lines
        .iter()
        .map(|l| {
            let c = re
                .captures(l)
                .unwrap_or_else(|| panic!("access log line: {l:?}"));
            (c[1].to_string(), c[2].to_string(), c[3].to_string())
        })
        .collect();
    let expected = [
        ("200", "GET", "/ping"),
        ("404", "GET", "/a%20b"),
        ("404", "POST", "/api/v1/job/pull"),
        ("200", "HEAD", "/ping"),
        ("200", "POST", "/api/v1/admin/server/shutdown"),
    ];
    for e in expected {
        assert!(
            parsed.contains(&(e.0.into(), e.1.into(), e.2.into())),
            "{e:?} not in {parsed:?}"
        );
    }
    // The latency is a Go duration (`12.5µs`) right-aligned on 13 characters.
    let line = lines.iter().find(|l| l.ends_with(" GET /ping")).unwrap();
    let latency = line
        .split(" - ")
        .nth(2)
        .and_then(|l| l.strip_suffix(" GET /ping"))
        .unwrap();
    let width = latency.chars().count();
    assert!(
        width == 13 || (width > 13 && !latency.starts_with(' ')),
        "{latency:?}"
    );
    let re = regex::Regex::new(r"^ *[0-9.]+(ns|µs|ms|s)$").unwrap();
    assert!(re.is_match(latency), "{latency:?}");
}

/// The backend's `notify event` debug lines (`backend.logVerbosity` 2 and more), as Go's
/// inMemory provider writes them: `jobbackendprovider.Event` in its member order, one line
/// per resource of a job that releases them, `EventJobUpdated` (16) for a visibility timeout
/// change (Go v1.1.2). With `jobs.logVerbosity` 3, the service logs each one again
/// (`Event received`), after the verbose line of the change.
#[test]
fn backend_notify_events() {
    let mut s = Server::start_with(T, |c| {
        c.replacen(
            "    logger:\n        level: \"info\"",
            "    logger:\n        level: \"debug\"",
            1,
        )
        .replacen("    logVerbosity: 1", "    logVerbosity: 3", 1)
    });
    let id = s.create(r#"{"topic":"n","lockResources":["r1","r2"]}"#);
    assert_eq!(s.pull("topic=n"), std::slice::from_ref(&id));
    let r = s.post(&format!(
        "/api/v1/job/{id}/visibilitytimeout?visibilitytimeout=100"
    ));
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(s.post(&format!("/api/v1/job/{id}/succeed")).status, 200);
    assert_eq!(s.post("/api/v1/resources/unlock").status, 200);
    assert_eq!(s.delete("/api/v1/jobs/").status, 200);
    assert_eq!(s.shutdown().code(), Some(0));
    let nil = "00000000-0000-0000-0000-000000000000";
    let event = |t: u8, job: &str, resource: Option<&str>| {
        let resource = resource.map_or("null".to_string(), |r| format!("\"{r}\""));
        format!(r#"{{"Type":{t},"JobUUID":"{job}","Resource":{resource}}}}}"#)
    };
    let expected = [
        event(0, &id, None),
        event(2, &id, None),
        event(3, &id, None),
        event(5, &id, None),
        event(16, &id, None),
        event(14, &id, Some("r1")),
        event(14, &id, Some("r2")),
        event(6, &id, None),
        event(13, nil, None),
        event(13, nil, None),
        event(12, nil, None),
    ];
    let log = s.log();
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains(r#""message":"notify event""#))
        .collect();
    assert_eq!(lines.len(), expected.len(), "{log}");
    for (line, event) in lines.iter().zip(&expected) {
        assert!(
            line.ends_with(&format!(
                r#""topic":"backendProvider","method":"NotifyChange","event":{event}"#
            )),
            "{line}\nexpected {event}"
        );
    }
    let received: Vec<&str> = log
        .lines()
        .filter(|l| {
            l.contains(r#""message":"Event received""#)
                || l.contains(r#""message":"Job enqueued""#)
                || l.contains(r#""message":"Job started""#)
        })
        .collect();
    assert_eq!(received.len(), expected.len() + 2, "{log}");
    let enqueued = format!(r#""method":"EnqueueJob","JobUUID":"{id}"}}"#);
    let started = format!(r#""method":"StartJob","JobUUID":"{id}"}}"#);
    assert!(received[2].ends_with(&enqueued), "{}", received[2]);
    assert!(received[4].ends_with(&started), "{}", received[4]);
    let received: Vec<&&str> = received
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 2 && *i != 4)
        .map(|(_, l)| l)
        .collect();
    for (line, event) in received.iter().zip(&expected) {
        assert!(
            line.ends_with(&format!(
                r#""topic":"job","method":"Run","eventType":{event}"#
            )),
            "{line}\nexpected {event}"
        );
    }
}

#[test]
fn restart_reloads_configuration() {
    let mut s = start();
    s.create("{}");
    let config = std::fs::read_to_string(&s.config_path).unwrap();
    std::fs::write(
        &s.config_path,
        config.replace("development-inMemory", "reloaded"),
    )
    .unwrap();
    assert_eq!(
        s.post("/api/v1/admin/server/restart").json(),
        json!({"code":200,"message":"success"})
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(r) = common::client().get(s.url("/api/v1/system/info")).send() {
            let v: Value = r.json().unwrap();
            if v["systemInfo"]["environment"] == "reloaded" {
                break;
            }
        }
        assert!(Instant::now() < deadline, "not restarted: {}", s.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    // Same process, jobs of the non-persistent in-memory backend are gone.
    assert_eq!(s.get("/api/v1/jobs/all").json()["jobs"], json!([]));
    let status = s.shutdown();
    assert_eq!(status.code(), Some(0));
    let log = s.log();
    assert!(log.contains("Restarting server ..."));
    assert!(log.contains("\"message\":\"End program\""));
}

#[test]
fn command_line_and_exit_status() {
    let (st, out, _) = run_binary(&["-version"]);
    assert_eq!(
        (st.code(), out.trim()),
        (Some(0), format!("Version: {}", minijob::VERSION).as_str())
    );
    let (st, _, err) = run_binary(&["-h"]);
    assert_eq!(st.code(), Some(0));
    assert!(
        err.contains("  -config string\n    \tFilepath to config.yaml (default \"config.yaml\")\n")
    );
    let (st, _, err) = run_binary(&["-unknown"]);
    assert_eq!(st.code(), Some(2));
    assert!(err.starts_with("flag provided but not defined: -unknown\n"));
    let (st, out, _) = run_binary(&["-config", "does-not-exist.yaml"]);
    assert_eq!(st.code(), Some(1));
    assert!(out.starts_with("error while opening configuration file does-not-exist.yaml : open does-not-exist.yaml: "), "{out}");
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.yaml");
    std::fs::write(&bad, "jobs:\n  maxAllowedJobs: abc\n").unwrap();
    let (st, out, _) = run_binary(&["-config", bad.to_str().unwrap()]);
    assert_eq!(st.code(), Some(1));
    assert!(
        out.contains("yaml: unmarshal errors:\n  line 2: cannot unmarshal !!str `abc` into uint"),
        "{out}"
    );
    std::fs::write(&bad, "environment: x\n").unwrap();
    let (st, out, _) = run_binary(&["-config", bad.to_str().unwrap()]);
    assert_eq!(st.code(), Some(1));
    assert_eq!(
        out,
        "Error while initializing logger\nno encoder name specified\n"
    );
    // Startup errors exit with 1 (Go: 0).
    let s = start();
    let cfg = std::fs::read_to_string(&s.config_path).unwrap();
    let cfg2 = dir.path().join("same-port.yaml");
    std::fs::write(&cfg2, cfg).unwrap();
    let (st, out, _) = run_binary(&["-config", cfg2.to_str().unwrap()]);
    assert_eq!(st.code(), Some(1), "{out}");
    assert!(out.contains("Web server stopped with error"));
    // Fiber's wording of Go's net.Listen error
    let bind = format!(
        "\"error\":\"failed to listen: listen tcp4 127.0.0.1:{}: bind: ",
        s.port
    );
    assert!(out.contains(&bind), "{out}");
    let cfg4 = dir.path().join("schema-missing.yaml");
    std::fs::write(
        &cfg4,
        std::fs::read_to_string(&s.config_path)
            .unwrap()
            .replace(
                "enable: false  # Enable JSON",
                "enable: true  # Enable JSON",
            )
            .replace(
                "path: \"\" # Path to JSON",
                "path: \"missing-schema.json\" # Path to JSON",
            ),
    )
    .unwrap();
    let (st, out, _) = run_binary(&["-config", cfg4.to_str().unwrap()]);
    assert_eq!(st.code(), Some(1), "{out}");
    let missing = if cfg!(windows) {
        "The system cannot find the file specified."
    } else {
        "no such file or directory"
    };
    assert!(
        out.contains(&format!(
            "\"error\":\"error reading schema file: open missing-schema.json: {missing}\""
        )),
        "{out}"
    );
    let cfg3 = dir.path().join("bad-backend.yaml");
    std::fs::write(
        &cfg3,
        std::fs::read_to_string(&s.config_path)
            .unwrap()
            .replace("type: \"inMemory\"", "type: \"nope\""),
    )
    .unwrap();
    let (st, out, _) = run_binary(&["-config", cfg3.to_str().unwrap()]);
    assert_eq!(st.code(), Some(1));
    assert!(out.contains("No such provider type exists: 'nope'"));
}

/// Accepts HTTP requests, answers 200 and forwards (head, body) of each request.
fn callback_receiver() -> (u16, std::sync::mpsc::Receiver<(String, Value)>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(i + 4);
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let len: usize = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .unwrap_or(0);
            while buf.len() < head_end + len {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body: Value =
                serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(Value::Null);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = tx.send((head, body));
        }
    });
    (port, rx)
}

#[test]
fn callbacks_on_completion() {
    let (port, rx) = callback_receiver();
    let mut s = Server::start_with(T, |c| {
        c.replace(
            "enablePersistentStorage: false",
            "enablePersistentStorage: true",
        )
    });
    let id = s.create(&format!(
        r#"{{"topic":"cb","callbackUrl":"http://127.0.0.1:{port}/hook"}}"#
    ));
    s.wait_state(&id, "queued", Duration::from_secs(3));
    assert_eq!(s.pull("topic=cb"), std::slice::from_ref(&id));
    assert_eq!(s.post(&format!("/api/v1/job/{id}/cancel")).status, 200);
    let (head, body) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("cancel callback");
    assert!(head.starts_with("POST /hook HTTP/1.1"), "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("user-agent: minijob-callback/1.0"), "{head}");
    assert!(lower.contains("content-type: application/json"), "{head}");
    // The payload is a snapshot at the transition (Go built it later, usually "queued").
    assert_eq!(
        (body["jobId"].as_str(), body["status"].as_str()),
        (Some(id.as_str()), Some("canceled"))
    );
    assert_eq!(body["job"]["id"], id);
    assert!(body["job"].get("state").is_none());
    assert_eq!(
        body["timestamp"],
        body["job"]["history"].as_array().unwrap().last().unwrap()["timestamp"]
    );
    assert_eq!(s.pull("topic=cb"), std::slice::from_ref(&id));
    assert_eq!(s.post(&format!("/api/v1/job/{id}/succeed")).status, 200);
    let (_, body) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("success callback");
    assert_eq!(body["status"], "succeeded");
    // No callback is sent again for completed jobs when the server restarts (Go resent all).
    assert_eq!(s.shutdown().code(), Some(0));
    s.spawn();
    assert_eq!(s.state(&id), "succeeded");
    assert!(rx.recv_timeout(Duration::from_secs(2)).is_err());
}

#[cfg(unix)]
#[test]
fn unix_signals() {
    let signal = |pid: u32, sig: &str| {
        let st = std::process::Command::new("kill")
            .args([sig, &pid.to_string()])
            .status()
            .unwrap();
        assert!(st.success());
    };
    let mut s = start();
    let pid = s.pid();
    // SIGHUP: reload and restart in the same process (Go: killed by the signal).
    signal(pid, "-HUP");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !s.log().contains("Restarting server ...") {
        assert!(Instant::now() < deadline, "{}", s.log());
        std::thread::sleep(Duration::from_millis(50));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while common::client().get(s.url("/ping")).send().is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(s.pid(), pid);
    signal(pid, "-TERM");
    assert_eq!(s.wait_exit().code(), Some(0));
    assert!(
        s.log()
            .contains("Received SIGTERM, shutting down server...")
    );
    s.spawn();
    signal(s.pid(), "-QUIT");
    assert_eq!(s.wait_exit().code(), Some(0));
}
