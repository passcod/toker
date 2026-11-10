//! The proxy routes: chat completions (the usage path, fully recorded) and
//! locally projected model discovery.
//!
//! Chat completions, in order:
//!
//! 1. Buffer the request body fully.
//! 2. Parse the frontend wire into canonical IR. An invalid Chat body gets a
//!    typed local compatibility error and never reaches a backend.
//! 3. Routing: the active policy revision resolves the model to a binding
//!    and the effective model to send.
//! 4. The selected backend adapter renders the request deterministically.
//! 5. **The cold-cache notice**: the
//!    OpenRouter Chat binding's gate, on the lane the request itself keys (session ×
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
//!    an OpenRouter lane-table note (that binding's clock is its
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
use crate::ir::Shape;
use crate::ir::canonical::{CanonEvent, CanonRole};
use crate::middleware::cold;
use crate::middleware::lanes;
use crate::observe::{SseEvent, SseSplitter, UsageObserver};
use crate::policy::force_newest;
use crate::providers::{Provider, credential_digest};
use crate::routing::ProtocolId;
use crate::store::CredentialDigest;
use crate::translate::{self, OpenAiChatRenderer};

use super::InFlightGuard;
use super::Server;
use super::record::{
    ColdOpenaiRecord, RecordCtx, RefusalCtx, now_ms, parse_error_type, record_error,
    record_measurement, record_openai_cold, record_openai_cold_quiet, record_unroutable, refused,
    retry_after_ms,
};
use super::record_anthropic::{
    AnthropicRecordCtx, QuotaBlockedRecord, record_anthropic_released, record_quota_blocked,
};

/// Request bodies are buffered for gating and canonical translation; 64 MiB
/// is far beyond any chat body, so hitting the cap is a client bug worth a
/// named status rather than a silent OOM.
pub(crate) const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;
/// Cap for buffered response bodies (non-streaming completions).
pub(crate) const MAX_RESPONSE_BUFFER: usize = 64 * 1024 * 1024;
/// Cap for buffered non-2xx bodies, which are small in practice.
pub(crate) const MAX_ERROR_BODY: usize = 16 * 1024 * 1024;
/// Maximum raw streaming prefix retained before the frontend head commits.
#[allow(dead_code)]
pub(crate) const PRECOMMIT_BODY_CAP: usize = 64 * 1024;

