//! Ledger row assembly for the Anthropic Messages proxy path — the
//! sibling of the openai path's [`record`] functions, over the
//! [`AnthropicCapture`] the side-observation produces instead of the
//! OpenAI [`UsageCapture`].
//!
//! Differences that are the protocol's, not accidents:
//!
//! - **Buckets come folded, not normalised.** The anthropic observer
//!   already produces the ledger's buckets (TTL-split
//!   reconciliation, iterations fallback), so the row copies them; the
//!   openai path's subtract-and-clamp arithmetic has no equivalent here.
//! - **Cost is always catalog-priced** ([`crate::catalog::price`] over the
//!   normalised response model, fast mode from `usage.speed`, the
//!   US-geo 1.1× multiplier from `usage.inference_geo`) — with the kind
//!   carrying the semantics per backend (plan: Storage): `estimated` for
//!   anthropic_api (the API bills it), `plan_equivalent` for anthropic_sub
//!   (list-price "what the plan is worth" — never billed). An unknown
//!   model prices to NULL with a one-time warning per model,
//!   never a guess.
//! - **`rate_limits` is this response's own meter snapshot**, parsed from
//!   its `anthropic-ratelimit-*` headers ([`parse_rate_limits`]); the
//!   `meters_state` table gets the same update from every response on a
//!   meter-source backend (the server does that, not this module). Error
//!   rows carry the failed response's own snapshot too, as the
//!   predecessor's did: a 429 carries no usage, and its meters are the
//!   only evidence of throttling the ledger gets (an earlier port left
//!   them off as "lean", which erased exactly that). The *blocked* rows
//!   carry a stale copy instead — [`record_anthropic_blocked`] ports
//!   that, since the stale snapshot is the block's own provenance.
//! - **`model` is the normalised identity, `raw_model` the wire form**
//!   (the normalise/raw pair, as the predecessor named them).
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
use crate::ir::{AnthropicShape, Release};
use crate::middleware::cold::Outlook;
use crate::middleware::lanes;
use crate::middleware::quota::{Grant, Meter, group};
use crate::middleware::system_change::{self, SystemCapture};
use crate::observe::AnthropicCapture;
use crate::providers::Provider;
use crate::store::{CostKind, RequestRow, RowKind};

use super::Server;
use super::record::{elapsed_ms, i64_of, now_ms, with_frontend};

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
    /// The request carried the ping header: its lane is recorded but
    /// excluded from liveness (plan: Ping tagging).
    pub(crate) ping: bool,
    /// The compaction retarget's provenance, when it rewrote this request:
    /// the models moved between, and whether
    /// the transform stripped breakpoints or merged system messages (the
    /// v1 schema records booleans; the counts ride the log line). A
    /// same-model strip carries no `downgraded_from` — that model also
    /// served the request — and only the strip flags say it happened.
    pub(crate) downgraded_from: Option<String>,
    pub(crate) downgraded_to: Option<String>,
    pub(crate) cache_stripped: Option<bool>,
    pub(crate) system_merged: Option<bool>,
    /// The force-newest rewrite's provenance, when it moved this request:
    /// the model asked for and the learned
    /// family newest it was moved onto. Unlike the retarget this
    /// transform changes only the model value — the conversation's
    /// cache_control survives, because the rewrite starts a conversation
    /// that should cache its prefix on the model it will actually use.
    pub(crate) forced_from: Option<String>,
    pub(crate) forced_to: Option<String>,
    /// Batch model-map provenance (the per-request
    /// from→to list), when the request was a batch the map claimed.
    pub(crate) model_mappings: Option<Value>,
    /// The frontend's name from its `/f/<frontend>` prefix, recorded in
    /// `extra` ([`with_frontend`]).
    pub(crate) frontend: Option<String>,
    /// The upstream refused this request's `thinking: disabled` and it was
    /// sent again as `between_tools` (the server's thinking retry), so the
    /// row describes the second attempt. Recorded in `extra` as
    /// `thinkingRewrite`; the refused first attempt has no row of its own.
    pub(crate) thinking_rewritten: bool,
}

/// Record a completed anthropic usage-path response: the measurement row
/// when the capture carries usage (only responses with usage are
/// accounted for — a hung-up stream, an all-keepalive stream, or a
/// usage-less capture on
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
            // Compared before the insert, so the lane's previous row is
            // still the newest one. Infallible: a failure records no
            // change and keeps the ladders.
            let system = ctx.shape.as_ref().map(|shape| {
                system_change::capture(&ctx.server.store, ctx.session_id.as_deref(), shape)
            });
            insert(
                ctx,
                measurement_row(
                    ctx,
                    ts_ms,
                    duration_ms,
                    &route,
                    capture,
                    rate_limits,
                    system.as_ref(),
                ),
            );
            // The lane table and the learned store update alongside the
            // row, on the response identity —
            // never the model asked for, which may have been rewritten.
            note_lane_and_model(ctx, capture, ts_ms);
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

    // The lane table just moved, so the sleep lock is re-evaluated
    // (the evaluate runs right after
    // the lane note — pings included, since a ping's response is
    // what marks its lane). Idempotent: the request's own in-flight hold,
    // if any, is still standing until its body is dropped.
    ctx.server.evaluate_awake();
}

