//! End-to-end proxy tests: a mock openrouter upstream (capturing every
//! request byte-for-byte) behind the real toker router, driven as a client.
//!
//! Asserts the unit's contract: usage requests and responses cross the
//! canonical Chat adapters, ledger rows carry the right buckets/cost kinds,
//! error rows are never priced, hangups abort the upstream and record nothing,
//! and the control endpoint stays gated and secret-free.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
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

use toker::config::{
    AnthropicApiConfig, AnthropicSubConfig, CodexSubConfig, Config, OpenRouterConfig,
};
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

/// A non-streaming completion carrying a 500k-token prompt — the
/// cold-gate tests' lane seeder: one recorded response with this shape
/// lands a lane holding 500,000 tokens, over the 175k cold bar.
fn big_non_stream_body(model: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": "gen-test-big",
        "provider": "z-ai",
        "model": model,
        "object": "chat.completion",
        "created": 1760000600,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "Done"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 500_000, "completion_tokens": 8, "total_tokens": 500_008},
    }))
    .expect("serialise big non-stream body")
}

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
        // The cold-gate tests' seeding model: a 500k-token response,
        // so the recorded request lands a lane over the cold bar.
        m if m.starts_with("big/") => raw_response(
            StatusCode::OK,
            "application/json",
            Bytes::from(big_non_stream_body(model)),
        ),
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
        "drop-mid" => {
            // First bytes, then the connection dies: a reset mid-body,
            // on the stream and the buffered shape alike.
            let first: &'static [u8] = if stream {
                b"data: {\"id\":\"gen-drop\",\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n"
            } else {
                br#"{"id":"gen-drop","choices":["#
            };
            let body = Body::from_stream(fail_after(first));
            let mut response = Response::new(body);
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                }),
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

/// A body that delivers `first`, then fails. The pause between them lets
/// the mock's hyper flush the headers and the first bytes; an error in
/// the same poll would abort the response before anything went out.
fn fail_after(first: &'static [u8]) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    futures::stream::unfold(0, move |step| async move {
        match step {
            0 => Some((Ok(Bytes::from_static(first)), 1)),
            1 => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Some((Err(std::io::Error::other("connection reset")), 2))
            }
            _ => None,
        }
    })
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

fn test_config(upstream: reqwest::Url, api_key_env: &str, api_key: Option<String>) -> Config {
    // The anthropic fields exist only so the Config literal compiles after
    // the anthropic unit grew it; no openai-path test touches an
    // anthropic route (the anthropic suite in server_anthropic.rs
    // exercises them).
    let anthropic_upstream: reqwest::Url =
        "https://api.anthropic.com".parse().expect("upstream url");
    Config {
        port: 0,
        db_path: PathBuf::from(":memory:"),
        session_header_names: vec!["x-toker-session".to_owned(), "x-session-id".to_owned()],
        ping_header_name: "x-toker-ping".to_owned(),
        default_backend_openai_chat: Some("openrouter".to_owned()),
        openrouter: Some(OpenRouterConfig {
            upstream,
            api_key_env: api_key_env.to_owned(),
            api_key_keyring: false,
            api_key,
            picker: None,
        }),
        default_backend_anthropic: Some("anthropic_sub".to_owned()),
        anthropic_sub: Some(AnthropicSubConfig {
            model_map: None,
            upstream: anthropic_upstream.clone(),
            claude_credentials_path: None,
            ..AnthropicSubConfig::default()
        }),
        anthropic_api: Some(AnthropicApiConfig {
            model_map: None,
            upstream: anthropic_upstream,
            api_key_env: api_key_env.to_owned(),
            api_key_keyring: false,
            api_key: None,
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
            auth_path: PathBuf::from("/nonexistent/toker-test-auth.json"),
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

/// [`spawn_toker`], with one fetched catalogue installed before serving —
/// the cold-gate tests' fixture. The background refresh task only spawns
/// in `serve`, which tests never run, so this is the only way a test's
/// server sees a catalogue.
async fn spawn_toker_cataloged(
    config: Config,
    source: &'static str,
    catalog: toker::catalog::FetchedCatalog,
) -> (SocketAddr, Arc<Store>) {
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    let server = Server::new(config, store.clone()).expect("build server");
    server.install_catalog(source, catalog);
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
        // opencode's native session id — the sticky-routing key that
        // rides upstream.
        .header("x-session-id", "ses-opencode-7f")
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

fn assert_complete_chat(body: &[u8], model: &str) {
    let value: Value = serde_json::from_slice(body).expect("canonical Chat response JSON");
    assert_eq!(value["model"], model);
    assert_eq!(value["provider"], "z-ai");
    assert_eq!(value["choices"][0]["message"]["content"], "Done");
    assert_eq!(value["choices"][0]["finish_reason"], "stop");
}

// ---------------------------------------------------------------------------
// The cold gate on the openai path
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_millis() as i64
}

/// A tools-carrying chat body, sized like the resume of the 500k
/// conversation [`seed_cold_lane`] seeds: the gate only stops a request
/// whose own upper bound (bytes / 2) could be that re-read. All the cold
/// tests drive the same session `post_chat` sends.
fn cold_body(model: &str, stream: bool) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "model": model,
        "stream": stream,
        "tools": [{"type": "function", "function": {"name": "run_command"}}],
        "messages": [
            {"role": "user", "content": "x".repeat(1_000_000)},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": "Hi"},
        ],
    }))
    .expect("serialise cold body")
}

