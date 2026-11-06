//! Import of the legacy `toker.toml` routing fields into the draft.

use anyhow::Context;
use serde_json::Value;

use crate::config::Config;
use crate::policy::{legacy, policy_bytes};
use crate::routing::RouteRegistry;

/// Reload `toker.toml` and compile its legacy routing fields (protocol
/// defaults, model maps, picker rules, `force_newest`, `compact_model`) into a
/// canonical policy document, ready to replace the draft. Any failure to load
/// or compile the file is returned as it is, so the draft stays as it was.
pub(super) fn legacy_policy() -> anyhow::Result<Value> {
    let config = Config::load().context("loading toker.toml")?;
    document_for(&config)
}

fn document_for(config: &Config) -> anyhow::Result<Value> {
    // Unlike a database-owned machine's bootstrap file, an import reads the
    // protocol defaults as routing input, so ambiguity is an error here.
    config.validate()?;
    let registry = RouteRegistry::configured_bindings(crate::picker::configured_bindings(config));
    let document = legacy::validated(config, &registry)?;
    let bytes = policy_bytes(&document).context("encoding the legacy policy")?;
    serde_json::from_slice(&bytes).context("reading the encoded legacy policy")
}

#[cfg(test)]
mod tests {
    use super::document_for;
    use crate::config::Config;

    fn load(text: &str) -> Config {
        let dir = crate::test_support::tempdir("tui-import-");
        std::fs::write(dir.join("toker.toml"), text).unwrap();
        Config::load_from(&dir.join("toker.toml")).unwrap()
    }

    #[test]
    fn the_legacy_fields_compile_into_a_canonical_document() {
        let config = load(
            "default_backend_anthropic = \"anthropic_api\"\n\
             [providers.anthropic_sub]\n[providers.anthropic_api]\n\
             [providers.anthropic_api.model_map]\n\"family:sonnet\" = \"claude-sonnet-5\"\n",
        );
        let document = document_for(&config).unwrap();
        assert!(document.get("format_version").is_some());
        assert!(document.to_string().contains("claude-sonnet-5"));
    }

    #[test]
    fn a_config_with_an_ambiguous_default_is_refused() {
        let config = load("[providers.anthropic_api]\n[providers.codex_sub]\n");
        let error = document_for(&config).unwrap_err();
        assert!(format!("{error:#}").contains("default_backend_anthropic"));
    }

    #[tokio::test]
    async fn the_daemon_accepts_the_imported_document_as_a_draft() {
        use std::sync::Arc;

        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        // The daemon's active revision comes from a config with no picker
        // rows; the imported file offers an empty picker, so the draft differs.
        let mut config = load("[providers.openrouter]\n");
        config.db_path = std::path::PathBuf::from(":memory:");
        let store = Arc::new(crate::store::Store::open(&config.db_path).unwrap());
        let app = crate::server::Server::new_authoritative(config, store)
            .unwrap()
            .router();
        let snapshot = |app: axum::Router| async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/_toker/routing/snapshot")
                        .header("x-toker-control", "routing-snapshot")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let bytes = axum::body::to_bytes(response.into_body(), 4 << 20)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };
        let before = snapshot(app.clone()).await;
        let generation = before["draft_generation"].as_i64().unwrap();

        let imported = load("[providers.openrouter]\npicker = []\n");
        let policy = document_for(&imported).unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/_toker/routing/draft")
                    .header("x-toker-control", "routing-draft-replace")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"expected_generation": generation, "policy": policy})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let after = snapshot(app.clone()).await;
        assert_eq!(after["draft_generation"], generation + 1);
        assert_ne!(after["draft"]["policy"], before["draft"]["policy"]);
        assert_eq!(
            after["active_revision"], before["active_revision"],
            "the active revision is untouched"
        );

        // The imported draft activates like any other.
        let activated = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_toker/routing/activate")
                    .header("x-toker-control", "routing-activate")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "expected_generation": generation + 1,
                            "scope": "new_lanes",
                            "force_cache_transition": false,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(activated.status(), 200);
    }
}
