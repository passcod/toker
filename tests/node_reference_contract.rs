//! The vendored parity contract (a verbatim copy of the predecessor
//! proxy's `node-reference-v1.json`, vendored at
//! `tests/fixtures/ctp/node-reference-v1.json`).
//!
//! The later phase-2 gate units build their parity tests against this
//! fixture: it is the language-neutral, content-free statement of the
//! pure decision interfaces (usage presence, route identity, quota
//! decision/release, cold decision, compaction decision, awake decision)
//! with the exact normalised results. This test keeps the vendored copy
//! honest: present, parseable, and carrying the v1 contract tag with the
//! documented case shape.

use std::fs;
use std::path::Path;

use serde_json::Value;

fn contract_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ctp/node-reference-v1.json")
}

#[test]
fn node_reference_fixture_parses_and_carries_the_v1_contract_tag() {
    let bytes = fs::read(contract_path()).expect("vendored contract fixture exists");
    let value: Value =
        serde_json::from_slice(&bytes).expect("vendored contract fixture is valid JSON");

    assert_eq!(
        value.get("contract").and_then(Value::as_str),
        Some("ctp-node-reference/v1"),
        "the contract tag pins the parity language and version"
    );

    let cases = value
        .get("cases")
        .and_then(Value::as_array)
        .expect("an ordered cases array");
    assert!(!cases.is_empty(), "the contract carries its case set");

    let mut ids = Vec::new();
    for case in cases {
        let id = case
            .get("id")
            .and_then(Value::as_str)
            .expect("every case has a stable id");
        assert!(
            case.get("operation").and_then(Value::as_str).is_some(),
            "case {id}: names one pure decision interface"
        );
        assert!(
            case.get("expected").is_some(),
            "case {id}: carries the exact normalised expected result"
        );
        assert!(
            case.get("input").is_some(),
            "case {id}: carries a synthetic input"
        );
        ids.push(id);
    }
    ids.sort();
    let mut duplicates = ids.windows(2).filter(|pair| pair[0] == pair[1]);
    assert!(
        duplicates.next().is_none(),
        "case ids are stable and unique"
    );
}
