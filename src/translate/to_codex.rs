//! The request direction: one Anthropic Messages body → one
//! [`ResponsesRequest`] (the request table lives in the parent
//! module's docs).
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
//!
//! Field placement: the routing fields, the wire constants, and the
//! field order are unit A's ([`ResponsesRequest::new`]); the sampling
//! translations (`max_output_tokens`, `temperature`, `top_p`, in that
//! order) ride in `extra`, emitted after the pinned fields.

use serde_json::{Map, Value, json};

use crate::providers::codex::{Item, ResponsesRequest, Tool};
use crate::translate::TranslateError;

/// Translate one Anthropic Messages request body into a codex
/// [`ResponsesRequest`].
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
    let mut request = ResponsesRequest::new(model, prompt_cache_key);
    let (input, leading_system) = input_of(body)?;
    let mut instructions = instructions_of(body)?;
    // Leading system-role messages (before any dialogue) belong with
    // the system prompt: the codex backend takes system content ONLY
    // via `instructions` (verified live: a system-role input item is
    // refused with "System messages are not allowed").
    for text in leading_system {
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(&text);
    }
    request.instructions = instructions;
    request.input = input;
    request.tools = tools_of(body)?;
    request.tool_choice = tool_choice_of(body)?.to_owned();
    sampling_of(body, &mut request.extra)?;
    Ok(request)
}

// ── system ──────────────────────────────────────────────────────────

/// `system` → `instructions`: a string is itself; a block array's
/// text pieces (each block's `text`, each bare string element, `""`
/// when a block carries none — the IR's `system_pieces` reading) join
/// on blank lines. Absent or `null` is the empty instruction.
fn instructions_of(body: &Value) -> Result<String, TranslateError> {
    match body.get("system") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(blocks)) => {
            let pieces: Vec<&str> = blocks
                .iter()
                .map(|block| match block {
                    Value::String(text) => text,
                    other => other.get("text").and_then(Value::as_str).unwrap_or(""),
                })
                .collect();
            Ok(pieces.join("\n\n"))
        }
        Some(other) => Err(TranslateError::Malformed {
            reason: format!(
                "system is neither a string nor a block array ({})",
                json_kind(other)
            ),
        }),
    }
}

// ── messages → input items ─────────────────────────────────────────

