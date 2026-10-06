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
//! on the api), error rows keep their meters but are never priced, the
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
use toker::config::{
    AnthropicApiConfig, AnthropicSubConfig, CodexSubConfig, Config, OpenRouterConfig,
};
use toker::ir::{PLAN_SENTINEL, Release, Request as IrRequest, SENTINEL};
use toker::middleware::notice::NoticeStyle;
use toker::middleware::quota::{Blocking, GateDecision, Meter, Meters, Rendering, decide};
use toker::server::Server;
use toker::store::{Allowance, CostKind, MetersSnapshot, RequestRow, RowKind, Store};

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
    method: String,
    path: String,
    query: Option<String>,
    headers: HeaderMap,
    body: Bytes,
}

impl MockState {
    fn capture(&self, parts: &axum::http::request::Parts, body: Bytes) {
        self.requests.lock().unwrap().push(CapturedRequest {
            method: parts.method.to_string(),
            path: parts.uri.path().to_owned(),
            query: parts.uri.query().map(str::to_owned),
            headers: parts.headers.clone(),
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
    mock.capture(&parts, body.clone());
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
                first: Some(Bytes::from_static(MESSAGE_START)),
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
        "drop-mid" if stream => {
            // First event, then the connection dies: a reset mid-turn.
            let body = Body::from_stream(fail_after(MESSAGE_START, "connection reset mid-turn"));
            let mut response = Response::new(body);
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            metered(&mut response, "0.4127");
            response
        }
        "drop-mid" | "drop-mid-401" => {
            // Half a JSON body, then the connection dies: what arrived
            // must not be forwarded as if it were the whole body.
            let body = Body::from_stream(fail_after(
                br#"{"type":"message","usage":{"#,
                "connection reset mid-body",
            ));
            let mut response = Response::new(body);
            if model == "drop-mid-401" {
                *response.status_mut() = StatusCode::UNAUTHORIZED;
            }
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            metered(&mut response, "0.4127");
            response
        }
        // OpenRouter's Anthropic endpoint, in the shapes measured on
        // 2026-10-06: the billed cost on the final usage (the delta, or
        // the plain body's), the serving provider on the message.
        "z-ai/glm-5.3-flash" if stream => raw_response(
            StatusCode::OK,
            "text/event-stream",
            Bytes::from_static(
                b"event: message_start\n\
                  data: {\"type\":\"message_start\",\"message\":{\"model\":\"z-ai/glm-5.3-flash\",\
                  \"usage\":{\"input_tokens\":0,\"output_tokens\":0},\"provider\":\"Friendli\"}}\n\n\
                  event: message_delta\n\
                  data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
                  \"usage\":{\"input_tokens\":18,\"output_tokens\":34,\"cost\":0.0000197}}\n\n\
                  event: message_stop\n\
                  data: {\"type\":\"message_stop\"}\n\n",
            ),
        ),
        "z-ai/glm-5.3-flash" => raw_response(
            StatusCode::OK,
            "application/json",
            Bytes::from_static(
                br#"{"type":"message","model":"z-ai/glm-5.3-flash","stop_reason":"end_turn",
                "usage":{"input_tokens":18,"output_tokens":32,"cost":1.87e-05},"provider":"Friendli"}"#,
            ),
        ),
        "stall-headers" => {
            // The upstream accepts the request and then says nothing at
            // all — not even the response headers.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            raw_response(StatusCode::OK, "application/json", Bytes::new())
        }
        "slow" if stream => {
            // A healthy stream that takes longer in total than the idle
            // timeout the slow test sets, but never pauses that long
            // between chunks: the timeout must leave it alone.
            let fixture = fixture("03_1h_write_crlf.sse");
            let pieces: Vec<Bytes> = fixture
                .chunks(fixture.len().div_ceil(8))
                .map(Bytes::copy_from_slice)
                .collect();
            let body = Body::from_stream(futures::stream::unfold(
                pieces.into_iter(),
                |mut pieces| async move {
                    let piece = pieces.next()?;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    Some((Ok::<_, std::io::Error>(piece), pieces))
                },
            ));
            let mut response = Response::new(body);
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            metered(&mut response, "0.4127");
            response
        }
        // A 5-minute-tier-only write (fixture 04): the lane-stickiness
        // probe — a warm follow-up whose writes landed on the short tier
        // must not shorten what the lane already holds.
        "5m-tier" if stream => {
            let mut response = raw_response(
                StatusCode::OK,
                "text/event-stream",
                fixture("04_5m_only_write.sse"),
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

/// A body that delivers `first`, then fails. The pause between them lets
/// the mock's hyper flush the headers and the first bytes; an error in
/// the same poll would abort the response before anything went out,
/// which is a different failure (no response at all).
fn fail_after(
    first: &'static [u8],
    error: &'static str,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    futures::stream::unfold(0, move |step| async move {
        match step {
            0 => Some((Ok(Bytes::from_static(first)), 1)),
            1 => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Some((Err(std::io::Error::other(error)), 2))
            }
            _ => None,
        }
    })
}

/// The opening event of a turn, alone: what a stream that fails or
/// stalls mid-turn has already delivered.
const MESSAGE_START: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":2}}}\n\n";

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
    let (_path, _headers, json, _stream) = read_and_capture(&mock, request).await;
    // A batch carrying the marker header model is rejected, so a test can
    // see the error row a batch writes (a successful one writes none).
    let rejected = json
        .get("requests")
        .and_then(Value::as_array)
        .is_some_and(|requests| {
            requests.iter().any(|request| {
                request.pointer("/params/model").and_then(Value::as_str) == Some("err-401")
            })
        });
    if rejected {
        return raw_response(
            StatusCode::UNAUTHORIZED,
            "application/json",
            Bytes::from_static(ERROR_BODY.as_bytes()),
        );
    }
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

/// Any path the mock's own route table does not name: a distinctive
/// status and body, metered, so a test can see the pass-through carried
/// the upstream's answer back unchanged.
async fn mock_unmatched(State(mock): State<MockState>, request: Request) -> Response {
    let (_path, _headers, _json, _stream) = read_and_capture(&mock, request).await;
    let mut response = raw_response(
        StatusCode::IM_A_TEAPOT,
        "application/octet-stream",
        Bytes::from_static(b"upstream-unmatched-body"),
    );
    metered(&mut response, "0.33");
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
        .fallback(mock_unmatched)
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
        ping_header_name: "x-toker-ping".to_owned(),
        default_backend_openai_chat: Some("openrouter".to_owned()),
        openrouter: Some(OpenRouterConfig {
            upstream: unused_openrouter,
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key_keyring: false,
            api_key: None,
            picker: None,
        }),
        default_backend_anthropic: Some(default_backend.to_owned()),
        anthropic_sub: Some(AnthropicSubConfig {
            model_map: None,
            upstream: anthropic_upstream.clone(),
        }),
        anthropic_api: Some(AnthropicApiConfig {
            model_map: None,
            upstream: anthropic_upstream,
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key_keyring: false,
            api_key,
        }),
        // The codex backend's config: never routed to in these suites
        // (the responses frontend lands later), pointed at an upstream
        // that never answers and an auth path that never exists — no
        // test may touch a real login.
        codex_sub: Some(CodexSubConfig {
            model_map: None,
            client_version: None,
            version_probe: false,
            upstream: "http://127.0.0.1:9/backend-api/codex"
                .parse()
                .expect("codex upstream url"),
            originator: "codex_cli_rs".to_owned(),
            auth_path: test_dir("codex-absent").join("auth.json"),
            refresh_url: "https://auth.openai.com/oauth/token"
                .parse()
                .expect("codex refresh url"),
        }),
        gates: toker::config::GatesConfig::default(),
        notices: toker::config::NoticesConfig::default(),
        // The sleep lock stays off in tests: the real spawner would take
        // a REAL idle-sleep lock on the host running the suite. The awake
        // suite (server_awake.rs) injects a fake spawner and turns it on.
        awake: false,
        transcript_roots: Vec::new(),
    }
}

async fn spawn_toker(config: Config) -> (SocketAddr, Arc<Store>) {
    spawn_toker_idle(config, None).await
}

/// [`spawn_toker`], with the upstream idle timeout shortened so a stall
/// test need not wait the real five minutes.
async fn spawn_toker_idle(
    config: Config,
    idle: Option<std::time::Duration>,
) -> (SocketAddr, Arc<Store>) {
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    let mut server = Server::new(config, store.clone()).expect("build server");
    if let Some(idle) = idle {
        server
            .set_upstream_idle_timeout(idle)
            .expect("rebuild the upstream client");
    }
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

/// A body whose tool set gives the request a tools-hash — without tools
/// there is no lane, just a session (the lane rule: a session is not a
/// cache entry).
fn tools_body(model: &str, stream: bool) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": model,
        "stream": stream,
        "tools": [
            {"name": "Read", "input_schema": {"type": "object"}},
            {"name": "Bash", "input_schema": {"type": "object"}},
        ],
        "messages": [{"role": "user", "content": "Hi"}],
    }))
    .expect("serialise tools body")
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
        "parity with the predecessor: claude's session header is forwarded, not swallowed by the proxy"
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
    // The response's own meter snapshot, in the stable wire shape.
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
        .load_meters("anthropic_sub")
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
        store.load_meters("anthropic_sub").expect("meters"),
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

