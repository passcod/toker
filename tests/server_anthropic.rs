//! End-to-end anthropic-proxy tests: a mock anthropic upstream (capturing
//! every request byte-for-byte) behind the real toker router, driven as a
//! client.
//!
//! Asserts the unit's contract: request bodies arrive upstream
//! byte-identical (echo capture) with the header discipline intact
//! (identity forced, credentials passed through or injected per backend,
//! claude's session header forwarded), responses pass through
//! byte-identically, rows carry the right buckets/betas/rate_limits and
//! the right cost kind per backend (plan_equivalent on the sub, estimated
//! on the api), error rows stay lean (no rate_limits, never priced), the
//! meters_state table feeds from every response on the meter-source
//! backend (not just accounted ones), fidelity drift is visible, and
//! unaccounted paths (count_tokens, batches, compression, non-JSON)
//! record nothing.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use bytes::Bytes;
use futures::Stream;
use serde_json::{Value, json};

use toker::catalog::{CostBuckets, price};
use toker::config::{AnthropicApiConfig, AnthropicSubConfig, Config, OpenRouterConfig};
use toker::ir::Request as IrRequest;
use toker::server::Server;
use toker::store::{CostKind, RequestRow, RowKind, Store};

/// An env name no test ever sets, so nothing resolves and nothing injects.
const UNSET_KEY_ENV: &str = "TOKER_TEST_KEY_UNSET_ANTH_5C";

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
        .join("tests/fixtures/anthropic_sse")
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

/// A realistic full `anthropic-ratelimit-*` set: one header per named
/// meter field, the known-but-unlifted `unified-reset`, and an unknown
/// experiment header that must survive in `other`. `util5h` varies per
/// serving path so tests can prove the meters feed moved.
fn metered(response: &mut Response, util5h: &'static str) {
    for (name, value) in [
        ("anthropic-ratelimit-unified-5h-utilization", util5h),
        ("anthropic-ratelimit-unified-5h-reset", "1769500800"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.2214"),
        ("anthropic-ratelimit-unified-7d-reset", "1769846400"),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("anthropic-ratelimit-unified-overage-utilization", "0.0004"),
        ("anthropic-ratelimit-unified-overage-reset", "1769500800"),
        ("anthropic-ratelimit-unified-overage-status", "allowed"),
        ("anthropic-ratelimit-unified-status", "allowed"),
        ("anthropic-ratelimit-unified-representative-claim", "5h"),
        ("anthropic-ratelimit-unified-overage-in-use", "false"),
        ("anthropic-ratelimit-unified-fallback-percentage", "12.5"),
        ("anthropic-ratelimit-unified-reset", "1769500800"),
        ("anthropic-ratelimit-experiment-thing", "42"),
    ] {
        response.headers_mut().insert(
            name.parse::<HeaderName>().expect("header name"),
            HeaderValue::from_static(value),
        );
    }
}

/// The stable shape the full set parses to (the provider unit pins the
/// same value; here it proves what actually landed in the row/table).
fn expected_rate_limits(util5h: &str) -> Value {
    json!({
        "util5h": serde_json::from_str::<serde_json::Number>(util5h).expect("number"),
        "reset5h": 1769500800,
        "util7d": 0.2214,
        "reset7d": 1769846400,
        "utilOverage": 0.0004,
        "resetOverage": 1769500800,
        "status": "allowed",
        "status5h": "allowed",
        "status7d": "allowed",
        "statusOverage": "allowed",
        "claim": "5h",
        "overageInUse": false,
        "fallbackPct": 12.5,
        "other": {"anthropic-ratelimit-experiment-thing": "42"},
    })
}

/// A non-streaming Messages response echoing the request's model, with
/// every usage field the observer models.
fn non_stream_body(model: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": "msg_test_ns",
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{"type": "text", "text": "Done."}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 9,
            "cache_read_input_tokens": 100,
            "cache_creation_input_tokens": 400,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 100,
                "ephemeral_1h_input_tokens": 300,
            },
            "output_tokens": 40,
            "server_tool_use": {"web_search_requests": 2, "code_execution_requests": 1},
            "speed": "standard",
            "inference_geo": "us",
        },
    }))
    .expect("serialise non-stream body")
}

const ERROR_BODY: &str =
    r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;