/// Record a non-2xx anthropic usage-path response (plan: Server core): an
/// error row with status, the error pair, retry-after, and the response's
/// own `rate_limits` — never priced, no usage buckets — plus the
/// fidelity-drift row when the request drifted. The meters ride the row
/// because a failure carries no usage but is the only evidence of
/// throttling (the predecessor's rule): a 429 with no meters on its row
/// says only that something said no.
pub(crate) fn record_anthropic_error(
    ctx: &AnthropicRecordCtx,
    status: u16,
    error_type: Option<String>,
    error_message: Option<String>,
    retry_after: Option<i64>,
    rate_limits: Option<&Value>,
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
            (ts_ms, duration_ms),
            &route,
            status,
            (error_type, error_message),
            retry_after,
            rate_limits,
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
///
/// Every row the API answered also marks its model served, on the
/// identity the RESPONSE named (the predecessor's `appendRow` did this
/// for every row). The mark before sending covers a lane deciding while
/// this request is in flight; this one covers what the upstream actually
/// served, which a host-side alias or an upstream substitution can make
/// differ from what was sent — and recency must follow the served
/// identity. In-memory and infallible, so it cannot cost the row.
fn insert(ctx: &AnthropicRecordCtx, mut row: RequestRow) {
    if ctx.thinking_rewritten {
        mark_thinking_rewrite(&mut row);
    }
    let row = with_frontend(row, ctx.frontend.as_deref());
    if let Err(error) = ctx.server.store.record_request(&row) {
        tracing::error!(%error, "ledger insert failed");
    }
    ctx.server
        .models
        .note_served(row.raw_model.as_deref().or(row.model.as_deref()), row.ts_ms);
}

/// Mark a row whose request went out the second time with `thinking`
/// rewritten to `between_tools`, the same way [`with_frontend`] adds its
/// key: into an object `extra`, or as a new one.
fn mark_thinking_rewrite(row: &mut RequestRow) {
    match &mut row.extra {
        Some(Value::Object(extra)) => {
            extra.insert("thinkingRewrite".to_owned(), json!("between_tools"));
        }
        Some(_) => {}
        None => row.extra = Some(json!({ "thinkingRewrite": "between_tools" })),
    }
}

/// Whether the capture carries any usage metric at all. Only responses
/// with usage are accounted for, so an error event alone on a 200 is not a
/// row, and neither is an all-keepalive
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

/// What this response taught the lane table and the learned model store,
/// after the API actually served it.
///
/// Both follow the **response** model — the model that actually served the
/// request — never the one asked for, which may have been rewritten. The
/// prompt total is what a cold resume would have to re-read: the whole
/// prefix, however it was billed this time. The TTL tier comes from where
/// the writes actually landed, sticky across the lane ([`lanes::lane_ttl`]).
///
/// Accounting must never break a session (invariant 6): a store error here
/// is logged and lost — the measurement row is already in.
fn note_lane_and_model(ctx: &AnthropicRecordCtx, capture: &AnthropicCapture, ts_ms: i64) {
    // The held total: fresh input + cache read + cache writes, missing
    // metrics
    // folding in as 0.
    let held = capture.input().unwrap_or(0)
        + capture.cache_read().unwrap_or(0)
        + capture.cache_write_total().unwrap_or(0);

    // A model is "seen" when a response named it. Days
    // are local calendar days; the system zone is the server's, read once
    // per response.
    if let Some(model) = capture.model() {
        let tz = jiff::tz::TimeZone::system();
        if let Err(error) = ctx.server.models.note_seen(model, ts_ms, held, &tz) {
            tracing::error!(%error, "learned model update failed");
        }
    }

    // The lane keyed by session × tools-hash, moved
    // only now that the response completed. `forced` is the upgrade this
    // response made (normalised from/to identities)
    // —
    // the lane keeps it while its cache is warm, and a compaction
    // neither starts nor ends one (merge_lane's rule). Identities
    // normalise; a value with no identity records no upgrade, never a
    // wrong one.
    let forced = ctx
        .forced_from
        .as_deref()
        .zip(ctx.forced_to.as_deref())
        .and_then(|(from, to)| {
            Some(lanes::Forced {
                from: crate::catalog::windows::model_identity(from)?,
                to: crate::catalog::windows::model_identity(to)?,
            })
        });
    let compaction = ctx
        .shape
        .as_ref()
        .is_some_and(AnthropicShape::is_compaction);
    if let Err(error) = lanes::note_lane_response(
        &ctx.server.store,
        lanes::LaneResponse {
            session_id: ctx.session_id.as_deref(),
            tools_hash: ctx.shape.as_ref().map(|shape| shape.tools_hash.as_str()),
            at_ms: ts_ms,
            prompt: i64::try_from(held).unwrap_or(i64::MAX),
            write_5m: capture.cache_write_5m().unwrap_or(0),
            write_1h: capture.cache_write_1h().unwrap_or(0),
            ping: ctx.ping,
            // The anthropic path keeps the tier ladder (the openai
            // path's explicit override is not its to carry).
            ttl_ms: None,
            forced,
            compaction,
        },
    ) {
        tracing::error!(%error, "lane table update failed");
    }
}

/// The cost kind the backend's semantics pick (plan: Storage): the same
/// list-price arithmetic, different meaning — the API bills it, the
/// subscription never does. `None` for openrouter: the catalogue prices
/// Anthropic's own API, not openrouter's providers or the other labs'
/// models it serves there, so an estimate from it would be a guessed
/// price (invariant 5). Its rows carry the `usage.cost` openrouter
/// bills, or no cost when it reports none.
fn cost_kind_of(backend_id: &str) -> Option<CostKind> {
    match backend_id {
        "anthropic_sub" => Some(CostKind::PlanEquivalent),
        "openrouter" => None,
        _ => Some(CostKind::Estimated),
    }
}

/// The catalog-priced cost for one capture:
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
    // The cost fold treats missing buckets as 0: cost is an estimate, and
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

/// The one-time unpriced-model warning: tokens are
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
/// module docs for the anthropic-specific fields. `system` is the
/// capture-time comparison with the lane's previous row, `None` when the
/// request had no shape.
fn measurement_row(
    ctx: &AnthropicRecordCtx,
    ts_ms: i64,
    duration_ms: i64,
    route: &str,
    capture: &AnthropicCapture,
    rate_limits: Option<&Value>,
    system: Option<&SystemCapture>,
) -> RequestRow {
    // The ladders ride only a lane's first row and the rows whose system
    // prompt changed; a row matching its predecessor drops them, as ctp's
    // `loggableShape` did.
    let ladders = shape_ladders(ctx.shape.as_ref(), system);
    let (cost_usd, cost_kind) = match cost_kind_of(ctx.backend.id()) {
        Some(kind) => cost_of(capture, kind),
        // Billed when the provider reported a cost; absent cost is never
        // filled in from a catalogue.
        None => (capture.cost(), capture.cost().map(|_| CostKind::Billed)),
    };
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
        // Only a ping request says so, never a non-ping one.
        ping: ctx.ping.then_some(true),
        // `model` is the normalised identity, `raw_model` the wire
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
        // counts, not the response's usage JSON (imported predecessor rows
        // carry no raw
        // usage either).
        usage_raw: None,
        cost_usd,
        cost_kind,
        rate_limits: rate_limits.cloned(),
        req_bytes: shape.map(|s| i64_of(s.req_bytes)),
        req_messages: shape.and_then(|s| s.req_messages.map(i64_of)),
        req_tools: shape.map(|s| i64_of(s.req_tools)),
        tools_hash: shape.map(|s| s.tools_hash.clone()),
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
        system_change: system.and_then(|system| system.change.clone()),
        system_ladder: ladders.and_then(|s| ladder_json(&s.system_ladder)),
        system_tail: ladders.and_then(|s| ladder_json(&s.system_tail)),
        // The armed state of each gate rides every row, because readers
        // cannot see the service's config (a `?` row would be a gate that
        // may simply have been off).
        gate_on: Some(ctx.server.config.gates.quota_enabled),
        cold_on: Some(ctx.server.config.gates.cold_enabled),
        // The adaptive rewrite's provenance, independent from host
        // mapping. The predecessor recorded `forcedTo` only when the host
        // map also
        // mapped, because before a
        // map the response model implicitly names the adaptive target;
        // toker has no host map, so the value is unambiguous and both
        // halves record (the same call the retarget's `downgraded_to`
        // already makes).
        forced_from: ctx.forced_from.clone(),
        forced_to: ctx.forced_to.clone(),
        downgraded_from: ctx.downgraded_from.clone(),
        downgraded_to: ctx.downgraded_to.clone(),
        // The v1 schema fixed these as booleans, so the row records THAT
        // the transform happened (the counts ride the log
        // line), and only when it did.
        cache_stripped: ctx.cache_stripped,
        system_merged: ctx.system_merged,
        model_mappings: ctx.model_mappings.clone(),
        drift_digest: None,
        status: None,
        error_type: None,
        retry_after_ms: None,
        // Where a summarisation wording sat, when one was near the end:
        // the evidence the detector's position rules are checked against.
        extra: measurement_extra(shape, capture),
        betas: ctx.betas.as_ref().map(|betas| betas.to_string()),
        geo: capture.geo().map(str::to_owned),
        fast: capture.speed().map(|speed| speed == "fast"),
    }
}

