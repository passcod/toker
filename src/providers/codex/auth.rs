//! The shared `auth.json` — the codex CLI's login, read and written by
//! both tools.
//!
//! The schema (codex-rs `login` crate, `AuthDotJson`):
//! `{ "auth_mode"?, "OPENAI_API_KEY"?, "tokens"?: { "id_token",
//! "access_token", "refresh_token", "account_id"? }, "last_refresh"?,
//! … }` — and the unknown fields are the point: the codex CLI owns this
//! file (its other login modes live in it too), so toker round-trips
//! the whole object and overlays only what a refresh rotates. A `load`
//! that silently dropped a field would corrupt the CLI's login.
//!
//! The tokens are JWTs — `header.payload.signature`, the payload
//! base64url without padding. The claims toker reads: `exp`, and under
//! `https://api.openai.com/auth`: `chatgpt_account_id`,
//! `chatgpt_plan_type`, `chatgpt_account_is_fedramp`.
//!
//! Refresh (codex-rs `request_chatgpt_token_refresh`): POST the
//! configured OAuth endpoint, JSON body
//! `{"grant_type":"refresh_token","client_id":…,"refresh_token":…}` →
//! `{ "id_token"?, "access_token"?, "refresh_token"? }`, each applied
//! only when present (the refresh token rotates — that is the normal
//! case), and `last_refresh` stamped.
//!
//! When to refresh (codex-rs `should_refresh_proactively`): when the
//! access token's `exp` is within 5 minutes of now; when `exp` cannot
//! be read, when the last refresh is older than 8 days.
//!
//! ## Rotation conflicts — the known risk
//!
//! The codex CLI refreshes the same file on its own schedule, so two
//! writers share one login. toker narrows the window rather than
//! pretending to close it: the file is **re-read right before every
//! refresh**, and if the tokens moved since toker loaded them (the CLI
//! refreshed meanwhile) toker adopts the CLI's newer tokens and skips
//! the network entirely. The race that remains — the CLI refreshing
//! between toker's re-read and its persist — is a last-writer-wins
//! clobber: toker's atomic rename overwrites the CLI's newer tokens,
//! and because OAuth refresh tokens rotate, the overwritten side's
//! refresh token is already consumed, so its next refresh fails with
//! `invalid_grant` / `refresh_token_reused` — and recovers on the
//! retry, because both tools re-read before refreshing (toker adopts
//! whatever the file now holds). Persists go through a temp file +
//! rename so a crash mid-write never leaves a half-written login for
//! the CLI to trip over.
//!
//! Invariant 2: token values never appear in logs or errors — the
//! `Debug` impl below redacts by hand, and error messages carry status
//! codes and paths only.

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use axum::http::header;

/// The OAuth client id the codex CLI refreshes with (codex-rs
/// `CLIENT_ID`).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Refresh when the access token's `exp` is within this window of now:
/// 5 minutes (codex-rs `CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES`).
const REFRESH_WINDOW_SECS: i64 = 5 * 60;

/// When the access token's `exp` cannot be read, refresh when the last
/// refresh is older than this: 8 days (codex-rs
/// `TOKEN_REFRESH_INTERVAL`).
const REFRESH_MAX_AGE_SECS: i64 = 8 * 24 * 60 * 60;

/// The auth claim namespace: `exp` sits at the top level, the chatgpt
/// claims under this key.
const AUTH_CLAIMS: &str = "https://api.openai.com/auth";

/// The JWT claims toker reads off the access and id tokens.
#[derive(Debug, Clone, Default, PartialEq)]
struct Claims {
    exp: Option<i64>,
    chatgpt_account_id: Option<String>,
    chatgpt_plan_type: Option<String>,
    chatgpt_account_is_fedramp: bool,
}

