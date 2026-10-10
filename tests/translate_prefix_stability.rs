//! The translated route's prefix-stability property
//! (docs/internals/routing.md): for a conversation that only
//! appends, the codex request reproduces byte-identically wherever the
//! conversation did not change — even though the upstream bytes never
//! existed in the frontend's wire format. 20 deterministic seeded
//! cases, in the same style as tests/anthropic_prefix_stability.rs:
//! a shared seeded prefix (model, system, tools, sampling) plus a
//! generated message list, sliced short then sliced long, asserting
//! the serialised codex request's earlier input items are byte-identical.

mod common;

use common::Rng;
use serde_json::{Map, Value, json};
use toker::ir::Request;
use toker::translate::to_codex;

const MODEL: &str = "gpt-5.2-codex";
const KEY: &str = "prefix-cache-key";

/// A seeded conversation: the prefix fields every turn shares, plus
/// the message list the property slices.
struct Seeded {
    prefix: Map<String, Value>,
    messages: Vec<Value>,
}

/// The seeded anthropic models/systems/tools, kept apart from
/// common/mod.rs's same-protocol generator: the translated property
/// wants tool_use/tool_result turns and block-array content, which the
/// passthrough corpus never emits.
const MODELS: &[&str] = &["claude-opus-5", "claude-sonnet-4.6", "claude-haiku-4.5"];
const SYSTEMS: &[&str] = &[
    "You are a careful assistant.",
    "Be terse and verify every claim.",
    "Prefer canonical JSON output.",
];
const TOOL_NAMES: &[&str] = &["read_file", "list_dir", "run_command"];
const WORDS: &[&str] = &[
    "alpha",
    "bravo",
    "charlie",
    "delta",
    "echo",
    "Wellington",
    "café",
    "日本語",
    "🎉",
];

fn phrase(rng: &mut Rng) -> String {
    (0..1 + rng.below(5) as usize)
        .map(|_| rng.pick(WORDS))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One seeded user message: string content or a text-block array,
/// sometimes with an image block riding beside the text.
fn user_message(rng: &mut Rng) -> Value {
    let text = phrase(rng);
    if rng.below(2) == 0 {
        return json!({"role": "user", "content": text});
    }
    let mut blocks = vec![json!({"type": "text", "text": text})];
    if rng.below(4) == 0 {
        blocks.push(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "aVBONyU="},
        }));
    }
    json!({"role": "user", "content": blocks})
}

/// The seeded conversation: alternating user/assistant turns, some of
/// them tool turns (assistant tool_use, then a user tool_result whose
/// content is a string or a text-block array).
fn seeded(rng: &mut Rng) -> Seeded {
    let mut prefix = Map::new();
    prefix.insert("model".to_owned(), json!(rng.pick(MODELS)));
    match rng.below(3) {
        0 => {
            prefix.insert("system".to_owned(), json!(rng.pick(SYSTEMS)));
        }
        1 => {
            prefix.insert(
                "system".to_owned(),
                json!([
                    {"type": "text", "text": rng.pick(SYSTEMS)},
                    {"type": "text", "text": rng.pick(SYSTEMS)},
                ]),
            );
        }
        _ => {}
    }
    if rng.below(2) == 0 {
        let tools: Vec<Value> = (0..1 + rng.below(2))
            .map(|_| {
                json!({
                    "name": rng.pick(TOOL_NAMES),
                    "description": "Seeded tool.",
                    "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
                })
            })
            .collect();
        prefix.insert("tools".to_owned(), Value::Array(tools));
    }
    if rng.below(3) == 0 {
        prefix.insert("max_tokens".to_owned(), json!(1024));
    }
    if rng.below(2) == 0 {
        prefix.insert("temperature".to_owned(), json!(0.7));
    }

    let mut messages = Vec::new();
    for turn in 0..16u64 {
        let text = phrase(rng);
        if rng.below(3) == 0 {
            // A tool turn: assistant tool_use, then the user's result.
            let id = format!("toolu_{turn:02}");
            let tool = rng.pick(TOOL_NAMES);
            messages.push(json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": text},
                    {"type": "tool_use", "id": id, "name": tool,
                     "input": {"path": format!("file-{turn}.txt")}},
                ],
            }));
            let result = if rng.below(2) == 0 {
                json!(phrase(rng))
            } else {
                json!([
                    {"type": "text", "text": phrase(rng)},
                    {"type": "text", "text": phrase(rng)},
                ])
            };
            messages.push(json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": id, "content": result}],
            }));
        } else if rng.below(2) == 0 {
            messages.push(json!({"role": "assistant", "content": text}));
        } else {
            messages.push(json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": text},
                    {"type": "thinking", "thinking": "seeded reasoning",
                     "signature": "seeded-signature"},
                    {"type": "redacted_thinking", "data": "seeded-opaque"},
                ],
            }));
        }
        messages.push(user_message(rng));
    }
    Seeded { prefix, messages }
}

