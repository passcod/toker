//! The composition: a responses-dialect turn → Anthropic SSE, in two
//! layers (the events table lives in the parent module's docs).
//!
//! - [`codex_backend`](super::codex_backend) — the backend adapter's
//!   interpretation half: the codex wire's
//!   [`ResponseEvent`](crate::providers::codex::ResponseEvent)s →
//!   the canonical turn model
//!   ([`CanonEvent`](crate::ir::canonical::CanonEvent)), and unit A's
//!   [`TurnCapture`](crate::providers::codex::TurnCapture) → the
//!   canonical final turn. The error table's interpretation half
//!   (the upstream's `code`/`kind` → the canonical error) lives
//!   there.
//! - [`anthropic_frontend`](super::anthropic_frontend) — the frontend
//!   adapter's rendering half: canonical turn events → the anthropic
//!   wire (the block-index state machine, the SSE event
//!   construction, the usage fields, the error rendering). Nothing
//!   backend-shaped happens there: it is fed canon events only.
//!
//! This module is the pair of the two, and adds nothing of its own.
//!
//! [`AnthropicStream`] is pure: a function of the events it is fed
//! and the model echo only — no clock, no counters, nothing else
//! (invariant 4). The same events always render the same bytes,
//! forever. The message id is the backend's own turn id, verbatim —
//! never minted here (a stream that never named one renders with
//! the frontend's constant placeholder).
//!
//! Every pinned byte below is the composition's own proof: the
//! corpus fixtures in, the anthropic SSE bytes out — byte-identical
//! to the pair module this split replaced. The per-layer behaviour
//! lives in the adapters' tests (canon events asserted there,
//! rendering asserted there).

use serde_json::Value;

use crate::ir::canonical::ResponseProjection;
use crate::observe::sse::SseEvent;
use crate::providers::codex::{ResponseError, ResponseEvent, TurnCapture};
use crate::translate::anthropic_frontend::{self, AnthropicRenderer, anthropic_from_canonical};
use crate::translate::codex_backend::{self, CanonStream};

/// The Anthropic SSE stream for one streamed turn: feed it the
/// turn's [`ResponseEvent`]s (in arrival order), collect the emitted
/// [`SseEvent`]s. One stream per turn — the backend adapter
/// interprets each event into the canonical turn model, the frontend
/// adapter renders each canonical event onto the anthropic wire.
#[derive(Debug, Clone)]
pub struct AnthropicStream {
    /// The backend adapter's interpretation: codex events → canon
    /// events.
    canon: CanonStream,
    /// The frontend adapter's rendering: canon events → anthropic
    /// SSE.
    renderer: AnthropicRenderer,
}

impl AnthropicStream {
    /// A fresh stream for one turn, echoing `model` in the
    /// `message_start` (the streaming path — non-streaming requests
    /// go through [`message_from_capture`]).
    pub fn new(model: &str) -> AnthropicStream {
        AnthropicStream {
            canon: CanonStream::new(),
            renderer: AnthropicRenderer::new(model),
        }
    }

    /// Feed one Responses event; every Anthropic SSE event it
    /// produced, in anthropic event order. Never fails — an event
    /// with no canonical shape (an `output_item.added`, an unknown
    /// kind) produces nothing, and a `function_call` whose fields do
    /// not fit the typed view is skipped rather than corrupting the
    /// stream (invariant 6).
    pub fn feed(&mut self, event: &ResponseEvent) -> Vec<SseEvent> {
        self.feed_projected(event, ResponseProjection::default())
    }

    /// Feed one Responses event through a request-derived response
    /// projection before rendering it onto the Anthropic wire.
    pub fn feed_projected(
        &mut self,
        event: &ResponseEvent,
        projection: ResponseProjection,
    ) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for canon in self.canon.feed(event) {
            if projection.allows(&canon) {
                out.extend(self.renderer.feed(&canon));
            }
        }
        out
    }

    /// Whether the turn's final-state event passed through — the
    /// caller's "the accounting is final" signal.
    pub fn turn_ended(&self) -> bool {
        self.renderer.turn_ended()
    }
}

/// The `stream:false` sibling: the whole turn (unit A's
/// [`TurnCapture`]) → the complete non-streaming Anthropic message
/// JSON — the capture folded into the canonical final turn (the
/// backend adapter), the canonical rendered onto the anthropic wire
/// (the frontend adapter), the same content assembly and usage
/// mapping as the streamed path. An errored turn yields the
/// anthropic error body instead (no content: anthropic's
/// non-streaming errors carry none).
pub fn message_from_capture(model: &str, capture: &TurnCapture) -> Value {
    anthropic_from_canonical(model, &codex_backend::canonical_turn_from_capture(capture))
}