/// `POST /v1/chat/completions` — the usage path.
pub(crate) async fn chat_completions(State(server): State<Server>, request: Request) -> Response {
    let started = Instant::now();
    let (parts, body) = request.into_parts();

    // Session identity, read by name only — request headers are never
    // captured wholesale: they carry credentials (spec: SEC#a-credential-is-never-logged-or-displayed).
    let session_id = session_id(&server.config().session_header_names, &parts.headers);
    // Ping tagging: a lane whose request carried the
    // ping header is recorded but excluded from liveness — the window
    // pinger's probe must never hold the sleep lock, on this path like
    // the anthropic one.
    let ping = lanes::is_ping(&parts.headers, &server.config().ping_header_name);
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
    let in_flight = Some(server.begin_in_flight(true, frontend.as_deref()));

    // 2.-5. Parse and route. Every supported Chat backend renders from
    // canonical IR, including the same-protocol OpenRouter binding.
    // The request's own shape and ask, kept past the record context: the
    // cold gate keys the lane on the session × tools-hash the request
    // itself carries, answers in the wire form the request asked for,
    // and exempts by the post-routing model.
    // A body that cannot be built into a request leaves a ledger row. Only
    // a parsed body can say whether it asked to stream, so one that is not
    // JSON at all keeps the JSON error.
    let refusal = RefusalCtx::new(
        started,
        frontend.as_deref(),
        "openai_chat",
        session_id.as_deref(),
        None,
        None,
    );
    let value: serde_json::Value = match serde_json::from_slice(&original) {
        Ok(value) => value,
        Err(_) => {
            return refusal.settle(
                &server,
                refused(compatibility_error(
                    "the request body is not valid JSON",
                    false,
                )),
            );
        }
    };
    let stream_asked = value.get("stream").and_then(serde_json::Value::as_bool) == Some(true);
    let canonical = match translate::from_openai_chat(&value) {
        Ok(canonical) => canonical,
        Err(error) => {
            let refusal = RefusalCtx::new(
                started,
                frontend.as_deref(),
                "openai_chat",
                session_id.as_deref(),
                value.get("model").and_then(serde_json::Value::as_str),
                None,
            );
            return refusal.settle(
                &server,
                refused(compatibility_error(&error.to_string(), stream_asked)),
            );
        }
    };
    let Some(model) = canonical.model.clone().filter(|model| !model.is_empty()) else {
        return compatibility_error("model is missing, empty, or not a string", stream_asked);
    };
    if let Some(guard) = &in_flight {
        guard.identify(&crate::store::InFlightIdentity {
            session_id: session_id.as_deref(),
            lineage: None,
            requested_model: Some(&model),
            req_messages: Some(canonical.messages.len() as i64),
        });
    }
    let mut inference = super::inference::InferenceRequest::new(model.clone(), canonical);
    let canonical = inference.canonical();
    let shape = Shape::from_canonical(&canonical, original.len() as u64);
    let system_messages = Some(
        canonical
            .messages
            .iter()
            .filter(|message| matches!(message.role, CanonRole::System | CanonRole::Developer))
            .count() as u64,
    );
    let stream_requested = canonical.stream == Some(true);
    let cold_lane_key = lanes::lane_key(session_id.as_deref(), Some(&shape.tools_hash));
    let cold_lane = cold_lane_key
        .as_ref()
        .and_then(|key| server.store.load_lane(key).ok().flatten());
    let pinned_binding_id = cold_lane
        .as_ref()
        .and_then(|lane| lane.pin.as_ref())
        .map(|pin| pin.binding_id.clone());
    let affinity_binding = pinned_binding_id
        .as_ref()
        .and_then(|binding_id| server.registry.binding(binding_id));
    // Provider-owned state belongs to the binding that produced it, which
    // only a pin records. A lane with no pin resolves as a new one does:
    // refusing instead would never recover, since the client replays the
    // same history on every retry and only a served response pins the lane.
    let binding_affinity = pinned_binding_id.filter(|_| canonical.requires_binding_affinity());
    let mut facts = server.request_facts(
        ProtocolId::OpenAiChat,
        frontend.as_deref(),
        Some(model.clone()),
        cold_lane_key.clone(),
        force_newest::prompt_bound(original.len() as u64),
        shape.req_messages,
        shape.req_tools,
        super::anthropic::required_capabilities(&canonical),
        binding_affinity,
        crate::policy::RequestClass::Inference,
    );
    facts.accounts = server.request_accounts(&parts.headers, ProtocolId::OpenAiChat);
    let credentials =
        server.credential_snapshot(&parts.headers, ProtocolId::OpenAiChat, facts.now_ms);
    let lane = server.lane_facts(
        cold_lane.as_ref(),
        affinity_binding,
        &facts.accounts,
        facts.now_ms,
        false,
    );
    let (target, policy_attempt) = match server.resolve_policy_attempt(
        &facts,
        &credentials,
        cold_lane.as_ref(),
        lane.as_ref(),
    ) {
        Ok(resolved) => {
            let target = match server.registry.resolve_binding(
                ProtocolId::OpenAiChat,
                &resolved.plan.binding_id,
                Some(model.as_str()),
                &resolved.plan.effective_model,
            ) {
                Ok(target) => target,
                Err(_) => {
                    tracing::error!(
                        binding_id = %resolved.plan.binding_id,
                        "active policy selected an unconfigured Chat binding"
                    );
                    return upstream_failure(
                        ErrorWire::Openai,
                        "the active routing policy selected an unavailable backend",
                    );
                }
            };
            (target, resolved)
        }
        Err(error) if error.is_route_unavailable() => {
            record_unroutable(
                &server,
                started,
                frontend.as_deref(),
                "openai_chat",
                session_id.as_deref(),
                Some(model.as_str()),
                None,
                &error,
                &facts,
            );
            return error_response(
                ErrorWire::Openai,
                StatusCode::SERVICE_UNAVAILABLE,
                WireError {
                    anthropic_type: "api_error",
                    openai_type: "server_error",
                    openai_code: Some("route_unavailable"),
                },
                "the active routing policy has no available backend for this request",
            );
        }
        Err(error) => {
            record_unroutable(
                &server,
                started,
                frontend.as_deref(),
                "openai_chat",
                session_id.as_deref(),
                Some(model.as_str()),
                None,
                &error,
                &facts,
            );
            tracing::error!(%error, "active Chat policy resolution failed");
            let response = upstream_failure(
                ErrorWire::Openai,
                "the active routing policy could not resolve this request",
            );
            return if error.is_no_default_route() {
                super::mark_not_configured(response, "openai_chat")
            } else {
                response
            };
        }
    };
    let gate_model = Some(policy_attempt.plan.effective_model.clone());
    let backend = target.provider().clone();
    let credential = super::quota_gate::request_credential(
        &server,
        backend.as_ref(),
        &parts.headers,
        ProtocolId::OpenAiChat,
    );
    let meters =
        super::quota_gate::load_meters_for(&server, backend.id(), credential.account_number);
    let release = crate::middleware::canonical::release_marker(&inference.canonical());
    if let Some((release, grant)) = super::quota_gate::grant_release(
        &server,
        session_id.as_deref(),
        backend.id(),
        credential.account_number,
        release,
        meters.as_ref(),
    ) {
        record_anthropic_released(
            &server,
            session_id
                .as_deref()
                .expect("a granted release has a session"),
            backend.id(),
            "openai_chat",
            release,
            &grant,
            meters.as_ref(),
            frontend.as_deref(),
        );
    }
    inference.strip_release(&original);
    let pin_transition = Some(policy_attempt.pin_transition.clone());
    let attempt = inference.prepare_policy(target.clone(), policy_attempt.plan.clone());
    if let Some(block) = super::quota_gate::decide_block(
        &server,
        backend.id(),
        session_id.as_deref(),
        credential.account_number,
        frontend.as_deref(),
        meters,
    ) {
        let body = cold::ColdBlocking::openai_turn(
            &block.text,
            Some(model.as_str()),
            stream_requested,
            facts.now_ms,
        );
        record_quota_blocked(QuotaBlockedRecord {
            server: &server,
            started,
            path: "/v1/chat/completions",
            session_id: session_id.as_deref(),
            backend_id: backend.id(),
            credential: &credential,
            frontend_protocol: "openai_chat",
            meter: block.meter,
            resets_at: block.resets_at,
            context_tokens: block.context_tokens,
            stale_meters: block.stale_meters.as_ref(),
            frontend: frontend.as_deref(),
            requested_model: Some(model.as_str()),
            effective_model: gate_model.as_deref(),
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
    let record = Some(RecordCtx {
        frontend: frontend.clone(),
        server: server.clone(),
        backend: backend.clone(),
        binding_id: attempt.binding().id().clone(),
        attempt: None,
        policy_routing: None,
        pin_transition,
        started,
        session_id: session_id.clone(),
        ping,
        requested_model: Some(attempt.requested_model().to_owned()),
        effective_model: gate_model.clone(),
        shape: Some(shape.clone()),
        system_messages,
        policy_attempt: Some(Arc::new(policy_attempt.clone())),
    });

    // 6. The cold-cache notice (see the module docs). Advisory like the
    // anthropic gate's: once per idle spell, re-armed by activity, and
    // the resend IS the release — there is no marker on this wire.
    // Skipped for unparseable bodies, which have neither a shape nor a
    // lane (the same verdict a keyless miss reaches), and every failure
    // below is "a lost notice, never a lost request": a store error
    // reads as absence and the request forwards.
    // Cache lifetime belongs to the selected binding. Only OpenRouter's
    // Chat lifetime is currently verified; assigning its ten-minute clock
    // to direct OpenAI, Codex, or Anthropic would turn missing evidence into
    // a false cold notice. Binding-scoped lane state in the live-routing
    // plan generalises this once each binding has a supported clock.
    let cold_armed = server.config().gates.cold_enabled && backend.id() == "openrouter";
    if cold_armed {
        let gates = &server.config().gates;
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
            if shape.summarising {
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
                    tools_hash: Some(shape.tools_hash.as_str()),
                    idle_ms,
                    prompt,
                    req_messages: None,
                    compact_target: None,
                    backend_id: backend.id(),
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
                    server.config().notices.style_for(frontend.as_deref()),
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
                    tools_hash: Some(shape.tools_hash.as_str()),
                    idle_ms,
                    prompt,
                    // The message count of the stopped request: the
                    // synthetic turn is appended to the client's
                    // transcript, so the next request in this lane
                    // should carry both.
                    req_messages: shape.req_messages.map(|messages| messages as i64),
                    compact_target: None,
                    backend_id: backend.id(),
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

    let adapter = target
        .binding()
        .canonical_backend()
        .map(|backend| backend.adapter());
    super::inference::coordinate_chat(
        server,
        adapter,
        attempt,
        policy_attempt,
        inference,
        super::inference::ChatAttemptInput {
            parts,
            max_tokens: value
                .get("max_completion_tokens")
                .filter(|value| !value.is_null())
                .or_else(|| value.get("max_tokens"))
                .cloned(),
            record,
            in_flight,
            session_id,
            client_wants_json: !stream_requested,
        },
    )
    .await
}

/// `GET /v1/models` is a local projection of the active policy revision's
/// publication for the request's profile. A profile that names no protocol
/// (an unfamiliar name, or none) gets the Chat shape, as the bare path
/// always did. No model catalogue request is a usage path or ledger row.
pub(crate) async fn models(State(server): State<Server>, request: Request) -> Response {
    let profile = super::frontend_profile_of(request.extensions()).and_then(|p| p.protocol());
    let protocol = profile.unwrap_or(ProtocolId::OpenAiChat);
    match server.active_publication(super::frontend_of(request.extensions()), protocol.into()) {
        offers::PolicyPublication::Ready(body) => axum::Json(body).into_response(),
        offers::PolicyPublication::Unresolved => {
            catalog_unavailable("The active policy publication cannot be resolved yet")
        }
    }
}

fn catalog_unavailable(message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({
            "error": {"type": "catalog_unavailable", "message": message}
        })),
    )
        .into_response()
}

/// Send one request upstream: mapped endpoint, cleaned headers, auth
/// injection, and the (possibly rewritten) body. `provider` decides the
/// endpoint mapping and the credential rules; `session_header_names` is
/// the strip list for [`upstream_request_headers`] — the openai path
/// passes the configured attribution headers, the anthropic path passes
/// an empty slice (the predecessor forwarded claude's session header
/// verbatim).
pub(crate) struct UpstreamResponse {
    pub(crate) response: reqwest::Response,
    pub(crate) credential_digest: Option<CredentialDigest>,
}

pub(crate) async fn send_upstream(
    server: &Server,
    provider: &dyn Provider,
    parts: &Parts,
    body: Bytes,
    session_header_names: &[String],
) -> Result<UpstreamResponse, reqwest::Error> {
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

/// The headers a request leaves with: cleaned, with the provider's own
/// credential rules applied. The send and the quota gate both read the
/// credential off this, so the gate is about the account the request will be
/// served on rather than a guess at it.
pub(crate) fn outgoing_headers(
    provider: &dyn Provider,
    incoming: &HeaderMap,
    session_header_names: &[String],
    frontend: ProtocolId,
) -> HeaderMap {
    // Pass-through-when-present: a frontend that
    // brings its own credential keeps it verbatim; the stored credential
    // is injected only when the request carries none. If neither exists
    // the request goes unauthenticated and the upstream's 401 body passes
    // through — visibly verifying the wiring.
    let mut headers = upstream_request_headers(incoming, session_header_names);
    // Before injection, so a dropped foreign credential leaves room for
    // the provider's own.
    provider.strip_foreign_credentials_for(&mut headers, frontend);
    if !provider.credential_present(&headers) {
        provider.inject_auth(&mut headers);
    }
    provider.prepare_protocol_headers(&mut headers);
    headers
}

/// The digest of the credential in `headers`, if they carry one.
pub(crate) fn outgoing_credential_digest(headers: &HeaderMap) -> Option<CredentialDigest> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
        .map(credential_digest)
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .filter(|key| !key.is_empty())
                .map(credential_digest)
        })
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
) -> Result<UpstreamResponse, reqwest::Error> {
    let url = provider.endpoint(path);
    let headers = outgoing_headers(provider, &parts.headers, session_header_names, frontend);
    let credential_digest = outgoing_credential_digest(&headers);
    let response = server
        .http
        .request(parts.method.clone(), url)
        .headers(headers)
        .body(body)
        .send()
        .await?;
    Ok(UpstreamResponse {
        response,
        credential_digest,
    })
}

/// A Chat binding's live universal-canonical response path. Accounting
/// observes the provider bytes before translation, so billed cost, estimates,
/// and verbatim usage remain provider evidence rather than renderer output.
pub(crate) async fn forward_chat_canonical(
    upstream: reqwest::Response,
    record: Option<RecordCtx>,
    in_flight: Option<InFlightGuard>,
    model: &str,
    expected_stream: bool,
) -> Response {
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    if is_compressed(&upstream_headers) {
        tracing::warn!("compressed canonical chat response could not be interpreted");
        if let Some(ctx) = record.as_ref() {
            record_error(
                ctx,
                status.as_u16(),
                Some("response_protocol".to_owned()),
                None,
            );
        }
        return upstream_failure(
            ErrorWire::Openai,
            "toker could not interpret a compressed upstream response",
        );
    }

    if !status.is_success() {
        let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
            if let Some(ctx) = record.as_ref() {
                record_error(
                    ctx,
                    status.as_u16(),
                    Some("ambiguous_transport".to_owned()),
                    None,
                );
            }
            return truncated_body(ErrorWire::Openai);
        };
        if buffered.rest.is_some() {
            if let Some(ctx) = record.as_ref() {
                record_error(
                    ctx,
                    status.as_u16(),
                    Some("response_protocol".to_owned()),
                    None,
                );
            }
            return upstream_failure(
                ErrorWire::Openai,
                "toker could not interpret an oversized upstream error",
            );
        }
        let rendered = serde_json::from_slice::<serde_json::Value>(&buffered.bytes)
            .ok()
            .and_then(|body| {
                translate::openai_chat_backend::canonical_turn_from_openai_chat(&body).ok()
            })
            .map(|turn| translate::openai_chat_from_canonical(model, &turn));
        let Some(rendered) = rendered else {
            if let Some(ctx) = record.as_ref() {
                record_error(
                    ctx,
                    status.as_u16(),
                    Some("response_protocol".to_owned()),
                    retry_after_ms(&upstream_headers),
                );
            }
            return upstream_failure(
                ErrorWire::Openai,
                "toker could not interpret the upstream error response",
            );
        };
        if let Some(ctx) = record.as_ref() {
            record_error(
                ctx,
                status.as_u16(),
                parse_error_type(&buffered.bytes),
                retry_after_ms(&upstream_headers),
            );
        }
        return json_response(status, &upstream_headers, &rendered);
    }

    if is_event_stream(&upstream_headers) != expected_stream {
        if let Some(ctx) = record.as_ref() {
            record_error(
                ctx,
                status.as_u16(),
                Some("response_protocol".to_owned()),
                None,
            );
        }
        return upstream_failure(
            ErrorWire::Openai,
            "upstream Chat response used the wrong streaming mode",
        );
    }

    if expected_stream {
        let stream = CanonicalChatStream::new(upstream, status.as_u16(), record, in_flight, model);
        let mut headers = response_headers(&upstream_headers, false);
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        return build_response(status, headers, Body::from_stream(stream));
    }

    let Ok(buffered) = buffer_up_to(upstream, MAX_RESPONSE_BUFFER).await else {
        if let Some(ctx) = record.as_ref() {
            record_error(
                ctx,
                status.as_u16(),
                Some("ambiguous_transport".to_owned()),
                None,
            );
        }
        return truncated_body(ErrorWire::Openai);
    };
    if buffered.rest.is_some() {
        if let Some(ctx) = record.as_ref() {
            record_error(
                ctx,
                status.as_u16(),
                Some("response_protocol".to_owned()),
                None,
            );
        }
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
            if let Some(ctx) = record.as_ref() {
                record_error(
                    ctx,
                    status.as_u16(),
                    Some("response_protocol".to_owned()),
                    None,
                );
            }
            return upstream_failure(
                ErrorWire::Openai,
                "toker could not interpret the upstream response",
            );
        }
    };
    if let Some(ctx) = record.as_ref() {
        let mut observer = UsageObserver::new();
        observer.observe_json(&buffered.bytes);
        let (reported_model, capture) = observer.finish_parts();
        record_measurement(
            ctx,
            capture.as_ref(),
            reported_model.as_deref(),
            status.as_u16(),
        );
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

/// A 400 for a Chat request toker cannot use, in Chat's own shape: a JSON
/// error, or for a request that asked to stream, the same error as the
/// stream's `data:` chunk, which is how a Chat stream reports one.
pub(crate) fn compatibility_error(message: &str, stream: bool) -> Response {
    super::codex::frontend_error_response(
        super::codex::CodexFrontendWire::OpenAiChat,
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        message,
        stream,
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
        if let Some(CanonEvent::TurnFailed { error }) = canonical
            .iter()
            .find(|event| matches!(event, CanonEvent::TurnFailed { .. }))
        {
            self.record_stream_error(translate::anthropic_frontend::anthropic_error_type(error));
        }
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

    fn record_stream_error(&mut self, error_type: &str) {
        if let Some(ctx) = self.ctx.take() {
            record_error(&ctx, self.status, Some(error_type.to_owned()), None);
        }
    }

    fn finish_recording(&mut self) {
        if let Some(ctx) = self.ctx.take() {
            let (reported_model, capture) = std::mem::take(&mut self.observer).finish_parts();
            record_measurement(
                &ctx,
                capture.as_ref(),
                reported_model.as_deref(),
                self.status,
            );
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
                    this.record_stream_error("ambiguous_transport");
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
        self.record_stream_error("client_cancelled");
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

pub(crate) fn chat_cross_record(ctx: RecordCtx, backend: Arc<dyn Provider>) -> AnthropicRecordCtx {
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
        lineage: None,
        requested_model: ctx.requested_model,
        effective_model: ctx.effective_model,
        backend,
        credential_digest: None,
        account_number: None,
        binding_id: ctx.binding_id,
        attempt: ctx.attempt,
        policy_routing: ctx.policy_routing,
        pin_transition: ctx.pin_transition,
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
        translation_report: None,
        safeguards: Default::default(),
        policy_attempt: ctx.policy_attempt,
    }
}

/// The session identity from the configured header names, in priority
/// order, read by name only (spec: SEC#a-credential-is-never-logged-or-displayed).
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
///
/// Where the upstream sent the overage utilisation and no monthly overage
/// period utilisation, the latter is added with the former's value.
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
    // spec: WCANON#response-headers
    // Claude Code drops the overage utilisation from its rate limit event
    // exactly while overage is drawn (the exhausted plan window is the
    // representative claim then), but passes the monthly overage period
    // utilisation through in every state. The API has not been seen
    // sending the monthly one, so a host reads the figure from there.
    if !outgoing.contains_key(OVERAGE_PERIOD_MONTHLY_UTILIZATION)
        && let Some(value) = upstream.get(OVERAGE_UTILIZATION)
    {
        outgoing.insert(OVERAGE_PERIOD_MONTHLY_UTILIZATION, value.clone());
    }
    outgoing
}

const OVERAGE_UTILIZATION: &str = "anthropic-ratelimit-unified-overage-utilization";
const OVERAGE_PERIOD_MONTHLY_UTILIZATION: &str =
    "anthropic-ratelimit-unified-overage-period-monthly-utilization";

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
    /// Bytes from the chunk that crossed the cap. `rest` begins after these.
    pub(crate) overflow: Option<Bytes>,
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
                    let retained = cap.saturating_sub(bytes.len()).min(chunk.len());
                    bytes.extend_from_slice(&chunk[..retained]);
                    return Ok(Buffered {
                        bytes,
                        overflow: (retained < chunk.len()).then(|| chunk.slice(retained..)),
                        rest: Some(response),
                    });
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => {
                return Ok(Buffered {
                    bytes,
                    overflow: None,
                    rest: None,
                });
            }
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
/// travels in headers (spec: SEC#a-credential-is-never-logged-or-displayed).
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
pub(crate) fn rebuilt_response(
    status: StatusCode,
    headers: HeaderMap,
    buffered: Buffered,
) -> reqwest::Response {
    let body = match buffered.rest {
        None => reqwest::Body::from(buffered.bytes),
        Some(rest) => reqwest::Body::wrap_stream(
            futures::stream::iter([Ok::<_, reqwest::Error>(Bytes::from(buffered.bytes))])
                .chain(futures::stream::iter(
                    buffered.overflow.map(Ok::<_, reqwest::Error>),
                ))
                .chain(rest.bytes_stream()),
        ),
    };
    let mut response = axum::http::Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    reqwest::Response::from(response)
}

pub(crate) fn buffered_body(buffered: Buffered) -> Body {
    match buffered.rest {
        Some(response) => {
            let prefix = Ok::<_, reqwest::Error>(Bytes::from(buffered.bytes));
            let overflow = buffered.overflow.map(Ok::<_, reqwest::Error>);
            Body::from_stream(
                futures::stream::iter([prefix])
                    .chain(futures::stream::iter(overflow))
                    .chain(response.bytes_stream()),
            )
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
    use super::{
        ErrorWire, OVERAGE_PERIOD_MONTHLY_UTILIZATION, OVERAGE_UTILIZATION, buffer_up_to,
        buffered_body, response_headers, transport_failure,
    };
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    header::HeaderName::from_static(name),
                    HeaderValue::from_static(value),
                )
            })
            .collect()
    }

    #[test]
    fn overage_utilisation_is_copied_into_the_monthly_header() {
        let upstream = headers(&[(OVERAGE_UTILIZATION, "0.4127")]);
        let outgoing = response_headers(&upstream, false);
        assert_eq!(outgoing.get(OVERAGE_UTILIZATION).unwrap(), "0.4127");
        assert_eq!(
            outgoing.get(OVERAGE_PERIOD_MONTHLY_UTILIZATION).unwrap(),
            "0.4127"
        );
    }

    #[test]
    fn a_value_that_is_not_a_number_is_copied_as_sent() {
        let upstream = headers(&[(OVERAGE_UTILIZATION, "about half")]);
        let outgoing = response_headers(&upstream, false);
        assert_eq!(
            outgoing.get(OVERAGE_PERIOD_MONTHLY_UTILIZATION).unwrap(),
            "about half"
        );
    }

    #[test]
    fn a_monthly_header_the_provider_sent_is_kept() {
        let upstream = headers(&[
            (OVERAGE_UTILIZATION, "0.4127"),
            (OVERAGE_PERIOD_MONTHLY_UTILIZATION, "0.9"),
        ]);
        let outgoing = response_headers(&upstream, false);
        let monthly: Vec<_> = outgoing
            .get_all(OVERAGE_PERIOD_MONTHLY_UTILIZATION)
            .iter()
            .collect();
        assert_eq!(monthly, ["0.9"]);
    }

    #[test]
    fn neither_header_is_added_when_the_provider_sent_no_overage_utilisation() {
        let upstream = headers(&[("anthropic-ratelimit-unified-5h-utilization", "0.2")]);
        let outgoing = response_headers(&upstream, false);
        assert!(!outgoing.contains_key(OVERAGE_UTILIZATION));
        assert!(!outgoing.contains_key(OVERAGE_PERIOD_MONTHLY_UTILIZATION));
    }

    #[test]
    fn a_monthly_header_alone_is_not_touched() {
        let upstream = headers(&[(OVERAGE_PERIOD_MONTHLY_UTILIZATION, "0.9")]);
        let outgoing = response_headers(&upstream, false);
        assert_eq!(
            outgoing.get(OVERAGE_PERIOD_MONTHLY_UTILIZATION).unwrap(),
            "0.9"
        );
        assert!(!outgoing.contains_key(OVERAGE_UTILIZATION));
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
    async fn capped_buffering_preserves_the_chunk_that_crossed_the_cap() {
        let response = axum::http::Response::builder()
            .status(StatusCode::OK)
            .body(reqwest::Body::from("abcdefgh"))
            .unwrap();
        let buffered = buffer_up_to(reqwest::Response::from(response), 3)
            .await
            .unwrap();
        assert_eq!(buffered.bytes, b"abc");
        let body = buffered_body(buffered);
        let bytes = axum::body::to_bytes(body, 16).await.unwrap();
        assert_eq!(bytes.as_ref(), b"abcdefgh");
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
