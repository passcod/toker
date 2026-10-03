//! The proxy routes: chat completions (the usage path, fully recorded) and
//! models (transparent forwarding).
//!
//! Chat completions, in order (plan: Server core):
//!
//! 1. Buffer the request body fully.
//! 2. Parse the IR. A non-JSON body is forwarded unchanged with no
//!    recording — the proxy never rejects what it cannot understand
//!    (invariant 6 spirit).
//! 3. Fidelity check (invariant 5): byte-compare the re-serialised IR with
//!    the original. Exact → forward the original, byte-identical by
//!    construction. Drift → **still** forward the original (safe), with a
//!    `fidelity-drift` row at completion — drift is a visible metric.
//! 4. Routing: `openrouter/…` routes to openrouter with the prefix
//!    stripped; bare models go to the protocol default (phase 1 hardcodes
//!    both).
//! 5. A routed request forwards the *serialised* form — a transformed
//!    request forwards what the IR produces, and purity (invariant 4)
//!    makes that stable. Recorded as requested vs effective model.
//! 6. Upstream request with hop-by-hop headers stripped,
//!    `accept-encoding: identity` forced (SSE observation needs plaintext),
//!    and the stored credential injected only when the incoming request
//!    carries no Authorization of its own (pass-through-when-present).
//! 7. A client hangup aborts the upstream (the body stream's Drop fires
//!    an [`AbortHandle`]); a hung-up stream records no row.
//! 8. Response branches: SSE streams through with the side observation;
//!    non-SSE bodies buffer, observe, and forward unchanged; unexpected
//!    compression passes through untouched and unledgered.
//! 9. Recording on completion only — [`record::RecordCtx`] → row.

use std::convert::Infallible;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures::future::{AbortHandle, Abortable};
use futures::stream::{Stream, StreamExt};

use crate::ir::{Fidelity, Request as IrRequest, compare};
use crate::observe::{SseSplitter, UsageObserver};

use super::Server;
use super::record::{
    RecordCtx, parse_error_type, record_error, record_measurement, retry_after_ms,
};

/// Request bodies are buffered for gating and the fidelity check; 64 MiB
/// is far beyond any chat body, so hitting the cap is a client bug worth a
/// named status rather than a silent OOM.
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;
/// Cap for buffered response bodies (non-streaming completions).
const MAX_RESPONSE_BUFFER: usize = 64 * 1024 * 1024;
/// Cap for buffered non-2xx bodies, which are small in practice.
const MAX_ERROR_BODY: usize = 16 * 1024 * 1024;

/// `POST /v1/chat/completions` — the usage path.
pub(crate) async fn chat_completions(State(server): State<Server>, request: Request) -> Response {
    let started = Instant::now();
    let (parts, body) = request.into_parts();

    // Session identity, read by name only — request headers are never
    // captured wholesale: they carry credentials (invariant 2).
    let session_id = session_id(&server.config.session_header_names, &parts.headers);

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

    // 2.-5. Parse, fidelity-check, route.
    let mut forward = original.clone();
    let mut record = None;
    if let Ok(mut ir) = IrRequest::parse(&original) {
        // 3. Invariant 5, verified per request: Exact is the normal case;
        // Drift forwards the original buffer either way, and lands a
        // visible fidelity-drift row at completion.
        let mut drift = None;
        if let Fidelity::Drift { digest, .. } = compare(&original, &ir.serialise()) {
            drift = Some(digest);
        }
        // 4. Routing: read the model through the typed view; a provider
        // prefix overrides the backend per request.
        let model = ir.openai_chat().model().map(str::to_owned);
        let mut effective_model = model.clone();
        if let Some(rest) = model.as_deref().and_then(strip_provider_prefix) {
            // 5. A deliberate transform: forward the serialised IR (pure
            // and deterministic, so the upstream prefix stays stable),
            // recorded as requested vs effective — nothing is "forced".
            effective_model = Some(rest.to_owned());
            ir.openai_chat_mut().set_model(rest);
            forward = Bytes::from(ir.serialise());
        }
        let shape = ir.openai_chat().shape();
        let system_messages = shape.req_messages.map(|_| {
            ir.openai_chat()
                .messages()
                .iter()
                .filter(|m| m.is_system())
                .count() as u64
        });
        record = Some(RecordCtx {
            server: server.clone(),
            started,
            session_id,
            requested_model: model,
            effective_model,
            drift,
            shape: Some(shape),
            system_messages,
        });
    }

    // 6. Upstream; 7.-9. in forward_upstream.
    match send_upstream(&server, &parts, forward).await {
        Ok(upstream) => forward_upstream(upstream, record).await,
        Err(error) => {
            // No upstream response: nothing measured, and the error row is
            // provider-response-shaped (status/type/retry-after), so this
            // surfaces as 502 unledgered and logged — never a fabricated
            // provider status.
            tracing::warn!(%error, "upstream request failed");
            plain_status(StatusCode::BAD_GATEWAY, "upstream request failed\n")
        }
    }
}

