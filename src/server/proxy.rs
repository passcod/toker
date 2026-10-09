//! The proxy routes: chat completions (the usage path, fully recorded) and
//! locally projected model discovery.
//!
//! Chat completions, in order (plan: Server core):
//!
//! 1. Buffer the request body fully.
//! 2. Parse the frontend wire into canonical IR. An invalid Chat body gets a
//!    typed local compatibility error and never reaches a backend.
//! 3. Routing: a known provider prefix selects that backend and is
//!    stripped; bare models go to the protocol default.
//! 4. The selected backend adapter renders the request deterministically.
//! 5. **The cold-cache notice** (plan: Middleware — cold gate): the openai
//!    path's own gate, on the lane the request itself keys (session ×
//!    tools-hash) and the post-routing model. A summarising request is
//!    exempt, as on the anthropic path. No quota outlook is applied on the
//!    Chat path yet. A per-model writes-free exemption applies to
//!    OpenRouter: when its fetched catalogue says the model's cache writes cost
//!    nothing, the re-read the notice warns about is free, and the
//!    withheld notice is a `cold-quiet` row. On fire: 200 with a
//!    synthetic openai turn naming no compaction target (this path never
//!    retargets one), a `cold` row, the lane marked noticed — never an
//!    error status; the resend IS the release (there is no marker on this
//!    wire).
//! 6. Upstream request with hop-by-hop headers stripped,
//!    `accept-encoding: identity` forced (SSE observation needs plaintext),
//!    and the stored credential injected only when the incoming request
//!    carries no Authorization of its own (pass-through-when-present).
//!    Another provider's credential is dropped first, never forwarded.
//! 7. A client hangup aborts the upstream (the body stream's Drop fires
//!    an [`AbortHandle`]); a hung-up stream records no row.
//! 8. The backend response is interpreted into canonical events or a complete
//!    turn, then rendered back to Chat. Provider bytes feed accounting before
//!    translation, preserving billed cost and raw usage evidence.
//! 9. Recording on completion only — [`record::RecordCtx`] → row, plus
//!    the lane-table note (the openai lane's clock is openrouter's
//!    10-minute sticky window, [`lanes::OPENAI_LANE_TTL_MS`]).

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::response::Response;
use bytes::Bytes;
use futures::future::{AbortHandle, Abortable};
use futures::stream::{Stream, StreamExt};

use crate::catalog::offers;
use crate::ir::{Request as IrRequest, Shape};
use crate::middleware::cold;
use crate::middleware::force_newest;
use crate::middleware::lanes;
use crate::observe::{SseEvent, SseSplitter, UsageObserver};
use crate::providers::Provider;
use crate::routing::{BackendAdapterId, ProtocolId};
use crate::translate::{self, OpenAiChatRenderer};

use super::InFlightGuard;
use super::Server;
use super::record::{
    ColdOpenaiRecord, RecordCtx, now_ms, parse_error_type, record_error, record_measurement,
    record_openai_cold, record_openai_cold_quiet, retry_after_ms,
};
use super::record_anthropic::AnthropicRecordCtx;

