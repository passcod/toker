//! The codex translation branch.
//!
//! A request routed to the codex_sub backend never byte-forwards: the
//! codex backend speaks the Responses dialect, so each supported frontend
//! goes through [`crate::translate`] and canonical IR/events in both
//! directions. Frontend-byte comparison does not apply to backend-rendered
//! canonical requests.
//!
//! Every other pipeline stage ran before this branch: the release
//! marker, the quota gate (anthropic_sub-only, never here), the cold
//! gate, the compaction retarget, force-newest, and the model routing
//! map — so the prepared attempt carries the final effective model used by
//! the codex request.
//!
//! Recording: a measurement row on a completed turn (usage from
//! [`crate::providers::codex::TurnCapture`], the usage object verbatim
//! in `usage_raw`, cost NULL — there is no honest per-token price for
//! codex slugs, never guessed), an error row on a failed one. The
//! codex meters (`x-codex-*` headers) feed the per-backend meter slot
//! on EVERY response — the "not just accounted ones" rule.

use std::collections::VecDeque;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;

use super::Server;
use crate::ir::AnthropicShape;
use crate::ir::canonical::ResponseProjection;
use crate::middleware::lanes;
use crate::observe::SseEvent;
use crate::policy::BindingId;
use crate::providers::Provider;
use crate::providers::codex::{ResponseError, ResponseEvent, ResponsesSse, TurnCapture};
use crate::routing::ProtocolId;
use crate::server::InFlightGuard;
use crate::server::proxy::{
    ErrorWire, MAX_ERROR_BODY, MAX_REQUEST_BODY, answers_stream_request, buffer_up_to,
    is_compressed, plain_status, session_id, transport_failure, truncated_body, upstream_failure,
};
use crate::server::record::{now_ms, record_unroutable};
use crate::server::record_anthropic::AnthropicRecordCtx;
use crate::server::record_anthropic::{record_codex_error, record_responses_measurement};
use crate::store::SessionLineage;
use crate::translate::{self, TranslateError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexFrontendWire {
    Anthropic,
    OpenAiChat,
    OpenAiResponses,
}

impl CodexFrontendWire {
    fn protocol(self) -> &'static str {
        match self {
            Self::Anthropic => ANTHROPIC_FRONTEND,
            Self::OpenAiChat => "openai_chat",
            Self::OpenAiResponses => RESPONSES_FRONTEND,
        }
    }

    fn error_wire(self) -> ErrorWire {
        match self {
            Self::Anthropic => ErrorWire::Anthropic,
            Self::OpenAiChat | Self::OpenAiResponses => ErrorWire::Openai,
        }
    }
}

const ANTHROPIC_FRONTEND: &str = "anthropic";
const RESPONSES_FRONTEND: &str = "openai_responses";

