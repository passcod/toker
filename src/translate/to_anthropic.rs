//! The response direction: a responses-dialect turn → Anthropic SSE
//! (the events table lives in the parent module's docs).
//!
//! [`AnthropicStream`] is a small explicit state machine over unit A's
//! typed [`ResponseEvent`]s: which content block is open (thinking or
//! text, at which index), the next block index, whether any function
//! call completed, whether the turn ended. Everything it emits is a
//! pure function of the events it has been fed and the model echo —
//! no clock, no counters, nothing else (invariant 4). Claude Code's
//! event names, exactly: `message_start`, `content_block_start`,
//! `content_block_delta`, `content_block_stop`, `message_delta`,
//! `message_stop`, `error` — no `ping` (claude tolerates its absence;
//! nothing upstream produces one).
//!
//! The message id is the codex response id, verbatim — the one
//! identity the turn actually has. A stream that never named one gets
//! the constant `"msg"`: purity forbids minting a fresh id (a random or
//! clock-derived id would break byte-stability for nothing).
//!
//! The `message_start` usage is **zeroed** — anthropic's own shape for
//! a provisional snapshot, which this is: the real usage only exists
//! at the turn's end and rides the `message_delta`. A turn that ends
//! without one (`response.incomplete` carries no usage) leaves the
//! zeros provisional, exactly like anthropic's own start usage; the
//! authoritative `message_delta` usage replaces it whenever it arrives.
//!
//! [`message_from_capture`] is the `stream:false` sibling: the same
//! content assembly over unit A's [`TurnCapture`] (which the routing
//! unit already folds for its own accounting), producing the complete
//! non-streaming Anthropic message JSON. Its content order is
//! categorical — thinking summaries (by index), text, then the tool
//! calls (in item order) — the capture keeps no cross-category arrival
//! order; for real turns the categories arrive in exactly that order
//! anyway, and the SSE path emits true arrival order for whatever
//! interleaving the upstream sends.

use serde_json::{Map, Value, json};

use crate::observe::sse::SseEvent;
use crate::providers::codex::{Item, ResponseError, ResponseEvent, TurnCapture, Usage};

/// The message id for a stream that never named one — a constant, not
/// a minted id (purity).
const UNNAMED_MESSAGE: &str = "msg";

/// The Anthropic SSE state machine for one streamed turn: feed it the
/// turn's [`ResponseEvent`]s (in arrival order), collect the emitted
/// [`SseEvent`]s. One stream per turn.
#[derive(Debug, Clone)]
pub struct AnthropicStream {
    /// The model echo — the model the caller wants the client to see
    /// (the requested anthropic model, post-middleware).
    model: String,
    /// Whether `response.created` passed through (message_start is
    /// emitted exactly once).
    started: bool,
    /// The codex response id, latched from `response.created`.
    message_id: Option<String>,
    /// The index the next opened content block gets.
    next_index: usize,
    /// The currently open content block, if any.
    open: Option<OpenBlock>,
    /// Whether any `function_call` item completed (the tool_use stop
    /// reason).
    function_calls: bool,
    /// Whether the turn's final-state event passed through.
    ended: bool,
}

/// The content block under assembly: its kind (which stream feeds it)
/// and its index.
#[derive(Debug, Clone, Copy)]
enum OpenBlock {
    Thinking { index: usize, summary_index: i64 },
    Text { index: usize },
}

/// The identity of a content block: text continues text; a summary
/// continues the thinking block of the same summary index.
#[derive(Debug, Clone, Copy)]
enum BlockKind {
    Thinking { summary_index: i64 },
    Text,
}

impl OpenBlock {
    /// Whether `other` continues this block.
    fn continues(&self, other: &BlockKind) -> bool {
        match (self, other) {
            (OpenBlock::Text { .. }, BlockKind::Text) => true,
            (
                OpenBlock::Thinking {
                    summary_index: a, ..
                },
                BlockKind::Thinking { summary_index: b },
            ) => a == b,
            _ => false,
        }
    }

    fn index(&self) -> usize {
        match self {
            OpenBlock::Thinking { index, .. } | OpenBlock::Text { index } => *index,
        }
    }
}