/// Request bodies are buffered for gating and canonical translation; 64 MiB
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
    let Ok(mut target) = server.registry.resolve(ProtocolId::OpenAiChat, None) else {
        return super::openai_not_configured();
    };
    let (parts, body) = request.into_parts();

    // Session identity, read by name only — request headers are never
    // captured wholesale: they carry credentials (invariant 2).
    let session_id = session_id(&server.config.session_header_names, &parts.headers);
    // Ping tagging (plan: Middleware): a lane whose request carried the
    // ping header is recorded but excluded from liveness — the window
    // pinger's probe must never hold the sleep lock, on this path like
    // the anthropic one.
    let ping = lanes::is_ping(&parts.headers, &server.config.ping_header_name);
    // The frontend's name from its base-URL prefix: picks the notice's
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

    // 2.-5. Parse and route. Every supported Chat backend renders from
    // canonical IR, including the same-protocol OpenRouter binding.
    let mut forward = original.clone();
    let mut record = None;
    // The request's own shape and ask, kept past the record context: the
    // cold gate keys the lane on the session × tools-hash the request
    // itself carries, answers in the wire form the request asked for,
    // and exempts by the post-routing model.
    let mut gate_shape: Option<Shape> = None;
    let mut stream_requested = false;
    let mut gate_model: Option<String> = None;
    let mut parsed_for_codex = None;
    if let Ok(mut ir) = IrRequest::parse(&original) {
        let model = ir.openai_chat().model().map(str::to_owned);
        target = match server
            .registry
            .resolve(ProtocolId::OpenAiChat, model.as_deref())
        {
            Ok(target) => target,
            Err(_) => return super::openai_not_configured(),
        };
        let effective_model = target.effective_model().map(str::to_owned);
        if effective_model.as_deref() != model.as_deref()
            && let Some(effective) = effective_model.as_deref()
        {
            ir.openai_chat_mut().set_model(effective);
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
            frontend: frontend.clone(),
            server: server.clone(),
            started,
            // Cloned, not moved: the cold gate below still keys the lane
            // on the session.
            session_id: session_id.clone(),
            ping,
            requested_model: model,
            effective_model,
            shape: Some(shape),
            system_messages,
        });
        parsed_for_codex = Some(ir);
    }

    let backend = target.provider().clone();

    if target
        .binding()
        .canonical_backend()
        .is_some_and(|binding| binding.adapter() == BackendAdapterId::OpenAiChatCompletions)
    {
        let Some(ir) = parsed_for_codex.as_ref() else {
            return compatibility_error(
                "the request body could not be parsed as an OpenAI Chat request",
            );
        };
        let rendered = translate::from_openai_chat(ir.value()).and_then(|mut canonical| {
            canonical.model.clone_from(&gate_model);
            translate::openai_chat_backend::render_openai_chat(
                &canonical,
                target.binding().dialect(),
            )
        });
        let rendered = match rendered {
            Ok(rendered) => rendered,
            Err(error) => return compatibility_error(&error.to_string()),
        };
        forward = match serde_json::to_vec(&rendered.value) {
            Ok(body) => Bytes::from(body),
            Err(error) => {
                tracing::error!(%error, "canonical chat request serialisation failed");
                return plain_status(StatusCode::INTERNAL_SERVER_ERROR, "translation failed\n");
            }
        };
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
            // A summarising request forwards, as on the anthropic path:
            // the notice exists to advise compacting, and stopping the
            // compaction would halt the user a keystroke after telling
            // them to go ahead.
            if gate_shape.as_ref().is_some_and(|shape| shape.summarising) {
                cold::Turn::Summarising
            } else {
                cold::Turn::Ordinary
            },
            gates.cold_min_tokens,
            Some(lanes::OPENAI_LANE_TTL_MS),
            now,
            // No quota outlook: this backend has no meter source, so
            // the decision is coldness + threshold + spell alone.
            None,
            Some(force_newest::prompt_bound(original.len() as u64)),
        ) {
            // The per-model writes-free exemption: the re-read this
            // notice warns about is what cache writes cost — when the
            // fetched openrouter catalogue says this model's writes are
            // free, the warning buys nothing. Only a POSITIVE verdict
            // exempts; an unknown model never does (conservative: the
            // gate applies).
            let writes_free = backend.id() == "openrouter"
                && gate_model.as_deref().is_some_and(|model| {
                    server
                        .catalogs
                        .read()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .cache_writes_free(backend.id(), model)
                        == Some(true)
                });
            if writes_free {
                // Withheld, and recorded as the anthropic path records it:
                // a `cold-quiet` row saying why, so silence reads as a
                // decision rather than a gate that stopped working. `at`
                // and `noticed_at` are untouched — nothing was said and
                // nothing reached upstream.
                record_openai_cold_quiet(ColdOpenaiRecord {
                    frontend: frontend.as_deref(),
                    server: &server,
                    started,
                    session_id: session_id.as_deref(),
                    tools_hash: gate_shape.as_ref().map(|shape| shape.tools_hash.as_str()),
                    idle_ms,
                    prompt,
                    req_messages: None,
                    compact_target: None,
                });
            } else {
                // No compaction target is named: this path never
                // retargets a compaction, so a `/compact` here runs on
                // whatever the client sends, and promising a cheaper one
                // would be the notice lying about what the proxy does.
                // And openrouter bills the re-read rather than metering
                // it against a rate-limit window, so the notice does not
                // say it does.
                let text = cold::ColdBlocking::notice_for(
                    // Openrouter's write multiple is per model in its
                    // catalogue; not read here, so left unsaid.
                    None,
                    idle_ms,
                    prompt,
                    None,
                    None,
                    now,
                    &jiff::tz::TimeZone::system(),
                    server.config.notices.style_for(frontend.as_deref()),
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
                    frontend: frontend.as_deref(),
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
                    compact_target: None,
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

    if target
        .binding()
        .canonical_backend()
        .is_some_and(|binding| binding.adapter() == BackendAdapterId::CodexResponses)
    {
        let record = record.map(|ctx| chat_codex_record(ctx, backend.clone()));
        return super::codex::turn(super::codex::CodexTurn {
            server,
            backend,
            parsed: parsed_for_codex,
            gate_shape: None,
            record,
            in_flight,
            session_id,
            thread_id: None,
            request_id: None,
            served_model: gate_model,
            stream_explicitly_false: !stream_requested,
            frontend_wire: super::codex::CodexFrontendWire::OpenAiChat,
        })
        .await;
    }

    // 7. Upstream; 8.-10. in forward_upstream.
    match send_upstream(
        &server,
        backend.as_ref(),
        &parts,
        forward,
        // Sticky routing: openrouter consumes the session headers as its
        // cache-affinity key, so they ride upstream, not into the strip
        // list (see send_upstream's docs).
        &[],
    )
    .await
    {
        Ok(upstream) => {
            forward_openrouter_canonical(
                upstream,
                record,
                in_flight,
                gate_model.as_deref().unwrap_or_default(),
            )
            .await
        }
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

/// `GET /v1/models` is a local route-graph projection for known OpenAI
/// profiles. An unknown or Anthropic profile retains path-driven legacy
/// forwarding. No model catalogue request is a usage path or ledger row.
pub(crate) async fn models(State(server): State<Server>, request: Request) -> Response {
    let profile = super::frontend_profile_of(request.extensions()).and_then(|p| p.protocol());
    if let Some(frontend @ (ProtocolId::OpenAiChat | ProtocolId::OpenAiResponses)) = profile {
        if server.registry.resolve(frontend, None).is_err() {
            return match frontend {
                ProtocolId::OpenAiChat => super::openai_not_configured(),
                ProtocolId::OpenAiResponses => super::responses_not_configured(),
                ProtocolId::AnthropicMessages => unreachable!(),
            };
        }
        let catalogs = server
            .catalogs
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let models = offers::for_frontend(&server.registry, &catalogs, frontend);
        if models.is_empty() {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": {
                        "type": "catalog_unavailable",
                        "message": "No provider model catalogue is available yet"
                    }
                })),
            )
                .into_response();
        }
        let body = match frontend {
            ProtocolId::OpenAiChat => offers::render_chat(&models),
            ProtocolId::OpenAiResponses => offers::render_responses(&models),
            ProtocolId::AnthropicMessages => unreachable!(),
        };
        return axum::Json(body).into_response();
    }
    let Some(openrouter) = server.registry.provider("openrouter").cloned() else {
        return super::anthropic::unmatched(State(server), request).await;
    };
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
    match send_upstream(&server, openrouter.as_ref(), &parts, body, &[]).await {
        Ok(upstream) => forward_upstream(upstream).await,
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
    send_upstream_to(
        server,
        provider,
        parts,
        body,
        session_header_names,
        ProtocolId::AnthropicMessages,
        path,
    )
    .await
}

