//! The frontend adapter: one Anthropic Messages body → one
//! [`CanonicalRequest`] (the canonical IR —
//! [`crate::ir::canonical`]).
//!
//! This file's domain is the FRONTEND WIRE and nothing else: the
//! role table (which role may carry which block kind), the block
//! parses, the string-or-blocks readings, and the shape reporting —
//! [`TranslateError::Malformed`] for shape violations,
//! [`TranslateError::UnsupportedBlock`] for a block with no faithful
//! parse. NO backend knowledge lives here: nothing is merged,
//! dropped, joined, or decided — system-role messages stay
//! messages, thinking blocks stay blocks, sampling rides as specs,
//! and what to DO with any of them is backend policy.
//!
//! The parse normalises SHAPE, never MEANING: a base64 image source
//! is already its `data:` URL, an absent tool description is `""` —
//! those are URL/description forms, not policy. Everything else
//! crosses exactly as the wire said it.
//!
//! [`from_anthropic`] is pure: a function of `body` only. The same
//! body always produces the same canonical, forever (invariant 4) —
//! and the error-selection order is fixed: messages, then system,
//! then tools, then tool choice, then thinking, the order the pair
//! module read them in (see [`tool_choice_of`] for the one
//! doubly-malformed corner that order cannot keep).

use serde_json::Value;

use crate::ir::canonical::{
    CanonBlock, CanonMessage, CanonRole, CanonTool, CanonToolChoice, CanonicalRequest,
    SamplingSpec, ThinkingSpec, ToolResultContent,
};
use crate::translate::TranslateError;

/// Parse one Anthropic Messages request body into the canonical IR.
///
/// `body` is the parsed `/v1/messages` JSON (the IR's
/// [`Request::value`](crate::ir::Request) — any well-formed value;
/// shape violations are reported as [`TranslateError::Malformed`],
/// never guessed into shape, and a block with no faithful parse as
/// [`TranslateError::UnsupportedBlock`], never silently dropped).
/// The CALLER decides policy on a typed failure — reject, or route
/// to a backend that speaks the body natively.
pub fn from_anthropic(body: &Value) -> Result<CanonicalRequest, TranslateError> {
    // The read order is the pair module's, so a body with several
    // problems reports the same first problem it always did:
    // messages, then system, then tools, then tool choice, then
    // thinking. Sampling and stream never error (lenient reads).
    let messages = messages_of(body)?;
    let system = system_of(body)?;
    let tools = tools_of(body)?;
    let tool_choice = tool_choice_of(body)?;
    let sampling = sampling_of(body);
    let thinking = thinking_of(body)?;
    let stream = body.get("stream").and_then(Value::as_bool);
    Ok(CanonicalRequest {
        system,
        messages,
        tools,
        sampling,
        thinking,
        stream,
        tool_choice,
    })
}

// ── messages ────────────────────────────────────────────────────────

/// `messages` → the canonical conversation, in order. A message
/// without a role, or with a role the wire does not define, is a
/// shape violation; system-role messages STAY messages — what to do
/// with them is backend policy, never a parse decision.
fn messages_of(body: &Value) -> Result<Vec<CanonMessage>, TranslateError> {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return Err(TranslateError::Malformed {
            reason: "messages is missing or not an array (not a /v1/messages body)".to_owned(),
        });
    };
    let mut out = Vec::with_capacity(messages.len());
    for (message_index, message) in messages.iter().enumerate() {
        let role = message.get("role").and_then(Value::as_str).ok_or_else(|| {
            TranslateError::Malformed {
                reason: format!("messages[{message_index}]: role is missing or not a string"),
            }
        })?;
        let role = match role {
            "user" => CanonRole::User,
            "assistant" => CanonRole::Assistant,
            "system" => CanonRole::System,
            other => {
                return Err(TranslateError::Malformed {
                    reason: format!(
                        "messages[{message_index}]: \
                         role {other:?} is not user, assistant, or system"
                    ),
                });
            }
        };
        if role == CanonRole::System {
            out.push(CanonMessage {
                role,
                blocks: system_blocks_of(message, message_index)?,
            });
            continue;
        }
        let content = message
            .get("content")
            .ok_or_else(|| TranslateError::Malformed {
                reason: format!("messages[{message_index}]: content is missing"),
            })?;
        let blocks = match content {
            // String content is one text block, even when empty — a
            // string message always yields its message item.
            Value::String(text) => vec![CanonBlock::Text(text.clone())],
            Value::Array(blocks) => content_blocks_of(role, blocks, message_index)?,
            other => {
                return Err(TranslateError::Malformed {
                    reason: format!(
                        "messages[{message_index}]: content is neither a string nor a \
                         block array ({})",
                        json_kind(other)
                    ),
                });
            }
        };
        out.push(CanonMessage { role, blocks });
    }
    Ok(out)
}

