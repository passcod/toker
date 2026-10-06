//! The frontend adapter, both directions: one Anthropic Messages
//! body → one [`CanonicalRequest`] (the request parse, below), and
//! the canonical turn model → Anthropic SSE / the complete message
//! JSON (the response render — see "The response direction" at the
//! bottom of these docs).
//!
//! This file's domain is the ANTHROPIC WIRE and the canonical, and
//! nothing else. Request side: the role table (which role may carry
//! which block kind), the block parses, the string-or-blocks
//! readings, and the shape reporting —
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
//!
//! # The response direction (rendering)
//!
//! [`AnthropicRenderer`] is the streamed path: a small explicit
//! state machine over canonical [`CanonEvent`]s — which content
//! block is open (thinking or text, at which index), the next block
//! index, whether the turn ended. Everything it emits is a pure
//! function of the events it has been fed and the model echo — no
//! clock, no counters, nothing else (invariant 4). Claude Code's
//! event names, exactly: `message_start`, `content_block_start`,
//! `content_block_delta`, `content_block_stop`, `message_delta`,
//! `message_stop`, `error` — no `ping` (claude tolerates its absence;
//! nothing upstream produces one).
//!
//! The message id is the backend's own turn id, verbatim — the one
//! identity the turn actually has. A turn that never named one gets
//! the constant `"msg"`: purity forbids minting a fresh id (a random
//! or clock-derived id would break byte-stability for nothing).
//!
//! The `message_start` usage is **zeroed** — anthropic's own shape for
//! a provisional snapshot, which this is: the real usage only exists
//! at the turn's end and rides the `message_delta`. A turn that ends
//! without one (the canonical `TurnEnded` may carry no usage) leaves
//! the zeros provisional, exactly like anthropic's own start usage;
//! the authoritative `message_delta` usage replaces it whenever it
//! arrives.
//!
//! [`anthropic_from_canonical`] is the `stream:false` sibling: the
//! same content assembly over the canonical final ([`CanonTurn`]),
//! producing the complete non-streaming Anthropic message JSON. Its
//! content order is categorical — thinking parts (by the backend's
//! part identity), text, then the tool calls (in completion order) —
//! the turn carries no cross-category arrival order; for real turns
//! the categories arrive in exactly that order anyway, and the SSE
//! path emits true arrival order for whatever interleaving the
//! upstream sends.
//!
//! Error events render from [`CanonError`]: the kind → anthropic's
//! `error.type` (the table's rendering half — the interpretation
//! half lives in the backend adapter), the message verbatim (the
//! backend resolved its own stand-in chain before the canonical),
//! and `retry_after` only for rate limits — the absolute reset
//! epoch, verbatim (a relative retry-after would need a clock, and
//! this is pure).

use serde_json::{Map, Value, json};

use crate::ir::canonical::{
    CanonBlock, CanonError, CanonErrorKind, CanonEvent, CanonMessage, CanonRole, CanonStopReason,
    CanonSystemPart, CanonTool, CanonToolChoice, CanonTurn, CanonicalExtension, CanonicalRequest,
    CanonicalUsage, SamplingSpec, ThinkingSpec, ToolResultContent,
};
use crate::observe::sse::SseEvent;
use crate::routing::DialectId;
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
    let extensions = extensions_of(body);
    Ok(CanonicalRequest {
        model: body.get("model").and_then(Value::as_str).map(str::to_owned),
        system,
        messages,
        tools,
        sampling,
        thinking,
        stream,
        tool_choice,
        extensions,
    })
}

fn extensions_of(body: &Value) -> Vec<CanonicalExtension> {
    const MODELED: &[&str] = &[
        "max_tokens",
        "messages",
        "model",
        "stop_sequences",
        "stream",
        "system",
        "temperature",
        "thinking",
        "tool_choice",
        "tools",
        "top_p",
    ];

    body.as_object()
        .into_iter()
        .flat_map(|object| object.iter())
        .filter(|(field, _)| !MODELED.contains(&field.as_str()))
        .map(|(field, value)| {
            CanonicalExtension::node_field(
                DialectId::AnthropicMessages,
                format!("$.{field}"),
                field,
                value.clone(),
            )
        })
        .collect()
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
                out.push(annotated_block(
                    block,
                    CanonBlock::Text(text.to_owned()),
                    &["type", "text"],
                    None,
                ));
            }
            (CanonRole::User, "image") => {
                out.push(annotated_block(
                    block,
                    CanonBlock::Image {
                        url: image_url_of(block, &at)?,
                    },
                    &["type", "source"],
                    None,
                ));
            }
            (CanonRole::User, "tool_result") => {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| TranslateError::Malformed {
                        reason: at("tool_result has no tool_use_id"),
                    })?;
                let content = tool_result_content_of(block.get("content"), &at)?;
                out.push(annotated_block(
                    block,
                    CanonBlock::ToolResult {
                        tool_use_id: tool_use_id.to_owned(),
                        content,
                    },
                    &["type", "tool_use_id", "content"],
                    block.get("is_error").and_then(Value::as_bool),
                ));
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
                out.push(annotated_block(
                    block,
                    CanonBlock::ToolUse {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        input: input.clone(),
                    },
                    &["type", "id", "name", "input"],
                    None,
                ));
            }
            // THINKING STAYS IN THE CANONICAL — including the signature and
            // the distinction between visible and redacted forms. Whether it
            // replays is backend policy, never a parse decision.
            (CanonRole::Assistant, "thinking") => out.push(annotated_block(
                block,
                CanonBlock::Thinking {
                    text: block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    signature: block
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
                &["type", "thinking", "signature"],
                None,
            )),
            (CanonRole::Assistant, "redacted_thinking") => out.push(annotated_block(
                block,
                CanonBlock::RedactedThinking {
                    data: block
                        .get("data")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                },
                &["type", "data"],
                None,
            )),
            (_, other) => {
                return Err(TranslateError::UnsupportedBlock {
                    kind: other.to_owned(),
                });
            }
        }
    }
    Ok(out)
}

