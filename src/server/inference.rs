//! Shared immutable canonical baseline and prepared inference attempts.
//!
//! Frontend parsing and request-global gates produce one [`InferenceRequest`].
//! Each candidate is prepared from a fresh clone, then backend-specific routing
//! transforms and rendering may mutate only that private [`InferenceAttempt`].

use std::sync::Arc;

use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::Response;
use serde_json::Value;

use crate::ir::canonical::CanonicalRequest;
use crate::middleware::cold::RetargetOutcome;
use crate::middleware::model_map::{MapTarget, ModelMap};
use crate::policy::AttemptPlan;
use crate::providers::{Provider, parse_rate_limits};
use crate::routing::{BackendAdapterId, BackendBinding, ModelTarget};
use crate::translate::{Rendered, TranslateError};

use super::codex::{CodexFrontendWire, frontend_error_response};
use super::record::RecordCtx;
use super::record_anthropic::AnthropicRecordCtx;
use super::{InFlightGuard, Server};

#[derive(Clone)]
pub(crate) struct InferenceRequest {
    requested_model: String,
    canonical: Arc<CanonicalRequest>,
}

impl InferenceRequest {
    pub(crate) fn new(requested_model: String, canonical: CanonicalRequest) -> Self {
        Self {
            requested_model,
            canonical: Arc::new(canonical),
        }
    }

    /// Clone the unchanged request-global baseline for candidate-local policy.
    pub(crate) fn canonical(&self) -> CanonicalRequest {
        self.canonical.as_ref().clone()
    }

    pub(crate) fn strip_release(&mut self, original: &[u8]) -> bool {
        crate::middleware::canonical::strip_release(Arc::make_mut(&mut self.canonical), original)
    }

    pub(crate) fn prepare(
        &self,
        target: ModelTarget,
        baseline_model: Option<String>,
    ) -> InferenceAttempt {
        let mut canonical = self.canonical();
        canonical.model.clone_from(&baseline_model);
        InferenceAttempt {
            target,
            requested_model: self.requested_model.clone(),
            effective_model: baseline_model,
            policy_plan: None,
            mapped: None,
            canonical,
        }
    }

    pub(crate) fn prepare_policy(
        &self,
        target: ModelTarget,
        plan: AttemptPlan,
    ) -> InferenceAttempt {
        let mut canonical = self.canonical();
        canonical.model = Some(plan.effective_model.clone());
        InferenceAttempt {
            target,
            requested_model: self.requested_model.clone(),
            effective_model: Some(plan.effective_model.clone()),
            policy_plan: Some(plan),
            mapped: None,
            canonical,
        }
    }
}

pub(crate) struct InferenceAttempt {
    target: ModelTarget,
    requested_model: String,
    effective_model: Option<String>,
    policy_plan: Option<AttemptPlan>,
    mapped: Option<MapTarget>,
    canonical: CanonicalRequest,
}

pub(crate) struct MessagesAttemptInput {
    pub(crate) parts: Parts,
    pub(crate) record: Option<AnthropicRecordCtx>,
    pub(crate) in_flight: Option<InFlightGuard>,
    pub(crate) session_id: Option<String>,
    pub(crate) client_wants_json: bool,
}

pub(crate) struct ChatAttemptInput {
    pub(crate) parts: Parts,
    pub(crate) max_tokens: Option<Value>,
    pub(crate) record: Option<RecordCtx>,
    pub(crate) in_flight: Option<InFlightGuard>,
    pub(crate) session_id: Option<String>,
    pub(crate) client_wants_json: bool,
}

pub(crate) struct ResponsesAttemptInput {
    pub(crate) parts: Parts,
    pub(crate) max_output_tokens: Option<Value>,
    pub(crate) had_prompt_cache_key: bool,
    pub(crate) record: Option<AnthropicRecordCtx>,
    pub(crate) in_flight: Option<InFlightGuard>,
    pub(crate) session_id: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) request_id: Option<String>,
    pub(crate) client_wants_json: bool,
}

impl InferenceAttempt {
    pub(crate) fn backend(&self) -> &Arc<dyn Provider> {
        self.target.provider()
    }

    pub(crate) fn binding(&self) -> &BackendBinding {
        self.target.binding()
    }

    pub(crate) fn requested_model(&self) -> &str {
        &self.requested_model
    }

