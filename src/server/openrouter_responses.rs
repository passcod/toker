//! OpenRouter's live-verified streamed Responses binding. The provider owns
//! auth, endpoint and attested billed cost; canonical adapters own both
//! directions. A JSON frontend response aggregates the verified SSE wire.

use std::collections::VecDeque;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;

use crate::ir::canonical::CanonicalRequest;
use crate::observe::{SseEvent, SseSplitter};
use crate::providers::codex::{ResponseError, ResponsesSse, TurnCapture};
use crate::routing::{ModelTarget, ProtocolId};
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

pub(crate) async fn turn(
    server: Server,
    parts: Parts,
    mut canonical: CanonicalRequest,
    had_prompt_cache_key: bool,
    target: ModelTarget,
    mut record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
) -> Response {
    let model = target.effective_model().unwrap_or_default().to_owned();
    let wants_stream = canonical.stream != Some(false);
    canonical.model = Some(model.clone());
    let rendered = match translate::openrouter_responses_backend::render_openrouter_responses(
        &canonical,
        had_prompt_cache_key,
    ) {
        Ok(rendered) => rendered,
        Err(error) => return invalid_request(&error.to_string(), wants_stream),
    };
    if !rendered.report.is_empty()
        && let Some(ctx) = record.as_mut()
    {
        ctx.translation_report = Some(rendered.report);
    }
    let body = Bytes::from(
        serde_json::to_vec(&rendered.value).expect("a rendered Responses request serialises"),
    );
    let upstream = match send_upstream_to(
        &server,
        target.provider().as_ref(),
        &parts,
        body,
        &[],
        ProtocolId::OpenAiResponses,
        "/v1/responses",
    )
    .await
    {
        Ok(upstream) => upstream,
        Err(error) => return transport_failure(ErrorWire::Openai, &error),
    };
    let status = upstream.status();
    let headers = upstream.headers().clone();
    if is_compressed(&headers) {
        return upstream_failure(
            ErrorWire::Openai,
            "toker could not interpret a compressed Responses response",
        );
    }
    if !status.is_success() {
        let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
            return truncated_body(ErrorWire::Openai);
        };
        if buffered.rest.is_some() {
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
        return upstream_failure(ErrorWire::Openai, "OpenRouter Responses did not stream");
    }
    if wants_stream {
        let mut outbound = response_headers(&headers, false);
        outbound.insert(
            header::CONTENT_TYPE,
            "text/event-stream".parse().expect("static content type"),
        );
        let body = Body::from_stream(stream_events(upstream, record, in_flight, model));
        let mut response = Response::new(body);
        *response.status_mut() = status;
        *response.headers_mut() = outbound;
        response
    } else {
        complete(upstream, record, in_flight, &model).await
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
        return upstream_failure(
            ErrorWire::Openai,
            "upstream Responses turn ended prematurely",
        );
    }
    if let Some(ctx) = record.as_ref()
        && capture.usage().is_some()
    {
        record_responses_measurement(
            ctx,
            &capture,
            None,
            200,
            "openai_responses",
            provider.as_deref(),
        );
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
                        state.record.take();
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
                    state.record.take();
                    state.ended = true;
                    return Some((Err(std::io::Error::other(error)), state));
                }
            }
        }
    })
}

impl StreamState {
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
            }
        }
        drop(self.in_flight.take());
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