/// Codex CLI usage path. Even though both sides speak Responses, the request
/// and response take the same canonical path as every cross-protocol route.
pub(crate) async fn responses(State(server): State<Server>, request: Request) -> Response {
    let started = Instant::now();
    let (parts, body) = request.into_parts();
    let original = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "responses request body exceeded toker's cap");
            return plain_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds toker's 64 MiB cap\n",
            );
        }
    };
    let frontend = super::frontend_of(&parts.extensions).map(str::to_owned);
    let in_flight = Some(server.begin_in_flight(true, frontend.as_deref()));
    let header_session = session_id(&server.config().session_header_names, &parts.headers);
    let ping = lanes::is_ping(&parts.headers, &server.config().ping_header_name);
    let thread_id = parts
        .headers
        .get("thread-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let request_id = parts
        .headers
        .get("x-client-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let parsed = serde_json::from_slice::<Value>(&original).ok();
    let canonical = parsed.as_ref().map(translate::from_openai_responses);
    let body_session = parsed
        .as_ref()
        .and_then(|request| request.get("prompt_cache_key"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let request_session = body_session.clone().or_else(|| header_session.clone());
    let lineage = (super::frontend_of(&parts.extensions) == Some("codex")
        && request_session == header_session)
        .then(|| {
            header_session
                .as_deref()
                .and_then(SessionLineage::codex_guardian)
        })
        .flatten();
    let requested_model = canonical
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .and_then(|request| request.model.clone())
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|request| request.get("model"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let Some(requested_model) = requested_model.filter(|model| !model.is_empty()) else {
        return frontend_error_response(
            CodexFrontendWire::OpenAiResponses,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "model is missing, empty, or not a string",
            false,
        );
    };
    let client_wants_json = parsed
        .as_ref()
        .is_none_or(|request| request.get("stream").and_then(Value::as_bool) != Some(true));
    let canonical_request = canonical.as_ref().and_then(|result| result.as_ref().ok());
    let shape = canonical_request.map(|canonical| {
        AnthropicShape::from_canonical_responses(
            canonical,
            original.len() as u64,
            parsed
                .as_ref()
                .and_then(|request| request.get("input"))
                .and_then(Value::as_array)
                .map(|input| input.len() as u64),
        )
    });
    if let Some(guard) = &in_flight {
        guard.identify(&crate::store::InFlightIdentity {
            session_id: request_session.as_deref(),
            lineage: lineage.as_ref(),
            requested_model: Some(&requested_model),
            req_messages: shape
                .as_ref()
                .and_then(|shape| shape.req_messages)
                .and_then(|count| i64::try_from(count).ok()),
        });
    }
    let lane_reference = shape.as_ref().and_then(|shape| {
        lanes::lane_key(request_session.as_deref(), Some(shape.tools_hash.as_str()))
    });
    let stored_lane = lane_reference.as_deref().and_then(|key| {
        server
            .store
            .load_lane(key)
            .inspect_err(|error| tracing::error!(%error, lane_key = key, "lane load failed"))
            .ok()
            .flatten()
    });
    let pinned_binding = stored_lane
        .as_ref()
        .and_then(|lane| lane.pin.as_ref())
        .map(|pin| pin.binding_id.clone());

    // A body that does not canonicalise names nothing for policy to route.
    let (Some(canonical_request), Some(shape)) = (canonical_request, shape.as_ref()) else {
        return super::record::RefusalCtx::new(
            started,
            frontend.as_deref(),
            RESPONSES_FRONTEND,
            request_session.as_deref(),
            Some(requested_model.as_str()),
            lineage.clone(),
        )
        .settle(
            &server,
            super::record::refused(super::inference::invalid_responses_body(client_wants_json)),
        );
    };
    let (target, policy_attempt) = {
        // Provider-owned state belongs to the binding that produced it,
        // which only a pin records. A lane with no pin resolves as a new one
        // does: refusing instead would never recover, since the client
        // replays the same history on every retry and only a served response
        // pins the lane.
        let binding_affinity = pinned_binding
            .clone()
            .filter(|_| canonical_request.requires_binding_affinity());
        let mut facts = server.request_facts(
            ProtocolId::OpenAiResponses,
            frontend.as_deref(),
            Some(requested_model.clone()),
            lane_reference.clone(),
            crate::policy::force_newest::prompt_bound(original.len() as u64),
            shape.req_messages,
            shape.req_tools,
            super::anthropic::required_capabilities(canonical_request),
            binding_affinity,
            crate::policy::RequestClass::Inference,
        );
        facts.accounts = server.request_accounts(&parts.headers, ProtocolId::OpenAiResponses);
        let credentials =
            server.credential_snapshot(&parts.headers, ProtocolId::OpenAiResponses, facts.now_ms);
        let lane = server.lane_facts(
            stored_lane.as_ref(),
            pinned_binding
                .as_ref()
                .and_then(|binding| server.registry.binding(binding)),
            &facts.accounts,
            facts.now_ms,
            false,
        );
        match server.resolve_policy_attempt(
            &facts,
            &credentials,
            stored_lane.as_ref(),
            lane.as_ref(),
        ) {
            Ok(resolved) => {
                let target = match server.registry.resolve_binding(
                    ProtocolId::OpenAiResponses,
                    &resolved.plan.binding_id,
                    Some(requested_model.as_str()),
                    &resolved.plan.effective_model,
                ) {
                    Ok(target) => target,
                    Err(_) => {
                        tracing::error!(
                            binding_id = %resolved.plan.binding_id,
                            "active policy selected an unconfigured Responses binding"
                        );
                        return frontend_error_response(
                            CodexFrontendWire::OpenAiResponses,
                            StatusCode::BAD_GATEWAY,
                            "server_error",
                            "the active routing policy selected an unavailable backend",
                            !client_wants_json,
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
                    RESPONSES_FRONTEND,
                    request_session.as_deref(),
                    Some(requested_model.as_str()),
                    lineage.clone(),
                    &error,
                    &facts,
                );
                return frontend_error_response(
                    CodexFrontendWire::OpenAiResponses,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "server_error",
                    "the active routing policy has no available backend for this request",
                    !client_wants_json,
                );
            }
            Err(error) => {
                record_unroutable(
                    &server,
                    started,
                    frontend.as_deref(),
                    RESPONSES_FRONTEND,
                    request_session.as_deref(),
                    Some(requested_model.as_str()),
                    lineage.clone(),
                    &error,
                    &facts,
                );
                tracing::error!(%error, "active Responses policy resolution failed");
                let response = frontend_error_response(
                    CodexFrontendWire::OpenAiResponses,
                    StatusCode::BAD_GATEWAY,
                    "server_error",
                    "the active routing policy could not resolve this request",
                    !client_wants_json,
                );
                return if error.is_no_default_route() {
                    super::mark_not_configured(response, RESPONSES_FRONTEND)
                } else {
                    response
                };
            }
        }
    };
    let backend = target.provider().clone();
    let effective_model = Some(policy_attempt.plan.effective_model.clone());
    let pin_transition = Some(policy_attempt.pin_transition.clone());
    let policy_record = Some(std::sync::Arc::new(policy_attempt.clone()));
    let Some(Ok(canonical)) = canonical else {
        unreachable!("a body that canonicalised was routed above");
    };
    let inference = super::inference::InferenceRequest::new(requested_model.clone(), canonical);
    let attempt = inference.prepare_policy(target.clone(), policy_attempt.plan.clone());

    let record = parsed.as_ref().map(|_| AnthropicRecordCtx {
        server: server.clone(),
        started,
        path: "/v1/responses",
        session_id: request_session.clone(),
        lineage: lineage.clone(),
        requested_model: Some(requested_model.clone()),
        effective_model: effective_model.clone(),
        backend: backend.clone(),
        credential_digest: None,
        account_number: None,
        binding_id: target.binding().id().clone(),
        attempt: None,
        policy_routing: None,
        pin_transition,
        betas: None,
        shape: Some(shape.clone()),
        ping,
        downgraded_from: None,
        downgraded_to: None,
        cache_stripped: None,
        system_merged: None,
        forced_from: None,
        forced_to: None,
        model_mappings: None,
        frontend,
        translation_report: None,
        safeguards: Default::default(),
        policy_attempt: policy_record,
    });

    let adapter = target
        .binding()
        .canonical_backend()
        .map(|backend| backend.adapter());
    super::inference::coordinate_responses(
        server,
        adapter,
        attempt,
        policy_attempt,
        inference,
        super::inference::ResponsesAttemptInput {
            parts,
            max_output_tokens: parsed
                .as_ref()
                .and_then(|request| request.get("max_output_tokens").cloned()),
            had_prompt_cache_key: parsed
                .as_ref()
                .is_some_and(|request| request.get("prompt_cache_key").is_some()),
            record,
            in_flight,
            session_id: request_session,
            thread_id,
            request_id,
            client_wants_json,
        },
    )
    .await
}

pub(crate) enum FailureResponse {
    Upstream(reqwest::Response),
    Local(Response),
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum ReadyOutcome {
    Accepted {
        ready: ReadyAttempt,
        upstream: reqwest::Response,
        cancellation: super::cancel::PrecommitCancellationGuard,
    },
    Failure {
        ready: ReadyAttempt,
        failure: super::attempt::AttemptFailure,
        response: FailureResponse,
        cancellation: super::cancel::PrecommitCancellationGuard,
    },
}

pub(crate) struct ReadyAttempt {
    parts: axum::http::request::Parts,
    backend: std::sync::Arc<dyn Provider>,
    binding_id: BindingId,
    effective_model: String,
    response_model: String,
    response_projection: ResponseProjection,
    meter_snapshot: Option<Value>,
    headers: HeaderMap,
    body: Vec<u8>,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    client_wants_json: bool,
    frontend_wire: CodexFrontendWire,
}

impl ReadyAttempt {
    pub(crate) fn binding_id(&self) -> &BindingId {
        &self.binding_id
    }

    pub(crate) fn in_flight(&self) -> Option<&InFlightGuard> {
        self.in_flight.as_ref()
    }

    pub(crate) fn provider(&self) -> &str {
        self.backend.id()
    }

    pub(crate) fn effective_model(&self) -> &str {
        &self.effective_model
    }

    pub(crate) async fn execute_precommit(
        mut self,
        server: &Server,
        provenance: crate::store::AttemptProvenance,
    ) -> ReadyOutcome {
        if let Some(ctx) = self.record.as_mut() {
            ctx.attempt = Some(provenance);
        }
        let cancellation = super::cancel::PrecommitCancellationGuard::responses(
            self.record.as_ref(),
            self.frontend_wire.protocol(),
            self.in_flight.as_ref(),
        );
        let url = self.backend.endpoint("/responses");
        let upstream = match server
            .http
            .post(url)
            .headers(std::mem::take(&mut self.headers))
            .body(std::mem::take(&mut self.body))
            .send()
            .await
        {
            Ok(upstream) => upstream,
            Err(error) => {
                tracing::warn!(%error, "codex upstream request failed");
                let class = if error.is_connect() {
                    crate::policy::FailureClass::TransportBeforeAccept
                } else {
                    crate::policy::FailureClass::AmbiguousTransport
                };
                return ReadyOutcome::Failure {
                    failure: super::attempt::AttemptFailure {
                        class: Some(class),
                        mechanism: if error.is_connect() {
                            super::attempt::FailureMechanism::TransportBeforeAccept
                        } else {
                            super::attempt::FailureMechanism::AmbiguousTransport
                        },
                        status: Some(StatusCode::BAD_GATEWAY.as_u16()),
                    },
                    response: FailureResponse::Local(transport_failure(
                        self.frontend_wire.error_wire(),
                        &error,
                    )),
                    ready: self,
                    cancellation,
                };
            }
        };
        note_meters(server, self.backend.as_ref(), upstream.headers());
        self.meter_snapshot = self.backend.meters(upstream.headers());
        let response_status = upstream.status();
        match super::attempt::precommit_responses(upstream).await {
            Ok(super::attempt::PrecommitOutcome::Accepted { response, .. }) => {
                if let Some(guard) = &self.in_flight {
                    guard.streaming();
                }
                ReadyOutcome::Accepted {
                    ready: self,
                    upstream: response,
                    cancellation,
                }
            }
            Ok(super::attempt::PrecommitOutcome::Failure {
                failure,
                response: Some(response),
            }) => ReadyOutcome::Failure {
                ready: self,
                failure,
                response: FailureResponse::Upstream(response),
                cancellation,
            },
            Ok(super::attempt::PrecommitOutcome::Failure {
                failure,
                response: None,
            }) => ReadyOutcome::Failure {
                response: FailureResponse::Local(upstream_failure(
                    self.frontend_wire.error_wire(),
                    "toker could not interpret the upstream response before commitment",
                )),
                ready: self,
                failure,
                cancellation,
            },
            Err(error) => {
                tracing::warn!(%error, "upstream response failed before commitment");
                ReadyOutcome::Failure {
                    failure: super::attempt::AttemptFailure {
                        class: Some(crate::policy::FailureClass::AmbiguousTransport),
                        mechanism: super::attempt::FailureMechanism::AmbiguousTransport,
                        status: Some(response_status.as_u16()),
                    },
                    response: FailureResponse::Local(truncated_body(
                        self.frontend_wire.error_wire(),
                    )),
                    ready: self,
                    cancellation,
                }
            }
        }
    }

    pub(crate) fn frontend_wire(&self) -> CodexFrontendWire {
        self.frontend_wire
    }

    pub(crate) fn attempt_provenance(&self) -> Option<&crate::store::AttemptProvenance> {
        self.record.as_ref()?.attempt.as_ref()
    }

    pub(crate) fn record(&self) -> &Option<AnthropicRecordCtx> {
        &self.record
    }

    pub(crate) fn parts(&self) -> &axum::http::request::Parts {
        &self.parts
    }

    pub(crate) fn take_retry_state(
        &mut self,
    ) -> (
        axum::http::request::Parts,
        Option<AnthropicRecordCtx>,
        Option<InFlightGuard>,
    ) {
        (
            self.parts.clone(),
            self.record.take(),
            self.in_flight.take(),
        )
    }

    pub(crate) fn restore_terminal_state(
        &mut self,
        record: Option<AnthropicRecordCtx>,
        in_flight: Option<InFlightGuard>,
    ) {
        self.record = record;
        self.in_flight = in_flight;
    }
}

pub(crate) async fn finish(outcome: ReadyOutcome, server: Server) -> Response {
    match outcome {
        ReadyOutcome::Accepted {
            ready,
            upstream,
            mut cancellation,
        } => {
            let response = forward(ready, server, upstream).await;
            cancellation.disarm();
            response
        }
        ReadyOutcome::Failure {
            ready,
            failure,
            response,
            mut cancellation,
        } => {
            let response = match response {
                FailureResponse::Upstream(upstream) => forward(ready, server, upstream).await,
                FailureResponse::Local(response) => {
                    record_failure(ready.record.as_ref(), ready.frontend_wire, &failure);
                    response
                }
            };
            cancellation.disarm();
            response
        }
    }
}

pub(crate) fn record_failure(
    record: Option<&AnthropicRecordCtx>,
    frontend_wire: CodexFrontendWire,
    failure: &super::attempt::AttemptFailure,
) {
    let Some(record) = record else {
        return;
    };
    let error_type = match failure.class {
        Some(crate::policy::FailureClass::Authentication) => "authentication",
        Some(crate::policy::FailureClass::ModelNotFound) => "model_not_found",
        Some(crate::policy::FailureClass::RateLimit) => "rate_limit",
        Some(crate::policy::FailureClass::ProviderQuota) => "provider_quota",
        Some(crate::policy::FailureClass::Overload) => "overload",
        Some(crate::policy::FailureClass::ServerError) => "server_error",
        Some(crate::policy::FailureClass::TransportBeforeAccept) => "transport_before_accept",
        Some(crate::policy::FailureClass::AmbiguousTransport) => "ambiguous_transport",
        None => match failure.mechanism {
            super::attempt::FailureMechanism::ResponseProtocol => "response_protocol",
            super::attempt::FailureMechanism::LocalPreparation => "local_preparation",
            super::attempt::FailureMechanism::HttpStatus
            | super::attempt::FailureMechanism::InBandEvent => "upstream_error",
            super::attempt::FailureMechanism::TransportBeforeAccept => "transport_before_accept",
            super::attempt::FailureMechanism::AmbiguousTransport => "ambiguous_transport",
        },
    };
    record_codex_error(
        record,
        failure.status.unwrap_or(StatusCode::BAD_GATEWAY.as_u16()),
        error_type,
        "upstream Responses attempt failed before commitment",
        None,
        frontend_wire.protocol(),
    );
}

pub(crate) struct AttemptInput {
    pub(crate) parts: axum::http::request::Parts,
    pub(crate) attempt: Result<super::inference::InferenceAttempt, TranslateError>,
    pub(crate) record: Option<AnthropicRecordCtx>,
    pub(crate) in_flight: Option<InFlightGuard>,
    pub(crate) session_id: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) request_id: Option<String>,
    pub(crate) client_wants_json: bool,
    pub(crate) frontend_wire: CodexFrontendWire,
}

pub(crate) async fn prepare(
    server: &Server,
    input: AttemptInput,
) -> Result<ReadyAttempt, Box<Response>> {
    let AttemptInput {
        parts,
        attempt,
        mut record,
        in_flight,
        session_id,
        thread_id,
        request_id,
        client_wants_json,
        frontend_wire,
    } = input;
    let attempt = match attempt {
        Ok(attempt) => attempt,
        Err(error) => {
            return Err(Box::new(translation_error_response(
                &error,
                frontend_wire,
                client_wants_json,
            )));
        }
    };
    let backend = attempt.backend().clone();
    let binding_id = attempt.binding().id().clone();
    let effective_model = attempt.effective_model().unwrap_or_default().to_owned();
    let response_model = attempt.requested_model().to_owned();
    let canonical = attempt.canonical();
    let response_projection = ResponseProjection::for_request(&canonical);
    if matches!(frontend_wire, CodexFrontendWire::Anthropic)
        && let Some(ctx) = record.as_mut()
    {
        ctx.safeguards = attempt.safeguards_row();
    }
    let prompt_cache_key = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let thread_id = thread_id.unwrap_or_else(|| prompt_cache_key.clone());
    let request_id = request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let rendered = translate::render_codex(&canonical, &prompt_cache_key).map_err(|error| {
        Box::new(translation_error_response(
            &error,
            frontend_wire,
            client_wants_json,
        ))
    })?;
    if !rendered.report.is_empty()
        && let Some(ctx) = record.as_mut()
    {
        ctx.translation_report = Some(rendered.report);
    }
    let body = serde_json::to_vec(&rendered.value).map_err(|error| {
        tracing::error!(%error, "codex request serialisation failed");
        Box::new(plain_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "translation failed\n",
        ))
    })?;

    // Refreshing is local preparation. A failure does not start an attempt and
    // must therefore precede the coordinator's sent-target observation.
    let now = now_ms() / 1000;
    let Some(codex) = server.codex_turn.clone() else {
        return Err(Box::new(match frontend_wire {
            CodexFrontendWire::Anthropic => super::anthropic_not_configured(),
            CodexFrontendWire::OpenAiChat => super::openai_not_configured(),
            CodexFrontendWire::OpenAiResponses => super::responses_not_configured(),
        }));
    };
    let auth = codex
        .auth_for_turn(&server.http, now)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "codex auth refresh failed");
            Box::new(upstream_failure(
                frontend_wire.error_wire(),
                "toker upstream error: the codex login could not be refreshed",
            ))
        })?;
    let headers = codex.turn_headers(auth.as_ref(), &prompt_cache_key, &thread_id, &request_id);

    Ok(ReadyAttempt {
        parts,
        backend,
        binding_id,
        effective_model,
        response_model,
        response_projection,
        meter_snapshot: None,
        headers,
        body,
        record,
        in_flight,
        client_wants_json,
        frontend_wire,
    })
}

fn note_meters(server: &Server, backend: &dyn Provider, headers: &HeaderMap) {
    let Some(snapshot) = backend.meters(headers) else {
        return;
    };
    server.note_quota(backend.id(), &snapshot);
    let row = crate::store::MetersSnapshot {
        updated_ms: now_ms(),
        snapshot,
    };
    if let Err(error) = server.store.save_meters(backend.id(), &row) {
        tracing::error!(%error, "codex meter snapshot save failed");
    }
}

async fn forward(ready: ReadyAttempt, _server: Server, upstream: reqwest::Response) -> Response {
    let ReadyAttempt {
        response_model,
        response_projection,
        meter_snapshot,
        record,
        in_flight,
        client_wants_json,
        frontend_wire,
        ..
    } = ready;
    let status = upstream.status();
    let headers = upstream.headers().clone();

    if is_compressed(&headers) {
        if let Some(ctx) = record.as_ref() {
            record_codex_error(
                ctx,
                status.as_u16(),
                "response_protocol",
                "upstream Responses response was compressed",
                None,
                frontend_wire.protocol(),
            );
        }
        return upstream_failure(
            frontend_wire.error_wire(),
            "toker could not interpret a compressed upstream response",
        );
    }

    if !status.is_success() {
        let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
            if let Some(ctx) = record.as_ref() {
                record_codex_error(
                    ctx,
                    status.as_u16(),
                    "ambiguous_transport",
                    "upstream response body failed after commitment",
                    None,
                    frontend_wire.protocol(),
                );
            }
            return truncated_body(frontend_wire.error_wire());
        };
        if buffered.rest.is_some() {
            if let Some(ctx) = record.as_ref() {
                record_codex_error(
                    ctx,
                    status.as_u16(),
                    "response_protocol",
                    "upstream error response was too large",
                    None,
                    frontend_wire.protocol(),
                );
            }
            return upstream_failure(
                frontend_wire.error_wire(),
                "toker could not interpret an oversized upstream error",
            );
        }
        let Some(error) = parse_upstream_error(&buffered.bytes) else {
            if let Some(ctx) = record.as_ref() {
                record_codex_error(
                    ctx,
                    status.as_u16(),
                    "response_protocol",
                    "upstream error response could not be interpreted",
                    None,
                    frontend_wire.protocol(),
                );
            }
            return upstream_failure(
                frontend_wire.error_wire(),
                "toker could not interpret the upstream error response",
            );
        };
        let kind = translate::anthropic_error_type(&error);
        let message = error
            .message
            .clone()
            .or_else(|| error.code.clone())
            .or_else(|| error.kind.clone())
            .unwrap_or_else(|| "upstream error".to_owned());
        if let Some(ctx) = record.as_ref() {
            record_codex_error(
                ctx,
                status.as_u16(),
                kind,
                &message,
                error.resets_at,
                frontend_wire.protocol(),
            );
        }
        return frontend_error_response(frontend_wire, status, kind, &message, !client_wants_json);
    }

    if !answers_stream_request(&headers) {
        if let Some(ctx) = record.as_ref() {
            record_codex_error(
                ctx,
                status.as_u16(),
                "response_protocol",
                &format!(
                    "upstream Responses response used the wrong streaming mode ({})",
                    super::proxy::content_type_detail(&headers)
                ),
                None,
                frontend_wire.protocol(),
            );
        }
        return upstream_failure(
            frontend_wire.error_wire(),
            "toker could not interpret a non-streaming upstream response",
        );
    }

    if client_wants_json {
        aggregated_turn(
            record,
            in_flight,
            response_model,
            upstream,
            meter_snapshot,
            frontend_wire,
            response_projection,
        )
        .await
    } else {
        streamed_turn(
            record,
            in_flight,
            response_model,
            upstream,
            meter_snapshot,
            frontend_wire,
            response_projection,
        )
        .await
    }
}

/// The `stream: false` shape: consume the whole turn, answer with the
/// complete Message JSON (translate's aggregation), record once. The
/// guard drops at return — a JSON answer is over when it is sent.
async fn aggregated_turn(
    record: Option<AnthropicRecordCtx>,
    _in_flight: Option<InFlightGuard>,
    model: String,
    upstream: reqwest::Response,
    meters: Option<Value>,
    frontend_wire: CodexFrontendWire,
    response_projection: ResponseProjection,
) -> Response {
    // The turn was recorded through the ctx (which carries the server);
    // the guard drops at return — a JSON answer is over when it is sent.
    let mut sse = ResponsesSse::new();
    let mut capture = TurnCapture::new();
    let mut upstream = upstream;
    loop {
        let chunk = match upstream.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "codex stream failed mid-turn");
                if let Some(ctx) = record.as_ref() {
                    record_codex_error(
                        ctx,
                        200,
                        "ambiguous_transport",
                        "upstream response body failed after commitment",
                        None,
                        frontend_wire.protocol(),
                    );
                }
                return upstream_failure(
                    frontend_wire.error_wire(),
                    "toker upstream error: the stream failed before the turn ended",
                );
            }
        };
        for event in sse.feed(&chunk) {
            capture.observe(&event);
        }
    }
    if let Some(event) = sse.finish() {
        capture.observe(&event);
    }

    if let Some(error) = capture.error() {
        let kind = translate::anthropic_error_type(error);
        let message = error
            .message
            .clone()
            .or_else(|| error.code.clone())
            .or_else(|| error.kind.clone())
            .unwrap_or_else(|| "upstream error".to_owned());
        if let Some(ctx) = record.as_ref() {
            record_codex_error(
                ctx,
                200,
                kind,
                &message,
                error.resets_at,
                frontend_wire.protocol(),
            );
        }
        return frontend_error_response(frontend_wire, StatusCode::OK, kind, &message, false);
    }
    if !capture.turn_ended() {
        // The stream closed before a terminator event (the codex client
        // treats this as an error; so does toker — never a partial row).
        tracing::warn!("codex stream closed before response.completed");
        if let Some(ctx) = record.as_ref() {
            record_codex_error(
                ctx,
                200,
                "response_protocol",
                "upstream Responses stream ended before completion",
                None,
                frontend_wire.protocol(),
            );
        }
        return upstream_failure(
            frontend_wire.error_wire(),
            "toker upstream error: the stream failed before the turn ended",
        );
    }

    let mut turn = translate::codex_backend::canonical_turn_from_capture(&capture);
    response_projection.apply(&mut turn);
    let message = match frontend_wire {
        CodexFrontendWire::Anthropic => {
            translate::anthropic_frontend::anthropic_from_canonical(&model, &turn)
        }
        CodexFrontendWire::OpenAiChat => translate::openai_chat_from_canonical(&model, &turn),
        CodexFrontendWire::OpenAiResponses => {
            translate::openai_responses_from_canonical(&model, &turn)
        }
    };
    if let Some(ctx) = record.as_ref() {
        record_responses_measurement(ctx, &capture, meters, 200, frontend_wire.protocol(), None);
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&message).unwrap_or_default()))
        .expect("a plain JSON body always builds")
}

