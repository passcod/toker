//! The Anthropic Messages frontend (plan: frontend adapters).
//!
//! Routes, all served by the anthropic backends ([`crate::providers`]):
//!
//! - `POST /v1/messages` — **the usage path** (ctp: `req.url.split("?")[0]
//!   === "/v1/messages"` — exactly, query stripped; axum matches on the
//!   path alone, so the route IS the gate's path check, and
//!   `count_tokens`/`batches` can never land in it). Fully recorded, and
//!   **the quota gate's only target**: on the anthropic_sub backend (the
//!   sole meter source), a spent meter is answered 200 with a synthetic
//!   assistant turn instead of forwarding — never an error status (ctp
//!   measured it on 2026-09-10: 529 retries silently, 429 mislabels, 403
//!   looks like broken credentials). The release marker is read from the
//!   ORIGINAL body, then stripped unconditionally (the frozen marker rule
//!   runs on this path for every backend and regardless of the gate's
//!   toggle — a toggled strip would change the cached prefix of every
//!   conversation carrying a marker).
//! - `POST /v1/messages/count_tokens`, `POST /v1/messages/batches` — the
//!   same pipeline end to end (buffer → IR parse → fidelity check →
//!   routing → forward → tee → record), but they are **never gated**
//!   (blocking them protects no quota, only breaks the client). Their
//!   responses carry no usage, so they record nothing in
//!   practice — their error and drift rows are real, ctp logs those too.
//! - The batch-result GETs and cancel — transparent forwarding, like the
//!   openai path's `/v1/models`: no recording, no observation.
//!
//! The pipeline mirrors the openai chat path ([`super::proxy`]) step for
//! step, with the anthropic observer ([`AnthropicObserver`]) riding the
//! stream instead of the OpenAI one, and two ctp rules the openai path has
//! no analogue for:
//!
//! 1. **The meters feed from every response** (ctp: "Feed the gate from
//!    every response, not just accounted ones: a 429 or a background call
//!    still reports the meters, and the gate must not go stale"). Only a
//!    meter-source backend feeds it — anthropic sub today; the plain
//!    API's RPM headers are not quota meters and must never overwrite
//!    the gate's snapshot.
//! 2. **Session headers pass through** — ctp forwards claude's
//!    `x-claude-code-session-id` to the upstream verbatim (it strips
//!    hop-by-hop only), and this path must behave the same. Only toker's
//!    own `x-toker-*` headers are proxy-addressed and stripped.
//!
//! A client hangup aborts the upstream and records no row, exactly like
//! the openai path; a hung-up stream is half a measurement, not a row.

use std::convert::Infallible;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures::future::{AbortHandle, Abortable};
use futures::stream::Stream;

use crate::ir::{Fidelity, Request as IrRequest, compare};
use crate::middleware::quota::{
    Blocking, GateDecision, Grant, Meter, Meters, Rendering, decide, grant_for,
};
use crate::observe::{AnthropicObserver, SseSplitter};
use crate::providers::{Provider, parse_rate_limits};
use crate::store::{Allowance, MetersSnapshot};

use super::Server;
use super::proxy::{
    MAX_ERROR_BODY, MAX_REQUEST_BODY, MAX_RESPONSE_BUFFER, UpstreamBody, buffer_up_to,
    buffered_body, build_response, is_compressed, is_event_stream, plain_status, response_headers,
    send_upstream, session_id,
};
use super::record::{now_ms, retry_after_ms};
use super::record_anthropic::{
    AnthropicRecordCtx, BlockedRecord, error_pair, record_anthropic_blocked,
    record_anthropic_error, record_anthropic_measurement, record_anthropic_released,
};

/// `POST /v1/messages` — the anthropic usage path, and the quota gate's
/// only target.
pub(crate) async fn messages(State(server): State<Server>, request: Request) -> Response {
    usage_path(server, request, "/v1/messages").await
}

