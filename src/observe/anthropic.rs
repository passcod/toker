//! Anthropic Messages usage observation — the sibling of the OpenAI-chat
//! [`usage`](super::usage) observer, over the same
//! [`SseSplitter`](super::sse::SseSplitter) events.
//!
//! Ports the predecessor's stream accounting: the `message_start` /
//! `message_delta` usage latch and the [`fold`] that collapses them into
//! one set of buckets. `message_start` carries the cache-creation TTL split
//! but only a provisional `output_tokens`; the final `message_delta`
//! carries authoritative totals, but its TTL split may live inside an
//! `iterations[]` array — a response that fell back across models has more
//! than one iteration, so the array is summed rather than trusting the
//! top-level scalars.
//!
//! TTL split reconciliation: the 5m/1h write split is
//! checked against the authoritative `cache_creation_input_tokens` total,
//! and any unexplained remainder is charged to the 1h tier — the expensive
//! one — so the estimate errs high rather than quietly under-reporting,
//! with `ttl_split_known = false` marking the apportionment. When only the
//! total is present with no split at all, the same rule applies: the whole
//! write charges to 1h, split unknown.
//!
//! Invariant 3 (absence ≠ zero): every bucket is `Option`. An absent field
//! and an explicit `null` are `None`, never `0`; a present `0` is a real
//! `0`. The [`UsagePresence`] map records which metrics the response
//! actually carried, so a reader can tell a reported zero from an
//! apportioned or absent one — the additive contract the predecessor's
//! usage accounting documents, extracted here from the response itself.
//! Where the fold
//! apportions despite absence (the 1h remainder, the forced 5m/1h zeros
//! when there is nothing to split), the bucket still holds the number the
//! ledger needs while presence stays `false`: a value without presence is
//! an estimate, not a measurement.
//!
//! Invariant 1 (no content stored): text deltas, tool-input fragments,
//! and content blocks are parsed past and dropped; the capture keeps
//! model, stop reason, speed/geo, the error pair, and token counts only.
//!
//! Invariant 6 (accounting must never break a session): every path here is
//! infallible. Malformed JSON, non-JSON events, and invalid UTF-8 are
//! skipped as unobservable. [`AnthropicObserver::finish`] returns `None`
//! when nothing usage-bearing or error-bearing was seen — a client hangup
//! or an all-keepalive stream records no row.

use serde_json::{Value, json};

use super::sse::SseEvent;

/// Which usage metrics the response actually carried, metric → reported.
///
/// The row's additive presence contract:
/// keys are this row's metric names and a
/// `false` means the provider did not report the metric even when the
/// compatibility numeric column holds a finite value (the fold's
/// apportionment). The predecessor's key spellings map on import:
/// `input`→`input`,
/// `cacheRead`→`cache_read`, `cacheCreateTotal`→`cache_write_total`,
/// `write5m`→`cache_write_5m`, `write1h`→`cache_write_1h`, `output`→
/// `output`, `thinking`→`reasoning`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UsagePresence {
    /// `input_tokens`, in the fold's selected sources.
    pub input: bool,
    /// `cache_read_input_tokens`, in the fold's selected sources.
    pub cache_read: bool,
    /// `cache_creation_input_tokens`, in the fold's selected sources.
    pub cache_write_total: bool,
    /// `cache_creation.ephemeral_5m_input_tokens`, in the split the fold
    /// used (selected sources, or the start fallback).
    pub cache_write_5m: bool,
    /// `cache_creation.ephemeral_1h_input_tokens`, likewise.
    pub cache_write_1h: bool,
    /// `output_tokens`, in the fold's selected sources.
    pub output: bool,
    /// `output_tokens_details.thinking_tokens` on the final delta
    /// (thinking is read from the delta only, never the start).
    pub reasoning: bool,
    /// `server_tool_use.web_search_requests` (delta first, start fallback).
    pub web_searches: bool,
    /// `server_tool_use.code_execution_requests`, likewise.
    pub code_execs: bool,
}

impl UsagePresence {
    /// The presence map as the row stores it: metric → bool, absence of a
    /// report explicit rather than implied.
    pub fn to_json(&self) -> Value {
        json!({
            "input": self.input,
            "cache_read": self.cache_read,
            "cache_write_total": self.cache_write_total,
            "cache_write_5m": self.cache_write_5m,
            "cache_write_1h": self.cache_write_1h,
            "output": self.output,
            "reasoning": self.reasoning,
            "web_searches": self.web_searches,
            "code_execs": self.code_execs,
        })
    }
}

/// One metric's verdict for a presence-aware reader: the
/// measurement-interpretation rules for a row that carries the map
/// (rules 1–3; rule 4 — legacy rows without a map reading
/// missing-as-zero — has
/// no toker rows to apply to).
///
/// `false` presence wins over a numeric value: the compatibility number is
/// an apportionment, not a measurement. A reported metric includes an
/// explicit zero. A claimed presence without a finite value is
/// unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measurement {
    /// The provider reported the metric; the value includes reported zeros.
    Measured(u64),
    /// The provider did not report the metric, or claimed it without a
    /// finite number.
    Unavailable,
}

/// Interpret one bucket against its presence flag (see [`Measurement`]).
pub fn measurement(value: Option<u64>, reported: bool) -> Measurement {
    match (value, reported) {
        (_, false) => Measurement::Unavailable,
        (Some(value), true) => Measurement::Measured(value),
        (None, true) => Measurement::Unavailable,
    }
}