/// One message's block array → the canonical blocks, in order. The
/// position table: `text` in either dialogue role; `image` and
/// `tool_result` in user messages; `tool_use`, `thinking`, and
/// `redacted_thinking` in assistant messages. Anything else — a kind
/// this table has never heard of, or a known kind in the wrong
/// position — is [`TranslateError::UnsupportedBlock`]: content is
/// never silently dropped, and the CALLER decides policy.
fn content_blocks_of(
    role: CanonRole,
    blocks: &[Value],
    message_index: usize,
) -> Result<Vec<CanonBlock>, TranslateError> {
    let mut out = Vec::with_capacity(blocks.len());
    for (block_index, block) in blocks.iter().enumerate() {
        let at = |what: &str| format!("messages[{message_index}] block {block_index}: {what}");
        let kind =
            block
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| TranslateError::Malformed {
                    reason: at("is not a block object with a type"),
                })?;
        match (role, kind) {
            (_, "text") => {
                let text = block.get("text").and_then(Value::as_str).ok_or_else(|| {
                    TranslateError::Malformed {
                        reason: at("text is missing or not a string"),
                    }
                })?;
                out.push(CanonBlock::Text(text.to_owned()));
            }
            (CanonRole::User, "image") => {
                out.push(CanonBlock::Image {
                    url: image_url_of(block, &at)?,
                });
            }
            (CanonRole::User, "tool_result") => {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| TranslateError::Malformed {
                        reason: at("tool_result has no tool_use_id"),
                    })?;
                let content = tool_result_content_of(block.get("content"), &at)?;
                out.push(CanonBlock::ToolResult {
                    tool_use_id: tool_use_id.to_owned(),
                    content,
                });
            }
            (CanonRole::Assistant, "tool_use") => {
                let id = block.get("id").and_then(Value::as_str).ok_or_else(|| {
                    TranslateError::Malformed {
                        reason: at("tool_use has no id"),
                    }
                })?;
                let name = block.get("name").and_then(Value::as_str).ok_or_else(|| {
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
                out.push(CanonBlock::ToolUse {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    input: input.clone(),
                });
            }
            // THINKING STAYS IN THE CANONICAL — whether reasoning
            // replays is backend policy (the capability a backend
            // declares), never a parse decision. The text is read
            // leniently (a block the pair module accepted and
            // dropped still parses; a redacted block's `data` is the
            // only content it has); the signature metadata is
            // frontend-wire replay machinery this IR does not carry.
            (CanonRole::Assistant, "thinking") => out.push(CanonBlock::Thinking {
                text: block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            }),
            (CanonRole::Assistant, "redacted_thinking") => out.push(CanonBlock::Thinking {
                text: block
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            }),
            (_, other) => {
                return Err(TranslateError::UnsupportedBlock {
                    kind: other.to_owned(),
                });
            }
        }
    }
    Ok(out)
}

/// A system-role message's blocks: string content is one text block;
/// a block array contributes each text block's `text`. Non-text
/// blocks are unsupported on this route — a system message carrying
/// one is a body toker refuses to parse rather than silently
/// truncating. Absent or `null` content reads as NO blocks.
fn system_blocks_of(message: &Value, index: usize) -> Result<Vec<CanonBlock>, TranslateError> {
    match message.get("content") {
        Some(Value::String(text)) => Ok(vec![CanonBlock::Text(text.clone())]),
        Some(Value::Array(blocks)) => {
            let mut out = Vec::with_capacity(blocks.len());
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
                out.push(CanonBlock::Text(
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
                ));
            }
            Ok(out)
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

// ── system, tools, tool choice, sampling, thinking ─────────────────

/// `system` → the prompt pieces, in order (the IR's `system_pieces`
/// reading): a string is itself; a block array contributes each
/// block's `text`, each bare string element verbatim, `""` for a
/// textless block. Absent or `null` reads as no pieces. How the
/// pieces JOIN is backend policy — the codex backend joins them on
/// blank lines into its `instructions`.
fn system_of(body: &Value) -> Result<Vec<String>, TranslateError> {
    match body.get("system") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![text.clone()]),
        Some(Value::Array(blocks)) => Ok(blocks
            .iter()
            .map(|block| match block {
                Value::String(text) => text.clone(),
                other => other
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            })
            .collect()),
        Some(other) => Err(TranslateError::Malformed {
            reason: format!(
                "system is neither a string nor a block array ({})",
                json_kind(other)
            ),
        }),
    }
}

/// `tools` → the canonical tools. Every entry must be a named
/// function tool with an object `input_schema` — a shape the
/// canonical cannot express faithfully (an unnamed entry, a missing
/// schema) is [`TranslateError::Malformed`], never a fabricated
/// schema. An absent description reads `""` (parse-time
/// normalisation: absence and emptiness read the same).
fn tools_of(body: &Value) -> Result<Vec<CanonTool>, TranslateError> {
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
        out.push(CanonTool {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters: parameters.clone(),
        });
    }
    Ok(out)
}

