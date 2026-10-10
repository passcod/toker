//! Per-response usage observation for the OpenAI Chat shape.
//!
//! The back half of the Server core's side-parser: a [`UsageObserver`]
//! takes the events the [`SseSplitter`](super::sse::SseSplitter) emits for
//! one response — plus the whole body on the non-streaming path — and
//! finishes with the [`UsageCapture`] the ledger row needs.
//!
//! Latching (plan: Server core): `id`, `model`, and `provider` latch from
//! every chunk that carries them, so earlier chunks are the fallback and
//! the final usage-bearing chunk wins when it repeats them. The `usage`
//! object latches wholesale from the final chunk that bears one — an
//! earlier snapshot is never mixed into a later one, since a blend of two
//! snapshots would fabricate a measurement (invariant 3).
//!
//! Verbatim capture: `usage_raw` and `cost_details` hold the original
//! data-line bytes of those objects — borrowed out of the event text via
//! [`serde_json::value::RawValue`], never a re-serialisation — because the
//! store's `usage_raw` column is byte-verbatim by design and the ledger
//! proxy stored the provider's usage object verbatim. Numbers inside are
//! read through the `arbitrary_precision` Value, so `cost` keeps the
//! provider's literal precision on its way to f64.
//!
//! Invariant 3 (absence ≠ zero): every field below is `Option`. An absent
//! field and an explicit `null` both land as `None` and are never
//! fabricated as `0`; a present `0` is a real `0`. The `total_tokens` and
//! `cost_details` siblings the ledger does not model are not latched at
//! all.
//!
//! [`UsageObserver::finish`] returns `None` when nothing usage-bearing was
//! seen — a client hangup or an all-keepalive stream records no row.

use serde_json::Value;
use serde_json::value::RawValue;

use super::sse::SseEvent;

/// The raw-JSON extraction shape for the verbatim `usage` object: borrows
/// the value's bytes out of the original event text.
#[derive(serde::Deserialize)]
struct UsageText<'a> {
    #[serde(borrow)]
    usage: Option<&'a RawValue>,
}

/// The same trick one level down, for the verbatim `cost_details` object.
#[derive(serde::Deserialize)]
struct CostDetailsText<'a> {
    #[serde(borrow)]
    cost_details: Option<&'a RawValue>,
}

/// What one response's usage observation captured: exactly the fields the
/// ledger row needs, all optional, no content (invariant 1).
///
/// Absence semantics (invariant 3): every accessor returns `None` for an
/// absent field *and* for an explicit `null` — the two are not
/// distinguished, and neither is ever reported as `0`. A present `0` is a
/// real `0` and comes back as `Some(0)`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsageCapture {
    id: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    service_tier: Option<String>,
    usage_raw: Option<String>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cost: Option<f64>,
    cost_details: Option<String>,
}

impl UsageCapture {
    /// The response id (`id` at the top level of the chunk).
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// The model that served the response (openrouter echoes the model
    /// per chunk).
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The serving provider — openrouter's field naming the upstream
    /// endpoint.
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    /// The processing tier the provider says served the request.
    pub fn service_tier(&self) -> Option<&str> {
        self.service_tier.as_deref()
    }

    /// The usage object exactly as it arrived: the original data-line
    /// bytes, not a re-serialisation. This is what the store's
    /// `usage_raw` column holds, byte for byte.
    pub fn usage_raw(&self) -> Option<&str> {
        self.usage_raw.as_deref()
    }

    /// `usage.prompt_tokens`.
    pub fn prompt_tokens(&self) -> Option<u64> {
        self.prompt_tokens
    }

    /// `usage.completion_tokens`.
    pub fn completion_tokens(&self) -> Option<u64> {
        self.completion_tokens
    }

    /// `usage.prompt_tokens_details.cached_tokens`.
    pub fn cached_tokens(&self) -> Option<u64> {
        self.cached_tokens
    }

    /// `usage.prompt_tokens_details.cache_write_tokens` — openrouter's
    /// cache-creation accounting (mostly zero; nonzero on the requests
    /// that ESTABLISH a cache entry, the expensive moments). Dropped
    /// before this field existed; the doc
    /// openrouter.ai/docs/guides/best-practices/prompt-caching names it
    /// and the live ledger's `usage_raw` carries it.
    pub fn cache_write_tokens(&self) -> Option<u64> {
        self.cache_write_tokens
    }