/// What one Anthropic response's observation captured: the folded usage
/// buckets plus the response facts the row needs. All optional fields are
/// absent when absent, never zero (invariant 3).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AnthropicCapture {
    model: Option<String>,
    stop_reason: Option<String>,
    speed: Option<String>,
    geo: Option<String>,
    error_type: Option<String>,
    error_message: Option<String>,
    message_stop: bool,
    input: Option<u64>,
    cache_read: Option<u64>,
    cache_write_total: Option<u64>,
    cache_write_5m: Option<u64>,
    cache_write_1h: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    web_searches: Option<u64>,
    code_execs: Option<u64>,
    iterations: u64,
    ttl_split_known: Option<bool>,
    presence: UsagePresence,
    cost: Option<f64>,
    serving_provider: Option<String>,
}

impl AnthropicCapture {
    /// `usage.cost` from the authoritative usage, as a provider that bills
    /// per request reports it (openrouter's Anthropic endpoint, in USD).
    /// Anthropic itself reports none; absent stays absent.
    pub fn cost(&self) -> Option<f64> {
        self.cost
    }

    /// The response's top-level `provider`: the upstream openrouter
    /// served the request through. Absent from Anthropic's own responses.
    pub fn serving_provider(&self) -> Option<&str> {
        self.serving_provider.as_deref()
    }

    /// The served model, verbatim from the response (`message.model`, or
    /// the non-streaming body's top level) — pre-normalisation; the row
    /// stores this as `raw_model` and the catalog normalises it for
    /// `model`.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The response's stop reason (`message_delta.delta.stop_reason`, or the
    /// non-streaming body's top level).
    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }

    /// The response's serving-speed tier (`usage.speed` / `message.speed`);
    /// `"fast"` is the flag the fast-mode price table keys on.
    pub fn speed(&self) -> Option<&str> {
        self.speed.as_deref()
    }

    /// The serving edge the provider reports (`usage.inference_geo`);
    /// `"us"` is the value the 1.1× multiplier keys on.
    pub fn geo(&self) -> Option<&str> {
        self.geo.as_deref()
    }

    /// The first error event's `error.type`, for an error row.
    pub fn error_type(&self) -> Option<&str> {
        self.error_type.as_deref()
    }

    /// The first error event's `error.message`.
    pub fn error_message(&self) -> Option<&str> {
        self.error_message.as_deref()
    }

    /// Whether a `message_stop` event was observed: the response completed
    /// its own protocol, as opposed to a stream cut off mid-flight (the
    /// server knows hangups by its own signal; this is the body's).
    pub fn message_stop_observed(&self) -> bool {
        self.message_stop
    }

    /// Base input tokens (`input_tokens`).
    pub fn input(&self) -> Option<u64> {
        self.input
    }

    /// Cache-read tokens (`cache_read_input_tokens`).
    pub fn cache_read(&self) -> Option<u64> {
        self.cache_read
    }

    /// Total cache-write tokens (`cache_creation_input_tokens`).
    pub fn cache_write_total(&self) -> Option<u64> {
        self.cache_write_total
    }

    /// The 5m-TTL cache-write share. `Some` whenever the total is: the fold
    /// partitions the total, apportioning by rule — see
    /// [`ttl_split_known`] and [`presence`] for what the number is worth.
    ///
    /// [`presence`]: AnthropicCapture::presence
    pub fn cache_write_5m(&self) -> Option<u64> {
        self.cache_write_5m
    }

    /// The 1h-TTL cache-write share, carrying any unexplained remainder
    /// from reconciliation (the expensive tier, by design).
    pub fn cache_write_1h(&self) -> Option<u64> {
        self.cache_write_1h
    }

    /// Output tokens (thinking included in the count).
    pub fn output(&self) -> Option<u64> {
        self.output
    }

    /// Thinking tokens (`output_tokens_details.thinking_tokens`; the row's
    /// `reasoning` bucket — the predecessor called it `thinking`).
    pub fn reasoning(&self) -> Option<u64> {
        self.reasoning
    }

    /// Server-side web searches (`server_tool_use.web_search_requests`).
    pub fn web_searches(&self) -> Option<u64> {
        self.web_searches
    }

    /// Server-side code executions
    /// (`server_tool_use.code_execution_requests`).
    pub fn code_execs(&self) -> Option<u64> {
        self.code_execs
    }

    /// Agentic iterations the response folded: `iterations[].len()` when
    /// the response fell back across models, else 1.
    pub fn iterations(&self) -> u64 {
        self.iterations
    }

    /// Whether the 5m/1h split is a reported split that reconciled against
    /// the total — `false` when the fold apportioned an unknown remainder
    /// (or the whole total) to the 1h tier, `None` when no cache write was
    /// reported at all, in which case there is nothing to split.
    pub fn ttl_split_known(&self) -> Option<bool> {
        self.ttl_split_known
    }

    /// Which usage metrics the response actually carried.
    pub fn presence(&self) -> UsagePresence {
        self.presence
    }
}

/// Live per-response observation for the Anthropic Messages shape: feed it
/// the response's SSE events (or its whole non-streaming JSON body), then
/// [`AnthropicObserver::finish`].
///
/// One observer per response. Every observe method is infallible: input
/// the observer cannot understand is skipped as unobservable (invariant 6)
/// — a lost measurement, never a lost response. The start and delta usage
/// objects latch whole (later `message_delta` events replace earlier ones,
/// since `output_tokens` arrives cumulative); the fold happens once, at
/// [`finish`](AnthropicObserver::finish).
#[derive(Debug, Default)]
pub struct AnthropicObserver {
    /// `message_start`'s `message.usage` — the provisional snapshot.
    start_usage: Option<Value>,
    /// The latest `message_delta`'s usage — authoritative, cumulative.
    delta_usage: Option<Value>,
    model: Option<String>,
    stop_reason: Option<String>,
    speed: Option<String>,
    geo: Option<String>,
    error: Option<(Option<String>, Option<String>)>,
    message_stop: bool,
    /// The top-level `provider` (`message_start`'s message, or the
    /// non-streaming body).
    provider: Option<String>,
}

