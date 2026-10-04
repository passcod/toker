//! The backend adapter: one [`CanonicalRequest`] → one codex
//! [`ResponsesRequest`] (unit A's wire types).
//!
//! Everything this module drops or reshapes is a fact about the
//! **codex backend** — verified against it, not assumed — declared
//! in
//! [`Capabilities::CODEX`](crate::ir::canonical::Capabilities::CODEX)
//! and enforced here, so it can never leak into another backend's
//! adapter: adding a backend is THIS file's shape, not a new pair.
//!
//! # This backend's costs (live-verified, per capability)
//!
//! | Cost (capability `false`) | Why | What the upstream *does* support |
//! | --- | --- | --- |
//! | thinking-block replay (`thinking_replay`) | forced: cross-provider reasoning is opaque — claude's blocks carry no `encrypted_content`, so they cannot ride the codex wire | reasoning itself: summaries and reasoning items, its own `encrypted_content` — the thinking **request** maps to the reasoning effort ([`effort_of`]) |
//! | sampling — `temperature`, `top_p`, `max_tokens`, `stop_sequences` (`sampling`) | refused: verified live ("Unsupported parameter: temperature"); the codex client sends none | its own defaults, echoed in every response |
//! | system-role input items (`system_in_messages`) | refused: verified live ("System messages are not allowed") | system content via `instructions` (leading) and the preceding-user merge (mid-conversation, ctp's pattern) |
//!
//! A future backend whose upstream supports these keeps them in ITS
//! adapter; the drops above are THIS backend's costs, not toker
//! policy. Conversely, what the upstream supports is always kept:
//! the thinking request maps to the reasoning effort, tools map,
//! images map. `stream` is read for nothing here — this backend's
//! turns always stream (a wire constant of unit A's constructor).
//!
//! The canonical is typed, so this adapter cannot fail on content —
//! its only [`TranslateError`] is a tool-choice intent this wire
//! cannot express (reported with the pair module's exact reason,
//! byte-identical, because the string rides the client error body).

use serde_json::{Value, json};

use crate::ir::canonical::{
    CanonBlock, CanonRole, CanonTool, CanonToolChoice, CanonicalRequest, ThinkingSpec,
};
use crate::providers::codex::{Item, ResponsesRequest, Tool};
use crate::translate::TranslateError;

/// Render one canonical request onto the codex wire.
///
/// `model` is the codex model slug to request and
/// `prompt_cache_key` the cache/session identity — both
/// CALLER-derived, both explicit parameters (purity: the canonical
/// carries the request, never the routing facts). The same canonical
/// and the same params always produce the same bytes, forever
/// (invariant 4), which is what keeps an appended conversation turn
/// byte-identical over its earlier input items (invariant 5 — the
/// prefix-stability property test pins it).
pub fn codex_from_canonical(
    canonical: &CanonicalRequest,
    model: &str,
    prompt_cache_key: &str,
) -> Result<ResponsesRequest, TranslateError> {
    let mut request = ResponsesRequest::new(model, prompt_cache_key);
    let (input, leading_system) = input_of(canonical);
    // The system prompt is the pieces joined on blank lines — THIS
    // backend's `instructions` form — plus any system-role message
    // texts that had no user turn to merge into (below).
    let mut instructions = canonical.system.join("\n\n");
    for text in leading_system {
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(&text);
    }
    request.instructions = instructions;
    request.input = input;
    request.tools = canonical.tools.iter().map(tool_of).collect();
    request.tool_choice = tool_choice_of(&canonical.tool_choice)?;
    // Sampling does not cross: [`Capabilities::CODEX`].sampling is
    // false — the backend refuses the parameters outright
    // (live-verified: "Unsupported parameter: temperature") and its
    // client never sends any. The carried specs stop here; nothing
    // is emitted, and `extra` stays empty. The drop is this
    // backend's declared cost, pinned by test — canonical in with
    // sampling, request out without.
    //
    // `stream` rides the canonical for backends that distinguish;
    // this backend's turns always stream (the wire constant), so it
    // is read for nothing here either.
    request.reasoning.effort = canonical.thinking.as_ref().map(effort_of);
    Ok(request)
}

// ── messages → input items ─────────────────────────────────────────

