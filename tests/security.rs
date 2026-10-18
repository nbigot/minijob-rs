//! Security-related behavior: CORS restrictions, request limits, path handling, output
//! escaping, malformed input, slow clients.
//!
//! Note: like the Go server, the API has no authentication (admin routes included); it must
//! be exposed only to trusted networks.

mod common;

use common::Server;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const T: &str = "standard/config/config.inmemory.yaml";

/// Sends raw bytes and returns the raw answer (the connection is closed by the request).
fn raw(s: &Server, request: &[u8]) -> String {
    let mut tcp = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    tcp.write_all(request).unwrap();
    let mut out = Vec::new();
    let _ = tcp.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn cors_restricted_origins() {
    let s = Server::start_with(T, |c| {
        c.replace(
            "allowOrigins: \"*\"",
            "allowOrigins: \"https://app.example.com, https://*.example.org\"",
        )
    });
    let r = s.send(
        s.request("GET", "/ping")
            .header("Origin", "https://evil.com"),
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.header("access-control-allow-origin"), "");
    assert_eq!(r.header("vary"), "Origin");
    let r = s.send(
        s.request("GET", "/ping")
            .header("Origin", "https://App.Example.com"),
    );
    assert_eq!(
        r.header("access-control-allow-origin"),
        "https://app.example.com"
    );
    let r = s.send(
        s.request("GET", "/ping")
            .header("Origin", "https://a.example.org"),
    );
    assert_eq!(
        r.header("access-control-allow-origin"),
        "https://a.example.org"
    );
    let r = s.send(
        s.request("OPTIONS", "/api/v1/jobs/")
            .header("Origin", "https://evil.com")
            .header("Access-Control-Request-Method", "DELETE"),
    );
    assert_eq!(r.status, 204);
    assert_eq!(r.header("access-control-allow-origin"), "");
}

#[test]
fn invalid_cors_configuration_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = common::prepare_config(T, dir.path(), 1, |c| {
        c.replace("allowOrigins: \"*\"", "allowOrigins: \"example.com\"")
    });
    let path = dir.path().join("c.yaml");
    std::fs::write(&path, cfg).unwrap();
    let (st, out, _) = common::run_binary(&["-config", path.to_str().unwrap()]);
    assert_eq!(st.code(), Some(1));
    assert!(
        out.contains("[CORS] Invalid origin format in configuration: example.com"),
        "{out}"
    );
}

#[test]
fn request_limits() {
    let s = Server::start(T);
    let answer = raw(
        &s,
        b"POST /api/v1/job/ HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 10485761\r\n\r\n",
    );
    assert!(answer.starts_with("HTTP/1.1 413"), "{answer}");
    assert!(answer.ends_with("Request Entity Too Large"), "{answer}");
    let max = vec![b' '; 10_485_760];
    let r = s.send(
        s.request("POST", "/api/v1/job/")
            .header("Content-Type", "application/json")
            .body(max),
    );
    assert_eq!(r.status, 400, "a body of exactly 10 MiB is read");
    // Chunked body above the limit.
    let mut req = b"POST /api/v1/job/ HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for _ in 0..11 {
        req.extend_from_slice(b"100000\r\n");
        req.extend(std::iter::repeat_n(b' ', 0x100000));
        req.extend_from_slice(b"\r\n");
    }
    req.extend_from_slice(b"0\r\n\r\n");
    assert!(raw(&s, &req).starts_with("HTTP/1.1 413"));
    // Oversized headers are refused without harming the server.
    let header = "a".repeat(20_000);
    let answer = raw(
        &s,
        format!("GET /ping HTTP/1.1\r\nHost: x\r\nX-Big: {header}\r\n\r\n").as_bytes(),
    );
    assert!(
        answer.is_empty() || answer.starts_with("HTTP/1.1 431"),
        "{answer}"
    );
    assert_eq!(s.get("/ping").status, 200);
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(data).unwrap();
    gz.finish().unwrap()
}

