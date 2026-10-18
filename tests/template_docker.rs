//! Web API with `config-templates/docker/config/config.yaml` (schema validation, persistence
//! in `/app/data`, listening on all interfaces; the test rewrites paths and port).

mod common;

use common::Server;
use serde_json::json;

const T: &str = "docker/config/config.yaml";

#[test]
fn docker_template() {
    let mut s = Server::start(T);
    let info = s.get("/api/v1/system/info").json();
    assert_eq!(info["systemInfo"]["environment"], "docker");
    let r = s.post_json("/api/v1/job/", "{}");
    assert_eq!(r.status, 400);
    assert_eq!(
        r.json()["validationErrors"][0]["tag"],
        "\"properties\" value is required"
    );
    let id = s.create(r#"{"properties":{"k":1}}"#);
    let r = s.get("/api/v1/system/resources").json();
    assert_eq!(r["resourceUsage"]["backend"]["value"], "InMemory");
    for p in [
        "/livez",
        "/readyz",
        "/metrics",
        "/monitor",
        "/docs/doc.json",
    ] {
        assert_eq!(s.get(p).status, 200, "{p}");
    }
    assert_eq!(s.shutdown().code(), Some(0));
    let saved = std::fs::read_to_string(s.data_dir().join("jobs.json")).unwrap();
    assert!(saved.contains(&id));
    s.spawn();
    assert_eq!(s.job(&id)["properties"], json!({"k": 1}));
}