/// The streaming shape: translate-through, one client event per upstream
/// event, the in-flight guard riding the stream (dropped when the client
/// goes away or the turn completes, whichever comes first). The attempt
/// records its terminal state: completion, client cancellation, or upstream
/// failure. An upstream that fails mid-turn (a reset, the
/// idle timeout, or a close before the turn's terminator) aborts the
/// client's response after the events already translated: the
/// translation must not end cleanly on a turn the upstream never
/// finished, or the client takes the truncated turn as complete.
async fn streamed_turn(
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    model: String,
    upstream: reqwest::Response,
    meters: Option<Value>,
    frontend_wire: CodexFrontendWire,
    response_projection: ResponseProjection,
) -> Response {
    let state = StreamState {
        upstream,
        sse: ResponsesSse::new(),
        frontend: FrontendStream::new(frontend_wire, &model),
        capture: TurnCapture::new(),
        pending: VecDeque::new(),
        done: false,
        failure: None,
        record,
        meters,
        in_flight,
        frontend_wire,
        response_projection,
    };

    let stream = futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(bytes) = state.pending.pop_front() {
                return Some((Ok(bytes), state));
            }
            if state.done {
                // A failure is yielded once, after the events that did
                // arrive, so hyper aborts the response; then the end.
                return state.failure.take().map(|error| (Err(error), state));
            }
            match state.upstream.chunk().await {
                Ok(Some(chunk)) => {
                    for event in state.sse.feed(&chunk) {
                        state.observe(&event);
                    }
                    if state.capture.turn_ended() {
                        state.finish();
                    }
                }
                Ok(None) => {
                    if let Some(event) = state.sse.finish() {
                        state.observe(&event);
                    }
                    if state.capture.turn_ended() || state.capture.error().is_some() {
                        state.finish();
                    } else {
                        // Closed before the terminator: the codex client
                        // treats this as an error, and so does the
                        // aggregated shape (502) — never a clean end.
                        tracing::warn!("codex stream closed before response.completed");
                        state.fail(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "codex stream closed before response.completed",
                        ));
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "codex stream failed mid-turn");
                    state.fail(std::io::Error::other(error));
                }
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))
        .body(Body::from_stream(stream))
        .expect("a plain SSE body always builds")
}

