//! The proxy routes: chat completions (the usage path, fully recorded) and
//! models (transparent forwarding).
//!
//! Chat completions, in order (plan: Server core):
//!
//! 1. Buffer the request body fully.
//! 2. Parse the IR. A non-JSON body is forwarded unchanged with no
//!    recording — the proxy never rejects what it cannot understand
//!    (invariant 6 spirit).
//! 3. Fidelity check (invariant 5): byte-compare the re-serialised IR with
//!    the original. Exact → forward the original, byte-identical by
//!    construction. Drift → **still** forward the original (safe), with a
//!    `fidelity-drift` row at completion — drift is a visible metric.
//! 4. Routing: `openrouter/…` routes to openrouter with the prefix
//!    stripped; bare models go to the protocol default (phase 1 hardcodes
//!    both).
//! 5. A routed request forwards the *serialised* form — a transformed
//!    request forwards what the IR produces, and purity (invariant 4)
//!    makes that stable. Recorded as requested vs effective model.
//! 6. **The cold-cache notice** (plan: Middleware — cold gate): the openai
//!    path's own gate, on the lane the request itself keys (session ×
//!    tools-hash) and the post-routing model. No quota outlook — this
//!    backend has no meter source — and a per-model writes-free
//!    exemption: when the fetched openrouter catalogue says the model's
//!    cache writes cost nothing, the re-read the notice warns about is
//!    free and the gate never fires for it. On fire: 200 with a
//!    synthetic openai turn, a `cold` row, the lane marked noticed —
//!    never an error status; the resend IS the release (there is no
//!    marker on this wire).
//! 7. Upstream request with hop-by-hop headers stripped,
//!    `accept-encoding: identity` forced (SSE observation needs plaintext),
//!    and the stored credential injected only when the incoming request
//!    carries no Authorization of its own (pass-through-when-present).
//!    Another provider's credential is dropped first, never forwarded.
//! 8. A client hangup aborts the upstream (the body stream's Drop fires
//!    an [`AbortHandle`]); a hung-up stream records no row.
//! 9. Response branches: SSE streams through with the side observation;
//!    non-SSE bodies buffer, observe, and forward unchanged; unexpected
//!    compression passes through untouched and unledgered.
//! 10. Recording on completion only — [`record::RecordCtx`] → row, plus
//!     the lane-table note (the openai lane's clock is openrouter's
//!     10-minute sticky window, [`lanes::OPENAI_LANE_TTL_MS`]).

use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures::future::{AbortHandle, Abortable};
use futures::stream::{Stream, StreamExt};

use crate::ir::{Fidelity, Request as IrRequest, Shape, compare};
use crate::middleware::cold;
use crate::middleware::lanes;
use crate::observe::{SseSplitter, UsageObserver};
use crate::providers::Provider;

use super::InFlightGuard;
use super::Server;
use super::record::{
    ColdOpenaiRecord, RecordCtx, now_ms, parse_error_type, record_error, record_measurement,
    record_openai_cold, retry_after_ms,
};

/// Request bodies are buffered for gating and the fidelity check; 64 MiB
/// is far beyond any chat body, so hitting the cap is a client bug worth a
/// named status rather than a silent OOM.
pub(crate) const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;
/// Cap for buffered response bodies (non-streaming completions).
pub(crate) const MAX_RESPONSE_BUFFER: usize = 64 * 1024 * 1024;
/// Cap for buffered non-2xx bodies, which are small in practice.
pub(crate) const MAX_ERROR_BODY: usize = 16 * 1024 * 1024;

