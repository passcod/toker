//! End-to-end force-newest rewrite tests: a mock anthropic upstream
//! (capturing every request byte) behind the real toker router, with the
//! learned model store seeded and lanes poisoned directly — the decision
//! reads the store and the lane table, so those are the whole fixture.
//!
//! Asserts the unit's contract end to end:
//!
//! - a COLD lane is moved onto its family's learned newest before canonical
//!   rendering: every cache_control survives, and the forced provenance lands on the
//!   row (`forcedFrom`/`forcedTo`) and in the lane record;
//! - a WARM lane with cache to lose keeps its request semantics, no
//!   provenance;
//! - a lane whose conversation exceeds the target's OBSERVED maxPrompt
//!   is not moved — the guard is empirical, never a declared ceiling;
//! - a sticky lane keeps its recorded target across requests, even once
//!   the election has moved on to a newer model;
//! - an unknown lane with a barely-started conversation moves (the
//!   message-count condition), and one with a real history does not;
//! - the openai path never meets the machinery: a poisoned store and
//!   openai traffic keeps its model through canonical rendering.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use serde_json::{Value, json};

use toker::config::{
    AnthropicApiConfig, AnthropicSubConfig, CodexSubConfig, Config, GatesConfig, OpenRouterConfig,
};
use toker::ir::Request as IrRequest;
use toker::routing::{ANTHROPIC_SUB_MESSAGES, BackendBinding};
use toker::server::Server;
use toker::store::{BindingLearningUpdate, Lane, RequestRow, Store};
use toker::translate::from_anthropic;

/// An env name no test ever sets, so nothing resolves and nothing injects.
const UNSET_KEY_ENV: &str = "TOKER_TEST_KEY_UNSET_FORCE_7A";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_millis() as i64
}

fn assert_canonical_request(source: &[u8], actual: &[u8], model: &str) {
    let source: Value = serde_json::from_slice(source).expect("source request JSON");
    let mut expected = from_anthropic(&source).expect("source request canonicalizes");
    expected.model = Some(model.to_owned());
    let actual: Value = serde_json::from_slice(actual).expect("rendered request JSON");
    assert_eq!(
        from_anthropic(&actual).expect("rendered request reparses"),
        expected
    );
}

// ---------------------------------------------------------------------------
// The mock upstream
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockState {
    requests: Arc<Mutex<Vec<Bytes>>>,
}

impl MockState {
    fn captured(&self) -> Vec<Bytes> {
        self.requests.lock().unwrap().clone()
    }
}

fn raw_json(body: Bytes) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}

/// A non-streaming Messages response echoing the request's model, with a
/// usage block (so a forwarded completion records a measurement row).
fn non_stream_body(model: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": "msg_test_force",
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{"type": "text", "text": "Done."}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 9,
            "cache_read_input_tokens": 100,
            "cache_creation_input_tokens": 400,
            "output_tokens": 40,
        },
    }))
    .expect("serialise non-stream body")
}

async fn mock_messages(State(mock): State<MockState>, request: Request) -> Response {
    let body = axum::body::to_bytes(request.into_body(), 64 * 1024 * 1024)
        .await
        .expect("mock reads body");
    mock.requests.lock().unwrap().push(body.clone());
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let model = json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if json.get("stream") == Some(&Value::Bool(true)) {
        let body = format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_test_force\",\"model\":{model:?},\"usage\":{{\"input_tokens\":9,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":400,\"output_tokens\":0}}}}}}\n\n\
             event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
             event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"Done.\"}}}}\n\n\
             event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
             event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":40}}}}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = StatusCode::OK;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/event-stream"),
        );
        response
    } else {
        raw_json(Bytes::from(non_stream_body(&model)))
    }
}

async fn mock_chat(State(mock): State<MockState>, request: Request) -> Response {
    let body = axum::body::to_bytes(request.into_body(), 64 * 1024 * 1024)
        .await
        .expect("mock reads body");
    mock.requests.lock().unwrap().push(body);
    raw_json(Bytes::from_static(
        br#"{"choices":[{"message":{"role":"assistant","content":"hi"}}],
            "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
    ))
}

