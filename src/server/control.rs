//! The `/_toker/*` control endpoints (plan: frontend adapters —
//! "localhost only, gated by a custom header (a cross-origin web page
//! cannot drive it without an unanswered preflight)").
//!
//! Gate: the `x-toker-control` header must carry the operation's verb
//! (`status`, `models-merge`, `session`). A page that cannot set a custom
//! header without an unanswered CORS preflight can never pass the gate. A
//! wrong or missing gate is a 403 naming what is missing — loopback-only, no
//! discovery value to suppress.
//!
//! `/_toker/session` is the attribution plugin's query (plan: "the
//! attribution plugin's queries"): opencode sends its session id on every
//! request, toker records it per ledger row, so the plugin asks for one
//! session's aggregate and the answer is exact — the predecessor-era
//! token-vector join is retired. The reply is counts, sums, and labels the rows already
//! carry: no content, no credentials (invariants 1-2).
//!
//! `/_toker/models/merge` is the promote-model handover
//! (the predecessor's control-merge semantics, made real):
//! the promote-model tool grants days to
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
use crate::store::SessionSummary;

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
    // Enabled backends only: an absent block is a disabled backend, with
    // nothing to report.
    let config = &server.config;
    let mut providers = serde_json::Map::new();
    if let Some(sub) = &config.anthropic_sub {
        providers.insert(
            "anthropic_sub".to_owned(),
            json!({ "upstream": sub.upstream.as_str() }),
        );
    }
    if let Some(api) = &config.anthropic_api {
        let sources = api.key_sources();
        providers.insert(
            "anthropic_api".to_owned(),
            json!({
                "upstream": api.upstream.as_str(),
                "api_key_env": api.api_key_env,
                // Key *sources* only — never values (invariant 2).
                "api_key_env_set": sources.env_set,
                "api_key_keyring_configured": sources.keyring_configured,
                "api_key_literal_set": sources.literal_set,
            }),
        );
    }
    if let Some(codex) = &config.codex_sub {
        providers.insert(
            "codex_sub".to_owned(),
            json!({ "upstream": codex.upstream.as_str() }),
        );
    }
    if let Some(openrouter) = &config.openrouter {
        let sources = openrouter.key_sources();
        providers.insert(
            "openrouter".to_owned(),
            json!({
                "upstream": openrouter.upstream.as_str(),
                "api_key_env": openrouter.api_key_env,
                "api_key_env_set": sources.env_set,
                "api_key_keyring_configured": sources.keyring_configured,
                "api_key_literal_set": sources.literal_set,
            }),
        );
    }
    let body = json!({
        "port": config.port,
        "db_path": config.db_path.display().to_string(),
        "session_header_names": config.session_header_names,
        "default_backend_openai_chat": config.default_backend_openai_chat,
        "default_backend_anthropic": config.default_backend_anthropic,
        "providers": providers,
        "requests": rows,
        "last_request_ts_ms": last_request_ts_ms,
        "uptime_s": server.started.elapsed().as_secs(),
    });
    Json(body).into_response()
}

/// `GET /_toker/session?session=<id>` — one session's attribution
/// aggregate for the opencode plugin: the count and span of its
/// measurement rows, token sums, and the billed-cost breakdowns by
/// serving provider and model. The session id is exact (toker records
/// the frontend's own session header per row), so there is no join to
/// do and no unmatched remainder to guess about.
///
/// Absence ≠ zero (invariant 3) in the reply body: a metric no row
/// carried is `null`, never 0, and `billed_total` is `null` when no row
/// was billed. A session id with no rows at all answers `requests: 0`
/// with `null`s — the endpoint cannot distinguish "absent session" from
/// "session that measured nothing", and the plugin renders nothing
/// either way. No content and no credentials leave (invariants 1-2).
pub(crate) async fn session(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "session") {
        return forbidden();
    }
    // The verb check ran; the query names the subject — a GET has no body
    // to carry it. Absence of the parameter is a client bug (the 400
    // names the required shape), an empty value is a real id answered
    // like any session with no rows.
    let Some(session_id) = session_param(request.uri().query()) else {
        return (
            StatusCode::BAD_REQUEST,
            "the session query parameter is required: /_toker/session?session=<id>\n",
        )
            .into_response();
    };
    match server.store.session_summary(&session_id) {
        Ok(summary) => Json(session_body(&session_id, &summary)).into_response(),
        Err(error) => store_error(error),
    }
}

/// The `/_toker/session` reply body. The breakdowns are arrays — empty
/// when there is nothing to break down, never `null`.
fn session_body(session_id: &str, summary: &SessionSummary) -> Value {
    let per_provider: Vec<Value> = summary
        .per_provider
        .iter()
        .map(|group| {
            json!({
                "provider": group.label,
                "requests": group.requests,
                "cost_usd": group.cost_usd,
            })
        })
        .collect();
    let per_model: Vec<Value> = summary
        .per_model
        .iter()
        .map(|group| {
            json!({
                "model": group.label,
                "requests": group.requests,
                "cost_usd": group.cost_usd,
            })
        })
        .collect();
    json!({
        "session": session_id,
        "requests": summary.requests,
        "first_ts_ms": summary.first_ts_ms,
        "last_ts_ms": summary.last_ts_ms,
        "tokens": {
            "input": summary.input,
            "output": summary.output,
            "reasoning": summary.reasoning,
            "cache_read": summary.cache_read,
            "cache_write_total": summary.cache_write_total,
        },
        "cost": {
            "billed_total": summary.billed_total,
            "per_provider": per_provider,
            "per_model": per_model,
        },
    })
}

