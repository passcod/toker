//! End-to-end coverage for the gated live routing control surface.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use toker::config::{Config, OpenRouterConfig};
use toker::server::Server;
use toker::store::Store;
use tower::ServiceExt;

const UNSET_KEY_ENV: &str = "TOKER_TEST_KEY_UNSET_ROUTING_CONTROL";

fn config() -> Config {
    Config {
        port: 0,
        db_path: PathBuf::from(":memory:"),
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
        default_backend_anthropic: None,
        anthropic_sub: None,
        anthropic_api: None,
        codex_sub: None,
        gates: toker::config::GatesConfig::default(),
        notices: toker::config::NoticesConfig::default(),
        awake: false,
        transcript_roots: Vec::new(),
    }
}

fn server() -> (Server, Arc<Store>) {
    let config = config();
    let store = Arc::new(Store::open(&config.db_path).unwrap());
    (
        Server::new_authoritative(config, store.clone()).expect("authoritative server"),
        store,
    )
}

async fn body(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn request(method: &str, uri: &str, gate: Option<&str>, value: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(gate) = gate {
        builder = builder.header("x-toker-control", gate);
    }
    let body = match value {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

#[tokio::test]
async fn routing_controls_are_local_and_operation_gated() {
    let (server, _) = server();
    let app = server.router();
    let response = app
        .clone()
        .oneshot(request("GET", "/_toker/routing/snapshot", None, None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/snapshot",
            Some("routing-snapshot"),
            Some(json!({})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = app
        .oneshot(request(
            "GET",
            "/_toker/routing/snapshot",
            Some("routing-snapshot"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body(response).await;
    assert_eq!(value["toker"], "routing-snapshot");
    assert_eq!(value["active_revision"]["revision_id"], 1);
}

#[tokio::test]
async fn apply_rejects_unknown_fields_and_preserves_the_active_revision_on_invalid_policy() {
    let (server, store) = server();
    let app = server.router();
    let response = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/apply",
            Some("routing-apply"),
            Some(json!({ "policy": {}, "surprise": true })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(store.load_policy_revisions().unwrap().len(), 1);

    let response = app
        .oneshot(request(
            "POST",
            "/_toker/routing/apply",
            Some("routing-apply"),
            Some(json!({ "policy": {} })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(store.load_policy_revisions().unwrap().len(), 1);
    assert_eq!(
        store
            .load_routing_policy_state()
            .unwrap()
            .active_revision_id,
        Some(1)
    );
}

#[tokio::test]
async fn restore_is_a_forward_revision_and_stale_publication_acknowledgement_conflicts() {
    let (server, store) = server();
    let app = server.router();
    let snapshot = app
        .clone()
        .oneshot(request(
            "GET",
            "/_toker/routing/snapshot",
            Some("routing-snapshot"),
            None,
        ))
        .await
        .unwrap();
    let snapshot = body(snapshot).await;
    let policy = snapshot["active_revision"]["policy"].clone();

    let applied = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/apply",
            Some("routing-apply"),
            Some(json!({ "policy": policy, "description": "same configuration" })),
        ))
        .await
        .unwrap();
    assert_eq!(applied.status(), StatusCode::OK);
    assert_eq!(body(applied).await["active_revision"]["revision_id"], 2);

    let restored = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/restore",
            Some("routing-restore"),
            Some(json!({ "revision_id": 1 })),
        ))
        .await
        .unwrap();
    assert_eq!(restored.status(), StatusCode::OK);
    let restored = body(restored).await;
    assert_eq!(restored["active_revision"]["revision_id"], 3);
    assert_eq!(restored["active_revision"]["parent_revision_id"], 1);
    assert_eq!(store.load_policy_revisions().unwrap().len(), 3);

    let publication = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/publication",
            Some("routing-publication"),
            Some(json!({
                "scope": { "kind": "protocol", "protocol": "openai_chat" },
                "adapter": "dynamic_chat_models",
                "expected_revision_id": 2,
                "expected_inventory_generation": 0,
                "outcome": "failed",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(publication.status(), StatusCode::CONFLICT);
    assert_eq!(body(publication).await["error"], "stale_publication");

    let applied = app
        .oneshot(request(
            "POST",
            "/_toker/routing/publication",
            Some("routing-publication"),
            Some(json!({
                "scope": { "kind": "protocol", "protocol": "openai_chat" },
                "adapter": "dynamic_chat_models",
                "expected_revision_id": 3,
                "expected_inventory_generation": 0,
                "outcome": "applied",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(applied.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body(applied).await["error"],
        "adapter_owned_result_required"
    );
}
