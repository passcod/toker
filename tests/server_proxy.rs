//! End-to-end proxy tests: a mock openrouter upstream (capturing every
//! request byte-for-byte) behind the real toker router, driven as a client.
//!
//! Asserts the unit's contract: request bodies arrive upstream
//! byte-identical (echo capture), responses pass through byte-identical,
//! ledger rows carry the right buckets/cost kinds, error rows are never
//! priced, fidelity drift is visible, hangups abort the upstream and
//! record nothing, and the control endpoint stays gated and secret-free.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use bytes::Bytes;
use futures::Stream;
use serde_json::Value;

use toker::config::{AnthropicApiConfig, AnthropicSubConfig, Config, OpenRouterConfig};
use toker::ir::Request as IrRequest;
use toker::server::Server;
use toker::store::{CostKind, RequestRow, RowKind, Store};

/// An env name no test ever sets, so the default helper injects no key.
const UNSET_KEY_ENV: &str = "TOKER_TEST_KEY_UNSET_4B1";

// ---------------------------------------------------------------------------
// The mock upstream
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MockState {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    upstream_dropped: Arc<AtomicBool>,
}

impl Default for MockState {
    fn default() -> Self {
        MockState {
            requests: Arc::new(Mutex::new(Vec::new())),
            upstream_dropped: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

impl MockState {
    fn capture(&self, path: &str, headers: &HeaderMap, body: Bytes) {
        self.requests.lock().unwrap().push(CapturedRequest {
            path: path.to_owned(),
            headers: headers.clone(),
            body,
        });
    }

    fn captured(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn fixture(name: &str) -> Bytes {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/openai_chat_sse")
        .join(name);
    Bytes::from(std::fs::read(path).expect("fixture exists"))
}

fn raw_response(status: StatusCode, content_type: &str, body: Bytes) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).expect("content type"),
    );
    response
}

/// A non-streaming completion with every usage field the capture models.
const NON_STREAM_BODY: &str = concat!(
    r#"{"id":"gen-test-ns","provider":"z-ai","model":"z-ai/glm-5.3","#,
    r#""object":"chat.completion","created":1760000600,"#,
    r#""choices":[{"index":0,"message":{"role":"assistant","content":"Done"},"finish_reason":"stop"}],"#,
    r#""usage":{"prompt_tokens":64,"completion_tokens":8,"total_tokens":72,"#,
    r#""prompt_tokens_details":{"cached_tokens":16},"#,
    r#""completion_tokens_details":{"reasoning_tokens":4},"#,
    r#""cost":0.000128,"cost_details":{"upstream":0.0001}}}"#,
);

/// OpenAI/OpenRouter error shape: `type` plus a numeric code.
const ERROR_BODY: &str = r#"{"error":{"message":"No auth credentials found.","type":"invalid_request_error","code":401}}"#;

const MODELS_BODY: &str = r#"{"object":"list","data":[{"id":"z-ai/glm-5.3","object":"model"},{"id":"openai/gpt-5.2","object":"model"}]}"#;

async fn mock_chat(State(mock): State<MockState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .expect("mock reads body");
    mock.capture(parts.uri.path(), &parts.headers, body.clone());

    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let model = json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let stream = json.get("stream") == Some(&Value::Bool(true));

    match model {
        "err-401" => {
            let mut response = raw_response(
                StatusCode::UNAUTHORIZED,
                "application/json",
                Bytes::from_static(ERROR_BODY.as_bytes()),
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("7"));
            response
        }
        "gzip-me" => {
            // "Compressed" bytes that are not really gzip: the point is
            // verbatim passthrough, not decompression.
            let mut response = raw_response(
                StatusCode::OK,
                "application/json",
                Bytes::from_static(b"\x1f\x8b-not-really-gzip"),
            );
            response
                .headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            response
        }
        "hang" if stream => {
            // First event, then nothing — the client hangs up mid-stream
            // and the abort must propagate (the Drop flag proves it).
            let body = Body::from_stream(Hanging {
                mock: mock.clone(),
                first: Some(Bytes::from_static(
                    b"data: {\"id\":\"gen-hang\",\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
                )),
            });
            let mut response = Response::new(body);
            *response.status_mut() = StatusCode::OK;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            response
        }
        _ if stream => raw_response(
            StatusCode::OK,
            "text/event-stream",
            fixture("01_simple_content.sse"),
        ),
        _ => raw_response(
            StatusCode::OK,
            "application/json",
            Bytes::from_static(NON_STREAM_BODY.as_bytes()),
        ),
    }
}

/// An SSE body that yields one chunk then never completes, marking its
/// Drop so the test can see the upstream abort.
struct Hanging {
    mock: MockState,
    first: Option<Bytes>,
}

impl Stream for Hanging {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut().first.take() {
            Some(chunk) => Poll::Ready(Some(Ok(chunk))),
            None => Poll::Pending,
        }
    }
}

impl Drop for Hanging {
    fn drop(&mut self) {
        self.mock.upstream_dropped.store(true, Ordering::SeqCst);
    }
}

async fn mock_models(State(mock): State<MockState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .expect("mock reads body");
    mock.capture(parts.uri.path(), &parts.headers, body);
    raw_response(
        StatusCode::OK,
        "application/json",
        Bytes::from_static(MODELS_BODY.as_bytes()),
    )
}

async fn spawn_mock() -> (MockState, reqwest::Url) {
    let state = MockState::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_chat))
        .route("/v1/models", get(mock_models))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("mock binds");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock serves");
    });
    let upstream: reqwest::Url = format!("http://{addr}/v1").parse().expect("upstream url");
    (state, upstream)
}

