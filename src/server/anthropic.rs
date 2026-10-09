//! The Anthropic Messages frontend (plan: frontend adapters).
//!
//! Routes, all served by the anthropic backends ([`crate::providers`]):
//!
//! - `POST /v1/messages` — **the usage path** (exactly:
//!   the path alone matches, query stripped; axum matches on the
//!   path alone, so the route IS the gate's path check, and
//!   `count_tokens`/`batches` can never land in it). Fully recorded, and
//!   **the quota gate's only target**: on the anthropic_sub backend (the
//!   sole meter source), a spent meter is answered 200 with a synthetic
//!   assistant turn instead of forwarding — never an error status
//!   (measured with the predecessor proxy on 2026-09-10: 529 retries
//!   silently, 429 mislabels, 403
//!   looks like broken credentials). The release marker is read from the
//!   ORIGINAL body, then stripped unconditionally (the frozen marker rule
//!   runs on this path for every backend and regardless of the gate's
//!   toggle — a toggled strip would change the cached prefix of every
//!   conversation carrying a marker). After middleware finishes, the final
//!   body parses into canonical IR and the selected Messages binding renders
//!   it. Provider bytes feed observation before canonical response rendering.
//! - `POST /v1/messages/count_tokens`, `POST /v1/messages/batches` — the
//!   same pipeline end to end (buffer → IR parse → fidelity check →
//!   routing → forward → tee → record), but they are **never gated**
//!   (blocking them protects no quota, only breaks the client). Their
//!   responses carry no usage, so they record nothing in
//!   practice — their error and drift rows are real, as the predecessor's
//!   were.
//! - The batch-result GETs and cancel — transparent forwarding, like the
//!   openai path's `/v1/models`: no recording, no observation.
//! - Every path the route table does not match — the router's fallback,
//!   transparent forwarding the same way ([`unmatched`]), except the
//!   `/_toker/` namespace, which never leaves the proxy.
//!
//! The pipeline mirrors the openai chat path ([`super::proxy`]) step for
//! step, with the anthropic observer ([`AnthropicObserver`]) riding the
//! stream instead of the OpenAI one, and two rules the openai path has
//! no analogue for:
//!
//! 1. **The meters feed from every response** ("Feed the gate from
//!    every response, not just accounted ones: a 429 or a background call
//!    still reports the meters, and the gate must not go stale" — the
//!    predecessor's rule). Only a
//!    meter-source backend feeds it — anthropic sub today; the plain
//!    API's RPM headers are not quota meters and must never overwrite
//!    the gate's snapshot.
//! 2. **Session headers pass through** — the predecessor forwards
//!    claude's
//!    `x-claude-code-session-id` to the upstream verbatim (it strips
//!    hop-by-hop only), and this path must behave the same. Only toker's
//!    own `x-toker-*` headers are proxy-addressed and stripped.
//!
//! A client hangup aborts the upstream and records no row, exactly like
//! the openai path; a hung-up stream is half a measurement, not a row.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use futures::future::{AbortHandle, Abortable};
use futures::stream::Stream;

use crate::config::GatesConfig;
use crate::ir::AnthropicShape;
use crate::ir::{Fidelity, Request as IrRequest, compare};
use crate::middleware::cold;
use crate::middleware::force_newest::{self, ForceDecision};
use crate::middleware::lanes;
use crate::middleware::quota::{
    Blocking, GateDecision, Meters, Rendering, decide, grant_for, plan_room,
};
use crate::observe::{AnthropicObserver, SseSplitter};
use crate::providers::{Provider, parse_rate_limits};
use crate::routing::ProtocolId;
use crate::store::{Allowance, MetersSnapshot};
use crate::translate;

use super::InFlightGuard;
use super::Server;
use super::codex;
use super::proxy::{
    ErrorWire, MAX_ERROR_BODY, MAX_REQUEST_BODY, MAX_RESPONSE_BUFFER, UpstreamBody, buffer_up_to,
    buffered_body, build_response, is_compressed, is_event_stream, plain_status, response_headers,
    send_upstream, session_id, transport_failure, truncated_body,
};
use super::record::{now_ms, retry_after_ms};
use super::record_anthropic::{
    AnthropicRecordCtx, BlockedRecord, ColdRecord, error_pair, record_anthropic_blocked,
    record_anthropic_cold, record_anthropic_cold_quiet, record_anthropic_cold_recap,
    record_anthropic_error, record_anthropic_measurement, record_anthropic_released,
};
use crate::middleware::model_map;

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

/// Any request the route table does not match: forwarded transparently
/// to the default anthropic backend, as the predecessor forwarded every
/// path but its control path. Claude calls more of the API than the
/// routes above name, and a 404 from the proxy for a path the upstream
/// serves breaks the client for nothing. Like the batch GETs, nothing is
/// recorded or observed (the body is never parsed); the meters still
/// feed.
///
/// Two answers stay local. The `/_toker/` namespace is the proxy's own,
/// so a path under it that matches no control route is a 404 here,
/// never a request to the provider. And the codex backend translates
/// Messages turns into the Responses dialect rather than serving
/// anthropic paths, so an unknown path routed to it has nothing upstream
/// to reach: it answers anthropic's own 404 shape instead of forwarding
/// a request the backend could only reject, with codex's bearer
/// attached.
pub(crate) async fn unmatched(State(server): State<Server>, request: Request) -> Response {
    let path = request.uri().path();
    if path == "/_toker" || path.starts_with("/_toker/") {
        return plain_status(StatusCode::NOT_FOUND, "no such toker control endpoint\n");
    }
    let Some(default) = server.default_anthropic() else {
        return super::anthropic_not_configured();
    };
    if default.id() == "codex_sub" {
        return codex::anthropic_error_response(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "this backend serves only the Messages API",
            false,
        );
    }
    transparent(server, request).await
}