fn annotated_block(
    source: &Value,
    block: CanonBlock,
    semantic_fields: &[&str],
    is_error: Option<bool>,
) -> CanonBlock {
    let extensions = source
        .as_object()
        .into_iter()
        .flat_map(|object| object.iter())
        .filter(|(field, _)| {
            !semantic_fields.contains(&field.as_str())
                && !(field.as_str() == "is_error" && is_error.is_some())
        })
        .map(|(field, value)| {
            CanonicalExtension::node_field(
                DialectId::AnthropicMessages,
                format!("$.messages[].content[].{field}"),
                field,
                value.clone(),
            )
        })
        .collect();
    block.annotated(is_error, extensions)
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
                let text = block.get("text").and_then(Value::as_str).ok_or_else(|| {
                    TranslateError::Malformed {
                        reason: format!(
                            "messages[{index}] block {block_index}: \
                             text is missing or not a string"
                        ),
                    }
                })?;
                out.push(annotated_block(
                    block,
                    CanonBlock::Text(text.to_owned()),
                    &["type", "text"],
                    None,
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

/// `system` → semantic text plus node-local opaque metadata, in order. A
/// string is plain text. A text block retains every field other than `type`
/// and `text` as a dialect extension attached to that part. Any other array
/// element remains opaque as a whole instead of becoming invented empty text.
/// Absent or `null` reads as no parts. How text joins, and which extensions
/// replay, is backend policy.
fn system_of(body: &Value) -> Result<Vec<CanonSystemPart>, TranslateError> {
    match body.get("system") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![CanonSystemPart::text(text)]),
        Some(Value::Array(blocks)) => Ok(blocks.iter().map(system_part_of).collect()),
        Some(other) => Err(TranslateError::Malformed {
            reason: format!(
                "system is neither a string nor a block array ({})",
                json_kind(other)
            ),
        }),
    }
}

fn system_part_of(part: &Value) -> CanonSystemPart {
    if let Value::String(text) = part {
        return CanonSystemPart::text(text);
    }

    let Some(object) = part.as_object() else {
        return CanonSystemPart::Opaque(CanonicalExtension::new(
            DialectId::AnthropicMessages,
            "$.system[]",
            part.clone(),
        ));
    };
    if object.get("type").is_some_and(|kind| kind != "text") {
        return CanonSystemPart::Opaque(CanonicalExtension::new(
            DialectId::AnthropicMessages,
            "$.system[]",
            part.clone(),
        ));
    }
    let Some(text) = object.get("text").and_then(Value::as_str) else {
        return CanonSystemPart::Opaque(CanonicalExtension::new(
            DialectId::AnthropicMessages,
            "$.system[]",
            part.clone(),
        ));
    };

    let extensions = object
        .iter()
        .filter(|(field, _)| field.as_str() != "type" && field.as_str() != "text")
        .map(|(field, value)| {
            CanonicalExtension::node_field(
                DialectId::AnthropicMessages,
                format!("$.system[].{field}"),
                field,
                value.clone(),
            )
        })
        .collect();
    CanonSystemPart::Text {
        text: text.to_owned(),
        extensions,
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
            extensions: tool
                .as_object()
                .into_iter()
                .flat_map(|object| object.iter())
                .filter(|(field, _)| {
                    !["name", "description", "input_schema"].contains(&field.as_str())
                })
                .map(|(field, value)| {
                    CanonicalExtension::node_field(
                        DialectId::AnthropicMessages,
                        format!("$.tools[].{field}"),
                        field,
                        value.clone(),
                    )
                })
                .collect(),
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
    let kind = block.get("type").and_then(Value::as_str)?;
    let (semantic, fields): (CanonBlock, &[&str]) = match kind {
        "text" => (
            CanonBlock::Text(block.get("text")?.as_str()?.to_owned()),
            &["type", "text"],
        ),
        "image" => (
            CanonBlock::Image {
                url: image_url_of(block, &|_| String::new()).ok()?,
            },
            &["type", "source"],
        ),
        "tool_use" => (
            CanonBlock::ToolUse {
                id: block.get("id")?.as_str()?.to_owned(),
                name: block.get("name")?.as_str()?.to_owned(),
                input: block
                    .get("input")
                    .filter(|input| input.is_object())?
                    .clone(),
            },
            &["type", "id", "name", "input"],
        ),
        "tool_result" => (
            CanonBlock::ToolResult {
                tool_use_id: block.get("tool_use_id")?.as_str()?.to_owned(),
                content: match block.get("content") {
                    None | Some(Value::Null) => ToolResultContent::String(String::new()),
                    Some(Value::String(text)) => ToolResultContent::String(text.clone()),
                    Some(Value::Array(blocks)) => tool_result_blocks_of(blocks),
                    Some(_) => return None,
                },
            },
            &["type", "tool_use_id", "content"],
        ),
        "thinking" => (
            CanonBlock::Thinking {
                text: block.get("thinking")?.as_str()?.to_owned(),
                signature: block
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            &["type", "thinking", "signature"],
        ),
        "redacted_thinking" => (
            CanonBlock::RedactedThinking {
                data: block.get("data")?.as_str()?.to_owned(),
            },
            &["type", "data"],
        ),
        _ => return None,
    };
    Some(annotated_block(
        block,
        semantic,
        fields,
        (kind == "tool_result")
            .then(|| block.get("is_error").and_then(Value::as_bool))
            .flatten(),
    ))
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

// ── the response direction: rendering ───────────────────────────────

/// The message id for a turn that never named one — a constant, not
/// a minted id (purity).
const UNNAMED_MESSAGE: &str = "msg";

/// The Anthropic SSE state machine for one streamed turn: feed it the
/// turn's [`CanonEvent`]s (in arrival order), collect the emitted
/// [`SseEvent`]s. One stream per turn — the response-direction
/// counterpart of the request parse: canonical in, anthropic out.
#[derive(Debug, Clone)]
pub struct AnthropicRenderer {
    /// The model echo — the model the caller wants the client to see
    /// (the requested anthropic model, post-middleware).
    model: String,
    /// Whether the turn started ([`CanonEvent::TurnStarted`] seen —
    /// `message_start` is emitted exactly once).
    started: bool,
    /// The backend's own turn id, latched from the first
    /// [`CanonEvent::TurnStarted`].
    message_id: Option<String>,
    /// The index the next opened content block gets.
    next_index: usize,
    /// The currently open content block, if any.
    open: Option<OpenBlock>,
    /// Whether the turn's final-state event passed through.
    ended: bool,
}

/// The content block under assembly: its kind (which stream feeds it)
/// and its index.
#[derive(Debug, Clone, Copy)]
enum OpenBlock {
    Thinking { index: usize, part: u64 },
    Text { index: usize },
}

/// The identity of a content block: text continues text; a reasoning
/// part continues the thinking block of the same part.
#[derive(Debug, Clone, Copy)]
enum BlockKind {
    Thinking { part: u64 },
    Text,
}

impl OpenBlock {
    /// Whether `other` continues this block.
    fn continues(&self, other: &BlockKind) -> bool {
        match (self, other) {
            (OpenBlock::Text { .. }, BlockKind::Text) => true,
            (OpenBlock::Thinking { part: a, .. }, BlockKind::Thinking { part: b }) => a == b,
            _ => false,
        }
    }

    fn index(&self) -> usize {
        match self {
            OpenBlock::Thinking { index, .. } | OpenBlock::Text { index } => *index,
        }
    }
}

impl AnthropicRenderer {
    /// A fresh renderer for one turn, echoing `model` in the
    /// `message_start` (the streaming path — non-streaming turns go
    /// through [`anthropic_from_canonical`]).
    pub fn new(model: &str) -> AnthropicRenderer {
        AnthropicRenderer {
            model: model.to_owned(),
            started: false,
            message_id: None,
            next_index: 0,
            open: None,
            ended: false,
        }
    }

    /// Feed one canonical turn event; every Anthropic SSE event it
    /// produced, in anthropic event order. Never fails — an event
    /// with no anthropic shape produces nothing (invariant 6).
    pub fn feed(&mut self, event: &CanonEvent) -> Vec<SseEvent> {
        let mut out = Vec::new();
        match event {
            CanonEvent::TurnStarted { turn_id } => {
                if !self.started {
                    self.started = true;
                    if self.message_id.is_none() {
                        self.message_id.clone_from(turn_id);
                    }
                    out.push(sse_event(
                        "message_start",
                        json!({
                            "type": "message_start",
                            "message": {
                                "id": self.message_id_or_default(),
                                "type": "message",
                                "role": "assistant",
                                "model": self.model,
                                "content": [],
                                "usage": {
                                    "input_tokens": 0,
                                    "cache_creation_input_tokens": 0,
                                    "cache_read_input_tokens": 0,
                                    "output_tokens": 0,
                                },
                            },
                        }),
                    ));
                }
            }
            CanonEvent::TextDelta { delta } => {
                let index = self.ensure_open(BlockKind::Text, &mut out);
                out.push(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": delta},
                    }),
                ));
            }
            CanonEvent::ThinkingDelta { part, delta } => {
                let block = BlockKind::Thinking { part: *part };
                let index = self.ensure_open(block, &mut out);
                out.push(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "thinking_delta", "thinking": delta},
                    }),
                ));
            }
            // The complete tool call: the arguments arrive whole on
            // the canonical, so ONE input_json_delta carries them
            // all — a tool_use block opened, filled, and closed.
            CanonEvent::ToolCall(call) => {
                self.close_open(&mut out);
                let index = self.next_index;
                self.next_index += 1;
                out.push(sse_event(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": {},
                        },
                    }),
                ));
                out.push(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": call.arguments,
                        },
                    }),
                ));
                out.push(sse_event(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": index}),
                ));
            }
            // A completed text part closes whatever block is open —
            // the pair module's message-item done closed
            // unconditionally, and this keeps that.
            CanonEvent::TextEnded => self.close_open(&mut out),
            // A completed reasoning part closes its thinking block
            // if one is open; a text block stays (the pair module's
            // reasoning-item done did the same).
            CanonEvent::ThinkingEnded => {
                if matches!(self.open, Some(OpenBlock::Thinking { .. })) {
                    self.close_open(&mut out);
                }
            }
            CanonEvent::TurnEnded { stop_reason, usage } => {
                self.close_open(&mut out);
                let mut data = Map::new();
                data.insert("type".to_owned(), json!("message_delta"));
                data.insert(
                    "delta".to_owned(),
                    json!({"stop_reason": stop_reason_of(stop_reason), "stop_sequence": null}),
                );
                if let Some(usage) = usage {
                    data.insert("usage".to_owned(), usage_json(usage));
                }
                out.push(sse_event("message_delta", Value::Object(data)));
                out.push(sse_event("message_stop", json!({"type": "message_stop"})));
                self.ended = true;
            }
            CanonEvent::TurnFailed { error } => {
                // No message_delta, no message_stop: anthropic error
                // streams end at the error, mid-block if one was
                // open (toker's own captured fixture 06 is the
                // shape).
                out.push(sse_event("error", anthropic_error_event_data(error)));
                self.ended = true;
            }
            CanonEvent::Error { error } => {
                out.push(sse_event("error", anthropic_error_event_data(error)));
            }
        }
        out
    }

    /// Whether the turn's final-state event passed through — the
    /// caller's "the accounting is final" signal.
    pub fn turn_ended(&self) -> bool {
        self.ended
    }

    /// Open a block of `kind` unless it continues the open one; the
    /// block's index (the open block's, or the freshly assigned one).
    fn ensure_open(&mut self, kind: BlockKind, out: &mut Vec<SseEvent>) -> usize {
        if !matches!(self.open, Some(open) if open.continues(&kind)) {
            self.close_open(out);
            out.push(self.open_block(kind));
        }
        match self.open {
            Some(open) => open.index(),
            None => unreachable!("the block was just opened"),
        }
    }

    /// Open a content block: assign its index, emit the start.
    fn open_block(&mut self, kind: BlockKind) -> SseEvent {
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some(match kind {
            BlockKind::Thinking { part } => OpenBlock::Thinking { index, part },
            BlockKind::Text => OpenBlock::Text { index },
        });
        let content_block = match kind {
            BlockKind::Thinking { .. } => json!({"type": "thinking", "thinking": ""}),
            BlockKind::Text => json!({"type": "text", "text": ""}),
        };
        sse_event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": content_block,
            }),
        )
    }

    /// Close the open content block, if any (emits its stop).
    fn close_open(&mut self, out: &mut Vec<SseEvent>) {
        if let Some(block) = self.open.take() {
            out.push(sse_event(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": block.index()}),
            ));
        }
    }

    fn message_id_or_default(&self) -> &str {
        self.message_id.as_deref().unwrap_or(UNNAMED_MESSAGE)
    }
}

