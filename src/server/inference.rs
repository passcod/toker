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
            policy_plan: plan,
            canonical,
        }
    }
}

pub(crate) struct InferenceAttempt {
    target: ModelTarget,
    requested_model: String,
    effective_model: Option<String>,
    policy_plan: AttemptPlan,
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

    pub(crate) fn policy_routing(
        &self,
    ) -> crate::store::Result<Option<crate::store::PolicyRouteProvenance>> {
        crate::store::PolicyRouteProvenance::from_attempt_plan(&self.policy_plan)
    }

    pub(crate) fn apply_compaction(
        &mut self,
        compaction_model: Option<&str>,
    ) -> Option<RetargetOutcome> {
        let outcome = crate::middleware::canonical::retarget_compaction(
            &mut self.canonical,
            compaction_model,
            true,
            None,
        )?;
        self.effective_model.clone_from(&self.canonical.model);
        Some(outcome)
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

enum MessagesFailureResponse {
    Upstream(reqwest::Response),
    Local(Response),
}

#[allow(clippy::large_enum_variant)]
enum ReadyMessagesOutcome {
    Accepted {
        upstream: reqwest::Response,
        record: Option<AnthropicRecordCtx>,
        in_flight: Option<InFlightGuard>,
        response_model: String,
        response_projection: crate::ir::canonical::ResponseProjection,
        expected_stream: bool,
        rate_limits: Option<Value>,
        cancellation: super::cancel::PrecommitCancellationGuard,
    },
    Failure {
        failure: super::attempt::AttemptFailure,
        response: MessagesFailureResponse,
        parts: Parts,
        record: Option<AnthropicRecordCtx>,
        in_flight: Option<InFlightGuard>,
        response_model: String,
        response_projection: crate::ir::canonical::ResponseProjection,
        expected_stream: bool,
        rate_limits: Option<Value>,
        provenance: crate::store::AttemptProvenance,
        cancellation: super::cancel::PrecommitCancellationGuard,
    },
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

    async fn execute_precommit(
        mut self,
        server: &Server,
        provenance: crate::store::AttemptProvenance,
    ) -> ReadyMessagesOutcome {
        if let Some(ctx) = self.record.as_mut() {
            ctx.attempt = Some(provenance.clone());
        }
        let mut cancellation =
            super::cancel::PrecommitCancellationGuard::anthropic(self.record.as_ref());
        let sent = self.body.clone();
        let upstream = match super::proxy::send_upstream(
            server,
            self.backend.as_ref(),
            &self.parts,
            self.body.clone(),
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
                return ReadyMessagesOutcome::Failure {
                    failure: super::attempt::AttemptFailure {
                        class: Some(class),
                        mechanism: if error.is_connect() {
                            super::attempt::FailureMechanism::TransportBeforeAccept
                        } else {
                            super::attempt::FailureMechanism::AmbiguousTransport
                        },
                        status: Some(StatusCode::BAD_GATEWAY.as_u16()),
                    },
                    response: MessagesFailureResponse::Local(super::proxy::transport_failure(
                        super::proxy::ErrorWire::Anthropic,
                        &error,
                    )),
                    parts: self.parts,
                    record: self.record,
                    in_flight: self.in_flight,
                    response_model: self.response_model,
                    response_projection: self.response_projection,
                    expected_stream: self.expected_stream,
                    rate_limits: None,
                    provenance,
                    cancellation,
                };
            }
        };
        super::anthropic::note_meters(server, self.backend.as_ref(), upstream.headers());
        let retry = match super::anthropic::retry_thinking_off(
            server,
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
            Err(failed) => {
                let provenance = self
                    .record
                    .as_ref()
                    .and_then(|ctx| ctx.attempt.clone())
                    .unwrap_or(provenance);
                return ReadyMessagesOutcome::Failure {
                    failure: failed.failure,
                    response: MessagesFailureResponse::Local(failed.response),
                    parts: self.parts,
                    record: self.record,
                    in_flight: self.in_flight,
                    response_model: self.response_model,
                    response_projection: self.response_projection,
                    expected_stream: self.expected_stream,
                    rate_limits: failed.rate_limits,
                    provenance,
                    cancellation,
                };
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
                super::anthropic::note_meters(server, self.backend.as_ref(), upstream.headers());
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
        let provenance = self
            .record
            .as_ref()
            .and_then(|ctx| ctx.attempt.clone())
            .unwrap_or(provenance);
        match outcome {
            Ok(super::attempt::PrecommitOutcome::Accepted { response, .. }) => {
                ReadyMessagesOutcome::Accepted {
                    upstream: response,
                    record: self.record,
                    in_flight: self.in_flight,
                    response_model: self.response_model,
                    response_projection: self.response_projection,
                    expected_stream: self.expected_stream,
                    rate_limits,
                    cancellation,
                }
            }
            Ok(super::attempt::PrecommitOutcome::Failure {
                failure,
                response: Some(response),
            }) => ReadyMessagesOutcome::Failure {
                failure,
                response: MessagesFailureResponse::Upstream(response),
                parts: self.parts,
                record: self.record,
                in_flight: self.in_flight,
                response_model: self.response_model,
                response_projection: self.response_projection,
                expected_stream: self.expected_stream,
                rate_limits,
                provenance,
                cancellation,
            },
            Ok(super::attempt::PrecommitOutcome::Failure {
                failure,
                response: None,
            }) => ReadyMessagesOutcome::Failure {
                failure,
                response: MessagesFailureResponse::Local(super::proxy::upstream_failure(
                    super::proxy::ErrorWire::Anthropic,
                    "toker could not interpret the upstream response before commitment",
                )),
                parts: self.parts,
                record: self.record,
                in_flight: self.in_flight,
                response_model: self.response_model,
                response_projection: self.response_projection,
                expected_stream: self.expected_stream,
                rate_limits,
                provenance,
                cancellation,
            },
            Err(error) => {
                tracing::warn!(%error, "upstream response failed before commitment");
                ReadyMessagesOutcome::Failure {
                    failure: super::attempt::AttemptFailure {
                        class: Some(crate::policy::FailureClass::AmbiguousTransport),
                        mechanism: super::attempt::FailureMechanism::AmbiguousTransport,
                        status: Some(response_status.as_u16()),
                    },
                    response: MessagesFailureResponse::Local(super::proxy::truncated_body(
                        super::proxy::ErrorWire::Anthropic,
                    )),
                    parts: self.parts,
                    record: self.record,
                    in_flight: self.in_flight,
                    response_model: self.response_model,
                    response_projection: self.response_projection,
                    expected_stream: self.expected_stream,
                    rate_limits,
                    provenance,
                    cancellation,
                }
            }
        }
    }
}

async fn finish_messages_outcome(outcome: ReadyMessagesOutcome) -> Response {
    match outcome {
        ReadyMessagesOutcome::Accepted {
            upstream,
            record,
            in_flight,
            response_model,
            response_projection,
            expected_stream,
            rate_limits,
            mut cancellation,
        } => {
            let response = super::anthropic::forward_canonical_messages(
                upstream,
                record,
                in_flight,
                &response_model,
                response_projection,
                expected_stream,
                rate_limits,
            )
            .await;
            cancellation.disarm();
            response
        }
        ReadyMessagesOutcome::Failure {
            failure,
            response,
            parts: _,
            record,
            in_flight,
            response_model,
            response_projection,
            expected_stream,
            rate_limits,
            provenance: _,
            mut cancellation,
        } => {
            let response = match response {
                MessagesFailureResponse::Upstream(response) => {
                    super::anthropic::forward_canonical_messages(
                        response,
                        record,
                        in_flight,
                        &response_model,
                        response_projection,
                        expected_stream,
                        rate_limits,
                    )
                    .await
                }
                MessagesFailureResponse::Local(response) => {
                    let failure_response = MessagesFailureResponse::Local(response);
                    record_messages_failure(
                        record.as_ref(),
                        &failure,
                        &failure_response,
                        rate_limits.as_ref(),
                    );
                    let MessagesFailureResponse::Local(response) = failure_response else {
                        unreachable!()
                    };
                    response
                }
            };
            cancellation.disarm();
            response
        }
    }
}

fn record_messages_failure(
    record: Option<&AnthropicRecordCtx>,
    failure: &super::attempt::AttemptFailure,
    response: &MessagesFailureResponse,
    rate_limits: Option<&Value>,
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
        MessagesFailureResponse::Upstream(response) => {
            super::record::retry_after_ms(response.headers())
        }
        MessagesFailureResponse::Local(_) => None,
    };
    super::record_anthropic::record_anthropic_error(
        record,
        failure.status.unwrap_or(StatusCode::BAD_GATEWAY.as_u16()),
        Some(error_type.to_owned()),
        None,
        retry_after,
        rate_limits,
    );
}

enum MessagesFailureTerminal {
    Native {
        response: MessagesFailureResponse,
        response_model: String,
        response_projection: crate::ir::canonical::ResponseProjection,
        expected_stream: bool,
        rate_limits: Option<Value>,
    },
    Codex {
        ready: Box<super::codex::ReadyAttempt>,
        response: super::codex::FailureResponse,
    },
}

struct PolicyMessagesFailure {
    failure: super::attempt::AttemptFailure,
    parts: Parts,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    provenance: crate::store::AttemptProvenance,
    cancellation: super::cancel::PrecommitCancellationGuard,
    terminal: MessagesFailureTerminal,
}

impl PolicyMessagesFailure {
    fn record(&self) {
        match &self.terminal {
            MessagesFailureTerminal::Native {
                response,
                rate_limits,
                ..
            } => record_messages_failure(
                self.record.as_ref(),
                &self.failure,
                response,
                rate_limits.as_ref(),
            ),
            MessagesFailureTerminal::Codex { ready, .. } => super::codex::record_failure(
                self.record.as_ref(),
                ready.frontend_wire(),
                &self.failure,
            ),
        }
    }

    async fn finish(self, server: &Server) -> Response {
        let Self {
            failure,
            parts,
            record,
            in_flight,
            provenance,
            cancellation,
            terminal,
        } = self;
        match terminal {
            MessagesFailureTerminal::Native {
                response,
                response_model,
                response_projection,
                expected_stream,
                rate_limits,
            } => {
                finish_messages_outcome(ReadyMessagesOutcome::Failure {
                    failure,
                    response,
                    parts,
                    record,
                    in_flight,
                    response_model,
                    response_projection,
                    expected_stream,
                    rate_limits,
                    provenance,
                    cancellation,
                })
                .await
            }
            MessagesFailureTerminal::Codex {
                mut ready,
                response,
            } => {
                ready.restore_terminal_state(record, in_flight);
                super::codex::finish(
                    super::codex::ReadyOutcome::Failure {
                        ready: *ready,
                        failure,
                        response,
                        cancellation,
                    },
                    server.clone(),
                )
                .await
            }
        }
    }
}

enum ChatFailureResponse {
    Upstream(reqwest::Response),
    Local(Response),
}

#[allow(clippy::large_enum_variant)]
enum ChatFailureTerminal {
    Native {
        response: ChatFailureResponse,
        record: Option<RecordCtx>,
        response_model: String,
        expected_stream: bool,
    },
    Anthropic {
        response: super::anthropic_target::FailureResponse,
        record: Option<AnthropicRecordCtx>,
        response_model: String,
        frontend: super::anthropic_target::FrontendWire,
        stream: bool,
    },
    Codex {
        ready: Box<super::codex::ReadyAttempt>,
        response: super::codex::FailureResponse,
    },
}

struct PolicyChatFailure {
    failure: super::attempt::AttemptFailure,
    parts: Parts,
    in_flight: Option<InFlightGuard>,
    cancellation: super::cancel::PrecommitCancellationGuard,
    terminal: ChatFailureTerminal,
}

impl PolicyChatFailure {
    fn record(&self) {
        match &self.terminal {
            ChatFailureTerminal::Native {
                response, record, ..
            } => record_chat_attempt_failure(record.as_ref(), &self.failure, response),
            ChatFailureTerminal::Anthropic { record, .. } => {
                super::anthropic_target::record_failure(record.as_ref(), &self.failure);
            }
            ChatFailureTerminal::Codex { ready, .. } => super::codex::record_failure(
                ready.record().as_ref(),
                ready.frontend_wire(),
                &self.failure,
            ),
        }
    }

    fn into_retry_state(mut self) -> (Parts, Option<InFlightGuard>) {
        self.record();
        self.cancellation.disarm();
        match &mut self.terminal {
            ChatFailureTerminal::Codex { ready, .. } => {
                let (parts, _record, in_flight) = ready.take_retry_state();
                (parts, in_flight)
            }
            _ => (self.parts, self.in_flight.take()),
        }
    }

    async fn finish(self, server: &Server) -> Response {
        let Self {
            failure,
            parts,
            in_flight,
            cancellation,
            terminal,
        } = self;
        match terminal {
            ChatFailureTerminal::Native {
                response,
                record,
                response_model,
                expected_stream,
            } => {
                finish_chat_failure(
                    failure,
                    response,
                    record,
                    in_flight,
                    response_model,
                    expected_stream,
                    cancellation,
                )
                .await
            }
            ChatFailureTerminal::Anthropic {
                response,
                record,
                response_model,
                frontend,
                stream,
            } => {
                super::anthropic_target::finish(super::anthropic_target::ReadyOutcome::Failure {
                    failure,
                    response,
                    parts,
                    record,
                    in_flight,
                    response_model,
                    frontend,
                    stream,
                    cancellation,
                })
                .await
            }
            ChatFailureTerminal::Codex { ready, response } => {
                drop(in_flight);
                super::codex::finish(
                    super::codex::ReadyOutcome::Failure {
                        ready: *ready,
                        failure,
                        response,
                        cancellation,
                    },
                    server.clone(),
                )
                .await
            }
        }
    }
}

#[allow(clippy::large_enum_variant)]
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
        parts: Parts,
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
        &mut self,
        server: &Server,
        provenance: crate::store::AttemptProvenance,
    ) -> ReadyChatOutcome {
        if let Some(ctx) = self.record.as_mut() {
            ctx.attempt = Some(provenance);
        }
        let record = self.record.take();
        let in_flight = self.in_flight.take();
        let cancellation = super::cancel::PrecommitCancellationGuard::chat(record.as_ref());
        let upstream = match super::proxy::send_upstream(
            server,
            self.backend.as_ref(),
            &self.parts,
            self.body.clone(),
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
                    parts: self.parts.clone(),
                    response: ChatFailureResponse::Local(super::proxy::transport_failure(
                        super::proxy::ErrorWire::Openai,
                        &error,
                    )),
                    record,
                    in_flight,
                    response_model: self.response_model.clone(),
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
                    record,
                    in_flight,
                    response_model: self.response_model.clone(),
                    expected_stream: self.expected_stream,
                    cancellation,
                }
            }
            Ok(super::attempt::PrecommitOutcome::Failure {
                failure,
                response: Some(response),
            }) => ReadyChatOutcome::Failure {
                failure,
                parts: self.parts.clone(),
                response: ChatFailureResponse::Upstream(response),
                record,
                in_flight,
                response_model: self.response_model.clone(),
                expected_stream: self.expected_stream,
                cancellation,
            },
            Ok(super::attempt::PrecommitOutcome::Failure {
                failure,
                response: None,
            }) => ReadyChatOutcome::Failure {
                failure,
                parts: self.parts.clone(),
                response: ChatFailureResponse::Local(super::proxy::upstream_failure(
                    super::proxy::ErrorWire::Openai,
                    "toker could not interpret the upstream response before commitment",
                )),
                record,
                in_flight,
                response_model: self.response_model.clone(),
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
                    parts: self.parts.clone(),
                    response: ChatFailureResponse::Local(super::proxy::truncated_body(
                        super::proxy::ErrorWire::Openai,
                    )),
                    record,
                    in_flight,
                    response_model: self.response_model.clone(),
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
            Self::Messages(ready) => {
                let outcome = ready.execute_precommit(&server, provenance).await;
                finish_messages_outcome(outcome).await
            }
            Self::Chat(mut ready) => match ready.execute_precommit(&server, provenance).await {
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
                    parts: _,
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
            Self::Anthropic(mut ready) => {
                let outcome = ready.execute_precommit(&server, provenance).await;
                super::anthropic_target::finish(outcome).await
            }
            Self::OpenRouter(mut ready) => {
                let outcome = ready.execute_precommit(&server, provenance).await;
                super::openrouter_responses::finish(outcome).await
            }
            Self::Codex(ready) => {
                let outcome = ready.execute_precommit(&server, provenance).await;
                super::codex::finish(outcome, server).await
            }
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
    policy_attempt: Option<super::PolicyAttempt>,
    inference: Option<InferenceRequest>,
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
    let prepared = prepare_messages_backend(
        &server,
        adapter,
        attempt,
        MessagesAttemptInput {
            parts,
            record,
            in_flight,
            session_id: session_id.clone(),
            client_wants_json,
        },
    )
    .await;
    let ready = match prepared {
        Ok(ready) => ready,
        Err(response) => return *response,
    };
    let provenance = super::attempt::initial_provenance();
    // An attempt only prepares from a body that policy routed, which also
    // kept its baseline.
    let (Some(policy_attempt), Some(inference)) = (policy_attempt, inference) else {
        tracing::error!("a prepared Messages attempt has no policy attempt");
        return super::codex::anthropic_error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "the request could not be routed",
            false,
        );
    };
    coordinate_policy_messages(
        server,
        ready,
        provenance,
        policy_attempt,
        inference,
        session_id,
        client_wants_json,
    )
    .await
}

async fn prepare_messages_backend(
    server: &Server,
    adapter: Option<BackendAdapterId>,
    attempt: Result<InferenceAttempt, TranslateError>,
    input: MessagesAttemptInput,
) -> Result<ReadyBackendAttempt, Box<Response>> {
    let MessagesAttemptInput {
        parts,
        record,
        in_flight,
        session_id,
        client_wants_json,
    } = input;
    match adapter {
        Some(BackendAdapterId::AnthropicMessages) => {
            let attempt = attempt.map_err(|error| {
                Box::new(super::codex::anthropic_error_response(
                    axum::http::StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("the request could not be translated: {error}"),
                    false,
                ))
            })?;
            prepare_messages(parts, attempt, record, in_flight).map(ReadyBackendAttempt::Messages)
        }
        Some(BackendAdapterId::CodexResponses) => super::codex::prepare(
            server,
            super::codex::AttemptInput {
                parts,
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
    }
}

async fn execute_policy_messages_precommit(
    server: &Server,
    ready: ReadyBackendAttempt,
    provenance: crate::store::AttemptProvenance,
) -> Result<Response, Box<PolicyMessagesFailure>> {
    match ready {
        ReadyBackendAttempt::Messages(ready) => {
            match ready.execute_precommit(server, provenance).await {
                outcome @ ReadyMessagesOutcome::Accepted { .. } => {
                    Ok(finish_messages_outcome(outcome).await)
                }
                ReadyMessagesOutcome::Failure {
                    failure,
                    response,
                    parts,
                    record,
                    in_flight,
                    response_model,
                    response_projection,
                    expected_stream,
                    rate_limits,
                    provenance,
                    cancellation,
                } => Err(Box::new(PolicyMessagesFailure {
                    failure,
                    parts,
                    record,
                    in_flight,
                    provenance,
                    cancellation,
                    terminal: MessagesFailureTerminal::Native {
                        response,
                        response_model,
                        response_projection,
                        expected_stream,
                        rate_limits,
                    },
                })),
            }
        }
        ReadyBackendAttempt::Codex(ready) => {
            match ready.execute_precommit(server, provenance.clone()).await {
                outcome @ super::codex::ReadyOutcome::Accepted { .. } => {
                    Ok(super::codex::finish(outcome, server.clone()).await)
                }
                super::codex::ReadyOutcome::Failure {
                    mut ready,
                    failure,
                    response,
                    cancellation,
                } => {
                    let provenance = ready.attempt_provenance().cloned().unwrap_or(provenance);
                    let (parts, record, in_flight) = ready.take_retry_state();
                    Err(Box::new(PolicyMessagesFailure {
                        failure,
                        parts,
                        record,
                        in_flight,
                        provenance,
                        cancellation,
                        terminal: MessagesFailureTerminal::Codex {
                            ready: Box::new(ready),
                            response,
                        },
                    }))
                }
            }
        }
        ready => Ok(ready.execute(server.clone(), provenance).await),
    }
}

#[allow(clippy::too_many_arguments)]
async fn coordinate_policy_messages(
    server: Server,
    mut ready: ReadyBackendAttempt,
    mut provenance: crate::store::AttemptProvenance,
    mut policy_attempt: super::PolicyAttempt,
    inference: InferenceRequest,
    session_id: Option<String>,
    client_wants_json: bool,
) -> Response {
    loop {
        server.note_binding_sent(ready.binding_id(), Some(ready.effective_model()));
        let mut failed =
            match execute_policy_messages_precommit(&server, ready, provenance.clone()).await {
                Ok(response) => return response,
                Err(failed) => failed,
            };
        let failure = failed.failure.clone();
        let failed_provenance = failed.provenance.clone();
        let Some(class) = failure.class else {
            return failed.finish(&server).await;
        };
        server.open_policy_circuit(&policy_attempt, class, super::record::now_ms());
        let next = match server.continue_policy_attempt(&policy_attempt, class) {
            Ok(Some(next)) => next,
            Ok(None) => return failed.finish(&server).await,
            Err(error) => {
                tracing::error!(%error, "policy fallback continuation failed");
                return failed.finish(&server).await;
            }
        };
        let target = match server.registry.resolve_binding(
            crate::routing::ProtocolId::AnthropicMessages,
            &next.plan.binding_id,
            next.plan.requested_model.as_deref(),
            &next.plan.effective_model,
        ) {
            Ok(target) => target,
            Err(error) => {
                tracing::error!(?error, binding_id = %next.plan.binding_id, "policy fallback selected an unavailable Messages binding");
                return failed.finish(&server).await;
            }
        };
        let Some(next_provenance) = super::attempt::next_provenance(&failed_provenance, class)
        else {
            return failed.finish(&server).await;
        };
        failed.record();
        failed.cancellation.disarm();
        if let Some(ctx) = failed.record.as_mut() {
            ctx.backend = target.provider().clone();
            ctx.binding_id = next.plan.binding_id.clone();
            ctx.attempt = None;
            ctx.policy_routing = crate::store::PolicyRouteProvenance::from_attempt_plan(&next.plan)
                .unwrap_or_else(|error| {
                    tracing::error!(%error, "policy fallback provenance omitted");
                    None
                });
            ctx.pin_transition = Some(next.pin_transition.clone());
            ctx.effective_model = Some(next.plan.effective_model.clone());
            let forced = next.plan.forced_pair();
            ctx.forced_from = forced.as_ref().map(|(from, _)| from.clone());
            ctx.forced_to = forced.map(|(_, to)| to);
            ctx.policy_attempt = Some(Arc::new(next.clone()));
        }
        let parts = failed.parts;
        let mut record = failed.record;
        let in_flight = failed.in_flight;
        let adapter = target
            .binding()
            .canonical_backend()
            .map(|backend| backend.adapter());
        let attempt = prepare_messages_fallback_attempt(&inference, target, &next, record.as_mut());
        let prepared = match prepare_messages_backend(
            &server,
            adapter,
            attempt,
            MessagesAttemptInput {
                parts,
                record,
                in_flight,
                session_id: session_id.clone(),
                client_wants_json,
            },
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(response) => return *response,
        };
        provenance = next_provenance;
        policy_attempt = next;
        ready = prepared;
    }
}

fn prepare_messages_fallback_attempt(
    inference: &InferenceRequest,
    target: ModelTarget,
    policy_attempt: &super::PolicyAttempt,
    record: Option<&mut AnthropicRecordCtx>,
) -> Result<InferenceAttempt, TranslateError> {
    let backend = target.provider().clone();
    let mut attempt = inference.prepare_policy(target, policy_attempt.plan.clone());
    if policy_attempt.plan.compaction
        && let Some(outcome) = attempt.apply_compaction(Some(&policy_attempt.plan.effective_model))
        && let Some(record) = record
    {
        let moved = outcome.from != outcome.to;
        record.downgraded_from = moved.then_some(outcome.from).flatten();
        record.downgraded_to = moved.then_some(outcome.to).flatten();
        record.cache_stripped = (outcome.stripped > 0).then_some(true);
        record.system_merged = (outcome.merged > 0).then_some(true);
    }
    if !backend.accepts_message_effort(&policy_attempt.plan.effective_model) {
        let source = attempt.render_json()?.value;
        attempt.strip_message_effort(&source);
    }
    Ok(attempt)
}

pub(crate) async fn coordinate_chat(
    server: Server,
    adapter: Option<BackendAdapterId>,
    attempt: InferenceAttempt,
    policy_attempt: super::PolicyAttempt,
    inference: InferenceRequest,
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
    let record_template = record.clone();
    let prepared = prepare_chat_backend(
        &server,
        adapter,
        attempt,
        ChatBackendInput {
            parts,
            max_tokens: max_tokens.clone(),
            record,
            in_flight,
            session_id: session_id.clone(),
            client_wants_json,
        },
    )
    .await;
    let ready = match prepared {
        Ok(ready) => ready,
        Err(response) => return *response,
    };
    let provenance = super::attempt::initial_provenance();
    coordinate_policy_chat(
        server,
        ready,
        provenance,
        policy_attempt,
        inference,
        max_tokens,
        session_id,
        client_wants_json,
        record_template,
    )
    .await
}

struct ChatBackendInput {
    parts: Parts,
    max_tokens: Option<Value>,
    record: Option<RecordCtx>,
    in_flight: Option<InFlightGuard>,
    session_id: Option<String>,
    client_wants_json: bool,
}

async fn prepare_chat_backend(
    server: &Server,
    adapter: Option<BackendAdapterId>,
    attempt: InferenceAttempt,
    input: ChatBackendInput,
) -> Result<ReadyBackendAttempt, Box<Response>> {
    let ChatBackendInput {
        parts,
        max_tokens,
        record,
        in_flight,
        session_id,
        client_wants_json,
    } = input;
    match adapter {
        Some(BackendAdapterId::OpenAiChatCompletions) => {
            prepare_chat(parts, attempt, record, in_flight).map(ReadyBackendAttempt::Chat)
        }
        Some(BackendAdapterId::AnthropicMessages) => {
            let backend = attempt.backend().clone();
            super::anthropic_target::prepare(
                server,
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
                server,
                super::codex::AttemptInput {
                    parts,
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
    }
}

async fn execute_policy_chat_precommit(
    server: &Server,
    ready: ReadyBackendAttempt,
    provenance: crate::store::AttemptProvenance,
) -> Result<Response, Box<PolicyChatFailure>> {
    match ready {
        ReadyBackendAttempt::Chat(mut ready) => {
            match ready.execute_precommit(server, provenance).await {
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
                    Ok(response)
                }
                ReadyChatOutcome::Failure {
                    failure,
                    response,
                    parts,
                    record,
                    in_flight,
                    response_model,
                    expected_stream,
                    cancellation,
                } => Err(Box::new(PolicyChatFailure {
                    failure,
                    parts,
                    in_flight,
                    cancellation,
                    terminal: ChatFailureTerminal::Native {
                        response,
                        record,
                        response_model,
                        expected_stream,
                    },
                })),
            }
        }
        ReadyBackendAttempt::Anthropic(mut ready) => {
            match ready.execute_precommit(server, provenance).await {
                outcome @ super::anthropic_target::ReadyOutcome::Accepted { .. } => {
                    Ok(super::anthropic_target::finish(outcome).await)
                }
                super::anthropic_target::ReadyOutcome::Failure {
                    failure,
                    response,
                    parts,
                    record,
                    in_flight,
                    response_model,
                    frontend,
                    stream,
                    cancellation,
                } => Err(Box::new(PolicyChatFailure {
                    failure,
                    parts,
                    in_flight,
                    cancellation,
                    terminal: ChatFailureTerminal::Anthropic {
                        response,
                        record,
                        response_model,
                        frontend,
                        stream,
                    },
                })),
            }
        }
        ReadyBackendAttempt::Codex(ready) => {
            match ready.execute_precommit(server, provenance).await {
                outcome @ super::codex::ReadyOutcome::Accepted { .. } => {
                    Ok(super::codex::finish(outcome, server.clone()).await)
                }
                super::codex::ReadyOutcome::Failure {
                    ready,
                    failure,
                    response,
                    cancellation,
                } => {
                    let parts = ready.parts().clone();
                    Err(Box::new(PolicyChatFailure {
                        failure,
                        parts,
                        in_flight: None,
                        cancellation,
                        terminal: ChatFailureTerminal::Codex {
                            ready: Box::new(ready),
                            response,
                        },
                    }))
                }
            }
        }
        ready => Ok(ready.execute(server.clone(), provenance).await),
    }
}

#[allow(clippy::too_many_arguments)]
async fn coordinate_policy_chat(
    server: Server,
    mut ready: ReadyBackendAttempt,
    mut provenance: crate::store::AttemptProvenance,
    mut policy_attempt: super::PolicyAttempt,
    inference: InferenceRequest,
    max_tokens: Option<Value>,
    session_id: Option<String>,
    client_wants_json: bool,
    record_template: Option<RecordCtx>,
) -> Response {
    loop {
        server.note_binding_sent(ready.binding_id(), Some(ready.effective_model()));
        let failed = match execute_policy_chat_precommit(&server, ready, provenance.clone()).await {
            Ok(response) => return response,
            Err(failed) => failed,
        };
        let failure = failed.failure.clone();
        let Some(class) = failure.class else {
            return failed.finish(&server).await;
        };
        server.open_policy_circuit(&policy_attempt, class, super::record::now_ms());
        let next = match server.continue_policy_attempt(&policy_attempt, class) {
            Ok(Some(next)) => next,
            Ok(None) => return failed.finish(&server).await,
            Err(error) => {
                tracing::error!(%error, "policy fallback continuation failed");
                return failed.finish(&server).await;
            }
        };
        let target = match server.registry.resolve_binding(
            crate::routing::ProtocolId::OpenAiChat,
            &next.plan.binding_id,
            next.plan.requested_model.as_deref(),
            &next.plan.effective_model,
        ) {
            Ok(target) => target,
            Err(error) => {
                tracing::error!(?error, binding_id = %next.plan.binding_id, "policy fallback selected an unavailable Chat binding");
                return failed.finish(&server).await;
            }
        };
        let Some(next_provenance) = super::attempt::next_provenance(&provenance, class) else {
            return failed.finish(&server).await;
        };
        let (parts, in_flight) = failed.into_retry_state();
        let mut next_record = record_template.clone();
        if let Some(ctx) = next_record.as_mut() {
            ctx.backend = target.provider().clone();
            ctx.binding_id = next.plan.binding_id.clone();
            ctx.attempt = None;
            ctx.policy_routing = crate::store::PolicyRouteProvenance::from_attempt_plan(&next.plan)
                .unwrap_or_else(|error| {
                    tracing::error!(%error, "policy fallback provenance omitted");
                    None
                });
            ctx.pin_transition = Some(next.pin_transition.clone());
            ctx.effective_model = Some(next.plan.effective_model.clone());
            ctx.policy_attempt = Some(Arc::new(next.clone()));
        }
        let adapter = target
            .binding()
            .canonical_backend()
            .map(|backend| backend.adapter());
        let next_attempt = inference.prepare_policy(target, next.plan.clone());
        let prepared = prepare_chat_backend(
            &server,
            adapter,
            next_attempt,
            ChatBackendInput {
                parts,
                max_tokens: max_tokens.clone(),
                record: next_record,
                in_flight,
                session_id: session_id.clone(),
                client_wants_json,
            },
        )
        .await;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(response) => return *response,
        };
        provenance = next_provenance;
        policy_attempt = next;
        ready = prepared;
    }
}

async fn finish_chat_failure(
    failure: super::attempt::AttemptFailure,
    response: ChatFailureResponse,
    record: Option<RecordCtx>,
    in_flight: Option<InFlightGuard>,
    response_model: String,
    expected_stream: bool,
    mut cancellation: super::cancel::PrecommitCancellationGuard,
) -> Response {
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

pub(crate) async fn coordinate_responses(
    server: Server,
    adapter: Option<BackendAdapterId>,
    attempt: InferenceAttempt,
    policy_attempt: super::PolicyAttempt,
    inference: InferenceRequest,
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
    let policy_routing = attempt.policy_routing().unwrap_or_else(|error| {
        tracing::error!(%error, "policy attempt provenance omitted");
        None
    });
    if let Some(ctx) = record.as_mut() {
        ctx.policy_routing = policy_routing;
    }
    let prepared = prepare_responses_backend(
        &server,
        adapter,
        Ok(attempt),
        ResponsesAttemptInput {
            parts,
            max_output_tokens: max_output_tokens.clone(),
            had_prompt_cache_key,
            record,
            in_flight,
            session_id: session_id.clone(),
            thread_id: thread_id.clone(),
            request_id: request_id.clone(),
            client_wants_json,
        },
    )
    .await;
    let ready = match prepared {
        Ok(ready) => ready,
        Err(response) => return *response,
    };
    let provenance = super::attempt::initial_provenance();
    coordinate_policy_responses(
        server,
        ready,
        provenance,
        policy_attempt,
        inference,
        had_prompt_cache_key,
        max_output_tokens,
        session_id,
        thread_id,
        request_id,
        client_wants_json,
    )
    .await
}

async fn prepare_responses_backend(
    server: &Server,
    adapter: Option<BackendAdapterId>,
    attempt: Result<InferenceAttempt, TranslateError>,
    input: ResponsesAttemptInput,
) -> Result<ReadyBackendAttempt, Box<Response>> {
    let ResponsesAttemptInput {
        parts,
        max_output_tokens,
        had_prompt_cache_key,
        record,
        in_flight,
        session_id,
        thread_id,
        request_id,
        client_wants_json,
    } = input;
    match adapter {
        Some(BackendAdapterId::AnthropicMessages) => {
            let attempt =
                attempt.map_err(|_| Box::new(invalid_responses_body(client_wants_json)))?;
            super::anthropic_target::prepare(
                server,
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
            let attempt =
                attempt.map_err(|_| Box::new(invalid_responses_body(client_wants_json)))?;
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
            server,
            super::codex::AttemptInput {
                parts,
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
    }
}

enum ResponsesFailureTerminal {
    OpenRouter {
        response: super::openrouter_responses::FailureResponse,
        response_model: String,
        wants_stream: bool,
    },
    Anthropic {
        response: super::anthropic_target::FailureResponse,
        response_model: String,
        frontend: super::anthropic_target::FrontendWire,
        stream: bool,
    },
    Codex {
        ready: Box<super::codex::ReadyAttempt>,
        response: super::codex::FailureResponse,
    },
}

struct PolicyResponsesFailure {
    failure: super::attempt::AttemptFailure,
    parts: Parts,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    cancellation: super::cancel::PrecommitCancellationGuard,
    terminal: ResponsesFailureTerminal,
}

impl PolicyResponsesFailure {
    fn record(&self) {
        match &self.terminal {
            ResponsesFailureTerminal::OpenRouter { .. } => {
                super::openrouter_responses::record_failure(self.record.as_ref(), &self.failure);
            }
            ResponsesFailureTerminal::Anthropic { .. } => {
                super::anthropic_target::record_failure(self.record.as_ref(), &self.failure);
            }
            ResponsesFailureTerminal::Codex { ready, .. } => {
                super::codex::record_failure(
                    self.record.as_ref(),
                    ready.frontend_wire(),
                    &self.failure,
                );
            }
        }
    }

    async fn finish(self, server: &Server) -> Response {
        let Self {
            failure,
            parts,
            record,
            in_flight,
            cancellation,
            terminal,
        } = self;
        match terminal {
            ResponsesFailureTerminal::OpenRouter {
                response,
                response_model,
                wants_stream,
            } => {
                super::openrouter_responses::finish(
                    super::openrouter_responses::ReadyOutcome::Failure {
                        failure,
                        response,
                        parts,
                        record,
                        in_flight,
                        response_model,
                        wants_stream,
                        cancellation,
                    },
                )
                .await
            }
            ResponsesFailureTerminal::Anthropic {
                response,
                response_model,
                frontend,
                stream,
            } => {
                super::anthropic_target::finish(super::anthropic_target::ReadyOutcome::Failure {
                    failure,
                    response,
                    parts,
                    record,
                    in_flight,
                    response_model,
                    frontend,
                    stream,
                    cancellation,
                })
                .await
            }
            ResponsesFailureTerminal::Codex {
                mut ready,
                response,
            } => {
                ready.restore_terminal_state(record, in_flight);
                super::codex::finish(
                    super::codex::ReadyOutcome::Failure {
                        ready: *ready,
                        failure,
                        response,
                        cancellation,
                    },
                    server.clone(),
                )
                .await
            }
        }
    }
}

async fn execute_policy_responses_precommit(
    server: &Server,
    ready: ReadyBackendAttempt,
    provenance: crate::store::AttemptProvenance,
) -> Result<Response, Box<PolicyResponsesFailure>> {
    match ready {
        ReadyBackendAttempt::OpenRouter(mut ready) => {
            match ready.execute_precommit(server, provenance).await {
                outcome @ super::openrouter_responses::ReadyOutcome::Accepted { .. } => {
                    Ok(super::openrouter_responses::finish(outcome).await)
                }
                super::openrouter_responses::ReadyOutcome::Failure {
                    failure,
                    response,
                    parts,
                    record,
                    in_flight,
                    response_model,
                    wants_stream,
                    cancellation,
                } => Err(Box::new(PolicyResponsesFailure {
                    failure,
                    parts,
                    record,
                    in_flight,
                    cancellation,
                    terminal: ResponsesFailureTerminal::OpenRouter {
                        response,
                        response_model,
                        wants_stream,
                    },
                })),
            }
        }
        ReadyBackendAttempt::Anthropic(mut ready) => {
            match ready.execute_precommit(server, provenance).await {
                outcome @ super::anthropic_target::ReadyOutcome::Accepted { .. } => {
                    Ok(super::anthropic_target::finish(outcome).await)
                }
                super::anthropic_target::ReadyOutcome::Failure {
                    failure,
                    response,
                    parts,
                    record,
                    in_flight,
                    response_model,
                    frontend,
                    stream,
                    cancellation,
                } => Err(Box::new(PolicyResponsesFailure {
                    failure,
                    parts,
                    record,
                    in_flight,
                    cancellation,
                    terminal: ResponsesFailureTerminal::Anthropic {
                        response,
                        response_model,
                        frontend,
                        stream,
                    },
                })),
            }
        }
        ReadyBackendAttempt::Codex(ready) => {
            match ready.execute_precommit(server, provenance).await {
                outcome @ super::codex::ReadyOutcome::Accepted { .. } => {
                    Ok(super::codex::finish(outcome, server.clone()).await)
                }
                super::codex::ReadyOutcome::Failure {
                    mut ready,
                    failure,
                    response,
                    cancellation,
                } => {
                    let frontend = ready.frontend_wire();
                    let (parts, record, in_flight) = ready.take_retry_state();
                    debug_assert_eq!(frontend, CodexFrontendWire::OpenAiResponses);
                    Err(Box::new(PolicyResponsesFailure {
                        failure,
                        parts,
                        record,
                        in_flight,
                        cancellation,
                        terminal: ResponsesFailureTerminal::Codex {
                            ready: Box::new(ready),
                            response,
                        },
                    }))
                }
            }
        }
        ready => Ok(ready.execute(server.clone(), provenance).await),
    }
}

#[allow(clippy::too_many_arguments)]
async fn coordinate_policy_responses(
    server: Server,
    mut ready: ReadyBackendAttempt,
    mut provenance: crate::store::AttemptProvenance,
    mut policy_attempt: super::PolicyAttempt,
    inference: InferenceRequest,
    had_prompt_cache_key: bool,
    max_output_tokens: Option<Value>,
    session_id: Option<String>,
    thread_id: Option<String>,
    request_id: Option<String>,
    client_wants_json: bool,
) -> Response {
    loop {
        server.note_binding_sent(ready.binding_id(), Some(ready.effective_model()));
        let mut failed =
            match execute_policy_responses_precommit(&server, ready, provenance.clone()).await {
                Ok(response) => return response,
                Err(failed) => failed,
            };
        let Some(class) = failed.failure.class else {
            return failed.finish(&server).await;
        };
        server.open_policy_circuit(&policy_attempt, class, super::record::now_ms());
        let next = match server.continue_policy_attempt(&policy_attempt, class) {
            Ok(Some(next)) => next,
            Ok(None) => return failed.finish(&server).await,
            Err(error) => {
                tracing::error!(%error, "policy fallback continuation failed");
                return failed.finish(&server).await;
            }
        };
        let target = match server.registry.resolve_binding(
            crate::routing::ProtocolId::OpenAiResponses,
            &next.plan.binding_id,
            next.plan.requested_model.as_deref(),
            &next.plan.effective_model,
        ) {
            Ok(target) => target,
            Err(error) => {
                tracing::error!(?error, binding_id = %next.plan.binding_id, "policy fallback selected an unavailable Responses binding");
                return failed.finish(&server).await;
            }
        };
        let Some(next_provenance) = super::attempt::next_provenance(&provenance, class) else {
            return failed.finish(&server).await;
        };
        failed.record();
        failed.cancellation.disarm();
        if let Some(ctx) = failed.record.as_mut() {
            ctx.backend = target.provider().clone();
            ctx.binding_id = next.plan.binding_id.clone();
            ctx.attempt = None;
            ctx.policy_routing = crate::store::PolicyRouteProvenance::from_attempt_plan(&next.plan)
                .unwrap_or_else(|error| {
                    tracing::error!(%error, "policy fallback provenance omitted");
                    None
                });
            ctx.pin_transition = Some(next.pin_transition.clone());
            ctx.effective_model = Some(next.plan.effective_model.clone());
            ctx.policy_attempt = Some(Arc::new(next.clone()));
        }
        let parts = failed.parts;
        let record = failed.record;
        let in_flight = failed.in_flight;
        let adapter = target
            .binding()
            .canonical_backend()
            .map(|backend| backend.adapter());
        let attempt = Ok(inference.prepare_policy(target, next.plan.clone()));
        let prepared = match prepare_responses_backend(
            &server,
            adapter,
            attempt,
            ResponsesAttemptInput {
                parts,
                max_output_tokens: max_output_tokens.clone(),
                had_prompt_cache_key,
                record,
                in_flight,
                session_id: session_id.clone(),
                thread_id: thread_id.clone(),
                request_id: request_id.clone(),
                client_wants_json,
            },
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(response) => return *response,
        };
        provenance = next_provenance;
        policy_attempt = next;
        ready = prepared;
    }
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
    use super::*;
    use crate::routing::ProtocolId;
    use crate::store::Store;

    /// What the active revision resolves a Chat request for `model` to,
    /// with the target it binds, over an OpenRouter-only config.
    fn resolved(model: &str) -> (ModelTarget, AttemptPlan) {
        let dir = crate::setup::test_dir("inference-attempt");
        std::fs::write(
            dir.join("toker.toml"),
            r#"
awake = false
db_path = ":memory:"
default_backend_openai_chat = "openrouter"

[providers.openrouter]
upstream = "http://127.0.0.1:9/v1"
api_key = "sk-test"
"#,
        )
        .unwrap();
        let config = crate::config::Config::load_from(&dir.join("toker.toml")).unwrap();
        let store = Arc::new(Store::open(":memory:").unwrap());
        let server = Server::with_seams(config, store, super::super::Seams::isolated()).unwrap();
        let facts = server.request_facts(
            ProtocolId::OpenAiChat,
            None,
            Some(model.to_owned()),
            None,
            0,
            None,
            0,
            Default::default(),
            None,
            crate::policy::RequestClass::Inference,
        );
        let credentials = server.credential_snapshot(
            &axum::http::HeaderMap::new(),
            ProtocolId::OpenAiChat,
            facts.now_ms,
        );
        let plan = server
            .resolve_policy_attempt(&facts, &credentials, None, None)
            .expect("the model resolves")
            .plan;
        let target = server
            .registry
            .resolve_binding(
                ProtocolId::OpenAiChat,
                &plan.binding_id,
                Some(model),
                &plan.effective_model,
            )
            .expect("the plan's binding is configured");
        (target, plan)
    }

    #[test]
    fn candidates_start_from_fresh_baseline_clones_with_separate_identities() {
        let (target, plan) = resolved("openrouter/invented-model");
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
        let attempt = request.prepare_policy(target, plan);

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
        let (target, plan) = resolved("openrouter/claude-opus-4-1");
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
        let mut attempt = request.prepare_policy(target, plan);
        let outcome = attempt
            .apply_compaction(Some("claude-haiku-3-5"))
            .expect("retargeted");

        assert_eq!(outcome.from.as_deref(), Some("claude-opus-4-1"));
        assert_eq!(outcome.to.as_deref(), Some("claude-haiku-3-5"));
        assert_eq!(attempt.effective_model(), Some("claude-haiku-3-5"));
        assert_eq!(
            request.canonical().model.as_deref(),
            Some("claude-opus-4-1")
        );
    }
}
