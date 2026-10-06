//! `toker picker sync` end to end: a mock openrouter listing in, claude's
//! settings file out. No network, no real settings: every path is a
//! temp directory.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::{Value, json};
use toker::config::Config;

fn test_dir(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "toker-picker-sync-{name}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

async fn spawn_listing(status: StatusCode) -> SocketAddr {
    let fixture = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/openrouter/models-2026-10-06.json"
    ))
    .expect("fixture");
    let app = Router::new().route(
        "/v1/models",
        get(move || async move { (status, fixture.clone()) }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("mock binds");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("mock serves") });
    addr
}

fn config(dir: &std::path::Path, toml: &str) -> Config {
    let path = dir.join("toker.toml");
    std::fs::write(
        &path,
        format!("db_path = \"{}\"\n{toml}", dir.join("absent.db").display()),
    )
    .expect("write config");
    Config::load_from(&path).expect("config loads")
}

fn picker_models(settings: &std::path::Path) -> Vec<String> {
    let value: Value =
        serde_json::from_str(&std::fs::read_to_string(settings).expect("settings")).expect("json");
    value["modelPicker"]["options"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["model"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_sync_writes_the_rule_rows_and_a_resync_changes_nothing() {
    let addr = spawn_listing(StatusCode::OK).await;
    let dir = test_dir("rows");
    let config = config(
        &dir,
        &format!(
            "[providers.openrouter]\nupstream = \"http://{addr}/v1\"\n\n\
             [[providers.openrouter.picker]]\nmatch = [\"moonshotai/kimi-k*\"]\n\
             exclude = [\"*-code\", \"*-thinking\"]\nbehaves_as = \"claude-sonnet-5\"\n\
             variant = \":floor\"\n"
        ),
    );
    let settings = dir.join("settings.json");
    std::fs::write(
        &settings,
        serde_json::to_string_pretty(&json!({
            "env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:18123/f/claude"},
            "modelPicker": {"options": [{"model": "claude-opus-4-8"}]},
        }))
        .expect("serialise"),
    )
    .expect("write settings");
    let http = reqwest::Client::new();

    let report = toker::picker::sync(&config, &settings, &http, false)
        .await
        .expect("sync");
    assert!(report.changed);
    assert_eq!(
        picker_models(&settings),
        ["claude-opus-4-8", "openrouter/moonshotai/kimi-k3:floor"],
        "the user's row stays first, the rule's row follows"
    );

    let again = toker::picker::sync(&config, &settings, &http, false)
        .await
        .expect("resync");
    assert!(!again.changed, "the same listing changes nothing");
}

#[tokio::test]
async fn a_failed_fetch_leaves_the_picker_alone() {
    let addr = spawn_listing(StatusCode::BAD_GATEWAY).await;
    let dir = test_dir("failed");
    let config = config(
        &dir,
        &format!("[providers.openrouter]\nupstream = \"http://{addr}/v1\"\n"),
    );
    let settings = dir.join("settings.json");
    let original = serde_json::to_string_pretty(&json!({
        "modelPicker": {"options": [{"model": "openrouter/kept/model"}]},
    }))
    .expect("serialise");
    std::fs::write(&settings, &original).expect("write settings");

    let result = toker::picker::sync(&config, &settings, &reqwest::Client::new(), false).await;
    assert!(result.is_err(), "the fetch error surfaces");
    assert_eq!(
        std::fs::read_to_string(&settings).expect("settings"),
        original,
        "a listing that could not be read never empties the picker"
    );
}

#[tokio::test]
async fn without_an_openrouter_block_toker_s_rows_go() {
    let dir = test_dir("no-block");
    let config = config(&dir, "[providers.anthropic_sub]\n");
    let settings = dir.join("settings.json");
    std::fs::write(
        &settings,
        serde_json::to_string_pretty(&json!({
            "modelPicker": {"options": [
                {"model": "openrouter/old/model"},
                {"model": "claude-opus-4-8"},
            ]},
        }))
        .expect("serialise"),
    )
    .expect("write settings");

    let report = toker::picker::sync(&config, &settings, &reqwest::Client::new(), false)
        .await
        .expect("sync");
    assert!(report.changed);
    assert_eq!(picker_models(&settings), ["claude-opus-4-8"]);
}
