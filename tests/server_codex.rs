//! End-to-end codex-branch tests: a mock codex upstream (capturing every
//! request byte-for-byte, speaking the Responses SSE dialect) behind the
//! real toker router, driven through the ANTHROPIC frontend as a client.
//!
//! Asserts the branch's contract: the request the mock receives is a
//! translated Responses request (model mapped through the provider's
//! model map, instructions/input items/tools from the anthropic body,
//! codex identity headers, the codex bearer — never the frontend's —
//! and the session as the cache key), the client receives anthropic SSE
//! (message_start/tool_use blocks/usage) or an aggregated Message JSON,
//! rows land with the codex buckets (the conservative 1h cache-write
//! split, the usage object verbatim, cost NULL — never guessed), the
//! codex meters feed the per-backend slot, translation failures answer
//! without reaching the upstream, and HTTP/in-band errors map through
//! the translate error table.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use serde_json::{Value, json};

use toker::config::{
    AnthropicApiConfig, AnthropicSubConfig, CodexSubConfig, Config, OpenRouterConfig,
};
use toker::middleware::model_map;
use toker::server::Server;
use toker::store::{RequestRow, RowKind, Store};

/// An env name no test ever sets, so nothing resolves and nothing injects.
const UNSET_KEY_ENV: &str = "TOKER_TEST_KEY_UNSET_CODEX_9C";