/// The same provider-owned auth and transport path for a cross-protocol
/// binding. `frontend` lets a provider distinguish a native credential from
/// a different client's bearer; `path` is the selected backend endpoint.
pub(crate) async fn send_upstream_to(
    server: &Server,
    provider: &dyn Provider,
    parts: &Parts,
    body: Bytes,
    session_header_names: &[String],
    frontend: ProtocolId,
    path: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = provider.endpoint(path);
    // Pass-through-when-present (plan: Credentials): a frontend that
    // brings its own credential keeps it verbatim; the stored credential
    // is injected only when the request carries none. If neither exists
    // the request goes unauthenticated and the upstream's 401 body passes
    // through — visibly verifying the wiring.
    let mut headers = upstream_request_headers(&parts.headers, session_header_names);
    // Before injection, so a dropped foreign credential leaves room for
    // the provider's own.
    provider.strip_foreign_credentials_for(&mut headers, frontend);
    if !provider.credential_present(&headers) {
        provider.inject_auth(&mut headers);
    }
    provider.prepare_protocol_headers(&mut headers);
    server
        .http
        .request(parts.method.clone(), url)
        .headers(headers)
        .body(body)
        .send()
        .await
}

/// Transparently forward one non-usage response such as a model catalogue.
pub(crate) async fn forward_upstream(upstream: reqwest::Response) -> Response {
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

    // Catalogue and other non-usage paths remain transparent: no canonical
    // inference adapter, observation, or recording applies to them.
    let body = Body::from_stream(upstream.bytes_stream());
    build_response(status, response_headers(&upstream_headers, false), body)
}

