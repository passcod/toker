//! Incremental SSE event splitting over arbitrary byte chunks.
//!
//! The front half of the Server core's opportunistic side-parser: the
//! server unit feeds each chunk of a response body it is *already*
//! forwarding through [`SseSplitter::feed`] and gets back the complete
//! [`SseEvent`]s that chunk closed off. Splitting never fails and never
//! gates the pass-through stream (invariant 6).
//!
//! Wire-format scope, from SSE plus the plan's tolerance note:
//!
//! - **Line endings**: `\n` and `\r\n` everywhere. An event boundary is
//!   a blank line in either dialect — `\n\n`, `\r\n\r\n`, or the mixed
//!   `\n\r\n` — handled directly at the boundary, not by an end-of-stream
//!   flush (the ledger proxy's `\r\n`-only dialect fell back there; this
//!   splitter does better). A lone `\r` is data, not a terminator.
//! - **Buffering**: only the bytes since the last complete line. Per-chunk
//!   work is proportional to the chunk, not to the stream, however long
//!   the stream runs.
//! - **Per event**: `data:` lines are kept, and so is the `event:`
//!   line's value (the responses dialect names its events that way —
//!   see [crate::providers::codex::sse]; the openai/anthropic observers
//!   ignore it, their dialects naming kinds inside the data JSON). A
//!   `data:` line that is not valid UTF-8 is skipped as unobservable —
//!   invariant 6: it loses a measurement, never a request. Comment
//!   lines (`:`-prefixed keep-alives) and other fields (`id:`,
//!   `retry:`) are parsed and ignored; an event left with no data lines
//!   is skipped, and so is the `[DONE]` sentinel.
//! - **Framing**: exactly one leading space after the colon is stripped
//!   (SSE semantics), one trailing `\r` per line, multi-line data joined
//!   by `\n`.
//!
//! Hand-rolled rather than `eventsource-stream` because the side-observer
//! is the plan's known hand-rolled exception and needs byte-level control
//! the off-the-shelf parser does not expose: both delimiter dialects
//! handled directly, byte-exact data-line text for the verbatim usage
//! capture (see [`usage`]), and per-line UTF-8 decoding so a multibyte
//! sequence split across chunks never panics.

use std::mem;

/// One complete SSE event: the payloads of its `data:` lines, in arrival
/// order, plus its `event:` line's value when it carried one.
///
/// Each element is one `data:` line's bytes after SSE framing — the
/// `data:` prefix removed, exactly one leading space after the colon
/// stripped, one trailing `\r` stripped — decoded as UTF-8 per complete
/// line. Non-UTF-8 data lines are absent (skipped as unobservable), not
/// lossy-converted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `data:` line payloads of this event, in arrival order.
    pub data_lines: Vec<String>,
    /// The `event:` line's value (one leading space after the colon
    /// stripped, like SSE framing): the event's kind name. `None` when
    /// the event carried no `event:` line — the openai/anthropic
    /// dialects never do; the responses dialect always does (and names
    /// the same kind inside the data JSON too).
    pub event: Option<String>,
}

impl SseEvent {
    /// The event's data: multi-line data joined by `\n` (SSE semantics).
    pub fn data(&self) -> String {
        self.data_lines.join("\n")
    }
}

/// Incremental SSE splitter: feed it response chunks, collect events.
///
/// Buffering is bounded by the length of the longest line (see the module
/// docs), so a stream of any length costs no more than the partial event
/// currently under assembly.
#[derive(Debug, Clone, Default)]
pub struct SseSplitter {
    /// Bytes since the last complete line — the only buffering.
    buffer: Vec<u8>,
    /// The `data:` lines of the event under assembly.
    pending: Vec<String>,
    /// The `event:` line's value of the event under assembly, if seen
    /// (the last one wins, per SSE dispatch semantics).
    pending_event: Option<String>,
}

impl SseSplitter {
    /// A fresh splitter with nothing under assembly.
    pub fn new() -> SseSplitter {
        SseSplitter::default()
    }

    /// Feed one chunk of the response body; every event the chunk closed
    /// off, in arrival order. Never fails (invariant 6).
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        self.buffer.extend_from_slice(chunk);
        let mut consumed = 0;
        while let Some(offset) = self.buffer[consumed..].iter().position(|&b| b == b'\n') {
            let raw = &self.buffer[consumed..consumed + offset];
            consumed += offset + 1;
            let line = raw.strip_suffix(b"\r").unwrap_or(raw);
            if line.is_empty() {
                // Blank line: event boundary. Emit the event under
                // assembly, if it has anything observable.
                if let Some(event) =
                    event_of(self.pending_event.take(), mem::take(&mut self.pending))
                {
                    events.push(event);
                }
            } else if let Some(data) = field_line_of(line, b"data") {
                self.pending.push(data);
            } else if let Some(event) = field_line_of(line, b"event") {
                self.pending_event = Some(event);
            }
        }
        // Keep only the partial line: the buffer never grows past one
        // line however long the stream runs.
        self.buffer.drain(..consumed);
        events
    }

    /// End of stream: process the unterminated trailing line (if any) as
    /// a final line, then flush the event under assembly.
    ///
    /// `None` when nothing is pending — a stream that ended on an event
    /// boundary, or carried only keep-alives to the end. Idempotent: a
    /// second call always returns `None`.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let tail = mem::take(&mut self.buffer);
            let line = tail.strip_suffix(b"\r").unwrap_or(&tail);
            if !line.is_empty() {
                if let Some(data) = field_line_of(line, b"data") {
                    self.pending.push(data);
                } else if let Some(event) = field_line_of(line, b"event") {
                    self.pending_event = Some(event);
                }
            }
        }
        event_of(self.pending_event.take(), mem::take(&mut self.pending))
    }
}