/// `tool_choice` → the canonical intent: absent or `null` is
/// [`CanonToolChoice::Auto`] (the wire default), `auto` carries,
/// `any` carries, a forced tool carries its name (when the wire gave
/// a string one), anything else carries its own type string — the
/// canonical never guesses a shape into expressibility, and a
/// BACKEND that cannot express one reports it. A tool_choice without
/// a type is a wire-shape violation, reported here.
///
/// One order corner, documented: the pair module read tool_choice
/// BEFORE `thinking`, so a body with both an inexpressible
/// tool_choice and a malformed thinking budget reported the
/// tool_choice; the split moves the inexpressible-shape report to
/// the backend adapter, so such a doubly-malformed body now reports
/// the thinking error first. The error set is unchanged — both were
/// and are [`TranslateError::Malformed`] — and no pinned test or
/// corpus fixture reads that corner.
fn tool_choice_of(body: &Value) -> Result<CanonToolChoice, TranslateError> {
    match body.get("tool_choice") {
        None | Some(Value::Null) => Ok(CanonToolChoice::Auto),
        Some(choice) => match choice.get("type").and_then(Value::as_str) {
            Some("auto") => Ok(CanonToolChoice::Auto),
            Some("any") => Ok(CanonToolChoice::Any),
            Some("tool") => Ok(CanonToolChoice::Tool {
                name: choice
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }),
            Some(other) => Ok(CanonToolChoice::Other {
                kind: other.to_owned(),
            }),
            None => Err(TranslateError::Malformed {
                reason: "tool_choice has no type".to_owned(),
            }),
        },
    }
}

/// The sampling parameters, CARRIED as specs: `temperature`, `top_p`,
/// `max_tokens`, `stop_sequences` — the things a backend that takes
/// sampling reads, and a backend that refuses it declares so (the
/// codex backend's declared cost). The read is lenient and never
/// errors — exactly the pair module's acceptance (it ignored the
/// fields entirely): a value that is not the number the wire
/// contract promises reads as absent, and non-string stop elements
/// are skipped, not fatal.
fn sampling_of(body: &Value) -> SamplingSpec {
    SamplingSpec {
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        max_tokens: body.get("max_tokens").and_then(Value::as_u64),
        stop_sequences: body
            .get("stop_sequences")
            .and_then(Value::as_array)
            .map(|stops| {
                stops
                    .iter()
                    .filter_map(|stop| stop.as_str().map(str::to_owned))
                    .collect()
            }),
    }
}

/// `thinking` → the request-side intent, parse `enabled` only:
/// absent, `null`, or any other shape means "the client did not ask"
/// (`None`) — the model's own default then governs, never guessed
/// here. A malformed budget is reported, never coerced.
fn thinking_of(body: &Value) -> Result<Option<ThinkingSpec>, TranslateError> {
    let Some(thinking) = body.get("thinking").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = thinking
        .as_object()
        .ok_or_else(|| TranslateError::Malformed {
            reason: "thinking is not an object".to_owned(),
        })?;
    if object.get("type").and_then(Value::as_str) != Some("enabled") {
        return Ok(None);
    }
    let budget_tokens = object
        .get("budget_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(|| TranslateError::Malformed {
            reason: "thinking.budget_tokens is missing or not a non-negative integer".to_owned(),
        })?;
    Ok(Some(ThinkingSpec { budget_tokens }))
}

// ── block content helpers ──────────────────────────────────────────

/// An `image` block's source → the URL (parse-time normalisation
/// only): a base64 source becomes the
/// `data:<media_type>;base64,<data>` URL, a url source passes
/// through verbatim. Any other source shape has no URL form and is
/// reported — the reason's wording is the pair module's, kept
/// byte-identical because the string rides the client error body.
fn image_url_of(block: &Value, at: &dyn Fn(&str) -> String) -> Result<String, TranslateError> {
    let source = block
        .get("source")
        .filter(|source| source.is_object())
        .ok_or_else(|| TranslateError::Malformed {
            reason: at("image has no source object"),
        })?;
    match source.get("type").and_then(Value::as_str) {
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
            Ok(format!("data:{media_type};base64,{data}"))
        }
        Some("url") => {
            let url = source.get("url").and_then(Value::as_str).ok_or_else(|| {
                TranslateError::Malformed {
                    reason: at("url image source has no url"),
                }
            })?;
            Ok(url.to_owned())
        }
        Some(other) => Err(TranslateError::Malformed {
            reason: at(&format!(
                "image source type {other:?} has no responses equivalent"
            )),
        }),
        None => Err(TranslateError::Malformed {
            reason: at("image source has no type"),
        }),
    }
}