/// The shared usage-path pipeline (see the module docs). `path` is the
/// route's own literal, for the per-request log line and the gate's
/// exact-path rule: only `"/v1/messages"` gates.
async fn usage_path(server: Server, request: Request, path: &'static str) -> Response {
    let started = Instant::now();
    let Some(default_backend) = server.default_anthropic().cloned() else {
        return super::anthropic_not_configured();
    };
    let (parts, body) = request.into_parts();

    // Session identity and betas, read by name only — request headers are
    // never captured wholesale: they carry credentials (invariant 2).
    let session_id = session_id(&server.config.session_header_names, &parts.headers);
    let betas = request_betas(&parts.headers);
    // Ping tagging (plan: Middleware): a lane whose request carried the
    // ping header is recorded but excluded from liveness — the window
    // pinger's probe must never hold the sleep lock.
    let ping = lanes::is_ping(&parts.headers, &server.config.ping_header_name);
    // The frontend's name from its base-URL prefix: picks the notices'
    // style, nothing else.
    let frontend = super::frontend_of(&parts.extensions).map(str::to_owned);

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

    // A gated non-ping request is in flight
    // (count_tokens and batches are
    // never `gated` — the exact-path match — so they do not count): a
    // request being served holds the machine awake, pings aside. The
    // guard's Drop is the decrement, so no early return — a blocked
    // answer, a cold notice, a 502 — can leak the count; for a streamed
    // response it rides the body stream.
    let in_flight = (path == "/v1/messages" && !ping).then(|| server.begin_in_flight());

    // 2.-5. Parse, fidelity-check, route.
    let mut forward = original.clone();
    let mut record = None;
    // The parsed IR outlives the block below: the compaction retarget —
    // the only middleware transform that changes model-visible prompt
    // structure — runs after the gates and rewrites it (the body is
    // re-serialised for the same reason).
    let mut parsed: Option<IrRequest> = None;
    // The request's own shape, kept past the record context (which takes
    // a clone): the cold gate and the retarget read it, and the row's
    // shape fields stay the PRE-transform shape's, in the same order.
    let mut gate_shape: Option<AnthropicShape> = None;
    let mut backend = default_backend;
    // The client's own wants: the client's own model and
    // whether it explicitly asked for a plain JSON Message — both read
    // BEFORE any transform, because the blocked answer renders the model
    // the client named and in the shape it asked for.
    let mut client_model: Option<String> = None;
    let mut stream_explicitly_false = false;
    // The model about to be sent upstream, for the served-model mark.
    let mut served_model: Option<String> = None;
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
            // A prefix naming a backend whose block is absent: answered
            // here, never sent to the default with the prefix still on.
            let Some(provider) = provider else {
                return super::anthropic_not_configured();
            };
            // 5. A deliberate transform: forward the serialised IR (pure
            // and deterministic, so the upstream prefix stays stable),
            // recorded as requested vs effective — nothing is "forced".
            backend = provider.clone();
            effective_model = Some(rest.to_owned());
            ir.anthropic_mut().set_model(rest);
            transformed = true;
        }

        // ── the quota gate + release marker ──
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
                .load_meters("anthropic_sub")
                .map(|snapshot| snapshot.map(|snapshot| snapshot.snapshot))
                .unwrap_or_else(|error| {
                    tracing::error!(%error, "meter snapshot load failed");
                    None
                });
        }
        if path == "/v1/messages" {
            // A release: grant/refresh an allowance for the
            // currently-exhausted meters only, and record it. The gate
            // fires on the marker + the session id + the toggle
            // — a sessionless request cannot hold an
            // allowance.
            if gate_armed
                && let Some(session) = session_id.as_deref()
                && let Some(release) = ir.anthropic().release_marker()
            {
                let meters = meters_snapshot.as_ref().map(Meters::over);
                let now = now_ms();
                let fresh = grant_for(meters, now);
                if let Err(error) =
                    crate::release::record_grant(&server.store, session, release, &fresh)
                {
                    tracing::error!(%error, "allowance record failed");
                }
                let merged = crate::release::merged(&server.store, session, &fresh, now);
                record_anthropic_released(
                    &server,
                    session,
                    backend.id(),
                    release,
                    &merged,
                    meters_snapshot.as_ref(),
                    frontend.as_deref(),
                );
            }

            // The strip: UNCONDITIONAL on this path — it runs for every
            // backend and regardless of the gate's toggle, because the
            // marker rule is a frozen public API and a toggled strip would
            // change the cached prefix of every conversation carrying a
            // marker (invariant 4; the marker rule's own contract). Record
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
        // The model this request is about to be sent on — the note-served
        // mark below needs it after `effective_model` moves into the
        // record context.
        served_model = effective_model.clone();
        record = Some(AnthropicRecordCtx {
            frontend: frontend.clone(),
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
            shape: Some(shape.clone()),
            ping,
            downgraded_from: None,
            downgraded_to: None,
            cache_stripped: None,
            system_merged: None,
            forced_from: None,
            forced_to: None,
            model_mappings: None,
            thinking_rewritten: false,
            translation_report: None,
        });
        gate_shape = Some(shape);
        parsed = Some(ir);
    }

    // ── the gate decision (after the strip) ──
    //
    // Runs on the exact `/v1/messages` path, only for the anthropic_sub
    // backend (the sole meter source), and only when the gate is enabled —
    // for every other backend this whole block is a no-op that never even
    // reads meters. It runs for unparseable bodies too (the decision is
    // on `gated` alone): a client that cannot parse an event stream still
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
                .load_meters("anthropic_sub")
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
            // docs). The context size is the session's largest lane's, so
            // the choice the block forces (resume this conversation when
            // the quota resets, or start clean) can be made from the
            // notice. A sessionless request, a store error, or a session
            // the table has forgotten is `None`, and the notice drops the
            // clause rather than guessing (absence ≠ zero, invariant 3) —
            // a lost clause, never a lost answer.
            let context_tokens = session_id.as_deref().and_then(|session| {
                server
                    .store
                    .load_lanes()
                    .ok()
                    .and_then(|lanes| lanes::session_prompt(&lanes, session))
            });
            // The over marker is offered only while the blocked meter's
            // plan has room for it.
            let plan_left = meters_snapshot
                .as_ref()
                .is_some_and(|snapshot| plan_room(Meters::over(snapshot), meter));
            let text = Blocking::notice(
                meter,
                resets_at,
                plan_left,
                context_tokens,
                &jiff::tz::TimeZone::system(),
                server.config.notices.style_for(frontend.as_deref()),
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
                frontend: frontend.as_deref(),
                server: &server,
                started,
                path,
                session_id: session_id.as_deref(),
                backend_id: backend.id(),
                meter,
                resets_at,
                // The same figure the notice states, or `None` where the
                // lane table cannot say — "not recorded", never "empty".
                context_tokens,
                stale_meters: meters_snapshot.as_ref(),
            });
            return build_response(StatusCode::OK, headers, Body::from(body));
        }
    }

    // ── the cold-cache notice, after the quota gate ──
    //
    // If both would fire, the harder stop wins (the quota block returned
    // above). This one is advisory: it fires once per idle spell and
    // re-arms, and has no release marker, because sending the request
    // again IS the override. Like the predecessor's
    // "cold on AND gated" it is not
    // restricted to the meter-source backend — the cold gate needs only
    // idle time and prompt size per lane, which the IR always has — and
    // it is skipped for unparseable bodies, which have neither a shape
    // nor a lane (the same verdict the `?`-lane miss reaches).
    // Every failure below is "a lost notice, never a lost request": a
    // store error reads as absence and the request forwards.
    let cold_armed = path == "/v1/messages" && server.config.gates.cold_enabled;
    let cold_lane_key = gate_shape
        .as_ref()
        .map(|shape| shape.tools_hash.as_str())
        .and_then(|tools| lanes::lane_key(session_id.as_deref(), Some(tools)));
    let cold_lane = cold_lane_key
        .as_ref()
        .and_then(|key| server.store.load_lane(key).ok().flatten());
    if cold_armed {
        let gates = &server.config.gates;
        let now = now_ms();
        let min_idle_ms = cold_idle_ms(gates);
        let turn = match gate_shape.as_ref() {
            Some(shape) if shape.summarising => cold::Turn::Summarising,
            Some(shape) if shape.recap => cold::Turn::Recap,
            _ => cold::Turn::Ordinary,
        };
        // Twice, deliberately: the first call is
        // the cheap one and decides whether anything would fire at all;
        // only then is the outlook worth measuring — a weight refit over
        // the recent ledger is nothing against a request about to be
        // stopped and far too much for every request.
        let fired = cold::decide_cold(
            cold_lane.as_ref(),
            turn,
            gates.cold_min_tokens,
            min_idle_ms,
            now,
            None,
            Some(force_newest::prompt_bound(original.len() as u64)),
        );
        // The writes-free exemption (the README's "if cache writes are
        // free at the backend, this doesn't apply"): a notice exists to
        // warn about a re-read the rate-limit window will meter — when
        // the backend's own fetched catalogue says this model's cache
        // writes cost nothing, the re-read is free and the interruption
        // buys nothing. Checked before the outlook: the exemption
        // answers the question the refit would price. Only a POSITIVE
        // free verdict exempts (Some(true)); an unknown model never
        // does — the conservative direction for a gate that fires.
        let writes_free = matches!(fired, cold::ColdDecision::Notice { .. })
            && writes_free_of(&server, backend.as_ref(), served_model.as_deref());
        let outlook = if writes_free {
            // The exemption answered before the outlook was worth
            // measuring — a weight refit prices a re-read the catalogue
            // already said is free.
            None
        } else {
            match &fired {
                cold::ColdDecision::Notice { prompt, .. } if gates.cold_outlook => {
                    // Priced as what the backend's model map will send
                    // (the predecessor's `previewMappedModel`): the fit's
                    // weights are keyed on served identities, and the
                    // client's alias is not one. Measured on the routed
                    // backend's own meters only.
                    let sent = served_model.as_deref().and_then(|model| {
                        model_map::preview_mapped_model(backend.model_map(), model)
                    });
                    cold::outlook_over(&server.store, backend.id(), sent, *prompt, gate_armed, now)
                }
                _ => None,
            }
        };
        // The exemption is a caller-side veto, not a verdict the decision
        // knows about: with it holding, the would-fire Notice is handled
        // below as a withheld one and the second decision never runs.
        let verdict = if writes_free {
            fired
        } else if matches!(fired, cold::ColdDecision::Notice { .. }) {
            cold::decide_cold(
                cold_lane.as_ref(),
                turn,
                gates.cold_min_tokens,
                min_idle_ms,
                now,
                outlook.as_ref(),
                Some(force_newest::prompt_bound(original.len() as u64)),
            )
        } else {
            fired
        };
        let shape_for_rows = || gate_shape.as_ref().map(|shape| shape.tools_hash.as_str());
        match verdict {
            // Quietened: the window is not projected to run out even with
            // the re-read. Recorded so silence is distinguishable from
            // breakage; `at` and `noticed_at` are untouched (nothing was
            // said, nothing reached upstream), so the lane stays cold and
            // a later request in the same idle spell is judged again
            // against meters that may have tightened.
            // A recap on a cold lane: answered here, never forwarded,
            // and the lane untouched (neither `at` nor `noticed_at`), so
            // the notice stays armed for the prompt the user sends next
            // and a compaction still sees the lane cold.
            cold::ColdDecision::Recap { idle_ms, prompt } => {
                record_anthropic_cold_recap(ColdRecord {
                    frontend: frontend.as_deref(),
                    server: &server,
                    started,
                    path,
                    session_id: session_id.as_deref(),
                    backend_id: backend.id(),
                    tools_hash: shape_for_rows(),
                    idle_ms,
                    prompt,
                    outlook: None,
                    writes_free: false,
                    req_messages: gate_shape
                        .as_ref()
                        .and_then(|shape| shape.req_messages)
                        .map(|messages| messages as i64),
                    compact_target: None,
                    gate_on: server.config.gates.quota_enabled,
                });
                let rendering = if stream_explicitly_false {
                    Rendering::Json
                } else {
                    Rendering::Sse
                };
                let body =
                    Blocking::blocked_turn(cold::RECAP_HELD, client_model.as_deref(), rendering);
                let mut headers = HeaderMap::new();
                headers.insert(
                    header::CONTENT_TYPE,
                    match rendering {
                        Rendering::Sse => header::HeaderValue::from_static("text/event-stream"),
                        Rendering::Json => header::HeaderValue::from_static("application/json"),
                    },
                );
                return build_response(StatusCode::OK, headers, Body::from(body));
            }
            cold::ColdDecision::Quiet {
                idle_ms,
                prompt,
                outlook,
            } => {
                record_anthropic_cold_quiet(ColdRecord {
                    frontend: frontend.as_deref(),
                    server: &server,
                    started,
                    path,
                    session_id: session_id.as_deref(),
                    backend_id: backend.id(),
                    tools_hash: shape_for_rows(),
                    idle_ms,
                    prompt,
                    // The outlook withholding: the measured figures ride
                    // the row, and the writes-free flag says it was not
                    // that exemption.
                    outlook: Some(&outlook),
                    writes_free: false,
                    req_messages: None,
                    compact_target: None,
                    gate_on: server.config.gates.quota_enabled,
                });
            }
            cold::ColdDecision::Notice {
                idle_ms,
                prompt,
                outlook,
            } => {
                // The writes-free exemption withheld this notice: no
                // interruption, a `cold-quiet` row that says why
                // (`writesFree`). `at` and `noticed_at` are untouched
                // (nothing was said, nothing reached upstream), so the
                // lane stays cold and a later request in the same idle
                // spell is judged again — the withheld-notice discipline.
                if writes_free {
                    record_anthropic_cold_quiet(ColdRecord {
                        frontend: frontend.as_deref(),
                        server: &server,
                        started,
                        path,
                        session_id: session_id.as_deref(),
                        backend_id: backend.id(),
                        tools_hash: shape_for_rows(),
                        idle_ms,
                        prompt,
                        // No quota figures were measured — the exemption
                        // answered before the outlook was worth pricing.
                        outlook: None,
                        writes_free: true,
                        req_messages: None,
                        compact_target: None,
                        gate_on: server.config.gates.quota_enabled,
                    });
                } else {
                    // The compact model is resolved, not assumed:
                    // the notice names the model a
                    // cheap `/compact` would actually run on, and stays
                    // silent about it when there is none.
                    //
                    // Named as the identity the backend's model map will
                    // send — what the compaction will actually run on.
                    let compact_on = server
                        .models
                        .compaction_target(
                            &compact_spec(gates),
                            prompt,
                            backend.model_map(),
                            &|model: &str| declared_window(&server, backend.as_ref(), model),
                        )
                        .ok()
                        .flatten()
                        .and_then(|target| {
                            model_map::preview_mapped_model(backend.model_map(), &target)
                                .map(str::to_owned)
                        });
                    let text = cold::ColdBlocking::notice_for(
                        // The re-read is written back at the lane's own
                        // tier; a lane with no recorded tier says nothing
                        // rather than guess one.
                        cold_lane
                            .as_ref()
                            .and_then(|lane| lanes::Ttl::from_ms(lane.ttl))
                            .map(lanes::Ttl::write_multiplier),
                        idle_ms,
                        prompt,
                        compact_on.as_deref(),
                        outlook.as_ref(),
                        now,
                        &jiff::tz::TimeZone::system(),
                        server.config.notices.style_for(frontend.as_deref()),
                    );
                    let rendering = if stream_explicitly_false {
                        Rendering::Json
                    } else {
                        Rendering::Sse
                    };
                    let body = Blocking::blocked_turn(&text, client_model.as_deref(), rendering);
                    // The lane remembers it has spoken; `at` does not move —
                    // the compaction the user runs after reading the notice
                    // must still be seen as cold, which is the whole point
                    // of the two clocks.
                    if let Some(key) = &cold_lane_key
                        && let Err(error) = cold::note_lane_notice(&server.store, key, now)
                    {
                        tracing::error!(%error, "lane notice mark failed");
                    }
                    record_anthropic_cold(ColdRecord {
                        frontend: frontend.as_deref(),
                        server: &server,
                        started,
                        path,
                        session_id: session_id.as_deref(),
                        backend_id: backend.id(),
                        tools_hash: shape_for_rows(),
                        idle_ms,
                        prompt,
                        // The message count of the stopped request: the
                        // synthetic turn is appended to the client's
                        // transcript, so the next request in this lane should
                        // carry both.
                        req_messages: gate_shape
                            .as_ref()
                            .and_then(|shape| shape.req_messages)
                            .map(|messages| messages as i64),
                        compact_target: compact_on.as_deref(),
                        outlook: outlook.as_ref(),
                        writes_free: false,
                        gate_on: server.config.gates.quota_enabled,
                    });
                    let mut headers = HeaderMap::new();
                    headers.insert(
                        header::CONTENT_TYPE,
                        match rendering {
                            Rendering::Sse => header::HeaderValue::from_static("text/event-stream"),
                            Rendering::Json => header::HeaderValue::from_static("application/json"),
                        },
                    );
                    return build_response(StatusCode::OK, headers, Body::from(body));
                }
            }
            cold::ColdDecision::Forward => {}
        }
    }

    // ── the compaction retarget ──
    //
    // A cold compaction, rewritten onto a cheaper model with its cache
    // writes removed. Deliberately NOT gated on cold_enabled (gated on
    // the path and the compaction test alone): the cold licence is the
    // lane's, not the notice toggle's. Coldness is judged on `at`, which
    // the notice above does not move — so the compaction the user runs
    // after reading the notice is still seen as cold. And the notice
    // above exempted it by the summarising flag, so nothing has
    // interrupted it: the gate exists to advise this.
    if path == "/v1/messages"
        && picks_from_anthropic_catalogue(backend.as_ref())
        && let Some(ir) = parsed.as_mut()
        && gate_shape
            .as_ref()
            .is_some_and(AnthropicShape::is_compaction)
    {
        let gates = &server.config.gates;
        let now = now_ms();
        let min_idle_ms = cold_idle_ms(gates);
        let lane_cold =
            cold::lane_is_cold(cold_lane.as_ref(), gates.cold_min_tokens, min_idle_ms, now);
        if lane_cold {
            // Resolved per request: a family spec follows what is actually
            // in use, and the size guard needs the prompt this lane is
            // carrying. `target` is optional — a compaction already on
            // the cheapest sensible model has no move to make, but a cold
            // lane still means its cache writes are bought and never
            // read, so the strip goes ahead without one.
            let prompt = cold_lane
                .as_ref()
                .and_then(|lane| lane.prompt_tokens)
                .filter(|prompt| *prompt >= 0)
                .unwrap_or_default() as u64;
            let target = server
                .models
                .compaction_target(
                    &compact_spec(gates),
                    prompt,
                    backend.model_map(),
                    &|model: &str| declared_window(&server, backend.as_ref(), model),
                )
                .ok()
                .flatten();
            if let Some(outcome) =
                cold::retarget_compaction(ir, target.as_deref(), true, backend.model_map())
            {
                // The transformed serialised body IS the point: the model
                // region changed and the breakpoints went, so the upstream
                // sees bytes that never existed on the frontend's wire.
                // The fidelity compare ran on the pre-transform body, so
                // this deliberate rewrite can never surface as drift.
                forward = Bytes::from(ir.serialise());
                // A same-model strip is not a downgrade, and recording one
                // would put a model in `downgradedFrom` that also served
                // the request — only `cacheStripped` says it happened.
                let downgraded_from = (outcome.to != outcome.from)
                    .then(|| outcome.from.clone())
                    .flatten();
                let downgraded_to = downgraded_from
                    .is_some()
                    .then(|| outcome.to.clone())
                    .flatten();
                if let Some(ctx) = record.as_mut() {
                    ctx.downgraded_from = downgraded_from;
                    ctx.downgraded_to = downgraded_to;
                    ctx.cache_stripped = (outcome.stripped > 0).then_some(true);
                    ctx.system_merged = (outcome.merged > 0).then_some(true);
                }
                served_model = outcome.to.clone();
                tracing::info!(
                    "compact: {} · {} breakpoint(s) dropped · {} system message(s) merged",
                    match (&outcome.from, &outcome.to) {
                        (Some(from), Some(to)) if from != to => format!("{from} → {to}"),
                        (Some(from), _) => from.clone(),
                        _ => "?".to_owned(),
                    },
                    outcome.stripped,
                    outcome.merged,
                );
            }
        }
    }

    // ── the force-newest rewrite ──
    //
    // Use the newest version of whatever model was asked for — but only
    // where no cache can be lost by it. A known lane that is cold has
    // nothing to lose; an UNKNOWN lane is the awkward one (the table
    // forgets lanes, so a session whose cache is warm upstream can look
    // brand new here), and qualifies when the request itself shows a
    // short conversation, or when nothing has been served on the model
    // it asks for within a full TTL — no cache it could read exists.
    // Once a lane has been moved it stays moved while its cache is warm:
    // that cache is on the new model now, and sending the client's
    // choice through would rebuild it on the old one.
    //
    // Never runs after a compaction retarget that moved the model
    // (`!downgradedFrom`): that rewrite already chose the model this
    // compaction will run on. The only body edit is set_model — every
    // cache_control survives, unlike the retarget, because this rewrite
    // STARTS a conversation that should cache its prefix on the model it
    // is actually going to use. The fidelity compare ran on the
    // pre-transform body, so this deliberate rewrite can never surface as
    // drift.
    if path == "/v1/messages"
        && server.config.gates.force_newest
        && picks_from_anthropic_catalogue(backend.as_ref())
        && record
            .as_ref()
            .is_none_or(|ctx| ctx.downgraded_from.is_none())
        && let Some(ir) = parsed.as_mut()
    {
        // The asked model: the model the body names as it
        // stands at this point — after routing, after any retarget —
        // which is `served_model`'s reading here. The lane is the same
        // record the cold gate loaded; the shape is the request's own,
        // pre-transform (the row's shape fields are).
        // The map preview:
        // recency reads the identity the request would be MAPPED to,
        // never the asked model — without this, a claimed model (only
        // ever served as its target) would always read as idle.
        let asked = served_model.clone();
        let served_as = match (backend.model_map(), asked.as_deref()) {
            (Some(map), Some(model)) => model_map::preview_mapped_model(Some(map), model),
            _ => None,
        };
        let decision = force_newest::decide(
            &force_newest::ForceContext {
                model: served_model.as_deref(),
                served_as,
                lane: cold_lane.as_ref(),
                req_messages: gate_shape.as_ref().and_then(|shape| shape.req_messages),
                compaction: gate_shape
                    .as_ref()
                    .is_some_and(AnthropicShape::is_compaction),
                body_bytes: forward.len() as u64,
                min_idle_ms: cold_idle_ms(&server.config.gates),
                now_ms: now_ms(),
                declared: &|model: &str| declared_window(&server, backend.as_ref(), model),
            },
            &server.models,
        );
        if let ForceDecision::Move(forced) = decision {
            ir.anthropic_mut().set_model(&forced.to);
            forward = Bytes::from(ir.serialise());
            if let Some(ctx) = record.as_mut() {
                ctx.forced_from = Some(forced.from.clone());
                ctx.forced_to = Some(forced.to.clone());
            }
            // The served-model mark
            // must see the model actually being sent.
            served_model = Some(forced.to.clone());
            tracing::info!("model: {} → {}", forced.from, forced.to);
        }
    }

    // ── the model routing map (the FINAL
    // routing stage) ──
    //
    // A provider carrying `[providers.<id>.model_map]` rewrites the
    // model positions in the serialised body: top-level on /v1/messages
    // and /count_tokens, per-request in batches. A deliberate transform —
    // the spliced bytes are the point, and the fidelity compare ran on
    // the pre-transform body, so a mapped model can never surface as
    // drift. Matching is exact-identity first, then family; unmatched
    // requests keep their exact bytes (the unchanged path). Applies AFTER
    // force-newest (which previewed through the map above), and BEFORE
    // the served-model mark — the mark must see what is actually sent.
    if let Some(map) = backend.model_map() {
        let rewrite = model_map::rewrite_mapped_models(Some(map), &forward, "POST", path);
        if rewrite.mapped {
            forward = Bytes::from(rewrite.body);
            if let Some(effective) = rewrite.effective_model.clone() {
                // Provenance (requested/effective pair): the
                // row's requested model keeps its first reading (what the
                // frontend asked, routing prefixes included); the
                // effective one becomes the mapped target.
                if let Some(ctx) = record.as_mut() {
                    ctx.effective_model = Some(effective.clone());
                }
                served_model = Some(effective);
                tracing::info!(
                    "model map: {} → {}",
                    rewrite.pre_map_model.as_deref().unwrap_or("?"),
                    served_model.as_deref().unwrap_or("?")
                );
            } else {
                // A batch has no single top-level model, so the map
                // reports no effective one: its provenance is the
                // per-request list of the entries the map matched (the
                // predecessor's `modelMappings`). Without it a mapped
                // batch's row could not say where its requests went.
                let mappings: Vec<serde_json::Value> = rewrite
                    .models
                    .iter()
                    .filter(|position| position.matched)
                    .map(|position| {
                        serde_json::json!({
                            "requestIndex": position.request_index,
                            "requestedModel": position.pre_map_model,
                            "effectiveModel": position.effective_model,
                        })
                    })
                    .collect();
                if !mappings.is_empty()
                    && let Some(ctx) = record.as_mut()
                {
                    ctx.model_mappings = Some(serde_json::Value::Array(mappings));
                }
            }
        }
    }

    // ── mid-conversation effort, for models that reject it ──
    //
    // Claude Code moves the reasoning effort with an `output_config` on a
    // system message and replays it every turn; a backend whose model does
    // not know the field 400s the request. Strip it there, after the map so
    // the check sees the model actually being sent.
    if path == "/v1/messages"
        && let Some(model) = served_model.as_deref()
        && !backend.accepts_message_effort(model)
        && let Ok(mut ir) = crate::ir::Request::parse(&forward)
        && ir.anthropic_mut().strip_message_effort()
    {
        forward = Bytes::from(ir.serialise());
        tracing::info!("stripped mid-conversation effort for {model}");
    }

    // ── the served-model mark, BEFORE the request goes ──
    //
    // Not after: a lane deciding while this one is still in flight must see
    // the final effective model as in use. The response may later name a
    // different served identity; that remains authoritative for
    // observations and accounting, and the row's insert marks it served
    // too (`record_anthropic::insert`). In-memory and infallible, like an
    // in-memory map. Gated to the exact `/v1/messages` path.
    if path == "/v1/messages" {
        server.models.note_served(served_model.as_deref(), now_ms());
    }

    // ── the codex branch: translate instead of byte-forward ──
    //
    // The codex backend speaks the Responses dialect, not Anthropic's:
    // a request routed here goes through the translation unit (both
    // directions), never byte-forwarded. count_tokens and batches have
    // no codex equivalent (the CLI defines none) — those paths answer
    // with a typed anthropic error instead of forwarding JSON the
    // backend would only reject.
    if backend.id() == "codex_sub" {
        if path == "/v1/messages" {
            return codex::turn(codex::CodexTurn {
                server,
                backend: backend.clone(),
                parsed,
                gate_shape,
                record,
                in_flight,
                session_id,
                thread_id: None,
                request_id: None,
                served_model,
                stream_explicitly_false,
                frontend_wire: codex::CodexFrontendWire::Anthropic,
            })
            .await;
        }
        // count_tokens and batches have no codex equivalent (the codex
        // client defines none): a typed anthropic error, never a
        // byte-forward of JSON the backend would only reject. No row —
        // the status is toker's own (the 502 rule: a proxy-generated
        // status is never a fabricated provider measurement). These
        // paths are JSON-only by definition (no stream flag exists on
        // them), so the error renders as the JSON object.
        return codex::anthropic_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "this backend does not support count_tokens or batch requests",
            false,
        );
    }

    // A stale anthropic models catalogue refreshes on this request's
    // own bearer when toker holds no key (spawned; never waited on).
    server.borrow_catalog_credential(backend.as_ref(), &parts.headers);

    // Native Messages bindings still traverse the universal canonical
    // request path. Parse the FINAL middleware output, not `parsed`: the
    // licensed body rewrites above may have reserialised it since ingress.
    if path == "/v1/messages" {
        let dialect = backend
            .bindings()
            .iter()
            .find(|binding| binding.protocol() == ProtocolId::AnthropicMessages)
            .map(|binding| binding.dialect());
        let rendered = serde_json::from_slice::<serde_json::Value>(&forward)
            .map_err(|error| translate::TranslateError::Malformed {
                reason: error.to_string(),
            })
            .and_then(|body| translate::from_anthropic(&body))
            .and_then(|mut canonical| {
                canonical.model.clone_from(&served_model);
                let Some(dialect) = dialect else {
                    return Err(translate::TranslateError::Malformed {
                        reason: format!("{} has no Messages binding", backend.id()),
                    });
                };
                translate::render_anthropic(&canonical, dialect)
            });
        let rendered = match rendered {
            Ok(rendered) => rendered,
            Err(error) => {
                let stream = serde_json::from_slice::<serde_json::Value>(&forward)
                    .ok()
                    .and_then(|body| body.get("stream").and_then(serde_json::Value::as_bool))
                    .unwrap_or(false);
                return codex::anthropic_error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("the request could not be translated: {error}"),
                    stream,
                );
            }
        };
        forward = match serde_json::to_vec(&rendered.value) {
            Ok(body) => Bytes::from(body),
            Err(error) => {
                tracing::error!(%error, "canonical Messages request serialisation failed");
                return plain_status(StatusCode::INTERNAL_SERVER_ERROR, "translation failed\n");
            }
        };
    }

    // 6. Upstream; 7.-9. in the response paths. Session headers pass
    // through (see the module docs), so the strip list is
    // empty; `x-toker-*` is stripped unconditionally either way.
    let sent = forward.clone();
    match send_upstream(&server, backend.as_ref(), &parts, forward, &[]).await {
        Ok(upstream) => {
            let upstream = if path == "/v1/messages" {
                match retry_thinking_off(
                    &server,
                    backend.as_ref(),
                    &parts,
                    sent,
                    upstream,
                    &mut record,
                )
                .await
                {
                    Ok(upstream) => upstream,
                    Err(response) => return *response,
                }
            } else {
                upstream
            };
            if path == "/v1/messages" {
                forward_canonical_messages(
                    server,
                    backend,
                    upstream,
                    record,
                    in_flight,
                    served_model.as_deref().unwrap_or_default(),
                )
                .await
            } else {
                forward_legacy_response(server, backend, upstream, record, in_flight).await
            }
        }
        Err(error) => {
            // No upstream response: nothing measured, and the error row is
            // provider-response-shaped (status/type/retry-after), so this
            // surfaces as 502 unledgered and logged — never a fabricated
            // provider status. The guard drops here too: the exchange is
            // over, however it ended.
            tracing::warn!(%error, "upstream request failed");
            transport_failure(ErrorWire::Anthropic, &error)
        }
    }
}