/// [`test_config`] with the openrouter block pointed at `openrouter`
/// (the mock, whose `/v1` the provider's base carries) and holding a
/// literal key.
fn openrouter_config(anthropic_upstream: reqwest::Url, openrouter: &reqwest::Url) -> Config {
    let mut config = test_config(anthropic_upstream, None, "anthropic_sub");
    config.openrouter = Some(OpenRouterConfig {
        upstream: format!("{}v1", openrouter).parse().expect("openrouter url"),
        api_key_env: UNSET_KEY_ENV.to_owned(),
        api_key_keyring: false,
        api_key: Some("sk-or-literal-test".to_owned()),
        picker: None,
    });
    config
}

#[tokio::test]
async fn the_openrouter_prefix_routes_anthropic_messages_to_openrouter() {
    let (anthropic, anthropic_upstream) = spawn_mock().await;
    let (openrouter, openrouter_upstream) = spawn_mock().await;
    let (addr, store) =
        spawn_toker(openrouter_config(anthropic_upstream, &openrouter_upstream)).await;

    let body = messages_body("openrouter/moonshotai/kimi-k3", false);
    let response = post_messages(
        addr,
        "/v1/messages",
        &[("authorization", "Bearer sk-ant-oat01-claude-oauth")],
        &body,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    assert!(anthropic.captured().is_empty(), "nothing reaches anthropic");
    let captured = openrouter.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].path, "/v1/messages");
    let mut expected = IrRequest::parse(&body).expect("parse");
    expected.anthropic_mut().set_model("moonshotai/kimi-k3");
    assert_eq!(captured[0].body.as_ref(), expected.serialise().as_slice());
    assert_eq!(
        captured[0]
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer sk-or-literal-test"),
        "the claude OAuth bearer is replaced by the stored openrouter key"
    );
    assert!(!captured[0].headers.contains_key("x-api-key"));

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.provider.as_deref(), Some("openrouter"));
    assert_eq!(row.route.as_deref(), Some("anthropic:openrouter"));
    assert_eq!(
        row.requested_model.as_deref(),
        Some("openrouter/moonshotai/kimi-k3")
    );
    assert_eq!(row.effective_model.as_deref(), Some("moonshotai/kimi-k3"));
    assert_eq!(
        row.cost_usd, None,
        "no reported cost, and never an anthropic-catalogue estimate"
    );
    assert_eq!(row.cost_kind, None);
}

#[tokio::test]
async fn an_openrouter_row_records_the_billed_cost_and_serving_provider() {
    let (_anthropic, anthropic_upstream) = spawn_mock().await;
    let (_openrouter, openrouter_upstream) = spawn_mock().await;
    let (addr, store) =
        spawn_toker(openrouter_config(anthropic_upstream, &openrouter_upstream)).await;

    for (stream, cost) in [(false, 1.87e-05), (true, 0.0000197)] {
        let body = messages_body("openrouter/z-ai/glm-5.3-flash", stream);
        let response = post_messages(addr, "/v1/messages", &[], &body).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.expect("drain the response");
        let rows = wait_for_rows(&store, 1 + usize::from(stream)).await;
        let row = rows.last().expect("the row");
        assert_eq!(row.cost_usd, Some(cost), "stream: {stream}");
        assert_eq!(row.cost_kind, Some(CostKind::Billed), "stream: {stream}");
        assert_eq!(
            row.extra
                .as_ref()
                .and_then(|extra| extra.get("serving_provider"))
                .and_then(Value::as_str),
            Some("Friendli"),
            "stream: {stream}"
        );
    }
}

#[tokio::test]
async fn the_openrouter_prefix_without_its_block_is_answered_locally() {
    let (mock, upstream) = spawn_mock().await;
    let mut config = test_config(upstream, None, "anthropic_sub");
    config.openrouter = None;
    config.default_backend_openai_chat = None;
    let (addr, _store) = spawn_toker(config).await;

    let body = messages_body("openrouter/moonshotai/kimi-k3", false);
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert!(response.headers().contains_key("x-toker-not-configured"));
    assert!(
        mock.captured().is_empty(),
        "never sent to the default with the prefix still on"
    );
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
async fn non_2xx_forwards_the_body_and_records_an_unpriced_error_row_with_its_meters() {
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
    // Never priced, no usage — but the response's own meters stay on
    // the row, as the predecessor's did: a failure's meters are the only
    // evidence of throttling the ledger gets.
    assert_eq!(row.cost_usd, None);
    assert_eq!(row.cost_kind, None);
    assert_eq!(row.rate_limits, Some(expected_rate_limits("0.77")));
    assert_eq!(row.usage_presence, None);
    assert_eq!(row.input, None);
    assert_eq!(row.usage_raw, None);

    // …but the meters_state table took the response's snapshot anyway
    // (feed from every response, not just accounted ones).
    let meters = store
        .load_meters("anthropic_sub")
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
        .load_meters("anthropic_sub")
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
    // The meters fed from the background poll — the "not just accounted
    // ones" rule.
    let meters = store
        .load_meters("anthropic_sub")
        .expect("meters")
        .expect("the background batch poll fed the meters");
    assert_eq!(meters.snapshot, expected_rate_limits("0.55"));
}

#[tokio::test]
async fn unmatched_paths_pass_through_with_method_query_and_body() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    // A method, path, and query toker's route table names nowhere, with
    // a body that is not JSON: all of it reaches the upstream unchanged.
    let body = b"\x00not json, never parsed\xff".as_slice();
    let response = client()
        .put(toker_url(addr, "/v1/files/file_123?beta=true&limit=2"))
        .header(header::AUTHORIZATION, "Bearer claude-oauth-token")
        .header("x-claude-code-session-id", "ccses-42")
        .header("x-toker-session", "addressed-to-the-proxy")
        .header(header::ACCEPT_ENCODING, "gzip")
        .body(body.to_vec())
        .send()
        .await
        .expect("unmatched request");
    assert_eq!(
        response.status(),
        StatusCode::IM_A_TEAPOT,
        "the upstream's status comes back"
    );
    let bytes = response.bytes().await.expect("unmatched bytes");
    assert_eq!(bytes.as_ref(), b"upstream-unmatched-body");

    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    let forwarded = &captured[0];
    assert_eq!(forwarded.method, "PUT");
    assert_eq!(forwarded.path, "/v1/files/file_123");
    assert_eq!(forwarded.query.as_deref(), Some("beta=true&limit=2"));
    assert_eq!(
        forwarded.body.as_ref(),
        body,
        "the body forwards byte-identical"
    );
    let header_of = |name: &str| {
        forwarded
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(
        header_of("authorization").as_deref(),
        Some("Bearer claude-oauth-token"),
        "the client's credential passes through"
    );
    assert_eq!(
        header_of("x-claude-code-session-id").as_deref(),
        Some("ccses-42"),
        "claude's session header rides upstream, as on every anthropic path"
    );
    assert_eq!(header_of("x-toker-session"), None, "x-toker-* never leaves");
    assert_eq!(header_of("accept-encoding").as_deref(), Some("identity"));

    // A bodiless GET passes through the same way.
    let response = client()
        .get(toker_url(addr, "/v1/organizations/usage?days=7"))
        .send()
        .await
        .expect("unmatched get");
    assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    let captured = mock.captured();
    assert_eq!(captured[1].method, "GET");
    assert_eq!(captured[1].path, "/v1/organizations/usage");
    assert_eq!(captured[1].query.as_deref(), Some("days=7"));
    assert!(captured[1].body.is_empty());

    // Not a usage path: no row. The meters still feed, as on every
    // response from the meter source.
    assert_no_rows(&store).await;
    let meters = store
        .load_meters("anthropic_sub")
        .expect("meters")
        .expect("the pass-through response fed the meters");
    assert_eq!(meters.snapshot, expected_rate_limits("0.33"));
}

#[tokio::test]
async fn unmatched_toker_paths_stay_local() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, _store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    for path in ["/_toker/unknown", "/_toker/status/extra", "/_toker"] {
        let response = client()
            .get(toker_url(addr, path))
            .header(header::AUTHORIZATION, "Bearer claude-oauth-token")
            .send()
            .await
            .expect("toker request");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    // A known control path with the wrong method is axum's 405, not the
    // fallback's forward.
    let response = client()
        .post(toker_url(addr, "/_toker/status"))
        .send()
        .await
        .expect("toker request");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

    assert!(
        mock.captured().is_empty(),
        "the control namespace never reaches the upstream"
    );
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
        "…cost left null for an unknown model — never a guess (the one-time warning fired)"
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
    // `model` is the normalised identity, `raw_model` the wire form.
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
        .load_meters("anthropic_sub")
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

/// Read a response body to its end: `Err` when the transfer was
/// aborted rather than terminated.
async fn read_to_end(mut response: reqwest::Response) -> Result<Vec<u8>, reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[tokio::test]
async fn an_upstream_failure_mid_stream_aborts_the_client_response_and_records_no_row() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    let response = post_messages(addr, "/v1/messages", &[], &messages_body("drop-mid", true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    // The stream once ended cleanly here, so claude saw a properly
    // terminated (truncated) turn and accepted it instead of retrying.
    let read = tokio::time::timeout(std::time::Duration::from_secs(10), read_to_end(response))
        .await
        .expect("the client response ends promptly");
    assert!(
        read.is_err(),
        "the client sees a transport error, not a clean end: {read:?}"
    );

    assert_no_rows(&store).await;
}

#[tokio::test]
async fn a_truncated_buffered_body_answers_502_and_records_no_row() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    // A 2xx body and an error body alike: half of either was once
    // forwarded under the upstream's status as if it were whole.
    for model in ["drop-mid", "drop-mid-401"] {
        let response = post_messages(addr, "/v1/messages", &[], &messages_body(model, false)).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{model}");
        assert_eq!(content_type(&response), "application/json", "{model}");
        let body = response.bytes().await.expect("the 502 itself is whole");
        assert!(
            !body.windows(7).any(|window| window == b"\"usage\""),
            "{model}: none of the truncated body is forwarded"
        );
        // The error is anthropic-shaped, so the client reports and
        // retries it like any API error instead of failing to parse it.
        let error: Value = serde_json::from_slice(&body).expect("a JSON error");
        assert_eq!(error["type"], "error", "{model}");
        assert_eq!(error["error"]["type"], "api_error", "{model}");
        assert!(error["error"]["message"].is_string(), "{model}");
    }

    assert_no_rows(&store).await;
}

#[tokio::test]
async fn a_stalled_upstream_stream_aborts_after_the_idle_timeout() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker_idle(
        test_config(upstream, None, "anthropic_sub"),
        Some(std::time::Duration::from_millis(300)),
    )
    .await;

    // The first event arrives, then the upstream goes silent on a live
    // connection — the stall that once held the sleep lock forever.
    let mut response = post_messages(addr, "/v1/messages", &[], &messages_body("hang", true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let chunk = response
        .chunk()
        .await
        .expect("first chunk")
        .expect("non-empty");
    assert!(!chunk.is_empty());
    let rest = tokio::time::timeout(std::time::Duration::from_secs(10), read_to_end(response))
        .await
        .expect("the idle timeout ends the stall");
    assert!(rest.is_err(), "a stall is an abort, not a clean end");

    // The upstream request is released too, and nothing is recorded.
    for _ in 0..100 {
        if mock.upstream_dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        mock.upstream_dropped.load(Ordering::SeqCst),
        "the stalled upstream request was dropped"
    );
    assert_no_rows(&store).await;
}

#[tokio::test]
async fn an_upstream_silent_before_its_headers_answers_502_after_the_idle_timeout() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker_idle(
        test_config(upstream, None, "anthropic_sub"),
        Some(std::time::Duration::from_millis(300)),
    )
    .await;

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        post_messages(
            addr,
            "/v1/messages",
            &[],
            &messages_body("stall-headers", true),
        ),
    )
    .await
    .expect("the idle timeout covers the wait for headers");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(content_type(&response), "application/json");
    let error: Value = response.json().await.expect("a JSON error");
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "api_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("toker upstream error: ")),
        "the transport's failure, named as toker's: {error}"
    );

    assert_no_rows(&store).await;
}