    /// `usage.completion_tokens_details.reasoning_tokens`.
    pub fn reasoning_tokens(&self) -> Option<u64> {
        self.reasoning_tokens
    }

    /// `usage.cost` as f64, parsed from the arbitrary-precision literal
    /// (openrouter's real billed cost). Absent or null cost — a provider
    /// that does not bill per request — is `None`, never `0.0`.
    pub fn cost(&self) -> Option<f64> {
        self.cost
    }

    /// The `usage.cost_details` object's original bytes, verbatim like
    /// [`UsageCapture::usage_raw`].
    pub fn cost_details(&self) -> Option<&str> {
        self.cost_details.as_deref()
    }
}

/// Live per-response usage observation: feed it the response's SSE events
/// (or its whole JSON body), then [`UsageObserver::finish`].
///
/// One observer per response. Every `observe_*` method is infallible:
/// input the observer cannot understand is skipped as unobservable
/// (invariant 6) — a lost measurement, never a lost response.
#[derive(Debug, Default)]
pub struct UsageObserver {
    capture: UsageCapture,
    saw_usage: bool,
}

impl UsageObserver {
    /// A fresh observer with nothing latched.
    pub fn new() -> UsageObserver {
        UsageObserver::default()
    }

    /// Observe one SSE event. Non-JSON data, JSON that is not an object,
    /// and chunks without a `usage` object all contribute at most
    /// id/model/provider latches and are otherwise skipped.
    pub fn observe_event(&mut self, event: &SseEvent) {
        self.observe_text(&event.data());
    }

    /// Observe a complete non-streaming JSON response body. A body that
    /// is not valid UTF-8 or not JSON is skipped as unobservable.
    pub fn observe_json(&mut self, body: &[u8]) {
        if let Ok(text) = std::str::from_utf8(body) {
            self.observe_text(text);
        }
    }

    /// Finish the response: the capture, or `None` when nothing
    /// usage-bearing was seen (a client hangup or an all-keepalive
    /// stream records no row).
    pub fn finish(self) -> Option<UsageCapture> {
        if self.saw_usage {
            Some(self.capture)
        } else {
            None
        }
    }

    /// The shared latch path over one JSON document's text (an event's
    /// joined data, or a non-streaming body).
    fn observe_text(&mut self, text: &str) {
        // Invariant 6: malformed JSON is unobservable, never an error.
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let Some(object) = value.as_object() else {
            return; // valid JSON, wrong shape (an array, a bare number)
        };

        // Latch id/model/provider from every object chunk: later chunks
        // overwrite, so the final usage-bearing chunk wins when it
        // repeats them and earlier chunks are the fallback when it
        // omits them.
        for (key, latched) in [
            ("id", &mut self.capture.id),
            ("model", &mut self.capture.model),
            ("provider", &mut self.capture.provider),
            ("service_tier", &mut self.capture.service_tier),
        ] {
            if let Some(seen) = object.get(key).and_then(Value::as_str) {
                *latched = Some(seen.to_string());
            }
        }

        // Usage-bearing means a present object: an absent or explicitly
        // null `usage` (or one of the wrong shape) is not usage-bearing.
        let Some(usage) = object.get("usage").filter(|usage| usage.is_object()) else {
            return;
        };

        // The whole usage latch is replaced, not merged: fields the final
        // usage-bearing object omits reset to None rather than surviving
        // from an earlier snapshot (invariant 3 — a blend of snapshots
        // would fabricate a measurement).
        self.capture.prompt_tokens = u64_at(usage, "prompt_tokens");
        self.capture.completion_tokens = u64_at(usage, "completion_tokens");
        self.capture.cached_tokens = usage
            .get("prompt_tokens_details")
            .and_then(|details| u64_at(details, "cached_tokens"));
        self.capture.cache_write_tokens = usage
            .get("prompt_tokens_details")
            .and_then(|details| u64_at(details, "cache_write_tokens"));
        self.capture.reasoning_tokens = usage
            .get("completion_tokens_details")
            .and_then(|details| u64_at(details, "reasoning_tokens"));
        self.capture.cost = usage.get("cost").and_then(Value::as_f64);

        // Verbatim capture: borrow the usage object's exact bytes out of
        // the original text, never a re-serialisation. If extraction
        // somehow fails on text that parsed, the typed fields above
        // still latch and the raw column stays honestly absent.
        self.capture.usage_raw = serde_json::from_str::<UsageText<'_>>(text)
            .ok()
            .and_then(|raw| raw.usage.map(|usage| usage.get().to_string()));
        self.capture.cost_details = self
            .capture
            .usage_raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<CostDetailsText<'_>>(raw).ok())
            .and_then(|raw| raw.cost_details.map(|details| details.get().to_string()));

        self.saw_usage = true;
    }
}