/// The unfold state: one upstream connection, two observers (the
/// frontend renderer for the client, the capture for the ledger), the
/// bytes awaiting delivery, and the in-flight guard that dies with it.
struct StreamState {
    upstream: reqwest::Response,
    sse: ResponsesSse,
    frontend: FrontendStream,
    capture: TurnCapture,
    pending: VecDeque<Bytes>,
    done: bool,
    /// The upstream failure still to be yielded to the client, once the
    /// pending events are out.
    failure: Option<std::io::Error>,
    record: Option<AnthropicRecordCtx>,
    meters: Option<Value>,
    /// The guard's Drop (when the client goes away, the turn completes,
    /// or the upstream fails) is the point — the in-flight count must
    /// live exactly as long as the exchange does.
    in_flight: Option<InFlightGuard>,
    frontend_wire: CodexFrontendWire,
    response_projection: ResponseProjection,
}

enum FrontendStream {
    Anthropic(translate::AnthropicStream),
    OpenAiChat {
        canonical: translate::codex_backend::CanonStream,
        renderer: translate::OpenAiChatRenderer,
    },
    OpenAiResponses {
        canonical: translate::codex_backend::CanonStream,
        renderer: translate::OpenAiResponsesRenderer,
    },
}

impl FrontendStream {
    fn new(wire: CodexFrontendWire, model: &str) -> FrontendStream {
        match wire {
            CodexFrontendWire::Anthropic => {
                FrontendStream::Anthropic(translate::AnthropicStream::new(model))
            }
            CodexFrontendWire::OpenAiChat => FrontendStream::OpenAiChat {
                canonical: translate::codex_backend::CanonStream::new(),
                renderer: translate::OpenAiChatRenderer::new(model),
            },
            CodexFrontendWire::OpenAiResponses => FrontendStream::OpenAiResponses {
                canonical: translate::codex_backend::CanonStream::new(),
                renderer: translate::OpenAiResponsesRenderer::new(model),
            },
        }
    }

