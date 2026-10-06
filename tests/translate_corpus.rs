//! The golden corpus for the request direction: every fixture in
//! tests/fixtures/anthropic/ that is a `/v1/messages` body, translated
//! to its codex request and pinned — the request table
//! ([`toker::translate`] module docs) hand-applied, fixture by fixture.
//!
//! Not every fixture translates: 03 carries a tool the Responses wire
//! cannot express faithfully (an unnamed custom-tool shape —
//! anthropic's own API rejects it), and 04 is a batches body (its
//! payloads nest under `requests[].params`, no top-level `messages`).
//! Both are pinned as the typed errors they produce, so the corpus
//! covers every fixture either way — and a new fixture cannot land
//! unpinned (`every_json_fixture_is_pinned_by_name` fails until a pin
//! is added).

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use toker::ir::Request;
use toker::providers::codex::ResponsesRequest;
use toker::translate::{TranslateError, to_codex};

const MODEL: &str = "gpt-5.2-codex";
const KEY: &str = "corpus-cache-key";

/// The fixtures this corpus pins, by name — must stay identical to the
/// directory's json files (`every_json_fixture_is_pinned_by_name`).
const PINNED: &[&str] = &[
    "01_minimal.json",
    "02_system_blocks.json",
    "03_tools.json",
    "04_batches.json",
    "05_compaction_preamble.json",
    "06_compaction_performing.json",
    "07_summariser_no_tools.json",
    "08_release_marker.json",
    "09_marker_negatives.json",
    "10_unicode_ladder.json",
    "11_tool_use_tool_result.json",
    "12_image_thinking.json",
];

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anthropic")
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(fixtures_dir().join(name)).expect("fixture exists")
}

fn fixture_value(name: &str) -> Value {
    serde_json::from_slice(&fixture(name)).expect("fixture is JSON")
}

fn translate(name: &str) -> Result<ResponsesRequest, TranslateError> {
    let request = Request::parse(&fixture(name)).expect("fixture parses");
    to_codex(request.value(), MODEL, KEY)
}

fn pinned(name: &str) -> Value {
    serde_json::to_value(translate(name).expect("translates")).expect("serialises")
}

/// The wire constants every pin carries (unit A's constructor, called
/// out once here so the per-fixture pins stay about the translation).
fn wire_constants_pinned(value: &Value) {
    assert_eq!(value["model"], json!(MODEL), "the caller's model param");
    assert_eq!(value["stream"], json!(true));
    assert_eq!(value["tool_choice"], json!("auto"));
    assert_eq!(value["parallel_tool_calls"], json!(false));
    assert_eq!(value["store"], json!(false));
    assert_eq!(
        value["include"],
        json!(["reasoning.encrypted_content"]),
        "the reasoning round-trip constant"
    );
    assert_eq!(value["prompt_cache_key"], json!(KEY));
    assert_eq!(value["reasoning"], json!({"effort": null}));
}

#[test]
fn every_json_fixture_is_pinned_by_name() {
    let mut names: Vec<String> = fs::read_dir(fixtures_dir())
        .expect("fixture directory exists")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json"))
        .collect();
    names.sort();
    assert_eq!(
        names, PINNED,
        "every anthropic corpus fixture must appear in PINNED, and \
         every PINNED entry must be a fixture"
    );
    assert!(
        names.len() >= 12,
        "the corpus must keep at least 12 fixtures"
    );
}

#[test]
fn minimal_translates_and_pins_the_full_request_bytes() {
    let value = pinned("01_minimal.json");
    wire_constants_pinned(&value);
    // The full serialised request, byte-pinned: the pinned field order
    // (routing fields, wire constants, cache key) with the sampling
    // translations riding last in their fixed order.
    let bytes = serde_json::to_string(&translate("01_minimal.json").expect("translates"))
        .expect("serialise");
    assert_eq!(
        bytes,
        concat!(
            r#"{"model":"gpt-5.2-codex","stream":true,"instructions":"You are a careful assistant.","#,
            r#""input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Summarise the current state."}]}],"#,
            r#""tools":[],"tool_choice":"auto","parallel_tool_calls":false,"reasoning":{"effort":null},"#,
            r#""store":false,"include":["reasoning.encrypted_content"],"prompt_cache_key":"corpus-cache-key"}"#
        )
    );
    // And the same pin as a value, for the table.
    assert_eq!(
        value,
        json!({
            "model": MODEL, "stream": true,
            "instructions": "You are a careful assistant.",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "Summarise the current state."}]},
            ],
            "tools": [],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "reasoning": {"effort": null},
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": KEY,
        })
    );
}

