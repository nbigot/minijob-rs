//! Web API with the `mock` backend (`backend.type: mock`).

mod common;

use common::Server;
use std::time::Duration;

#[test]
fn mock_backend() {
    let mut s = Server::start_with("standard/config/config.inmemory.yaml", |c| {
        c.replace("type: \"inMemory\"", "type: \"mock\"")
    });
    let r = s.get("/api/v1/system/resources").json();
    assert_eq!(r["resourceUsage"]["backend"]["value"], "Mock");
    // The Go mock limits itself to 100 jobs internally but never refused a creation.
    for _ in 0..120 {
        s.create("{}");
    }
    let id = s.create(r#"{"topic":"m"}"#);
    s.wait_state(&id, "queued", Duration::from_secs(3));
    assert_eq!(s.pull("topic=m"), std::slice::from_ref(&id));
    assert_eq!(s.post(&format!("/api/v1/job/{id}/succeed")).status, 200);
    assert_eq!(s.get("/readyz").status, 200);
    assert_eq!(s.shutdown().code(), Some(0));
    // Nothing is kept by the mock backend.
    s.spawn();
    assert_eq!(s.get("/api/v1/jobs/?limit=1").json()["total"], 0);
}