/// The `session` query parameter — the first occurrence wins, decoded per
/// the `application/x-www-form-urlencoded` rules (a query string is that
/// format). `None` when the parameter is absent; an empty value is a
/// real (empty) session id, not absence.
fn session_param(query: Option<&str>) -> Option<String> {
    for pair in query?.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue; // a key with no `=` carries no value
        };
        if key == "session" {
            return Some(percent_decode(value));
        }
    }
    None
}

/// Percent-decode a query value: `+` is space, `%XX` is a hex byte. An
/// invalid escape passes through verbatim — session ids never carry one,
/// and strict rejection would turn a naming quirk into a hard failure.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if bytes.get(index + 1).is_some_and(|b| b.is_ascii_hexdigit())
                && bytes.get(index + 2).is_some_and(|b| b.is_ascii_hexdigit()) =>
            {
                let hex = |byte: u8| (byte as char).to_digit(16).unwrap_or(0) as u8;
                out.push(hex(bytes[index + 1]) * 16 + hex(bytes[index + 2]));
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `POST /_toker/models/merge` — the promote-model handover
/// (replaces the phase-1
/// 501 stub).
///
/// Body: one entry, `{"model": id, "days": [...], "maxPrompt": n}` — the
/// single-model shape of the store merge, with the
/// day-list validation: `days` must be an array of day strings (non-strings and
/// empties dropped, the rest deduped and sorted), `maxPrompt` a finite
/// number when present. Validation failures answer 400 (`unparseable
/// store`, the frozen wording); a wrong control verb, method, or content type
/// answers 403 (`not a control request`).
///
/// The semantics are `mergeSeen(into, from, {only: true})`: days union and
/// the higher `maxPrompt` for a model **already served**, nothing for one
/// the store has never seen (never invents). The predecessor answered 200
/// with the unknown model on its `refused` list; toker answers **404** with
/// the `known` list, so the caller learns the typo instead of a silent
/// no-op.
///
/// Every granted day must be one the store already holds for some model.
/// The predecessor's promote tool only ever sent such days, but its
/// endpoint took any strings; here a day outside the store answers **400**
/// `invented days` naming them, and nothing is written. An invented day
/// enlarges the bar's denominator for every family at once.
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
    // All three are checked up front and answered alike: method, verb,
    // content type. The verb check already ran above.
    if parts.method != Method::POST || !json_content_type(&parts.headers) {
        return forbidden();
    }
    let bytes = match axum::body::to_bytes(body, MAX_CONTROL_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => return merge_error(StatusCode::BAD_REQUEST, "unparseable store"),
    };

    // The incoming entry is validated before anything else: a store that
    // does not
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
    // A non-finite or absent maxPrompt reads as 0 — the
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
                // The `targets` map, one family touched: what the election
                // names now, so the caller reports the effect.
                "targets": {family: target},
            }))
            .into_response()
        }
        Ok(MergeOutcome::Unseen) => {
            // The frozen reason word ("unseen") and the `known` list, on
            // a status that cannot be mistaken for success.
            let known = server.models.known_models().unwrap_or_default();
            let mut reply = json!({
                "toker": "models-merge",
                "ok": false,
                "error": "unseen",
            });
            reply["known"] = json!(known);
            (StatusCode::NOT_FOUND, Json(reply)).into_response()
        }
        Ok(MergeOutcome::InventedDays(invented)) => {
            // A grant may only hand over days the store already holds:
            // an invented one enlarges every family's bar denominator.
            // Refused whole, so a caller never half-applies a promotion.
            let mut reply = json!({
                "toker": "models-merge",
                "ok": false,
                "error": "invented days",
            });
            reply["days"] = json!(invented);
            (StatusCode::BAD_REQUEST, Json(reply)).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "models merge store failure");
            merge_error(StatusCode::INTERNAL_SERVER_ERROR, "store failure")
        }
    }
}

/// `content-type` starts with `application/json`
/// (the prefix check) — the second half of what keeps a browser
/// out: a cross-origin page cannot send either half without an
/// unanswered preflight.
fn json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
}

/// The error replies: `{toker: "models-merge", ok: false, error}` — the
/// reply signed with the proxy's own name.
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

#[cfg(test)]
mod tests {
    use super::{percent_decode, session_param};

    #[test]
    fn session_param_first_occurrence_wins_and_keys_without_value_are_skipped() {
        assert_eq!(session_param(None), None);
        // No `=`: a key with no value, and other keys never match.
        assert_eq!(session_param(Some("session")), None);
        assert_eq!(
            session_param(Some("other=x&session=ses-1")),
            Some("ses-1".to_owned())
        );
        assert_eq!(
            session_param(Some("session=a&session=b")),
            Some("a".to_owned())
        );
        // An empty value is a real (empty) id, not absence.
        assert_eq!(session_param(Some("session=")), Some(String::new()));
    }

    #[test]
    fn percent_decode_follows_the_query_string_rules() {
        assert_eq!(percent_decode("ses_abc123"), "ses_abc123");
        assert_eq!(percent_decode("a+b"), "a b", "`+` is space");
        assert_eq!(percent_decode("ses%2Fx"), "ses/x");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        // Invalid escapes pass through verbatim rather than failing.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%2"), "%2");
    }
}