#[test]
fn system_blocks_translate_to_joined_instructions() {
    let value = pinned("02_system_blocks.json");
    wire_constants_pinned(&value);
    // Each semantic text block and the bare string element join on blank
    // lines. The object without text remains an opaque canonical part and is
    // reported by the loss-aware adapter rather than fabricating empty text.
    assert_eq!(
        value["instructions"],
        json!("You are a careful assistant.\n\nSecond block.\n\nbare string element")
    );
    assert_eq!(
        value["input"],
        json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "Hi"}]},
        ])
    );
    assert_eq!(
        serde_json::to_value(
            translate("02_system_blocks.json")
                .expect("translates")
                .extra
        )
        .expect("serialise"),
        json!({}),
        "the anthropic stream flag maps to nothing: the codex turn always streams"
    );
}

#[test]
fn a_tool_the_wire_cannot_express_fails_the_translation() {
    // read_file and list_dir are clean function tools; the third entry
    // has no name and no input_schema — a shape anthropic's own API
    // rejects and the Responses function tool cannot carry. Reported,
    // never fabricated into a callable.
    let error = translate("03_tools.json").expect_err("the unnamed tool fails the body");
    assert!(
        matches!(&error, TranslateError::Malformed { reason } if reason.contains("tools[2]")),
        "the reason names the offending entry: {error:?}"
    );
}

#[test]
fn the_batches_body_is_not_a_messages_body() {
    let error = translate("04_batches.json").expect_err("no top-level messages");
    assert!(
        matches!(&error, TranslateError::Malformed { reason } if reason.contains("messages")),
        "the batches body is reported, not guessed into shape: {error:?}"
    );
}

#[test]
fn compaction_preamble_translates_block_content_in_order() {
    let value = pinned("05_compaction_preamble.json");
    wire_constants_pinned(&value);
    assert_eq!(value["instructions"], json!("You are a careful assistant."));
    // The user's two text blocks group into ONE message item; the
    // assistant string becomes one output_text item.
    assert_eq!(
        value["input"],
        json!([
            {"type": "message", "role": "user", "content": [
                {"type": "input_text",
                 "text": "This session is being continued from a previous conversation. The summary follows."},
                {"type": "input_text",
                 "text": "This session is being continued from a previous conversation (second generation)."},
            ]},
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "Understood."}]},
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "Continue the work."}]},
        ])
    );
}

#[test]
fn compaction_performing_translates_the_tools_and_messages() {
    let value = pinned("06_compaction_performing.json");
    wire_constants_pinned(&value);
    // The tool has no description — "" on the wire, never fabricated.
    assert_eq!(
        value["tools"],
        json!([
            {"type": "function", "name": "read_file", "description": "",
             "strict": false, "parameters": {"type": "object"}},
        ])
    );
    assert_eq!(
        value["input"],
        json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "Earlier work."}]},
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "Done."}]},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text",
                 "text": "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\nYour task is to create a detailed summary of the conversation so far."},
            ]},
        ])
    );
}

#[test]
fn the_summariser_translates_with_no_system() {
    let value = pinned("07_summariser_no_tools.json");
    wire_constants_pinned(&value);
    assert_eq!(
        value["instructions"],
        json!(""),
        "no system, no instructions"
    );
    assert_eq!(
        value["input"],
        json!([
            {"type": "message", "role": "user", "content": [
                {"type": "input_text",
                 "text": "Your task is to create a detailed summary of this conversation for a title."},
            ]},
        ])
    );
}

#[test]
fn the_release_marker_rides_through_and_system_roles_translate() {
    let value = pinned("08_release_marker.json");
    wire_constants_pinned(&value);
    // The marker is middleware's to strip, not translation's — it
    // rides in the user text verbatim. The trailing mid-conversation
    // system-role message merges into the PRECEDING user turn as a
    // `[PROMPT_INJECTION]`-prefixed text part (the same transform the
    // predecessor proxy used for
    // the same problem; the codex backend refuses system-role input
    // items — verified live: "System messages are not allowed").
    assert_eq!(
        value["input"],
        json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "Earlier work."}]},
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "Ok."}]},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "$#$BURN$#$ keep going"},
                {"type": "input_text", "text": "[PROMPT_INJECTION] reminder"},
            ]},
        ])
    );
    assert_eq!(value["instructions"], json!("You are a careful assistant."));
}