/// `POST /v1/messages/count_tokens` — same pipeline, never a gate target.
pub(crate) async fn count_tokens(State(server): State<Server>, request: Request) -> Response {
    usage_path(server, request, "/v1/messages/count_tokens").await
}

/// `POST /v1/messages/batches` — same pipeline, never a gate target.
pub(crate) async fn batches_create(State(server): State<Server>, request: Request) -> Response {
    usage_path(server, request, "/v1/messages/batches").await
}

/// `GET /v1/messages/batches` — transparent forwarding.
pub(crate) async fn batches_list(State(server): State<Server>, request: Request) -> Response {
    transparent(server, request).await
}

/// `GET /v1/messages/batches/{id}` — transparent forwarding.
pub(crate) async fn batches_get(State(server): State<Server>, request: Request) -> Response {
    transparent(server, request).await
}

/// `GET /v1/messages/batches/{id}/results` — transparent forwarding.
pub(crate) async fn batches_results(State(server): State<Server>, request: Request) -> Response {
    transparent(server, request).await
}

/// `POST /v1/messages/batches/{id}/cancel` — transparent forwarding: batch
/// management, not a usage path.
pub(crate) async fn batches_cancel(State(server): State<Server>, request: Request) -> Response {
    transparent(server, request).await
}

/// The shared usage-path pipeline (see the module docs). `path` is the
/// route's own literal, for the per-request log line and the gate's
/// exact-path rule: only `"/v1/messages"` gates.
async fn usage_path(server: Server, request: Request, path: &'static str) -> Response {
    let started = Instant::now();
    let (parts, body) = request.into_parts();

    // Session identity and betas, read by name only — request headers are
    // never captured wholesale: they carry credentials (invariant 2).
    let session_id = session_id(&server.config.session_header_names, &parts.headers);
    let betas = request_betas(&parts.headers);

    // 1. Buffer the request body fully.
    let original = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "request body exceeded toker's cap");
            return plain_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds toker's 64 MiB cap\n",
            );
        }
    };

    // 2.-5. Parse, fidelity-check, route.
    let mut forward = original.clone();
    let mut record = None;
    let mut backend = server.default_anthropic().clone();
    // ctp `clientWants` (proxy.mjs:1047-1054): the client's own model and
    // whether it explicitly asked for a plain JSON Message — both read
    // BEFORE any transform, because the blocked answer renders the model
    // the client named and in the shape it asked for.
    let mut client_model: Option<String> = None;
    let mut stream_explicitly_false = false;
    // The gate's meter snapshot, loaded at most once per request and only
    // when the gate is armed (every other backend must be a no-op without
    // even reading meters).
    let mut meters_snapshot: Option<serde_json::Value> = None;
    if let Ok(mut ir) = IrRequest::parse(&original) {
        // 3. Invariant 5, verified per request: Exact is the normal case;
        // Drift forwards the original buffer either way, and lands a
        // visible fidelity-drift row at completion.
        let mut drift = None;
        if let Fidelity::Drift { digest, .. } = compare(&original, &ir.serialise()) {
            drift = Some(digest);
        }
        client_model = ir.anthropic().model().map(str::to_owned);
        stream_explicitly_false = ir.anthropic().stream_explicitly_false();
        // 4. Routing: read the model through the typed view; a provider
        // prefix overrides the backend per request.
        let model = client_model.clone();
        let mut effective_model = model.clone();
        let mut transformed = false;
        if let Some((provider, rest)) = model
            .as_deref()
            .and_then(|model| strip_anthropic_prefix(&server, model))
        {
            // 5. A deliberate transform: forward the serialised IR (pure
            // and deterministic, so the upstream prefix stays stable),
            // recorded as requested vs effective — nothing is "forced".
            backend = provider.clone();
            effective_model = Some(rest.to_owned());
            ir.anthropic_mut().set_model(rest);
            transformed = true;
        }

        // ── the quota gate + release marker, ctp proxy.mjs:1135-1241 ──
        //
        // Sequence (every step's order is measured, not stylistic):
        // release check on the ORIGINAL body → grant/record → the
        // unconditional strip → the gate decision. A release is read
        // before the strip because the strip removes the very marker the
        // release is made of.
        let gate_armed = path == "/v1/messages"
            && server.config.gates.quota_enabled
            && backend.id() == "anthropic_sub";
        if gate_armed {
            meters_snapshot = server
                .store
                .load_meters()
                .map(|snapshot| snapshot.map(|snapshot| snapshot.snapshot))
                .unwrap_or_else(|error| {
                    tracing::error!(%error, "meter snapshot load failed");
                    None
                });
        }
        if path == "/v1/messages" {
            // A release: grant/refresh an allowance for the
            // currently-exhausted meters only, and record it. ctp gates
            // this on the marker + the session id + the toggle
            // (proxy.mjs:1136) — a sessionless request cannot hold an
            // allowance.
            if gate_armed
                && let Some(session) = session_id.as_deref()
                && ir.anthropic().carries_release()
            {
                let meters = meters_snapshot.as_ref().map(Meters::over);
                let now = now_ms();
                let fresh = grant_for(meters, now);
                for (meter, reset) in [
                    (Meter::FiveHour, fresh.five_hour),
                    (Meter::SevenDay, fresh.seven_day),
                ] {
                    if let Some(reset) = reset
                        && let Err(error) = server.store.record_allowance(&Allowance {
                            session_id: session.to_owned(),
                            meter: meter.as_str().to_owned(),
                            reset_value: reset,
                        })
                    {
                        tracing::error!(%error, "allowance record failed");
                    }
                }
                // ctp merges, never replaces (proxy.mjs:1137-1147): a fresh
                // null for a meter defers to the allowance already held,
                // so a release while only the 5-hour window is spent must
                // not wipe an existing 7-day allowance.
                let merged = Grant {
                    five_hour: fresh
                        .five_hour
                        .or_else(|| prior_live(&server, session, "5h", now)),
                    seven_day: fresh
                        .seven_day
                        .or_else(|| prior_live(&server, session, "7d", now)),
                };
                record_anthropic_released(
                    &server,
                    session,
                    backend.id(),
                    &merged,
                    meters_snapshot.as_ref(),
                );
            }

            // The strip: UNCONDITIONAL on this path — it runs for every
            // backend and regardless of the gate's toggle, because the
            // marker rule is a frozen public API and a toggled strip would
            // change the cached prefix of every conversation carrying a
            // marker (invariant 4; ctp proxy.mjs:1166-1177). Record
            // nothing for the strip itself: the released row is the
            // user-visible event, and the strip is the API's own rule.
            let pre_strip = ir.serialise();
            ir.anthropic_mut().strip_release();
            if ir.serialise() != pre_strip {
                transformed = true;
            }
        }
        if transformed {
            // A deliberate transform: forward the serialised IR. When the
            // only transform was a strip on a drifted (non-canonical)
            // body, this surfaces as the drift row already recorded above
            // — the marker still must not reach the model.
            forward = Bytes::from(ir.serialise());
        }
        let shape = ir.anthropic().shape();
        record = Some(AnthropicRecordCtx {
            server: server.clone(),
            started,
            path,
            // Cloned, not moved: the gate decision below still reads the
            // session (allowances lookup, blocked row).
            session_id: session_id.clone(),
            requested_model: model,
            effective_model,
            drift,
            backend: backend.clone(),
            betas,
            shape: Some(shape),
        });
    }

    // ── the gate decision (after the strip, ctp proxy.mjs:1195) ──
    //
    // Runs on the exact `/v1/messages` path, only for the anthropic_sub
    // backend (the sole meter source), and only when the gate is enabled —
    // for every other backend this whole block is a no-op that never even
    // reads meters. It runs for unparseable bodies too (ctp decides on
    // `gated` alone): a client that cannot parse an event stream still
    // gets the SSE turn, since `streamFalse` could not be read.
    let gate_armed = path == "/v1/messages"
        && server.config.gates.quota_enabled
        && backend.id() == "anthropic_sub";
    if gate_armed {
        if meters_snapshot.is_none() {
            // The unparseable-body case: the release/strip section above
            // never ran, so the snapshot was never loaded.
            meters_snapshot = server
                .store
                .load_meters()
                .map(|snapshot| snapshot.map(|snapshot| snapshot.snapshot))
                .unwrap_or_else(|error| {
                    tracing::error!(%error, "meter snapshot load failed");
                    None
                });
        }
        let allowances = allowances_for_session(&server, session_id.as_deref());
        let decision = decide(
            meters_snapshot.as_ref().map(Meters::over),
            &allowances,
            now_ms(),
        );
        if let GateDecision::Block { meter, resets_at } = decision {
            // Answer 200 with a synthetic assistant turn, never an error
            // status — measured against a real client (see the module
            // docs). The context size is the session's largest lane's,
            // which the lane table does not track yet: `None`, so the
            // notice drops the clause rather than guessing (absence ≠
            // zero, invariant 3).
            let text = Blocking::notice(
                meter,
                resets_at,
                None,
                &jiff::tz::TimeZone::system(),
                server.config.gates.notice_style,
            );
            let rendering = if stream_explicitly_false {
                Rendering::Json
            } else {
                Rendering::Sse
            };
            let body = Blocking::blocked_turn(&text, client_model.as_deref(), rendering);
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                match rendering {
                    Rendering::Sse => header::HeaderValue::from_static("text/event-stream"),
                    Rendering::Json => header::HeaderValue::from_static("application/json"),
                },
            );
            record_anthropic_blocked(BlockedRecord {
                server: &server,
                started,
                path,
                session_id: session_id.as_deref(),
                backend_id: backend.id(),
                meter,
                resets_at,
                // The lane table does not track prompts yet: `None`, so
                // the notice drops the clause rather than guessing
                // (absence ≠ zero, invariant 3).
                context_tokens: None,
                stale_meters: meters_snapshot.as_ref(),
            });
            return build_response(StatusCode::OK, headers, Body::from(body));
        }
    }

    // 6. Upstream; 7.-9. in forward_response. Session headers pass
    // through (ctp parity — see the module docs), so the strip list is
    // empty; `x-toker-*` is stripped unconditionally either way.
    match send_upstream(&server, backend.as_ref(), &parts, forward, &[]).await {
        Ok(upstream) => forward_response(server, backend, upstream, record).await,
        Err(error) => {
            // No upstream response: nothing measured, and the error row is
            // provider-response-shaped (status/type/retry-after), so this
            // surfaces as 502 unledgered and logged — never a fabricated
            // provider status.
            tracing::warn!(%error, "upstream request failed");
            plain_status(StatusCode::BAD_GATEWAY, "upstream request failed\n")
        }
    }
}