/// The `stream:false` sibling: the canonical final turn
/// ([`CanonTurn`]) → the complete non-streaming Anthropic message
/// JSON — the same content assembly and usage mapping as the streamed
/// path. An errored turn yields the anthropic error body instead (no
/// content: anthropic's non-streaming errors carry none).
pub fn anthropic_from_canonical(model: &str, turn: &CanonTurn) -> Value {
    if let Some(error) = &turn.error {
        return anthropic_error_event_data(error);
    }

    let mut content: Vec<Value> = Vec::new();
    for summary in turn.thinking.values() {
        content.push(json!({"type": "thinking", "thinking": summary}));
    }
    if !turn.text.is_empty() {
        content.push(json!({"type": "text", "text": turn.text}));
    }
    for call in &turn.tool_calls {
        // The arguments are a JSON string on the canonical (the
        // backend's whole-arguments form); anthropic's tool_use.input
        // is the object. A string that does not parse is a degraded
        // upstream — the empty object keeps the shape valid rather
        // than inventing content.
        let input: Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
        content.push(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": input,
        }));
    }

    let mut message = Map::new();
    message.insert(
        "id".to_owned(),
        json!(turn.turn_id.as_deref().unwrap_or(UNNAMED_MESSAGE)),
    );
    message.insert("type".to_owned(), json!("message"));
    message.insert("role".to_owned(), json!("assistant"));
    message.insert("model".to_owned(), json!(model));
    message.insert("content".to_owned(), Value::Array(content));
    message.insert(
        "stop_reason".to_owned(),
        json!(stop_reason_of(&turn.stop_reason)),
    );
    message.insert("stop_sequence".to_owned(), Value::Null);
    if let Some(usage) = &turn.usage {
        message.insert("usage".to_owned(), usage_json(usage));
    }
    Value::Object(message)
}