#[tokio::test]
async fn the_idle_timeout_leaves_a_slow_but_live_stream_alone() {
    let (_mock, upstream) = spawn_mock().await;
    // Eight chunks 100 ms apart: longer in total than the 300 ms idle
    // timeout, never that long between reads.
    let (addr, store) = spawn_toker_idle(
        test_config(upstream, None, "anthropic_sub"),
        Some(std::time::Duration::from_millis(300)),
    )
    .await;

    let response = post_messages(addr, "/v1/messages", &[], &messages_body("slow", true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = read_to_end(response)
        .await
        .expect("a live stream completes");
    assert_eq!(
        body,
        fixture("03_1h_write_crlf.sse").to_vec(),
        "every byte arrives"
    );

    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows.len(), 1, "a completed slow stream still records");
}

/// Whether raw request bytes still contain the release marker.
fn contains_marker(bytes: &[u8]) -> bool {
    bytes
        .windows(SENTINEL.len())
        .any(|window| window == SENTINEL.as_bytes())
}

// ---------------------------------------------------------------------------
// The quota gate (anthropic_sub only — the sole meter source)
// ---------------------------------------------------------------------------

/// Poison the meters_state snapshot: a 5-hour window at `util5h` with its
/// reset `offset_secs` from now, a healthy 7-day one. The snapshot is the
/// stable wire shape `parse_rate_limits` stores, so the gate reads it
/// exactly as it reads a real one. Returns the 5h reset and the snapshot.
fn poison_meters(store: &Store, util5h: f64, offset_secs: i64) -> (i64, Value) {
    let now_ms = jiff::Timestamp::now().as_millisecond();
    let now_secs = now_ms / 1000;
    let reset5h = now_secs + offset_secs;
    let snapshot = json!({
        "util5h": util5h, "reset5h": reset5h,
        "util7d": 0.2, "reset7d": now_secs + 5 * 86400,
        "utilOverage": Value::Null, "resetOverage": Value::Null, "status": Value::Null,
        "status5h": Value::Null, "status7d": Value::Null, "statusOverage": Value::Null,
        "claim": Value::Null, "overageInUse": false, "fallbackPct": Value::Null,
        "other": {},
    });
    store
        .save_meters(
            "anthropic_sub",
            &MetersSnapshot {
                updated_ms: now_ms,
                snapshot: snapshot.clone(),
            },
        )
        .expect("poison meters");
    (reset5h, snapshot)
}

/// A Messages body without a `stream` field — the shape that must be
/// answered with the SSE turn, since a client that omitted the field
/// cannot be assumed to parse a plain JSON body.
fn messages_body_no_stream(model: &str) -> Vec<u8> {
    format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"Hi"}}]}}"#).into_bytes()
}

fn content_type(response: &reqwest::Response) -> &str {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .expect("content type set")
}

#[tokio::test]
async fn a_spent_meter_blocks_with_a_synthetic_200_and_never_reaches_upstream() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;
    let (reset5h, snapshot) = poison_meters(&store, 1.0, 3600);

    // `stream` omitted → the SSE turn. The notice names the meter, the
    // reset (in the local zone, the same one the server renders in), and
    // the resume path; the turn carries the model the client asked for.
    let body = messages_body_no_stream("claude-opus-5");
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK, "never an error status");
    assert_eq!(content_type(&response), "text/event-stream");
    let bytes = response.bytes().await.expect("blocked bytes");
    let tz = jiff::tz::TimeZone::system();
    // GatesConfig::default() above → the notice renders in the default
    // style, the generic GFM alert; the expected turn is built the same
    // way, wrapper and all.
    let notice = Blocking::notice(
        Meter::FiveHour,
        Some(reset5h),
        false,
        None,
        &tz,
        NoticeStyle::Gfm,
    );
    let expected = Blocking::blocked_turn(&notice, Some("claude-opus-5"), Rendering::Sse);
    assert_eq!(
        bytes.as_ref(),
        expected.as_slice(),
        "the synthetic SSE turn, the exact event shape"
    );

    // Upstream NEVER hit: the gate is the only thing that may stop a
    // session, and it stops it before the wire.
    assert!(
        mock.captured().is_empty(),
        "a blocked request never reaches upstream"
    );

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.kind, Some(RowKind::Blocked));
    assert_eq!(row.frontend.as_deref(), Some("anthropic"));
    assert_eq!(row.provider.as_deref(), Some("anthropic_sub"));
    assert_eq!(row.route.as_deref(), Some("anthropic:anthropic_sub"));
    assert_eq!(row.session_id.as_deref(), Some("ccses-42"));
    assert_eq!(row.gate_on, Some(true));
    assert!(row.duration_ms.is_some());
    // Parity: the stale snapshot the block was decided on.
    assert_eq!(row.rate_limits, Some(snapshot));
    assert_eq!(
        row.extra.as_ref().and_then(|extra| extra.get("meter")),
        Some(&json!("5h"))
    );
    assert_eq!(
        row.extra
            .as_ref()
            .and_then(|extra| extra.get("resets_at"))
            .and_then(Value::as_i64),
        Some(reset5h)
    );
    assert!(
        row.extra
            .as_ref()
            .is_some_and(|extra| extra.get("context_tokens") == Some(&Value::Null)),
        "not recorded yet — never printed as a zero"
    );
    // A proxy-written row: no usage, no model, never priced (the
    // predecessor's blocked
    // row carries no model either).
    assert_eq!(row.model, None);
    assert_eq!(row.requested_model, None);
    assert_eq!(row.effective_model, None);
    assert_eq!(row.input, None);
    assert_eq!(row.usage_presence, None);
    assert_eq!(row.cost_usd, None);
    assert_eq!(row.cost_kind, None);
    assert_eq!(row.status, None);

    // An explicit `stream: false` gets the plain JSON Message instead —
    // a client that asked for one cannot parse an event stream.
    let body = messages_body("claude-opus-5", false);
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/json");
    let bytes = response.bytes().await.expect("blocked bytes");
    let expected = Blocking::blocked_turn(&notice, Some("claude-opus-5"), Rendering::Json);
    assert_eq!(bytes.as_ref(), expected.as_slice());

    assert!(mock.captured().is_empty(), "still nothing upstream");
    let rows = wait_for_rows(&store, 2).await;
    assert!(
        rows.iter().all(|row| row.kind == Some(RowKind::Blocked)),
        "both blocked rows recorded"
    );
}

