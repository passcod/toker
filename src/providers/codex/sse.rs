//! The responses-dialect SSE parser: typed events off the generic
//! splitter, plus the one-turn capture.
//!
//! The dialect (codex-rs `sse/responses.rs`, the CLI's own reader):
//!
//! - Every event carries an `event:` line naming its kind **and** the
//!   same kind inside the data JSON (`type`). The JSON `type` is
//!   authoritative when the two ever disagree (the codex client reads
//!   the JSON; the `event:` line is the fallback when the data carries
//!   no `type`).
//! - There is **no `[DONE]` sentinel**: a well-formed stream ends at
//!   `response.completed` or `response.incomplete`. The parser never
//!   requires one — [`ResponsesSse::finish`] flushes a trailing event
//!   cut off mid-line, and [`ResponseEvent::ends_turn`] marks the
//!   terminator that did arrive.
//! - Function-call **arguments arrive complete via
//!   `response.output_item.done`** — the `function_call_arguments.delta`
//!   events are ignorable, and the capture takes the done item only.
//!
//! Malformed data — non-JSON payloads, known kinds whose fields do not
//! fit the typed views — degrades to the raw [`serde_json::Value`] on
//! [`ResponseEvent::Unknown`] rather than being dropped (invariant 6:
//! a parse failure loses the typing, never the bytes).
//!
//! [`TurnCapture`] is the observer half (the sibling of
//! [`crate::observe::usage::UsageObserver`] and
//! [`crate::observe::AnthropicObserver`]): fold every typed event of
//! one turn into the response id, the done items, the text, the
//! reasoning summaries, and the usage.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::observe::sse::{SseEvent, SseSplitter};

use super::types::{CompletedResponse, FunctionCall, Item, ResponseError, ResponseEvent, Usage};

/// The incremental responses-dialect parser: feed it response-body
/// chunks, collect typed events. Pure and self-contained — bytes in,
/// [`ResponseEvent`]s out — so the routing unit only wires chunk
/// boundaries to it, and nothing here can fail a stream.
#[derive(Debug, Clone, Default)]
pub struct ResponsesSse {
    /// The generic splitter (line framing, both newline dialects,
    /// UTF-8-per-line) — extended to keep the `event:` field value,
    /// which the openai/anthropic observers still ignore.
    splitter: SseSplitter,
    /// Whether a final-state event has passed through (see
    /// [`ResponseEvent::ends_turn`]).
    ended: bool,
}

impl ResponsesSse {
    /// A fresh parser with nothing under assembly.
    pub fn new() -> ResponsesSse {
        ResponsesSse::default()
    }

    /// Feed one response-body chunk; every typed event the chunk closed
    /// off, in arrival order. Never fails.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<ResponseEvent> {
        let mut events = Vec::new();
        for raw in self.splitter.feed(chunk) {
            let Some(event) = event_of(raw) else {
                continue; // unparseable: skipped as unobservable
            };
            self.ended |= event.ends_turn();
            events.push(event);
        }
        events
    }

    /// End of stream: flush the unterminated trailing event, if any.
    /// `None` when nothing parseable was pending — the dialect's
    /// streams end at `response.completed`, so this is usually the
    /// empty case, but a stream cut off mid-event still yields its
    /// last whole event. Idempotent.
    pub fn finish(&mut self) -> Option<ResponseEvent> {
        let event = self.splitter.finish().and_then(event_of)?;
        self.ended |= event.ends_turn();
        Some(event)
    }

    /// Whether a final-state event (`response.completed`,
    /// `response.incomplete`, `response.failed`) has been seen — the
    /// `[DONE]`-less stream's own terminator, for a caller deciding
    /// whether the turn's accounting is final.
    pub fn turn_ended(&self) -> bool {
        self.ended
    }
}