// ---------------------------------------------------------------------------
// The toker server
// ---------------------------------------------------------------------------

fn test_dir(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir =
        PathBuf::from("/tmp/opencode").join(format!("server-{name}-{}-{n}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

fn test_config(upstream: reqwest::Url, api_key_env: &str, api_key: Option<String>) -> Config {
    // The anthropic fields exist only so the Config literal compiles after
    // the anthropic unit grew it; no openai-path test touches an
    // anthropic route (the anthropic suite in server_anthropic.rs
    // exercises them).
    let anthropic_upstream: reqwest::Url =
        "https://api.anthropic.com".parse().expect("upstream url");
    Config {
        port: 0,
        db_path: test_dir("db").join("toker.db"),
        session_header_names: vec!["x-toker-session".to_owned(), "x-session-id".to_owned()],
        default_backend_openai_chat: "openrouter".to_owned(),
        openrouter: OpenRouterConfig {
            upstream,
            api_key_env: api_key_env.to_owned(),
            api_key,
        },
        default_backend_anthropic: "anthropic_sub".to_owned(),
        anthropic_sub: AnthropicSubConfig {
            upstream: anthropic_upstream.clone(),
        },
        anthropic_api: AnthropicApiConfig {
            upstream: anthropic_upstream,
            api_key_env: api_key_env.to_owned(),
            api_key: None,
        },
    }
}

async fn spawn_toker(config: Config) -> (SocketAddr, Arc<Store>) {
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    let server = Server::new(config, store.clone()).expect("build server");
    let app = server.router();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("toker binds");
    let addr = listener.local_addr().expect("toker addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("toker serves");
    });
    (addr, store)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("client")
}

fn toker_url(addr: SocketAddr, path: &str) -> reqwest::Url {
    format!("http://{addr}{path}").parse().expect("toker url")
}

/// Poll the ledger until it holds exactly `count` rows (rows land at
/// response completion, which races the client seeing the last byte).
async fn wait_for_rows(store: &Store, count: usize) -> Vec<RequestRow> {
    for _ in 0..200 {
        let rows = store.requests_since(0, 1000).expect("read rows");
        if rows.len() == count {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    store.requests_since(0, 1000).expect("read rows")
}

async fn post_chat(addr: SocketAddr, body: &[u8]) -> reqwest::Response {
    client()
        .post(toker_url(addr, "/v1/chat/completions"))
        .header("x-toker-session", "ses-test-1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .await
        .expect("chat request")
}

fn chat_body(model: &str, stream: bool) -> Vec<u8> {
    format!(
        r#"{{"model":"{model}","messages":[{{"role":"user","content":"Hi"}}],"stream":{stream}}}"#
    )
    .into_bytes()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_completions_pass_through_byte_identically_and_ledger() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let body = chat_body("z-ai/glm-5.3", true);
    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/event-stream")),
        "SSE content type passes through"
    );

    // Response passthrough is byte-identical to the fixture.
    let bytes = response.bytes().await.expect("sse bytes");
    let expected = fixture("01_simple_content.sse");
    assert_eq!(bytes, expected, "SSE stream passes through verbatim");

    // Request bytes arrive upstream byte-identical (Exact fidelity → the
    // original buffer is forwarded).
    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].path, "/v1/chat/completions");
    assert_eq!(captured[0].body.as_ref(), body.as_slice());
    assert_eq!(
        captured[0]
            .headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok()),
        Some("identity"),
        "identity is forced on the upstream request"
    );
    assert!(
        !captured[0].headers.contains_key("x-toker-session"),
        "the session header is toker's, never forwarded"
    );
    assert!(
        !captured[0].headers.contains_key(header::AUTHORIZATION),
        "no key configured → no auth injected, openrouter-shaped 401s verify the wiring"
    );

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.kind, None, "a real measurement has no kind");
    assert_eq!(row.frontend.as_deref(), Some("openai_chat"));
    assert_eq!(row.provider.as_deref(), Some("openrouter"));
    assert_eq!(row.route.as_deref(), Some("openai_chat:openrouter"));
    assert_eq!(row.session_id.as_deref(), Some("ses-test-1"));
    assert_eq!(row.model.as_deref(), Some("z-ai/glm-5.3"));
    assert_eq!(row.raw_model.as_deref(), Some("z-ai/glm-5.3"));
    assert_eq!(row.requested_model.as_deref(), Some("z-ai/glm-5.3"));
    assert_eq!(row.effective_model.as_deref(), Some("z-ai/glm-5.3"));
    assert_eq!(row.input, Some(128));
    assert_eq!(row.output, Some(16));
    assert_eq!(row.cache_read, None, "fixture 01 reports no cached_tokens");
    assert_eq!(row.reasoning, None);
    assert_eq!(row.cost_usd, Some(0.000192));
    assert_eq!(row.cost_kind, Some(CostKind::Billed));
    assert_eq!(
        row.usage_raw.as_deref(),
        Some(concat!(
            r#"{"prompt_tokens":128,"completion_tokens":16,"total_tokens":144,"#,
            r#""cost":0.000192,"cost_details":{"upstream":0.00016,"router":0.000032}}"#,
        )),
        "usage_raw is byte-verbatim from the stream"
    );
    assert_eq!(
        row.usage_presence,
        Some(serde_json::json!({
            "prompt_tokens": true, "completion_tokens": true,
            "cached_tokens": false, "reasoning_tokens": false, "cost": true,
        }))
    );
    assert_eq!(
        row.extra
            .as_ref()
            .and_then(|extra| extra.get("serving_provider")),
        Some(&serde_json::json!("z-ai")),
        "the serving provider is kept (ledger parity)"
    );
    assert_eq!(row.req_bytes, Some(body.len() as i64));
    assert_eq!(row.req_messages, Some(1));
    assert_eq!(row.req_tools, Some(0));
    assert!(row.duration_ms.is_some());
    assert_eq!(row.status, None);
    assert_eq!(row.drift_digest, None, "no drift for a canonical body");
}