    fn feed(
        &mut self,
        event: &ResponseEvent,
        response_projection: ResponseProjection,
    ) -> Vec<SseEvent> {
        match self {
            FrontendStream::Anthropic(stream) => stream.feed_projected(event, response_projection),
            FrontendStream::OpenAiChat {
                canonical,
                renderer,
            } => canonical
                .feed(event)
                .iter()
                .filter(|event| response_projection.allows(event))
                .flat_map(|event| renderer.feed(event))
                .collect(),
            FrontendStream::OpenAiResponses {
                canonical,
                renderer,
            } => canonical
                .feed(event)
                .iter()
                .filter(|event| response_projection.allows(event))
                .flat_map(|event| renderer.feed(event))
                .collect(),
        }
    }
}

impl StreamState {
    /// One upstream event through both observers: the frontend renderer
    /// emits the client's events, the capture latches the ledger's.
    fn observe(&mut self, event: &ResponseEvent) {
        self.capture.observe(event);
        for emitted in self.frontend.feed(event, self.response_projection) {
            self.pending.push_back(sse_bytes(&emitted));
        }
    }

    /// The upstream failed mid-turn: record the physical attempt, release the
    /// in-flight hold now, and queue the error for the client.
    fn fail(&mut self, error: std::io::Error) {
        self.done = true;
        if let Some(ctx) = self.record.take() {
            let error_type = if error.kind() == std::io::ErrorKind::UnexpectedEof {
                "response_protocol"
            } else {
                "ambiguous_transport"
            };
            record_codex_error(
                &ctx,
                200,
                error_type,
                "upstream Responses stream ended before completion",
                None,
                self.frontend_wire.protocol(),
            );
        }
        self.failure = Some(error);
        drop(self.in_flight.take());
    }