/// One whole SSE event → its typed form.
///
/// `None` only when the data is not parseable JSON at all (unobservable,
/// skipped). Known kinds whose payload does not fit the typed view land
/// on [`ResponseEvent::Unknown`] with the kind and raw data intact.
fn event_of(raw: SseEvent) -> Option<ResponseEvent> {
    let data = raw.data();
    let value: Value = serde_json::from_str(&data).ok()?;
    // The JSON `type` names the kind; the `event:` line is the fallback.
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .or(raw.event.as_deref())?
        .to_owned();
    let typed = match kind.as_str() {
        "response.created" => {
            let response = object_of(&value, "response");
            let response_id = response
                .and_then(|response| response.get("id"))
                .and_then(Value::as_str)
                .map(|id| id.to_owned());
            let model = response
                .and_then(|response| response.get("model"))
                .and_then(Value::as_str)
                .map(|model| model.to_owned());
            Some(ResponseEvent::Created { response_id, model })
        }
        "response.output_item.added" => {
            item_of(&value).map(|item| ResponseEvent::OutputItemAdded {
                item,
                data: value.clone(),
            })
        }
        "response.output_item.done" => item_of(&value).map(|item| ResponseEvent::OutputItemDone {
            item,
            data: value.clone(),
        }),
        "response.output_text.delta" => {
            str_of(&value, "delta").map(|delta| ResponseEvent::OutputTextDelta { delta })
        }
        "response.reasoning_summary_text.delta" => str_of(&value, "delta").and_then(|delta| {
            int_of(&value, "summary_index").map(|summary_index| {
                ResponseEvent::ReasoningSummaryDelta {
                    delta,
                    summary_index,
                }
            })
        }),
        "response.completed" => object_of(&value, "response")
            .and_then(|response| serde_json::from_value::<CompletedResponse>(response.clone()).ok())
            .map(|response| ResponseEvent::Completed { response }),
        "response.incomplete" => Some(ResponseEvent::Incomplete {
            reason: value
                .pointer("/response/incomplete_details/reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
            usage: value
                .pointer("/response/usage")
                .and_then(|usage| serde_json::from_value(usage.clone()).ok()),
        }),
        "response.failed" => {
            error_of(&value, "/response/error").map(|error| ResponseEvent::Failed { error })
        }
        "error" => error_of(&value, "/error").map(|error| ResponseEvent::Error { error }),
        _ => Some(ResponseEvent::Unknown {
            kind: kind.clone(),
            data: value.clone(),
        }),
    };
    Some(typed.unwrap_or(ResponseEvent::Unknown {
        kind: kind.clone(),
        data: value,
    }))
}

/// The `item` of an output_item event, when it is an object.
fn item_of(value: &Value) -> Option<Item> {
    value
        .get("item")
        .filter(|item| item.is_object())
        .cloned()
        .map(Item)
}

/// One string field of the event JSON.
fn str_of(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_owned)
}

/// One integer field of the event JSON.
fn int_of(value: &Value, field: &str) -> Option<i64> {
    value.get(field).and_then(Value::as_i64)
}

/// One object field of the event JSON.
fn object_of<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
    value.get(field).filter(|field| field.is_object())
}

/// The error payload at `pointer`, typed.
fn error_of(value: &Value, pointer: &str) -> Option<ResponseError> {
    let error = value.pointer(pointer)?;
    serde_json::from_value(error.clone()).ok()
}

/// What one turn added up to — the observer half of the codex side
/// parser (see the module docs). Fields are `Option`/empty-defaults
/// where the wire's absence is a fact, never zero (invariant 3).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnCapture {
    /// The response id, from `response.created` and confirmed by
    /// `response.completed` when it repeats one.
    response_id: Option<String>,
    /// The model slug the response named at `response.created` — the
    /// identity the upstream actually engaged (the routing unit's
    /// `model`/`raw_model` columns).
    model: Option<String>,
    /// The items that completed (`output_item.done` only — where the
    /// function-call arguments arrive whole).
    items: Vec<Item>,
    /// The assistant text, the `output_text` deltas in arrival order.
    text: String,
    /// The reasoning summaries by index, their deltas joined.
    reasoning_summaries: BTreeMap<i64, String>,
    /// The usage of `response.completed`.
    usage: Option<Usage>,
    /// `response.completed`'s `end_turn`, when said.
    end_turn: Option<bool>,
    /// `response.incomplete`'s reason.
    incomplete_reason: Option<String>,
    /// The first error the turn hit (`response.failed` or a top-level
    /// `error` event).
    error: Option<ResponseError>,
    /// Whether the turn's final-state event passed through.
    ended: bool,
}

impl TurnCapture {
    /// A fresh capture for one turn.
    pub fn new() -> TurnCapture {
        TurnCapture::default()
    }

