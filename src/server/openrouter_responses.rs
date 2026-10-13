//! OpenRouter's live-verified streamed Responses binding. The provider owns
//! auth, endpoint and attested billed cost; canonical adapters own both
//! directions. A JSON frontend response aggregates the verified SSE wire.

use std::collections::VecDeque;
use std::sync::Arc;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;

use crate::observe::{SseEvent, SseSplitter};
use crate::policy::BindingId;
use crate::providers::Provider;
use crate::providers::codex::{ResponseError, ResponsesSse, TurnCapture};
use crate::routing::ProtocolId;
use crate::translate::{self, OpenAiResponsesRenderer};

use super::InFlightGuard;
use super::Server;
use super::codex::{CodexFrontendWire, frontend_error_response};
use super::proxy::{
    ErrorWire, MAX_ERROR_BODY, buffer_up_to, is_compressed, is_event_stream, response_headers,
    send_upstream_to, transport_failure, truncated_body, upstream_failure,
};
use super::record_anthropic::{
    AnthropicRecordCtx, record_codex_error, record_responses_measurement,
};

pub(crate) struct ReadyAttempt {
    parts: Parts,
    backend: Arc<dyn Provider>,
    binding_id: BindingId,
    effective_model: String,
    response_model: String,
    body: Bytes,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    wants_stream: bool,
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
            &[],
            ProtocolId::OpenAiResponses,
            "/v1/responses",
        )
        .await
        {
            Ok(upstream) => upstream,
            Err(error) => {
                if let Some(ctx) = self.record.as_ref() {
                    record_codex_error(
                        ctx,
                        StatusCode::BAD_GATEWAY.as_u16(),
                        super::attempt::transport_failure_type(&error),
                        "upstream transport failed before response headers",
                        None,
                        "openai_responses",
                    );
                }
                return transport_failure(ErrorWire::Openai, &error);
            }
        };
        let response_status = upstream.status();
        let upstream = match super::attempt::precommit_responses(upstream).await {
            Ok(super::attempt::PrecommitOutcome::Accepted { response, .. })
            | Ok(super::attempt::PrecommitOutcome::Failure {
                response: Some(response),
                ..
            }) => response,
            Ok(super::attempt::PrecommitOutcome::Failure { response: None, .. }) => {
                if let Some(ctx) = self.record.as_ref() {
                    record_codex_error(
                        ctx,
                        response_status.as_u16(),
                        "response_protocol",
                        "upstream response could not be interpreted before commitment",
                        None,
                        "openai_responses",
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
                    record_codex_error(
                        ctx,
                        response_status.as_u16(),
                        "ambiguous_transport",
                        "upstream response body failed before commitment",
                        None,
                        "openai_responses",
                    );
                }
                return truncated_body(ErrorWire::Openai);
            }
        };
        forward(
            upstream,
            self.record,
            self.in_flight,
            self.response_model,
            self.wants_stream,
        )
        .await
    }
}

pub(crate) fn prepare(
    parts: Parts,
    attempt: super::inference::InferenceAttempt,
    had_prompt_cache_key: bool,
    mut record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
) -> Result<ReadyAttempt, Box<Response>> {
    let effective_model = attempt.effective_model().unwrap_or_default().to_owned();
    let response_model = attempt.requested_model().to_owned();
    let canonical = attempt.canonical();
    let wants_stream = canonical.stream != Some(false);
    let rendered = translate::openrouter_responses_backend::render_openrouter_responses(
        &canonical,
        had_prompt_cache_key,
    )
    .map_err(|error| Box::new(invalid_request(&error.to_string(), wants_stream)))?;
    if !rendered.report.is_empty()
        && let Some(ctx) = record.as_mut()
    {
        ctx.translation_report = Some(rendered.report);
    }
    let body = Bytes::from(
        serde_json::to_vec(&rendered.value).expect("a rendered Responses request serialises"),
    );
    Ok(ReadyAttempt {
        parts,
        backend: attempt.backend().clone(),
        binding_id: attempt.binding().id().clone(),
        effective_model,
        response_model,
        body,
        record,
        in_flight,
        wants_stream,
    })
}