async fn read_and_capture(mock: &MockState, request: Request) -> (String, HeaderMap, Value, bool) {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .expect("mock reads body");
    let path = parts.uri.path().to_owned();
    mock.capture(&path, &parts.headers, body.clone());
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let stream = json.get("stream") == Some(&Value::Bool(true));
    (path, parts.headers, json, stream)
}

async fn mock_messages(State(mock): State<MockState>, request: Request) -> Response {
    let (_path, _headers, json, stream) = read_and_capture(&mock, request).await;
    let model = json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    match model.as_str() {
        "err-401" => {
            let mut response = raw_response(
                StatusCode::UNAUTHORIZED,
                "application/json",
                Bytes::from_static(ERROR_BODY.as_bytes()),
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
            metered(&mut response, "0.77");
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
            metered(&mut response, "0.4127");
            response
        }
        "hang" if stream => {
            // First event, then nothing — the client hangs up mid-stream
            // and the abort must propagate (the Drop flag proves it).
            let body = Body::from_stream(Hanging {
                mock: mock.clone(),
                first: Some(Bytes::from_static(
                    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":2}}}\n\n",
                )),
            });
            let mut response = Response::new(body);
            *response.status_mut() = StatusCode::OK;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            metered(&mut response, "0.4127");
            response
        }
        _ if stream => {
            // The CRLF-dialect fixture: fast mode, us geo, a 1h-tier write
            // with a reconciling split, thinking zero — the richest
            // capture shape.
            let mut response = raw_response(
                StatusCode::OK,
                "text/event-stream",
                fixture("03_1h_write_crlf.sse"),
            );
            metered(&mut response, "0.4127");
            response
        }
        _ => {
            let mut response = raw_response(
                StatusCode::OK,
                "application/json",
                Bytes::from(non_stream_body(&model)),
            );
            metered(&mut response, "0.4127");
            response
        }
    }
}

async fn mock_count_tokens(State(mock): State<MockState>, request: Request) -> Response {
    let (_path, _headers, _json, _stream) = read_and_capture(&mock, request).await;
    let mut response = raw_response(
        StatusCode::OK,
        "application/json",
        Bytes::from_static(br#"{"input_tokens":42}"#),
    );
    metered(&mut response, "0.99");
    response
}

/// Every batch-management path (create, list, retrieve, results, cancel)
/// answers with a batch object that carries no usage.
async fn mock_batches(State(mock): State<MockState>, request: Request) -> Response {
    let (_path, _headers, _json, _stream) = read_and_capture(&mock, request).await;
    let mut response = raw_response(
        StatusCode::OK,
        "application/json",
        Bytes::from_static(
            br#"{"id":"batch_123","object":"message_batch","status":"in_progress"}"#,
        ),
    );
    metered(&mut response, "0.55");
    response
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

async fn spawn_mock() -> (MockState, reqwest::Url) {
    let state = MockState::default();
    let app = Router::new()
        .route("/v1/messages", post(mock_messages))
        .route("/v1/messages/count_tokens", post(mock_count_tokens))
        .route("/v1/messages/batches", post(mock_batches).get(mock_batches))
        .route("/v1/messages/batches/{id}", get(mock_batches))
        .route("/v1/messages/batches/{id}/results", get(mock_batches))
        .route("/v1/messages/batches/{id}/cancel", post(mock_batches))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("mock binds");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock serves");
    });
    let upstream: reqwest::Url = format!("http://{addr}").parse().expect("upstream url");
    (state, upstream)
}

// ---------------------------------------------------------------------------
// The toker server
// ---------------------------------------------------------------------------

fn test_dir(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from("/tmp/opencode").join(format!(
        "server-anthropic-{name}-{}-{n}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

/// The anthropic test config: both anthropic backends point at the mock;
/// the openai fields exist only to compile and are never routed to.
fn test_config(
    anthropic_upstream: reqwest::Url,
    api_key: Option<String>,
    default_backend: &str,
) -> Config {
    let unused_openrouter: reqwest::Url = "http://127.0.0.1:9/v1".parse().expect("upstream url");
    Config {
        port: 0,
        db_path: test_dir("db").join("toker.db"),
        session_header_names: vec![
            "x-toker-session".to_owned(),
            "x-claude-code-session-id".to_owned(),
            "x-session-id".to_owned(),
        ],
        default_backend_openai_chat: "openrouter".to_owned(),
        openrouter: OpenRouterConfig {
            upstream: unused_openrouter,
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key: None,
        },
        default_backend_anthropic: default_backend.to_owned(),
        anthropic_sub: AnthropicSubConfig {
            upstream: anthropic_upstream.clone(),
        },
        anthropic_api: AnthropicApiConfig {
            upstream: anthropic_upstream,
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key,
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

/// Assert no row ever lands (give any racing write a moment to prove its
/// absence).
async fn assert_no_rows(store: &Store) {
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(store.count_requests().expect("count"), 0);
}

async fn post_messages(
    addr: SocketAddr,
    path: &str,
    headers: &[(&'static str, &'static str)],
    body: &[u8],
) -> reqwest::Response {
    let mut request = client()
        .post(toker_url(addr, path))
        .header("x-claude-code-session-id", "ccses-42")
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request
        .body(body.to_vec())
        .send()
        .await
        .expect("messages request")
}

fn messages_body(model: &str, stream: bool) -> Vec<u8> {
    format!(
        r#"{{"model":"{model}","messages":[{{"role":"user","content":"Hi"}}],"stream":{stream}}}"#
    )
    .into_bytes()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_messages_pass_through_byte_identically_and_ledger() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    let body = messages_body("claude-opus-5", true);
    let response = post_messages(
        addr,
        "/v1/messages",
        &[
            ("authorization", "Bearer claude-oauth-token"),
            (
                "anthropic-beta",
                "context-1m-2025-08-07,fast-mode-2025-09-preview",
            ),
        ],
        &body,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/event-stream")),
        "SSE content type passes through"
    );

    // Response passthrough is byte-identical to the fixture (CRLF dialect).
    let bytes = response.bytes().await.expect("sse bytes");
    let expected = fixture("03_1h_write_crlf.sse");
    assert_eq!(bytes, expected, "SSE stream passes through verbatim");

    // Request bytes arrive upstream byte-identical, with the header
    // discipline intact.
    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].path, "/v1/messages");
    assert_eq!(captured[0].body.as_ref(), body.as_slice());
    assert_eq!(
        captured[0]
            .headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok()),
        Some("identity"),
        "identity is forced on the upstream request"
    );
    assert_eq!(
        captured[0]
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer claude-oauth-token"),
        "claude's OAuth bearer passes through verbatim (pass-through-when-present)"
    );
    assert_eq!(
        captured[0]
            .headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok()),
        Some("2023-06-01"),
        "anthropic-version passes through"
    );
    assert_eq!(
        captured[0]
            .headers
            .get("anthropic-beta")
            .and_then(|v| v.to_str().ok()),
        Some("context-1m-2025-08-07,fast-mode-2025-09-preview"),
        "anthropic-beta passes through — the upstream needs the flags"
    );
    assert_eq!(
        captured[0]
            .headers
            .get("x-claude-code-session-id")
            .and_then(|v| v.to_str().ok()),
        Some("ccses-42"),
        "ctp parity: claude's session header is forwarded, not swallowed by the proxy"
    );
    assert!(
        !captured[0].headers.contains_key("x-api-key"),
        "the sub injects nothing — no stored token yet"
    );

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.kind, None, "a real measurement has no kind");
    assert_eq!(row.frontend.as_deref(), Some("anthropic"));
    assert_eq!(row.provider.as_deref(), Some("anthropic_sub"));
    assert_eq!(row.route.as_deref(), Some("anthropic:anthropic_sub"));
    assert_eq!(row.session_id.as_deref(), Some("ccses-42"));
    assert_eq!(row.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(row.raw_model.as_deref(), Some("claude-opus-5"));
    assert_eq!(row.requested_model.as_deref(), Some("claude-opus-5"));
    assert_eq!(row.effective_model.as_deref(), Some("claude-opus-5"));
    // Fixture 03's folded buckets.
    assert_eq!(row.input, Some(2));
    assert_eq!(row.cache_read, Some(0));
    assert_eq!(row.cache_write_total, Some(82_420));
    assert_eq!(row.cache_write_5m, Some(0));
    assert_eq!(row.cache_write_1h, Some(82_420));
    assert_eq!(row.ttl_split_known, Some(true));
    assert_eq!(row.output, Some(13));
    assert_eq!(row.reasoning, Some(0));
    assert_eq!(row.iterations, Some(1));
    assert_eq!(
        row.usage_presence,
        Some(json!({
            "input": true, "cache_read": true, "cache_write_total": true,
            "cache_write_5m": true, "cache_write_1h": true, "output": true,
            "reasoning": true, "web_searches": false, "code_execs": false,
        }))
    );
    assert_eq!(row.fast, Some(true), "fixture 03 reports speed: fast");
    assert_eq!(row.geo.as_deref(), Some("us"));
    assert_eq!(
        row.betas.as_deref(),
        Some(r#"["context-1m-2025-08-07","fast-mode-2025-09-preview"]"#),
        "the request's anthropic-beta flags, as a JSON array"
    );
    // The response's own meter snapshot, in the stable ctp shape.
    assert_eq!(row.rate_limits, Some(expected_rate_limits("0.4127")));
    // Plan-equivalent: list-price "what the plan is worth", never billed.
    assert_eq!(row.cost_kind, Some(CostKind::PlanEquivalent));
    let priced = price("claude-opus-5", true, Some("us")).expect("fast+us priced");
    let expected_cost = priced.cost_usd(&CostBuckets {
        input: 2,
        cache_read: 0,
        cache_write_5m: 0,
        cache_write_1h: 82_420,
        output: 13,
        web_searches: 0,
    });
    assert_eq!(row.cost_usd, Some(expected_cost));
    assert!((expected_cost - 1.813977).abs() < 1e-9, "{expected_cost}");
    assert_eq!(row.req_bytes, Some(body.len() as i64));
    assert_eq!(row.req_messages, Some(1));
    assert_eq!(row.req_tools, Some(0));
    assert_eq!(row.system_chars, Some(0));
    assert!(row.system_hash.is_some());
    assert_eq!(row.status, None);
    assert_eq!(row.drift_digest, None);
    assert!(row.duration_ms.is_some());

    // The meters feed from this response: meters_state holds the snapshot.
    let meters = store
        .load_meters()
        .expect("meters")
        .expect("a snapshot exists");
    assert_eq!(meters.snapshot, expected_rate_limits("0.4127"));
    assert!(meters.updated_ms > 0);
}

#[tokio::test]
async fn non_streaming_messages_ledger_with_estimated_cost_for_the_api_backend() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(
        upstream,
        Some("sk-ant-literal-test".to_owned()),
        "anthropic_sub",
    ))
    .await;

    let body = messages_body("anthropic_api/claude-sonnet-5", false);
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body bytes");
    assert_eq!(
        bytes.as_ref(),
        non_stream_body("claude-sonnet-5").as_slice(),
        "non-SSE body passes through unchanged"
    );

    // A routed request forwards the serialised rewrite.
    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    let mut expected = IrRequest::parse(&body).expect("parse");
    expected.anthropic_mut().set_model("claude-sonnet-5");
    assert_eq!(
        captured[0].body.as_ref(),
        expected.serialise().as_slice(),
        "the routed request is the serialised rewrite"
    );
    assert_ne!(captured[0].body.as_ref(), body.as_slice());
    assert_eq!(
        captured[0]
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("sk-ant-literal-test"),
        "no client credential → the stored API key injects as x-api-key"
    );

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.provider.as_deref(), Some("anthropic_api"));
    assert_eq!(row.route.as_deref(), Some("anthropic:anthropic_api"));
    assert_eq!(
        row.requested_model.as_deref(),
        Some("anthropic_api/claude-sonnet-5")
    );
    assert_eq!(row.effective_model.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(row.model.as_deref(), Some("claude-sonnet-5"));
    // Estimated: the API bills this catalog price.
    assert_eq!(row.cost_kind, Some(CostKind::Estimated));
    let priced = price("claude-sonnet-5", false, Some("us")).expect("priced");
    let expected_cost = priced.cost_usd(&CostBuckets {
        input: 9,
        cache_read: 100,
        cache_write_5m: 100,
        cache_write_1h: 300,
        output: 40,
        web_searches: 2,
    });
    assert_eq!(row.cost_usd, Some(expected_cost));
    assert!((expected_cost - 0.0220768).abs() < 1e-9, "{expected_cost}");
    assert_eq!(row.fast, Some(false), "speed: standard, not fast");
    assert_eq!(row.geo.as_deref(), Some("us"));
    assert_eq!(row.web_searches, Some(2));
    assert_eq!(row.code_execs, Some(1));
    assert_eq!(row.ttl_split_known, Some(true));
    assert_eq!(row.betas, None, "no anthropic-beta header on this request");
    assert_eq!(row.rate_limits, Some(expected_rate_limits("0.4127")));

    // The api is not a meter source: meters_state stays untouched.
    assert_eq!(
        store.load_meters().expect("meters"),
        None,
        "the API's RPM headers are not quota meters and never overwrite the gate's snapshot"
    );
}

#[tokio::test]
async fn bare_models_go_to_the_configured_default_backend() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(
        upstream,
        Some("sk-ant-literal-test".to_owned()),
        "anthropic_api",
    ))
    .await;

    let body = messages_body("claude-sonnet-5", false);
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK);

    // Untransformed: the original buffer is forwarded byte-identical.
    let captured = mock.captured();
    assert_eq!(captured[0].body.as_ref(), body.as_slice());
    assert_eq!(
        captured[0]
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("sk-ant-literal-test")
    );

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.provider.as_deref(), Some("anthropic_api"));
    assert_eq!(row.requested_model.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(row.effective_model.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(row.cost_kind, Some(CostKind::Estimated));
}

#[tokio::test]
async fn the_generic_anthropic_prefix_routes_to_the_default_and_strips() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    let body = messages_body("anthropic/claude-opus-5", false);
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK);

    let captured = mock.captured();
    let mut expected = IrRequest::parse(&body).expect("parse");
    expected.anthropic_mut().set_model("claude-opus-5");
    assert_eq!(captured[0].body.as_ref(), expected.serialise().as_slice());

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.provider.as_deref(), Some("anthropic_sub"));
    assert_eq!(
        row.requested_model.as_deref(),
        Some("anthropic/claude-opus-5")
    );
    assert_eq!(row.effective_model.as_deref(), Some("claude-opus-5"));
    assert_eq!(row.cost_kind, Some(CostKind::PlanEquivalent));
}