/// The session's stored allowances, or none for a sessionless request (ctp
/// proxy.mjs:1196 passes `allowances.get(sessionId)`, which is undefined
/// without a session — decide then sees no allowances). A store error
/// loses the allowances, never the request (invariant 6): the gate
/// treats it as "nothing held", the conservative reading.
fn allowances_for_session(server: &Server, session_id: Option<&str>) -> Vec<Allowance> {
    let Some(session) = session_id else {
        return Vec::new();
    };
    server
        .store
        .load_allowances()
        .map(|allowances| {
            allowances
                .into_iter()
                .filter(|allowance| allowance.session_id == session)
                .collect()
        })
        .unwrap_or_else(|error| {
            tracing::error!(%error, "allowances load failed");
            Vec::new()
        })
}

/// The live prior allowance a session holds for one meter (ctp's merge
/// rule, proxy.mjs:1141: `fresh ?? prior`). ctp stores one value per
/// meter per session; the store's reset-value keying can hold several
/// across rolled windows, and the one still in force is the live
/// (future-reset) row with the greatest reset — a rolled window's rows
/// are inert by the value match and never answer here for long.
fn prior_live(server: &Server, session: &str, meter: &str, now_ms: i64) -> Option<i64> {
    server
        .store
        .load_allowances()
        .ok()?
        .into_iter()
        .filter(|allowance| allowance.session_id == session && allowance.meter == meter)
        .map(|allowance| allowance.reset_value)
        .filter(|reset| reset.saturating_mul(1000) > now_ms)
        .max()
}