/// The lane key a cold body keys to (`post_chat`'s session × the body's
/// tools hash).
fn cold_lane_key(body: &[u8]) -> String {
    let shape = IrRequest::parse(body).expect("parse").openai_chat().shape();
    format!("ses-test-1|{}", shape.tools_hash)
}

/// A fetched openrouter catalogue whose entries are raw listing objects
/// — the pricing fixture the writes-free exemption reads.
fn openrouter_catalog(entries: Vec<(&str, serde_json::Value)>) -> toker::catalog::FetchedCatalog {
    toker::catalog::FetchedCatalog {
        fetched_at_ms: 0,
        models: entries
            .into_iter()
            .map(|(id, raw)| toker::catalog::FetchedModel {
                id: id.to_owned(),
                context_window: Some(200_000),
                raw,
            })
            .collect(),
    }
}

/// Seed the lane this body keys with a 500k prompt — via a real recorded
/// openai response, so the lane derivation itself is under test — then
/// move its clock 11 minutes back: past openrouter's 10-minute sticky
/// window (the gate must fire), inside the anthropic hour tier (which
/// must not govern this path).
async fn seed_cold_lane(addr: SocketAddr, store: &Store, body: &[u8]) {
    let response = post_chat(addr, body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let rows = wait_for_rows(store, 1).await;
    assert_eq!(rows[0].kind, None, "the seeding response is a measurement");

    let mut lane = store
        .load_lane(&cold_lane_key(body))
        .expect("load lane")
        .expect("the recorded response grew the lane");
    assert_eq!(lane.prompt_tokens, Some(500_000), "the held total");
    assert_eq!(
        lane.ttl,
        Some(600_000),
        "openrouter's sticky window, not a tier guess"
    );
    lane.updated_ms = now_ms() - 11 * 60_000;
    store.upsert_lane(&lane).expect("poison the lane's clock");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_completions_cross_canonical_adapters_and_ledger() {
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

    let bytes = response.bytes().await.expect("sse bytes");
    let rendered = std::str::from_utf8(&bytes).expect("rendered SSE is UTF-8");
    assert!(rendered.contains("Hello"));
    assert!(rendered.contains("data: [DONE]"));
    assert!(rendered.contains(r#""provider":"z-ai""#));
    assert!(rendered.contains(r#""cost":0.000192"#));

    // The request is rendered deterministically through canonical IR.
    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].path, "/v1/chat/completions");
    let sent: Value = serde_json::from_slice(&captured[0].body).expect("rendered request");
    assert_eq!(sent["model"], "z-ai/glm-5.3");
    assert_eq!(sent["messages"][0]["content"][0]["text"], "Hi");
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
    assert!(
        captured[0]
            .headers
            .get("x-session-id")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("ses-opencode")),
        "the session id rides upstream: openrouter's sticky-routing key \
         (conversation → same endpoint → warm caches). It carries no secret."
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
            "cached_tokens": false, "cache_write_tokens": false,
            "reasoning_tokens": false, "cost": true,
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
    assert_eq!(row.drift_digest, None);
}

#[tokio::test]
async fn non_streaming_completions_buffer_observe_and_ledger() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let body = chat_body("z-ai/glm-5.3", false);
    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body bytes");
    assert_complete_chat(&bytes, "z-ai/glm-5.3");
    let sent: Value = serde_json::from_slice(&mock.captured()[0].body).expect("rendered request");
    assert_eq!(sent["messages"][0]["content"][0]["text"], "Hi");

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
async fn the_openrouter_prefix_routes_and_the_canonical_request_strips_it() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let body = chat_body("openrouter/z-ai/glm-5.3", false);
    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);

    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    let sent: Value = serde_json::from_slice(&captured[0].body).expect("rendered request");
    assert_eq!(sent["model"], "z-ai/glm-5.3");
    assert_eq!(sent["messages"][0]["content"][0]["text"], "Hi");
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
async fn non_2xx_renders_a_canonical_error_and_records_an_unpriced_error_row() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let body = chat_body("err-401", false);
    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = response.bytes().await.expect("error bytes");
    let error: Value = serde_json::from_slice(&bytes).expect("rendered error");
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert_eq!(error["error"]["message"], "No auth credentials found.");
    assert!(error["error"].get("code").is_none());
    let sent: Value = serde_json::from_slice(&mock.captured()[0].body).expect("rendered request");
    assert_eq!(sent["model"], "err-401");

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
async fn noncanonical_json_is_normalised_without_a_legacy_drift_row() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    // `\/` is legal JSON that deterministic canonical rendering normalises.
    let body = br#"{"model":"z-ai/glm-5.3","messages":[{"role":"user","content":"a\/b"}]}"#;
    let response = post_chat(addr, body).await;
    assert_eq!(response.status(), StatusCode::OK);

    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    let sent: Value = serde_json::from_slice(&captured[0].body).expect("rendered request");
    assert_eq!(sent["messages"][0]["content"][0]["text"], "a/b");
    assert_ne!(captured[0].body.as_ref(), body.as_slice());

    let rows = wait_for_rows(&store, 1).await;
    assert!(
        rows.iter()
            .all(|row| row.kind != Some(RowKind::FidelityDrift))
    );
    let measurement = &rows[0];
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
async fn known_chat_profile_projects_routable_models_locally() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream, UNSET_KEY_ENV, None);
    let listing = serde_json::json!({
        "data": [
            {"id": "invented/one", "created": 123, "context_length": 64000,
             "pricing": {"prompt": "0.000001"}},
            {"id": "invented/two"}
        ]
    });
    let catalog = toker::catalog::fetched::parse_openrouter(&listing, 1).expect("catalog");
    let (addr, store) = spawn_toker_cataloged(config, "openrouter", catalog).await;

    let response = client()
        .get(toker_url(addr, "/f/opencode/v1/models"))
        .send()
        .await
        .expect("models request");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("models JSON");
    let ids: Vec<_> = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|entry| entry["id"].as_str().expect("id"))
        .collect();
    assert_eq!(
        ids,
        [
            "invented/one",
            "invented/two",
            "openrouter/invented/one",
            "openrouter/invented/two"
        ]
    );
    assert_eq!(body["data"][0]["object"], "model");
    assert_eq!(body["data"][0]["owned_by"], "openrouter");
    assert!(body["data"][0].get("pricing").is_none());
    assert!(
        mock.captured().is_empty(),
        "discovery never reaches upstream"
    );
    assert_eq!(store.count_requests().expect("count"), 0);
}