    /// Fold one typed event into the capture. Never fails, never gates a
    /// stream (invariant 6).
    pub fn observe(&mut self, event: &ResponseEvent) {
        match event {
            ResponseEvent::Created { response_id, model } => {
                if self.response_id.is_none() {
                    self.response_id.clone_from(response_id);
                }
                if self.model.is_none() {
                    self.model.clone_from(model);
                }
            }
            ResponseEvent::OutputItemDone { item, .. } => self.items.push(item.clone()),
            ResponseEvent::OutputTextDelta { delta } => self.text.push_str(delta),
            ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            } => {
                self.reasoning_summaries
                    .entry(*summary_index)
                    .or_default()
                    .push_str(delta);
            }
            ResponseEvent::Completed { response } => {
                if response.id.is_some() {
                    self.response_id.clone_from(&response.id);
                }
                self.usage.clone_from(&response.usage);
                self.end_turn = response.end_turn;
            }
            ResponseEvent::Incomplete { reason, usage } => {
                self.incomplete_reason = reason.clone();
                self.usage.clone_from(usage);
            }
            ResponseEvent::Failed { error } | ResponseEvent::Error { error } => {
                if self.error.is_none() {
                    self.error = Some(error.clone());
                }
            }
            ResponseEvent::OutputItemAdded { .. } | ResponseEvent::Unknown { .. } => {}
        }
        self.ended |= event.ends_turn();
    }

    /// The response id, when the stream named one.
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    /// The model slug the response named, when it carried one.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The completed output items, in arrival order (function-call
    /// arguments whole).
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// The function calls among the completed items, typed.
    pub fn function_calls(&self) -> Vec<FunctionCall> {
        self.items
            .iter()
            .filter_map(Item::as_function_call)
            .collect()
    }

    /// The assistant text: the `output_text` deltas joined.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The reasoning summaries by index, each index's deltas joined.
    pub fn reasoning_summaries(&self) -> &BTreeMap<i64, String> {
        &self.reasoning_summaries
    }

    /// The usage of `response.completed`.
    pub fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }

    /// `response.completed`'s `end_turn`, when said.
    pub fn end_turn(&self) -> Option<bool> {
        self.end_turn
    }

    /// `response.incomplete`'s reason, when the turn stopped short.
    pub fn incomplete_reason(&self) -> Option<&str> {
        self.incomplete_reason.as_deref()
    }

    /// The first error the turn hit.
    pub fn error(&self) -> Option<&ResponseError> {
        self.error.as_ref()
    }

    /// Whether the turn's final-state event passed through — the
    /// caller's "the accounting is final" signal.
    pub fn turn_ended(&self) -> bool {
        self.ended
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::super::types::ResponseEvent;
    use super::{ResponsesSse, TurnCapture};
    use serde_json::json;

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_sse")
    }

    fn fixture_names() -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(fixtures_dir())
            .expect("fixture directory exists")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".sse"))
            .collect();
        names.sort();
        assert!(
            names.len() >= 3,
            "the codex SSE corpus must keep at least 3 fixtures"
        );
        names
    }

    fn fixture(name: &str) -> Vec<u8> {
        fs::read(fixtures_dir().join(name)).expect("fixture exists")
    }

    /// Feed a whole stream in the given parts, then flush; every typed
    /// event, in arrival order.
    fn run(parts: &[&[u8]]) -> Vec<ResponseEvent> {
        let mut parser = ResponsesSse::new();
        let mut events = Vec::new();
        for part in parts {
            events.extend(parser.feed(part));
        }
        if let Some(event) = parser.finish() {
            events.push(event);
        }
        events
    }

    #[test]
    fn fixtures_parse_identically_at_every_possible_boundary() {
        for name in fixture_names() {
            let bytes = fixture(&name);
            let whole = run(&[&bytes]);
            assert!(!whole.is_empty(), "{name}: fixture must yield events");
            // Every split point — inside event delimiters and inside
            // multibyte UTF-8 sequences alike (the tool-call fixture
            // carries one) — must produce the identical event sequence.
            for i in 0..=bytes.len() {
                assert_eq!(
                    run(&[&bytes[..i], &bytes[i..]]),
                    whole,
                    "{name}: boundary at {i}"
                );
            }
        }
    }

    #[test]
    fn fixtures_parse_byte_at_a_time() {
        for name in fixture_names() {
            let bytes = fixture(&name);
            let mut parser = ResponsesSse::new();
            let mut events = Vec::new();
            for &byte in &bytes {
                events.extend(parser.feed(&[byte]));
            }
            if let Some(event) = parser.finish() {
                events.push(event);
            }
            assert_eq!(events, run(&[&bytes]), "{name}: byte-at-a-time splits");
        }
    }

    #[test]
    fn the_tool_call_turn_parses_into_the_expected_event_sequence() {
        let bytes = fixture("01_tool_call_turn.sse");
        let events = run(&[&bytes]);
        let kinds: Vec<&str> = events.iter().map(ResponseEvent::kind).collect();
        assert_eq!(
            kinds,
            vec![
                "response.created",
                "response.output_item.added",
                "response.reasoning_summary_text.delta",
                "response.reasoning_summary_text.delta",
                "response.output_item.done",
                "response.output_item.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_item.done",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.delta",
                "response.output_item.done",
                "response.completed",
            ],
            "every event in the fixture, in order (the ignorable kinds \
             pass through as unknowns, never dropped)"
        );
        let created = &events[0];
        let (created_id, created_model) = match created {
            ResponseEvent::Created { response_id, model } => (response_id, model),
            other => panic!("unexpected first event: {other:?}"),
        };
        assert_eq!(*created_id, Some("resp_6f3c9a".to_owned()));
        assert_eq!(
            *created_model,
            Some("gpt-5.2-codex".to_owned()),
            "the response's own model slug is captured"
        );
        // The last event is the terminator.
        assert_eq!(kinds.last(), Some(&"response.completed"));
        assert!(events.last().expect("event").ends_turn());
    }

    #[test]
    fn the_tool_call_turn_captures_function_calls_text_summaries_and_usage() {
        let mut capture = TurnCapture::new();
        for event in run(&[&fixture("01_tool_call_turn.sse")]) {
            capture.observe(&event);
        }
        assert!(capture.turn_ended());
        assert_eq!(capture.response_id(), Some("resp_6f3c9a"));
        assert_eq!(
            capture.model(),
            Some("gpt-5.2-codex"),
            "the capture latches the response's own model slug"
        );

        // The function call: buffered from output_item.done, arguments
        // COMPLETE — never assembled from the ignorable deltas.
        let calls = capture.function_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].call_id, "call_read1");
        assert_eq!(
            calls[0].arguments, r#"{"path":"src/main.rs"}"#,
            "the done item carries the whole arguments string"
        );

        // The encrypted reasoning, message, and function-call items completed.
        assert_eq!(capture.items().len(), 3);
        let reasoning = capture.items()[0].as_reasoning().expect("reasoning item");
        assert_eq!(reasoning.id.as_deref(), Some("rs_1"));
        assert_eq!(
            reasoning.encrypted_content.as_deref(),
            Some("opaque-encrypted-reasoning")
        );
        let message = capture.items()[1].as_message().expect("message item");
        assert_eq!(message.role, "assistant");
        assert_eq!(message.content[0].kind, "output_text");
        assert_eq!(message.content[0].text, "I'll read the files, then café.");

        // The text: the output_text deltas joined.
        assert_eq!(capture.text(), "I'll read the files, then café.");

        // The reasoning summary: one index, its deltas joined.
        assert_eq!(
            capture.reasoning_summaries().get(&0).map(String::as_str),
            Some("Reading the thread files.")
        );

        // The usage, extracted whole.
        let usage = capture.usage().expect("usage");
        assert_eq!(usage.input_tokens, 1234);
        assert_eq!(usage.output_tokens, 210);
        assert_eq!(usage.total_tokens, 1444);
        assert_eq!(
            usage
                .input_tokens_details
                .as_ref()
                .expect("details")
                .cached_tokens,
            Some(512)
        );
        assert_eq!(
            usage
                .input_tokens_details
                .as_ref()
                .expect("details")
                .cache_write_tokens,
            Some(64)
        );
        assert_eq!(
            usage
                .output_tokens_details
                .as_ref()
                .expect("details")
                .reasoning_tokens,
            Some(96)
        );
        assert_eq!(capture.end_turn(), Some(false));
        assert_eq!(capture.error(), None);
        assert_eq!(capture.incomplete_reason(), None);
    }

    #[test]
    fn the_incomplete_fixture_ends_the_turn_with_its_reason() {
        // The CRLF fixture: both dialects parse identically.
        let bytes = fixture("02_incomplete_crlf.sse");
        let events = run(&[&bytes]);
        let last = events.last().expect("terminal event");
        assert_eq!(last.kind(), "response.incomplete");
        assert!(last.ends_turn());
        let mut capture = TurnCapture::new();
        for event in events {
            capture.observe(&event);
        }
        assert!(capture.turn_ended());
        assert_eq!(capture.incomplete_reason(), Some("max_output_tokens"));
        assert_eq!(capture.usage(), None, "no completed, no usage");
    }

    #[test]
    fn the_failed_fixture_parses_error_shapes_and_latches_the_first_error() {
        let events = run(&[&fixture("03_failed.sse")]);
        let kinds: Vec<&str> = events.iter().map(ResponseEvent::kind).collect();
        assert_eq!(kinds, vec!["response.created", "error", "response.failed"]);

        // The top-level error event, typed.
        match &events[1] {
            ResponseEvent::Error { error } => {
                assert_eq!(error.code.as_deref(), Some("server_error"));
                assert_eq!(error.message.as_deref(), Some("Upstream overloaded."));
                assert!(
                    !events[1].ends_turn(),
                    "a bare error event does not end the turn"
                );
            }
            other => panic!("unexpected second event: {other:?}"),
        }

        // response.failed: the terminal error, with its rate-limit reset.
        match &events[2] {
            ResponseEvent::Failed { error } => {
                assert_eq!(error.kind.as_deref(), Some("invalid_request_error"));
                assert_eq!(error.code.as_deref(), Some("rate_limit_exceeded"));
                assert_eq!(error.resets_at, Some(1_800_000_900));
            }
            other => panic!("unexpected final event: {other:?}"),
        }

        let mut capture = TurnCapture::new();
        for event in events {
            capture.observe(&event);
        }
        assert_eq!(
            capture.error().expect("latched").code.as_deref(),
            Some("server_error"),
            "the FIRST error latches"
        );
        assert!(capture.turn_ended(), "response.failed is a final state");
    }

    #[test]
    fn unknown_kinds_and_unparseable_data_do_not_break_the_stream() {
        let stream = b"event: response.new_tool_event\n\
                       data: {\"type\":\"response.new_tool_event\",\"weird\":[1,2]}\n\n\
                       data: not json at all\n\n\
                       event: response.output_text.delta\n\
                       data: {\"output_index\":0,\"content_index\":0,\"delta\":\"after\"}\n\n";
        let events = run(&[stream]);
        // The unknown kind passes through with its data; the non-JSON
        // line is skipped; the JSON-without-type falls back to the
        // `event:` line and parses typed.
        assert_eq!(events.len(), 2);
        match &events[0] {
            ResponseEvent::Unknown { kind, data } => {
                assert_eq!(kind, "response.new_tool_event");
                assert_eq!(data["weird"], json!([1, 2]));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(events[1].kind(), "response.output_text.delta");

        // A known kind whose payload does not fit the typed view keeps
        // its bytes on Unknown instead of being dropped.
        let malformed = b"data: {\"type\":\"response.completed\",\"response\":42}\n\n";
        let events = run(&[malformed]);
        match &events[0] {
            ResponseEvent::Unknown { kind, data } => {
                assert_eq!(kind, "response.completed");
                assert_eq!(data["response"], json!(42));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn a_stream_cut_off_mid_event_flushes_its_last_whole_event() {
        // No trailing blank line, the final completed event still whole
        // on its last line: finish() delivers it.
        let bytes = fixture("01_tool_call_turn.sse");
        let cut = &bytes[..bytes.len() - 1];
        let mut parser = ResponsesSse::new();
        let mut events = parser.feed(cut);
        let tail = parser.finish().expect("the cut-off event flushes");
        events.push(tail);
        assert_eq!(events.last().expect("event").kind(), "response.completed");
        assert!(parser.turn_ended());
        assert!(parser.finish().is_none(), "flush is once");

        // Cut in the middle of the completed event's data line: that
        // event is lost (a measurement, never a request — invariant 6),
        // and the turn never ends.
        let mut parser = ResponsesSse::new();
        let events = parser.feed(&bytes[..bytes.len() - 60]);
        let tail = parser.finish();
        assert_ne!(
            events.len() + usize::from(tail.is_some()),
            0,
            "the prefix before the cut still parsed"
        );
        assert!(
            !parser.turn_ended(),
            "no terminator arrived — the accounting is not final"
        );
    }

    #[test]
    fn a_done_sentinel_is_tolerated_before_the_real_terminator() {
        // The dialect carries no [DONE]; a client-side re-framer might
        // still send one. The splitter drops it and the parser moves on.
        let stream = b"data: [DONE]\n\n\
                      event: response.completed\n\
                      data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":null}}\n\n";
        let mut parser = ResponsesSse::new();
        let events = parser.feed(stream);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind(), "response.completed");
        assert!(parser.turn_ended());
    }
}