    pub(crate) fn effective_model(&self) -> Option<&str> {
        self.effective_model.as_deref()
    }

    pub(crate) fn mapped(&self) -> Option<&MapTarget> {
        self.mapped.as_ref()
    }

    pub(crate) fn policy_routing(
        &self,
    ) -> crate::store::Result<Option<crate::store::PolicyRouteProvenance>> {
        self.policy_plan
            .as_ref()
            .map(crate::store::PolicyRouteProvenance::from_attempt_plan)
            .transpose()
            .map(Option::flatten)
    }

    pub(crate) fn apply_compaction(
        &mut self,
        compaction_model: Option<&str>,
        model_map: Option<&ModelMap>,
    ) -> Option<RetargetOutcome> {
        let outcome = crate::middleware::canonical::retarget_compaction(
            &mut self.canonical,
            compaction_model,
            true,
            model_map,
        )?;
        self.effective_model.clone_from(&self.canonical.model);
        Some(outcome)
    }

    pub(crate) fn set_model(&mut self, model: Option<String>) {
        self.canonical.model.clone_from(&model);
        self.effective_model = model;
        self.mapped = None;
    }

    pub(crate) fn apply_model_map(&mut self, model_map: Option<&ModelMap>) {
        self.mapped = crate::middleware::canonical::map_model(&mut self.canonical, model_map);
        self.effective_model.clone_from(&self.canonical.model);
    }

    pub(crate) fn strip_message_effort(&mut self, source: &serde_json::Value) -> bool {
        crate::middleware::canonical::strip_message_effort(&mut self.canonical, source)
    }

    pub(crate) fn stream(&self) -> Option<bool> {
        self.canonical.stream
    }

    pub(crate) fn prepare_messages_transport(&mut self, stream: bool, max_tokens: u64) {
        self.canonical.stream = Some(stream);
        self.canonical.sampling.max_tokens = Some(max_tokens);
        self.canonical
            .extensions
            .retain(|extension| extension.wire_path() != "$.max_output_tokens");
    }

    pub(crate) fn render_json(&self) -> Result<Rendered<serde_json::Value>, TranslateError> {
        let canonical = &self.canonical;
        match self
            .binding()
            .canonical_backend()
            .map(|backend| backend.adapter())
        {
            Some(BackendAdapterId::OpenAiChatCompletions) => {
                crate::translate::openai_chat_backend::render_openai_chat(
                    canonical,
                    self.binding().dialect(),
                )
            }
            Some(BackendAdapterId::AnthropicMessages) => {
                crate::translate::render_anthropic(canonical, self.binding().dialect())
            }
            _ => Err(TranslateError::Malformed {
                reason: format!("{} has no JSON attempt renderer", self.binding().id()),
            }),
        }
    }

    /// Clone this candidate's private canonical IR for one backend renderer.
    pub(crate) fn canonical(&self) -> CanonicalRequest {
        let mut canonical = self.canonical.clone();
        canonical.model.clone_from(&self.effective_model);
        canonical
    }
}

struct ReadyMessagesAttempt {
    parts: Parts,
    backend: Arc<dyn Provider>,
    binding_id: crate::policy::BindingId,
    effective_model: String,
    response_model: String,
    response_projection: crate::ir::canonical::ResponseProjection,
    expected_stream: bool,
    body: bytes::Bytes,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
}

impl ReadyMessagesAttempt {
    fn binding_id(&self) -> &crate::policy::BindingId {
        &self.binding_id
    }

    fn effective_model(&self) -> &str {
        &self.effective_model
    }

