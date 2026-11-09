//! End-to-end tests of revision one's upgrade policy ("force newest") behind
//! the real toker router and a mock anthropic upstream that captures every
//! request byte.
//!
//! The evidence is the binding's learned models and the ledger's recency
//! rows, seeded straight into the store before the server is built. The
//! decision itself is policy's (`policy::resolve`); these tests pin what
//! reaches the wire and the ledger:
//!
//! - a conversation that has barely started, or whose model nothing has
//!   served within a full cache TTL, is moved onto the learned newest of its
//!   family before canonical rendering: every cache_control survives, the
//!   upgrade is in the row's policy trace, and the row and the lane carry the
//!   forced pair;
//! - a lane with a cache to lose keeps its model, a conversation recently
//!   served on its model keeps it, and no provenance is recorded;
//! - the guard on the target's observed prompt ceiling is empirical, never a
//!   declared one;
//! - an upgraded lane keeps its recorded target on later turns, even once
//!   the election has moved on to a newer model, and never errors for it;
//! - learning is binding-local: evidence for one backend moves nothing on
//!   another, and the openai path never meets the machinery.

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
use toker::store::{BindingLearningUpdate, RequestRow, Store, StoredRoutingTraceEvent};
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
        credential_digest: None,
        account_number: None,
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

/// The upgrade a row's policy trace recorded, as `(from, to)`.
fn traced_upgrade(row: &RequestRow) -> Option<(String, String)> {
    row.policy_routing
        .as_ref()?
        .trace
        .events
        .iter()
        .find_map(|event| match event {
            StoredRoutingTraceEvent::Upgrade { from, to } => Some((from.clone(), to.clone())),
            _ => None,
        })
}

/// A request whose first user turn carries `padding` bytes of filler, for a
/// prompt bound above a ceiling the target was seen to hold.
fn padded_body(model: &str, padding: usize) -> Vec<u8> {
    let mut body: Value = serde_json::from_slice(&tools_body(model, 2)).expect("tools body");
    body["messages"][0]["content"] = json!("x".repeat(padding));
    serde_json::to_vec(&body).expect("serialise padded body")
}

/// A ledger row that served `model` on the subscription binding at `ts_ms`:
/// what the startup seed reads as recency for that binding and model.
fn served_row(ts_ms: i64, model: &str) -> RequestRow {
    let mut row = bare_row(ts_ms);
    row.binding_id = Some(ANTHROPIC_SUB_MESSAGES.id().clone());
    row.provider = Some("anthropic_sub".to_owned());
    row.raw_model = Some(model.to_owned());
    row.model = Some(model.to_owned());
    row.effective_model = Some(model.to_owned());
    row
}