#[tokio::test]
async fn auth_passes_through_when_present_and_injects_when_absent() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, _store) = spawn_toker(test_config(
        upstream,
        Some("sk-ant-literal-test".to_owned()),
        "anthropic_api",
    ))
    .await;

    // Absent client auth → the stored literal is injected.
    let body = messages_body("claude-sonnet-5", false);
    assert_eq!(
        post_messages(addr, "/v1/messages", &[], &body)
            .await
            .status(),
        StatusCode::OK
    );
    let captured = mock.captured();
    assert_eq!(
        captured[0]
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("sk-ant-literal-test"),
        "no client credential → the resolved key is injected"
    );

    // Present bearer → verbatim pass-through, the stored key unused.
    assert_eq!(
        post_messages(
            addr,
            "/v1/messages",
            &[("authorization", "Bearer client-own-token")],
            &body,
        )
        .await
        .status(),
        StatusCode::OK
    );
    let captured = mock.captured();
    assert_eq!(
        captured[1]
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer client-own-token"),
        "pass-through-when-present: the frontend's credential wins"
    );
    assert!(
        !captured[1].headers.contains_key("x-api-key"),
        "a request that brings a bearer gets no injection"
    );

    // Present x-api-key → kept verbatim, never replaced.
    assert_eq!(
        post_messages(
            addr,
            "/v1/messages",
            &[("x-api-key", "client-own-ak")],
            &body
        )
        .await
        .status(),
        StatusCode::OK
    );
    let captured = mock.captured();
    assert_eq!(
        captured[2]
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("client-own-ak"),
        "a request that brings its own x-api-key keeps it"
    );

    // The sub never injects: no stored token exists yet.
    let (mock, upstream) = spawn_mock().await;
    let (addr, _store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;
    assert_eq!(
        post_messages(addr, "/v1/messages", &[], &body)
            .await
            .status(),
        StatusCode::OK
    );
    let captured = mock.captured();
    assert!(
        !captured[0].headers.contains_key("x-api-key")
            && !captured[0].headers.contains_key(header::AUTHORIZATION),
        "the sub injects nothing when the request carries no bearer — the upstream 401s visibly"
    );
}