    /// The turn ended: write the row, close out.
    fn finish(&mut self) {
        self.done = true;
        let Some(ctx) = self.record.as_ref() else {
            return;
        };
        if let Some(error) = self.capture.error() {
            let kind = translate::anthropic_error_type(error);
            let message = error
                .message
                .clone()
                .or_else(|| error.code.clone())
                .or_else(|| error.kind.clone())
                .unwrap_or_else(|| "upstream error".to_owned());
            record_codex_error(
                ctx,
                200,
                kind,
                &message,
                error.resets_at,
                self.frontend_wire.protocol(),
            );
        } else if self.capture.turn_ended() {
            record_responses_measurement(
                ctx,
                &self.capture,
                self.meters.clone(),
                200,
                self.frontend_wire.protocol(),
                None,
            );
        }
        self.record.take();
    }
}

impl Drop for StreamState {
    fn drop(&mut self) {
        if let Some(ctx) = self.record.take() {
            record_codex_error(
                &ctx,
                200,
                "client_cancelled",
                "client disconnected before the Responses turn completed",
                None,
                self.frontend_wire.protocol(),
            );
        }
    }
}

/// One emitted frontend SSE event to its wire bytes.
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

/// The upstream's HTTP error body → a typed [`ResponseError`] for the
/// mapping table. Two shapes exist on the wire (both verified live): the
/// `{"error": {...}}` wrapper the SSE error events carry, and the
/// FastAPI-style `{"detail": "..."}` the backend's request-validation
/// 400s use. An unparseable or unrecognised body is a protocol failure;
/// it must not become a fabricated generic provider error.
fn parse_upstream_error(body: &[u8]) -> Option<ResponseError> {
    let wrapper = serde_json::from_slice::<Value>(body).ok()?;
    let value = if let Some(error) = wrapper.get("error") {
        error.clone()
    } else {
        let detail = wrapper.get("detail")?.clone();
        // FastAPI's string detail is itself a recognised error shape.
        if let Some(message) = detail.as_str() {
            return Some(ResponseError {
                kind: None,
                code: None,
                message: Some(message.to_owned()),
                resets_at: None,
            });
        }
        detail
    };
    let error = serde_json::from_value::<ResponseError>(value).ok()?;
    (error.kind.is_some() || error.code.is_some()).then_some(error)
}