/// Transparent forwarding (the batch-result paths): routed to the default
/// anthropic backend, auth rules applied, bytes both ways untouched — no
/// recording, no observation, like the openai `/v1/models` path. The
/// meters still feed: a background batch poll is exactly the call ctp's
/// "not just accounted ones" rule names.
async fn transparent(server: Server, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "request body exceeded toker's cap");
            return plain_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds toker's 64 MiB cap\n",
            );
        }
    };
    let backend = server.default_anthropic().clone();
    match send_upstream(&server, backend.as_ref(), &parts, body, &[]).await {
        Ok(upstream) => forward_response(server, backend, upstream, None).await,
        Err(error) => {
            tracing::warn!(%error, "upstream request failed");
            plain_status(StatusCode::BAD_GATEWAY, "upstream request failed\n")
        }
    }
}

/// Anthropic routing (plan: Routing). A provider's own name selects that
/// backend; the generic `anthropic/` family prefix selects the protocol
/// default (it names the protocol, not a provider — plan: "`provider/model`
/// names override per request … `anthropic/claude-opus-5`"); both are
/// stripped from the model. Anything else — bare names, other protocols'
/// prefixes — goes to the configured default, untransformed: routing the
/// anthropic frontend to an openai backend is cross-protocol translation,
/// a later phase's work, not a model-string edit.
fn strip_anthropic_prefix<'a>(
    server: &'a Server,
    model: &'a str,
) -> Option<(&'a Arc<dyn Provider>, &'a str)> {
    if let Some(rest) = model.strip_prefix("anthropic_sub/") {
        Some((&server.anthropic_sub, rest))
    } else if let Some(rest) = model.strip_prefix("anthropic_api/") {
        Some((&server.anthropic_api, rest))
    } else {
        model
            .strip_prefix("anthropic/")
            .map(|rest| (server.default_anthropic(), rest))
    }
}

