//! End-to-end coverage for the gated live routing control lifecycle.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use toker::config::{Config, OpenRouterConfig};
use toker::server::Server;
use toker::store::{Lane, LanePin, Store};

mod common;
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
        Server::with_seams(config, store.clone(), toker::server::Seams::isolated())
            .expect("authoritative server"),
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

/// POST a control operation and return its status and body.
async fn post(app: &axum::Router, path: &str, operation: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(request(
            "POST",
            &format!("/_toker/routing/{path}"),
            Some(operation),
            Some(body),
        ))
        .await
        .unwrap();
    let status = response.status();
    (status, body_of(response).await)
}

async fn body_of(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn describe(description: &str) -> Value {
    json!({"operations": [{"kind": "set_description", "description": description}]})
}

async fn live_revision(app: &axum::Router) -> i64 {
    snapshot(app).await["active_revision"]["revision_id"]
        .as_i64()
        .unwrap()
}

fn warm_lane(store: &Store) {
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
                account_number: None,
                baseline_model: "base".to_owned(),
                effective_model: "effective".to_owned(),
            }),
        })
        .unwrap();
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

    // Each operation takes only its own name, and no other operation's.
    for (path, own, other) in [
        ("save", "routing-save", "routing-validate"),
        ("widen", "routing-widen", "routing-save"),
        ("validate", "routing-validate", "routing-preview"),
        ("preview", "routing-preview", "routing-validate"),
        ("reactivate", "routing-reactivate", "routing-save"),
    ] {
        let uri = format!("/_toker/routing/{path}");
        let refused = app
            .clone()
            .oneshot(request("POST", &uri, Some(other), Some(json!({}))))
            .await
            .unwrap();
        assert_eq!(
            refused.status(),
            StatusCode::FORBIDDEN,
            "{path} under {other}"
        );
        // Under its own name an empty body is a bad request, not a refusal
        // of the caller.
        let own_name = app
            .clone()
            .oneshot(request("POST", &uri, Some(own), Some(json!({}))))
            .await
            .unwrap();
        assert_eq!(own_name.status(), StatusCode::BAD_REQUEST, "{path}");
    }

    let snapshot = snapshot(&app).await;
    assert_eq!(snapshot["toker"], "routing-snapshot");
    assert_eq!(snapshot["active_revision"]["revision_id"], 1);
    for gone in ["draft", "draft_generation", "draft_diagnostics"] {
        assert!(snapshot.get(gone).is_none(), "{gone} is retired");
    }
    assert_eq!(snapshot["activation"]["scope"], "new_lanes");
    assert_eq!(snapshot["activation"]["widened_ms"], Value::Null);
    assert_eq!(snapshot["hit_counts"]["coverage"], "complete");
    assert!(snapshot.get("offers").is_some());
    assert!(snapshot.get("lane_impacts").is_some());
    assert!(!snapshot.to_string().contains(UNSET_KEY_ENV));
}

#[tokio::test]
async fn the_draft_and_activate_operations_are_gone() {
    let (server, _) = server();
    let app = server.router();
    for (method, path, operation) in [
        ("PUT", "draft", "routing-draft-replace"),
        ("PATCH", "draft", "routing-draft-edit"),
        ("POST", "activate", "routing-activate"),
    ] {
        let response = app
            .clone()
            .oneshot(request(
                method,
                &format!("/_toker/routing/{path}"),
                Some(operation),
                Some(json!({})),
            ))
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::OK, "{method} {path}");
        assert!(
            body_of(response)
                .await
                .get("toker")
                .is_none_or(|operation| operation != "routing-draft-edit"
                    && operation != "routing-activate"),
            "{method} {path} is not served"
        );
    }
}

