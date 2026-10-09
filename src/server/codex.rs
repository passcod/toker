//! The codex translation branch (plan: Phases — "Codex").
//!
//! A request routed to the codex_sub backend never byte-forwards: the
//! codex backend speaks the Responses dialect, so the anthropic-frontend
//! body goes through [`crate::translate`] both ways —
//! [`translate::to_codex`] for the request, [`translate::AnthropicStream`]
//! for the response. The fidelity byte-compare is meaningless
//! cross-protocol (the upstream bytes never existed on the frontend's
//! wire), so translated routes skip it by construction.
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
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;

use super::Server;
use crate::ir::{AnthropicShape, Fidelity, Request as IrRequest, compare};
use crate::middleware::lanes;
use crate::observe::SseEvent;
use crate::providers::Provider;
use crate::providers::codex::{ResponseError, ResponseEvent, ResponsesSse, TurnCapture};
use crate::server::InFlightGuard;
use crate::server::proxy::{
    ErrorWire, MAX_ERROR_BODY, MAX_REQUEST_BODY, buffer_up_to, buffered_body, build_response,
    forward_upstream, is_compressed, plain_status, response_headers, session_id, transport_failure,
    truncated_body, upstream_failure, upstream_request_headers,
};
use crate::server::record::now_ms;
use crate::server::record_anthropic::AnthropicRecordCtx;
use crate::server::record_anthropic::{record_codex_error, record_codex_measurement};
use crate::translate::{self, TranslateError};

/// Everything the branch needs from the anthropic pipeline, moved in.
pub(crate) struct CodexTurn {
    pub(crate) server: Server,
    /// The routed backend (`codex_sub`), for endpoint + meter parsing.
    pub(crate) backend: Arc<dyn Provider>,
    /// The parsed, middleware-transformed request.
    pub(crate) parsed: Option<IrRequest>,
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
    /// The final effective model (routing, retarget, force-newest, map).
    pub(crate) served_model: Option<String>,
    /// Whether the client explicitly asked for a plain JSON Message.
    pub(crate) stream_explicitly_false: bool,
}

const ANTHROPIC_FRONTEND: &str = "anthropic";
const RESPONSES_FRONTEND: &str = "openai_responses";

/// Codex's model catalogue, in the exact shape the CLI expects.
///
/// `/v1/models` is a frontend spelling only. The subscription backend's
/// catalogue lives at `/models?client_version=...`; returning OpenRouter's
/// list here makes Codex miss the exact slug and fall back to guessed model
/// metadata. This path is not usage and writes no ledger row.
pub(crate) async fn models(State(server): State<Server>, request: Request) -> Response {
    let Some(codex) = server.codex_turn.clone() else {
        return super::responses_not_configured();
    };
    let auth = match codex
        .auth_for_turn(&server.http, jiff::Timestamp::now().as_second())
        .await
    {
        Ok(auth) => auth,
        Err(error) => {
            tracing::warn!(%error, "codex auth refresh failed for models catalogue");
            return transport_failure(ErrorWire::Openai, &error);
        }
    };
    let mut url = codex.endpoint("/models");
    url.query_pairs_mut()
        .append_pair("client_version", &codex.client_version());
    let mut headers = upstream_request_headers(request.headers(), &[]);
    headers.extend(codex.models_headers(auth.as_ref()));
    match server.http.get(url).headers(headers).send().await {
        Ok(upstream) => forward_upstream(upstream, None, None).await,
        Err(error) => {
            tracing::warn!(%error, "codex models upstream request failed");
            transport_failure(ErrorWire::Openai, &error)
        }
    }
}