/// `POST /v1/chat/completions` — the usage path.
pub(crate) async fn chat_completions(State(server): State<Server>, request: Request) -> Response {
    let started = Instant::now();
    let (parts, body) = request.into_parts();

    // Session identity, read by name only — request headers are never
    // captured wholesale: they carry credentials (invariant 2).
    let session_id = session_id(&server.config.session_header_names, &parts.headers);
    // Ping tagging (plan: Middleware): a lane whose request carried the
    // ping header is recorded but excluded from liveness — the window
    // pinger's probe must never hold the sleep lock, on this path like
    // the anthropic one.
    let ping = lanes::is_ping(&parts.headers, &server.config.ping_header_name);

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

    // A request being served holds the machine awake: a lane's
    // `updated_ms` moves only when a response
    // finishes, and one long turn can outlast a 5-minute tier. Every
    // chat completion counts while it runs, pings included — a running
    // request is genuinely holding the machine, whatever tagged it; a
    // ping's LANE is what never holds the lock, and the ping flag on the
    // lane note below is what excludes it. The guard's Drop is the
    // decrement, so no early return — a cold notice, a 502 — can leak
    // it; for a streamed response it rides the body stream.
    let in_flight = Some(server.begin_in_flight());

    // 2.-5. Parse, fidelity-check, route.
    let mut forward = original.clone();
    let mut record = None;
    // The request's own shape and ask, kept past the record context: the
    // cold gate keys the lane on the session × tools-hash the request
    // itself carries, answers in the wire form the request asked for,
    // and exempts by the post-routing model.
    let mut gate_shape: Option<Shape> = None;
    let mut stream_requested = false;
    let mut gate_model: Option<String> = None;
    if let Ok(mut ir) = IrRequest::parse(&original) {
        // 3. Invariant 5, verified per request: Exact is the normal case;
        // Drift forwards the original buffer either way, and lands a
        // visible fidelity-drift row at completion.
        let mut drift = None;
        if let Fidelity::Drift { digest, .. } = compare(&original, &ir.serialise()) {
            drift = Some(digest);
        }
        // 4. Routing: read the model through the typed view; a provider
        // prefix overrides the backend per request.
        let model = ir.openai_chat().model().map(str::to_owned);
        let mut effective_model = model.clone();
        if let Some(rest) = model.as_deref().and_then(strip_provider_prefix) {
            // 5. A deliberate transform: forward the serialised IR (pure
            // and deterministic, so the upstream prefix stays stable),
            // recorded as requested vs effective — nothing is "forced".
            effective_model = Some(rest.to_owned());
            ir.openai_chat_mut().set_model(rest);
            forward = Bytes::from(ir.serialise());
        }
        let shape = ir.openai_chat().shape();
        let system_messages = shape.req_messages.map(|_| {
            ir.openai_chat()
                .messages()
                .iter()
                .filter(|m| m.is_system())
                .count() as u64
        });
        stream_requested = ir.openai_chat().stream();
        gate_model = effective_model.clone();
        gate_shape = Some(shape.clone());
        record = Some(RecordCtx {
            server: server.clone(),
            started,
            // Cloned, not moved: the cold gate below still keys the lane
            // on the session.
            session_id: session_id.clone(),
            ping,
            requested_model: model,
            effective_model,
            drift,
            shape: Some(shape),
            system_messages,
        });
    }

    // 6. The cold-cache notice (see the module docs). Advisory like the
    // anthropic gate's: once per idle spell, re-armed by activity, and
    // the resend IS the release — there is no marker on this wire.
    // Skipped for unparseable bodies, which have neither a shape nor a
    // lane (the same verdict a keyless miss reaches), and every failure
    // below is "a lost notice, never a lost request": a store error
    // reads as absence and the request forwards.
    let cold_armed = server.config.gates.cold_enabled;
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
        // The openai lane's clock is openrouter's own: sticky sessions
        // expire after 10 minutes of inactivity — not the anthropic
        // 5m/1h tier ladder, so the floor is the provider's window and
        // never the lane's stored tier (a lane shared with anthropic
        // traffic must not be judged on that wire's clock).
        if let cold::ColdDecision::Notice {
            idle_ms, prompt, ..
        } = cold::decide_cold(
            cold_lane.as_ref(),
            // No summarising detection exists on this wire — the openai
            // IR has no compaction shape, so nothing is exempt.
            false,
            gates.cold_min_tokens,
            Some(lanes::OPENAI_LANE_TTL_MS),
            now,
            // No quota outlook: this backend has no meter source, so
            // the decision is coldness + threshold + spell alone.
            None,
        ) {
            // The per-model writes-free exemption: the re-read this
            // notice warns about is what cache writes cost — when the
            // fetched openrouter catalogue says this model's writes are
            // free, the warning buys nothing. Only a POSITIVE verdict
            // exempts; an unknown model never does (conservative: the
            // gate applies). The skip records nothing — it is a debug
            // line, observable without a row per request.
            let writes_free = gate_model.as_deref().is_some_and(|model| {
                server
                    .catalogs
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .cache_writes_free(server.openrouter.id(), model)
                    == Some(true)
            });
            if writes_free {
                tracing::debug!(
                    model = gate_model.as_deref().unwrap_or("?"),
                    "cold gate skipped: the fetched catalogue says this model's cache writes are free"
                );
            } else {
                // The compact model is resolved, not assumed — the same
                // rule the anthropic gate keeps: the notice names the
                // model a cheap `/compact` would actually run on, and
                // stays silent about it when there is none.
                let compact_on = server
                    .models
                    .compaction_target(&compact_spec(gates), prompt, None)
                    .ok()
                    .flatten();
                let text = cold::ColdBlocking::notice(
                    idle_ms,
                    prompt,
                    compact_on.as_deref(),
                    None,
                    now,
                    &jiff::tz::TimeZone::system(),
                    gates.notice_style,
                );
                let body = cold::ColdBlocking::openai_turn(
                    &text,
                    gate_model.as_deref(),
                    stream_requested,
                    now,
                );
                // The lane remembers it has spoken; `at` does not move —
                // the compaction the user runs after reading the notice
                // must still be seen as cold, which is the whole point
                // of the two clocks.
                if let Some(key) = &cold_lane_key
                    && let Err(error) = cold::note_lane_notice(&server.store, key, now)
                {
                    tracing::error!(%error, "lane notice mark failed");
                }
                record_openai_cold(ColdOpenaiRecord {
                    server: &server,
                    started,
                    session_id: session_id.as_deref(),
                    tools_hash: gate_shape.as_ref().map(|shape| shape.tools_hash.as_str()),
                    idle_ms,
                    prompt,
                    // The message count of the stopped request: the
                    // synthetic turn is appended to the client's
                    // transcript, so the next request in this lane
                    // should carry both.
                    req_messages: gate_shape
                        .as_ref()
                        .and_then(|shape| shape.req_messages)
                        .map(|messages| messages as i64),
                    compact_target: compact_on.as_deref(),
                });
                let mut headers = HeaderMap::new();
                headers.insert(
                    header::CONTENT_TYPE,
                    if stream_requested {
                        HeaderValue::from_static("text/event-stream")
                    } else {
                        HeaderValue::from_static("application/json")
                    },
                );
                return build_response(StatusCode::OK, headers, Body::from(body));
            }
        }
    }

    // 7. Upstream; 8.-10. in forward_upstream.
    match send_upstream(
        &server,
        server.openrouter.as_ref(),
        &parts,
        forward,
        // Sticky routing: openrouter consumes the session headers as its
        // cache-affinity key, so they ride upstream, not into the strip
        // list (see send_upstream's docs).
        &[],
    )
    .await
    {
        Ok(upstream) => forward_upstream(upstream, record, in_flight).await,
        Err(error) => {
            // No upstream response: nothing measured, and the error row is
            // provider-response-shaped (status/type/retry-after), so this
            // surfaces as 502 unledgered and logged — never a fabricated
            // provider status. The guard drops here too: the exchange is
            // over, however it ended.
            tracing::warn!(%error, "upstream request failed");
            transport_failure(ErrorWire::Openai, &error)
        }
    }
}

