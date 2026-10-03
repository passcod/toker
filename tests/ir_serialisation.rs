//! Invariant 5's corpus and monitor tests (plan: Invariants 4 and 5).
//!
//! - [`fixtures_corpus_round_trips_byte_exactly`] and
//!   [`generated_bodies_round_trip_byte_exactly`]:
//!   `serialise(parse(body)) == body` over realistic OpenAI-chat bodies —
//!   the round-trip byte-equality corpus the plan requires.
//! - [`non_canonical_inputs_drift_and_the_monitor_reports_it`]: legal JSON
//!   outside serde_json's canonical form (a `\/` escape, inter-token
//!   whitespace) must NOT round-trip, and `compare` must say so — the
//!   monitor detects drift, it does not assume its absence.
//! - [`set_model_changes_only_the_model_value_region`] and
//!   [`set_model_inserts_at_the_end_when_absent`]: the one deliberate byte
//!   edit touches only the model value's region, in place or appended.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Rng, conversation_body};
use toker::ir::{Fidelity, Request, compare};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/openai_chat")
}

fn fixture_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(fixtures_dir())
        .expect("fixture directory exists")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    paths
}

#[test]
fn fixtures_corpus_round_trips_byte_exactly() {
    let paths = fixture_paths();
    assert!(
        paths.len() >= 10,
        "the corpus must keep at least 10 fixtures, found {}: {paths:?}",
        paths.len()
    );
    for path in &paths {
        let original = fs::read(path).expect("read fixture");
        let request =
            Request::parse(&original).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(
            request.serialise(),
            original,
            "{}: round-trip must be byte-exact",
            path.display()
        );
    }
}

#[test]
fn generated_bodies_round_trip_byte_exactly() {
    for seed in 1..=4u64 {
        let mut rng = Rng::seeded(seed);
        let body = conversation_body(&mut rng, 3 + seed as usize);
        let request = Request::parse(&body).expect("generated body parses");
        assert_eq!(
            request.serialise(),
            body,
            "seed {seed}: generated bodies are canonical"
        );
    }
}

#[test]
fn non_canonical_inputs_drift_and_the_monitor_reports_it() {
    // `\/` is a legal JSON escape for `/`; serde_json's canonical form
    // never emits it. This input must NOT round-trip — proving the corpus
    // above tests something — and the monitor must catch what it misses.
    let escaped = b"{\"model\":\"a\\/b\",\"messages\":[]}";
    let request = Request::parse(escaped).expect("\\/ is legal JSON");
    let serialised = request.serialise();
    assert_ne!(serialised.as_slice(), escaped.as_slice());
    assert_eq!(
        serialised.as_slice(),
        b"{\"model\":\"a/b\",\"messages\":[]}".as_slice()
    );
    match compare(escaped, &serialised) {
        Fidelity::Drift { offset: 11, .. } => {}
        other => panic!("a \\/ escape must drift at the escape, got {other:?}"),
    }

    // Whitespace between tokens: same story, region at the first space.
    let padded = b"{ \"model\": \"x\", \"messages\": [] }";
    let request = Request::parse(padded).expect("whitespace is legal JSON");
    let serialised = request.serialise();
    assert_eq!(
        serialised.as_slice(),
        b"{\"model\":\"x\",\"messages\":[]}".as_slice()
    );
    match compare(padded, &serialised) {
        Fidelity::Drift { offset: 1, .. } => {}
        other => panic!("whitespace must drift at the first space, got {other:?}"),
    }

    // And the normal case really is Exact, not assumed.
    let canonical = b"{\"model\":\"x\",\"messages\":[]}";
    assert_eq!(compare(canonical, canonical), Fidelity::Exact);
}

fn find(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap_or_else(|| panic!("expected {needle:?} in the body"))
}

#[test]
fn set_model_changes_only_the_model_value_region() {
    let body =
        br#"{"model":"gpt-5.2-mini","messages":[{"role":"user","content":"Hi"}],"stream":true}"#;
    let before = Request::parse(body).expect("parse").serialise();

    let mut request = Request::parse(body).expect("parse");
    request.openai_chat_mut().set_model("gpt-5.2");
    let after = request.serialise();

    assert_ne!(before, after);
    assert_eq!(request.openai_chat().model(), Some("gpt-5.2"));

    let old_value = br#""gpt-5.2-mini""#;
    let new_value = br#""gpt-5.2""#;
    let at = find(&before, old_value);
    assert_eq!(at, find(&after, new_value), "the model key does not move");
    assert_eq!(
        &before[..at],
        &after[..at],
        "everything before the model value is byte-identical"
    );
    assert_eq!(
        &before[at + old_value.len()..],
        &after[at + new_value.len()..],
        "everything after the model value is byte-identical"
    );
}

#[test]
fn set_model_inserts_at_the_end_when_absent() {
    let body = br#"{"messages":[{"role":"user","content":"Hi"}],"stream":false}"#;
    let mut request = Request::parse(body).expect("parse");
    request.openai_chat_mut().set_model("glm-5.3");
    let after = request.serialise();

    let tail: &[u8] = br#","model":"glm-5.3"}"#;
    assert_eq!(after.len(), body.len() - 1 + tail.len());
    assert_eq!(&after[..body.len() - 1], &body[..body.len() - 1]);
    assert_eq!(&after[body.len() - 1..], tail);
    assert_eq!(request.openai_chat().model(), Some("glm-5.3"));
}