// ---------------------------------------------------------------------------
// The mock codex upstream
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MockState {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl Default for MockState {
    fn default() -> Self {
        MockState {
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

fn fixture(name: &str) -> Bytes {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex_sse")
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

/// A realistic `x-codex-*` set on every mock response: primary/secondary
/// windows, credits, the limit name — the per-backend meter feed.
fn metered(response: &mut Response) {
    for (name, value) in [
        ("x-codex-primary-used-percent", "12.5"),
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-primary-reset-at", "1769500800"),
        ("x-codex-secondary-used-percent", "42.75"),
        ("x-codex-secondary-window-minutes", "10080"),
        ("x-codex-secondary-reset-at", "1769846400"),
        ("x-codex-limit-name", "gpt-5.6-sol"),
        ("x-codex-credits-has-credits", "true"),
        ("x-codex-credits-unlimited", "false"),
        ("x-codex-credits-balance", "5"),
    ] {
        response.headers_mut().insert(
            name.parse::<HeaderName>().expect("header name"),
            HeaderValue::from_static(value),
        );
    }
}

/// The shape the full header set parses to (the codex provider unit pins
/// the same value; here it proves what actually landed in the slot).
fn expected_meters() -> Value {
    json!({
        "primary": {
            "used_percent": 12.5,
            "window_minutes": 300,
            "resets_at": 1769500800,
        },
        "secondary": {
            "used_percent": 42.75,
            "window_minutes": 10080,
            "resets_at": 1769846400,
        },
        "limit_name": "gpt-5.6-sol",
        "credits": {
            "has_credits": true,
            "unlimited": false,
            "balance": "5",
        },
        "rate_limit_reached_type": Value::Null,
        "other": {},
    })
}

async fn mock_responses(State(mock): State<MockState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .expect("mock reads body");
    mock.requests.lock().unwrap().push(CapturedRequest {
        path: parts.uri.path().to_owned(),
        headers: parts.headers.clone(),
        body: body.clone(),
    });
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let model = json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let mut response = match model.as_str() {
        "gpt-5.6-sol" => raw_response(
            StatusCode::OK,
            "text/event-stream",
            fixture("01_tool_call_turn.sse"),
        ),
        "err-401" => raw_response(
            StatusCode::UNAUTHORIZED,
            "application/json",
            Bytes::from_static(
                br#"{"error":{"type":"authentication_error","message":"bad token"}}"#,
            ),
        ),
        "failed" => raw_response(
            StatusCode::OK,
            "text/event-stream",
            fixture("03_failed.sse"),
        ),
        other => {
            return raw_response(
                StatusCode::BAD_REQUEST,
                "application/json",
                Bytes::from(format!(
                    r#"{{"error":{{"code":"unknown_model","message":"{other}"}}}}"#
                )),
            );
        }
    };
    metered(&mut response);
    response
}

// ---------------------------------------------------------------------------
// The auth fixture: a login whose access token never needs a refresh
// during a test (exp far in the future), so no test ever hits the
// refresh endpoint.
// ---------------------------------------------------------------------------

fn b64url(payload: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
}

fn login_dir(tag: &str) -> PathBuf {
    let dir = test_dir(tag);
    let claims = json!({
        "exp": 4102444800u64,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acc_test_1",
            "chatgpt_plan_type": "pro",
            "chatgpt_account_is_fedramp": false,
        },
    });
    let jwt = format!(
        "{}.{}.sig-test",
        b64url(r#"{"alg":"RS256"}"#),
        b64url(&claims.to_string()),
    );
    let auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": jwt.clone(),
            "access_token": jwt,
            "refresh_token": "rt-test",
            "account_id": "acc_test_1",
        },
        "last_refresh": "2026-10-03T00:00:00Z",
    });
    std::fs::write(dir.join("auth.json"), auth.to_string()).expect("write auth fixture");
    dir
}

fn test_dir(tag: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp/opencode")
        .join("server-codex")
        .join(format!("{}-{}", std::process::id(), tag));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

// ---------------------------------------------------------------------------
// toker + config
// ---------------------------------------------------------------------------

fn test_config(
    tag: &str,
    codex_upstream: reqwest::Url,
    auth_path: PathBuf,
    family_map: bool,
) -> Config {
    let unused: reqwest::Url = "http://127.0.0.1:9".parse().expect("upstream url");
    Config {
        port: 0,
        db_path: test_dir(tag).join("toker.db"),
        session_header_names: vec![
            "x-toker-session".to_owned(),
            "x-claude-code-session-id".to_owned(),
            "x-session-id".to_owned(),
        ],
        ping_header_name: "x-toker-ping".to_owned(),
        default_backend_openai_chat: "openrouter".to_owned(),
        openrouter: OpenRouterConfig {
            upstream: "http://127.0.0.1:9/v1".parse().expect("url"),
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key: None,
        },
        default_backend_anthropic: "codex_sub".to_owned(),
        anthropic_sub: AnthropicSubConfig {
            model_map: None,
            upstream: unused.clone(),
        },
        anthropic_api: AnthropicApiConfig {
            model_map: None,
            upstream: unused,
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key: None,
        },
        codex_sub: CodexSubConfig {
            client_version: Some("0.154.0".to_owned()),
            version_probe: false,
            model_map: family_map.then(|| {
                model_map::parse_model_map(r#"{"family:opus":"gpt-5.6-sol"}"#)
                    .expect("valid map")
                    .expect("a map")
            }),
            upstream: codex_upstream,
            originator: "codex_cli_rs".to_owned(),
            auth_path,
            refresh_url: "https://auth.openai.com/oauth/token"
                .parse()
                .expect("refresh url"),
        },
        gates: toker::config::GatesConfig::default(),
        awake: false,
        transcript_roots: Vec::new(),
    }
}

async fn spawn_mock() -> (reqwest::Url, MockState) {
    let mock = MockState::default();
    let app = Router::new()
        .route("/backend-api/codex/responses", post(mock_responses))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("mock binds");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock serves");
    });
    (
        format!("http://{addr}/backend-api/codex")
            .parse()
            .expect("url"),
        mock,
    )
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

/// A claude-shaped streaming Messages request for `model`, with a system
/// prompt, tools, and one user turn.
fn messages_body(model: &str, stream: bool) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": model,
        "max_tokens": 1024,
        "stream": stream,
        "system": "You are a test.",
        "tools": [{
            "name": "read_file",
            "description": "Read a file",
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
        }],
        "messages": [{"role": "user", "content": "Read the config."}],
    }))
    .expect("serialise body")
}

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

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("client")
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_streaming_turn_translates_both_ways_and_records() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(
        "live",
        upstream,
        login_dir("live").join("auth.json"),
        true,
    ))
    .await;

    let response = client()
        .post(format!("http://{addr}/v1/messages"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("x-claude-code-session-id", "ses-codex-1")
        // The frontend's own (dummy) credential: must NEVER reach the
        // codex backend (pass-through-when-present's explicit opt-out).
        .header(header::AUTHORIZATION, "Bearer frontend-dummy")
        .body(messages_body("claude-opus-5", true))
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    let sse = response.text().await.expect("read sse");

    // The client sees anthropic events: the turn opens, text arrives, the
    // tool_use block rides (arguments whole), usage lands in message_delta.
    assert!(
        sse.contains("event: message_start"),
        "no message_start:\n{sse}"
    );
    assert!(sse.contains("event: content_block_delta"));
    assert!(
        sse.contains(r#""type":"tool_use""#),
        "no tool_use block:\n{sse}"
    );
    assert!(sse.contains(r#""stop_reason":"tool_use""#));
    assert!(sse.contains(r#""cache_read_input_tokens":512"#));
    assert!(
        sse.contains("event: message_stop"),
        "no message_stop:\n{sse}"
    );

    // The upstream saw a translated Responses request.
    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "exactly one upstream request");
    let request = &requests[0];
    assert_eq!(request.path, "/backend-api/codex/responses");
    let body: Value = serde_json::from_slice(&request.body).expect("codex body is json");
    assert_eq!(body["model"], "gpt-5.6-sol", "the family map applied");
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["instructions"], "You are a test.");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(body["input"][0]["role"], "user");
    assert_eq!(
        body["prompt_cache_key"], "ses-codex-1",
        "the session is the cache key"
    );

    // Headers: codex identity, session-keyed, and toker-signed auth —
    // never the frontend's bearer.
    let headers = &request.headers;
    assert_eq!(headers.get("originator").unwrap(), "codex_cli_rs");
    assert_eq!(headers.get("session-id").unwrap(), "ses-codex-1");
    assert_eq!(headers.get("thread-id").unwrap(), "ses-codex-1");
    assert!(headers.get("x-client-request-id").is_some());
    let bearer = headers.get(header::AUTHORIZATION).expect("codex bearer");
    assert!(
        bearer.to_str().unwrap().contains("sig-test"),
        "the codex login signs, not the frontend's: {bearer:?}"
    );
    assert_eq!(headers.get("chatgpt-account-id").unwrap(), "acc_test_1");

    // The row: the response's own slug is the model, the map's target is
    // the effective one, buckets follow the openai three-way split, the
    // write charges the 1h tier conservatively, cost stays NULL.
    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.kind, None, "a measurement");
    assert_eq!(row.frontend.as_deref(), Some("anthropic"));
    assert_eq!(row.provider.as_deref(), Some("codex_sub"));
    assert_eq!(row.model.as_deref(), Some("gpt-5.2-codex"));
    assert_eq!(row.requested_model.as_deref(), Some("claude-opus-5"));
    assert_eq!(row.effective_model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(row.input, Some(1234 - 512 - 64));
    assert_eq!(row.cache_read, Some(512));
    assert_eq!(row.cache_write_total, Some(64));
    assert_eq!(row.cache_write_1h, Some(64), "the conservative 1h charge");
    assert_eq!(
        row.ttl_split_known,
        Some(false),
        "the split is apportioned, not known"
    );
    assert_eq!(row.output, Some(210));
    assert_eq!(row.reasoning, Some(96));
    assert_eq!(row.cost_usd, None, "no honest price for a codex slug");
    assert_eq!(row.cost_kind, None);
    let usage_raw = row.usage_raw.as_deref().expect("usage verbatim");
    assert!(usage_raw.contains(r#""input_tokens":1234"#));
    assert!(usage_raw.contains(r#""cache_write_tokens":64"#));

    // The codex meters fed the per-backend slot from this response.
    let meters = store
        .load_meters("codex_sub")
        .expect("load")
        .expect("a snapshot exists");
    assert_eq!(meters.snapshot, expected_meters());
}

#[tokio::test]
async fn a_non_streaming_request_gets_an_aggregated_message() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(
        "ns",
        upstream,
        login_dir("ns").join("auth.json"),
        true,
    ))
    .await;

    let response = client()
        .post(format!("http://{addr}/v1/messages"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-claude-code-session-id", "ses-codex-ns")
        .body(messages_body("claude-opus-5", false))
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let message: Value =
        serde_json::from_str(&response.text().await.expect("read body")).expect("json message");
    assert_eq!(message["type"], "message");
    assert_eq!(message["stop_reason"], "tool_use");
    assert_eq!(message["usage"]["cache_read_input_tokens"], 512);
    // The upstream still saw a streaming Responses request (toker always
    // streams from codex and aggregates) — one request, one row.
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].kind, None);
}

#[tokio::test]
async fn an_untranslatable_body_never_reaches_the_upstream() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(
        "ut",
        upstream,
        login_dir("ut").join("auth.json"),
        true,
    ))
    .await;

    // An assistant-side image block: translate fails as UnsupportedBlock.
    let body = serde_json::to_vec(&json!({
        "model": "claude-opus-5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{
            "role": "assistant",
            "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}],
        }],
    }))
    .expect("serialise");
    let response = client()
        .post(format!("http://{addr}/v1/messages"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // The client asked to stream, so the error rides an SSE error event.
    let sse = response.text().await.expect("body");
    assert!(sse.contains("event: error"), "the SSE error shape: {sse}");
    assert!(
        sse.contains("invalid_request_error"),
        "the mapped type: {sse}"
    );
    assert!(
        sse.to_ascii_lowercase().contains("image"),
        "the error names the unsupported thing: {sse}"
    );
    assert!(
        mock.requests.lock().unwrap().is_empty(),
        "nothing forwarded"
    );
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].kind, Some(RowKind::Error));
    assert_eq!(rows[0].status, Some(400));
    assert_eq!(rows[0].error_type.as_deref(), Some("invalid_request_error"));
}

#[tokio::test]
async fn an_http_error_maps_through_the_error_table() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(
        "e401",
        upstream,
        login_dir("401").join("auth.json"),
        false,
    ))
    .await;

    // No family map: the bare model rides through untouched, and the
    // mock's err-401 arm answers the request for it.
    let response = client()
        .post(format!("http://{addr}/v1/messages"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(messages_body("err-401", false))
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let error: Value =
        serde_json::from_str(&response.text().await.expect("body")).expect("error json");
    assert_eq!(
        error["error"]["type"], "authentication_error",
        "mapped verbatim"
    );
    assert_eq!(error["error"]["message"], "bad token");
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].kind, Some(RowKind::Error));
    assert_eq!(rows[0].status, Some(401), "the real upstream status");
    assert_eq!(rows[0].error_type.as_deref(), Some("authentication_error"));
    // The meters still fed from the error response (every response feeds
    // them).
    assert!(store.load_meters("codex_sub").expect("load").is_some());
}

