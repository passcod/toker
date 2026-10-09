//! End-to-end tests for `GET /_toker/session` — the attribution
//! plugin's query endpoint: the gate (403 without the control verb), the
//! 400 for a missing parameter, the zero-and-nulls shape for a session
//! with no rows, and the aggregate itself (billed sums, null-vs-zero,
//! per-provider ordering) driven against a seeded scratch DB, plus one
//! full round trip — a chat completion through the mock upstream, gated
//! by `x-session-id` (the header opencode sends), read back through the
//! endpoint.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use serde_json::Value;

use toker::config::{
    AnthropicApiConfig, AnthropicSubConfig, CodexSubConfig, Config, OpenRouterConfig,
};
use toker::server::Server;
use toker::store::{CostKind, RequestRow, RowKind, Store};

/// An env name no test ever sets, so nothing resolves and nothing injects.
const UNSET_KEY_ENV: &str = "TOKER_TEST_KEY_UNSET_SESSION_7C";

// ---------------------------------------------------------------------------
// The mock upstream (the server_proxy pattern — only the openai chat
// path is exercised here)
// ---------------------------------------------------------------------------

const NON_STREAM_BODY: &str = concat!(
    r#"{"id":"gen-test-ns","provider":"z-ai","model":"z-ai/glm-5.3","#,
    r#""object":"chat.completion","created":1760000600,"#,
    r#""choices":[{"index":0,"message":{"role":"assistant","content":"Done"},"finish_reason":"stop"}],"#,
    r#""usage":{"prompt_tokens":64,"completion_tokens":8,"total_tokens":72,"#,
    r#""prompt_tokens_details":{"cached_tokens":16},"#,
    r#""completion_tokens_details":{"reasoning_tokens":4},"#,
    r#""cost":0.000128,"cost_details":{"upstream":0.0001}}}"#,
);

async fn mock_chat(State(()): State<()>, request: Request) -> Response {
    let (_, body) = request.into_parts();
    let _ = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .expect("mock reads body");
    let mut response = Response::new(Body::from(NON_STREAM_BODY.as_bytes()));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/json".parse().expect("content type"),
    );
    response
}

async fn spawn_mock() -> reqwest::Url {
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_chat))
        .with_state(());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("mock binds");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock serves");
    });
    format!("http://{addr}/v1").parse().expect("upstream url")
}

// ---------------------------------------------------------------------------
// The toker server
// ---------------------------------------------------------------------------

