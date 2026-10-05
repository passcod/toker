//! End-to-end sleep-lock tests: the real toker router with a **fake**
//! spawner injected — the tests on this machine must never take a real
//! idle-sleep lock, so nothing here spawns systemd-inhibit or
//! gnome-session-inhibit. The fake records every spawn and kill, and
//! the ledger's `awake` rows carry the held/want/until/reason the state
//! machine decided.
//!
//! Asserts the unit's contract end to end, against both usage paths:
//!
//! - a live lane holds the lock: the inhibitor spawns, one `awake` row
//!   lands, and the response that refreshes the lane keeps it held
//!   (no double spawn, no release row);
//! - lane expiry + nothing in flight releases it: the inhibitor is
//!   killed and a released row lands — and the release is driven by an
//!   openai request's end-of-exchange evaluation, proving the two paths
//!   share the one lock;
//! - a request in flight holds with no live lanes at all (each path
//!   proven separately — anthropic streamed, openai buffered), and the
//!   hold ends with the exchange;
//! - a ping lane never holds: a ping request that refreshes a live
//!   ping lane spawns nothing and writes no row;
//! - `awake = false` never holds, never spawns, never writes a row;
//! - an error-path request (no upstream at all) still cycles the
//!   in-flight count cleanly — the guard's Drop is the decrement, so
//!   two consecutive failures produce two full hold/release cycles
//!   (a leaked count would hold the lock forever and skip the second);
//! - a stalled upstream on a live connection releases the lock once the
//!   upstream idle timeout fires, without the client hanging up.

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
use toker::middleware::awake::{InhibitCommand, InhibitLock, LockSpawner};
use toker::server::Server;
use toker::store::{Lane, RequestRow, RowKind, Store};

/// An env name no test ever sets, so nothing resolves and nothing injects.
const UNSET_KEY_ENV: &str = "TOKER_TEST_KEY_UNSET_AWAKE_7A";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_millis() as i64
}

const HOUR: i64 = 3_600_000;

// ---------------------------------------------------------------------------
// The fake spawner: records spawns and kills, takes no real lock
// ---------------------------------------------------------------------------

/// What the tests observe: every spawn attempt and every release kill.
#[derive(Clone, Default)]
struct AwakeProbe {
    spawns: Arc<AtomicU64>,
    kills: Arc<AtomicU64>,
}

impl AwakeProbe {
    fn spawns(&self) -> u64 {
        self.spawns.load(Ordering::SeqCst)
    }
    fn kills(&self) -> u64 {
        self.kills.load(Ordering::SeqCst)
    }
}

struct ProbeSpawner(AwakeProbe);

impl LockSpawner for ProbeSpawner {
    fn spawn(&mut self, _command: &InhibitCommand) -> Result<Box<dyn InhibitLock>, String> {
        self.0.spawns.fetch_add(1, Ordering::SeqCst);
        // Never a real process: the fake lock stands until released and
        // never "exits on its own".
        Ok(Box::new(ProbeLock(self.0.clone())))
    }
}

struct ProbeLock(AwakeProbe);

impl InhibitLock for ProbeLock {
    fn kill(&mut self) {
        self.0.kills.fetch_add(1, Ordering::SeqCst);
    }
    fn exited(&mut self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// The mock upstream and the toker server
// ---------------------------------------------------------------------------

/// Every request the mocks see, for asserting what reached upstream.
#[derive(Clone, Default)]
struct MockState {
    requests: Arc<Mutex<Vec<Bytes>>>,
}

fn raw_response(status: StatusCode, content_type: &'static str, body: Bytes) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type),
    );
    response
}

/// A complete anthropic SSE turn with usage in both events, so a
/// forwarded completion records a measurement row and moves its lane.
fn sse_turn() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":9,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":400}}}\n\n",
    );
    body.extend_from_slice(
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":40}}\n\n",
    );
    body.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    body
}

