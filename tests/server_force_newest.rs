//! End-to-end force-newest rewrite tests: a mock anthropic upstream
//! (capturing every request byte) behind the real toker router, with the
//! learned model store seeded and lanes poisoned directly — the decision
//! reads the store and the lane table, so those are the whole fixture.
//!
//! Asserts the unit's contract end to end:
//!
//! - a COLD lane is moved onto its family's learned newest: the upstream
//!   body is the request with the model value swapped and nothing else
//!   (byte-asserted — every cache_control survives, the prefix-stability
//!   property of the transform), and the forced provenance lands on the
//!   row (`forcedFrom`/`forcedTo`) and in the lane record;
//! - a WARM lane with cache to lose is left byte-identical, no
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
use std::sync::atomic::{AtomicU64, Ordering};
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
use toker::server::Server;
use toker::store::{Lane, ModelEntry, RequestRow, Store};

/// An env name no test ever sets, so nothing resolves and nothing injects.
const UNSET_KEY_ENV: &str = "TOKER_TEST_KEY_UNSET_FORCE_7A";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_millis() as i64
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
    raw_json(Bytes::from(non_stream_body(&model)))
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

fn test_dir(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from("/tmp/opencode")
        .join(format!("server-force-{name}-{}-{n}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

fn test_config(upstream: reqwest::Url) -> Config {
    // The openai-chat backend's base includes the /v1 prefix (the
    // openrouter convention); the anthropic backends' base is the API
    // root. Both stand for the same mock.
    let base = upstream.as_str().trim_end_matches('/');
    let openrouter_upstream: reqwest::Url = format!("{base}/v1").parse().expect("url");
    Config {
        port: 0,
        db_path: test_dir("db").join("toker.db"),
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
            api_key: None,
            picker: None,
        }),
        default_backend_anthropic: Some("anthropic_sub".to_owned()),
        anthropic_sub: Some(AnthropicSubConfig {
            model_map: None,
            upstream: upstream.clone(),
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
            auth_path: test_dir("codex-absent").join("auth.json"),
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
    let server = Server::new(config, store).expect("build server");
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

/// Nine days of history: comfortably
/// above the election bar (needed = 4.5).
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

fn entry(model_id: &str, max_prompt: i64) -> ModelEntry {
    ModelEntry {
        model_id: model_id.to_owned(),
        days_json: Some(json!(D)),
        max_prompt: Some(max_prompt),
        context_window_json: None,
    }
}

/// Teach the store the opus family: opus-4-8 (what the requests ask for)
/// and opus-5 (the learned newest), proven at `max_prompt`.
fn seed_opus(store: &Store, max_prompt: i64) {
    store
        .upsert_model(&entry("claude-opus-5", max_prompt))
        .expect("seed opus-5");
    store
        .upsert_model(&entry("claude-opus-4-8", max_prompt))
        .expect("seed opus-4-8");
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
    }
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// A cold lane is moved onto the family's learned newest. The upstream
/// body is the request with the model value swapped — every other byte
/// identical, cache_control included — and the forced provenance lands
/// on the row and in the lane record.
#[tokio::test]
async fn a_cold_lane_moves_to_the_learned_newest() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    seed_opus(&store, 200_000);

    let body = tools_body("claude-opus-4-8", 2);
    // Cold: idle two hours (an unrecorded TTL tier is the long one), a
    // prefix below the cold gate's 175k notice bar so nothing
    // interrupts the request.
    store
        .upsert_lane(&poisoned_lane(&body, now_ms() - 2 * 3_600_000, 174_000))
        .expect("poison lane");
    let addr = spawn_toker(config, store.clone()).await;

    let response = post_messages(addr, "ccses-42", &body).await;
    assert_eq!(response.status(), StatusCode::OK);

    // The transform's prefix stability: only the model token's bytes
    // changed, cache_control and all else byte-identical.
    let expected = with_model(&body, "claude-opus-4-8", "claude-opus-5");
    let captured = mock.captured();
    assert_eq!(captured.len(), 1, "the request reached upstream");
    assert_eq!(
        captured[0],
        Bytes::from(expected),
        "upstream body: the model swapped, everything else byte-identical"
    );

    // The row: the three identities stay distinct, and the forced
    // provenance records the move.
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, None, "a real measurement, not a proxy row");
    assert_eq!(rows[0].requested_model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(
        rows[0].effective_model.as_deref(),
        Some("claude-opus-4-8"),
        "routing rewrites nothing here: effective is still the asked model"
    );
    assert_eq!(rows[0].model.as_deref(), Some("claude-opus-5"), "served");
    assert_eq!(rows[0].forced_from.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(rows[0].forced_to.as_deref(), Some("claude-opus-5"));

    // The lane keeps the upgrade: its cache lives on the new model now.
    let lane = store
        .load_lane(&lane_key_of(&body))
        .expect("load lane")
        .expect("the lane exists");
    assert_eq!(lane.forced_from.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(lane.forced_to.as_deref(), Some("claude-opus-5"));
}

/// An openrouter-routed lane is never moved, however cold: the newest
/// model comes from the Anthropic catalogue, and a bare `claude-*` id at
/// openrouter is not the model the user picked.
#[tokio::test]
async fn an_openrouter_lane_is_never_moved() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    seed_opus(&store, 200_000);

    let body = tools_body("openrouter/claude-opus-4-8", 2);
    store
        .upsert_lane(&poisoned_lane(&body, now_ms() - 2 * 3_600_000, 174_000))
        .expect("poison lane");
    let addr = spawn_toker(config, store.clone()).await;

    let response = post_messages(addr, "ccses-42", &body).await;
    assert_eq!(response.status(), StatusCode::OK);

    let captured = mock.captured();
    assert_eq!(captured.len(), 1, "the request reached upstream");
    assert_eq!(
        captured[0],
        Bytes::from(with_model(
            &body,
            "openrouter/claude-opus-4-8",
            "claude-opus-4-8"
        )),
        "only the routing prefix is stripped"
    );
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows[0].provider.as_deref(), Some("openrouter"));
    assert_eq!(rows[0].forced_from, None);
    assert_eq!(rows[0].forced_to, None);
}

/// A warm lane has a cache to lose: the request forwards byte-identical
/// and records no provenance.
#[tokio::test]
async fn a_warm_lane_with_cache_to_lose_is_left_alone() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    seed_opus(&store, 200_000);

    let body = tools_body("claude-opus-4-8", 2);
    // Warm: the lane spoke a second ago.
    store
        .upsert_lane(&poisoned_lane(&body, now_ms() - 1_000, 174_000))
        .expect("poison lane");
    let addr = spawn_toker(config, store.clone()).await;

    let response = post_messages(addr, "ccses-42", &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0],
        Bytes::from(body.clone()),
        "a warm lane forwards byte-identical"
    );

    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].forced_from, None);
    assert_eq!(rows[0].forced_to, None);
}

