//! The `/_toker/*` control endpoints (plan: frontend adapters —
//! "localhost only, gated by a custom header (a cross-origin web page
//! cannot drive it without an unanswered preflight)").
//!
//! Gate: the `x-toker-control` header must carry the operation's verb
//! (`status`, `models-merge`). A page that cannot set a custom header
//! without an unanswered CORS preflight can never pass the gate. A wrong
//! or missing gate is a 403 naming what is missing — loopback-only, no
//! discovery value to suppress.
//!
//! `/_toker/models/merge` is the promote-model handover (ctp
//! `controlMerge`, proxy.mjs:1068-1099): promote-model.mjs grants days to
//! a model the proxy has already served, so a promotion applies to the
//! running process instead of waiting for a restart. It can only add to
//! what the store already knows, and only for models it has served — the
//! worst it can do is what promote-model does on purpose. The custom
//! header and the JSON content type are what keep a web page out.

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::Server;
use crate::middleware::models::{MergeIncoming, MergeOutcome};

const CONTROL_HEADER: &str = "x-toker-control";

/// A control body larger than this is a client bug, not a promotion —
/// the handover carries one model's days and ceiling.
const MAX_CONTROL_BODY: usize = 1024 * 1024;

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

/// `POST /_toker/models/merge` — the promote-model handover (ctp
/// `controlMerge`, proxy.mjs:1068-1099, made real; replaces the phase-1
/// 501 stub).
///
/// Body: one entry, `{"model": id, "days": [...], "maxPrompt": n}` — the
/// single-model shape of ctp's store merge, with ctp's `pruneSeen`
/// validation: `days` must be an array of day strings (non-strings and
/// empties dropped, the rest deduped and sorted), `maxPrompt` a finite
/// number when present. Validation failures answer 400 (`unparseable
/// store`, ctp's wording); a wrong control verb, method, or content type
/// answers 403 (`not a control request`).
///
/// The semantics are `mergeSeen(into, from, {only: true})`: days union and
/// the higher `maxPrompt` for a model **already served**, nothing for one
/// the store has never seen (never invents). ctp answered 200 with the
/// unknown model on its `refused` list; toker answers **404** with the
/// `known` list, so the caller learns the typo instead of a silent no-op.
///
/// The reply carries the effect, not the intent: `target` is what the
/// family's election now names, because a promotion does not guarantee the
/// slot — a newer version may already hold it, and saying so beats leaving
/// the caller to discover it. The path is never forwarded, whatever the
/// outcome.
pub(crate) async fn models_merge(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "models-merge") {
        return forbidden();
    }
    let (parts, body) = request.into_parts();
    // ctp checks all three up front and answers them alike: method, verb,
    // content type. The verb check already ran above.
    if parts.method != Method::POST || !json_content_type(&parts.headers) {
        return forbidden();
    }
    let bytes = match axum::body::to_bytes(body, MAX_CONTROL_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => return merge_error(StatusCode::BAD_REQUEST, "unparseable store"),
    };

    // ctp `pruneSeen` over the incoming entry: a store that does not
    // validate is a store that does not apply, never a best-effort guess.
    let incoming: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return merge_error(StatusCode::BAD_REQUEST, "unparseable store"),
    };
    let Some(model) = incoming.get("model").and_then(Value::as_str) else {
        return merge_error(StatusCode::BAD_REQUEST, "unparseable store");
    };
    let Some(model) = crate::catalog::windows::model_identity(model) else {
        return merge_error(StatusCode::BAD_REQUEST, "unparseable store");
    };
    let Some(days) = incoming.get("days").and_then(Value::as_array) else {
        return merge_error(StatusCode::BAD_REQUEST, "unparseable store");
    };
    let days: Vec<String> = {
        let mut days: Vec<String> = days
            .iter()
            .filter_map(|day| day.as_str().map(str::to_owned))
            .filter(|day| !day.is_empty())
            .collect();
        days.sort();
        days.dedup();
        days
    };
    // ctp `pruneSeen`: a non-finite or absent maxPrompt reads as 0 — the
    // merge then simply cannot raise the ceiling, only the days.
    let max_prompt = incoming
        .get("maxPrompt")
        .and_then(Value::as_f64)
        .filter(|prompt| prompt.is_finite())
        .map(|prompt| prompt.max(0.0) as i64);

    match server.models.merge(&MergeIncoming {
        model_id: model,
        days,
        max_prompt,
    }) {
        Ok(MergeOutcome::Merged { entry, target }) => {
            let family = crate::middleware::models::family_of(&entry.model_id)
                .map(|family| family.name)
                .unwrap_or_default();
            Json(json!({
                "toker": "models-merge",
                "ok": true,
                "merged": [entry.model_id],
                "refused": [],
                // ctp's `targets`, one family touched: what the election
                // names now, so the caller reports the effect.
                "targets": {family: target},
            }))
            .into_response()
        }
        Ok(MergeOutcome::Unseen) => {
            // ctp's reason word ("unseen") and its `known` list, on a
            // status that cannot be mistaken for success.
            let known = server.models.known_models().unwrap_or_default();
            let mut reply = json!({
                "toker": "models-merge",
                "ok": false,
                "error": "unseen",
            });
            reply["known"] = json!(known);
            (StatusCode::NOT_FOUND, Json(reply)).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "models merge store failure");
            merge_error(StatusCode::INTERNAL_SERVER_ERROR, "store failure")
        }
    }
}

/// `content-type` starts with `application/json` (ctp controlMerge's
/// `String(...).startsWith`) — the second half of what keeps a browser
/// out: a cross-origin page cannot send either half without an
/// unanswered preflight.
fn json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
}

/// ctp's error replies: `{ctp: "models-merge", ok: false, error}` — the
/// proxy's name swapped for toker's.
fn merge_error(status: StatusCode, error: &str) -> Response {
    (
        status,
        Json(json!({
            "toker": "models-merge",
            "ok": false,
            "error": error,
        })),
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