    async fn execute(
        mut self,
        server: Server,
        provenance: crate::store::AttemptProvenance,
    ) -> Response {
        if let Some(ctx) = self.record.as_mut() {
            ctx.attempt = Some(provenance);
        }
        let mut cancellation =
            super::cancel::PrecommitCancellationGuard::anthropic(self.record.as_ref());
        let sent = self.body.clone();
        let upstream = match super::proxy::send_upstream(
            &server,
            self.backend.as_ref(),
            &self.parts,
            self.body,
            &[],
        )
        .await
        {
            Ok(upstream) => upstream,
            Err(error) => {
                cancellation.disarm();
                tracing::warn!(%error, "upstream request failed");
                if let Some(ctx) = self.record.as_ref() {
                    super::record_anthropic::record_anthropic_error(
                        ctx,
                        StatusCode::BAD_GATEWAY.as_u16(),
                        Some(super::attempt::transport_failure_type(&error).to_owned()),
                        None,
                        None,
                        None,
                    );
                }
                return super::proxy::transport_failure(super::proxy::ErrorWire::Anthropic, &error);
            }
        };
        super::anthropic::note_meters(&server, self.backend.as_ref(), upstream.headers());
        let retry = match super::anthropic::retry_thinking_off(
            &server,
            self.backend.as_ref(),
            &self.parts,
            sent,
            upstream,
            &mut self.record,
            &mut cancellation,
        )
        .await
        {
            Ok(retry) => retry,
            Err(response) => {
                cancellation.disarm();
                return *response;
            }
        };
        cancellation.update_anthropic(self.record.as_ref());
        let response = match retry {
            super::anthropic::ThinkingRetry::Original(upstream) => {
                let status = upstream.status();
                let rate_limits = parse_rate_limits(upstream.headers());
                (
                    super::attempt::precommit_anthropic(upstream).await,
                    rate_limits,
                    status,
                )
            }
            super::anthropic::ThinkingRetry::Retried(upstream) => {
                super::anthropic::note_meters(&server, self.backend.as_ref(), upstream.headers());
                let status = upstream.status();
                let rate_limits = parse_rate_limits(upstream.headers());
                (
                    super::attempt::precommit_anthropic(upstream).await,
                    rate_limits,
                    status,
                )
            }
            super::anthropic::ThinkingRetry::Inspected(outcome) => {
                let (rate_limits, status) = match &outcome {
                    super::attempt::PrecommitOutcome::Accepted { response, .. }
                    | super::attempt::PrecommitOutcome::Failure {
                        response: Some(response),
                        ..
                    } => (parse_rate_limits(response.headers()), response.status()),
                    _ => unreachable!("thinking inspection always retains its response"),
                };
                (Ok(outcome), rate_limits, status)
            }
        };
        let (outcome, rate_limits, response_status) = response;
        let upstream = match outcome {
            Ok(super::attempt::PrecommitOutcome::Accepted { response, .. })
            | Ok(super::attempt::PrecommitOutcome::Failure {
                response: Some(response),
                ..
            }) => response,
            Ok(super::attempt::PrecommitOutcome::Failure { response: None, .. }) => {
                cancellation.disarm();
                if let Some(ctx) = self.record.as_ref() {
                    super::record_anthropic::record_anthropic_error(
                        ctx,
                        response_status.as_u16(),
                        Some("response_protocol".to_owned()),
                        None,
                        None,
                        rate_limits.as_ref(),
                    );
                }
                return super::proxy::upstream_failure(
                    super::proxy::ErrorWire::Anthropic,
                    "toker could not interpret the upstream response before commitment",
                );
            }
            Err(error) => {
                cancellation.disarm();
                tracing::warn!(%error, "upstream response failed before commitment");
                if let Some(ctx) = self.record.as_ref() {
                    super::record_anthropic::record_anthropic_error(
                        ctx,
                        response_status.as_u16(),
                        Some("ambiguous_transport".to_owned()),
                        None,
                        None,
                        rate_limits.as_ref(),
                    );
                }
                return super::proxy::truncated_body(super::proxy::ErrorWire::Anthropic);
            }
        };
        let response = super::anthropic::forward_canonical_messages(
            upstream,
            self.record,
            self.in_flight,
            &self.response_model,
            self.response_projection,
            self.expected_stream,
            rate_limits,
        )
        .await;
        cancellation.disarm();
        response
    }
}

enum ChatFailureResponse {
    Upstream(reqwest::Response),
    Local(Response),
}

enum ReadyChatOutcome {
    Accepted {
        upstream: reqwest::Response,
        record: Option<RecordCtx>,
        in_flight: Option<InFlightGuard>,
        response_model: String,
        expected_stream: bool,
        cancellation: super::cancel::PrecommitCancellationGuard,
    },
    Failure {
        failure: super::attempt::AttemptFailure,
        response: ChatFailureResponse,
        record: Option<RecordCtx>,
        in_flight: Option<InFlightGuard>,
        response_model: String,
        expected_stream: bool,
        cancellation: super::cancel::PrecommitCancellationGuard,
    },
}