/// The OpenRouter Chat binding's live universal-canonical response path.
/// Accounting observes the provider bytes before translation, so billed cost
/// and verbatim usage remain provider evidence rather than renderer output.
async fn forward_openrouter_canonical(
    upstream: reqwest::Response,
    record: Option<RecordCtx>,
    in_flight: Option<InFlightGuard>,
    model: &str,
) -> Response {
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    if is_compressed(&upstream_headers) {
        tracing::warn!("compressed canonical chat response could not be interpreted");
        return upstream_failure(
            ErrorWire::Openai,
            "toker could not interpret a compressed upstream response",
        );
    }

    if !status.is_success() {
        let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
            return truncated_body(ErrorWire::Openai);
        };
        if buffered.rest.is_some() {
            return upstream_failure(
                ErrorWire::Openai,
                "toker could not interpret an oversized upstream error",
            );
        }
        if let Some(ctx) = record.as_ref() {
            record_error(
                ctx,
                status.as_u16(),
                parse_error_type(&buffered.bytes),
                retry_after_ms(&upstream_headers),
            );
        }
        let rendered = serde_json::from_slice::<serde_json::Value>(&buffered.bytes)
            .ok()
            .and_then(|body| {
                translate::openai_chat_backend::canonical_turn_from_openai_chat(&body).ok()
            })
            .map(|turn| translate::openai_chat_from_canonical(model, &turn));
        let Some(rendered) = rendered else {
            return upstream_failure(
                ErrorWire::Openai,
                "toker could not interpret the upstream error response",
            );
        };
        return json_response(status, &upstream_headers, &rendered);
    }

    if is_event_stream(&upstream_headers) {
        let stream = CanonicalChatStream::new(upstream, status.as_u16(), record, in_flight, model);
        let mut headers = response_headers(&upstream_headers, false);
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        return build_response(status, headers, Body::from_stream(stream));
    }

    let Ok(buffered) = buffer_up_to(upstream, MAX_RESPONSE_BUFFER).await else {
        return truncated_body(ErrorWire::Openai);
    };
    if buffered.rest.is_some() {
        return upstream_failure(
            ErrorWire::Openai,
            "toker could not interpret an oversized upstream response",
        );
    }
    let body = match serde_json::from_slice::<serde_json::Value>(&buffered.bytes)
        .map_err(|error| error.to_string())
        .and_then(|body| {
            translate::openai_chat_backend::canonical_turn_from_openai_chat(&body)
                .map_err(|error| error.to_string())
        }) {
        Ok(turn) => translate::openai_chat_from_canonical(model, &turn),
        Err(error) => {
            tracing::warn!(%error, "canonical chat response interpretation failed");
            return upstream_failure(
                ErrorWire::Openai,
                "toker could not interpret the upstream response",
            );
        }
    };
    if let Some(ctx) = record.as_ref() {
        let mut observer = UsageObserver::new();
        observer.observe_json(&buffered.bytes);
        let capture = observer.finish();
        record_measurement(ctx, capture.as_ref(), status.as_u16());
    }
    json_response(status, &upstream_headers, &body)
}

fn json_response(
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

fn compatibility_error(message: &str) -> Response {
    error_response(
        ErrorWire::Openai,
        StatusCode::BAD_REQUEST,
        WireError {
            anthropic_type: "invalid_request_error",
            openai_type: "invalid_request_error",
            openai_code: None,
        },
        message,
    )
}

struct CanonicalChatStream {
    inner: Pin<Box<Abortable<UpstreamBody>>>,
    abort: AbortHandle,
    splitter: SseSplitter,
    backend: translate::openai_chat_backend::OpenAiChatResponseStream,
    frontend: OpenAiChatRenderer,
    observer: UsageObserver,
    pending: VecDeque<Bytes>,
    upstream_done: bool,
    ctx: Option<RecordCtx>,
    in_flight: Option<InFlightGuard>,
    status: u16,
}

impl CanonicalChatStream {
    fn new(
        response: reqwest::Response,
        status: u16,
        ctx: Option<RecordCtx>,
        in_flight: Option<InFlightGuard>,
        model: &str,
    ) -> CanonicalChatStream {
        let (abort, registration) = AbortHandle::new_pair();
        let stream: UpstreamBody = Box::pin(response.bytes_stream());
        CanonicalChatStream {
            inner: Box::pin(Abortable::new(stream, registration)),
            abort,
            splitter: SseSplitter::new(),
            backend: translate::openai_chat_backend::OpenAiChatResponseStream::new(),
            frontend: OpenAiChatRenderer::new(model),
            observer: UsageObserver::new(),
            pending: VecDeque::new(),
            upstream_done: false,
            ctx,
            in_flight,
            status,
        }
    }

    fn translate_event(&mut self, event: SseEvent) {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.observer.observe_event(&event);
        }));
        let canonical = self.backend.feed_sse(&event);
        for emitted in canonical.iter().flat_map(|event| self.frontend.feed(event)) {
            self.pending.push_back(sse_bytes(&emitted));
        }
    }

    fn finish_translation(&mut self) {
        if let Some(event) = self.splitter.finish() {
            self.translate_event(event);
        }
        let canonical = self.backend.finish();
        for emitted in canonical.iter().flat_map(|event| self.frontend.feed(event)) {
            self.pending.push_back(sse_bytes(&emitted));
        }
        self.upstream_done = true;
    }

    fn finish_recording(&mut self) {
        if let Some(ctx) = self.ctx.take() {
            let capture = std::mem::take(&mut self.observer).finish();
            record_measurement(&ctx, capture.as_ref(), self.status);
        }
        drop(self.in_flight.take());
    }
}

