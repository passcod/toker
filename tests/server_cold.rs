//! End-to-end cold-gate and compaction-retarget tests: a mock anthropic
//! upstream (capturing every request byte) behind the real toker router,
//! with lanes poisoned directly in the store — the gate reads idle time
//! and prompt size per lane, so the lane table is the whole fixture.
//!
//! Asserts the unit's contract end to end:
//!
//! - a cold lane is answered with the synthetic notice turn, nothing
//!   reaches upstream, a `cold` row lands (idleMs/lastPrompt/reqMessages/
//!   compactTarget/quota figures; NO rate_limits), and the lane is
//!   marked noticed — the resend in the same idle spell forwards;
//! - a would-fire notice the quota outlook can absorb is withheld: a
//!   `cold-quiet` row lands and the request forwards;
//! - a summarising request (a real compaction among them) is exempt from
//!   the notice, and a COLD compaction is retargeted — the upstream body
//!   is the transformed bytes (model swapped, no cache_control anywhere,
//!   the mid-conversation system message merged), asserted byte for
//!   byte, with the downgrade provenance on the row;
//! - a WARM compaction passes through untouched;
//! - a model whose listing entry prices its cache writes at nothing is
//!   exempt from the notice: the request forwards and a `cold-quiet`
//!   row with `writesFree` records the withholding (the
//!   withheld-notice-with-reason discipline).

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
use toker::store::{Lane, ModelEntry, RequestRow, RowKind, Store};

/// An env name no test ever sets, so nothing resolves and nothing injects.
const UNSET_KEY_ENV: &str = "TOKER_TEST_KEY_UNSET_COLD_7A";

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
        "id": "msg_test_cold",
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

/// One mock standing for the anthropic backend.
async fn spawn_mock() -> (MockState, reqwest::Url) {
    let state = MockState::default();
    let app = Router::new()
        .route("/v1/messages", post(mock_messages))
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
        .join(format!("server-cold-{name}-{}-{n}", std::process::id()));
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
        default_backend_openai_chat: "openrouter".to_owned(),
        openrouter: OpenRouterConfig {
            // Only the openai-isolation test routes here.
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
/// the writes-free exemption's fixture. The background refresh task
/// only spawns in `serve`, which tests never run, so this is the only
/// way a test's server sees a catalogue.
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

async fn post_messages(addr: SocketAddr, body: &[u8]) -> reqwest::Response {
    client()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-claude-code-session-id", "ccses-42")
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(body.to_vec())
        .send()
        .await
        .expect("messages request")
}

/// Poll the ledger until it holds at least `count` rows.
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

async fn assert_no_more_rows(store: &Store, count: usize) {
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        store.requests_since(0, 1000).expect("rows").len(),
        count,
        "no further rows landed"
    );
}

/// The tools body the anthropic tests use (streaming, so the gates answer
/// with the SSE turn): without tools there is no lane, just a session —
/// the lane rule: a session is not a cache entry.
fn tools_body(model: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": model,
        "stream": true,
        "tools": [
            {"name": "Read", "input_schema": {"type": "object"}},
            {"name": "Bash", "input_schema": {"type": "object"}},
        ],
        "messages": [{"role": "user", "content": "Hi"}],
    }))
    .expect("serialise tools body")
}

fn lane_key_of(body: &[u8]) -> String {
    let shape = IrRequest::parse(body).expect("parse").anthropic().shape();
    format!("ccses-42|{}", shape.tools_hash)
}

/// Poison the lane this body belongs to: a 200k prefix, idle for
/// `idle_ms`, never noticed.
fn poison_cold_lane(store: &Store, body: &[u8], idle_ms: i64) {
    store
        .upsert_lane(&Lane {
            key: lane_key_of(body),
            session_id: Some("ccses-42".to_owned()),
            tools_hash: Some(
                IrRequest::parse(body)
                    .expect("parse")
                    .anthropic()
                    .shape()
                    .tools_hash,
            ),
            updated_ms: now_ms() - idle_ms,
            prompt_tokens: Some(200_000),
            ttl: None,
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
        })
        .expect("poison lane");
}

