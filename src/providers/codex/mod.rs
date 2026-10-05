//! The codex subscription backend (plan: "Backend providers") — the
//! OpenAI-Responses protocol against the ChatGPT backend API, the wire
//! the official codex CLI speaks (`POST
//! …/backend-api/codex/responses`, SSE responses).
//!
//! Clean-room note: built from the codex CLI's own source (codex-rs:
//! the client, login, and rate-limit crates) — the only reference.
//! This unit is the client + auth half: the provider identity, the
//! shared `~/.codex/auth.json` login (load, JWT claims, refresh), the
//! request wire types, the responses-dialect SSE parser, and the
//! `x-codex-*` usage-limit meters. The routing unit that serves
//! `/v1/responses` onto this backend lands later (plan: the OpenAI
//! Responses frontend adapter, [`crate::proto::openai_responses`]); HTTP
//! only for now — the CLI's websocket transport and zstd request
//! compression are not replicated.
//!
//! ## Auth is ALWAYS toker-signed — never pass-through
//!
//! [`Provider::credential_present`] answers `false` unconditionally,
//! whatever the incoming request carries. The responses frontend's
//! clients talk to a proxy by pointing at it with *some* credential
//! (claude ships a dummy bearer to satisfy its own client config), and
//! that value is a frontend-to-toker arrangement — never a
//! chatgpt.com credential. Forwarding it upstream would leak the
//! frontend's secret to a third party and fail anyway, so the server's
//! pass-through-when-present rule is deliberately switched OFF for this
//! provider: [`Provider::inject_auth`] runs on every request, and it
//! **first strips any `authorization` the frontend brought**, then
//! injects the codex bearer, the account id, and the FedRAMP marker.
//! With no stored login the request goes up cleanly unauthenticated and
//! chatgpt.com's 401 body passes through — the visible wiring check
//! (the ledger-proxy lesson) — without the dummy bearer riding along.
//! [`auth_headers`] is the shared implementation, exported for the
//! routing unit.
//!
//! ## Layout
//!
//! - `auth` ([`CodexAuth`]): the shared `auth.json` — load, JWT claims
//!   (`exp`, account id, plan type, FedRAMP), [`CodexAuth::needs_refresh`],
//!   the refresh-token grant with re-read-before-refresh and atomic
//!   persist.
//! - `types` ([`ResponsesRequest`]): the request body (routing fields
//!   first, `store:false` and `include:["reasoning.encrypted_content"]`
//!   pinned), the input [`Item`]s and [`Tool`]s, [`Usage`], and the SSE
//!   [`ResponseEvent`] types.
//! - `sse` ([`ResponsesSse`]): the incremental responses-dialect parser
//!   (typed events off the generic splitter; no `[DONE]`, the stream
//!   ends at `response.completed`/`response.incomplete`), plus
//!   [`TurnCapture`], the one-turn accumulator.
//! - `meters` ([`parse_usage_limits`]): the `x-codex-*` usage-limit
//!   header snapshot (the plan names the codex sub as the meter source
//!   the gate's open interface is waiting for).
//!
//! Invariant 2 (credentials): the codex tokens are the user's ChatGPT
//! login. They never appear in the ledger, in a log line, or in an
//! error message — `CodexAuth`'s `Debug` redacts them by hand.

mod auth;
mod meters;
mod sse;
mod types;

pub use auth::{CLIENT_ID, CodexAuth};
pub use meters::parse_usage_limits;
pub use sse::{ResponsesSse, TurnCapture};
pub use types::{
    CompletedResponse, ContentPart, FunctionCall, FunctionCallOutput, FunctionTool, Item,
    MessageItem, Reasoning, ReasoningItem, ResponseError, ResponseEvent, ResponsesRequest, Tool,
    Usage,
};

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::Context;
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use reqwest::Url;
use serde_json::Value;

use super::Provider;

/// The `version` header: toker's own crate version (the codex client
/// sends its version the same way).
/// The codex client version toker identifies as. **Not toker's own
/// crate version**: the backend gates models by this header (a request
/// with an old version is refused with "requires a newer version of
/// Codex"), so toker must speak a version the ecosystem recognizes.
/// Resolved at construction: config override → the installed CLI's
/// own `version.json` (`latest_version`, which the CLI's update check
/// keeps current) → this floor, the newest version verified to work.
pub const DEFAULT_CLIENT_VERSION: &str = "0.154.0";