async fn mock_messages(State(mock): State<MockState>, request: Request) -> Response {
    let body = axum::body::to_bytes(request.into_body(), 64 * 1024 * 1024)
        .await
        .expect("mock reads body");
    mock.requests.lock().unwrap().push(body.clone());
    let model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|json| json.get("model").and_then(Value::as_str).map(str::to_owned));
    if model.as_deref() == Some("stall") {
        // The turn opens, then the upstream goes silent on a live
        // connection: never another byte, never an end.
        let turn = sse_turn();
        let first_event = turn
            .windows(2)
            .position(|pair| pair == b"\n\n")
            .expect("the turn has events")
            + 2;
        let opening = Bytes::copy_from_slice(&turn[..first_event]);
        let body = Body::from_stream(futures::StreamExt::chain(
            futures::stream::iter([Ok::<_, std::io::Error>(opening)]),
            futures::stream::pending(),
        ));
        let mut response = Response::new(body);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/event-stream"),
        );
        return response;
    }
    raw_response(StatusCode::OK, "text/event-stream", Bytes::from(sse_turn()))
}

async fn mock_chat(State(mock): State<MockState>, request: Request) -> Response {
    let body = axum::body::to_bytes(request.into_body(), 64 * 1024 * 1024)
        .await
        .expect("mock reads body");
    mock.requests.lock().unwrap().push(body);
    raw_response(
        StatusCode::OK,
        "application/json",
        Bytes::from_static(
            br#"{"id":"chatcmpl-test","choices":[{"message":{"role":"assistant","content":"hi"}}],
                "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
        ),
    )
}

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