#[tokio::test]
async fn missing_local_catalogue_is_unavailable_not_an_empty_model_list() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;
    let response = client()
        .get(toker_url(addr, "/f/opencode/v1/models"))
        .send()
        .await
        .expect("models request");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.expect("error JSON");
    assert_eq!(body["error"]["type"], "catalog_unavailable");
    assert!(mock.captured().is_empty());
    assert_eq!(store.count_requests().expect("count"), 0);
}

#[tokio::test]
async fn models_never_carries_an_anthropic_credential_to_openrouter() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, _store) = spawn_toker(test_config(
        upstream,
        UNSET_KEY_ENV,
        Some("sk-or-stored".to_owned()),
    ))
    .await;

    // Claude's OAuth bearer, and an Anthropic API key in its own header:
    // both are dropped, and the stored openrouter key takes the bearer's
    // place as if the request had carried none.
    let response = client()
        .get(toker_url(addr, "/v1/models"))
        .header(header::AUTHORIZATION, "Bearer sk-ant-oat01-claude-oauth")
        .header("x-api-key", "sk-ant-api03-key")
        .send()
        .await
        .expect("models request");
    assert_eq!(response.status(), StatusCode::OK);
    let captured = mock.captured();
    assert_eq!(
        captured[0]
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer sk-or-stored"),
        "the anthropic bearer is replaced by the stored key"
    );
    assert!(
        captured[0].headers.get("x-api-key").is_none(),
        "x-api-key never reaches openrouter"
    );

    // The frontend's own openrouter key still passes through verbatim.
    let response = client()
        .get(toker_url(addr, "/v1/models"))
        .header(header::AUTHORIZATION, "Bearer sk-or-v1-client")
        .send()
        .await
        .expect("models request");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        mock.captured()[1]
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer sk-or-v1-client")
    );
}

#[tokio::test]
async fn non_json_bodies_are_rejected_before_the_backend() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let response = post_chat(addr, b"not json at all").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(mock.captured().is_empty(), "the malformed body stays local");

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        store.count_requests().expect("count"),
        0,
        "no row for an unparseable body"
    );
}