#[test]
fn compressed_bodies_are_limited_once_decoded() {
    let s = Server::start(T);
    let send = |encoding: &str, body: Vec<u8>| {
        s.send(
            s.request("POST", "/api/v1/job/")
                .header("Content-Type", "application/json")
                .header("Content-Encoding", encoding)
                .body(body),
        )
    };
    // 10 MiB + 1 of spaces: a few kilobytes on the wire, refused once decoded.
    let over = gzip(&vec![b' '; 10_485_761]);
    assert!(over.len() < 20_000);
    let r = send("gzip", over);
    assert_eq!(r.status, 413, "{}", r.text());
    // Nested encodings multiply the ratio: 64 MiB in a few hundred bytes.
    let bomb = gzip(&gzip(&vec![0u8; 1 << 26]));
    assert!(bomb.len() < 1_000);
    let r = send("gzip, gzip", bomb);
    assert_eq!(r.status, 413, "{}", r.text());
    // A decoded body of exactly the limit is read (and rejected as JSON, not as too large).
    let mut max = vec![b' '; 10_485_760];
    max[..2].copy_from_slice(b"{}");
    let r = send("gzip", gzip(&max));
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(s.get("/ping").status, 200);
}

#[test]
fn request_smuggling_is_rejected() {
    let s = Server::start(T);
    let answer = raw(
        &s,
        b"POST /api/v1/job/ HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n0\r\n\r\nGET /api/v1/jobs/all HTTP/1.1\r\nHost: x\r\n\r\n",
    );
    assert!(
        !answer.contains("\"jobs\""),
        "smuggled request answered: {answer}"
    );
}

#[test]
fn paths_are_not_resolved_on_disk_and_are_escaped() {
    let s = Server::start(T);
    for p in [
        "/docs/../Cargo.toml",
        "/docs/..%2f..%2fCargo.toml",
        "/docs/%2e%2e/Cargo.toml",
    ] {
        let answer = raw(
            &s,
            format!("GET {p} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        );
        assert!(!answer.contains("[package]"), "{p}: {answer}");
    }
    let answer = raw(
        &s,
        b"GET /x'\"&a HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(
        answer.starts_with("HTTP/1.1 404") || answer.starts_with("HTTP/1.1 400"),
        "{answer}"
    );
    if answer.starts_with("HTTP/1.1 404") {
        assert!(
            answer.ends_with("Cannot GET /x&#39;&#34;&amp;a"),
            "{answer}"
        );
    }
}

#[test]
fn event_log_lines_stay_valid_json() {
    let s = Server::start_with("standard/config/config.loadtest.yaml", |c| {
        c.replace(
            "eventsLogger:\n    enable: false",
            "eventsLogger:\n    enable: true",
        )
    });
    let topic = "a\"b\\c\nd</script>";
    let body = serde_json::json!({"topic": topic}).to_string();
    let id = s.create(&body);
    s.wait_state(&id, "queued", Duration::from_secs(3));
    let log = std::fs::read_to_string(s.dir.path().join("events.log")).unwrap();
    let mut found = false;
    for line in log.lines() {
        let v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
        if v["jobUUID"] == id.as_str() {
            assert_eq!(v["topic"], topic);
            found = true;
        }
    }
    assert!(found, "{log}");
}

#[test]
fn malformed_bodies() {
    let s = Server::start(T);
    // Deep nesting is refused by the JSON parser (limit 128) instead of exhausting the stack.
    let deep = format!(
        "{{\"properties\":{{\"a\":{}{}}}}}",
        "[".repeat(5000),
        "]".repeat(5000)
    );
    assert_eq!(s.post_json("/api/v1/job/", &deep).status, 400);
    // Invalid UTF-8 is replaced by U+FFFD, as in Go.
    let r = s.send(
        s.request("POST", "/api/v1/job/")
            .header("Content-Type", "application/json")
            .body(b"{\"topic\":\"a\xffb\"}".to_vec()),
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["job"]["topic"], "a\u{FFFD}b");
    // A callback with an unsupported scheme does not affect the server.
    let id = s.create(r#"{"topic":"cb","callbackUrl":"file:///etc/passwd"}"#);
    s.wait_state(&id, "queued", Duration::from_secs(3));
    assert_eq!(s.pull("topic=cb"), std::slice::from_ref(&id));
    assert_eq!(s.post(&format!("/api/v1/job/{id}/succeed")).status, 200);
    assert_eq!(s.get("/ping").status, 200);
}

#[test]
fn slow_clients_are_disconnected() {
    let s = Server::start(T);
    let mut tcp = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
    tcp.write_all(b"GET /ping HTTP/1.1\r\nHost: x\r\n").unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let start = Instant::now();
    let mut buf = [0u8; 256];
    let n = tcp.read(&mut buf).unwrap_or(0);
    let answer = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "connection kept {:?}",
        start.elapsed()
    );
    assert!(
        answer.is_empty() || answer.starts_with("HTTP/1.1 408"),
        "{answer}"
    );
}