struct ReadyChatAttempt {
    parts: Parts,
    backend: Arc<dyn Provider>,
    binding_id: crate::policy::BindingId,
    effective_model: String,
    response_model: String,
    expected_stream: bool,
    body: bytes::Bytes,
    record: Option<RecordCtx>,
    in_flight: Option<InFlightGuard>,
}

impl ReadyChatAttempt {
    fn binding_id(&self) -> &crate::policy::BindingId {
        &self.binding_id
    }

    fn effective_model(&self) -> &str {
        &self.effective_model
    }

    async fn execute_precommit(
        mut self,
        server: &Server,
        provenance: crate::store::AttemptProvenance,
    ) -> ReadyChatOutcome {
        if let Some(ctx) = self.record.as_mut() {
            ctx.attempt = Some(provenance);
        }
        let cancellation = super::cancel::PrecommitCancellationGuard::chat(self.record.as_ref());
        let upstream = match super::proxy::send_upstream(
            server,
            self.backend.as_ref(),
            &self.parts,
            self.body,
            &[],
        )
        .await
        {
            Ok(upstream) => upstream,
            Err(error) => {
                tracing::warn!(%error, "upstream request failed");
                let class = if error.is_connect() {
                    crate::policy::FailureClass::TransportBeforeAccept
                } else {
                    crate::policy::FailureClass::AmbiguousTransport
                };
                return ReadyChatOutcome::Failure {
                    failure: super::attempt::AttemptFailure {
                        class: Some(class),
                        mechanism: if error.is_connect() {
                            super::attempt::FailureMechanism::TransportBeforeAccept
                        } else {
                            super::attempt::FailureMechanism::AmbiguousTransport
                        },
                        status: Some(StatusCode::BAD_GATEWAY.as_u16()),
                    },
                    response: ChatFailureResponse::Local(super::proxy::transport_failure(
                        super::proxy::ErrorWire::Openai,
                        &error,
                    )),
                    record: self.record,
                    in_flight: self.in_flight,
                    response_model: self.response_model,
                    expected_stream: self.expected_stream,
                    cancellation,
                };
            }
        };
        let response_status = upstream.status();
        match super::attempt::precommit_openai_chat(upstream).await {
            Ok(super::attempt::PrecommitOutcome::Accepted { response, .. }) => {
                ReadyChatOutcome::Accepted {
                    upstream: response,
                    record: self.record,
                    in_flight: self.in_flight,
                    response_model: self.response_model,
                    expected_stream: self.expected_stream,
                    cancellation,
                }
            }
            Ok(super::attempt::PrecommitOutcome::Failure {
                failure,
                response: Some(response),
            }) => ReadyChatOutcome::Failure {
                failure,
                response: ChatFailureResponse::Upstream(response),
                record: self.record,
                in_flight: self.in_flight,
                response_model: self.response_model,
                expected_stream: self.expected_stream,
                cancellation,
            },
            Ok(super::attempt::PrecommitOutcome::Failure {
                failure,
                response: None,
            }) => ReadyChatOutcome::Failure {
                failure,
                response: ChatFailureResponse::Local(super::proxy::upstream_failure(
                    super::proxy::ErrorWire::Openai,
                    "toker could not interpret the upstream response before commitment",
                )),
                record: self.record,
                in_flight: self.in_flight,
                response_model: self.response_model,
                expected_stream: self.expected_stream,
                cancellation,
            },
            Err(error) => {
                tracing::warn!(%error, "upstream response failed before commitment");
                ReadyChatOutcome::Failure {
                    failure: super::attempt::AttemptFailure {
                        class: Some(crate::policy::FailureClass::AmbiguousTransport),
                        mechanism: super::attempt::FailureMechanism::AmbiguousTransport,
                        status: Some(response_status.as_u16()),
                    },
                    response: ChatFailureResponse::Local(super::proxy::truncated_body(
                        super::proxy::ErrorWire::Openai,
                    )),
                    record: self.record,
                    in_flight: self.in_flight,
                    response_model: self.response_model,
                    expected_stream: self.expected_stream,
                    cancellation,
                }
            }
        }
    }
}

fn record_chat_attempt_failure(
    record: Option<&RecordCtx>,
    failure: &super::attempt::AttemptFailure,
    response: &ChatFailureResponse,
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
    let retry_after = match response {
        ChatFailureResponse::Upstream(response) => {
            super::record::retry_after_ms(response.headers())
        }
        ChatFailureResponse::Local(_) => None,
    };
    super::record::record_error(
        record,
        failure.status.unwrap_or(StatusCode::BAD_GATEWAY.as_u16()),
        Some(error_type.to_owned()),
        retry_after,
    );
}