async fn forward(
    upstream: reqwest::Response,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    response_model: String,
    wants_stream: bool,
) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    if is_compressed(&headers) {
        record_terminal_failure(record.as_ref(), status, "response_protocol");
        return upstream_failure(
            ErrorWire::Openai,
            "toker could not interpret a compressed Responses response",
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
        let error = parse_error(&buffered.bytes);
        let kind = translate::anthropic_error_type(&error);
        let message = error
            .message
            .or(error.code)
            .or(error.kind)
            .unwrap_or_else(|| "upstream error".to_owned());
        if let Some(ctx) = record.as_ref() {
            record_codex_error(
                ctx,
                status.as_u16(),
                kind,
                &message,
                error.resets_at,
                "openai_responses",
            );
        }
        return frontend_error_response(
            CodexFrontendWire::OpenAiResponses,
            status,
            kind,
            &message,
            wants_stream,
        );
    }
    if !is_event_stream(&headers) {
        record_terminal_failure(record.as_ref(), status, "response_protocol");
        return upstream_failure(ErrorWire::Openai, "OpenRouter Responses did not stream");
    }
    if wants_stream {
        let mut outbound = response_headers(&headers, false);
        outbound.insert(
            header::CONTENT_TYPE,
            "text/event-stream".parse().expect("static content type"),
        );
        let body = Body::from_stream(stream_events(upstream, record, in_flight, response_model));
        let mut response = Response::new(body);
        *response.status_mut() = status;
        *response.headers_mut() = outbound;
        response
    } else {
        complete(upstream, record, in_flight, &response_model).await
    }
}

fn invalid_request(message: &str, stream: bool) -> Response {
    frontend_error_response(
        CodexFrontendWire::OpenAiResponses,
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        message,
        stream,
    )
}

fn record_terminal_failure(
    record: Option<&AnthropicRecordCtx>,
    status: StatusCode,
    error_type: &str,
) {
    if let Some(ctx) = record {
        record_codex_error(
            ctx,
            status.as_u16(),
            error_type,
            "upstream Responses response ended before completion",
            None,
            "openai_responses",
        );
    }
}

fn parse_error(bytes: &[u8]) -> ResponseError {
    let value = serde_json::from_slice::<Value>(bytes).unwrap_or(Value::Null);
    serde_json::from_value(value.get("error").cloned().unwrap_or(value)).unwrap_or_default()
}

async fn complete(
    mut upstream: reqwest::Response,
    record: Option<AnthropicRecordCtx>,
    _in_flight: Option<InFlightGuard>,
    model: &str,
) -> Response {
    let mut splitter = ResponsesSse::new();
    let mut evidence = SseSplitter::new();
    let mut provider = None;
    let mut capture = TurnCapture::new();
    loop {
        match upstream.chunk().await {
            Ok(Some(chunk)) => {
                for event in evidence.feed(&chunk) {
                    note_provider(&event, &mut provider);
                }
                for event in splitter.feed(&chunk) {
                    capture.observe(&event);
                }
            }
            Ok(None) => break,
            Err(_) => {
                record_terminal_failure(record.as_ref(), StatusCode::OK, "ambiguous_transport");
                return upstream_failure(ErrorWire::Openai, "upstream Responses stream failed");
            }
        }
    }
    if let Some(event) = evidence.finish() {
        note_provider(&event, &mut provider);
    }
    if let Some(event) = splitter.finish() {
        capture.observe(&event);
    }
    if let Some(error) = capture.error() {
        let kind = translate::anthropic_error_type(error);
        let message = error.message.as_deref().unwrap_or("upstream error");
        if let Some(ctx) = record.as_ref() {
            record_codex_error(ctx, 200, kind, message, error.resets_at, "openai_responses");
        }
        return frontend_error_response(
            CodexFrontendWire::OpenAiResponses,
            StatusCode::OK,
            kind,
            message,
            false,
        );
    }
    if !capture.turn_ended() {
        record_terminal_failure(record.as_ref(), StatusCode::OK, "response_protocol");
        return upstream_failure(
            ErrorWire::Openai,
            "upstream Responses turn ended prematurely",
        );
    }
    if let Some(ctx) = record.as_ref() {
        if capture.usage().is_some() {
            record_responses_measurement(
                ctx,
                &capture,
                None,
                200,
                "openai_responses",
                provider.as_deref(),
            );
        } else {
            ctx.server.note_binding_observed(
                &ctx.binding_id,
                capture.model(),
                super::record::now_ms(),
                None,
            );
        }
    }
    let turn = translate::codex_backend::canonical_turn_from_capture(&capture);
    let value = translate::openai_responses_from_canonical(model, &turn);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&value).expect("canonical response serialises"),
        ))
        .expect("static JSON response")
}