/// `GET /v1/models` — transparent forwarding. The model list is not a
/// usage path: no recording, no observation (plan: the frontend fetches
/// it through the base URL; the simplest correct dogfooding behavior).
/// Every frontend lands here, claude included, so the openrouter
/// provider strips any Anthropic credential the request carries before
/// it leaves (see [`Provider::strip_foreign_credentials`]).
pub(crate) async fn models(State(server): State<Server>, request: Request) -> Response {
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
    match send_upstream(&server, server.openrouter.as_ref(), &parts, body, &[]).await {
        Ok(upstream) => forward_upstream(upstream, None, None).await,
        Err(error) => {
            tracing::warn!(%error, "upstream request failed");
            transport_failure(ErrorWire::Openai, &error)
        }
    }
}

/// Send one request upstream: mapped endpoint, cleaned headers, auth
/// injection, and the (possibly rewritten) body. `provider` decides the
/// endpoint mapping and the credential rules; `session_header_names` is
/// the strip list for [`upstream_request_headers`] — the openai path
/// passes the configured attribution headers, the anthropic path passes
/// an empty slice (the predecessor forwarded claude's session header
/// verbatim).
pub(crate) async fn send_upstream(
    server: &Server,
    provider: &dyn Provider,
    parts: &Parts,
    body: Bytes,
    session_header_names: &[String],
) -> Result<reqwest::Response, reqwest::Error> {
    let path = parts
        .uri
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or_else(|| parts.uri.path());
    let url = provider.endpoint(path);
    // Pass-through-when-present (plan: Credentials): a frontend that
    // brings its own credential keeps it verbatim; the stored credential
    // is injected only when the request carries none. If neither exists
    // the request goes unauthenticated and the upstream's 401 body passes
    // through — visibly verifying the wiring.
    let mut headers = upstream_request_headers(&parts.headers, session_header_names);
    // Before injection, so a dropped foreign credential leaves room for
    // the provider's own.
    provider.strip_foreign_credentials(&mut headers);
    if !provider.credential_present(&parts.headers) {
        provider.inject_auth(&mut headers);
    }
    server
        .http
        .request(parts.method.clone(), url)
        .headers(headers)
        .body(body)
        .send()
        .await
}