enum ReadyBackendAttempt {
    Messages(ReadyMessagesAttempt),
    Chat(ReadyChatAttempt),
    Anthropic(super::anthropic_target::ReadyAttempt),
    OpenRouter(super::openrouter_responses::ReadyAttempt),
    Codex(super::codex::ReadyAttempt),
}

impl ReadyBackendAttempt {
    fn binding_id(&self) -> &crate::policy::BindingId {
        match self {
            Self::Messages(ready) => ready.binding_id(),
            Self::Chat(ready) => ready.binding_id(),
            Self::Anthropic(ready) => ready.binding_id(),
            Self::OpenRouter(ready) => ready.binding_id(),
            Self::Codex(ready) => ready.binding_id(),
        }
    }

    fn effective_model(&self) -> &str {
        match self {
            Self::Messages(ready) => ready.effective_model(),
            Self::Chat(ready) => ready.effective_model(),
            Self::Anthropic(ready) => ready.effective_model(),
            Self::OpenRouter(ready) => ready.effective_model(),
            Self::Codex(ready) => ready.effective_model(),
        }
    }

    async fn execute(
        self,
        server: Server,
        provenance: crate::store::AttemptProvenance,
    ) -> Response {
        match self {
            Self::Messages(ready) => ready.execute(server, provenance).await,
            Self::Chat(ready) => match ready.execute_precommit(&server, provenance).await {
                ReadyChatOutcome::Accepted {
                    upstream,
                    record,
                    in_flight,
                    response_model,
                    expected_stream,
                    mut cancellation,
                } => {
                    let response = super::proxy::forward_chat_canonical(
                        upstream,
                        record,
                        in_flight,
                        &response_model,
                        expected_stream,
                    )
                    .await;
                    cancellation.disarm();
                    response
                }
                ReadyChatOutcome::Failure {
                    failure,
                    response,
                    record,
                    in_flight,
                    response_model,
                    expected_stream,
                    mut cancellation,
                } => {
                    let response = match response {
                        ChatFailureResponse::Upstream(response) => {
                            super::proxy::forward_chat_canonical(
                                response,
                                record,
                                in_flight,
                                &response_model,
                                expected_stream,
                            )
                            .await
                        }
                        response @ ChatFailureResponse::Local(_) => {
                            record_chat_attempt_failure(record.as_ref(), &failure, &response);
                            let ChatFailureResponse::Local(response) = response else {
                                unreachable!()
                            };
                            response
                        }
                    };
                    cancellation.disarm();
                    response
                }
            },
            Self::Anthropic(ready) => ready.execute(server, provenance).await,
            Self::OpenRouter(ready) => ready.execute(server, provenance).await,
            Self::Codex(ready) => ready.execute(server, provenance).await,
        }
    }
}

fn prepare_messages(
    parts: Parts,
    attempt: InferenceAttempt,
    mut record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
) -> Result<ReadyMessagesAttempt, Box<Response>> {
    let effective_model = attempt.effective_model().unwrap_or_default().to_owned();
    let response_model = attempt.requested_model().to_owned();
    let expected_stream = attempt.stream().unwrap_or(false);
    let response_projection =
        crate::ir::canonical::ResponseProjection::for_request(&attempt.canonical());
    let rendered = attempt.render_json().map_err(|error| {
        let stream = attempt.stream().unwrap_or(false);
        Box::new(super::codex::anthropic_error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!("the request could not be translated: {error}"),
            stream,
        ))
    })?;
    if !rendered.report.is_empty()
        && let Some(ctx) = record.as_mut()
    {
        ctx.translation_report = Some(rendered.report);
    }
    let body = serde_json::to_vec(&rendered.value)
        .map(bytes::Bytes::from)
        .map_err(|error| {
            tracing::error!(%error, "canonical Messages request serialisation failed");
            Box::new(super::proxy::plain_status(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "translation failed\n",
            ))
        })?;
    Ok(ReadyMessagesAttempt {
        parts,
        backend: attempt.backend().clone(),
        binding_id: attempt.binding().id().clone(),
        effective_model,
        response_model,
        response_projection,
        expected_stream,
        body,
        record,
        in_flight,
    })
}

