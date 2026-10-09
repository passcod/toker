//! The backend adapter, both directions: one [`CanonicalRequest`] →
//! one codex [`ResponsesRequest`] (the request render), and the
//! codex wire's turn → the canonical turn model (the response
//! interpretation — unit A's wire types both ways).
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
//! | system-role input items (`system_in_messages`) | refused: verified live ("System messages are not allowed") | system content via `instructions` (leading) and the preceding-user merge (mid-conversation, the same merge the predecessor proxy used) |
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
//!
//! # The response direction (interpretation)
//!
//! [`CanonStream`] folds the turn's [`ResponseEvent`]s into
//! [`CanonEvent`]s — the backend's own event dialect read INTO the
//! canonical turn model — and
//! [`canonical_turn_from_capture`] folds unit A's whole-turn
//! [`TurnCapture`] into the canonical final ([`CanonTurn`]) for the
//! non-streaming path. The interpretation table (what the codex
//! dialect's events MEAN canonically):
//!
//! | codex event | canonical event |
//! |---|---|
//! | `response.created` | [`TurnStarted`](CanonEvent::TurnStarted) — the response id, when the event named one |
//! | `response.output_text.delta` | [`TextDelta`](CanonEvent::TextDelta) |
//! | `response.reasoning_summary_text.delta` | [`ThinkingDelta`](CanonEvent::ThinkingDelta) — `part` is the summary index, the backend's own part identity |
//! | `output_item.done` of a `message` | [`TextEnded`](CanonEvent::TextEnded) — the text part completed |
//! | `output_item.done` of a `reasoning` item | [`ThinkingEnded`](CanonEvent::ThinkingEnded) — the reasoning part completed |
//! | `output_item.done` of a `function_call` | [`ToolCall`](CanonEvent::ToolCall), COMPLETE — the done item carries the whole arguments; a call whose fields do not fit the typed view is skipped, never corrupted into the stream (invariant 6) |
//! | `output_item.done` of any other kind | nothing |
//! | `response.completed` | [`TurnEnded`](CanonEvent::TurnEnded) — `tool_use` when any function call completed, else `end_turn`; the usage → [`CanonicalUsage`] |
//! | `response.incomplete` | [`TurnEnded`](CanonEvent::TurnEnded) — `content_filter` reads as the refusal, anything else as the incomplete stop reason (verbatim) |
//! | `response.failed` | [`TurnFailed`](CanonEvent::TurnFailed) — terminal |
//! | a top-level `error` | [`Error`](CanonEvent::Error) — not terminal |
//! | `output_item.added`, unknown kinds | nothing (they never had a canonical shape) |
//!
//! ## The error table's interpretation half
//!
//! The upstream's error fields → [`CanonError`]. `code` first, then
//! `kind`; first match wins:
//!
//! | upstream (`code`, then `kind`) | canonical kind |
//! |---|---|
//! | `code` containing `rate_limit`, or `kind` `rate_limit_error` | [`RateLimit`](CanonErrorKind::RateLimit) — `resets_at` rides when carried |
//! | `code` containing `context_length` | [`InvalidRequest`](CanonErrorKind::InvalidRequest) |
//! | `code` containing `quota` or `usage_limit` | [`InvalidRequest`](CanonErrorKind::InvalidRequest) |
//! | `kind` already one of anthropic's own type names | the typed variant that renders it |
//! | anything else | [`Api`](CanonErrorKind::Api) |
//!
//! The message resolves HERE, on the backend's own wire fields
//! (the canonical carries the result, never the fallback chain):
//! the wire's message, standing in on the `code`, then the `kind`,
//! then the constant `"upstream error"` — never invented.

use serde_json::{Value, json};

use crate::ir::canonical::{
    CanonBlock, CanonError, CanonErrorKind, CanonEvent, CanonMessage, CanonRole, CanonStopReason,
    CanonSystemPart, CanonTool, CanonToolCall, CanonToolChoice, CanonTurn, CanonicalExtension,
    CanonicalRequest, CanonicalUsage, ThinkingSpec,
};
use crate::providers::codex::{
    Item, ResponseError, ResponseEvent, ResponsesRequest, Tool, TurnCapture, Usage,
};
use crate::routing::DialectId;
use crate::translate::{
    Rendered, TranslateError, TranslationLoss, TranslationLossReason, TranslationReport,
};

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
    let mut routed = canonical.clone();
    routed.model = Some(model.to_owned());
    Ok(render_codex(&routed, prompt_cache_key)?.value)
}

/// Render one canonical request and report every semantic omission.
///
/// [`codex_from_canonical`] remains as the live-handler compatibility seam
/// while phase 2 moves callers onto this explicit result.
pub fn render_codex(
    canonical: &CanonicalRequest,
    prompt_cache_key: &str,
) -> Result<Rendered<ResponsesRequest>, TranslateError> {
    let model = canonical
        .model
        .as_deref()
        .ok_or_else(|| TranslateError::Malformed {
            reason: "canonical request has no model".to_owned(),
        })?;
    let mut request = ResponsesRequest::new(model, prompt_cache_key);
    let mut report = loss_report(canonical);
    let (input, leading_system) = input_of(canonical, &mut report);
    // The system prompt is the pieces joined on blank lines — THIS
    // backend's `instructions` form — plus any system-role message
    // texts that had no user turn to merge into (below).
    let mut instructions = canonical
        .system
        .iter()
        .filter_map(CanonSystemPart::semantic_text)
        .collect::<Vec<_>>()
        .join("\n\n");
    for text in leading_system {
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(&text);
    }
    request.instructions = instructions;
    request.input = input;
    request.tools = canonical
        .tools
        .iter()
        .map(|tool| tool_of(tool, &mut report))
        .collect();
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
    request.reasoning.effort = canonical.thinking.as_ref().and_then(effort_of);
    replay_request_extensions(&mut request, &canonical.extensions, &mut report);
    Ok(Rendered {
        value: request,
        report,
    })
}