struct StreamState {
    upstream: reqwest::Response,
    sse: ResponsesSse,
    evidence: SseSplitter,
    provider: Option<String>,
    capture: TurnCapture,
    backend: translate::codex_backend::CanonStream,
    frontend: OpenAiResponsesRenderer,
    pending: VecDeque<Bytes>,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    ended: bool,
}

fn stream_events(
    upstream: reqwest::Response,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    model: String,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    let state = StreamState {
        upstream,
        sse: ResponsesSse::new(),
        evidence: SseSplitter::new(),
        provider: None,
        capture: TurnCapture::new(),
        backend: translate::codex_backend::CanonStream::new(),
        frontend: OpenAiResponsesRenderer::new(&model),
        pending: VecDeque::new(),
        record,
        in_flight,
        ended: false,
    };
    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(bytes) = state.pending.pop_front() {
                return Some((Ok(bytes), state));
            }
            if state.ended {
                state.finish();
                return None;
            }
            match state.upstream.chunk().await {
                Ok(Some(chunk)) => state.feed(&chunk),
                Ok(None) => {
                    if let Some(event) = state.evidence.finish() {
                        note_provider(&event, &mut state.provider);
                    }
                    if let Some(event) = state.sse.finish() {
                        state.feed_event(event);
                    }
                    if state.frontend.turn_ended() {
                        state.ended = true;
                    } else {
                        state.record_failure("response_protocol");
                        state.ended = true;
                        return Some((
                            Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "Responses turn ended without a terminal event",
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
            record_codex_error(
                &ctx,
                200,
                error_type,
                "upstream Responses stream ended before completion",
                None,
                "openai_responses",
            );
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        for event in self.evidence.feed(chunk) {
            note_provider(&event, &mut self.provider);
        }
        for event in self.sse.feed(chunk) {
            self.feed_event(event);
        }
    }

    fn feed_event(&mut self, event: crate::providers::codex::ResponseEvent) {
        self.capture.observe(&event);
        for canonical in self.backend.feed(&event) {
            for rendered in self.frontend.feed(&canonical) {
                self.pending.push_back(sse_bytes(&rendered));
            }
        }
    }

    fn finish(&mut self) {
        if let Some(ctx) = self.record.take()
            && self.capture.turn_ended()
        {
            if let Some(error) = self.capture.error() {
                let kind = translate::anthropic_error_type(error);
                let message = error.message.as_deref().unwrap_or("upstream error");
                record_codex_error(
                    &ctx,
                    200,
                    kind,
                    message,
                    error.resets_at,
                    "openai_responses",
                );
            } else if self.capture.usage().is_some() {
                record_responses_measurement(
                    &ctx,
                    &self.capture,
                    None,
                    200,
                    "openai_responses",
                    self.provider.as_deref(),
                );
            } else {
                ctx.server.note_binding_observed(
                    &ctx.binding_id,
                    self.capture.model(),
                    super::record::now_ms(),
                    None,
                );
            }
        }
        drop(self.in_flight.take());
    }
}

impl Drop for StreamState {
    fn drop(&mut self) {
        if self.record.is_some() {
            self.record_failure("client_cancelled");
        }
    }
}

fn note_provider(event: &SseEvent, provider: &mut Option<String>) {
    let Ok(value) = serde_json::from_str::<Value>(&event.data()) else {
        return;
    };
    if let Some(named) = value
        .get("response")
        .and_then(|response| response.get("provider"))
        .and_then(Value::as_str)
    {
        *provider = Some(named.to_owned());
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
