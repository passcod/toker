//! Typed, read-only views over OpenAI Responses request bodies.
//!
//! The legacy wire view remains for administrative/body metadata; inference
//! shapes are extracted from canonical IR before backend rendering.

use serde_json::Value;

use super::anthropic::{AnthropicShape, SystemBlockDigest};
use super::canonical::{CanonSystemPart, CanonicalRequest};
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

impl AnthropicShape {
    /// The Responses frontend's shape from its canonical request. `input`
    /// may be a string or an array; the wire array length is passed through
    /// so absence never becomes a fabricated one-message count.
    pub fn from_canonical_responses(
        request: &CanonicalRequest,
        req_bytes: u64,
        input_array_len: Option<u64>,
    ) -> AnthropicShape {
        let instructions = request
            .system
            .iter()
            .filter_map(CanonSystemPart::semantic_text)
            .collect::<Vec<_>>()
            .join("");
        let names: Vec<&str> = request
            .extensions
            .iter()
            .find(|extension| extension.wire_path() == "$.tools")
            .and_then(|extension| extension.value().as_array())
            .map(|tools| {
                tools
                    .iter()
                    .map(|tool| {
                        tool.get("name")
                            .and_then(Value::as_str)
                            .or_else(|| tool.pointer("/function/name").and_then(Value::as_str))
                            .unwrap_or("?")
                    })
                    .collect()
            })
            .unwrap_or_else(|| {
                request
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect()
            });
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
            req_bytes,
            req_messages: input_array_len,
            req_tools: names.len() as u64,
            tools_hash: short_hash(names.join("\0").as_bytes()),
            system_chars: instructions_units,
            system_hash: short_hash(instructions.as_bytes()),
            system_blocks,
            system_messages: None,
            compact_generations: None,
            summarising: false,
            compact_marker: None,
            recap: false,
            system_ladder: Vec::new(),
            system_tail: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translate::from_openai_responses;

    #[test]
    fn canonical_shape_matches_wire_shape_for_array_string_and_opaque_tools() {
        for body in [
            r#"{"model":"m","instructions":"Secret😊","input":[{"role":"user","content":"hi"}],"tools":[{"type":"function","name":"echo","parameters":{"type":"object"}}]}"#,
            r#"{"model":"m","input":"hello","tools":[]}"#,
            r#"{"model":"m","input":[],"tools":[{"type":"web_search_preview"},{"type":"function","name":"echo","parameters":{"type":"object"}}]}"#,
        ] {
            let ir = Request::parse(body.as_bytes()).unwrap();
            let canonical = from_openai_responses(ir.value()).unwrap();
            let input_array_len = ir.value()["input"]
                .as_array()
                .map(|input| input.len() as u64);
            assert_eq!(
                AnthropicShape::from_canonical_responses(
                    &canonical,
                    body.len() as u64,
                    input_array_len,
                ),
                ir.openai_responses().shape(),
                "shape drift for {body}"
            );
        }
    }

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