#[tokio::test]
async fn non_streaming_completions_buffer_observe_and_ledger() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let body = chat_body("z-ai/glm-5.3", false);
    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body bytes");
    assert_eq!(
        bytes.as_ref(),
        NON_STREAM_BODY.as_bytes(),
        "non-SSE body passes through unchanged"
    );

    assert_eq!(mock.captured()[0].body.as_ref(), body.as_slice());

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.kind, None);
    assert_eq!(row.input, Some(48), "64 prompt - 16 cached");
    assert_eq!(row.output, Some(4), "8 completion - 4 reasoning");
    assert_eq!(row.cache_read, Some(16));
    assert_eq!(row.reasoning, Some(4));
    assert_eq!(row.cost_usd, Some(0.000128));
    assert_eq!(row.cost_kind, Some(CostKind::Billed));
    assert_eq!(row.model.as_deref(), Some("z-ai/glm-5.3"));
    assert_eq!(row.system_chars, Some(0));
    assert_eq!(row.system_messages, Some(0));
}

#[tokio::test]
async fn the_openrouter_prefix_routes_strips_and_rewrites_the_bytes() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let body = chat_body("openrouter/z-ai/glm-5.3", false);
    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);

    // A transformed request forwards the serialised (rewritten) form: the
    // model the upstream sees is the stripped one, in the IR's canonical
    // bytes.
    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    let mut expected = IrRequest::parse(&body).expect("parse");
    expected.openai_chat_mut().set_model("z-ai/glm-5.3");
    assert_eq!(
        captured[0].body.as_ref(),
        expected.serialise().as_slice(),
        "the routed request is the serialised rewrite"
    );
    assert_ne!(captured[0].body.as_ref(), body.as_slice());

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(
        row.requested_model.as_deref(),
        Some("openrouter/z-ai/glm-5.3")
    );
    assert_eq!(row.effective_model.as_deref(), Some("z-ai/glm-5.3"));
    assert_eq!(row.model.as_deref(), Some("z-ai/glm-5.3"));
}

