//! `POST /_toker/shutdown` end to end: the gate, the drain, and the forced
//! stop that does not wait for it.
//!
//! The drain is the point of the endpoint, so it is tested on the real
//! listener path ([`Server::serve_listener`]), not a bare router: a
//! response under way when the shutdown lands finishes canonically, new
//! connections are no longer accepted, and the serve call returns once
//! the stream is done. The sleep lock is on over a fake spawner (never a
//! real inhibitor), to show the exit kills it without writing a row.
//! Last, `toker restart` runs over HTTP against it, with a second server
//! on the same port standing in for systemd's restart.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use serde_json::{Value, json};

use toker::config::{AnthropicSubConfig, Config, GatesConfig};
use toker::middleware::awake::{InhibitCommand, InhibitLock, LockSpawner};
use toker::server::Server;
use toker::store::{Lane, RowKind, Store};

// ---------------------------------------------------------------------------
// The mock upstream: a turn whose second half waits for the test
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockState {
    /// Released by the test once the shutdown has been requested.
    gate: Arc<tokio::sync::Notify>,
    /// Every request body the mock saw.
    requests: Arc<Mutex<Vec<Bytes>>>,
}

fn first_half() -> Bytes {
    Bytes::from_static(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":9,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":400}}}\n\n",
    )
}