/// A `tool_result`'s content: the wire's two shapes. A string is the
/// output text; absent or `null` is the empty text. A block array
/// becomes [`ToolResultContent::Blocks`] ONLY when the typed parse
/// reproduces the source bytes exactly (the guard compares the
/// output-string form a backend will emit against the raw JSON) —
/// anything else (extra fields, unknown kinds, exotic shapes) keeps
/// the raw-JSON string, the only lossless form it has and exactly
/// what the pair module emitted. A non-string non-array content is a
/// shape violation, reported.
fn tool_result_content_of(
    content: Option<&Value>,
    at: &dyn Fn(&str) -> String,
) -> Result<ToolResultContent, TranslateError> {
    match content {
        None | Some(Value::Null) => Ok(ToolResultContent::String(String::new())),
        Some(Value::String(text)) => Ok(ToolResultContent::String(text.clone())),
        Some(Value::Array(blocks)) => Ok(tool_result_blocks_of(blocks)),
        Some(other) => Err(TranslateError::Malformed {
            reason: at(&format!(
                "tool_result content is neither a string nor a \
                 block array ({})",
                json_kind(other)
            )),
        }),
    }
}

/// A tool_result's block-array content: the typed path when it is
/// byte-exact, the raw-JSON string otherwise. The parse is lenient —
/// nothing here can fail the translation, because no array shape
/// ever did.
fn tool_result_blocks_of(blocks: &[Value]) -> ToolResultContent {
    let raw = serde_json::to_string(blocks).expect("a parsed Value always serialises");
    match blocks.iter().map(lenient_block_of).collect() {
        Some(blocks) => {
            let typed = ToolResultContent::Blocks(blocks);
            if typed.output_text() == raw {
                typed
            } else {
                ToolResultContent::String(raw)
            }
        }
        None => ToolResultContent::String(raw),
    }
}