/// A u64 field: absent, null, and non-numeric all read as `None` (absence
/// ≠ zero); a present `0` is a real `0`.
fn u64_at(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::{UsageCapture, UsageObserver};
    use crate::observe::sse::SseSplitter;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/openai_chat_sse")
    }

    fn fixture(name: &str) -> Vec<u8> {
        fs::read(fixtures_dir().join(name)).expect("fixture exists")
    }

    /// Run a whole byte stream through splitter + observer and finish.
    fn observe(parts: &[&[u8]]) -> Option<UsageCapture> {
        let mut splitter = SseSplitter::new();
        let mut observer = UsageObserver::new();
        for part in parts {
            for event in splitter.feed(part) {
                observer.observe_event(&event);
            }
        }
        if let Some(event) = splitter.finish() {
            observer.observe_event(&event);
        }
        observer.finish()
    }

    #[test]
    fn simple_stream_fixture_is_captured() {
        let capture = observe(&[&fixture("01_simple_content.sse")]).expect("usage-bearing");

        assert_eq!(capture.id(), Some("gen-1760000000-3f2a"));
        assert_eq!(capture.model(), Some("z-ai/glm-5.3"));
        assert_eq!(capture.provider(), Some("z-ai"));
        assert_eq!(capture.prompt_tokens(), Some(128));
        assert_eq!(capture.completion_tokens(), Some(16));
        assert_eq!(capture.cost(), Some(0.000192));
        assert_eq!(
            capture.cost_details(),
            Some(r#"{"upstream":0.00016,"router":0.000032}"#)
        );
        // Byte-verbatim: the usage object exactly as the data line carried
        // it, key order and number literals included.
        assert_eq!(
            capture.usage_raw(),
            Some(concat!(
                r#"{"prompt_tokens":128,"completion_tokens":16,"total_tokens":144,"#,
                r#""cost":0.000192,"cost_details":{"upstream":0.00016,"router":0.000032}}"#,
            ))
        );

        // Invariant 1: the deltas' content never surfaces anywhere in the
        // capture.
        assert!(!format!("{capture:?}").contains("Hello"));
    }

    #[test]
    fn tool_call_fixture_carries_the_token_details() {
        let capture = observe(&[&fixture("02_tool_calls.sse")]).expect("usage-bearing");
        assert_eq!(capture.id(), Some("gen-1760000100-77e1"));
        assert_eq!(capture.provider(), Some("z-ai"));
        assert_eq!(capture.prompt_tokens(), Some(512));
        assert_eq!(capture.completion_tokens(), Some(48));
        assert_eq!(capture.cached_tokens(), Some(384));
        assert_eq!(capture.reasoning_tokens(), Some(12));
        assert_eq!(capture.cost(), Some(0.00084));
        assert_eq!(
            capture.usage_raw(),
            Some(concat!(
                r#"{"prompt_tokens":512,"completion_tokens":48,"total_tokens":560,"#,
                r#""prompt_tokens_details":{"cached_tokens":384,"audio_tokens":0},"#,
                r#""completion_tokens_details":{"reasoning_tokens":12,"accepted_prediction_tokens":3},"#,
                r#""cost":0.00084,"cost_details":{"upstream":0.00072,"router":0.00012}}"#,
            ))
        );
        // The tool-call argument fragments are not in the capture either.
        assert!(!format!("{capture:?}").contains("Wellington"));
    }

    #[test]
    fn crlf_unicode_stream_fixture_is_captured() {
        let capture = observe(&[&fixture("03_unicode_crlf.sse")]).expect("usage-bearing");
        assert_eq!(capture.id(), Some("gen-1760000200-c0de"));
        assert_eq!(capture.model(), Some("deepseek/deepseek-chat-v4"));
        assert_eq!(capture.provider(), Some("deepseek"));
        assert_eq!(capture.prompt_tokens(), Some(96));
        assert_eq!(capture.completion_tokens(), Some(24));
        assert_eq!(capture.cached_tokens(), Some(64));
        assert_eq!(capture.cost(), Some(0.000288));
        assert_eq!(capture.cost_details(), Some(r#"{"upstream":0.00024}"#));
    }

    #[test]
    fn interim_usage_chunks_lose_to_the_final_one() {
        let capture = observe(&[&fixture("04_interim_usage.sse")]).expect("usage-bearing");
        // The final usage-bearing chunk wins over the message_start-ish
        // interim one.
        assert_eq!(capture.prompt_tokens(), Some(802));
        assert_eq!(capture.completion_tokens(), Some(40));
        assert_eq!(capture.cached_tokens(), Some(512));
        assert_eq!(capture.cost(), Some(0.001688));
        assert_eq!(
            capture.usage_raw(),
            Some(concat!(
                r#"{"prompt_tokens":802,"completion_tokens":40,"total_tokens":842,"#,
                r#""prompt_tokens_details":{"cached_tokens":512},"#,
                r#""cost":0.001688,"cost_details":{"upstream":0.00144}}"#,
            ))
        );
    }

    #[test]
    fn stream_without_usage_records_no_row() {
        // The fixture completed but the provider never sent usage; id and
        // model latched anyway, yet finish is None: nothing usage-bearing
        // was seen.
        let capture = observe(&[&fixture("05_no_usage.sse")]);
        assert_eq!(capture, None);

        // Keep-alives only.
        let all_keepalive = b": OPENROUTER PROCESSING\n\n: ping\n\n";
        assert_eq!(observe(&[all_keepalive]), None);

        // A hangup mid-content: delta chunks, no usage, no [DONE].
        let hangup = b"data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n";
        assert_eq!(observe(&[hangup]), None);
    }

    #[test]
    fn captures_match_at_every_chunk_boundary() {
        // The end-to-end observation property: no chunk boundary — mid
        // delimiter, mid multibyte — changes what is captured.
        for name in [
            "01_simple_content.sse",
            "02_tool_calls.sse",
            "03_unicode_crlf.sse",
            "04_interim_usage.sse",
            "05_no_usage.sse",
        ] {
            let bytes = fixture(name);
            let whole = observe(&[&bytes]);
            for i in 0..=bytes.len() {
                assert_eq!(
                    observe(&[&bytes[..i], &bytes[i..]]),
                    whole,
                    "{name}: boundary at {i}"
                );
            }
        }
    }

    #[test]
    fn earlier_chunks_latch_id_model_provider_as_fallback() {
        // The final usage-bearing chunk omits id/model/provider; the
        // earlier chunks' values survive as the fallback.
        let stream = concat!(
            "data: {\"id\":\"gen-f1\",\"model\":\"z-ai/glm-5.3\",\"provider\":\"z-ai\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        );
        let capture = observe(&[stream.as_bytes()]).expect("usage-bearing");
        assert_eq!(capture.id(), Some("gen-f1"));
        assert_eq!(capture.model(), Some("z-ai/glm-5.3"));
        assert_eq!(capture.provider(), Some("z-ai"));
        assert_eq!(capture.prompt_tokens(), Some(7));
        assert_eq!(capture.completion_tokens(), Some(3));
        assert_eq!(
            capture.usage_raw(),
            Some(r#"{"prompt_tokens":7,"completion_tokens":3}"#)
        );
    }

    #[test]
    fn later_id_model_provider_chunks_overwrite() {
        // A later chunk repeating id/model/provider wins over an earlier
        // one (final usage-bearing chunk wins).
        let stream = concat!(
            "data: {\"id\":\"old\",\"model\":\"old/model\",\"provider\":\"old\",\"choices\":[]}\n\n",
            "data: {\"id\":\"new\",\"model\":\"new/model\",\"provider\":\"new\",\"choices\":[],\"usage\":{\"prompt_tokens\":1}}\n\n",
        );
        let capture = observe(&[stream.as_bytes()]).expect("usage-bearing");
        assert_eq!(capture.id(), Some("new"));
        assert_eq!(capture.model(), Some("new/model"));
        assert_eq!(capture.provider(), Some("new"));
    }

    #[test]
    fn later_usage_replaces_earlier_usage_wholesale() {
        // The interim usage has cached_tokens; the final one does not.
        // The final object replaces the latch wholesale — cached_tokens
        // resets to None rather than blending snapshots (invariant 3).
        let stream = concat!(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":9},\"cost\":0.5}}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20}}\n\n",
        );
        let capture = observe(&[stream.as_bytes()]).expect("usage-bearing");
        assert_eq!(capture.prompt_tokens(), Some(20));
        assert_eq!(capture.cached_tokens(), None);
        assert_eq!(capture.cost(), None);
        assert_eq!(capture.usage_raw(), Some(r#"{"prompt_tokens":20}"#));
    }

    #[test]
    fn absence_is_not_zero() {
        // Absent fields, explicit nulls, and non-numeric values are all
        // None — never fabricated as 0.
        let stream = concat!(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":null,",
            "\"completion_tokens\":\"8\",\"total_tokens\":5}}\n\n",
        );
        let capture = observe(&[stream.as_bytes()]).expect("usage-bearing");
        assert_eq!(capture.prompt_tokens(), None, "explicit null is None");
        assert_eq!(capture.completion_tokens(), None, "non-numeric is None");
        assert_eq!(capture.cached_tokens(), None, "absent details is None");
        assert_eq!(capture.reasoning_tokens(), None);
        assert_eq!(capture.cost(), None, "absent cost is None, not 0.0");
        assert_eq!(capture.cost_details(), None);

        // A present 0 is a real 0, including zero cost.
        let stream = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"cost\":0}}\n\n";
        let capture = observe(&[stream.as_bytes()]).expect("usage-bearing");
        assert_eq!(capture.prompt_tokens(), Some(0));
        assert_eq!(capture.cost(), Some(0.0));

        // Explicit null usage is not usage-bearing: no capture, no row.
        let stream = "data: {\"id\":\"x\",\"choices\":[],\"usage\":null}\n\n";
        assert_eq!(observe(&[stream.as_bytes()]), None);
    }

    #[test]
    fn non_streaming_json_body_is_captured() {
        let body = concat!(
            r#"{"id":"gen-1760000500-9d3c","provider":"z-ai","model":"z-ai/glm-5.3","#,
            r#""object":"chat.completion","created":1760000500,"#,
            r#""choices":[{"index":0,"message":{"role":"assistant","content":"Done"},"finish_reason":"stop"}],"#,
            r#""usage":{"prompt_tokens":64,"completion_tokens":8,"total_tokens":72,"#,
            r#""cost":0.0000000123456789,"cost_details":{"upstream":0.0000000100000}},"system_fingerprint":null}"#,
        );
        let mut observer = UsageObserver::new();
        observer.observe_json(body.as_bytes());
        let capture = observer.finish().expect("usage-bearing");
        assert_eq!(capture.id(), Some("gen-1760000500-9d3c"));
        assert_eq!(capture.model(), Some("z-ai/glm-5.3"));
        assert_eq!(capture.provider(), Some("z-ai"));
        assert_eq!(capture.prompt_tokens(), Some(64));
        assert_eq!(capture.completion_tokens(), Some(8));
        // The arbitrary-precision literal survives to f64 at full
        // precision, and the raw text keeps the provider's bytes.
        assert_eq!(capture.cost(), Some(0.0000000123456789));
        assert_eq!(
            capture.usage_raw(),
            Some(concat!(
                r#"{"prompt_tokens":64,"completion_tokens":8,"total_tokens":72,"#,
                r#""cost":0.0000000123456789,"cost_details":{"upstream":0.0000000100000}}"#,
            ))
        );
        assert_eq!(
            capture.cost_details(),
            Some(r#"{"upstream":0.0000000100000}"#)
        );

        // Bodies the observer cannot read lose the measurement, never the
        // response (invariant 6): invalid UTF-8 and non-JSON are skipped.
        let mut observer = UsageObserver::new();
        observer.observe_json(&[0xff, 0xfe]);
        observer.observe_json(b"not json at all");
        assert_eq!(observer.finish(), None);
    }

    #[test]
    fn unobservable_events_are_skipped_without_losing_later_ones() {
        let stream = concat!(
            "data: not json\n\n", // non-JSON data
            "data: {\"a\":",      // JSON split over two data
            "\n",                 // lines: unparseable joined
            "data: 1}\n\n",       // (multi-line data joins \n)
            "data: [1,2]\n\n",    // JSON of the wrong shape
            "data: 42\n\n",       // ditto
            "data: {\"id\":\"keep\",\"choices\":[],\"usage\":{\"prompt_tokens\":9}}\n\n",
        );
        let capture = observe(&[stream.as_bytes()]).expect("usage-bearing");
        assert_eq!(capture.id(), Some("keep"));
        assert_eq!(capture.prompt_tokens(), Some(9));
    }
}