/// The canonical messages → the input item list, in order, plus any
/// LEADING system-role message texts. Grouping rule: consecutive
/// text/image blocks of one message become ONE message item;
/// `tool_use`/`tool_result` become their own items, and text after
/// them opens a new message item (order preserved at item
/// granularity).
///
/// Mid-conversation system messages (claude Code's reminders) merge
/// into the PRECEDING user turn's item as `[PROMPT_INJECTION]`-prefixed
/// text parts — ctp's exact transform for the same problem (sonnet 5
/// refuses system entries in `messages[]`; the codex backend refuses
/// system-role input items — live-verified: "System messages are not
/// allowed", the `system_in_messages: false` capability). With no
/// preceding user item, the text falls back to the leading set
/// (instructions) — never dropped, never a system item.
fn input_of(canonical: &CanonicalRequest) -> (Vec<Item>, Vec<String>) {
    let mut items: Vec<Item> = Vec::with_capacity(canonical.messages.len());
    let mut leading_system: Vec<String> = Vec::new();
    for message in &canonical.messages {
        match message.role {
            CanonRole::System => {
                for block in &message.blocks {
                    // The frontend's system parse yields text blocks
                    // only; the canonical is typed, so anything else
                    // cannot occur — and skipping keeps this total.
                    let CanonBlock::Text(text) = block else {
                        continue;
                    };
                    if let Some(user_item) = last_user_item_mut(&mut items) {
                        user_item
                            .0
                            .get_mut("content")
                            .and_then(Value::as_array_mut)
                            .expect("a message item's content is an array")
                            .push(text_part(&format!("[PROMPT_INJECTION] {text}"), false));
                    } else {
                        leading_system.push(text.clone());
                    }
                }
            }
            role => {
                let role = wire_role(role);
                let mut parts: Vec<Value> = Vec::new();
                for block in &message.blocks {
                    match block {
                        CanonBlock::Text(text) => {
                            parts.push(text_part(text, role == "assistant"));
                        }
                        CanonBlock::Image { url } => {
                            parts.push(json!({"type": "input_image", "image_url": url}));
                        }
                        CanonBlock::ToolUse { id, name, input } => {
                            let arguments = serde_json::to_string(input)
                                .expect("a parsed Value always serialises");
                            items.extend(flush(&mut parts, role));
                            items.push(Item::function_call(name, &arguments, id));
                        }
                        CanonBlock::ToolResult {
                            tool_use_id,
                            content,
                        } => {
                            items.extend(flush(&mut parts, role));
                            items.push(Item::function_call_output(
                                tool_use_id,
                                &content.output_text(),
                            ));
                        }
                        // DROPPED, loudly — this backend's declared
                        // cost: [`Capabilities::CODEX`].
                        // thinking_replay is false (cross-provider
                        // reasoning is opaque; see the module docs).
                        // The drop does not split the message's parts.
                        CanonBlock::Thinking { .. } => {}
                    }
                }
                items.extend(flush(&mut parts, role));
            }
        }
    }
    (items, leading_system)
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

// ── tools, tool choice, thinking ────────────────────────────────────

/// One canonical tool → the flat Responses function shape
/// (`strict: false` — the codex client's own setting; the schema
/// crosses verbatim).
fn tool_of(tool: &CanonTool) -> Tool {
    Tool::function(
        &tool.name,
        &tool.description,
        false,
        tool.parameters.clone(),
    )
}

/// The canonical tool-choice intent → the wire's string form:
/// [`CanonToolChoice::Auto`] is the wire constant `auto` (absence IS
/// auto), `any` is the Responses `required`. A forced tool or an
/// unknown type has no faithful string form on THIS wire — reported,
/// not guessed, with the pair module's exact reason (the string
/// rides the client error body).
fn tool_choice_of(choice: &CanonToolChoice) -> Result<String, TranslateError> {
    match choice {
        CanonToolChoice::Auto => Ok("auto".to_owned()),
        CanonToolChoice::Any => Ok("required".to_owned()),
        CanonToolChoice::Tool { .. } => Err(TranslateError::Malformed {
            reason: format!("tool_choice type {:?} has no responses equivalent", "tool"),
        }),
        CanonToolChoice::Other { kind } => Err(TranslateError::Malformed {
            reason: format!("tool_choice type {kind:?} has no responses equivalent"),
        }),
    }
}

/// The thinking intent → the codex reasoning effort, the upstream's
/// own knob for the same intent (it SUPPORTS reasoning — only the
/// REPLAY of thinking blocks is protocol-forced out; see the module
/// docs). Budget tiers follow claude's own ladder (1024 floor, 10 k
/// standard, 32 k extended): below 16 k → `low`, below 32 k →
/// `medium`, otherwise `high` — a deliberate, documented policy
/// translation, not a silent default. `None` stays `None`: the
/// model's own default effort then governs (the codex catalog's
/// `default_reasoning_level`), which is exactly what "the client did
/// not ask" means.
fn effort_of(thinking: &ThinkingSpec) -> String {
    match thinking.budget_tokens {
        0..=16_383 => "low",
        16_384..=32_767 => "medium",
        _ => "high",
    }
    .to_owned()
}

// ── shared ──────────────────────────────────────────────────────────

/// A canonical dialogue role → the wire's role string.
fn wire_role(role: CanonRole) -> &'static str {
    match role {
        CanonRole::User => "user",
        CanonRole::Assistant => "assistant",
        CanonRole::System => "system",
    }
}