/// The `anthropic-beta` request header, split into its flags (ctp
/// `requestBetas`): a comma-separated list of feature flags and nothing
/// else, read by name only (invariant 2). Worth recording because flags
/// change what a request costs and how it is bounded. `None` when the
/// header is absent — absent ≠ empty; a present-but-empty header is a
/// real empty list. Invariant 1: the flags are fixed feature names
/// (`fast-mode-…`, `context-1m-…`), not content.
fn request_betas(headers: &HeaderMap) -> Option<serde_json::Value> {
    let raw = headers.get_all("anthropic-beta");
    let mut values = raw.iter();
    values.next()?;
    let flags: Vec<serde_json::Value> = raw
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|flag| !flag.is_empty())
        .map(|flag| serde_json::Value::String(flag.to_owned()))
        .collect();
    Some(serde_json::Value::Array(flags))
}

/// Forward one upstream response to the client, branching on compression /
/// status / content-type — the anthropic mirror of the openai
/// `forward_upstream`, plus the meter feed. `record` is the usage-path
/// completion context; `None` means transparent forwarding.
async fn forward_response(
    server: Server,
    backend: Arc<dyn Provider>,
    upstream: reqwest::Response,
    record: Option<AnthropicRecordCtx>,
) -> Response {
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();

    // ctp rule: feed the meters from EVERY response, not just accounted
    // ones — a 429, a count_tokens, a background batch poll still report
    // the meters, and the gate must not go stale. Only a meter-source
    // backend has meters to report (the sub; the API's RPM headers are
    // not quota meters and must not overwrite the gate's snapshot).
    if let Some(meters) = backend.meters(&upstream_headers) {
        let snapshot = MetersSnapshot {
            updated_ms: now_ms(),
            snapshot: meters,
        };
        if let Err(error) = server.store.save_meters(&snapshot) {
            tracing::error!(%error, "meter snapshot save failed");
        }
    }
    // The row's own copy — this response's headers, parsed. Error rows
    // take none of this (lean); the meters_state table took the update
    // above regardless.
    let rate_limits = parse_rate_limits(&upstream_headers);

    // Unexpected compression — shouldn't happen, identity is forced —
    // passes through untouched with no recording (ledger-proxy behavior:
    // never mis-parse a compressed stream).
    if is_compressed(&upstream_headers) {
        tracing::debug!("compressed upstream response passed through unledgered");
        let body = Body::from_stream(upstream.bytes_stream());
        return build_response(status, response_headers(&upstream_headers, true), body);
    }

    let Some(ctx) = record else {
        // Transparent forwarding (batch GETs, non-JSON bodies): stream
        // through; nothing observed, nothing recorded.
        let body = Body::from_stream(upstream.bytes_stream());
        return build_response(status, response_headers(&upstream_headers, false), body);
    };

    // Non-2xx on a usage path: error row (status, error pair, retry-after
    // — never priced), body forwarded unchanged.
    if !status.is_success() {
        let buffered = buffer_up_to(upstream, MAX_ERROR_BODY).await;
        let (error_type, error_message) = error_pair(&buffered.bytes);
        let retry_after = retry_after_ms(&upstream_headers);
        record_anthropic_error(
            &ctx,
            status.as_u16(),
            error_type,
            error_message,
            retry_after,
        );
        let body = buffered_body(buffered);
        return build_response(status, response_headers(&upstream_headers, false), body);
    }

    // SSE: chunks stream through with backpressure, each also feeding the
    // side observation.
    if is_event_stream(&upstream_headers) {
        let stream = AnthropicObservedStream::new(upstream, status.as_u16(), ctx, rate_limits);
        let body = Body::from_stream(stream);
        return build_response(status, response_headers(&upstream_headers, false), body);
    }

    // Non-SSE: buffer, observe, forward the original bytes unchanged.
    let buffered = buffer_up_to(upstream, MAX_RESPONSE_BUFFER).await;
    if buffered.rest.is_some() {
        tracing::warn!("non-streaming response exceeded the buffer cap; passed through unledgered");
        let body = buffered_body(buffered);
        return build_response(status, response_headers(&upstream_headers, false), body);
    }
    let mut observer = AnthropicObserver::new();
    observer.observe_json(&buffered.bytes);
    let capture = observer.finish();
    record_anthropic_measurement(
        &ctx,
        capture.as_ref(),
        rate_limits.as_ref(),
        status.as_u16(),
    );
    let body = Body::from(Bytes::from(buffered.bytes));
    build_response(status, response_headers(&upstream_headers, false), body)
}