#[tokio::test]
async fn a_block_states_the_size_of_the_session_s_largest_lane() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;
    let (reset5h, _) = poison_meters(&store, 1.0, 3600);
    // The main agent's lane, the tool-less title summariser's, and a
    // stranger's bigger one: the notice reports the conversation being
    // decided about, which is the session's largest lane — never the lane
    // of the request that happened to hit the wall, never another session.
    for (key, prompt) in [
        ("ccses-42|main", 412_345),
        ("ccses-42|e3b0c44298fc", 2_000),
        ("ccses-420|main", 900_000),
    ] {
        let (session, tools) = key.split_once('|').expect("a lane key");
        store
            .upsert_lane(&toker::store::Lane {
                key: key.to_owned(),
                session_id: Some(session.to_owned()),
                tools_hash: Some(tools.to_owned()),
                updated_ms: 1_000,
                prompt_tokens: Some(prompt),
                ttl: None,
                ping: None,
                noticed_at: None,
                forced_from: None,
                forced_to: None,
            })
            .expect("seed lane");
    }

    let body = messages_body_no_stream("claude-opus-5");
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("blocked bytes");
    let tz = jiff::tz::TimeZone::system();
    let notice = Blocking::notice(
        Meter::FiveHour,
        Some(reset5h),
        false,
        Some(412_345),
        &tz,
        NoticeStyle::Gfm,
    );
    assert!(notice.contains("This session's context is 412,345 tokens."));
    let expected = Blocking::blocked_turn(&notice, Some("claude-opus-5"), Rendering::Sse);
    assert_eq!(bytes.as_ref(), expected.as_slice());
    assert!(mock.captured().is_empty());

    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(
        rows[0]
            .extra
            .as_ref()
            .and_then(|extra| extra.get("context_tokens")),
        Some(&json!(412_345)),
        "the row carries the figure the notice stated"
    );
}

#[tokio::test]
async fn a_mapped_batch_records_which_requests_the_map_moved() {
    // A batch has no single top-level model, so its provenance is the
    // per-request list of the entries the map matched. A batch writes a
    // row only when it fails, and that row is where the list must land.
    let (mock, upstream) = spawn_mock().await;
    let mut config = test_config(upstream, Some("sk-test".to_owned()), "anthropic_api");
    config.anthropic_api.as_mut().expect("enabled").model_map =
        toker::middleware::model_map::parse_model_map(
            r#"{"family:haiku": "err-401", "model:claude-opus-4-5": "claude-opus-5"}"#,
        )
        .expect("the test map parses");
    let (addr, store) = spawn_toker(config).await;

    let body = json!({"requests": [
        {"custom_id": "a", "params": {"model": "claude-haiku-4-5", "max_tokens": 1, "messages": []}},
        {"custom_id": "b", "params": {"model": "claude-sonnet-5", "max_tokens": 1, "messages": []}},
        {"custom_id": "c", "params": {"model": "claude-opus-4-5", "max_tokens": 1, "messages": []}},
    ]});
    let body = serde_json::to_vec(&body).expect("batch body");
    let response = post_messages(addr, "/v1/messages/batches", &[], &body).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let sent: Value =
        serde_json::from_slice(&mock.captured()[0].body).expect("the forwarded batch is JSON");
    assert_eq!(
        sent.pointer("/requests/0/params/model"),
        Some(&json!("err-401"))
    );
    assert_eq!(
        sent.pointer("/requests/1/params/model"),
        Some(&json!("claude-sonnet-5")),
        "an unmatched request keeps its model"
    );

    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].kind, Some(RowKind::Error));
    assert_eq!(
        rows[0].model_mappings,
        Some(json!([
            {"requestIndex": 0, "requestedModel": "claude-haiku-4-5", "effectiveModel": "err-401"},
            {"requestIndex": 2, "requestedModel": "claude-opus-4-5", "effectiveModel": "claude-opus-5"},
        ])),
        "only the matched entries, each with its index"
    );
}

#[tokio::test]
async fn the_notice_style_follows_the_frontend_prefix() {
    // One spent meter, one decision: the frontend's base-URL prefix picks
    // the wrapping through the `[notices]` table, and an unprefixed or
    // unnamed frontend gets the table's default.
    let (mock, upstream) = spawn_mock().await;
    let mut config = test_config(upstream, None, "anthropic_sub");
    config.notices.default = NoticeStyle::Plain;
    let (addr, store) = spawn_toker(config).await;
    let (reset5h, _snapshot) = poison_meters(&store, 1.0, 3600);
    let tz = jiff::tz::TimeZone::system();
    for (path, style) in [
        ("/v1/messages", NoticeStyle::Plain),
        ("/f/claude/v1/messages", NoticeStyle::Block),
        ("/f/workhorse/v1/messages", NoticeStyle::Toker),
        ("/f/someone-else/v1/messages", NoticeStyle::Plain),
    ] {
        let response =
            post_messages(addr, path, &[], &messages_body_no_stream("claude-opus-5")).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let bytes = response.bytes().await.expect("blocked bytes");
        let notice = Blocking::notice(Meter::FiveHour, Some(reset5h), false, None, &tz, style);
        let expected = Blocking::blocked_turn(&notice, Some("claude-opus-5"), Rendering::Sse);
        assert_eq!(
            bytes.as_ref(),
            expected.as_slice(),
            "{path}: {style:?}, served byte for byte"
        );
    }
    assert!(mock.captured().is_empty());
}

#[tokio::test]
async fn a_prefixed_request_is_forwarded_without_its_prefix() {
    // The prefix is toker's, never the upstream's: the frontend's path
    // and query arrive as they would unprefixed.
    let (mock, upstream) = spawn_mock().await;
    let (addr, _store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;
    let body = messages_body("claude-opus-5", false);
    let response = post_messages(addr, "/f/claude/v1/messages?beta=true", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].path, "/v1/messages");
    assert_eq!(captured[0].query.as_deref(), Some("beta=true"));
}

#[tokio::test]
async fn rows_name_the_prefixed_frontend_in_extra() {
    // The `frontend` column stays the protocol (and `route` stays built
    // from it); the client's own name rides in `extra`, only when a
    // prefix named it.
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;
    let (_reset5h, _snapshot) = poison_meters(&store, 1.0, 3600);
    for path in ["/f/claude/v1/messages", "/v1/messages"] {
        let response =
            post_messages(addr, path, &[], &messages_body_no_stream("claude-opus-5")).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    let rows = wait_for_rows(&store, 2).await;
    for row in &rows {
        assert_eq!(row.kind, Some(RowKind::Blocked));
        assert_eq!(row.frontend.as_deref(), Some("anthropic"));
        assert_eq!(row.route.as_deref(), Some("anthropic:anthropic_sub"));
    }
    let named: Vec<Option<&Value>> = rows
        .iter()
        .map(|row| row.extra.as_ref().and_then(|extra| extra.get("frontend")))
        .collect();
    assert_eq!(named, vec![Some(&json!("claude")), None]);
}

#[tokio::test]
async fn an_expired_spent_reading_fails_open_and_forwards() {
    // The rule that un-wedges the gate: a blocked request can never
    // refresh meters, so a reading whose window has already passed must
    // stop counting. Forwarding is self-correcting — the response carries
    // fresh headers either way.
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;
    poison_meters(&store, 1.0, -3600);

    let body = messages_body("claude-opus-5", false);
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body bytes");
    assert_eq!(
        bytes.as_ref(),
        non_stream_body("claude-opus-5").as_slice(),
        "forwarded, and the response passes through"
    );
    assert_eq!(mock.captured().len(), 1, "the request forwarded");

    // A real measurement, not a blocked row.
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].kind, None);
}