#[cfg(test)]
mod tests {
    use super::super::TranslateError;
    use super::super::codex_backend::codex_from_canonical;
    use crate::ir::canonical::{
        CanonBlock, CanonMessage, CanonRole, CanonTool, CanonToolChoice, CanonicalRequest,
        Capabilities, SamplingSpec, ThinkingSpec, ToolResultContent,
    };
    use serde_json::{Value, json};

    const MODEL: &str = "gpt-5.2-codex";
    const KEY: &str = "unit-cache-key";

    fn item_of(request: &super::super::codex_backend::ResponsesRequest, index: usize) -> Value {
        serde_json::to_value(&request.input[index]).expect("serialise item")
    }

    fn tool_of(request: &super::super::codex_backend::ResponsesRequest, index: usize) -> Value {
        serde_json::to_value(&request.tools[index]).expect("serialise tool")
    }

    fn message(role: CanonRole, blocks: Vec<CanonBlock>) -> CanonMessage {
        CanonMessage { role, blocks }
    }

    // ── the drops are THIS backend's doing ──────────────────────

    #[test]
    fn canonical_sampling_stops_at_this_backend() {
        // The capability declaration the drop enforces: the codex
        // backend refuses sampling outright (live-verified:
        // "Unsupported parameter: temperature").
        let caps = Capabilities::CODEX;
        assert!(!caps.sampling);
        let canonical = CanonicalRequest {
            sampling: SamplingSpec {
                temperature: Some(0.3),
                top_p: Some(0.95),
                max_tokens: Some(4096),
                stop_sequences: Some(vec!["\n\nHuman:".to_owned()]),
            },
            thinking: Some(ThinkingSpec {
                budget_tokens: 2048,
            }),
            messages: vec![message(
                CanonRole::User,
                vec![CanonBlock::Text("Hi".to_owned())],
            )],
            ..CanonicalRequest::default()
        };
        let request = codex_from_canonical(&canonical, MODEL, KEY).expect("renders");
        assert!(request.extra.is_empty(), "no sampling fields cross");
        // None of the carried fields ride along anywhere in the
        // request — loudly absent from the wire, documented in the
        // module docs.
        let bytes = serde_json::to_string(&request).expect("serialise");
        assert!(!bytes.contains("max_output_tokens"));
        assert!(!bytes.contains("temperature"));
        assert!(!bytes.contains("top_p"));
        assert!(!bytes.contains("stop_sequences"));
        // The thinking budget crossed as the effort, not as the
        // budget: the wire has its own knob-speak.
        assert!(!bytes.contains("budget_tokens"));
        assert!(!bytes.contains("4096"));
        assert_eq!(request.reasoning.effort.as_deref(), Some("low"));
        // And the stream flag is read for nothing: this backend's
        // turns always stream.
        let streamed = CanonicalRequest {
            stream: Some(false),
            ..canonical
        };
        assert!(
            codex_from_canonical(&streamed, MODEL, KEY)
                .expect("renders")
                .stream
        );
    }