/// Feed the meters from EVERY response, not just accounted ones: a 429, a
/// count_tokens, a background batch poll, a 400 the thinking retry
/// swallowed still report the meters, and the gate must not go stale. Only
/// a meter-source backend has meters to report (the sub; the API's RPM
/// headers are not quota meters and must not overwrite the gate's
/// snapshot).
fn note_meters(server: &Server, backend: &dyn Provider, headers: &HeaderMap) {
    let Some(meters) = backend.meters(headers) else {
        return;
    };
    server.note_quota(backend.id(), &meters);
    let snapshot = MetersSnapshot {
        updated_ms: now_ms(),
        snapshot: meters,
    };
    if let Err(error) = server.store.save_meters(backend.id(), &snapshot) {
        tracing::error!(%error, "meter snapshot save failed");
    }
}

/// Send the request once more with `thinking: disabled` rewritten to
/// `between_tools`, when the upstream refused the first with a 400 that
/// names that form. Any other response comes back as it arrived.
///
/// Claude Code turns thinking off on its side calls (the auto-mode
/// classifier among them) and decides per model whether `disabled` is
/// allowed from a list built into the client. From 2026-10-08 Anthropic
/// started refusing it on `claude-sonnet-5` in bursts, with "To turn
/// thinking off on this model, send `between_tools`", while the same call
/// succeeded between bursts; a client built before that keeps sending
/// `disabled`, and every Bash permission check in auto mode fails. The
/// rewrite waits for the refusal rather than running up front, because
/// whether `between_tools` is accepted where `disabled` still is was never
/// observed, and the refusal is the only evidence a model wants it.
///
/// Once only: a second refusal is forwarded and recorded like any error.
/// The refused response's meters still feed the gate.
async fn retry_thinking_off(
    server: &Server,
    backend: &dyn Provider,
    parts: &axum::http::request::Parts,
    sent: Bytes,
    upstream: reqwest::Response,
    record: &mut Option<AnthropicRecordCtx>,
) -> Result<reqwest::Response, Box<Response>> {
    if upstream.status() != StatusCode::BAD_REQUEST {
        return Ok(upstream);
    }
    let Some(retry) = between_tools_body(&sent) else {
        return Ok(upstream);
    };
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
        return Err(Box::new(truncated_body(ErrorWire::Anthropic)));
    };
    let wants_between_tools = buffered.rest.is_none()
        && error_pair(&buffered.bytes)
            .1
            .is_some_and(|message| message.contains("between_tools"));
    if !wants_between_tools {
        return Ok(rebuilt_response(status, headers, buffered));
    }
    note_meters(server, backend, &headers);
    tracing::info!("upstream refused thinking: disabled; sending again as between_tools");
    if let Some(ctx) = record {
        ctx.thinking_rewritten = true;
    }
    send_upstream(server, backend, parts, retry, &[])
        .await
        .map_err(|error| {
            tracing::warn!(%error, "upstream request failed");
            Box::new(transport_failure(ErrorWire::Anthropic, &error))
        })
}