fn prepare_chat(
    parts: Parts,
    attempt: InferenceAttempt,
    record: Option<RecordCtx>,
    in_flight: Option<InFlightGuard>,
) -> Result<ReadyChatAttempt, Box<Response>> {
    let effective_model = attempt.effective_model().unwrap_or_default().to_owned();
    let response_model = attempt.requested_model().to_owned();
    let expected_stream = attempt.stream().unwrap_or(false);
    let rendered = attempt
        .render_json()
        .map_err(|error| Box::new(super::proxy::compatibility_error(&error.to_string())))?;
    let body = serde_json::to_vec(&rendered.value)
        .map(bytes::Bytes::from)
        .map_err(|error| {
            tracing::error!(%error, "canonical chat request serialisation failed");
            Box::new(super::proxy::plain_status(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "translation failed\n",
            ))
        })?;
    Ok(ReadyChatAttempt {
        parts,
        backend: attempt.backend().clone(),
        binding_id: attempt.binding().id().clone(),
        effective_model,
        response_model,
        expected_stream,
        body,
        record,
        in_flight,
    })
}

pub(crate) async fn coordinate_messages(
    server: Server,
    adapter: Option<BackendAdapterId>,
    attempt: Result<InferenceAttempt, TranslateError>,
    input: MessagesAttemptInput,
) -> Response {
    let MessagesAttemptInput {
        parts,
        mut record,
        in_flight,
        session_id,
        client_wants_json,
    } = input;
    let policy_routing = attempt
        .as_ref()
        .ok()
        .map(InferenceAttempt::policy_routing)
        .transpose()
        .unwrap_or_else(|error| {
            tracing::error!(%error, "policy attempt provenance omitted");
            None
        })
        .flatten();
    if let Some(ctx) = record.as_mut() {
        ctx.policy_routing = policy_routing;
    }
    if let Err(error @ TranslateError::Malformed { .. }) = &attempt {
        return super::codex::anthropic_error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!("the request could not be translated: {error}"),
            false,
        );
    }
    let prepared = match adapter {
        Some(BackendAdapterId::AnthropicMessages) => {
            let attempt = match attempt {
                Ok(attempt) => attempt,
                Err(error) => {
                    return super::codex::anthropic_error_response(
                        axum::http::StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("the request could not be translated: {error}"),
                        false,
                    );
                }
            };
            prepare_messages(parts, attempt, record, in_flight).map(ReadyBackendAttempt::Messages)
        }
        Some(BackendAdapterId::CodexResponses) => super::codex::prepare(
            &server,
            super::codex::AttemptInput {
                attempt,
                record,
                in_flight,
                session_id,
                thread_id: None,
                request_id: None,
                client_wants_json,
                frontend_wire: CodexFrontendWire::Anthropic,
            },
        )
        .await
        .map(ReadyBackendAttempt::Codex),
        _ => Err(Box::new(super::anthropic_not_configured())),
    };
    let ready = match prepared {
        Ok(ready) => ready,
        Err(response) => return *response,
    };
    server.note_binding_sent(ready.binding_id(), Some(ready.effective_model()));
    ready
        .execute(server, super::attempt::initial_provenance())
        .await
}