#[tokio::test]
async fn non_2xx_forwards_the_body_and_records_a_lean_error_row_that_still_feeds_the_meters() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    let body = messages_body("err-401", false);
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
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
    assert_eq!(row.frontend.as_deref(), Some("anthropic"));
    assert_eq!(row.provider.as_deref(), Some("anthropic_sub"));
    assert_eq!(row.status, Some(401));
    assert_eq!(row.error_type.as_deref(), Some("authentication_error"));
    assert_eq!(
        row.extra
            .as_ref()
            .and_then(|extra| extra.get("error_message"))
            .and_then(Value::as_str),
        Some("invalid x-api-key"),
        "the schema has no error_message column; the pair's message half rides in extra"
    );
    assert_eq!(
        row.retry_after_ms,
        Some(30_000),
        "retry-after: 30 → 30000 ms"
    );
    assert_eq!(row.requested_model.as_deref(), Some("err-401"));
    assert_eq!(row.effective_model.as_deref(), Some("err-401"));
    assert_eq!(row.session_id.as_deref(), Some("ccses-42"));
    // Lean, like the openai error rows: never priced, no usage, no
    // rate_limits (the deliberate divergence from ctp, documented in the
    // record module).
    assert_eq!(row.cost_usd, None);
    assert_eq!(row.cost_kind, None);
    assert_eq!(row.rate_limits, None);
    assert_eq!(row.usage_presence, None);
    assert_eq!(row.input, None);
    assert_eq!(row.usage_raw, None);

    // …but the meters_state table took the response's snapshot anyway
    // (ctp rule: feed from every response, not just accounted ones).
    let meters = store
        .load_meters()
        .expect("meters")
        .expect("fed from the 401");
    assert_eq!(meters.snapshot, expected_rate_limits("0.77"));
}