/// `ChatGPT-Account-ID` — sent when the login names an account.
const CHATGPT_ACCOUNT_ID: HeaderName = HeaderName::from_static("chatgpt-account-id");

/// `X-OpenAI-Fedramp: true` — sent when the id token says FedRAMP.
const X_OPENAI_FEDRAMP: HeaderName = HeaderName::from_static("x-openai-fedramp");

/// The codex subscription backend: upstream
/// `https://chatgpt.com/backend-api/codex`, auth always toker-signed
/// from the shared `auth.json`, quota meters from the `x-codex-*`
/// response headers.
pub struct CodexSub {
    /// The upstream base (the codex backend root, no `/responses`
    /// suffix — frontend paths carry their own, like anthropic's).
    upstream: Url,
    /// The `originator` the codex client identifies itself with
    /// (config; default `codex_cli_rs`).
    originator: String,
    /// Where the shared `auth.json` lives.
    auth_path: PathBuf,
    /// The OAuth refresh endpoint (config).
    refresh_url: Url,
    /// The loaded login, reused across turns (the refresh flow
    /// replaces it in place). `None` while no `auth.json` exists.
    auth: Mutex<Option<CodexAuth>>,
    /// Serialises refreshes: one token refresh in flight at a time —
    /// a second turn that waited adopts the refreshed login instead of
    /// refreshing again with an already-rotated token.
    refresh_lock: tokio::sync::Mutex<()>,
    /// The operator's model routing map
    /// (`[providers.codex_sub.model_map]`): claude asks for
    /// `claude-opus-5`, the codex backend receives `gpt-5.6-sol`.
    model_map: Option<crate::middleware::model_map::ModelMap>,
    /// The codex client version to identify as (see
    /// [`DEFAULT_CLIENT_VERSION`]) — resolved at construction from the
    /// local sources (config override → the installed CLI's own
    /// `version.json` → the built-in floor), then **upgraded in place**
    /// by the background probe when the ecosystem's latest release is
    /// newer. An [`RwLock`] because [`CodexSub::turn_headers`] is sync.
    client_version: std::sync::RwLock<String>,
}

impl CodexSub {
    /// Build the provider, loading `auth.json` once. A missing file is
    /// fine (no login: requests go upstream unauthenticated and the
    /// 401 body passes through); a present-but-corrupt file is an
    /// error, surfacing at startup rather than as a mystery 401 mid
    /// session.
    pub fn new(
        upstream: Url,
        originator: String,
        auth_path: PathBuf,
        refresh_url: Url,
        model_map: Option<crate::middleware::model_map::ModelMap>,
        client_version: Option<String>,
        version_probe: bool,
    ) -> anyhow::Result<CodexSub> {
        let auth = CodexAuth::load(&auth_path)
            .with_context(|| format!("loading {}", auth_path.display()))?;
        // The version handshake reads the CLI's own records beside the
        // shared login — resolve it before the path moves into the
        // provider. A pin wins absolutely: the probe never runs past
        // an operator-chosen version.
        let pinned = client_version.is_some();
        let client_version = client_version
            .or_else(|| installed_cli_version(&auth_path))
            .unwrap_or_else(|| DEFAULT_CLIENT_VERSION.to_owned());
        let provider = CodexSub {
            upstream,
            originator,
            auth_path,
            refresh_url,
            auth: Mutex::new(auth),
            refresh_lock: tokio::sync::Mutex::new(()),
            model_map,
            client_version: std::sync::RwLock::new(client_version),
        };
        // A pinned version wins absolutely (the operator chose it);
        // the probe only complements a resolved one.
        if version_probe && !pinned {
            provider.spawn_latest_version_probe();
        }
        Ok(provider)
    }

