//! Responses frontend to the Anthropic API's Messages binding.
//!
//! Request and response both cross canonical adapters. The API key and
//! version header remain provider-owned; a foreign frontend bearer is removed
//! by the provider before its key is injected. Observation sees provider
//! bytes before the Responses renderer sees canonical events.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;

use crate::ir::Request as IrRequest;
use crate::ir::canonical::CanonEvent;
use crate::observe::{AnthropicObserver, SseEvent, SseSplitter};
use crate::routing::{ModelTarget, ProtocolId};
use crate::translate::{self, AnthropicResponseStream, OpenAiResponsesRenderer};

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

pub(crate) async fn turn(
    server: Server,
    parts: Parts,
    parsed: IrRequest,
    target: ModelTarget,
    mut record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
) -> Response {
    let model = target.effective_model().unwrap_or_default().to_owned();
    let stream = parsed.value().get("stream").and_then(Value::as_bool) != Some(false);
    let mut canonical = match translate::from_openai_responses(parsed.value()) {
        Ok(canonical) => canonical,
        Err(error) => return invalid_request(&error.to_string(), stream),
    };
    canonical.model = Some(model.clone());
    canonical.stream = Some(stream);

    // Responses makes this limit optional; Messages requires one. Prefer
    // the caller's positive bound. Otherwise use the model's own declared
    // maximum from the fetched Anthropic catalogue, never a guessed limit.
    let requested_limit = parsed.value().get("max_output_tokens");
    let limit = match requested_limit {
        Some(Value::Number(number)) => number.as_u64().filter(|limit| *limit > 0),
        None | Some(Value::Null) => declared_output_limit(&server, &model),
        Some(_) => None,
    };
    let Some(limit) = limit else {
        return invalid_request(
            "max_output_tokens must be a positive integer, or the Anthropic model catalogue must declare max_tokens",
            stream,
        );
    };
    canonical.sampling.max_tokens = Some(limit);
    canonical
        .extensions
        .retain(|extension| extension.wire_path() != "$.max_output_tokens");

    let rendered = match translate::render_anthropic(&canonical, target.binding().dialect()) {
        Ok(rendered) => rendered,
        Err(error) => return invalid_request(&error.to_string(), stream),
    };
    if !rendered.report.is_empty()
        && let Some(ctx) = record.as_mut()
    {
        ctx.translation_report = Some(rendered.report);
    }
    let body = match serde_json::to_vec(&rendered.value) {
        Ok(body) => Bytes::from(body),
        Err(error) => {
            tracing::error!(%error, "Messages backend serialisation failed");
            return upstream_failure(ErrorWire::Openai, "toker could not render the request");
        }
    };
    let backend = target.provider();
    let upstream = match send_upstream_to(
        &server,
        backend.as_ref(),
        &parts,
        body,
        &server.config.session_header_names,
        ProtocolId::OpenAiResponses,
        "/v1/messages",
    )
    .await
    {
        Ok(upstream) => upstream,
        Err(error) => return transport_failure(ErrorWire::Openai, &error),
    };
    forward(upstream, record, in_flight, &model).await
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

fn invalid_request(message: &str, stream: bool) -> Response {
    frontend_error_response(
        CodexFrontendWire::OpenAiResponses,
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
) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    if is_compressed(&headers) {
        return upstream_failure(
            ErrorWire::Openai,
            "toker could not interpret a compressed Messages response",
        );
    }
    if !status.is_success() {
        let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
            return truncated_body(ErrorWire::Openai);
        };
        if buffered.rest.is_some() {
            return upstream_failure(ErrorWire::Openai, "upstream error was too large");
        }
        let parsed = serde_json::from_slice::<Value>(&buffered.bytes).ok();
        let Some(turn) = parsed
            .as_ref()
            .and_then(|body| translate::canonical_turn_from_anthropic(body).ok())
        else {
            return upstream_failure(ErrorWire::Openai, "invalid upstream error response");
        };
        if let Some(ctx) = record.as_ref() {
            let (kind, message) = error_pair(parsed.as_ref());
            record_anthropic_error(ctx, status.as_u16(), kind, message, None, None);
        }
        return json_response(
            status,
            &headers,
            &translate::openai_responses_from_canonical(model, &turn),
        );
    }
    if is_event_stream(&headers) {
        let mut response_headers = response_headers(&headers, false);
        response_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        let body = Body::from_stream(stream_events(upstream, record, in_flight, model.to_owned()));
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
        return truncated_body(ErrorWire::Openai);
    };
    if buffered.rest.is_some() {
        return upstream_failure(ErrorWire::Openai, "upstream response was too large");
    }
    let turn = match serde_json::from_slice::<Value>(&buffered.bytes)
        .ok()
        .and_then(|body| translate::canonical_turn_from_anthropic(&body).ok())
    {
        Some(turn) => turn,
        None => return upstream_failure(ErrorWire::Openai, "invalid upstream response"),
    };
    if let Some(ctx) = record.as_ref() {
        let mut observer = AnthropicObserver::new();
        observer.observe_json(&buffered.bytes);
        let capture = observer.finish();
        record_anthropic_measurement(ctx, capture.as_ref(), None, status.as_u16());
    }
    json_response(
        status,
        &headers,
        &translate::openai_responses_from_canonical(model, &turn),
    )
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
    frontend: OpenAiResponsesRenderer,
    observer: AnthropicObserver,
    pending: VecDeque<Bytes>,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    ended: bool,
    succeeded: bool,
}

fn stream_events(
    upstream: reqwest::Response,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    model: String,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    let state = StreamState {
        upstream,
        splitter: SseSplitter::new(),
        backend: AnthropicResponseStream::new(),
        frontend: OpenAiResponsesRenderer::new(&model),
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
                    let capture = std::mem::take(&mut state.observer).finish();
                    record_anthropic_measurement(&ctx, capture.as_ref(), None, 200);
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
                        state.record.take();
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
                    state.record.take();
                    state.ended = true;
                    return Some((Err(std::io::Error::other(error)), state));
                }
            }
        }
    })
}

impl StreamState {
    fn feed(&mut self, event: SseEvent) {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.observer.observe_event(&event);
        }));
        for canonical in self.backend.feed_sse(&event) {
            match canonical {
                CanonEvent::TurnEnded { .. } => self.succeeded = true,
                CanonEvent::TurnFailed { .. } => self.succeeded = false,
                _ => {}
            }
            for emitted in self.frontend.feed(&canonical) {
                self.pending.push_back(sse_bytes(&emitted));
            }
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