/// The sent body with `thinking: disabled` made `between_tools`
/// ([`crate::ir::AnthropicBodyMut::thinking_between_tools`]), or `None`
/// when it did not turn thinking off (or does not parse), so there is
/// nothing to retry.
fn between_tools_body(sent: &Bytes) -> Option<Bytes> {
    let mut ir = IrRequest::parse(sent).ok()?;
    ir.anthropic_mut()
        .thinking_between_tools()
        .then(|| Bytes::from(ir.serialise()))
}

/// A response put back together from what [`retry_thinking_off`] buffered
/// to read its error, so [`forward_legacy_response`] handles it as if it had
/// never been read: the same status, headers, and bytes, with any
/// unbuffered remainder chained on.
fn rebuilt_response(
    status: StatusCode,
    headers: HeaderMap,
    buffered: super::proxy::Buffered,
) -> reqwest::Response {
    let body = match buffered.rest {
        None => reqwest::Body::from(buffered.bytes),
        Some(rest) => reqwest::Body::wrap_stream(
            futures::stream::iter([Ok::<_, reqwest::Error>(Bytes::from(buffered.bytes))])
                .chain(rest.bytes_stream()),
        ),
    };
    let mut response = axum::http::Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    reqwest::Response::from(response)
}

/// Whether the rewrites that pick their target from the Anthropic model
/// catalogue (the compaction retarget, force-newest) may run on this
/// backend. Both write a bare `claude-*` id, which on openrouter would
/// move a conversation off the model the user picked onto whatever
/// openrouter makes of that id. Codex keeps them: its model map turns
/// their `claude-*` targets into codex ids before the request leaves.
fn picks_from_anthropic_catalogue(backend: &dyn Provider) -> bool {
    backend.id() != "openrouter"
}

