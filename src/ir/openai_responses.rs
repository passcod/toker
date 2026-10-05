//! Typed, read-only views over OpenAI Responses request bodies.
//!
//! Native Codex requests stay byte-identical on their way to the codex
//! backend. This view exists only to extract content-free ledger shape and
//! attribution; it never renders or mutates the request.

use serde_json::Value;

use super::anthropic::{AnthropicShape, SystemBlockDigest};
use super::{Request, short_hash};

#[derive(Debug, Clone, Copy)]
pub struct ResponsesBody<'a> {
    request: &'a Request,
}

impl Request {
    pub fn openai_responses(&self) -> ResponsesBody<'_> {
        ResponsesBody { request: self }
    }
}

impl<'a> ResponsesBody<'a> {
    pub fn model(&self) -> Option<&'a str> {
        self.request.value().get("model").and_then(Value::as_str)
    }

    pub fn prompt_cache_key(&self) -> Option<&'a str> {
        self.request
            .value()
            .get("prompt_cache_key")
            .and_then(Value::as_str)
    }

    /// Content-free shape, expressed in the shared ledger shape type.
    pub fn shape(&self) -> AnthropicShape {
        let value = self.request.value();
        let instructions = value
            .get("instructions")
            .and_then(Value::as_str)
            .unwrap_or("");
        let tool_names: Vec<&str> = value
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .or_else(|| tool.pointer("/function/name").and_then(Value::as_str))
                    .unwrap_or("?")
            })
            .collect();
        let instructions_units = instructions.encode_utf16().count() as u64;
        let system_blocks = if instructions.is_empty() {
            Vec::new()
        } else {
            vec![SystemBlockDigest {
                chars: instructions_units,
                hash: short_hash(instructions.as_bytes()),
            }]
        };

        AnthropicShape {
            req_bytes: self.request.req_bytes(),
            req_messages: value
                .get("input")
                .and_then(Value::as_array)
                .map(|input| input.len() as u64),
            req_tools: tool_names.len() as u64,
            tools_hash: short_hash(tool_names.join("\0").as_bytes()),
            system_chars: instructions_units,
            system_hash: short_hash(instructions.as_bytes()),
            system_blocks,
            system_messages: None,
            compact_generations: None,
            summarising: false,
            compact_marker: None,
            recap: false,
            // Responses instructions are normally below one ladder rung.
            // Empty vectors say no rung was measured, never that it matched.
            system_ladder: Vec::new(),
            system_tail: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_content_free_shape_and_attribution() {
        let body = br#"{"model":"gpt-5","prompt_cache_key":"session-1","instructions":"secret","input":[{"role":"user","content":"never store me"}],"tools":[{"type":"function","name":"shell"}]}"#;
        let request = Request::parse(body).unwrap();
        let view = request.openai_responses();
        let shape = view.shape();
        assert_eq!(view.model(), Some("gpt-5"));
        assert_eq!(view.prompt_cache_key(), Some("session-1"));
        assert_eq!(shape.req_bytes, body.len() as u64);
        assert_eq!(shape.req_messages, Some(1));
        assert_eq!(shape.req_tools, 1);
        assert_eq!(shape.system_chars, 6);
    }
}