#[tokio::test]
async fn count_tokens_forwards_unledgered_but_feeds_the_meters() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    let body = messages_body("claude-opus-5", false);
    let response = post_messages(addr, "/v1/messages/count_tokens", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("count_tokens bytes");
    assert_eq!(
        bytes.as_ref(),
        br#"{"input_tokens":42}"#,
        "byte passthrough"
    );
    let captured = mock.captured();
    assert_eq!(captured[0].path, "/v1/messages/count_tokens");
    assert_eq!(captured[0].body.as_ref(), body.as_slice());

    // count_tokens responses carry no usage: no row.
    assert_no_rows(&store).await;
    // …but the meters moved — from the unaccounted response.
    let meters = store
        .load_meters()
        .expect("meters")
        .expect("the unaccounted count_tokens response fed the meters");
    assert_eq!(meters.snapshot, expected_rate_limits("0.99"));
}

#[tokio::test]
async fn batch_paths_forward_transparently_and_feeds_the_meters() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    // Create: POST /v1/messages/batches — the same pipeline, but the
    // response carries no usage, so no row.
    let create = br#"{"requests":[{"params":{"model":"claude-opus-5","messages":[]}}]}"#;
    let response = post_messages(addr, "/v1/messages/batches", &[], create).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("create bytes");
    assert_eq!(
        bytes.as_ref(),
        br#"{"id":"batch_123","object":"message_batch","status":"in_progress"}"#,
        "create response passes through"
    );
    assert_eq!(
        mock.captured()[0].body.as_ref(),
        create.as_slice(),
        "the batches body (params nested, no top-level model) forwards byte-identical"
    );

    // Results GET: transparent forwarding.
    let response = client()
        .get(toker_url(addr, "/v1/messages/batches/batch_123/results"))
        .header(header::AUTHORIZATION, "Bearer claude-oauth-token")
        .send()
        .await
        .expect("results request");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("results bytes");
    assert_eq!(
        bytes.as_ref(),
        br#"{"id":"batch_123","object":"message_batch","status":"in_progress"}"#
    );
    let captured = mock.captured();
    assert_eq!(captured[1].path, "/v1/messages/batches/batch_123/results");
    assert_eq!(
        captured[1]
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer claude-oauth-token"),
        "auth passes through on the transparent path too"
    );

    // Cancel POST: transparent forwarding.
    let response = client()
        .post(toker_url(addr, "/v1/messages/batches/batch_123/cancel"))
        .send()
        .await
        .expect("cancel request");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        mock.captured()[2].path,
        "/v1/messages/batches/batch_123/cancel"
    );

    // Nothing recorded on any batch path.
    assert_no_rows(&store).await;
    // The meters fed from the background poll — ctp's "not just accounted
    // ones" rule.
    let meters = store
        .load_meters()
        .expect("meters")
        .expect("the background batch poll fed the meters");
    assert_eq!(meters.snapshot, expected_rate_limits("0.55"));
}