impl AnthropicStream {
    /// A fresh stream for one turn, echoing `model` in the
    /// `message_start` (the streaming path — non-streaming requests go
    /// through [`message_from_capture`]).
    pub fn new(model: &str) -> AnthropicStream {
        AnthropicStream {
            model: model.to_owned(),
            started: false,
            message_id: None,
            next_index: 0,
            open: None,
            function_calls: false,
            ended: false,
        }
    }

    /// Feed one Responses event; every Anthropic SSE event it
    /// produced, in anthropic event order. Never fails — an event
    /// with no anthropic shape (an `output_item.added`, an unknown
    /// kind) produces nothing, and a `function_call` whose fields do
    /// not fit the typed view is skipped rather than corrupting the
    /// stream (invariant 6).
    pub fn feed(&mut self, event: &ResponseEvent) -> Vec<SseEvent> {
        let mut out = Vec::new();
        match event {
            ResponseEvent::Created { response_id, .. } => {
                if !self.started {
                    self.started = true;
                    if self.message_id.is_none() {
                        self.message_id.clone_from(response_id);
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
            ResponseEvent::OutputTextDelta { delta } => {
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
            ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            } => {
                let block = BlockKind::Thinking {
                    summary_index: *summary_index,
                };
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
            ResponseEvent::OutputItemDone { item } => self.item_done(item, &mut out),
            ResponseEvent::Completed { response } => {
                self.close_open(&mut out);
                let stop_reason = if self.function_calls {
                    "tool_use"
                } else {
                    "end_turn"
                };
                let mut data = Map::new();
                data.insert("type".to_owned(), json!("message_delta"));
                data.insert(
                    "delta".to_owned(),
                    json!({"stop_reason": stop_reason, "stop_sequence": null}),
                );
                if let Some(usage) = &response.usage {
                    data.insert("usage".to_owned(), usage_json(usage));
                }
                out.push(sse_event("message_delta", Value::Object(data)));
                out.push(sse_event("message_stop", json!({"type": "message_stop"})));
                self.ended = true;
            }
            ResponseEvent::Incomplete { reason } => {
                self.close_open(&mut out);
                out.push(sse_event(
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": {
                            "stop_reason": incomplete_stop_reason(reason.as_deref()),
                            "stop_sequence": null,
                        },
                    }),
                ));
                out.push(sse_event("message_stop", json!({"type": "message_stop"})));
                self.ended = true;
            }
            ResponseEvent::Failed { error } => {
                // No message_delta, no message_stop: anthropic error
                // streams end at the error, mid-block if one was open
                // (toker's own captured fixture 06 is the shape).
                out.push(sse_event("error", error_event_data(error)));
                self.ended = true;
            }
            ResponseEvent::Error { error } => {
                out.push(sse_event("error", error_event_data(error)));
            }
            ResponseEvent::OutputItemAdded { .. } | ResponseEvent::Unknown { .. } => {}
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
            BlockKind::Thinking { summary_index } => OpenBlock::Thinking {
                index,
                summary_index,
            },
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

    /// One `output_item.done`: a message item closes the block it was
    /// streaming (its text already went out via the deltas); a
    /// function-call item becomes a complete tool_use block — the
    /// arguments arrive whole here (unit A's parser buffers them from
    /// the done item), so ONE input_json_delta carries them all;
    /// a reasoning item closes a thinking block if one is open.
    fn item_done(&mut self, item: &Item, out: &mut Vec<SseEvent>) {
        match item.kind() {
            Some("message") => self.close_open(out),
            Some("reasoning") => {
                if matches!(self.open, Some(OpenBlock::Thinking { .. })) {
                    self.close_open(out);
                }
            }
            Some("function_call") => {
                if let Some(call) = item.as_function_call() {
                    self.close_open(out);
                    let index = self.next_index;
                    self.next_index += 1;
                    self.function_calls = true;
                    out.push(sse_event(
                        "content_block_start",
                        json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {
                                "type": "tool_use",
                                "id": call.call_id,
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
            }
            _ => {}
        }
    }

    fn message_id_or_default(&self) -> &str {
        self.message_id.as_deref().unwrap_or(UNNAMED_MESSAGE)
    }
}

/// The `stream:false` sibling: the whole turn (unit A's
/// [`TurnCapture`]) → the complete non-streaming Anthropic message
/// JSON — the same content assembly and usage mapping as the streamed
/// path. An errored turn yields the anthropic error body instead (no
/// content: anthropic's non-streaming errors carry none).
pub fn message_from_capture(model: &str, capture: &TurnCapture) -> Value {
    if let Some(error) = capture.error() {
        return error_event_data(error);
    }

    let mut content: Vec<Value> = Vec::new();
    for summary in capture.reasoning_summaries().values() {
        content.push(json!({"type": "thinking", "thinking": summary}));
    }
    if !capture.text().is_empty() {
        content.push(json!({"type": "text", "text": capture.text()}));
    }
    for call in capture.function_calls() {
        // The arguments are a JSON string on the responses wire;
        // anthropic's tool_use.input is the object. A string that
        // does not parse is a degraded upstream — the empty object
        // keeps the shape valid rather than inventing content.
        let input: Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
        content.push(json!({
            "type": "tool_use",
            "id": call.call_id,
            "name": call.name,
            "input": input,
        }));
    }

    let stop_reason = if capture.incomplete_reason().is_some() {
        incomplete_stop_reason(capture.incomplete_reason())
    } else if !capture.function_calls().is_empty() {
        "tool_use"
    } else {
        "end_turn"
    };

    let mut message = Map::new();
    message.insert(
        "id".to_owned(),
        json!(capture.response_id().unwrap_or(UNNAMED_MESSAGE)),
    );
    message.insert("type".to_owned(), json!("message"));
    message.insert("role".to_owned(), json!("assistant"));
    message.insert("model".to_owned(), json!(model));
    message.insert("content".to_owned(), Value::Array(content));
    message.insert("stop_reason".to_owned(), json!(stop_reason));
    message.insert("stop_sequence".to_owned(), Value::Null);
    if let Some(usage) = capture.usage() {
        message.insert("usage".to_owned(), usage_json(usage));
    }
    Value::Object(message)
}

// ── the mapping helpers ────────────────────────────────────────────

/// `response.incomplete`'s reason → anthropic's stop reason:
/// `content_filter` is the refusal; everything else (known
/// `max_output_tokens` and unknown alike) stopped at the budget —
/// `max_tokens`, the only budget-shaped anthropic stop reason.
fn incomplete_stop_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("content_filter") => "refusal",
        _ => "max_tokens",
    }
}

/// Responses usage → the anthropic field names (the usage table).
/// Absent stays absent (invariant 3); reasoning tokens ride inside
/// `output_tokens` on both protocols and are not broken out (the
/// parent module's translation note).
fn usage_json(usage: &Usage) -> Value {
    let mut map = Map::new();
    map.insert("input_tokens".to_owned(), json!(usage.input_tokens));
    if let Some(details) = &usage.input_tokens_details {
        if let Some(cache_write) = details.cache_write_tokens {
            map.insert("cache_creation_input_tokens".to_owned(), json!(cache_write));
        }
        if let Some(cached) = details.cached_tokens {
            map.insert("cache_read_input_tokens".to_owned(), json!(cached));
        }
    }
    map.insert("output_tokens".to_owned(), json!(usage.output_tokens));
    Value::Object(map)
}

/// A Responses error payload → the anthropic error event's data JSON
/// (the error table). The message passes through verbatim, standing in
/// on the code, then the kind, then the constant — never invented.
fn error_event_data(error: &ResponseError) -> Value {
    let kind = anthropic_error_type(error);
    let message = error
        .message
        .clone()
        .or_else(|| error.code.clone())
        .or_else(|| error.kind.clone())
        .unwrap_or_else(|| "upstream error".to_owned());
    let mut error_object = Map::new();
    error_object.insert("type".to_owned(), json!(kind));
    error_object.insert("message".to_owned(), json!(message));
    if kind == "rate_limit_error"
        && let Some(resets_at) = error.resets_at
    {
        // The absolute reset epoch, verbatim — a relative
        // retry-after would need a clock, and this is pure.
        error_object.insert("retry_after".to_owned(), json!(resets_at));
    }
    json!({"type": "error", "error": Value::Object(error_object)})
}

/// The error table: `code` first, then `kind`; first match wins.
fn anthropic_error_type(error: &ResponseError) -> &'static str {
    let code = error.code.as_deref().unwrap_or("").to_ascii_lowercase();
    if code.contains("rate_limit") {
        return "rate_limit_error";
    }
    if code.contains("context_length") {
        return "invalid_request_error";
    }
    if code.contains("quota") || code.contains("usage_limit") {
        return "invalid_request_error";
    }
    match error.kind.as_deref() {
        // Kinds that are already anthropic type names — verbatim.
        Some("rate_limit_error") => "rate_limit_error",
        Some("invalid_request_error") => "invalid_request_error",
        Some("authentication_error") => "authentication_error",
        Some("permission_error") => "permission_error",
        Some("not_found_error") => "not_found_error",
        Some("request_too_large") => "request_too_large",
        Some("overloaded_error") => "overloaded_error",
        // Anything else the upstream called itself — anthropic's generic.
        _ => "api_error",
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
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::super::to_anthropic::{AnthropicStream, message_from_capture};
    use crate::observe::sse::SseEvent;
    use crate::providers::codex::{
        CompletedResponse, ContentPart, Item, ResponseError, ResponseEvent, ResponsesSse,
        TurnCapture,
    };
    use serde_json::{Value, json};

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_sse")
    }

    fn fixture(name: &str) -> Vec<u8> {
        fs::read(fixtures_dir().join(name)).expect("fixture exists")
    }

    /// Parse a fixture's whole event stream (the unit under test is
    /// fed unit A's typed events, so fixtures go through unit A's
    /// parser first — the same plumbing unit C will wire).
    fn events_of(bytes: &[u8]) -> Vec<ResponseEvent> {
        let mut parser = ResponsesSse::new();
        let mut events = parser.feed(bytes);
        if let Some(tail) = parser.finish() {
            events.push(tail);
        }
        events
    }

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

    /// Feed every event through one fresh stream; the rendered wire
    /// bytes.
    fn stream_bytes(model: &str, events: &[ResponseEvent]) -> String {
        let mut stream = AnthropicStream::new(model);
        let mut out = String::new();
        for event in events {
            for emitted in stream.feed(event) {
                out.push_str(&render_one(&emitted));
            }
        }
        out
    }

    /// The expected wire bytes of an (event name, data JSON) sequence.
    fn wire(pairs: &[(&str, &str)]) -> String {
        pairs
            .iter()
            .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
            .collect()
    }

    /// The start of message_start's message object, for the pins.
    fn message_start(id: &str, model: &str) -> String {
        format!(
            r#"{{"type":"message_start","message":{{"id":"{id}","type":"message","role":"assistant","model":"{model}","content":[],"usage":{{"input_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0}}}}}}"#
        )
    }

    #[test]
    fn the_tool_call_turn_pins_the_emitted_sse_bytes() {
        let events = events_of(&fixture("01_tool_call_turn.sse"));
        let expected = wire(&[
            (
                "message_start",
                &message_start("resp_6f3c9a", "claude-opus-5"),
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Reading the "}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"thread files."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"I'll read the files, then "}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"café."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_read1","name":"read_file","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src/main.rs\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":2}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":1234,"cache_creation_input_tokens":64,"cache_read_input_tokens":512,"output_tokens":210}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let mut stream = AnthropicStream::new("claude-opus-5");
        let mut rendered = String::new();
        for event in &events {
            for emitted in stream.feed(event) {
                rendered.push_str(&render_one(&emitted));
            }
        }
        assert_eq!(rendered, expected, "byte-pinned anthropic SSE");
        assert!(stream.turn_ended());
        assert!(
            !rendered.contains("ping"),
            "claude tolerates no ping; none is emitted"
        );
    }

    #[test]
    fn the_incomplete_turn_pins_the_emitted_sse_bytes() {
        let events = events_of(&fixture("02_incomplete_crlf.sse"));
        let expected = wire(&[
            (
                "message_start",
                &message_start("resp_trunc", "claude-opus-5"),
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"A partial answer runs out of room when the "}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"output budget is spent."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            // No usage key: response.incomplete carried none (absence ≠
            // zero, invariant 3).
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        assert_eq!(stream_bytes("claude-opus-5", &events), expected);
        let mut stream = AnthropicStream::new("claude-opus-5");
        for event in &events {
            stream.feed(event);
        }
        assert!(stream.turn_ended());
    }

    #[test]
    fn the_failed_turn_pins_the_error_events() {
        // Both the mid-turn `error` event and the terminal
        // `response.failed` map to anthropic error events, in arrival
        // order (the state machine reports; unit C decides policy).
        // No message_delta, no message_stop — anthropic error streams
        // end at the error.
        let events = events_of(&fixture("03_failed.sse"));
        let expected = wire(&[
            (
                "message_start",
                &message_start("resp_fail", "claude-opus-5"),
            ),
            (
                "error",
                r#"{"type":"error","error":{"type":"api_error","message":"Upstream overloaded."}}"#,
            ),
            (
                "error",
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"Rate limit reached for gpt-5.2-codex on weekly limits. Please try again in 900s.","retry_after":1800000900}}"#,
            ),
        ]);
        assert_eq!(stream_bytes("claude-opus-5", &events), expected);
    }

    #[test]
    fn text_after_tool_use_takes_the_next_index() {
        let events = vec![
            ResponseEvent::Created {
                response_id: Some("resp_1".to_owned()),
                model: None,
            },
            ResponseEvent::OutputTextDelta {
                delta: "Part one.".to_owned(),
            },
            ResponseEvent::OutputItemDone {
                item: Item::message("assistant", vec![ContentPart::output_text("Part one.")]),
            },
            ResponseEvent::OutputItemDone {
                item: Item::function_call("read_file", r#"{"a":1}"#, "call_1"),
            },
            ResponseEvent::OutputTextDelta {
                delta: "Part two.".to_owned(),
            },
            ResponseEvent::OutputItemDone {
                item: Item::message("assistant", vec![ContentPart::output_text("Part two.")]),
            },
            ResponseEvent::Completed {
                response: CompletedResponse {
                    id: Some("resp_1".to_owned()),
                    usage: None,
                    end_turn: None,
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
            ResponseEvent::Created {
                response_id: Some("resp_1".to_owned()),
                model: None,
            },
            ResponseEvent::OutputTextDelta {
                delta: "First.".to_owned(),
            },
            ResponseEvent::ReasoningSummaryDelta {
                delta: "Thought.".to_owned(),
                summary_index: 0,
            },
            ResponseEvent::OutputTextDelta {
                delta: "Last.".to_owned(),
            },
            ResponseEvent::Completed {
                response: CompletedResponse::default(),
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
            ResponseEvent::Created {
                response_id: Some("resp_1".to_owned()),
                model: None,
            },
            ResponseEvent::ReasoningSummaryDelta {
                delta: "A".to_owned(),
                summary_index: 0,
            },
            ResponseEvent::ReasoningSummaryDelta {
                delta: "B".to_owned(),
                summary_index: 0,
            },
            ResponseEvent::ReasoningSummaryDelta {
                delta: "C".to_owned(),
                summary_index: 1,
            },
            ResponseEvent::Completed {
                response: CompletedResponse::default(),
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
    fn a_mid_stream_error_does_not_close_the_open_block() {
        // toker's own captured fixture 06 is the shape: anthropic error
        // events cut the stream mid-block, no content_block_stop, no
        // message_delta, no message_stop.
        let events = vec![
            ResponseEvent::Created {
                response_id: Some("resp_1".to_owned()),
                model: None,
            },
            ResponseEvent::OutputTextDelta {
                delta: "half a reply".to_owned(),
            },
            ResponseEvent::Error {
                error: ResponseError {
                    kind: Some("overloaded_error".to_owned()),
                    message: Some("Overloaded".to_owned()),
                    ..ResponseError::default()
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
        let mut stream = AnthropicStream::new("claude-opus-5");
        let mut rendered = String::new();
        for event in &events {
            for emitted in stream.feed(event) {
                rendered.push_str(&render_one(&emitted));
            }
        }
        assert_eq!(rendered, expected);
        assert!(
            !stream.turn_ended(),
            "a bare error event is not the turn's end"
        );

        // response.failed IS the turn's end.
        let mut stream = AnthropicStream::new("claude-opus-5");
        stream.feed(&ResponseEvent::Failed {
            error: ResponseError::default(),
        });
        assert!(stream.turn_ended());
    }

    #[test]
    fn the_error_table_maps_best_effort() {
        let error_event = |error: ResponseError| {
            let mut stream = AnthropicStream::new("claude-opus-5");
            let emitted = stream.feed(&ResponseEvent::Error { error });
            assert_eq!(emitted.len(), 1);
            let event = &emitted[0];
            assert_eq!(event.event.as_deref(), Some("error"));
            serde_json::from_str::<Value>(&event.data()).expect("data is JSON")
        };
        let error = |kind: Option<&str>,
                     code: Option<&str>,
                     message: Option<&str>,
                     resets_at: Option<i64>| ResponseError {
            kind: kind.map(str::to_owned),
            code: code.map(str::to_owned),
            message: message.map(str::to_owned),
            resets_at,
        };

        // rate_limit codes → rate_limit_error, with the absolute reset
        // epoch as retry_after when carried.
        assert_eq!(
            error_event(error(
                None,
                Some("rate_limit_exceeded"),
                Some("Rate limit reached."),
                Some(1_800_000_900)
            )),
            json!({"type": "error", "error": {
                "type": "rate_limit_error",
                "message": "Rate limit reached.",
                "retry_after": 1_800_000_900,
            }})
        );
        // No reset time, no retry_after.
        assert_eq!(
            error_event(error(
                None,
                Some("rate_limit_exceeded"),
                Some("Rate limit reached."),
                None
            )),
            json!({"type": "error", "error": {
                "type": "rate_limit_error",
                "message": "Rate limit reached.",
            }})
        );
        // Case spellings still match (the code is lowercased first).
        assert_eq!(
            error_event(error(None, Some("RATE_LIMIT_EXCEEDED"), Some("m"), None))["error"]["type"],
            json!("rate_limit_error")
        );
        // The kind alone maps too.
        assert_eq!(
            error_event(error(Some("rate_limit_error"), None, Some("m"), None))["error"]["type"],
            json!("rate_limit_error")
        );
        // Context length and quota/usage limits → invalid_request_error.
        assert_eq!(
            error_event(error(
                None,
                Some("context_length_exceeded"),
                Some("m"),
                None
            ))["error"]["type"],
            json!("invalid_request_error")
        );
        assert_eq!(
            error_event(error(None, Some("usage_limit_reached"), Some("m"), None))["error"]["type"],
            json!("invalid_request_error")
        );
        assert_eq!(
            error_event(error(None, Some("monthly_quota_reached"), Some("m"), None))["error"]["type"],
            json!("invalid_request_error")
        );
        // Kinds that are already anthropic type names pass verbatim;
        // anything else is anthropic's generic.
        assert_eq!(
            error_event(error(Some("invalid_request_error"), None, Some("m"), None))["error"]["type"],
            json!("invalid_request_error")
        );
        assert_eq!(
            error_event(error(None, Some("server_error"), Some("m"), None))["error"]["type"],
            json!("api_error")
        );
        // A missing message stands in on the code, then the kind, then
        // the constant — never invented.
        assert_eq!(
            error_event(error(None, Some("server_error"), None, None)),
            json!({"type": "error", "error": {"type": "api_error", "message": "server_error"}})
        );
        assert_eq!(
            error_event(error(Some("overloaded_error"), None, None, None)),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "overloaded_error"}})
        );
        assert_eq!(
            error_event(ResponseError::default()),
            json!({"type": "error", "error": {"type": "api_error", "message": "upstream error"}})
        );
    }

    #[test]
    fn unmapped_events_emit_nothing() {
        let mut stream = AnthropicStream::new("claude-opus-5");
        assert!(
            stream
                .feed(&ResponseEvent::OutputItemAdded {
                    item: Item::message("assistant", vec![]),
                })
                .is_empty()
        );
        assert!(
            stream
                .feed(&ResponseEvent::Unknown {
                    kind: "response.new_thing".to_owned(),
                    data: json!({"weird": true}),
                })
                .is_empty()
        );
        assert!(!stream.turn_ended());
        // And a later created still opens the message exactly once.
        let emitted = stream.feed(&ResponseEvent::Created {
            response_id: Some("resp_1".to_owned()),
            model: None,
        });
        assert_eq!(emitted.len(), 1);
        let again = stream.feed(&ResponseEvent::Created {
            response_id: None,
            model: None,
        });
        assert!(again.is_empty(), "message_start is emitted exactly once");
    }

    #[test]
    fn the_aggregated_tool_call_turn_is_pinned() {
        let mut capture = TurnCapture::new();
        for event in events_of(&fixture("01_tool_call_turn.sse")) {
            capture.observe(&event);
        }
        let message = message_from_capture("claude-opus-5", &capture);
        assert_eq!(
            serde_json::to_string(&message).expect("serialise"),
            concat!(
                r#"{"id":"resp_6f3c9a","type":"message","role":"assistant","model":"claude-opus-5","#,
                r#""content":[{"type":"thinking","thinking":"Reading the thread files."},"#,
                r#"{"type":"text","text":"I'll read the files, then café."},"#,
                r#"{"type":"tool_use","id":"call_read1","name":"read_file","input":{"path":"src/main.rs"}}],"#,
                r#""stop_reason":"tool_use","stop_sequence":null,"#,
                r#""usage":{"input_tokens":1234,"cache_creation_input_tokens":64,"cache_read_input_tokens":512,"output_tokens":210}}"#
            ),
            "byte-pinned non-streaming message"
        );
    }

    #[test]
    fn the_aggregated_incomplete_turn_pins_max_tokens_and_no_usage() {
        let mut capture = TurnCapture::new();
        for event in events_of(&fixture("02_incomplete_crlf.sse")) {
            capture.observe(&event);
        }
        let message = message_from_capture("claude-opus-5", &capture);
        assert_eq!(
            message,
            json!({
                "id": "resp_trunc",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-5",
                "content": [
                    {"type": "text",
                     "text": "A partial answer runs out of room when the output budget is spent."},
                ],
                "stop_reason": "max_tokens",
                "stop_sequence": null,
            }),
            "no usage key: the incomplete turn carried none"
        );
    }

    #[test]
    fn an_errored_capture_aggregates_to_the_error_body() {
        let mut capture = TurnCapture::new();
        for event in events_of(&fixture("03_failed.sse")) {
            capture.observe(&event);
        }
        // The FIRST error latches in the capture (unit A); the error
        // body carries it.
        assert_eq!(
            message_from_capture("claude-opus-5", &capture),
            json!({"type": "error",
                   "error": {"type": "api_error", "message": "Upstream overloaded."}})
        );
    }

    #[test]
    fn an_empty_capture_aggregates_to_the_empty_message() {
        // No events at all: the placeholder id (purity forbids minting
        // one), no content, end_turn, no usage key.
        let capture = TurnCapture::new();
        assert_eq!(
            message_from_capture("claude-opus-5", &capture),
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
    fn unparseable_arguments_aggregate_to_the_empty_input_object() {
        let mut capture = TurnCapture::new();
        capture.observe(&ResponseEvent::OutputItemDone {
            item: Item::function_call("read_file", "not json at all", "call_1"),
        });
        capture.observe(&ResponseEvent::Completed {
            response: CompletedResponse::default(),
        });
        let message = message_from_capture("claude-opus-5", &capture);
        assert_eq!(
            message["content"],
            json!([{"type": "tool_use", "id": "call_1", "name": "read_file", "input": {}}])
        );
        assert_eq!(message["stop_reason"], json!("tool_use"));
    }

    #[test]
    fn the_content_filter_reason_maps_to_refusal() {
        let mut stream = AnthropicStream::new("claude-opus-5");
        let mut rendered = String::new();
        for event in [
            ResponseEvent::Created {
                response_id: Some("resp_1".to_owned()),
                model: None,
            },
            ResponseEvent::Incomplete {
                reason: Some("content_filter".to_owned()),
            },
        ] {
            for emitted in stream.feed(&event) {
                rendered.push_str(&render_one(&emitted));
            }
        }
        assert!(rendered.contains(r#""stop_reason":"refusal""#));
        assert!(stream.turn_ended());

        // An unknown reason stops at the budget — max_tokens.
        let mut stream = AnthropicStream::new("claude-opus-5");
        stream.feed(&ResponseEvent::Incomplete { reason: None });
        stream.feed(&ResponseEvent::Incomplete {
            reason: Some("something_new".to_owned()),
        });
        // (only the first incomplete matters; the stream has ended)
        assert!(stream.turn_ended());
    }

    #[test]
    fn streaming_is_pure() {
        let events = events_of(&fixture("01_tool_call_turn.sse"));
        let first = stream_bytes("claude-opus-5", &events);
        for _ in 0..3 {
            assert_eq!(stream_bytes("claude-opus-5", &events), first);
        }
    }
}
