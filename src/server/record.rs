//! Ledger row assembly for the proxy path.
//!
//! The server's completion points (stream end, buffered body end, error
//! body end) hand a [`RecordCtx`] plus what the side-observation captured
//! to the record functions here, which build the [`RequestRow`], insert
//! it, and emit the per-request tracing line.
//!
//! Token-bucket normalization mirrors the ledger proxy's join plugin
//! exactly (`openrouter-ledger-proxy/plugins/ledger-cost/join.mjs`), which
//! the attribution join (phase 5) will rely on both sides observing the
//! same arithmetic:
//!
//! - `input  = max(0, prompt_tokens − cached_tokens)` — join.mjs also
//!   subtracts `cache_write_tokens`; openrouter reports none (an
//!   anthropic-ism), so the term contributes 0 exactly like join.mjs's
//!   `num()` coercion of a missing value.
//! - `output = max(0, completion_tokens − reasoning_tokens)` — clamps at 0
//!   like join.mjs's `Math.max(0, …)` (a provider that reports
//!   `reasoning > completion` clamps rather than going negative).
//! - `cache_read = cached_tokens`, `reasoning = reasoning_tokens` —
//!   unclamped in join.mjs, unclamped here.
//!
//! Absence ≠ zero (invariant 3): a bucket derived from an absent source
//! metric stays `None` (input needs `prompt_tokens`, output needs
//! `completion_tokens`); a bucket that *is* its source metric (cache_read,
//! reasoning) is absent when the metric is. Which metrics the provider
//! actually reported lands in `usage_presence`.
//!
//! The clock ([`now_ms`], [`RecordCtx::started`]) feeds row fields only —
//! ts and duration — never any byte decision (invariant 4).
//!
//! The recording path must never fail a request (invariant 6): store
//! errors are logged and swallowed; the insert is a single-row local
//! SQLite write, brief enough to run inline at stream completion.

use std::time::Instant;

use serde_json::{Value, json};

use crate::ir::Shape;
use crate::middleware::awake;
use crate::observe::UsageCapture;
use crate::store::{CostKind, RequestRow, RowKind};

use super::Server;

/// Everything a completion point knows about one request, minus the
/// response itself.
pub(crate) struct RecordCtx {
    /// The server (for the store and the route/backend id).
    pub(crate) server: Server,
    /// Request start, for `duration_ms` — the only clock use (invariant 4).
    pub(crate) started: Instant,
    /// Session identity, read by header name only (invariant 2).
    pub(crate) session_id: Option<String>,
    /// The model the frontend asked for, `provider/model` routing included.
    pub(crate) requested_model: Option<String>,
    /// The model that served, after the routing rewrite (phase 1: the
    /// `openrouter/` prefix strip; nothing else rewrites).
    pub(crate) effective_model: Option<String>,
    /// The fidelity monitor's divergence digest, when re-serialisation
    /// drifted from the original bytes (invariant 5).
    pub(crate) drift: Option<String>,
    /// The content-free request shape ([`crate::ir::openai_chat::Shape`]).
    pub(crate) shape: Option<Shape>,
    /// System-family message count, for the row's `system_messages`.
    pub(crate) system_messages: Option<u64>,
}

/// Epoch milliseconds now. The clock's only role: row fields.
pub(crate) fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// Wall-clock duration in milliseconds, saturated to i64.
pub(crate) fn elapsed_ms(started: Instant) -> i64 {
    started.elapsed().as_millis().min(i64::MAX as u128) as i64
}

