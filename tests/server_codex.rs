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

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
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

/// The first events of the tool-call fixture — whole events, everything
/// before the first completed item — with no terminator.
fn turn_opening() -> Bytes {
    let turn = fixture("01_tool_call_turn.sse");
    let cut = turn
        .windows(b"event: response.output_item.done".len())
        .position(|window| window == b"event: response.output_item.done")
        .expect("the fixture completes an item");
    turn.slice(..cut)
}

fn custom_tool_turn() -> Bytes {
    Bytes::from_static(
        br#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_probe","object":"response","status":"in_progress","model":"custom-tool-probe"}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_probe","role":"assistant","content":[],"status":"in_progress"}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":2,"item_id":"msg_probe","output_index":0,"content_index":0,"delta":"PROBE_TEXT"}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":3,"output_index":0,"item":{"type":"message","id":"msg_probe","role":"assistant","content":[{"type":"output_text","text":"PROBE_TEXT"}],"status":"completed"}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":4,"output_index":1,"item":{"type":"custom_tool_call","id":"ctc_probe","call_id":"call_probe","name":"exec","input":"PROBE_INPUT","status":"in_progress"}}

event: response.custom_tool_call_input.delta
data: {"type":"response.custom_tool_call_input.delta","sequence_number":5,"item_id":"ctc_probe","output_index":1,"delta":"PROBE_INPUT"}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":6,"output_index":1,"item":{"type":"custom_tool_call","id":"ctc_probe","call_id":"call_probe","name":"exec","input":"PROBE_INPUT","status":"completed"}}

event: response.completed
data: {"type":"response.completed","sequence_number":7,"response":{"id":"resp_probe","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2},"end_turn":false}}

"#,
    )
}

async fn mock_responses(State(mock): State<MockState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .expect("mock reads body");
    mock.requests.lock().unwrap().push(CapturedRequest {
        path: parts
            .uri
            .path_and_query()
            .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string),
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
        "custom-tool-probe" => {
            raw_response(StatusCode::OK, "text/event-stream", custom_tool_turn())
        }
        "err-401" => raw_response(
            StatusCode::UNAUTHORIZED,
            "application/json",
            Bytes::from_static(
                br#"{"error":{"type":"authentication_error","message":"bad token"}}"#,
            ),
        ),
        // The turn's opening events, then the connection dies (a reset
        // mid-turn) or closes cleanly before `response.completed`.
        "drop-mid" | "eof-mid" => {
            let opening = turn_opening();
            let reset = model == "drop-mid";
            let body = Body::from_stream(futures::stream::unfold(0, move |step| {
                let opening = opening.clone();
                async move {
                    match step {
                        0 => Some((Ok(opening), 1)),
                        // The pause lets the mock flush the headers and
                        // the opening, and toker its translation of it;
                        // an end in the same poll would abort before
                        // anything went out — a different failure.
                        1 => {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            reset.then(|| (Err(std::io::Error::other("connection reset")), 2))
                        }
                        _ => None,
                    }
                }
            }));
            let mut response = Response::new(body);
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            response
        }
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

async fn mock_models(State(mock): State<MockState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 1024)
        .await
        .expect("mock reads models body");
    mock.requests.lock().unwrap().push(CapturedRequest {
        path: parts
            .uri
            .path_and_query()
            .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string),
        headers: parts.headers,
        body,
    });
    let mut response = raw_response(
        StatusCode::OK,
        "application/json",
        Bytes::from_static(
            br#"{"models":[{"slug":"gpt-5.6-sol","context_window":272000,"max_context_window":872000}]}"#,
        ),
    );
    response
        .headers_mut()
        .insert(header::ETAG, HeaderValue::from_static("\"catalog-v1\""));
    response
}

async fn mock_anthropic_messages(State(mock): State<MockState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .expect("mock reads Messages body");
    let value: Value = serde_json::from_slice(&body).expect("Messages JSON");
    mock.requests.lock().unwrap().push(CapturedRequest {
        path: parts.uri.path().to_owned(),
        headers: parts.headers,
        body,
    });
    match value["model"].as_str().unwrap_or_default() {
        "error" => raw_response(
            StatusCode::UNAUTHORIZED,
            "application/json",
            Bytes::from_static(
                br#"{"type":"error","error":{"type":"authentication_error","message":"bad upstream key"}}"#,
            ),
        ),
        "stream" | "failed" | "eof" => {
            let mut body = concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_api\",\"usage\":{\"input_tokens\":7}}}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"shell\",\"input\":{}}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"pwd\\\"}\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            )
            .to_owned();
            if value["model"] == "failed" {
                body.push_str("event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n");
            } else if value["model"] == "stream" {
                body.push_str("event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":3}}\n\n");
                body.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
            }
            if value["model"] == "eof" {
                let opening = Bytes::from(body);
                let stream = futures::stream::unfold(0, move |step| {
                    let opening = opening.clone();
                    async move {
                        match step {
                            0 => Some((Ok::<_, std::io::Error>(opening), 1)),
                            1 => {
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                None
                            }
                            _ => None,
                        }
                    }
                });
                let mut response = Response::new(Body::from_stream(stream));
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                response
            } else {
                raw_response(StatusCode::OK, "text/event-stream", Bytes::from(body))
            }
        }
        _ => raw_response(
            StatusCode::OK,
            "application/json",
            Bytes::from_static(br#"{"id":"msg_api","type":"message","model":"claude-sonnet-5","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":3}}"#),
        ),
    }
}

async fn mock_openrouter_responses(State(mock): State<MockState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 64 * 1024 * 1024).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    mock.requests.lock().unwrap().push(CapturedRequest {
        path: parts.uri.path().to_owned(),
        headers: parts.headers,
        body,
    });
    if value["model"] == "bad" {
        return raw_response(
            StatusCode::TOO_MANY_REQUESTS,
            "application/json",
            Bytes::from_static(
                br#"{"error":{"code":"rate_limit_exceeded","message":"retry later"}}"#,
            ),
        );
    }
    if value["model"] == "numeric-error" {
        return raw_response(
            StatusCode::BAD_REQUEST,
            "application/json",
            Bytes::from_static(br#"{"error":{"code":400,"message":"invalid model"}}"#),
        );
    }
    if value["model"] == "numeric-failed" {
        return raw_response(
            StatusCode::OK,
            "text/event-stream",
            Bytes::from_static(br#"data: {"type":"response.failed","response":{"error":{"code":400,"message":"invalid streamed model"}}}

"#),
        );
    }
    if value["model"] == "failed" {
        return raw_response(
            StatusCode::OK,
            "text/event-stream",
            Bytes::from_static(br#"data: {"type":"response.failed","response":{"error":{"code":"server_error","message":"upstream failed"}}}

"#),
        );
    }
    if value["model"] == "cut" {
        return raw_response(
            StatusCode::OK,
            "text/event-stream",
            Bytes::from_static(
                br#"data: {"type":"response.created","response":{"id":"gen_cut"}}

"#,
            ),
        );
    }
    let mut body = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"gen_test\",\"model\":\"openai/gpt-4.1-mini\",\"status\":\"in_progress\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"echo\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"text\\\":\\\"hello\\\"}\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"echo\",\"arguments\":\"{\\\"text\\\":\\\"hello\\\"}\"}}\n\n",
    ).to_owned();
    let terminal = if value["model"] == "short" {
        "response.incomplete"
    } else {
        "response.completed"
    };
    body.push_str(&format!(
        "data: {{\"type\":\"{terminal}\",\"response\":{{\"id\":\"gen_test\",\"model\":\"openai/gpt-4.1-mini\",\"provider\":\"Example Router\",\"incomplete_details\":{{\"reason\":\"max_output_tokens\"}},\"usage\":{{\"input_tokens\":60,\"output_tokens\":6,\"total_tokens\":66,\"cost\":0.0000336}}}}}}\n\n"
    ));
    body.push_str("data: [DONE]\n\n");
    raw_response(StatusCode::OK, "text/event-stream", Bytes::from(body))
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

fn write_login(dir: &std::path::Path) {
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
}

struct TestConfig {
    config: Config,
    dir: common::TestDir,
}

impl std::ops::Deref for TestConfig {
    type Target = Config;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

impl std::ops::DerefMut for TestConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.config
    }
}

// ---------------------------------------------------------------------------
// toker + config
// ---------------------------------------------------------------------------

fn test_config(tag: &str, codex_upstream: reqwest::Url, family_map: bool) -> TestConfig {
    let dir = common::tempdir(&format!("server-codex-{tag}-"));
    write_login(&dir);
    let unused: reqwest::Url = "http://127.0.0.1:9".parse().expect("upstream url");
    let config = Config {
        port: 0,
        db_path: dir.join("toker.db"),
        session_header_names: vec![
            "x-toker-session".to_owned(),
            "x-claude-code-session-id".to_owned(),
            "x-session-id".to_owned(),
        ],
        ping_header_name: "x-toker-ping".to_owned(),
        default_backend_openai_chat: Some("openrouter".to_owned()),
        openrouter: Some(OpenRouterConfig {
            upstream: "http://127.0.0.1:9/v1".parse().expect("url"),
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key_keyring: false,
            api_key: None,
            picker: None,
        }),
        openai_api: None,
        default_backend_anthropic: Some("codex_sub".to_owned()),
        anthropic_sub: Some(AnthropicSubConfig {
            model_map: None,
            upstream: unused.clone(),
            claude_credentials_path: None,
            ..AnthropicSubConfig::default()
        }),
        anthropic_api: Some(AnthropicApiConfig {
            model_map: None,
            upstream: unused,
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key_keyring: false,
            api_key: None,
        }),
        codex_sub: Some(CodexSubConfig {
            client_version: Some("0.154.0".to_owned()),
            version_probe: false,
            model_map: family_map.then(|| {
                model_map::parse_model_map(r#"{"family:opus":"gpt-5.6-sol"}"#)
                    .expect("valid map")
                    .expect("a map")
            }),
            upstream: codex_upstream,
            originator: "codex_cli_rs".to_owned(),
            auth_path: dir.join("auth.json"),
            refresh_url: "https://auth.openai.com/oauth/token"
                .parse()
                .expect("refresh url"),
        }),
        gates: toker::config::GatesConfig::default(),
        notices: toker::config::NoticesConfig::default(),
        awake: false,
        transcript_roots: Vec::new(),
    };
    TestConfig { config, dir }
}

async fn spawn_mock() -> (reqwest::Url, MockState) {
    let mock = MockState::default();
    let app = Router::new()
        .route("/backend-api/codex/responses", post(mock_responses))
        .route("/backend-api/codex/models", get(mock_models))
        .route("/v1/messages", post(mock_anthropic_messages))
        .route("/v1/responses", post(mock_openrouter_responses))
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

fn anthropic_responses_config(tag: &str, upstream: reqwest::Url) -> TestConfig {
    let mut fixture = test_config(tag, upstream.clone(), false);
    fixture.config.anthropic_api.as_mut().unwrap().upstream = upstream
        .origin()
        .ascii_serialization()
        .parse()
        .expect("Anthropic mock URL");
    fixture.config.anthropic_api.as_mut().unwrap().api_key = Some("sk-ant-test".to_owned());
    fixture
}

fn subscription_responses_config(
    tag: &str,
    upstream: reqwest::Url,
    held_token: Option<&str>,
) -> TestConfig {
    let mut fixture = test_config(tag, upstream.clone(), false);
    let login_path = fixture.dir.join("claude-credentials.json");
    std::fs::write(
        &login_path,
        r#"{"claudeAiOauth":{"accessToken":"local-token","expiresAt":9999999999999}}"#,
    )
    .expect("write isolated Claude login");
    let sub = fixture.config.anthropic_sub.as_mut().unwrap();
    sub.upstream = upstream.origin().ascii_serialization().parse().unwrap();
    sub.oauth_token_env = UNSET_KEY_ENV.to_owned();
    sub.oauth_token = held_token.map(str::to_owned);
    sub.claude_credentials_path = Some(login_path);
    fixture
}

fn openrouter_responses_config(tag: &str, upstream: reqwest::Url) -> TestConfig {
    let mut fixture = test_config(tag, upstream.clone(), false);
    let router = fixture.config.openrouter.as_mut().unwrap();
    router.upstream = format!("{}/v1", upstream.origin().ascii_serialization())
        .parse()
        .unwrap();
    router.api_key = Some("sk-or-test".to_owned());
    fixture
}

async fn spawn_toker(fixture: TestConfig) -> (SocketAddr, Arc<Store>) {
    let TestConfig { config, dir } = fixture;
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    let server = Server::new(config, store.clone()).expect("build server");
    let app = server.router();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("toker binds");
    let addr = listener.local_addr().expect("toker addr");
    tokio::spawn(async move {
        let _dir = dir;
        axum::serve(listener, app).await.expect("toker serves");
    });
    (addr, store)
}

async fn spawn_toker_cataloged(fixture: TestConfig) -> (SocketAddr, Arc<Store>) {
    let TestConfig { config, dir } = fixture;
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    let server = Server::new(config, store.clone()).expect("build server");
    let raw = json!({
        "models": [{
            "slug": "gpt-5.6-sol",
            "context_window": 272000,
            "max_context_window": 872000
        }]
    });
    server.install_catalog(
        "codex_sub",
        toker::catalog::fetched::parse_codex(&raw, 1).expect("catalog"),
    );
    let app = server.router();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("toker binds");
    let addr = listener.local_addr().expect("toker addr");
    tokio::spawn(async move {
        let _dir = dir;
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
async fn codex_models_projects_the_local_catalogue_without_upstream_fetch() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, _store) = spawn_toker_cataloged(test_config("models", upstream, false)).await;

    let response = client()
        .get(format!("http://{addr}/f/codex/v1/models"))
        .header(header::AUTHORIZATION, "Bearer frontend-must-not-pass")
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("catalogue JSON");
    assert_eq!(body["models"][0]["slug"], "codex_sub/gpt-5.6-sol");
    assert_eq!(body["models"][1]["slug"], "gpt-5.6-sol");
    assert_eq!(body["models"][0]["max_context_window"], 872000);
    assert!(mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn responses_traverses_canonical_ir_and_records_as_codex() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config("native", upstream, false)).await;
    let body = serde_json::to_vec(&json!({
        "model": "codex_sub/gpt-5.6-sol",
        "prompt_cache_key": "codex-session-1",
        "instructions": "private instructions",
        "input": [
            {"role": "user", "content": "private prompt"},
            {"type": "function_call", "name": "inspect_image", "call_id": "call-1",
             "arguments": "{\"path\":\"shot.png\"}"},
            {"type": "function_call_output", "call_id": "call-1", "output": [
                {"type": "input_text", "text": "Image read successfully"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAECAw==",
                 "detail": "auto"}
            ]},
            {"type": "additional_tools", "id": "at_1", "role": "developer",
             "tools": [{"type": "custom", "name": "exec"}]},
            {"type": "future_provider_item", "opaque": true}
        ],
        "tools": [{"type": "function", "name": "shell", "parameters": {}, "strict": true}],
        "reasoning": {"effort": "xhigh", "summary": "auto"},
        "parallel_tool_calls": true,
        "store": true,
        "include": ["reasoning.encrypted_content", "message.output_text.logprobs"],
        "service_tier": "flex",
        "stream": true,
    }))
    .expect("request body");

    let response = client()
        .post(format!("http://{addr}/f/codex/v1/responses"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, "Bearer frontend-must-not-pass")
        .header("thread-id", "thread-native")
        .header("x-client-request-id", "request-native")
        .body(body.clone())
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::OK);
    let response_bytes = response.bytes().await.expect("response bytes");
    let response_text = String::from_utf8(response_bytes.to_vec()).expect("Responses SSE is UTF-8");
    assert!(response_text.contains("event: response.created"));
    assert!(response_text.contains("event: response.output_text.delta"));
    assert!(response_text.contains("event: response.completed"));
    assert!(response_text.contains(r#""id":"rs_1""#));
    assert!(response_text.contains(r#""encrypted_content":"opaque-encrypted-reasoning""#));

    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/backend-api/codex/responses");
    assert_ne!(
        requests[0].body, body,
        "the canonical request was re-rendered"
    );
    let rendered: Value = serde_json::from_slice(&requests[0].body).expect("rendered request JSON");
    assert_eq!(rendered["model"], "gpt-5.6-sol");
    assert_eq!(rendered["instructions"], "private instructions");
    assert_eq!(rendered["input"][0]["content"][0]["text"], "private prompt");
    assert_eq!(
        rendered["input"][1],
        json!({
            "type": "function_call", "name": "inspect_image", "call_id": "call-1",
            "arguments": "{\"path\":\"shot.png\"}"
        })
    );
    assert_eq!(
        rendered["input"][2],
        json!({
            "type": "function_call_output", "call_id": "call-1", "output": [
                {"type": "input_text", "text": "Image read successfully"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAECAw==",
                 "detail": "auto"}
            ]
        })
    );
    assert_eq!(
        rendered["input"][3],
        json!({
            "type": "additional_tools", "id": "at_1", "role": "developer",
            "tools": [{"type": "custom", "name": "exec"}]
        })
    );
    assert_eq!(
        rendered["input"][4],
        json!({"type": "future_provider_item", "opaque": true})
    );
    assert_eq!(rendered["tools"][0]["name"], "shell");
    assert_eq!(rendered["tools"][0]["strict"], true);
    assert_eq!(rendered["reasoning"]["effort"], "xhigh");
    assert_eq!(rendered["reasoning"]["summary"], "auto");
    assert_eq!(rendered["parallel_tool_calls"], true);
    assert_eq!(rendered["store"], true);
    assert_eq!(rendered["include"][1], "message.output_text.logprobs");
    assert_eq!(rendered["service_tier"], "flex");
    assert_eq!(rendered["prompt_cache_key"], "codex-session-1");
    assert_eq!(
        requests[0].headers.get("session-id").unwrap(),
        "codex-session-1"
    );
    assert_eq!(
        requests[0].headers.get("thread-id").unwrap(),
        "thread-native"
    );
    assert_eq!(
        requests[0].headers.get("x-client-request-id").unwrap(),
        "request-native"
    );
    assert_ne!(
        requests[0].headers.get(header::AUTHORIZATION).unwrap(),
        "Bearer frontend-must-not-pass"
    );

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.frontend.as_deref(), Some("openai_responses"));
    assert_eq!(row.route.as_deref(), Some("openai_responses:codex_sub"));
    assert_eq!(row.provider.as_deref(), Some("codex_sub"));
    assert_eq!(row.session_id.as_deref(), Some("codex-session-1"));
    assert_eq!(
        row.requested_model.as_deref(),
        Some("codex_sub/gpt-5.6-sol")
    );
    assert_eq!(row.effective_model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(row.req_messages, Some(5));
    assert_eq!(row.req_tools, Some(1));
    assert_eq!(row.system_chars, Some(20));
    assert_eq!(row.input, Some(658));
    assert_eq!(row.cache_read, Some(512));
    assert_eq!(row.cache_write_total, Some(64));
    assert_eq!(row.cache_write_1h, Some(64));
    assert_eq!(row.output, Some(210));
    assert_eq!(row.reasoning, Some(96));
    let summary = store
        .session_summary("codex-session-1")
        .expect("session summary");
    assert_eq!(summary.input, Some(658));
    assert_eq!(summary.cache_read, Some(512));
    assert_eq!(summary.cache_write_total, Some(64));
    assert_eq!(summary.output, Some(210));
    assert_eq!(
        row.extra.as_ref().and_then(|extra| extra.get("frontend")),
        Some(&json!("codex"))
    );
}

#[tokio::test]
async fn native_responses_replays_provider_owned_custom_tool_calls() {
    let (upstream, _mock) = spawn_mock().await;
    let (addr, _store) = spawn_toker(test_config("native-custom-tool", upstream, false)).await;
    let request = json!({
        "model": "codex_sub/custom-tool-probe",
        "prompt_cache_key": "custom-tool-session",
        "instructions": "PROBE_INSTRUCTIONS",
        "input": [{"role": "user", "content": "PROBE_REQUEST"}],
        "tools": [{
            "type": "custom", "name": "exec", "description": "PROBE_TOOL",
            "format": {"type": "text"}
        }],
        "stream": true
    });
    let response = client()
        .post(format!("http://{addr}/f/codex/v1/responses"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, "Bearer frontend-must-not-pass")
        .json(&request)
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.expect("Responses SSE");
    assert!(body.contains(r#""type":"custom_tool_call""#));
    assert!(body.contains(r#""type":"response.custom_tool_call_input.delta""#));
    assert!(body.contains(r#""input":"PROBE_INPUT""#));
    assert!(body.contains(r#""end_turn":false"#));
    assert!(!body.contains(r#""end_turn":true"#));

    let mut complete_request = request;
    complete_request["stream"] = json!(false);
    let response = client()
        .post(format!("http://{addr}/f/codex/v1/responses"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, "Bearer frontend-must-not-pass")
        .json(&complete_request)
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("complete Responses JSON");
    assert_eq!(body["end_turn"], false);
    assert_eq!(body["output"][0]["content"][0]["text"], "PROBE_TEXT");
    assert_eq!(body["output"][1]["type"], "custom_tool_call");
    assert_eq!(body["output"][1]["input"], "PROBE_INPUT");
}

#[tokio::test]
async fn responses_to_anthropic_api_render_json_and_replace_foreign_auth() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(anthropic_responses_config("api-json", upstream)).await;
    let response = client()
        .post(format!("http://{addr}/v1/responses"))
        .header(header::AUTHORIZATION, "Bearer codex-foreign-token")
        .json(&json!({
            "model": "anthropic_api/claude-sonnet-5",
            "input": [{"role": "user", "content": "hello"}],
            "max_output_tokens": 128,
            "stream": false,
        }))
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("Responses JSON");
    assert_eq!(body["output"][0]["content"][0]["text"], "hello");
    assert_eq!(body["usage"]["input_tokens"], 7);

    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/messages");
    assert!(requests[0].headers.get(header::AUTHORIZATION).is_none());
    assert_eq!(requests[0].headers.get("x-api-key").unwrap(), "sk-ant-test");
    assert_eq!(
        requests[0].headers.get("anthropic-version").unwrap(),
        "2023-06-01"
    );
    let rendered: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(rendered["model"], "claude-sonnet-5");
    assert_eq!(rendered["max_tokens"], 128);
    assert_eq!(rendered["messages"][0]["content"][0]["text"], "hello");
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(
        rows[0].route.as_deref(),
        Some("openai_responses:anthropic_api")
    );
    assert_eq!(rows[0].provider.as_deref(), Some("anthropic_api"));
}

#[tokio::test]
async fn responses_to_anthropic_api_stream_tools_errors_and_eof() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(anthropic_responses_config("api-stream", upstream)).await;
    for (model, expected) in [
        ("stream", "event: response.completed"),
        ("failed", "event: response.failed"),
    ] {
        let response = client()
            .post(format!("http://{addr}/v1/responses"))
            .json(&json!({
                "model": format!("anthropic_api/{model}"),
                "input": "hello",
                "max_output_tokens": 128,
                "stream": true,
            }))
            .send()
            .await
            .expect("toker answers");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.expect("Responses SSE");
        assert!(body.contains(expected), "{body}");
        assert!(body.contains("response.output_item.done"), "{body}");
    }
    assert_eq!(
        wait_for_rows(&store, 1).await.len(),
        1,
        "failed stream unledgered"
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 2);

    let eof = client()
        .post(format!("http://{addr}/v1/responses"))
        .json(&json!({
            "model": "anthropic_api/eof",
            "input": "hello",
            "max_output_tokens": 128,
            "stream": true,
        }))
        .send()
        .await
        .expect("stream begins");
    assert_eq!(eof.status(), StatusCode::OK);
    assert!(
        eof.bytes().await.is_err(),
        "a premature close aborts the response"
    );
    assert_eq!(
        wait_for_rows(&store, 1).await.len(),
        1,
        "truncated stream unledgered"
    );
}

#[tokio::test]
async fn responses_to_anthropic_api_reject_missing_limit_without_upstream() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(anthropic_responses_config("api-limit", upstream)).await;
    let response = client()
        .post(format!("http://{addr}/v1/responses"))
        .json(&json!({"model":"anthropic_api/claude-sonnet-5", "input":"hello", "stream":false}))
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.text().await.unwrap().contains("max_output_tokens"));
    assert!(mock.requests.lock().unwrap().is_empty());
    assert!(wait_for_rows(&store, 0).await.is_empty());
}

#[tokio::test]
async fn responses_to_anthropic_subscription_prefers_held_token_then_local_login() {
    let (upstream, mock) = spawn_mock().await;
    for (tag, held) in [("sub-held", Some("held-token")), ("sub-login", None)] {
        let (addr, store) =
            spawn_toker(subscription_responses_config(tag, upstream.clone(), held)).await;
        let response = client()
            .post(format!("http://{addr}/v1/responses"))
            .header(header::AUTHORIZATION, "Bearer codex-foreign-token")
            .json(&json!({
                "model": "anthropic_sub/claude-sonnet-5",
                "input": "hello",
                "max_output_tokens": 128,
                "stream": false,
            }))
            .send()
            .await
            .expect("toker answers");
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response.bytes().await.unwrap();
        assert_eq!(
            wait_for_rows(&store, 1).await[0].route.as_deref(),
            Some("openai_responses:anthropic_sub")
        );
    }
    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    for (request, expected) in requests
        .iter()
        .zip(["Bearer held-token", "Bearer local-token"])
    {
        assert_eq!(request.path, "/v1/messages");
        assert_eq!(
            request.headers.get(header::AUTHORIZATION).unwrap(),
            expected
        );
        assert_eq!(
            request.headers.get("anthropic-beta").unwrap(),
            "oauth-2025-04-20"
        );
        assert!(request.headers.get("x-api-key").is_none());
    }
}

#[tokio::test]
async fn foreign_subscription_route_without_credential_stops_locally() {
    let (upstream, mock) = spawn_mock().await;
    let mut fixture = subscription_responses_config("sub-no-token", upstream, None);
    fixture
        .config
        .anthropic_sub
        .as_mut()
        .unwrap()
        .claude_credentials_path = None;
    let (addr, store) = spawn_toker(fixture).await;
    let response = client()
        .post(format!("http://{addr}/v1/responses"))
        .header(header::AUTHORIZATION, "Bearer foreign-token")
        .json(&json!({
            "model":"anthropic_sub/claude-sonnet-5",
            "input":"hello",
            "max_output_tokens":64,
            "stream":false,
        }))
        .send()
        .await
        .expect("local answer");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(mock.requests.lock().unwrap().is_empty());
    assert!(wait_for_rows(&store, 0).await.is_empty());
}

#[tokio::test]
async fn chat_to_anthropic_bindings_renders_complete_and_streaming_turns() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(anthropic_responses_config("chat-api", upstream.clone())).await;
    let complete = client()
        .post(format!("http://{addr}/v1/chat/completions"))
        .header(header::AUTHORIZATION, "Bearer foreign-chat-token")
        .json(&json!({
            "model":"anthropic_api/claude-sonnet-5",
            "messages":[{"role":"user","content":"hello"}],
            "max_tokens":128,
        }))
        .send()
        .await
        .expect("chat request answers");
    assert_eq!(complete.status(), StatusCode::OK);
    let body: Value = complete.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "hello");

    let (sub_addr, sub_store) = spawn_toker(subscription_responses_config(
        "chat-sub",
        upstream,
        Some("held-token"),
    ))
    .await;
    let streaming = client()
        .post(format!("http://{sub_addr}/v1/chat/completions"))
        .json(&json!({
            "model":"anthropic_sub/stream",
            "messages":[{"role":"user","content":"hello"}],
            "max_completion_tokens":128,
            "stream":true,
        }))
        .send()
        .await
        .expect("chat stream answers");
    assert_eq!(streaming.status(), StatusCode::OK);
    let sse = streaming.text().await.unwrap();
    assert!(sse.contains("tool_calls"), "{sse}");
    assert!(sse.contains("[DONE]"), "{sse}");

    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.path, "/v1/messages");
        let rendered: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(rendered["max_tokens"], 128);
    }
    assert_eq!(requests[0].headers.get("x-api-key").unwrap(), "sk-ant-test");
    assert!(requests[0].headers.get(header::AUTHORIZATION).is_none());
    assert_eq!(
        requests[1].headers.get(header::AUTHORIZATION).unwrap(),
        "Bearer held-token"
    );
    assert_eq!(
        wait_for_rows(&store, 1).await[0].route.as_deref(),
        Some("openai_chat:anthropic_api")
    );
    assert_eq!(
        wait_for_rows(&sub_store, 1).await[0].route.as_deref(),
        Some("openai_chat:anthropic_sub")
    );
}

#[tokio::test]
async fn explicit_anthropic_chat_route_needs_no_chat_default() {
    let (upstream, mock) = spawn_mock().await;
    let mut fixture = anthropic_responses_config("chat-explicit", upstream);
    fixture.config.openrouter = None;
    fixture.config.default_backend_openai_chat = None;
    fixture.config.codex_sub = None;
    fixture.config.default_backend_anthropic = Some("anthropic_api".to_owned());
    let (addr, _) = spawn_toker(fixture).await;
    let response = client()
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&json!({
            "model":"anthropic_api/claude-sonnet-5",
            "messages":[{"role":"user","content":"hello"}],
            "max_tokens":64,
        }))
        .send()
        .await
        .expect("chat answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn openrouter_responses_streams_tools_and_records_attested_cost() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) =
        spawn_toker(openrouter_responses_config("router-responses", upstream)).await;
    let response = client()
        .post(format!("http://{addr}/f/codex/v1/responses"))
        .header(header::AUTHORIZATION, "Bearer foreign-codex-token")
        .json(&json!({
            "model":"openrouter/openai/gpt-4.1-mini",
            "instructions":"Use the provided tool.",
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Call echo."}]}],
            "tools":[{"type":"function","name":"echo","description":"Echo text","parameters":{"type":"object"},"strict":true}],
            "tool_choice":"required",
            "max_output_tokens":64,
            "stream":true,
            "store":false,
        }))
        .send().await.expect("router answers");
    assert_eq!(response.status(), StatusCode::OK);
    let sse = response.text().await.unwrap();
    assert!(sse.contains("response.output_item.done"), "{sse}");
    assert!(sse.contains("response.completed"), "{sse}");

    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/responses");
    assert_eq!(
        requests[0].headers.get(header::AUTHORIZATION).unwrap(),
        "Bearer sk-or-test"
    );
    let rendered: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(rendered["model"], "openai/gpt-4.1-mini");
    assert_eq!(rendered["max_output_tokens"], 64);
    assert_eq!(rendered["tools"][0]["strict"], true);
    assert_eq!(rendered["store"], false);
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(
        rows[0].route.as_deref(),
        Some("openai_responses:openrouter")
    );
    assert_eq!(rows[0].cost_usd, Some(0.0000336));
    assert_eq!(rows[0].cost_kind, Some(toker::store::CostKind::Billed));
    assert_eq!(
        rows[0].extra.as_ref().unwrap()["serving_provider"],
        "Example Router"
    );
}

#[tokio::test]
async fn openrouter_responses_incomplete_usage_and_errors() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(openrouter_responses_config("router-short", upstream)).await;
    let complete = client()
        .post(format!("http://{addr}/v1/responses"))
        .json(&json!({"model":"openrouter/short","input":"hello","stream":false}))
        .send()
        .await
        .unwrap();
    let status = complete.status();
    let body: Value = complete.json().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "incomplete");
    assert_eq!(wait_for_rows(&store, 1).await[0].cost_usd, Some(0.0000336));

    let error = client()
        .post(format!("http://{addr}/v1/responses"))
        .json(&json!({"model":"openrouter/bad","input":"hello","stream":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(error.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(mock.requests.lock().unwrap().len(), 2);
    assert_eq!(wait_for_rows(&store, 2).await[1].kind, Some(RowKind::Error));

    let numeric = client()
        .post(format!("http://{addr}/v1/responses"))
        .json(&json!({"model":"openrouter/numeric-error","input":"hello","stream":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(numeric.status(), StatusCode::BAD_REQUEST);
    let body: Value = numeric.json().await.unwrap();
    assert_eq!(body["error"]["message"], "invalid model");
    let rows = wait_for_rows(&store, 3).await;
    assert_eq!(rows[2].kind, Some(RowKind::Error));
}

#[tokio::test]
async fn openrouter_responses_failed_turn_records_error_but_cut_turn_stays_quiet() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(openrouter_responses_config("router-failures", upstream)).await;
    let failed = client()
        .post(format!("http://{addr}/v1/responses"))
        .json(&json!({"model":"openrouter/failed","input":"hello","stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), StatusCode::OK);
    let failed_body = failed.text().await.unwrap();
    assert!(failed_body.contains("response.failed"), "{failed_body}");
    assert_eq!(wait_for_rows(&store, 1).await[0].kind, Some(RowKind::Error));

    let numeric = client()
        .post(format!("http://{addr}/v1/responses"))
        .json(&json!({"model":"openrouter/numeric-failed","input":"hello","stream":false}))
        .send()
        .await
        .unwrap();
    let numeric_body: Value = numeric.json().await.unwrap();
    assert_eq!(numeric_body["error"]["message"], "invalid streamed model");
    assert_eq!(wait_for_rows(&store, 2).await[1].kind, Some(RowKind::Error));

    let cut = client()
        .post(format!("http://{addr}/v1/responses"))
        .json(&json!({"model":"openrouter/cut","input":"hello","stream":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(cut.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(mock.requests.lock().unwrap().len(), 3);
    assert_eq!(wait_for_rows(&store, 2).await.len(), 2);
}

#[tokio::test]
async fn non_streaming_responses_are_aggregated_from_canonical_turns() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config("responses-json", upstream, false)).await;
    let response = client()
        .post(format!("http://{addr}/v1/responses"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(
            serde_json::to_vec(&json!({
                "model": "gpt-5.6-sol",
                "prompt_cache_key": "responses-json-session",
                "input": "private prompt",
                "stream": false
            }))
            .expect("request body"),
        )
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let body: Value = response.json().await.expect("Responses JSON");
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["output"][0]["type"], "reasoning");
    assert_eq!(body["output"][0]["id"], "rs_1");
    assert_eq!(
        body["output"][0]["encrypted_content"],
        "opaque-encrypted-reasoning"
    );
    assert_eq!(body["output"][1]["type"], "message");
    assert_eq!(body["output"][2]["type"], "function_call");
    assert_eq!(body["usage"]["input_tokens"], 1234);

    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&requests[0].body).expect("upstream JSON");
    assert_eq!(upstream_body["stream"], true, "the backend always streams");
    assert_eq!(wait_for_rows(&store, 1).await[0].kind, None);
}

#[tokio::test]
async fn chat_frontend_routes_to_codex_and_translates_both_ways() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config("chat-to-codex", upstream, false)).await;
    let response = client()
        .post(format!("http://{addr}/f/opencode/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-toker-session", "chat-codex-session")
        .body(
            json!({
                "model": "codex_sub/gpt-5.6-sol",
                "messages": [{"role": "user", "content": "private prompt"}],
                "stream": true
            })
            .to_string(),
        )
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.expect("chat SSE");
    assert!(body.contains("chat.completion.chunk"));
    assert!(body.contains("\"tool_calls\""));
    assert!(body.contains("data: [DONE]"));

    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let request: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(request["model"], "gpt-5.6-sol");
    assert_eq!(request["input"][0]["role"], "user");

    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].frontend.as_deref(), Some("openai_chat"));
    assert_eq!(rows[0].route.as_deref(), Some("openai_chat:codex_sub"));
    assert_eq!(rows[0].provider.as_deref(), Some("codex_sub"));
    assert_eq!(rows[0].session_id.as_deref(), Some("chat-codex-session"));
}

#[tokio::test]
async fn chat_can_use_codex_as_its_default_and_return_plain_json() {
    let (upstream, _mock) = spawn_mock().await;
    let mut config = test_config("chat-default-codex", upstream, false);
    config.default_backend_openai_chat = Some("codex_sub".to_owned());
    let (addr, store) = spawn_toker(config).await;
    let response = client()
        .post(format!("http://{addr}/f/opencode/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-toker-session", "chat-default-session")
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role": "user", "content": "private prompt"}],
            "stream": false
        }))
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("chat completion JSON");
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["model"], "gpt-5.6-sol");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "read_file"
    );

    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].route.as_deref(), Some("openai_chat:codex_sub"));
}

#[tokio::test]
async fn an_invalid_responses_body_is_rejected_before_upstream_and_not_ledgered() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config("native-invalid", upstream, false)).await;
    let body = Bytes::from_static(b"not json at all");
    let response = client()
        .post(format!("http://{addr}/v1/responses"))
        .body(body.clone())
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = response.bytes().await.expect("body");
    assert!(mock.requests.lock().unwrap().is_empty());
    assert!(wait_for_rows(&store, 0).await.is_empty());
}

#[tokio::test]
async fn a_streaming_turn_translates_both_ways_and_records() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config("live", upstream, true)).await;

    let response = client()
        .post(format!("http://{addr}/v1/messages"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("x-claude-code-session-id", "ses-codex-1")
        // The frontend's own (dummy) credential: must NEVER reach the
        // codex backend (pass-through-when-present's explicit opt-out).
        .header(header::AUTHORIZATION, "Bearer frontend-dummy")
        .body(messages_body("codex_sub/claude-opus-5", true))
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
    assert_eq!(
        row.requested_model.as_deref(),
        Some("codex_sub/claude-opus-5")
    );
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
    assert_eq!(
        row.extra
            .as_ref()
            .and_then(|extra| extra.get("translation_losses")),
        Some(&json!([{
            "path": "sampling.max_tokens",
            "reason": "unsupported_by_binding",
            "count": 1,
        }])),
        "semantic omissions are visible without carrying their values"
    );
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
    let (addr, store) = spawn_toker(test_config("ns", upstream, true)).await;

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
    let (addr, store) = spawn_toker(test_config("ut", upstream, true)).await;

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
    assert_eq!(
        rows[0].req_messages, None,
        "unsupported input has no canonical shape"
    );
}

#[tokio::test]
async fn an_http_error_maps_through_the_error_table() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config("e401", upstream, false)).await;

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
    let (addr, store) = spawn_toker(test_config("fail", upstream, false)).await;

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
    let (addr, store) = spawn_toker(test_config("ct", upstream, false)).await;

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

#[tokio::test]
async fn unmatched_and_batch_paths_on_a_codex_default_answer_locally() {
    let (upstream, mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config("unmatched", upstream, false)).await;

    // An unmatched path: the codex backend serves no anthropic paths, so
    // the answer is anthropic's own 404 shape, not a forward.
    let response = client()
        .get(format!("http://{addr}/v1/files?limit=2"))
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error: Value =
        serde_json::from_str(&response.text().await.expect("body")).expect("error json");
    assert_eq!(error["error"]["type"], "not_found_error");

    // A batch GET: the same typed error as batch creation on this
    // backend.
    let response = client()
        .get(format!("http://{addr}/v1/messages/batches/batch_123"))
        .send()
        .await
        .expect("toker answers");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value =
        serde_json::from_str(&response.text().await.expect("body")).expect("error json");
    assert_eq!(error["error"]["type"], "invalid_request_error");

    assert!(
        mock.requests.lock().unwrap().is_empty(),
        "nothing forwarded"
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(store.requests_since(0, 100).expect("rows").is_empty());
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
async fn an_upstream_failure_mid_turn_aborts_the_translated_stream_and_records_no_row() {
    let (upstream, _mock) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config("drop-mid", upstream, false)).await;

    // A reset mid-turn, and a clean close before the terminator: the
    // translation once ended cleanly on both, handing claude a turn the
    // upstream never finished.
    for model in ["drop-mid", "eof-mid"] {
        let response = client()
            .post(format!("http://{addr}/v1/messages"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(messages_body(model, true))
            .send()
            .await
            .expect("toker answers");
        assert_eq!(response.status(), StatusCode::OK, "{model}");
        let read = tokio::time::timeout(std::time::Duration::from_secs(10), read_to_end(response))
            .await
            .expect("the client response ends promptly");
        assert!(
            read.is_err(),
            "{model}: the client sees a transport error, not a clean end: {read:?}"
        );
    }

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(store.count_requests().expect("count"), 0, "no row");
}