pub(crate) async fn coordinate_chat(
    server: Server,
    adapter: Option<BackendAdapterId>,
    attempt: InferenceAttempt,
    input: ChatAttemptInput,
) -> Response {
    let ChatAttemptInput {
        parts,
        max_tokens,
        mut record,
        in_flight,
        session_id,
        client_wants_json,
    } = input;
    let policy_routing = attempt.policy_routing().unwrap_or_else(|error| {
        tracing::error!(%error, "policy attempt provenance omitted");
        None
    });
    if let Some(ctx) = record.as_mut() {
        ctx.policy_routing = policy_routing;
    }
    let prepared = match adapter {
        Some(BackendAdapterId::OpenAiChatCompletions) => {
            prepare_chat(parts, attempt, record, in_flight).map(ReadyBackendAttempt::Chat)
        }
        Some(BackendAdapterId::AnthropicMessages) => {
            let backend = attempt.backend().clone();
            super::anthropic_target::prepare(
                &server,
                parts,
                attempt,
                max_tokens,
                record.map(|ctx| super::proxy::chat_cross_record(ctx, backend)),
                in_flight,
                super::anthropic_target::FrontendWire::Chat,
            )
            .map(ReadyBackendAttempt::Anthropic)
        }
        Some(BackendAdapterId::CodexResponses) => {
            let backend = attempt.backend().clone();
            super::codex::prepare(
                &server,
                super::codex::AttemptInput {
                    attempt: Ok(attempt),
                    record: record.map(|ctx| super::proxy::chat_cross_record(ctx, backend)),
                    in_flight,
                    session_id,
                    thread_id: None,
                    request_id: None,
                    client_wants_json,
                    frontend_wire: CodexFrontendWire::OpenAiChat,
                },
            )
            .await
            .map(ReadyBackendAttempt::Codex)
        }
        _ => Err(Box::new(super::openai_not_configured())),
    };
    let ready = match prepared {
        Ok(ready) => ready,
        Err(response) => return *response,
    };
    server.note_binding_sent(ready.binding_id(), Some(ready.effective_model()));
    ready
        .execute(server, super::attempt::initial_provenance())
        .await
}

pub(crate) async fn coordinate_responses(
    server: Server,
    adapter: Option<BackendAdapterId>,
    attempt: Result<InferenceAttempt, TranslateError>,
    input: ResponsesAttemptInput,
) -> Response {
    let ResponsesAttemptInput {
        parts,
        max_output_tokens,
        had_prompt_cache_key,
        mut record,
        in_flight,
        session_id,
        thread_id,
        request_id,
        client_wants_json,
    } = input;
    let policy_routing = attempt
        .as_ref()
        .ok()
        .map(InferenceAttempt::policy_routing)
        .transpose()
        .unwrap_or_else(|error| {
            tracing::error!(%error, "policy attempt provenance omitted");
            None
        })
        .flatten();
    if let Some(ctx) = record.as_mut() {
        ctx.policy_routing = policy_routing;
    }
    let prepared = match adapter {
        Some(BackendAdapterId::AnthropicMessages) => {
            let attempt = match attempt {
                Ok(attempt) => attempt,
                Err(_) => return invalid_responses_body(client_wants_json),
            };
            super::anthropic_target::prepare(
                &server,
                parts,
                attempt,
                max_output_tokens,
                record,
                in_flight,
                super::anthropic_target::FrontendWire::Responses,
            )
            .map(ReadyBackendAttempt::Anthropic)
        }
        Some(BackendAdapterId::OpenRouterResponses) => {
            let attempt = match attempt {
                Ok(attempt) => attempt,
                Err(_) => return invalid_responses_body(client_wants_json),
            };
            super::openrouter_responses::prepare(
                parts,
                attempt,
                had_prompt_cache_key,
                record,
                in_flight,
            )
            .map(ReadyBackendAttempt::OpenRouter)
        }
        Some(BackendAdapterId::CodexResponses) => super::codex::prepare(
            &server,
            super::codex::AttemptInput {
                attempt,
                record,
                in_flight,
                session_id,
                thread_id,
                request_id,
                client_wants_json,
                frontend_wire: CodexFrontendWire::OpenAiResponses,
            },
        )
        .await
        .map(ReadyBackendAttempt::Codex),
        _ => Err(Box::new(super::responses_not_configured())),
    };
    let ready = match prepared {
        Ok(ready) => ready,
        Err(response) => return *response,
    };
    server.note_binding_sent(ready.binding_id(), Some(ready.effective_model()));
    ready
        .execute(server, super::attempt::initial_provenance())
        .await
}