impl Stream for CanonicalChatStream {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(bytes) = this.pending.pop_front() {
                return std::task::Poll::Ready(Some(Ok(bytes)));
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
                    tracing::warn!(%error, "upstream canonical chat stream failed");
                    this.ctx.take();
                    drop(this.in_flight.take());
                    return std::task::Poll::Ready(Some(Err(error)));
                }
                std::task::Poll::Ready(None) => this.finish_translation(),
            }
        }
    }
}

impl Drop for CanonicalChatStream {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

fn sse_bytes(event: &SseEvent) -> Bytes {
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

fn chat_codex_record(ctx: RecordCtx, backend: Arc<dyn Provider>) -> AnthropicRecordCtx {
    let shape = ctx.shape.map(|shape| crate::ir::AnthropicShape {
        req_bytes: shape.req_bytes,
        req_messages: shape.req_messages,
        req_tools: shape.req_tools,
        tools_hash: shape.tools_hash,
        system_chars: shape.system_chars,
        system_hash: shape.system_hash,
        system_blocks: shape
            .system_blocks
            .into_iter()
            .map(|block| crate::ir::SystemBlockDigest {
                chars: block.chars,
                hash: block.hash,
            })
            .collect(),
        system_messages: ctx.system_messages,
        compact_generations: None,
        summarising: shape.summarising,
        compact_marker: None,
        recap: false,
        system_ladder: Vec::new(),
        system_tail: Vec::new(),
    });
    AnthropicRecordCtx {
        server: ctx.server,
        started: ctx.started,
        path: "/v1/chat/completions",
        session_id: ctx.session_id,
        requested_model: ctx.requested_model,
        effective_model: ctx.effective_model,
        backend,
        betas: None,
        shape,
        ping: ctx.ping,
        downgraded_from: None,
        downgraded_to: None,
        cache_stripped: None,
        system_merged: None,
        forced_from: None,
        forced_to: None,
        model_mappings: None,
        frontend: ctx.frontend,
        thinking_rewritten: false,
        translation_report: None,
    }
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
    error_response(
        wire,
        StatusCode::BAD_GATEWAY,
        WireError {
            anthropic_type: "api_error",
            openai_type: "server_error",
            openai_code: None,
        },
        message,
    )
}

/// One proxy-generated error's type names, per wire: anthropic's
/// `error.type`, and openai's `error.type` plus its optional `code`.
pub(crate) struct WireError {
    pub(crate) anthropic_type: &'static str,
    pub(crate) openai_type: &'static str,
    pub(crate) openai_code: Option<&'static str>,
}

/// A JSON error toker answers itself, in the route's own wire shape with
/// the content-type set, so the client reports the message rather than
/// failing to parse one.
pub(crate) fn error_response(
    wire: ErrorWire,
    status: StatusCode,
    kind: WireError,
    message: &str,
) -> Response {
    let body = match wire {
        ErrorWire::Anthropic => serde_json::json!({
            "type": "error",
            "error": { "type": kind.anthropic_type, "message": message },
        }),
        ErrorWire::Openai => serde_json::json!({
            "error": {
                "message": message,
                "type": kind.openai_type,
                "param": null,
                "code": kind.openai_code,
            },
        }),
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    build_response(
        status,
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

/// The upstream body stream type, boxed so the canonical stream adapter can
/// own it while remaining a named [`Stream`].
pub(crate) type UpstreamBody = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

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
    use super::{ErrorWire, transport_failure};
    use axum::http::{StatusCode, header};

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