fn second_half() -> Bytes {
    Bytes::from_static(
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":40}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    )
}

async fn mock_messages(State(mock): State<MockState>, request: Request) -> Response {
    let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .expect("mock reads body");
    mock.requests.lock().unwrap().push(body);
    let gate = mock.gate.clone();
    let stream = futures::stream::unfold(0u8, move |step| {
        let gate = gate.clone();
        async move {
            match step {
                0 => Some((Ok::<_, std::io::Error>(first_half()), 1)),
                1 => {
                    gate.notified().await;
                    Some((Ok(second_half()), 2))
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

async fn spawn_mock() -> (MockState, reqwest::Url) {
    let state = MockState::default();
    let app = Router::new()
        .route("/v1/messages", post(mock_messages))
        .fallback(|| async { StatusCode::IM_A_TEAPOT })
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
// The fake sleep lock
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct AwakeProbe {
    spawns: Arc<AtomicU64>,
    kills: Arc<AtomicU64>,
}

struct ProbeSpawner(AwakeProbe);

impl LockSpawner for ProbeSpawner {
    fn spawn(&mut self, _command: &InhibitCommand) -> Result<Box<dyn InhibitLock>, String> {
        self.0.spawns.fetch_add(1, Ordering::SeqCst);
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
// The toker server
// ---------------------------------------------------------------------------

fn test_config(upstream: reqwest::Url) -> Config {
    Config {
        port: 0,
        db_path: PathBuf::from(":memory:"),
        session_header_names: vec!["x-claude-code-session-id".to_owned()],
        ping_header_name: "x-toker-ping".to_owned(),
        default_backend_openai_chat: None,
        openrouter: None,
        default_backend_anthropic: Some("anthropic_sub".to_owned()),
        anthropic_sub: Some(AnthropicSubConfig {
            model_map: None,
            upstream,
            claude_credentials_path: None,
            ..AnthropicSubConfig::default()
        }),
        anthropic_api: None,
        codex_sub: None,
        gates: GatesConfig::default(),
        notices: toker::config::NoticesConfig::default(),
        // On over the fake spawner only: the real one would take a real
        // idle-sleep lock on the machine running the suite.
        awake: true,
        transcript_roots: Vec::new(),
    }
}

struct Toker {
    addr: SocketAddr,
    store: Arc<Store>,
    probe: AwakeProbe,
    instance: String,
    served: tokio::task::JoinHandle<anyhow::Result<()>>,
}

/// toker on the real listener path, bound to `port` (0 for any).
async fn spawn_toker(config: Config, port: u16) -> Toker {
    let probe = AwakeProbe::default();
    let store = Arc::new(Store::open(&config.db_path).expect("open store"));
    let server =
        Server::with_awake_spawner(config, store.clone(), Box::new(ProbeSpawner(probe.clone())))
            .expect("build server");
    let instance = server.instance().to_owned();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("toker binds");
    let addr = listener.local_addr().expect("toker addr");
    let served = tokio::spawn(server.serve_listener(listener));
    Toker {
        addr,
        store,
        probe,
        instance,
        served,
    }
}

fn client() -> reqwest::Client {
    // A fresh connection per request: a pooled one would hide whether
    // the listener still accepts.
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .expect("client")
}

async fn request_shutdown(addr: SocketAddr, instance: &str) -> reqwest::Response {
    post_shutdown(addr, json!({ "instance": instance })).await
}

async fn post_shutdown(addr: SocketAddr, body: Value) -> reqwest::Response {
    client()
        .post(format!("http://{addr}/_toker/shutdown"))
        .header("x-toker-control", "shutdown")
        .json(&body)
        .send()
        .await
        .expect("shutdown request")
}

fn awake_rows(store: &Store) -> Vec<bool> {
    store
        .requests_since(0, 1000)
        .expect("read rows")
        .into_iter()
        .filter(|row| row.kind == Some(RowKind::Awake))
        .map(|row| {
            row.extra
                .as_ref()
                .and_then(|extra| extra.get("held"))
                .and_then(Value::as_bool)
                .expect("an awake row says whether it holds")
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_refuses_a_wrong_gate_and_never_forwards() {
    let (mock, upstream) = spawn_mock().await;
    let toker = spawn_toker(test_config(upstream), 0).await;
    let url = format!("http://{}/_toker/shutdown", toker.addr);
    let body = json!({ "instance": toker.instance }).to_string();

    // Wrong or missing verb, or no JSON content type: the 403 the other
    // control paths answer.
    for (verb, content_type) in [
        (None, Some("application/json")),
        (Some("status"), Some("application/json")),
        (Some("models-merge"), Some("application/json")),
        (Some("shutdown"), None),
        (Some("shutdown"), Some("text/plain")),
        (Some("shutdown"), Some("application/x-www-form-urlencoded")),
    ] {
        let mut request = client().post(&url).body(body.clone());
        if let Some(verb) = verb {
            request = request.header("x-toker-control", verb);
        }
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let response = request.send().await.expect("shutdown request");
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "verb {verb:?} with content type {content_type:?} does not pass the gate"
        );
    }

    // A GET is not a shutdown, whatever its headers say.
    let response = client()
        .get(&url)
        .header("x-toker-control", "shutdown")
        .header(header::CONTENT_TYPE, "application/json")
        .send()
        .await
        .expect("get");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

    // Past the gate, a body that is not `{"instance": id}` is a 400.
    for bad in [&b"not json"[..], br#"{}"#, br#"{"instance": 7}"#] {
        let response = client()
            .post(&url)
            .header("x-toker-control", "shutdown")
            .header(header::CONTENT_TYPE, "application/json")
            .body(bad.to_vec())
            .send()
            .await
            .expect("shutdown request");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let reply: Value = response.json().await.expect("reply");
        assert_eq!(reply["error"], json!("unparseable request"));
    }

    // Another instance's id: a 409 naming this one.
    let response = request_shutdown(toker.addr, "not-this-instance").await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let reply: Value = response.json().await.expect("reply");
    assert_eq!(reply["error"], json!("instance mismatch"));
    assert_eq!(reply["instance"], json!(toker.instance));

    // None of that reached the upstream, and the server still serves.
    assert!(mock.requests.lock().unwrap().is_empty());
    let status = client()
        .get(format!("http://{}/_toker/status", toker.addr))
        .header("x-toker-control", "status")
        .send()
        .await
        .expect("status");
    assert_eq!(status.status(), StatusCode::OK);
    assert!(!toker.served.is_finished());
}

#[tokio::test]
async fn status_names_the_instance() {
    let (_mock, upstream) = spawn_mock().await;
    let toker = spawn_toker(test_config(upstream), 0).await;
    let reply: Value = client()
        .get(format!("http://{}/_toker/status", toker.addr))
        .header("x-toker-control", "status")
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("reply");
    assert_eq!(reply["instance"], json!(toker.instance));
    assert_eq!(reply["in_flight"], json!(0));
    assert_eq!(reply["version"], json!(env!("CARGO_PKG_VERSION")));
    assert!(reply["started_ms"].as_i64().is_some_and(|ms| ms > 0));
}

#[tokio::test]
async fn a_stream_under_way_finishes_while_new_connections_are_refused() {
    let (mock, upstream) = spawn_mock().await;
    let toker = spawn_toker(test_config(upstream), 0).await;

    // A live lane, so the sleep lock still wants to hold when the process
    // exits: the exit must kill it without a release row.
    let body = serde_json::to_vec(&json!({
        "model": "claude-opus-5",
        "stream": true,
        "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "Hi"}],
    }))
    .expect("body");
    let shape = toker::ir::Request::parse(&body)
        .expect("parse")
        .anthropic()
        .shape();
    let now = jiff::Timestamp::now().as_millisecond();
    toker
        .store
        .upsert_lane(&Lane {
            key: format!("ses-1|{}", shape.tools_hash),
            session_id: Some("ses-1".to_owned()),
            tools_hash: Some(shape.tools_hash.clone()),
            updated_ms: now - 1_000,
            prompt_tokens: Some(200_000),
            ttl: Some(3_600_000),
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
        })
        .expect("lane");

    let mut response = client()
        .post(format!("http://{}/v1/messages", toker.addr))
        .header("x-claude-code-session-id", "ses-1")
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(body)
        .send()
        .await
        .expect("messages request");
    assert_eq!(response.status(), StatusCode::OK);
    let mut received = Vec::new();
    received.extend_from_slice(&response.chunk().await.expect("chunk").expect("first half"));
    let opening = String::from_utf8(received.clone()).expect("canonical SSE");
    assert!(opening.contains("event: message_start"));
    assert!(opening.contains(r#""model":"claude-opus-5""#));

    let shutdown = request_shutdown(toker.addr, &toker.instance).await;
    assert_eq!(shutdown.status(), StatusCode::ACCEPTED);
    let reply: Value = shutdown.json().await.expect("reply");
    assert_eq!(reply["ok"], json!(true));
    assert_eq!(reply["in_flight"], json!(1));

    // The listener closes: a new connection is refused (on a direct bind;
    // under socket activation it would queue for the next instance).
    let mut refused = false;
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(toker.addr).await.is_err() {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(refused, "the listener stops accepting once asked to drain");
    assert!(
        !toker.served.is_finished(),
        "the server waits for the stream under way"
    );

    // The rest of the canonical turn arrives without the drain cutting it.
    mock.gate.notify_one();
    while let Some(chunk) = response.chunk().await.expect("the stream is not cut") {
        received.extend_from_slice(&chunk);
    }
    let received = String::from_utf8(received).expect("canonical SSE");
    assert!(received.contains("event: message_delta"));
    assert!(received.contains(r#""stop_reason":"end_turn""#));
    assert!(received.contains(r#""output_tokens":40"#));
    assert!(received.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));

    let served = tokio::time::timeout(Duration::from_secs(5), toker.served)
        .await
        .expect("the server returns once drained")
        .expect("the serve task did not panic");
    served.expect("a drained exit is a clean one");

    // The drained turn was recorded like any other.
    let measured = toker
        .store
        .requests_since(0, 1000)
        .expect("rows")
        .into_iter()
        .filter(|row| row.kind.is_none())
        .count();
    assert_eq!(measured, 1);

    // The lock: taken while the turn ran, killed at exit, and the only
    // awake row is the hold. The exit is not the sessions going quiet.
    assert_eq!(toker.probe.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(toker.probe.kills.load(Ordering::SeqCst), 1);
    assert_eq!(awake_rows(&toker.store), vec![true]);
}

#[tokio::test]
async fn a_forced_shutdown_does_not_wait_for_a_stream_but_exits_cleanly() {
    let (mock, upstream) = spawn_mock().await;
    let toker = spawn_toker(test_config(upstream), 0).await;

    // Same live lane as the drain test: the lock is held when the exit
    // comes, and a clean exit kills it without a release row.
    let body = serde_json::to_vec(&json!({
        "model": "claude-opus-5",
        "stream": true,
        "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "Hi"}],
    }))
    .expect("body");
    let shape = toker::ir::Request::parse(&body)
        .expect("parse")
        .anthropic()
        .shape();
    let now = jiff::Timestamp::now().as_millisecond();
    toker
        .store
        .upsert_lane(&Lane {
            key: format!("ses-1|{}", shape.tools_hash),
            session_id: Some("ses-1".to_owned()),
            tools_hash: Some(shape.tools_hash.clone()),
            updated_ms: now - 1_000,
            prompt_tokens: Some(200_000),
            ttl: Some(3_600_000),
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
        })
        .expect("lane");

    let mut response = client()
        .post(format!("http://{}/v1/messages", toker.addr))
        .header("x-claude-code-session-id", "ses-1")
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(body)
        .send()
        .await
        .expect("messages request");
    assert_eq!(response.status(), StatusCode::OK);
    response.chunk().await.expect("chunk").expect("first half");

    // A forced stop that is not a boolean is refused, and stops nothing.
    let bad = post_shutdown(
        toker.addr,
        json!({ "instance": toker.instance, "force": "yes" }),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert!(!toker.served.is_finished());

    // The upstream gate is never released: the stream would hang for
    // good. A plain drain would wait on it; the forced one does not.
    let shutdown = post_shutdown(
        toker.addr,
        json!({ "instance": toker.instance, "force": true }),
    )
    .await;
    assert_eq!(shutdown.status(), StatusCode::ACCEPTED);
    let served = tokio::time::timeout(Duration::from_secs(5), toker.served)
        .await
        .expect("the server returns without the stream finishing")
        .expect("the serve task did not panic");
    served.expect("a forced exit is still a clean one");
    drop(mock);

    // The exit path ran: the lock was killed once, and the exit is not
    // the sessions going quiet, so there is no release row.
    assert_eq!(toker.probe.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(toker.probe.kills.load(Ordering::SeqCst), 1);
    assert_eq!(awake_rows(&toker.store), vec![true]);
}

#[tokio::test]
async fn restart_waits_hands_over_and_sees_the_new_instance() {
    use toker::cmds::{HttpControl, Pacing, RestartOpts, restart_run};

    let (_mock, upstream) = spawn_mock().await;
    let old = spawn_toker(test_config(upstream.clone()), 0).await;
    let port = old.addr.port();
    let old_instance = old.instance.clone();

    // What systemd's `Restart=always` does, in miniature: once the old
    // instance has returned, a new one serves the same port.
    let successor = tokio::spawn(async move {
        old.served
            .await
            .expect("old serve task")
            .expect("old drained cleanly");
        spawn_toker(test_config(upstream), port).await
    });

    let pacing = Pacing {
        poll: Duration::from_millis(20),
        request_timeout: Duration::from_secs(2),
        up_timeout: Duration::from_secs(10),
    };
    let mut out = Vec::new();
    restart_run(
        &HttpControl::new(port, pacing.request_timeout).expect("client"),
        port,
        &RestartOpts::default(),
        &pacing,
        &mut out,
    )
    .await
    .expect("restart succeeds");
    let new = successor.await.expect("successor");
    assert_ne!(new.instance, old_instance);
    let out = String::from_utf8(out).expect("utf-8");
    assert!(
        out.contains(&format!("toker {} is up", &new.instance[..8])),
        "{out}"
    );
}