/// The session's stored allowances, or none for a sessionless request
/// (a sessionless lookup is undefined — the gate then sees no
/// allowances). A store error
/// loses the allowances, never the request (invariant 6): the gate
/// treats it as "nothing held", the conservative reading.
fn allowances_for_session(server: &Server, session_id: Option<&str>) -> Vec<Allowance> {
    let Some(session) = session_id else {
        return Vec::new();
    };
    server
        .store
        .load_session_allowances(session)
        .unwrap_or_else(|error| {
            tracing::error!(%error, "allowances load failed");
            Vec::new()
        })
}

/// The cold gate's idle floor override, in ms
/// (minutes, deliberately fractional — unset follows
/// the TTL tier the lane was last seen writing). Config validation
/// already rejects the negative.
fn cold_idle_ms(gates: &GatesConfig) -> Option<i64> {
    gates
        .cold_idle_min
        .map(|minutes| (minutes * 60_000.0) as i64)
}

/// Whether the backend's fetched catalogue says `model`'s cache writes
/// are free — the cold gate's writes-free exemption, consulted only
/// when a notice would otherwise fire. The model is the one about to
/// be sent, previewed through the backend's model map (the map's own
/// rewrite stage runs after the gate, so the exemption must see the
/// identity the upstream will actually bill). Only a POSITIVE verdict
/// exempts; unknown — no catalogue, no entry, a pricing-less entry —
/// never does (invariant 3: a gate fires on ignorance).
fn writes_free_of(server: &Server, backend: &dyn Provider, model: Option<&str>) -> bool {
    let Some(model) = model.filter(|model| !model.is_empty()) else {
        return false;
    };
    let effective = match backend.model_map() {
        Some(map) => model_map::preview_mapped_model(Some(map), model),
        None => Some(model),
    };
    let Some(effective) = effective else {
        return false;
    };
    let catalogs = server
        .catalogs
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    catalogs.cache_writes_free(backend.id(), effective) == Some(true)
}