#[tokio::test]
async fn unexpected_compression_is_rejected_and_unledgered() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let response = post_chat(addr, &chat_body("gzip-me", false)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response.headers().get(header::CONTENT_ENCODING).is_none());

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
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let response = post_chat(addr, &chat_body("drop-mid", true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let read = tokio::time::timeout(std::time::Duration::from_secs(10), read_to_end(response))
        .await
        .expect("the client response ends promptly");
    assert!(
        read.is_err(),
        "the client sees a transport error, not a clean end: {read:?}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(store.count_requests().expect("count"), 0);
}

#[tokio::test]
async fn a_truncated_buffered_body_answers_502_and_records_no_row() {
    let (_mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    let response = post_chat(addr, &chat_body("drop-mid", false)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let body = response.bytes().await.expect("the 502 itself is whole");
    assert!(
        !body.windows(8).any(|window| window == b"gen-drop"),
        "none of the truncated body is forwarded"
    );
    // OpenAI-shaped, the error object chat clients already parse.
    let error: serde_json::Value = serde_json::from_slice(&body).expect("a JSON error");
    assert_eq!(error["error"]["type"], "server_error");
    assert!(error["error"]["message"].is_string());
    assert!(error.get("type").is_none(), "not the anthropic envelope");

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
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
    // The status request itself is not an upstream exchange.
    assert_eq!(value["in_flight"], 0);

    // The merge endpoint is gated the same way: no verb → 403, and the
    // verb alone is not enough — a JSON content type is demanded too, so
    // a
    // browser cannot drive it without an unanswered preflight. The real
    // merge behaviour is the anthropic suite's (models are learned from
    // anthropic responses).
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
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn poisoned_meters_state_never_blocks_the_openai_path() {
    // The quota gate is anthropic_sub-only — today's sole meter source —
    // and the openai frontend routes to openrouter, so a poisoned
    // meters_state (a spent reading from another backend's snapshot) must
    // not stop a chat completion. The openai path never consults meters
    // at all: it forwards untouched and records normally.
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    // A spent 5-hour window with its reset comfortably in the future —
    // exactly the reading that blocks the anthropic_sub path.
    let now_secs = jiff::Timestamp::now().as_millisecond() / 1000;
    store
        .save_meters(
            "anthropic_sub",
            &toker::store::MetersSnapshot {
                updated_ms: jiff::Timestamp::now().as_millisecond(),
                snapshot: serde_json::json!({
                    "util5h": 1.0, "reset5h": now_secs + 3600,
                    "util7d": 0.2, "reset7d": now_secs + 5 * 86400,
                    "utilOverage": serde_json::Value::Null,
                    "resetOverage": serde_json::Value::Null,
                    "status": serde_json::Value::Null,
                    "status5h": serde_json::Value::Null,
                    "status7d": serde_json::Value::Null,
                    "statusOverage": serde_json::Value::Null,
                    "claim": serde_json::Value::Null,
                    "overageInUse": false,
                    "fallbackPct": serde_json::Value::Null,
                    "other": {},
                }),
            },
        )
        .expect("poison meters");

    let body = chat_body("z-ai/glm-5.3", false);
    let response = post_chat(addr, &body).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the gate never fires for the openai path"
    );
    let bytes = response.bytes().await.expect("body bytes");
    assert_complete_chat(&bytes, "z-ai/glm-5.3");
    assert_eq!(mock.captured().len(), 1, "the request forwarded");
    let sent: Value = serde_json::from_slice(&mock.captured()[0].body).expect("rendered request");
    assert_eq!(sent["model"], "z-ai/glm-5.3");

    // A normal measurement row, not a blocked one.
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(
        rows[0].kind, None,
        "a real API measurement, never a gate row"
    );
    assert_eq!(rows[0].provider.as_deref(), Some("openrouter"));
    assert_eq!(rows[0].input, Some(48));
}

#[tokio::test]
async fn the_openai_path_grows_lanes_on_the_openrouter_clock_not_learned_models() {
    // The phase-2 exclusion is reversed: a recorded openai response
    // grows the lane table (session × tools-hash, openrouter's sticky
    // 10-minute window as the TTL) — but still teaches the learned
    // model store nothing (that stays anthropic-path middleware, and
    // the openai wire has no host model map to feed).
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    // A session header, a ping header, and a tools-carrying body, so
    // the lane has everything to key on and the ping flag to record.
    let body = serde_json::to_vec(&serde_json::json!({
        "model": "z-ai/glm-5.3",
        "stream": false,
        "tools": [{"type": "function", "function": {"name": "run_command"}}],
        "messages": [{"role": "user", "content": "Hi"}],
    }))
    .expect("serialise tools body");
    let response = client()
        .post(toker_url(addr, "/v1/chat/completions"))
        .header("x-toker-session", "ses-openai-1")
        .header("x-toker-ping", "1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.clone())
        .send()
        .await
        .expect("chat request");
    assert_eq!(response.status(), StatusCode::OK);

    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].kind, None, "a real measurement, fully recorded");
    assert_eq!(rows[0].session_id.as_deref(), Some("ses-openai-1"));
    let lanes = store.load_lanes().expect("lanes");
    assert_eq!(lanes.len(), 1, "the openai path writes its lane");
    let lane = &lanes[0];
    assert_eq!(lane.session_id.as_deref(), Some("ses-openai-1"));
    assert!(lane.tools_hash.is_some());
    assert_eq!(
        lane.ttl,
        Some(600_000),
        "the lane runs on openrouter's sticky window"
    );
    assert_eq!(lane.ping, Some(true), "the ping header is recorded");
    assert_eq!(lane.prompt_tokens, Some(64), "input + read + writes held");
    assert!(
        store.load_models().expect("models").is_empty(),
        "the openai path teaches the learned store nothing"
    );
    assert_eq!(
        mock.captured().len(),
        1,
        "the request forwarded exactly once"
    );
}

#[tokio::test]
async fn a_cold_charged_writes_lane_gets_the_synthetic_turn_and_no_upstream() {
    let (mock, upstream) = spawn_mock().await;
    // The fetched catalogue with a PRICED entry: cache writes are
    // charged (the live gpt-5.6-sol figure), so the exemption does not
    // apply.
    let (addr, store) = spawn_toker_cataloged(
        test_config(upstream, UNSET_KEY_ENV, None),
        "openrouter",
        openrouter_catalog(vec![(
            "big/charged-model",
            serde_json::json!({
                "id": "big/charged-model",
                "pricing": {
                    "prompt": "0.00000125",
                    "completion": "0.00001",
                    "input_cache_read": "0.000000125",
                    "input_cache_write": "0.0000025"
                }
            }),
        )]),
    )
    .await;

    let body = cold_body("big/charged-model", false);
    seed_cold_lane(addr, &store, &body).await;
    assert_eq!(mock.captured().len(), 1, "only the seed went upstream");

    // The gated request: 200 with the synthetic JSON turn, never an
    // error status — and never forwarded.
    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/json")),
        "a non-stream request is answered with the JSON turn"
    );
    let bytes = response.bytes().await.expect("notice bytes");
    let value: Value = serde_json::from_slice(&bytes).expect("the notice is a chat completion");
    assert_eq!(value["id"], "chatcmpl-toker-cold");
    assert_eq!(value["model"], "big/charged-model");
    assert_eq!(value["choices"][0]["message"]["role"], "assistant");
    let text = value["choices"][0]["message"]["content"]
        .as_str()
        .expect("the notice text");
    // The measured idle and prompt size, the rest pinned by the
    // snapshot. The stamp is the server's wall clock in the system zone,
    // so it is filtered out.
    assert!(text.contains("11m"), "the measured idle: {text}");
    assert!(text.contains("500,000"), "the measured prompt: {text}");
    insta::with_settings!({filters => vec![(r" at \d{2}:\d{2}:", " at [HH:MM]:")]}, {
        insta::assert_snapshot!(text);
    });
    // The GFM alert is the unprefixed default, at the cold notice's
    // warning level.
    assert!(text.contains("> [!WARNING]"), "{text}");
    // This path never retargets a compaction, so the notice promises
    // no cheaper one; and openrouter bills the re-read rather than
    // metering it against a rate-limit window.
    assert!(!text.contains("The proxy would run it on"), "{text}");
    assert!(!text.contains("rate-limit window"), "{text}");
    assert_eq!(
        value["usage"],
        serde_json::json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}),
        "nothing reached upstream: the synthetic turn claims no tokens"
    );

    // Upstream was never hit for the gated request.
    assert_eq!(mock.captured().len(), 1, "the gate answered, not the API");

    // The cold row: the openai shape, the anthropic cold row's fields.
    let rows = wait_for_rows(&store, 2).await;
    let cold = rows
        .iter()
        .find(|row| row.kind == Some(RowKind::Cold))
        .expect("a cold row is recorded");
    assert_eq!(cold.frontend.as_deref(), Some("openai_chat"));
    assert_eq!(cold.provider.as_deref(), Some("openrouter"));
    assert_eq!(cold.route.as_deref(), Some("openai_chat:openrouter"));
    assert_eq!(cold.session_id.as_deref(), Some("ses-test-1"));
    let expected_tools = cold_lane_key(&body)
        .split_once('|')
        .expect("the lane key carries both halves")
        .1
        .to_owned();
    assert_eq!(cold.tools_hash.as_deref(), Some(expected_tools.as_str()));
    assert_eq!(cold.req_messages, Some(3));
    assert_eq!(cold.cold_on, Some(true));
    assert_eq!(cold.gate_on, None, "no quota gate exists on this path");
    assert_eq!(cold.rate_limits, None, "nothing reached upstream");
    let extra = cold.extra.as_ref().expect("the payload rides `extra`");
    assert!(
        (extra["idleMs"].as_i64().expect("idleMs") - 660_000).abs() < 60_000,
        "{extra}"
    );
    assert_eq!(extra["lastPrompt"], serde_json::json!(500_000));
    assert_eq!(extra["reqMessages"], serde_json::json!(3));
    assert_eq!(extra["compactTarget"], Value::Null);
    assert_eq!(extra["quotaExtra"], Value::Null, "no outlook on this path");
    assert_eq!(extra["util5h"], Value::Null);

    // The lane remembers it has spoken; `at` did not move.
    let lane = store
        .load_lane(&cold_lane_key(&body))
        .expect("load")
        .expect("the poisoned lane");
    assert!(
        lane.noticed_at
            .is_some_and(|noticed| noticed > lane.updated_ms)
    );
    assert_eq!(lane.prompt_tokens, Some(500_000));

    // The resend in the same idle spell forwards: sending the request
    // again IS the release — there is no marker on this wire.
    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body");
    assert_complete_chat(&bytes, "big/charged-model");
    assert_eq!(mock.captured().len(), 2);

    let rows = wait_for_rows(&store, 3).await;
    assert_eq!(
        rows.iter()
            .filter(|row| row.kind == Some(RowKind::Cold))
            .count(),
        1,
        "once per idle spell: no second notice row"
    );
    let measurements = rows.iter().filter(|row| row.kind.is_none()).count();
    assert_eq!(measurements, 2, "the seed and the resend both recorded");
}