/// `GET /v1/models` — transparent forwarding. The model list is not a
/// usage path: no recording, no observation (plan: the frontend fetches
/// it through the base URL; the simplest correct dogfooding behavior).
pub(crate) async fn models(State(server): State<Server>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "request body exceeded toker's cap");
            return plain_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds toker's 64 MiB cap\n",
            );
        }
    };
    match send_upstream(&server, &parts, body).await {
        Ok(upstream) => forward_upstream(upstream, None).await,
        Err(error) => {
            tracing::warn!(%error, "upstream request failed");
            plain_status(StatusCode::BAD_GATEWAY, "upstream request failed\n")
        }
    }
}

/// Send one request upstream: mapped endpoint, cleaned headers, auth
/// injection, and the (possibly rewritten) body.
async fn send_upstream(
    server: &Server,
    parts: &Parts,
    body: Bytes,
) -> Result<reqwest::Response, reqwest::Error> {
    let path = parts
        .uri
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or_else(|| parts.uri.path());
    let url = server.openrouter.endpoint(path);
    let mut headers = upstream_request_headers(&parts.headers, &server.config.session_header_names);
    // Pass-through-when-present (plan: Credentials): a frontend that
    // brings its own Authorization keeps it verbatim; the stored
    // credential is injected only when the request carries none. If
    // neither exists the request goes unauthenticated and openrouter's
    // 401 body passes through — visibly verifying the wiring.
    if !parts.headers.contains_key(header::AUTHORIZATION) {
        server.openrouter.inject_auth(&mut headers);
    }
    server
        .http
        .request(parts.method.clone(), url)
        .headers(headers)
        .body(body)
        .send()
        .await
}

/// Forward one upstream response to the client, branching on
/// compression / status / content-type. `record` is the chat-path
/// completion context; `None` means pure transparent forwarding.
pub(crate) async fn forward_upstream(
    upstream: reqwest::Response,
    record: Option<RecordCtx>,
) -> Response {
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();

    // Unexpected compression — shouldn't happen, identity is forced —
    // passes through untouched with no recording (ledger-proxy behavior:
    // never mis-parse a compressed stream).
    if is_compressed(&upstream_headers) {
        tracing::debug!("compressed upstream response passed through unledgered");
        let body = Body::from_stream(upstream.bytes_stream());
        return build_response(status, response_headers(&upstream_headers, true), body);
    }

    let Some(ctx) = record else {
        // Transparent forwarding (models, non-JSON chat bodies): stream
        // through; nothing observed, nothing recorded.
        let body = Body::from_stream(upstream.bytes_stream());
        return build_response(status, response_headers(&upstream_headers, false), body);
    };

    // Non-2xx on a usage path: error row (status, type, retry-after —
    // never priced), body forwarded unchanged.
    if !status.is_success() {
        let buffered = buffer_up_to(upstream, MAX_ERROR_BODY).await;
        let error_type = parse_error_type(&buffered.bytes);
        let retry_after = retry_after_ms(&upstream_headers);
        record_error(&ctx, status.as_u16(), error_type, retry_after);
        let body = buffered_body(buffered);
        return build_response(status, response_headers(&upstream_headers, false), body);
    }

    // SSE: chunks stream through with backpressure (axum Body from a
    // stream), each also feeding the side observation.
    if is_event_stream(&upstream_headers) {
        let stream = ObservedStream::new(upstream, status.as_u16(), ctx);
        let body = Body::from_stream(stream);
        return build_response(status, response_headers(&upstream_headers, false), body);
    }

    // Non-SSE: buffer, observe, forward the original bytes unchanged.
    let buffered = buffer_up_to(upstream, MAX_RESPONSE_BUFFER).await;
    if buffered.rest.is_some() {
        tracing::warn!("non-streaming response exceeded the buffer cap; passed through unledgered");
        let body = buffered_body(buffered);
        return build_response(status, response_headers(&upstream_headers, false), body);
    }
    let mut observer = UsageObserver::new();
    observer.observe_json(&buffered.bytes);
    let capture = observer.finish();
    record_measurement(&ctx, capture.as_ref(), status.as_u16());
    let body = Body::from(Bytes::from(buffered.bytes));
    build_response(status, response_headers(&upstream_headers, false), body)
}