/// `messages` → the input item list, in order, plus any LEADING
/// system-role message texts (see [`to_codex`] — the codex backend
/// accepts system content only via `instructions`). Grouping rule:
/// consecutive text/image blocks of one message become ONE message
/// item; `tool_use`/`tool_result` become their own items, and text
/// after them opens a new message item (order preserved at item
/// granularity).
///
/// Mid-conversation system messages (claude Code's reminders) merge
/// into the PRECEDING user turn's item as `[PROMPT_INJECTION]`-prefixed
/// text parts — ctp's exact transform for the same problem (sonnet 5
/// refuses system entries in `messages[]`; the codex backend refuses
/// system-role input items), which keeps the role sequence the
/// conversation already had. With no preceding user item, the text
/// falls back to the leading set (instructions) — never dropped, never
/// a system item.
fn input_of(body: &Value) -> Result<(Vec<Item>, Vec<String>), TranslateError> {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return Err(TranslateError::Malformed {
            reason: "messages is missing or not an array (not a /v1/messages body)".to_owned(),
        });
    };
    let mut items: Vec<Item> = Vec::with_capacity(messages.len());
    let mut leading_system: Vec<String> = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        let role = message.get("role").and_then(Value::as_str).ok_or_else(|| {
            TranslateError::Malformed {
                reason: format!("messages[{message_index}]: role is missing or not a string"),
            }
        })?;
        if !matches!(role, "user" | "assistant" | "system") {
            return Err(TranslateError::Malformed {
                reason: format!(
                    "messages[{message_index}]: role {role:?} is not user, assistant, or system"
                ),
            });
        }
        if role == "system" {
            for text in system_texts_of(message, message_index)? {
                if let Some(user_item) = last_user_item_mut(&mut items) {
                    user_item
                        .0
                        .get_mut("content")
                        .and_then(Value::as_array_mut)
                        .expect("a message item's content is an array")
                        .push(text_part(&format!("[PROMPT_INJECTION] {text}"), false));
                } else {
                    leading_system.push(text);
                }
            }
            continue;
        }
        let content = message
            .get("content")
            .ok_or_else(|| TranslateError::Malformed {
                reason: format!("messages[{message_index}]: content is missing"),
            })?;
        match content {
            // String content is one text part, even when empty — a
            // string message always yields its message item.
            Value::String(text) => {
                items.push(message_item(
                    role,
                    vec![text_part(text, role == "assistant")],
                ));
            }
            Value::Array(blocks) => {
                let mut parts: Vec<Value> = Vec::new();
                for (block_index, block) in blocks.iter().enumerate() {
                    let at = |what: &str| {
                        format!("messages[{message_index}] block {block_index}: {what}")
                    };
                    let kind = block.get("type").and_then(Value::as_str).ok_or_else(|| {
                        TranslateError::Malformed {
                            reason: at("is not a block object with a type"),
                        }
                    })?;
                    match (role, kind) {
                        (_, "text") => {
                            let text =
                                block.get("text").and_then(Value::as_str).ok_or_else(|| {
                                    TranslateError::Malformed {
                                        reason: at("text is missing or not a string"),
                                    }
                                })?;
                            parts.push(text_part(text, role == "assistant"));
                        }
                        ("user", "image") => {
                            parts.push(image_part(block, &at)?);
                        }
                        ("user", "tool_result") => {
                            let tool_use_id = block
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .ok_or_else(|| TranslateError::Malformed {
                                    reason: at("tool_result has no tool_use_id"),
                                })?;
                            let output = match block.get("content") {
                                None | Some(Value::Null) => String::new(),
                                Some(Value::String(text)) => text.clone(),
                                Some(Value::Array(blocks)) => serde_json::to_string(blocks)
                                    .expect("a parsed Value always serialises"),
                                Some(other) => {
                                    return Err(TranslateError::Malformed {
                                        reason: at(&format!(
                                            "tool_result content is neither a string nor a \
                                             block array ({})",
                                            json_kind(other)
                                        )),
                                    });
                                }
                            };
                            items.extend(flush(&mut parts, role));
                            items.push(Item::function_call_output(tool_use_id, &output));
                        }
                        ("assistant", "tool_use") => {
                            let id = block.get("id").and_then(Value::as_str).ok_or_else(|| {
                                TranslateError::Malformed {
                                    reason: at("tool_use has no id"),
                                }
                            })?;
                            let name =
                                block.get("name").and_then(Value::as_str).ok_or_else(|| {
                                    TranslateError::Malformed {
                                        reason: at("tool_use has no name"),
                                    }
                                })?;
                            let input = block
                                .get("input")
                                .filter(|input| input.is_object())
                                .ok_or_else(|| TranslateError::Malformed {
                                    reason: at("tool_use input is missing or not an object"),
                                })?;
                            let arguments = serde_json::to_string(input)
                                .expect("a parsed Value always serialises");
                            items.extend(flush(&mut parts, role));
                            items.push(Item::function_call(name, &arguments, id));
                        }
                        // Dropped, loudly — see the parent module's
                        // docs: claude's reasoning cannot be replayed
                        // cross-provider. The drop does not split the
                        // message's parts.
                        ("assistant", "thinking") | ("assistant", "redacted_thinking") => {}
                        (_, other) => {
                            return Err(TranslateError::UnsupportedBlock {
                                kind: other.to_owned(),
                            });
                        }
                    }
                }
                items.extend(flush(&mut parts, role));
            }
            other => {
                return Err(TranslateError::Malformed {
                    reason: format!(
                        "messages[{message_index}]: content is neither a string nor a block \
                         array ({})",
                        json_kind(other)
                    ),
                });
            }
        }
    }
    Ok((items, leading_system))
}