#[tokio::test]
async fn a_save_applies_its_edit_to_the_live_document_and_activates_for_new_and_cold_lanes() {
    let (server, store) = server();
    let app = server.router();
    let before = snapshot(&app).await;
    let (status, saved) = post(
        &app,
        "save",
        "routing-save",
        json!({
            "expected_revision_id": 1,
            "edit": describe("edited in a typed control request"),
            "description": "rename",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["toker"], "routing-save");
    assert_eq!(saved["active_revision"]["revision_id"], 2);
    assert_eq!(saved["active_revision"]["source"], "save");
    assert_eq!(saved["active_revision"]["parent_revision_id"], 1);
    assert_eq!(saved["active_revision"]["description"], "rename");
    assert_eq!(
        saved["active_revision"]["activation_scope"],
        "new_and_cold_lanes"
    );
    assert_eq!(saved["activation"]["scope"], "new_and_cold_lanes");
    assert_eq!(
        saved["active_revision"]["policy"]["description"],
        "edited in a typed control request"
    );
    // Nothing else in the document moved.
    for key in [
        "defaults",
        "presentations",
        "routes",
        "rewrites",
        "classifiers",
    ] {
        assert_eq!(
            saved["active_revision"]["policy"][key], before["active_revision"]["policy"][key],
            "{key}"
        );
    }
    assert_eq!(live_revision(&app).await, 2);
    assert_eq!(store.load_policy_revisions().unwrap().len(), 2);
    assert_eq!(
        store
            .load_routing_policy_state()
            .unwrap()
            .active_revision_id,
        Some(2)
    );
}

#[tokio::test]
async fn a_save_is_strict_about_what_it_carries() {
    let (server, store) = server();
    let app = server.router();
    for body in [
        // The old draft envelope.
        json!({"expected_generation": 1, "edit": describe("x")}),
        json!({"expected_revision_id": 1, "edits": [{"kind": "set_description"}]}),
        json!({"expected_revision_id": 1, "edit": describe("x"), "surprise": true}),
        json!({"expected_revision_id": 1, "edit": {"operations": []}}),
        json!({"expected_revision_id": 1, "edit": {"operations": [{
            "kind": "set_description", "description": "x", "unexpected": true,
        }]}}),
    ] {
        let (status, _) = post(&app, "save", "routing-save", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    assert_eq!(store.load_policy_revisions().unwrap().len(), 1);
}

#[tokio::test]
async fn a_save_against_a_moved_revision_conflicts_naming_the_live_one_and_changes_nothing() {
    let (server, store) = server();
    let app = server.router();
    let (status, _) = post(
        &app,
        "save",
        "routing-save",
        json!({"expected_revision_id": 1, "edit": describe("first")}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, conflict) = post(
        &app,
        "save",
        "routing-save",
        json!({"expected_revision_id": 1, "edit": describe("second")}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["error"], "stale_revision");
    assert_eq!(conflict["live_revision_id"], 2);
    assert_eq!(store.load_policy_revisions().unwrap().len(), 2);
    let live = snapshot(&app).await;
    assert_eq!(live["active_revision"]["policy"]["description"], "first");
}

#[tokio::test]
async fn a_refused_save_changes_nothing() {
    let (server, store) = server();
    let app = server.router();
    // An edit naming a record that is not there.
    let (status, refused) = post(
        &app,
        "save",
        "routing-save",
        json!({"expected_revision_id": 1, "edit": {"operations": [
            {"kind": "remove_route", "id": "no-such-route"},
        ]}}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    assert_eq!(refused["error"], "record_not_found");

    // A document that applies but does not compile: a rewrite to a route
    // that does not exist.
    let (status, refused) = post(
        &app,
        "save",
        "routing-save",
        json!({"expected_revision_id": 1, "edit": {"operations": [{
            "kind": "add_rewrite",
            "rewrite": {
                "id": "to-nowhere",
                "scope": {"kind": "protocol", "protocol": "openai_chat"},
                "priority": 1,
                "model_matcher": {"kind": "exact", "value": "alpha"},
                "target": {"route_id": "no-such-route"},
            },
        }]}}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["error"], "invalid_policy");
    let diagnostics = refused["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic["severity"] == "error"
                && diagnostic["path"]
                    .as_str()
                    .is_some_and(|path| !path.is_empty())),
        "{refused}"
    );

    assert_eq!(store.load_policy_revisions().unwrap().len(), 1);
    assert_eq!(live_revision(&app).await, 1);
}

#[tokio::test]
async fn validation_reports_diagnostics_with_their_paths_and_changes_nothing() {
    let (server, store) = server();
    let app = server.router();

    let (status, valid) = post(
        &app,
        "validate",
        "routing-validate",
        json!({"expected_revision_id": 1, "edit": describe("fine")}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{valid}");
    assert_eq!(valid["active_revision_id"], 1);
    assert_eq!(valid["valid"], true, "{valid}");
    assert_eq!(valid["refusal"], Value::Null);
    let diagnostics = valid["diagnostics"].as_array().unwrap();
    assert!(
        !diagnostics.is_empty()
            && diagnostics
                .iter()
                .all(|diagnostic| diagnostic["severity"] != "error"),
        "a policy that compiles still reports its notes: {valid}"
    );
    for diagnostic in diagnostics {
        for key in ["code", "severity", "record_id", "path", "sentence", "fix"] {
            assert!(diagnostic.get(key).is_some(), "{key}: {diagnostic}");
        }
        // Every code the compiler raises explains itself for the form.
        assert!(diagnostic["sentence"].is_string(), "{diagnostic}");
        assert!(diagnostic["fix"].is_string(), "{diagnostic}");
    }

    // A document that does not compile is a report, not an HTTP failure.
    let (status, invalid) = post(
        &app,
        "validate",
        "routing-validate",
        json!({"expected_revision_id": 1, "edit": {"operations": [{
            "kind": "add_rewrite",
            "rewrite": {
                "id": "to-nowhere",
                "scope": {"kind": "protocol", "protocol": "openai_chat"},
                "priority": 1,
                "model_matcher": {"kind": "exact", "value": "alpha"},
                "target": {"route_id": "no-such-route"},
            },
        }]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{invalid}");
    assert_eq!(invalid["valid"], false);
    assert!(
        invalid["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["severity"] == "error"
                && diagnostic["code"] == "missing-route"
                && diagnostic["sentence"].is_string()
                && diagnostic["fix"].is_string()),
        "{invalid}"
    );

    // An edit that cannot be applied has nothing to compile.
    let (status, refused) = post(
        &app,
        "validate",
        "routing-validate",
        json!({"expected_revision_id": 1, "edit": {"operations": [
            {"kind": "remove_route", "id": "no-such-route"},
        ]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{refused}");
    assert_eq!(refused["valid"], false);
    assert_eq!(refused["refusal"]["error"], "record_not_found");

    let (status, stale) = post(
        &app,
        "validate",
        "routing-validate",
        json!({"expected_revision_id": 7, "edit": describe("late")}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale["live_revision_id"], 1);
    assert_eq!(store.load_policy_revisions().unwrap().len(), 1);
}

fn invented(model: &str) -> Value {
    json!({
        "kind": "invented",
        "protocol": "openai_chat",
        "model": model,
        "prompt_bound": 100,
        "message_count": 1,
        "tool_count": 0,
    })
}

/// An edit adding a route and a rewrite that sends `model` to it, built from
/// the route a default of the live revision sends to.
fn reroute(model: &str, active: &Value) -> Value {
    let policy = &active["active_revision"]["policy"];
    let default = policy["defaults"]
        .as_array()
        .unwrap()
        .iter()
        .find(|default| {
            default["scope"]["kind"] == "protocol" && default["scope"]["protocol"] == "openai_chat"
        })
        .expect("a default for the chat protocol");
    let mut route = policy["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|route| route["id"] == default["target"]["route_id"])
        .expect("the default's route")
        .clone();
    route["id"] = json!("elsewhere");
    route["display_name"] = json!("Elsewhere");
    route["chain"]["first"]["id"] = json!("elsewhere-first");
    route["chain"]["first"]["model"] = json!({"kind": "exact", "model_id": "vendor/elsewhere"});
    let mut target = default["target"].clone();
    target["route_id"] = json!("elsewhere");
    json!({"operations": [
        {"kind": "add_route", "route": route},
        {"kind": "add_rewrite", "rewrite": {
            "id": "to-elsewhere",
            "scope": {"kind": "protocol", "protocol": "openai_chat"},
            "priority": 1,
            "model_matcher": {"kind": "exact", "value": model},
            "target": target,
        }},
    ]})
}

#[tokio::test]
async fn preview_resolves_against_the_live_revision_an_unsaved_edit_or_a_kept_revision() {
    let (server, store) = server();
    let app = server.router();
    let live = snapshot(&app).await;

    let rejected = app
        .clone()
        .oneshot(request(
            "POST",
            "/_toker/routing/preview",
            Some("routing-preview"),
            Some(json!({
                "mode": "resolve",
                "request": {"kind": "invented", "protocol": "openai_chat", "prompt": "never accepted"},
            })),
        ))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    // The mode is required, and the old unqualified body is refused.
    let (status, _) = post(
        &app,
        "preview",
        "routing-preview",
        json!({"protocol": "openai_chat", "model": "alpha"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, active) = post(
        &app,
        "preview",
        "routing-preview",
        json!({"mode": "resolve", "request": invented("alpha")}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{active}");
    assert_eq!(active["active_revision_id"], 1);
    assert_eq!(active["against_revision_id"], 1);
    assert!(active["result"]["outcome"].is_string());
    assert_eq!(active["request"]["model"], "alpha");
    assert_eq!(active["request"]["class"], "inference");
    assert!(!active.to_string().contains("never accepted"));
    let active_route = active["result"]["route_id"].clone();
    assert!(active_route.is_string(), "{active}");
    // What caught the request is named in stable words, not Debug output, and
    // each candidate the walk passed says why by the reach's own codes.
    assert!(
        active["result"]["provenance"]["kind"].is_string(),
        "{active}"
    );
    assert!(active["result"]["provider"].is_string(), "{active}");
    assert!(active["result"]["cost"].is_string(), "{active}");
    let steps = active["result"]["trace"].as_array().unwrap();
    assert_eq!(steps[0]["kind"], "lookup");
    assert!(steps[0]["provenance"]["kind"].is_string());
    let candidate = steps
        .iter()
        .find(|step| step["kind"] == "candidate")
        .unwrap();
    assert!(candidate["binding_id"].is_string());
    assert!(candidate["skipped"].is_null(), "{candidate}");

    // An unsaved edit, which changes where the request goes and nothing live.
    let (status, edited) = post(
        &app,
        "preview",
        "routing-preview",
        json!({
            "mode": "resolve",
            "expected_revision_id": 1,
            "against": {"kind": "edit", "edit": reroute("alpha", &live)},
            "request": invented("alpha"),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["against_revision_id"], Value::Null);
    assert_eq!(edited["result"]["route_id"], "elsewhere");
    assert_eq!(live_revision(&app).await, 1);
    assert_eq!(store.load_policy_revisions().unwrap().len(), 1);

    // An edit that does not compile is refused with its diagnostics.
    let (status, refused) = post(
        &app,
        "preview",
        "routing-preview",
        json!({
            "mode": "resolve",
            "against": {"kind": "edit", "edit": {"operations": [{
                "kind": "add_rewrite",
                "rewrite": {
                    "id": "to-nowhere",
                    "scope": {"kind": "protocol", "protocol": "openai_chat"},
                    "priority": 1,
                    "model_matcher": {"kind": "exact", "value": "alpha"},
                    "target": {"route_id": "no-such-route"},
                },
            }]}},
            "request": invented("alpha"),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");

    // A kept revision, after a save has replaced the live one.
    let (status, _) = post(
        &app,
        "save",
        "routing-save",
        json!({"expected_revision_id": 1, "edit": reroute("alpha", &live)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, kept) = post(
        &app,
        "preview",
        "routing-preview",
        json!({
            "mode": "resolve",
            "against": {"kind": "revision", "revision_id": 1},
            "request": invented("alpha"),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    assert_eq!(kept["against_revision_id"], 1);
    assert_eq!(kept["result"]["route_id"], active_route);
    let (_, current) = post(
        &app,
        "preview",
        "routing-preview",
        json!({"mode": "resolve", "request": invented("alpha")}),
    )
    .await;
    assert_eq!(current["result"]["route_id"], "elsewhere");

    let (status, missing) = post(
        &app,
        "preview",
        "routing-preview",
        json!({
            "mode": "resolve",
            "against": {"kind": "revision", "revision_id": 99},
            "request": invented("alpha"),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["error"], "revision_not_found");
}

/// Send a chat request no backend can serve, so that the ledger gets a
/// route-unavailable row carrying the facts the resolver was given.
async fn send_unservable(app: &axum::Router, model: &str) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": model,
                        "messages": [{"role": "user", "content": "hi"}],
                        "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}],
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// A route-unavailable row as an older version wrote it: policy-routed, with
/// no resolver facts and no marker.
fn insert_row_without_facts(db: &std::path::Path, ts_ms: i64) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO requests
         (ts_ms, kind, policy_revision_id, policy_route_id, route_fingerprint,
          attempt_group_id, requested_model, routing_trace)
         VALUES (?1, 'route-unavailable', 1, 'route', ?2,
                 'abcdefabcdefabcdefabcdefabcdefab', 'old-model', ?3)",
        rusqlite::params![
            ts_ms,
            "22".repeat(32),
            r#"{"version":1,"events":[{"kind":"lookup","source":"protocol_default","presentation_id":null,"rewrite_id":null}]}"#,
        ],
    )
    .unwrap();
}

fn file_server(dir: &std::path::Path) -> (Server, Arc<Store>, PathBuf) {
    let mut config = config();
    config.db_path = dir.join("toker.db");
    let store = Arc::new(Store::open(&config.db_path).unwrap());
    let db = config.db_path.clone();
    (
        Server::with_seams(config, store.clone(), toker::server::Seams::isolated())
            .expect("authoritative server"),
        store,
        db,
    )
}

#[tokio::test]
async fn a_recorded_request_replays_from_its_facts_and_an_old_row_is_not_replayable() {
    let dir = common::tempdir("toker-routing-replay-");
    let (server, _, db) = file_server(&dir);
    let app = server.router();
    send_unservable(&app, "alpha").await;
    insert_row_without_facts(&db, jiff::Timestamp::now().as_millisecond());

    let (status, listing) = post(
        &app,
        "preview",
        "routing-preview",
        json!({"mode": "recent_requests"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    let requests = listing["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2, "{listing}");
    let recorded = requests
        .iter()
        .find(|request| request["replayable"] == true)
        .expect("the request sent now carries its facts");
    let old = requests
        .iter()
        .find(|request| request["replayable"] == false)
        .expect("the old row is listed, flagged");
    assert_eq!(old["not_replayable_reason"], "facts_not_recorded");
    assert_eq!(old["request"], Value::Null);
    assert_eq!(old["matched"], Value::Null);
    assert_eq!(old["requested_model"], "old-model");
    assert_eq!(recorded["request"]["protocol"], "openai_chat");
    assert_eq!(recorded["request"]["model"], "alpha");
    assert_eq!(recorded["request"]["tool_count"], 1);
    assert_eq!(recorded["request"]["required_capabilities"]["tools"], true);
    assert_eq!(recorded["matched"]["kind"], "default");
    assert_eq!(recorded["matched"]["scope"], "profile");
    assert_eq!(recorded["kind"], "route-unavailable");

    let (status, replayed) = post(
        &app,
        "preview",
        "routing-preview",
        json!({"mode": "resolve", "request": {"kind": "recorded", "request_id": recorded["id"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(replayed["request"]["model"], "alpha");
    assert_ne!(replayed["result"]["outcome"], "not_replayable");

    // The old row says so, rather than being replayed with a guess.
    let (status, refused) = post(
        &app,
        "preview",
        "routing-preview",
        json!({"mode": "resolve", "request": {"kind": "recorded", "request_id": old["id"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{refused}");
    assert_eq!(refused["result"]["outcome"], "not_replayable");
    assert_eq!(refused["result"]["reason"], "facts_not_recorded");
    assert_eq!(refused["request"], Value::Null);

    let (status, missing) = post(
        &app,
        "preview",
        "routing-preview",
        json!({"mode": "resolve", "request": {"kind": "recorded", "request_id": 9999}}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["error"], "request_not_found");
}

#[tokio::test]
async fn replaying_recent_traffic_returns_the_requests_an_edit_changes_and_counts_the_rest() {
    let dir = common::tempdir("toker-routing-replay-recent-");
    let (server, _, db) = file_server(&dir);
    let app = server.router();
    let live = snapshot(&app).await;
    send_unservable(&app, "alpha").await;
    send_unservable(&app, "beta").await;
    insert_row_without_facts(&db, jiff::Timestamp::now().as_millisecond());

    let (status, replay) = post(
        &app,
        "preview",
        "routing-preview",
        json!({
            "mode": "replay_recent",
            "expected_revision_id": 1,
            "edit": reroute("alpha", &live),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["replayed"], 2);
    assert_eq!(replay["unreplayable"], 1);
    assert_eq!(replay["truncated"], false);
    assert_eq!(replay["changed_total"], 1);
    let changed = replay["changed"].as_array().unwrap();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0]["request"]["model"], "alpha");
    assert_ne!(
        changed[0]["under_active"]["route_id"],
        changed[0]["under_edit"]["route_id"]
    );
    assert_eq!(changed[0]["under_edit"]["route_id"], "elsewhere");
    assert_eq!(changed[0]["under_edit"]["matched"]["kind"], "rewrite");
    assert_eq!(changed[0]["under_edit"]["matched"]["id"], "to-elsewhere");
    assert!(replay["window"]["until_ms"].as_i64().is_some());

    // An edit that bears on none of it changes none of it.
    let (status, quiet) = post(
        &app,
        "preview",
        "routing-preview",
        json!({"mode": "replay_recent", "edit": describe("no effect")}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{quiet}");
    assert_eq!(quiet["changed_total"], 0);
    assert_eq!(quiet["changed"].as_array().unwrap().len(), 0);
    assert_eq!(quiet["unreplayable"], 1);

    let (status, stale) = post(
        &app,
        "preview",
        "routing-preview",
        json!({"mode": "replay_recent", "expected_revision_id": 5, "edit": describe("x")}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale["live_revision_id"], 1);
}

#[tokio::test]
async fn hit_counts_read_as_a_floor_while_the_window_holds_rows_from_before_the_facts() {
    let dir = common::tempdir("toker-routing-hits-");
    let (server, _, db) = file_server(&dir);
    let app = server.router();
    let now = jiff::Timestamp::now().as_millisecond();

    // Only a row from before the facts: nothing was recorded.
    insert_row_without_facts(&db, now);
    let hits = snapshot(&app).await["hit_counts"].clone();
    assert_eq!(hits["coverage"], "not_recorded", "{hits}");
    assert_eq!(hits["unrecorded_requests"], 1);
    assert_eq!(hits["recorded_requests"], 0);
    assert_eq!(hits["defaults"].as_array().unwrap().len(), 0);

    // A request with its facts beside it: what it caught is counted, and
    // the count says it is at least that many.
    send_unservable(&app, "alpha").await;
    send_unservable(&app, "beta").await;
    let hits = snapshot(&app).await["hit_counts"].clone();
    assert_eq!(hits["coverage"], "floor", "{hits}");
    assert_eq!(hits["recorded_requests"], 2);
    assert_eq!(hits["unrecorded_requests"], 1);
    let defaults = hits["defaults"].as_array().unwrap();
    assert_eq!(defaults.len(), 1, "{hits}");
    assert_eq!(defaults[0]["hits"], 2);
    assert_eq!(defaults[0]["scope"]["kind"], "profile");

    // A row older than the window is not in it.
    let day = 24 * 60 * 60 * 1000;
    let dir = common::tempdir("toker-routing-hits-old-");
    let (server, _, db) = file_server(&dir);
    let app = server.router();
    insert_row_without_facts(&db, now - 2 * day);
    send_unservable(&app, "alpha").await;
    let hits = snapshot(&app).await["hit_counts"].clone();
    assert_eq!(hits["coverage"], "complete", "{hits}");
    assert_eq!(hits["unrecorded_requests"], 0);
}

#[tokio::test]
async fn a_widening_goes_forward_only_and_a_save_starts_from_its_own_scope() {
    let (server, store) = server();
    let app = server.router();
    assert_eq!(snapshot(&app).await["activation"]["scope"], "new_lanes");

    let (status, widened) = post(
        &app,
        "widen",
        "routing-widen",
        json!({"expected_revision_id": 1, "scope": "new_and_cold_lanes"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{widened}");
    assert_eq!(widened["activation"]["scope"], "new_and_cold_lanes");
    assert!(widened["activation"]["widened_ms"].is_i64());
    let live = snapshot(&app).await;
    assert_eq!(live["activation"]["scope"], "new_and_cold_lanes");
    assert!(live["activation"]["widened_ms"].is_i64());
    // The revision itself is as it was: widening is not an edit.
    assert_eq!(live["active_revision"]["activation_scope"], "new_lanes");
    assert_eq!(store.load_policy_revisions().unwrap().len(), 1);

    // It never narrows, and never repeats itself.
    for scope in ["new_lanes", "new_and_cold_lanes"] {
        let (status, refused) = post(
            &app,
            "widen",
            "routing-widen",
            json!({"expected_revision_id": 1, "scope": scope}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{scope}");
        assert_eq!(refused["error"], "not_a_widening");
        assert_eq!(refused["current"]["scope"], "new_and_cold_lanes");
    }

    // A widening made against a revision that has since been replaced.
    let (status, stale) = post(
        &app,
        "widen",
        "routing-widen",
        json!({"expected_revision_id": 9, "scope": "immediate", "force_cache_transition": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale["error"], "stale_revision");
    assert_eq!(stale["live_revision_id"], 1);

    // Immediate needs consent while a lane might be warm.
    warm_lane(&store);
    let (status, denied) = post(
        &app,
        "widen",
        "routing-widen",
        json!({"expected_revision_id": 1, "scope": "immediate"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(denied["error"], "forced_cache_transition_required");
    assert_eq!(denied["lane_impacts"]["unknown"], 1);
    assert_eq!(
        snapshot(&app).await["activation"]["scope"],
        "new_and_cold_lanes",
        "a refused widening changed nothing"
    );
    let (status, forced) = post(
        &app,
        "widen",
        "routing-widen",
        json!({"expected_revision_id": 1, "scope": "immediate", "force_cache_transition": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{forced}");
    assert_eq!(forced["activation"]["forced_cache_transition"], true);

    // It survives a restart: the state row holds it.
    let widened = store.load_routing_policy_state().unwrap().widened.unwrap();
    assert_eq!(widened.scope, toker::policy::ActivationScope::Immediate);
    assert!(widened.forced_cache_transition);

    // The next activation starts from its own scope, whatever was widened.
    let (status, saved) = post(
        &app,
        "save",
        "routing-save",
        json!({"expected_revision_id": 1, "edit": describe("after widening")}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["activation"]["scope"], "new_and_cold_lanes");
    assert_eq!(saved["activation"]["forced_cache_transition"], false);
    assert_eq!(saved["activation"]["widened_ms"], Value::Null);
    assert_eq!(store.load_routing_policy_state().unwrap().widened, None);
}

#[tokio::test]
async fn a_save_needs_no_consent_however_warm_the_lanes_are() {
    let (server, store) = server();
    let app = server.router();
    warm_lane(&store);
    let (status, saved) = post(
        &app,
        "save",
        "routing-save",
        json!({"expected_revision_id": 1, "edit": describe("with a warm lane")}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["lane_impacts"]["unknown"], 1);
}

#[tokio::test]
async fn a_reactivation_copies_a_kept_revision_for_new_and_cold_lanes() {
    let (server, store) = server();
    let app = server.router();
    let (status, _) = post(
        &app,
        "save",
        "routing-save",
        json!({"expected_revision_id": 1, "edit": describe("second")}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The old scope, force and generation fields are gone from the body.
    for body in [
        json!({"expected_generation": 2, "revision_id": 1}),
        json!({"expected_revision_id": 2, "revision_id": 1, "scope": "immediate"}),
        json!({"expected_revision_id": 2, "revision_id": 1, "force_cache_transition": true}),
    ] {
        let (status, _) = post(&app, "reactivate", "routing-reactivate", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    let (status, stale) = post(
        &app,
        "reactivate",
        "routing-reactivate",
        json!({"expected_revision_id": 1, "revision_id": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale["live_revision_id"], 2);

    let (status, missing) = post(
        &app,
        "reactivate",
        "routing-reactivate",
        json!({"expected_revision_id": 2, "revision_id": 42}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["error"], "revision_not_found");
    assert_eq!(store.load_policy_revisions().unwrap().len(), 2);

    let (status, reactivated) = post(
        &app,
        "reactivate",
        "routing-reactivate",
        json!({"expected_revision_id": 2, "revision_id": 1, "description": "back"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reactivated}");
    assert_eq!(reactivated["active_revision"]["revision_id"], 3);
    assert_eq!(reactivated["active_revision"]["parent_revision_id"], 1);
    assert_eq!(reactivated["active_revision"]["source"], "reactivation");
    assert_eq!(
        reactivated["active_revision"]["activation_scope"],
        "new_and_cold_lanes"
    );
    assert_eq!(
        reactivated["active_revision"]["policy"]["description"],
        snapshot_policy_description(&store, 1)
    );
    assert_eq!(store.load_policy_revisions().unwrap().len(), 3);
}

#[tokio::test]
async fn each_revision_says_what_it_changed_from_the_one_before_it() {
    let (server, _store) = server();
    let app = server.router();
    let (status, _) = post(
        &app,
        "save",
        "routing-save",
        json!({"expected_revision_id": 1, "edit": describe("second")}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = post(
        &app,
        "reactivate",
        "routing-reactivate",
        json!({"expected_revision_id": 2, "revision_id": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let response = app
        .clone()
        .oneshot(request(
            "GET",
            "/_toker/routing/revisions",
            Some("routing-revisions"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let revisions = body_of(response).await;
    let revisions = revisions["revisions"].as_array().unwrap();
    assert_eq!(revisions.len(), 3);
    let at = |id: i64| {
        revisions
            .iter()
            .find(|revision| revision["revision_id"] == id)
            .unwrap()
    };
    // Nothing is before the first, so it is counted, not diffed.
    assert!(at(1)["summary"].as_str().unwrap().starts_with("setup: "));
    assert_eq!(at(1)["changes"], json!([]));
    // A save that only described the policy says so.
    assert_eq!(at(2)["summary"], "describe the policy");
    assert_eq!(at(2)["changes"][0]["change"], "changed");
    assert_eq!(at(2)["changes"][0]["kind"], "policy");
    // A reactivation names what it copied, and its diff is from what was live.
    assert_eq!(at(3)["summary"], "reactivate rev 1");
    assert_eq!(at(3)["changes"][0]["kind"], "policy");
}

fn snapshot_policy_description(store: &Store, revision: i64) -> Value {
    let revision = store.load_policy_revision(revision).unwrap().unwrap();
    let bytes = toker::policy::policy_bytes(&revision.document).unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["description"].clone()
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

#[tokio::test]
async fn the_snapshot_carries_derived_content_for_the_active_revision() {
    let (server, store) = server();
    let app = server.router();
    let derived = snapshot(&app).await["derived"].clone();

    let availability = derived["availability"].as_array().expect("availability");
    assert!(!availability.is_empty());
    for binding in availability {
        for key in [
            "binding_id",
            "provider",
            "configured",
            "enabled",
            "credential",
            "catalogue",
            "capabilities",
            "cost_class",
        ] {
            assert!(binding.get(key).is_some(), "{key}: {binding}");
        }
    }
    assert!(derived["circuits"].as_array().unwrap().is_empty());

    let active = &derived["active"];
    let routes = active["routes"].as_array().expect("routes");
    assert!(!routes.is_empty());
    for route in routes {
        let candidates = route["candidates"].as_array().unwrap();
        let edges = route["edges"].as_array().unwrap();
        assert_eq!(edges.len(), candidates.len().saturating_sub(1), "{route}");
        for edge in edges {
            assert!(edge["crosses_cost_boundary"].is_boolean(), "{edge}");
        }
        assert_eq!(route["pinned_lanes"]["warm"], 0);
    }
    assert!(active["classifiers"].is_array());
    // The facts the tables and details read are worked out on the service.
    for key in [
        "scopes",
        "presentation_records",
        "rewrites",
        "defaults",
        "upgrades",
        "providers",
        "first_refused_tab",
    ] {
        assert!(active.get(key).is_some(), "{key}");
    }
    let rewrites = active["rewrites"].as_array().unwrap();
    assert!(
        !rewrites.is_empty(),
        "revision one writes a rewrite per prefix"
    );
    for rewrite in rewrites {
        for key in [
            "scope_label",
            "position",
            "matcher",
            "reach",
            "hits",
            "shadowed_by",
        ] {
            assert!(rewrite.get(key).is_some(), "{key}: {rewrite}");
        }
        assert!(rewrite["reach"]["outcome"].is_string(), "{rewrite}");
        // Nothing has been served, so every count is a known zero.
        assert_eq!(rewrite["hits"], json!({"count": 0, "state": "exact"}));
    }
    // A fresh store has no lanes on any revision.
    assert!(derived["revisions"].as_array().unwrap().is_empty());
    // A fresh revision one authors no presentations; the rows carry their
    // state when it has them.
    for presentation in active["presentations"].as_array().unwrap() {
        assert!(
            ["listed", "left_out"].contains(&presentation["desired"].as_str().unwrap()),
            "{presentation}"
        );
    }

    // A lane pinned to a route counts under that route.
    let route_id = routes[0]["id"].as_str().unwrap().to_owned();
    store
        .upsert_lane(&Lane {
            key: "pinned-lane".to_owned(),
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
            pin: None,
        })
        .unwrap();
    // The upsert leaves pins alone; they only move by compare-and-swap.
    let pin = LanePin {
        policy_revision_id: 1,
        policy_route_id: route_id.parse().unwrap(),
        route_fingerprint: "11".repeat(32).parse().unwrap(),
        candidate_id: "candidate".parse().unwrap(),
        binding_id: "openrouter.openai_chat".parse().unwrap(),
        account_number: None,
        baseline_model: "base".to_owned(),
        effective_model: "effective".to_owned(),
    };
    store
        .compare_and_swap_lane_pin("pinned-lane", 0, None, Some(&pin))
        .unwrap();
    let derived = snapshot(&app).await["derived"].clone();
    let pinned = &derived["active"]["routes"][0]["pinned_lanes"];
    let counted: u64 = ["warm", "cold", "unknown"]
        .into_iter()
        .map(|key| pinned[key].as_u64().unwrap())
        .sum();
    assert_eq!(counted, 1, "{pinned}");
    // And under the revision it is pinned to.
    let revisions = derived["revisions"].as_array().unwrap();
    assert_eq!(revisions.len(), 1, "{revisions:?}");
    assert_eq!(revisions[0]["revision_id"], 1);
    assert_eq!(revisions[0]["total"], 1);
}

#[tokio::test]
async fn a_saved_billed_edge_and_virtual_presentation_derive_both() {
    let (server, _) = server();
    let app = server.router();
    let initial = snapshot(&app).await;
    let mut routes = initial["active_revision"]["policy"]["routes"].clone();
    assert!(routes.is_array());
    let route = json!({
        "id": "mixed",
        "display_name": "Mixed",
        "chain": {
            "first": {
                "id": "sub",
                "binding_id": "codex_sub.codex_responses",
                "model": {"kind": "exact", "model_id": "gpt-model"},
                "enabled": true,
            },
            "steps": [{
                "edge": {
                    "preflight": "enabled",
                    "post_send": [],
                    "billed_boundary": "allow",
                    "ambiguous_transport": "deny",
                    "cache_transition": "allow_cache_loss",
                },
                "candidate": {
                    "id": "billed",
                    "binding_id": "openrouter.openrouter_responses",
                    "model": {"kind": "exact", "model_id": "vendor/model"},
                    "enabled": true,
                },
            }],
        },
        "lane_transition": "move_when_cold",
    });
    routes.as_array_mut().unwrap().push(route.clone());
    let (status, saved) = post(
        &app,
        "save",
        "routing-save",
        json!({"expected_revision_id": 1, "edit": {"operations": [
            {"kind": "add_route", "route": route},
            {"kind": "add_presentation", "presentation": {
                "id": "mixed-responses",
                "scope": {"kind": "protocol", "protocol": "openai_responses"},
                "slug": "mixed",
                "enabled": true,
                "source": {"kind": "virtual", "target": {"route_id": "mixed"}},
            }},
        ]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    let snapshot = snapshot(&app).await;
    let active = &snapshot["derived"]["active"];
    let route = active["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|route| route["id"] == "mixed")
        .expect("the saved route");
    let edge = &route["edges"][0];
    assert_eq!(edge["from_cost"], "subscription");
    assert_eq!(edge["to_cost"], "billed");
    assert_eq!(edge["crosses_cost_boundary"], true);
    assert_eq!(edge["billed_boundary"], "allow");

    // A virtual presentation with no codex record is left out of the
    // Responses list, and says why.
    let presentation = active["presentations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|presentation| presentation["slug"] == "mixed")
        .expect("the saved presentation");
    assert_eq!(presentation["adapter"], "dynamic_responses_models");
    assert_eq!(presentation["desired"], "left_out");
    assert_eq!(presentation["reason"], "no_codex_record");
    // The Responses list is rendered from the active revision on every
    // request, so it already holds exactly what the revision lists: not this.
    assert_eq!(presentation["applied"], false);
    assert!(snapshot.get("draft").is_none());
}