/// A Responses error payload → the anthropic error event's data JSON
/// (the error table): the backend adapter interprets the payload into
/// the canonical error, the frontend adapter renders it.
pub fn error_event_data(error: &ResponseError) -> Value {
    anthropic_frontend::anthropic_error_event_data(&codex_backend::canon_error_from_response(error))
}

/// The error table, composed: a Responses error payload → the
/// anthropic `error.type` (the interpretation half in the backend
/// adapter, the rendering half in the frontend adapter).
pub fn anthropic_error_type(error: &ResponseError) -> &'static str {
    anthropic_frontend::anthropic_error_type(&codex_backend::canon_error_from_response(error))
}

#[cfg(test)]
mod tests {
    //! The composition's own tests: the end-to-end bytes a pair has —
    //! the corpus fixtures in, the anthropic SSE bytes out, the
    //! aggregated captures, the unmapped events, purity. The
    //! per-layer details live in the adapters' tests
    //! ([`codex_backend`](super::codex_backend) asserting canon
    //! events, [`anthropic_frontend`](super::anthropic_frontend)
    //! asserting the rendering).

    use std::fs;
    use std::path::{Path, PathBuf};

    use super::super::to_anthropic::{AnthropicStream, error_event_data, message_from_capture};
    use crate::observe::sse::SseEvent;
    use crate::providers::codex::{
        CompletedResponse, ContentPart, Item, ResponseError, ResponseEvent, ResponsesSse,
        TurnCapture,
    };
    use serde_json::json;

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_sse")
    }

    fn fixture(name: &str) -> Vec<u8> {
        fs::read(fixtures_dir().join(name)).expect("fixture exists")
    }

    /// Parse a fixture's whole event stream (the composition is fed
    /// unit A's typed events, so fixtures go through unit A's parser
    /// first — the same plumbing the server wires).
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
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque-encrypted-reasoning"}}"#,
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
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"I'll read the files, then "}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"café."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":2}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"call_read1","name":"read_file","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src/main.rs\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":3}"#,
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
            // No usage key: the incomplete turn carried none (absence
            // ≠ zero, invariant 3).
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
        // order (the state machine reports; the server decides
        // policy). No message_delta, no message_stop — anthropic
        // error streams end at the error.
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
        // The composed indexing: text, a tool call, then text again —
        // each part its own block, in order.
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
                data: json!({}),
            },
            ResponseEvent::OutputItemDone {
                item: Item::function_call("read_file", r#"{"a":1}"#, "call_1"),
                data: json!({}),
            },
            ResponseEvent::OutputTextDelta {
                delta: "Part two.".to_owned(),
            },
            ResponseEvent::OutputItemDone {
                item: Item::message("assistant", vec![ContentPart::output_text("Part two.")]),
                data: json!({}),
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
    fn unmapped_events_emit_nothing() {
        let mut stream = AnthropicStream::new("claude-opus-5");
        assert!(
            stream
                .feed(&ResponseEvent::OutputItemAdded {
                    item: Item::message("assistant", vec![]),
                    data: json!({}),
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
    fn the_error_table_maps_through_the_composition() {
        // The wiring proof — the full table lives in the layers
        // (codex_backend interprets, anthropic_frontend renders); here
        // the pair must produce the same bytes the pair module did
        // for the shapes the server actually meets.
        let error = |kind: Option<&str>,
                     code: Option<&str>,
                     message: Option<&str>,
                     resets_at: Option<i64>| ResponseError {
            kind: kind.map(str::to_owned),
            code: code.map(str::to_owned),
            message: message.map(str::to_owned),
            resets_at,
        };
        // A rate-limit code, with the reset as retry_after.
        assert_eq!(
            error_event_data(&error(
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
        // A kind that is already an anthropic type name passes
        // verbatim through both layers.
        assert_eq!(
            error_event_data(&error(Some("overloaded_error"), None, None, None)),
            json!({"type": "error", "error": {"type": "overloaded_error",
                                             "message": "overloaded_error"}})
        );
        // And the bare default.
        assert_eq!(
            error_event_data(&ResponseError::default()),
            json!({"type": "error", "error": {"type": "api_error",
                                              "message": "upstream error"}})
        );
        assert_eq!(
            super::anthropic_error_type(&error(None, Some("server_error"), Some("m"), None)),
            "api_error"
        );
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
                r#"{"type":"redacted_thinking","data":"opaque-encrypted-reasoning"},"#,
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
                usage: None,
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
        stream.feed(&ResponseEvent::Incomplete {
            reason: None,
            usage: None,
        });
        stream.feed(&ResponseEvent::Incomplete {
            reason: Some("something_new".to_owned()),
            usage: None,
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