    /// The loaded login, cloned out of the cache — the snapshot
    /// [`Provider::inject_auth`] signs with. Refresh is not attempted
    /// here; [`CodexSub::auth_for_turn`] is the refreshing variant.
    pub fn auth(&self) -> Option<CodexAuth> {
        self.auth
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The login for one turn: the cached snapshot, refreshed first
    /// when [`CodexAuth::needs_refresh`] says so. `None` when no login
    /// is stored — the request then goes upstream unauthenticated.
    ///
    /// `now` is the caller's clock in unix seconds, passed in so tests
    /// pin the thresholds exactly and no hidden clock lives here.
    pub async fn auth_for_turn(
        &self,
        http: &reqwest::Client,
        now: i64,
    ) -> anyhow::Result<Option<CodexAuth>> {
        let Some(auth) = self.auth() else {
            return Ok(None);
        };
        if !auth.needs_refresh(now) {
            return Ok(Some(auth));
        }
        // One refresh at a time; a turn that waited picks up the winner's
        // login instead of refreshing again.
        let _guard = self.refresh_lock.lock().await;
        let Some(auth) = self.auth() else {
            return Ok(None);
        };
        if !auth.needs_refresh(now) {
            return Ok(Some(auth));
        }
        let refreshed = auth
            .refresh(http, &self.refresh_url, now)
            .await
            .with_context(|| format!("refreshing {}", self.auth_path.display()))?;
        *self.auth.lock().unwrap_or_else(PoisonError::into_inner) = Some(refreshed.clone());
        Ok(Some(refreshed))
    }

    /// The codex client version toker currently identifies as: the one
    /// resolved at construction (config pin → the installed CLI's
    /// `version.json` → the built-in floor), upgraded in place when the
    /// background latest-release probe found something newer — max
    /// wins, so a stale local record never downgrades a probe result.
    /// The `version` handshake header and the models endpoint's
    /// `client_version` query must speak the same version, so both
    /// read it from here.
    pub fn client_version(&self) -> String {
        let mut client_version = self
            .client_version
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        // The background probe's landing: max wins, so the ecosystem's
        // latest release upgrades a stale local record, and nothing
        // ever downgrades.
        if let Some(latest) = LATEST_PROBE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            && semver_newer(latest, &client_version)
        {
            client_version.clone_from(latest);
        }
        client_version
    }

    /// The full codex request-header set for one turn — the codex
    /// client's header block ([`Provider::inject_auth`]'s auth headers
    /// plus the dialect's identity headers), built for the routing unit
    /// to apply to an outgoing request.
    ///
    /// - `prompt_cache_key` → the `session-id` header (the ChatGPT
    ///   backend derives cache affinity from it — the codex client
    ///   sends its prompt cache key there, not its session id).
    /// - `thread_id` → the `thread-id` header.
    /// - `request_id` → the `x-client-request-id` header (the codex
    ///   client sends its thread id here; toker takes it as its own
    ///   parameter so the pipeline can pass a per-request uuid).
    ///
    /// `Accept: text/event-stream` and `Content-Type: application/json`
    /// are constants of this backend's turn shape and included here;
    /// invalid header bytes in any value skip that header rather than
    /// failing the turn (the upstream's 4xx names the gap visibly).
    pub fn turn_headers(
        &self,
        auth: Option<&CodexAuth>,
        prompt_cache_key: &str,
        thread_id: &str,
        request_id: &str,
    ) -> HeaderMap {
        let mut headers = HeaderMap::new();
        auth_headers(auth, &mut headers);
        insert(&mut headers, "originator", &self.originator);
        let client_version = self.client_version();
        insert(&mut headers, "version", &client_version);
        insert(&mut headers, "session-id", prompt_cache_key);
        insert(&mut headers, "thread-id", thread_id);
        insert(&mut headers, "x-client-request-id", request_id);
        insert(
            &mut headers,
            "user-agent",
            &user_agent(&self.originator, &client_version),
        );
        insert(&mut headers, "accept", "text/event-stream");
        insert(&mut headers, "content-type", "application/json");
        headers
    }
}

/// The codex auth headers for one outgoing request — and the
/// pass-through breaker (see the module docs): any `authorization` the
/// frontend brought is **always removed**, then
///
/// - `Authorization: Bearer <access_token>` when toker holds a login,
/// - `ChatGPT-Account-ID` when the login names an account,
/// - `X-OpenAI-Fedramp: true` when the id token says FedRAMP
///   (the codex client's `BearerAuthProvider` set, same spellings).
///
/// With no login the request carries no authorization at all: the
/// frontend's dummy bearer is not forwarded, and the upstream's 401
/// body passes through as the visible wiring check.
pub fn auth_headers(auth: Option<&CodexAuth>, outgoing: &mut HeaderMap) {
    // ALWAYS: never forward a frontend credential to chatgpt.com.
    outgoing.remove(header::AUTHORIZATION);
    let Some(auth) = auth else {
        return;
    };
    if let Some(token) = auth.access_token()
        && let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}"))
    {
        outgoing.insert(header::AUTHORIZATION, value);
    }
    if let Some(account_id) = auth.account_id()
        && let Ok(value) = HeaderValue::from_str(account_id)
    {
        outgoing.insert(CHATGPT_ACCOUNT_ID, value);
    }
    if auth.is_fedramp() {
        outgoing.insert(X_OPENAI_FEDRAMP, HeaderValue::from_static("true"));
    }
}

