//! End-to-end routing-control tests against the real loopback router.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{StatusCode, header};
use serde_json::{Value, json};

use toker::config::{
    AnthropicApiConfig, AnthropicSubConfig, CodexSubConfig, Config, OpenRouterConfig,
};
use toker::server::Server;
use toker::store::Store;

const CONTROL_HEADER: &str = "x-toker-control";
const UNSET_KEY_ENV: &str = "TOKER_ROUTING_CONTROL_TEST_KEY_UNSET";

fn config(dir: &std::path::Path) -> Config {
    Config {
        port: 0,
        db_path: dir.join("toker.db"),
        session_header_names: vec!["x-toker-session".to_owned()],
        ping_header_name: "x-toker-ping".to_owned(),
        default_backend_openai_chat: Some("openrouter".to_owned()),
        openrouter: Some(OpenRouterConfig {
            upstream: "http://127.0.0.1:9/v1".parse().unwrap(),
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key_keyring: false,
            api_key: None,
            picker: None,
        }),
        openai_api: None,
        default_backend_anthropic: Some("anthropic_sub".to_owned()),
        anthropic_sub: Some(AnthropicSubConfig {
            model_map: None,
            upstream: "http://127.0.0.1:9/v1".parse().unwrap(),
            claude_credentials_path: None,
            ..AnthropicSubConfig::default()
        }),
        anthropic_api: Some(AnthropicApiConfig {
            model_map: None,
            upstream: "http://127.0.0.1:9/v1".parse().unwrap(),
            api_key_env: UNSET_KEY_ENV.to_owned(),
            api_key_keyring: false,
            api_key: None,
        }),
        codex_sub: Some(CodexSubConfig {
            model_map: None,
            client_version: None,
            version_probe: false,
            upstream: "http://127.0.0.1:9/backend-api/codex".parse().unwrap(),
            originator: "codex_cli_rs".to_owned(),
            auth_path: dir.join("codex-auth.json"),
            refresh_url: "https://auth.openai.com/oauth/token".parse().unwrap(),
        }),
        gates: toker::config::GatesConfig::default(),
        notices: toker::config::NoticesConfig::default(),
        awake: false,
        transcript_roots: vec![],
    }
}

async fn spawn() -> (SocketAddr, Arc<Store>) {
    let dir = common::tempdir("toker-routing-control-");
    let config = config(&dir);
    let store = Arc::new(Store::open(&config.db_path).unwrap());
    let server = Server::new_authoritative(config, store.clone()).unwrap();
    let app = server.router();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (address, store)
}

fn url(address: SocketAddr, path: &str) -> reqwest::Url {
    format!("http://{address}{path}").parse().unwrap()
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn snapshot(address: SocketAddr) -> Value {
    let response = client()
        .get(url(address, "/_toker/routing/snapshot"))
        .header(CONTROL_HEADER, "routing-snapshot")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

#[tokio::test]
async fn routing_snapshot_and_revisions_are_gated() {
    let (address, _store) = spawn().await;

    for request in [
        client().get(url(address, "/_toker/routing/snapshot")),
        client()
            .get(url(address, "/_toker/routing/snapshot"))
            .header(CONTROL_HEADER, "wrong"),
        client()
            .post(url(address, "/_toker/routing/snapshot"))
            .header(CONTROL_HEADER, "routing-snapshot"),
    ] {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    let state = snapshot(address).await;
    assert!(state["active"]["revision"]["revision_id"].is_i64());
    assert!(state["active"]["document"].is_object());
    assert!(state.get("credentials").is_none());
}

#[tokio::test]
async fn apply_creates_an_immediate_immutable_child_and_restore_creates_another() {
    let (address, store) = spawn().await;
    let before = snapshot(address).await;
    let base_id = before["active"]["revision"]["revision_id"]
        .as_i64()
        .unwrap();
    let mut document = before["active"]["document"].clone();
    document["description"] = json!("a live routing edit");

    let response = client()
        .post(url(address, "/_toker/routing/apply"))
        .header(CONTROL_HEADER, "routing-apply")
        .header(header::CONTENT_TYPE, "application/json")
        .json(&document)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let applied: Value = response.json().await.unwrap();
    let applied_id = applied["active_revision"]["revision_id"].as_i64().unwrap();
    assert_eq!(applied["active_revision"]["parent_revision_id"], base_id);
    // Revision one uses legacy-only compatibility constructs, so an unchanged
    // document retains that compiler origin until an authored edit removes them.
    assert_eq!(applied["active_revision"]["source"], "legacy");

    let response = client()
        .post(url(address, "/_toker/routing/restore"))
        .header(CONTROL_HEADER, "routing-restore")
        .header(header::CONTENT_TYPE, "application/json")
        .json(&json!({"revision_id": base_id, "description": "undo live edit"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let restored: Value = response.json().await.unwrap();
    let restored_id = restored["active_revision"]["revision_id"].as_i64().unwrap();
    assert_eq!(restored["active_revision"]["parent_revision_id"], base_id);
    assert_ne!(restored_id, base_id);
    assert_ne!(restored_id, applied_id);
    assert_eq!(
        store
            .load_policy_revision(base_id)
            .unwrap()
            .unwrap()
            .description,
        before["active"]["revision"]["description"]
            .as_str()
            .map(str::to_owned),
    );
}

#[tokio::test]
async fn invalid_apply_leaves_the_active_revision_unchanged() {
    let (address, _store) = spawn().await;
    let before = snapshot(address).await;
    let revision_id = before["active"]["revision"]["revision_id"].clone();
    let mut document = before["active"]["document"].clone();
    document["routes"] = json!([{
        "id": "unknown-binding",
        "display_name": "Unknown binding",
        "parameters": [],
        "lane_transition": "preserve_pinned",
        "chain": {
            "first": {
                "id": "candidate",
                "binding_id": "missing.dialect",
                "model": {"kind": "exact", "model_id": "missing"}
            }
        }
    }]);

    let response = client()
        .post(url(address, "/_toker/routing/apply"))
        .header(CONTROL_HEADER, "routing-apply")
        .header(header::CONTENT_TYPE, "application/json")
        .json(&document)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let response: Value = response.json().await.unwrap();
    assert_eq!(response["error"], "invalid_policy");
    assert!(response["diagnostics"].is_array());

    assert_eq!(
        snapshot(address).await["active"]["revision"]["revision_id"],
        revision_id
    );
}

#[tokio::test]
async fn mutations_require_their_own_gate_and_json_body() {
    let (address, _store) = spawn().await;
    let document = snapshot(address).await["active"]["document"].clone();
    for request in [
        client().post(url(address, "/_toker/routing/apply")),
        client()
            .post(url(address, "/_toker/routing/apply"))
            .header(CONTROL_HEADER, "routing-restore"),
        client()
            .post(url(address, "/_toker/routing/apply"))
            .header(CONTROL_HEADER, "routing-apply")
            .body(document.to_string()),
    ] {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