fn bare_row(ts_ms: i64) -> RequestRow {
    RequestRow {
        id: None,
        ts_ms,
        duration_ms: None,
        kind: None,
        frontend: None,
        // The seeded measurements are the subscription's: the outlook
        // reads only the routed backend's own rows.
        provider: Some("anthropic_sub".to_owned()),
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

/// The synthetic ledger the quiet test seeds: four complete 5-hour
/// windows the weight fit can price (utilisation advancing 0.10 → 0.40
/// over 21 rows each, all claude-opus-5 at 50k fresh tokens a row), plus
/// the two burn readings of the window now running (0.50 → 0.52,
/// resetting 3.5h out). The unit tests pin the same shapes offline; here
/// they make the live wiring produce a measurable, on-track outlook.
fn seed_fit_rows(store: &Store) {
    let now = now_ms();
    let hour = 3_600_000i64;
    let minute = 60_000i64;
    let mut rows = Vec::new();
    for window in 0..4i64 {
        let reset_ms = now - 90 * minute - 300 * minute * window;
        let reset_s = reset_ms / 1000;
        let mut row = bare_row(reset_ms - 5 * hour + 10 * minute);
        row.model = Some("claude-opus-5".to_owned());
        row.input = Some(50_000);
        row.rate_limits = Some(json!({"util5h": 0.10, "reset5h": reset_s}));
        rows.push(row);
        for step in 1..=20 {
            let mut row = bare_row(reset_ms - 5 * hour + 10 * minute + step * 14 * minute);
            row.model = Some("claude-opus-5".to_owned());
            row.input = Some(50_000);
            row.rate_limits =
                Some(json!({"util5h": 0.10 + 0.30 * step as f64 / 20.0, "reset5h": reset_s}));
            rows.push(row);
        }
    }
    let reset_s = (now + 210 * minute) / 1000;
    for (at, util) in [(now - 20 * minute, 0.50), (now - 2_000, 0.52)] {
        let mut row = bare_row(at);
        row.model = Some("claude-opus-5".to_owned());
        row.input = Some(1);
        row.rate_limits = Some(json!({"util5h": util, "reset5h": reset_s}));
        rows.push(row);
    }
    for row in rows {
        store.record_request(&row).expect("seed row");
    }
}

/// A compaction-shaped body: the summarisation instruction line-anchored
/// in the last user message, the session's tool set, cache_control in all
/// three positions, and one mid-conversation system message to merge.
fn compaction_body() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "claude-opus-5",
        "stream": false,
        "system": [
            {"type": "text", "text": "You are careful.", "cache_control": {"type": "ephemeral"}},
        ],
        "tools": [
            {"name": "Read", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral"}},
            {"name": "Bash", "input_schema": {"type": "object"}},
        ],
        "messages": [
            {"role": "user", "content": "Earlier work."},
            {"role": "system", "content": [{"type": "text", "text": "[reminder]"}]},
            {"role": "user", "content": [
                {"type": "text", "text": "Your task is to create a detailed summary of the conversation so far.",
                 "cache_control": {"type": "ephemeral"}},
            ]},
        ],
    }))
    .expect("serialise compaction body")
}

/// Teach the store a sonnet the lane's prefix fits on, so the retarget
/// has a target to resolve.
fn seed_sonnet(store: &Store) {
    store
        .upsert_model(&ModelEntry {
            model_id: "claude-sonnet-5".to_owned(),
            days_json: Some(json!([
                "2026-09-26",
                "2026-09-27",
                "2026-09-28",
                "2026-09-29",
                "2026-09-30",
                "2026-10-01",
                "2026-10-02",
                "2026-10-03",
            ])),
            max_prompt: Some(1_000_000),
            context_window_json: None,
        })
        .expect("seed model");
}