/// The maxPrompt guard is empirical: a target never observed holding a
/// conversation this size is not sent one, cold lane or not.
#[tokio::test]
async fn a_conversation_the_target_has_not_held_is_not_moved() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    // opus-5 proven at 50k only; the lane carries 174k.
    seed_opus(&store, 50_000);

    let body = tools_body("claude-opus-4-8", 2);
    store
        .upsert_lane(&poisoned_lane(&body, now_ms() - 2 * 3_600_000, 174_000))
        .expect("poison lane");
    let addr = spawn_toker(config, store.clone()).await;

    let response = post_messages(addr, "ccses-42", &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0],
        Bytes::from(body.clone()),
        "the target has never held 174k: no move"
    );

    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].forced_from, None, "no provenance for a refusal");
}

/// A sticky lane keeps its recorded target across requests — even once
/// the election has moved on to a newer model.
#[tokio::test]
async fn a_sticky_lane_moves_to_its_recorded_target() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    // The election now names opus-5-5; the lane was moved onto opus-5
    // and its cache lives THERE.
    seed_opus(&store, 200_000);
    store
        .upsert_model(&entry("claude-opus-5-5", 200_000))
        .expect("seed opus-5-5");

    let body = tools_body("claude-opus-4-8", 2);
    // Warm, and already moved.
    let mut lane = poisoned_lane(&body, now_ms() - 1_000, 174_000);
    lane.forced_from = Some("claude-opus-4-8".to_owned());
    lane.forced_to = Some("claude-opus-5".to_owned());
    store.upsert_lane(&lane).expect("poison lane");
    let addr = spawn_toker(config, store.clone()).await;

    // First request: the recorded target, not the newer election.
    let response = post_messages(addr, "ccses-42", &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let expected = with_model(&body, "claude-opus-4-8", "claude-opus-5");
    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0],
        Bytes::from(expected),
        "the lane's recorded target, not the election's newer opus-5-5"
    );

    // The lane kept its record from the response; the next request in the
    // same conversation moves the same way.
    let response = post_messages(addr, "ccses-42", &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let captured = mock.captured();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[1], captured[0], "the second request sticks too");

    let rows = wait_for_rows(&store, 2).await;
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.forced_from.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(row.forced_to.as_deref(), Some("claude-opus-5"));
    }
}