/// u64 → i64 for the row's integer columns, saturated.
pub(crate) fn i64_of(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Record a completed usage-path response: the measurement row (when usage
/// was observed — a completed stream with nothing usage-bearing records no
/// row, like a hung-up one), plus the fidelity-drift row when the request
/// drifted. `status` is the upstream status the client saw.
pub(crate) fn record_measurement(ctx: &RecordCtx, capture: Option<&UsageCapture>, status: u16) {
    let ts_ms = now_ms();
    let duration_ms = elapsed_ms(ctx.started);
    let route = route_of(ctx);

    if let Some(digest) = &ctx.drift {
        insert(ctx, drift_row(ts_ms, &route, digest));
    }

    let model = capture.and_then(UsageCapture::model);
    if let Some(capture) = capture {
        insert(
            ctx,
            measurement_row(ctx, ts_ms, duration_ms, &route, capture),
        );
    }

    tracing::info!(
        "POST /v1/chat/completions → {} ledgered={} model={} provider={}{} ({:.1}s)",
        status,
        if capture.is_some() { "yes" } else { "no" },
        model.or(ctx.effective_model.as_deref()).unwrap_or("?"),
        ctx.server.openrouter.id(),
        drift_note(ctx),
        ctx.started.elapsed().as_secs_f64(),
    );
}

/// Record a non-2xx usage-path response (plan: Server core): an error row
/// with status, error type, and retry-after — never priced, no usage
/// buckets — plus the fidelity-drift row when the request drifted.
pub(crate) fn record_error(
    ctx: &RecordCtx,
    status: u16,
    error_type: Option<String>,
    retry_after_ms: Option<i64>,
) {
    let ts_ms = now_ms();
    let duration_ms = elapsed_ms(ctx.started);
    let route = route_of(ctx);

    if let Some(digest) = &ctx.drift {
        insert(ctx, drift_row(ts_ms, &route, digest));
    }
    insert(
        ctx,
        error_row(
            ctx,
            ts_ms,
            duration_ms,
            &route,
            status,
            error_type,
            retry_after_ms,
        ),
    );

    tracing::info!(
        "POST /v1/chat/completions → {} ledgered=error model={} provider={}{} ({:.1}s)",
        status,
        ctx.effective_model.as_deref().unwrap_or("?"),
        ctx.server.openrouter.id(),
        drift_note(ctx),
        ctx.started.elapsed().as_secs_f64(),
    );
}

/// The route column, `frontend:backend`.
fn route_of(ctx: &RecordCtx) -> String {
    format!("openai_chat:{}", ctx.server.openrouter.id())
}

/// Record one sleep-lock transition (ctp: `kind: "awake"`, the row
/// `evaluateAwake` writes on every held flip, proxy.mjs:472-479).
///
/// The row is what separates "released because the sessions went quiet"
/// from "the lock quietly stopped working": both leave a machine that
/// sleeps. `want` differing from `held` is a lock that could not be
/// taken. ctp's shape `{held, want, until, reason}` rides the
/// kind-specific payload column; `until` is epoch milliseconds (ctp
/// logged an ISO string — toker's rows keep the ts_ms convention), `None`
/// where the hold rests on something without an expiry or there is no
/// hold. No duration, no session, never priced — a proxy-written row,
/// excluded from API measurements by its kind.
pub(crate) fn record_awake(server: &Server, transition: &awake::AwakeTransition, now: i64) {
    let row = RequestRow {
        id: None,
        ts_ms: now,
        duration_ms: None,
        kind: Some(RowKind::Awake),
        frontend: None,
        provider: None,
        route: None,
        session_id: None,
        ping: None,
        model: None,
        raw_model: None,
        requested_model: None,
        effective_model: None,
        input: None,
        cache_read: None,
        cache_write_total: None,
        cache_write_5m: None,
        cache_write_1h: None,
        output: None,
        reasoning: None,
        iterations: None,
        web_searches: None,
        code_execs: None,
        ttl_split_known: None,
        usage_presence: None,
        usage_raw: None,
        cost_usd: None,
        cost_kind: None,
        rate_limits: None,
        req_bytes: None,
        req_messages: None,
        req_tools: None,
        tools_hash: None,
        system_chars: None,
        system_hash: None,
        system_blocks: None,
        system_messages: None,
        compact_generations: None,
        summarising: None,
        system_change: None,
        system_ladder: None,
        system_tail: None,
        gate_on: None,
        cold_on: None,
        forced_from: None,
        forced_to: None,
        downgraded_from: None,
        downgraded_to: None,
        cache_stripped: None,
        system_merged: None,
        model_mappings: None,
        drift_digest: None,
        status: None,
        error_type: None,
        retry_after_ms: None,
        extra: Some(json!({
            "held": transition.held,
            "want": transition.want,
            "until": transition.until,
            "reason": transition.reason,
        })),
        betas: None,
        geo: None,
        fast: None,
    };
    if let Err(error) = server.store.record_request(&row) {
        tracing::error!(%error, "ledger insert failed");
    }
    tracing::info!(
        "sleep lock {} ({})",
        if transition.held { "held" } else { "released" },
        transition.reason,
    );
}

fn drift_note(ctx: &RecordCtx) -> String {
    match &ctx.drift {
        Some(digest) => format!(" drift={digest}"),
        None => String::new(),
    }
}

/// Insert one row; a store failure loses the row, never the request
/// (invariant 6) — it is logged as the visible breakage it is.
fn insert(ctx: &RecordCtx, row: RequestRow) {
    if let Err(error) = ctx.server.store.record_request(&row) {
        tracing::error!(%error, "ledger insert failed");
    }
}

/// The normalized token buckets from one capture (see the module docs for
/// the formulas and their join.mjs lineage).
pub(crate) struct TokenBuckets {
    /// `max(0, prompt_tokens − cached_tokens)`.
    pub input: Option<i64>,
    /// `max(0, completion_tokens − reasoning_tokens)`.
    pub output: Option<i64>,
    /// `cached_tokens`, unclamped.
    pub cache_read: Option<i64>,
    /// `cache_write_tokens`, unclamped — the cache-creation count.
    pub cache_write_total: Option<i64>,
    /// `reasoning_tokens`, unclamped.
    pub reasoning: Option<i64>,
    /// metric → reported? Distinguishes "reported zero" from "absent".
    pub usage_presence: Value,
}

/// Normalize one capture into the ledger's token buckets (see the module
/// docs).
pub(crate) fn token_buckets(capture: &UsageCapture) -> TokenBuckets {
    // join.mjs's num(): a missing sub-metric contributes 0 to the
    // arithmetic of a metric that *is* present.
    let cached = capture.cached_tokens().unwrap_or(0);
    let written = capture.cache_write_tokens().unwrap_or(0);
    let reasoning = capture.reasoning_tokens().unwrap_or(0);
    TokenBuckets {
        input: capture
            .prompt_tokens()
            .map(|prompt| i64_of(prompt.saturating_sub(cached).saturating_sub(written))),
        output: capture
            .completion_tokens()
            .map(|completion| i64_of(completion.saturating_sub(reasoning))),
        cache_read: capture.cached_tokens().map(i64_of),
        cache_write_total: capture.cache_write_tokens().map(i64_of),
        reasoning: capture.reasoning_tokens().map(i64_of),
        usage_presence: json!({
            "prompt_tokens": capture.prompt_tokens().is_some(),
            "completion_tokens": capture.completion_tokens().is_some(),
            "cached_tokens": capture.cached_tokens().is_some(),
            "cache_write_tokens": capture.cache_write_tokens().is_some(),
            "reasoning_tokens": capture.reasoning_tokens().is_some(),
            "cost": capture.cost().is_some(),
        }),
    }
}

/// The measurement row (kind `None` = a real API measurement): requested
/// vs effective model, buckets, verbatim usage, billed cost when reported,
/// shape fields, and openrouter's serving provider in `extra` (ledger
/// parity; there is no dedicated column).
fn measurement_row(
    ctx: &RecordCtx,
    ts_ms: i64,
    duration_ms: i64,
    route: &str,
    capture: &UsageCapture,
) -> RequestRow {
    let buckets = token_buckets(capture);
    let cost = capture.cost();
    let model = capture.model();
    // openrouter's serving provider and the response id, for cross-
    // referencing the provider's own logs (ledger parity); only keys that
    // exist land in the JSON — absence stays absence.
    let mut extra_map = serde_json::Map::new();
    if let Some(provider) = capture.provider() {
        extra_map.insert("serving_provider".to_owned(), json!(provider));
    }
    if let Some(id) = capture.id() {
        extra_map.insert("response_id".to_owned(), json!(id));
    }
    let extra = (!extra_map.is_empty()).then(|| Value::Object(extra_map));
    let shape = ctx.shape.as_ref();
    RequestRow {
        id: None,
        ts_ms,
        duration_ms: Some(duration_ms),
        kind: None,
        frontend: Some("openai_chat".to_owned()),
        provider: Some(ctx.server.openrouter.id().to_owned()),
        route: Some(route.to_owned()),
        session_id: ctx.session_id.clone(),
        ping: None,
        model: model.map(str::to_owned),
        // Verbatim from the provider, pre-normalisation — identical to
        // `model` in phase 1 (no model normalisation exists yet).
        raw_model: model.map(str::to_owned),
        requested_model: ctx.requested_model.clone(),
        effective_model: ctx.effective_model.clone(),
        input: buckets.input,
        cache_read: buckets.cache_read,
        // The wire has no TTL tiers: the conservative apportionment
        // charges the whole write to the 1-hour tier, flagged as an
        // apportioned guess — never a silent cheaper split.
        cache_write_total: buckets.cache_write_total,
        cache_write_5m: None,
        cache_write_1h: buckets.cache_write_total,
        output: buckets.output,
        reasoning: buckets.reasoning,
        iterations: None,
        web_searches: None,
        code_execs: None,
        ttl_split_known: buckets.cache_write_total.is_some().then_some(false),
        usage_presence: Some(buckets.usage_presence),
        usage_raw: capture.usage_raw().map(str::to_owned),
        cost_usd: cost,
        // Billed when the provider reported a cost; absent cost is never
        // estimated (that is a later unit's explicit kind).
        cost_kind: cost.map(|_| CostKind::Billed),
        rate_limits: None,
        req_bytes: shape.map(|s| i64_of(s.req_bytes)),
        req_messages: shape.and_then(|s| s.req_messages.map(i64_of)),
        req_tools: shape.map(|s| i64_of(s.req_tools)),
        tools_hash: shape.and_then(|s| s.tools_hash.clone()),
        system_chars: shape.map(|s| i64_of(s.system_chars)),
        system_hash: shape.map(|s| s.system_hash.clone()),
        system_blocks: shape.map(|s| {
            Value::Array(
                s.system_blocks
                    .iter()
                    .map(|block| json!({"hash": block.hash, "chars": i64_of(block.chars)}))
                    .collect(),
            )
        }),
        system_messages: ctx.system_messages.map(i64_of),
        compact_generations: None,
        summarising: None,
        system_change: None,
        system_ladder: None,
        system_tail: None,
        gate_on: None,
        cold_on: None,
        forced_from: None,
        forced_to: None,
        downgraded_from: None,
        downgraded_to: None,
        cache_stripped: None,
        system_merged: None,
        model_mappings: None,
        drift_digest: None,
        status: None,
        error_type: None,
        retry_after_ms: None,
        extra,
        betas: None,
        geo: None,
        fast: None,
    }
}

/// The error row (plan: non-2xx on a usage path): status, error type, and
/// retry-after; never priced, no usage. Deliberately lean — the request's
/// shape is not re-measured on the way to a failure the provider already
/// summarised.
fn error_row(
    ctx: &RecordCtx,
    ts_ms: i64,
    duration_ms: i64,
    route: &str,
    status: u16,
    error_type: Option<String>,
    retry_after_ms: Option<i64>,
) -> RequestRow {
    RequestRow {
        id: None,
        ts_ms,
        duration_ms: Some(duration_ms),
        kind: Some(RowKind::Error),
        frontend: Some("openai_chat".to_owned()),
        provider: Some(ctx.server.openrouter.id().to_owned()),
        route: Some(route.to_owned()),
        session_id: ctx.session_id.clone(),
        ping: None,
        model: None,
        raw_model: None,
        requested_model: ctx.requested_model.clone(),
        effective_model: ctx.effective_model.clone(),
        input: None,
        cache_read: None,
        cache_write_total: None,
        cache_write_5m: None,
        cache_write_1h: None,
        output: None,
        reasoning: None,
        iterations: None,
        web_searches: None,
        code_execs: None,
        ttl_split_known: None,
        usage_presence: None,
        usage_raw: None,
        cost_usd: None,
        cost_kind: None,
        rate_limits: None,
        req_bytes: None,
        req_messages: None,
        req_tools: None,
        tools_hash: None,
        system_chars: None,
        system_hash: None,
        system_blocks: None,
        system_messages: None,
        compact_generations: None,
        summarising: None,
        system_change: None,
        system_ladder: None,
        system_tail: None,
        gate_on: None,
        cold_on: None,
        forced_from: None,
        forced_to: None,
        downgraded_from: None,
        downgraded_to: None,
        cache_stripped: None,
        system_merged: None,
        model_mappings: None,
        drift_digest: None,
        status: Some(status as i64),
        error_type,
        retry_after_ms,
        extra: None,
        betas: None,
        geo: None,
        fast: None,
    }
}

/// The fidelity-drift row (invariant 5): drift is a visible, queryable
/// metric. Lean by design — frontend, route, digest, time.
fn drift_row(ts_ms: i64, route: &str, digest: &str) -> RequestRow {
    RequestRow {
        id: None,
        ts_ms,
        duration_ms: None,
        kind: Some(RowKind::FidelityDrift),
        frontend: Some("openai_chat".to_owned()),
        provider: None,
        route: Some(route.to_owned()),
        session_id: None,
        ping: None,
        model: None,
        raw_model: None,
        requested_model: None,
        effective_model: None,
        input: None,
        cache_read: None,
        cache_write_total: None,
        cache_write_5m: None,
        cache_write_1h: None,
        output: None,
        reasoning: None,
        iterations: None,
        web_searches: None,
        code_execs: None,
        ttl_split_known: None,
        usage_presence: None,
        usage_raw: None,
        cost_usd: None,
        cost_kind: None,
        rate_limits: None,
        req_bytes: None,
        req_messages: None,
        req_tools: None,
        tools_hash: None,
        system_chars: None,
        system_hash: None,
        system_blocks: None,
        system_messages: None,
        compact_generations: None,
        summarising: None,
        system_change: None,
        system_ladder: None,
        system_tail: None,
        gate_on: None,
        cold_on: None,
        forced_from: None,
        forced_to: None,
        downgraded_from: None,
        downgraded_to: None,
        cache_stripped: None,
        system_merged: None,
        model_mappings: None,
        drift_digest: Some(digest.to_owned()),
        status: None,
        error_type: None,
        retry_after_ms: None,
        extra: None,
        betas: None,
        geo: None,
        fast: None,
    }
}

/// The error type from a provider error body: the OpenAI/OpenRouter shape
/// `{"error": {"type": …, "code": …}}` — `type` when present, else `code`
/// (openrouter sends a numeric code, e.g. 401). `None` for anything the
/// observer cannot understand (invariant 6: unparseable loses the detail,
/// not the request).
pub(crate) fn parse_error_type(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let error = value.get("error")?;
    if let Some(kind) = error.get("type").and_then(Value::as_str) {
        return Some(kind.to_owned());
    }
    match error.get("code") {
        Some(Value::String(code)) => Some(code.to_owned()),
        Some(Value::Number(code)) => Some(code.to_string()),
        _ => None,
    }
}

/// `retry-after` in milliseconds. The delta-seconds form only — the
/// HTTP-date form is ignored in phase 1 (no provider sends it on these
/// paths) and reads as absent.
pub(crate) fn retry_after_ms(headers: &axum::http::HeaderMap) -> Option<i64> {
    let raw = headers
        .get(axum::http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    let seconds: f64 = raw.parse().ok()?;
    Some((seconds * 1000.0).round().clamp(0.0, i64::MAX as f64) as i64)
}

#[cfg(test)]
mod tests {
    use super::{TokenBuckets, parse_error_type, retry_after_ms, token_buckets};
    use crate::observe::{UsageCapture, UsageObserver};
    use serde_json::json;

    /// Build a capture from a non-streaming usage object, the way the
    /// server's buffered path produces one.
    fn capture(usage: &serde_json::Value) -> UsageCapture {
        let body =
            json!({"id": "gen-test", "model": "z-ai/glm-5.3", "choices": [], "usage": usage});
        let mut observer = UsageObserver::new();
        observer.observe_json(body.to_string().as_bytes());
        observer.finish().expect("usage-bearing")
    }

    fn buckets(usage: serde_json::Value) -> TokenBuckets {
        token_buckets(&capture(&usage))
    }

    #[test]
    fn full_capture_normalizes_like_join_mjs() {
        let b = buckets(json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 40, "cache_write_tokens": 5},
            "completion_tokens_details": {"reasoning_tokens": 10},
            "cost": 0.001
        }));
        assert_eq!(b.input, Some(55), "100 - 40 cached - 5 written");
        assert_eq!(b.output, Some(40), "50 - 10 reasoning");
        assert_eq!(b.cache_read, Some(40));
        assert_eq!(b.cache_write_total, Some(5));
        assert_eq!(b.reasoning, Some(10));
        assert_eq!(
            b.usage_presence,
            json!({
                "prompt_tokens": true, "completion_tokens": true,
                "cached_tokens": true, "cache_write_tokens": true,
                "reasoning_tokens": true, "cost": true,
            })
        );
    }

    #[test]
    fn over_reporting_subsets_clamp_at_zero_like_join_mjs() {
        // cached > prompt and reasoning > completion: join.mjs's
        // Math.max(0, …) clamps; saturating_sub clamps the same way.
        let b = buckets(json!({
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "prompt_tokens_details": {"cached_tokens": 90},
            "completion_tokens_details": {"reasoning_tokens": 70}
        }));
        assert_eq!(b.input, Some(0), "10 - 90 clamps to 0, never negative");
        assert_eq!(b.output, Some(0), "4 - 70 clamps to 0");
        // The subset metrics themselves stay unclamped, exactly as the
        // ledger's vector keeps the raw cached/reasoning values.
        assert_eq!(b.cache_read, Some(90));
        assert_eq!(b.reasoning, Some(70));
    }

    #[test]
    fn a_missing_subset_contributes_zero_to_a_present_metric() {
        // num() coercion: no cached_tokens → input = prompt in full.
        let b = buckets(json!({"prompt_tokens": 100, "completion_tokens": 4}));
        assert_eq!(b.input, Some(100));
        assert_eq!(b.output, Some(4));
        assert_eq!(b.cache_read, None, "the bucket's own metric is absent");
        assert_eq!(b.cache_write_total, None);
        assert_eq!(b.reasoning, None);
        assert_eq!(
            b.usage_presence,
            json!({
                "prompt_tokens": true, "completion_tokens": true,
                "cached_tokens": false, "cache_write_tokens": false,
                "reasoning_tokens": false, "cost": false,
            })
        );
    }

    #[test]
    fn a_cache_write_subtracts_from_input_and_charges_the_1h_tier() {
        // The three-way join.mjs subtraction, and the conservative
        // apportionment: no TTL tiers on this wire, so the whole write
        // lands on the 1-hour tier, flagged as an apportioned guess.
        let b = buckets(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 4,
            "prompt_tokens_details": {"cached_tokens": 300, "cache_write_tokens": 250},
        }));
        assert_eq!(b.input, Some(450));
        assert_eq!(b.cache_read, Some(300));
        assert_eq!(b.cache_write_total, Some(250));
        // The 1-hour apportionment and ttl_split_known land on the ROW
        // (measurement_row) — pinned by the integration suite.
    }

    #[test]
    fn absent_source_metrics_keep_buckets_absent() {
        // Absence ≠ zero (invariant 3): no prompt_tokens → no input, even
        // with cached present; a present 0 is a real 0.
        let b = buckets(json!({
            "completion_tokens": 4,
            "prompt_tokens_details": {"cached_tokens": 3}
        }));
        assert_eq!(b.input, None);
        assert_eq!(b.cache_read, Some(3));

        let b = buckets(json!({"prompt_tokens": 0, "completion_tokens": 0}));
        assert_eq!(b.input, Some(0), "present 0 is a real 0");
        assert_eq!(b.output, Some(0));
    }

    #[test]
    fn error_types_come_from_the_openai_shape_with_a_code_fallback() {
        assert_eq!(
            parse_error_type(br#"{"error":{"type":"invalid_request_error","code":401}}"#)
                .as_deref(),
            Some("invalid_request_error"),
            "type wins when present"
        );
        assert_eq!(
            parse_error_type(br#"{"error":{"message":"No auth credentials found.","code":401}}"#)
                .as_deref(),
            Some("401"),
            "openrouter's numeric code falls back to its literal"
        );
        assert_eq!(
            parse_error_type(br#"{"error":{"code":"rate_limited"}}"#).as_deref(),
            Some("rate_limited"),
            "string codes read as strings"
        );
        assert_eq!(parse_error_type(br#"{"error":{}}"#), None);
        assert_eq!(parse_error_type(br#"{"message":"plain error"}"#), None);
        assert_eq!(
            parse_error_type(b"not json"),
            None,
            "unparseable loses detail only"
        );
    }

    #[test]
    fn retry_after_reads_delta_seconds_only() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_static("7"),
        );
        assert_eq!(retry_after_ms(&headers), Some(7_000));
        headers.insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_static("0.5"),
        );
        assert_eq!(retry_after_ms(&headers), Some(500));
        headers.insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(
            retry_after_ms(&headers),
            None,
            "HTTP-date form reads as absent"
        );
        assert_eq!(retry_after_ms(&axum::http::HeaderMap::new()), None);
    }
}