#[tokio::test]
async fn a_release_marker_grants_an_allowance_records_a_released_row_and_strips() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;
    let (reset5h, snapshot) = poison_meters(&store, 1.0, 3600);

    // The marker typed into the conversation, opening the last user
    // message — exactly the turn the human types it on.
    let body = serde_json::to_vec(&json!({
        "model": "claude-opus-5",
        "messages": [{"role": "user", "content": "$#$BURN$#$ go on"}],
    }))
    .expect("release body");
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK, "the release forwards");

    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    assert!(
        !contains_marker(&captured[0].body),
        "the marker never reaches the model"
    );
    // The strip is byte-exact: what went upstream is the IR round-trip
    // with the marker spliced out.
    let mut expected = IrRequest::parse(&body).expect("parse");
    expected.anthropic_mut().strip_release();
    assert_eq!(captured[0].body.as_ref(), expected.serialise().as_slice());

    // The released row (the trace of "this session may spend overage this
    // window"), plus the normal measurement for the forwarded request.
    let rows = wait_for_rows(&store, 2).await;
    let released = rows
        .iter()
        .find(|row| row.kind == Some(RowKind::Released))
        .expect("a released row");
    assert_eq!(released.session_id.as_deref(), Some("ccses-42"));
    assert_eq!(released.provider.as_deref(), Some("anthropic_sub"));
    assert_eq!(released.gate_on, Some(true));
    assert_eq!(
        released.rate_limits,
        Some(snapshot.clone()),
        "parity: the stale snapshot the grant rested on"
    );
    assert_eq!(
        released
            .extra
            .as_ref()
            .and_then(|extra| extra.get("fiveHour")),
        Some(&json!(reset5h)),
        "granted for the exhausted meter only, keyed by reset value"
    );
    assert_eq!(
        released
            .extra
            .as_ref()
            .and_then(|extra| extra.get("sevenDay")),
        Some(&Value::Null),
        "the healthy 7-day meter was not granted"
    );
    assert_eq!(
        released.duration_ms, None,
        "no duration on released rows — ported behaviour"
    );
    assert!(
        rows.iter().any(|row| row.kind.is_none()),
        "the forwarded request recorded its measurement"
    );

    // The allowance landed in the store, keyed session + meter + reset.
    assert_eq!(
        store.load_allowances().expect("allowances"),
        vec![Allowance {
            session_id: "ccses-42".to_owned(),
            meter: "5h".to_owned(),
            reset_value: reset5h,
            release: Release::Overage,
        }]
    );

    // And it un-gates the still-spent snapshot it was granted against.
    let allowances = store.load_allowances().expect("allowances");
    assert_eq!(
        decide(
            Some(Meters::over(&snapshot)),
            &allowances,
            jiff::Timestamp::now().as_millisecond(),
        ),
        GateDecision::Forward,
        "held == current: released for this window"
    );
}

/// The over marker through the real router: offered while the plan has
/// room, it forwards past the gate until the plan is spent, then the gate
/// stops the session again and offers only the burn marker, which widens
/// the same allowance to overage.
#[tokio::test]
async fn the_over_marker_spends_the_plan_and_stops_before_overage() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;
    let (reset5h, snapshot) = poison_meters(&store, 0.99, 3600);
    let reading = |util: f64| {
        let mut reading = snapshot.clone();
        reading["util5h"] = json!(util);
        store
            .save_meters(
                "anthropic_sub",
                &MetersSnapshot {
                    updated_ms: jiff::Timestamp::now().as_millisecond(),
                    snapshot: reading,
                },
            )
            .expect("meters");
    };
    let typed = |text: &str| {
        serde_json::to_vec(&json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": text}],
        }))
        .expect("body")
    };
    let plain = typed("carry on");

    // At the gate with room: blocked, and the notice offers both markers.
    let blocked = post_messages(addr, "/v1/messages", &[], &plain).await;
    let text = blocked.text().await.expect("notice");
    assert!(text.contains("almost spent"), "{text}");
    assert!(text.contains("over marker"), "{text}");
    assert!(mock.captured().is_empty());

    // The over marker forwards, stripped, and grants a plan allowance.
    let released = typed(&format!("{PLAN_SENTINEL} use the rest"));
    let response = post_messages(addr, "/v1/messages", &[], &released).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.bytes().await;
    assert_eq!(mock.captured().len(), 1);
    assert!(
        !mock.captured()[0]
            .body
            .windows(PLAN_SENTINEL.len())
            .any(|window| window == PLAN_SENTINEL.as_bytes()),
        "the over marker never reaches the model"
    );
    let allowance = Allowance {
        session_id: "ccses-42".to_owned(),
        meter: "5h".to_owned(),
        reset_value: reset5h,
        release: Release::Plan,
    };
    assert_eq!(
        store.load_allowances().expect("allowances"),
        vec![allowance.clone()]
    );
    let rows = wait_for_rows(&store, 3).await;
    let row = rows
        .iter()
        .find(|row| row.kind == Some(RowKind::Released))
        .expect("a released row");
    assert_eq!(
        row.extra.as_ref().and_then(|extra| extra.get("release")),
        Some(&json!("plan"))
    );

    // Still room: plain turns forward.
    let response = post_messages(addr, "/v1/messages", &[], &plain).await;
    let _ = response.bytes().await;
    assert_eq!(mock.captured().len(), 2);

    // The plan is spent: stopped again, offered only the burn marker.
    reading(1.0);
    let blocked = post_messages(addr, "/v1/messages", &[], &plain).await;
    let text = blocked.text().await.expect("notice");
    assert!(text.contains("is spent until"), "{text}");
    assert!(!text.contains("over marker"), "{text}");
    assert_eq!(mock.captured().len(), 2, "nothing reached upstream");

    // The burn marker widens the same allowance to overage.
    let burn = typed(&format!("{SENTINEL} go on"));
    let response = post_messages(addr, "/v1/messages", &[], &burn).await;
    let _ = response.bytes().await;
    assert_eq!(mock.captured().len(), 3);
    assert_eq!(
        store.load_allowances().expect("allowances"),
        vec![Allowance {
            release: Release::Overage,
            ..allowance
        }]
    );
}

#[tokio::test]
async fn the_gate_reads_only_the_session_s_own_allowances_and_survives_the_prune() {
    // The gate loads one session's rows by key rather than the whole
    // table; another session's allowance for the very window that is
    // spent must not release this one, and pruning the ended windows
    // must not change what the gate decides.
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;
    let (reset5h, _snapshot) = poison_meters(&store, 1.0, 3600);
    let held = |session: &str, reset_value: i64| Allowance {
        session_id: session.to_owned(),
        meter: "5h".to_owned(),
        reset_value,
        release: Release::Overage,
    };
    // This session's release for a window that has already rolled, and
    // a neighbour's release for the current one.
    store
        .record_allowance(&held("ccses-42", reset5h - 5 * 3600))
        .expect("record");
    store
        .record_allowance(&held("ccses-other", reset5h))
        .expect("record");

    let body = messages_body_no_stream("claude-opus-5");
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.expect("blocked bytes");
    assert!(
        mock.captured().is_empty(),
        "blocked: nothing held for this window"
    );

    // The prune takes only the rolled window; the gate still blocks.
    let now = jiff::Timestamp::now().as_millisecond();
    assert_eq!(store.prune_allowances(now).expect("prune"), 1);
    assert_eq!(
        store.load_allowances().expect("allowances"),
        vec![held("ccses-other", reset5h)]
    );
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    response.bytes().await.expect("blocked bytes");
    assert!(mock.captured().is_empty(), "still blocked after the prune");

    // This session's own release for the current window forwards, and a
    // prune leaves it in force.
    store
        .record_allowance(&held("ccses-42", reset5h))
        .expect("record");
    assert_eq!(store.prune_allowances(now).expect("prune"), 0);
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.expect("forwarded bytes");
    assert_eq!(mock.captured().len(), 1, "released for this window");
}