/// One complete, non-empty line → the named SSE field's payload.
///
/// Comments (a `:`-leading keep-alive), other fields (`id:`, `retry:`),
/// and a field line without a colon whose name does not match contribute
/// nothing. A value that is not valid UTF-8 contributes nothing either —
/// skipped as unobservable rather than lossily decoded (invariant 6: the
/// skipped line can leave the event's data unparseable downstream, losing
/// the measurement, never the request).
fn field_line_of(line: &[u8], field: &[u8]) -> Option<String> {
    let (name, value) = match line.iter().position(|&b| b == b':') {
        Some(colon) => (&line[..colon], &line[colon + 1..]),
        // SSE: a line without a colon is a field with an empty value.
        None => (line, &[][..]),
    };
    if name != field {
        return None;
    }
    // Exactly one leading space after the colon (SSE framing).
    let value = value.strip_prefix(b" ").unwrap_or(value);
    let text = std::str::from_utf8(value).ok()?;
    Some(text.to_string())
}

/// Close the event whose data lines are `data_lines` (and whose `event:`
/// line, if any, named it `event`).
///
/// `None` for the two events that carry nothing observable: an event with
/// no data lines at all (comment/other-field-only), and the `[DONE]`
/// sentinel (always a single `data: [DONE]` line).
fn event_of(event: Option<String>, data_lines: Vec<String>) -> Option<SseEvent> {
    if data_lines.is_empty() {
        return None;
    }
    if data_lines.len() == 1 && data_lines[0] == "[DONE]" {
        return None;
    }
    Some(SseEvent { data_lines, event })
}