/// One block of a tool_result's content array, leniently: `None`
/// when the block is not a shape the typed path models — the guard
/// in [`tool_result_blocks_of`] then keeps the raw bytes instead.
/// (Images parse to their normalised URL form, which the guard will
/// reject for base64 sources — the raw anthropic source shape is
/// what the output string must reproduce.)
fn lenient_block_of(block: &Value) -> Option<CanonBlock> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => Some(CanonBlock::Text(block.get("text")?.as_str()?.to_owned())),
        "image" => Some(CanonBlock::Image {
            url: image_url_of(block, &|_| String::new()).ok()?,
        }),
        "tool_use" => Some(CanonBlock::ToolUse {
            id: block.get("id")?.as_str()?.to_owned(),
            name: block.get("name")?.as_str()?.to_owned(),
            input: block
                .get("input")
                .filter(|input| input.is_object())?
                .clone(),
        }),
        "tool_result" => Some(CanonBlock::ToolResult {
            tool_use_id: block.get("tool_use_id")?.as_str()?.to_owned(),
            content: match block.get("content") {
                None | Some(Value::Null) => ToolResultContent::String(String::new()),
                Some(Value::String(text)) => ToolResultContent::String(text.clone()),
                Some(Value::Array(blocks)) => tool_result_blocks_of(blocks),
                Some(_) => return None,
            },
        }),
        "thinking" => Some(CanonBlock::Thinking {
            text: block.get("thinking")?.as_str()?.to_owned(),
        }),
        "redacted_thinking" => Some(CanonBlock::Thinking {
            text: block.get("data")?.as_str()?.to_owned(),
        }),
        _ => None,
    }
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
    use super::from_anthropic;
    use crate::ir::canonical::{
        CanonBlock, CanonMessage, CanonRole, CanonTool, CanonToolChoice, SamplingSpec,
        ThinkingSpec, ToolResultContent,
    };
    use serde_json::{Value, json};

    // ── what the pair module used to decide early, now carried ────

    #[test]
    fn system_role_messages_stay_messages_in_the_canonical() {
        // The pair module merged a mid-conversation system message
        // into the preceding user turn AT PARSE TIME — a backend
        // policy decision. The canonical keeps the message a
        // MESSAGE: it is backend policy what to do with it.
        let body = json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "user", "content": "Earlier work."},
                {"role": "system", "content": [{"type": "text", "text": "reminder"}]},
                {"role": "user", "content": "Next."},
            ],
        });
        let canonical = from_anthropic(&body).expect("parses");
        assert_eq!(canonical.messages.len(), 3, "three messages, no merge");
        assert_eq!(
            canonical.messages[1],
            CanonMessage {
                role: CanonRole::System,
                blocks: vec![CanonBlock::Text("reminder".to_owned())],
            }
        );
        assert_eq!(canonical.messages[0].role, CanonRole::User);
        assert_eq!(canonical.messages[2].role, CanonRole::User);

        // A leading system message is a message too — hoisting it
        // into the system prompt is the backend's doing.
        let body = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "Preamble."},
                {"role": "user", "content": "Hi"},
            ],
        });
        let canonical = from_anthropic(&body).expect("parses");
        assert_eq!(canonical.messages[0].role, CanonRole::System);
        assert_eq!(
            canonical.messages[0].blocks,
            vec![CanonBlock::Text("Preamble.".to_owned())]
        );
        // And the top-level system stays SEPARATE pieces, not joined.
        let body = json!({
            "model": "m",
            "system": [{"type": "text", "text": "One."}, {"type": "text", "text": "Two."}],
            "messages": [{"role": "system", "content": "Preamble."}],
        });
        let canonical = from_anthropic(&body).expect("parses");
        assert_eq!(
            canonical.system,
            vec!["One.".to_owned(), "Two.".to_owned()],
            "pieces, not a joined instruction"
        );
    }

    #[test]
    fn thinking_blocks_stay_blocks_in_the_canonical() {
        // The pair module dropped thinking AT PARSE TIME — backend
        // policy. The canonical carries the blocks: the drop is a
        // backend's declared capability (the codex backend's
        // thinking_replay is false), never a parse decision.
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
        let canonical = from_anthropic(&body).expect("parses");
        assert_eq!(
            canonical.messages[0].blocks,
            vec![
                CanonBlock::Thinking {
                    text: "secret reasoning".to_owned(),
                },
                CanonBlock::Thinking {
                    text: "opaque-blob".to_owned(),
                },
                CanonBlock::Text("Answer.".to_owned()),
            ]
        );

        // Thinking in a USER message is not a policy question — it
        // is a block with no faithful parse in that position,
        // reported.
        let misplaced = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "thinking", "thinking": "user-side"},
            ]}],
        });
        assert_eq!(
            from_anthropic(&misplaced),
            Err(TranslateError::UnsupportedBlock {
                kind: "thinking".to_owned()
            })
        );
    }

    #[test]
    fn sampling_rides_as_specs_never_a_parse_time_discard() {
        // The pair module dropped sampling AT PARSE TIME — the codex
        // backend's refusal, baked into a pair. The canonical carries
        // the specs; a refusing backend declares so.
        let body = json!({
            "model": "claude-opus-5",
            "max_tokens": 4096,
            "temperature": 0.3,
            "top_p": 0.95,
            "stop_sequences": ["\n\nHuman:", "STOP"],
            "messages": [{"role": "user", "content": "Hi"}],
        });
        assert_eq!(
            from_anthropic(&body).expect("parses").sampling,
            SamplingSpec {
                temperature: Some(0.3),
                top_p: Some(0.95),
                max_tokens: Some(4096),
                stop_sequences: Some(vec!["\n\nHuman:".to_owned(), "STOP".to_owned()]),
            }
        );
        // Absent reads absent, on every field.
        let bare = json!({"model": "m", "messages": [{"role": "user", "content": "Hi"}]});
        assert_eq!(
            from_anthropic(&bare).expect("parses").sampling,
            SamplingSpec {
                temperature: None,
                top_p: None,
                max_tokens: None,
                stop_sequences: None,
            }
        );
        // Zero is a value, not an absence.
        let zero = json!({"model": "m", "temperature": 0,
                          "messages": [{"role": "user", "content": "Hi"}]});
        assert_eq!(
            from_anthropic(&zero).expect("parses").sampling.temperature,
            Some(0.0)
        );
        // A value that is not the promised number reads as absent —
        // the pair module ignored the field entirely, so nothing
        // here errors.
        let garbage = json!({"model": "m", "temperature": "warm", "max_tokens": null,
                             "messages": [{"role": "user", "content": "Hi"}]});
        let sampling = from_anthropic(&garbage).expect("parses").sampling;
        assert_eq!(sampling.temperature, None);
        assert_eq!(sampling.max_tokens, None);
    }

    #[test]
    fn the_stream_flag_is_carried_tri_state() {
        let body = |stream: Value| {
            json!({"model": "m", "stream": stream,
                   "messages": [{"role": "user", "content": "Hi"}]})
        };
        assert_eq!(
            from_anthropic(&body(json!(true))).expect("parses").stream,
            Some(true)
        );
        assert_eq!(
            from_anthropic(&body(json!(false))).expect("parses").stream,
            Some(false)
        );
        assert_eq!(
            from_anthropic(&json!({"model": "m",
                                   "messages": [{"role": "user", "content": "Hi"}]}))
            .expect("parses")
            .stream,
            None,
            "absent stays absent, never guessed"
        );
    }

    // ── the wire reads, relocated from the pair module ─────────────

    #[test]
    fn system_pieces_parse_in_order_and_absent_reads_empty() {
        let join = json!({
            "model": "m",
            "system": [{"type": "text", "text": "One.", "cache_control": {"type": "ephemeral"}},
                       "bare string element",
                       {"no_text": true}],
            "messages": [{"role": "user", "content": "Hi"}],
        });
        // The pieces: each block's text (cache_control is metadata,
        // not text), each bare string element, "" for the block
        // without one — in order, NOT joined (the join is backend
        // policy).
        assert_eq!(
            from_anthropic(&join).expect("parses").system,
            vec![
                "One.".to_owned(),
                "bare string element".to_owned(),
                String::new()
            ]
        );

        let absent = json!({"model": "m", "messages": [{"role": "user", "content": "Hi"}]});
        assert_eq!(
            from_anthropic(&absent).expect("parses").system,
            Vec::<String>::new()
        );

        let null = json!({"model": "m", "system": null,
                          "messages": [{"role": "user", "content": "Hi"}]});
        assert_eq!(
            from_anthropic(&null).expect("parses").system,
            Vec::<String>::new()
        );

        let string = json!({"model": "m", "system": "One prompt.",
                            "messages": [{"role": "user", "content": "Hi"}]});
        assert_eq!(
            from_anthropic(&string).expect("parses").system,
            vec!["One prompt.".to_owned()]
        );

        let malformed = json!({"model": "m", "system": 5,
                               "messages": [{"role": "user", "content": "Hi"}]});
        assert!(matches!(
            from_anthropic(&malformed),
            Err(TranslateError::Malformed { .. })
        ));
    }

    #[test]
    fn image_sources_parse_to_urls_at_parse_time() {
        let body = |source: Value| {
            json!({
                "model": "m",
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "Look:"},
                    {"type": "image", "source": source},
                ]}],
            })
        };
        // A base64 source is already its data: URL — parse-time
        // normalisation, the only kind there is.
        assert_eq!(
            from_anthropic(&body(
                json!({"type": "base64", "media_type": "image/png", "data": "aVBONyU="})
            ))
            .expect("parses")
            .messages[0]
                .blocks[1],
            CanonBlock::Image {
                url: "data:image/png;base64,aVBONyU=".to_owned()
            }
        );
        // A url source passes through verbatim.
        assert_eq!(
            from_anthropic(&body(
                json!({"type": "url", "url": "https://example.test/chart.png"})
            ))
            .expect("parses")
            .messages[0]
                .blocks[1],
            CanonBlock::Image {
                url: "https://example.test/chart.png".to_owned()
            }
        );
        // A file source and a shapeless source are reported.
        assert!(matches!(
            from_anthropic(&body(json!({"type": "file", "file_id": "file_1"}))),
            Err(TranslateError::Malformed { .. })
        ));
        assert!(matches!(
            from_anthropic(&body(json!("not an object"))),
            Err(TranslateError::Malformed { .. })
        ));

        // An image in an ASSISTANT message has no faithful parse in
        // that position — reported, never an invented block.
        let assistant = json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": [
                {"type": "image",
                 "source": {"type": "base64", "media_type": "image/png", "data": "aVBONyU="}},
            ]}],
        });
        assert_eq!(
            from_anthropic(&assistant),
            Err(TranslateError::UnsupportedBlock {
                kind: "image".to_owned()
            })
        );
    }

    #[test]
    fn tool_result_content_parses_blocks_only_when_byte_exact() {
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
        assert_eq!(
            from_anthropic(&body(json!("plain text")))
                .expect("parses")
                .messages[0]
                .blocks[0],
            CanonBlock::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: ToolResultContent::String("plain text".to_owned()),
            }
        );
        // A clean block array parses typed — and its output-string
        // form reproduces the raw bytes exactly.
        let clean = from_anthropic(&body(json!([{"type": "text", "text": "src holds"},
                         {"type": "text", "text": "two modules."}])))
        .expect("parses");
        assert_eq!(
            clean.messages[0].blocks[0],
            CanonBlock::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: ToolResultContent::Blocks(vec![
                    CanonBlock::Text("src holds".to_owned()),
                    CanonBlock::Text("two modules.".to_owned()),
                ]),
            }
        );
        assert_eq!(
            match &clean.messages[0].blocks[0] {
                CanonBlock::ToolResult { content, .. } => content.output_text(),
                _ => unreachable!(),
            },
            "[{\"type\":\"text\",\"text\":\"src holds\"},{\"type\":\"text\",\"text\":\"two modules.\"}]",
        );
        // Extra fields or unknown kinds: the typed path cannot
        // reproduce the bytes, so the raw-JSON string rides — the
        // only lossless form, exactly what the pair module emitted.
        let extra = from_anthropic(&body(json!([{"type": "text", "text": "kept",
                         "cache_control": {"type": "ephemeral"}}])))
        .expect("parses");
        assert_eq!(
            extra.messages[0].blocks[0],
            CanonBlock::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: ToolResultContent::String(
                    "[{\"type\":\"text\",\"text\":\"kept\",\"cache_control\":{\"type\":\"ephemeral\"}}]"
                        .to_owned(),
                ),
            }
        );
        let unknown = from_anthropic(&body(json!([{"type": "banana"}]))).expect("parses");
        assert_eq!(
            unknown.messages[0].blocks[0],
            CanonBlock::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: ToolResultContent::String("[{\"type\":\"banana\"}]".to_owned()),
            }
        );
        // A base64 image inside keeps its raw anthropic shape (the
        // typed path normalises to a data: URL, which is not the
        // source bytes).
        let image = from_anthropic(&body(json!([{"type": "image",
                         "source": {"type": "base64", "media_type": "image/png",
                                    "data": "aVBONyU="}}])))
        .expect("parses");
        assert_eq!(
            image.messages[0].blocks[0],
            CanonBlock::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: ToolResultContent::String(
                    "[{\"type\":\"image\",\"source\":{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"aVBONyU=\"}}]"
                        .to_owned(),
                ),
            }
        );
        // Absent content is the empty output.
        let absent = from_anthropic(&body(Value::Null)).expect("parses");
        assert_eq!(
            absent.messages[0].blocks[0],
            CanonBlock::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: ToolResultContent::String(String::new()),
            }
        );
        // Anything else is a shape violation, reported.
        assert!(matches!(
            from_anthropic(&body(json!(5))),
            Err(TranslateError::Malformed { .. })
        ));
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
            from_anthropic(&unknown),
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
            from_anthropic(&tool_use_in_user),
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
            from_anthropic(&tool_result_in_assistant),
            Err(TranslateError::UnsupportedBlock {
                kind: "tool_result".to_owned()
            })
        );
        // And a non-text block in a system message is unsupported on
        // this route — never a silent truncation.
        let image_in_system = json!({
            "model": "m",
            "messages": [{"role": "system", "content": [
                {"type": "image", "source": {"type": "url", "url": "https://x.test/i.png"}},
            ]}],
        });
        assert_eq!(
            from_anthropic(&image_in_system),
            Err(TranslateError::UnsupportedBlock {
                kind: "image".to_owned()
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
            // a system message's blocks must be text blocks.
            body(json!([{"role": "system", "content": [{"text": "no type"}]}])),
            body(json!([{"role": "system", "content": 5}])),
        ];
        for case in cases {
            assert!(
                matches!(from_anthropic(&case), Err(TranslateError::Malformed { .. })),
                "case must be reported as malformed: {case}"
            );
        }
        // String content always parses — even empty, a message of
        // its own (the backend yields its item).
        let empty =
            from_anthropic(&body(json!([{"role": "user", "content": ""}]))).expect("parses");
        assert_eq!(
            empty.messages[0].blocks,
            vec![CanonBlock::Text(String::new())]
        );
        // And a system message with NO content reads as no blocks.
        let bare = from_anthropic(&body(json!([{"role": "system"}]))).expect("parses");
        assert_eq!(bare.messages[0].blocks, Vec::<CanonBlock>::new());
    }

    #[test]
    fn tools_parse_to_the_canonical_shape() {
        let body = json!({
            "model": "m",
            "tools": [
                {"name": "read_file", "input_schema": {"type": "object"}},
                {"name": "list_dir", "description": "List a directory",
                 "input_schema": {"type": "object", "properties": {}}},
            ],
            "messages": [{"role": "user", "content": "Hi"}],
        });
        assert_eq!(
            from_anthropic(&body).expect("parses").tools,
            vec![
                CanonTool {
                    name: "read_file".to_owned(),
                    description: String::new(),
                    parameters: json!({"type": "object"}),
                },
                CanonTool {
                    name: "list_dir".to_owned(),
                    description: "List a directory".to_owned(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
            ],
            "absent description reads empty — parse-time normalisation"
        );

        // Absent/null tools read as none.
        let none = json!({"model": "m", "messages": [{"role": "user", "content": "Hi"}]});
        assert!(from_anthropic(&none).expect("parses").tools.is_empty());
        let null = json!({"model": "m", "tools": null,
                          "messages": [{"role": "user", "content": "Hi"}]});
        assert!(from_anthropic(&null).expect("parses").tools.is_empty());

        // A tool the canonical cannot express faithfully — an
        // unnamed entry (anthropic's own API-invalid custom shapes),
        // a missing or non-object schema — is reported, never
        // fabricated.
        let unnamed = json!({"model": "m", "tools": [{"type": "custom_tool"}],
                             "messages": [{"role": "user", "content": "Hi"}]});
        let error = from_anthropic(&unnamed).unwrap_err();
        assert!(
            matches!(&error, TranslateError::Malformed { reason } if reason.contains("tools[0]")),
            "the reason names the entry: {error:?}"
        );
        let no_schema = json!({"model": "m", "tools": [{"name": "n"}],
                               "messages": [{"role": "user", "content": "Hi"}]});
        assert!(matches!(
            from_anthropic(&no_schema),
            Err(TranslateError::Malformed { .. })
        ));
        let not_array = json!({"model": "m", "tools": {"name": "n"},
                               "messages": [{"role": "user", "content": "Hi"}]});
        assert!(matches!(
            from_anthropic(&not_array),
            Err(TranslateError::Malformed { .. })
        ));
    }

    #[test]
    fn tool_choice_parses_the_wire_shapes_and_carries_the_rest() {
        let body = |tool_choice: Value| {
            json!({"model": "m", "tool_choice": tool_choice,
                   "messages": [{"role": "user", "content": "Hi"}]})
        };
        // Absent and null read the wire default.
        assert_eq!(
            from_anthropic(&json!({"model": "m",
                                   "messages": [{"role": "user", "content": "Hi"}]}))
            .expect("parses")
            .tool_choice,
            CanonToolChoice::Auto
        );
        assert_eq!(
            from_anthropic(&body(Value::Null))
                .expect("parses")
                .tool_choice,
            CanonToolChoice::Auto
        );
        assert_eq!(
            from_anthropic(&body(json!({"type": "auto"})))
                .expect("parses")
                .tool_choice,
            CanonToolChoice::Auto
        );
        assert_eq!(
            from_anthropic(&body(json!({"type": "any"})))
                .expect("parses")
                .tool_choice,
            CanonToolChoice::Any
        );
        // A forced tool carries its name; without one, the shape
        // still carries — the BACKEND reports what it cannot
        // express, never a flattening into auto/any.
        assert_eq!(
            from_anthropic(&body(json!({"type": "tool", "name": "read_file"})))
                .expect("parses")
                .tool_choice,
            CanonToolChoice::Tool {
                name: Some("read_file".to_owned())
            }
        );
        assert_eq!(
            from_anthropic(&body(json!({"type": "tool"})))
                .expect("parses")
                .tool_choice,
            CanonToolChoice::Tool { name: None }
        );
        assert_eq!(
            from_anthropic(&body(json!({"type": "banana"})))
                .expect("parses")
                .tool_choice,
            CanonToolChoice::Other {
                kind: "banana".to_owned()
            }
        );
        // No type is a wire-shape violation, reported — including a
        // tool_choice that is not an object at all.
        for bad in [json!({}), json!("auto"), json!(5)] {
            assert!(
                matches!(
                    from_anthropic(&body(bad.clone())),
                    Err(TranslateError::Malformed { .. })
                ),
                "case must be reported as malformed: {bad}"
            );
        }
    }

    #[test]
    fn thinking_requests_parse_to_the_spec() {
        // `enabled` with a budget carries the intent — the only
        // request-side shape that does.
        assert_eq!(
            from_anthropic(&json!({"model": "m",
                                   "thinking": {"type": "enabled", "budget_tokens": 20_000},
                                   "messages": [{"role": "user", "content": "Hi"}]}))
            .expect("parses")
            .thinking,
            Some(ThinkingSpec {
                budget_tokens: 20_000
            })
        );
        // Absent and disabled both mean "the client did not ask" —
        // never guessed here.
        for body in [
            json!({"model": "m", "messages": [{"role": "user", "content": "Hi"}]}),
            json!({"model": "m", "thinking": {"type": "disabled"},
                   "messages": [{"role": "user", "content": "Hi"}]}),
            json!({"model": "m", "thinking": null,
                   "messages": [{"role": "user", "content": "Hi"}]}),
            json!({"model": "m", "thinking": {},
                   "messages": [{"role": "user", "content": "Hi"}]}),
        ] {
            assert_eq!(
                from_anthropic(&body).expect("parses").thinking,
                None,
                "case reads as not asked: {body}"
            );
        }
        // Malformed budgets are reported, never coerced.
        for body in [
            json!({"model": "m", "thinking": {"type": "enabled"},
                   "messages": [{"role": "user", "content": "Hi"}]}),
            json!({"model": "m", "thinking": {"type": "enabled", "budget_tokens": "4096"},
                   "messages": [{"role": "user", "content": "Hi"}]}),
            json!({"model": "m", "thinking": 5,
                   "messages": [{"role": "user", "content": "Hi"}]}),
        ] {
            assert!(matches!(
                from_anthropic(&body),
                Err(TranslateError::Malformed { .. })
            ));
        }
    }
}