fn loss_report(canonical: &CanonicalRequest) -> TranslationReport {
    let mut report = TranslationReport::default();
    let unsupported = TranslationLossReason::UnsupportedByBinding;

    if canonical.sampling.temperature.is_some() {
        report.push(TranslationLoss::new("sampling.temperature", unsupported, 1));
    }
    if canonical.sampling.top_p.is_some() {
        report.push(TranslationLoss::new("sampling.top_p", unsupported, 1));
    }
    if canonical.sampling.max_tokens.is_some() {
        report.push(TranslationLoss::new("sampling.max_tokens", unsupported, 1));
    }
    if canonical.sampling.stop_sequences.is_some() {
        report.push(TranslationLoss::new(
            "sampling.stop_sequences",
            unsupported,
            1,
        ));
    }

    let developer_messages = canonical
        .messages
        .iter()
        .filter(|message| message.role == CanonRole::Developer)
        .count();
    if developer_messages > 0 {
        report.push(TranslationLoss::new(
            "messages[].role.developer",
            TranslationLossReason::NotRepresentable,
            developer_messages,
        ));
    }

    let thinking_blocks = canonical
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| {
            matches!(
                block.semantic(),
                CanonBlock::Thinking { .. } | CanonBlock::RedactedThinking { .. }
            )
        })
        .count();
    if thinking_blocks > 0 {
        report.push(TranslationLoss::new(
            "messages[].blocks[].thinking",
            unsupported,
            thinking_blocks,
        ));
    }

    for part in &canonical.system {
        match part {
            CanonSystemPart::Text { extensions, .. } => {
                for extension in extensions {
                    report.push(TranslationLoss::new(
                        extension.wire_path(),
                        TranslationLossReason::IncompatibleExtensionDialect,
                        1,
                    ));
                }
            }
            CanonSystemPart::Opaque(extension) => report.push(TranslationLoss::new(
                extension.wire_path(),
                TranslationLossReason::IncompatibleExtensionDialect,
                1,
            )),
        }
    }

    report
}

fn replay_request_extensions(
    request: &mut ResponsesRequest,
    extensions: &[CanonicalExtension],
    report: &mut TranslationReport,
) {
    for extension in extensions {
        if extension.source() != DialectId::CodexResponses {
            report_incompatible(extension, report);
            continue;
        }
        let Some(name) = extension.wire_name() else {
            report_not_representable(extension, report);
            continue;
        };
        if extension.wire_path().starts_with("$.reasoning.") {
            request
                .reasoning
                .extra
                .insert(name.to_owned(), extension.value().clone());
            continue;
        }
        match name {
            "store" => match extension.value().as_bool() {
                Some(value) => request.store = value,
                None => report_not_representable(extension, report),
            },
            "parallel_tool_calls" => match extension.value().as_bool() {
                Some(value) => request.parallel_tool_calls = value,
                None => report_not_representable(extension, report),
            },
            "include" => match serde_json::from_value(extension.value().clone()) {
                Ok(value) => request.include = value,
                Err(_) => report_not_representable(extension, report),
            },
            "tools" => match extension.value().as_array() {
                Some(tools) => {
                    request.tools = tools.iter().cloned().map(Tool).collect();
                }
                None => report_not_representable(extension, report),
            },
            _ => {
                request
                    .extra
                    .insert(name.to_owned(), extension.value().clone());
            }
        }
    }
}

fn report_incompatible(extension: &CanonicalExtension, report: &mut TranslationReport) {
    report.push(TranslationLoss::new(
        extension.wire_path(),
        TranslationLossReason::IncompatibleExtensionDialect,
        1,
    ));
}

fn report_not_representable(extension: &CanonicalExtension, report: &mut TranslationReport) {
    report.push(TranslationLoss::new(
        extension.wire_path(),
        TranslationLossReason::NotRepresentable,
        1,
    ));
}