/// A system-role message's text pieces, in order. String content is
/// the one piece; a block array contributes each text block's `text`.
/// Non-text blocks are unsupported on this route — a system message
/// carrying one is a body toker refuses to translate rather than
/// silently truncating.
fn system_texts_of(message: &Value, index: usize) -> Result<Vec<String>, TranslateError> {
    match message.get("content") {
        Some(Value::String(text)) => Ok(vec![text.clone()]),
        Some(Value::Array(blocks)) => {
            let mut texts = Vec::new();
            for (block_index, block) in blocks.iter().enumerate() {
                let kind = block.get("type").and_then(Value::as_str).ok_or_else(|| {
                    TranslateError::Malformed {
                        reason: format!(
                            "messages[{index}] block {block_index}: \
                             a system block has no type"
                        ),
                    }
                })?;
                if kind != "text" {
                    return Err(TranslateError::UnsupportedBlock {
                        kind: kind.to_owned(),
                    });
                }
                texts.push(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| TranslateError::Malformed {
                            reason: format!(
                                "messages[{index}] block {block_index}: \
                                     text is missing or not a string"
                            ),
                        })?
                        .to_owned(),
                );
            }
            Ok(texts)
        }
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(other) => Err(TranslateError::Malformed {
            reason: format!(
                "messages[{index}]: system content is neither a string nor a \
                 text-block array ({})",
                json_kind(other)
            ),
        }),
    }
}

/// The most recent user message item, mutably — the merge target for a
/// mid-conversation system message (ctp's preceding-user rule).
fn last_user_item_mut(items: &mut [Item]) -> Option<&mut Item> {
    items.iter_mut().rev().find(|item| {
        item.0.get("type").and_then(Value::as_str) == Some("message")
            && item.0.get("role").and_then(Value::as_str) == Some("user")
    })
}

/// Flush the pending text/image parts of one message into its message
/// item — nothing when no parts accumulated (an all-thinking assistant
/// message yields no item; a `tool_result` after a flush yields only
/// its own item).
fn flush(parts: &mut Vec<Value>, role: &str) -> Option<Item> {
    if parts.is_empty() {
        return None;
    }
    let parts = std::mem::take(parts);
    Some(message_item(role, parts))
}

/// One message input item with the given content parts.
fn message_item(role: &str, parts: Vec<Value>) -> Item {
    Item(json!({"type": "message", "role": role, "content": parts}))
}

/// One text content part: `input_text` for user/system messages,
/// `output_text` for assistant messages.
fn text_part(text: &str, output: bool) -> Value {
    json!({"type": if output { "output_text" } else { "input_text" }, "text": text})
}

/// One `input_image` content part from an anthropic `image` block:
/// a base64 source becomes the `data:<media_type>;base64,<data>` URL,
/// a url source passes through verbatim.
fn image_part(block: &Value, at: &dyn Fn(&str) -> String) -> Result<Value, TranslateError> {
    let source = block
        .get("source")
        .filter(|source| source.is_object())
        .ok_or_else(|| TranslateError::Malformed {
            reason: at("image has no source object"),
        })?;
    let image_url = match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .ok_or_else(|| TranslateError::Malformed {
                    reason: at("base64 image source has no media_type"),
                })?;
            let data = source.get("data").and_then(Value::as_str).ok_or_else(|| {
                TranslateError::Malformed {
                    reason: at("base64 image source has no data"),
                }
            })?;
            format!("data:{media_type};base64,{data}")
        }
        Some("url") => {
            let url = source.get("url").and_then(Value::as_str).ok_or_else(|| {
                TranslateError::Malformed {
                    reason: at("url image source has no url"),
                }
            })?;
            url.to_owned()
        }
        Some(other) => {
            return Err(TranslateError::Malformed {
                reason: at(&format!(
                    "image source type {other:?} has no responses equivalent"
                )),
            });
        }
        None => {
            return Err(TranslateError::Malformed {
                reason: at("image source has no type"),
            });
        }
    };
    Ok(json!({"type": "input_image", "image_url": image_url}))
}

// ── tools ───────────────────────────────────────────────────────────

/// `tools` → the flat function-tool list. Every entry must be a named
/// function tool with an object `input_schema` — a shape the Responses
/// function tool cannot express faithfully (an unnamed entry, a
/// missing schema) is [`TranslateError::Malformed`], never a
/// fabricated schema.
fn tools_of(body: &Value) -> Result<Vec<Tool>, TranslateError> {
    let Some(tools) = body.get("tools").filter(|tools| !tools.is_null()) else {
        return Ok(Vec::new());
    };
    let Value::Array(tools) = tools else {
        return Err(TranslateError::Malformed {
            reason: format!("tools is not an array ({})", json_kind(tools)),
        });
    };
    let mut out = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let at = |what: &str| format!("tools[{index}]: {what}");
        let name =
            tool.get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| TranslateError::Malformed {
                    reason: at("no name — not a callable function tool"),
                })?;
        let description = match tool.get("description") {
            None | Some(Value::Null) => "",
            Some(Value::String(text)) => text,
            Some(other) => {
                return Err(TranslateError::Malformed {
                    reason: at(&format!(
                        "description is not a string ({})",
                        json_kind(other)
                    )),
                });
            }
        };
        let parameters = tool
            .get("input_schema")
            .filter(|schema| schema.is_object())
            .ok_or_else(|| TranslateError::Malformed {
                reason: at("input_schema is missing or not an object"),
            })?;
        out.push(Tool::function(name, description, false, parameters.clone()));
    }
    Ok(out)
}