/// Phase-1 routing (plan: Routing): an `openrouter/` prefix overrides the
/// backend per request and is stripped from the model. Anything else —
/// bare names, other providers' prefixes — goes to the protocol's
/// configured default backend, untransformed. Generalising the prefix set
/// is a later phase's work.
pub(crate) fn strip_provider_prefix(model: &str) -> Option<&str> {
    model.strip_prefix("openrouter/")
}

/// The session identity from the configured header names, in priority
/// order, read by name only (invariant 2).
fn session_id(names: &[String], headers: &HeaderMap) -> Option<String> {
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
/// control headers (addressed to the proxy, not the provider).
fn upstream_request_headers(incoming: &HeaderMap, session_header_names: &[String]) -> HeaderMap {
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
fn response_headers(upstream: &HeaderMap, keep_content_encoding: bool) -> HeaderMap {
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
    outgoing
}

fn is_compressed(headers: &HeaderMap) -> bool {
    match headers
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
    {
        Some(encoding) => !encoding.trim().eq_ignore_ascii_case("identity"),
        None => false,
    }
}

fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.starts_with("text/event-stream"))
}

/// A partial buffer of a response body: `rest` is `Some` when the cap
/// overflowed and the response (positioned after `bytes`) remains, so the
/// caller can forward the remainder verbatim.
struct Buffered {
    bytes: Vec<u8>,
    rest: Option<reqwest::Response>,
}

/// Buffer a response body up to `cap` bytes.
async fn buffer_up_to(mut response: reqwest::Response, cap: usize) -> Buffered {
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len() + chunk.len() > cap {
                    return Buffered {
                        bytes,
                        rest: Some(response),
                    };
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => return Buffered { bytes, rest: None },
            Err(error) => {
                tracing::warn!(%error, "upstream response body failed mid-transfer");
                return Buffered { bytes, rest: None };
            }
        }
    }
}

/// The response body for a buffered-then-maybe-overflowed response: the
/// buffered prefix chained onto the remaining upstream stream, byte order
/// preserved.
fn buffered_body(buffered: Buffered) -> Body {
    match buffered.rest {
        Some(response) => {
            let prefix = Ok::<_, reqwest::Error>(Bytes::from(buffered.bytes));
            Body::from_stream(futures::stream::iter(vec![prefix]).chain(response.bytes_stream()))
        }
        None => Body::from(Bytes::from(buffered.bytes)),
    }
}