#[tokio::test]
async fn auth_passes_through_when_present_and_injects_when_absent() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, _store) = spawn_toker(test_config(
        upstream,
        UNSET_KEY_ENV,
        Some("sk-literal-test".to_owned()),
    ))
    .await;

    // Absent client auth → the stored literal is injected.
    let body = chat_body("z-ai/glm-5.3", false);
    assert_eq!(post_chat(addr, &body).await.status(), StatusCode::OK);
    let captured = mock.captured();
    assert_eq!(
        captured[0]
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer sk-literal-test"),
        "no client auth → the resolved key is injected"
    );

    // Present client auth → verbatim pass-through, the stored key unused.
    let response = client()
        .post(toker_url(addr, "/v1/chat/completions"))
        .header(header::AUTHORIZATION, "Bearer client-own-token")
        .body(chat_body("z-ai/glm-5.3", false))
        .send()
        .await
        .expect("chat request");
    assert_eq!(response.status(), StatusCode::OK);
    let captured = mock.captured();
    assert_eq!(
        captured[1]
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer client-own-token"),
        "pass-through-when-present: the frontend's credential wins"
    );
}

#[tokio::test]
async fn the_env_key_source_wins_over_the_literal() {
    // A name only this test touches, so parallel tests cannot observe the
    // set_var (edition-2024 unsafe env mutation).
    unsafe { std::env::set_var("TOKER_TEST_KEY_ENV_PICK_4B", "sk-from-env") };
    let (mock, upstream) = spawn_mock().await;
    let (addr, _store) = spawn_toker(test_config(
        upstream,
        "TOKER_TEST_KEY_ENV_PICK_4B",
        Some("sk-literal-loses".to_owned()),
    ))
    .await;

    let body = chat_body("z-ai/glm-5.3", false);
    assert_eq!(post_chat(addr, &body).await.status(), StatusCode::OK);
    assert_eq!(
        mock.captured()[0]
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer sk-from-env"),
        "env beats the literal"
    );
}

#[tokio::test]
async fn non_2xx_forwards_the_body_and_records_an_unpriced_error_row() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let body = chat_body("err-401", false);
    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = response.bytes().await.expect("error bytes");
    assert_eq!(
        bytes.as_ref(),
        ERROR_BODY.as_bytes(),
        "the error body passes through unchanged"
    );
    assert_eq!(mock.captured()[0].body.as_ref(), body.as_slice());

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.kind, Some(RowKind::Error));
    assert_eq!(row.status, Some(401));
    assert_eq!(row.error_type.as_deref(), Some("invalid_request_error"));
    assert_eq!(row.retry_after_ms, Some(7_000), "retry-after: 7 → 7000 ms");
    assert_eq!(row.cost_usd, None, "error rows are never priced");
    assert_eq!(row.cost_kind, None);
    assert_eq!(row.usage_raw, None);
    assert_eq!(row.input, None);
    assert_eq!(row.requested_model.as_deref(), Some("err-401"));
    assert_eq!(row.effective_model.as_deref(), Some("err-401"));
    assert_eq!(row.session_id.as_deref(), Some("ses-test-1"));
}

#[tokio::test]
async fn fidelity_drift_forwards_the_original_bytes_and_is_recorded_visibly() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    // `\/` is legal JSON that serde_json's canonical form never emits: the
    // IR re-serialises to "/", so the monitor must report drift — and the
    // upstream still receives the ORIGINAL bytes (invariant 5: drift never
    // changes what is forwarded).
    let body = br#"{"model":"z-ai/glm-5.3","messages":[{"role":"user","content":"a\/b"}]}"#;
    let response = post_chat(addr, body).await;
    assert_eq!(response.status(), StatusCode::OK);

    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].body.as_ref(),
        body.as_slice(),
        "the original, non-canonical buffer is forwarded"
    );

    let rows = wait_for_rows(&store, 2).await;
    let mut drift_rows = rows
        .iter()
        .filter(|row| row.kind == Some(RowKind::FidelityDrift));
    let drift = drift_rows.next().expect("a fidelity-drift row is recorded");
    assert_eq!(drift.frontend.as_deref(), Some("openai_chat"));
    assert_eq!(drift.route.as_deref(), Some("openai_chat:openrouter"));
    let digest = drift.drift_digest.as_deref().expect("drift digest set");
    assert_eq!(digest.len(), 12, "the sha256/12 short digest");
    assert!(drift_rows.next().is_none(), "exactly one drift row");

    let measurement = rows
        .iter()
        .find(|row| row.kind.is_none())
        .expect("the measurement row is recorded too");
    assert_eq!(
        measurement.input,
        Some(48),
        "64 prompt - 16 cached, per the normalization"
    );
    assert_eq!(measurement.cache_read, Some(16));
}