#[tokio::test]
async fn a_writes_free_model_is_exempt_the_gate_forwards_and_records_cold_quiet() {
    let (mock, upstream) = spawn_mock().await;
    // The z-ai shape: the pricing object itemises prompt/completion/
    // cache-read and OMITS the write price — the documented free signal.
    let (addr, store) = spawn_toker_cataloged(
        test_config(upstream, UNSET_KEY_ENV, None),
        "openrouter",
        openrouter_catalog(vec![(
            "big/free-model",
            serde_json::json!({
                "id": "big/free-model",
                "pricing": {
                    "prompt": "0.00000011",
                    "completion": "0.00000043",
                    "input_cache_read": "0.0000000022"
                }
            }),
        )]),
    )
    .await;

    // The same cold 500k lane the charged test fires on.
    let body = cold_body("big/free-model", false);
    seed_cold_lane(addr, &store, &body).await;
    let poisoned_at = store
        .load_lane(&cold_lane_key(&body))
        .expect("load")
        .expect("lane")
        .updated_ms;

    // The request forwards normally: the re-read the notice would warn
    // about is free, so the interruption buys nothing.
    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body");
    assert_complete_chat(&bytes, "big/free-model");
    assert_eq!(mock.captured().len(), 2, "the request really went upstream");

    let rows = wait_for_rows(&store, 3).await;
    assert!(
        !rows.iter().any(|row| row.kind == Some(RowKind::Cold)),
        "no notice was given"
    );
    // The withheld notice is recorded, so a quiet spell reads as a
    // decision rather than a gate that stopped working.
    let quiet = rows
        .iter()
        .find(|row| row.kind == Some(RowKind::ColdQuiet))
        .expect("a cold-quiet row says why the notice was withheld");
    assert_eq!(quiet.frontend.as_deref(), Some("openai_chat"));
    assert_eq!(quiet.provider.as_deref(), Some("openrouter"));
    assert_eq!(quiet.session_id.as_deref(), Some("ses-test-1"));
    let extra = quiet.extra.as_ref().expect("the payload rides `extra`");
    assert_eq!(extra["writesFree"], serde_json::json!(true));
    assert_eq!(extra["lastPrompt"], serde_json::json!(500_000));
    assert_eq!(
        rows.iter().filter(|row| row.kind.is_none()).count(),
        2,
        "the seed and the exempted request both recorded"
    );
    // The lane was not marked noticed (nothing was said), and its clock
    // moved with the served response.
    let lane = store
        .load_lane(&cold_lane_key(&body))
        .expect("load")
        .expect("lane");
    assert_eq!(
        lane.noticed_at, None,
        "a skipped notice is not a spoken one"
    );
    assert!(
        lane.updated_ms > poisoned_at,
        "the served response re-touched the lane"
    );
}

