//! Go time layouts and durations against results of Go 1.25's `time.Time.Format` and
//! `time.Duration.String` (log timestamps, `timeEncoder: {layout: ...}`, event durations).
//!
//! `fixtures/go-time-format.json` holds random concatenations of every layout element and of
//! look-alike text (`Janu`, `Mond`, `_2006`, `.0x`), formatted at times from year 0 to 9999 with
//! various offsets (named `UTC` at offset 0, unnamed otherwise), and durations up to the largest.

use chrono::DateTime;
use minijob::log::gotime;
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
struct Vectors {
    format: Vec<FormatCase>,
    duration: Vec<DurationCase>,
}

#[derive(Deserialize)]
struct FormatCase {
    layout: String,
    time: String,
    expected: String,
}

#[derive(Deserialize)]
struct DurationCase {
    nanos: u64,
    expected: String,
}

fn vectors() -> Vectors {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/go-time-format.json"
    );
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn layouts_like_go() {
    let cases = vectors().format;
    assert!(cases.len() >= 1000);
    for case in &cases {
        let t = DateTime::parse_from_rfc3339(&case.time).unwrap();
        assert_eq!(
            gotime::format(&t, &case.layout),
            case.expected,
            "layout {:?} at {}",
            case.layout,
            case.time
        );
    }
}

#[test]
fn durations_like_go() {
    let cases = vectors().duration;
    assert!(cases.len() >= 200);
    for case in &cases {
        assert_eq!(
            gotime::duration_string(Duration::from_nanos(case.nanos)),
            case.expected,
            "{} ns",
            case.nanos
        );
    }
}