/// A measurement row's `extra`: the shape's diagnostics, plus the
/// serving provider when the response names one (openrouter's, for
/// cross-referencing its own logs, as the openai route records it).
fn measurement_extra(shape: Option<&AnthropicShape>, capture: &AnthropicCapture) -> Option<Value> {
    let mut extra = match shape.and_then(shape_extra) {
        Some(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    if let Some(provider) = capture.serving_provider() {
        extra.insert("serving_provider".to_owned(), json!(provider));
    }
    (!extra.is_empty()).then_some(Value::Object(extra))
}

/// The shape's diagnostics for a measurement row's `extra`: where a
/// compaction wording sat, and whether the request was a recap (so what
/// forwarded recaps cost stays measurable). `None` when neither applies.
fn shape_extra(shape: &AnthropicShape) -> Option<Value> {
    let mut extra = serde_json::Map::new();
    if let Some(marker) = &shape.compact_marker {
        extra.insert("compactMarker".to_owned(), marker.to_json());
    }
    if shape.recap {
        extra.insert("recap".to_owned(), Value::Bool(true));
    }
    (!extra.is_empty()).then_some(Value::Object(extra))
}

/// The shape whose ladders the row keeps, or `None` when it drops them.
fn shape_ladders<'a>(
    shape: Option<&'a AnthropicShape>,
    system: Option<&SystemCapture>,
) -> Option<&'a AnthropicShape> {
    shape.filter(|_| system.is_none_or(|system| system.keep_ladders))
}