#[tokio::test]
async fn a_summarising_request_on_a_cold_lane_forwards_without_a_notice() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;
    let body = cold_body("big/charged-model", false);
    seed_cold_lane(addr, &store, &body).await;

    // The same lane, now asking for the summary the notice would advise:
    // stopping it would halt the user a keystroke after telling them to
    // go ahead.
    let mut compaction: Value = serde_json::from_slice(&body).expect("cold body");
    compaction["messages"] = serde_json::json!([{
        "role": "user",
        "content": "Your task is to create a detailed summary of the conversation so far.",
    }]);
    let compaction = serde_json::to_vec(&compaction).expect("compaction body");
    assert_eq!(
        cold_lane_key(&compaction),
        cold_lane_key(&body),
        "same lane"
    );
    let response = post_chat(addr, &compaction).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body");
    assert_complete_chat(&bytes, "big/charged-model");
    assert_eq!(mock.captured().len(), 2);
    let rows = wait_for_rows(&store, 2).await;
    assert!(
        rows.iter().all(|row| row.kind.is_none()),
        "no cold row, quiet or otherwise: {rows:?}"
    );
    assert_eq!(rows[1].summarising, Some(true));
}

#[tokio::test]
async fn a_model_unknown_to_the_catalogue_fires_conservatively() {
    let (mock, upstream) = spawn_mock().await;
    // A catalogue that exists but does not carry the request's model:
    // unknown is never free (absence ≠ zero, invariant 3), so the gate
    // applies exactly as it did before the exemption existed.
    let (addr, store) = spawn_toker_cataloged(
        test_config(upstream, UNSET_KEY_ENV, None),
        "openrouter",
        openrouter_catalog(vec![(
            "big/some-other-model",
            serde_json::json!({"id": "big/some-other-model", "pricing": {"prompt": "0.000001"}}),
        )]),
    )
    .await;

    let body = cold_body("big/unknown-model", false);
    seed_cold_lane(addr, &store, &body).await;
    assert_eq!(mock.captured().len(), 1);

    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("notice bytes");
    let value: Value = serde_json::from_slice(&bytes).expect("the synthetic turn");
    assert_eq!(
        value["id"], "chatcmpl-toker-cold",
        "the gate fired for a model the catalogue does not know"
    );
    assert_eq!(mock.captured().len(), 1, "upstream was never hit");

    let rows = wait_for_rows(&store, 2).await;
    assert!(rows.iter().any(|row| row.kind == Some(RowKind::Cold)));
}