/// The `User-Agent`, built from the originator like the codex client's
/// (`{originator}/{version} (…)`) with toker's own platform facts: the
/// CLI's shape, honestly filled in.
fn user_agent(originator: &str, client_version: &str) -> String {
    format!(
        "{originator}/{client_version} ({} {}; toker)",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Where the codex CLI's own updater looks for the latest release
/// (doctor/updates.rs in its source) — the authoritative,
/// never-stale source, independent of whether a codex CLI is installed
/// at all.
const GITHUB_LATEST_RELEASE_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";

impl CodexSub {
    /// Upgrade the client version from the ecosystem's latest release,
    /// in the background: resolve locally first (the caller serves
    /// immediately), then let the probe land whenever it lands — a slow
    /// or unreachable GitHub never delays or fails a turn, and a probe
    /// that finds nothing newer (or errors) is a no-op. Only spawned
    /// when a tokio runtime is running (the daemon, tokio tests);
    /// constructed outside one, the local resolution stands.
    fn spawn_latest_version_probe(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let current = self
            .client_version
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let originator = self.originator.clone();
        handle.spawn(async move {
            match latest_github_release(&originator, &current).await {
                Ok(latest) if semver_newer(&latest, &current) => {
                    tracing::info!(
                        "codex client version: {current} → {latest}                          (the ecosystem's latest release; picked up on the next turn)"
                    );
                    LATEST_PROBE
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .replace(latest);
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(%error, "codex latest-release probe failed (keeping the local version)");
                }
            }
        });
    }
}

/// The probe's landing slot: the spawned task cannot reach the provider
/// (it moved into the server), so the upgrade travels through here and
/// [`CodexSub::turn_headers`] consults it after its own resolution —
/// max wins, so a stale local `version.json` never downgrades a probe
/// result. (One provider per process: the daemon builds exactly one.)
static LATEST_PROBE: Mutex<Option<String>> = Mutex::new(None);

/// Fetch the ecosystem's latest codex release tag from GitHub, the same
/// request the codex CLI's updater makes (5 s budget, one request; the
/// GitHub API requires a User-Agent). The tag arrives as `rust-vX.Y.Z`.
async fn latest_github_release(originator: &str, current: &str) -> anyhow::Result<String> {
    #[derive(serde::Deserialize)]
    struct ReleaseInfo {
        tag_name: String,
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let info: ReleaseInfo = client
        .get(GITHUB_LATEST_RELEASE_URL)
        .header("user-agent", format!("{originator}/{current} (toker)"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let version = info
        .tag_name
        .strip_prefix("rust-v")
        .ok_or_else(|| anyhow::anyhow!("unexpected release tag {:?}", info.tag_name))?;
    Ok(version.to_owned())
}

/// Semver-ish "is `a` strictly newer than `b`": compare x.y.z tuples;
/// anything unparseable is never newer (a malformed tag never downgrades
/// or loops an upgrade).
fn semver_newer(a: &str, b: &str) -> bool {
    match (semver_triple(a), semver_triple(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn semver_triple(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    (parts.next().is_none()).then_some((major, minor, patch))
}

/// The installed codex CLI's own notion of the current version, from
/// the `version.json` its update check maintains beside `auth.json`.
/// The CLI keeps this fresh; toker reading it tracks the ecosystem
/// without guessing. `None` when the file is absent or unreadable.
fn installed_cli_version(auth_path: &Path) -> Option<String> {
    let version_path = auth_path.parent()?.join("version.json");
    let raw = std::fs::read_to_string(version_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("latest_version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Insert one header, skipping values whose bytes are not valid header
/// bytes (never fail a turn over an identity string).
fn insert(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (name.parse::<HeaderName>(), HeaderValue::from_str(value)) {
        headers.insert(name, value);
    }
}

/// The endpoint mapping: the frontend path (query included) is appended
/// to the base whole — `/responses` → `…/backend-api/codex/responses`
/// (anthropic's shape: no prefix to strip, the frontend paths are the
/// upstream's).
fn endpoint_of(upstream: &Url, path: &str) -> Url {
    let base = upstream.as_str().trim_end_matches('/');
    let url = format!("{base}{path}");
    // Infallible: the base parsed as a URL at config load, and the pieces
    // above are valid path/query fragments of one.
    Url::parse(&url).expect("validated upstream base makes every endpoint valid")
}

impl Provider for CodexSub {
    fn id(&self) -> &str {
        "codex_sub"
    }

    fn model_map(&self) -> Option<&crate::middleware::model_map::ModelMap> {
        self.model_map.as_ref()
    }

    fn endpoint(&self, path: &str) -> Url {
        endpoint_of(&self.upstream, path)
    }

    /// Always `false`: auth is ALWAYS toker-signed (see the module
    /// docs). This is the explicit opt-out of the server's
    /// pass-through-when-present rule — a request that carries its own
    /// `authorization` still gets [`Provider::inject_auth`], which
    /// replaces it with the codex bearer.
    fn credential_present(&self, _incoming: &HeaderMap) -> bool {
        false
    }

    /// The codex bearer + account id + FedRAMP marker, over a stripped
    /// `authorization` — [`auth_headers`].
    fn inject_auth(&self, outgoing: &mut HeaderMap) {
        auth_headers(self.auth().as_ref(), outgoing);
    }

    /// The codex sub is a meter source: its quota snapshot is the parsed
    /// `x-codex-*` headers (the plan's gate interface was left open for
    /// exactly this).
    fn meters(&self, headers: &HeaderMap) -> Option<Value> {
        parse_usage_limits(headers)
    }

    fn is_meter_source(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::Provider;
    use super::{
        CHATGPT_ACCOUNT_ID, CodexSub, DEFAULT_CLIENT_VERSION, X_OPENAI_FEDRAMP, auth_headers,
        user_agent,
    };
    use crate::providers::codex::auth::tests::{auth_file, spawn_refresh_mock};
    use axum::http::{HeaderMap, HeaderValue, header};
    use serde_json::json;

    fn provider(auth_path: &std::path::Path) -> CodexSub {
        CodexSub::new(
            "https://chatgpt.com/backend-api/codex"
                .parse()
                .expect("upstream url"),
            "codex_cli_rs".to_owned(),
            auth_path.to_owned(),
            "https://auth.openai.com/oauth/token"
                .parse()
                .expect("refresh url"),
            None,
            None,
            false,
        )
        .expect("provider builds")
    }

    #[test]
    fn frontend_paths_map_onto_the_codex_backend_root() {
        let provider = provider(std::path::Path::new("/nonexistent/auth.json"));
        assert_eq!(provider.id(), "codex_sub");
        assert_eq!(
            provider.endpoint("/responses").as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            provider.endpoint("/responses?include=raw").as_str(),
            "https://chatgpt.com/backend-api/codex/responses?include=raw"
        );
        let slashed = CodexSub::new(
            "http://localhost:9/backend-api/codex/"
                .parse()
                .expect("url"),
            "codex_cli_rs".to_owned(),
            std::path::PathBuf::from("/nonexistent/auth.json"),
            "https://auth.openai.com/oauth/token".parse().expect("url"),
            None,
            None,
            false,
        )
        .expect("provider builds");
        assert_eq!(
            slashed.endpoint("/responses").as_str(),
            "http://localhost:9/backend-api/codex/responses",
            "a trailing slash on the base never doubles up"
        );
    }

    #[test]
    fn auth_is_always_toker_signed_never_passed_through() {
        let anonymous = provider(std::path::Path::new("/nonexistent/auth.json"));
        assert!(
            !anonymous.credential_present(&HeaderMap::new()),
            "no credential of its own — still no pass-through"
        );

        // The frontend's dummy bearer must NOT count as "already
        // credentialed" — codex auth is always toker's.
        let mut incoming = HeaderMap::new();
        incoming.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer dummy-from-claude"),
        );
        assert!(
            !anonymous.credential_present(&incoming),
            "the pass-through rule is off for the codex backend"
        );

        // And inject_auth REPLACES the dummy, never forwards it.
        let mut outgoing = HeaderMap::new();
        outgoing.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer dummy-from-claude"),
        );
        anonymous.inject_auth(&mut outgoing);
        assert!(
            outgoing.is_empty(),
            "no login → the dummy bearer is stripped and nothing injected: \
             the upstream 401 passes through visibly"
        );

        let with_login = provider(&auth_file("signed", None));
        let login = with_login.auth().expect("login loaded");
        let mut outgoing = HeaderMap::new();
        outgoing.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer dummy-from-claude"),
        );
        with_login.inject_auth(&mut outgoing);
        assert_eq!(
            outgoing
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some(
                format!(
                    "Bearer {}",
                    login.access_token().expect("the login's token")
                )
                .as_str()
            ),
            "the codex bearer replaces whatever the frontend brought"
        );
    }

    #[test]
    fn turn_headers_carry_the_full_codex_block() {
        let signed = provider(&auth_file("headers", None));
        let auth = signed.auth().expect("login loaded");
        let headers = signed.turn_headers(
            Some(&auth),
            "session-cache-key",
            "thread-uuid-1",
            "request-uuid-2",
        );
        assert_eq!(
            headers.get("originator").and_then(|v| v.to_str().ok()),
            Some("codex_cli_rs")
        );
        assert_eq!(
            headers.get("version").and_then(|v| v.to_str().ok()),
            Some(DEFAULT_CLIENT_VERSION),
            "no version.json beside the fixture auth → the built-in floor"
        );
        assert_eq!(
            headers.get("session-id").and_then(|v| v.to_str().ok()),
            Some("session-cache-key"),
            "session-id carries the prompt cache key"
        );
        assert_eq!(
            headers.get("thread-id").and_then(|v| v.to_str().ok()),
            Some("thread-uuid-1")
        );
        assert_eq!(
            headers
                .get("x-client-request-id")
                .and_then(|v| v.to_str().ok()),
            Some("request-uuid-2")
        );
        assert_eq!(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {}", auth.access_token().expect("the login's token")).as_str())
        );
        assert_eq!(
            headers
                .get(CHATGPT_ACCOUNT_ID)
                .and_then(|v| v.to_str().ok()),
            Some("acct-789")
        );
        assert_eq!(
            headers.get(X_OPENAI_FEDRAMP).and_then(|v| v.to_str().ok()),
            Some("true"),
            "the id token says fedramp"
        );
        assert_eq!(
            headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let user_agent = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .expect("user agent present");
        assert!(
            user_agent.starts_with(&format!("codex_cli_rs/{DEFAULT_CLIENT_VERSION} ("))
                && user_agent.ends_with("; toker)"),
            "the user agent carries the resolved client version: {user_agent}"
        );

        // Without a login: the same block minus the auth headers.
        let anonymous = provider(std::path::Path::new("/nonexistent/auth.json"));
        let headers = anonymous.turn_headers(None, "k", "t", "r");
        assert!(headers.get(header::AUTHORIZATION).is_none());
        assert!(headers.get(CHATGPT_ACCOUNT_ID).is_none());
        assert!(headers.get(X_OPENAI_FEDRAMP).is_none());
        assert_eq!(
            headers.get("originator").and_then(|v| v.to_str().ok()),
            Some("codex_cli_rs")
        );
    }

    #[test]
    fn the_originator_is_configurable_and_flows_everywhere() {
        let provider = CodexSub::new(
            "https://chatgpt.com/backend-api/codex"
                .parse()
                .expect("url"),
            "my_tools_proxy".to_owned(),
            std::path::PathBuf::from("/nonexistent/auth.json"),
            "https://auth.openai.com/oauth/token".parse().expect("url"),
            None,
            None,
            false,
        )
        .expect("provider builds");
        let headers = provider.turn_headers(None, "k", "t", "r");
        assert_eq!(
            headers.get("originator").and_then(|v| v.to_str().ok()),
            Some("my_tools_proxy")
        );
        assert!(
            user_agent("my_tools_proxy", DEFAULT_CLIENT_VERSION).starts_with("my_tools_proxy/"),
            "the user agent follows the originator"
        );
    }

    #[test]
    fn only_meter_headers_produce_a_snapshot() {
        let provider = provider(std::path::Path::new("/nonexistent/auth.json"));
        let mut headers = HeaderMap::new();
        assert!(
            provider.meters(&headers).is_none(),
            "no x-codex-* headers → no snapshot"
        );
        headers.insert(
            "x-codex-primary-used-percent",
            HeaderValue::from_static("12.5"),
        );
        assert!(provider.meters(&headers).is_some());
    }

    #[tokio::test]
    async fn a_turn_reuses_the_cached_login_until_it_needs_refresh() {
        let now = 1_800_000_000;
        let fresh = provider(&auth_file("cache-fresh", None));
        let http = reqwest::Client::new();
        // Fresh exp → no refresh, and no server to even ask.
        let auth = fresh
            .auth_for_turn(&http, now)
            .await
            .expect("auth for turn")
            .expect("login present");
        assert_eq!(
            auth.access_token(),
            fresh.auth().expect("cached").access_token(),
            "a fresh login is handed out untouched — no refresh, no server to even ask"
        );

        // Stale exp → refresh against the mock, cached back.
        let dir = auth_file("cache-stale", None);
        let (seen, url) = spawn_refresh_mock(
            json!({"access_token": "rotated-access", "refresh_token": "rotated-refresh"}),
        )
        .await;
        let stale = CodexSub::new(
            "https://chatgpt.com/backend-api/codex"
                .parse()
                .expect("url"),
            "codex_cli_rs".to_owned(),
            dir.clone(),
            url,
            None,
            None,
            false,
        )
        .expect("provider builds");
        let stale_now = now + 400_000; // past the fixture's exp
        let auth = stale
            .auth_for_turn(&http, stale_now)
            .await
            .expect("refresh for turn")
            .expect("login present");
        assert_eq!(auth.access_token(), Some("rotated-access"));
        assert_eq!(seen.lock().unwrap().len(), 1, "exactly one refresh");
        assert_eq!(
            stale.auth().expect("cached").access_token(),
            Some("rotated-access"),
            "the refreshed login is cached back onto the provider"
        );
        // A second turn does not refresh again.
        let auth = stale
            .auth_for_turn(&http, stale_now)
            .await
            .expect("auth for turn")
            .expect("login present");
        assert_eq!(auth.access_token(), Some("rotated-access"));
        assert_eq!(seen.lock().unwrap().len(), 1, "still exactly one refresh");
    }

    /// The fixture facts the header tests lean on: the shared auth
    /// helper writes a JWT access token whose claims name account
    /// `acct-789`, plan `plus`, fedramp true — pinned here so a change
    /// to the fixture shows up as a test failure, not a silent header
    /// drift.
    #[test]
    fn the_auth_fixture_parses_the_way_the_header_tests_expect() {
        use crate::providers::codex::auth::tests::FRESH_EXP;
        let dir = auth_file("fixture-shape", None);
        let provider = provider(&dir);
        let auth = provider.auth().expect("login loaded");
        assert_eq!(
            auth.account_id(),
            Some("acct-789"),
            "no explicit tokens.account_id → the claim's account"
        );
        assert_eq!(auth.plan_type(), Some("plus"));
        assert!(auth.is_fedramp(), "the id token carries the fedramp claim");
        assert_eq!(auth.expires_at(), Some(FRESH_EXP));
    }

    #[test]
    fn auth_headers_without_a_login_still_strip_the_frontend_bearer() {
        let mut outgoing = HeaderMap::new();
        outgoing.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer anything"),
        );
        auth_headers(None, &mut outgoing);
        assert!(
            outgoing.get(header::AUTHORIZATION).is_none(),
            "the frontend's credential never rides to chatgpt.com"
        );
    }
}