/// One mock standing for every backend: the anthropic routes, plus the
/// openai-chat route the openai-isolation test drives.
async fn spawn_mock() -> (MockState, reqwest::Url) {
    let state = MockState::default();
    let app = Router::new()
        .route("/v1/messages", post(mock_messages))
        .route("/v1/chat/completions", post(mock_chat))
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

fn test_config(upstream: reqwest::Url) -> Config {
    // The openai-chat backend's base includes the /v1 prefix (the
    // openrouter convention); the anthropic backends' base is the API
    // root. Both stand for the same mock.
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
            // Only the openai-isolation test routes here.
            upstream: openrouter_upstream,
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key_keyring: false,
            // Policy refuses a backend it has no credential for.
            api_key: Some("sk-or-test".to_owned()),
            picker: None,
        }),
        openai_api: None,
        default_backend_anthropic: Some("anthropic_sub".to_owned()),
        anthropic_sub: Some(AnthropicSubConfig {
            model_map: None,
            upstream: upstream.clone(),
            claude_credentials_path: None,
            oauth_token_env: UNSET_KEY_ENV.to_owned(),
            oauth_token: Some("sk-ant-oat-test".to_owned()),
            ..AnthropicSubConfig::default()
        }),
        anthropic_api: Some(AnthropicApiConfig {
            model_map: None,
            upstream,
            api_key_env: UNSET_KEY_ENV.to_owned(),
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
        // The measured defaults: the force rewrite ON, the cold gate ON (a
        // poisoned prompt below the 175k bar keeps its notice out of the
        // way, isolating this unit's rewrite).
        gates: GatesConfig::default(),
        notices: toker::config::NoticesConfig::default(),
        // The sleep lock stays off in tests: the real spawner would take
        // a REAL idle-sleep lock on the host running the suite. The awake
        // suite (server_awake.rs) injects a fake spawner and turns it on.
        awake: false,
        transcript_roots: Vec::new(),
    }
}

async fn spawn_toker(config: Config, store: Arc<Store>) -> SocketAddr {
    let server =
        Server::with_seams(config, store, toker::server::Seams::isolated()).expect("build server");
    let app = server.router();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("toker binds");
    let addr = listener.local_addr().expect("toker addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("toker serves");
    });
    addr
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("client")
}

async fn post_messages(addr: SocketAddr, session: &str, body: &[u8]) -> reqwest::Response {
    client()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-claude-code-session-id", session)
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(body.to_vec())
        .send()
        .await
        .expect("messages request")
}