// ── the response direction: the rendering helpers ───────────────────

/// The canonical stop reason → anthropic's: the natural ends and the
/// budget carry directly; an unknown incompleteness stops at the
/// budget — `max_tokens`, the only budget-shaped anthropic stop
/// reason.
fn stop_reason_of(reason: &CanonStopReason) -> &'static str {
    match reason {
        CanonStopReason::EndTurn => "end_turn",
        CanonStopReason::ToolUse => "tool_use",
        CanonStopReason::MaxTokens => "max_tokens",
        CanonStopReason::Refusal => "refusal",
        CanonStopReason::Incomplete(_) => "max_tokens",
    }
}

/// The canonical usage buckets → the anthropic field names (the usage
/// table). Absent stays absent (invariant 3); reasoning tokens ride
/// inside `output_tokens` on both protocols and are not broken out
/// (the pair module's translation note).
fn usage_json(usage: &CanonicalUsage) -> Value {
    let mut map = Map::new();
    if let Some(input) = usage.input {
        map.insert("input_tokens".to_owned(), json!(input));
    }
    if let Some(cache_write) = usage.cache_write {
        map.insert("cache_creation_input_tokens".to_owned(), json!(cache_write));
    }
    if let Some(cache_read) = usage.cache_read {
        map.insert("cache_read_input_tokens".to_owned(), json!(cache_read));
    }
    if let Some(output) = usage.output {
        map.insert("output_tokens".to_owned(), json!(output));
    }
    Value::Object(map)
}

/// A canonical error → the anthropic error event's data JSON (the
/// error table's rendering half): the kind → the type, the message
/// verbatim (already resolved backend-side), `retry_after` only for
/// rate limits.
pub fn anthropic_error_event_data(error: &CanonError) -> Value {
    let kind = anthropic_error_type(error);
    let mut error_object = Map::new();
    error_object.insert("type".to_owned(), json!(kind));
    error_object.insert("message".to_owned(), json!(error.message));
    if matches!(error.kind, CanonErrorKind::RateLimit)
        && let Some(resets_at) = error.resets_at
    {
        // The absolute reset epoch, verbatim — a relative
        // retry-after would need a clock, and this is pure.
        error_object.insert("retry_after".to_owned(), json!(resets_at));
    }
    json!({"type": "error", "error": Value::Object(error_object)})
}

/// The error table's rendering half: the canonical kind → anthropic's
/// `error.type`. The interpretation half (the upstream's `code`
/// then `kind` → the canonical kind) lives in the backend adapter.
pub fn anthropic_error_type(error: &CanonError) -> &'static str {
    match error.kind {
        CanonErrorKind::RateLimit => "rate_limit_error",
        CanonErrorKind::InvalidRequest => "invalid_request_error",
        CanonErrorKind::Authentication => "authentication_error",
        CanonErrorKind::Permission => "permission_error",
        CanonErrorKind::NotFound => "not_found_error",
        CanonErrorKind::TooLarge => "request_too_large",
        CanonErrorKind::Overloaded => "overloaded_error",
        CanonErrorKind::Api => "api_error",
    }
}