/// Native Codex CLI usage path. The request and response bytes stay in the
/// Responses dialect end-to-end; observation is side-band only.
pub(crate) async fn responses(State(server): State<Server>, request: Request) -> Response {
    let started = Instant::now();
    let Some(backend) = server.codex_sub.clone() else {
        return super::responses_not_configured();
    };
    let Some(codex) = server.codex_turn.clone() else {
        return super::responses_not_configured();
    };
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

    let parsed = IrRequest::parse(&original).ok();
    let body_session = parsed
        .as_ref()
        .and_then(|request| request.openai_responses().prompt_cache_key())
        .map(str::to_owned);
    let cache_key = body_session
        .clone()
        .or_else(|| header_session.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let model = parsed
        .as_ref()
        .and_then(|request| request.openai_responses().model())
        .map(str::to_owned);
    let drift =
        parsed
            .as_ref()
            .and_then(|request| match compare(&original, &request.serialise()) {
                Fidelity::Exact => None,
                Fidelity::Drift { digest, .. } => Some(digest),
            });
    let record = parsed.as_ref().map(|request| AnthropicRecordCtx {
        server: server.clone(),
        started,
        path: "/v1/responses",
        session_id: body_session.or(header_session),
        requested_model: model.clone(),
        effective_model: model.clone(),
        drift,
        backend: backend.clone(),
        betas: None,
        shape: Some(request.openai_responses().shape()),
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
    });

    let auth = match codex.auth_for_turn(&server.http, now_ms() / 1000).await {
        Ok(auth) => auth,
        Err(error) => {
            tracing::warn!(%error, "codex auth refresh failed");
            return upstream_failure(
                ErrorWire::Openai,
                "toker upstream error: the codex login could not be refreshed",
            );
        }
    };
    let thread_id = parts
        .headers
        .get("thread-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or(&cache_key);
    let request_id = parts
        .headers
        .get("x-client-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let headers = codex.turn_headers(auth.as_ref(), &cache_key, thread_id, &request_id);
    let suffix = parts
        .uri
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or("/v1/responses")
        .strip_prefix("/v1")
        .unwrap_or("/responses");
    let upstream = match server
        .http
        .post(backend.endpoint(suffix))
        .headers(headers)
        .body(original)
        .send()
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => {
            tracing::warn!(%error, "codex upstream request failed");
            return transport_failure(ErrorWire::Openai, &error);
        }
    };

    native_response(server, backend, upstream, record, in_flight).await
}

async fn native_response(
    server: Server,
    backend: Arc<dyn Provider>,
    upstream: reqwest::Response,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let meters = backend.meters(&headers);
    if let Some(snapshot) = &meters {
        server.note_quota(backend.id(), snapshot);
        if let Err(error) = server.store.save_meters(
            backend.id(),
            &crate::store::MetersSnapshot {
                updated_ms: now_ms(),
                snapshot: snapshot.clone(),
            },
        ) {
            tracing::error!(%error, "codex meter snapshot save failed");
        }
    }

    if is_compressed(&headers) {
        tracing::debug!("compressed codex response passed through unledgered");
        return build_response(
            status,
            response_headers(&headers, true),
            Body::from_stream(upstream.bytes_stream()),
        );
    }
    if !status.is_success() {
        let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
            return truncated_body(ErrorWire::Openai);
        };
        if buffered.rest.is_none()
            && let Some(ctx) = record.as_ref()
        {
            let error = parse_upstream_error(&buffered.bytes);
            let kind = error
                .kind
                .as_deref()
                .or(error.code.as_deref())
                .unwrap_or("api_error");
            let message = error
                .message
                .as_deref()
                .or(error.code.as_deref())
                .unwrap_or("upstream error");
            record_codex_error(
                ctx,
                status.as_u16(),
                kind,
                message,
                error.resets_at,
                RESPONSES_FRONTEND,
            );
        }
        return build_response(
            status,
            response_headers(&headers, false),
            buffered_body(buffered),
        );
    }

    let state = NativeStreamState {
        upstream,
        parser: ResponsesSse::new(),
        capture: TurnCapture::new(),
        done: false,
        failure: None,
        record,
        meters,
        in_flight,
    };
    let stream = futures::stream::unfold(state, |mut state| async move {
        if state.done {
            return state.failure.take().map(|error| (Err(error), state));
        }
        match state.upstream.chunk().await {
            Ok(Some(chunk)) => {
                state.observe(&chunk);
                if state.capture.turn_ended() {
                    state.finish();
                }
                Some((Ok(chunk), state))
            }
            Ok(None) => {
                if let Some(event) = state.parser.finish() {
                    state.capture.observe(&event);
                }
                if state.capture.turn_ended() || state.capture.error().is_some() {
                    state.finish();
                    None
                } else {
                    tracing::warn!("codex stream closed before a final response event");
                    state.record = None;
                    state.done = true;
                    drop(state.in_flight.take());
                    Some((
                        Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "codex stream closed before a final response event",
                        )),
                        state,
                    ))
                }
            }
            Err(error) => {
                tracing::warn!(%error, "codex stream failed mid-turn");
                state.record = None;
                state.done = true;
                drop(state.in_flight.take());
                Some((Err(std::io::Error::other(error)), state))
            }
        }
    });
    build_response(
        status,
        response_headers(&headers, false),
        Body::from_stream(stream),
    )
}