impl AnthropicObserver {
    /// A fresh observer with nothing latched.
    pub fn new() -> AnthropicObserver {
        AnthropicObserver::default()
    }

    /// Observe one SSE event. Events without a recognisable `type`, and
    /// payload shapes the observer cannot read, contribute at most
    /// model/stop/error latches and are otherwise skipped.
    pub fn observe_event(&mut self, event: &SseEvent) {
        self.observe_text(&event.data());
    }

    /// Observe a complete non-streaming JSON response body (the buffered
    /// path: the body's top-level `usage` plays the delta's role as the
    /// authoritative snapshot, its `model`/`stop_reason` latch, and its
    /// `error` object latches for an error row). A body that is not valid
    /// UTF-8, not JSON, or not an object is skipped as unobservable.
    pub fn observe_json(&mut self, body: &[u8]) {
        let Ok(text) = std::str::from_utf8(body) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let Some(object) = value.as_object() else {
            return;
        };
        if let Some(usage) = object.get("usage").filter(|usage| usage.is_object()) {
            self.delta_usage = Some(usage.clone());
            self.latch_speed_geo(usage);
        }
        if let Some(model) = object.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_owned());
        }
        if let Some(stop_reason) = object.get("stop_reason").and_then(Value::as_str) {
            self.stop_reason = Some(stop_reason.to_owned());
        }
        if let Some(provider) = object.get("provider").and_then(Value::as_str) {
            self.provider = Some(provider.to_owned());
        }
        if object.get("error").is_some() {
            self.observe_error(object);
        }
    }

    /// Finish the response: the capture, or `None` when nothing
    /// usage-bearing or error-bearing was seen (a client hangup or an
    /// all-keepalive stream records no row).
    pub fn finish(self) -> Option<AnthropicCapture> {
        let has_usage = self.start_usage.is_some() || self.delta_usage.is_some();
        if !has_usage && self.error.is_none() {
            return None;
        }
        Some(self.fold())
    }

    /// The shared latch path over one JSON document's text.
    fn observe_text(&mut self, text: &str) {
        // Invariant 6: malformed JSON is unobservable, never an error.
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let Some(object) = value.as_object() else {
            return; // valid JSON, wrong shape
        };
        match object.get("type").and_then(Value::as_str) {
            Some("message_start") => self.observe_start(object),
            Some("message_delta") => self.observe_delta(object),
            Some("message_stop") => self.message_stop = true,
            Some("error") => self.observe_error(object),
            _ => {} // content blocks, pings, anything else: parsed past
        }
    }

    /// `message_start`: the provisional usage snapshot plus the response's
    /// model.
    fn observe_start(&mut self, object: &serde_json::Map<String, Value>) {
        let Some(message) = object.get("message").and_then(Value::as_object) else {
            return;
        };
        if let Some(usage) = message.get("usage").filter(|usage| usage.is_object()) {
            self.start_usage = Some(usage.clone());
            self.latch_speed_geo(usage);
        }
        if let Some(model) = message.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_owned());
        }
        if let Some(provider) = message.get("provider").and_then(Value::as_str) {
            self.provider = Some(provider.to_owned());
        }
        // The message-level speed is read beside the usage one.
        if self.speed.is_none()
            && let Some(speed) = message.get("speed").and_then(Value::as_str)
        {
            self.speed = Some(speed.to_owned());
        }
    }

    /// `message_delta`: the authoritative usage snapshot (each delta
    /// replaces the last — `output_tokens` arrives cumulative) and the stop
    /// reason.
    fn observe_delta(&mut self, object: &serde_json::Map<String, Value>) {
        if let Some(usage) = object.get("usage").filter(|usage| usage.is_object()) {
            self.delta_usage = Some(usage.clone());
            self.latch_speed_geo(usage);
        }
        if let Some(stop_reason) = object
            .get("delta")
            .and_then(Value::as_object)
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(Value::as_str)
        {
            self.stop_reason = Some(stop_reason.to_owned());
        }
    }

    /// `error`: the first error event latches — the initial failure
    /// explains the stream; later ones are usually retry echoes.
    fn observe_error(&mut self, object: &serde_json::Map<String, Value>) {
        if self.error.is_some() {
            return;
        }
        let Some(error) = object.get("error").and_then(Value::as_object) else {
            return;
        };
        self.error = Some((
            error.get("type").and_then(Value::as_str).map(str::to_owned),
            error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ));
    }

    /// The non-streaming body path and the message_start/delta speed/geo
    /// latch share this: `usage.speed` and `usage.inference_geo`.
    fn latch_speed_geo(&mut self, usage: &Value) {
        if let Some(speed) = usage.get("speed").and_then(Value::as_str) {
            self.speed = Some(speed.to_owned());
        }
        if let Some(geo) = usage.get("inference_geo").and_then(Value::as_str) {
            self.geo = Some(geo.to_owned());
        }
    }

    /// Collapse the latched snapshots into the capture.
    fn fold(self) -> AnthropicCapture {
        // Source selection: a non-empty `iterations[]` on the delta is
        // authoritative and summed; otherwise the delta alone is (the
        // start's scalars are a second report of the same measurement, and
        // summing both would double-count); the start alone when no delta
        // ever arrived.
        let iterations: Vec<&Value> = self
            .delta_usage
            .as_ref()
            .and_then(|delta| delta.get("iterations"))
            .and_then(Value::as_array)
            .map(|entries| entries.iter().filter(|entry| entry.is_object()).collect())
            .unwrap_or_default();
        let using_iterations = !iterations.is_empty();
        let iterations_count = if using_iterations {
            iterations.len() as u64
        } else {
            1
        };
        let scalar_sources: Vec<&Value> = if using_iterations {
            iterations
        } else {
            match (&self.delta_usage, &self.start_usage) {
                (Some(delta), _) => vec![delta],
                (None, Some(start)) => vec![start],
                (None, None) => vec![],
            }
        };

        let input = sum_metric(&scalar_sources, "input_tokens");
        let cache_read = sum_metric(&scalar_sources, "cache_read_input_tokens");
        let cache_write_total = sum_metric(&scalar_sources, "cache_creation_input_tokens");
        let output = sum_metric(&scalar_sources, "output_tokens");

        // The TTL split: from the selected sources' `cache_creation`
        // objects, falling back to the start's (replace, not add —
        // the fallback only runs when no selected source carried a split).
        let mut split_seen = false;
        let mut w5_seen = false;
        let mut w1_seen = false;
        let mut w5 = 0u64;
        let mut w1 = 0u64;
        for source in &scalar_sources {
            if let Some(split) = source.get("cache_creation").and_then(Value::as_object) {
                split_seen = true;
                if let Some(tokens) = split
                    .get("ephemeral_5m_input_tokens")
                    .and_then(Value::as_u64)
                {
                    w5 = w5.saturating_add(tokens);
                    w5_seen = true;
                }
                if let Some(tokens) = split
                    .get("ephemeral_1h_input_tokens")
                    .and_then(Value::as_u64)
                {
                    w1 = w1.saturating_add(tokens);
                    w1_seen = true;
                }
            }
        }
        if !split_seen
            && let Some(start) = &self.start_usage
            && let Some(split) = start.get("cache_creation").and_then(Value::as_object)
        {
            split_seen = true;
            if let Some(tokens) = split
                .get("ephemeral_5m_input_tokens")
                .and_then(Value::as_u64)
            {
                w5 = tokens;
                w5_seen = true;
            }
            if let Some(tokens) = split
                .get("ephemeral_1h_input_tokens")
                .and_then(Value::as_u64)
            {
                w1 = tokens;
                w1_seen = true;
            }
        }

        // Reconciliation against the authoritative total.
        let (cache_write_5m, cache_write_1h, ttl_split_known) = match cache_write_total {
            // No cache write reported: nothing to split, no claim to make.
            None => (None, None, None),
            // Nothing to split is a known split.
            Some(0) => (Some(0), Some(0), Some(true)),
            Some(total) if !split_seen => {
                // Only the total, no split: charge it all to the 1h
                // (expensive) tier and say the split is unknown.
                (Some(0), Some(total), Some(false))
            }
            Some(total) => {
                let split_sum = w5 as i128 + w1 as i128;
                if split_sum == total as i128 {
                    (Some(w5), Some(w1), Some(true))
                } else {
                    // The unknown remainder goes to the 1h tier; a split
                    // that over-reports the total clamps there (w5 pins
                    // to the total rather than going negative).
                    let w1_adjusted = w1 as i128 + (total as i128 - split_sum);
                    if w1_adjusted < 0 {
                        (Some(total), Some(0), Some(false))
                    } else {
                        (Some(w5), Some(w1_adjusted as u64), Some(false))
                    }
                }
            }
        };

        // Thinking and server tools come from the delta's top level only —
        // the start's provisional snapshot has neither, and iterations
        // carry token counts, not tool counts.
        let reasoning = self
            .delta_usage
            .as_ref()
            .and_then(|delta| delta.get("output_tokens_details"))
            .and_then(|details| details.get("thinking_tokens"))
            .and_then(Value::as_u64);
        let server_tools = self
            .delta_usage
            .as_ref()
            .and_then(|delta| delta.get("server_tool_use"))
            .filter(|tools| tools.is_object())
            .or_else(|| {
                self.start_usage
                    .as_ref()
                    .and_then(|start| start.get("server_tool_use"))
                    .filter(|tools| tools.is_object())
            });
        let web_searches = server_tools
            .and_then(|tools| tools.get("web_search_requests"))
            .and_then(Value::as_u64);
        let code_execs = server_tools
            .and_then(|tools| tools.get("code_execution_requests"))
            .and_then(Value::as_u64);

        let presence = UsagePresence {
            input: input.is_some(),
            cache_read: cache_read.is_some(),
            cache_write_total: cache_write_total.is_some(),
            cache_write_5m: w5_seen,
            cache_write_1h: w1_seen,
            output: output.is_some(),
            reasoning: reasoning.is_some(),
            web_searches: web_searches.is_some(),
            code_execs: code_execs.is_some(),
        };

        AnthropicCapture {
            model: self.model,
            stop_reason: self.stop_reason,
            speed: self.speed,
            geo: self.geo,
            error_type: self.error.as_ref().and_then(|(kind, _)| kind.clone()),
            error_message: self.error.as_ref().and_then(|(_, message)| message.clone()),
            message_stop: self.message_stop,
            input,
            cache_read,
            cache_write_total,
            cache_write_5m,
            cache_write_1h,
            output,
            reasoning,
            web_searches,
            code_execs,
            iterations: iterations_count,
            ttl_split_known,
            presence,
            // The delta's (or the body's) alone: openrouter puts the cost
            // on the final usage, and the start's provisional snapshot
            // has none to report.
            cost: self
                .delta_usage
                .as_ref()
                .and_then(|usage| usage.get("cost"))
                .and_then(Value::as_f64),
            serving_provider: self.provider,
        }
    }
}

