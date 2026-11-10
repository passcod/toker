//! End-to-end tests for the in-flight record: a request is recorded in
//! the store from the moment toker begins handling it, moves through its
//! stages as the exchange does, and leaves only once its last ledger row
//! has been written (spec: SATT#while-a-request-is-in-flight).
//!
//! The mock upstream holds each exchange where the test says, so every
//! assertion about a stage is made while that stage is the one standing.

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::sync::Notify;

use toker::config::{
    AnthropicApiConfig, AnthropicSubConfig, CodexSubConfig, Config, GatesConfig, OpenRouterConfig,
};
use toker::store::{InFlightRequest, InFlightStage, RowKind, Store};

/// An env name no test ever sets, so nothing resolves and nothing injects.
const UNSET_KEY_ENV: &str = "TOKER_TEST_KEY_UNSET_IN_FLIGHT_5D";

/// The two points a held exchange waits at: before it answers at all, and
/// after its first event, before the rest.
#[derive(Clone, Default)]
struct Holds {
    reply: Arc<Notify>,
    finish: Arc<Notify>,
}

const OPENING: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":9,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":400}}}\n\n";

const REST: &[u8] = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":40}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// A model named `hold` is answered only when the test releases it; any
/// other is answered at once.
async fn mock_messages(State(holds): State<Holds>, request: Request) -> Response {
    let body = axum::body::to_bytes(request.into_body(), 64 * 1024 * 1024)
        .await
        .expect("mock reads body");
    let model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|json| json.get("model").and_then(Value::as_str).map(str::to_owned));
    let held = model.as_deref() == Some("hold");
    if held {
        holds.reply.notified().await;
    }
    let finish = holds.finish.clone();
    let stream = futures::stream::unfold(0u8, move |step| {
        let finish = finish.clone();
        async move {
            match step {
                0 => Some((Ok::<_, std::io::Error>(Bytes::from_static(OPENING)), 1)),
                1 => {
                    if held {
                        finish.notified().await;
                    }
                    Some((Ok(Bytes::from_static(REST)), 2))
                }
                _ => None,
            }
        }
    });
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    response
}

async fn spawn_mock() -> (Holds, reqwest::Url) {
    let holds = Holds::default();
    let app = Router::new()
        .route("/v1/messages", post(mock_messages))
        .with_state(holds.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("mock binds");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock serves");
    });
    let upstream: reqwest::Url = format!("http://{addr}").parse().expect("upstream url");
    (holds, upstream)
}

fn test_config(upstream: reqwest::Url) -> Config {
    let base = upstream.as_str().trim_end_matches('/');
    let openrouter_upstream: reqwest::Url = format!("{base}/v1").parse().expect("url");
    Config {
        port: 0,
        db_path: PathBuf::from(":memory:"),
        session_header_names: vec![
            "x-toker-session".to_owned(),
            "x-claude-code-session-id".to_owned(),
        ],
        ping_header_name: "x-toker-ping".to_owned(),
        default_backend_openai_chat: Some("openrouter".to_owned()),
        openrouter: Some(OpenRouterConfig {
            upstream: openrouter_upstream,
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key_keyring: false,
            api_key: Some("sk-or-test".to_owned()),
            picker: None,
        }),
        openai_api: None,
        default_backend_anthropic: Some("anthropic_sub".to_owned()),
        anthropic_sub: Some(AnthropicSubConfig {
            model_map: None,
            upstream: upstream.clone(),
            claude_credentials_path: None,
            ..AnthropicSubConfig::default()
        }),
        anthropic_api: Some(AnthropicApiConfig {
            model_map: None,
            upstream,
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key_keyring: false,
            api_key: None,
        }),
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
        gates: GatesConfig::default(),
        notices: toker::config::NoticesConfig::default(),
        awake: false,
        transcript_roots: Vec::new(),
    }
}

async fn spawn_toker() -> (Holds, SocketAddr, Arc<Store>) {
    let (holds, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    let addr = common::serve(common::server(config, &store)).await;
    (holds, addr, store)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("client")
}

fn body(model: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": model,
        "stream": true,
        "messages": [
            {"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "Hello"},
            {"role": "user", "content": "Again"},
        ],
    }))
    .expect("serialise body")
}

fn post_messages(
    addr: SocketAddr,
    model: &str,
    ping: bool,
) -> tokio::task::JoinHandle<reqwest::Response> {
    let mut request = client()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-claude-code-session-id", "ccses-42")
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .header(header::AUTHORIZATION, "Bearer sk-ant-oat-test");
    if ping {
        request = request.header("x-toker-ping", "1");
    }
    let request = request.body(body(model));
    tokio::spawn(async move { request.send().await.expect("messages request") })
}