#[tokio::test]
async fn marker_stripping_is_unconditional_and_a_healthy_release_still_records() {
    // No poisoning at all: meters_state absent (cold start), unknown
    // meters forward — and the release is still granted and recorded
    // (the release fires on marker + session, not on exhaustion), with
    // null reset values, because a release that left no trace could never
    // explain later spending.
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    let body = serde_json::to_vec(&json!({
        "model": "claude-opus-5",
        "messages": [{"role": "user", "content": "$#$BURN$#$ go on"}],
    }))
    .expect("release body");
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK);

    let captured = mock.captured();
    assert_eq!(captured.len(), 1, "unknown meters forward");
    assert!(
        !contains_marker(&captured[0].body),
        "the strip runs regardless of the meters — the marker rule is frozen"
    );

    let rows = wait_for_rows(&store, 2).await;
    let released = rows
        .iter()
        .find(|row| row.kind == Some(RowKind::Released))
        .expect("the release is recorded even when nothing is exhausted");
    assert_eq!(
        released
            .extra
            .as_ref()
            .and_then(|extra| extra.get("fiveHour")),
        Some(&Value::Null),
        "nothing was exhausted: no reset to grant"
    );
    assert_eq!(
        released
            .extra
            .as_ref()
            .and_then(|extra| extra.get("sevenDay")),
        Some(&Value::Null)
    );
    assert_eq!(
        released.rate_limits, None,
        "no snapshot existed for the grant to rest on"
    );
    assert!(
        rows.iter().any(|row| row.kind.is_none()),
        "the forwarded request recorded its measurement"
    );
    assert!(
        store.load_allowances().expect("allowances").is_empty(),
        "no exhausted meters: no allowance rows"
    );
}

#[tokio::test]
async fn a_disabled_gate_forwards_but_the_strip_stays_on() {
    // The toggle arms the gate and the release recording
    // (gate-on plus the release/gate checks) — but never the strip: the
    // marker rule is a
    // frozen public API, and gating it on the flag would change the cached
    // prefix of every conversation carrying a marker.
    let (mock, upstream) = spawn_mock().await;
    let mut config = test_config(upstream, None, "anthropic_sub");
    config.gates.quota_enabled = false;
    let (addr, store) = spawn_toker(config).await;
    poison_meters(&store, 1.0, 3600);

    // A spent meter forwards…
    let response = post_messages(
        addr,
        "/v1/messages",
        &[],
        &messages_body("claude-opus-5", false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "the gate is off");
    assert_eq!(mock.captured().len(), 1);

    // …and a marker run strips but records no released row (the release
    // is gated on the gate toggle too).
    let body = serde_json::to_vec(&json!({
        "model": "claude-opus-5",
        "messages": [{"role": "user", "content": "$#$BURN$#$ go on"}],
    }))
    .expect("release body");
    let response = post_messages(addr, "/v1/messages", &[], &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let captured = mock.captured();
    assert_eq!(captured.len(), 2);
    assert!(
        !contains_marker(&captured[1].body),
        "the strip is unconditional"
    );

    let rows = wait_for_rows(&store, 2).await;
    assert!(
        rows.iter().all(|row| row.kind.is_none()),
        "no blocked rows, no released rows — only the two measurements"
    );
    assert!(store.load_allowances().expect("allowances").is_empty());
}

#[tokio::test]
async fn count_tokens_and_the_api_backend_never_gate() {
    // count_tokens is not the gated path — blocking it protects no quota,
    // only breaks the client — and neither is the api backend, which is
    // not a meter source. Both must forward against live poisoned meters.
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(
        upstream,
        Some("sk-ant-literal-test".to_owned()),
        "anthropic_sub",
    ))
    .await;

    // count_tokens: same pipeline, never a gate target.
    poison_meters(&store, 1.0, 3600);
    let response = post_messages(
        addr,
        "/v1/messages/count_tokens",
        &[],
        &messages_body("claude-opus-5", false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.captured().len(), 1, "forwarded untouched");
    assert_no_rows(&store).await;

    // The api backend: the gate never fires, and the api is not a meter
    // source either, so meters_state keeps its poisoned snapshot.
    poison_meters(&store, 1.0, 3600);
    let response = post_messages(
        addr,
        "/v1/messages",
        &[],
        &messages_body("anthropic_api/claude-opus-5", false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.captured().len(), 2, "routed to the api and forwarded");
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].kind, None, "a measurement, not a blocked row");
    assert_eq!(rows[0].provider.as_deref(), Some("anthropic_api"));
    let meters = store
        .load_meters("anthropic_sub")
        .expect("meters")
        .expect("still poisoned");
    assert_eq!(
        meters.snapshot.get("util5h").and_then(Value::as_f64),
        Some(1.0),
        "the api's RPM-style headers never overwrite the gate's snapshot"
    );
}

#[tokio::test]
async fn an_unparseable_body_on_the_gated_path_still_gates() {
    // The decision is on `gated` alone, not on the body's
    // parseability — a broken client must not be able to duck under a
    // spent quota. `clientWants` reads model/stream independently and
    // falls back to null/false on a JSON failure, so the answer is the
    // SSE turn under the default model.
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;
    let (reset5h, _snapshot) = poison_meters(&store, 1.0, 3600);

    let response = post_messages(addr, "/v1/messages", &[], b"not json at all").await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "blocked, never an error status — even for a broken body"
    );
    assert_eq!(content_type(&response), "text/event-stream");
    let bytes = response.bytes().await.expect("blocked bytes");
    let tz = jiff::tz::TimeZone::system();
    let notice = Blocking::notice(
        Meter::FiveHour,
        Some(reset5h),
        false,
        None,
        &tz,
        NoticeStyle::Gfm,
    );
    let expected = Blocking::blocked_turn(&notice, None, Rendering::Sse);
    assert_eq!(
        bytes.as_ref(),
        expected.as_slice(),
        "SSE turn, default model — clientWants' fallback"
    );
    assert!(mock.captured().is_empty());

    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].kind, Some(RowKind::Blocked));
}

// ---------------------------------------------------------------------------
// The lane table + learned model store (phase 2, unit 5)
// ---------------------------------------------------------------------------