/// The backend's declared context window for `model`, from its fetched
/// models listing: authoritative over the learned ceiling in the rewrites'
/// fit check ([`crate::middleware::models::fits_context`]). Looked up as
/// the identity the backend's model map will send, since the listing is
/// keyed by what the provider serves. `None` when there is no listing or
/// it names no window, which leaves the learned ceiling to decide.
fn declared_window(server: &Server, backend: &dyn Provider, model: &str) -> Option<u64> {
    let effective = model_map::preview_mapped_model(backend.model_map(), model)?;
    let catalogs = server
        .catalogs
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    catalogs.context_window_of(backend.id(), effective)
}

/// The compaction retarget's model spec:
/// a family name resolved against what is actually in use (the default,
/// "sonnet" — not Haiku: its window is 200k and the lanes this fires on
/// routinely hold three times that), an explicit model id, or "off".
fn compact_spec(gates: &GatesConfig) -> String {
    gates
        .compact_model
        .clone()
        .unwrap_or_else(|| "sonnet".to_owned())
}

/// Transparent forwarding (the batch-result paths and every unmatched
/// path): routed to the default
/// anthropic backend, auth rules applied, bytes both ways untouched — no
/// recording, no observation, like the openai `/v1/models` path. The
/// meters still feed: a background batch poll is exactly the call the
/// "not just accounted ones" rule names. No in-flight hold either — the
/// count is only on the exact `/v1/messages` path.
async fn transparent(server: Server, request: Request) -> Response {
    let Some(backend) = server.default_anthropic().cloned() else {
        return super::anthropic_not_configured();
    };
    // The batch paths have no codex equivalent: the same typed error the
    // usage path answers for count_tokens and batch creation, rather
    // than an anthropic request sent to the codex upstream.
    if backend.id() == "codex_sub" {
        return codex::anthropic_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "this backend does not support count_tokens or batch requests",
            false,
        );
    }
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
    match send_upstream(&server, backend.as_ref(), &parts, body, &[]).await {
        Ok(upstream) => forward_legacy_response(server, backend, upstream, None, None).await,
        Err(error) => {
            tracing::warn!(%error, "upstream request failed");
            transport_failure(ErrorWire::Anthropic, &error)
        }
    }
}