/// Forward one upstream response to the client, branching on
/// compression / status / content-type. `record` is the chat-path
/// completion context; `None` means pure transparent forwarding.
/// `in_flight` is the request's sleep-lock hold: it rides the SSE
/// stream (dropping when axum drops the body — the stream-close
/// semantics, "however the exchange ends") and drops at the end of this
/// function on every
/// other branch, after whatever row was owed has landed.
pub(crate) async fn forward_upstream(
    upstream: reqwest::Response,
    record: Option<RecordCtx>,
    in_flight: Option<InFlightGuard>,
) -> Response {
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();

    // Unexpected compression — shouldn't happen, identity is forced —
    // passes through untouched with no recording (ledger-proxy behavior:
    // never mis-parse a compressed stream).
    if is_compressed(&upstream_headers) {
        tracing::debug!("compressed upstream response passed through unledgered");
        let body = Body::from_stream(upstream.bytes_stream());
        return build_response(status, response_headers(&upstream_headers, true), body);
    }

    let Some(ctx) = record else {
        // Transparent forwarding (models, non-JSON chat bodies): stream
        // through; nothing observed, nothing recorded.
        let body = Body::from_stream(upstream.bytes_stream());
        return build_response(status, response_headers(&upstream_headers, false), body);
    };

    // Non-2xx on a usage path: error row (status, type, retry-after —
    // never priced), body forwarded unchanged.
    if !status.is_success() {
        let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
            return truncated_body(ErrorWire::Openai);
        };
        let error_type = parse_error_type(&buffered.bytes);
        let retry_after = retry_after_ms(&upstream_headers);
        record_error(&ctx, status.as_u16(), error_type, retry_after);
        let body = buffered_body(buffered);
        return build_response(status, response_headers(&upstream_headers, false), body);
    }

    // SSE: chunks stream through with backpressure (axum Body from a
    // stream), each also feeding the side observation.
    if is_event_stream(&upstream_headers) {
        let stream = ObservedStream::new(upstream, status.as_u16(), ctx, in_flight);
        let body = Body::from_stream(stream);
        return build_response(status, response_headers(&upstream_headers, false), body);
    }

    // Non-SSE: buffer, observe, forward the original bytes unchanged.
    let Ok(buffered) = buffer_up_to(upstream, MAX_RESPONSE_BUFFER).await else {
        return truncated_body(ErrorWire::Openai);
    };
    if buffered.rest.is_some() {
        tracing::warn!("non-streaming response exceeded the buffer cap; passed through unledgered");
        let body = buffered_body(buffered);
        return build_response(status, response_headers(&upstream_headers, false), body);
    }
    let mut observer = UsageObserver::new();
    observer.observe_json(&buffered.bytes);
    let capture = observer.finish();
    record_measurement(&ctx, capture.as_ref(), status.as_u16());
    let body = Body::from(Bytes::from(buffered.bytes));
    build_response(status, response_headers(&upstream_headers, false), body)
}