#[tokio::test]
async fn an_in_band_failure_maps_and_records_status_200() {
    // The mock stays bound for the test's lifetime (the upstream must
    // outlive the turn); its captured requests are not read here —
    // the row assertions are the point.
    let (upstream, _mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(
        "fail",
        upstream,
        login_dir("fail").join("auth.json"),
        false,
    ))
    .await;

    let response = client()
        .post(format!("http://{addr}/v1/messages"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(messages_body("failed", true))
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::OK);
    let sse = response.text().await.expect("read sse");
    assert!(
        sse.contains("event: error"),
        "the error event reaches the client:\n{sse}"
    );
    assert!(
        sse.contains("overloaded"),
        "the message passes through: {sse}"
    );

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.kind, Some(RowKind::Error));
    assert_eq!(row.status, Some(200), "the enclosing response was a 200");
    assert_eq!(
        row.error_type.as_deref(),
        Some("api_error"),
        "server_error maps to the generic"
    );
}

#[tokio::test]
async fn count_tokens_on_a_codex_route_answers_a_typed_error() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(
        "ct",
        upstream,
        login_dir("ct").join("auth.json"),
        false,
    ))
    .await;

    let response = client()
        .post(format!("http://{addr}/v1/messages/count_tokens"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(messages_body("claude-opus-5", false))
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let status = response.status();
    let text = response.text().await.expect("body");
    let error: Value = serde_json::from_str(&text)
        .unwrap_or_else(|_| panic!("error json, got status {status:?} body {text:?}"));
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert!(
        mock.requests.lock().unwrap().is_empty(),
        "nothing forwarded"
    );
    // No row: a toker-generated status is never a fabricated provider
    // measurement (the 502 rule).
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(store.requests_since(0, 100).expect("rows").is_empty());
}