/// The refusal for a request that cannot be translated for this backend. It
/// is marked, so the coordinator of the first attempt writes the ledger row
/// (the request has no attempt to attach it to).
fn translation_error_response(
    error: &TranslateError,
    frontend_wire: CodexFrontendWire,
    client_wants_json: bool,
) -> Response {
    let (status, message) = translate_failure(error);
    super::record::refused(frontend_error_response(
        frontend_wire,
        status,
        "invalid_request_error",
        &message,
        !client_wants_json,
    ))
}

/// A translation failure → the HTTP status and message the client sees.
fn translate_failure(error: &TranslateError) -> (StatusCode, String) {
    match error {
        TranslateError::UnsupportedBlock { kind } => (
            StatusCode::BAD_REQUEST,
            format!(
                "this backend does not accept {kind} content blocks; \
                 the request could not be translated"
            ),
        ),
        TranslateError::Malformed { reason } => (
            StatusCode::BAD_REQUEST,
            format!("the request could not be translated: {reason}"),
        ),
    }
}

/// An anthropic error in the shape the client asked for (SSE error
/// event when streaming, the JSON error object otherwise).
pub(crate) fn anthropic_error_response(
    status: StatusCode,
    kind: &str,
    message: &str,
    stream: bool,
) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": kind, "message": message },
    });
    if stream {
        let mut bytes = String::new();
        bytes.push_str("event: error\ndata: ");
        bytes.push_str(&serde_json::to_string(&body).unwrap_or_default());
        bytes.push_str("\n\n");
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(bytes))
            .expect("a plain SSE body always builds")
    } else {
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap_or_default()))
            .expect("a plain JSON body always builds")
    }
}

