//! The codex translation branch (plan: Phases — "Codex").
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
//! map — so [`CodexTurn::served_model`] is the final effective model,
//! the one the codex request carries.
//!
//! Recording: a measurement row on a completed turn (usage from
//! [`crate::providers::codex::TurnCapture`], the usage object verbatim
//! in `usage_raw`, cost NULL — there is no honest per-token price for
//! codex slugs, never guessed), an error row on a failed one. The
//! codex meters (`x-codex-*` headers) feed the per-backend meter slot
//! on EVERY response — the "not just accounted ones" rule.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;

use super::Server;
use crate::ir::AnthropicShape;
use crate::ir::canonical::{CanonicalRequest, ResponseProjection};
use crate::middleware::lanes;
use crate::observe::SseEvent;
use crate::providers::Provider;
use crate::providers::codex::{ResponseError, ResponseEvent, ResponsesSse, TurnCapture};
use crate::routing::{BackendAdapterId, ProtocolId};
use crate::server::InFlightGuard;
use crate::server::proxy::{
    ErrorWire, MAX_ERROR_BODY, MAX_REQUEST_BODY, buffer_up_to, plain_status, session_id,
    transport_failure, truncated_body, upstream_failure,
};
use crate::server::record::now_ms;
use crate::server::record_anthropic::AnthropicRecordCtx;
use crate::server::record_anthropic::{record_codex_error, record_responses_measurement};
use crate::translate::{self, TranslateError};

/// Everything the branch needs from a translated frontend pipeline,
/// moved in.
pub(crate) struct CodexTurn {
    pub(crate) server: Server,
    /// The routed backend (`codex_sub`), for endpoint + meter parsing.
    pub(crate) backend: Arc<dyn Provider>,
    /// The parsed, middleware-transformed request.
    pub(crate) canonical: Option<Result<CanonicalRequest, TranslateError>>,
    /// The request's own shape, pre-transform (the row's shape fields).
    pub(crate) gate_shape: Option<AnthropicShape>,
    /// The record context, when the pipeline built one.
    pub(crate) record: Option<AnthropicRecordCtx>,
    /// The in-flight guard: rides the response stream when streaming,
    /// drops at return otherwise — a request being served holds the
    /// machine awake, and no early return may leak the count.
    pub(crate) in_flight: Option<InFlightGuard>,
    /// Session identity — the prompt-cache key when present.
    pub(crate) session_id: Option<String>,
    /// Provider-binding request identities. Native Responses clients supply
    /// these; other frontends leave them absent and the binding derives them.
    pub(crate) thread_id: Option<String>,
    pub(crate) request_id: Option<String>,
    /// The final effective model (routing, retarget, force-newest, map).
    pub(crate) served_model: Option<String>,
    /// Whether the client explicitly asked for a plain JSON Message.
    pub(crate) stream_explicitly_false: bool,
    pub(crate) frontend_wire: CodexFrontendWire,
}

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
    let in_flight = Some(server.begin_in_flight());
    let header_session = session_id(&server.config.session_header_names, &parts.headers);
    let ping = lanes::is_ping(&parts.headers, &server.config.ping_header_name);
    let frontend = super::frontend_of(&parts.extensions).map(str::to_owned);
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
    let target = match server
        .registry
        .resolve(ProtocolId::OpenAiResponses, requested_model.as_deref())
    {
        Ok(target) => target,
        Err(_) => return super::responses_not_configured(),
    };
    let backend = target.provider().clone();
    let effective_model = target.effective_model().map(str::to_owned);
    let stream_explicitly_false = parsed
        .as_ref()
        .is_some_and(|request| request.get("stream").and_then(Value::as_bool) == Some(false));
    let record = parsed.as_ref().map(|request| AnthropicRecordCtx {
        server: server.clone(),
        started,
        path: "/v1/responses",
        session_id: request_session.clone(),
        requested_model: requested_model.clone(),
        effective_model: effective_model.clone(),
        backend: backend.clone(),
        betas: None,
        shape: canonical
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .map(|canonical| {
                AnthropicShape::from_canonical_responses(
                    canonical,
                    original.len() as u64,
                    request
                        .get("input")
                        .and_then(Value::as_array)
                        .map(|input| input.len() as u64),
                )
            }),
        ping,
        downgraded_from: None,
        downgraded_to: None,
        cache_stripped: None,
        system_merged: None,
        forced_from: None,
        forced_to: None,
        model_mappings: None,
        frontend,
        thinking_rewritten: false,
        translation_report: None,
    });

    if target
        .binding()
        .canonical_backend()
        .is_some_and(|binding| binding.adapter() == BackendAdapterId::AnthropicMessages)
    {
        let Some(Ok(canonical)) = canonical else {
            return frontend_error_response(
                CodexFrontendWire::OpenAiResponses,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "the request body could not be parsed for translation to this backend",
                !stream_explicitly_false,
            );
        };
        return super::anthropic_target::turn(
            server,
            parts,
            super::anthropic_target::AnthropicInput {
                canonical,
                limit_field: parsed
                    .as_ref()
                    .and_then(|request| request.get("max_output_tokens").cloned()),
            },
            target,
            record,
            in_flight,
            super::anthropic_target::FrontendWire::Responses,
        )
        .await;
    }

    if target
        .binding()
        .canonical_backend()
        .is_some_and(|binding| binding.adapter() == BackendAdapterId::OpenRouterResponses)
    {
        let Some(Ok(canonical)) = canonical else {
            return frontend_error_response(
                CodexFrontendWire::OpenAiResponses,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "the request body could not be parsed for translation to this backend",
                !stream_explicitly_false,
            );
        };
        return super::openrouter_responses::turn(
            server,
            parts,
            canonical,
            parsed
                .as_ref()
                .is_some_and(|request| request.get("prompt_cache_key").is_some()),
            target,
            record,
            in_flight,
        )
        .await;
    }

    turn(CodexTurn {
        server,
        backend,
        canonical,
        gate_shape: None,
        record,
        in_flight,
        session_id: request_session,
        thread_id,
        request_id,
        served_model: effective_model,
        stream_explicitly_false,
        frontend_wire: CodexFrontendWire::OpenAiResponses,
    })
    .await
}