    #[test]
    fn canonical_thinking_blocks_do_not_replay_here() {
        // The capability declaration the drop enforces:
        // cross-provider reasoning is opaque — protocol-forced out.
        let caps = Capabilities::CODEX;
        assert!(!caps.thinking_replay);
        let canonical = CanonicalRequest {
            messages: vec![message(
                CanonRole::Assistant,
                vec![
                    CanonBlock::Thinking {
                        text: "secret reasoning".to_owned(),
                    },
                    CanonBlock::Thinking {
                        text: "opaque-blob".to_owned(),
                    },
                    CanonBlock::Text("Answer.".to_owned()),
                ],
            )],
            ..CanonicalRequest::default()
        };
        let request = codex_from_canonical(&canonical, MODEL, KEY).expect("renders");
        assert_eq!(request.input.len(), 1, "only the text item survives");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "message", "role": "assistant",
                   "content": [{"type": "output_text", "text": "Answer."}]})
        );
        let bytes = serde_json::to_string(&request).expect("serialise");
        assert!(!bytes.contains("secret reasoning"));
        assert!(!bytes.contains("opaque-blob"));

        // An all-thinking message yields no item at all — the drop
        // does not fabricate an empty one.
        let all_thinking = CanonicalRequest {
            messages: vec![message(
                CanonRole::Assistant,
                vec![CanonBlock::Thinking {
                    text: "only thoughts".to_owned(),
                }],
            )],
            ..CanonicalRequest::default()
        };
        assert!(
            codex_from_canonical(&all_thinking, MODEL, KEY)
                .expect("renders")
                .input
                .is_empty()
        );
    }

    #[test]
    fn a_midstream_system_message_merges_into_the_preceding_user_turn() {
        // The capability declaration the merge enforces: the codex
        // backend refuses system-role input items (live-verified:
        // "System messages are not allowed"). ctp's transform: merge
        // into the preceding user turn, role sequence unchanged.
        let caps = Capabilities::CODEX;
        assert!(!caps.system_in_messages);
        let canonical = CanonicalRequest {
            messages: vec![
                message(
                    CanonRole::User,
                    vec![CanonBlock::Text("Earlier work.".to_owned())],
                ),
                message(
                    CanonRole::System,
                    vec![CanonBlock::Text("reminder".to_owned())],
                ),
            ],
            ..CanonicalRequest::default()
        };
        let request = codex_from_canonical(&canonical, MODEL, KEY).expect("renders");
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
        let canonical = CanonicalRequest {
            system: vec!["Base prompt.".to_owned()],
            messages: vec![
                message(
                    CanonRole::System,
                    vec![CanonBlock::Text("Preamble.".to_owned())],
                ),
                message(CanonRole::User, vec![CanonBlock::Text("Hi".to_owned())]),
            ],
            ..CanonicalRequest::default()
        };
        let request = codex_from_canonical(&canonical, MODEL, KEY).expect("renders");
        assert_eq!(request.instructions, "Base prompt.\n\nPreamble.");
        assert_eq!(
            request.input.len(),
            1,
            "the leading system message is not an input item"
        );
        assert_eq!(item_of(&request, 0)["role"], "user");
    }

    #[test]
    fn a_system_message_after_only_assistant_turns_falls_back_to_instructions() {
        let canonical = CanonicalRequest {
            messages: vec![
                message(
                    CanonRole::Assistant,
                    vec![CanonBlock::Text("Hello.".to_owned())],
                ),
                message(
                    CanonRole::System,
                    vec![CanonBlock::Text("reminder".to_owned())],
                ),
            ],
            ..CanonicalRequest::default()
        };
        let request = codex_from_canonical(&canonical, MODEL, KEY).expect("renders");
        assert_eq!(
            request.instructions, "reminder",
            "no user item to merge into"
        );
        assert_eq!(request.input.len(), 1);
    }

    // ── the render table, relocated from the pair module ─────────

    #[test]
    fn the_canonical_block_table_renders_in_order() {
        let canonical = CanonicalRequest {
            messages: vec![
                message(
                    CanonRole::User,
                    vec![
                        CanonBlock::Text("Hello".to_owned()),
                        CanonBlock::Image {
                            url: "data:image/png;base64,aVBONyU=".to_owned(),
                        },
                    ],
                ),
                message(
                    CanonRole::Assistant,
                    vec![
                        CanonBlock::Text("Hi".to_owned()),
                        CanonBlock::ToolUse {
                            id: "toolu_1".to_owned(),
                            name: "get_weather".to_owned(),
                            input: json!({"city": "Wellington"}),
                        },
                    ],
                ),
                message(
                    CanonRole::User,
                    vec![CanonBlock::ToolResult {
                        tool_use_id: "toolu_1".to_owned(),
                        content: ToolResultContent::String("18C".to_owned()),
                    }],
                ),
                message(
                    CanonRole::Assistant,
                    vec![CanonBlock::Text("Sunny in Wellington.".to_owned())],
                ),
            ],
            ..CanonicalRequest::default()
        };
        let request = codex_from_canonical(&canonical, MODEL, KEY).expect("renders");
        assert_eq!(request.input.len(), 5, "text+tool_use split into two items");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "Hello"},
                {"type": "input_image", "image_url": "data:image/png;base64,aVBONyU="},
            ]})
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
    }

    #[test]
    fn consecutive_blocks_group_and_text_after_a_tool_opens_a_new_item() {
        let canonical = CanonicalRequest {
            messages: vec![message(
                CanonRole::User,
                vec![
                    CanonBlock::Text("first".to_owned()),
                    CanonBlock::Text("second".to_owned()),
                    CanonBlock::ToolResult {
                        tool_use_id: "t1".to_owned(),
                        content: ToolResultContent::String("done".to_owned()),
                    },
                    CanonBlock::Text("after".to_owned()),
                ],
            )],
            ..CanonicalRequest::default()
        };
        let request = codex_from_canonical(&canonical, MODEL, KEY).expect("renders");
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
    fn tool_result_content_shapes_render_to_the_output_string() {
        let canonical = |content: ToolResultContent| CanonicalRequest {
            messages: vec![message(
                CanonRole::User,
                vec![CanonBlock::ToolResult {
                    tool_use_id: "t1".to_owned(),
                    content,
                }],
            )],
            ..CanonicalRequest::default()
        };
        // A string is the output text, verbatim.
        let request = codex_from_canonical(
            &canonical(ToolResultContent::String("plain text".to_owned())),
            MODEL,
            KEY,
        )
        .expect("renders");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "function_call_output", "call_id": "t1", "output": "plain text"})
        );
        // A clean block array renders as the JSON of the array — the
        // lossless string form (images and exotic blocks ride within
        // it via the frontend's raw fallback, byte-preserved).
        let request = codex_from_canonical(
            &canonical(ToolResultContent::Blocks(vec![
                CanonBlock::Text("src holds".to_owned()),
                CanonBlock::Text("two modules.".to_owned()),
            ])),
            MODEL,
            KEY,
        )
        .expect("renders");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "function_call_output", "call_id": "t1",
                   "output": "[{\"type\":\"text\",\"text\":\"src holds\"},{\"type\":\"text\",\"text\":\"two modules.\"}]"})
        );
        // Absent content is the empty output.
        let request = codex_from_canonical(
            &canonical(ToolResultContent::String(String::new())),
            MODEL,
            KEY,
        )
        .expect("renders");
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "function_call_output", "call_id": "t1", "output": ""})
        );
    }

    #[test]
    fn an_empty_text_block_still_yields_its_message_item() {
        // String content always yielded its item in the pair module,
        // empty or not — the canonical keeps that: one Text block,
        // one item with one (empty) part.
        let canonical = CanonicalRequest {
            messages: vec![message(
                CanonRole::User,
                vec![CanonBlock::Text(String::new())],
            )],
            ..CanonicalRequest::default()
        };
        let request = codex_from_canonical(&canonical, MODEL, KEY).expect("renders");
        assert_eq!(request.input.len(), 1);
        assert_eq!(
            item_of(&request, 0),
            json!({"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": ""}]})
        );
    }

    #[test]
    fn tools_render_to_the_flat_function_shape() {
        let canonical = CanonicalRequest {
            tools: vec![
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
            messages: vec![message(
                CanonRole::User,
                vec![CanonBlock::Text("Hi".to_owned())],
            )],
            ..CanonicalRequest::default()
        };
        let request = codex_from_canonical(&canonical, MODEL, KEY).expect("renders");
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
        // No tools, none rendered.
        let bare = CanonicalRequest::default();
        assert!(
            codex_from_canonical(&bare, MODEL, KEY)
                .expect("renders")
                .tools
                .is_empty()
        );
    }

    #[test]
    fn tool_choice_renders_auto_and_any_and_reports_the_rest() {
        let canonical = |tool_choice: CanonToolChoice| CanonicalRequest {
            tool_choice,
            messages: vec![message(
                CanonRole::User,
                vec![CanonBlock::Text("Hi".to_owned())],
            )],
            ..CanonicalRequest::default()
        };
        assert_eq!(
            codex_from_canonical(&canonical(CanonToolChoice::Auto), MODEL, KEY)
                .expect("renders")
                .tool_choice,
            "auto"
        );
        assert_eq!(
            codex_from_canonical(&canonical(CanonToolChoice::Any), MODEL, KEY)
                .expect("renders")
                .tool_choice,
            "required"
        );
        // A forced tool has no faithful string form on this wire —
        // reported with the pair module's exact reason.
        assert_eq!(
            codex_from_canonical(
                &canonical(CanonToolChoice::Tool {
                    name: Some("read_file".to_owned())
                }),
                MODEL,
                KEY
            ),
            Err(TranslateError::Malformed {
                reason: "tool_choice type \"tool\" has no responses equivalent".to_owned()
            })
        );
        assert_eq!(
            codex_from_canonical(
                &canonical(CanonToolChoice::Other {
                    kind: "banana".to_owned()
                }),
                MODEL,
                KEY
            ),
            Err(TranslateError::Malformed {
                reason: "tool_choice type \"banana\" has no responses equivalent".to_owned()
            })
        );
    }

    #[test]
    fn thinking_budgets_map_to_reasoning_effort_tiers() {
        // The upstream SUPPORTS reasoning — the intent crosses (only
        // thinking-block REPLAY is protocol-forced out). Tiers follow
        // claude's own budget ladder.
        for (budget, effort) in [
            (1024, "low"),
            (10_000, "low"),
            (16_383, "low"),
            (16_384, "medium"),
            (24_000, "medium"),
            (32_767, "medium"),
            (32_768, "high"),
            (64_000, "high"),
        ] {
            let canonical = CanonicalRequest {
                thinking: Some(ThinkingSpec {
                    budget_tokens: budget,
                }),
                messages: vec![message(
                    CanonRole::User,
                    vec![CanonBlock::Text("Hi".to_owned())],
                )],
                ..CanonicalRequest::default()
            };
            let request = codex_from_canonical(&canonical, MODEL, KEY).expect("renders");
            assert_eq!(
                request.reasoning.effort.as_deref(),
                Some(effort),
                "budget {budget}"
            );
        }
        // "The client did not ask" stays None — the model's own
        // default effort governs, never guessed here.
        let bare = CanonicalRequest {
            messages: vec![message(
                CanonRole::User,
                vec![CanonBlock::Text("Hi".to_owned())],
            )],
            ..CanonicalRequest::default()
        };
        assert_eq!(
            codex_from_canonical(&bare, MODEL, KEY)
                .expect("renders")
                .reasoning
                .effort,
            None
        );
        // Purity: same budget, same effort bytes, every time.
        let canonical = CanonicalRequest {
            thinking: Some(ThinkingSpec {
                budget_tokens: 20_000,
            }),
            messages: vec![message(
                CanonRole::User,
                vec![CanonBlock::Text("Hi".to_owned())],
            )],
            ..CanonicalRequest::default()
        };
        let first =
            serde_json::to_string(&codex_from_canonical(&canonical, MODEL, KEY).expect("renders"))
                .expect("serialise");
        for _ in 0..2 {
            let again = serde_json::to_string(
                &codex_from_canonical(&canonical, MODEL, KEY).expect("renders"),
            )
            .expect("serialise");
            assert_eq!(first, again);
        }
    }
}