// ── tool choice and sampling ────────────────────────────────────────

/// `tool_choice` → the wire's string form: absent/`auto` is the wire
/// constant, `any` is the Responses `required`. Anything else (a
/// forced tool, an unknown type) has no faithful string form and is
/// reported, not guessed.
fn tool_choice_of(body: &Value) -> Result<&'static str, TranslateError> {
    match body.get("tool_choice") {
        None | Some(Value::Null) => Ok("auto"),
        Some(choice) => match choice.get("type").and_then(Value::as_str) {
            Some("auto") => Ok("auto"),
            Some("any") => Ok("required"),
            Some(other) => Err(TranslateError::Malformed {
                reason: format!("tool_choice type {other:?} has no responses equivalent"),
            }),
            None => Err(TranslateError::Malformed {
                reason: "tool_choice has no type".to_owned(),
            }),
        },
    }
}

/// The sampling translations, in a fixed order (purity): `max_tokens`
/// → `max_output_tokens` (omitted when the request omits it — the
/// codex client itself sends none), `temperature` and `top_p`
/// verbatim. All three ride in `extra`, after the pinned fields.
/// `stop_sequences` and `top_k` are dropped (no Responses equivalent —
/// the parent module's docs).
/// Sampling parameters (`max_tokens`, `temperature`, `top_p`) are a
/// **translation cost, dropped loudly**: the codex backend rejects them
/// outright (verified live: `"Unsupported parameter: temperature"`),
/// and its client never sends any — the `temperature`/`top_p` a codex
/// response echoes are the backend's own defaults, not accepted knobs.
/// The drop is documented beside the thinking-block drop in the module
/// docs; nothing is silently mangled, the parameters just do not cross
/// this protocol. They still land in the request's SHAPE record via
/// the row's `req_bytes`, and claude's own retry budget is unchanged.
fn sampling_of(_body: &Value, _extra: &mut Map<String, Value>) -> Result<(), TranslateError> {
    Ok(())
}

// ── shared ──────────────────────────────────────────────────────────

