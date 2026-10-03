//! Anthropic IR corpus tests: round-trip byte-equality plus shape
//! extraction pinned against values computed with ctp's own formulas
//! (claude-token-proxy proxy.mjs `requestShape`, run on each fixture via
//! Node — the digests below are ctp's output, not toker's, so any drift in
//! the port fails here). Release-marker parity is pinned the same way.
//!
//! Invariants covered: 5 (corpus round-trip), 1 (shapes carry no content),
//! 3 (batches bodies read `messages` as absent, not zero), 4 (strip is a
//! pure, byte-stable splice-equivalent).

use std::fs;
use std::path::{Path, PathBuf};

use toker::ir::{AnthropicShape, Request, SENTINEL, SystemBlockDigest};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anthropic")
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(fixtures_dir().join(name)).expect("fixture exists")
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

fn parse_fixture(name: &str) -> Request {
    let bytes = fixture(name);
    Request::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn shape_of(name: &str) -> AnthropicShape {
    parse_fixture(name).anthropic().shape()
}

/// A short-hand for a node-pinned block digest.
fn block(chars: u64, hash: &str) -> SystemBlockDigest {
    SystemBlockDigest {
        chars,
        hash: hash.to_owned(),
    }
}

// A test constructor that spells out every shape field — 12 args is the
// point here (the row/shape structs have no builders by design).
#[allow(clippy::too_many_arguments)]
fn expected_shape(
    req_bytes: u64,
    req_messages: Option<u64>,
    req_tools: u64,
    tools_hash: Option<&str>,
    system_chars: u64,
    system_hash: &str,
    system_blocks: Vec<SystemBlockDigest>,
    system_messages: Option<u64>,
    compact_generations: Option<u64>,
    summarising: bool,
    system_ladder: Vec<String>,
    system_tail: Vec<String>,
) -> AnthropicShape {
    AnthropicShape {
        req_bytes,
        req_messages,
        req_tools,
        tools_hash: tools_hash.map(str::to_owned),
        system_chars,
        system_hash: system_hash.to_owned(),
        system_blocks,
        system_messages,
        compact_generations,
        summarising,
        system_ladder,
        system_tail,
    }
}

fn digests(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn fixtures_corpus_round_trips_byte_exactly() {
    let paths = fixture_paths();
    assert!(
        paths.len() >= 10,
        "the anthropic corpus must keep at least 10 fixtures, found {}: {paths:?}",
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
fn minimal_body_shape_matches_ctp() {
    // ctp: reqBytes 171, systemChars 28, tail over the last 8/16/24 units.
    assert_eq!(
        shape_of("01_minimal.json"),
        expected_shape(
            171,
            Some(1),
            0,
            None,
            28,
            "9c5ab41ee459",
            vec![block(28, "9c5ab41ee459")],
            None,
            None,
            false,
            Vec::new(),
            digests(&["6474dee9737a", "a094adab6cc9", "e09053a85fc1"]),
        )
    );
    let request = parse_fixture("01_minimal.json");
    let view = request.anthropic();
    assert_eq!(view.model(), Some("claude-opus-5"));
    assert!(!view.stream());
    assert_eq!(view.messages().len(), 1);
}

#[test]
fn system_blocks_shape_matches_ctp_including_fallback_pieces() {
    // The bare string element and the text-less object both count as
    // pieces (the latter as ""), per ctp's `b?.text || ""`.
    assert_eq!(
        shape_of("02_system_blocks.json"),
        expected_shape(
            263,
            Some(1),
            0,
            None,
            60,
            "45c5eddeef7b",
            vec![
                block(28, "9c5ab41ee459"),
                block(13, "8e1ffd405a99"),
                block(19, "83c174dc9481"),
                block(0, "e3b0c44298fc"),
            ],
            None,
            None,
            false,
            Vec::new(),
            digests(&[
                "c060ae816c1a",
                "9d8cc92d97b0",
                "0c76f8f06bde",
                "5a3db31d99b4",
                "f726ad07455d",
                "cb046e9924db",
                "64f2aa3761e1",
            ]),
        )
    );
    assert!(parse_fixture("02_system_blocks.json").anthropic().stream());
}

#[test]
fn tools_shape_matches_ctps_name_extraction_and_join() {
    // read_file, list_dir, then the type fallback for the type-only entry.
    assert_eq!(
        shape_of("03_tools.json"),
        expected_shape(
            312,
            Some(1),
            3,
            Some("f6593ae36305"),
            9,
            "28c7339ead79",
            vec![block(9, "28c7339ead79")],
            None,
            None,
            false,
            Vec::new(),
            digests(&["1c4d8b7a394f"]),
        )
    );
}

#[test]
fn batches_body_shapes_without_a_top_level_messages() {
    // Absence ≠ zero (invariant 3): the params-nested messages must not
    // be read, and the shape must still extract.
    assert_eq!(
        shape_of("04_batches.json"),
        expected_shape(
            246,
            None,
            0,
            None,
            0,
            "e3b0c44298fc",
            Vec::new(),
            None,
            None,
            false,
            Vec::new(),
            Vec::new(),
        )
    );
}

#[test]
fn compaction_preamble_counts_generations_in_the_first_message() {
    // Two continuation preambles across two text blocks of message one.
    assert_eq!(
        shape_of("05_compaction_preamble.json"),
        expected_shape(
            413,
            Some(3),
            0,
            None,
            28,
            "9c5ab41ee459",
            vec![block(28, "9c5ab41ee459")],
            None,
            Some(2),
            false,
            Vec::new(),
            digests(&["6474dee9737a", "a094adab6cc9", "e09053a85fc1"]),
        )
    );
}

#[test]
fn performing_compaction_is_summarising_with_tools() {
    let shape = shape_of("06_compaction_performing.json");
    assert_eq!(
        shape,
        expected_shape(
            366,
            Some(3),
            1,
            Some("c9f8123bd272"),
            0,
            "e3b0c44298fc",
            Vec::new(),
            None,
            None,
            true,
            Vec::new(),
            Vec::new(),
        )
    );
    assert!(
        shape.is_compaction(),
        "summarising with the session's tools"
    );
}

#[test]
fn routine_summariser_is_summarising_but_not_a_compaction() {
    // ctp cold.mjs `isCompaction`: the title summariser carries the
    // wording but no tools — the measured separator between a compaction
    // and a routine one-shot.
    let shape = shape_of("07_summariser_no_tools.json");
    assert_eq!(
        shape,
        expected_shape(
            170,
            Some(1),
            0,
            None,
            0,
            "e3b0c44298fc",
            Vec::new(),
            None,
            None,
            true,
            Vec::new(),
            Vec::new(),
        )
    );
    assert!(!shape.is_compaction());
}

#[test]
fn release_marker_fixture_carries_and_shapes() {
    // The marker opens the last user message, with Claude Code's trailing
    // mid-conversation system message after it.
    let request = parse_fixture("08_release_marker.json");
    assert!(request.anthropic().carries_release());
    assert_eq!(
        request.anthropic().shape(),
        expected_shape(
            296,
            Some(4),
            0,
            None,
            28,
            "9c5ab41ee459",
            vec![block(28, "9c5ab41ee459")],
            Some(1),
            None,
            false,
            Vec::new(),
            digests(&["6474dee9737a", "a094adab6cc9", "e09053a85fc1"]),
        )
    );
}

#[test]
fn strip_is_byte_equal_to_the_hand_done_splice() {
    // ctp's splice on this fixture (verified in Node): keep the opening
    // quote, remove the 10 marker bytes. toker's IR removal must produce
    // exactly those bytes — the equivalence the strip's doc claims.
    let original = fixture("08_release_marker.json");
    let mut request = Request::parse(&original).expect("parse");
    request.anthropic_mut().strip_release();

    let expected: &[u8] = br#"{"model":"claude-opus-5","system":"You are a careful assistant.","messages":[{"role":"user","content":"Earlier work."},{"role":"assistant","content":"Ok."},{"role":"user","content":[{"type":"text","text":" keep going"}]},{"role":"system","content":[{"type":"text","text":"reminder"}]}]}"#;
    assert_eq!(request.serialise(), expected);

    // The splice window assertions, ctp limit-sentinel.mjs style:
    // everything outside the spliced literal is preserved byte for byte.
    let out = request.serialise();
    let needle = format!("\"{SENTINEL}");
    let at = original
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())
        .expect("needle in the original");
    assert_eq!(
        &original[..at],
        &out[..at],
        "bytes before the needle differ"
    );
    let after_marker = at + 1 + SENTINEL.len();
    assert_eq!(
        &original[after_marker..],
        &out[at + 1..],
        "bytes after the marker differ"
    );
    assert_eq!(out.len(), original.len() - SENTINEL.len());
    assert!(!request.anthropic().carries_release(), "gone once stripped");

    // Idempotent: a second pass finds nothing to do.
    let once = request.serialise();
    request.anthropic_mut().strip_release();
    assert_eq!(request.serialise(), once);
}

#[test]
fn marker_negatives_never_fire_or_strip() {
    // Mid-text marker (code fragment and mid-sentence), the compaction
    // wordings quoted mid-line in the last message, the resume preamble
    // outside the first message: ctp's shape is all-absent, the strip is
    // a byte-for-byte no-op, and the marker never reads as carried.
    let request = parse_fixture("09_marker_negatives.json");
    assert!(!request.anthropic().carries_release());
    assert_eq!(
        request.anthropic().shape(),
        expected_shape(
            473,
            Some(3),
            0,
            None,
            0,
            "e3b0c44298fc",
            Vec::new(),
            None,
            None,
            false,
            Vec::new(),
            Vec::new(),
        )
    );
    let mut request = request;
    let original = fixture("09_marker_negatives.json");
    request.anthropic_mut().strip_release();
    assert_eq!(
        request.serialise(),
        original,
        "an ambiguous or unanchored marker must leave the body untouched"
    );
}

#[test]
fn unicode_system_ladder_matches_ctps_utf16_semantics() {
    // 16587 UTF-16 units (not scalar chars): two prefix rungs, and the
    // tail ladder at all 44 offsets. Pinned against ctp's Node output on
    // this exact fixture; the sampled tail entries are the first (8
    // units), the 16th (128), the last fine step (256), the first coarse
    // step (320), and the last (1024).
    let shape = shape_of("10_unicode_ladder.json");
    assert_eq!(shape.req_bytes, 17770);
    assert_eq!(shape.system_chars, 16587);
    assert_eq!(shape.system_hash, "8326feee8b41");
    assert_eq!(shape.system_blocks, vec![block(16587, "8326feee8b41")]);
    assert_eq!(
        shape.system_ladder,
        digests(&["002a5387bb5f", "183c4a306988"])
    );
    assert_eq!(shape.system_tail.len(), 44);
    assert_eq!(shape.system_tail[0], "2f75f2283379");
    assert_eq!(shape.system_tail[15], "845d21892116");
    assert_eq!(shape.system_tail[31], "e1edb4a2df26");
    assert_eq!(shape.system_tail[32], "b931773ab671");
    assert_eq!(shape.system_tail[43], "aeb7c64058d3");
}