/// Phase-1 routing (plan: Routing): an `openrouter/` prefix overrides the
/// backend per request and is stripped from the model. Anything else —
/// bare names, other providers' prefixes — goes to the protocol's
/// configured default backend, untransformed. Generalising the prefix set
/// is a later phase's work.
pub(crate) fn strip_provider_prefix(model: &str) -> Option<&str> {
    model.strip_prefix("openrouter/")
}

/// The compaction retarget's model spec — the mirror of the anthropic
/// path's (`crate::server::anthropic`): a family name resolved against
/// what is actually in use (the default, "sonnet"), an explicit model
/// id, or "off". The notice names a `/compact` target only when one
/// resolves; an unarmed proxy promising a cheap compaction would be the
/// feature lying about its own configuration.
fn compact_spec(gates: &crate::config::GatesConfig) -> String {
    gates
        .compact_model
        .clone()
        .unwrap_or_else(|| "sonnet".to_owned())
}

/// The session identity from the configured header names, in priority
/// order, read by name only (invariant 2).
pub(crate) fn session_id(names: &[String], headers: &HeaderMap) -> Option<String> {
    for name in names {
        if let Some(value) = headers
            .get(name.as_str())
            .and_then(|value| value.to_str().ok())
        {
            return Some(value.to_owned());
        }
    }
    None
}

/// Connection-specific headers that must never cross a proxy hop.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// The headers for the upstream request: the client's headers minus
/// hop-by-hop, minus `host`/`content-length` (the transport re-frames),
/// minus `accept-encoding` (identity is forced — the SSE observation needs
/// plaintext; ledger-proxy lesson), minus toker's own attribution and
/// control headers (addressed to the proxy, not the provider), minus the
/// configured session-attribution header names.
///
/// The session-name strip is the caller's choice: the openai path strips
/// them all, while the anthropic path passes `&[]` — row parity, since
/// claude's `x-claude-code-session-id` is forwarded verbatim by the proxy
/// toker replaces and the upstream already receives it in production.
/// `x-toker-*` is stripped unconditionally either way: those headers are
/// addressed to the proxy, never the provider.
pub(crate) fn upstream_request_headers(
    incoming: &HeaderMap,
    session_header_names: &[String],
) -> HeaderMap {
    let mut outgoing = HeaderMap::with_capacity(incoming.len());
    for (name, value) in incoming {
        let name_str = name.as_str();
        if is_hop_by_hop(name)
            || name == header::HOST
            || name == header::CONTENT_LENGTH
            || name == header::ACCEPT_ENCODING
            || name_str.starts_with("x-toker-")
            || session_header_names
                .iter()
                .any(|session| session.eq_ignore_ascii_case(name_str))
        {
            continue;
        }
        outgoing.append(name, value.clone());
    }
    outgoing.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    outgoing
}

/// The headers for the client-facing response: the upstream's minus
/// hop-by-hop, minus `content-length` (the transport re-frames the
/// body), minus `content-encoding` — unless `keep_content_encoding`, for
/// the untouched compressed-passthrough branch, where the bytes are
/// verbatim and the encoding must stay.
pub(crate) fn response_headers(upstream: &HeaderMap, keep_content_encoding: bool) -> HeaderMap {
    let mut outgoing = HeaderMap::with_capacity(upstream.len());
    for (name, value) in upstream {
        if is_hop_by_hop(name)
            || name == header::CONTENT_LENGTH
            || (!keep_content_encoding && name == header::CONTENT_ENCODING)
        {
            continue;
        }
        outgoing.append(name, value.clone());
    }
    outgoing
}

pub(crate) fn is_compressed(headers: &HeaderMap) -> bool {
    match headers
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
    {
        Some(encoding) => !encoding.trim().eq_ignore_ascii_case("identity"),
        None => false,
    }
}

pub(crate) fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.starts_with("text/event-stream"))
}

/// A partial buffer of a response body: `rest` is `Some` when the cap
/// overflowed and the response (positioned after `bytes`) remains, so the
/// caller can forward the remainder verbatim.
pub(crate) struct Buffered {
    pub(crate) bytes: Vec<u8>,
    pub(crate) rest: Option<reqwest::Response>,
}