/// The error row (plan: non-2xx on a usage path): status, the error pair,
/// retry-after, and the response's own meters; never priced, no usage
/// (see the module docs for why the meters stay). The request's shape is not
/// re-measured on the way
/// to a failure the provider already summarised. `timing` is the row's
/// `(ts_ms, duration_ms)`; `error` is the `(type, message)` pair from the
/// response's error object.
fn error_row(
    ctx: &AnthropicRecordCtx,
    timing: (i64, i64),
    route: &str,
    status: u16,
    error: (Option<String>, Option<String>),
    retry_after_ms: Option<i64>,
    rate_limits: Option<&Value>,
) -> RequestRow {
    let (ts_ms, duration_ms) = timing;
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
        rate_limits: rate_limits.cloned(),
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
        gate_on: Some(ctx.server.config.gates.quota_enabled),
        cold_on: Some(ctx.server.config.gates.cold_enabled),
        forced_from: None,
        forced_to: None,
        downgraded_from: None,
        downgraded_to: None,
        cache_stripped: None,
        system_merged: None,
        // There is no response model on a rejected request, so the
        // client → effective chain is what explains where it went; for a
        // batch, that chain is the per-request list.
        model_mappings: ctx.model_mappings.clone(),
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

/// Record the release-marker row (the `kind: "released"` row).
///
/// A release is logged the moment it is granted, because "a release left no
/// trace in the log at all, only on stderr, so nothing afterwards could
/// explain why a session kept spending past a limit that was blocking
/// everything else" — a lesson carried from the predecessor. The grant
/// itself is the caller's (the allowances
/// table, keyed by reset value); this records what is now in force.
///
/// `grant` is the **merged** view — fresh grants for the exhausted meters,
/// the prior live allowance for the rest — matching the predecessor's row,
/// which
/// carries the session's whole allowance entry, nulls included. The row
/// carries `rate_limits`: the stale snapshot the grant was decided on
/// (row parity: the last-seen meters). No duration, no
/// usage, never priced — a proxy-written row, excluded from API
/// measurements by its kind.
pub(crate) fn record_anthropic_released(
    server: &Server,
    session_id: &str,
    backend_id: &str,
    release: Release,
    grant: &Grant,
    stale_meters: Option<&Value>,
    frontend: Option<&str>,
) {
    let row = crate::release::released_row(
        session_id,
        backend_id,
        release,
        grant,
        stale_meters,
        &server.config.gates,
        now_ms(),
    );
    let row = with_frontend(row, frontend);
    if let Err(error) = server.store.record_request(&row) {
        tracing::error!(%error, "ledger insert failed");
    }
    tracing::info!(
        "POST /v1/messages → released {} ({release:?}) five_hour={:?} seven_day={:?}",
        session_id,
        grant.five_hour,
        grant.seven_day,
    );
}

/// The inputs of one blocked row (see [`record_anthropic_blocked`]) —
/// a struct because the pieces are exactly the row's own fields, and
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
    /// The frontend's prefix name, for `extra.frontend`.
    pub(crate) frontend: Option<&'a str>,
}

