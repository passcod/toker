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
        ping_schedule: false,
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

/// What a client does with a snapshot row: send it back with one field
/// changed. The service writes the row it is given whole, so everything the
/// snapshot carries has to survive the trip.
async fn put_back(app: &axum::Router, path: &str, mut row: Value, change: Value) -> StatusCode {
    for (name, value) in change.as_object().expect("an object") {
        row[name] = value.clone();
    }
    let object = row.as_object_mut().expect("a row");
    object.remove("credential");
    app.clone()
        .oneshot(request("PUT", path, Some("config-edit"), Some(row)))
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn an_edit_sent_back_from_the_snapshot_leaves_every_other_field_as_it_was() {
    let (server, _store) = server();
    let app = server.router();

    // Fields the page does not show: the refresh endpoint, the session
    // header names and the transcript roots.
    let status = put_back(
        &app,
        "/_toker/config/provider",
        provider(&snapshot(&app).await, "codex_sub").clone(),
        json!({"enabled": true, "refresh_url": "http://login.invalid/token"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let status = put_back(
        &app,
        "/_toker/config/settings",
        snapshot(&app).await["settings"].clone(),
        json!({
            "session_headers": ["x-session-id"],
            "transcript_roots": ["/harness/repos/.claude"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Toggle one thing on each, the way the page does.
    let before = snapshot(&app).await;
    assert_eq!(
        put_back(
            &app,
            "/_toker/config/provider",
            provider(&before, "codex_sub").clone(),
            json!({"enabled": false}),
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        put_back(
            &app,
            "/_toker/config/settings",
            before["settings"].clone(),
            json!({"quota_enabled": false}),
        )
        .await,
        StatusCode::OK
    );

    let after = snapshot(&app).await;
    let mut expected = provider(&before, "codex_sub").clone();
    expected["enabled"] = json!(false);
    assert_eq!(provider(&after, "codex_sub"), &expected);
    assert_eq!(
        provider(&after, "codex_sub")["refresh_url"],
        json!("http://login.invalid/token")
    );
    let mut expected = before["settings"].clone();
    expected["quota_enabled"] = json!(false);
    assert_eq!(after["settings"], expected);
    assert_eq!(
        after["settings"]["session_headers"],
        json!(["x-session-id"])
    );
    assert_eq!(
        after["settings"]["transcript_roots"],
        json!(["/harness/repos/.claude"])
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

/// A server whose openrouter is on and whose listing names one model with
/// the given window, or none.
fn server_listing(window: Option<u64>, on: bool) -> (Server, Arc<Store>) {
    let mut config = config();
    if on {
        config.default_backend_openai_chat = Some("openrouter".to_owned());
        config.openrouter = Some(toker::config::OpenRouterConfig {
            upstream: "http://127.0.0.1:9/v1".parse().unwrap(),
            api_key_env: "TOKER_TEST_KEY_UNSET_CONFIG_CONTROL".to_owned(),
            api_key_keyring: false,
            api_key: None,
            picker: None,
        });
    }
    let store = Arc::new(Store::open(&config.db_path).unwrap());
    let mut catalogs = toker::catalog::FetchedCatalogs::default();
    catalogs.set(
        "openrouter",
        toker::catalog::FetchedCatalog {
            fetched_at_ms: 1,
            models: vec![toker::catalog::FetchedModel {
                id: "vendor/model-a".to_owned(),
                context_window: window,
                chronology: None,
                raw: json!({"id": "vendor/model-a"}),
            }],
        },
    );
    (
        Server::with_seams(
            config,
            store.clone(),
            toker::server::Seams::isolated().with_catalogs(catalogs),
        )
        .expect("authoritative server"),
        store,
    )
}

async fn set_cold_minimum(app: &axum::Router, tokens: u64) -> (StatusCode, Value) {
    let mut row = snapshot(app).await["settings"].clone();
    row["cold_min_tokens"] = json!(tokens);
    let response = app
        .clone()
        .oneshot(request(
            "PUT",
            "/_toker/config/settings",
            Some("config-edit"),
            Some(row),
        ))
        .await
        .unwrap();
    (response.status(), body(response).await)
}

#[tokio::test]
async fn a_cold_minimum_beyond_every_resolved_window_is_refused_naming_the_largest() {
    let (server, store) = server_listing(Some(128_000), true);
    let app = server.router();

    // The page checks a minimum as it is typed against the same figure.
    assert_eq!(snapshot(&app).await["largest_context_window"], 128_000);

    let (status, refusal) = set_cold_minimum(&app, 200_000).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");
    assert_eq!(refusal["setting"], "cold_min_tokens");
    assert!(
        refusal["error"].as_str().unwrap().contains("128000"),
        "{refusal}"
    );
    // The stored value, and the running one, are as they were.
    assert_eq!(
        store.load_configuration().unwrap().settings.cold_min_tokens,
        None
    );
    assert_eq!(
        server.config().gates.cold_min_tokens,
        toker::config::GatesConfig::default().cold_min_tokens
    );

    // The largest window itself can be reached, so it is accepted.
    let (status, accepted) = set_cold_minimum(&app, 128_000).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(
        store.load_configuration().unwrap().settings.cold_min_tokens,
        Some(128_000)
    );
    assert_eq!(server.config().gates.cold_min_tokens, 128_000);
}

#[tokio::test]
async fn a_cold_minimum_is_accepted_where_no_window_resolves() {
    // A listing that names no window, for a model the hand-verified
    // catalogue does not know.
    let (server, store) = server_listing(None, true);
    let app = server.router();
    assert_eq!(snapshot(&app).await["largest_context_window"], Value::Null);
    let (status, accepted) = set_cold_minimum(&app, 5_000_000).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(
        store.load_configuration().unwrap().settings.cold_min_tokens,
        Some(5_000_000)
    );

    // A provider that is off offers nothing, whatever its listing says.
    let (server, _) = server_listing(Some(128_000), false);
    let (status, accepted) = set_cold_minimum(&server.router(), 5_000_000).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
}

#[tokio::test]
async fn an_edit_that_leaves_the_cold_minimum_alone_is_not_refused_for_it() {
    let (server, store) = server_listing(Some(128_000), true);
    let app = server.router();
    // A minimum that was fine when it was stored and is not now.
    let mut row = store.load_configuration().unwrap().settings;
    row.cold_min_tokens = Some(900_000);
    store.put_config_settings(&row).unwrap();

    let status = put_back(
        &app,
        "/_toker/config/settings",
        snapshot(&app).await["settings"].clone(),
        json!({"quota_enabled": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        store.load_configuration().unwrap().settings.cold_min_tokens,
        Some(900_000)
    );
    // The edit is not refused for it, but the condition is not silent: the
    // snapshot says the minimum in force is out of every window's reach.
    assert_eq!(snapshot(&app).await["cold_min_unreachable"], true);
}

/// A minimum is only refused as it is typed, so one that was reachable when
/// it was stored is stranded by turning off the provider that supplied the
/// largest window, and an unset one defaults to a figure never checked at
/// all. Both are said on the snapshot the settings page reads.
#[tokio::test]
async fn a_stranded_cold_minimum_is_said_on_the_snapshot_without_refusing_anything() {
    // The built-in default is larger than this listing's only window, and
    // nothing ever typed it.
    let (server, store) = server_listing(Some(128_000), true);
    let app = server.router();
    assert_eq!(
        store.load_configuration().unwrap().settings.cold_min_tokens,
        None
    );
    assert_eq!(snapshot(&app).await["cold_min_unreachable"], true);

    // A minimum the largest window reaches is not out of reach.
    let (status, accepted) = set_cold_minimum(&app, 128_000).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(snapshot(&app).await["cold_min_unreachable"], false);

    // With no window resolving there is nothing to be out of reach of.
    let (server, _) = server_listing(None, true);
    let snapshot = snapshot(&server.router()).await;
    assert_eq!(snapshot["largest_context_window"], Value::Null);
    assert_eq!(snapshot["cold_min_unreachable"], false);
}

#[tokio::test]
async fn an_edit_made_once_the_store_was_replaced_is_refused_and_not_applied() {
    // spec: DLIFE#keeping-to-its-store
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("toker.db");
    let config = Config {
        db_path: db.clone(),
        ..config()
    };
    let store = Arc::new(Store::open(&db).unwrap());
    let server = Server::with_seams(config, store.clone(), toker::server::Seams::isolated())
        .expect("server");
    let app = server.router();

    let other = dir.path().join("other.db");
    drop(Store::open(&other).unwrap());
    std::fs::rename(&other, &db).expect("rename over");

    let response = app
        .clone()
        .oneshot(request(
            "PUT",
            "/_toker/config/provider",
            Some("config-edit"),
            Some(json!({ "provider": "openrouter", "enabled": true })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let reply = body(response).await;
    assert_eq!(reply["setting"], "store");
    assert!(
        reply["error"]
            .as_str()
            .is_some_and(|error| error.contains("store was replaced")),
        "{reply}"
    );
    assert!(
        store.load_configuration().unwrap().providers.is_empty(),
        "nothing was written to the store the service opened"
    );
    assert!(
        server.config().openrouter.is_none(),
        "nor to the live state"
    );
}

fn active_revision(store: &Store) -> toker::store::PolicyRevision {
    let id = store
        .load_routing_policy_state()
        .unwrap()
        .active_revision_id
        .expect("a revision is active");
    store.load_policy_revision(id).unwrap().expect("it is kept")
}

async fn set_enabled(app: &axum::Router, provider: &str, enabled: bool) {
    let response = app
        .clone()
        .oneshot(request(
            "PUT",
            "/_toker/config/provider",
            Some("config-edit"),
            Some(json!({ "provider": provider, "enabled": enabled })),
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        body(response).await
    );
}

#[tokio::test]
async fn enabling_a_provider_gives_each_protocol_without_a_default_one_to_it() {
    // spec: DCONF#editing
    use toker::policy::{FrontendProtocol, FrontendScope};
    let (server, store) = server();
    let app = server.router();
    let before = active_revision(&store);
    assert!(
        before.document.document().defaults.is_empty(),
        "a machine set up with no provider has no defaults"
    );

    set_enabled(&app, "codex_sub", true).await;

    let after = active_revision(&store);
    assert_ne!(
        after.revision_id, before.revision_id,
        "a revision was saved"
    );
    assert_eq!(
        after.description.as_deref(),
        Some(
            "default for anthropic_messages → codex_sub, openai_chat → codex_sub, \
             openai_responses → codex_sub, on enabling codex_sub"
        )
    );
    let document = after.document.document();
    for protocol in [
        FrontendProtocol::AnthropicMessages,
        FrontendProtocol::OpenaiChat,
        FrontendProtocol::OpenaiResponses,
    ] {
        let default = document
            .defaults
            .iter()
            .find(|default| default.scope == FrontendScope::Protocol { protocol })
            .unwrap_or_else(|| panic!("{protocol:?} has a default"));
        assert_eq!(
            default.target.route_id.as_str(),
            "route-codex_sub-codex_responses"
        );
        assert!(
            document.defaults.iter().any(|default| matches!(
                &default.scope,
                FrontendScope::Profile { profile, protocol: scoped }
                    if profile.as_str() == "unprefixed" && *scoped == protocol
            )),
            "{protocol:?} has an unprefixed default"
        );
    }
    let route = document
        .routes
        .iter()
        .find(|route| route.id.as_str() == "route-codex_sub-codex_responses")
        .expect("the codex route");
    assert!(route.chain.first.enabled, "the route reaches codex");
}

#[tokio::test]
async fn enabling_a_provider_whose_protocols_have_defaults_saves_no_revision() {
    let (server, store) = server();
    let app = server.router();
    set_enabled(&app, "codex_sub", true).await;
    let with_codex = active_revision(&store);

    // OpenRouter serves the same three protocols, each now with a default.
    set_enabled(&app, "openrouter", true).await;
    assert_eq!(active_revision(&store).revision_id, with_codex.revision_id);
}

#[tokio::test]
async fn enabling_a_provider_again_leaves_the_defaults_alone() {
    let (server, store) = server();
    let app = server.router();
    set_enabled(&app, "codex_sub", true).await;
    let enabled = active_revision(&store);

    // Already on: not a transition.
    set_enabled(&app, "codex_sub", true).await;
    assert_eq!(active_revision(&store).revision_id, enabled.revision_id);

    // Off and on again: disabling removes nothing, so there is nothing to add.
    set_enabled(&app, "codex_sub", false).await;
    set_enabled(&app, "codex_sub", true).await;
    let again = active_revision(&store);
    assert_eq!(again.revision_id, enabled.revision_id);
    assert_eq!(
        again.document.document().defaults,
        enabled.document.document().defaults
    );
}