/// Buffer a response body up to `cap` bytes. An upstream failure
/// mid-transfer (a reset, the idle timeout) is an error, not a short
/// buffer: these bytes were once forwarded as a whole body under the
/// upstream's status, so a truncated turn reached the client as a
/// complete one, and a client that accepts it never retries.
pub(crate) async fn buffer_up_to(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<Buffered, reqwest::Error> {
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len() + chunk.len() > cap {
                    return Ok(Buffered {
                        bytes,
                        rest: Some(response),
                    });
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(Buffered { bytes, rest: None }),
            Err(error) => {
                tracing::warn!(%error, "upstream response body failed mid-transfer");
                return Err(error);
            }
        }
    }
}

/// Which wire a proxy-generated failure answers on: the error has to be
/// in the shape the route's own clients parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorWire {
    /// The Messages routes: `{"type":"error","error":{…}}`.
    Anthropic,
    /// The chat-completions routes: `{"error":{…}}`.
    Openai,
}

/// The 502 toker answers when the upstream gave it nothing to forward —
/// the request never got a response, or the response died before its
/// body was whole. A JSON error in the route's own wire shape, with the
/// content-type set: a bare text body under a 502 reached claude as an
/// unparseable error, where the predecessor answered an anthropic-shaped
/// `api_error` the client already knows how to report and retry. No row:
/// the status is toker's own, never a fabricated provider measurement.
pub(crate) fn upstream_failure(wire: ErrorWire, message: &str) -> Response {
    let body = match wire {
        ErrorWire::Anthropic => serde_json::json!({
            "type": "error",
            "error": { "type": "api_error", "message": message },
        }),
        ErrorWire::Openai => serde_json::json!({
            "error": {
                "message": message,
                "type": "server_error",
                "param": null,
                "code": null,
            },
        }),
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    build_response(
        StatusCode::BAD_GATEWAY,
        headers,
        Body::from(serde_json::to_vec(&body).unwrap_or_default()),
    )
}

/// The 502 for a request that never got an upstream response (a refused
/// connection, the read timeout before the headers). The transport's own
/// error names what failed; it carries the URL but no credential, which
/// travels in headers (invariant 2).
pub(crate) fn transport_failure(wire: ErrorWire, error: &dyn std::fmt::Display) -> Response {
    upstream_failure(wire, &format!("toker upstream error: {error}"))
}

/// The answer when [`buffer_up_to`] failed: the body never arrived
/// whole, so nothing of it is forwarded and no row is written — the
/// buffered mirror of an aborted stream.
pub(crate) fn truncated_body(wire: ErrorWire) -> Response {
    upstream_failure(
        wire,
        "toker upstream error: the response failed before its body was complete",
    )
}

/// The response body for a buffered-then-maybe-overflowed response: the
/// buffered prefix chained onto the remaining upstream stream, byte order
/// preserved.
pub(crate) fn buffered_body(buffered: Buffered) -> Body {
    match buffered.rest {
        Some(response) => {
            let prefix = Ok::<_, reqwest::Error>(Bytes::from(buffered.bytes));
            Body::from_stream(futures::stream::iter(vec![prefix]).chain(response.bytes_stream()))
        }
        None => Body::from(Bytes::from(buffered.bytes)),
    }
}

/// The upstream body stream type, boxed so [`ObservedStream`] can name it.
pub(crate) type UpstreamBody = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// The SSE response stream with the usage observation riding alongside:
/// bytes pass through verbatim (backpressured by axum's poll-driven body),
/// and each chunk *also* feeds the splitter/observer. Observation is a
/// side effect that can never fail the stream (invariant 6): the observe
/// APIs are infallible by construction, and the calls additionally run
/// under [`std::panic::catch_unwind`] so no observation bug can take a
/// live session down — the measurement is lost, not the response.
struct ObservedStream {
    /// The upstream body, wrapped [`Abortable`] so the handle below can
    /// stop it.
    inner: Pin<Box<Abortable<UpstreamBody>>>,
    /// Fires in [`Drop`]: when axum drops the response body — client
    /// hangup, shutdown — the upstream request is aborted too.
    abort: AbortHandle,
    splitter: SseSplitter,
    observer: UsageObserver,
    /// The recording context, taken at completion: only a completed
    /// stream records (a hung-up one records nothing, plan: Server core).
    ctx: Option<RecordCtx>,
    /// The request's sleep-lock hold, riding the stream: it drops when
    /// axum drops the body — natural completion or client hangup — so the
    /// in-flight count never leaks on a streamed response (the
    /// body-close event).
    in_flight: Option<InFlightGuard>,
    status: u16,
}

impl ObservedStream {
    fn new(
        response: reqwest::Response,
        status: u16,
        ctx: RecordCtx,
        in_flight: Option<InFlightGuard>,
    ) -> ObservedStream {
        let (abort, registration) = AbortHandle::new_pair();
        let stream: UpstreamBody = Box::pin(response.bytes_stream());
        ObservedStream {
            inner: Box::pin(Abortable::new(stream, registration)),
            abort,
            splitter: SseSplitter::new(),
            observer: UsageObserver::new(),
            ctx: Some(ctx),
            in_flight,
            status,
        }
    }
}

impl Stream for ObservedStream {
    /// The upstream's own error type: an upstream failure is yielded, not
    /// swallowed, so hyper aborts the client's response instead of
    /// terminating it cleanly. A cleanly ended stream once let a
    /// connection reset mid-turn reach the client as a complete
    /// (truncated) turn, which it accepted rather than retried.
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
                // Yielded, so the client sees a transport error (the
                // predecessor destroyed the response), never a clean end.
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
                    record_measurement(&ctx, capture.as_ref(), this.status);
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

impl Drop for ObservedStream {
    fn drop(&mut self) {
        // Client hangup → axum drops the body → abort the upstream. Also
        // fires after natural completion, where it is a no-op.
        self.abort.abort();
    }
}

/// Feed one chunk to the side observation, panic-guarded (invariant 6:
/// accounting must never break a session — a lost measurement is the worst
/// outcome, never a lost response).
fn observe_chunk(splitter: &mut SseSplitter, observer: &mut UsageObserver, chunk: &[u8]) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        for event in splitter.feed(chunk) {
            observer.observe_event(&event);
        }
    }));
}