/// The SSE response stream with the anthropic observation riding alongside:
/// bytes pass through verbatim, each chunk *also* feeds the
/// splitter/observer. Observation is a side effect that can never fail the
/// stream (invariant 6): the observe APIs are infallible by construction,
/// and the calls additionally run under [`std::panic::catch_unwind`] so no
/// observation bug can take a live session down — the measurement is lost,
/// not the response. A dropped body (client hangup) aborts the upstream
/// and records nothing.
struct AnthropicObservedStream {
    /// The upstream body, wrapped [`Abortable`] so the handle below can
    /// stop it.
    inner: Pin<Box<Abortable<UpstreamBody>>>,
    /// Fires in [`Drop`]: when axum drops the response body — client
    /// hangup, shutdown — the upstream request is aborted too.
    abort: AbortHandle,
    splitter: SseSplitter,
    observer: AnthropicObserver,
    /// The recording context, taken at completion: only a completed
    /// stream records (a hung-up one records nothing, plan: Server core).
    ctx: Option<AnthropicRecordCtx>,
    /// This response's own meter snapshot, for the measurement row.
    rate_limits: Option<serde_json::Value>,
    status: u16,
}

impl AnthropicObservedStream {
    fn new(
        response: reqwest::Response,
        status: u16,
        ctx: AnthropicRecordCtx,
        rate_limits: Option<serde_json::Value>,
    ) -> AnthropicObservedStream {
        let (abort, registration) = AbortHandle::new_pair();
        let stream: UpstreamBody = Box::pin(response.bytes_stream());
        AnthropicObservedStream {
            inner: Box::pin(Abortable::new(stream, registration)),
            abort,
            splitter: SseSplitter::new(),
            observer: AnthropicObserver::new(),
            ctx: Some(ctx),
            rate_limits,
            status,
        }
    }
}