/// Sum one metric over the fold's selected sources:
/// absent and malformed entries contribute nothing, and the sum is `None`
/// when no source reported the metric at all (absence ≠ zero).
fn sum_metric(sources: &[&Value], key: &str) -> Option<u64> {
    let mut sum = 0u64;
    let mut reported = false;
    for source in sources {
        if let Some(tokens) = source.get(key).and_then(Value::as_u64) {
            sum = sum.saturating_add(tokens);
            reported = true;
        }
    }
    reported.then_some(sum)
}

#[cfg(test)]
mod tests {
    use super::{AnthropicCapture, AnthropicObserver, Measurement, measurement};
    use crate::observe::sse::SseSplitter;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anthropic_sse")
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
            "the Anthropic SSE corpus must keep at least 5 fixtures"
        );
        names
    }

    fn fixture(name: &str) -> Vec<u8> {
        fs::read(fixtures_dir().join(name)).expect("fixture exists")
    }

    /// Run a whole byte stream through splitter + observer and finish.
    fn observe(parts: &[&[u8]]) -> Option<AnthropicCapture> {
        let mut splitter = SseSplitter::new();
        let mut observer = AnthropicObserver::new();
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

    /// One streaming response from start/delta usage objects.
    fn stream(start: &str, delta: &str) -> Option<AnthropicCapture> {
        let stream = format!(
            "event: message_start\ndata: {start}\n\n\
             event: message_delta\ndata: {delta}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        observe(&[stream.as_bytes()])
    }

    #[test]
    fn simple_text_stream_is_captured() {
        let capture = observe(&[&fixture("01_simple_text.sse")]).expect("usage-bearing");
        assert_eq!(capture.model(), Some("claude-sonnet-5"));
        assert_eq!(capture.stop_reason(), Some("end_turn"));
        assert_eq!(capture.input(), Some(12));
        assert_eq!(capture.cache_read(), Some(0), "explicit zero is real");
        assert_eq!(capture.cache_write_total(), Some(0));
        // Nothing to split is a known split, with the forced zeros marked
        // unreported: the totals say zero, the split was never carried.
        assert_eq!(capture.cache_write_5m(), Some(0));
        assert_eq!(capture.cache_write_1h(), Some(0));
        assert_eq!(capture.ttl_split_known(), Some(true));
        assert_eq!(
            capture.output(),
            Some(27),
            "delta's output wins over start's"
        );
        assert_eq!(capture.reasoning(), None);
        assert_eq!(capture.iterations(), 1);
        assert!(capture.message_stop_observed());
        assert_eq!(capture.speed(), None);
        assert_eq!(capture.geo(), None);
        let presence = capture.presence();
        assert!(presence.input && presence.cache_read && presence.cache_write_total);
        assert!(!presence.cache_write_5m && !presence.cache_write_1h);
        assert!(presence.output);
        assert!(!presence.reasoning && !presence.web_searches && !presence.code_execs);
    }

    #[test]
    fn tool_use_stream_carries_thinking_and_no_content() {
        let capture = observe(&[&fixture("02_tool_use.sse")]).expect("usage-bearing");
        assert_eq!(capture.model(), Some("claude-opus-5"));
        assert_eq!(capture.stop_reason(), Some("tool_use"));
        assert_eq!(capture.input(), Some(4));
        assert_eq!(capture.output(), Some(65));
        assert_eq!(capture.reasoning(), Some(22));
        // Invariant 1: neither the text nor the tool-input fragments (the
        // content the stream carried) surface anywhere in the capture.
        let debug = format!("{capture:?}");
        assert!(!debug.contains("Wellington"));
        assert!(!debug.contains("celsius"));
        assert!(!debug.contains("Checking"));
    }

    #[test]
    fn one_hour_write_with_split_and_fast_us_geo_is_captured() {
        let capture = observe(&[&fixture("03_1h_write_crlf.sse")]).expect("usage-bearing");
        assert_eq!(capture.model(), Some("claude-opus-5"));
        assert_eq!(capture.input(), Some(2));
        assert_eq!(capture.cache_write_total(), Some(82_420));
        assert_eq!(capture.cache_write_5m(), Some(0), "reported zero share");
        assert_eq!(capture.cache_write_1h(), Some(82_420));
        assert_eq!(capture.ttl_split_known(), Some(true));
        assert_eq!(capture.output(), Some(13));
        assert_eq!(capture.reasoning(), Some(0), "explicit zero is a real zero");
        assert_eq!(
            capture.iterations(),
            1,
            "single iteration, not the count 1 fallback"
        );
        assert_eq!(capture.speed(), Some("fast"));
        assert_eq!(capture.geo(), Some("us"));
        assert_eq!(capture.stop_reason(), Some("end_turn"));
        let presence = capture.presence();
        assert!(presence.cache_write_5m && presence.cache_write_1h && presence.reasoning);
        assert!(!debug_has(&capture, "Kia ora"), "text deltas are dropped");
    }

    #[test]
    fn five_minute_only_write_falls_back_to_the_start_split() {
        let capture = observe(&[&fixture("04_5m_only_write.sse")]).expect("usage-bearing");
        assert_eq!(capture.input(), Some(5));
        assert_eq!(capture.cache_read(), Some(82_420));
        assert_eq!(capture.cache_write_total(), Some(1_200));
        // The delta carried no split; the start's 5m-only split is the
        // fallback and reconciles exactly.
        assert_eq!(capture.cache_write_5m(), Some(1_200));
        assert_eq!(capture.cache_write_1h(), Some(0));
        assert_eq!(capture.ttl_split_known(), Some(true));
        assert_eq!(capture.output(), Some(2_400));
        assert_eq!(capture.reasoning(), Some(900));
        let presence = capture.presence();
        assert!(presence.cache_write_5m && presence.cache_write_1h);
    }

    #[test]
    fn iterations_fallback_sums_the_array_not_the_scalars() {
        let capture = observe(&[&fixture("05_iterations_fallback.sse")]).expect("usage-bearing");
        // When the delta carries non-empty iterations[], the top-level
        // scalars are ignored — the array is summed.
        assert_eq!(capture.input(), Some(6), "3 + 3, not the 999 top level");
        assert_eq!(
            capture.cache_read(),
            Some(5),
            "0 + 5, not the 999 top level"
        );
        assert_eq!(capture.output(), Some(30), "12 + 18");
        assert_eq!(capture.cache_write_total(), Some(500));
        assert_eq!(capture.cache_write_5m(), Some(500));
        assert_eq!(capture.cache_write_1h(), Some(0));
        assert_eq!(capture.ttl_split_known(), Some(true));
        assert_eq!(capture.iterations(), 2);
        // The second iteration carried no 1h share at all: the forced
        // zero is present as a value but not as a report.
        assert!(!capture.presence().cache_write_1h);
        assert!(capture.presence().cache_write_5m);
    }

    #[test]
    fn error_event_mid_stream_latches_for_an_error_row() {
        let capture = observe(&[&fixture("06_error_event.sse")]).expect("error-bearing");
        assert_eq!(capture.error_type(), Some("overloaded_error"));
        assert_eq!(capture.error_message(), Some("Overloaded"));
        // The start's provisional usage still latched: the row can carry
        // both the failure and what was measured before it.
        assert_eq!(capture.model(), Some("claude-haiku-4-5-20251001"));
        assert_eq!(capture.input(), Some(400));
        assert_eq!(capture.output(), Some(1), "provisional start output");
        assert_eq!(capture.stop_reason(), None);
        assert!(!capture.message_stop_observed());
        // Invariant 1 again: the partial reply text is not in the capture.
        assert!(!debug_has(&capture, "Searching"));
    }

    #[test]
    fn stream_without_usage_records_no_row() {
        assert_eq!(observe(&[&fixture("07_no_usage.sse")]), None);

        // Keep-alives only: also nothing.
        let pings = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";
        assert_eq!(observe(&[pings]), None);

        // A hangup mid-content: deltas but never a usage event.
        let hangup =
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"half\"}}\n\n";
        assert_eq!(observe(&[hangup]), None);
    }

    #[test]
    fn captures_match_at_every_chunk_boundary() {
        // The end-to-end observation property: no chunk boundary — mid
        // delimiter, mid multibyte (fixture 03 carries CRLF and an emoji)
        // — changes what is captured.
        for name in fixture_names() {
            let bytes = fixture(&name);
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
    fn captures_match_byte_at_a_time() {
        for name in fixture_names() {
            let bytes = fixture(&name);
            let mut splitter = SseSplitter::new();
            let mut observer = AnthropicObserver::new();
            for &byte in &bytes {
                for event in splitter.feed(&[byte]) {
                    observer.observe_event(&event);
                }
            }
            if let Some(event) = splitter.finish() {
                observer.observe_event(&event);
            }
            assert_eq!(
                observer.finish(),
                observe(&[&bytes]),
                "{name}: byte-at-a-time splits"
            );
        }
    }

    #[test]
    fn start_only_streams_fold_the_provisional_snapshot() {
        // No message_delta ever arrived (upstream closed early but clean):
        // account from the start's usage alone.
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-5\",\"usage\":{\"input_tokens\":7,\"cache_read_input_tokens\":40,\"cache_creation_input_tokens\":0,\"output_tokens\":2}}}\n\n",
        );
        let capture = observe(&[stream.as_bytes()]).expect("usage-bearing");
        assert_eq!(capture.input(), Some(7));
        assert_eq!(capture.cache_read(), Some(40));
        assert_eq!(capture.output(), Some(2), "provisional start output");
        assert_eq!(capture.iterations(), 1);
        assert!(!capture.message_stop_observed());
    }

    #[test]
    fn a_billed_cost_comes_from_the_final_usage_and_absent_stays_absent() {
        // OpenRouter's shape: the provider on the message, the cost on the
        // delta. Anthropic's own responses carry neither.
        let capture = stream(
            r#"{"type":"message_start","message":{"model":"m","provider":"Friendli","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":18,"output_tokens":34,"cost":0.0000197}}"#,
        )
        .expect("usage-bearing");
        assert_eq!(capture.cost(), Some(0.0000197));
        assert_eq!(capture.serving_provider(), Some("Friendli"));

        let capture = stream(
            r#"{"type":"message_start","message":{"model":"m","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":1,"output_tokens":2}}"#,
        )
        .expect("usage-bearing");
        assert_eq!(capture.cost(), None, "no cost reported, none recorded");
        assert_eq!(capture.serving_provider(), None);

        let mut observer = AnthropicObserver::new();
        observer.observe_json(
            br#"{"model":"m","usage":{"input_tokens":18,"output_tokens":32,"cost":1.87e-05},"provider":"Friendli"}"#,
        );
        let capture = observer.finish().expect("usage-bearing");
        assert_eq!(capture.cost(), Some(1.87e-05));
        assert_eq!(capture.serving_provider(), Some("Friendli"));
    }

    #[test]
    fn later_message_deltas_replace_not_sum() {
        // output_tokens arrives cumulative: the final delta is the
        // authoritative total, never a sum of the deltas that carried it.
        let start = r#"{"type":"message_start","message":{"model":"m","usage":{"input_tokens":10,"output_tokens":1}}}"#;
        let first =
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":10,"output_tokens":5}}"#;
        let second =
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":10,"output_tokens":50}}"#;
        let stream = format!(
            "event: message_start\ndata: {start}\n\n\
             event: message_delta\ndata: {first}\n\n\
             event: message_delta\ndata: {second}\n\n"
        );
        let capture = observe(&[stream.as_bytes()]).expect("usage-bearing");
        assert_eq!(capture.output(), Some(50), "5 + 50 would be a double count");
    }

    #[test]
    fn empty_iterations_array_falls_back_to_the_delta_scalars() {
        // An empty array is no fallback.
        let capture = stream(
            r#"{"type":"message_start","message":{"model":"m","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":4,"output_tokens":9,"iterations":[]}}"#,
        )
        .expect("usage-bearing");
        assert_eq!(
            capture.input(),
            Some(4),
            "the delta's scalars are the source"
        );
        assert_eq!(capture.output(), Some(9));
        assert_eq!(capture.iterations(), 1);
    }

    #[test]
    fn total_without_any_split_charges_the_whole_write_to_1h() {
        let capture = stream(
            r#"{"type":"message_start","message":{"model":"m","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":700}}}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":700}}"#,
        )
        .expect("usage-bearing");
        assert_eq!(capture.cache_write_total(), Some(700));
        assert_eq!(capture.cache_write_5m(), Some(0));
        assert_eq!(
            capture.cache_write_1h(),
            Some(700),
            "the expensive tier, by rule"
        );
        assert_eq!(capture.ttl_split_known(), Some(false));
        let presence = capture.presence();
        assert!(presence.cache_write_total);
        assert!(!presence.cache_write_5m && !presence.cache_write_1h);
        // The value/presence split is the additive contract: the 1h bucket
        // holds the estimate, the reader knows it was not reported.
        assert_eq!(
            measurement(capture.cache_write_1h(), presence.cache_write_1h),
            Measurement::Unavailable
        );
        assert_eq!(
            measurement(capture.cache_write_total(), presence.cache_write_total),
            Measurement::Measured(700)
        );
    }

    #[test]
    fn split_that_does_not_reconcile_charges_the_remainder_to_1h() {
        // 400 + 500 reported, 1,000 authoritative: the extra 100 lands on
        // 1h and the split is marked unknown.
        let capture = stream(
            r#"{"type":"message_start","message":{"model":"m","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":1000,"cache_creation":{"ephemeral_5m_input_tokens":400,"ephemeral_1h_input_tokens":500}}}}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":1000}}"#,
        )
        .expect("usage-bearing");
        assert_eq!(capture.cache_write_5m(), Some(400));
        assert_eq!(capture.cache_write_1h(), Some(600));
        assert_eq!(capture.ttl_split_known(), Some(false));
        // The reported halves stay measured; the 1h total is an estimate.
        assert!(capture.presence().cache_write_1h);
    }

    #[test]
    fn split_that_over_reports_clamps_to_the_total() {
        // 400 + 0 reported against a 100 total: the fold clamps w1 up from
        // -300 and pins w5 to the total.
        let capture = stream(
            r#"{"type":"message_start","message":{"model":"m","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":400,"ephemeral_1h_input_tokens":0}}}}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":100}}"#,
        )
        .expect("usage-bearing");
        assert_eq!(capture.cache_write_5m(), Some(100));
        assert_eq!(capture.cache_write_1h(), Some(0));
        assert_eq!(capture.ttl_split_known(), Some(false));
    }

    #[test]
    fn a_cache_creation_object_without_ephemeral_keys_is_a_zero_split() {
        // A present `cache_creation` object itself claims the split
        // exists, so the fold does not fall back to the start — and the
        // empty split reconciles the whole total onto 1h, unknown.
        let capture = stream(
            r#"{"type":"message_start","message":{"model":"m","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":300,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20}}}}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":300,"cache_creation":{}}}"#,
        )
        .expect("usage-bearing");
        assert_eq!(capture.cache_write_5m(), Some(0));
        assert_eq!(capture.cache_write_1h(), Some(300));
        assert_eq!(capture.ttl_split_known(), Some(false));
        assert!(!capture.presence().cache_write_5m);
        assert!(!capture.presence().cache_write_1h);
    }

    #[test]
    fn server_tool_use_counts_come_from_delta_then_start() {
        let capture = stream(
            r#"{"type":"message_start","message":{"model":"claude-haiku-4-5-20251001","usage":{"input_tokens":400,"output_tokens":1,"server_tool_use":{"web_search_requests":2,"code_execution_requests":1}}}}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":400,"output_tokens":120}}"#,
        )
        .expect("usage-bearing");
        assert_eq!(capture.web_searches(), Some(2), "start fallback");
        assert_eq!(capture.code_execs(), Some(1));

        // The delta's own server tools win over the start's.
        let capture = stream(
            r#"{"type":"message_start","message":{"model":"m","usage":{"input_tokens":1,"output_tokens":1,"server_tool_use":{"web_search_requests":2}}}}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":1,"output_tokens":2,"server_tool_use":{"web_search_requests":5}}}"#,
        )
        .expect("usage-bearing");
        assert_eq!(capture.web_searches(), Some(5));
        // An empty object is still the delta's claim: no fallback, and no
        // metric reported.
        let capture = stream(
            r#"{"type":"message_start","message":{"model":"m","usage":{"input_tokens":1,"output_tokens":1,"server_tool_use":{"web_search_requests":2}}}}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":1,"output_tokens":2,"server_tool_use":{}}}"#,
        )
        .expect("usage-bearing");
        assert_eq!(capture.web_searches(), None);
        assert!(!capture.presence().web_searches);
    }

    #[test]
    fn absence_is_not_zero_throughout() {
        // A usage object with only output_tokens: nothing else is
        // fabricated, and nothing to split makes no split claim.
        let capture = stream(
            r#"{"type":"message_start","message":{"model":"m","usage":{"output_tokens":3}}}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"output_tokens":9}}"#,
        )
        .expect("usage-bearing");
        assert_eq!(capture.input(), None);
        assert_eq!(capture.cache_read(), None);
        assert_eq!(capture.cache_write_total(), None);
        assert_eq!(capture.cache_write_5m(), None);
        assert_eq!(capture.cache_write_1h(), None);
        assert_eq!(capture.ttl_split_known(), None);
        assert_eq!(capture.output(), Some(9));
        let presence = capture.presence();
        assert!(presence.output);
        assert!(!presence.input && !presence.cache_read && !presence.cache_write_total);

        // Explicit nulls are absence too, and non-numbers are unobservable.
        let capture = stream(
            r#"{"type":"message_start","message":{"model":"m","usage":{"input_tokens":null,"output_tokens":2}}}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":"8","output_tokens":4,"cache_read_input_tokens":12}}"#,
        )
        .expect("usage-bearing");
        assert_eq!(capture.input(), None, "a string number is not a number");
        assert_eq!(capture.output(), Some(4));
        assert_eq!(capture.cache_read(), Some(12));
    }

    #[test]
    fn non_streaming_json_body_is_captured() {
        let body = concat!(
            r#"{"id":"msg_08","type":"message","role":"assistant","model":"claude-opus-5","#,
            r#""stop_reason":"end_turn","stop_sequence":null,"content":[{"type":"text","text":"Done."}],"#,
            r#""usage":{"input_tokens":9,"cache_creation_input_tokens":400,"cache_read_input_tokens":100,"#,
            r#""cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":300},"#,
            r#""output_tokens":40,"server_tool_use":{"web_search_requests":2},"speed":"standard","inference_geo":"us"}}"#,
        );
        let mut observer = AnthropicObserver::new();
        observer.observe_json(body.as_bytes());
        let capture = observer.finish().expect("usage-bearing");
        assert_eq!(capture.model(), Some("claude-opus-5"));
        assert_eq!(capture.stop_reason(), Some("end_turn"));
        assert_eq!(capture.input(), Some(9));
        assert_eq!(capture.cache_read(), Some(100));
        assert_eq!(capture.cache_write_total(), Some(400));
        assert_eq!(capture.cache_write_5m(), Some(100));
        assert_eq!(capture.cache_write_1h(), Some(300));
        assert_eq!(capture.ttl_split_known(), Some(true));
        assert_eq!(capture.output(), Some(40));
        assert_eq!(capture.web_searches(), Some(2));
        assert_eq!(capture.speed(), Some("standard"));
        assert_eq!(capture.geo(), Some("us"));
        assert_eq!(capture.iterations(), 1);
        assert!(!debug_has(&capture, "Done."), "content is dropped");

        // An error body latches the error pair with no usage buckets.
        let mut observer = AnthropicObserver::new();
        observer.observe_json(
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"mock 429"}}"#,
        );
        let capture = observer.finish().expect("error-bearing");
        assert_eq!(capture.error_type(), Some("rate_limit_error"));
        assert_eq!(capture.error_message(), Some("mock 429"));
        assert_eq!(capture.input(), None);

        // Bodies the observer cannot read lose the measurement, never the
        // response (invariant 6).
        let mut observer = AnthropicObserver::new();
        observer.observe_json(&[0xff, 0xfe]);
        observer.observe_json(b"not json at all");
        observer.observe_json(b"[1,2]");
        assert_eq!(observer.finish(), None);
    }

    #[test]
    fn first_error_event_wins_and_only_errors_make_an_error_capture() {
        let stream = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"first\"}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"second\"}}\n\n",
        );
        let capture = observe(&[stream.as_bytes()]).expect("error-bearing");
        assert_eq!(capture.error_type(), Some("overloaded_error"));
        assert_eq!(capture.error_message(), Some("first"));

        // An error event with no shape the observer reads is not
        // error-bearing: no capture, no row.
        let stream = "event: error\ndata: {\"type\":\"error\"}\n\n";
        assert_eq!(observe(&[stream.as_bytes()]), None);
    }

    #[test]
    fn unobservable_events_are_skipped_without_losing_later_ones() {
        let stream = concat!(
            "data: not json\n\n",            // non-JSON data
            "data: [1,2]\n\n",               // JSON of the wrong shape
            "data: {\"type\":\"ping\"}\n\n", // a type the observer parses past
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":6}}\n\n",
        );
        let capture = observe(&[stream.as_bytes()]).expect("usage-bearing");
        assert_eq!(capture.output(), Some(6));
    }

    #[test]
    fn presence_map_serialises_with_the_row_metric_names() {
        let capture = observe(&[&fixture("04_5m_only_write.sse")]).expect("usage-bearing");
        let json = capture.presence().to_json();
        assert_eq!(
            json,
            serde_json::json!({
                "input": true,
                "cache_read": true,
                "cache_write_total": true,
                "cache_write_5m": true,
                "cache_write_1h": true,
                "output": true,
                "reasoning": true,
                "web_searches": false,
                "code_execs": false,
            })
        );
    }

    #[test]
    fn measurement_interpretation_is_rules_one_to_three() {
        // Rule 1: false presence wins over a finite value.
        assert_eq!(measurement(Some(700), false), Measurement::Unavailable);
        // Rule 2: a finite value is measured, including an explicit zero,
        // whether the key says true or the value is present.
        assert_eq!(measurement(Some(0), true), Measurement::Measured(0));
        assert_eq!(measurement(Some(42), true), Measurement::Measured(42));
        // Rule 3: claimed presence without a finite value is unavailable.
        assert_eq!(measurement(None, true), Measurement::Unavailable);
        // Absence of both is unavailable, never zero.
        assert_eq!(measurement(None, false), Measurement::Unavailable);
    }

    fn debug_has(capture: &AnthropicCapture, needle: &str) -> bool {
        format!("{capture:?}").contains(needle)
    }
}