/// Anthropic routing (plan: Routing). A provider's own name selects that
/// backend; the generic `anthropic/` family prefix selects the protocol
/// default (it names the protocol, not a provider — plan: "`provider/model`
/// names override per request … `anthropic/claude-opus-5`"); both are
/// stripped from the model. `openrouter/` is the one non-anthropic
/// provider here: openrouter serves the Anthropic Messages wire itself
/// (its `…/api/v1/messages`), so the route uses the Messages canonical
/// adapter with openrouter's key in place of the client's anthropic credential
/// (`OpenRouter::strip_foreign_credentials`). This is how a Claude Code
/// `modelPicker` row reaches an openrouter model. Anything else — bare names,
/// other prefixes — goes to the configured default, with no routing rewrite.
/// The provider is `None` when the prefix names a backend whose block is
/// absent.
fn strip_anthropic_prefix<'a>(
    server: &'a Server,
    model: &'a str,
) -> Option<(Option<&'a Arc<dyn Provider>>, &'a str)> {
    if let Some(rest) = model.strip_prefix("anthropic_sub/") {
        Some((server.anthropic_sub.as_ref(), rest))
    } else if let Some(rest) = model.strip_prefix("anthropic_api/") {
        Some((server.anthropic_api.as_ref(), rest))
    } else if let Some(rest) = model.strip_prefix("openrouter/") {
        Some((server.openrouter.as_ref(), rest))
    } else {
        model
            .strip_prefix("anthropic/")
            .map(|rest| (server.default_anthropic(), rest))
    }
}

/// The `anthropic-beta` request header, split into its flags:
/// a comma-separated list of feature flags and nothing
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

/// A native Messages binding's universal-canonical response path. The
/// provider bytes feed accounting before interpretation; only the client wire
/// is rendered from canonical events or a canonical complete turn.
async fn forward_canonical_messages(
    server: Server,
    backend: Arc<dyn Provider>,
    upstream: reqwest::Response,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    model: &str,
) -> Response {
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();

    note_meters(&server, backend.as_ref(), &upstream_headers);
    let rate_limits = parse_rate_limits(&upstream_headers);

    if is_compressed(&upstream_headers) {
        tracing::warn!("compressed canonical Messages response could not be interpreted");
        return super::proxy::upstream_failure(
            ErrorWire::Anthropic,
            "toker could not interpret a compressed upstream response",
        );
    }

    if !status.is_success() {
        let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
            return truncated_body(ErrorWire::Anthropic);
        };
        if buffered.rest.is_some() {
            return super::proxy::upstream_failure(
                ErrorWire::Anthropic,
                "toker could not interpret an oversized upstream error",
            );
        }
        if let Some(ctx) = record.as_ref() {
            let (error_type, error_message) = error_pair(&buffered.bytes);
            record_anthropic_error(
                ctx,
                status.as_u16(),
                error_type,
                error_message,
                retry_after_ms(&upstream_headers),
                rate_limits.as_ref(),
            );
        }
        let rendered = serde_json::from_slice::<serde_json::Value>(&buffered.bytes)
            .ok()
            .and_then(|body| translate::canonical_turn_from_anthropic(&body).ok())
            .map(|turn| translate::anthropic_frontend::anthropic_from_canonical(model, &turn));
        let Some(rendered) = rendered else {
            return super::proxy::upstream_failure(
                ErrorWire::Anthropic,
                "toker could not interpret the upstream error response",
            );
        };
        return anthropic_json_response(status, &upstream_headers, &rendered);
    }

    if is_event_stream(&upstream_headers) {
        let stream = CanonicalMessagesStream::new(
            upstream,
            status.as_u16(),
            record,
            rate_limits,
            in_flight,
            model,
        );
        let mut headers = response_headers(&upstream_headers, false);
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        return build_response(status, headers, Body::from_stream(stream));
    }

    let Ok(buffered) = buffer_up_to(upstream, MAX_RESPONSE_BUFFER).await else {
        return truncated_body(ErrorWire::Anthropic);
    };
    if buffered.rest.is_some() {
        return super::proxy::upstream_failure(
            ErrorWire::Anthropic,
            "toker could not interpret an oversized upstream response",
        );
    }
    let turn = match serde_json::from_slice::<serde_json::Value>(&buffered.bytes)
        .map_err(|error| error.to_string())
        .and_then(|body| {
            translate::canonical_turn_from_anthropic(&body).map_err(|error| error.to_string())
        }) {
        Ok(turn) => turn,
        Err(error) => {
            tracing::warn!(%error, "canonical Messages response interpretation failed");
            return super::proxy::upstream_failure(
                ErrorWire::Anthropic,
                "toker could not interpret the upstream response",
            );
        }
    };
    if let Some(ctx) = record.as_ref() {
        let mut observer = AnthropicObserver::new();
        observer.observe_json(&buffered.bytes);
        let capture = observer.finish();
        record_anthropic_measurement(ctx, capture.as_ref(), rate_limits.as_ref(), status.as_u16());
    }
    let body = translate::anthropic_frontend::anthropic_from_canonical(model, &turn);
    anthropic_json_response(status, &upstream_headers, &body)
}

fn anthropic_json_response(
    status: StatusCode,
    upstream_headers: &HeaderMap,
    value: &serde_json::Value,
) -> Response {
    let mut headers = response_headers(upstream_headers, false);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    build_response(
        status,
        headers,
        Body::from(serde_json::to_vec(value).unwrap_or_default()),
    )
}