/// An unknown lane with a barely-started conversation moves (the
/// message-count condition); one with a real history does not. The
/// asked model is marked recently served via the startup seed, so the
/// served-recency condition cannot be what qualifies.
#[tokio::test]
async fn an_unknown_lane_with_a_short_conversation_moves() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    seed_opus(&store, 200_000);
    // A served row for the asked model, moments ago: `servedOn` seeded
    // from the ledger tail at startup, so the model reads as warm and
    // only the message count can qualify the lane.
    let mut served = bare_row(now_ms());
    served.raw_model = Some("claude-opus-4-8".to_owned());
    served.model = Some("claude-opus-4-8".to_owned());
    store.record_request(&served).expect("seed served row");
    let addr = spawn_toker(config, store.clone()).await;

    // Two messages: a conversation that has barely started.
    let short = tools_body("claude-opus-4-8", 2);
    let response = post_messages(addr, "ccses-42", &short).await;
    assert_eq!(response.status(), StatusCode::OK);
    // Three messages, a different session (a different lane): a real
    // history, recently-served model — nothing qualifies.
    let long = tools_body("claude-opus-4-8", 3);
    let response = post_messages(addr, "ccses-43", &long).await;
    assert_eq!(response.status(), StatusCode::OK);

    let captured = mock.captured();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[0],
        Bytes::from(with_model(&short, "claude-opus-4-8", "claude-opus-5")),
        "the short conversation moved"
    );
    assert_eq!(
        captured[1],
        Bytes::from(long.clone()),
        "the real history forwarded byte-identical"
    );

    // Three rows: the served-row seed plus the two requests.
    let rows = wait_for_rows(&store, 3).await;
    assert_eq!(rows.len(), 3);
    let moved = rows
        .iter()
        .find(|row| row.session_id.as_deref() == Some("ccses-42"))
        .expect("the short conversation's row");
    assert_eq!(moved.forced_from.as_deref(), Some("claude-opus-4-8"));
    let unmoved = rows
        .iter()
        .find(|row| row.session_id.as_deref() == Some("ccses-43"))
        .expect("the real history's row");
    assert_eq!(unmoved.forced_from, None);
}

/// The openai path never meets the force machinery: a poisoned store cannot
/// change its routed model, and its rows carry no force provenance.
#[tokio::test]
async fn the_openai_path_never_meets_the_force_machinery() {
    let (mock, upstream) = spawn_mock().await;
    let config = test_config(upstream);
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    // Poisoned: had the middleware leaked into the openai path, this
    // store would rewrite the request.
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
}