#[test]
fn marker_negatives_translate_the_block_arrays() {
    let value = pinned("09_marker_negatives.json");
    wire_constants_pinned(&value);
    assert_eq!(value["instructions"], json!(""));
    assert_eq!(
        value["input"],
        json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "Start the work."}]},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text",
                 "text": "Noted. I read that 'This session is being continued from a previous conversation' marks a resume."},
            ]},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text",
                 "text": "Context: the guard is:\nif (tag === \"$#$BURN$#$\") burn();"},
                {"type": "input_text",
                 "text": "Also note the wording 'Your task is to create a detailed summary of' is quoted mid-line, and please run $#$BURN$#$ later."},
            ]},
        ])
    );
}

#[test]
fn the_unicode_ladder_carries_the_system_verbatim() {
    let value = pinned("10_unicode_ladder.json");
    wire_constants_pinned(&value);
    // The instructions are the system string verbatim — 16,542
    // characters of unicode, café to 🎉, never re-wrapped or trimmed.
    let source = fixture_value("10_unicode_ladder.json");
    assert_eq!(value["instructions"], source["system"]);
    assert_eq!(
        value["input"],
        json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "Begin."}]},
        ])
    );
    // Sampling never crosses (the live-verified drop): the fixture
    // carries max_tokens 4096, the translated request carries nothing.
    assert!(value.get("max_output_tokens").is_none());
}

#[test]
fn tool_use_tool_result_pairs_translate_to_calls_and_outputs() {
    let value = pinned("11_tool_use_tool_result.json");
    wire_constants_pinned(&value);
    assert_eq!(value["instructions"], json!("Be terse."));
    assert_eq!(
        value["tools"],
        json!([
            {"type": "function", "name": "read_file", "description": "Read a file",
             "strict": false,
             "parameters": {"type": "object",
                            "properties": {"path": {"type": "string"}},
                            "required": ["path"]}},
            {"type": "function", "name": "list_dir", "description": "List a directory",
             "strict": false,
             "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}},
        ])
    );
    assert_eq!(
        value["input"],
        json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "Read the config."}]},
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "Reading."}]},
            {"type": "function_call", "name": "read_file",
             "arguments": "{\"path\":\"toker.toml\"}", "call_id": "toolu_01A"},
            {"type": "function_call_output", "call_id": "toolu_01A",
             "output": "# toker config\nupstream = \"anthropic\""},
            {"type": "function_call", "name": "list_dir",
             "arguments": "{\"path\":\"src\"}", "call_id": "toolu_01B"},
            // The text before the tool_result flushes its own message
            // item; the block-array tool_result content rides as the
            // JSON of the array.
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "The directory:"}]},
            {"type": "function_call_output", "call_id": "toolu_01B",
             "output": "[{\"type\":\"text\",\"text\":\"src holds\"},{\"type\":\"text\",\"text\":\"two modules.\"}]"},
        ])
    );
    // Sampling never crosses (the live-verified drop): the fixture
    // carries max_tokens 2048, the translated request carries nothing.
    assert!(value.get("max_output_tokens").is_none());
}

#[test]
fn images_translate_and_thinking_drops() {
    let value = pinned("12_image_thinking.json");
    wire_constants_pinned(&value);
    assert_eq!(value["instructions"], json!(""));
    assert_eq!(
        value["input"],
        json!([
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "What is in this picture?"},
                {"type": "input_image", "image_url": "data:image/png;base64,aVBONyU="},
            ]},
            // The assistant's thinking and redacted_thinking blocks
            // are DROPPED (the loud note in the translate docs); only
            // the text item survives.
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "It is a chart."}]},
        ])
    );
    // The loud drop, asserted: none of the reasoning content rides
    // anywhere in the request.
    let bytes = serde_json::to_string(&translate("12_image_thinking.json").expect("translates"))
        .expect("serialise");
    assert!(!bytes.contains("The image shows a chart."));
    assert!(!bytes.contains("EncryptedBlob123"));
    assert!(!bytes.contains("EqQBCkgIBRABGAIiQK3"));
    // Sampling never crosses (the live-verified drop): the fixture
    // carries max_tokens 512, the translated request carries nothing.
    assert!(value.get("max_output_tokens").is_none());
}
