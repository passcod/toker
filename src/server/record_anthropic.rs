//! Ledger row assembly for the Anthropic Messages proxy path — the
//! sibling of the openai path's [`record`] functions, over the
//! [`AnthropicCapture`] the side-observation produces instead of the
//! OpenAI [`UsageCapture`].
//!
//! Differences that are the protocol's, not accidents:
//!
//! - **Buckets come folded, not normalised.** The anthropic observer
//!   already produces the ledger's buckets (ctp `foldUsage`: TTL-split
//!   reconciliation, iterations fallback), so the row copies them; the
//!   openai path's join.mjs arithmetic has no equivalent here.
//! - **Cost is always catalog-priced** ([`crate::catalog::price`] over the
//!   normalised response model, fast mode from `usage.speed`, the
//!   US-geo 1.1× multiplier from `usage.inference_geo`) — with the kind
//!   carrying the semantics per backend (plan: Storage): `estimated` for
//!   anthropic_api (the API bills it), `plan_equivalent` for anthropic_sub
//!   (list-price "what the plan is worth" — never billed). An unknown
//!   model prices to NULL with a one-time warning per model (ctp's rule),
//!   never a guess.
//! - **`rate_limits` is this response's own meter snapshot**, parsed from
//!   its `anthropic-ratelimit-*` headers ([`parse_rate_limits`]); the
//!   `meters_state` table gets the same update from every response on a
//!   meter-source backend (the server does that, not this module). Error
//!   rows carry none — lean, like the openai error rows; that is a
//!   deliberate divergence from ctp, which embeds the response's meters
//!   on its error rows (ctp's *blocked* rows do carry a stale copy —
//!   [`record_anthropic_blocked`] ports that, since the stale snapshot is
//!   the block's own provenance; an error row describes a response, which
//!   has fresh headers of its own).
//! - **`model` is the normalised identity, `raw_model` the wire form**
//!   (ctp: `normaliseModel` / `rawModel`).
//! - **`betas`** is the request's `anthropic-beta` header split into
//!   flags, stored as a JSON array; `None` when the header is absent —
//!   absent ≠ empty.
//!
//! The clock feeds row fields only — never bytes (invariant 4) — and
//! store errors lose the row, never the request (invariant 6).
//!
//! [`record`]: super::record
//! [`UsageCapture`]: crate::observe::UsageCapture

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde_json::{Value, json};

use crate::catalog::{CostBuckets, normalise_model_id, price};
use crate::ir::AnthropicShape;
use crate::middleware::quota::{Grant, Meter};
use crate::observe::AnthropicCapture;
use crate::providers::Provider;
use crate::store::{CostKind, RequestRow, RowKind};

use super::Server;
use super::record::{elapsed_ms, i64_of, now_ms};

/// Everything a completion point knows about one anthropic request,
/// minus the response itself.
pub(crate) struct AnthropicRecordCtx {
    /// The server (for the store).
    pub(crate) server: Server,
    /// Request start, for `duration_ms` — the only clock use (invariant 4).
    pub(crate) started: Instant,
    /// The frontend path, for the per-request log line.
    pub(crate) path: &'static str,
    /// Session identity, read by header name only (invariant 2).
    pub(crate) session_id: Option<String>,
    /// The model the frontend asked for, `provider/model` routing included.
    pub(crate) requested_model: Option<String>,
    /// The model that served, after the routing rewrite.
    pub(crate) effective_model: Option<String>,
    /// The fidelity monitor's divergence digest, when re-serialisation
    /// drifted from the original bytes (invariant 5).
    pub(crate) drift: Option<String>,
    /// The selected backend — provider column, route, cost semantics.
    pub(crate) backend: Arc<dyn Provider>,
    /// The request's `anthropic-beta` flags as a JSON array; `None` when
    /// the header was absent.
    pub(crate) betas: Option<Value>,
    /// The content-free request shape ([`AnthropicShape`]).
    pub(crate) shape: Option<AnthropicShape>,
}