fn extra_of(row: &RequestRow) -> Value {
    row.extra.clone().expect("the payload rides `extra`")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cold_lane_gets_the_notice_a_cold_row_and_no_upstream_then_the_resend_forwards() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream)).await;

    let body = tools_body("claude-opus-5");
    poison_cold_lane(&store, &body, 2 * 3_600_000);

    let response = post_messages(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/event-stream")),
        "the notice is served as the synthetic SSE turn, never an error status"
    );
    let bytes = response.bytes().await.expect("notice bytes");
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.starts_with("event: message_start\n"));
    assert!(
        text.contains("prompt cache had expired after 2h idle"),
        "{text}"
    );
    assert!(
        text.contains("would re-read 200,000 tokens as fresh input"),
        "{text}"
    );
    assert!(text.contains("Fired once for that idle spell.]"), "{text}");
    // No model entry exists to resolve a compact target onto, so the
    // notice stays silent about one — an unarmed proxy promising a cheap
    // compaction would be the feature lying about its configuration.
    assert!(!text.contains("The proxy would run it on"), "{text}");
    // The notice rides the configured style's wrapper, like the quota
    // gate's (the GFM alert is the default).
    assert!(text.contains("> [!NOTE]"), "{text}");

    // Nothing reached upstream.
    assert!(mock.captured().is_empty(), "the gate answered, not the API");

    let rows = wait_for_rows(&store, 1).await;
    let row = &rows[0];
    assert_eq!(row.kind, Some(RowKind::Cold));
    assert_eq!(row.session_id.as_deref(), Some("ccses-42"));
    assert_eq!(row.provider.as_deref(), Some("anthropic_sub"));
    assert_eq!(row.cold_on, Some(true));
    assert_eq!(row.gate_on, Some(true));
    // NO rate_limits: nothing reached upstream, so the only meters
    // available would be the proxy's own stale copy.
    assert_eq!(row.rate_limits, None);
    let extra = extra_of(row);
    assert!(
        (extra["idleMs"].as_i64().expect("idleMs") - 7_200_000i64).abs() < 60_000,
        "{}",
        extra
    );
    assert_eq!(extra["lastPrompt"], json!(200_000));
    assert_eq!(extra["reqMessages"], json!(1));
    assert_eq!(extra["compactTarget"], Value::Null);
    assert_eq!(
        extra["quotaExtra"],
        Value::Null,
        "a young log has no weights"
    );
    assert_eq!(extra["quotaMeter"], Value::Null);

    // The lane remembers it has spoken; `at` did not move — the
    // compaction the user runs after reading the notice must still be
    // seen as cold.
    let lane = store
        .load_lane(&lane_key_of(&body))
        .expect("load")
        .expect("the poisoned lane");
    assert!(
        lane.noticed_at
            .is_some_and(|noticed| noticed > lane.updated_ms)
    );
    assert_eq!(lane.prompt_tokens, Some(200_000));

    // The resend in the same idle spell forwards: sending the request
    // again IS the override.
    let response = post_messages(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body");
    assert_eq!(
        bytes.as_ref(),
        non_stream_body("claude-opus-5").as_slice(),
        "the resend is served by the upstream, not the gate"
    );

    let rows = wait_for_rows(&store, 2).await;
    assert_eq!(
        rows.iter()
            .filter(|row| row.kind == Some(RowKind::Cold))
            .count(),
        1,
        "once per idle spell: no second notice row"
    );
    let measurement = rows
        .iter()
        .find(|row| row.kind.is_none())
        .expect("measurement");
    assert_eq!(measurement.model.as_deref(), Some("claude-opus-5"));
    assert_no_more_rows(&store, 2).await;
}