/// Record the quota-block row (the `kind: "blocked"` row).
///
/// `stale_meters` is the snapshot the block was decided on — the
/// predecessor's blocked
/// rows carry the stale copy (the last-seen meters), and that copy has a
/// known worth: it is the proxy's own
/// last reading, not a header from this request, so counting it reports
/// the proxy's staleness back as the API's.
///
/// No model columns (the predecessor's blocked row carries none), no
/// usage, never
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
        frontend,
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
        // A block proves the quota gate was armed; the cold gate's state
        // is the config at request time, like every other row.
        gate_on: Some(true),
        cold_on: Some(record.server.config.gates.cold_enabled),
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
        // The meter/resets_at/context_tokens row fields, in the
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
    let row = with_frontend(row, frontend);
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

/// The cold-notice row (the `kind: "cold"` row).
///
/// The record of a notice the user was interrupted with: the idle spell
/// measured, the prefix that would be re-read, the message count of the
/// stopped request (the synthetic turn is appended to the client's
/// transcript like any other reply, so the next request in this lane
/// should carry both — recorded so the log can answer whether that held),
/// the compaction model the notice named, and the quota figures the
/// decision rested on.
///
/// **No `rate_limits`** (row parity, and the row shape's rule for
/// proxy-written kinds): nothing reached upstream, so the only meters
/// available would be the proxy's own stale copy, and a row carrying
/// those gets counted as an observation of the API. No usage, never
/// priced.
pub(crate) fn record_anthropic_cold(record: ColdRecord<'_>) {
    write_cold_row(record, RowKind::Cold, "COLD");
}

/// The held-recap row: the `cold` row's payload under its own kind, so a
/// held recap is visible without reading as a notice (the lane reseed
/// restores `noticed_at` from `cold` rows only, and a held recap must not
/// spend the notice).
pub(crate) fn record_anthropic_cold_recap(record: ColdRecord<'_>) {
    write_cold_row(record, RowKind::ColdRecap, "RECAP held");
}

fn write_cold_row(record: ColdRecord<'_>, kind: RowKind, verb: &str) {
    let ColdRecord {
        server,
        started,
        path,
        session_id,
        backend_id,
        tools_hash,
        idle_ms,
        prompt,
        req_messages,
        compact_target,
        outlook,
        // A fired notice means the exemption did not apply; the row's
        // own existence says so.
        writes_free: _,
        gate_on,
        frontend,
    } = record;
    let outlook = outlook.cloned();
    let row = RequestRow {
        id: None,
        ts_ms: now_ms(),
        duration_ms: Some(elapsed_ms(started)),
        kind: Some(kind),
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
        rate_limits: None,
        req_bytes: None,
        req_messages,
        req_tools: None,
        tools_hash: tools_hash.map(str::to_owned),
        system_chars: None,
        system_hash: None,
        system_blocks: None,
        system_messages: None,
        compact_generations: None,
        summarising: None,
        system_change: None,
        system_ladder: None,
        system_tail: None,
        gate_on: Some(gate_on),
        cold_on: Some(true),
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
        // The cold-row fields, in the kind-specific payload column.
        extra: Some(json!({
            "idleMs": idle_ms,
            "lastPrompt": prompt,
            "reqMessages": req_messages,
            "compactTarget": compact_target,
            "quotaExtra": outlook.as_ref().and_then(|o| o.extra),
            "quotaBound": outlook.as_ref().and_then(|o| o.bound),
            "quotaMeter": outlook.as_ref().filter(|o| o.known && !o.on_track).and_then(|o| o.meter.map(Meter::as_str)),
            "util5h": outlook.as_ref().and_then(|o| o.util),
        })),
        betas: None,
        geo: None,
        fast: None,
    };
    let row = with_frontend(row, frontend);
    if let Err(error) = server.store.record_request(&row) {
        tracing::error!(%error, "ledger insert failed");
    }
    tracing::info!(
        "POST {path} → {verb} {} idle {} · {} tokens",
        session_id
            .and_then(|session| session.get(0..8))
            .unwrap_or("?"),
        crate::middleware::cold::human_idle(idle_ms),
        group(prompt),
    );
}