/// Record a completed anthropic usage-path response: the measurement row
/// when the capture carries usage (ctp accounts only responses with usage
/// — a hung-up stream, an all-keepalive stream, or a usage-less capture on
/// a 200 records no row), plus the fidelity-drift row when the request
/// drifted. `rate_limits` is this response's own meter snapshot, parsed by
/// the server from its headers. `status` is the upstream status the client
/// saw.
pub(crate) fn record_anthropic_measurement(
    ctx: &AnthropicRecordCtx,
    capture: Option<&AnthropicCapture>,
    rate_limits: Option<&Value>,
    status: u16,
) {
    let ts_ms = now_ms();
    let duration_ms = elapsed_ms(ctx.started);
    let route = route_of(ctx);

    if let Some(digest) = &ctx.drift {
        insert(ctx, drift_row(ts_ms, &route, digest));
    }

    let ledgered = match capture {
        Some(capture) if usage_bearing(capture) => {
            insert(
                ctx,
                measurement_row(ctx, ts_ms, duration_ms, &route, capture, rate_limits),
            );
            true
        }
        _ => false,
    };

    let model = capture
        .and_then(AnthropicCapture::model)
        .or(ctx.effective_model.as_deref());
    tracing::info!(
        "POST {} → {} ledgered={} model={} provider={}{} ({:.1}s)",
        ctx.path,
        status,
        if ledgered { "yes" } else { "no" },
        model.unwrap_or("?"),
        ctx.backend.id(),
        drift_note(ctx),
        ctx.started.elapsed().as_secs_f64(),
    );
}

/// Record a non-2xx anthropic usage-path response (plan: Server core): an
/// error row with status, the error pair, and retry-after — never priced,
/// no usage buckets, **no `rate_limits`** (lean, like the openai error
/// rows; the `meters_state` table still took the response's snapshot, the
/// server did that before this ran) — plus the fidelity-drift row when the
/// request drifted.
pub(crate) fn record_anthropic_error(
    ctx: &AnthropicRecordCtx,
    status: u16,
    error_type: Option<String>,
    error_message: Option<String>,
    retry_after: Option<i64>,
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
            (error_type, error_message),
            retry_after,
        ),
    );

    tracing::info!(
        "POST {} → {} ledgered=error model={} provider={}{} ({:.1}s)",
        ctx.path,
        status,
        ctx.effective_model.as_deref().unwrap_or("?"),
        ctx.backend.id(),
        drift_note(ctx),
        ctx.started.elapsed().as_secs_f64(),
    );
}

/// The route column, `frontend:backend`.
fn route_of(ctx: &AnthropicRecordCtx) -> String {
    format!("anthropic:{}", ctx.backend.id())
}

fn drift_note(ctx: &AnthropicRecordCtx) -> String {
    match &ctx.drift {
        Some(digest) => format!(" drift={digest}"),
        None => String::new(),
    }
}

/// Insert one row; a store failure loses the row, never the request
/// (invariant 6) — it is logged as the visible breakage it is.
fn insert(ctx: &AnthropicRecordCtx, row: RequestRow) {
    if let Err(error) = ctx.server.store.record_request(&row) {
        tracing::error!(%error, "ledger insert failed");
    }
}

/// Whether the capture carries any usage metric at all. ctp accounts only
/// responses with usage (`if (!startUsage && !deltaUsage) return`), so an
/// error event alone on a 200 is not a row, and neither is an all-keepalive
/// stream (which finishes to no capture at all).
fn usage_bearing(capture: &AnthropicCapture) -> bool {
    let presence = capture.presence();
    presence.input
        || presence.cache_read
        || presence.cache_write_total
        || presence.cache_write_5m
        || presence.cache_write_1h
        || presence.output
        || presence.reasoning
        || presence.web_searches
        || presence.code_execs
}

/// The cost kind the backend's semantics pick (plan: Storage): the same
/// list-price arithmetic, different meaning — the API bills it, the
/// subscription never does.
fn cost_kind_of(backend_id: &str) -> CostKind {
    if backend_id == "anthropic_sub" {
        CostKind::PlanEquivalent
    } else {
        CostKind::Estimated
    }
}