/// Poll the lanes table until it holds exactly `count` rows — the lane
/// upsert lands at response completion right after (not atomically with)
/// the ledger row, so a row-count wait is not a lane-count wait.
async fn wait_for_lanes(store: &Store, count: usize) -> Vec<toker::store::Lane> {
    for _ in 0..200 {
        let lanes = store.load_lanes().expect("read lanes");
        if lanes.len() == count {
            return lanes;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    store.load_lanes().expect("read lanes")
}

/// The one lane the session `ccses-42` holds (post_messages' session id),
/// or panic naming what was actually there.
fn the_lane(lanes: Vec<toker::store::Lane>) -> toker::store::Lane {
    let ours: Vec<_> = lanes
        .iter()
        .filter(|lane| lane.session_id.as_deref() == Some("ccses-42"))
        .collect();
    assert_eq!(ours.len(), 1, "one lane for the session, got {lanes:?}");
    ours[0].clone()
}

#[tokio::test]
async fn lane_rows_written_on_response_upsert_not_duplicate() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    // First response: fixture 03 — a 1h-tier write of 82,420 tokens.
    let response = post_messages(
        addr,
        "/v1/messages",
        &[("authorization", "Bearer claude-oauth-token")],
        &tools_body("claude-opus-5", true),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let rows = wait_for_rows(&store, 1).await;
    let first_ts = rows[0].ts_ms;
    let lanes = wait_for_lanes(&store, 1).await;
    let lane = the_lane(lanes);
    assert_eq!(
        lane.prompt_tokens,
        Some(82_422),
        "prompt = fresh input + cache read + cache writes (2 + 0 + 82,420)"
    );
    assert_eq!(
        lane.ttl,
        Some(3_600_000),
        "the 1h-tier write sets the hour tier"
    );
    assert_eq!(lane.ping, None, "an ordinary request is not a ping");
    assert_eq!(
        lane.noticed_at, None,
        "no cold notice has fired (that unit is next)"
    );

    // Second response in the same lane: fixture 04 (via the mock's 5m-tier
    // arm) — a warm follow-up whose writes landed on the 5-minute tier.
    let response = post_messages(
        addr,
        "/v1/messages",
        &[("authorization", "Bearer claude-oauth-token")],
        &tools_body("5m-tier", true),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let rows = wait_for_rows(&store, 2).await;

    // Upsert, not insert: the second response UPDATES the one lane.
    let lanes = wait_for_lanes(&store, 1).await;
    assert_eq!(lanes.len(), 1, "same session × tools-hash is one lane");
    let lane = the_lane(lanes);
    assert_eq!(lane.updated_ms, rows[1].ts_ms.max(first_ts));
    assert!(
        lane.updated_ms > first_ts,
        "`at` moved with the served response"
    );
    assert_eq!(
        lane.prompt_tokens,
        Some(5 + 82_420 + 1_200),
        "the later reading is the lane"
    );
    // THE stickiness rule, end to end: a later 5m-only write does not
    // shorten the 1h prefix the lane already holds.
    assert_eq!(lane.ttl, Some(3_600_000));

    // The served model was learned: the response identity (fixture 04
    // names claude-opus-5), with today's local day and the held prompt as
    // the empirical ceiling.
    let models = store.load_models().expect("models");
    assert_eq!(models.len(), 1, "both fixtures served the same identity");
    assert_eq!(models[0].model_id, "claude-opus-5");
    let days = models[0]
        .days_json
        .as_ref()
        .and_then(Value::as_array)
        .expect("days recorded");
    assert_eq!(days.len(), 1, "both responses landed on one local day");
    assert_eq!(
        models[0].max_prompt,
        Some(83_625),
        "the ceiling is the max held"
    );
    assert_eq!(mock.captured().len(), 2);

    // A different tools-hash is a different conversation, not an update
    // (the lane rule: one session interleaves several prefixes).
    let mut other_tools = tools_body("claude-opus-5", true);
    other_tools.extend_from_slice(br#",{"role":"user","content":"again"}]"#);
    let response = post_messages(
        addr,
        "/v1/messages",
        &[("authorization", "Bearer claude-oauth-token")],
        &serde_json::to_vec(&json!({
            "model": "claude-opus-5", "stream": true,
            "tools": [
                {"name": "Read", "input_schema": {"type": "object"}},
                {"name": "Grep", "input_schema": {"type": "object"}},
            ],
            "messages": [{"role": "user", "content": "Hi"}],
        }))
        .expect("body"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    wait_for_lanes(&store, 2).await;
}

#[tokio::test]
async fn a_tool_less_request_keys_the_empty_list_s_lane_and_a_sessionless_one_none() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    // No tools is a lane of its own — the empty list's hash, the key the
    // predecessor's imported rows carry for the same requests.
    let response = post_messages(
        addr,
        "/v1/messages",
        &[],
        &messages_body("claude-opus-5", true),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    wait_for_rows(&store, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let lanes = store.load_lanes().expect("lanes");
    assert_eq!(
        lanes
            .iter()
            .map(|lane| lane.key.as_str())
            .collect::<Vec<_>>(),
        vec!["ccses-42|e3b0c44298fc"],
        "a tool-less request keys the empty list's lane"
    );
    let rows = store.requests_since(0, 10).expect("rows");
    assert_eq!(rows[0].tools_hash.as_deref(), Some("e3b0c44298fc"));

    // Without a session there is no conversation to attribute it to:
    // the row records, no lane does.
    let response = client()
        .post(toker_url(addr, "/v1/messages"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(tools_body("claude-opus-5", true))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.bytes().await;
    wait_for_rows(&store, 2).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        store.load_lanes().expect("lanes").len(),
        1,
        "no sessionless lane"
    );

    // count_tokens carries the same pipeline but its responses hold no
    // usage: nothing records, nothing lanes.
    let response = post_messages(
        addr,
        "/v1/messages/count_tokens",
        &[],
        &tools_body("claude-opus-5", false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(store.load_lanes().expect("lanes").len(), 1);
    assert_eq!(mock.captured().len(), 3, "every request still forwarded");
}

#[tokio::test]
async fn ping_tagged_requests_record_the_lane_but_flag_it() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    // The window pinger's probe: same pipeline, same lane, flagged.
    let response = post_messages(
        addr,
        "/v1/messages",
        &[("x-toker-ping", "1")],
        &tools_body("claude-opus-5", true),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(
        rows[0].ping,
        Some(true),
        "parity: only a ping request says so"
    );
    let lane = the_lane(wait_for_lanes(&store, 1).await);
    assert_eq!(lane.ping, Some(true), "recorded, so restarts remember it");

    // A later ordinary request clears the flag — the flag describes the
    // latest request, and a lane wrongly marked one is a session the
    // machine may sleep through.
    let response = post_messages(
        addr,
        "/v1/messages",
        &[],
        &tools_body("claude-opus-5", true),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    wait_for_rows(&store, 2).await;
    let lane = the_lane(wait_for_lanes(&store, 1).await);
    assert_eq!(lane.ping, None);
    assert_eq!(store.requests_since(0, 10).expect("rows")[1].ping, None);
}

#[tokio::test]
async fn lanes_reseed_on_restart_from_the_requests_table() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream, None, "anthropic_sub");
    let (addr, store) = spawn_toker(config.clone()).await;

    // Two responses in one lane: a 1h-tier write, then a 5m-tier one.
    for model in ["claude-opus-5", "5m-tier"] {
        let response = post_messages(addr, "/v1/messages", &[], &tools_body(model, true)).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    wait_for_rows(&store, 2).await;
    let lane = the_lane(wait_for_lanes(&store, 1).await);
    let live_prompt = lane.prompt_tokens;
    let live_ttl = lane.ttl;
    assert_eq!(live_ttl, Some(3_600_000), "sticky across both responses");

    // A brand-new Store handle over the same database sees the lane: the
    // table is durable, not in-process state.
    let reopened = Store::open(&config.db_path).expect("reopen store");
    assert_eq!(reopened.load_lanes().expect("lanes").len(), 1);

    // Lose the table entirely (the state a restart-before-flush could
    // lose in the predecessor) — a restart must rebuild it from the
    // ledger, exactly the
    // sessions that went quiet before it.
    {
        let conn = rusqlite::Connection::open(&config.db_path).expect("wipe connection");
        conn.execute("DELETE FROM lanes", []).expect("wipe lanes");
    }
    assert!(
        store.load_lanes().expect("lanes").is_empty(),
        "the wipe took"
    );

    // Restart: a new Server over the same database reseeds from the
    // requests table, last-wins by ts.
    let (_addr2, store2) = spawn_toker(config).await;
    let lanes = store2.load_lanes().expect("lanes");
    assert_eq!(lanes.len(), 1, "the lane is rebuilt, not invented");
    let lane = the_lane(lanes);
    assert_eq!(lane.session_id.as_deref(), Some("ccses-42"));
    assert_eq!(
        lane.tools_hash,
        store.load_lanes().expect("lanes")[0].tools_hash
    );
    assert_eq!(lane.prompt_tokens, live_prompt, "same per-row derivation");
    assert_eq!(lane.ttl, live_ttl, "the stickiness rule replays over rows");
    assert_eq!(lane.noticed_at, None);
    assert_eq!(mock.captured().len(), 2, "the restart forwarded nothing");
}

#[tokio::test]
async fn models_merge_endpoint_merges_served_models_and_moves_the_election() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    // The learned store as weeks of history would leave it: an incumbent
    // with eight days, a newcomer with one. The bar is min(7, 9/2) = 4.5,
    // so the newcomer's single day is a trial, not an adoption.
    let incumbent_days: Vec<String> = (0..8).map(|i| format!("2026-09-2{}", i)).collect();
    let incumbent_refs: Vec<&str> = incumbent_days.iter().map(String::as_str).collect();
    store
        .upsert_model(&toker::store::ModelEntry {
            model_id: "claude-opus-5".to_owned(),
            days_json: Some(json!(incumbent_refs)),
            max_prompt: Some(150_000),
            context_window_json: None,
        })
        .expect("upsert incumbent");
    store
        .upsert_model(&toker::store::ModelEntry {
            model_id: "claude-opus-5-5".to_owned(),
            days_json: Some(json!(["2026-10-01"])),
            max_prompt: Some(180_000),
            context_window_json: None,
        })
        .expect("upsert newcomer");

    // A merge for the incumbent changes nothing it did not already have,
    // and reports the family's target as it stands: the newcomer is below
    // the bar, so the incumbent holds the family.
    let response = client()
        .post(toker_url(addr, "/_toker/models/merge"))
        .header("x-toker-control", "models-merge")
        .header(header::CONTENT_TYPE, "application/json")
        .json(&json!({"model": "claude-opus-5", "days": [], "maxPrompt": 100}))
        .send()
        .await
        .expect("merge incumbent");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("body");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["merged"], json!(["claude-opus-5"]));
    assert_eq!(body["targets"], json!({"opus": "claude-opus-5"}));
    // Only adds: a LOWER maxPrompt cannot shrink the ceiling.
    assert_eq!(
        store
            .load_model("claude-opus-5")
            .expect("load")
            .expect("entry")
            .max_prompt,
        Some(150_000)
    );

    // The promotion: grant the newcomer days the log already has. The
    // union clears the bar and the election moves — the reply reports the
    // effect, not the intent.
    let granted = incumbent_days[..5].to_vec();
    let response = client()
        .post(toker_url(addr, "/_toker/models/merge"))
        .header("x-toker-control", "models-merge")
        .header(header::CONTENT_TYPE, "application/json")
        .json(&json!({"model": "claude-opus-5-5", "days": granted, "maxPrompt": 200_000}))
        .send()
        .await
        .expect("merge newcomer");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("body");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["targets"], json!({"opus": "claude-opus-5-5"}));
    let entry = store
        .load_model("claude-opus-5-5")
        .expect("load")
        .expect("entry");
    // Days union (1 + 5 granted = 6), the higher ceiling kept.
    assert_eq!(
        entry.days_json,
        Some(json!([
            "2026-09-20",
            "2026-09-21",
            "2026-09-22",
            "2026-09-23",
            "2026-09-24",
            "2026-10-01"
        ])),
        "days union, sorted and deduped"
    );
    assert_eq!(entry.max_prompt, Some(200_000));

    // Invented days: a date no model was served on is refused whole, so
    // the bar's denominator (every held day) cannot be enlarged through
    // the endpoint, and nothing in the grant is written.
    let response = client()
        .post(toker_url(addr, "/_toker/models/merge"))
        .header("x-toker-control", "models-merge")
        .header(header::CONTENT_TYPE, "application/json")
        .json(&json!({
            "model": "claude-opus-5-5",
            "days": ["2026-09-25", "2026-10-02", "2026-10-03"],
            "maxPrompt": 900_000,
        }))
        .send()
        .await
        .expect("merge invented days");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("body");
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"], json!("invented days"));
    assert_eq!(body["days"], json!(["2026-10-02", "2026-10-03"]));
    let after = store
        .load_model("claude-opus-5-5")
        .expect("load")
        .expect("entry");
    assert_eq!(after, entry, "a refused grant writes nothing");

    // Unknown model: 4xx naming the store's known identities — never a
    // silent refusal, never an invented entry.
    let response = client()
        .post(toker_url(addr, "/_toker/models/merge"))
        .header("x-toker-control", "models-merge")
        .header(header::CONTENT_TYPE, "application/json")
        .json(&json!({"model": "claude-fable-9", "days": ["2026-10-01"]}))
        .send()
        .await
        .expect("merge unknown");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("body");
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"], json!("unseen"));
    assert_eq!(body["known"], json!(["claude-opus-5", "claude-opus-5-5"]));
    assert_eq!(
        store.load_model("claude-fable-9").expect("load"),
        None,
        "a model the proxy never served is never invented"
    );
}

#[tokio::test]
async fn models_merge_endpoint_gates_and_validates_the_body() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, _store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    // Control-merge gating: a wrong verb, method, or
    // content type is 403 "not a control request" — the two halves (custom
    // header + JSON content type) are what keep a web page out.
    for headers in [
        vec![],
        vec![("x-toker-control", "status")],
        vec![("x-toker-control", "models-merge")],
    ] {
        let mut request = client().post(toker_url(addr, "/_toker/models/merge"));
        for (name, value) in &headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.expect("merge request");
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "headers {headers:?} do not pass the gate"
        );
    }

    // Right gate, unparseable store: the 400.
    let response = client()
        .post(toker_url(addr, "/_toker/models/merge"))
        .header("x-toker-control", "models-merge")
        .header(header::CONTENT_TYPE, "application/json")
        .body(b"not json"[..].to_vec())
        .send()
        .await
        .expect("merge request");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("body");
    assert_eq!(body["error"], json!("unparseable store"));

    // Parseable but not a valid entry (no days array): the same 400 —
    // The merge drops what does not validate rather than guessing.
    for body in [
        json!({"days": []}),
        json!({"model": "claude-opus-5", "days": "2026-10-01"}),
        json!({"model": "", "days": []}),
    ] {
        let response = client()
            .post(toker_url(addr, "/_toker/models/merge"))
            .header("x-toker-control", "models-merge")
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .expect("merge request");
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "body {body} is an unparseable store"
        );
    }
}

#[tokio::test]
async fn promote_hands_the_grant_to_a_running_server() {
    let (_mock, upstream) = spawn_mock().await;
    let config = test_config(upstream, None, "anthropic_sub");
    let db = config.db_path.clone();
    let (addr, store) = spawn_toker(config).await;
    let incumbent_days: Vec<String> = (0..9).map(|i| format!("2026-09-2{i}")).collect();
    let incumbent_refs: Vec<&str> = incumbent_days.iter().map(String::as_str).collect();
    store
        .upsert_model(&toker::store::ModelEntry {
            model_id: "claude-opus-5".to_owned(),
            days_json: Some(json!(incumbent_refs)),
            max_prompt: Some(480_000),
            context_window_json: None,
        })
        .expect("upsert incumbent");
    store
        .upsert_model(&toker::store::ModelEntry {
            model_id: "claude-opus-5-5".to_owned(),
            days_json: Some(json!(["2026-09-28"])),
            max_prompt: Some(4_000),
            context_window_json: None,
        })
        .expect("upsert newcomer");

    let mut out = Vec::new();
    toker::cmds::promote_run(
        &db,
        addr.port(),
        &toker::cmds::PromoteOpts {
            model: "claude-opus-5-5".to_owned(),
            days: None,
            max_prompt: None,
            dry_run: false,
        },
        &mut out,
    )
    .await
    .expect("promotes through the server");
    let report = String::from_utf8(out).expect("utf-8");
    assert!(
        report.contains("merged into the running server"),
        "{report}"
    );
    assert!(
        report.contains("opus now rewrites to claude-opus-5-5"),
        "{report}"
    );
    // The bar at nine active days is 4.5, so the grant is five days, all
    // of them held; the ceiling is the family's best.
    let entry = store
        .load_model("claude-opus-5-5")
        .expect("load")
        .expect("entry");
    assert_eq!(
        entry.days_json,
        Some(json!([
            "2026-09-24",
            "2026-09-25",
            "2026-09-26",
            "2026-09-27",
            "2026-09-28"
        ]))
    );
    assert_eq!(entry.max_prompt, Some(480_000));
}

/// A tooled, non-streaming body whose system prompt is long enough to cut
/// prefix rungs and a full tail, with `marker` 21 units before its end.
fn system_body(marker: &str) -> Vec<u8> {
    let system = format!("{}{marker}{}", "x".repeat(20_000), "y".repeat(20));
    serde_json::to_vec(&json!({
        "model": "claude-sonnet-5",
        "stream": false,
        "system": system,
        "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "Hi"}],
    }))
    .expect("serialise system body")
}

/// Send one system-prompt request and return the ledger's rows once its
/// row has landed.
async fn send_system(
    addr: SocketAddr,
    store: &Store,
    marker: &str,
    rows: usize,
) -> Vec<RequestRow> {
    let response = post_messages(addr, "/v1/messages", &[], &system_body(marker)).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.expect("body bytes");
    let landed = wait_for_rows(store, rows).await;
    assert_eq!(landed.len(), rows);
    landed
}

#[tokio::test]
async fn capture_localises_a_system_change_and_keeps_ladders_only_where_it_matters() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, None, "anthropic_sub")).await;

    // The lane's first row keeps its ladders, as the baseline, and claims
    // no change: there is nothing to compare with.
    let rows = send_system(addr, &store, "A", 1).await;
    assert!(rows[0].system_ladder.is_some() && rows[0].system_tail.is_some());
    assert_eq!(rows[0].system_change, None);

    // The same prompt again: no ladders, no change.
    let rows = send_system(addr, &store, "A", 2).await;
    assert_eq!(rows[1].system_ladder, None);
    assert_eq!(rows[1].system_tail, None);
    assert_eq!(rows[1].system_change, None);

    // A changed prompt: localised against the first row's rungs (its
    // predecessor dropped them), and its own ladders kept.
    let rows = send_system(addr, &store, "B", 3).await;
    assert!(rows[2].system_ladder.is_some() && rows[2].system_tail.is_some());
    insta::assert_snapshot!(
        rows[2].system_change.as_ref().expect("localised").to_string(),
        @r#"{"delta":0,"where":"block 0 (20021 → 20021 chars), 16-24 bytes from the end"}"#
    );
}

#[tokio::test]
async fn a_restarted_server_finds_the_lanes_baseline_in_the_ledger() {
    let (_mock, upstream) = spawn_mock().await;
    let config = test_config(upstream, None, "anthropic_sub");
    let (addr, store) = spawn_toker(config.clone()).await;
    send_system(addr, &store, "A", 1).await;
    send_system(addr, &store, "A", 2).await;

    // A new server over the same ledger: no in-memory state carries over,
    // and the change is still localised.
    let (addr, store) = spawn_toker(config).await;
    let rows = send_system(addr, &store, "B", 3).await;
    insta::assert_snapshot!(
        rows[2].system_change.as_ref().expect("localised").to_string(),
        @r#"{"delta":0,"where":"block 0 (20021 → 20021 chars), 16-24 bytes from the end"}"#
    );
}