/// The inputs of one cold-notice row (see [`record_anthropic_cold`]).
pub(crate) struct ColdRecord<'a> {
    /// The server (for the store).
    pub(crate) server: &'a Server,
    /// Request start, for `duration_ms`.
    pub(crate) started: Instant,
    /// The frontend path, for the per-request log line.
    pub(crate) path: &'static str,
    /// Session identity, read by header name only (invariant 2).
    pub(crate) session_id: Option<&'a str>,
    /// The backend the request would have reached.
    pub(crate) backend_id: &'a str,
    /// The lane's tools-hash — the cold gate is a per-lane decision.
    pub(crate) tools_hash: Option<&'a str>,
    /// How long the lane sat idle before this request.
    pub(crate) idle_ms: i64,
    /// The prefix the next request would re-read.
    pub(crate) prompt: u64,
    /// The message count of the stopped request.
    pub(crate) req_messages: Option<i64>,
    /// The model the notice promised a cheap `/compact` on, when one
    /// resolved.
    pub(crate) compact_target: Option<&'a str>,
    /// The quota outlook the decision rested on, when one was measured.
    pub(crate) outlook: Option<&'a Outlook>,
    /// Whether the notice was withheld because the backend's fetched
    /// catalogue says the model's cache writes are free — the
    /// writes-free exemption. For an outlook-withheld row this is
    /// `false`: the row says which reason held it back.
    pub(crate) writes_free: bool,
    /// Whether the quota gate is armed (a view
    /// reading this row cannot infer the toggle from anywhere else).
    pub(crate) gate_on: bool,
    /// The frontend's prefix name, for `extra.frontend`.
    pub(crate) frontend: Option<&'a str>,
}

