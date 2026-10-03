//! The `/_toker/*` control endpoints (plan: frontend adapters —
//! "localhost only, gated by a custom header (a cross-origin web page
//! cannot drive it without an unanswered preflight)").
//!
//! Gate: the `x-toker-control` header must carry the operation's verb
//! (`status`, `models-merge`). A page that cannot set a custom header
//! without an unanswered CORS preflight can never pass the gate. A wrong
//! or missing gate is a 403 naming what is missing — loopback-only, no
//! discovery value to suppress.

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::Server;

const CONTROL_HEADER: &str = "x-toker-control";

/// `GET /_toker/status` — the resolved config (sans secrets), ledger row
/// count, last request ts, and uptime.
pub(crate) async fn status(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "status") {
        return forbidden();
    }
    let rows = match server.store.count_requests() {
        Ok(rows) => rows,
        Err(error) => return store_error(error),
    };
    let last_request_ts_ms = match server.store.requests_since(0, 1) {
        Ok(rows) => rows.last().map(|row| row.ts_ms),
        Err(error) => return store_error(error),
    };
    let sources = server.config.openrouter.key_sources();
    let anthropic_api_sources = server.config.anthropic_api.key_sources();
    let body = json!({
        "port": server.config.port,
        "db_path": server.config.db_path.display().to_string(),
        "session_header_names": server.config.session_header_names,
        "default_backend_openai_chat": server.config.default_backend_openai_chat,
        "default_backend_anthropic": server.config.default_backend_anthropic,
        "providers": {
            "openrouter": {
                "upstream": server.config.openrouter.upstream.as_str(),
                "api_key_env": server.config.openrouter.api_key_env,
                // Key *sources* only — never values (invariant 2).
                "api_key_env_set": sources.env_set,
                "api_key_literal_set": sources.literal_set,
            },
            "anthropic_sub": {
                "upstream": server.config.anthropic_sub.upstream.as_str(),
            },
            "anthropic_api": {
                "upstream": server.config.anthropic_api.upstream.as_str(),
                "api_key_env": server.config.anthropic_api.api_key_env,
                "api_key_env_set": anthropic_api_sources.env_set,
                "api_key_literal_set": anthropic_api_sources.literal_set,
            },
        },
        "requests": rows,
        "last_request_ts_ms": last_request_ts_ms,
        "uptime_s": server.started.elapsed().as_secs(),
    });
    Json(body).into_response()
}

/// `POST /_toker/models/merge` — phase-2 control parity placeholder.
pub(crate) async fn models_merge(request: Request) -> Response {
    if !control_token_ok(request.headers(), "models-merge") {
        return forbidden();
    }
    (
        StatusCode::NOT_IMPLEMENTED,
        "models/merge arrives with phase 2 control parity\n",
    )
        .into_response()
}

/// The gate check: the control header's value names the operation.
fn control_token_ok(headers: &HeaderMap, verb: &str) -> bool {
    headers
        .get(CONTROL_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(verb))
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        "this endpoint requires the x-toker-control header naming its operation\n",
    )
        .into_response()
}

fn store_error(error: crate::store::Error) -> Response {
    tracing::error!(%error, "control endpoint store read failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "ledger read failed\n").into_response()
}
