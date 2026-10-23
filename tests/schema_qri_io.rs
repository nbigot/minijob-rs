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
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/qri-io-schema-cases.json"
    );
    let cases: Vec<Case> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(cases.len() > 300);
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
    assert!(checked > 3000, "{checked} instances checked");
}
