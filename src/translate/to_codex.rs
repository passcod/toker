//! The composition: one Anthropic Messages body → one codex
//! [`ResponsesRequest`], in two layers (the request table lives in
//! the parent module's docs).
//!
//! - [`from_anthropic`](super::anthropic_frontend::from_anthropic) —
//!   the frontend adapter: the anthropic wire body → the canonical IR
//!   ([`CanonicalRequest`](crate::ir::canonical)). Wire-shape
//!   reporting is ITS domain.
//! - [`codex_from_canonical`](super::codex_backend) — the backend
//!   adapter: the canonical IR → the codex wire. What THIS backend
//!   refuses is its declared capability, enforced there — the
//!   pair-scoped cost table lives in ITS module docs.
//!
//! This module is the pair of the two, and adds nothing of its own.
//!
//! [`to_codex`] is pure: a function of (`body`, `model`,
//! `prompt_cache_key`) only. `model` and `prompt_cache_key` are
//! caller-derived facts, passed in as explicit inputs — the model
//! slug is unit C's anthropic→codex mapping and the cache key is the
//! session-id header value, and neither may be invented here. The
//! same body and the same params always produce the same bytes,
//! forever (invariant 4), which is what keeps an appended
//! conversation turn byte-identical over its earlier input items
//! (invariant 5 — the prefix-stability property test pins it).

use serde_json::Value;

use crate::providers::codex::ResponsesRequest;
use crate::translate::TranslateError;
use crate::translate::anthropic_frontend::from_anthropic;
use crate::translate::codex_backend::codex_from_canonical;

/// Translate one Anthropic Messages request body into a codex
/// [`ResponsesRequest`]: the frontend adapter
/// ([`from_anthropic`]) parses the body into the canonical IR, the
/// backend adapter ([`codex_from_canonical`]) renders it onto the
/// codex wire.
///
/// `body` is the parsed `/v1/messages` JSON (the IR's
/// [`Request::value`](crate::ir::Request) — any well-formed value;
/// shape violations are reported as [`TranslateError::Malformed`],
/// never guessed into shape). `model` is the codex model slug to
/// request and `prompt_cache_key` the cache/session identity — both
/// caller-derived, both explicit (purity).
pub fn to_codex(
    body: &Value,
    model: &str,
    prompt_cache_key: &str,
) -> Result<ResponsesRequest, TranslateError> {
    codex_from_canonical(&from_anthropic(body)?, model, prompt_cache_key)
}

#[cfg(test)]
mod tests {
    //! The composition's own tests: the end-to-end behaviour a pair
    //! has — the full request table over a body, the drops as seen
    //! from the wire, the caller params, and purity. The per-layer
    //! details live in the adapters' tests
    //! ([`from_anthropic`](super::from_anthropic),
    //! [`codex_from_canonical`](super::codex_backend)).

    use super::to_codex;
    use serde_json::{Value, json};

    const MODEL: &str = "gpt-5.2-codex";
    const KEY: &str = "unit-cache-key";

    fn item_of(request: &super::super::to_codex::ResponsesRequest, index: usize) -> Value {
        serde_json::to_value(&request.input[index]).expect("serialise item")
    }

    fn tool_of(request: &super::super::to_codex::ResponsesRequest, index: usize) -> Value {
        serde_json::to_value(&request.tools[index]).expect("serialise tool")
    }