/// The withheld-notice row (the `kind: "cold-quiet"` row).
///
/// A suppressed notice is a re-read the user never hears about, so it is
/// recorded — otherwise the notice count simply falls and no view can tell
/// a quiet fortnight from a gate that stopped working. Absence of
/// instrumentation must never read as absence of the thing. `at` and
/// `noticed_at` are untouched (nothing was said, nothing reached
/// upstream); the lane stays cold, so a later request in the same idle
/// spell is judged again against meters that may have tightened.
///
/// The `util5h` here is the MEASURED figure the decision rested on, from
/// the burn — never the proxy's last-seen copy of the meters, which would
/// report the proxy's own staleness as the API's. **No `rate_limits`**,
/// for the same reason. The withholding reason rides `extra`:
/// `writesFree: true` for the cache-writes-free exemption (no quota
/// figures were measured — the exemption answered first), the quota
/// figures for an outlook withholding.
pub(crate) fn record_anthropic_cold_quiet(record: ColdRecord<'_>) {
    let ColdRecord {
        server,
        started,
        path,
        session_id,
        backend_id,
        tools_hash,
        idle_ms,
        prompt,
        outlook,
        writes_free,
        gate_on,
        frontend,
        ..
    } = record;
    let outlook = outlook.cloned();
    let row = RequestRow {
        id: None,
        ts_ms: now_ms(),
        duration_ms: Some(elapsed_ms(started)),
        kind: Some(RowKind::ColdQuiet),
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
        rate_limits: None,
        req_bytes: None,
        req_messages: None,
        req_tools: None,
        tools_hash: tools_hash.map(str::to_owned),
        system_chars: None,
        system_hash: None,
        system_blocks: None,
        system_messages: None,
        compact_generations: None,
        summarising: None,
        system_change: None,
        system_ladder: None,
        system_tail: None,
        gate_on: Some(gate_on),
        cold_on: Some(true),
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
            "idleMs": idle_ms,
            "lastPrompt": prompt,
            // The withholding reason: writes free at the backend (the
            // exemption, measured nothing), or the quota figures the
            // outlook rested on.
            "writesFree": writes_free,
            "quotaExtra": outlook.as_ref().and_then(|o| o.extra),
            "quotaBound": outlook.as_ref().and_then(|o| o.bound),
            "util5h": outlook.as_ref().and_then(|o| o.util),
        })),
        betas: None,
        geo: None,
        fast: None,
    };
    let row = with_frontend(row, frontend);
    if let Err(error) = server.store.record_request(&row) {
        tracing::error!(%error, "ledger insert failed");
    }
    tracing::info!(
        "POST {path} → cold-quiet {} idle {} · {} tokens",
        session_id
            .and_then(|session| session.get(0..8))
            .unwrap_or("?"),
        crate::middleware::cold::human_idle(idle_ms),
        group(prompt),
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

// ── the codex translation branch's rows ──────────────────────────────────

/// Record a completed codex turn (kind `None` = a real API measurement).
///
/// Buckets follow the parity rule already used on the openai
/// path: fresh input = input_tokens − cached − cache-write, clamped at
/// zero (never a negative fabricated). The codex usage object has no
/// TTL tiers — one `cache_write_tokens` figure — so the conservative
/// split applies: the whole write charges to the 1-hour tier with
/// `ttl_split_known = false`, never a silent guess at a cheaper split.
///
/// The usage object rides `usage_raw` verbatim (the codex `Usage`
/// serialises every member the wire carried, unknown ones included —
/// the same ledger-parity rule openrouter's cost enjoys). Cost is NULL
/// with no kind: there is no honest per-token price for codex slugs —
/// the subscription insight lives in the meter snapshot, not a
/// fabricated number.
pub(crate) fn record_codex_measurement(
    ctx: &AnthropicRecordCtx,
    capture: &crate::providers::codex::TurnCapture,
    meters: Option<Value>,
    _status: u16,
    frontend_protocol: &'static str,
) {
    let ts_ms = now_ms();
    let duration_ms = elapsed_ms(ctx.started);
    let route = format!("{frontend_protocol}:{}", ctx.backend.id());
    if let Some(digest) = &ctx.drift {
        insert(ctx, drift_row(ts_ms, &route, digest));
    }
    let usage = capture.usage();
    let shape = ctx.shape.as_ref();

    // The three-way subtraction, clamped (never negative).
    let cached = usage
        .and_then(|usage| usage.input_tokens_details.as_ref())
        .and_then(|details| details.cached_tokens);
    let written = usage
        .and_then(|usage| usage.input_tokens_details.as_ref())
        .and_then(|details| details.cache_write_tokens);
    let reasoning = usage
        .and_then(|usage| usage.output_tokens_details.as_ref())
        .and_then(|details| details.reasoning_tokens);
    let (input, cache_read, cache_write_total) = match usage {
        Some(usage) => {
            let fresh = usage
                .input_tokens
                .saturating_sub(cached.unwrap_or(0))
                .saturating_sub(written.unwrap_or(0));
            (Some(i64_of(fresh)), cached.map(i64_of), written.map(i64_of))
        }
        None => (None, None, None),
    };

    // The presence map: which metrics the usage object actually carried
    // (invariant 3 — reported zero ≠ absent).
    let mut presence = serde_json::Map::new();
    if usage.is_some() {
        presence.insert("input".to_owned(), json!(true));
        presence.insert("output".to_owned(), json!(true));
        presence.insert("cache_read".to_owned(), json!(cached.is_some()));
        presence.insert("cache_write_total".to_owned(), json!(written.is_some()));
        presence.insert("cache_write_1h".to_owned(), json!(written.is_some()));
        presence.insert("reasoning".to_owned(), json!(reasoning.is_some()));
    }

    let held_input = input;
    let row = RequestRow {
        id: None,
        ts_ms,
        duration_ms: Some(duration_ms),
        kind: None,
        frontend: Some(frontend_protocol.to_owned()),
        provider: Some(ctx.backend.id().to_owned()),
        route: Some(route),
        session_id: ctx.session_id.clone(),
        ping: ctx.ping.then_some(true),
        // The response's own slug is authoritative (the routing unit's
        // `model` capture); the effective model — map and all — is the
        // fallback when the response did not name itself.
        model: capture
            .model()
            .map(str::to_owned)
            .or_else(|| ctx.effective_model.clone()),
        raw_model: capture.model().map(str::to_owned),
        requested_model: ctx.requested_model.clone(),
        effective_model: ctx.effective_model.clone(),
        input,
        cache_read,
        cache_write_total,
        cache_write_5m: None,
        cache_write_1h: cache_write_total,
        output: usage.map(|usage| i64_of(usage.output_tokens)),
        reasoning: reasoning.map(i64_of),
        iterations: None,
        web_searches: None,
        code_execs: None,
        ttl_split_known: cache_write_total.is_some().then_some(false),
        usage_presence: (!presence.is_empty()).then(|| Value::Object(presence)),
        usage_raw: usage.and_then(|usage| serde_json::to_string(usage).ok()),
        // No honest price for a codex slug — never guessed (invariant 3).
        cost_usd: None,
        cost_kind: None,
        // This response's own meter snapshot, parsed from its headers.
        rate_limits: meters,
        req_bytes: shape.map(|s| i64_of(s.req_bytes)),
        req_messages: shape.and_then(|s| s.req_messages.map(i64_of)),
        req_tools: shape.map(|s| i64_of(s.req_tools)),
        tools_hash: shape.map(|s| s.tools_hash.clone()),
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
        system_change: None,
        system_ladder: shape.and_then(|s| ladder_json(&s.system_ladder)),
        system_tail: shape.and_then(|s| ladder_json(&s.system_tail)),
        gate_on: Some(ctx.server.config.gates.quota_enabled),
        cold_on: Some(ctx.server.config.gates.cold_enabled),
        forced_from: ctx.forced_from.clone(),
        forced_to: ctx.forced_to.clone(),
        downgraded_from: ctx.downgraded_from.clone(),
        downgraded_to: ctx.downgraded_to.clone(),
        cache_stripped: ctx.cache_stripped,
        system_merged: ctx.system_merged,
        model_mappings: ctx.model_mappings.clone(),
        drift_digest: None,
        status: None,
        error_type: None,
        retry_after_ms: None,
        extra: None,
        betas: ctx.betas.clone().map(|betas| betas.to_string()),
        geo: None,
        fast: None,
    };
    insert(ctx, row);

    // The same lane-and-model note the anthropic measurement path makes
    // (a translated request is still an anthropic-frontend turn): the
    // learned store grows, the lane clock moves, and the sleep lock
    // re-evaluates — a codex-served conversation keeps a live lane
    // warm exactly like a native one.
    let usage = capture.usage();
    let cached = usage
        .and_then(|usage| usage.input_tokens_details.as_ref())
        .and_then(|details| details.cached_tokens);
    let written = usage
        .and_then(|usage| usage.input_tokens_details.as_ref())
        .and_then(|details| details.cache_write_tokens);
    let held = held_input.unwrap_or(0)
        + i64::try_from(cached.unwrap_or(0)).unwrap_or(i64::MAX)
        + i64::try_from(written.unwrap_or(0)).unwrap_or(i64::MAX);
    let model = capture
        .model()
        .map(str::to_owned)
        .or_else(|| ctx.effective_model.clone());
    if let Some(model) = model.as_deref() {
        let tz = jiff::tz::TimeZone::system();
        if let Err(error) =
            ctx.server
                .models
                .note_seen(model, ts_ms, u64::try_from(held).unwrap_or(u64::MAX), &tz)
        {
            tracing::error!(%error, "learned model update failed");
        }
    }
    let compaction = ctx
        .shape
        .as_ref()
        .is_some_and(AnthropicShape::is_compaction);
    if let Err(error) = lanes::note_lane_response(
        &ctx.server.store,
        lanes::LaneResponse {
            session_id: ctx.session_id.as_deref(),
            tools_hash: ctx.shape.as_ref().map(|shape| shape.tools_hash.as_str()),
            at_ms: ts_ms,
            prompt: held,
            write_5m: 0,
            write_1h: written.unwrap_or(0),
            ping: ctx.ping,
            ttl_ms: None,
            forced: None,
            compaction,
        },
    ) {
        tracing::error!(%error, "lane update failed");
    }
    ctx.server.evaluate_awake();

    let model = capture.model().unwrap_or("?");
    tracing::info!(
        "POST {} → {} ledgered=yes model={} provider={} ({:.1}s)",
        ctx.path,
        _status,
        model,
        ctx.backend.id(),
        ctx.started.elapsed().as_secs_f64(),
    );
}

/// Record a failed codex turn: lean like every error row — status (the
/// real HTTP status, or 200 for an in-band `response.failed` mid-stream),
/// the mapped anthropic error type, the message in `extra`. `resets_at`
/// is an ABSOLUTE epoch the upstream reports; it rides `extra` as-is —
/// the `retry_after_ms` column means a duration and would misread it.
pub(crate) fn record_codex_error(
    ctx: &AnthropicRecordCtx,
    status: u16,
    error_type: &str,
    message: &str,
    resets_at: Option<i64>,
    frontend_protocol: &'static str,
) {
    let ts_ms = now_ms();
    let route = format!("{frontend_protocol}:{}", ctx.backend.id());
    if let Some(digest) = &ctx.drift {
        insert(ctx, drift_row(ts_ms, &route, digest));
    }
    let mut extra = serde_json::Map::new();
    extra.insert("error_message".to_owned(), json!(message));
    if let Some(resets_at) = resets_at {
        extra.insert("resets_at".to_owned(), json!(resets_at));
    }
    let row = RequestRow {
        id: None,
        ts_ms,
        duration_ms: Some(elapsed_ms(ctx.started)),
        kind: Some(RowKind::Error),
        frontend: Some(frontend_protocol.to_owned()),
        provider: Some(ctx.backend.id().to_owned()),
        route: Some(route),
        session_id: ctx.session_id.clone(),
        ping: ctx.ping.then_some(true),
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
        gate_on: Some(ctx.server.config.gates.quota_enabled),
        cold_on: Some(ctx.server.config.gates.cold_enabled),
        forced_from: None,
        forced_to: None,
        downgraded_from: None,
        downgraded_to: None,
        cache_stripped: None,
        system_merged: None,
        model_mappings: None,
        drift_digest: None,
        status: Some(status as i64),
        error_type: Some(error_type.to_owned()),
        retry_after_ms: None,
        extra: Some(Value::Object(extra)),
        betas: None,
        geo: None,
        fast: None,
    };
    insert(ctx, row);
    tracing::info!(
        "POST {} → {} ledgered=error type={} provider={} ({:.1}s)",
        ctx.path,
        status,
        error_type,
        ctx.backend.id(),
        ctx.started.elapsed().as_secs_f64(),
    );
}

#[cfg(test)]
mod tests {
    use super::super::record::i64_of;
    use super::{cost_kind_of, ladder_json};
    use crate::store::CostKind;

    #[test]
    fn cost_kinds_follow_the_backend_semantics() {
        assert_eq!(
            cost_kind_of("anthropic_sub"),
            Some(CostKind::PlanEquivalent)
        );
        assert_eq!(cost_kind_of("anthropic_api"), Some(CostKind::Estimated));
        assert_eq!(
            cost_kind_of("openrouter"),
            None,
            "the anthropic catalogue never prices an openrouter row"
        );
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