fn test_dir(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from("/tmp/opencode")
        .join(format!("server-awake-{name}-{}-{n}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

/// The config both servers build on. `awake` is the caller's choice —
/// this suite is the one that exercises it on.
fn test_config(upstream: reqwest::Url, awake: bool) -> Config {
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
        default_backend_openai_chat: "openrouter".to_owned(),
        openrouter: OpenRouterConfig {
            upstream: openrouter_upstream,
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key: None,
        },
        default_backend_anthropic: "anthropic_sub".to_owned(),
        anthropic_sub: AnthropicSubConfig {
            model_map: None,
            upstream: upstream.clone(),
        },
        anthropic_api: AnthropicApiConfig {
            model_map: None,
            upstream,
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key: None,
        },
        // The codex backend's config: never routed to in these suites
        // (the responses frontend lands later), pointed at an upstream
        // that never answers and an auth path that never exists — no
        // test may touch a real login.
        codex_sub: CodexSubConfig {
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
        },
        gates: GatesConfig::default(),
        awake,
        transcript_roots: Vec::new(),
    }
}

/// toker over the injected fake spawner — never the real one; see the
/// module docs.
async fn spawn_toker(config: Config) -> (SocketAddr, Arc<Store>, AwakeProbe) {
    spawn_toker_idle(config, None).await
}

/// [`spawn_toker`], with the upstream idle timeout shortened so a stall
/// test need not wait the real five minutes.
async fn spawn_toker_idle(
    config: Config,
    idle: Option<std::time::Duration>,
) -> (SocketAddr, Arc<Store>, AwakeProbe) {
    let probe = AwakeProbe::default();
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    let mut server =
        Server::with_awake_spawner(config, store.clone(), Box::new(ProbeSpawner(probe.clone())))
            .expect("build server");
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
    (addr, store, probe)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("client")
}

/// POST /v1/messages, session-tagged; `ping` adds the ping header.
async fn post_messages(addr: SocketAddr, body: &[u8], ping: bool) -> reqwest::Response {
    let mut request = client()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-claude-code-session-id", "ccses-42")
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01");
    if ping {
        request = request.header("x-toker-ping", "1");
    }
    request
        .body(body.to_vec())
        .send()
        .await
        .expect("messages request")
}

async fn post_chat(addr: SocketAddr, body: &[u8]) -> reqwest::Response {
    client()
        .post(format!("http://{addr}/v1/chat/completions"))
        .header("x-toker-session", "openai-ses-42")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .await
        .expect("chat request")
}

/// A request with no session header: it can never key a lane (a tool-less
/// one with a session keys the empty tool list's), so whatever it holds
/// is its own in-flight count alone.
async fn post_sessionless(addr: SocketAddr, path: &str, body: &[u8]) -> reqwest::Response {
    client()
        .post(format!("http://{addr}{path}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(body.to_vec())
        .send()
        .await
        .expect("sessionless request")
}

/// Poll the ledger until it holds at least `count` awake rows.
async fn wait_for_awake_rows(
    store: &Store,
    count: usize,
) -> Vec<(bool, bool, Option<i64>, String)> {
    for _ in 0..200 {
        let rows = awake_rows(store);
        if rows.len() >= count {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    awake_rows(store)
}

/// The awake rows so far: (held, want, until, reason) in ledger order.
fn awake_rows(store: &Store) -> Vec<(bool, bool, Option<i64>, String)> {
    store
        .requests_since(0, 1000)
        .expect("read rows")
        .into_iter()
        .filter(|row| row.kind == Some(RowKind::Awake))
        .map(|row| {
            let RequestRow {
                extra: Some(extra), ..
            } = row
            else {
                panic!("an awake row carries its payload in extra");
            };
            (
                extra.get("held") == Some(&Value::Bool(true)),
                extra.get("want") == Some(&Value::Bool(true)),
                extra.get("until").and_then(Value::as_i64),
                extra
                    .get("reason")
                    .and_then(Value::as_str)
                    .expect("the reason is always present")
                    .to_owned(),
            )
        })
        .collect()
}

/// The lane key of a tools body, for poisoning.
fn lane_key_of(body: &[u8]) -> (String, String) {
    let shape = toker::ir::Request::parse(body)
        .expect("parse")
        .anthropic()
        .shape();
    let tools = shape.tools_hash;
    (format!("ccses-42|{tools}"), tools)
}

/// Poison the lane this body belongs to.
fn poison_lane(store: &Store, body: &[u8], at_ms: i64, ping: bool) {
    let (key, tools) = lane_key_of(body);
    store
        .upsert_lane(&Lane {
            key,
            session_id: Some("ccses-42".to_owned()),
            tools_hash: Some(tools),
            updated_ms: at_ms,
            prompt_tokens: Some(200_000),
            ttl: Some(HOUR),
            ping: ping.then_some(true),
            noticed_at: None,
            forced_from: None,
            forced_to: None,
        })
        .expect("poison lane");
}

/// A tools body (a lane exists), streaming — the client's own ask.
/// `tools` distinguishes lanes within the one session.
fn tools_body(model: &str, tools: &[&str]) -> Vec<u8> {
    let tools: Vec<serde_json::Value> = tools
        .iter()
        .map(|name| json!({"name": name, "input_schema": {"type": "object"}}))
        .collect();
    serde_json::to_vec(&json!({
        "model": model,
        "stream": true,
        "tools": tools,
        "messages": [{"role": "user", "content": "Hi"}],
    }))
    .expect("serialise tools body")
}

/// The main-agent lane the tests poison.
fn main_tools_body() -> Vec<u8> {
    tools_body("claude-opus-5", &["Read", "Bash"])
}

/// A second lane in the same session — the ping probe's own tools set.
fn probe_tools_body() -> Vec<u8> {
    tools_body("claude-haiku-4.5", &["Read", "Probe"])
}

/// A tools-less body: a session, but no lane — a session is not a cache
/// entry.
fn bare_body(model: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": model,
        "stream": true,
        "messages": [{"role": "user", "content": "Hi"}],
    }))
    .expect("serialise bare body")
}

fn chat_body() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "z-ai/glm-5.3",
        "messages": [{"role": "user", "content": "Hi"}],
    }))
    .expect("serialise chat body")
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_live_lane_holds_the_lock_and_writes_one_row() {
    let (mock, upstream) = spawn_mock().await;
    let _ = mock;
    let config = test_config(upstream, true);
    let (addr, store, probe) = spawn_toker(config).await;

    // A live lane: its cache would survive an hour past its last
    // response. The evaluation that first finds it must run with
    // NOTHING in flight — in the decision a request's own arrival would
    // read
    // "1 in flight" first (the in-flight check runs before
    // lanes), so the trigger here is a ping request on a second lane:
    // its response marks its own lane and re-evaluates at zero
    // in-flight, and the LIVE lane's name is what lands in the row.
    let body = main_tools_body();
    let poisoned_at = now_ms();
    poison_lane(&store, &body, poisoned_at - 1_000, false);

    let response = post_messages(addr, &probe_tools_body(), true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.expect("read body");
    assert!(text.contains("message_stop"), "the SSE turn completed");

    let rows = wait_for_awake_rows(&store, 1).await;
    assert_eq!(
        rows,
        vec![(
            true,
            true,
            Some(poisoned_at - 1_000 + HOUR),
            "1 live lane".to_owned()
        )],
        "held on the live lane, `until` its cache expiry"
    );
    assert_eq!(probe.spawns(), 1);
    assert_eq!(probe.kills(), 0);

    // A real request on the live lane — in flight while it runs, then
    // refreshing the lane — keeps the lock held: no release row, no
    // double spawn, no kill.
    let response = post_messages(addr, &body, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.text().await.expect("read body");

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        awake_rows(&store).len(),
        1,
        "a refreshed lane stays held: no further rows"
    );
    assert_eq!(probe.spawns(), 1, "no double spawn while held");
    assert_eq!(probe.kills(), 0);
}

#[tokio::test]
async fn lane_expiry_with_nothing_in_flight_releases_the_lock() {
    let (mock, upstream) = spawn_mock().await;
    let _ = mock;
    let config = test_config(upstream, true);
    let (addr, store, probe) = spawn_toker(config).await;

    // The hold: a live lane, taken via the ping-probe evaluation (zero
    // in-flight, as above).
    let body = main_tools_body();
    let poisoned_at = now_ms();
    poison_lane(&store, &body, poisoned_at - 1_000, false);
    let response = post_messages(addr, &probe_tools_body(), true).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.text().await.expect("read body");
    let rows = wait_for_awake_rows(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(probe.spawns(), 1);

    // The lane goes quiet for two hours. Nothing re-poisons it —
    // the table just ages.
    poison_lane(&store, &body, now_ms() - 2 * HOUR, false);

    // An openai request, sessionless and so lane-less: it is in flight
    // while it runs (so the lock is not released mid-exchange), and its
    // END is what finds the expired lane and nothing in flight — the
    // release, on the shared lock, from the other protocol's path.
    let response = post_sessionless(addr, "/v1/chat/completions", &chat_body()).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.text().await.expect("read body");

    let rows = wait_for_awake_rows(&store, 2).await;
    assert_eq!(
        rows,
        vec![
            (
                true,
                true,
                Some(poisoned_at - 1_000 + HOUR),
                "1 live lane".to_owned()
            ),
            (false, false, None, "no live lanes".to_owned()),
        ],
        "the expired lane releases the lock at the exchange's end"
    );
    assert_eq!(probe.spawns(), 1, "the openai request rode the held lock");
    assert_eq!(probe.kills(), 1, "release kills the inhibitor's group");
}

#[tokio::test]
async fn an_in_flight_anthropic_request_holds_with_no_live_lanes() {
    let (mock, upstream) = spawn_mock().await;
    let _ = mock;
    let config = test_config(upstream, true);
    let (addr, store, probe) = spawn_toker(config).await;

    // No lanes at all: the request itself is the hold, and the SSE
    // response's completion is the release.
    let response = post_sessionless(addr, "/v1/messages", &bare_body("claude-opus-5")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.expect("read body");
    assert!(text.contains("message_stop"));

    let rows = wait_for_awake_rows(&store, 2).await;
    assert_eq!(
        rows,
        vec![
            // An in-flight hold has no `until` — it
            // rests on something without an expiry.
            (true, true, None, "1 in flight".to_owned()),
            (false, false, None, "no live lanes".to_owned()),
        ],
        "in flight holds; the stream's end releases"
    );
    assert_eq!(probe.spawns(), 1);
    assert_eq!(probe.kills(), 1);
}

#[tokio::test]
async fn an_in_flight_openai_request_holds_with_no_live_lanes() {
    let (mock, upstream) = spawn_mock().await;
    let _ = mock;
    let config = test_config(upstream, true);
    let (addr, store, probe) = spawn_toker(config).await;

    let response = post_sessionless(addr, "/v1/chat/completions", &chat_body()).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.text().await.expect("read body");

    let rows = wait_for_awake_rows(&store, 2).await;
    assert_eq!(
        rows,
        vec![
            (true, true, None, "1 in flight".to_owned()),
            (false, false, None, "no live lanes".to_owned()),
        ],
        "a running request holds the machine awake regardless of protocol"
    );
    assert_eq!(probe.spawns(), 1);
    assert_eq!(probe.kills(), 1);
}

#[tokio::test]
async fn a_ping_lane_never_holds_the_lock() {
    let (mock, upstream) = spawn_mock().await;
    let _ = mock;
    let config = test_config(upstream, true);
    let (addr, store, probe) = spawn_toker(config).await;

    // A LIVE ping lane, one second old on the hour tier — and a ping
    // request that refreshes it. A ping opens a quota window on a timer;
    // its cache must not keep the machine up.
    let body = main_tools_body();
    poison_lane(&store, &body, now_ms() - 1_000, true);

    let response = post_messages(addr, &body, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.expect("read body");
    assert!(text.contains("message_stop"));

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        awake_rows(&store).is_empty(),
        "a ping lane never holds: no row ever lands"
    );
    assert_eq!(probe.spawns(), 0);
    assert_eq!(probe.kills(), 0);

    // And the lane is recorded as a ping, not dropped: it is excluded
    // from liveness, not forgotten.
    let (key, _) = lane_key_of(&body);
    let lane = store.load_lane(&key).expect("lane").expect("recorded");
    assert_eq!(lane.ping, Some(true));
}

#[tokio::test]
async fn awake_off_never_holds_never_spawns_never_records() {
    let (mock, upstream) = spawn_mock().await;
    let _ = mock;
    let config = test_config(upstream, false);
    let (addr, store, probe) = spawn_toker(config).await;

    // A live lane AND a request in flight, and the machinery stays
    // absent: `awake = false` is the off switch — no lock, ever.
    let body = main_tools_body();
    poison_lane(&store, &body, now_ms() - 1_000, false);
    let response = post_messages(addr, &body, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.text().await.expect("read body");

    let response = post_chat(addr, &chat_body()).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.text().await.expect("read body");

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(awake_rows(&store).is_empty());
    assert_eq!(probe.spawns(), 0);
    assert_eq!(probe.kills(), 0);
}

#[tokio::test]
async fn error_paths_never_leak_the_in_flight_count() {
    // No mock: every upstream send fails, so the exchange ends on the
    // 502 path — the guard's Drop is the only decrement, and two
    // consecutive failures must produce two full hold/release cycles.
    // A leaked count would hold the lock forever: the second request's
    // begin would find nothing to flip and no new row would land.
    let upstream: reqwest::Url = "http://127.0.0.1:9".parse().expect("unroutable url");
    let config = test_config(upstream, true);
    let (addr, store, probe) = spawn_toker(config).await;

    for _ in 0..2 {
        let response = post_messages(addr, &bare_body("claude-opus-5"), false).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    wait_for_awake_rows(&store, 4).await;

    let rows = awake_rows(&store);
    assert_eq!(
        rows,
        vec![
            (true, true, None, "1 in flight".to_owned()),
            (false, false, None, "no live lanes".to_owned()),
            (true, true, None, "1 in flight".to_owned()),
            (false, false, None, "no live lanes".to_owned()),
        ],
        "the count cycles cleanly across error paths"
    );
    assert_eq!(probe.spawns(), 2);
    assert_eq!(probe.kills(), 2);
}

#[tokio::test]
async fn a_stalled_upstream_releases_the_lock_after_the_idle_timeout() {
    let (_mock, upstream) = spawn_mock().await;
    let config = test_config(upstream, true);
    let (addr, store, probe) =
        spawn_toker_idle(config, Some(std::time::Duration::from_millis(300))).await;

    // The client stays connected throughout: with no idle timeout, the
    // stalled upstream kept the request in flight, and the lock held,
    // for as long as the connection lived.
    let response = post_messages(addr, &bare_body("stall"), false).await;
    assert_eq!(response.status(), StatusCode::OK);

    let rows = wait_for_awake_rows(&store, 2).await;
    assert_eq!(
        rows,
        vec![
            (true, true, None, "1 in flight".to_owned()),
            (false, false, None, "no live lanes".to_owned()),
        ],
        "the stall holds, then the idle timeout releases"
    );
    assert_eq!(probe.spawns(), 1);
    assert_eq!(probe.kills(), 1);
    // Released by the timeout, not by the client going away.
    drop(response);
}
