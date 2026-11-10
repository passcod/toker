//! The `/_toker/*` control endpoints.
//!
//! Gate: the `x-toker-control` header must carry the operation's verb
//! (`status`, `session`, `shutdown`, and one per routing operation). A page
//! cannot set a custom header without a CORS preflight nothing answers, so
//! none can pass the gate. A wrong or missing gate is a 403 naming what is
//! missing: loopback-only, so there is no discovery value to suppress.
//!
//! `/_toker/session` is the attribution plugin's query: opencode sends its
//! session id on every request, toker records it per ledger row, so the
//! plugin asks for one session's aggregate and the answer is exact. The reply
//! is counts, sums, and labels the rows already carry: no content, no
//! credentials (spec: SEC#no-prompt-or-completion-content-is-recorded, spec: SEC#a-credential-is-never-logged-or-displayed).
//!
//! `/_toker/shutdown` is `toker restart`'s second step: the CLI waits
//! for a quiet moment on `status`, then asks this instance, by the id
//! `status` gave it, to drain and exit; systemd starts the next one. It
//! is gated exactly like the other mutating endpoints (verb header and a JSON
//! content type). The worst a caller can do with
//! it is what `systemctl --user restart` does: by default the drain lets
//! every response under way finish, and with `force` they are cut, but
//! the exit is the normal one either way.

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

pub(super) mod config;
pub(crate) mod derived;
mod revision_diff;
pub(super) mod routing;

use super::Server;
use crate::middleware::cold::ttl_of;
use crate::store::{Lane, SessionSummary};

const CONTROL_HEADER: &str = "x-toker-control";

/// A control body larger than this is a client bug: the largest one is a
/// routing policy document.
const MAX_CONTROL_BODY: usize = 1024 * 1024;