pub(crate) fn invalid_responses_body(client_wants_json: bool) -> Response {
    frontend_error_response(
        CodexFrontendWire::OpenAiResponses,
        axum::http::StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "the request body could not be parsed for translation to this backend",
        !client_wants_json,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::middleware::model_map::parse_model_map;
    use crate::providers::{OpenRouter, Provider};
    use crate::routing::{ProtocolId, RouteRegistry};

    #[test]
    fn candidates_start_from_fresh_baseline_clones_with_separate_identities() {
        let provider: Arc<dyn Provider> = Arc::new(OpenRouter::new(
            "https://openrouter.ai/api/v1".parse().unwrap(),
            None,
        ));
        let registry = RouteRegistry::new(
            [provider],
            [(ProtocolId::OpenAiChat, "openrouter".to_owned())],
        );
        let target = registry
            .resolve(ProtocolId::OpenAiChat, Some("openrouter/invented-model"))
            .unwrap();
        let mut original = CanonicalRequest {
            model: Some("openrouter/invented-model".to_owned()),
            ..CanonicalRequest::default()
        };
        original
            .system
            .push(crate::ir::canonical::CanonSystemPart::text(
                "private fixture",
            ));
        let request = InferenceRequest::new("openrouter/invented-model".to_owned(), original);

        let mut first = request.canonical();
        first.model = Some("mutated-by-first-candidate".to_owned());
        first.system.clear();
        let attempt = request.prepare(target, Some("invented-model".to_owned()));

        assert_eq!(attempt.backend().id(), "openrouter");
        assert_eq!(attempt.binding().provider(), "openrouter");
        assert_eq!(attempt.requested_model(), "openrouter/invented-model");
        assert_eq!(attempt.effective_model(), Some("invented-model"));
        let second = attempt.canonical();
        assert_eq!(second.model.as_deref(), Some("invented-model"));
        assert_eq!(second.system.len(), 1);
        let rendered = attempt.render_json().expect("chat rendering");
        assert_eq!(rendered.value["model"], "invented-model");
    }

    #[test]
    fn compaction_is_candidate_local_and_preserves_the_baseline() {
        let provider: Arc<dyn Provider> = Arc::new(OpenRouter::new(
            "https://openrouter.ai/api/v1".parse().unwrap(),
            None,
        ));
        let registry = RouteRegistry::new(
            [provider],
            [(ProtocolId::OpenAiChat, "openrouter".to_owned())],
        );
        let target = registry
            .resolve(ProtocolId::OpenAiChat, Some("openrouter/claude-opus-4-1"))
            .unwrap();
        let mut canonical = CanonicalRequest {
            model: Some("claude-opus-4-1".to_owned()),
            ..CanonicalRequest::default()
        };
        canonical.messages.push(crate::ir::canonical::CanonMessage {
            role: crate::ir::canonical::CanonRole::User,
            blocks: vec![crate::ir::canonical::CanonBlock::Text("prompt".to_owned())],
            extensions: vec![],
        });
        let request = InferenceRequest::new("openrouter/claude-opus-4-1".to_owned(), canonical);
        let mut attempt = request.prepare(target, Some("claude-opus-4-1".to_owned()));
        let outcome = attempt
            .apply_compaction(Some("claude-haiku-3-5"), None)
            .expect("retargeted");

        assert_eq!(outcome.from.as_deref(), Some("claude-opus-4-1"));
        assert_eq!(outcome.to.as_deref(), Some("claude-haiku-3-5"));
        assert_eq!(attempt.effective_model(), Some("claude-haiku-3-5"));
        assert_eq!(
            request.canonical().model.as_deref(),
            Some("claude-opus-4-1")
        );
    }

    #[test]
    fn provider_mapping_is_candidate_local_and_keeps_baseline_identity() {
        let provider: Arc<dyn Provider> = Arc::new(OpenRouter::new(
            "https://openrouter.ai/api/v1".parse().unwrap(),
            None,
        ));
        let registry = RouteRegistry::new(
            [provider],
            [(ProtocolId::OpenAiChat, "openrouter".to_owned())],
        );
        let target = registry
            .resolve(ProtocolId::OpenAiChat, Some("openrouter/asked-model"))
            .unwrap();
        let request = InferenceRequest::new(
            "openrouter/asked-model".to_owned(),
            CanonicalRequest {
                model: Some("openrouter/asked-model".to_owned()),
                ..CanonicalRequest::default()
            },
        );
        let map = parse_model_map(r#"{"model:asked-model":"served-model"}"#)
            .unwrap()
            .unwrap();
        let mut mapped = request.prepare(target.clone(), Some("asked-model".to_owned()));
        mapped.apply_model_map(Some(&map));
        let plain = request.prepare(target, Some("asked-model".to_owned()));

        assert_eq!(mapped.effective_model(), Some("served-model"));
        assert_eq!(mapped.canonical().model.as_deref(), Some("served-model"));
        assert_eq!(plain.effective_model(), Some("asked-model"));
        assert_eq!(
            request.canonical().model.as_deref(),
            Some("openrouter/asked-model")
        );
    }
}