/// The catalog-priced cost for one capture (ctp `ratesFor` + `costOf`):
/// list prices over the folded buckets — missing metrics contribute 0,
/// cost being an estimate — with fast mode and the US-geo multiplier from
/// the response, and the kind carrying the backend's semantics. An
/// unknown model prices to `(None, None)` after a one-time warning.
fn cost_of(capture: &AnthropicCapture, kind: CostKind) -> (Option<f64>, Option<CostKind>) {
    let Some(model) = capture.model() else {
        return (None, None);
    };
    let fast = capture.speed() == Some("fast");
    let geo = capture.geo();
    let Some(priced) = price(model, fast, geo) else {
        warn_unpriced_once(model);
        return (None, None);
    };
    // ctp `costOf` folds missing buckets in as 0: cost is an estimate, and
    // the presence map (stored beside the buckets) carries the verdict on
    // each number.
    let buckets = CostBuckets {
        input: capture.input().unwrap_or(0),
        cache_read: capture.cache_read().unwrap_or(0),
        cache_write_5m: capture.cache_write_5m().unwrap_or(0),
        cache_write_1h: capture.cache_write_1h().unwrap_or(0),
        output: capture.output().unwrap_or(0),
        web_searches: capture.web_searches().unwrap_or(0),
    };
    (Some(priced.cost_usd(&buckets)), Some(kind))
}

/// ctp's one-time unpriced-model warning (`unknownModels`): tokens are
/// counted, the cost is left null, and the complaint is logged once per
/// model per process — never per request.
fn warn_unpriced_once(model: &str) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut warned) = warned.lock()
        && warned.insert(model.to_owned())
    {
        tracing::warn!("no price entry for {model:?} — tokens counted, cost left null");
    }
}

/// A ladder column: JSON array of digests. `None` when there are no rungs
/// — an empty ladder localises nothing, and absence stays absence
/// (invariant 3).
fn ladder_json(rungs: &[String]) -> Option<String> {
    (!rungs.is_empty()).then(|| json!(rungs).to_string())
}

/// The measurement row (kind `None` = a real API measurement). See the
/// module docs for the anthropic-specific fields.
fn measurement_row(
    ctx: &AnthropicRecordCtx,
    ts_ms: i64,
    duration_ms: i64,
    route: &str,
    capture: &AnthropicCapture,
    rate_limits: Option<&Value>,
) -> RequestRow {
    let (cost_usd, cost_kind) = cost_of(capture, cost_kind_of(ctx.backend.id()));
    let shape = ctx.shape.as_ref();
    RequestRow {
        id: None,
        ts_ms,
        duration_ms: Some(duration_ms),
        kind: None,
        frontend: Some("anthropic".to_owned()),
        provider: Some(ctx.backend.id().to_owned()),
        route: Some(route.to_owned()),
        session_id: ctx.session_id.clone(),
        ping: None,
        // ctp: `model` is the normalised identity, `raw_model` the wire
        // form the provider actually served.
        model: capture.model().and_then(normalise_model_id),
        raw_model: capture.model().map(str::to_owned),
        requested_model: ctx.requested_model.clone(),
        effective_model: ctx.effective_model.clone(),
        input: capture.input().map(i64_of),
        cache_read: capture.cache_read().map(i64_of),
        cache_write_total: capture.cache_write_total().map(i64_of),
        cache_write_5m: capture.cache_write_5m().map(i64_of),
        cache_write_1h: capture.cache_write_1h().map(i64_of),
        output: capture.output().map(i64_of),
        reasoning: capture.reasoning().map(i64_of),
        iterations: Some(i64_of(capture.iterations())),
        web_searches: capture.web_searches().map(i64_of),
        code_execs: capture.code_execs().map(i64_of),
        ttl_split_known: capture.ttl_split_known(),
        usage_presence: Some(capture.presence().to_json()),
        // The anthropic fold is computed, not echoed: the capture keeps
        // counts, not the response's usage JSON (ctp rows carry no raw
        // usage either).
        usage_raw: None,
        cost_usd,
        cost_kind,
        rate_limits: rate_limits.cloned(),
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
        system_messages: shape.and_then(|s| s.system_messages.map(i64_of)),
        compact_generations: shape.and_then(|s| s.compact_generations.map(i64_of)),
        summarising: shape.map(|s| s.summarising),
        // Lane-localised system-change detection is the lanes unit's
        // middleware, not this unit's.
        system_change: None,
        system_ladder: shape.and_then(|s| ladder_json(&s.system_ladder)),
        system_tail: shape.and_then(|s| ladder_json(&s.system_tail)),
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
        extra: None,
        betas: ctx.betas.as_ref().map(|betas| betas.to_string()),
        geo: capture.geo().map(str::to_owned),
        fast: capture.speed().map(|speed| speed == "fast"),
    }
}

