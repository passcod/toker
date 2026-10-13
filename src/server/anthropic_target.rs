//! OpenAI frontends to an Anthropic Messages binding.
//!
//! Request and response both cross canonical adapters. The API key and
//! version header remain provider-owned; a foreign frontend bearer is removed
//! by the provider before its key is injected. Observation sees provider
//! bytes before either frontend renderer sees canonical events.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;

use crate::ir::canonical::CanonEvent;
use crate::observe::{AnthropicObserver, SseEvent, SseSplitter};
use crate::policy::BindingId;
use crate::providers::Provider;
use crate::routing::ProtocolId;
use crate::translate::{
    self, AnthropicResponseStream, OpenAiChatRenderer, OpenAiResponsesRenderer,
};

use super::InFlightGuard;
use super::Server;
use super::codex::{CodexFrontendWire, frontend_error_response};
use super::proxy::{
    ErrorWire, MAX_ERROR_BODY, MAX_RESPONSE_BUFFER, buffer_up_to, is_compressed, is_event_stream,
    response_headers, send_upstream_to, transport_failure, truncated_body, upstream_failure,
};
use super::record_anthropic::{
    AnthropicRecordCtx, record_anthropic_error, record_anthropic_measurement,
};

#[derive(Clone, Copy)]
pub(crate) enum FrontendWire {
    Chat,
    Responses,
}

impl FrontendWire {
    fn protocol(self) -> ProtocolId {
        match self {
            Self::Chat => ProtocolId::OpenAiChat,
            Self::Responses => ProtocolId::OpenAiResponses,
        }
    }

    fn codex_wire(self) -> CodexFrontendWire {
        match self {
            Self::Chat => CodexFrontendWire::OpenAiChat,
            Self::Responses => CodexFrontendWire::OpenAiResponses,
        }
    }

    fn render_complete(self, model: &str, turn: &crate::ir::canonical::CanonTurn) -> Value {
        match self {
            Self::Chat => translate::openai_chat_from_canonical(model, turn),
            Self::Responses => translate::openai_responses_from_canonical(model, turn),
        }
    }
}

pub(crate) struct ReadyAttempt {
    parts: Parts,
    backend: Arc<dyn Provider>,
    binding_id: BindingId,
    effective_model: String,
    response_model: String,
    body: Bytes,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    frontend: FrontendWire,
    stream: bool,
}

impl ReadyAttempt {
    pub(crate) fn binding_id(&self) -> &BindingId {
        &self.binding_id
    }

    pub(crate) fn effective_model(&self) -> &str {
        &self.effective_model
    }

    pub(crate) async fn execute(mut self, server: Server) -> Response {
        if let Some(ctx) = self.record.as_mut() {
            ctx.attempt = Some(super::attempt::initial_provenance());
        }
        let upstream = match send_upstream_to(
            &server,
            self.backend.as_ref(),
            &self.parts,
            self.body,
            &server.config.session_header_names,
            self.frontend.protocol(),
            "/v1/messages",
        )
        .await
        {
            Ok(upstream) => upstream,
            Err(error) => {
                if let Some(ctx) = self.record.as_ref() {
                    record_anthropic_error(
                        ctx,
                        StatusCode::BAD_GATEWAY.as_u16(),
                        Some(super::attempt::transport_failure_type(&error).to_owned()),
                        None,
                        None,
                        None,
                    );
                }
                return transport_failure(ErrorWire::Openai, &error);
            }
        };
        let response_status = upstream.status();
        let upstream = match super::attempt::precommit_anthropic(upstream).await {
            Ok(super::attempt::PrecommitOutcome::Accepted { response, .. })
            | Ok(super::attempt::PrecommitOutcome::Failure {
                response: Some(response),
                ..
            }) => response,
            Ok(super::attempt::PrecommitOutcome::Failure { response: None, .. }) => {
                if let Some(ctx) = self.record.as_ref() {
                    record_anthropic_error(
                        ctx,
                        response_status.as_u16(),
                        Some("response_protocol".to_owned()),
                        None,
                        None,
                        None,
                    );
                }
                return upstream_failure(
                    ErrorWire::Openai,
                    "toker could not interpret the upstream response before commitment",
                );
            }
            Ok(super::attempt::PrecommitOutcome::Cancelled) => {
                return upstream_failure(
                    ErrorWire::Openai,
                    "toker upstream request was cancelled before response commitment",
                );
            }
            Err(error) => {
                tracing::warn!(%error, "upstream response failed before commitment");
                if let Some(ctx) = self.record.as_ref() {
                    record_anthropic_error(
                        ctx,
                        response_status.as_u16(),
                        Some("ambiguous_transport".to_owned()),
                        None,
                        None,
                        None,
                    );
                }
                return truncated_body(ErrorWire::Openai);
            }
        };
        forward(
            upstream,
            self.record,
            self.in_flight,
            &self.response_model,
            self.frontend,
            self.stream,
        )
        .await
    }
}