struct NativeStreamState {
    upstream: reqwest::Response,
    parser: ResponsesSse,
    capture: TurnCapture,
    done: bool,
    failure: Option<std::io::Error>,
    record: Option<AnthropicRecordCtx>,
    meters: Option<Value>,
    in_flight: Option<InFlightGuard>,
}

impl NativeStreamState {
    fn observe(&mut self, bytes: &[u8]) {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            for event in self.parser.feed(bytes) {
                self.capture.observe(&event);
            }
        }));
    }

    fn finish(&mut self) {
        self.done = true;
        let Some(ctx) = self.record.as_ref() else {
            return;
        };
        if let Some(error) = self.capture.error() {
            let kind = error
                .kind
                .as_deref()
                .or(error.code.as_deref())
                .unwrap_or("api_error");
            let message = error
                .message
                .as_deref()
                .or(error.code.as_deref())
                .unwrap_or("upstream error");
            record_codex_error(ctx, 200, kind, message, error.resets_at, RESPONSES_FRONTEND);
        } else {
            record_codex_measurement(
                ctx,
                &self.capture,
                self.meters.clone(),
                200,
                RESPONSES_FRONTEND,
            );
        }
        drop(self.in_flight.take());
    }
}

/// One turn: translate, send, translate back, record.
pub(crate) async fn turn(args: CodexTurn) -> Response {
    let CodexTurn {
        server,
        backend,
        parsed,
        gate_shape,
        record,
        in_flight,
        session_id,
        served_model,
        stream_explicitly_false,
    } = args;

    // A shape with no messages is untranslatable chat-wise (count_tokens
    // and batches never reach this branch — the pipeline intercepts
    // them with a typed error before here).
    let _ = gate_shape;

    let Some(ir) = parsed else {
        // An untranslatable body on a translated route: the honest
        // answer names the problem; nothing reached the upstream, and
        // there is no row (no measurement, no provider response).
        return anthropic_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "the request body could not be parsed for translation to this backend",
            !stream_explicitly_false,
        );
    };

    let model = served_model.clone().unwrap_or_default();
    let prompt_cache_key = session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let thread_id = prompt_cache_key.clone();
    let request_id = uuid::Uuid::new_v4().to_string();

    // The translation itself — pure; a typed failure never reaches the
    // upstream and answers with an anthropic error naming the cause.
    let request = match translate::to_codex(ir.value(), &model, &prompt_cache_key) {
        Ok(request) => request,
        Err(error) => {
            let (status, message) = translate_failure(&error);
            if let Some(ctx) = record.as_ref() {
                record_codex_error(
                    ctx,
                    status.as_u16(),
                    "invalid_request_error",
                    &message,
                    None,
                    ANTHROPIC_FRONTEND,
                );
            }
            return anthropic_error_response(
                status,
                "invalid_request_error",
                &message,
                !stream_explicitly_false,
            );
        }
    };
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
        return super::anthropic_not_configured();
    };
    let auth = match codex.auth_for_turn(&server.http, now).await {
        Ok(auth) => auth,
        Err(error) => {
            tracing::warn!(%error, "codex auth refresh failed");
            // A fixed message, not the error's: it names local paths, and
            // the log line above already has the detail.
            return upstream_failure(
                ErrorWire::Anthropic,
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
            return transport_failure(ErrorWire::Anthropic, &error);
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
        // A provider error, translated: the client sees an anthropic
        // error naming the mapped type; the row records the real
        // upstream status with the same mapping.
        let Ok(buffered) = buffer_up_to(upstream, MAX_ERROR_BODY).await else {
            return truncated_body(ErrorWire::Anthropic);
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
                ANTHROPIC_FRONTEND,
            );
        }
        return anthropic_error_response(status, kind, &message, !stream_explicitly_false);
    }

    if stream_explicitly_false {
        aggregated_turn(
            server,
            backend,
            record,
            in_flight,
            model,
            upstream,
            meter_snapshot,
        )
        .await
    } else {
        streamed_turn(
            server,
            backend,
            record,
            in_flight,
            model,
            upstream,
            meter_snapshot,
        )
        .await
    }
}

