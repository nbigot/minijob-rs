//! JSON schema validation against results of qri-io/jsonschema v0.2.1 (the Go server's
//! validator, built with Go 1.25): random schemas over every keyword, Go regexp patterns, and
//! strings for the `format` checks (url.Parse, mail.ParseAddress, time.Parse).
//!
//! `fixtures/qri-io-schema-cases.json` holds the schemas, the instances and the errors qri-io
//! reported, sorted. Cases with documented differences are left out: references to draft-07
//! `definitions` or to an `$anchor` (unresolved by qri-io), and `contains` messages (Go printed
//! memory addresses).

use minijob::gojson::{decode::parse_value, fmt_v};
use minijob::schema::Schema;
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    schema: String,
    compiles: bool,
    instances: Vec<String>,
    #[serde(default)]
    expected: Vec<Vec<[String; 3]>>,
}

#[test]
fn same_errors_as_qri_io() {
    let checked = check_cases("qri-io-schema-cases.json");
    assert!(checked > 3000, "{checked} instances checked");
}

/// Schemas whose references loop or recurse (`$ref` to `""`, `#`, `#/items/0`...,
/// `$recursiveRef`, `$recursiveAnchor`), validated by Go minijob v1.1.2: qri-io with its
/// loop-safe `$ref` and `definitions` as `$defs`, built with Go 1.25, a fresh schema for
/// each instance (qri-io keeps the references it resolved). `fixtures/go-schema-ref-loops.json`
/// leaves out `$anchor` references, `contains` messages and the schemas Go refuses.
#[test]
fn reference_loops_like_go() {
    let checked = check_cases("go-schema-ref-loops.json");
    assert!(checked >= 1200, "{checked} instances checked");
}

/// Checks the cases of a fixture; returns the number of instances checked.
fn check_cases(fixture: &str) -> usize {
    let path = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let cases: Vec<Case> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(cases.len() >= 200);
    let mut checked = 0;
    for (i, case) in cases.iter().enumerate() {
        let schema = Schema::compile(case.schema.as_bytes());
        assert_eq!(
            schema.is_ok(),
            case.compiles,
            "case {i}: {}: {:?}",
            case.schema,
            schema.err()
        );
        let Ok(schema) = schema else { continue };
        for (instance, expected) in case.instances.iter().zip(&case.expected) {
            let doc = parse_value(instance.as_bytes()).unwrap();
            let mut errors: Vec<[String; 3]> = schema
                .validate(&doc)
                .into_iter()
                .map(|e| [e.property_path, e.message, fmt_v(&e.invalid_value)])
                .collect();
            errors.sort();
            assert_eq!(
                &errors, expected,
                "case {i}\nschema:   {}\ninstance: {instance}",
                case.schema
            );
            checked += 1;
        }
    }
    checked
}

#[derive(Deserialize)]
struct ErrorCase {
    schema: String,
    error: String,
    #[serde(default)]
    instances: Vec<String>,
    #[serde(default)]
    expected: Vec<Vec<[String; 3]>>,
}

/// Schemas as qri-io decodes them (Go's `json.Unmarshal` into its keyword types):
/// `fixtures/qri-io-schema-errors.json` holds every keyword set to values of each JSON kind,
/// with the error qri-io gave (empty when it compiled), and keywords set to `null`, `-0`,
/// empty values or with `null` elements (zero values, false or nil schemas), with the errors
/// of a few instances. JSON syntax errors are left out (worded by serde), as are panics.
#[test]
fn same_compile_errors_as_qri_io() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/qri-io-schema-errors.json"
    );
    let cases: Vec<ErrorCase> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(cases.len() > 1500);
    let mut validated = 0;
    for case in &cases {
        let schema = Schema::compile(case.schema.as_bytes());
        assert_eq!(
            schema.as_ref().err().map_or("", String::as_str),
            case.error,
            "schema {}",
            case.schema
        );
        let Ok(schema) = schema else { continue };
        for (instance, expected) in case.instances.iter().zip(&case.expected) {
            let doc = parse_value(instance.as_bytes()).unwrap();
            let mut errors: Vec<[String; 3]> = schema
                .validate(&doc)
                .into_iter()
                .map(|e| [e.property_path, e.message, fmt_v(&e.invalid_value)])
                .collect();
            errors.sort();
            let mut expected = expected.clone();
            expected.sort();
            assert_eq!(
                errors, expected,
                "schema {}\ninstance {instance}",
                case.schema
            );
            validated += 1;
        }
    }
    assert!(validated > 1500, "{validated} instances checked");
}