#[tokio::test]
async fn a_warm_openai_lane_forwards_without_a_notice() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker_cataloged(
        test_config(upstream, UNSET_KEY_ENV, None),
        "openrouter",
        openrouter_catalog(vec![(
            "big/charged-model",
            serde_json::json!({
                "id": "big/charged-model",
                "pricing": {"prompt": "0.00000125", "input_cache_write": "0.0000025"}
            }),
        )]),
    )
    .await;

    let body = cold_body("big/charged-model", false);
    // Seed WITHOUT moving the clock: the lane is 500k tokens but its
    // cache is live — idle ~0, well inside openrouter's 10-minute
    // window — so the notice must not fire (a false alarm costs the
    // user a turn).
    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    wait_for_rows(&store, 1).await;

    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body");
    assert_complete_chat(&bytes, "big/charged-model");
    assert_eq!(mock.captured().len(), 2);

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let rows = store.requests_since(0, 100).expect("rows");
    assert!(
        !rows
            .iter()
            .any(|row| matches!(row.kind, Some(RowKind::Cold | RowKind::ColdQuiet))),
        "no notice on a lane inside its sticky window"
    );
}

#[tokio::test]
async fn the_openai_notice_renders_as_sse_for_stream_requests() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker_cataloged(
        test_config(upstream, UNSET_KEY_ENV, None),
        "openrouter",
        openrouter_catalog(vec![(
            "big/charged-model",
            serde_json::json!({
                "id": "big/charged-model",
                "pricing": {"prompt": "0.00000125", "input_cache_write": "0.0000025"}
            }),
        )]),
    )
    .await;

    // Seed on the same lane (session and tools; the stream flag is not
    // part of the key), then ask to stream.
    let seed = cold_body("big/charged-model", false);
    seed_cold_lane(addr, &store, &seed).await;
    assert_eq!(mock.captured().len(), 1);

    let response = post_chat(addr, &cold_body("big/charged-model", true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/event-stream")),
        "a stream request is answered with the SSE turn"
    );
    let bytes = response.bytes().await.expect("notice bytes");
    let text = String::from_utf8_lossy(&bytes);
    // One content delta then the terminator, the captured-fixture
    // dialect — parse the chunk, then require [DONE] last.
    assert!(text.starts_with("data: {"), "{text}");
    assert!(text.ends_with("data: [DONE]\n"), "{text}");
    let chunk = text
        .strip_prefix("data: ")
        .and_then(|rest| rest.split("\n\n").next())
        .expect("the delta chunk");
    let chunk: Value = serde_json::from_str(chunk).expect("the chunk parses");
    assert_eq!(chunk["id"], "chatcmpl-toker-cold");
    assert_eq!(chunk["object"], "chat.completion.chunk");
    assert_eq!(chunk["choices"][0]["delta"]["role"], "assistant");
    assert!(
        chunk["choices"][0]["delta"]["content"]
            .as_str()
            .expect("the notice text")
            .contains("prompt cache expired after 11m idle")
    );
    assert_eq!(mock.captured().len(), 1, "upstream was never hit");

    let rows = wait_for_rows(&store, 2).await;
    assert!(rows.iter().any(|row| row.kind == Some(RowKind::Cold)));
}

// ---------------------------------------------------------------------------
// Optional backends
// ---------------------------------------------------------------------------

/// A catch-all upstream: every request, any path, is captured and
/// answered 200 — so a request that should never have left toker shows
/// up here whatever path it took.
async fn spawn_catch_all() -> (MockState, reqwest::Url) {
    let state = MockState::default();
    let app = Router::new()
        .fallback(mock_models)
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("mock binds");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock serves");
    });
    (state, format!("http://{addr}").parse().expect("mock url"))
}

#[tokio::test]
async fn a_protocol_with_no_backend_answers_not_configured_and_reaches_no_upstream() {
    let (mock, upstream) = spawn_catch_all().await;

    // Only the anthropic subscription block, like this machine's config:
    // the openai routes have no backend.
    let mut config = test_config(upstream.join("v1").expect("v1"), UNSET_KEY_ENV, None);
    config.openrouter = None;
    config.default_backend_openai_chat = None;
    config.anthropic_sub.as_mut().expect("enabled").upstream = upstream.clone();
    config.anthropic_api = None;
    config.codex_sub = None;
    let (addr, store) = spawn_toker(config).await;

    let response = post_chat(addr, &chat_body("z-ai/glm-5.3", false)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers()["x-toker-not-configured"],
        "openai_chat",
        "the probe-visible marker of toker's own answer"
    );
    let body: Value = response.json().await.expect("openai-shaped JSON");
    assert_eq!(body["error"]["code"], "backend_not_configured");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("openai_chat")),
        "{body}"
    );

    // A prefix naming a disabled anthropic backend is answered, never
    // sent to the default with the prefix still on.
    let response = client()
        .post(toker_url(addr, "/v1/messages"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"model":"anthropic_api/claude-opus-5","max_tokens":1,"messages":[]}"#)
        .send()
        .await
        .expect("messages request");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("anthropic-shaped JSON");
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "not_found_error");
    assert!(mock.captured().is_empty(), "nothing reached any upstream");

    // `/v1/models` is the anthropic default's when there is no openai
    // backend: claude asks for it too.
    let response = client()
        .get(toker_url(addr, "/v1/models"))
        .send()
        .await
        .expect("models request");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.captured().len(), 1);
    assert_eq!(mock.captured()[0].path, "/v1/models");

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(store.count_requests().expect("count"), 0, "no rows either");
}