#[cfg(test)]
mod tests {
    use super::{SseEvent, SseSplitter};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/openai_chat_sse")
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
            names.len() >= 5,
            "the SSE corpus must keep at least 5 fixtures"
        );
        names
    }

    fn fixture(name: &str) -> Vec<u8> {
        fs::read(fixtures_dir().join(name)).expect("fixture exists")
    }

    /// Feed a whole byte stream in the given parts, then flush; every
    /// event, in arrival order.
    fn run(parts: &[&[u8]]) -> Vec<SseEvent> {
        let mut splitter = SseSplitter::new();
        let mut events = Vec::new();
        for part in parts {
            events.extend(splitter.feed(part));
        }
        if let Some(event) = splitter.finish() {
            events.push(event);
        }
        events
    }

    #[test]
    fn fixtures_split_identically_at_every_possible_boundary() {
        for name in fixture_names() {
            let bytes = fixture(&name);
            let whole = run(&[&bytes]);
            assert!(!whole.is_empty(), "{name}: fixture must yield events");
            // Every split point, which lands inside event delimiters and
            // inside multibyte UTF-8 sequences alike (fixture 03 carries
            // both), must produce the identical event sequence.
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
    fn fixtures_split_byte_at_a_time() {
        for name in fixture_names() {
            let bytes = fixture(&name);
            let mut splitter = SseSplitter::new();
            let mut events = Vec::new();
            for &byte in &bytes {
                events.extend(splitter.feed(&[byte]));
            }
            if let Some(event) = splitter.finish() {
                events.push(event);
            }
            assert_eq!(events, run(&[&bytes]), "{name}: byte-at-a-time splits");
        }
    }

    #[test]
    fn data_framing_strips_one_space_and_keeps_the_rest() {
        let events = run(&[b"data: plain\n\ndata:tight\n\ndata:  spaced\n\n"]);
        let data: Vec<String> = events.iter().map(|event| event.data()).collect();
        assert_eq!(
            data,
            vec![
                "plain".to_string(),
                "tight".to_string(),
                " spaced".to_string()
            ],
            "exactly one leading space is stripped"
        );
        // The raw line payloads are exposed too.
        assert_eq!(events[2].data_lines, vec![" spaced".to_string()]);
    }

    #[test]
    fn crlf_dialect_splits_directly() {
        let events = run(&[
            b"data: {\"one\":1}\r\n\r\ndata: {\"two\":2}\r\n\r\n",
            b"data: {\"three\":3}\n\r\ndata: [DONE]\r\n\r\n",
        ]);
        let data: Vec<String> = events.iter().map(|event| event.data()).collect();
        assert_eq!(
            data,
            vec![
                "{\"one\":1}".to_string(),
                "{\"two\":2}".to_string(),
                "{\"three\":3}".to_string(),
            ],
            "\\r\\n\\r\\n and the mixed \\n\\r\\n both split; [DONE] is dropped"
        );
    }

    #[test]
    fn comments_keepalives_and_other_fields_are_skipped() {
        // Keep-alive comments alone: no events at all.
        assert!(run(&[b": OPENROUTER PROCESSING\n\n: ping\n\n"]).is_empty());
        // Other fields alone: no events either.
        assert!(run(&[b"event: message\nid: 7\nretry: 100\n\n"]).is_empty());
        // A comment inside an event does not stop its data from counting,
        // and other fields are ignored beside data lines.
        let events = run(&[b": note\nevent: message\nid: 7\ndata: {\"a\":1}\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data_lines, vec!["{\"a\":1}".to_string()]);
    }

    #[test]
    fn multiple_data_lines_join_with_newlines() {
        let events = run(&[b"data: first\ndata: second\ndata: third\n\n"]);
        assert_eq!(events.len(), 1, "still one event");
        assert_eq!(events[0].data_lines.len(), 3);
        assert_eq!(events[0].data(), "first\nsecond\nthird");
    }

    #[test]
    fn empty_data_lines_are_kept() {
        let events = run(&[b"data:\n\ndata: \n\n"]);
        // `data:` and `data: ` (one space, stripped) are both the empty
        // value: real data lines, so the events exist.
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data(), "");
        assert_eq!(events[1].data(), "");
    }

    #[test]
    fn done_sentinel_is_skipped_but_does_not_close_the_stream() {
        let events = run(&[b"data: [DONE]\n\ndata: {\"after\":true}\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data(), "{\"after\":true}");
    }

    #[test]
    fn finish_flushes_pending_event_then_nothing() {
        let mut splitter = SseSplitter::new();
        assert_eq!(splitter.finish(), None, "nothing pending on a fresh feed");

        // An event cut off by end of stream, no trailing boundary, and a
        // trailing \r on the final partial line.
        let mut splitter = SseSplitter::new();
        assert!(splitter.feed(b"data: tail\r").is_empty());
        assert_eq!(splitter.finish().expect("flush pending").data(), "tail");
        assert_eq!(splitter.finish(), None, "flush is once");

        // A partial event with only other fields flushes to nothing.
        let mut splitter = SseSplitter::new();
        assert!(splitter.feed(b"event: dangling").is_empty());
        assert_eq!(splitter.finish(), None);
    }

    #[test]
    fn invalid_utf8_lines_are_skipped_without_panic() {
        // An invalid byte in the only data line: the line is skipped, so
        // the event has no data lines and is not emitted.
        assert!(run(&[b"data: \xff\xfe\n\n"]).is_empty());

        // An invalid line beside a valid one: only the valid one survives.
        let events = run(&[b"data: ok\ndata: \xff\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data_lines, vec!["ok".to_string()]);

        // A multibyte sequence split across chunks is whole by the time
        // the line is, so it decodes: U+4E16 = e4 b8 96.
        let events = run(&[b"data: \xe4\xb8", b"\x96\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data(), "\u{4e16}");
    }

    #[test]
    fn lone_cr_is_data_not_a_terminator() {
        // Only \n and \r\n terminate lines; a lone \r inside a line is
        // kept as data.
        let events = run(&[b"data: a\rb\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data(), "a\rb");
    }

    #[test]
    fn event_lines_are_captured_and_last_wins() {
        // The responses dialect names its events with an `event:` line;
        // the capture is additive — data behavior is unchanged.
        let events = run(&[
            b"event: response.created\ndata: {\"a\":1}\n\n",
            b"event: response.completed\r\ndata: {\"b\":2}\r\n\r\n",
        ]);
        assert_eq!(events[0].event.as_deref(), Some("response.created"));
        assert_eq!(events[0].data(), "{\"a\":1}");
        assert_eq!(
            events[1].event.as_deref(),
            Some("response.completed"),
            "the \\r\\n dialect strips the event line's trailing \\r"
        );
        assert_eq!(events[1].data(), "{\"b\":2}");

        // The last `event:` line before dispatch wins, per SSE semantics.
        let events = run(&[b"event: first\nevent: second\ndata: {}\n\n"]);
        assert_eq!(events[0].event.as_deref(), Some("second"));

        // Exactly one leading space after the colon is stripped (the
        // second stays, like `data:` framing), and an event line
        // without data still yields no event.
        let events = run(&[b"event:  spaced-kind\ndata: x\n\n"]);
        assert_eq!(events[0].event.as_deref(), Some(" spaced-kind"));
        assert!(run(&[b"event: only-a-kind\n\n"]).is_empty());
        assert!(
            run(&[b"event: [DONE]\n\n"]).is_empty(),
            "the sentinel rule is a data rule — an event line alone never emits"
        );
    }
}