/// A local error in the shape of the wire the client speaks, which for a
/// stream is that wire's own error frame: Messages and Responses send an
/// `event: error`, Chat a bare `data:` chunk.
pub(crate) fn frontend_error_response(
    wire: CodexFrontendWire,
    status: StatusCode,
    kind: &str,
    message: &str,
    stream: bool,
) -> Response {
    let (json, frame) = match wire {
        CodexFrontendWire::Anthropic => {
            return anthropic_error_response(status, kind, message, stream);
        }
        CodexFrontendWire::OpenAiChat => {
            let json = serde_json::json!({
                "error": {"message": message, "type": kind, "param": null, "code": null}
            });
            (json.clone(), json)
        }
        CodexFrontendWire::OpenAiResponses => (
            serde_json::json!({"error": {"type": kind, "message": message}}),
            serde_json::json!({
                "type": "error",
                "sequence_number": 0,
                "error": {"type": kind, "code": kind, "message": message},
            }),
        ),
    };
    let (content_type, body) = if stream {
        let frame = serde_json::to_string(&frame).unwrap_or_else(|_| "{}".to_owned());
        let event = if wire == CodexFrontendWire::OpenAiResponses {
            "event: error\n"
        } else {
            ""
        };
        ("text/event-stream", format!("{event}data: {frame}\n\n"))
    } else {
        (
            "application/json",
            serde_json::to_string(&json).unwrap_or_else(|_| "{}".to_owned()),
        )
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("a plain error response builds")
}