/// The upstream body stream type, boxed so [`ObservedStream`] can name it.
type UpstreamBody = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// The SSE response stream with the usage observation riding alongside:
/// bytes pass through verbatim (backpressured by axum's poll-driven body),
/// and each chunk *also* feeds the splitter/observer. Observation is a
/// side effect that can never fail the stream (invariant 6): the observe
/// APIs are infallible by construction, and the calls additionally run
/// under [`std::panic::catch_unwind`] so no observation bug can take a
/// live session down — the measurement is lost, not the response.
struct ObservedStream {
    /// The upstream body, wrapped [`Abortable`] so the handle below can
    /// stop it.
    inner: Pin<Box<Abortable<UpstreamBody>>>,
    /// Fires in [`Drop`]: when axum drops the response body — client
    /// hangup, shutdown — the upstream request is aborted too.
    abort: AbortHandle,
    splitter: SseSplitter,
    observer: UsageObserver,
    /// The recording context, taken at completion: only a completed
    /// stream records (a hung-up one records nothing, plan: Server core).
    ctx: Option<RecordCtx>,
    status: u16,
}

impl ObservedStream {
    fn new(response: reqwest::Response, status: u16, ctx: RecordCtx) -> ObservedStream {
        let (abort, registration) = AbortHandle::new_pair();
        let stream: UpstreamBody = Box::pin(response.bytes_stream());
        ObservedStream {
            inner: Box::pin(Abortable::new(stream, registration)),
            abort,
            splitter: SseSplitter::new(),
            observer: UsageObserver::new(),
            ctx: Some(ctx),
            status,
        }
    }
}

impl Stream for ObservedStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                observe_chunk(&mut this.splitter, &mut this.observer, &chunk);
                std::task::Poll::Ready(Some(Ok(chunk)))
            }
            std::task::Poll::Ready(Some(Err(error))) => {
                // Upstream transport died mid-stream: the response is
                // truncated wherever the client lost it. No completion, no
                // row — drop the context so a later poll cannot record one.
                tracing::warn!(%error, "upstream response stream failed");
                this.ctx.take();
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Ready(None) => {
                // Natural completion: flush the splitter's tail, finish the
                // observation, record. Recording failures log, never
                // propagate (invariant 6).
                if let Some(ctx) = this.ctx.take() {
                    let splitter = &mut this.splitter;
                    let observer = &mut this.observer;
                    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        if let Some(event) = splitter.finish() {
                            observer.observe_event(&event);
                        }
                    }));
                    let capture = std::mem::take(observer).finish();
                    record_measurement(&ctx, capture.as_ref(), this.status);
                }
                std::task::Poll::Ready(None)
            }
        }
    }
}

impl Drop for ObservedStream {
    fn drop(&mut self) {
        // Client hangup → axum drops the body → abort the upstream. Also
        // fires after natural completion, where it is a no-op.
        self.abort.abort();
    }
}

/// Feed one chunk to the side observation, panic-guarded (invariant 6:
/// accounting must never break a session — a lost measurement is the worst
/// outcome, never a lost response).
fn observe_chunk(splitter: &mut SseSplitter, observer: &mut UsageObserver, chunk: &[u8]) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        for event in splitter.feed(chunk) {
            observer.observe_event(&event);
        }
    }));
}

/// Build a response with an explicit status and header set.
fn build_response(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// A minimal status-page response for proxy-level failures.
fn plain_status(status: StatusCode, message: &'static str) -> Response {
    let mut response = Response::new(Body::from(message));
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::strip_provider_prefix;

    #[test]
    fn the_openrouter_prefix_strips_and_everything_else_goes_to_the_default() {
        assert_eq!(
            strip_provider_prefix("openrouter/z-ai/glm-5.3"),
            Some("z-ai/glm-5.3"),
            "the provider prefix routes and strips"
        );
        // Nested provider models survive intact — only toker's own prefix
        // is toker's to strip.
        assert_eq!(
            strip_provider_prefix("openrouter/openai/gpt-5.2"),
            Some("openai/gpt-5.2")
        );
        assert_eq!(strip_provider_prefix("openrouter/"), Some(""));
        // Bare models route to the protocol default, untransformed.
        assert_eq!(strip_provider_prefix("z-ai/glm-5.3"), None);
        // Other providers' prefixes are not toker's to intercept in
        // phase 1: they go to the default backend unchanged.
        assert_eq!(strip_provider_prefix("anthropic/claude-opus-5"), None);
    }
}