struct CanonicalMessagesStream {
    inner: Pin<Box<Abortable<UpstreamBody>>>,
    abort: AbortHandle,
    splitter: SseSplitter,
    backend: translate::AnthropicResponseStream,
    frontend: translate::anthropic_frontend::AnthropicRenderer,
    observer: AnthropicObserver,
    pending: VecDeque<Bytes>,
    failure: Option<std::io::Error>,
    upstream_done: bool,
    ctx: Option<AnthropicRecordCtx>,
    rate_limits: Option<serde_json::Value>,
    in_flight: Option<InFlightGuard>,
    status: u16,
}

impl CanonicalMessagesStream {
    fn new(
        response: reqwest::Response,
        status: u16,
        ctx: Option<AnthropicRecordCtx>,
        rate_limits: Option<serde_json::Value>,
        in_flight: Option<InFlightGuard>,
        model: &str,
    ) -> CanonicalMessagesStream {
        let (abort, registration) = AbortHandle::new_pair();
        let stream: UpstreamBody = Box::pin(response.bytes_stream());
        CanonicalMessagesStream {
            inner: Box::pin(Abortable::new(stream, registration)),
            abort,
            splitter: SseSplitter::new(),
            backend: translate::AnthropicResponseStream::new(),
            frontend: translate::anthropic_frontend::AnthropicRenderer::new(model),
            observer: AnthropicObserver::new(),
            pending: VecDeque::new(),
            failure: None,
            upstream_done: false,
            ctx,
            rate_limits,
            in_flight,
            status,
        }
    }

    fn translate_event(&mut self, event: crate::observe::sse::SseEvent) {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.observer.observe_event(&event);
        }));
        let canonical = self.backend.feed_sse(&event);
        for emitted in canonical.iter().flat_map(|event| self.frontend.feed(event)) {
            self.pending.push_back(anthropic_sse_bytes(&emitted));
        }
    }

    fn finish_translation(&mut self) {
        if let Some(event) = self.splitter.finish() {
            self.translate_event(event);
        }
        if !self.frontend.turn_ended() {
            self.ctx.take();
            self.failure = Some(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "upstream Messages stream closed before the turn ended",
            ));
        }
        self.upstream_done = true;
    }

    fn finish_recording(&mut self) {
        if let Some(ctx) = self.ctx.take() {
            let capture = std::mem::take(&mut self.observer).finish();
            record_anthropic_measurement(
                &ctx,
                capture.as_ref(),
                self.rate_limits.as_ref(),
                self.status,
            );
        }
        drop(self.in_flight.take());
    }
}

impl Stream for CanonicalMessagesStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(bytes) = this.pending.pop_front() {
                return std::task::Poll::Ready(Some(Ok(bytes)));
            }
            if let Some(error) = this.failure.take() {
                drop(this.in_flight.take());
                return std::task::Poll::Ready(Some(Err(error)));
            }
            if this.upstream_done {
                this.finish_recording();
                return std::task::Poll::Ready(None);
            }
            match this.inner.as_mut().poll_next(cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(Some(Ok(chunk))) => {
                    for event in this.splitter.feed(&chunk) {
                        this.translate_event(event);
                    }
                }
                std::task::Poll::Ready(Some(Err(error))) => {
                    tracing::warn!(%error, "upstream canonical Messages stream failed");
                    this.ctx.take();
                    drop(this.in_flight.take());
                    this.upstream_done = true;
                    return std::task::Poll::Ready(Some(Err(std::io::Error::other(error))));
                }
                std::task::Poll::Ready(None) => this.finish_translation(),
            }
        }
    }
}

impl Drop for CanonicalMessagesStream {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

fn anthropic_sse_bytes(event: &crate::observe::sse::SseEvent) -> Bytes {
    let mut out = String::new();
    if let Some(name) = &event.event {
        out.push_str("event: ");
        out.push_str(name);
        out.push('\n');
    }
    for line in &event.data_lines {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    Bytes::from(out)
}

/// Forward one upstream response to the client, branching on compression /
/// status / content-type — the anthropic mirror of the openai
/// `forward_upstream`, plus the meter feed. This remains for non-inference
/// Messages surfaces. `record` is their optional completion context; `None`
/// means transparent forwarding. `in_flight` is
/// the request's sleep-lock hold: it rides the SSE stream (dropping when
/// axum drops the body — the close semantics, "however the exchange ends") and
/// drops at the end of this function on every other branch, after
/// whatever row was owed has landed.
async fn forward_legacy_response(
    server: Server,
    backend: Arc<dyn Provider>,
    upstream: reqwest::Response,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
) -> Response {
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();

    note_meters(&server, backend.as_ref(), &upstream_headers);
    // The row's own copy — this response's headers, parsed, for the
    // measurement row and the error row alike (a failure's meters are the
    // only evidence of throttling); the meters_state table took the
    // update above regardless.
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
        let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
            return truncated_body(ErrorWire::Anthropic);
        };
        let (error_type, error_message) = error_pair(&buffered.bytes);
        let retry_after = retry_after_ms(&upstream_headers);
        record_anthropic_error(
            &ctx,
            status.as_u16(),
            error_type,
            error_message,
            retry_after,
            rate_limits.as_ref(),
        );
        let body = buffered_body(buffered);
        return build_response(status, response_headers(&upstream_headers, false), body);
    }

    // SSE: chunks stream through with backpressure, each also feeding the
    // side observation.
    if is_event_stream(&upstream_headers) {
        let stream =
            AnthropicObservedStream::new(upstream, status.as_u16(), ctx, rate_limits, in_flight);
        let body = Body::from_stream(stream);
        return build_response(status, response_headers(&upstream_headers, false), body);
    }

    // Non-SSE: buffer, observe, forward the original bytes unchanged.
    let Ok(buffered) = buffer_up_to(upstream, MAX_RESPONSE_BUFFER).await else {
        return truncated_body(ErrorWire::Anthropic);
    };
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
    /// The request's sleep-lock hold, riding the stream: it drops when
    /// axum drops the body — natural completion or client hangup — so the
    /// in-flight count never leaks on a streamed response (the
    /// body-close event).
    in_flight: Option<InFlightGuard>,
    status: u16,
}

impl AnthropicObservedStream {
    fn new(
        response: reqwest::Response,
        status: u16,
        ctx: AnthropicRecordCtx,
        rate_limits: Option<serde_json::Value>,
        in_flight: Option<InFlightGuard>,
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
            in_flight,
            status,
        }
    }
}

impl Stream for AnthropicObservedStream {
    /// The upstream's own error type, yielded so hyper aborts the
    /// client's response (see the openai `ObservedStream`): a clean end
    /// let a reset mid-turn reach claude as a complete, truncated turn.
    type Item = reqwest::Result<Bytes>;

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
                // Upstream transport died mid-stream (a reset, the idle
                // timeout): the response is truncated. No completion, no
                // row — drop the context so a later poll cannot record one.
                tracing::warn!(%error, "upstream response stream failed");
                this.ctx.take();
                // The exchange is over however it ended (the close
                // event fires on failure too): the in-flight hold goes
                // with it.
                drop(this.in_flight.take());
                // Yielded, so the client sees a transport error and
                // retries, never a clean end.
                std::task::Poll::Ready(Some(Err(error)))
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
                // The response is done, so the in-flight hold ends now —
                // the body-close event fires at stream end, and a
                // hung-up stream ends it in Drop instead.
                drop(this.in_flight.take());
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