/// A conversation that has barely started is moved onto the family's
/// learned newest. The upstream body is the request with the model value
/// swapped, cache_control included, and the upgrade lands on the row (its
/// policy trace and the forced pair) and in the lane record.
#[tokio::test]
async fn a_new_conversation_moves_to_the_learned_newest() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    seed_opus(&store, 200_000);
    let addr = spawn_toker(config, store.clone()).await;

    let body = tools_body("claude-opus-4-8", 2);
    let response = post_messages(addr, "ccses-42", &body).await;
    assert_eq!(response.status(), StatusCode::OK);

    // Canonical rendering changed only the model semantics and kept every
    // cache-control extension.
    let expected = with_model(&body, "claude-opus-4-8", "claude-opus-5");
    let captured = mock.captured();
    assert_eq!(captured.len(), 1, "the request reached upstream");
    assert_canonical_request(&expected, &captured[0], "claude-opus-5");

    // The row: the identities stay distinct, the trace says where the model
    // moved, and the forced pair records the move.
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, None, "a real measurement, not a proxy row");
    assert_eq!(rows[0].requested_model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(rows[0].effective_model.as_deref(), Some("claude-opus-5"));
    assert_eq!(rows[0].model.as_deref(), Some("claude-opus-5"), "served");
    assert_eq!(rows[0].forced_from.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(rows[0].forced_to.as_deref(), Some("claude-opus-5"));
    assert_eq!(
        traced_upgrade(&rows[0]),
        Some(("claude-opus-4-8".to_owned(), "claude-opus-5".to_owned()))
    );

    // The lane keeps the upgrade: its cache lives on the new model now.
    let lane = store
        .load_lane(&lane_key_of(&body))
        .expect("load lane")
        .expect("the lane exists");
    assert_eq!(lane.forced_from.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(lane.forced_to.as_deref(), Some("claude-opus-5"));
    let pin = lane.pin.expect("the lane is pinned to its route");
    assert_eq!(pin.baseline_model, "claude-opus-4-8");
    assert_eq!(pin.effective_model, "claude-opus-5");
}

/// With the upgrade policy switched off nothing moves, whatever has been
/// learned.
#[tokio::test]
async fn a_disabled_upgrade_policy_moves_nothing() {
    let (mock, upstream) = spawn_mock().await;
    let mut config = test_config(upstream);
    config.gates.force_newest = false;
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    seed_opus(&store, 200_000);
    let addr = spawn_toker(config, store.clone()).await;

    let body = tools_body("claude-opus-4-8", 2);
    let response = post_messages(addr, "ccses-42", &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    assert_canonical_request(&body, &captured[0], "claude-opus-4-8");

    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].forced_from, None);
    assert_eq!(rows[0].forced_to, None);
    assert_eq!(traced_upgrade(&rows[0]), None);
}

/// An openrouter-routed request is never moved, however much the
/// subscription backend has learned: the evidence belongs to its own
/// binding, and a bare `claude-*` id at openrouter is not the model the user
/// picked.
#[tokio::test]
async fn an_openrouter_request_is_never_moved() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    seed_opus(&store, 200_000);
    let addr = spawn_toker(config, store.clone()).await;

    let body = tools_body("openrouter/claude-opus-4-8", 2);
    let response = post_messages(addr, "ccses-42", &body).await;
    assert_eq!(response.status(), StatusCode::OK);

    let captured = mock.captured();
    assert_eq!(captured.len(), 1, "the request reached upstream");
    let expected = with_model(&body, "openrouter/claude-opus-4-8", "claude-opus-4-8");
    assert_canonical_request(&expected, &captured[0], "claude-opus-4-8");
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].provider.as_deref(), Some("openrouter"));
    assert_eq!(rows[0].forced_from, None);
    assert_eq!(rows[0].forced_to, None);
    assert_eq!(traced_upgrade(&rows[0]), None);
}

/// A lane with a cache to lose keeps its model: the lane is known and warm,
/// so a newer model learned since is not worth rebuilding the cache for,
/// even though the conversation is short enough to qualify were it new.
#[tokio::test]
async fn a_warm_lane_with_cache_to_lose_is_left_alone() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    let addr = spawn_toker(config, store.clone()).await;

    // Nothing newer is known yet: the first turn opens the lane on the
    // model the client asked for.
    let body = tools_body("claude-opus-4-8", 2);
    let response = post_messages(addr, "ccses-42", &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    wait_for_rows(&store, 1).await;

    // A newer model is learned, and the lane speaks again moments later.
    seed_opus(&store, 200_000);
    let response = post_messages(addr, "ccses-42", &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let captured = mock.captured();
    assert_eq!(captured.len(), 2);
    assert_canonical_request(&body, &captured[1], "claude-opus-4-8");

    let rows = wait_for_rows(&store, 2).await;
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.forced_from, None);
        assert_eq!(row.forced_to, None);
        assert_eq!(traced_upgrade(row), None);
    }
}