fn test_config(upstream: reqwest::Url) -> Config {
    let anthropic_upstream: reqwest::Url = "http://127.0.0.1:9".parse().expect("upstream url");
    Config {
        port: 0,
        db_path: PathBuf::from(":memory:"),
        session_header_names: vec![
            "x-toker-session".to_owned(),
            "x-claude-code-session-id".to_owned(),
            "x-session-id".to_owned(),
        ],
        ping_header_name: "x-toker-ping".to_owned(),
        default_backend_openai_chat: Some("openrouter".to_owned()),
        openrouter: Some(OpenRouterConfig {
            upstream,
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key_keyring: false,
            api_key: None,
            picker: None,
        }),
        default_backend_anthropic: Some("anthropic_sub".to_owned()),
        anthropic_sub: Some(AnthropicSubConfig {
            model_map: None,
            upstream: anthropic_upstream.clone(),
        }),
        anthropic_api: Some(AnthropicApiConfig {
            model_map: None,
            upstream: anthropic_upstream,
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
        gates: toker::config::GatesConfig::default(),
        notices: toker::config::NoticesConfig::default(),
        awake: false,
        transcript_roots: Vec::new(),
    }
}

/// Spawn toker over a fresh scratch DB and hand back the store so tests
/// can seed rows directly — the endpoint is a ledger read, so the seed
/// *is* the scenario.
async fn spawn_toker(upstream: reqwest::Url) -> (SocketAddr, Arc<Store>) {
    let config = test_config(upstream);
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

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("client")
}

async fn get_session(addr: SocketAddr, query: &str, verb: Option<&str>) -> reqwest::Response {
    let mut request = client().get(format!("http://{addr}/_toker/session{query}"));
    if let Some(verb) = verb {
        request = request.header("x-toker-control", verb);
    }
    request.send().await.expect("session request")
}

// ---------------------------------------------------------------------------
// Row seeding (the bare_row pattern from the other server suites)
// ---------------------------------------------------------------------------

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
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_endpoint_is_gated_like_the_other_control_endpoints() {
    let upstream = spawn_mock().await;
    let (addr, _store) = spawn_toker(upstream).await;

    // No control header → 403; the wrong verb → 403. The gate answers
    // before the query is even looked at, so a bare page probe with a
    // session id learns nothing.
    let response = get_session(addr, "?session=ses-x", None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = get_session(addr, "?session=ses-x", Some("status")).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // The right verb answers, even for a session that does not exist.
    let response = get_session(addr, "?session=ses-x", Some("session")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json")),
        "the reply is JSON"
    );
    let value: Value = response.json().await.expect("session body");
    assert_eq!(value["session"], "ses-x", "the session id echoes back");
}

#[tokio::test]
async fn a_missing_session_parameter_is_a_client_bug() {
    let upstream = spawn_mock().await;
    let (addr, _store) = spawn_toker(upstream).await;

    for query in ["", "?other=ses-x", "?session"] {
        let response = get_session(addr, query, Some("session")).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "query {query:?} names no session"
        );
    }
}

#[tokio::test]
async fn an_absent_session_answers_zero_requests_with_nulls() {
    let upstream = spawn_mock().await;
    let (addr, store) = spawn_toker(upstream).await;

    // Rows exist — just not for the session asked about. An absent
    // session is indistinguishable from one that measured nothing, and
    // the answer is the same on purpose.
    let mut row = bare_row(1_000);
    row.session_id = Some("ses-real".to_owned());
    row.cost_usd = Some(0.01);
    row.cost_kind = Some(CostKind::Billed);
    store.record_request(&row).expect("seed");

    let response = get_session(addr, "?session=ses-missing", Some("session")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response.json().await.expect("session body");
    assert_eq!(value["session"], "ses-missing");
    assert_eq!(value["requests"], 0, "absence ≠ zero everywhere else");
    assert_eq!(value["first_ts_ms"], Value::Null);
    assert_eq!(value["last_ts_ms"], Value::Null);
    for field in [
        "input",
        "output",
        "reasoning",
        "cache_read",
        "cache_write_total",
    ] {
        assert_eq!(value["tokens"][field], Value::Null, "tokens.{field}");
    }
    assert_eq!(value["cost"]["billed_total"], Value::Null);
    assert_eq!(value["cost"]["per_provider"], serde_json::json!([]));
    assert_eq!(value["cost"]["per_model"], serde_json::json!([]));
}

#[tokio::test]
async fn a_seeded_session_aggregates_billed_cost_tokens_and_breakdowns() {
    let upstream = spawn_mock().await;
    let (addr, store) = spawn_toker(upstream).await;

    // Three billed openrouter rows — two upstream endpoints plus one
    // with no serving_provider named (falls back to the backend id) —
    // a plan-equivalent subscription row, and a proxy error row, all in
    // one session.
    let mut relace = bare_row(1_000);
    relace.session_id = Some("ses-agg".to_owned());
    relace.provider = Some("openrouter".to_owned());
    relace.model = Some("z-ai/glm-5.3".to_owned());
    relace.input = Some(100);
    relace.output = Some(40);
    relace.cost_usd = Some(0.01);
    relace.cost_kind = Some(CostKind::Billed);
    relace.extra = Some(serde_json::json!({"serving_provider": "Relace"}));
    store.record_request(&relace).expect("seed");

    let mut inference = bare_row(2_000);
    inference.session_id = Some("ses-agg".to_owned());
    inference.provider = Some("openrouter".to_owned());
    inference.model = Some("z-ai/glm-5.3".to_owned());
    inference.input = Some(50);
    inference.cache_read = Some(10);
    inference.cost_usd = Some(0.02);
    inference.cost_kind = Some(CostKind::Billed);
    inference.extra = Some(serde_json::json!({"serving_provider": "InferenceNet"}));
    store.record_request(&inference).expect("seed");

    let mut fallback = bare_row(3_000);
    fallback.session_id = Some("ses-agg".to_owned());
    fallback.provider = Some("openrouter".to_owned());
    fallback.model = Some("openai/gpt-5.2".to_owned());
    fallback.output = Some(5);
    fallback.cost_usd = Some(0.005);
    fallback.cost_kind = Some(CostKind::Billed);
    store.record_request(&fallback).expect("seed");

    let mut sub = bare_row(4_000);
    sub.session_id = Some("ses-agg".to_owned());
    sub.provider = Some("anthropic_sub".to_owned());
    sub.model = Some("claude-opus-5".to_owned());
    sub.input = Some(7);
    sub.cache_write_total = Some(900);
    sub.cost_usd = Some(1.5);
    sub.cost_kind = Some(CostKind::PlanEquivalent);
    store.record_request(&sub).expect("seed");

    let mut error = bare_row(5_000);
    error.session_id = Some("ses-agg".to_owned());
    error.kind = Some(RowKind::Error);
    error.input = Some(999);
    store.record_request(&error).expect("seed");

    let response = get_session(addr, "?session=ses-agg", Some("session")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response.json().await.expect("session body");
    assert_eq!(value["session"], "ses-agg");
    assert_eq!(
        value["requests"], 4,
        "three openrouter rows and the sub row"
    );
    assert_eq!(value["first_ts_ms"], 1_000);
    assert_eq!(value["last_ts_ms"], 4_000);
    // Token sums over the measurement rows: input 100+50+7, output
    // 40+5, cache_read 10, cache_write_total 900 (the sub row's),
    // reasoning absent — no row carried it.
    assert_eq!(value["tokens"]["input"], 157);
    assert_eq!(value["tokens"]["output"], 45);
    assert_eq!(value["tokens"]["cache_read"], 10);
    assert_eq!(value["tokens"]["cache_write_total"], 900);
    assert_eq!(value["tokens"]["reasoning"], Value::Null);
    // Billed only: the 1.5 plan-equivalent is not spend.
    assert_eq!(value["cost"]["billed_total"], 0.035);
    // Cost-desc breakdowns, partitioning the billed total exactly; the
    // sub never appears in a cost breakdown.
    assert_eq!(
        value["cost"]["per_provider"],
        serde_json::json!([
            {"provider": "InferenceNet", "requests": 1, "cost_usd": 0.02},
            {"provider": "Relace", "requests": 1, "cost_usd": 0.01},
            {"provider": "openrouter", "requests": 1, "cost_usd": 0.005},
        ]),
        "serving_provider first, backend id fallback, cost-desc"
    );
    assert_eq!(
        value["cost"]["per_model"],
        serde_json::json!([
            {"model": "z-ai/glm-5.3", "requests": 2, "cost_usd": 0.03},
            {"model": "openai/gpt-5.2", "requests": 1, "cost_usd": 0.005},
        ])
    );
}

#[tokio::test]
async fn a_real_zero_billed_sum_stays_zero() {
    let upstream = spawn_mock().await;
    let (addr, store) = spawn_toker(upstream).await;

    // A billed row that cost nothing: the sum is a real 0.0, and it must
    // read back as 0, not null (absence is for no billed rows).
    let mut row = bare_row(1_000);
    row.session_id = Some("ses-zero".to_owned());
    row.provider = Some("openrouter".to_owned());
    row.model = Some("z-ai/glm-5.3".to_owned());
    row.input = Some(0);
    row.cost_usd = Some(0.0);
    row.cost_kind = Some(CostKind::Billed);
    row.extra = Some(serde_json::json!({"serving_provider": "Relace"}));
    store.record_request(&row).expect("seed");

    let response = get_session(addr, "?session=ses-zero", Some("session")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response.json().await.expect("session body");
    assert_eq!(value["requests"], 1);
    assert_eq!(value["tokens"]["input"], 0, "a reported zero is a zero");
    assert_eq!(
        value["cost"]["billed_total"], 0.0,
        "a real zero sum stays 0"
    );
    assert_eq!(
        value["cost"]["per_provider"],
        serde_json::json!([
            {"provider": "Relace", "requests": 1, "cost_usd": 0.0},
        ])
    );
}

#[tokio::test]
async fn a_chat_completion_is_attributed_by_session_header_end_to_end() {
    let upstream = spawn_mock().await;
    let (addr, store) = spawn_toker(upstream).await;

    // opencode's own header (x-session-id) drives the attribution: one
    // chat completion through the mock, then the endpoint answers with
    // exactly that row's cost.
    let body = br#"{"model":"z-ai/glm-5.3","messages":[{"role":"user","content":"Hi"}]}"#;
    let response = client()
        .post(format!("http://{addr}/v1/chat/completions"))
        .header("x-session-id", "ses-e2e")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .await
        .expect("chat request");
    assert_eq!(response.status(), StatusCode::OK);

    for _ in 0..200 {
        if store.count_requests().expect("count") == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(store.count_requests().expect("count"), 1, "the row landed");

    let response = get_session(addr, "?session=ses-e2e", Some("session")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response.json().await.expect("session body");
    assert_eq!(value["session"], "ses-e2e");
    assert_eq!(value["requests"], 1);
    assert_eq!(value["tokens"]["input"], 48, "64 prompt - 16 cached");
    assert_eq!(value["tokens"]["cache_read"], 16);
    assert_eq!(value["tokens"]["output"], 4, "8 completion - 4 reasoning");
    assert_eq!(value["tokens"]["reasoning"], 4);
    assert_eq!(value["cost"]["billed_total"], 0.000128);
    assert_eq!(
        value["cost"]["per_provider"],
        serde_json::json!([
            {"provider": "z-ai", "requests": 1, "cost_usd": 0.000128},
        ]),
        "the serving provider the upstream reported, not the backend id"
    );
    assert_eq!(
        value["cost"]["per_model"],
        serde_json::json!([
            {"model": "z-ai/glm-5.3", "requests": 1, "cost_usd": 0.000128},
        ])
    );
    assert!(
        value["first_ts_ms"].is_i64() && value["last_ts_ms"].is_i64(),
        "the row's timestamps are known"
    );
}
