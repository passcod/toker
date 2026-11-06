//! End-to-end coverage for the gated live routing control lifecycle.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use toker::config::{Config, OpenRouterConfig};
use toker::server::Server;
use toker::store::{Lane, LanePin, Store};
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

async fn snapshot(app: &axum::Router) -> Value {
    body(
        app.clone()
            .oneshot(request(
                "GET",
                "/_toker/routing/snapshot",
                Some("routing-snapshot"),
                None,
            ))
            .await
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn routing_controls_are_local_operation_gated_and_snapshot_is_content_free() {
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
            "/_toker/routing/validate",
            Some("routing-preview"),
            Some(json!({})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let snapshot = snapshot(&app).await;
    assert_eq!(snapshot["toker"], "routing-snapshot");
    assert_eq!(snapshot["active_revision"]["revision_id"], 1);
    assert_eq!(snapshot["draft_generation"], 1);
    assert!(snapshot.get("offers").is_some());
    assert!(snapshot.get("lane_impacts").is_some());
    assert!(!snapshot.to_string().contains(UNSET_KEY_ENV));
}

#[tokio::test]
async fn draft_replace_uses_generation_cas_and_denies_unknown_fields() {
    let (server, _) = server();
    let app = server.router();
    let initial = snapshot(&app).await;
    let generation = initial["draft_generation"].as_i64().unwrap();
    let policy = initial["draft"]["policy"].clone();

    let response = app
        .clone()
        .oneshot(request(
            "PUT",
            "/_toker/routing/draft",
            Some("routing-draft-replace"),
            Some(json!({
                "expected_generation": generation,
                "policy": policy,
                "surprise": true,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let updated = app
        .clone()
        .oneshot(request(
            "PUT",
            "/_toker/routing/draft",
            Some("routing-draft-replace"),
            Some(json!({
                "expected_generation": generation,
                "policy": policy,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(updated.status(), StatusCode::OK);
    let updated = body(updated).await;
    let next_generation = updated["generation"].as_i64().unwrap();
    assert_eq!(next_generation, generation + 1);
    assert_eq!(updated["draft"]["policy"], policy);

    let stale = app
        .oneshot(request(
            "PUT",
            "/_toker/routing/draft",
            Some("routing-draft-replace"),
            Some(json!({
                "expected_generation": generation,
                "policy": policy,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let stale = body(stale).await;
    assert_eq!(stale["error"], "stale_draft");
    assert_eq!(stale["generation"], next_generation);
}

#[tokio::test]
async fn typed_patch_only_changes_its_target_and_validation_is_non_mutating() {
    let (server, store) = server();
    let app = server.router();
    let initial = snapshot(&app).await;
    let generation = initial["draft_generation"].as_i64().unwrap();
    let before = initial["draft"]["policy"].clone();

    let obsolete_envelope = app
        .clone()
        .oneshot(request(
            "PATCH",
            "/_toker/routing/draft",
            Some("routing-draft-edit"),
            Some(json!({
                "expected_generation": generation,
                "edits": [{
                    "kind": "set_description",
                    "description": "the server must not retain its old edit vocabulary",
                }],
            })),
        ))
        .await
        .unwrap();
    assert_eq!(obsolete_envelope.status(), StatusCode::BAD_REQUEST);

    let unknown_operation_field = app
        .clone()
        .oneshot(request(
            "PATCH",
            "/_toker/routing/draft",
            Some("routing-draft-edit"),
            Some(json!({
                "expected_generation": generation,
                "edit": {
                    "operations": [{
                        "kind": "set_description",
                        "description": "the policy DTO is strict",
                        "unexpected": true,
                    }],
                },
            })),
        ))
        .await
        .unwrap();
    assert_eq!(unknown_operation_field.status(), StatusCode::BAD_REQUEST);

    let patched = app
        .clone()
        .oneshot(request(
            "PATCH",
            "/_toker/routing/draft",
            Some("routing-draft-edit"),
            Some(json!({
                "expected_generation": generation,
                "edit": {
                    "operations": [{
                        "kind": "set_description",
                        "description": "edited in a typed control request",
                    }],
                },
            })),
        ))
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::OK);
    let patched = body(patched).await;
    let generation = patched["generation"].as_i64().unwrap();
    assert_eq!(
        patched["draft"]["policy"]["description"],
        "edited in a typed control request"
    );
    for key in [
        "format_version",
        "defaults",
        "presentations",
        "routes",
        "rewrites",
        "classifiers",
        "upgrades",
        "compaction",
    ] {
        assert_eq!(patched["draft"]["policy"][key], before[key]);
    }

    let stale = app
        .clone()
        .oneshot(request(
            "PATCH",
            "/_toker/routing/draft",
            Some("routing-draft-edit"),
            Some(json!({
                "expected_generation": generation - 1,
                "edit": {
                    "operations": [{
                        "kind": "set_description",
                        "description": "stale edit",
                    }],
                },
            })),
        ))
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let stale = body(stale).await;
    assert_eq!(stale["error"], "stale_draft");
    assert_eq!(stale["generation"], generation);
    assert_eq!(stale["draft"], patched["draft"]);

    let validated = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/validate",
            Some("routing-validate"),
            Some(json!({ "expected_generation": generation })),
        ))
        .await
        .unwrap();
    assert_eq!(validated.status(), StatusCode::OK);
    let validated = body(validated).await;
    assert_eq!(validated["generation"], generation);
    assert!(validated["diagnostics"].is_array());
    assert_eq!(store.load_policy_revisions().unwrap().len(), 1);
}

#[tokio::test]
async fn preview_rejects_content_fields_and_only_projects_content_free_trace() {
    let (server, _) = server();
    let app = server.router();
    let generation = snapshot(&app).await["draft_generation"].as_i64().unwrap();

    let rejected = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/preview",
            Some("routing-preview"),
            Some(json!({
                "expected_generation": generation,
                "protocol": "openai_chat",
                "prompt": "never accepted",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

    let preview = app
        .oneshot(request(
            "POST",
            "/_toker/routing/preview",
            Some("routing-preview"),
            Some(json!({
                "expected_generation": generation,
                "protocol": "openai_chat",
                "model": "unmatched-model",
                "prompt_bound": 100,
                "message_count": 1,
                "tool_count": 0,
            })),
        ))
        .await
        .unwrap();
    // The draft compiles under the compiler its activation would use, so the
    // preview resolves rather than reporting diagnostics activation would not.
    assert_eq!(preview.status(), StatusCode::OK);
    let preview = body(preview).await;
    assert!(preview["result"]["outcome"].is_string());
    assert!(preview["diagnostics"].is_array());
    assert!(!preview.to_string().contains("never accepted"));
}

#[tokio::test]
async fn activation_keeps_the_editable_draft_and_reactivation_moves_forward() {
    let (server, store) = server();
    let app = server.router();
    let initial = snapshot(&app).await;
    let generation = initial["draft_generation"].as_i64().unwrap();

    let activated = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/activate",
            Some("routing-activate"),
            Some(json!({
                "expected_generation": generation,
                "scope": "new_lanes",
                "description": "same document as a fresh revision",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(activated.status(), StatusCode::OK);
    let activated = body(activated).await;
    assert_eq!(activated["active_revision"]["revision_id"], 2);
    assert_eq!(activated["draft"]["base_revision_id"], 2);
    let generation = activated["generation"].as_i64().unwrap();

    let reactivated = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/reactivate",
            Some("routing-reactivate"),
            Some(json!({
                "expected_generation": generation,
                "revision_id": 1,
                "scope": "new_lanes",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(reactivated.status(), StatusCode::OK);
    let reactivated = body(reactivated).await;
    assert_eq!(reactivated["active_revision"]["revision_id"], 3);
    assert_eq!(reactivated["active_revision"]["parent_revision_id"], 1);
    assert_eq!(store.load_policy_revisions().unwrap().len(), 3);
}

#[tokio::test]
async fn immediate_activation_requires_an_explicit_warm_cache_override() {
    let (server, store) = server();
    let app = server.router();
    let initial = snapshot(&app).await;
    let generation = initial["draft_generation"].as_i64().unwrap();

    store
        .upsert_lane(&Lane {
            key: "warm-lane".to_owned(),
            session_id: None,
            tools_hash: None,
            updated_ms: jiff::Timestamp::now().as_millisecond(),
            prompt_tokens: None,
            ttl: None,
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
            pin_generation: 0,
            pin: Some(LanePin {
                policy_revision_id: 1,
                policy_route_id: "route".parse().unwrap(),
                route_fingerprint: "11".repeat(32).parse().unwrap(),
                candidate_id: "candidate".parse().unwrap(),
                binding_id: "openrouter.openai_chat".parse().unwrap(),
                baseline_model: "base".to_owned(),
                effective_model: "effective".to_owned(),
            }),
        })
        .unwrap();

    let denied = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/activate",
            Some("routing-activate"),
            Some(json!({
                "expected_generation": generation,
                "scope": "immediate",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::CONFLICT);
    let denied = body(denied).await;
    assert_eq!(denied["error"], "forced_cache_transition_required");
    assert_eq!(denied["lane_impacts"]["unknown"], 1);

    let forced = app
        .oneshot(request(
            "POST",
            "/_toker/routing/activate",
            Some("routing-activate"),
            Some(json!({
                "expected_generation": generation,
                "scope": "immediate",
                "force_cache_transition": true,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(forced.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_refused_reactivation_leaves_the_draft_and_its_generation_alone() {
    let (server, store) = server();
    let app = server.router();
    let generation = snapshot(&app).await["draft_generation"].as_i64().unwrap();
    store
        .upsert_lane(&Lane {
            key: "warm-lane".to_owned(),
            session_id: None,
            tools_hash: None,
            updated_ms: jiff::Timestamp::now().as_millisecond(),
            prompt_tokens: None,
            ttl: None,
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
            pin_generation: 0,
            pin: Some(LanePin {
                policy_revision_id: 1,
                policy_route_id: "route".parse().unwrap(),
                route_fingerprint: "11".repeat(32).parse().unwrap(),
                candidate_id: "candidate".parse().unwrap(),
                binding_id: "openrouter.openai_chat".parse().unwrap(),
                baseline_model: "base".to_owned(),
                effective_model: "effective".to_owned(),
            }),
        })
        .unwrap();

    let denied = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/reactivate",
            Some("routing-reactivate"),
            Some(json!({
                "expected_generation": generation,
                "revision_id": 1,
                "scope": "immediate",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::CONFLICT);
    assert_eq!(
        body(denied).await["error"],
        "forced_cache_transition_required"
    );
    // The refusal did not consume the draft: the same generation still works.
    let after = snapshot(&app).await;
    assert_eq!(after["draft_generation"].as_i64().unwrap(), generation);
    assert_eq!(store.load_policy_revisions().unwrap().len(), 1);
}

#[tokio::test]
async fn stale_publication_acknowledgement_conflicts_and_generic_call_cannot_claim_applied() {
    let (server, _) = server();
    let app = server.router();
    let current = snapshot(&app).await["active_revision"]["revision_id"]
        .as_i64()
        .unwrap();
    let stale = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/publication",
            Some("routing-publication"),
            Some(json!({
                "scope": { "kind": "protocol", "protocol": "openai_chat" },
                "adapter": "dynamic_chat_models",
                "expected_revision_id": current - 1,
                "expected_inventory_generation": 0,
                "outcome": "failed",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);

    let applied = app
        .oneshot(request(
            "POST",
            "/_toker/routing/publication",
            Some("routing-publication"),
            Some(json!({
                "scope": { "kind": "protocol", "protocol": "openai_chat" },
                "adapter": "dynamic_chat_models",
                "expected_revision_id": current,
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