    #[test]
    fn the_full_block_table_translates_in_order() {
        let body = json!({
            "model": "claude-sonnet-4.6",
            "system": [{"type": "text", "text": "One."}, {"type": "text", "text": "Two."}],
            "tools": [{
                "name": "get_weather",
                "description": "Weather",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}},
            }],
            "tool_choice": {"type": "any"},
            "max_tokens": 512,
            "temperature": 0.2,
            "top_p": 0.9,
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Hi"},
                    {"type": "tool_use", "id": "toolu_1", "name": "get_weather",
                     "input": {"city": "Wellington"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "18C"},
                ]},
                {"role": "assistant", "content": "Sunny in Wellington."},
            ],
        });
        let request = to_codex(&body, MODEL, KEY).expect("translates");

        assert_eq!(request.instructions, "One.\n\nTwo.");
        assert_eq!(request.tool_choice, "required");
        assert_eq!(request.input.len(), 5, "text+tool_use split into two items");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "Hello"}]})
        );
        assert_eq!(
            item_of(&request, 1),
            json!({"type": "message", "role": "assistant",
                   "content": [{"type": "output_text", "text": "Hi"}]})
        );
        assert_eq!(
            item_of(&request, 2),
            json!({"type": "function_call", "name": "get_weather",
                   "arguments": "{\"city\":\"Wellington\"}", "call_id": "toolu_1"})
        );
        assert_eq!(
            item_of(&request, 3),
            json!({"type": "function_call_output", "call_id": "toolu_1", "output": "18C"})
        );
        assert_eq!(
            item_of(&request, 4),
            json!({"type": "message", "role": "assistant",
                   "content": [{"type": "output_text", "text": "Sunny in Wellington."}]})
        );
        assert_eq!(
            tool_of(&request, 0),
            json!({"type": "function", "name": "get_weather", "description": "Weather",
                   "strict": false,
                   "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}})
        );
        // Sampling does not cross (the live-verified drop): the body
        // carried max_tokens/temperature/top_p, the request carries none.
        assert!(request.extra.is_empty());
    }

    #[test]
    fn sampling_parameters_do_not_cross_this_protocol() {
        // Verified live: "Unsupported parameter: temperature" — the
        // backend rejects them, its client sends none, and the values a
        // response echoes are the backend's own defaults. All sampling
        // knobs are a documented translation cost, dropped like
        // thinking blocks — loudly in the docs, absent from the wire.
        let body = json!({
            "model": "claude-opus-5",
            "max_tokens": 4096,
            "temperature": 0.3,
            "top_p": 0.95,
            "stop_sequences": ["\n\nHuman:"],
            "top_k": 40,
            "metadata": {"user_id": "user_1"},
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "messages": [{"role": "user", "content": "Hi"}],
        });
        let request = to_codex(&body, MODEL, KEY).expect("translates");
        assert!(request.extra.is_empty(), "no sampling fields cross");
        // None of the dropped fields ride along anywhere in the request.
        let bytes = serde_json::to_string(&request).expect("serialise");
        assert!(!bytes.contains("max_output_tokens"));
        assert!(!bytes.contains("temperature"));
        assert!(!bytes.contains("top_p"));
        assert!(!bytes.contains("stop_sequences"));
        assert!(!bytes.contains("top_k"));
        assert!(!bytes.contains("user_id"));
        assert!(!bytes.contains("budget_tokens"));
    }

    #[test]
    fn the_extra_fields_follow_the_pinned_fields_in_a_fixed_order() {
        let body = json!({
            "model": "claude-opus-5",
            "top_p": 0.95,
            "max_tokens": 1024,
            "temperature": 0.3,
            "messages": [{"role": "user", "content": "Hi"}],
        });
        // The source order is top_p, max_tokens, temperature — none of
        // them cross now (the sampling drop), so the request is exactly
        // the pinned fields, in their fixed order.
        let request = to_codex(&body, MODEL, KEY).expect("translates");
        assert!(request.extra.is_empty());
        let value = serde_json::to_value(&request).expect("serialise");
        let keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys.last(),
            Some(&"prompt_cache_key"),
            "the pinned fields end at the cache key; no sampling fields follow"
        );
    }

    #[test]
    fn the_model_and_cache_key_are_caller_params_not_body_facts() {
        let body = json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": "Hi"}],
        });
        let request = to_codex(&body, "gpt-5.6-sol", "key-2").expect("translates");
        assert_eq!(request.model, "gpt-5.6-sol");
        assert_eq!(request.prompt_cache_key.as_deref(), Some("key-2"));
        // The body's own model never rides along as anything else.
        let bytes = serde_json::to_string(&request).expect("serialise");
        assert_eq!(bytes.matches("claude-opus-5").count(), 0);
    }

    #[test]
    fn translation_is_pure() {
        let body = json!({
            "model": "claude-sonnet-4.6",
            "system": "Be terse.",
            "max_tokens": 2048,
            "temperature": 0.7,
            "messages": [
                {"role": "user", "content": "Read the config."},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Reading."},
                    {"type": "tool_use", "id": "t", "name": "read_file",
                     "input": {"path": "toker.toml"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t", "content": "the config"},
                ]},
            ],
        });
        let first = serde_json::to_string(&to_codex(&body, MODEL, KEY).expect("translates"))
            .expect("serialise");
        for _ in 0..3 {
            let again = serde_json::to_string(&to_codex(&body, MODEL, KEY).expect("translates"))
                .expect("serialise");
            assert_eq!(first, again, "same input, same bytes, every call");
        }
    }
}
