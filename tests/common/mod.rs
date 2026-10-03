//! Shared helpers for the IR integration tests: a deterministic PRNG and a
//! canonical-body generator. Seeded, not random — the corpus and the
//! prefix-stability property reproduce byte-for-byte on every run, without
//! a proptest dependency.
//!
//! This module is compiled into every integration-test binary, and each binary
//! uses a different subset of the helpers — hence the module-level dead_code
//! allow below.

#![allow(dead_code)]

/// splitmix64: small, deterministic, plenty to vary generated bodies.
pub struct Rng(u64);

impl Rng {
    pub fn seeded(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

pub const WORDS: &[&str] = &[
    "alpha",
    "bravo",
    "charlie",
    "delta",
    "echo",
    "foxtrot",
    "golf",
    "hotel",
    "india",
    "juliet",
    "kilo",
    "lima",
    "Wellington",
    "café",
    "日本語",
    "🎉",
];
const MODELS: &[&str] = &[
    "openai/gpt-5.2",
    "z-ai/glm-5.3",
    "anthropic/claude-sonnet-4.6",
    "qwen/qwen4-max",
];
const TOOL_NAMES: &[&str] = &["read_file", "list_dir", "run_command"];

/// A canonical OpenAI-chat body with `messages` as the last key — the shape
/// the prefix-stability property splices at. `message_count` messages of
/// alternating roles, plus a seeded-random spread of the surrounding keys.
pub fn conversation_body(rng: &mut Rng, message_count: usize) -> Vec<u8> {
    let mut map = serde_json::Map::new();
    map.insert("model".to_owned(), serde_json::json!(rng.pick(MODELS)));
    if rng.below(2) == 0 {
        map.insert("stream".to_owned(), serde_json::json!(true));
    }
    if rng.below(2) == 0 {
        map.insert("temperature".to_owned(), serde_json::json!(0.7));
    }
    if rng.below(3) == 0 {
        let tools: Vec<_> = (0..1 + rng.below(2))
            .map(|_| {
                serde_json::json!({
                    "type": "function",
                    "function": {"name": rng.pick(TOOL_NAMES)},
                })
            })
            .collect();
        map.insert("tools".to_owned(), serde_json::Value::Array(tools));
    }
    let mut messages = Vec::with_capacity(message_count);
    for i in 0..message_count {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        let words = 1 + rng.below(5) as usize;
        let content: String = (0..words)
            .map(|_| rng.pick(WORDS))
            .collect::<Vec<_>>()
            .join(" ");
        messages.push(serde_json::json!({"role": role, "content": content}));
    }
    map.insert("messages".to_owned(), serde_json::Value::Array(messages));
    serde_json::to_vec(&serde_json::Value::Object(map)).expect("serialise generated body")
}

const ANTHROPIC_MODELS: &[&str] = &["claude-opus-5", "claude-sonnet-4.6", "claude-haiku-4.5"];
const ANTHROPIC_SYSTEMS: &[&str] = &[
    "You are a careful assistant.",
    "Be terse and verify every claim.",
    "Prefer canonical JSON output.",
];

/// A canonical Anthropic Messages body with `messages` as the last key —
/// the shape the anthropic prefix-stability property splices at. Uses the
/// same seeded spread of surrounding keys as the OpenAI-chat generator,
/// with Anthropic's top-level `system` and `tools` fields.
pub fn anthropic_conversation_body(rng: &mut Rng, message_count: usize) -> Vec<u8> {
    let mut map = serde_json::Map::new();
    map.insert(
        "model".to_owned(),
        serde_json::json!(rng.pick(ANTHROPIC_MODELS)),
    );
    if rng.below(2) == 0 {
        map.insert(
            "system".to_owned(),
            serde_json::json!(rng.pick(ANTHROPIC_SYSTEMS)),
        );
    }
    if rng.below(3) == 0 {
        let tools: Vec<_> = (0..1 + rng.below(2))
            .map(|_| {
                serde_json::json!({
                    "name": rng.pick(TOOL_NAMES),
                    "input_schema": {"type": "object"},
                })
            })
            .collect();
        map.insert("tools".to_owned(), serde_json::Value::Array(tools));
    }
    if rng.below(2) == 0 {
        map.insert("stream".to_owned(), serde_json::json!(true));
    }
    if rng.below(3) == 0 {
        map.insert("max_tokens".to_owned(), serde_json::json!(1024));
    }
    let mut messages = Vec::with_capacity(message_count);
    for i in 0..message_count {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        let words = 1 + rng.below(5) as usize;
        let content: String = (0..words)
            .map(|_| rng.pick(WORDS))
            .collect::<Vec<_>>()
            .join(" ");
        messages.push(serde_json::json!({"role": role, "content": content}));
    }
    map.insert("messages".to_owned(), serde_json::Value::Array(messages));
    serde_json::to_vec(&serde_json::Value::Object(map)).expect("serialise generated body")
}