/// The body with the first `count` messages (the prefix is shared, so
/// short and long differ only in the message tail).
fn body_of(seeded: &Seeded, count: usize) -> Vec<u8> {
    let mut map = seeded.prefix.clone();
    map.insert(
        "messages".to_owned(),
        Value::Array(seeded.messages[..count].to_vec()),
    );
    serde_json::to_vec(&Value::Object(map)).expect("serialise generated body")
}

fn translate(bytes: &[u8]) -> serde_json::Value {
    let request = Request::parse(bytes).expect("generated body parses");
    let translated = to_codex(request.value(), MODEL, KEY).expect("generated body translates");
    serde_json::to_value(&translated).expect("serialises")
}

#[test]
fn appending_turns_keeps_the_serialised_input_prefix_byte_identical() {
    for case in 0..20u64 {
        let mut rng = Rng::seeded(0x7A05_C0DE + case);
        let seeded = seeded(&mut rng);
        let base_count = 2 + rng.below(10) as usize;
        let appended = 1 + rng.below(4) as usize;

        let short = translate(&body_of(&seeded, base_count));
        let long = translate(&body_of(&seeded, base_count + appended));

        // Everything but the input is identical: model, instructions,
        // tools, the wire constants, the sampling translations.
        let mut short_rest = short.clone();
        let mut long_rest = long.clone();
        short_rest
            .as_object_mut()
            .expect("object")
            .remove("input")
            .expect("input");
        long_rest
            .as_object_mut()
            .expect("object")
            .remove("input")
            .expect("input");
        assert_eq!(
            short_rest, long_rest,
            "case {case}: everything but the conversation is stable"
        );

        // The input arrays: every earlier item byte-identical, and the
        // short array a strict byte-prefix of the long one (the only
        // change starts exactly where the appended items begin).
        let short_items: Vec<&Value> = short["input"].as_array().expect("array").iter().collect();
        let long_items: Vec<&Value> = long["input"].as_array().expect("array").iter().collect();
        assert!(
            long_items.len() > short_items.len(),
            "case {case}: the appended turns must add input items"
        );
        for (index, (a, b)) in short_items.iter().zip(long_items.iter()).enumerate() {
            assert_eq!(
                serde_json::to_string(a).expect("serialise"),
                serde_json::to_string(b).expect("serialise"),
                "case {case}: item {index} must be byte-identical"
            );
        }
        let short_input = serde_json::to_string(&short["input"]).expect("serialise");
        let long_input = serde_json::to_string(&long["input"]).expect("serialise");
        assert_eq!(
            &long_input[..short_input.len() - 1],
            &short_input[..short_input.len() - 1],
            "case {case}: the serialised input array is prefix-stable"
        );
        assert_eq!(
            long_input.as_bytes()[short_input.len() - 1],
            b',',
            "case {case}: the change starts exactly at the appended items"
        );
    }
}