/// The error row (plan: non-2xx on a usage path): status, the error pair,
/// and retry-after; never priced, no usage, no `rate_limits` (lean, like
/// the openai error rows — see the module docs for the deliberate
/// divergence from ctp). The request's shape is not re-measured on the way
/// to a failure the provider already summarised. `error` is the
/// `(type, message)` pair from the response's error object.
fn error_row(
    ctx: &AnthropicRecordCtx,
    ts_ms: i64,
    duration_ms: i64,
    route: &str,
    status: u16,
    error: (Option<String>, Option<String>),
    retry_after_ms: Option<i64>,
) -> RequestRow {
    let (error_type, error_message) = error;
    RequestRow {
        id: None,
        ts_ms,
        duration_ms: Some(duration_ms),
        kind: Some(RowKind::Error),
        frontend: Some("anthropic".to_owned()),
        provider: Some(ctx.backend.id().to_owned()),
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
        // The schema has no error_message column; the pair's message half
        // rides in `extra` (the kind-specific payload column).
        extra: error_message.map(|message| json!({ "error_message": message })),
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
        frontend: Some("anthropic".to_owned()),
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

/// Record the release-marker row (ctp: `kind: "released"`, proxy.mjs:1154).
///
/// ctp logs a release the moment it is granted, because "a release left no
/// trace in the log at all, only on stderr, so nothing afterwards could
/// explain why a session kept spending past a limit that was blocking
/// everything else". The grant itself is the caller's (the allowances
/// table, keyed by reset value); this records what is now in force.
///
/// `grant` is the **merged** view — fresh grants for the exhausted meters,
/// the prior live allowance for the rest — matching ctp's row, which
/// carries the session's whole allowance entry, nulls included. The row
/// carries `rate_limits`: the stale snapshot the grant was decided on (ctp
/// parity: `rateLimits: lastMeters`). No duration (ctp omits it), no
/// usage, never priced — a proxy-written row, excluded from API
/// measurements by its kind.
pub(crate) fn record_anthropic_released(
    server: &Server,
    session_id: &str,
    backend_id: &str,
    grant: &Grant,
    stale_meters: Option<&Value>,
) {
    let row = RequestRow {
        id: None,
        ts_ms: now_ms(),
        duration_ms: None,
        kind: Some(RowKind::Released),
        frontend: Some("anthropic".to_owned()),
        provider: Some(backend_id.to_owned()),
        route: Some(format!("anthropic:{backend_id}")),
        session_id: Some(session_id.to_owned()),
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
        // ctp parity: the stale snapshot the grant rested on. A blocked
        // request can never refresh meters, so this is often the same
        // spent reading the NEXT blocked row will carry — that is the
        // point of recording it (ctp limit.mjs:150-154's note: measure the
        // reset lag from response rows only, never these).
        rate_limits: stale_meters.cloned(),
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
        gate_on: Some(true),
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
        // ctp's `fiveHour`/`sevenDay` row fields, in the kind-specific
        // payload column (the schema has no dedicated columns).
        extra: Some(json!({
            "fiveHour": grant.five_hour,
            "sevenDay": grant.seven_day,
        })),
        betas: None,
        geo: None,
        fast: None,
    };
    if let Err(error) = server.store.record_request(&row) {
        tracing::error!(%error, "ledger insert failed");
    }
    tracing::info!(
        "POST /v1/messages → released {} five_hour={:?} seven_day={:?}",
        session_id,
        grant.five_hour,
        grant.seven_day,
    );
}

/// The inputs of one blocked row (see [`record_anthropic_blocked`]) —
/// a struct because the pieces are exactly the ctp row's own fields, and
/// a nine-argument call site would be positional-number soup.
pub(crate) struct BlockedRecord<'a> {
    /// The server (for the store).
    pub(crate) server: &'a Server,
    /// Request start, for `duration_ms`.
    pub(crate) started: Instant,
    /// The frontend path, for the per-request log line.
    pub(crate) path: &'static str,
    /// Session identity, read by header name only (invariant 2).
    pub(crate) session_id: Option<&'a str>,
    /// The backend the request would have reached (the gate's own).
    pub(crate) backend_id: &'a str,
    /// Which meter hit its limit.
    pub(crate) meter: Meter,
    /// When that meter's window resets, epoch seconds.
    pub(crate) resets_at: Option<i64>,
    /// The session's largest-lane prompt, when the lane table knows it —
    /// a count or `None`, never content (invariant 1).
    pub(crate) context_tokens: Option<u64>,
    /// The snapshot the block was decided on — the stale copy.
    pub(crate) stale_meters: Option<&'a Value>,
}

/// Record the quota-block row (ctp: `kind: "blocked"`, proxy.mjs:1220-1233).
///
/// `stale_meters` is the snapshot the block was decided on — ctp's blocked
/// rows carry the stale copy (`rateLimits: lastMeters`), and ctp
/// limit.mjs:150-154 warns what that copy is worth: it is the proxy's own
/// last reading, not a header from this request, so counting it reports
/// the proxy's staleness back as the API's.
///
/// No model columns (ctp's blocked row carries none), no usage, never
/// priced; a proxy-written row, excluded from API measurements by its kind.
pub(crate) fn record_anthropic_blocked(record: BlockedRecord<'_>) {
    let BlockedRecord {
        server,
        started,
        path,
        session_id,
        backend_id,
        meter,
        resets_at,
        context_tokens,
        stale_meters,
    } = record;
    let row = RequestRow {
        id: None,
        ts_ms: now_ms(),
        duration_ms: Some(elapsed_ms(started)),
        kind: Some(RowKind::Blocked),
        frontend: Some("anthropic".to_owned()),
        provider: Some(backend_id.to_owned()),
        route: Some(format!("anthropic:{backend_id}")),
        session_id: session_id.map(str::to_owned),
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
        rate_limits: stale_meters.cloned(),
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
        gate_on: Some(true),
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
        // ctp's `meter`/`resetsAt`/`contextTokens` row fields, in the
        // kind-specific payload column.
        extra: Some(json!({
            "meter": meter.as_str(),
            "resets_at": resets_at,
            "context_tokens": context_tokens,
        })),
        betas: None,
        geo: None,
        fast: None,
    };
    if let Err(error) = server.store.record_request(&row) {
        tracing::error!(%error, "ledger insert failed");
    }
    tracing::info!(
        "POST {path} → BLOCKED {} {}",
        meter.as_str(),
        session_id
            .and_then(|session| session.get(0..8))
            .unwrap_or("?"),
    );
}

/// Parse the error pair from a buffered non-2xx body via the observer
/// (its JSON path latches `error.type`/`error.message`); a body it cannot
/// read loses the detail, never the request (invariant 6).
pub(crate) fn error_pair(body: &[u8]) -> (Option<String>, Option<String>) {
    let mut observer = crate::observe::AnthropicObserver::new();
    observer.observe_json(body);
    match observer.finish() {
        Some(capture) => (
            capture.error_type().map(str::to_owned),
            capture.error_message().map(str::to_owned),
        ),
        None => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::super::record::i64_of;
    use super::{cost_kind_of, ladder_json};
    use crate::store::CostKind;

    #[test]
    fn cost_kinds_follow_the_backend_semantics() {
        assert_eq!(cost_kind_of("anthropic_sub"), CostKind::PlanEquivalent);
        assert_eq!(cost_kind_of("anthropic_api"), CostKind::Estimated);
    }

    #[test]
    fn ladders_store_as_json_arrays_and_empty_ones_stay_absent() {
        assert_eq!(
            ladder_json(&["abc".to_owned(), "def".to_owned()]),
            Some(r#"["abc","def"]"#.to_owned())
        );
        assert_eq!(ladder_json(&[]), None, "no rungs = nothing localised");
        assert_eq!(i64_of(u64::MAX), i64::MAX, "saturated, never wrapped");
    }
}