#[tokio::test]
async fn fidelity_drift_forwards_the_original_bytes_and_is_recorded_visibly() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    // `\/` is legal JSON that serde_json's canonical form never emits: the
    // IR re-serialises to "/", so the monitor must report drift — and the
    // upstream still receives the ORIGINAL bytes (invariant 5: drift never
    // changes what is forwarded).
    let body = br#"{"model":"claude-opus-5","messages":[{"role":"user","content":"a\/b"}]}"#;
    let response = post_messages(addr, "/v1/messages", &[], body).await;
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
    assert_eq!(drift.frontend.as_deref(), Some("anthropic"));
    assert_eq!(drift.route.as_deref(), Some("anthropic:anthropic_sub"));
    let digest = drift.drift_digest.as_deref().expect("drift digest set");
    assert_eq!(digest.len(), 12, "the sha256/12 short digest");
    assert!(drift_rows.next().is_none(), "exactly one drift row");

    let measurement = rows
        .iter()
        .find(|row| row.kind.is_none())
        .expect("the measurement row is recorded too");
    assert_eq!(measurement.input, Some(9));
    assert_eq!(measurement.cache_read, Some(100));
}

#[tokio::test]
async fn unknown_models_price_to_null_with_no_cost_kind() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    let response = post_messages(
        addr,
        "/v1/messages",
        &[],
        &messages_body("claude-mystery-9", false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(
        row.model.as_deref(),
        Some("claude-mystery-9"),
        "tokens counted…"
    );
    assert_eq!(row.input, Some(9));
    assert_eq!(
        row.cost_usd, None,
        "…cost left null for an unknown model — never a guess (ctp's one-time warning fired)"
    );
    assert_eq!(row.cost_kind, None);
}

#[tokio::test]
async fn row_model_is_normalised_and_raw_model_is_verbatim() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    let response = post_messages(
        addr,
        "/v1/messages",
        &[],
        &messages_body("claude-haiku-4-5-20251001", false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    // ctp: `model` is the normalised identity, `raw_model` the wire form.
    assert_eq!(row.model.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(row.raw_model.as_deref(), Some("claude-haiku-4-5-20251001"));
    // …and the snapshot suffix folded to a priced identity.
    assert_eq!(row.cost_kind, Some(CostKind::PlanEquivalent));
    let priced = price("claude-haiku-4-5", false, Some("us")).expect("priced");
    let expected_cost = priced.cost_usd(&CostBuckets {
        input: 9,
        cache_read: 100,
        cache_write_5m: 100,
        cache_write_1h: 300,
        output: 40,
        web_searches: 2,
    });
    assert_eq!(row.cost_usd, Some(expected_cost));
}

#[tokio::test]
async fn non_json_bodies_forward_unchanged_with_no_row_but_the_meters_feed() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    let response = post_messages(addr, "/v1/messages", &[], b"not json at all").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        mock.captured()[0].body.as_ref(),
        b"not json at all",
        "unparseable bodies forward unchanged (invariant 6 spirit)"
    );

    assert_no_rows(&store).await;
    let meters = store
        .load_meters()
        .expect("meters")
        .expect("even the unparseable request's response fed the meters");
    assert_eq!(meters.snapshot, expected_rate_limits("0.4127"));
}

#[tokio::test]
async fn compressed_responses_pass_through_untouched_and_unledgered() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    let response = post_messages(addr, "/v1/messages", &[], &messages_body("gzip-me", false)).await;
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

    assert_no_rows(&store).await;
}

#[tokio::test]
async fn client_hangup_aborts_the_upstream_and_records_no_row() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    let mut response = post_messages(addr, "/v1/messages", &[], &messages_body("hang", true)).await;
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