/// Poll the in-flight table until `ready` holds of its rows.
async fn wait_in_flight(
    store: &Store,
    what: &str,
    ready: impl Fn(&[InFlightRequest]) -> bool,
) -> Vec<InFlightRequest> {
    for _ in 0..200 {
        let rows = store.in_flight_rows().expect("in-flight rows");
        if ready(&rows) {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "timed out waiting for {what}: {:?}",
        store.in_flight_rows().expect("in-flight rows")
    );
}

/// The counter the sleep lock and `toker restart` read, which a ping does
/// not move.
async fn counted(addr: SocketAddr) -> u64 {
    let response = client()
        .get(format!("http://{addr}/_toker/status"))
        .header("x-toker-control", "status")
        .send()
        .await
        .expect("status request");
    assert_eq!(response.status(), StatusCode::OK);
    let status: Value = response.json().await.expect("status body");
    status["in_flight"].as_u64().expect("in_flight count")
}

#[tokio::test]
async fn a_request_is_in_flight_from_arrival_until_its_row_lands() {
    let (holds, addr, store) = spawn_toker().await;
    let response = post_messages(addr, "hold", false);

    // Sent upstream and unanswered: in the store, with nothing yet in the
    // ledger to show for it.
    let rows = wait_in_flight(&store, "a request sent and waiting", |rows| {
        rows.len() == 1 && rows[0].stage == InFlightStage::Waiting
    })
    .await;
    let request = &rows[0];
    assert_eq!(request.session_id.as_deref(), Some("ccses-42"));
    assert_eq!(request.requested_model.as_deref(), Some("hold"));
    assert_eq!(request.req_messages, Some(3));
    assert_eq!(request.provider.as_deref(), Some("anthropic_sub"));
    assert_eq!(
        request.binding_id.as_deref(),
        Some("anthropic_sub.anthropic_messages")
    );
    assert_eq!(request.effective_model.as_deref(), Some("hold"));
    assert_eq!(request.attempt_index.map(|index| index.get()), Some(0));
    assert!(request.attempt_group_id.is_some());
    assert_eq!(store.count_requests().expect("count"), 0, "no row yet");
    assert_eq!(counted(addr).await, 1, "the sleep lock counts it");

    // The first event arrives: streaming.
    holds.reply.notify_one();
    wait_in_flight(&store, "a response streaming", |rows| {
        rows.len() == 1 && rows[0].stage == InFlightStage::Streaming
    })
    .await;
    assert_eq!(store.count_requests().expect("count"), 0, "still no row");

    // The response ends. The record goes only after the row is written,
    // so by the time a reader finds it gone the row is there.
    holds.finish.notify_one();
    let response = response.await.expect("client task");
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.expect("body");
    wait_in_flight(&store, "the record to go", |rows| rows.is_empty()).await;
    assert_eq!(
        store.count_requests().expect("count"),
        1,
        "the row had landed before the request left in flight"
    );
    assert_eq!(counted(addr).await, 0);
}

#[tokio::test]
async fn a_ping_is_in_flight_without_holding_the_sleep_lock() {
    let (holds, addr, store) = spawn_toker().await;
    let ping = post_messages(addr, "hold", true);
    wait_in_flight(&store, "the ping", |rows| rows.len() == 1).await;
    assert_eq!(counted(addr).await, 0, "a ping holds nothing awake");
    holds.reply.notify_one();
    holds.finish.notify_one();
    ping.await
        .expect("client task")
        .bytes()
        .await
        .expect("body");
    wait_in_flight(&store, "the record to go", |rows| rows.is_empty()).await;
}

#[tokio::test]
async fn a_client_that_hangs_up_leaves_in_flight_after_its_row() {
    let (_holds, addr, store) = spawn_toker().await;
    let response = post_messages(addr, "hold", false);
    wait_in_flight(&store, "a request sent and waiting", |rows| {
        rows.len() == 1 && rows[0].stage == InFlightStage::Waiting
    })
    .await;

    // The client goes away before any answer.
    response.abort();
    let _ = response.await;
    wait_in_flight(&store, "the record to go", |rows| rows.is_empty()).await;
    let rows = store.requests_since(0, 10).expect("rows");
    assert_eq!(rows.len(), 1, "the cancellation row had landed first");
    assert_eq!(rows[0].kind, Some(RowKind::Error));
    assert_eq!(rows[0].error_type.as_deref(), Some("client_cancelled"));
    assert_eq!(counted(addr).await, 0);
}

#[tokio::test]
async fn a_record_a_crash_left_behind_is_cleared_when_the_service_starts() {
    let (_holds, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    store.begin_in_flight(1, None).expect("left behind");
    assert_eq!(store.in_flight_rows().expect("rows").len(), 1);
    let server = common::server(config, &store);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("binds");
    tokio::spawn(async move {
        let _ = server.serve_listener(listener).await;
    });
    wait_in_flight(&store, "the stale record to go", |rows| rows.is_empty()).await;
}