#[tokio::test]
async fn an_on_track_outlook_withholds_the_notice_and_records_cold_quiet() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream)).await;

    // Seed the ledger the outlook needs: enough complete windows to fit
    // the re-read's weight, and a live 5-hour window the burn measures
    // heading nowhere near its wall.
    seed_fit_rows(&store);
    let body = tools_body("claude-opus-5");
    poison_cold_lane(&store, &body, 2 * 3_600_000);

    let response = post_messages(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body");
    assert_eq!(
        bytes.as_ref(),
        non_stream_body("claude-opus-5").as_slice(),
        "the request forwards — the notice was withheld"
    );
    assert_eq!(mock.captured().len(), 1, "it really went upstream");

    // 86 seeded rows + the cold-quiet row + the measurement.
    let rows = wait_for_rows(&store, 88).await;
    let quiet = rows
        .iter()
        .find(|row| row.kind == Some(RowKind::ColdQuiet))
        .expect("a withheld notice is recorded");
    assert_eq!(quiet.session_id.as_deref(), Some("ccses-42"));
    assert_eq!(quiet.rate_limits, None, "no stale meters on a proxy row");
    assert_eq!(quiet.cold_on, Some(true));
    let extra = extra_of(quiet);
    assert_eq!(extra["lastPrompt"], json!(200_000));
    assert!(
        (extra["quotaExtra"].as_f64().expect("the fitted share") - 0.06).abs() < 1e-6,
        "{}",
        extra
    );
    assert_eq!(extra["quotaBound"], json!(false));
    assert!((extra["util5h"].as_f64().expect("the measured util") - 0.52).abs() < 1e-9);
    // And no notice row: the suppression is the quiet row, visible —
    // silence must be distinguishable from breakage.
    assert!(
        !rows.iter().any(|row| row.kind == Some(RowKind::Cold)),
        "no notice fired"
    );
    // The lane was NOT marked noticed: nothing was said, and a later
    // request in the same idle spell is judged again against meters that
    // may have tightened. (The served response moved `at`; the notice
    // memory is what must be absent.)
    let lane = store
        .load_lane(&lane_key_of(&body))
        .expect("load")
        .expect("lane");
    assert_eq!(
        lane.noticed_at, None,
        "a withheld notice is not a spoken one"
    );
    assert_no_more_rows(&store, 88).await;
}

#[tokio::test]
async fn a_cold_compaction_is_exempt_from_the_notice_and_retargeted_upstream() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream)).await;

    let body = compaction_body();
    poison_cold_lane(&store, &body, 2 * 3_600_000);
    seed_sonnet(&store);

    let response = post_messages(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body");
    assert_eq!(
        bytes.as_ref(),
        non_stream_body("claude-sonnet-5").as_slice(),
        "the upstream served the RETARGETED request — it echoes the swapped model"
    );

    // The upstream body is the transform's bytes, asserted exactly: the
    // model changed, every cache_control went, and the mid-conversation
    // system message merged into the PRECEDING user turn as a
    // [system]-prefixed block.
    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].as_ref(),
        serde_json::to_vec(&json!({
            "model": "claude-sonnet-5",
            "stream": false,
            "system": [
                {"type": "text", "text": "You are careful."},
            ],
            "tools": [
                {"name": "Read", "input_schema": {"type": "object"}},
                {"name": "Bash", "input_schema": {"type": "object"}},
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "Earlier work."},
                    {"type": "text", "text": "[system] [reminder]"},
                ]},
                {"role": "user", "content": [
                    {"type": "text", "text": "Your task is to create a detailed summary of the conversation so far."},
                ]},
            ],
        }))
        .expect("serialise expected upstream body")
        .as_slice(),
        "the retargeted body reaches upstream byte for byte"
    );

    let rows = wait_for_rows(&store, 1).await;
    let row = rows
        .iter()
        .find(|row| row.kind.is_none())
        .expect("the measurement row");
    assert_eq!(
        row.model.as_deref(),
        Some("claude-sonnet-5"),
        "the response model is the retarget's"
    );
    assert_eq!(row.downgraded_from.as_deref(), Some("claude-opus-5"));
    assert_eq!(row.downgraded_to.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(row.cache_stripped, Some(true), "three breakpoints went");
    assert_eq!(row.system_merged, Some(true), "one system message merged");
    // The summarising request was never interrupted by the cold gate:
    // the gate exists to advise compaction, so it never stops one.
    assert!(
        !rows.iter().any(|row| row.kind == Some(RowKind::Cold)),
        "no notice for a summarising request"
    );
    assert!(
        !rows.iter().any(|row| row.kind == Some(RowKind::ColdQuiet)),
        "no quiet row either"
    );
    assert_no_more_rows(&store, 1).await;
}