/// The `stream: false` shape: consume the whole turn, answer with the
/// complete Message JSON (translate's aggregation), record once.
/// The `stream: false` shape: consume the whole turn, answer with the
/// complete Message JSON (translate's aggregation), record once. The
/// guard drops at return — a JSON answer is over when it is sent.
async fn aggregated_turn(
    _server: Server,
    _backend: Arc<dyn Provider>,
    record: Option<AnthropicRecordCtx>,
    _in_flight: Option<InFlightGuard>,
    model: String,
    upstream: reqwest::Response,
    meters: Option<Value>,
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
                    ErrorWire::Anthropic,
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
                ANTHROPIC_FRONTEND,
            );
        }
        return anthropic_error_response(StatusCode::OK, kind, &message, false);
    }
    if !capture.turn_ended() {
        // The stream closed before a terminator event (the codex client
        // treats this as an error; so does toker — never a partial row).
        tracing::warn!("codex stream closed before response.completed");
        return upstream_failure(
            ErrorWire::Anthropic,
            "toker upstream error: the stream failed before the turn ended",
        );
    }

    let message = translate::message_from_capture(&model, &capture);
    if let Some(ctx) = record.as_ref() {
        record_codex_measurement(ctx, &capture, meters, 200, ANTHROPIC_FRONTEND);
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
#[allow(clippy::too_many_arguments)]
async fn streamed_turn(
    _server: Server,
    _backend: Arc<dyn Provider>,
    record: Option<AnthropicRecordCtx>,
    in_flight: Option<InFlightGuard>,
    model: String,
    upstream: reqwest::Response,
    meters: Option<Value>,
) -> Response {
    let state = StreamState {
        upstream,
        sse: ResponsesSse::new(),
        anthropic: translate::AnthropicStream::new(&model),
        capture: TurnCapture::new(),
        pending: VecDeque::new(),
        done: false,
        failure: None,
        record,
        meters,
        in_flight,
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
/// anthropic stream for the client, the capture for the ledger), the
/// bytes awaiting delivery, and the in-flight guard that dies with it.
struct StreamState {
    upstream: reqwest::Response,
    sse: ResponsesSse,
    anthropic: translate::AnthropicStream,
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
}

impl StreamState {
    /// One upstream event through both observers: the anthropic stream
    /// emits the client's events, the capture latches the ledger's.
    fn observe(&mut self, event: &ResponseEvent) {
        for emitted in self.anthropic.feed(event) {
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
                ANTHROPIC_FRONTEND,
            );
        } else if self.capture.turn_ended() {
            record_codex_measurement(
                ctx,
                &self.capture,
                self.meters.clone(),
                200,
                ANTHROPIC_FRONTEND,
            );
        }
        // A stream that died without a terminator never reaches here
        // ([`StreamState::fail`]), and a hung-up client produces no row.
    }
}

/// One emitted anthropic event → its wire bytes (the same framing the
/// quota gate's synthetic turns use).
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