/// One emitted Anthropic SSE event: the event name on its `event:`
/// line's value, the JSON on its single `data:` line.
fn sse_event(name: &str, data: Value) -> SseEvent {
    SseEvent {
        data_lines: vec![serde_json::to_string(&data).expect("a built Value always serialises")],
        event: Some(name.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::TranslateError;
    use super::anthropic_from_canonical;
    use super::from_anthropic;
    use super::{AnthropicRenderer, anthropic_error_event_data, anthropic_error_type};
    use crate::ir::canonical::{
        CanonBlock, CanonError, CanonErrorKind, CanonEvent, CanonMessage, CanonRole,
        CanonStopReason, CanonSystemPart, CanonTool, CanonToolCall, CanonToolChoice, CanonTurn,
        CanonicalExtension, CanonicalUsage, SamplingSpec, ThinkingSpec, ToolResultContent,
    };
    use crate::observe::sse::SseEvent;
    use crate::routing::DialectId;
    use serde_json::{Value, json};

    // ── what the pair module used to decide early, now carried ────

    #[test]
    fn unmodeled_fields_become_opaque_extensions_without_debug_content() {
        let body = json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": "Hi"}],
            "metadata": {"user_id": "secret-user"},
            "top_k": 17,
        });
        let canonical = from_anthropic(&body).expect("parses");
        assert_eq!(canonical.extensions.len(), 2);
        assert_eq!(
            canonical.extensions[0].source(),
            DialectId::AnthropicMessages
        );
        assert_eq!(canonical.extensions[0].wire_path(), "$.metadata");
        assert_eq!(canonical.extensions[0].wire_name(), Some("metadata"));
        assert_eq!(canonical.extensions[0].value(), &body["metadata"]);
        assert_eq!(canonical.extensions[1].wire_path(), "$.top_k");

        let debug = format!("{:?}", canonical.extensions[0]);
        assert!(debug.contains("<opaque>"));
        assert!(!debug.contains("secret-user"));
    }

    #[test]
    fn message_block_metadata_stays_attached_and_replays_exactly() {
        let source = json!([
            {
                "type": "text",
                "text": "cached prompt",
                "cache_control": {"type": "ephemeral", "secret": "cache-value"}
            },
            {
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": "failed",
                "is_error": true,
                "future_field": {"secret": "future-value"}
            }
        ]);
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": source.clone()}],
        });
        let canonical = from_anthropic(&body).expect("parses");

        assert_eq!(
            canonical.messages[0]
                .blocks
                .iter()
                .map(CanonBlock::wire_value)
                .collect::<Vec<_>>(),
            source.as_array().expect("source is an array").clone()
        );
        assert!(matches!(
            &canonical.messages[0].blocks[1],
            CanonBlock::Annotated {
                is_error: Some(true),
                extensions,
                ..
            } if extensions.len() == 1
                && extensions[0].wire_name() == Some("future_field")
        ));
        let debug = format!("{:?}", canonical.messages[0].blocks);
        assert!(!debug.contains("cache-value"));
        assert!(!debug.contains("future-value"));
    }

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
        assert_eq!(canonical.model.as_deref(), Some("m"));
        assert_eq!(
            canonical.system,
            vec![CanonSystemPart::text("One."), CanonSystemPart::text("Two.")],
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
        assert_eq!(canonical.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(
            canonical.messages[0].blocks,
            vec![
                CanonBlock::Thinking {
                    text: "secret reasoning".to_owned(),
                    signature: Some("sig-1".to_owned()),
                },
                CanonBlock::RedactedThinking {
                    data: "opaque-blob".to_owned(),
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
        // Text remains semantic, cache metadata stays attached to its node,
        // and an element with no text remains opaque rather than turning into
        // invented empty text. The join is backend policy.
        assert_eq!(
            from_anthropic(&join).expect("parses").system,
            vec![
                CanonSystemPart::Text {
                    text: "One.".to_owned(),
                    extensions: vec![CanonicalExtension::node_field(
                        DialectId::AnthropicMessages,
                        "$.system[].cache_control",
                        "cache_control",
                        json!({"type": "ephemeral"}),
                    )],
                },
                CanonSystemPart::text("bare string element"),
                CanonSystemPart::Opaque(CanonicalExtension::new(
                    DialectId::AnthropicMessages,
                    "$.system[]",
                    json!({"no_text": true}),
                )),
            ]
        );

        let absent = json!({"model": "m", "messages": [{"role": "user", "content": "Hi"}]});
        assert_eq!(
            from_anthropic(&absent).expect("parses").system,
            Vec::<CanonSystemPart>::new()
        );

        let null = json!({"model": "m", "system": null,
                          "messages": [{"role": "user", "content": "Hi"}]});
        assert_eq!(
            from_anthropic(&null).expect("parses").system,
            Vec::<CanonSystemPart>::new()
        );

        let string = json!({"model": "m", "system": "One prompt.",
                            "messages": [{"role": "user", "content": "Hi"}]});
        assert_eq!(
            from_anthropic(&string).expect("parses").system,
            vec![CanonSystemPart::text("One prompt.")]
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
        // Modeled block metadata stays node-local and the typed path can now
        // reproduce it exactly. Unknown block kinds still take the raw-JSON
        // string path.
        let extra = from_anthropic(&body(json!([{"type": "text", "text": "kept",
                         "cache_control": {"type": "ephemeral"}}])))
        .expect("parses");
        assert_eq!(
            extra.messages[0].blocks[0],
            CanonBlock::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: ToolResultContent::Blocks(vec![
                    CanonBlock::Text("kept".to_owned()).annotated(
                        None,
                        vec![CanonicalExtension::node_field(
                            DialectId::AnthropicMessages,
                            "$.messages[].content[].cache_control",
                            "cache_control",
                            json!({"type": "ephemeral"}),
                        )],
                    )
                ]),
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
                {"name": "read_file", "input_schema": {"type": "object"},
                 "cache_control": {"type": "ephemeral"}},
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
                    extensions: vec![CanonicalExtension::node_field(
                        DialectId::AnthropicMessages,
                        "$.tools[].cache_control",
                        "cache_control",
                        json!({"type": "ephemeral"}),
                    )],
                },
                CanonTool {
                    name: "list_dir".to_owned(),
                    description: "List a directory".to_owned(),
                    parameters: json!({"type": "object", "properties": {}}),
                    extensions: Vec::new(),
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

    // ── the response direction: rendering ─────────────────────────
    //
    // The renderer's tests feed HAND-BUILT canon events — the
    // interpretation that produces them is the backend adapter's,
    // tested there; the end-to-end bytes (fixture in, anthropic SSE
    // out) are the composition's, tested in to_anthropic.

    /// The SSE wire bytes of one emitted event: the `event:` line, the
    /// `data:` line, the blank separator.
    fn render_one(event: &SseEvent) -> String {
        let mut out = String::new();
        out.push_str("event: ");
        out.push_str(event.event.as_deref().unwrap_or(""));
        out.push('\n');
        for line in &event.data_lines {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        out
    }

    /// The expected wire bytes of an (event name, data JSON) sequence.
    fn wire(pairs: &[(&str, &str)]) -> String {
        pairs
            .iter()
            .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
            .collect()
    }

    /// Feed every canon event through one fresh renderer; the
    /// rendered wire bytes.
    fn stream_bytes(model: &str, events: &[CanonEvent]) -> String {
        let mut renderer = AnthropicRenderer::new(model);
        let mut out = String::new();
        for event in events {
            for emitted in renderer.feed(event) {
                out.push_str(&render_one(&emitted));
            }
        }
        out
    }

    /// The start of message_start's message object, for the pins.
    fn message_start(id: &str, model: &str) -> String {
        format!(
            r#"{{"type":"message_start","message":{{"id":"{id}","type":"message","role":"assistant","model":"{model}","content":[],"usage":{{"input_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0}}}}}}"#
        )
    }

    #[test]
    fn text_after_tool_use_takes_the_next_index() {
        let events = vec![
            CanonEvent::TurnStarted {
                turn_id: Some("resp_1".to_owned()),
            },
            CanonEvent::TextDelta {
                delta: "Part one.".to_owned(),
            },
            CanonEvent::TextEnded,
            CanonEvent::ToolCall(CanonToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"a":1}"#.to_owned(),
            }),
            CanonEvent::TextDelta {
                delta: "Part two.".to_owned(),
            },
            CanonEvent::TextEnded,
            CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::ToolUse,
                usage: None,
            },
        ];
        let expected = wire(&[
            ("message_start", &message_start("resp_1", "claude-opus-5")),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Part one."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"read_file","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Part two."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":2}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        assert_eq!(stream_bytes("claude-opus-5", &events), expected);
    }

    #[test]
    fn thinking_after_text_takes_the_next_index() {
        let events = vec![
            CanonEvent::TurnStarted {
                turn_id: Some("resp_1".to_owned()),
            },
            CanonEvent::TextDelta {
                delta: "First.".to_owned(),
            },
            CanonEvent::ThinkingDelta {
                part: 0,
                delta: "Thought.".to_owned(),
            },
            CanonEvent::TextDelta {
                delta: "Last.".to_owned(),
            },
            CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::EndTurn,
                usage: None,
            },
        ];
        let expected = wire(&[
            ("message_start", &message_start("resp_1", "claude-opus-5")),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"First."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"Thought."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Last."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":2}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        assert_eq!(stream_bytes("claude-opus-5", &events), expected);
    }

    #[test]
    fn summary_indexes_open_separate_thinking_blocks() {
        let events = vec![
            CanonEvent::TurnStarted {
                turn_id: Some("resp_1".to_owned()),
            },
            CanonEvent::ThinkingDelta {
                part: 0,
                delta: "A".to_owned(),
            },
            CanonEvent::ThinkingDelta {
                part: 0,
                delta: "B".to_owned(),
            },
            CanonEvent::ThinkingDelta {
                part: 1,
                delta: "C".to_owned(),
            },
            CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::EndTurn,
                usage: None,
            },
        ];
        let expected = wire(&[
            ("message_start", &message_start("resp_1", "claude-opus-5")),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"A"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"B"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"C"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        assert_eq!(stream_bytes("claude-opus-5", &events), expected);
    }

    #[test]
    fn a_completed_reasoning_part_closes_only_a_thinking_block() {
        // ThinkingEnded closes the open thinking block; a TEXT block
        // under a reasoning completion stays open (the pair module's
        // reasoning-item done did the same).
        let events = vec![
            CanonEvent::TurnStarted {
                turn_id: Some("resp_1".to_owned()),
            },
            CanonEvent::ThinkingDelta {
                part: 0,
                delta: "thought".to_owned(),
            },
            CanonEvent::ThinkingEnded,
            CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::EndTurn,
                usage: None,
            },
        ];
        let expected = wire(&[
            ("message_start", &message_start("resp_1", "claude-opus-5")),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"thought"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        assert_eq!(stream_bytes("claude-opus-5", &events), expected);

        let events = vec![
            CanonEvent::TurnStarted { turn_id: None },
            CanonEvent::TextDelta {
                delta: "text".to_owned(),
            },
            CanonEvent::ThinkingEnded,
            CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::EndTurn,
                usage: None,
            },
        ];
        let expected = wire(&[
            ("message_start", &message_start("msg", "claude-opus-5")),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"text"}}"#,
            ),
            // No content_block_stop: a reasoning completion does not
            // close a text block; the turn's end does.
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        assert_eq!(stream_bytes("claude-opus-5", &events), expected);
    }

    #[test]
    fn a_completed_text_part_closes_whatever_block_was_open() {
        // TextEnded closes unconditionally — the pair module's
        // message-item done closed any open block, and this keeps
        // that, even the exotic thinking-block-open-under-a-text-end.
        let events = vec![
            CanonEvent::TurnStarted { turn_id: None },
            CanonEvent::ThinkingDelta {
                part: 3,
                delta: "orphaned".to_owned(),
            },
            CanonEvent::TextEnded,
            CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::EndTurn,
                usage: None,
            },
        ];
        let expected = wire(&[
            ("message_start", &message_start("msg", "claude-opus-5")),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"orphaned"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        assert_eq!(stream_bytes("claude-opus-5", &events), expected);
    }

    #[test]
    fn a_mid_stream_error_does_not_close_the_open_block() {
        // toker's own captured fixture 06 is the shape: anthropic
        // error events cut the stream mid-block, no
        // content_block_stop, no message_delta, no message_stop.
        let events = vec![
            CanonEvent::TurnStarted {
                turn_id: Some("resp_1".to_owned()),
            },
            CanonEvent::TextDelta {
                delta: "half a reply".to_owned(),
            },
            CanonEvent::Error {
                error: CanonError {
                    kind: CanonErrorKind::Overloaded,
                    message: "Overloaded".to_owned(),
                    resets_at: None,
                },
            },
        ];
        let expected = wire(&[
            ("message_start", &message_start("resp_1", "claude-opus-5")),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half a reply"}}"#,
            ),
            (
                "error",
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            ),
        ]);
        let mut renderer = AnthropicRenderer::new("claude-opus-5");
        let mut rendered = String::new();
        for event in &events {
            for emitted in renderer.feed(event) {
                rendered.push_str(&render_one(&emitted));
            }
        }
        assert_eq!(rendered, expected);
        assert!(
            !renderer.turn_ended(),
            "a mid-turn error is not the turn's end"
        );

        // TurnFailed IS the turn's end.
        let mut renderer = AnthropicRenderer::new("claude-opus-5");
        renderer.feed(&CanonEvent::TurnFailed {
            error: CanonError {
                kind: CanonErrorKind::Api,
                message: "upstream error".to_owned(),
                resets_at: None,
            },
        });
        assert!(renderer.turn_ended());
    }

    #[test]
    fn message_start_renders_exactly_once_and_the_id_is_never_invented() {
        // The turn's id latches from the FIRST TurnStarted; a later
        // one changes nothing. A turn that never named one keeps the
        // constant placeholder — never a minted id (purity).
        let mut renderer = AnthropicRenderer::new("claude-opus-5");
        let emitted = renderer.feed(&CanonEvent::TurnStarted {
            turn_id: Some("resp_1".to_owned()),
        });
        assert_eq!(emitted.len(), 1);
        let again = renderer.feed(&CanonEvent::TurnStarted {
            turn_id: Some("resp_2".to_owned()),
        });
        assert!(again.is_empty(), "message_start is emitted exactly once");
        let data = emitted[0].data();
        assert!(
            data.contains(r#""id":"resp_1""#),
            "the first id latches: {data}"
        );

        let mut renderer = AnthropicRenderer::new("claude-opus-5");
        let emitted = renderer.feed(&CanonEvent::TurnStarted { turn_id: None });
        assert!(emitted[0].data().contains(r#""id":"msg""#));
    }

    #[test]
    fn the_canonical_stop_reasons_render_to_the_anthropic_names() {
        for (reason, stop) in [
            (CanonStopReason::EndTurn, "end_turn"),
            (CanonStopReason::ToolUse, "tool_use"),
            (CanonStopReason::MaxTokens, "max_tokens"),
            (CanonStopReason::Refusal, "refusal"),
            (CanonStopReason::Incomplete(String::new()), "max_tokens"),
            (
                CanonStopReason::Incomplete("something_new".to_owned()),
                "max_tokens",
            ),
        ] {
            let rendered = stream_bytes(
                "claude-opus-5",
                &[CanonEvent::TurnEnded {
                    stop_reason: reason.clone(),
                    usage: None,
                }],
            );
            assert!(
                rendered.contains(&format!(r#""stop_reason":"{stop}""#)),
                "{reason:?} renders {stop}: {rendered}"
            );
        }
    }

    #[test]
    fn the_canonical_usage_renders_the_anthropic_fields_in_order() {
        // The buckets → the anthropic field names, in the table's
        // order; absent stays absent (invariant 3); the reasoning
        // bucket rides inside output_tokens and is never broken out.
        let usage = CanonicalUsage {
            input: Some(1234),
            cache_read: Some(512),
            cache_write: Some(64),
            output: Some(210),
            reasoning: Some(96),
            raw: json!({"input_tokens": 1234}),
        };
        let rendered = stream_bytes(
            "claude-opus-5",
            &[CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::EndTurn,
                usage: Some(usage),
            }],
        );
        assert!(rendered.contains(
            r#""usage":{"input_tokens":1234,"cache_creation_input_tokens":64,"cache_read_input_tokens":512,"output_tokens":210}"#
        ));

        // A usage without the details omits the cache keys entirely.
        let partial = CanonicalUsage {
            input: Some(10),
            cache_read: None,
            cache_write: None,
            output: Some(5),
            reasoning: None,
            raw: json!({}),
        };
        let rendered = stream_bytes(
            "claude-opus-5",
            &[CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::EndTurn,
                usage: Some(partial),
            }],
        );
        assert!(
            rendered.contains(r#""usage":{"input_tokens":10,"output_tokens":5}"#),
            "absent details are omitted, never zeroed: {rendered}"
        );
    }

    #[test]
    fn the_error_table_renders_canon_errors_per_kind() {
        let error_event = |error: CanonError| {
            let mut renderer = AnthropicRenderer::new("claude-opus-5");
            let emitted = renderer.feed(&CanonEvent::Error { error });
            assert_eq!(emitted.len(), 1);
            let event = &emitted[0];
            assert_eq!(event.event.as_deref(), Some("error"));
            serde_json::from_str::<Value>(&event.data()).expect("data is JSON")
        };
        let canon = |kind: CanonErrorKind, message: &str, resets_at: Option<i64>| CanonError {
            kind,
            message: message.to_owned(),
            resets_at,
        };

        // Every kind renders its anthropic type name.
        for (kind, type_name) in [
            (CanonErrorKind::RateLimit, "rate_limit_error"),
            (CanonErrorKind::InvalidRequest, "invalid_request_error"),
            (CanonErrorKind::Authentication, "authentication_error"),
            (CanonErrorKind::Permission, "permission_error"),
            (CanonErrorKind::NotFound, "not_found_error"),
            (CanonErrorKind::TooLarge, "request_too_large"),
            (CanonErrorKind::Overloaded, "overloaded_error"),
            (CanonErrorKind::Api, "api_error"),
        ] {
            assert_eq!(anthropic_error_type(&canon(kind, "m", None)), type_name);
        }
        // retry_after rides ONLY for rate limits, and only when the
        // reset was carried — the absolute epoch, verbatim.
        assert_eq!(
            error_event(canon(
                CanonErrorKind::RateLimit,
                "Rate limit reached.",
                Some(1_800_000_900)
            )),
            json!({"type": "error", "error": {
                "type": "rate_limit_error",
                "message": "Rate limit reached.",
                "retry_after": 1_800_000_900,
            }})
        );
        assert_eq!(
            error_event(canon(CanonErrorKind::RateLimit, "m", None)),
            json!({"type": "error", "error": {
                "type": "rate_limit_error",
                "message": "m",
            }}),
            "no reset time, no retry_after"
        );
        assert_eq!(
            error_event(canon(CanonErrorKind::Api, "m", Some(5))),
            json!({"type": "error", "error": {
                "type": "api_error",
                "message": "m",
            }}),
            "a non-rate-limit never carries retry_after"
        );
        // The message renders verbatim — the backend resolved any
        // stand-in chain before the canonical boundary.
        assert_eq!(
            anthropic_error_event_data(&canon(CanonErrorKind::Api, "server_error", None)),
            json!({"type": "error", "error": {"type": "api_error", "message": "server_error"}})
        );
    }

    #[test]
    fn the_canonical_turn_renders_the_complete_message() {
        // The categorical content order: thinking parts (by the
        // backend's part identity), text, the tool calls (in
        // completion order); usage when the turn reported one.
        let turn = CanonTurn {
            turn_id: Some("resp_1".to_owned()),
            stop_reason: CanonStopReason::ToolUse,
            usage: Some(CanonicalUsage {
                input: Some(1234),
                cache_read: Some(512),
                cache_write: Some(64),
                output: Some(210),
                reasoning: Some(96),
                raw: json!({}),
            }),
            error: None,
            tool_calls: vec![CanonToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"src/main.rs"}"#.to_owned(),
            }],
            text: "I'll read the files.".to_owned(),
            thinking: [
                (0, "Reading the thread files.".to_owned()),
                (1, "More thought.".to_owned()),
            ]
            .into(),
        };
        assert_eq!(
            serde_json::to_string(&anthropic_from_canonical("claude-opus-5", &turn))
                .expect("serialise"),
            concat!(
                r#"{"id":"resp_1","type":"message","role":"assistant","model":"claude-opus-5","#,
                r#""content":[{"type":"thinking","thinking":"Reading the thread files."},"#,
                r#"{"type":"thinking","thinking":"More thought."},"#,
                r#"{"type":"text","text":"I'll read the files."},"#,
                r#"{"type":"tool_use","id":"call_1","name":"read_file","input":{"path":"src/main.rs"}}],"#,
                r#""stop_reason":"tool_use","stop_sequence":null,"#,
                r#""usage":{"input_tokens":1234,"cache_creation_input_tokens":64,"cache_read_input_tokens":512,"output_tokens":210}}"#
            ),
            "byte-pinned non-streaming message"
        );
    }

    #[test]
    fn unparseable_arguments_aggregate_to_the_empty_input_object() {
        let turn = CanonTurn {
            turn_id: None,
            stop_reason: CanonStopReason::ToolUse,
            usage: None,
            error: None,
            tool_calls: vec![CanonToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "not json at all".to_owned(),
            }],
            text: String::new(),
            thinking: BTreeMap::new(),
        };
        let message = anthropic_from_canonical("claude-opus-5", &turn);
        assert_eq!(
            message["content"],
            json!([{"type": "tool_use", "id": "call_1", "name": "read_file", "input": {}}])
        );
        assert_eq!(message["stop_reason"], json!("tool_use"));
    }

    #[test]
    fn an_errored_canonical_turn_renders_the_error_body() {
        let turn = CanonTurn {
            turn_id: Some("resp_fail".to_owned()),
            stop_reason: CanonStopReason::EndTurn,
            usage: None,
            error: Some(CanonError {
                kind: CanonErrorKind::Api,
                message: "Upstream overloaded.".to_owned(),
                resets_at: None,
            }),
            tool_calls: Vec::new(),
            text: "partial".to_owned(),
            thinking: BTreeMap::new(),
        };
        // The error body carries no content: anthropic's
        // non-streaming errors carry none.
        assert_eq!(
            anthropic_from_canonical("claude-opus-5", &turn),
            json!({"type": "error",
                   "error": {"type": "api_error", "message": "Upstream overloaded."}})
        );
    }

    #[test]
    fn an_empty_canonical_turn_renders_the_empty_message() {
        // No turn facts at all: the placeholder id (purity forbids
        // minting one), no content, end_turn, no usage key.
        let turn = CanonTurn {
            turn_id: None,
            stop_reason: CanonStopReason::EndTurn,
            usage: None,
            error: None,
            tool_calls: Vec::new(),
            text: String::new(),
            thinking: BTreeMap::new(),
        };
        assert_eq!(
            anthropic_from_canonical("claude-opus-5", &turn),
            json!({
                "id": "msg",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-5",
                "content": [],
                "stop_reason": "end_turn",
                "stop_sequence": null,
            })
        );
    }

    #[test]
    fn rendering_is_pure() {
        let events = [
            CanonEvent::TurnStarted {
                turn_id: Some("resp_1".to_owned()),
            },
            CanonEvent::ThinkingDelta {
                part: 0,
                delta: "thought".to_owned(),
            },
            CanonEvent::TextDelta {
                delta: "text".to_owned(),
            },
            CanonEvent::ToolCall(CanonToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{}".to_owned(),
            }),
            CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::ToolUse,
                usage: Some(CanonicalUsage {
                    input: Some(1),
                    cache_read: None,
                    cache_write: None,
                    output: Some(2),
                    reasoning: None,
                    raw: json!({}),
                }),
            },
        ];
        let first = stream_bytes("claude-opus-5", &events);
        for _ in 0..3 {
            assert_eq!(stream_bytes("claude-opus-5", &events), first);
        }
    }
}
