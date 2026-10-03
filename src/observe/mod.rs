//! Opportunistic response observation: the side-parser that runs beside a
//! pass-through byte stream.
//!
//! Plan: Server core — "an opportunistic, crash-proof SSE side-parser
//! extracts usage/model/cost" around a response stream the server is
//! already forwarding. This module is pure and self-contained — bytes in,
//! [`UsageCapture`] out — so the server unit only has to wire chunk
//! boundaries to it. No I/O, no clocks, no state beyond the one response
//! under observation.
//!
//! - [`sse`]: the incremental event splitter ([`SseSplitter`]) — tolerant
//!   of both `\n\n` and `\r\n\r\n` event delimiters (no end-of-stream
//!   fallback for the `\r\n` dialect), skipping keep-alive comments,
//!   other fields, and `[DONE]`, capturing the `event:` line's value
//!   (the responses dialect's kind names — the openai/anthropic
//!   observers ignore it), and buffering only the pending partial
//!   event so per-chunk work stays small however long the stream runs.
//! - [`usage`]: the per-response [`UsageObserver`] — latches `id`,
//!   `model`, `provider`, and the verbatim `usage` object from the final
//!   usage-bearing chunk, with earlier chunks as fallback.
//! - [`anthropic`]: the Anthropic Messages sibling, [`AnthropicObserver`]
//!   — folds `message_start`/`message_delta` usage into one set of buckets
//!   (ctp's `foldUsage`: TTL-split reconciliation, iterations fallback),
//!   latches model/stop reason/speed/geo, and captures `error` events for
//!   error rows.
//!
//! Invariant 6 (accounting must never break a session): every parse path
//! here is infallible at the stream level. Malformed JSON, non-JSON
//! events, and invalid UTF-8 are skipped as unobservable — a parse
//! failure loses a measurement, never a request. A stream with nothing
//! usage-bearing on it (a client hangup, an all-keepalive stream)
//! produces no capture and so no row.
//!
//! Invariant 3 (absence ≠ zero): every captured field is `Option`. An
//! absent field and an explicit `null` both land as `None` — never `0`,
//! never `""` — while a present `0` is a real `0`.
//!
//! Invariant 1 (no content stored): the observer keeps ids, model and
//! provider names, and the usage object. Message deltas, tool-call
//! arguments, and every other choice-bearing field are parsed past and
//! dropped.

pub mod anthropic;
pub mod sse;
pub mod usage;

pub use anthropic::{AnthropicCapture, AnthropicObserver, Measurement, UsagePresence, measurement};
pub use sse::{SseEvent, SseSplitter};
pub use usage::{UsageCapture, UsageObserver};