/// A JSON value's kind, for error messages.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::super::TranslateError;
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
    fn consecutive_blocks_group_and_text_after_a_tool_opens_a_new_item() {
        let body = json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "first"},
                    {"type": "text", "text": "second"},
                    {"type": "tool_result", "tool_use_id": "t1", "content": "done"},
                    {"type": "text", "text": "after"},
                ]},
            ],
        });
        let request = to_codex(&body, MODEL, KEY).expect("translates");
        assert_eq!(request.input.len(), 3);
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "first"},
                {"type": "input_text", "text": "second"},
            ]})
        );
        assert_eq!(
            item_of(&request, 1),
            json!({"type": "function_call_output", "call_id": "t1", "output": "done"})
        );
        assert_eq!(
            item_of(&request, 2),
            json!({"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "after"}]})
        );
    }

    #[test]
    fn system_pieces_join_on_blank_lines_and_absent_reads_empty() {
        let join = json!({
            "model": "m",
            "system": [{"type": "text", "text": "One.", "cache_control": {"type": "ephemeral"}},
                       "bare string element",
                       {"no_text": true}],
            "messages": [{"role": "user", "content": "Hi"}],
        });
        assert_eq!(
            to_codex(&join, MODEL, KEY)
                .expect("translates")
                .instructions,
            "One.\n\nbare string element\n\n"
        );

        let absent = json!({"model": "m", "messages": [{"role": "user", "content": "Hi"}]});
        assert_eq!(
            to_codex(&absent, MODEL, KEY)
                .expect("translates")
                .instructions,
            ""
        );

        let null = json!({"model": "m", "system": null,
                          "messages": [{"role": "user", "content": "Hi"}]});
        assert_eq!(
            to_codex(&null, MODEL, KEY)
                .expect("translates")
                .instructions,
            ""
        );

        let malformed = json!({"model": "m", "system": 5,
                               "messages": [{"role": "user", "content": "Hi"}]});
        assert!(matches!(
            to_codex(&malformed, MODEL, KEY),
            Err(TranslateError::Malformed { .. })
        ));
    }

    #[test]
    fn assistant_thinking_blocks_are_dropped_not_translated() {
        // The LOUD drop (the parent module's docs): claude's reasoning
        // cannot be replayed cross-provider. Neither the thinking text,
        // the signature, nor the redacted blob may leak into the
        // translated request.
        let body = json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "secret reasoning",
                     "signature": "sig-1"},
                    {"type": "redacted_thinking", "data": "opaque-blob"},
                    {"type": "text", "text": "Answer."},
                ]},
            ],
        });
        let request = to_codex(&body, MODEL, KEY).expect("translates");
        assert_eq!(request.input.len(), 1, "only the text item survives");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "message", "role": "assistant",
                   "content": [{"type": "output_text", "text": "Answer."}]})
        );
        let bytes = serde_json::to_string(&request).expect("serialise");
        assert!(!bytes.contains("secret reasoning"));
        assert!(!bytes.contains("opaque-blob"));
        assert!(!bytes.contains("sig-1"));

        // Thinking in a USER message is not the drop case — it is a
        // block with no mapping there, reported.
        let misplaced = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "thinking", "thinking": "user-side"},
            ]}],
        });
        assert_eq!(
            to_codex(&misplaced, MODEL, KEY),
            Err(TranslateError::UnsupportedBlock {
                kind: "thinking".to_owned()
            })
        );
    }

    #[test]
    fn tool_result_content_shapes_map_to_the_output_string() {
        let body = |content: Value| {
            json!({
                "model": "m",
                "messages": [
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "t1", "content": content},
                    ]},
                ],
            })
        };
        // A string is the output text, verbatim.
        let request = to_codex(&body(json!("plain text")), MODEL, KEY).expect("translates");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "function_call_output", "call_id": "t1", "output": "plain text"})
        );
        // A block array is the JSON of the array — the lossless string
        // form (images inside ride within it).
        let request = to_codex(
            &body(json!([{"type": "text", "text": "src holds"},
                         {"type": "text", "text": "two modules."}])),
            MODEL,
            KEY,
        )
        .expect("translates");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "function_call_output", "call_id": "t1",
                   "output": "[{\"type\":\"text\",\"text\":\"src holds\"},{\"type\":\"text\",\"text\":\"two modules.\"}]"})
        );
        // Absent content is the empty output; is_error is metadata,
        // dropped (documented).
        let request = to_codex(&body(Value::Null), MODEL, KEY).expect("translates");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "function_call_output", "call_id": "t1", "output": ""})
        );
        // Anything else is a shape violation, reported.
        assert!(matches!(
            to_codex(&body(json!(5)), MODEL, KEY),
            Err(TranslateError::Malformed { .. })
        ));
    }

    #[test]
    fn image_blocks_translate_to_input_image_parts() {
        let body = |source: Value| {
            json!({
                "model": "m",
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "Look:"},
                    {"type": "image", "source": source},
                ]}],
            })
        };
        let request = to_codex(
            &body(json!({"type": "base64", "media_type": "image/png", "data": "aVBONyU="})),
            MODEL,
            KEY,
        )
        .expect("translates");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "Look:"},
                {"type": "input_image", "image_url": "data:image/png;base64,aVBONyU="},
            ]})
        );

        let request = to_codex(
            &body(json!({"type": "url", "url": "https://example.test/chart.png"})),
            MODEL,
            KEY,
        )
        .expect("translates");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "Look:"},
                {"type": "input_image", "image_url": "https://example.test/chart.png"},
            ]})
        );

        // A file source and a shapeless source are reported.
        assert!(matches!(
            to_codex(
                &body(json!({"type": "file", "file_id": "file_1"})),
                MODEL,
                KEY
            ),
            Err(TranslateError::Malformed { .. })
        ));
        assert!(matches!(
            to_codex(&body(json!("not an object")), MODEL, KEY),
            Err(TranslateError::Malformed { .. })
        ));

        // An image in an ASSISTANT message has no wire form (assistant
        // parts are output_text) — reported, never an invalid item.
        let assistant = json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": [
                {"type": "image",
                 "source": {"type": "base64", "media_type": "image/png", "data": "aVBONyU="}},
            ]}],
        });
        assert_eq!(
            to_codex(&assistant, MODEL, KEY),
            Err(TranslateError::UnsupportedBlock {
                kind: "image".to_owned()
            })
        );
    }

    #[test]
    fn unknown_and_misplaced_blocks_fail_with_the_typed_error() {
        // Unknown kind, in either role.
        let unknown = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "document", "source": {"type": "url"}},
            ]}],
        });
        assert_eq!(
            to_codex(&unknown, MODEL, KEY),
            Err(TranslateError::UnsupportedBlock {
                kind: "document".to_owned()
            })
        );
        // Known kinds, wrong role.
        let tool_use_in_user = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "tool_use", "id": "t", "name": "n", "input": {}},
            ]}],
        });
        assert_eq!(
            to_codex(&tool_use_in_user, MODEL, KEY),
            Err(TranslateError::UnsupportedBlock {
                kind: "tool_use".to_owned()
            })
        );
        let tool_result_in_assistant = json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "x"},
            ]}],
        });
        assert_eq!(
            to_codex(&tool_result_in_assistant, MODEL, KEY),
            Err(TranslateError::UnsupportedBlock {
                kind: "tool_result".to_owned()
            })
        );
    }

    #[test]
    fn shape_violations_are_reported_never_guessed() {
        let body = |messages: Value| json!({"model": "m", "messages": messages});
        let cases = [
            // messages missing / not an array — the batches body shape.
            json!({"model": "m", "requests": []}),
            body(json!(5)),
            // role / content missing or of the wrong kind.
            body(json!([{"content": "Hi"}])),
            body(json!([{"role": 5, "content": "Hi"}])),
            body(json!([{"role": "persona", "content": "Hi"}])),
            body(json!([{"role": "user"}])),
            body(json!([{"role": "user", "content": 5}])),
            // block-level shape violations.
            body(json!([{"role": "user", "content": ["bare string"]}])),
            body(json!([{"role": "user", "content": [{"text": "no type"}]}])),
            body(json!([{"role": "user", "content": [{"type": "text", "text": 5}]}])),
            body(json!([{"role": "assistant", "content": [
                {"type": "tool_use", "name": "n", "input": {}},
            ]}])),
            body(json!([{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t", "name": "n", "input": "not an object"},
            ]}])),
            body(json!([{"role": "user", "content": [
                {"type": "tool_result", "content": "no tool_use_id"},
            ]}])),
        ];
        for case in cases {
            assert!(
                matches!(
                    to_codex(&case, MODEL, KEY),
                    Err(TranslateError::Malformed { .. })
                ),
                "case must be reported as malformed: {case}"
            );
        }
    }

    #[test]
    fn mid_conversation_system_messages_merge_into_the_preceding_user_turn() {
        // claude Code's mid-conversation system reminders: the codex
        // backend takes system content ONLY via `instructions` (a
        // system-role input item is refused with "System messages are
        // not allowed" — verified live). ctp's transform for the same
        // problem: merge into the preceding user turn, role sequence
        // unchanged.
        let body = json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "user", "content": "Earlier work."},
                {"role": "system", "content": [{"type": "text", "text": "reminder"}]},
            ],
        });
        let request = to_codex(&body, MODEL, KEY).expect("translates");
        assert_eq!(request.input.len(), 1, "no separate system item");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "message", "role": "user",
            "content": [
                {"type": "input_text", "text": "Earlier work."},
                {"type": "input_text", "text": "[PROMPT_INJECTION] reminder"},
            ]})
        );
    }

    #[test]
    fn leading_system_messages_ride_the_instructions() {
        let body = json!({
            "model": "claude-opus-5",
            "system": "Base prompt.",
            "messages": [
                {"role": "system", "content": "Preamble."},
                {"role": "user", "content": "Hi"},
            ],
        });
        let request = to_codex(&body, MODEL, KEY).expect("translates");
        assert_eq!(request.instructions, "Base prompt.\n\nPreamble.");
        assert_eq!(
            request.input.len(),
            1,
            "the leading system item is not input"
        );
        assert_eq!(item_of(&request, 0)["role"], "user");
    }

    #[test]
    fn a_system_message_after_only_assistant_turns_falls_back_to_instructions() {
        let body = json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "assistant", "content": "Hello."},
                {"role": "system", "content": "reminder"},
            ],
        });
        let request = to_codex(&body, MODEL, KEY).expect("translates");
        assert_eq!(
            request.instructions, "reminder",
            "no user item to merge into"
        );
        assert_eq!(request.input.len(), 1);
    }

    #[test]
    fn tools_translate_to_the_flat_function_shape() {
        let body = json!({
            "model": "m",
            "tools": [
                {"name": "read_file", "input_schema": {"type": "object"}},
                {"name": "list_dir", "description": "List a directory",
                 "input_schema": {"type": "object", "properties": {}}},
            ],
            "messages": [{"role": "user", "content": "Hi"}],
        });
        let request = to_codex(&body, MODEL, KEY).expect("translates");
        assert_eq!(request.tools.len(), 2);
        assert_eq!(
            tool_of(&request, 0),
            json!({"type": "function", "name": "read_file", "description": "",
                   "strict": false, "parameters": {"type": "object"}})
        );
        assert_eq!(
            tool_of(&request, 1),
            json!({"type": "function", "name": "list_dir",
                   "description": "List a directory", "strict": false,
                   "parameters": {"type": "object", "properties": {}}})
        );

        // Absent/null tools read as none.
        let none = json!({"model": "m", "messages": [{"role": "user", "content": "Hi"}]});
        assert!(
            to_codex(&none, MODEL, KEY)
                .expect("translates")
                .tools
                .is_empty()
        );
        let null = json!({"model": "m", "tools": null,
                          "messages": [{"role": "user", "content": "Hi"}]});
        assert!(
            to_codex(&null, MODEL, KEY)
                .expect("translates")
                .tools
                .is_empty()
        );

        // A tool this wire cannot express faithfully — an unnamed entry
        // (anthropic's API-invalid custom shapes), a missing or
        // non-object schema — is reported, never fabricated.
        let unnamed = json!({"model": "m", "tools": [{"type": "custom_tool"}],
                             "messages": [{"role": "user", "content": "Hi"}]});
        let error = to_codex(&unnamed, MODEL, KEY).unwrap_err();
        assert!(
            matches!(&error, TranslateError::Malformed { reason } if reason.contains("tools[0]")),
            "the reason names the entry: {error:?}"
        );
        let no_schema = json!({"model": "m", "tools": [{"name": "n"}],
                               "messages": [{"role": "user", "content": "Hi"}]});
        assert!(matches!(
            to_codex(&no_schema, MODEL, KEY),
            Err(TranslateError::Malformed { .. })
        ));
        let not_array = json!({"model": "m", "tools": {"name": "n"},
                               "messages": [{"role": "user", "content": "Hi"}]});
        assert!(matches!(
            to_codex(&not_array, MODEL, KEY),
            Err(TranslateError::Malformed { .. })
        ));
    }

    #[test]
    fn tool_choice_maps_auto_and_any_and_reports_the_rest() {
        let body = |tool_choice: Value| {
            json!({"model": "m", "tool_choice": tool_choice,
                   "messages": [{"role": "user", "content": "Hi"}]})
        };
        for absent in [json!({}), body(Value::Null)] {
            let mut case = absent;
            if case.get("tool_choice").is_none() {
                case = json!({"model": "m",
                              "messages": [{"role": "user", "content": "Hi"}]});
            }
            assert_eq!(
                to_codex(&case, MODEL, KEY).expect("translates").tool_choice,
                "auto"
            );
        }
        assert_eq!(
            to_codex(&body(json!({"type": "auto"})), MODEL, KEY)
                .expect("translates")
                .tool_choice,
            "auto"
        );
        assert_eq!(
            to_codex(&body(json!({"type": "any"})), MODEL, KEY)
                .expect("translates")
                .tool_choice,
            "required"
        );
        // A forced tool has no faithful string form — reported.
        assert!(matches!(
            to_codex(
                &body(json!({"type": "tool", "name": "read_file"})),
                MODEL,
                KEY
            ),
            Err(TranslateError::Malformed { .. })
        ));
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