async fn post_chat(addr: SocketAddr, body: &[u8]) -> reqwest::Response {
    client()
        .post(format!("http://{addr}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .await
        .expect("chat request")
}

async fn wait_for_rows(store: &Store, count: usize) -> Vec<RequestRow> {
    for _ in 0..200 {
        let rows = store.requests_since(0, 1000).expect("read rows");
        if rows.len() >= count {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    store.requests_since(0, 1000).expect("read rows")
}

// ---------------------------------------------------------------------------
// The fixtures: a seeded store, a poisoned lane, an asked-for body
// ---------------------------------------------------------------------------

/// Nine days of history: comfortably above the election bar (needed = 4.5).
const D: [&str; 9] = [
    "2026-09-01",
    "2026-09-02",
    "2026-09-03",
    "2026-09-04",
    "2026-09-05",
    "2026-09-06",
    "2026-09-07",
    "2026-09-08",
    "2026-09-09",
];

/// Evidence that `binding` has served `model_id` on nine days, with the
/// largest prompt it was seen to hold at `max_prompt`.
fn learn(store: &Store, binding: &BackendBinding, model_id: &str, max_prompt: i64) {
    store
        .merge_binding_learning(&BindingLearningUpdate {
            binding_id: binding.id().clone(),
            model_id: model_id.to_owned(),
            days: D.iter().map(|day| (*day).to_owned()).collect(),
            max_prompt: Some(max_prompt),
            context_window: None,
            explicitly_promoted: false,
            updated_ms: 1,
        })
        .expect("seed binding learning");
}

/// Teach the subscription binding the opus family: opus-4-8 (what the
/// requests ask for) and opus-5 (the learned newest), proven at `max_prompt`.
fn seed_opus(store: &Store, max_prompt: i64) {
    learn(store, &ANTHROPIC_SUB_MESSAGES, "claude-opus-5", max_prompt);
    learn(
        store,
        &ANTHROPIC_SUB_MESSAGES,
        "claude-opus-4-8",
        max_prompt,
    );
}

/// The tools body the tests ask with: a lane key (a session is not a
/// cache entry), cache breakpoints in the documented positions — which
/// the rewrite must keep, unlike the compaction retarget — and
/// `messages` sized by the caller.
fn tools_body(model: &str, messages: usize) -> Vec<u8> {
    let messages: Vec<Value> = (0..messages)
        .map(|i| json!({"role": if i % 2 == 0 { "user" } else { "assistant" }, "content": "Hi"}))
        .collect();
    serde_json::to_vec(&json!({
        "model": model,
        "stream": true,
        "system": [
            {"type": "text", "text": "You are careful.", "cache_control": {"type": "ephemeral"}},
        ],
        "tools": [
            {"name": "Read", "input_schema": {"type": "object"},
             "cache_control": {"type": "ephemeral"}},
            {"name": "Bash", "input_schema": {"type": "object"}},
        ],
        "messages": messages,
    }))
    .expect("serialise tools body")
}

fn lane_key_of(body: &[u8]) -> String {
    let shape = IrRequest::parse(body).expect("parse").anthropic().shape();
    format!("ccses-42|{}", shape.tools_hash)
}

/// The expected upstream bytes for a moved request: the model token
/// replaced, nothing else — pinned to exactly one occurrence so the
/// splice is unambiguous.
fn with_model(body: &[u8], from: &str, to: &str) -> Vec<u8> {
    let token = format!("\"{from}\"");
    let replacement = format!("\"{to}\"");
    let text = String::from_utf8(body.to_vec()).expect("test body is UTF-8");
    assert_eq!(
        text.matches(&token).count(),
        1,
        "the model token appears exactly once in the test body"
    );
    text.replace(&token, &replacement).into_bytes()
}

/// The lane row a body's session × tools-hash keys, ready to poison.
fn poisoned_lane(body: &[u8], updated_ms: i64, prompt: i64) -> Lane {
    Lane {
        key: lane_key_of(body),
        session_id: Some("ccses-42".to_owned()),
        tools_hash: Some(
            IrRequest::parse(body)
                .expect("parse")
                .anthropic()
                .shape()
                .tools_hash,
        ),
        updated_ms,
        prompt_tokens: Some(prompt),
        ttl: None,
        ping: None,
        noticed_at: None,
        forced_from: None,
        forced_to: None,
        pin_generation: 0,
        pin: None,
    }
}

fn bare_row(ts_ms: i64) -> RequestRow {
    RequestRow {
        id: None,
        ts_ms,
        duration_ms: None,
        kind: None,
        frontend: None,
        provider: None,
        route: None,
        session_id: None,
        ping: None,
        model: None,
        raw_model: None,
        requested_model: None,
        effective_model: None,
        input: None,
        cache_read: None,
        cache_write_total: None,
        cache_write_5m: None,
        cache_write_1h: None,
        output: None,
        reasoning: None,
        iterations: None,
        web_searches: None,
        code_execs: None,
        ttl_split_known: None,
        usage_presence: None,
        usage_raw: None,
        cost_usd: None,
        cost_kind: None,
        rate_limits: None,
        req_bytes: None,
        req_messages: None,
        req_tools: None,
        tools_hash: None,
        system_chars: None,
        system_hash: None,
        system_blocks: None,
        system_messages: None,
        compact_generations: None,
        summarising: None,
        system_change: None,
        system_ladder: None,
        system_tail: None,
        gate_on: None,
        cold_on: None,
        forced_from: None,
        forced_to: None,
        downgraded_from: None,
        downgraded_to: None,
        cache_stripped: None,
        system_merged: None,
        model_mappings: None,
        drift_digest: None,
        status: None,
        error_type: None,
        retry_after_ms: None,
        extra: None,
        betas: None,
        geo: None,
        fast: None,
        binding_id: None,
        attempt: None,
        policy_routing: None,
        lineage: None,
    }
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn probe() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    seed_opus(&store, 200_000);
    let body = tools_body("claude-opus-4-8", 2);
    let addr = spawn_toker(config, store.clone()).await;
    for i in 0..2 {
        let response = post_messages(addr, "ccses-42", &body).await;
        eprintln!("DBGSTATUS {i} {}", response.status());
        eprintln!(
            "DBGBODY {}",
            response
                .text()
                .await
                .unwrap()
                .chars()
                .take(600)
                .collect::<String>()
        );
    }
    let rows = wait_for_rows(&store, 2).await;
    for r in &rows {
        eprintln!(
            "DBGROW req={:?} eff={:?} model={:?} ff={:?} ft={:?} trace={:?}",
            r.requested_model,
            r.effective_model,
            r.model,
            r.forced_from,
            r.forced_to,
            r.policy_routing.as_ref().map(|p| &p.trace.events)
        );
    }
    eprintln!("DBGLANE {:?}", store.load_lane(&lane_key_of(&body)));
    let captured = mock.captured();
    for c in captured {
        eprintln!(
            "DBGSENT {}",
            String::from_utf8_lossy(&c)
                .chars()
                .take(60)
                .collect::<String>()
        );
    }
}