#[tokio::test]
async fn no_anthropic_backend_answers_every_anthropic_path_not_configured() {
    let (mock, upstream) = spawn_catch_all().await;
    let mut config = test_config(upstream.join("v1").expect("v1"), UNSET_KEY_ENV, None);
    config.anthropic_sub = None;
    config.anthropic_api = None;
    config.codex_sub = None;
    config.default_backend_anthropic = None;
    let (addr, _store) = spawn_toker(config).await;

    let requests = [
        client()
            .post(toker_url(addr, "/v1/messages"))
            .body(r#"{"model":"claude-opus-5","max_tokens":1,"messages":[]}"#),
        client()
            .post(toker_url(addr, "/v1/messages/count_tokens"))
            .body("{}"),
        client().get(toker_url(addr, "/v1/messages/batches")),
        // The unmatched-path fallback forwards to the default anthropic
        // backend; with none, it answers too.
        client().get(toker_url(addr, "/v1/files")),
    ];
    for request in requests {
        let response = request.send().await.expect("request");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()["x-toker-not-configured"], "anthropic");
        let body: Value = response.json().await.expect("anthropic-shaped JSON");
        assert_eq!(body["error"]["type"], "not_found_error");
    }
    assert!(mock.captured().is_empty(), "nothing reached any upstream");
}

#[tokio::test]
async fn a_keyring_key_is_read_by_the_service_and_injected() {
    let (mock, upstream) = spawn_mock().await;
    let mut config = test_config(upstream, UNSET_KEY_ENV, None);
    config.openrouter.as_mut().expect("enabled").api_key_keyring = true;
    // Never the real keyring: a map holding the key.
    let secrets = Arc::new(toker::secrets::MemoryStore::new());
    toker::secrets::SecretStore::set(secrets.as_ref(), "openrouter", "sk-from-keyring")
        .expect("seed the fake keyring");
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    let server = Server::with_seams(
        config,
        store,
        Box::new(toker::middleware::awake::ProcessSpawner),
        secrets,
    )
    .expect("build server");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("toker binds");
    let addr = listener.local_addr().expect("toker addr");
    let app = server.router();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("toker serves");
    });

    let body = chat_body("z-ai/glm-5.3", false);
    assert_eq!(post_chat(addr, &body).await.status(), StatusCode::OK);
    assert_eq!(
        mock.captured()[0]
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer sk-from-keyring"),
    );
}

#[tokio::test]
async fn an_unavailable_keyring_reads_as_no_key() {
    let (mock, upstream) = spawn_mock().await;
    let mut config = test_config(upstream, UNSET_KEY_ENV, None);
    config.openrouter.as_mut().expect("enabled").api_key_keyring = true;
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    // The service still starts: a keyring failure costs the key, never
    // the listener.
    let server = Server::with_seams(
        config,
        store,
        Box::new(toker::middleware::awake::ProcessSpawner),
        Arc::new(toker::secrets::MemoryStore::unavailable()),
    )
    .expect("build server");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("toker binds");
    let addr = listener.local_addr().expect("toker addr");
    let app = server.router();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("toker serves");
    });

    let body = chat_body("z-ai/glm-5.3", false);
    assert_eq!(post_chat(addr, &body).await.status(), StatusCode::OK);
    assert!(
        mock.captured()[0]
            .headers
            .get(header::AUTHORIZATION)
            .is_none(),
        "no key, nothing injected"
    );
}

#[tokio::test]
async fn the_frontend_prefix_is_stripped_before_routing() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, _store) = spawn_toker(test_config(upstream, UNSET_KEY_ENV, None)).await;

    // toker's own endpoint under a prefix: what setup's verify probes
    // before pointing a frontend at that prefix.
    let response = client()
        .get(toker_url(addr, "/f/claude/_toker/status"))
        .header("x-toker-control", "status")
        .send()
        .await
        .expect("status request");
    assert_eq!(response.status(), StatusCode::OK);

    // A usage path under a prefix routes like the bare path, and the
    // prefix never reaches the upstream.
    let body = chat_body("z-ai/glm-5.3", false);
    let response = client()
        .post(toker_url(addr, "/f/opencode/v1/chat/completions"))
        .body(body)
        .send()
        .await
        .expect("chat request");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.captured()[0].path, "/v1/chat/completions");
}