/// The payload claims of one JWT, or `None` when it is not a
/// three-part, dot-split JWT with a non-empty base64url-no-pad JSON
/// payload (codex-rs `decode_jwt_payload`). No signature verification:
/// the token came from the user's own auth file — it is a credential to
/// use, not a party to authenticate.
fn jwt_claims(jwt: &str) -> Option<Claims> {
    // Exactly three non-empty parts (codex-rs `decode_jwt_payload`); the
    // header and signature are framing this parser never inspects.
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let value: Value = serde_json::from_slice(&payload).ok()?;
    let object = value.as_object()?;
    let auth = object.get(AUTH_CLAIMS).and_then(Value::as_object);
    let claim = |name: &str| {
        auth.and_then(|auth| auth.get(name))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    Some(Claims {
        exp: object.get("exp").and_then(Value::as_i64),
        chatgpt_account_id: claim("chatgpt_account_id"),
        chatgpt_plan_type: claim("chatgpt_plan_type"),
        chatgpt_account_is_fedramp: auth
            .and_then(|auth| auth.get("chatgpt_account_is_fedramp"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// The `tokens` object of auth.json, the fields toker uses.
#[derive(Clone)]
struct Tokens {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    /// The explicit `account_id` field, when present — it outranks the
    /// JWT claims (codex-rs `get_account_id`).
    account_id: Option<String>,
}

/// One loaded `auth.json` — the codex CLI's login, shared. Built from
/// the file (and from refresh results applied back onto it); the
/// unknown fields ride along unparsed.
#[derive(Clone)]
pub struct CodexAuth {
    /// Where the file lives — where refresh persists.
    path: PathBuf,
    /// The whole file object. Refresh overlays onto it and persists
    /// the rest untouched (the CLI owns this file too).
    file: Map<String, Value>,
    tokens: Option<Tokens>,
    access_claims: Option<Claims>,
    id_claims: Option<Claims>,
    /// `last_refresh` in unix seconds, RFC3339 or numeric on the wire.
    last_refresh: Option<i64>,
}

impl std::fmt::Debug for CodexAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Invariant 2: token values never appear in Debug output.
        f.debug_struct("CodexAuth")
            .field("path", &self.path)
            .field("account_id", &self.account_id())
            .field("plan_type", &self.plan_type())
            .field("fedramp", &self.is_fedramp())
            .field(
                "expires_at",
                &self.access_claims.as_ref().and_then(|claims| claims.exp),
            )
            .field("last_refresh", &self.last_refresh)
            .field("tokens", &"<redacted>")
            .finish()
    }
}

impl CodexAuth {
    /// Load `auth.json`. `Ok(None)` when the file does not exist (no
    /// login: requests go upstream unauthenticated and chatgpt.com's
    /// 401 body passes through). A present-but-unparseable file is an
    /// error — a corrupt login must not silently read as "no login".
    pub fn load(path: &Path) -> anyhow::Result<Option<CodexAuth>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing {} as auth.json", path.display()))?;
        let Some(file) = value.as_object().cloned() else {
            bail!("{} is not a JSON object", path.display());
        };
        CodexAuth::parse(path, file).map(Some)
    }

    /// Rebuild the typed state from the whole file object — shared by
    /// [`CodexAuth::load`] and the post-refresh persist, so the same
    /// parse rules hold both ways.
    fn parse(path: &Path, file: Map<String, Value>) -> anyhow::Result<CodexAuth> {
        let tokens = file
            .get("tokens")
            .and_then(Value::as_object)
            .map(|tokens| Tokens {
                access_token: tokens
                    .get("access_token")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                refresh_token: tokens
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                id_token: tokens
                    .get("id_token")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                account_id: tokens
                    .get("account_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        let (access_claims, id_claims) = match &tokens {
            Some(tokens) => (
                tokens.access_token.as_deref().and_then(jwt_claims),
                tokens.id_token.as_deref().and_then(jwt_claims),
            ),
            None => (None, None),
        };
        let last_refresh = match file.get("last_refresh") {
            // The codex CLI writes chrono `DateTime<Utc>`: RFC3339.
            Some(Value::String(text)) => text
                .parse::<jiff::Timestamp>()
                .ok()
                .map(|stamp| stamp.as_second()),
            // Older/other writers may stamp plain unix seconds.
            Some(Value::Number(number)) => number.as_i64(),
            _ => None,
        };
        Ok(CodexAuth {
            path: path.to_owned(),
            file,
            tokens,
            access_claims,
            id_claims,
            last_refresh,
        })
    }

    /// Where this login lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The access token — the bearer. Never logged (invariant 2).
    pub fn access_token(&self) -> Option<&str> {
        self.tokens
            .as_ref()
            .and_then(|tokens| tokens.access_token.as_deref())
    }

    /// The refresh token — present only on ChatGPT-logins; an API-key
    /// auth mode has none and never refreshes.
    pub fn refresh_token(&self) -> Option<&str> {
        self.tokens
            .as_ref()
            .and_then(|tokens| tokens.refresh_token.as_deref())
    }

    /// The account id, "when known" (the `ChatGPT-Account-ID` header):
    /// the auth.json `tokens.account_id` field first (codex-rs
    /// `get_account_id`), else the access token's claim, else the id
    /// token's claim.
    pub fn account_id(&self) -> Option<&str> {
        self.tokens
            .as_ref()
            .and_then(|tokens| tokens.account_id.as_deref())
            .or(self
                .access_claims
                .as_ref()
                .and_then(|claims| claims.chatgpt_account_id.as_deref()))
            .or(self
                .id_claims
                .as_ref()
                .and_then(|claims| claims.chatgpt_account_id.as_deref()))
    }

    /// The ChatGPT plan type the claims carry, "when known": the access
    /// token's claim, else the id token's.
    pub fn plan_type(&self) -> Option<&str> {
        self.access_claims
            .as_ref()
            .and_then(|claims| claims.chatgpt_plan_type.as_deref())
            .or(self
                .id_claims
                .as_ref()
                .and_then(|claims| claims.chatgpt_plan_type.as_deref()))
    }

    /// Whether the workspace routes through the FedRAMP edge: the id
    /// token says it (the wire rule — codex-rs `is_fedramp_account`);
    /// the access token's claim is the fallback when no id token
    /// parses.
    pub fn is_fedramp(&self) -> bool {
        self.id_claims
            .as_ref()
            .is_some_and(|claims| claims.chatgpt_account_is_fedramp)
            || self
                .access_claims
                .as_ref()
                .is_some_and(|claims| claims.chatgpt_account_is_fedramp)
    }

    /// The access token's `exp` in unix seconds, when readable.
    pub fn expires_at(&self) -> Option<i64> {
        self.access_claims.as_ref().and_then(|claims| claims.exp)
    }

    /// The last refresh in unix seconds, when the file stamps one.
    pub fn last_refresh(&self) -> Option<i64> {
        self.last_refresh
    }

    /// Whether the login should refresh before its next turn (codex-rs
    /// `should_refresh_proactive`, same thresholds):
    ///
    /// - `exp` readable → refresh when it is within 5 minutes of `now`
    ///   (inclusive);
    /// - `exp` unreadable or absent → refresh when the last refresh is
    ///   older than 8 days (strictly older — a login that never
    ///   refreshed carries no `last_refresh` and is trusted until the
    ///   upstream says otherwise, matching the CLI);
    /// - no refresh token → never: there is nothing to refresh with
    ///   (an API-key `auth_mode`, for instance).
    pub fn needs_refresh(&self, now: i64) -> bool {
        if self.refresh_token().is_none() {
            return false;
        }
        match self.expires_at() {
            Some(exp) => exp <= now + REFRESH_WINDOW_SECS,
            None => match self.last_refresh {
                Some(last) => last < now - REFRESH_MAX_AGE_SECS,
                None => false,
            },
        }
    }

    /// Refresh the login and persist it, returning the post-refresh
    /// state for the caller to cache (the provider does — see
    /// [`super::CodexSub::auth_for_turn`]).
    ///
    /// **Re-read-first**: the file is re-read right before anything
    /// touches the network; when the tokens moved since `self` was
    /// loaded (the codex CLI refreshed meanwhile), its newer tokens —
    /// possibly an already-rotated refresh token — are adopted as-is
    /// and no refresh is sent. Otherwise the refresh grant goes to
    /// `refresh_url` and the granted tokens are overlaid onto the
    /// re-read state and persisted atomically (see the module docs for
    /// the rotation-conflict risk that remains).
    pub async fn refresh(
        &self,
        http: &reqwest::Client,
        refresh_url: &reqwest::Url,
        now: i64,
    ) -> anyhow::Result<CodexAuth> {
        let current = match CodexAuth::load(&self.path)? {
            Some(current) => current,
            None => bail!("{} disappeared before refresh", self.path.display()),
        };
        if current.access_token() != self.access_token()
            || current.refresh_token() != self.refresh_token()
        {
            // The CLI (or another toker) refreshed meanwhile. Its tokens
            // are newer — adopt them, no network.
            return Ok(current);
        }
        let Some(refresh_token) = current.refresh_token() else {
            bail!("no refresh token in {}", self.path.display());
        };
        let response = http
            .post(refresh_url.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(
                json!({
                    "grant_type": "refresh_token",
                    "client_id": CLIENT_ID,
                    "refresh_token": refresh_token,
                })
                .to_string(),
            )
            .send()
            .await
            .with_context(|| format!("refreshing at {refresh_url}"))?;
        let status = response.status();
        if !status.is_success() {
            // The error body's `error` code only (never the raw body:
            // nothing echoes, but auth flows print nothing they don't
            // have to).
            let body = response.text().await.unwrap_or_default();
            let code = serde_json::from_str::<Value>(&body).ok().and_then(|value| {
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            match code {
                Some(code) => bail!("refresh failed: HTTP {status}: {code}"),
                None => bail!("refresh failed: HTTP {status}"),
            }
        }
        let granted: RefreshGrant = response
            .json()
            .await
            .with_context(|| format!("parsing the refresh response from {refresh_url}"))?;
        current.apply_grant(granted, now)
    }

    /// Overlay one refresh grant onto this (re-read) state and persist:
    /// each granted token replaces its field, `last_refresh` stamps as
    /// RFC3339 (what the codex CLI writes), and every field toker does
    /// not know survives untouched. Returns the state re-parsed from
    /// the persisted object.
    fn apply_grant(self, granted: RefreshGrant, now: i64) -> anyhow::Result<CodexAuth> {
        let path = self.path.clone();
        let mut file = self.file;
        let tokens = file
            .entry("tokens")
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(tokens) = tokens.as_object_mut() else {
            bail!("tokens in {} is not an object", path.display());
        };
        if let Some(id_token) = &granted.id_token {
            tokens.insert("id_token".to_owned(), json!(id_token));
        }
        if let Some(access_token) = &granted.access_token {
            tokens.insert("access_token".to_owned(), json!(access_token));
        }
        if let Some(refresh_token) = &granted.refresh_token {
            tokens.insert("refresh_token".to_owned(), json!(refresh_token));
        }
        let stamp = jiff::Timestamp::from_second(now)
            .map_err(|error| anyhow::anyhow!("last_refresh stamp {now} out of range: {error}"))?
            .to_string();
        file.insert("last_refresh".to_owned(), json!(stamp));
        let pretty = serde_json::to_string_pretty(&Value::Object(file.clone()))
            .context("serialising the refreshed auth.json")?;
        write_atomic(&path, &pretty)?;
        CodexAuth::parse(&path, file)
    }
}

/// The refresh grant (codex-rs `RefreshResponse`): each token only when
/// present — the refresh token rotates by design, and the server may
/// omit tokens it did not change.
#[derive(Deserialize)]
struct RefreshGrant {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

/// Write `text` to `path` atomically: a temp file in the same directory
/// — 0600, it holds the login — fsynced, then renamed over the target.
/// Same-directory rename is atomic, so the codex CLI never reads a
/// half-written auth.json (see the module docs).
fn write_atomic(path: &Path, text: &str) -> anyhow::Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory).with_context(|| format!("creating {}", directory.display()))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "auth.json".to_owned());
    let temp = path.with_file_name(format!("{name}.tmp-{}", std::process::id()));
    let result = (|| -> anyhow::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temp)
            .with_context(|| format!("creating {}", temp.display()))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path).with_context(|| format!("installing {}", path.display()))
    })();
    if result.is_err() {
        // Best effort: never leave the temp file behind.
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{AUTH_CLAIMS, CLIENT_ID, CodexAuth, jwt_claims};
    use axum::Router;
    use axum::extract::{Request, State};
    use axum::http::{StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::{Value, json};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// A fresh scratch directory under /tmp/opencode, unique per call
    /// (the tests/server_* pattern). NEVER the real `~/.codex` — these
    /// tests build fixture logins, never touch a real one.
    pub(crate) fn test_dir(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = PathBuf::from("/tmp/opencode")
            .join(format!("codex-auth-{name}-{}-{n}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// Encode one JWT part: base64url, no padding.
    fn b64url(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// A three-part JWT whose payload carries `claims` — the fixture
    /// tokens are unsigned and unverified (the parser never verifies).
    pub(crate) fn jwt(claims: &Value) -> String {
        format!(
            "{}.{}.{}",
            b64url(br#"{"alg":"RS256","typ":"JWT"}"#),
            b64url(claims.to_string().as_bytes()),
            b64url(b"fixture-signature")
        )
    }

    /// The default access-token `exp` for the fixture login: comfortably
    /// fresh at the tests' `now` (1.8e9).
    pub(crate) const FRESH_EXP: i64 = 1_800_100_000;

    /// Write a fixture `auth.json` and return its path.
    ///
    /// The fixture login: an access token whose claims name account
    /// `acct-789`, plan `plus`, fedramp `true`, and `exp` =
    /// `exp.unwrap_or(FRESH_EXP)`; refresh token `codex-refresh-token`;
    /// an id token carrying the fedramp claim; `last_refresh`
    /// 2026-09-26 (under 8 days old at the tests' `now`).
    pub(crate) fn auth_file(name: &str, exp: Option<i64>) -> PathBuf {
        let exp = exp.unwrap_or(FRESH_EXP);
        let claims = |fedramp: bool| {
            json!({
                "sub": "user-1",
                "exp": exp,
                AUTH_CLAIMS: {"chatgpt_account_id": "acct-789",
                              "chatgpt_plan_type": "plus",
                              "chatgpt_account_is_fedramp": fedramp},
            })
        };
        let body = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": jwt(&claims(true)),
                "access_token": jwt(&claims(false)),
                "refresh_token": "codex-refresh-token",
            },
            "last_refresh": "2026-09-26T00:00:00Z",
        });
        let path = test_dir(name).join("auth.json");
        fs::write(&path, serde_json::to_string_pretty(&body).expect("fixture"))
            .expect("write fixture auth.json");
        path
    }

    /// Rewrite `path`'s auth.json to the given tokens and last_refresh
    /// — the "the CLI refreshed meanwhile" fixture.
    pub(crate) fn rewrite_auth(path: &PathBuf, tokens: Value, last_refresh: Value) {
        let body = json!({"tokens": tokens, "last_refresh": last_refresh});
        fs::write(path, serde_json::to_string_pretty(&body).expect("fixture"))
            .expect("rewrite fixture auth.json");
    }

    // ── the refresh mock (the tests/server_* in-process axum pattern) ──

    /// An in-process OAuth endpoint at `/oauth/token`: records every
    /// request body (parsed JSON) and answers `response` with
    /// `status`.
    pub(crate) async fn spawn_refresh_mock_responding(
        status: StatusCode,
        response: Value,
    ) -> (Arc<Mutex<Vec<Value>>>, reqwest::Url) {
        #[derive(Clone)]
        struct Mock {
            seen: Arc<Mutex<Vec<Value>>>,
            status: StatusCode,
            response: String,
        }

        async fn oauth_token(State(mock): State<Mock>, request: Request) -> Response {
            let body = axum::body::to_bytes(request.into_body(), 1 << 20)
                .await
                .expect("mock reads body");
            mock.seen
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&body).expect("mock body is JSON"));
            (
                mock.status,
                [(header::CONTENT_TYPE, "application/json")],
                mock.response.clone(),
            )
                .into_response()
        }

        let mock = Mock {
            seen: Arc::new(Mutex::new(Vec::new())),
            status,
            response: response.to_string(),
        };
        let seen = mock.seen.clone();
        let app = Router::new()
            .route("/oauth/token", post(oauth_token))
            .with_state(mock);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("mock binds");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock serves");
        });
        let url: reqwest::Url = format!("http://{addr}/oauth/token")
            .parse()
            .expect("mock url");
        (seen, url)
    }

    /// The happy-path refresh mock: HTTP 200 with `response`.
    pub(crate) async fn spawn_refresh_mock(
        response: Value,
    ) -> (Arc<Mutex<Vec<Value>>>, reqwest::Url) {
        spawn_refresh_mock_responding(StatusCode::OK, response).await
    }

    // ── JWT parsing / claims ──

    #[test]
    fn jwt_payload_claims_parse_from_base64url_no_pad() {
        let claims = jwt_claims(&jwt(&json!({
            "exp": 1_800_000_300,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-42",
                "chatgpt_plan_type": "pro",
                "chatgpt_account_is_fedramp": true,
            }
        })))
        .expect("claims parse");
        assert_eq!(claims.exp, Some(1_800_000_300));
        assert_eq!(claims.chatgpt_account_id.as_deref(), Some("acct-42"));
        assert_eq!(claims.chatgpt_plan_type.as_deref(), Some("pro"));
        assert!(claims.chatgpt_account_is_fedramp);

        // exp is optional; fedramp defaults false; absent auth namespace.
        let claims = jwt_claims(&jwt(&json!({"exp": 7}))).expect("claims parse");
        assert_eq!(claims.exp, Some(7));
        assert_eq!(claims.chatgpt_account_id, None);
        assert!(!claims.chatgpt_account_is_fedramp);

        let claims = jwt_claims(&jwt(&json!({}))).expect("claims parse");
        assert_eq!(claims.exp, None);
    }

    #[test]
    fn non_jwt_shapes_do_not_parse() {
        // Two parts, four parts, empty payload/signature, bad base64,
        // non-JSON payload — all unparseable, never a panic.
        for bad in [
            "two.parts",
            "a.b.c.d",
            ".payload.sig",
            "header..sig",
            "header.!!!.sig",
            "header.aGVsbG8.sig", // "hello" is valid base64 but not JSON
        ] {
            assert!(jwt_claims(bad).is_none(), "{bad:?} must not parse");
        }
        // Padding the codex CLI never writes must not parse either —
        // URL_SAFE_NO_PAD rejects it, matching the CLI's decoder.
        let padded_json = format!("{}.e30=.sig", b64url(br#"{"alg":"RS256"}"#));
        assert!(
            jwt_claims(&padded_json).is_none(),
            "padded base64 must not parse"
        );
    }

    // ── load ──

    #[test]
    fn load_reads_the_schema_and_missing_means_none() {
        let path = auth_file("schema", None);
        let auth = CodexAuth::load(&path).expect("parse").expect("present");
        assert_eq!(auth.path(), path);
        assert!(auth.access_token().is_some());
        assert_eq!(auth.refresh_token(), Some("codex-refresh-token"));
        // No explicit tokens.account_id → the claims' account id.
        assert_eq!(auth.account_id(), Some("acct-789"));
        assert_eq!(auth.plan_type(), Some("plus"));
        assert!(auth.is_fedramp(), "the id token's fedramp claim is read");
        assert_eq!(auth.expires_at(), Some(FRESH_EXP));
        assert_eq!(auth.last_refresh(), Some(1_790_380_800)); // 2026-09-26T00:00:00Z

        // An explicit tokens.account_id outranks the claims.
        let dir = test_dir("explicit-account");
        fs::write(
            dir.join("auth.json"),
            serde_json::to_string_pretty(&json!({
                "tokens": {
                    "id_token": jwt(&json!({"https://api.openai.com/auth": {"chatgpt_account_id": "from-claims"}})),
                    "access_token": jwt(&json!({})),
                    "refresh_token": "r",
                    "account_id": "explicit-acct",
                }
            }))
            .expect("fixture"),
        )
        .expect("write");
        let auth = CodexAuth::load(&dir.join("auth.json"))
            .expect("parse")
            .expect("present");
        assert_eq!(auth.account_id(), Some("explicit-acct"));

        // Missing file: no login.
        let missing = test_dir("missing").join("auth.json");
        assert!(CodexAuth::load(&missing).expect("load").is_none());

        // Present-but-corrupt is an error, never "no login".
        let dir = test_dir("corrupt");
        fs::write(dir.join("auth.json"), "{not json").expect("write");
        assert!(CodexAuth::load(&dir.join("auth.json")).is_err());
        fs::write(dir.join("auth.json"), "[1,2]").expect("write");
        assert!(
            CodexAuth::load(&dir.join("auth.json")).is_err(),
            "a non-object file must not load"
        );

        // No tokens at all (an API-key auth mode): loads, but nothing to
        // sign or refresh with.
        let dir = test_dir("key-only");
        fs::write(
            dir.join("auth.json"),
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-x"}"#,
        )
        .expect("write");
        let auth = CodexAuth::load(&dir.join("auth.json"))
            .expect("parse")
            .expect("present");
        assert_eq!(auth.access_token(), None);
        assert_eq!(auth.refresh_token(), None);
        assert!(!auth.needs_refresh(1_800_000_000));
    }

    #[test]
    fn debug_never_carries_a_token_value() {
        let path = auth_file("debug", None);
        let auth = CodexAuth::load(&path).expect("parse").expect("present");
        let rendered = format!("{auth:?}");
        assert!(rendered.contains("<redacted>"));
        for value in ["codex-refresh-token", "fixture-signature"] {
            assert!(!rendered.contains(value), "Debug leaked {value:?}");
        }
        // The whole JWT is long and unmistakable; its b64 header bytes
        // must not appear either.
        let token = auth.access_token().expect("token");
        assert!(!rendered.contains(token));
    }

    // ── needs_refresh ──

    #[test]
    fn needs_refresh_refreshes_within_five_minutes_of_exp() {
        let now = 1_800_000_000;
        // exp 5 minutes away (inclusive) → refresh; a second further → not.
        let edge = auth_file("exp-edge", Some(now + 300));
        let auth = CodexAuth::load(&edge).expect("parse").expect("present");
        assert!(auth.needs_refresh(now));
        let far = auth_file("exp-far", Some(now + 301));
        let auth = CodexAuth::load(&far).expect("parse").expect("present");
        assert!(!auth.needs_refresh(now));
        // Already expired → refresh, of course.
        let past = auth_file("exp-past", Some(now - 1));
        let auth = CodexAuth::load(&past).expect("parse").expect("present");
        assert!(auth.needs_refresh(now));
    }

    #[test]
    fn needs_refresh_falls_back_to_last_refresh_age_when_exp_is_unreadable() {
        let now = 1_800_000_000;
        // An unparseable-exp access token (opaque string) + last_refresh.
        let dir = test_dir("no-exp");
        fs::write(
            dir.join("auth.json"),
            serde_json::to_string_pretty(&json!({
                "tokens": {"access_token": "opaque-not-a-jwt", "refresh_token": "r"},
                "last_refresh": now - 8 * 24 * 60 * 60,
            }))
            .expect("fixture"),
        )
        .expect("write");
        let auth = CodexAuth::load(&dir.join("auth.json"))
            .expect("parse")
            .expect("present");
        // Exactly 8 days old: not yet (the CLI's `<` comparison).
        assert!(!auth.needs_refresh(now));
        let dir = test_dir("no-exp-older");
        fs::write(
            dir.join("auth.json"),
            serde_json::to_string_pretty(&json!({
                "tokens": {"access_token": "opaque-not-a-jwt", "refresh_token": "r"},
                "last_refresh": now - 8 * 24 * 60 * 60 - 1,
            }))
            .expect("fixture"),
        )
        .expect("write");
        let auth = CodexAuth::load(&dir.join("auth.json"))
            .expect("parse")
            .expect("present");
        assert!(auth.needs_refresh(now));

        // No exp, no last_refresh → trust it (the CLI's rule).
        let dir = test_dir("no-exp-no-last");
        fs::write(
            dir.join("auth.json"),
            r#"{"tokens":{"access_token":"opaque","refresh_token":"r"}}"#,
        )
        .expect("write");
        let auth = CodexAuth::load(&dir.join("auth.json"))
            .expect("parse")
            .expect("present");
        assert!(!auth.needs_refresh(now));
    }

    // ── refresh ──

    #[tokio::test]
    async fn refresh_sends_the_grant_and_persists_rotated_tokens() {
        let now = 1_800_000_400;
        let path = auth_file("refresh-happy", Some(now - 1)); // expired → refresh
        let auth = CodexAuth::load(&path).expect("parse").expect("present");
        assert!(auth.needs_refresh(now));

        let new_access = jwt(&json!({"exp": now + 100_000,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-NEW",
                "chatgpt_plan_type": "pro", "chatgpt_account_is_fedramp": false}}));
        let new_id = jwt(&json!({"exp": now + 100_000,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-NEW",
                "chatgpt_plan_type": "pro", "chatgpt_account_is_fedramp": false}}));
        let (seen, url) = spawn_refresh_mock(json!({
            "access_token": new_access,
            "id_token": new_id,
            "refresh_token": "rotated-refresh-token",
            "token_type": "Bearer",
        }))
        .await;

        let refreshed = auth
            .refresh(&reqwest::Client::new(), &url, now)
            .await
            .expect("refresh");
        assert_eq!(refreshed.access_token(), Some(new_access.as_str()));
        assert_eq!(refreshed.refresh_token(), Some("rotated-refresh-token"));
        // Claims re-parse from the granted tokens — the id token flipped
        // the fedramp claim the fixture login carried.
        assert_eq!(refreshed.account_id(), Some("acct-NEW"));
        assert_eq!(refreshed.plan_type(), Some("pro"));
        assert!(!refreshed.is_fedramp());
        assert_eq!(refreshed.last_refresh(), Some(now));
        assert!(!refreshed.needs_refresh(now));

        // The request: the codex grant, JSON, exactly these three keys.
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0],
            json!({
                "grant_type": "refresh_token",
                "client_id": CLIENT_ID,
                "refresh_token": "codex-refresh-token",
            })
        );

        // The file: pretty-printed, tokens rotated, last_refresh stamped
        // in RFC3339, and the CLI's unknown fields intact.
        let text = fs::read_to_string(&path).expect("read persisted file");
        let parsed: Value = serde_json::from_str(&text).expect("persisted file parses");
        assert_eq!(parsed["tokens"]["access_token"], json!(new_access));
        assert_eq!(
            parsed["tokens"]["refresh_token"],
            json!("rotated-refresh-token")
        );
        assert_eq!(
            parsed["tokens"]["id_token"],
            json!(new_id),
            "the granted id token persisted too"
        );
        assert_eq!(
            parsed["auth_mode"],
            json!("chatgpt"),
            "unknown fields survive"
        );
        assert_eq!(
            parsed["last_refresh"],
            json!(
                jiff::Timestamp::from_second(now)
                    .expect("stamp")
                    .to_string()
            ),
            "last_refresh is stamped as RFC3339, the codex CLI's format"
        );
        assert!(text.contains("\n  "), "the file is pretty-printed");
    }

    #[tokio::test]
    async fn refresh_persists_only_the_tokens_the_server_returned() {
        let now = 1_800_000_400;
        let path = auth_file("refresh-partial", Some(now - 1));
        let auth = CodexAuth::load(&path).expect("parse").expect("present");
        // No refresh_token in the grant → the old one stays (a server
        // that did not rotate it).
        let (seen, url) = spawn_refresh_mock(json!({"access_token": "opaque-new"})).await;
        let refreshed = auth
            .refresh(&reqwest::Client::new(), &url, now)
            .await
            .expect("refresh");
        assert_eq!(refreshed.access_token(), Some("opaque-new"));
        assert_eq!(refreshed.refresh_token(), Some("codex-refresh-token"));
        let parsed: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(
            parsed["tokens"]["refresh_token"],
            json!("codex-refresh-token")
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refresh_rereads_and_adopts_when_the_cli_refreshed_meanwhile() {
        let now = 1_800_000_400;
        let path = auth_file("reread-wins", Some(now - 1));
        let auth = CodexAuth::load(&path).expect("parse").expect("present");

        // The CLI refreshes while toker holds its snapshot: the file now
        // carries newer tokens.
        rewrite_auth(
            &path,
            json!({"access_token": "cli-newer-access", "refresh_token": "cli-newer-refresh"}),
            json!("2026-10-01T00:00:00Z"),
        );

        let (seen, url) = spawn_refresh_mock(json!({"access_token": "never-used"})).await;
        let refreshed = auth
            .refresh(&reqwest::Client::new(), &url, now)
            .await
            .expect("refresh resolves without the network");
        assert_eq!(refreshed.access_token(), Some("cli-newer-access"));
        assert_eq!(refreshed.refresh_token(), Some("cli-newer-refresh"));
        assert!(
            seen.lock().unwrap().is_empty(),
            "no refresh was sent — the CLI's newer tokens win"
        );
        // And nothing was written: the file still holds the CLI's state.
        let parsed: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(parsed["tokens"]["access_token"], json!("cli-newer-access"));
        assert_eq!(
            parsed["last_refresh"],
            json!("2026-10-01T00:00:00Z"),
            "toker did not restamp over the CLI's own refresh"
        );
    }

    #[tokio::test]
    async fn refresh_failures_do_not_touch_the_file() {
        let now = 1_800_000_400;
        let path = auth_file("refresh-rejected", Some(now - 1));
        let auth = CodexAuth::load(&path).expect("parse").expect("present");
        let before = fs::read_to_string(&path).expect("read");

        let (seen, url) = spawn_refresh_mock_responding(
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid_grant", "error_description": "refresh token reused"}),
        )
        .await;
        let error = auth
            .refresh(&reqwest::Client::new(), &url, now)
            .await
            .expect_err("a rejected refresh is an error");
        let message = format!("{error:#}");
        assert!(message.contains("400"), "the status is surfaced: {message}");
        assert!(
            message.contains("invalid_grant"),
            "the code is surfaced: {message}"
        );
        // Never the token value.
        assert!(!message.contains("codex-refresh-token"));
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            before,
            "a failed refresh persists nothing"
        );
        assert_eq!(seen.lock().unwrap().len(), 1);

        // A non-JSON error body → status only, still no leak.
        let path = auth_file("refresh-nonjson", Some(now - 1));
        let auth = CodexAuth::load(&path).expect("parse").expect("present");
        let (_seen, url) = spawn_refresh_mock_responding(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!("plain text body"),
        )
        .await;
        let error = auth
            .refresh(&reqwest::Client::new(), &url, now)
            .await
            .expect_err("an error status is an error");
        assert!(format!("{error:#}").contains("500"));
    }

    #[tokio::test]
    async fn refresh_errors_when_the_login_disappeared_or_has_no_refresh_token() {
        let now = 1_800_000_400;
        // The file vanished between load and refresh (a logout, say):
        // never resurrect a deleted login.
        let path = auth_file("refresh-vanished", Some(now - 1));
        let auth = CodexAuth::load(&path).expect("parse").expect("present");
        fs::remove_file(&path).expect("remove");
        let (_seen, url) = spawn_refresh_mock(json!({"access_token": "x"})).await;
        assert!(
            auth.refresh(&reqwest::Client::new(), &url, now)
                .await
                .is_err()
        );

        // tokens without a refresh token: nothing to refresh with.
        let dir = test_dir("refresh-tokenless");
        fs::write(
            dir.join("auth.json"),
            r#"{"tokens":{"access_token":"opaque","refresh_token":null}}"#,
        )
        .expect("write");
        let auth = CodexAuth::load(&dir.join("auth.json"))
            .expect("parse")
            .expect("present");
        assert!(
            auth.refresh(&reqwest::Client::new(), &url, now)
                .await
                .is_err()
        );
    }
}