/// `GET /_toker/status` — the resolved config (sans secrets), ledger row
/// count, last request ts, and uptime.
pub(crate) async fn status(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "status") {
        return forbidden();
    }
    let rows = match server.store.count_logical_requests() {
        Ok(rows) => rows,
        Err(error) => return store_error(error),
    };
    let last_request_ts_ms = match server.store.requests_since(0, 1) {
        Ok(rows) => rows.last().map(|row| row.ts_ms),
        Err(error) => return store_error(error),
    };
    // Enabled backends only: an absent block is a disabled backend, with
    // nothing to report.
    let config = &server.config();
    let mut providers = serde_json::Map::new();
    if let Some(sub) = &config.anthropic_sub {
        let sources = sub.oauth_sources();
        providers.insert(
            "anthropic_sub".to_owned(),
            json!({
                "upstream": sub.upstream.as_str(),
                "oauth_token_env": sub.oauth_token_env,
                "oauth_token_env_set": sources.env_set,
                "oauth_token_keyring_configured": sources.keyring_configured,
                "oauth_token_literal_set": sources.literal_set,
                "claude_login_configured": sub.claude_credentials_path.is_some(),
            }),
        );
    }
    if let Some(api) = &config.anthropic_api {
        let sources = api.key_sources();
        providers.insert(
            "anthropic_api".to_owned(),
            json!({
                "upstream": api.upstream.as_str(),
                "api_key_env": api.api_key_env,
                // Key *sources* only — never values (spec: SEC#a-credential-is-never-logged-or-displayed).
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
    if let Some(api) = &config.openai_api {
        let sources = api.key_sources();
        providers.insert(
            "openai_api".to_owned(),
            json!({
                "upstream": api.upstream.as_str(),
                "api_key_env": api.api_key_env,
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
        "policy_revision_id": server.policy_runtime.capture().revision_id(),
        "providers": providers,
        "requests": rows,
        "last_request_ts_ms": last_request_ts_ms,
        "uptime_s": server.started.elapsed().as_secs(),
        // Exchanges whose response has not finished (the sleep lock's own
        // count). Zero is when a restart cuts nobody off: a restart aborts
        // every response still streaming.
        "in_flight": server.in_flight.load(std::sync::atomic::Ordering::SeqCst),
        // Which process answered: `toker restart` waits for this to
        // change after it asks for a shutdown.
        "instance": &*server.instance,
        "started_ms": server.started_ms,
        "version": env!("CARGO_PKG_VERSION"),
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
/// Absence ≠ zero (spec: STORE#not-recorded-is-not-zero) in the reply body: a metric no row
/// carried is `null`, never 0, and `billed_total` is `null` when no row
/// was billed. A session id with no rows at all answers `requests: 0`
/// with `null`s — the endpoint cannot distinguish "absent session" from
/// "session that measured nothing", and the plugin renders nothing
/// either way. No content and no credentials leave (spec: SEC#no-prompt-or-completion-content-is-recorded, spec: SEC#a-credential-is-never-logged-or-displayed).
///
/// `lanes` is the session's rows from the lane table (exact session id,
/// newest first), each with a state computed now (see `lane_body`). A
/// session the table does not know, or has pruned, answers `[]`, never an
/// invented cold lane.
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
    let summary = match server.store.session_summary(&session_id) {
        Ok(summary) => summary,
        Err(error) => return store_error(error),
    };
    let lanes = match server.store.load_session_lanes(&session_id) {
        Ok(lanes) => lanes,
        Err(error) => return store_error(error),
    };
    Json(session_body(
        &session_id,
        &summary,
        &lanes,
        super::record::now_ms(),
    ))
    .into_response()
}

/// The `/_toker/session` reply body. The breakdowns and `lanes` are arrays
/// — empty when there is nothing to break down, never `null`.
fn session_body(session_id: &str, summary: &SessionSummary, lanes: &[Lane], now_ms: i64) -> Value {
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
        "lanes": lanes.iter().map(|lane| lane_body(lane, now_ms)).collect::<Vec<_>>(),
    })
}

/// How many leading characters of a lane's tool-set digest identify it in
/// the reply. The digest is already a one-way hash of the tool list; the
/// reply carries a short prefix so a client can tell lanes apart without
/// holding the lane key.
const TOOLS_ID_CHARS: usize = 6;

/// One lane as the session reply shows it: the operational metadata the
/// `lanes` row already keeps, plus a state computed at query time. Never
/// content (spec: SEC#no-prompt-or-completion-content-is-recorded), and a field the lane never recorded is `null`,
/// not zero (spec: STORE#not-recorded-is-not-zero's absence rule).
///
/// - `state`: `ping` for a ping lane (its cache is not the conversation's,
///   so it is never called warm or cold), else `cold` once the idle time
///   has reached the cache's lifetime (`cold::ttl_of`, the gate's own
///   reading, so an unrecorded tier is the long one) and `warm` before.
///   A lane stamped in the future (a clock that moved) has no idle time
///   to measure: `null`, as the cold gate treats it.
/// - `warm_remaining_ms`: only for a warm lane whose TTL was actually
///   recorded. An unrecorded tier gives the gate a long default but
///   gives this view nothing to count down from.
/// - `forced`: the sticky model transition, `null` when the lane has none.
fn lane_body(lane: &Lane, now_ms: i64) -> Value {
    let idle_ms = (now_ms >= lane.updated_ms).then(|| now_ms - lane.updated_ms);
    let ttl_ms = ttl_of(lane);
    let state = match idle_ms {
        _ if lane.ping == Some(true) => Some("ping"),
        None => None,
        Some(idle) if idle >= ttl_ms => Some("cold"),
        Some(_) => Some("warm"),
    };
    let warm_remaining_ms = match (state, idle_ms, lane.ttl) {
        (Some("warm"), Some(idle), Some(_)) => Some(ttl_ms - idle),
        _ => None,
    };
    let forced = (lane.forced_from.is_some() || lane.forced_to.is_some())
        .then(|| json!({"from": lane.forced_from, "to": lane.forced_to}));
    json!({
        "tools_id": lane
            .tools_hash
            .as_ref()
            .map(|hash| hash.chars().take(TOOLS_ID_CHARS).collect::<String>()),
        "state": state,
        "ping": lane.ping == Some(true),
        "updated_ms": lane.updated_ms,
        "idle_ms": idle_ms,
        "prompt_tokens": lane.prompt_tokens,
        "ttl_ms": lane.ttl,
        "warm_remaining_ms": warm_remaining_ms,
        "noticed_ms": lane.noticed_at,
        "forced": forced,
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

/// `POST /_toker/shutdown` — drain and exit, for `toker restart`.
///
/// Body: `{"instance": id}`, the id `/_toker/status` reported, and
/// optionally `"force": true` to stop without waiting for connections
/// under way (see [`Server::serve_listener`]): the restart's second
/// request, once the drain it asked for first has had its time. Any other
/// `force` is a 400. The gate is
/// the other mutating endpoints': a wrong verb, method, or content type is a 403. A body
/// that is not that object is a 400; an id that is not this instance's is
/// a 409 carrying the current one, so a restart that raced another never
/// stops an instance it did not see. Otherwise the answer is 202 and the
/// drain begins (see [`Server::serve_listener`]): this response finishes
/// like every other, then the listener closes. The path is never
/// forwarded, whatever the outcome.
pub(crate) async fn shutdown(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "shutdown") {
        return forbidden();
    }
    let (parts, body) = request.into_parts();
    if parts.method != Method::POST || !json_content_type(&parts.headers) {
        return forbidden();
    }
    let Ok(bytes) = axum::body::to_bytes(body, MAX_CONTROL_BODY).await else {
        return shutdown_error(StatusCode::BAD_REQUEST, "unparseable request");
    };
    let incoming: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return shutdown_error(StatusCode::BAD_REQUEST, "unparseable request"),
    };
    let Some(instance) = incoming.get("instance").and_then(Value::as_str) else {
        return shutdown_error(StatusCode::BAD_REQUEST, "unparseable request");
    };
    if instance != &*server.instance {
        let mut reply = json!({
            "toker": "shutdown",
            "ok": false,
            "error": "instance mismatch",
        });
        reply["instance"] = json!(&*server.instance);
        return (StatusCode::CONFLICT, Json(reply)).into_response();
    }
    let force = match incoming.get("force") {
        None => false,
        Some(Value::Bool(force)) => *force,
        Some(_) => return shutdown_error(StatusCode::BAD_REQUEST, "unparseable request"),
    };
    let in_flight = server.in_flight.load(std::sync::atomic::Ordering::SeqCst);
    server.begin_shutdown(force);
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "toker": "shutdown",
            "ok": true,
            "instance": &*server.instance,
            "in_flight": in_flight,
        })),
    )
        .into_response()
}

fn shutdown_error(status: StatusCode, error: &str) -> Response {
    (
        status,
        Json(json!({
            "toker": "shutdown",
            "ok": false,
            "error": error,
        })),
    )
        .into_response()
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

#[cfg(test)]
mod lane_tests {
    use serde_json::{Value, json};

    use super::lane_body;
    use crate::store::Lane;

    const NOW: i64 = 10_000_000;
    const MIN: i64 = 60_000;

    fn lane(updated_ms: i64, ttl: Option<i64>) -> Lane {
        Lane {
            key: "ses|0123456789ab".to_owned(),
            session_id: Some("ses".to_owned()),
            tools_hash: Some("0123456789ab".to_owned()),
            updated_ms,
            prompt_tokens: Some(180_000),
            ttl,
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
            pin_generation: 0,
            pin: None,
        }
    }

    #[test]
    fn a_warm_lane_counts_down_its_recorded_ttl() {
        let body = lane_body(&lane(NOW - 2 * MIN, Some(5 * MIN)), NOW);
        assert_eq!(body["state"], "warm");
        assert_eq!(body["idle_ms"], 2 * MIN);
        assert_eq!(body["warm_remaining_ms"], 3 * MIN);
        assert_eq!(body["ttl_ms"], 5 * MIN);
        assert_eq!(body["tools_id"], "012345", "a short prefix, not the digest");
        assert_eq!(body["prompt_tokens"], 180_000);
        assert_eq!(body["forced"], Value::Null);
        assert_eq!(body["noticed_ms"], Value::Null);
    }

    #[test]
    fn a_lane_is_cold_at_exactly_its_ttl() {
        let body = lane_body(&lane(NOW - 5 * MIN, Some(5 * MIN)), NOW);
        assert_eq!(body["state"], "cold");
        assert_eq!(body["warm_remaining_ms"], Value::Null);
        let body = lane_body(&lane(NOW - 5 * MIN + 1, Some(5 * MIN)), NOW);
        assert_eq!(body["state"], "warm");
        assert_eq!(body["warm_remaining_ms"], 1);
    }

    #[test]
    fn an_unrecorded_ttl_reads_the_gates_long_tier_but_counts_nothing_down() {
        let body = lane_body(&lane(NOW - 20 * MIN, None), NOW);
        assert_eq!(body["state"], "warm", "ttl_of's long default, as the gate");
        assert_eq!(body["ttl_ms"], Value::Null);
        assert_eq!(
            body["warm_remaining_ms"],
            Value::Null,
            "no recorded tier, no invented countdown"
        );
        let body = lane_body(&lane(NOW - 61 * MIN, None), NOW);
        assert_eq!(body["state"], "cold");
    }

    #[test]
    fn a_ping_lane_is_never_warm_or_cold() {
        let mut ping = lane(NOW - MIN, Some(5 * MIN));
        ping.ping = Some(true);
        let body = lane_body(&ping, NOW);
        assert_eq!(body["state"], "ping");
        assert_eq!(body["ping"], true);
        assert_eq!(body["warm_remaining_ms"], Value::Null);
        ping.updated_ms = NOW - 3 * 3_600_000;
        assert_eq!(lane_body(&ping, NOW)["state"], "ping", "idle or not");
    }

    #[test]
    fn a_future_stamp_has_no_state_and_missing_fields_stay_null() {
        let mut future = lane(NOW + MIN, Some(5 * MIN));
        future.prompt_tokens = None;
        future.tools_hash = None;
        let body = lane_body(&future, NOW);
        assert_eq!(body["state"], Value::Null);
        assert_eq!(body["idle_ms"], Value::Null);
        assert_eq!(body["warm_remaining_ms"], Value::Null);
        assert_eq!(body["prompt_tokens"], Value::Null, "not zero");
        assert_eq!(body["tools_id"], Value::Null);
    }

    #[test]
    fn the_sticky_model_transition_and_notice_time_ride_along_when_present() {
        let mut moved = lane(NOW - MIN, Some(5 * MIN));
        moved.noticed_at = Some(NOW - 30_000);
        moved.forced_from = Some("model-a".to_owned());
        moved.forced_to = Some("model-b".to_owned());
        let body = lane_body(&moved, NOW);
        assert_eq!(body["forced"], json!({"from": "model-a", "to": "model-b"}));
        assert_eq!(body["noticed_ms"], NOW - 30_000);
    }
}