pub(crate) fn prepare(
    server: &Server,
    parts: Parts,
    mut attempt: super::inference::InferenceAttempt,
    limit_field: Option<Value>,
    mut record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    frontend: FrontendWire,
) -> Result<ReadyAttempt, Box<Response>> {
    let effective_model = attempt.effective_model().unwrap_or_default().to_owned();
    let response_model = attempt.requested_model().to_owned();
    let stream = match frontend {
        FrontendWire::Chat => attempt.stream() == Some(true),
        FrontendWire::Responses => attempt.stream() != Some(false),
    };

    // Responses makes this limit optional; Messages requires one. Prefer
    // the caller's positive bound. Otherwise use the model's own declared
    // maximum from the fetched Anthropic catalogue, never a guessed limit.
    let limit = match limit_field.as_ref() {
        Some(Value::Number(number)) => number.as_u64().filter(|limit| *limit > 0),
        None | Some(Value::Null) => declared_output_limit(server, &effective_model),
        Some(_) => None,
    };
    let Some(limit) = limit else {
        return Err(Box::new(invalid_request(
            "an output token limit is required: supply a positive max_output_tokens/max_tokens or fetch an Anthropic catalogue with max_tokens",
            stream,
            frontend,
        )));
    };
    attempt.prepare_messages_transport(stream, limit);

    let rendered = attempt
        .render_json()
        .map_err(|error| Box::new(invalid_request(&error.to_string(), stream, frontend)))?;
    if !rendered.report.is_empty()
        && let Some(ctx) = record.as_mut()
    {
        ctx.translation_report = Some(rendered.report);
    }
    let body = serde_json::to_vec(&rendered.value)
        .map(Bytes::from)
        .map_err(|error| {
            tracing::error!(%error, "Messages backend serialisation failed");
            Box::new(upstream_failure(
                ErrorWire::Openai,
                "toker could not render the request",
            ))
        })?;
    let backend = attempt.backend().clone();
    if !backend.foreign_auth_available() {
        return Err(Box::new(frontend_error_response(
            frontend.codex_wire(),
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "the selected backend has no usable subscription credential",
            stream,
        )));
    }
    Ok(ReadyAttempt {
        parts,
        backend,
        binding_id: attempt.binding().id().clone(),
        effective_model,
        response_model,
        body,
        record,
        in_flight,
        frontend,
        stream,
    })
}

fn declared_output_limit(server: &Server, model: &str) -> Option<u64> {
    let catalogs = server
        .catalogs
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    catalogs
        .get("anthropic")?
        .models
        .iter()
        .find(|entry| entry.id == model)?
        .raw
        .get("max_tokens")?
        .as_u64()
        .filter(|limit| *limit > 0)
}

fn invalid_request(message: &str, stream: bool, frontend: FrontendWire) -> Response {
    frontend_error_response(
        frontend.codex_wire(),
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        message,
        stream,
    )
}

