//! The configuration control surface: editing live, refusing what it cannot
//! use, and never carrying a credential back out.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use toker::config::Config;
use toker::server::Server;
use toker::store::Store;
use tower::ServiceExt;

fn config() -> Config {
    Config {
        port: 0,
        db_path: PathBuf::from(":memory:"),
        session_header_names: vec!["x-toker-session".to_owned()],
        ping_header_name: "x-toker-ping".to_owned(),
        default_backend_openai_chat: None,
        openrouter: None,
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
                "/_toker/config/snapshot",
                Some("config-snapshot"),
                None,
            ))
            .await
            .unwrap(),
    )
    .await
}

fn provider<'a>(snapshot: &'a Value, name: &str) -> &'a Value {
    snapshot["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|row| row["provider"] == name)
        .expect("a row per provider toker knows")
}

#[tokio::test]
async fn the_snapshot_lists_every_provider_whether_or_not_it_is_on() {
    let (server, _store) = server();
    let app = server.router();
    let snapshot = snapshot(&app).await;

    let names: Vec<&str> = snapshot["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .map(|row| row["provider"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        names,
        [
            "anthropic_api",
            "anthropic_sub",
            "codex_sub",
            "openai_api",
            "openrouter"
        ]
    );
    assert!(
        snapshot["providers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["enabled"] == json!(false))
    );
    // Shown so the page can report them, never offered for editing.
    assert!(snapshot["db_path"].is_string());
    assert!(snapshot["port"].is_number());
}

#[tokio::test]
async fn an_edit_changes_the_row_and_the_running_state_together() {
    let (server, store) = server();
    let app = server.router();

    assert!(server.config().openrouter.is_none());
    let response = app
        .clone()
        .oneshot(request(
            "PUT",
            "/_toker/config/provider",
            Some("config-edit"),
            Some(json!({
                "provider": "openrouter",
                "enabled": true,
                "upstream": "http://gateway.invalid/v1"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The row landed...
    let stored = store.load_configuration().expect("load");
    let row = stored
        .providers
        .iter()
        .find(|row| row.provider == "openrouter")
        .expect("a row");
    assert!(row.enabled);
    assert_eq!(row.upstream.as_deref(), Some("http://gateway.invalid/v1"));

    // ...and the service is already serving by it, with no restart.
    let live = server.config();
    let openrouter = live.openrouter.as_ref().expect("enabled live");
    assert_eq!(openrouter.upstream.as_str(), "http://gateway.invalid/v1");
}

#[tokio::test]
async fn a_refused_value_leaves_the_stored_one_as_it_was() {
    let (server, store) = server();
    let app = server.router();

    app.clone()
        .oneshot(request(
            "PUT",
            "/_toker/config/provider",
            Some("config-edit"),
            Some(json!({
                "provider": "openrouter",
                "enabled": true,
                "upstream": "http://good.invalid/v1"
            })),
        ))
        .await
        .unwrap();

    let response = app
        .clone()
        .oneshot(request(
            "PUT",
            "/_toker/config/provider",
            Some("config-edit"),
            Some(json!({
                "provider": "openrouter",
                "enabled": true,
                "upstream": "not a url at all"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let refusal = body(response).await;
    assert_eq!(refusal["ok"], json!(false));
    assert!(
        refusal["setting"].is_string(),
        "the refusal names a setting"
    );

    let stored = store.load_configuration().expect("load");
    let row = stored
        .providers
        .iter()
        .find(|row| row.provider == "openrouter")
        .expect("a row");
    assert_eq!(row.upstream.as_deref(), Some("http://good.invalid/v1"));
    assert_eq!(
        server
            .config()
            .openrouter
            .as_ref()
            .expect("still on")
            .upstream
            .as_str(),
        "http://good.invalid/v1"
    );
}

#[tokio::test]
async fn a_provider_toker_does_not_know_is_refused() {
    let (server, _store) = server();
    let app = server.router();
    let response = app
        .oneshot(request(
            "PUT",
            "/_toker/config/provider",
            Some("config-edit"),
            Some(json!({ "provider": "mystery", "enabled": true })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_notice_style_outside_the_routers_names_is_refused_when_set() {
    let (server, store) = server();
    let app = server.router();

    for bad in ["Claude", "with space"] {
        let response = app
            .clone()
            .oneshot(request(
                "PUT",
                "/_toker/config/notice",
                Some("config-edit"),
                Some(json!({ "profile": bad, "style": "block" })),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
    }

    let response = app
        .clone()
        .oneshot(request(
            "PUT",
            "/_toker/config/notice",
            Some("config-edit"),
            Some(json!({ "profile": "opencode", "style": "toker" })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        store.load_configuration().unwrap().notices.get("opencode"),
        Some(&"toker".to_owned())
    );

    // A null style returns the profile to the default.
    app.clone()
        .oneshot(request(
            "PUT",
            "/_toker/config/notice",
            Some("config-edit"),
            Some(json!({ "profile": "opencode", "style": null })),
        ))
        .await
        .unwrap();
    assert!(
        store
            .load_configuration()
            .unwrap()
            .notices
            .contains_key("opencode")
            .eq(&false)
    );
}

#[tokio::test]
async fn a_credential_goes_in_through_its_own_operation_and_never_comes_back() {
    let (server, store) = server();
    let app = server.router();

    app.clone()
        .oneshot(request(
            "PUT",
            "/_toker/config/provider",
            Some("config-edit"),
            Some(json!({ "provider": "openrouter", "enabled": true })),
        ))
        .await
        .unwrap();

    let response = app
        .clone()
        .oneshot(request(
            "PUT",
            "/_toker/config/credential",
            Some("config-credential"),
            Some(json!({ "provider": "openrouter", "literal": "sk-the-secret" })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        store
            .load_configuration()
            .unwrap()
            .providers
            .iter()
            .find(|row| row.provider == "openrouter")
            .unwrap()
            .cred_literal
            .as_deref(),
        Some("sk-the-secret")
    );

    let snapshot = snapshot(&app).await;
    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(
        !rendered.contains("sk-the-secret"),
        "no operation returns a credential value: {rendered}"
    );
    assert_eq!(
        provider(&snapshot, "openrouter")["credential"]["literal_set"],
        json!(true),
        "the page can still say whether one is set"
    );
}

#[tokio::test]
async fn an_absent_literal_leaves_the_stored_one_alone_and_null_clears_it() {
    let (server, store) = server();
    let app = server.router();

    let literal = |value: Value| {
        request(
            "PUT",
            "/_toker/config/credential",
            Some("config-credential"),
            Some(json!({ "provider": "openrouter", "literal": value })),
        )
    };
    app.clone()
        .oneshot(literal(json!("sk-the-secret")))
        .await
        .unwrap();

    // Changing only the source must not drop the credential.
    app.clone()
        .oneshot(request(
            "PUT",
            "/_toker/config/credential",
            Some("config-credential"),
            Some(json!({ "provider": "openrouter", "keyring": true })),
        ))
        .await
        .unwrap();
    let stored = store.load_configuration().unwrap();
    let row = stored
        .providers
        .iter()
        .find(|row| row.provider == "openrouter")
        .unwrap();
    assert_eq!(row.cred_literal.as_deref(), Some("sk-the-secret"));
    assert_eq!(row.cred_keyring, Some(true));

    app.clone().oneshot(literal(json!(null))).await.unwrap();
    let stored = store.load_configuration().unwrap();
    assert_eq!(
        stored
            .providers
            .iter()
            .find(|row| row.provider == "openrouter")
            .unwrap()
            .cred_literal,
        None
    );
}

#[tokio::test]
async fn the_configuration_operations_are_gated_like_every_other_control() {
    let (server, _store) = server();
    let app = server.router();

    for (method, uri, gate) in [
        ("GET", "/_toker/config/snapshot", "config-snapshot"),
        ("PUT", "/_toker/config/provider", "config-edit"),
        ("PUT", "/_toker/config/credential", "config-credential"),
    ] {
        // No operation header at all.
        let response = app
            .clone()
            .oneshot(request(method, uri, None, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri} ungated");

        // The wrong operation named.
        let response = app
            .clone()
            .oneshot(request(method, uri, Some("routing-snapshot"), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri} mis-gated");

        // The right operation, the wrong method.
        let wrong = if method == "GET" { "PUT" } else { "GET" };
        let response = app
            .clone()
            .oneshot(request(wrong, uri, Some(gate), None))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{uri} wrong method"
        );
    }
}