#[tokio::test]
async fn models_passthrough_is_byte_identical_and_unledgered() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let response = client()
        .get(toker_url(addr, "/v1/models"))
        .send()
        .await
        .expect("models request");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("models bytes");
    assert_eq!(bytes.as_ref(), MODELS_BODY.as_bytes());
    assert_eq!(mock.captured()[0].path, "/v1/models");

    // Not a usage path: nothing recorded. Give any racing write a moment
    // to prove its absence.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(store.count_requests().expect("count"), 0);
}

#[tokio::test]
async fn non_json_bodies_forward_unchanged_with_no_row() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let response = post_chat(addr, b"not json at all").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        mock.captured()[0].body.as_ref(),
        b"not json at all",
        "unparseable bodies forward unchanged (invariant 6 spirit)"
    );

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        store.count_requests().expect("count"),
        0,
        "no row for an unparseable body"
    );
}

#[tokio::test]
async fn unexpected_compression_passes_through_untouched_and_unledgered() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let response = post_chat(addr, &chat_body("gzip-me", false)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok()),
        Some("gzip"),
        "the encoding header passes through with the bytes"
    );
    let bytes = response.bytes().await.expect("bytes");
    assert_eq!(
        bytes.as_ref(),
        b"\x1f\x8b-not-really-gzip",
        "bytes untouched"
    );

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        store.count_requests().expect("count"),
        0,
        "compressed responses are unledgered"
    );
}

#[tokio::test]
async fn client_hangup_aborts_the_upstream_and_records_no_row() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let mut response = client()
        .post(toker_url(addr, "/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(chat_body("hang", true))
        .send()
        .await
        .expect("chat request");
    assert_eq!(response.status(), StatusCode::OK);

    // Read one chunk, then hang up mid-stream.
    let chunk = response
        .chunk()
        .await
        .expect("first chunk")
        .expect("non-empty");
    assert!(!chunk.is_empty());
    drop(response);

    // The abort propagates to the upstream (its body stream is dropped).
    for _ in 0..100 {
        if mock.upstream_dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        mock.upstream_dropped.load(Ordering::SeqCst),
        "client hangup aborted the upstream request"
    );

    // A hung-up stream records no row (plan: Server core).
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(store.count_requests().expect("count"), 0);
}

#[tokio::test]
async fn control_status_is_gated_and_secret_free() {
    let (_mock, upstream) = spawn_mock().await;
    let secret = "sk-super-secret-never-printed";
    let (addr, _store) = spawn_toker(test_config(
        upstream.clone(),
        UNSET_KEY_ENV,
        Some(secret.to_owned()),
    ))
    .await;

    let url = toker_url(addr, "/_toker/status");

    // Ungated → 403, both without the header and with the wrong verb.
    let response = client()
        .get(url.clone())
        .send()
        .await
        .expect("status request");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = client()
        .get(url.clone())
        .header("x-toker-control", "wrong")
        .send()
        .await
        .expect("status request");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Gated → the config echo, sans secrets.
    let response = client()
        .get(url.clone())
        .header("x-toker-control", "status")
        .send()
        .await
        .expect("status request");
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.expect("status body");
    assert!(
        !text.contains(secret),
        "invariant 2: no key value ever leaves"
    );
    let value: Value = serde_json::from_str(&text).expect("status is JSON");
    assert_eq!(
        value["providers"]["openrouter"]["upstream"],
        upstream.as_str()
    );
    assert_eq!(
        value["providers"]["openrouter"]["api_key_env"],
        UNSET_KEY_ENV
    );
    assert_eq!(
        value["providers"]["openrouter"]["api_key_literal_set"],
        true
    );
    assert_eq!(value["providers"]["openrouter"]["api_key_env_set"], false);
    assert_eq!(value["requests"], 0);
    assert_eq!(value["last_request_ts_ms"], Value::Null);
    assert!(value["uptime_s"].is_u64());

    // The merge stub is gated the same way and answers 501.
    let response = client()
        .post(toker_url(addr, "/_toker/models/merge"))
        .send()
        .await
        .expect("merge request");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = client()
        .post(toker_url(addr, "/_toker/models/merge"))
        .header("x-toker-control", "models-merge")
        .send()
        .await
        .expect("merge request");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
}