async fn forward(
    upstream: reqwest::Response,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    model: &str,
    frontend: FrontendWire,
    expected_stream: bool,
) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    if is_compressed(&headers) {
        record_terminal_failure(record.as_ref(), status, "response_protocol");
        return upstream_failure(
            ErrorWire::Openai,
            "toker could not interpret a compressed Messages response",
        );
    }
    if !status.is_success() {
        let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
            record_terminal_failure(record.as_ref(), status, "ambiguous_transport");
            return truncated_body(ErrorWire::Openai);
        };
        if buffered.rest.is_some() {
            record_terminal_failure(record.as_ref(), status, "response_protocol");
            return upstream_failure(ErrorWire::Openai, "upstream error was too large");
        }
        let parsed = serde_json::from_slice::<Value>(&buffered.bytes).ok();
        let Some(turn) = parsed
            .as_ref()
            .and_then(|body| translate::canonical_turn_from_anthropic(body).ok())
        else {
            record_terminal_failure(record.as_ref(), status, "response_protocol");
            return upstream_failure(ErrorWire::Openai, "invalid upstream error response");
        };
        if let Some(ctx) = record.as_ref() {
            let (kind, message) = error_pair(parsed.as_ref());
            record_anthropic_error(ctx, status.as_u16(), kind, message, None, None);
        }
        return json_response(status, &headers, &frontend.render_complete(model, &turn));
    }
    if is_event_stream(&headers) != expected_stream {
        record_terminal_failure(record.as_ref(), status, "response_protocol");
        return upstream_failure(
            ErrorWire::Openai,
            "upstream Messages response used the wrong streaming mode",
        );
    }
    if expected_stream {
        let mut response_headers = response_headers(&headers, false);
        response_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        let body = Body::from_stream(stream_events(
            upstream,
            record,
            in_flight,
            model.to_owned(),
            frontend,
        ));
        return Response::builder()
            .status(status)
            .body(body)
            .map(|mut response| {
                *response.headers_mut() = response_headers;
                response
            })
            .expect("valid streaming response");
    }
    let Ok(buffered) = buffer_up_to(upstream, MAX_RESPONSE_BUFFER).await else {
        record_terminal_failure(record.as_ref(), status, "ambiguous_transport");
        return truncated_body(ErrorWire::Openai);
    };
    if buffered.rest.is_some() {
        record_terminal_failure(record.as_ref(), status, "response_protocol");
        return upstream_failure(ErrorWire::Openai, "upstream response was too large");
    }
    let turn = match serde_json::from_slice::<Value>(&buffered.bytes)
        .ok()
        .and_then(|body| translate::canonical_turn_from_anthropic(&body).ok())
    {
        Some(turn) => turn,
        None => {
            record_terminal_failure(record.as_ref(), status, "response_protocol");
            return upstream_failure(ErrorWire::Openai, "invalid upstream response");
        }
    };
    if let Some(ctx) = record.as_ref() {
        let mut observer = AnthropicObserver::new();
        observer.observe_json(&buffered.bytes);
        let (reported_model, capture) = observer.finish_parts();
        record_anthropic_measurement(
            ctx,
            capture.as_ref(),
            reported_model.as_deref(),
            None,
            status.as_u16(),
        );
    }
    json_response(status, &headers, &frontend.render_complete(model, &turn))
}

fn record_terminal_failure(
    record: Option<&AnthropicRecordCtx>,
    status: StatusCode,
    error_type: &str,
) {
    if let Some(ctx) = record {
        record_anthropic_error(
            ctx,
            status.as_u16(),
            Some(error_type.to_owned()),
            None,
            None,
            None,
        );
    }
}