/// One turn: translate, send, translate back, record.
pub(crate) async fn turn(args: CodexTurn) -> Response {
    let CodexTurn {
        server,
        backend,
        canonical,
        gate_shape,
        mut record,
        in_flight,
        session_id,
        thread_id,
        request_id,
        served_model,
        stream_explicitly_false,
        frontend_wire,
    } = args;

    // A shape with no messages is untranslatable chat-wise (count_tokens
    // and batches never reach this branch — the pipeline intercepts
    // them with a typed error before here).
    let _ = gate_shape;

    let Some(canonical) = canonical else {
        // An untranslatable body on a translated route: the honest
        // answer names the problem; nothing reached the upstream, and
        // there is no row (no measurement, no provider response).
        return frontend_error_response(
            frontend_wire,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "the request body could not be parsed for translation to this backend",
            !stream_explicitly_false,
        );
    };
    let response_projection = canonical
        .as_ref()
        .ok()
        .map(ResponseProjection::for_request)
        .unwrap_or_default();

    let model = served_model.clone().unwrap_or_default();
    let prompt_cache_key = session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let thread_id = thread_id.unwrap_or_else(|| prompt_cache_key.clone());
    let request_id = request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // The translation itself — pure; a typed failure never reaches the
    // upstream and answers in the frontend's error shape.
    let rendering = canonical.and_then(|mut canonical| {
        canonical.model = Some(model.clone());
        translate::render_codex(&canonical, &prompt_cache_key)
    });
    let rendered = match rendering {
        Ok(rendered) => rendered,
        Err(error) => {
            let (status, message) = translate_failure(&error);
            if let Some(ctx) = record.as_ref() {
                record_codex_error(
                    ctx,
                    status.as_u16(),
                    "invalid_request_error",
                    &message,
                    None,
                    frontend_wire.protocol(),
                );
            }
            return frontend_error_response(
                frontend_wire,
                status,
                "invalid_request_error",
                &message,
                !stream_explicitly_false,
            );
        }
    };
    if !rendered.report.is_empty()
        && let Some(ctx) = record.as_mut()
    {
        ctx.translation_report = Some(rendered.report);
    }
    let request = rendered.value;
    let body = match serde_json::to_vec(&request) {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(%error, "codex request serialisation failed");
            return plain_status(StatusCode::INTERNAL_SERVER_ERROR, "translation failed\n");
        }
    };

    // Auth for this turn (refreshing first when the login says so). A
    // refresh failure is a toker-side failure: no upstream response, no
    // row, 502 — the same shape the byte-forward paths give.
    let now = now_ms() / 1000;
    // Only the codex backend routes here, and it exists only when its
    // block does; the check keeps a broken invariant a typed answer.
    let Some(codex) = server.codex_turn.clone() else {
        return match frontend_wire {
            CodexFrontendWire::Anthropic => super::anthropic_not_configured(),
            CodexFrontendWire::OpenAiChat => super::openai_not_configured(),
            CodexFrontendWire::OpenAiResponses => super::responses_not_configured(),
        };
    };
    let auth = match codex.auth_for_turn(&server.http, now).await {
        Ok(auth) => auth,
        Err(error) => {
            tracing::warn!(%error, "codex auth refresh failed");
            // A fixed message, not the error's: it names local paths, and
            // the log line above already has the detail.
            return upstream_failure(
                frontend_wire.error_wire(),
                "toker upstream error: the codex login could not be refreshed",
            );
        }
    };

    let headers = codex.turn_headers(auth.as_ref(), &prompt_cache_key, &thread_id, &request_id);
    let url = backend.endpoint("/responses");
    let upstream = match server
        .http
        .post(url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => {
            tracing::warn!(%error, "codex upstream request failed");
            return transport_failure(frontend_wire.error_wire(), &error);
        }
    };

    let status = upstream.status();
    let meter_snapshot = backend.meters(upstream.headers());
    if let Some(snapshot) = &meter_snapshot {
        server.note_quota(backend.id(), snapshot);
        let row = crate::store::MetersSnapshot {
            updated_ms: now_ms(),
            snapshot: snapshot.clone(),
        };
        if let Err(error) = server.store.save_meters(backend.id(), &row) {
            tracing::error!(%error, "codex meter snapshot save failed");
        }
    }

    if !status.is_success() {
        // A provider error, translated: the client sees its protocol's
        // error naming the mapped type; the row records the real
        // upstream status with the same mapping.
        let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
            return truncated_body(frontend_wire.error_wire());
        };
        let error = parse_upstream_error(&buffered.bytes);
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
        return frontend_error_response(
            frontend_wire,
            status,
            kind,
            &message,
            !stream_explicitly_false,
        );
    }

    if stream_explicitly_false {
        aggregated_turn(
            record,
            in_flight,
            model,
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
            model,
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
/// goes away or the turn completes, whichever comes first). The row
/// lands when the turn completes — a hung-up stream records nothing,
/// like every other path. An upstream that fails mid-turn (a reset, the
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
        for emitted in self.frontend.feed(event, self.response_projection) {
            self.pending.push_back(sse_bytes(&emitted));
        }
        self.capture.observe(event);
    }

    /// The upstream failed mid-turn: no row (no usage arrived), the
    /// in-flight hold released now, and the error queued for the client.
    fn fail(&mut self, error: std::io::Error) {
        self.done = true;
        self.record = None;
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
        // A stream that died without a terminator never reaches here
        // ([`StreamState::fail`]), and a hung-up client produces no row.
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
/// 400s use. An unparseable body degrades to a generic error with no
/// invented fields.
fn parse_upstream_error(body: &[u8]) -> ResponseError {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|wrapper| {
            if let Some(error) = wrapper.get("error") {
                return serde_json::from_value::<ResponseError>(error.clone()).ok();
            }
            if let Some(detail) = wrapper.get("detail").cloned() {
                // {"detail": "..."} — the message is the whole payload.
                if let Some(message) = detail.as_str() {
                    return Some(ResponseError {
                        kind: None,
                        code: None,
                        message: Some(message.to_owned()),
                        resets_at: None,
                    });
                }
                // A structured detail object still names a message.
                return serde_json::from_value::<ResponseError>(detail).ok();
            }
            None
        })
        .unwrap_or_default()
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

pub(crate) fn frontend_error_response(
    wire: CodexFrontendWire,
    status: StatusCode,
    kind: &str,
    message: &str,
    stream: bool,
) -> Response {
    if wire == CodexFrontendWire::Anthropic {
        return anthropic_error_response(status, kind, message, stream);
    }
    let value = serde_json::json!({
        "error": {"type": kind, "message": message}
    });
    let json = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned());
    let (content_type, body) = if stream {
        ("text/event-stream", format!("data: {json}\n\n"))
    } else {
        ("application/json", json)
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("a plain error response builds")
}