/// Build a response with an explicit status and header set.
pub(crate) fn build_response(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// A minimal status-page response for proxy-level failures.
pub(crate) fn plain_status(status: StatusCode, message: &'static str) -> Response {
    let mut response = Response::new(Body::from(message));
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::{ErrorWire, strip_provider_prefix, transport_failure};
    use axum::http::{StatusCode, header};

    #[test]
    fn the_openrouter_prefix_strips_and_everything_else_goes_to_the_default() {
        assert_eq!(
            strip_provider_prefix("openrouter/z-ai/glm-5.3"),
            Some("z-ai/glm-5.3"),
            "the provider prefix routes and strips"
        );
        // Nested provider models survive intact — only toker's own prefix
        // is toker's to strip.
        assert_eq!(
            strip_provider_prefix("openrouter/openai/gpt-5.2"),
            Some("openai/gpt-5.2")
        );
        assert_eq!(strip_provider_prefix("openrouter/"), Some(""));
        // Bare models route to the protocol default, untransformed.
        assert_eq!(strip_provider_prefix("z-ai/glm-5.3"), None);
        // Other providers' prefixes are not toker's to intercept in
        // phase 1: they go to the default backend unchanged.
        assert_eq!(strip_provider_prefix("anthropic/claude-opus-5"), None);
    }

    async fn error_of(response: axum::response::Response) -> serde_json::Value {
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .expect("a whole body");
        serde_json::from_slice(&body).expect("a JSON error")
    }

    #[tokio::test]
    async fn a_transport_failure_answers_in_the_route_s_own_error_shape() {
        let anthropic = error_of(transport_failure(
            ErrorWire::Anthropic,
            &"connection refused",
        ))
        .await;
        assert_eq!(
            anthropic,
            serde_json::json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": "toker upstream error: connection refused",
                },
            })
        );
        let openai = error_of(transport_failure(ErrorWire::Openai, &"connection refused")).await;
        assert_eq!(
            openai,
            serde_json::json!({
                "error": {
                    "message": "toker upstream error: connection refused",
                    "type": "server_error",
                    "param": null,
                    "code": null,
                },
            })
        );
    }
}