fn error_pair(body: Option<&Value>) -> (Option<String>, Option<String>) {
    let error = body.and_then(|body| body.get("error"));
    (
        error
            .and_then(|error| error.get("type"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        error
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    )
}

fn json_response(status: StatusCode, headers: &HeaderMap, value: &Value) -> Response {
    let mut outbound = response_headers(headers, false);
    outbound.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let mut response = Response::new(Body::from(serde_json::to_vec(value).unwrap_or_default()));
    *response.status_mut() = status;
    *response.headers_mut() = outbound;
    response
}

struct StreamState {
    upstream: reqwest::Response,
    splitter: SseSplitter,
    backend: AnthropicResponseStream,
    frontend: FrontendRenderer,
    observer: AnthropicObserver,
    pending: VecDeque<Bytes>,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    ended: bool,
    succeeded: bool,
}

enum FrontendRenderer {
    Chat(OpenAiChatRenderer),
    Responses(OpenAiResponsesRenderer),
}

impl FrontendRenderer {
    fn new(model: &str, wire: FrontendWire) -> Self {
        match wire {
            FrontendWire::Chat => Self::Chat(OpenAiChatRenderer::new(model)),
            FrontendWire::Responses => Self::Responses(OpenAiResponsesRenderer::new(model)),
        }
    }

    fn feed(&mut self, event: &CanonEvent) -> Vec<SseEvent> {
        match self {
            Self::Chat(renderer) => renderer.feed(event),
            Self::Responses(renderer) => renderer.feed(event),
        }
    }

    fn turn_ended(&self) -> bool {
        match self {
            Self::Chat(renderer) => renderer.turn_ended(),
            Self::Responses(renderer) => renderer.turn_ended(),
        }
    }
}

fn stream_events(
    upstream: reqwest::Response,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    model: String,
    frontend: FrontendWire,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    let state = StreamState {
        upstream,
        splitter: SseSplitter::new(),
        backend: AnthropicResponseStream::new(),
        frontend: FrontendRenderer::new(&model, frontend),
        observer: AnthropicObserver::new(),
        pending: VecDeque::new(),
        record,
        in_flight,
        ended: false,
        succeeded: false,
    };
    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(bytes) = state.pending.pop_front() {
                return Some((Ok(bytes), state));
            }
            if state.ended {
                if let Some(ctx) = state.record.take().filter(|_| state.succeeded) {
                    let (reported_model, capture) =
                        std::mem::take(&mut state.observer).finish_parts();
                    record_anthropic_measurement(
                        &ctx,
                        capture.as_ref(),
                        reported_model.as_deref(),
                        None,
                        200,
                    );
                }
                drop(state.in_flight.take());
                return None;
            }
            match state.upstream.chunk().await {
                Ok(Some(chunk)) => {
                    for event in state.splitter.feed(&chunk) {
                        state.feed(event);
                    }
                }
                Ok(None) => {
                    if let Some(event) = state.splitter.finish() {
                        state.feed(event);
                    }
                    if state.frontend.turn_ended() {
                        state.ended = true;
                    } else {
                        state.record_failure("response_protocol");
                        state.ended = true;
                        return Some((
                            Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "Messages stream ended without a terminal event",
                            )),
                            state,
                        ));
                    }
                }
                Err(error) => {
                    state.record_failure("ambiguous_transport");
                    state.ended = true;
                    return Some((Err(std::io::Error::other(error)), state));
                }
            }
        }
    })
}

impl StreamState {
    fn record_failure(&mut self, error_type: &str) {
        if let Some(ctx) = self.record.take() {
            record_anthropic_error(&ctx, 200, Some(error_type.to_owned()), None, None, None);
        }
    }

    fn feed(&mut self, event: SseEvent) {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.observer.observe_event(&event);
        }));
        for canonical in self.backend.feed_sse(&event) {
            match &canonical {
                CanonEvent::TurnEnded { .. } => self.succeeded = true,
                CanonEvent::TurnFailed { error } => {
                    self.succeeded = false;
                    if let Some(ctx) = self.record.take() {
                        record_anthropic_error(
                            &ctx,
                            200,
                            Some(
                                translate::anthropic_frontend::anthropic_error_type(error)
                                    .to_owned(),
                            ),
                            None,
                            None,
                            None,
                        );
                    }
                }
                _ => {}
            }
            for emitted in self.frontend.feed(&canonical) {
                self.pending.push_back(sse_bytes(&emitted));
            }
        }
    }
}

impl Drop for StreamState {
    fn drop(&mut self) {
        if self.record.is_some() {
            self.record_failure("client_cancelled");
        }
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