#[tokio::test]
async fn a_warm_compaction_passes_through_untouched() {
    let (mock, upstream) = spawn_mock().await;
    let (addr, store) = spawn_toker(test_config(upstream)).await;

    let body = compaction_body();
    // Idle zero: warm. On a warm lane the breakpoints are what earn the
    // free read — measured, the delta actually written is a few hundred
    // tokens against hundreds of thousands read back — so the retarget
    // declines its cold licence.
    poison_cold_lane(&store, &body, 0);
    seed_sonnet(&store);

    let response = post_messages(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body");
    assert_eq!(
        bytes.as_ref(),
        non_stream_body("claude-opus-5").as_slice(),
        "the upstream served the request on the model it asked for"
    );

    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].as_ref(),
        body.as_slice(),
        "the body is untouched"
    );

    let rows = wait_for_rows(&store, 1).await;
    let row = rows
        .iter()
        .find(|row| row.kind.is_none())
        .expect("measurement");
    assert_eq!(row.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(row.downgraded_from, None, "no retarget on a warm lane");
    assert_eq!(row.downgraded_to, None);
    assert_eq!(row.cache_stripped, None);
    assert_eq!(row.system_merged, None);
}

#[tokio::test]
async fn a_writes_free_model_exempts_the_anthropic_gate_and_records_cold_quiet() {
    let (mock, upstream) = spawn_mock().await;
    let mut config = test_config(upstream);
    // A model-mapped route: the map (whose rewrite stage runs AFTER the
    // cold gate) moves claude-opus-5 onto a model whose listing entry
    // prices its cache writes at nothing — the exemption must see the
    // identity the upstream will actually bill.
    config.anthropic_sub.model_map = toker::middleware::model_map::parse_model_map(
        r#"{"model:claude-opus-5":"free/claude-sonnet"}"#,
    )
    .expect("the map parses");
    // The backend's fetched catalogue: an entry whose pricing object is
    // itemised with the write price OMITTED — the documented free
    // signal. The real anthropic presence list carries no pricing at
    // all (unknown → the gate fires), so this fixture is the
    // cross-protocol wiring the exemption exists for.
    let catalog = toker::catalog::FetchedCatalog {
        fetched_at_ms: 0,
        models: vec![toker::catalog::FetchedModel {
            id: "free/claude-sonnet".to_owned(),
            context_window: None,
            raw: json!({
                "id": "free/claude-sonnet",
                "pricing": {
                    "prompt": "0.00000011",
                    "completion": "0.00000043",
                    "input_cache_read": "0.0000000022"
                }
            }),
        }],
    };
    let (addr, store) = spawn_toker_cataloged(config, "anthropic", catalog).await;

    // The same cold 200k lane the notice test fires on.
    let body = tools_body("claude-opus-5");
    poison_cold_lane(&store, &body, 2 * 3_600_000);

    let response = post_messages(addr, &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.expect("body");
    assert_eq!(
        bytes.as_ref(),
        non_stream_body("free/claude-sonnet").as_slice(),
        "the request forwarded on the MAPPED model — no interruption"
    );
    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    let sent: Value = serde_json::from_slice(&captured[0]).expect("the forwarded body");
    assert_eq!(sent["model"], "free/claude-sonnet");

    // The skip is visible: a cold-quiet row with writesFree — silence
    // distinguishable from breakage — and no notice row.
    let rows = wait_for_rows(&store, 2).await;
    let quiet = rows
        .iter()
        .find(|row| row.kind == Some(RowKind::ColdQuiet))
        .expect("the withheld notice is recorded");
    assert_eq!(quiet.session_id.as_deref(), Some("ccses-42"));
    assert_eq!(quiet.provider.as_deref(), Some("anthropic_sub"));
    assert_eq!(quiet.cold_on, Some(true));
    assert_eq!(quiet.rate_limits, None, "no stale meters on a proxy row");
    let extra = extra_of(quiet);
    assert_eq!(extra["writesFree"], json!(true), "{extra}");
    assert_eq!(extra["lastPrompt"], json!(200_000));
    assert_eq!(
        extra["quotaExtra"],
        Value::Null,
        "the exemption answered before the outlook was worth measuring"
    );
    assert!(
        !rows.iter().any(|row| row.kind == Some(RowKind::Cold)),
        "no notice fired"
    );

    // The lane was NOT marked noticed (nothing was said); its clock
    // moved with the served response.
    let lane = store
        .load_lane(&lane_key_of(&body))
        .expect("load")
        .expect("lane");
    assert_eq!(
        lane.noticed_at, None,
        "a withheld notice is not a spoken one"
    );
    assert_no_more_rows(&store, 2).await;
}