fn replay_node_extensions(
    value: &mut Value,
    extensions: &[CanonicalExtension],
    report: &mut TranslationReport,
) {
    let object = value
        .as_object_mut()
        .expect("a Responses node renders as an object");
    for extension in extensions {
        if extension.source() != DialectId::CodexResponses {
            report_incompatible(extension, report);
        } else if let Some(name) = extension.wire_name() {
            object.insert(name.to_owned(), extension.value().clone());
        } else {
            report_not_representable(extension, report);
        }
    }
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
/// text parts — the same transform the predecessor proxy used for the
/// same problem (sonnet 5
/// refuses system entries in `messages[]`; the codex backend refuses
/// system-role input items — live-verified: "System messages are not
/// allowed", the `system_in_messages: false` capability). With no
/// preceding user item, the text falls back to the leading set
/// (instructions) — never dropped, never a system item.
fn input_of(
    canonical: &CanonicalRequest,
    report: &mut TranslationReport,
) -> (Vec<Item>, Vec<String>) {
    let mut items: Vec<Item> = Vec::with_capacity(canonical.messages.len());
    let mut leading_system: Vec<String> = Vec::new();
    for message in &canonical.messages {
        if let Some(item) = opaque_codex_input_item(message, report) {
            items.push(item);
            continue;
        }
        match message.role {
            CanonRole::System | CanonRole::Developer => {
                for extension in &message.extensions {
                    report_unrepresentable_or_incompatible(extension, report);
                }
                for block in &message.blocks {
                    // The frontend's system parse yields text blocks
                    // only; the canonical is typed, so anything else
                    // cannot occur — and skipping keeps this total.
                    let CanonBlock::Text(text) = block.semantic() else {
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
                let first_item = items.len();
                let role = wire_role(role);
                let mut parts: Vec<Value> = Vec::new();
                for block in &message.blocks {
                    match block.semantic() {
                        CanonBlock::Text(text) => {
                            let mut part = text_part(text, role == "assistant");
                            replay_block_extensions(&mut part, block, report);
                            parts.push(part);
                        }
                        CanonBlock::Image { url } => {
                            let mut part = json!({"type": "input_image", "image_url": url});
                            replay_block_extensions(&mut part, block, report);
                            parts.push(part);
                        }
                        CanonBlock::ToolUse { id, name, input } => {
                            let arguments = serde_json::to_string(input)
                                .expect("a parsed Value always serialises");
                            items.extend(flush(&mut parts, role));
                            let mut item = Item::function_call(name, &arguments, id);
                            replay_block_extensions(&mut item.0, block, report);
                            items.push(item);
                        }
                        CanonBlock::ToolResult {
                            tool_use_id,
                            content,
                        } => {
                            items.extend(flush(&mut parts, role));
                            let mut item =
                                Item::function_call_output(tool_use_id, &content.output_text());
                            replay_block_extensions(&mut item.0, block, report);
                            items.push(item);
                        }
                        // DROPPED, loudly — this backend's declared
                        // cost: [`Capabilities::CODEX`].
                        // thinking_replay is false (cross-provider
                        // reasoning is opaque; see the module docs).
                        // The drop does not split the message's parts.
                        CanonBlock::Thinking { .. } | CanonBlock::RedactedThinking { .. } => {}
                        CanonBlock::Annotated { .. } => {
                            unreachable!("semantic() removes annotations")
                        }
                    }
                }
                items.extend(flush(&mut parts, role));
                if let Some(item) = items[first_item..]
                    .iter_mut()
                    .find(|item| item.kind() == Some("message"))
                {
                    replay_node_extensions(&mut item.0, &message.extensions, report);
                } else {
                    for extension in &message.extensions {
                        if extension.source() == DialectId::CodexResponses {
                            report_not_representable(extension, report);
                        } else {
                            report_incompatible(extension, report);
                        }
                    }
                }
            }
        }
    }
    (items, leading_system)
}

fn opaque_codex_input_item(message: &CanonMessage, report: &mut TranslationReport) -> Option<Item> {
    if !message.blocks.is_empty() {
        return None;
    }
    let item_index = message.extensions.iter().position(|extension| {
        let Some(kind) = extension
            .value()
            .as_object()
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
        else {
            return false;
        };
        extension.source() == DialectId::CodexResponses
            && extension.wire_name().is_none()
            && extension.wire_path().strip_prefix("$.input[].") == Some(kind)
    })?;
    let item = Item(message.extensions[item_index].value().clone());
    for (index, extension) in message.extensions.iter().enumerate() {
        if index == item_index {
            continue;
        }
        if extension.source() == DialectId::CodexResponses {
            report_not_representable(extension, report);
        } else {
            report_incompatible(extension, report);
        }
    }
    Some(item)
}

fn report_unrepresentable_or_incompatible(
    extension: &CanonicalExtension,
    report: &mut TranslationReport,
) {
    if extension.source() == DialectId::CodexResponses {
        report_not_representable(extension, report);
    } else {
        report_incompatible(extension, report);
    }
}

fn replay_block_extensions(value: &mut Value, block: &CanonBlock, report: &mut TranslationReport) {
    let CanonBlock::Annotated {
        is_error,
        extensions,
        ..
    } = block
    else {
        return;
    };
    if is_error.is_some() {
        report.push(TranslationLoss::new(
            "messages[].blocks[].tool_result.is_error",
            TranslationLossReason::UnsupportedByBinding,
            1,
        ));
    }
    replay_node_extensions(value, extensions, report);
}

/// The most recent user message item, mutably — the merge target for a
/// mid-conversation system message (the preceding-user rule).
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
fn tool_of(tool: &CanonTool, report: &mut TranslationReport) -> Tool {
    let mut rendered = Tool::function(
        &tool.name,
        &tool.description,
        false,
        tool.parameters.clone(),
    );
    replay_node_extensions(&mut rendered.0, &tool.extensions, report);
    rendered
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
        CanonToolChoice::None => Ok("none".to_owned()),
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
fn effort_of(thinking: &ThinkingSpec) -> Option<String> {
    match thinking {
        ThinkingSpec::Effort(effort) => Some(effort.clone()),
        ThinkingSpec::BudgetTokens(0..=16_383) => Some("low".to_owned()),
        ThinkingSpec::BudgetTokens(16_384..=32_767) => Some("medium".to_owned()),
        ThinkingSpec::BudgetTokens(_) => Some("high".to_owned()),
        ThinkingSpec::Disabled | ThinkingSpec::BetweenTools => None,
    }
}

// ── shared ──────────────────────────────────────────────────────────

/// A canonical dialogue role → the wire's role string.
fn wire_role(role: CanonRole) -> &'static str {
    match role {
        CanonRole::User => "user",
        CanonRole::Assistant => "assistant",
        CanonRole::System => "system",
        CanonRole::Developer => "developer",
    }
}

// ── the response direction: interpretation ───────────────────────────

/// The response-direction interpreter: feed it the turn's
/// [`ResponseEvent`]s (in arrival order), collect the
/// [`CanonEvent`]s they mean (the interpretation table lives in the
/// module docs). One stream per turn.
///
/// The interpretation is stateful in exactly one fact — whether any
/// function call completed, the `completed` stop reason's input.
/// Everything else is a pure function of the event under
/// interpretation (invariant 4).
#[derive(Debug, Clone, Default)]
pub struct CanonStream {
    /// Whether any `function_call` item completed with a typed view
    /// (the tool_use stop reason) — a call whose fields did not fit
    /// the typed view was skipped, so it does not count.
    function_calls: bool,
}

impl CanonStream {
    /// A fresh interpreter for one turn.
    pub fn new() -> CanonStream {
        CanonStream::default()
    }

    /// Feed one Responses event; every canonical event it means, in
    /// arrival order. Never fails — an event with no canonical shape
    /// (an `output_item.added`, an unknown kind, a `function_call`
    /// whose fields do not fit the typed view) produces nothing
    /// rather than corrupting the stream (invariant 6).
    pub fn feed(&mut self, event: &ResponseEvent) -> Vec<CanonEvent> {
        let mut out = Vec::new();
        match event {
            ResponseEvent::Created { response_id, .. } => {
                out.push(CanonEvent::TurnStarted {
                    turn_id: response_id.clone(),
                });
            }
            ResponseEvent::OutputTextDelta { delta } => {
                out.push(CanonEvent::TextDelta {
                    delta: delta.clone(),
                });
            }
            ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            } => {
                // The summary index IS the part identity, and the
                // cast is the identity for every index the wire
                // really carries (array positions, non-negative); the
                // two's-complement bit pattern stays injective even
                // for a negative index an exotic upstream invented,
                // so part distinctness never collapses.
                out.push(CanonEvent::ThinkingDelta {
                    part: *summary_index as u64,
                    delta: delta.clone(),
                });
            }
            ResponseEvent::OutputItemDone { item } => self.item_done(item, &mut out),
            ResponseEvent::Completed { response } => {
                out.push(CanonEvent::TurnEnded {
                    stop_reason: if self.function_calls {
                        CanonStopReason::ToolUse
                    } else {
                        CanonStopReason::EndTurn
                    },
                    usage: response.usage.as_ref().map(canonical_usage),
                });
            }
            ResponseEvent::Incomplete { reason, usage } => {
                out.push(CanonEvent::TurnEnded {
                    stop_reason: match reason.as_deref() {
                        // The only reason vocabulary this wire's
                        // refusal has; anything else stopped at a
                        // budget and carries its own reason.
                        Some("content_filter") => CanonStopReason::Refusal,
                        other => CanonStopReason::Incomplete(other.unwrap_or_default().to_owned()),
                    },
                    usage: usage.as_ref().map(canonical_usage),
                });
            }
            ResponseEvent::Failed { error } => {
                out.push(CanonEvent::TurnFailed {
                    error: canon_error_from_response(error),
                });
            }
            ResponseEvent::Error { error } => {
                out.push(CanonEvent::Error {
                    error: canon_error_from_response(error),
                });
            }
            ResponseEvent::OutputItemAdded { .. } | ResponseEvent::Unknown { .. } => {}
        }
        out
    }

    /// One `output_item.done`: a message item's text part completed
    /// (its deltas already streamed); a reasoning item's part
    /// completed; a function-call item is a COMPLETE tool call —
    /// the arguments arrive whole here (unit A's parser buffers them
    /// from the done item), and the call counts for the stop reason
    /// only when its fields fit the typed view.
    fn item_done(&mut self, item: &Item, out: &mut Vec<CanonEvent>) {
        match item.kind() {
            Some("message") => out.push(CanonEvent::TextEnded),
            Some("reasoning") => {
                if let Some(data) = item
                    .as_reasoning()
                    .and_then(|reasoning| reasoning.encrypted_content)
                {
                    out.push(CanonEvent::RedactedThinking { data });
                }
                out.push(CanonEvent::ThinkingEnded);
            }
            Some("function_call") => {
                if let Some(call) = item.as_function_call() {
                    self.function_calls = true;
                    out.push(CanonEvent::ToolCall(CanonToolCall {
                        id: call.call_id,
                        name: call.name,
                        arguments: call.arguments,
                    }));
                }
            }
            _ => {}
        }
    }
}

/// One Responses usage → the canonical buckets, with the raw usage
/// carried verbatim (every member the wire carried, re-serialised).
fn canonical_usage(usage: &Usage) -> CanonicalUsage {
    CanonicalUsage {
        input: Some(usage.input_tokens),
        cache_read: usage
            .input_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens),
        cache_write: usage
            .input_tokens_details
            .as_ref()
            .and_then(|details| details.cache_write_tokens),
        output: Some(usage.output_tokens),
        reasoning: usage
            .output_tokens_details
            .as_ref()
            .and_then(|details| details.reasoning_tokens),
        serving_provider: None,
        raw: serde_json::to_value(usage).expect("a parsed Usage always serialises"),
    }
}

/// A Responses error payload → the canonical error (the
/// interpretation half of the error table — the module docs). The
/// message resolves here, on the backend's own wire fields: the
/// wire's message, standing in on the `code`, then the `kind`, then
/// the constant — never invented.
pub fn canon_error_from_response(error: &ResponseError) -> CanonError {
    let kind = canon_error_kind(error);
    let message = error
        .message
        .clone()
        .or_else(|| error.code.clone())
        .or_else(|| error.kind.clone())
        .unwrap_or_else(|| "upstream error".to_owned());
    CanonError {
        kind,
        message,
        resets_at: error.resets_at,
    }
}

/// The error table's interpretation half: `code` first (lowercased —
/// case spellings still match), then `kind`; first match wins.
fn canon_error_kind(error: &ResponseError) -> CanonErrorKind {
    let code = error.code.as_deref().unwrap_or("").to_ascii_lowercase();
    if code.contains("rate_limit") {
        return CanonErrorKind::RateLimit;
    }
    if code.contains("context_length") {
        return CanonErrorKind::InvalidRequest;
    }
    if code.contains("quota") || code.contains("usage_limit") {
        return CanonErrorKind::InvalidRequest;
    }
    // Kinds that are already anthropic's own type names read as the
    // typed variant that renders them — verbatim through the table,
    // without a passthrough shape (see the canonical's docs).
    match error.kind.as_deref() {
        Some("rate_limit_error") => CanonErrorKind::RateLimit,
        Some("invalid_request_error") => CanonErrorKind::InvalidRequest,
        Some("authentication_error") => CanonErrorKind::Authentication,
        Some("permission_error") => CanonErrorKind::Permission,
        Some("not_found_error") => CanonErrorKind::NotFound,
        Some("request_too_large") => CanonErrorKind::TooLarge,
        Some("overloaded_error") => CanonErrorKind::Overloaded,
        _ => CanonErrorKind::Api,
    }
}

/// Unit A's whole-turn [`TurnCapture`] → the canonical final
/// ([`CanonTurn`]) — the non-streaming path's fold. The capture
/// already holds the turn's own facts (its response id, its items,
/// its text, its summaries, its usage, its first error); this reads
/// them into the canonical with the same stop-reason logic the
/// streaming fold applies: the incomplete reason first (its
/// `content_filter` reads as the refusal), then any completed
/// function call (the tool_use stop reason), else the natural end.
pub fn canonical_turn_from_capture(capture: &TurnCapture) -> CanonTurn {
    CanonTurn {
        turn_id: capture.response_id().map(str::to_owned),
        stop_reason: match capture.incomplete_reason() {
            Some("content_filter") => CanonStopReason::Refusal,
            Some(other) => CanonStopReason::Incomplete(other.to_owned()),
            None if !capture.function_calls().is_empty() => CanonStopReason::ToolUse,
            None => CanonStopReason::EndTurn,
        },
        usage: capture.usage().map(canonical_usage),
        error: capture.error().map(canon_error_from_response),
        tool_calls: capture
            .function_calls()
            .into_iter()
            .map(|call| CanonToolCall {
                id: call.call_id,
                name: call.name,
                arguments: call.arguments,
            })
            .collect(),
        blocks: blocks_from_capture(capture),
        text: capture.text().to_owned(),
        thinking: capture
            .reasoning_summaries()
            .iter()
            .map(|(part, text)| (*part as u64, text.clone()))
            .collect(),
    }
}

fn blocks_from_capture(capture: &TurnCapture) -> Option<Vec<CanonBlock>> {
    if capture.items().is_empty() {
        return None;
    }
    let mut blocks = Vec::new();
    let mut has_reasoning = false;
    for item in capture.items() {
        match item.kind()? {
            "reasoning" => {
                has_reasoning = true;
                let reasoning = item.as_reasoning()?;
                blocks.extend(
                    reasoning
                        .summary
                        .into_iter()
                        .map(|summary| CanonBlock::Thinking {
                            text: summary.text,
                            signature: None,
                        }),
                );
                if let Some(data) = reasoning.encrypted_content {
                    blocks.push(CanonBlock::RedactedThinking { data });
                }
            }
            "message" => {
                let message = item.as_message()?;
                for part in message.content {
                    if part.kind != "output_text" {
                        return None;
                    }
                    blocks.push(CanonBlock::Text(part.text));
                }
            }
            "function_call" => {
                let call = item.as_function_call()?;
                let input: Value = serde_json::from_str(&call.arguments).ok()?;
                if !input.is_object() {
                    return None;
                }
                blocks.push(CanonBlock::ToolUse {
                    id: call.call_id,
                    name: call.name,
                    input,
                });
            }
            _ => return None,
        }
    }
    if !capture.reasoning_summaries().is_empty() && !has_reasoning {
        return None;
    }
    Some(blocks)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::super::codex_backend::{codex_from_canonical, render_codex};
    use super::super::{TranslateError, TranslationLoss, TranslationLossReason};
    use super::{CanonStream, canon_error_from_response, canonical_turn_from_capture};
    use crate::ir::canonical::{
        CanonBlock, CanonError, CanonErrorKind, CanonEvent, CanonMessage, CanonRole,
        CanonStopReason, CanonSystemPart, CanonTool, CanonToolCall, CanonToolChoice, CanonTurn,
        CanonicalExtension, CanonicalRequest, CanonicalUsage, Capabilities, SamplingSpec,
        ThinkingSpec, ToolResultContent,
    };
    use crate::providers::codex::{
        CompletedResponse, ContentPart, Item, ResponseError, ResponseEvent, ResponsesSse,
        TurnCapture,
    };
    use crate::routing::DialectId;
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
        CanonMessage {
            role,
            blocks,
            extensions: Vec::new(),
        }
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
            model: Some(MODEL.to_owned()),
            sampling: SamplingSpec {
                temperature: Some(0.3),
                top_p: Some(0.95),
                max_tokens: Some(4096),
                stop_sequences: Some(vec!["\n\nHuman:".to_owned()]),
            },
            thinking: Some(ThinkingSpec::BudgetTokens(2048)),
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
        let rendered = render_codex(&canonical, KEY).expect("renders with report");
        assert_eq!(
            rendered.report.losses(),
            &[
                TranslationLoss::new(
                    "sampling.temperature",
                    TranslationLossReason::UnsupportedByBinding,
                    1,
                ),
                TranslationLoss::new(
                    "sampling.top_p",
                    TranslationLossReason::UnsupportedByBinding,
                    1,
                ),
                TranslationLoss::new(
                    "sampling.max_tokens",
                    TranslationLossReason::UnsupportedByBinding,
                    1,
                ),
                TranslationLoss::new(
                    "sampling.stop_sequences",
                    TranslationLossReason::UnsupportedByBinding,
                    1,
                ),
            ]
        );
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
    fn the_backend_renders_the_model_owned_by_the_canonical() {
        let canonical = CanonicalRequest {
            model: Some("effective-model".to_owned()),
            ..CanonicalRequest::default()
        };
        let rendered = render_codex(&canonical, KEY).expect("model is present");
        assert_eq!(rendered.value.model, "effective-model");

        assert!(matches!(
            render_codex(&CanonicalRequest::default(), KEY),
            Err(TranslateError::Malformed { reason }) if reason.contains("no model")
        ));
    }

    #[test]
    fn canonical_thinking_blocks_do_not_replay_here() {
        // The capability declaration the drop enforces:
        // cross-provider reasoning is opaque — protocol-forced out.
        let caps = Capabilities::CODEX;
        assert!(!caps.thinking_replay);
        let canonical = CanonicalRequest {
            model: Some(MODEL.to_owned()),
            messages: vec![message(
                CanonRole::Assistant,
                vec![
                    CanonBlock::Thinking {
                        text: "secret reasoning".to_owned(),
                        signature: Some("sig-1".to_owned()),
                    },
                    CanonBlock::RedactedThinking {
                        data: "opaque-blob".to_owned(),
                    },
                    CanonBlock::Text("Answer.".to_owned()),
                ],
            )],
            ..CanonicalRequest::default()
        };
        let rendered = render_codex(&canonical, KEY).expect("renders with report");
        assert_eq!(
            rendered.report.losses(),
            &[TranslationLoss::new(
                "messages[].blocks[].thinking",
                TranslationLossReason::UnsupportedByBinding,
                2,
            )]
        );
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
                    signature: None,
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
    fn incompatible_opaque_extensions_are_reported_without_their_values() {
        let canonical = CanonicalRequest {
            model: Some(MODEL.to_owned()),
            extensions: vec![CanonicalExtension::new(
                DialectId::AnthropicMessages,
                "$.metadata",
                json!({"user_id": "secret-user"}),
            )],
            ..CanonicalRequest::default()
        };
        let rendered = render_codex(&canonical, KEY).expect("renders with report");
        assert_eq!(
            rendered.report.losses(),
            &[TranslationLoss::new(
                "$.metadata",
                TranslationLossReason::IncompatibleExtensionDialect,
                1,
            )]
        );
        let report = format!("{:?}", rendered.report);
        assert!(!report.contains("secret-user"));
    }

    #[test]
    fn system_metadata_and_opaque_parts_are_reported_but_not_rendered() {
        let canonical = CanonicalRequest {
            model: Some(MODEL.to_owned()),
            system: vec![
                CanonSystemPart::Text {
                    text: "Keep this instruction.".to_owned(),
                    extensions: vec![CanonicalExtension::new(
                        DialectId::AnthropicMessages,
                        "$.system[].cache_control",
                        json!({"type": "ephemeral", "secret": "metadata-value"}),
                    )],
                },
                CanonSystemPart::Opaque(CanonicalExtension::new(
                    DialectId::AnthropicMessages,
                    "$.system[]",
                    json!({"future_prompt": "opaque-value"}),
                )),
            ],
            ..CanonicalRequest::default()
        };

        let rendered = render_codex(&canonical, KEY).expect("renders with report");
        assert_eq!(rendered.value.instructions, "Keep this instruction.");
        assert_eq!(
            rendered.report.losses(),
            &[
                TranslationLoss::new(
                    "$.system[].cache_control",
                    TranslationLossReason::IncompatibleExtensionDialect,
                    1,
                ),
                TranslationLoss::new(
                    "$.system[]",
                    TranslationLossReason::IncompatibleExtensionDialect,
                    1,
                ),
            ]
        );
        let report = format!("{:?}", rendered.report);
        assert!(!report.contains("metadata-value"));
        assert!(!report.contains("opaque-value"));
    }

    #[test]
    fn message_block_metadata_is_reported_without_changing_semantic_rendering() {
        let canonical = CanonicalRequest {
            model: Some(MODEL.to_owned()),
            tools: vec![CanonTool {
                name: "cached_tool".to_owned(),
                description: String::new(),
                parameters: json!({"type": "object"}),
                extensions: vec![CanonicalExtension::node_field(
                    DialectId::AnthropicMessages,
                    "$.tools[].cache_control",
                    "cache_control",
                    json!({"type": "ephemeral", "secret": "tool-cache-value"}),
                )],
            }],
            messages: vec![message(
                CanonRole::User,
                vec![
                    CanonBlock::Text("cached prompt".to_owned()).annotated(
                        None,
                        vec![CanonicalExtension::node_field(
                            DialectId::AnthropicMessages,
                            "$.messages[].content[].cache_control",
                            "cache_control",
                            json!({"type": "ephemeral", "secret": "cache-value"}),
                        )],
                    ),
                    CanonBlock::ToolResult {
                        tool_use_id: "toolu_1".to_owned(),
                        content: ToolResultContent::String("failed".to_owned()),
                    }
                    .annotated(
                        Some(true),
                        vec![CanonicalExtension::node_field(
                            DialectId::AnthropicMessages,
                            "$.messages[].content[].future_field",
                            "future_field",
                            json!({"secret": "future-value"}),
                        )],
                    ),
                ],
            )],
            ..CanonicalRequest::default()
        };

        let rendered = render_codex(&canonical, KEY).expect("renders with report");
        assert_eq!(rendered.value.input.len(), 2);
        assert_eq!(
            rendered.report.losses(),
            &[
                TranslationLoss::new(
                    "$.messages[].content[].cache_control",
                    TranslationLossReason::IncompatibleExtensionDialect,
                    1,
                ),
                TranslationLoss::new(
                    "messages[].blocks[].tool_result.is_error",
                    TranslationLossReason::UnsupportedByBinding,
                    1,
                ),
                TranslationLoss::new(
                    "$.messages[].content[].future_field",
                    TranslationLossReason::IncompatibleExtensionDialect,
                    1,
                ),
                TranslationLoss::new(
                    "$.tools[].cache_control",
                    TranslationLossReason::IncompatibleExtensionDialect,
                    1,
                ),
            ]
        );
        let report = format!("{:?}", rendered.report);
        assert!(!report.contains("cache-value"));
        assert!(!report.contains("future-value"));
        assert!(!report.contains("tool-cache-value"));
    }

    #[test]
    fn a_midstream_system_message_merges_into_the_preceding_user_turn() {
        // The capability declaration the merge enforces: the codex
        // backend refuses system-role input items (live-verified:
        // "System messages are not allowed"). The transform: merge
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
            system: vec![CanonSystemPart::text("Base prompt.")],
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
                    extensions: Vec::new(),
                },
                CanonTool {
                    name: "list_dir".to_owned(),
                    description: "List a directory".to_owned(),
                    parameters: json!({"type": "object", "properties": {}}),
                    extensions: Vec::new(),
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
                thinking: Some(ThinkingSpec::BudgetTokens(budget)),
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
            thinking: Some(ThinkingSpec::BudgetTokens(20_000)),
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

        // Responses ingress already speaks in effort tiers. They cross this
        // same-dialect boundary verbatim, including a future tier this
        // adapter has not learned; converting it through a token budget would
        // be an unsupported guess.
        let native = CanonicalRequest {
            thinking: Some(ThinkingSpec::Effort("xhigh".to_owned())),
            model: Some(MODEL.to_owned()),
            ..CanonicalRequest::default()
        };
        assert_eq!(
            render_codex(&native, KEY)
                .expect("native effort renders")
                .value
                .reasoning
                .effort
                .as_deref(),
            Some("xhigh")
        );
    }

    // ── the response direction: interpretation ────────────────────

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_sse")
    }

    fn fixture(name: &str) -> Vec<u8> {
        fs::read(fixtures_dir().join(name)).expect("fixture exists")
    }

    /// Parse a fixture's whole event stream (unit A's parser first —
    /// the same plumbing the server wires), then interpret every
    /// event through one fresh canon stream.
    fn canon_of(bytes: &[u8]) -> Vec<CanonEvent> {
        let mut parser = ResponsesSse::new();
        let mut events = parser.feed(bytes);
        if let Some(tail) = parser.finish() {
            events.push(tail);
        }
        let mut stream = CanonStream::new();
        let mut canon = Vec::new();
        for event in &events {
            canon.extend(stream.feed(event));
        }
        canon
    }

    #[test]
    fn the_tool_call_fixture_interprets_to_the_canon_event_sequence() {
        // The ignorable kinds (output_item.added, the function_call
        // argument deltas) produce nothing; the message's done is
        // the text part's end; the call arrives complete.
        assert_eq!(
            canon_of(&fixture("01_tool_call_turn.sse")),
            vec![
                CanonEvent::TurnStarted {
                    turn_id: Some("resp_6f3c9a".to_owned()),
                },
                CanonEvent::ThinkingDelta {
                    part: 0,
                    delta: "Reading the ".to_owned(),
                },
                CanonEvent::ThinkingDelta {
                    part: 0,
                    delta: "thread files.".to_owned(),
                },
                CanonEvent::TextDelta {
                    delta: "I'll read the files, then ".to_owned(),
                },
                CanonEvent::TextDelta {
                    delta: "café.".to_owned(),
                },
                CanonEvent::TextEnded,
                CanonEvent::ToolCall(CanonToolCall {
                    id: "call_read1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: r#"{"path":"src/main.rs"}"#.to_owned(),
                }),
                CanonEvent::TurnEnded {
                    stop_reason: CanonStopReason::ToolUse,
                    usage: Some(CanonicalUsage {
                        input: Some(1234),
                        cache_read: Some(512),
                        cache_write: Some(64),
                        output: Some(210),
                        reasoning: Some(96),
                        serving_provider: None,
                        raw: json!({
                            "input_tokens": 1234,
                            "input_tokens_details": {"cached_tokens": 512, "cache_write_tokens": 64},
                            "output_tokens": 210,
                            "output_tokens_details": {"reasoning_tokens": 96},
                            "total_tokens": 1444,
                        }),
                    }),
                },
            ],
            "the fixture's whole turn, interpreted"
        );
    }

    #[test]
    fn the_incomplete_fixture_interprets_to_an_incomplete_turn_ended() {
        // The CRLF-framed fixture parses (unit A) and interprets
        // identically to an LF stream; the message item never
        // completed, so there is no TextEnded — the turn's own end
        // is the boundary. The incomplete reason carries, and no
        // usage: the event carried none (absence ≠ zero).
        assert_eq!(
            canon_of(&fixture("02_incomplete_crlf.sse")),
            vec![
                CanonEvent::TurnStarted {
                    turn_id: Some("resp_trunc".to_owned()),
                },
                CanonEvent::TextDelta {
                    delta: "A partial answer runs out of room when the ".to_owned(),
                },
                CanonEvent::TextDelta {
                    delta: "output budget is spent.".to_owned(),
                },
                CanonEvent::TurnEnded {
                    stop_reason: CanonStopReason::Incomplete("max_output_tokens".to_owned()),
                    usage: None,
                },
            ]
        );
    }

    #[test]
    fn the_failed_fixture_interprets_both_errors_to_canon_errors() {
        // The mid-turn error event is non-terminal in the canon too;
        // response.failed is. The first latches on the capture side
        // of unit A — this layer reports both, in arrival order.
        assert_eq!(
            canon_of(&fixture("03_failed.sse")),
            vec![
                CanonEvent::TurnStarted {
                    turn_id: Some("resp_fail".to_owned()),
                },
                CanonEvent::Error {
                    error: CanonError {
                        kind: CanonErrorKind::Api,
                        message: "Upstream overloaded.".to_owned(),
                        resets_at: None,
                    },
                },
                CanonEvent::TurnFailed {
                    error: CanonError {
                        kind: CanonErrorKind::RateLimit,
                        message: "Rate limit reached for gpt-5.2-codex on weekly limits. \
                             Please try again in 900s."
                            .to_owned(),
                        resets_at: Some(1_800_000_900),
                    },
                },
            ]
        );
    }

    #[test]
    fn each_event_kind_interprets_to_its_canon_shape() {
        let mut stream = CanonStream::new();
        // created: the response id verbatim, when named — never
        // invented.
        assert_eq!(
            stream.feed(&ResponseEvent::Created {
                response_id: None,
                model: Some("gpt-5.2-codex".to_owned()),
            }),
            vec![CanonEvent::TurnStarted { turn_id: None }]
        );
        assert_eq!(
            stream.feed(&ResponseEvent::Created {
                response_id: Some("resp_1".to_owned()),
                model: None,
            }),
            vec![CanonEvent::TurnStarted {
                turn_id: Some("resp_1".to_owned())
            }],
            "every created says so; the FRONTEND renders start once"
        );
        assert_eq!(
            stream.feed(&ResponseEvent::OutputTextDelta {
                delta: "text".to_owned()
            }),
            vec![CanonEvent::TextDelta {
                delta: "text".to_owned()
            }]
        );
        // The summary index is the part identity.
        assert_eq!(
            stream.feed(&ResponseEvent::ReasoningSummaryDelta {
                delta: "thought".to_owned(),
                summary_index: 2,
            }),
            vec![CanonEvent::ThinkingDelta {
                part: 2,
                delta: "thought".to_owned(),
            }]
        );
        // Item dones: message → the text part ended, reasoning →
        // the thinking part ended, unknown kinds → nothing.
        assert_eq!(
            stream.feed(&ResponseEvent::OutputItemDone {
                item: Item::message("assistant", vec![ContentPart::output_text("text")]),
            }),
            vec![CanonEvent::TextEnded]
        );
        assert_eq!(
            stream.feed(&ResponseEvent::OutputItemDone {
                item: Item::reasoning(vec!["thought".to_owned()], None),
            }),
            vec![CanonEvent::ThinkingEnded]
        );
        assert_eq!(
            stream.feed(&ResponseEvent::OutputItemDone {
                item: Item(json!({"type": "web_search_call", "id": "ws_1"})),
            }),
            Vec::<CanonEvent>::new()
        );
        // A function_call whose fields do not fit the typed view is
        // skipped, never corrupted — and does not count as a call.
        assert_eq!(
            stream.feed(&ResponseEvent::OutputItemDone {
                item: Item(json!({"type": "function_call", "name": 5})),
            }),
            Vec::<CanonEvent>::new()
        );
        assert_eq!(
            stream.feed(&ResponseEvent::Completed {
                response: CompletedResponse::default(),
            }),
            vec![CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::EndTurn,
                usage: None,
            }],
            "the skipped call does not read as tool_use"
        );
        // The unmappable kinds: nothing.
        assert_eq!(
            stream.feed(&ResponseEvent::OutputItemAdded {
                item: Item::message("assistant", vec![]),
            }),
            Vec::<CanonEvent>::new()
        );
        assert_eq!(
            stream.feed(&ResponseEvent::Unknown {
                kind: "response.new_thing".to_owned(),
                data: json!({"weird": true}),
            }),
            Vec::<CanonEvent>::new()
        );
    }

    #[test]
    fn a_completed_turn_reads_its_stop_reason_from_the_items() {
        let call = || ResponseEvent::OutputItemDone {
            item: Item::function_call("read_file", r#"{"a":1}"#, "call_1"),
        };
        // No call: the natural end.
        let mut stream = CanonStream::new();
        assert_eq!(
            stream.feed(&ResponseEvent::Completed {
                response: CompletedResponse::default(),
            }),
            vec![CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::EndTurn,
                usage: None,
            }]
        );
        // One call: tool_use, with the usage folded.
        let mut stream = CanonStream::new();
        stream.feed(&call());
        assert_eq!(
            stream.feed(&ResponseEvent::Completed {
                response: CompletedResponse {
                    id: Some("resp_1".to_owned()),
                    usage: Some(
                        serde_json::from_value(json!({
                            "input_tokens": 10,
                            "input_tokens_details": {"cached_tokens": 4},
                            "output_tokens": 5,
                            "total_tokens": 15,
                        }))
                        .expect("usage parses"),
                    ),
                    end_turn: Some(false),
                },
            }),
            vec![CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::ToolUse,
                // The usage buckets: the cached tokens read, the
                // cache write absent (absence ≠ zero), the reasoning
                // absent, the raw carried verbatim.
                usage: Some(CanonicalUsage {
                    input: Some(10),
                    cache_read: Some(4),
                    cache_write: None,
                    output: Some(5),
                    reasoning: None,
                    serving_provider: None,
                    raw: json!({
                        "input_tokens": 10,
                        "input_tokens_details": {"cached_tokens": 4},
                        "output_tokens": 5,
                        // The absent detail serialises as the wire's
                        // explicit null (unit A's own shape), not an
                        // omitted key.
                        "output_tokens_details": null,
                        "total_tokens": 15,
                    }),
                }),
            }]
        );
    }

    #[test]
    fn the_incomplete_reasons_map_to_the_canonical_stop_reasons() {
        let mut stream = CanonStream::new();
        assert_eq!(
            stream.feed(&ResponseEvent::Incomplete {
                reason: Some("content_filter".to_owned()),
                usage: None,
            }),
            vec![CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::Refusal,
                usage: None,
            }],
            "content_filter is the refusal"
        );
        for (reason, expected) in [
            (
                Some("max_output_tokens"),
                CanonStopReason::Incomplete("max_output_tokens".to_owned()),
            ),
            (
                Some("something_new"),
                CanonStopReason::Incomplete("something_new".to_owned()),
            ),
            (None, CanonStopReason::Incomplete(String::new())),
        ] {
            let mut stream = CanonStream::new();
            assert_eq!(
                stream.feed(&ResponseEvent::Incomplete {
                    reason: reason.map(str::to_owned),
                    usage: None,
                }),
                vec![CanonEvent::TurnEnded {
                    stop_reason: expected,
                    usage: None,
                }],
                "the incomplete reason carries: {reason:?}"
            );
        }
    }

    #[test]
    fn the_error_table_interprets_to_canon_errors() {
        let error = |kind: Option<&str>,
                     code: Option<&str>,
                     message: Option<&str>,
                     resets_at: Option<i64>| ResponseError {
            kind: kind.map(str::to_owned),
            code: code.map(str::to_owned),
            message: message.map(str::to_owned),
            resets_at,
        };
        let canon = |error: ResponseError| canon_error_from_response(&error);

        // rate_limit codes → RateLimit, the reset riding when carried.
        assert_eq!(
            canon(error(
                None,
                Some("rate_limit_exceeded"),
                Some("Rate limit reached."),
                Some(1_800_000_900)
            )),
            CanonError {
                kind: CanonErrorKind::RateLimit,
                message: "Rate limit reached.".to_owned(),
                resets_at: Some(1_800_000_900),
            }
        );
        assert_eq!(
            canon(error(
                None,
                Some("rate_limit_exceeded"),
                Some("Rate limit reached."),
                None
            ))
            .resets_at,
            None
        );
        // Case spellings still match (the code is lowercased first).
        assert_eq!(
            canon(error(None, Some("RATE_LIMIT_EXCEEDED"), Some("m"), None)).kind,
            CanonErrorKind::RateLimit
        );
        // The kind alone maps too.
        assert_eq!(
            canon(error(Some("rate_limit_error"), None, Some("m"), None)).kind,
            CanonErrorKind::RateLimit
        );
        // Context length and quota/usage limits → InvalidRequest.
        assert_eq!(
            canon(error(
                None,
                Some("context_length_exceeded"),
                Some("m"),
                None
            ))
            .kind,
            CanonErrorKind::InvalidRequest
        );
        assert_eq!(
            canon(error(None, Some("usage_limit_reached"), Some("m"), None)).kind,
            CanonErrorKind::InvalidRequest
        );
        assert_eq!(
            canon(error(None, Some("monthly_quota_reached"), Some("m"), None)).kind,
            CanonErrorKind::InvalidRequest
        );
        // Kinds that are already anthropic type names → the typed
        // variant that renders them.
        assert_eq!(
            canon(error(Some("invalid_request_error"), None, Some("m"), None)).kind,
            CanonErrorKind::InvalidRequest
        );
        assert_eq!(
            canon(error(Some("overloaded_error"), None, Some("m"), None)).kind,
            CanonErrorKind::Overloaded
        );
        assert_eq!(
            canon(error(None, Some("server_error"), Some("m"), None)).kind,
            CanonErrorKind::Api
        );
        // The message stands in on the code, then the kind, then the
        // constant — resolved HERE, on the backend's own fields.
        assert_eq!(
            canon(error(None, Some("server_error"), None, None)).message,
            "server_error"
        );
        assert_eq!(
            canon(error(Some("overloaded_error"), None, None, None)),
            CanonError {
                kind: CanonErrorKind::Overloaded,
                message: "overloaded_error".to_owned(),
                resets_at: None,
            }
        );
        assert_eq!(
            canon(ResponseError::default()),
            CanonError {
                kind: CanonErrorKind::Api,
                message: "upstream error".to_owned(),
                resets_at: None,
            }
        );
    }

    #[test]
    fn the_capture_folds_to_the_canonical_turn() {
        let capture_of = |name: &str| {
            let mut parser = ResponsesSse::new();
            let mut events = parser.feed(&fixture(name));
            if let Some(tail) = parser.finish() {
                events.push(tail);
            }
            let mut capture = TurnCapture::new();
            for event in &events {
                capture.observe(event);
            }
            capture
        };

        // The tool-call fixture: everything the turn was.
        assert_eq!(
            canonical_turn_from_capture(&capture_of("01_tool_call_turn.sse")),
            CanonTurn {
                turn_id: Some("resp_6f3c9a".to_owned()),
                stop_reason: CanonStopReason::ToolUse,
                usage: Some(CanonicalUsage {
                    input: Some(1234),
                    cache_read: Some(512),
                    cache_write: Some(64),
                    output: Some(210),
                    reasoning: Some(96),
                    serving_provider: None,
                    raw: json!({
                        "input_tokens": 1234,
                        "input_tokens_details": {"cached_tokens": 512, "cache_write_tokens": 64},
                        "output_tokens": 210,
                        "output_tokens_details": {"reasoning_tokens": 96},
                        "total_tokens": 1444,
                    }),
                }),
                error: None,
                tool_calls: vec![CanonToolCall {
                    id: "call_read1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: r#"{"path":"src/main.rs"}"#.to_owned(),
                }],
                blocks: None,
                text: "I'll read the files, then café.".to_owned(),
                thinking: [(0, "Reading the thread files.".to_owned())].into(),
            }
        );

        // The incomplete fixture: the reason carried, no usage.
        let turn = canonical_turn_from_capture(&capture_of("02_incomplete_crlf.sse"));
        assert_eq!(turn.turn_id.as_deref(), Some("resp_trunc"));
        assert_eq!(
            turn.stop_reason,
            CanonStopReason::Incomplete("max_output_tokens".to_owned())
        );
        assert_eq!(turn.usage, None);
        assert_eq!(
            turn.text,
            "A partial answer runs out of room when the output budget is spent."
        );

        // The failed fixture: the FIRST error latched (unit A), read
        // as the canonical error.
        let turn = canonical_turn_from_capture(&capture_of("03_failed.sse"));
        assert_eq!(
            turn.error,
            Some(CanonError {
                kind: CanonErrorKind::Api,
                message: "Upstream overloaded.".to_owned(),
                resets_at: None,
            })
        );

        // An empty capture: nothing invented — no id, the natural
        // end, no usage, no content.
        assert_eq!(
            canonical_turn_from_capture(&TurnCapture::new()),
            CanonTurn {
                turn_id: None,
                stop_reason: CanonStopReason::EndTurn,
                usage: None,
                error: None,
                tool_calls: Vec::new(),
                blocks: None,
                text: String::new(),
                thinking: [].into(),
            }
        );
    }
}
