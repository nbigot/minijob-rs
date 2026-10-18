//! Web API with `config-templates/standard/config/config.yaml`: JSON-schema validation,
//! CORS, in-memory backend persisted to a file every 30 s, retention policy.

mod common;

use common::Server;
use serde_json::{Value, json};
use std::time::Duration;

const T: &str = "standard/config/config.yaml";
const ZERO: &str = "00000000-0000-0000-0000-000000000000";

fn invalid(errors: Value) -> Value {
    json!({"error":"invalid json content","code":1023,"jobUUID":ZERO,"validationErrors":errors})
}

/// Expected bodies are the answers of the Go server to the same requests.
#[test]
fn schema_validation_like_go() {
    let s = Server::start(T);
    let cases: Vec<(&str, Value)> = vec![
        (
            r#"{"topic":5}"#,
            invalid(json!([
                {"failedField":"/","tag":"\"properties\" value is required","value":"map[topic:5]"},
                {"failedField":"/topic","tag":"type should be string, got integer","value":"5"}
            ])),
        ),
        (
            r#"{"properties":{},"callbackUrl":"x"}"#,
            invalid(
                json!([{"failedField":"/","tag":"additional properties are not allowed","value":"map[callbackUrl:x properties:map[]]"}]),
            ),
        ),
        (
            r#"{"properties":{},"priority":1.5,"lockResources":["a",2]}"#,
            invalid(json!([
                {"failedField":"/lockResources/1","tag":"type should be string, got integer","value":"2"},
                {"failedField":"/priority","tag":"type should be integer, got number","value":"1.5"}
            ])),
        ),
        (
            r#""x""#,
            invalid(
                json!([{"failedField":"/","tag":"type should be object, got string","value":"x"}]),
            ),
        ),
        (
            "{",
            json!({"error":"invalid json body format","code":1022,"jobUUID":ZERO}),
        ),
        (
            r#"{"properties":{"a":1},"priority":"5"}"#,
            invalid(
                json!([{"failedField":"/priority","tag":"type should be integer, got string","value":"5"}]),
            ),
        ),
        (
            r#"{"properties":null}"#,
            invalid(
                json!([{"failedField":"/properties","tag":"type should be object, got null","value":"<nil>"}]),
            ),
        ),
        (
            r#"{"properties":{},"topic":null}"#,
            invalid(
                json!([{"failedField":"/topic","tag":"type should be string, got null","value":"<nil>"}]),
            ),
        ),
        (
            r#"{"properties":{},"lockResources":"a"}"#,
            invalid(
                json!([{"failedField":"/lockResources","tag":"type should be array, got string","value":"a"}]),
            ),
        ),
        (
            "[]",
            invalid(
                json!([{"failedField":"/","tag":"type should be object, got array","value":"[]"}]),
            ),
        ),
        (
            r#"{"properties":{},"delay":-1.0}"#,
            json!({"error":"cannot create job from request","code":1002,"jobUUID":ZERO}),
        ),
        (
            r#"{"properties":{},"startAfter":1e3}"#,
            json!({"error":"cannot create job from request","code":1002,"jobUUID":ZERO}),
        ),
        (
            r#"{"properties":{"x":1},"name":["a",1]}"#,
            invalid(
                json!([{"failedField":"/name","tag":"type should be string, got array","value":"[a 1]"}]),
            ),
        ),
    ];
    for (body, expected) in cases {
        let r = s.post_json("/api/v1/job/", body);
        assert_eq!(r.status, 400, "{body}");
        assert_eq!(r.json(), expected, "{body}");
    }
    let r = s.post_json("/api/v1/job/", r#"{"properties":{"k":"v"},"topic":"ok"}"#);
    assert_eq!(r.status, 200);
}

#[test]
fn cors() {
    let s = Server::start(T);
    let r = s.send(
        s.request("POST", "/api/v1/job/")
            .header("Origin", "http://x.com")
            .header("Content-Type", "application/json")
            .body(r#"{"properties":{}}"#),
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.header("access-control-allow-origin"), "*");
    assert_eq!(r.header("vary"), "");
    let r = s.send(
        s.request("OPTIONS", "/api/v1/job/")
            .header("Origin", "http://x.com")
            .header("Access-Control-Request-Method", "POST")
            .header("Access-Control-Request-Headers", "content-type"),
    );
    assert_eq!(r.status, 204);
    assert_eq!(
        r.header("vary"),
        "Access-Control-Request-Method, Access-Control-Request-Headers, Origin"
    );
    assert_eq!(
        r.header("access-control-allow-methods"),
        "GET,POST,HEAD,PUT,DELETE,PATCH"
    );
    assert_eq!(r.header("access-control-allow-headers"), "content-type");
    assert!(r.body.is_empty());
    // OPTIONS without Access-Control-Request-Method goes to the router.
    let r = s.send(
        s.request("OPTIONS", "/api/v1/job/")
            .header("Origin", "http://x.com"),
    );
    assert_eq!((r.status, r.header("vary")), (405, "Origin"));
}

#[test]
fn persistence_across_restarts() {
    let mut s = Server::start(T);
    let file = s.data_dir().join("jobs.json");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "{\"jobs\":{}}",
        "empty file at start"
    );
    let a = s.create(r#"{"properties":{"n":1},"topic":"p","lockResources":["db"]}"#);
    let b = s.create(r#"{"properties":{"n":2},"topic":"p","lockResources":["db"]}"#);
    let delayed = s.create(r#"{"properties":{},"topic":"p","delay":3600}"#);
    s.wait_state(&a, "queued", Duration::from_secs(3));
    assert_eq!(s.pull(&format!("jobuuid={a}")), std::slice::from_ref(&a));
    let before: Vec<Value> = [&a, &b, &delayed].iter().map(|id| s.job(id)).collect();
    // Shut down before the 30 s save: the final save must contain everything (B08).
    assert_eq!(s.shutdown().code(), Some(0));
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["jobs"].as_object().unwrap().len(), 3);
    s.spawn();
    for (id, old) in [&a, &b, &delayed].iter().zip(&before) {
        assert_eq!(&s.job(id), old, "{id}");
    }
    // The running job still holds the lock; the second one is still blocked.
    assert_eq!(
        s.get("/api/v1/resources/").json()["resources"]["db"],
        a.as_str()
    );
    assert_eq!(s.post(&format!("/api/v1/job/{a}/succeed")).status, 200);
    s.wait_state(&b, "queued", Duration::from_secs(3));
    assert_eq!(s.state(&delayed), "delayed");
    assert_eq!(s.shutdown().code(), Some(0));
}

#[test]
fn crash_keeps_a_valid_file() {
    let mut s = Server::start_with(T, |c| c.replace("writeFrequency: 30", "writeFrequency: 1"));
    let file = s.data_dir().join("jobs.json");
    for i in 0..50 {
        s.create(&format!(r#"{{"properties":{{"i":{i}}}}}"#));
    }
    std::thread::sleep(Duration::from_millis(2500));
    s.kill();
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["jobs"].as_object().unwrap().len(), 50);
    s.spawn();
    assert_eq!(s.get("/api/v1/jobs/?limit=100").json()["total"], 50);
}