impl Stream for AnthropicObservedStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                observe_chunk(&mut this.splitter, &mut this.observer, &chunk);
                std::task::Poll::Ready(Some(Ok(chunk)))
            }
            std::task::Poll::Ready(Some(Err(error))) => {
                // Upstream transport died mid-stream: the response is
                // truncated wherever the client lost it. No completion, no
                // row — drop the context so a later poll cannot record one.
                tracing::warn!(%error, "upstream response stream failed");
                this.ctx.take();
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Ready(None) => {
                // Natural completion: flush the splitter's tail, finish the
                // observation, record. Recording failures log, never
                // propagate (invariant 6).
                if let Some(ctx) = this.ctx.take() {
                    let splitter = &mut this.splitter;
                    let observer = &mut this.observer;
                    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        if let Some(event) = splitter.finish() {
                            observer.observe_event(&event);
                        }
                    }));
                    let capture = std::mem::take(observer).finish();
                    let rate_limits = this.rate_limits.take();
                    record_anthropic_measurement(
                        &ctx,
                        capture.as_ref(),
                        rate_limits.as_ref(),
                        this.status,
                    );
                }
                std::task::Poll::Ready(None)
            }
        }
    }
}

impl Drop for AnthropicObservedStream {
    fn drop(&mut self) {
        // Client hangup → axum drops the body → abort the upstream. Also
        // fires after natural completion, where it is a no-op.
        self.abort.abort();
    }
}

/// Feed one chunk to the side observation, panic-guarded (invariant 6:
/// accounting must never break a session — a lost measurement is the worst
/// outcome, never a lost response).
fn observe_chunk(splitter: &mut SseSplitter, observer: &mut AnthropicObserver, chunk: &[u8]) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        for event in splitter.feed(chunk) {
            observer.observe_event(&event);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::super::proxy::strip_provider_prefix;
    use super::request_betas;
    use axum::http::{HeaderMap, HeaderValue};
    use serde_json::json;

    #[test]
    fn betas_split_trim_and_keep_absence_distinct_from_empty() {
        let mut headers = HeaderMap::new();
        assert_eq!(request_betas(&headers), None, "absent stays absent");

        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("context-1m-2025-08-07, fast-mode-2025-09-preview "),
        );
        assert_eq!(
            request_betas(&headers),
            Some(json!([
                "context-1m-2025-08-07",
                "fast-mode-2025-09-preview"
            ])),
            "comma-split, trimmed, empties dropped"
        );

        // A present-but-empty header is a real empty list, not absence.
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-beta", HeaderValue::from_static(""));
        assert_eq!(request_betas(&headers), Some(json!([])));
    }

    #[test]
    fn the_openai_prefix_stripping_is_untouched_by_the_anthropic_unit() {
        // The openai path's router keeps its phase-1 shape: `anthropic/…`
        // is NOT an openai path route.
        assert_eq!(strip_provider_prefix("anthropic/claude-opus-5"), None);
        assert_eq!(
            strip_provider_prefix("openrouter/z-ai/glm-5.3"),
            Some("z-ai/glm-5.3")
        );
    }
}