/// A lane pinned to a binding with no cache domain (anthropic_sub) reads as
/// cold from the TTL it recorded, so a newer model learned while it idled
/// past that TTL is taken: the cache is gone, nothing is lost by moving. A
/// lane that recorded no TTL has no clock, and unknown is never cold.
#[tokio::test]
async fn an_idle_pinned_lane_moves_on_its_recorded_ttl_only() {
    let body = tools_body("claude-opus-4-8", 5);
    for (recorded_ttl, moved) in [(Some(300_000), true), (None, false)] {
        let (mock, upstream) = spawn_mock().await;
        let config = test_config(upstream);
        let store = Arc::new(Store::open(&config.db_path).expect("open store"));
        seed_opus(&store, 200_000);
        let addr = spawn_toker(config, store.clone()).await;

        // The first turn pins the lane to the model asked for: five messages
        // with nothing served is no new conversation, so it is not moved.
        let response = post_messages(addr, "ccses-42", &body).await;
        assert_eq!(response.status(), StatusCode::OK);
        wait_for_rows(&store, 1).await;

        // The lane then idles for twenty minutes.
        let mut lane = store
            .load_lane(&lane_key_of(&body))
            .expect("load lane")
            .expect("the lane exists");
        lane.ttl = recorded_ttl;
        lane.updated_ms = now_ms() - 20 * 60_000;
        store.upsert_lane(&lane).expect("age the lane");

        let response = post_messages(addr, "ccses-42", &body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let captured = mock.captured();
        assert_eq!(captured.len(), 2);
        let rows = wait_for_rows(&store, 2).await;
        let row = rows
            .iter()
            .max_by_key(|row| row.ts_ms)
            .expect("the second turn's row");
        if moved {
            let expected = with_model(&body, "claude-opus-4-8", "claude-opus-5");
            assert_canonical_request(&expected, &captured[1], "claude-opus-5");
            assert_eq!(row.forced_to.as_deref(), Some("claude-opus-5"));
        } else {
            assert_canonical_request(&body, &captured[1], "claude-opus-4-8");
            assert_eq!(row.forced_from, None);
            assert_eq!(traced_upgrade(row), None);
        }
    }
}

/// The ceiling guard is empirical: a target never observed holding a
/// prompt this size is not sent one. The same request moves once the target
/// has been seen holding it.
#[tokio::test]
async fn a_conversation_the_target_has_not_held_is_not_moved() {
    // The bound is bytes / 2, so this is a prompt of about 60k tokens: over
    // 50k, under 200k.
    let body = padded_body("claude-opus-4-8", 120_000);
    for (max_prompt, moved) in [(50_000, false), (200_000, true)] {
        let (mock, upstream) = spawn_mock().await;
        let config = test_config(upstream);
        let store = Arc::new(Store::open(&config.db_path).expect("open store"));
        seed_opus(&store, max_prompt);
        let addr = spawn_toker(config, store.clone()).await;

        let response = post_messages(addr, "ccses-42", &body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let captured = mock.captured();
        assert_eq!(captured.len(), 1);
        let rows = wait_for_rows(&store, 1).await;
        assert_eq!(rows.len(), 1);
        if moved {
            let expected = with_model(&body, "claude-opus-4-8", "claude-opus-5");
            assert_canonical_request(&expected, &captured[0], "claude-opus-5");
            assert_eq!(rows[0].forced_to.as_deref(), Some("claude-opus-5"));
        } else {
            assert_canonical_request(&body, &captured[0], "claude-opus-4-8");
            assert_eq!(rows[0].forced_from, None, "no provenance for a refusal");
            assert_eq!(traced_upgrade(&rows[0]), None);
        }
    }
}

/// An upgraded lane keeps its recorded target across turns, even once the
/// election has moved on to a newer model, and a longer conversation does
/// not stop it. The turn after the move is answered, not refused.
#[tokio::test]
async fn an_upgraded_lane_keeps_its_recorded_target() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    seed_opus(&store, 200_000);
    let addr = spawn_toker(config, store.clone()).await;

    // The first turn moves the lane onto opus-5, where its cache lives.
    let body = tools_body("claude-opus-4-8", 2);
    let response = post_messages(addr, "ccses-42", &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    wait_for_rows(&store, 1).await;

    // The election now names opus-5-5. The next turn still goes to the
    // recorded target, and so does the one after it.
    learn(&store, &ANTHROPIC_SUB_MESSAGES, "claude-opus-5-5", 200_000);
    for _ in 0..2 {
        let response = post_messages(addr, "ccses-42", &body).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    let longer = tools_body("claude-opus-4-8", 5);
    let response = post_messages(addr, "ccses-42", &longer).await;
    assert_eq!(response.status(), StatusCode::OK);

    let expected = with_model(&body, "claude-opus-4-8", "claude-opus-5");
    let captured = mock.captured();
    assert_eq!(captured.len(), 4);
    assert_canonical_request(&expected, &captured[0], "claude-opus-5");
    assert_eq!(captured[1], captured[0], "the next turn sticks too");
    assert_eq!(captured[2], captured[0], "and the one after it");
    let expected_longer = with_model(&longer, "claude-opus-4-8", "claude-opus-5");
    assert_canonical_request(&expected_longer, &captured[3], "claude-opus-5");

    let rows = wait_for_rows(&store, 4).await;
    assert_eq!(rows.len(), 4);
    for row in &rows {
        assert_eq!(row.forced_from.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(row.forced_to.as_deref(), Some("claude-opus-5"));
    }
}

/// An unknown lane (nothing recorded for its session) with a barely-started
/// conversation moves; one with a real history, on a model that was served
/// moments ago, does not. The served row is the recency evidence, so the
/// served-recency condition cannot be what qualifies the short one.
#[tokio::test]
async fn an_unknown_lane_with_a_short_conversation_moves() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    seed_opus(&store, 200_000);
    // The asked model was served moments ago: it reads as warm, so only the
    // message count can qualify a lane.
    store
        .record_request(&served_row(now_ms(), "claude-opus-4-8"))
        .expect("seed served row");
    let addr = spawn_toker(config, store.clone()).await;

    // Two messages: a conversation that has barely started.
    let short = tools_body("claude-opus-4-8", 2);
    let response = post_messages(addr, "ccses-42", &short).await;
    assert_eq!(response.status(), StatusCode::OK);
    // Three messages, a different session (a different lane): a real
    // history, recently-served model, nothing qualifies.
    let long = tools_body("claude-opus-4-8", 3);
    let response = post_messages(addr, "ccses-43", &long).await;
    assert_eq!(response.status(), StatusCode::OK);

    let captured = mock.captured();
    assert_eq!(captured.len(), 2);
    let expected_short = with_model(&short, "claude-opus-4-8", "claude-opus-5");
    assert_canonical_request(&expected_short, &captured[0], "claude-opus-5");
    assert_canonical_request(&long, &captured[1], "claude-opus-4-8");

    // Three rows: the served-row seed plus the two requests.
    let rows = wait_for_rows(&store, 3).await;
    assert_eq!(rows.len(), 3);
    let moved = rows
        .iter()
        .find(|row| row.session_id.as_deref() == Some("ccses-42"))
        .expect("the short conversation's row");
    assert_eq!(moved.forced_from.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(
        traced_upgrade(moved),
        Some(("claude-opus-4-8".to_owned(), "claude-opus-5".to_owned()))
    );
    let unmoved = rows
        .iter()
        .find(|row| row.session_id.as_deref() == Some("ccses-43"))
        .expect("the real history's row");
    assert_eq!(unmoved.forced_from, None);
    assert_eq!(traced_upgrade(unmoved), None);
}

/// An unknown lane with a real history still moves when nothing has been
/// served on its model for a full cache TTL: no cache it could read exists.
/// Recency that does not reach back that far proves nothing, so the same
/// request stays put when the ledger only goes back a few minutes.
#[tokio::test]
async fn an_unknown_lane_idle_for_a_full_ttl_moves_only_on_covered_evidence() {
    let long = tools_body("claude-opus-4-8", 3);
    for (served_ago_ms, moved) in [(3 * 24 * 3_600_000, true), (5 * 60_000, false)] {
        let (mock, upstream) = spawn_mock().await;
        let config = test_config(upstream);
        let store = Arc::new(Store::open(&config.db_path).expect("open store"));
        seed_opus(&store, 200_000);
        // The model was last served `served_ago_ms` ago, on another
        // conversation; the ledger reaches back exactly that far.
        store
            .record_request(&served_row(now_ms() - served_ago_ms, "claude-opus-4-8"))
            .expect("seed served row");
        let addr = spawn_toker(config, store.clone()).await;

        let response = post_messages(addr, "ccses-42", &long).await;
        assert_eq!(response.status(), StatusCode::OK);
        let captured = mock.captured();
        assert_eq!(captured.len(), 1);
        let rows = wait_for_rows(&store, 2).await;
        let row = rows
            .iter()
            .find(|row| row.session_id.as_deref() == Some("ccses-42"))
            .expect("the request's row");
        if moved {
            let expected = with_model(&long, "claude-opus-4-8", "claude-opus-5");
            assert_canonical_request(&expected, &captured[0], "claude-opus-5");
            assert_eq!(row.forced_to.as_deref(), Some("claude-opus-5"));
        } else {
            assert_canonical_request(&long, &captured[0], "claude-opus-4-8");
            assert_eq!(row.forced_from, None);
            assert_eq!(traced_upgrade(row), None);
        }
    }
}

/// The openai path never meets the upgrade machinery: learned models cannot
/// change its routed model, and its rows carry no force provenance.
#[tokio::test]
async fn the_openai_path_never_meets_the_force_machinery() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    // Poisoned: had the upgrade policy leaked into the openai path, this
    // evidence would rewrite the request.
    seed_opus(&store, 200_000);
    let addr = spawn_toker(config, store.clone()).await;

    let body = serde_json::to_vec(&json!({
        "model": "claude-opus-4-8",
        "messages": [{"role": "user", "content": "Hi"}],
    }))
    .expect("serialise openai body");
    let response = post_chat(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);

    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    let sent: serde_json::Value =
        serde_json::from_slice(&captured[0]).expect("canonical Chat request");
    assert_eq!(sent["model"], "claude-opus-4-8");
    assert_eq!(sent["messages"][0]["content"][0]["text"], "Hi");

    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].frontend.as_deref(),
        Some("openai_chat"),
        "the openai row"
    );
    assert_eq!(rows[0].forced_from, None);
    assert_eq!(rows[0].forced_to, None);
    assert_eq!(traced_upgrade(&rows[0]), None);
}
