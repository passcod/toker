//! Runtime configuration (`toker.toml`) load.
//!
//! Plan: "Setup wizard" — no config files written by hand unless wanted;
//! `toker.toml` exists for hand-editing later, patched atomically via
//! `toml_edit` (formatting-preserving) by setup. This unit is the read
//! side: resolve the file (`$XDG_CONFIG_HOME/toker/toker.toml`, fallback
//! `~/.config/toker/toker.toml`, `TOKER_CONFIG` overrides the path), layer
//! env overrides over it (`TOKER_PORT`, `TOKER_DB`, `TOKER_UPSTREAM`), and
//! expose a [`Config`] the server and CLI can use.
//!
//! spec: SEC#a-credential-is-never-logged-or-displayed (credentials): the resolved config holds the key sources
//! (env var name, whether the key is in the keyring, optional literal),
//! and `toker status` reports only which sources are set — never the key
//! values. The keyring itself is read only by the service
//! ([`crate::secrets::read_key`]), never at load.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use crate::middleware::model_map::{self, ModelMap};
use crate::middleware::notice::NoticeStyle;
use crate::store;

/// The default listener port.
pub const DEFAULT_PORT: u16 = 18_123;

/// Default session-attribution header names, read by name only (plan:
/// Attribution, headers-first; spec: SEC#a-credential-is-never-logged-or-displayed — request headers are never
/// captured wholesale). `x-claude-code-session-id` is claude's own
/// header; `x-toker-session` is what setup injects into opencode.
pub const DEFAULT_SESSION_HEADERS: &[&str] = &[
    "x-toker-session",
    "x-claude-code-session-id",
    "x-session-id",
];

/// The default ping-tagging header. The window
/// pinger injects it via `ANTHROPIC_CUSTOM_HEADERS`, read by name only
/// (spec: SEC#a-credential-is-never-logged-or-displayed).
pub const DEFAULT_PING_HEADER: &str = "x-toker-ping";

/// OpenRouter's upstream base, including the `/v1` prefix.
pub const DEFAULT_OPENROUTER_UPSTREAM: &str = "https://openrouter.ai/api/v1";

/// The env var holding the OpenRouter API key.
pub const DEFAULT_OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// OpenAI's API base, including the `/v1` prefix.
pub const DEFAULT_OPENAI_API_UPSTREAM: &str = "https://api.openai.com/v1";

/// The conventional env var holding an OpenAI API key.
pub const DEFAULT_OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";

/// The openai_chat protocol's backends, in historical-default order.
pub const OPENAI_CHAT_BACKENDS: &[&str] = &[
    "openrouter",
    "openai_api",
    "codex_sub",
    "anthropic_api",
    "anthropic_sub",
];

/// Anthropic's upstream base — the API root, no `/v1` prefix: the
/// frontend's `/v1/messages…` paths are already the upstream's paths.
pub const DEFAULT_ANTHROPIC_UPSTREAM: &str = "https://api.anthropic.com";

/// The env var holding the Anthropic API key.
pub const DEFAULT_ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
pub const DEFAULT_ANTHROPIC_SUB_TOKEN_ENV: &str = "TOKER_ANTHROPIC_SUB_TOKEN";
pub const DEFAULT_CLAUDE_CREDENTIALS_PATH: &str = "~/.claude/.credentials.json";

/// The anthropic protocol's backends. The first is the historical
/// default: a config that enables it among others and names no default
/// keeps routing there, as every config before optional backends did.
pub const ANTHROPIC_BACKENDS: &[&str] = &["anthropic_sub", "anthropic_api", "codex_sub"];

/// The codex subscription backend's upstream: the ChatGPT backend API's
/// codex root. Frontend paths append (`/responses` →
/// `https://chatgpt.com/backend-api/codex/responses`), like the
/// anthropic providers' base.
pub const DEFAULT_CODEX_UPSTREAM: &str = "https://chatgpt.com/backend-api/codex";

/// The `originator` the codex client identifies itself with (the
/// ChatGPT backend routes on it; the codex CLI's `DEFAULT_ORIGINATOR`).
pub const DEFAULT_CODEX_ORIGINATOR: &str = "codex_cli_rs";

/// The codex CLI's login file — shared: toker reads and refreshes the
/// same login the CLI owns.
pub const DEFAULT_CODEX_AUTH_PATH: &str = "~/.codex/auth.json";

/// The OAuth refresh endpoint (the codex CLI's `REFRESH_TOKEN_URL`).
pub const DEFAULT_CODEX_REFRESH_URL: &str = "https://auth.openai.com/oauth/token";

/// The machine's resolved configuration, from its store and the two
/// settings that cannot live there (spec: DCONF).
///
/// The file-only routing fields are empty unless this was parsed from a
/// file for setup's import; nothing at request time reads them.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The loopback listener port.
    pub port: u16,
    /// The SQLite ledger path.
    pub db_path: PathBuf,
    /// Session-attribution header names, in priority order.
    pub session_header_names: Vec<String>,
    /// The header the window pinger tags its requests with; a lane whose
    /// request carried it is recorded but excluded from liveness (plan:
    /// Ping tagging).
    pub ping_header_name: String,
    /// The default backend for the openai_chat protocol, always one of the
    /// enabled providers; `None` when the protocol has no backend. Read only
    /// to author revision one (`policy::legacy`) and by setup's import;
    /// nothing at request time reads it, because the active revision owns
    /// routing.
    pub default_backend_openai_chat: Option<String>,
    /// The openrouter provider block; `None` when absent, which is the
    /// disabled state: a `[providers.X]` block's presence enables X.
    pub openrouter: Option<OpenRouterConfig>,
    /// The OpenAI API provider block (`None`: disabled).
    pub openai_api: Option<OpenAiApiConfig>,
    /// The default backend for the anthropic protocol, always one of the
    /// enabled providers; `None` when no anthropic backend is enabled. Read
    /// only to author revision one (`policy::legacy`) and by setup's import;
    /// nothing at request time reads it.
    pub default_backend_anthropic: Option<String>,
    /// The anthropic subscription provider block (`None`: disabled).
    pub anthropic_sub: Option<AnthropicSubConfig>,
    /// The anthropic API provider block (`None`: disabled).
    pub anthropic_api: Option<AnthropicApiConfig>,
    /// The codex subscription provider block (`None`: disabled).
    pub codex_sub: Option<CodexSubConfig>,
    /// The middleware gates block.
    pub gates: GatesConfig,
    /// The `[notices]` table: how each frontend's gate notices render.
    pub notices: NoticesConfig,
    /// The idle-sleep lock (on by default, disabled with exactly
    /// `awake = false`): while any
    /// lane is live or any request is in flight, hold an idle-only
    /// sleep lock so desktop idle-suspend cannot kill running sessions
    /// (see [`crate::middleware::awake`]). Off → never hold, never
    /// spawn, never write awake rows.
    pub awake: bool,
    /// Extra transcript roots for the dashboard's session labels
    /// (a colon-separated list, like a PATH entry): `~/.claude` and
    /// `$CLAUDE_CONFIG_DIR` are always searched, and these add
    /// harnesses that run their agents under a config directory of
    /// their own — Workhorse does — whose transcripts this shell's
    /// environment cannot see. A leading `~/` expands at lookup time.
    /// Transcripts are only ever READ, at view time; nothing from them
    /// reaches the ledger (spec: SEC#no-prompt-or-completion-content-is-recorded).
    pub transcript_roots: Vec<PathBuf>,
}

/// The `[gates]` block, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct GatesConfig {
    /// The quota gate. Arms only on
    /// the anthropic_sub backend — today's only meter source — where it
    /// blocks a request whose quota window is spent, answering with a
    /// synthetic assistant turn; every other backend is a no-op. The
    /// marker *stripping* is the frozen marker rule's, not the toggle's:
    /// it runs on the `/v1/messages` path regardless of this setting (a
    /// toggled strip would change the cached prefix of every conversation
    /// carrying a marker).
    pub quota_enabled: bool,
    /// The cold gate: a session
    /// resumed after its prompt cache expired would re-read its whole
    /// prefix as fresh input, so the gate interrupts once per idle spell
    /// with a notice advising `/compact`. Advisory — it fires once,
    /// re-arms after another idle spell, and has no override; sending the
    /// request again IS the override. Needs only idle time + prompt size
    /// per lane, so it runs on every backend the anthropic frontend
    /// routes to, not just the meter source.
    pub cold_enabled: bool,
    /// The cold gate's quota outlook: when a notice
    /// would fire, project the 5-hour window's wall and withhold the
    /// notice when the window can absorb the re-read — recording a
    /// `cold-quiet` row so the suppression is visible, never silent.
    pub cold_outlook: bool,
    /// Below this a rebuild is too cheap for the interruption to be worth
    /// it (default [`crate::middleware::cold::
    /// DEFAULT_MIN_TOKENS`]: 175,000, chosen against the log rather than
    /// picked round).
    pub cold_min_tokens: u64,
    /// The idle floor override, in minutes and deliberately fractional:
    /// `None` follows the TTL tier the lane
    /// was last seen writing, which tracks what the client actually does
    /// rather than pinning an hour here.
    pub cold_idle_min: Option<f64>,
    /// The model a cold compaction is rewritten onto:
    /// a family name resolved against what is
    /// actually in use (`"sonnet"`, the default), an explicit model id,
    /// or `"off"` to disable the *model change* — a cold lane's cache
    /// writes are still stripped, which needs no target. Sonnet rather
    /// than Haiku: Haiku 4.5's window is 200k and the lanes this fires on
    /// routinely hold three times that.
    pub compact_model: Option<String>,
    /// The force-newest model rewrite (default
    /// ON — disabled with exactly `force_newest = false`):
    /// transparently move a request onto the newest version of its
    /// model's family that the log has proven, but only where no cache
    /// can be lost by the move — a cold lane, an unknown lane with a
    /// barely-started conversation, or a model nothing has been served on
    /// within a full cache TTL. Never downgrades, never exceeds a
    /// target's observed maxPrompt, and sticky once moved (the lane's
    /// cache lives on the new model). Anthropic usage path only, in
    /// the fixed sequencing slot: after the compaction retarget, before
    /// the served-model mark.
    pub force_newest: bool,
}

impl Default for GatesConfig {
    fn default() -> Self {
        GatesConfig {
            quota_enabled: true,
            cold_enabled: true,
            cold_outlook: true,
            cold_min_tokens: crate::middleware::cold::DEFAULT_MIN_TOKENS,
            cold_idle_min: None,
            compact_model: None,
            force_newest: true,
        }
    }
}

/// The `[notices]` table, resolved: the notice style per frontend, the frontend
/// being the name
/// in the `/f/<frontend>` prefix of the base URL it was pointed at. An
/// unprefixed request, or a frontend the table does not name, gets the
/// `default` style.
///
/// The table's keys are frontend names plus `default`; the built-in
/// entries ([`NoticesConfig::default`]) hold unless the file overrides
/// them. The old `[gates] notice_style` key still loads, as the
/// `default` it always effectively was (it applied to every client);
/// naming both is an error rather than a silent precedence.
#[derive(Debug, Clone, PartialEq)]
pub struct NoticesConfig {
    /// The style for every frontend not named below.
    pub default: NoticeStyle,
    /// Per-frontend styles, by prefix name.
    pub frontends: std::collections::BTreeMap<String, NoticeStyle>,
}

impl Default for NoticesConfig {
    /// claude renders the insight-style block, Workhorse its own
    /// `[!TOKER]` alert, everything else the GFM alert.
    fn default() -> Self {
        NoticesConfig {
            default: NoticeStyle::Gfm,
            frontends: [
                ("claude".to_owned(), NoticeStyle::Block),
                ("workhorse".to_owned(), NoticeStyle::Toker),
            ]
            .into_iter()
            .collect(),
        }
    }
}

impl NoticesConfig {
    /// The style for a request from `frontend` (`None`: unprefixed).
    pub fn style_for(&self, frontend: Option<&str>) -> NoticeStyle {
        frontend
            .and_then(|name| self.frontends.get(name))
            .copied()
            .unwrap_or(self.default)
    }
}

/// Whether `name` can be a frontend's prefix name: lowercase ASCII
/// letters, digits, `-` and `_`. The router accepts exactly these, so a
/// table key outside them could never match a request.
pub fn is_frontend_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// The openrouter provider block, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenRouterConfig {
    /// Upstream base including `/v1`, e.g. `https://openrouter.ai/api/v1`.
    pub upstream: reqwest::Url,
    /// The env var the API key is read from.
    pub api_key_env: String,
    /// The key is in the OS keyring (`api_key_keyring = true`), under
    /// [`crate::secrets::KEYRING_SERVICE`] / `openrouter`.
    pub api_key_keyring: bool,
    /// An optional literal key, used only when neither the env var nor
    /// the keyring yields one.
    pub api_key: Option<String>,
    /// The `/model` picker rules (`[[providers.openrouter.picker]]`).
    /// `None` is the built-in set ([`crate::picker::default_rules`]),
    /// `Some(empty)` offers nothing; only `toker picker sync` reads it.
    pub picker: Option<Vec<crate::picker::PickerRule>>,
}

impl OpenRouterConfig {
    /// The API key to use: the env var when set, else the keyring entry
    /// when the config says there is one (`keyring` reads it — the
    /// service passes [`crate::secrets::read_key`]), else the configured
    /// literal. `None` when none yields a key — requests then go upstream
    /// with only the frontend's own credential, or unauthenticated, and
    /// openrouter's 401 body passes through, which verifies the wiring
    /// (ledger-proxy lesson).
    pub fn api_key(&self, keyring: impl FnOnce() -> Option<String>) -> Option<String> {
        self.api_key_with_env(&ProcessEnvironment, keyring)
    }

    fn api_key_with_env(
        &self,
        environment: &impl Environment,
        keyring: impl FnOnce() -> Option<String>,
    ) -> Option<String> {
        resolve_api_key(
            environment,
            &self.api_key_env,
            self.api_key_keyring,
            keyring,
            &self.api_key,
        )
    }

    /// Which key sources are set, for status reporting (spec: SEC#a-credential-is-never-logged-or-displayed: never
    /// the values).
    pub fn key_sources(&self) -> KeySources {
        self.key_sources_with_env(&ProcessEnvironment)
    }

    fn key_sources_with_env(&self, environment: &impl Environment) -> KeySources {
        key_sources_of(
            environment,
            &self.api_key_env,
            self.api_key_keyring,
            &self.api_key,
        )
    }
}

/// The direct OpenAI API provider block, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiApiConfig {
    /// Upstream base including `/v1`.
    pub upstream: reqwest::Url,
    /// The env var the API key is read from.
    pub api_key_env: String,
    /// The key is in the OS keyring, under `openai_api`.
    pub api_key_keyring: bool,
    /// An optional literal key, used only after env and keyring sources.
    pub api_key: Option<String>,
}

impl OpenAiApiConfig {
    pub fn api_key(&self, keyring: impl FnOnce() -> Option<String>) -> Option<String> {
        resolve_api_key(
            &ProcessEnvironment,
            &self.api_key_env,
            self.api_key_keyring,
            keyring,
            &self.api_key,
        )
    }

    pub fn key_sources(&self) -> KeySources {
        key_sources_of(
            &ProcessEnvironment,
            &self.api_key_env,
            self.api_key_keyring,
            &self.api_key,
        )
    }
}

/// The anthropic subscription provider block, resolved.
#[derive(Clone, PartialEq)]
pub struct AnthropicSubConfig {
    /// Upstream base — the API root, no `/v1` prefix.
    pub upstream: reqwest::Url,
    /// The model routing map: `[providers.anthropic_sub.model_map]`, an
    /// optional operator policy. Revision one's generator compiles it into an
    /// alias policy on this backend's candidate; it is not read at request
    /// time. `None` when the table is absent (the default: no mapping).
    pub model_map: Option<ModelMap>,
    /// Toker-held OAuth token sources, preferred to Claude's local login
    /// when both are available for a foreign frontend.
    pub oauth_token_env: String,
    pub oauth_token_keyring: bool,
    pub oauth_token: Option<String>,
    /// Claude Code's local login file. `None` disables that fallback (used
    /// by isolated tests); file-loaded configs default to Claude's path.
    pub claude_credentials_path: Option<PathBuf>,
}

impl std::fmt::Debug for AnthropicSubConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnthropicSubConfig")
            .field("upstream", &self.upstream)
            .field("model_map", &self.model_map)
            .field("oauth_token_env", &self.oauth_token_env)
            .field("oauth_token_keyring", &self.oauth_token_keyring)
            .field("oauth_token_set", &self.oauth_token.is_some())
            .field("claude_credentials_path", &self.claude_credentials_path)
            .finish()
    }
}

impl AnthropicSubConfig {
    pub fn oauth_token(&self, keyring: impl FnOnce() -> Option<String>) -> Option<String> {
        resolve_api_key(
            &ProcessEnvironment,
            &self.oauth_token_env,
            self.oauth_token_keyring,
            keyring,
            &self.oauth_token,
        )
    }

    pub fn oauth_sources(&self) -> KeySources {
        key_sources_of(
            &ProcessEnvironment,
            &self.oauth_token_env,
            self.oauth_token_keyring,
            &self.oauth_token,
        )
    }
}

/// The anthropic API provider block, resolved — the same KeySources
/// pattern as openrouter.
#[derive(Debug, Clone, PartialEq)]
pub struct AnthropicApiConfig {
    /// Upstream base — the API root, no `/v1` prefix.
    pub upstream: reqwest::Url,
    /// The model routing map on this backend (see
    /// [`AnthropicSubConfig::model_map`]).
    pub model_map: Option<ModelMap>,
    /// The env var the API key is read from.
    pub api_key_env: String,
    /// The key is in the OS keyring, under `anthropic_api` (see
    /// [`OpenRouterConfig::api_key_keyring`]).
    pub api_key_keyring: bool,
    /// An optional literal key, used only when neither the env var nor
    /// the keyring yields one.
    pub api_key: Option<String>,
}

impl AnthropicApiConfig {
    /// The API key to use, resolved like [`OpenRouterConfig::api_key`].
    /// `None` when no source yields one — requests then go upstream with
    /// only the frontend's own credential, or unauthenticated, and
    /// anthropic's 401 body passes through.
    pub fn api_key(&self, keyring: impl FnOnce() -> Option<String>) -> Option<String> {
        self.api_key_with_env(&ProcessEnvironment, keyring)
    }

    fn api_key_with_env(
        &self,
        environment: &impl Environment,
        keyring: impl FnOnce() -> Option<String>,
    ) -> Option<String> {
        resolve_api_key(
            environment,
            &self.api_key_env,
            self.api_key_keyring,
            keyring,
            &self.api_key,
        )
    }

    /// Which key sources are set, for status reporting (spec: SEC#a-credential-is-never-logged-or-displayed).
    pub fn key_sources(&self) -> KeySources {
        self.key_sources_with_env(&ProcessEnvironment)
    }

    fn key_sources_with_env(&self, environment: &impl Environment) -> KeySources {
        key_sources_of(
            environment,
            &self.api_key_env,
            self.api_key_keyring,
            &self.api_key,
        )
    }
}

/// The codex subscription provider block, resolved. No key sources:
/// auth is the shared `auth.json` login (see [`crate::providers::codex`]
/// — always toker-signed, never pass-through), reported by presence
/// only (spec: SEC#a-credential-is-never-logged-or-displayed: never the values).
#[derive(Debug, Clone, PartialEq)]
pub struct CodexSubConfig {
    /// Upstream base — the codex backend root, no `/responses` suffix.
    pub upstream: reqwest::Url,
    /// The `originator` header value.
    pub originator: String,
    /// Where the shared `auth.json` lives (`~` expanded at load).
    pub auth_path: PathBuf,
    /// The OAuth refresh endpoint.
    pub refresh_url: reqwest::Url,
    /// The model routing map on this backend (see
    /// [`AnthropicSubConfig::model_map`]) — the anthropic→codex model
    /// pairings that make the translated route useful at all.
    pub model_map: Option<ModelMap>,
    /// The codex client version to identify as (`version` header). The
    /// backend gates models by this (an old version is refused with
    /// "requires a newer version of Codex"), so toker must speak a
    /// version the ecosystem recognizes — `None` (the default) resolves
    /// to the installed CLI's own `version.json` `latest_version`, then
    /// the built-in floor, and is upgraded in the background by the
    /// GitHub latest-release probe. A pinned value wins absolutely.
    pub client_version: Option<String>,
    /// Fetch the ecosystem's latest codex release at startup (the same
    /// GitHub request the codex CLI's updater makes; 5 s budget,
    /// non-fatal, never delays serving) and upgrade the version
    /// handshake to it. Default on; a pinned `client_version` disables
    /// it regardless.
    pub version_probe: bool,
}

/// The defaults of every provider block: what an empty `[providers.X]`
/// block resolves to, and what the setup wizard writes when it enables a
/// backend. The constants are known-good URLs, so the parses cannot fail.
fn default_url(url: &str) -> reqwest::Url {
    parse_upstream(url).expect("the built-in default URLs parse")
}

impl Default for OpenRouterConfig {
    fn default() -> Self {
        OpenRouterConfig {
            upstream: default_url(DEFAULT_OPENROUTER_UPSTREAM),
            api_key_env: DEFAULT_OPENROUTER_API_KEY_ENV.to_owned(),
            api_key_keyring: false,
            api_key: None,
            picker: None,
        }
    }
}

impl Default for OpenAiApiConfig {
    fn default() -> Self {
        OpenAiApiConfig {
            upstream: DEFAULT_OPENAI_API_UPSTREAM
                .parse()
                .expect("built-in OpenAI API upstream is a URL"),
            api_key_env: DEFAULT_OPENAI_API_KEY_ENV.to_owned(),
            api_key_keyring: false,
            api_key: None,
        }
    }
}

impl Default for AnthropicSubConfig {
    fn default() -> Self {
        AnthropicSubConfig {
            upstream: default_url(DEFAULT_ANTHROPIC_UPSTREAM),
            model_map: None,
            oauth_token_env: DEFAULT_ANTHROPIC_SUB_TOKEN_ENV.to_owned(),
            oauth_token_keyring: false,
            oauth_token: None,
            claude_credentials_path: Some(default_claude_credentials_path()),
        }
    }
}

impl Default for AnthropicApiConfig {
    fn default() -> Self {
        AnthropicApiConfig {
            upstream: default_url(DEFAULT_ANTHROPIC_UPSTREAM),
            model_map: None,
            api_key_env: DEFAULT_ANTHROPIC_API_KEY_ENV.to_owned(),
            api_key_keyring: false,
            api_key: None,
        }
    }
}

impl Default for CodexSubConfig {
    fn default() -> Self {
        CodexSubConfig {
            upstream: default_url(DEFAULT_CODEX_UPSTREAM),
            originator: DEFAULT_CODEX_ORIGINATOR.to_owned(),
            auth_path: expand_tilde(DEFAULT_CODEX_AUTH_PATH),
            refresh_url: default_url(DEFAULT_CODEX_REFRESH_URL),
            model_map: None,
            client_version: None,
            version_probe: true,
        }
    }
}

/// Resolve one provider's `[providers.<id>.model_map]` TOML table into the
/// parsed, validated [`ModelMap`] (the committed parser's domain — the
/// table is rendered as the JSON object it expects, so every selector
/// rule, canonical-folding, and duplicate check runs exactly once, in the
/// tested place). `None` when the table is absent: the disabled state is
/// absence, like every other optional block. A present table with bad
/// selector syntax **fails the load** — a typo'd policy must not silently
/// read as "no mapping" while the operator believes their routing is on.
fn parse_model_map_table(
    provider: &'static str,
    table: &toml::Table,
) -> anyhow::Result<Option<ModelMap>> {
    let mut object = serde_json::Map::new();
    for (selector, target) in table {
        let Some(target) = target.as_str() else {
            bail!(
                "providers.{provider}.model_map[{selector:?}]: the target must \
                 be a string, not {}",
                toml_type_of(target)
            );
        };
        object.insert(
            selector.clone(),
            serde_json::Value::String(target.to_owned()),
        );
    }
    let raw = serde_json::to_string(&object)
        .map_err(|error| anyhow::anyhow!("serialising providers.{provider}.model_map: {error}"))?;
    model_map::parse_model_map(&raw).with_context(|| format!("providers.{provider}.model_map"))
}

/// The write-side inverse of [`parse_model_map_table`]: a parsed
/// [`ModelMap`] back as the wire table — canonical selector → target,
/// exact identities first then families, deterministic per spec: WCANON#rendering.
/// The round-trip is exact on canonical selectors (the parse side
/// folds non-canonical ones); see [`Config::to_file`].
#[cfg(test)]
fn model_map_table(map: &ModelMap) -> toml::Table {
    let mut table = toml::Table::new();
    for (selector, target) in map.entries() {
        table.insert(selector.to_owned(), toml::Value::String(target.to_owned()));
    }
    table
}

/// A TOML value's kind, for the non-string-target error (serde's own
/// wording names types confusingly for config errors).
fn toml_type_of(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Float(_) => "a float",
        toml::Value::Boolean(_) => "a boolean",
        toml::Value::Datetime(_) => "a datetime",
        toml::Value::Array(_) => "an array",
        toml::Value::Table(_) => "a table",
    }
}

/// Resolve an API key: the named env var when set, else the keyring
/// entry when configured and readable, else the literal (the shared
/// KeySources resolution every api-key provider uses). The keyring is
/// consulted only when configured, so a config without it never touches
/// the secret service.
fn resolve_api_key(
    environment: &impl Environment,
    api_key_env: &str,
    keyring_configured: bool,
    keyring: impl FnOnce() -> Option<String>,
    literal: &Option<String>,
) -> Option<String> {
    if let Some(key) = environment
        .var_os(api_key_env)
        .filter(|key| !key.is_empty())
    {
        return key.into_string().ok();
    }
    if keyring_configured && let Some(key) = keyring().filter(|key| !key.is_empty()) {
        return Some(key);
    }
    literal.clone()
}

/// Which key sources are set — the whole of what status may report about
/// credentials (spec: SEC#a-credential-is-never-logged-or-displayed). The keyring is reported as configured, not
/// read: only the service reads it.
fn key_sources_of(
    environment: &impl Environment,
    api_key_env: &str,
    keyring: bool,
    literal: &Option<String>,
) -> KeySources {
    KeySources {
        env_set: environment
            .var_os(api_key_env)
            .is_some_and(|key| !key.is_empty()),
        keyring_configured: keyring,
        literal_set: literal.is_some(),
    }
}

/// Whether each key source is configured — the whole of what status may
/// report about credentials. `env_set` is this process's environment,
/// which for `toker status` is the shell's, not the service's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySources {
    pub env_set: bool,
    pub keyring_configured: bool,
    pub literal_set: bool,
}

trait Environment {
    fn var_os(&self, name: &str) -> Option<OsString>;
}

struct ProcessEnvironment;

impl Environment for ProcessEnvironment {
    fn var_os(&self, name: &str) -> Option<OsString> {
        env::var_os(name)
    }
}

impl Config {
    /// The resolved configuration a store holds (spec: DCONF#in-the-store).
    ///
    /// A nullable column is "not set" and resolves to the built-in default
    /// here, so a machine that never set one follows the default when it
    /// moves. A provider with no row, or a row saying off, is disabled.
    ///
    /// The file-only routing fields are left at their empty defaults: they
    /// are read only to author revision one, from a file setup imported
    /// (spec: RMIG#the-routing-fields). On a store-loaded config
    /// `policy::legacy` therefore sees no model maps and no named protocol
    /// defaults, and generates revision one from the enabled providers
    /// alone, which is what a fresh machine should get.
    pub fn from_store(store: &crate::store::Store) -> anyhow::Result<Config> {
        Self::from_configuration(
            &store.load_configuration()?,
            &ProcessEnvironment,
            Strictness::Degrade,
        )
    }

    /// Resolve rows that are not (yet) in the store. The control surface
    /// resolves a candidate this way before writing it, so a value it
    /// cannot use is refused with the stored one left as it was
    /// (spec: DCONF#editing).
    pub(crate) fn resolve(configuration: &crate::store::Configuration) -> anyhow::Result<Config> {
        Self::from_configuration(configuration, &ProcessEnvironment, Strictness::Refuse)
    }

    fn from_configuration(
        configuration: &crate::store::Configuration,
        environment: &impl Environment,
        strictness: Strictness,
    ) -> anyhow::Result<Config> {
        let row = |name: &str| {
            configuration
                .providers
                .iter()
                .find(|row| row.provider == name && row.enabled)
        };
        let upstream = |row: &crate::store::ProviderConfigRow, fallback: &str| {
            parse_upstream(row.upstream.as_deref().unwrap_or(fallback))
        };

        let openrouter = row("openrouter")
            .map(|row| -> anyhow::Result<OpenRouterConfig> {
                Ok(OpenRouterConfig {
                    upstream: upstream(row, DEFAULT_OPENROUTER_UPSTREAM)?,
                    api_key_env: row
                        .cred_env
                        .clone()
                        .unwrap_or_else(|| DEFAULT_OPENROUTER_API_KEY_ENV.to_owned()),
                    api_key_keyring: row.cred_keyring.unwrap_or(false),
                    api_key: row.cred_literal.clone(),
                    picker: None,
                })
            })
            .transpose();
        let openrouter = strictness.usable("openrouter", openrouter)?;
        let openai_api = row("openai_api")
            .map(|row| -> anyhow::Result<OpenAiApiConfig> {
                Ok(OpenAiApiConfig {
                    upstream: upstream(row, DEFAULT_OPENAI_API_UPSTREAM)?,
                    api_key_env: row
                        .cred_env
                        .clone()
                        .unwrap_or_else(|| DEFAULT_OPENAI_API_KEY_ENV.to_owned()),
                    api_key_keyring: row.cred_keyring.unwrap_or(false),
                    api_key: row.cred_literal.clone(),
                })
            })
            .transpose();
        let openai_api = strictness.usable("openai_api", openai_api)?;
        let anthropic_sub = row("anthropic_sub")
            .map(|row| -> anyhow::Result<AnthropicSubConfig> {
                Ok(AnthropicSubConfig {
                    upstream: upstream(row, DEFAULT_ANTHROPIC_UPSTREAM)?,
                    model_map: None,
                    oauth_token_env: row
                        .cred_env
                        .clone()
                        .unwrap_or_else(|| DEFAULT_ANTHROPIC_SUB_TOKEN_ENV.to_owned()),
                    oauth_token_keyring: row.cred_keyring.unwrap_or(false),
                    oauth_token: row.cred_literal.clone(),
                    claude_credentials_path: Some(default_claude_credentials_path()),
                })
            })
            .transpose();
        let anthropic_sub = strictness.usable("anthropic_sub", anthropic_sub)?;
        let anthropic_api = row("anthropic_api")
            .map(|row| -> anyhow::Result<AnthropicApiConfig> {
                Ok(AnthropicApiConfig {
                    upstream: upstream(row, DEFAULT_ANTHROPIC_UPSTREAM)?,
                    model_map: None,
                    api_key_env: row
                        .cred_env
                        .clone()
                        .unwrap_or_else(|| DEFAULT_ANTHROPIC_API_KEY_ENV.to_owned()),
                    api_key_keyring: row.cred_keyring.unwrap_or(false),
                    api_key: row.cred_literal.clone(),
                })
            })
            .transpose();
        let anthropic_api = strictness.usable("anthropic_api", anthropic_api)?;
        let codex_sub = row("codex_sub")
            .map(|row| -> anyhow::Result<CodexSubConfig> {
                Ok(CodexSubConfig {
                    upstream: upstream(row, DEFAULT_CODEX_UPSTREAM)?,
                    originator: DEFAULT_CODEX_ORIGINATOR.to_owned(),
                    auth_path: expand_tilde(DEFAULT_CODEX_AUTH_PATH),
                    refresh_url: parse_upstream(
                        row.refresh_url
                            .as_deref()
                            .unwrap_or(DEFAULT_CODEX_REFRESH_URL),
                    )?,
                    model_map: None,
                    client_version: row.client_version.clone(),
                    version_probe: row.version_probe.unwrap_or(true),
                })
            })
            .transpose();
        let codex_sub = strictness.usable("codex_sub", codex_sub)?;

        let settings = &configuration.settings;
        let mut notices = NoticesConfig::default();
        if let Some(style) = &settings.notice_default
            && let Some(style) =
                strictness.usable("notices", parse_notice_style(style).map(Some))?
        {
            notices.default = style;
        }
        if !configuration.notices.is_empty() {
            notices.frontends.clear();
            for (profile, style) in &configuration.notices {
                if let Some(style) =
                    strictness.usable("notices", parse_notice_style(style).map(Some))?
                {
                    notices.frontends.insert(profile.clone(), style);
                }
            }
        }

        let enabled_anthropic: Vec<&str> = [
            ("anthropic_sub", anthropic_sub.is_some()),
            ("anthropic_api", anthropic_api.is_some()),
            ("codex_sub", codex_sub.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect();
        let enabled_openai: Vec<&str> = [
            ("openrouter", openrouter.is_some()),
            ("openai_api", openai_api.is_some()),
            ("codex_sub", codex_sub.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect();

        Ok(Config {
            port: resolve_port(environment)?,
            db_path: resolve_db_path(environment)?,
            session_header_names: settings.session_headers.clone().unwrap_or_else(|| {
                DEFAULT_SESSION_HEADERS
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect()
            }),
            ping_header_name: settings
                .ping_header
                .clone()
                .unwrap_or_else(|| DEFAULT_PING_HEADER.to_owned()),
            default_backend_openai_chat: default_backend(None, &enabled_openai, Some("openrouter")),
            openrouter,
            openai_api,
            default_backend_anthropic: default_backend(
                None,
                &enabled_anthropic,
                Some("anthropic_sub"),
            ),
            anthropic_sub,
            anthropic_api,
            codex_sub,
            notices,
            gates: GatesConfig {
                quota_enabled: settings.quota_enabled.unwrap_or(true),
                cold_enabled: settings.cold_enabled.unwrap_or(true),
                cold_outlook: settings.cold_outlook.unwrap_or(true),
                cold_min_tokens: settings
                    .cold_min_tokens
                    .unwrap_or(crate::middleware::cold::DEFAULT_MIN_TOKENS),
                cold_idle_min: settings.cold_idle_min,
                compact_model: None,
                force_newest: true,
            },
            awake: settings.awake.unwrap_or(true),
            transcript_roots: settings
                .transcript_roots
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|root| expand_tilde(root))
                .collect(),
        })
    }

    /// This configuration as store rows, for setup's import
    /// (spec: RMIG#migrating-the-file-and-the-store-together).
    ///
    /// A value matching its built-in default is stored as "not set" rather
    /// than written out, so an imported machine keeps following the default
    /// as it moves, exactly as a machine that never had a file does. The
    /// routing fields are not carried: they feed revision one and die with
    /// the file (spec: RMIG#the-routing-fields).
    pub fn to_configuration(&self) -> crate::store::Configuration {
        use crate::store::{Configuration, ProviderConfigRow, SettingsConfigRow};

        /// `Some(value)` only where it differs from the default.
        fn set<T: PartialEq>(value: T, default: T) -> Option<T> {
            (value != default).then_some(value)
        }

        let mut providers = Vec::new();
        let mut push = |provider: &str, row: Option<ProviderConfigRow>| {
            providers.push(row.unwrap_or_else(|| ProviderConfigRow {
                provider: provider.to_owned(),
                enabled: false,
                ..ProviderConfigRow::default()
            }));
        };

        push(
            "openrouter",
            self.openrouter.as_ref().map(|block| ProviderConfigRow {
                provider: "openrouter".to_owned(),
                enabled: true,
                upstream: set(
                    block.upstream.as_str().to_owned(),
                    DEFAULT_OPENROUTER_UPSTREAM.to_owned(),
                ),
                cred_env: set(
                    block.api_key_env.clone(),
                    DEFAULT_OPENROUTER_API_KEY_ENV.to_owned(),
                ),
                cred_keyring: set(block.api_key_keyring, false),
                cred_literal: block.api_key.clone(),
                ..ProviderConfigRow::default()
            }),
        );
        push(
            "openai_api",
            self.openai_api.as_ref().map(|block| ProviderConfigRow {
                provider: "openai_api".to_owned(),
                enabled: true,
                upstream: set(
                    block.upstream.as_str().to_owned(),
                    DEFAULT_OPENAI_API_UPSTREAM.to_owned(),
                ),
                cred_env: set(
                    block.api_key_env.clone(),
                    DEFAULT_OPENAI_API_KEY_ENV.to_owned(),
                ),
                cred_keyring: set(block.api_key_keyring, false),
                cred_literal: block.api_key.clone(),
                ..ProviderConfigRow::default()
            }),
        );
        push(
            "anthropic_sub",
            self.anthropic_sub.as_ref().map(|block| ProviderConfigRow {
                provider: "anthropic_sub".to_owned(),
                enabled: true,
                upstream: set(
                    block.upstream.as_str().to_owned(),
                    DEFAULT_ANTHROPIC_UPSTREAM.to_owned(),
                ),
                cred_env: set(
                    block.oauth_token_env.clone(),
                    DEFAULT_ANTHROPIC_SUB_TOKEN_ENV.to_owned(),
                ),
                cred_keyring: set(block.oauth_token_keyring, false),
                cred_literal: block.oauth_token.clone(),
                ..ProviderConfigRow::default()
            }),
        );
        push(
            "anthropic_api",
            self.anthropic_api.as_ref().map(|block| ProviderConfigRow {
                provider: "anthropic_api".to_owned(),
                enabled: true,
                upstream: set(
                    block.upstream.as_str().to_owned(),
                    DEFAULT_ANTHROPIC_UPSTREAM.to_owned(),
                ),
                cred_env: set(
                    block.api_key_env.clone(),
                    DEFAULT_ANTHROPIC_API_KEY_ENV.to_owned(),
                ),
                cred_keyring: set(block.api_key_keyring, false),
                cred_literal: block.api_key.clone(),
                ..ProviderConfigRow::default()
            }),
        );
        push(
            "codex_sub",
            self.codex_sub.as_ref().map(|block| ProviderConfigRow {
                provider: "codex_sub".to_owned(),
                enabled: true,
                upstream: set(
                    block.upstream.as_str().to_owned(),
                    DEFAULT_CODEX_UPSTREAM.to_owned(),
                ),
                refresh_url: set(
                    block.refresh_url.as_str().to_owned(),
                    DEFAULT_CODEX_REFRESH_URL.to_owned(),
                ),
                client_version: block.client_version.clone(),
                version_probe: set(block.version_probe, true),
                ..ProviderConfigRow::default()
            }),
        );
        providers.sort_by(|a, b| a.provider.cmp(&b.provider));

        let defaults = GatesConfig::default();
        let built_in_notices = NoticesConfig::default();
        // All or nothing: a stored table replaces the built-in one, so a
        // file that changed one entry has to carry every entry it kept.
        let notices = if self.notices.frontends == built_in_notices.frontends {
            Default::default()
        } else {
            self.notices
                .frontends
                .iter()
                .map(|(profile, style)| (profile.clone(), style.name().to_owned()))
                .collect()
        };

        Configuration {
            providers,
            settings: SettingsConfigRow {
                session_headers: set(
                    self.session_header_names.clone(),
                    DEFAULT_SESSION_HEADERS
                        .iter()
                        .map(|name| (*name).to_owned())
                        .collect(),
                ),
                ping_header: set(
                    self.ping_header_name.clone(),
                    DEFAULT_PING_HEADER.to_owned(),
                ),
                awake: set(self.awake, true),
                transcript_roots: set(
                    self.transcript_roots
                        .iter()
                        .map(|root| root.display().to_string())
                        .collect::<Vec<_>>(),
                    Vec::new(),
                ),
                quota_enabled: set(self.gates.quota_enabled, defaults.quota_enabled),
                cold_enabled: set(self.gates.cold_enabled, defaults.cold_enabled),
                cold_outlook: set(self.gates.cold_outlook, defaults.cold_outlook),
                cold_min_tokens: set(self.gates.cold_min_tokens, defaults.cold_min_tokens),
                cold_idle_min: self.gates.cold_idle_min,
                notice_default: set(
                    self.notices.default.name().to_owned(),
                    built_in_notices.default.name().to_owned(),
                ),
            },
            notices,
        }
    }

    /// The machine's configuration, from its store (spec: DCONF#in-the-store).
    ///
    /// Opens the store at the path the environment resolves to, because the
    /// store's own path is one of the two things that cannot be read from
    /// it. Every command reaches configuration through here, and every
    /// command but setup has already refused if a configuration file is
    /// still present, so there is no second source to layer over.
    pub fn load() -> anyhow::Result<Config> {
        let environment = ProcessEnvironment;
        let db_path = resolve_db_path(&environment)?;
        let store = crate::store::Store::open(&db_path)
            .with_context(|| format!("opening the store at {}", db_path.display()))?;
        let config = Self::from_configuration(
            &store.load_configuration()?,
            &environment,
            Strictness::Degrade,
        )?;
        config.validate_retained()?;
        Ok(config)
    }

    /// Load the resolved configuration from an explicit file, defaults
    /// ← file, with **no env overrides**: the read side of setup's
    /// import, so nothing in the importing operator's environment can
    /// reach the rows the import writes. A missing file is the
    /// defaults; a present
    /// file that does not parse or validate is an error — a
    /// hand-edited typo must not be silently clobbered.
    ///
    /// Loading does not check the legacy routing fields (the protocol
    /// defaults) against the enabled backends: once the database owns
    /// routing the file need not carry them, and every command that only
    /// wants the providers and the listener must still load it. Whatever
    /// reads them as routing input calls [`Config::validate`] first.
    pub fn load_from(path: &Path) -> anyhow::Result<Config> {
        let file = match fs::read_to_string(path) {
            Ok(text) => toml::from_str::<FileConfig>(&text)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        let config = Self::resolve_file(file)?;
        config.validate_retained()?;
        Ok(config)
    }

    /// The write-side wire mapping — the inverse of
    /// [`Config::resolve_file`]: this resolved config as the
    /// `toker.toml` shape. Only the file reader's own round-trip
    /// assertions use it, nothing writes a file. Every key the reader
    /// understands is written explicitly — the wizard writes the file
    /// it fully understands, defaults included. Round-trip
    /// normalisations, deliberate and asserted there: upstream URLs in
    /// their parsed normalised form (a bare host gains its trailing
    /// slash), `~`-leading paths already expanded at load, model-map
    /// selectors canonical; comments and hand formatting are not
    /// carried.
    #[cfg(test)]
    pub(crate) fn to_file(&self) -> FileConfig {
        self.to_file_with_routing(true)
    }

    #[cfg(test)]
    fn to_file_with_routing(&self, include_routing: bool) -> FileConfig {
        FileConfig {
            port: Some(self.port),
            db_path: Some(self.db_path.to_string_lossy().into_owned()),
            session_header_names: Some(self.session_header_names.clone()),
            ping_header_name: Some(self.ping_header_name.clone()),
            default_backend_openai_chat: include_routing
                .then(|| self.default_backend_openai_chat.clone())
                .flatten(),
            default_backend_anthropic: include_routing
                .then(|| self.default_backend_anthropic.clone())
                .flatten(),
            awake: Some(self.awake),
            transcript_roots: Some(
                self.transcript_roots
                    .iter()
                    .map(|root| root.to_string_lossy().into_owned())
                    .collect(),
            ),
            // Only the enabled blocks: a block's presence is what enables
            // its provider, so writing a disabled one would enable it.
            providers: FileProviders {
                openrouter: self.openrouter.as_ref().map(|openrouter| FileOpenRouter {
                    upstream: Some(openrouter.upstream.to_string()),
                    api_key_env: Some(openrouter.api_key_env.clone()),
                    api_key_keyring: openrouter.api_key_keyring.then_some(true),
                    api_key: openrouter.api_key.clone(),
                    picker: include_routing.then(|| openrouter.picker.clone()).flatten(),
                }),
                openai_api: self.openai_api.as_ref().map(|api| FileOpenAiApi {
                    upstream: Some(api.upstream.to_string()),
                    api_key_env: Some(api.api_key_env.clone()),
                    api_key_keyring: api.api_key_keyring.then_some(true),
                    api_key: api.api_key.clone(),
                }),
                anthropic_sub: self.anthropic_sub.as_ref().map(|sub| FileAnthropicSub {
                    upstream: Some(sub.upstream.to_string()),
                    oauth_token_env: Some(sub.oauth_token_env.clone()),
                    oauth_token_keyring: sub.oauth_token_keyring.then_some(true),
                    oauth_token: sub.oauth_token.clone(),
                    claude_credentials_path: sub
                        .claude_credentials_path
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                    model_map: include_routing
                        .then(|| sub.model_map.as_ref().map(model_map_table))
                        .flatten(),
                }),
                anthropic_api: self.anthropic_api.as_ref().map(|api| FileAnthropicApi {
                    upstream: Some(api.upstream.to_string()),
                    api_key_env: Some(api.api_key_env.clone()),
                    api_key_keyring: api.api_key_keyring.then_some(true),
                    api_key: api.api_key.clone(),
                    model_map: include_routing
                        .then(|| api.model_map.as_ref().map(model_map_table))
                        .flatten(),
                }),
                codex_sub: self.codex_sub.as_ref().map(|codex| FileCodexSub {
                    upstream: Some(codex.upstream.to_string()),
                    originator: Some(codex.originator.clone()),
                    auth_path: Some(codex.auth_path.to_string_lossy().into_owned()),
                    refresh_url: Some(codex.refresh_url.to_string()),
                    client_version: codex.client_version.clone(),
                    version_probe: Some(codex.version_probe),
                    model_map: include_routing
                        .then(|| codex.model_map.as_ref().map(model_map_table))
                        .flatten(),
                }),
            },
            notices: std::iter::once(("default".to_owned(), self.notices.default))
                .chain(
                    self.notices
                        .frontends
                        .iter()
                        .map(|(name, style)| (name.clone(), *style)),
                )
                .collect(),
            gates: FileGates {
                quota_enabled: Some(self.gates.quota_enabled),
                // Written as `[notices] default` instead: the rewrite
                // moves an old file onto the new spelling.
                notice_style: None,
                cold_enabled: Some(self.gates.cold_enabled),
                cold_outlook: Some(self.gates.cold_outlook),
                cold_min_tokens: Some(self.gates.cold_min_tokens),
                cold_idle_min: self.gates.cold_idle_min,
                compact_model: include_routing
                    .then(|| self.gates.compact_model.clone())
                    .flatten(),
                force_newest: include_routing.then_some(self.gates.force_newest),
            },
        }
    }

    /// The file→resolved mapping shared by [`Config::load`] and
    /// [`Config::load_from`]: every default applied, every block
    /// parsed. No env, no validation — both are the callers' side.
    fn resolve_file(file: FileConfig) -> anyhow::Result<Config> {
        let FileProviders {
            openrouter,
            openai_api,
            anthropic_sub,
            anthropic_api,
            codex_sub,
        } = file.providers;
        let openrouter = openrouter
            .map(|block| -> anyhow::Result<OpenRouterConfig> {
                Ok(OpenRouterConfig {
                    upstream: parse_upstream(
                        block
                            .upstream
                            .as_deref()
                            .unwrap_or(DEFAULT_OPENROUTER_UPSTREAM),
                    )?,
                    api_key_env: block
                        .api_key_env
                        .unwrap_or_else(|| DEFAULT_OPENROUTER_API_KEY_ENV.to_owned()),
                    api_key_keyring: block.api_key_keyring.unwrap_or(false),
                    api_key: block.api_key,
                    picker: block
                        .picker
                        .map(|rules| {
                            for rule in &rules {
                                rule.validate().context("[providers.openrouter] picker")?;
                            }
                            anyhow::Ok(rules)
                        })
                        .transpose()?,
                })
            })
            .transpose()?;
        let openai_api = openai_api
            .map(|block| -> anyhow::Result<OpenAiApiConfig> {
                Ok(OpenAiApiConfig {
                    upstream: parse_upstream(
                        block
                            .upstream
                            .as_deref()
                            .unwrap_or(DEFAULT_OPENAI_API_UPSTREAM),
                    )?,
                    api_key_env: block
                        .api_key_env
                        .unwrap_or_else(|| DEFAULT_OPENAI_API_KEY_ENV.to_owned()),
                    api_key_keyring: block.api_key_keyring.unwrap_or(false),
                    api_key: block.api_key,
                })
            })
            .transpose()?;
        let anthropic_sub = anthropic_sub
            .map(|block| -> anyhow::Result<AnthropicSubConfig> {
                Ok(AnthropicSubConfig {
                    upstream: parse_upstream(
                        block
                            .upstream
                            .as_deref()
                            .unwrap_or(DEFAULT_ANTHROPIC_UPSTREAM),
                    )?,
                    model_map: match &block.model_map {
                        Some(table) => parse_model_map_table("anthropic_sub", table)?,
                        None => None,
                    },
                    oauth_token_env: block
                        .oauth_token_env
                        .unwrap_or_else(|| DEFAULT_ANTHROPIC_SUB_TOKEN_ENV.to_owned()),
                    oauth_token_keyring: block.oauth_token_keyring.unwrap_or(false),
                    oauth_token: block.oauth_token,
                    claude_credentials_path: Some(
                        block
                            .claude_credentials_path
                            .as_deref()
                            .map(expand_tilde)
                            .unwrap_or_else(default_claude_credentials_path),
                    ),
                })
            })
            .transpose()?;
        let anthropic_api = anthropic_api
            .map(|block| -> anyhow::Result<AnthropicApiConfig> {
                Ok(AnthropicApiConfig {
                    upstream: parse_upstream(
                        block
                            .upstream
                            .as_deref()
                            .unwrap_or(DEFAULT_ANTHROPIC_UPSTREAM),
                    )?,
                    model_map: match &block.model_map {
                        Some(table) => parse_model_map_table("anthropic_api", table)?,
                        None => None,
                    },
                    api_key_env: block
                        .api_key_env
                        .unwrap_or_else(|| DEFAULT_ANTHROPIC_API_KEY_ENV.to_owned()),
                    api_key_keyring: block.api_key_keyring.unwrap_or(false),
                    api_key: block.api_key,
                })
            })
            .transpose()?;
        let codex_sub = codex_sub
            .map(|block| -> anyhow::Result<CodexSubConfig> {
                Ok(CodexSubConfig {
                    upstream: parse_upstream(
                        block.upstream.as_deref().unwrap_or(DEFAULT_CODEX_UPSTREAM),
                    )?,
                    originator: block
                        .originator
                        .unwrap_or_else(|| DEFAULT_CODEX_ORIGINATOR.to_owned()),
                    auth_path: expand_tilde(
                        block
                            .auth_path
                            .as_deref()
                            .unwrap_or(DEFAULT_CODEX_AUTH_PATH),
                    ),
                    refresh_url: parse_upstream(
                        block
                            .refresh_url
                            .as_deref()
                            .unwrap_or(DEFAULT_CODEX_REFRESH_URL),
                    )?,
                    model_map: match &block.model_map {
                        Some(table) => parse_model_map_table("codex_sub", table)?,
                        None => None,
                    },
                    client_version: block.client_version,
                    version_probe: block.version_probe.unwrap_or(true),
                })
            })
            .transpose()?;

        let enabled_anthropic: Vec<&str> = [
            ("anthropic_sub", anthropic_sub.is_some()),
            ("anthropic_api", anthropic_api.is_some()),
            ("codex_sub", codex_sub.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect();
        let enabled_openai_legacy: Vec<&str> = [
            ("openrouter", openrouter.is_some()),
            ("openai_api", openai_api.is_some()),
            ("codex_sub", codex_sub.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect();
        let mut enabled_openai = enabled_openai_legacy.clone();
        if anthropic_api.is_some() {
            enabled_openai.push("anthropic_api");
        }
        if anthropic_sub.is_some() {
            enabled_openai.push("anthropic_sub");
        }

        let config = Config {
            port: file.port.unwrap_or(DEFAULT_PORT),
            db_path: match file.db_path {
                Some(path) => PathBuf::from(path),
                None => store::default_db_path().map_err(anyhow::Error::from)?,
            },
            session_header_names: file.session_header_names.unwrap_or_else(|| {
                DEFAULT_SESSION_HEADERS
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect()
            }),
            ping_header_name: file
                .ping_header_name
                .unwrap_or_else(|| DEFAULT_PING_HEADER.to_owned()),
            default_backend_openai_chat: default_backend(
                file.default_backend_openai_chat.clone(),
                if file.default_backend_openai_chat.is_some() {
                    &enabled_openai
                } else {
                    &enabled_openai_legacy
                },
                Some("openrouter"),
            ),
            openrouter,
            openai_api,
            default_backend_anthropic: default_backend(
                file.default_backend_anthropic,
                &enabled_anthropic,
                Some("anthropic_sub"),
            ),
            anthropic_sub,
            anthropic_api,
            codex_sub,
            notices: resolve_notices(file.notices, file.gates.notice_style)?,
            gates: GatesConfig {
                quota_enabled: file.gates.quota_enabled.unwrap_or(true),
                cold_enabled: file.gates.cold_enabled.unwrap_or(true),
                cold_outlook: file.gates.cold_outlook.unwrap_or(true),
                cold_min_tokens: file
                    .gates
                    .cold_min_tokens
                    .unwrap_or(crate::middleware::cold::DEFAULT_MIN_TOKENS),
                cold_idle_min: file.gates.cold_idle_min,
                compact_model: file.gates.compact_model,
                // On unless disabled.
                force_newest: file.gates.force_newest.unwrap_or(true),
            },
            // On unless disabled.
            awake: file.awake.unwrap_or(true),
            transcript_roots: file
                .transcript_roots
                .map(|roots| roots.into_iter().map(PathBuf::from).collect())
                .unwrap_or_default(),
        };
        Ok(config)
    }

    /// Cross-field validation the individual pieces cannot see. Also
    /// the wizard's gate: setup re-runs it over the wizard's changes
    /// before anything is written.
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        self.validate_retained()?;
        self.validate_routing()
    }

    pub(crate) fn validate_retained(&self) -> anyhow::Result<()> {
        // Session header names must be usable as header names, or the
        // per-request lookup by name would silently never match.
        for name in &self.session_header_names {
            if name.is_empty() {
                bail!("session_header_names must not contain an empty name");
            }
            if let Err(error) = axum::http::HeaderName::from_bytes(name.as_bytes()) {
                bail!("session_header_names entry {name:?} is not a valid header name: {error}");
            }
        }
        // The ping header is read by name on every gated request; an
        // unusable name would silently disable ping tagging.
        if self.ping_header_name.is_empty() {
            bail!("ping_header_name must not be empty");
        }
        if let Err(error) = axum::http::HeaderName::from_bytes(self.ping_header_name.as_bytes()) {
            bail!(
                "ping_header_name {:?} is not a valid header name: {error}",
                self.ping_header_name
            );
        }
        // The cold gate's idle floor is a duration: a negative floor would
        // fire on every request the moment a lane exists.
        if let Some(minutes) = self.gates.cold_idle_min
            && !(minutes.is_finite() && minutes >= 0.0)
        {
            bail!("cold_idle_min must be a non-negative number of minutes");
        }
        // The codex originator rides on every request header: an
        // unusable value would silently never reach the upstream.
        if let Some(codex) = &self.codex_sub
            && let Err(error) = axum::http::HeaderValue::from_str(&codex.originator)
        {
            bail!(
                "providers.codex_sub originator {:?} is not a valid header value: {error}",
                codex.originator
            );
        }
        Ok(())
    }

    fn validate_routing(&self) -> anyhow::Result<()> {
        check_default(
            "default_backend_anthropic",
            self.default_backend_anthropic.as_deref(),
            &self.enabled_anthropic(),
            ANTHROPIC_BACKENDS,
        )?;
        let chat_enabled = self.enabled_openai_chat();
        let legacy_chat_enabled: Vec<&str> = chat_enabled
            .iter()
            .copied()
            .filter(|provider| matches!(*provider, "openrouter" | "openai_api" | "codex_sub"))
            .collect();
        check_default(
            "default_backend_openai_chat",
            self.default_backend_openai_chat.as_deref(),
            if self.default_backend_openai_chat.is_some() {
                &chat_enabled
            } else {
                &legacy_chat_enabled
            },
            OPENAI_CHAT_BACKENDS,
        )
    }
}

impl Config {
    /// The enabled anthropic-protocol backends, in [`ANTHROPIC_BACKENDS`]
    /// order.
    pub fn enabled_anthropic(&self) -> Vec<&'static str> {
        [
            ("anthropic_sub", self.anthropic_sub.is_some()),
            ("anthropic_api", self.anthropic_api.is_some()),
            ("codex_sub", self.codex_sub.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect()
    }

    /// The enabled openai_chat-protocol backends.
    pub fn enabled_openai_chat(&self) -> Vec<&'static str> {
        [
            self.openrouter.is_some().then_some("openrouter"),
            self.openai_api.is_some().then_some("openai_api"),
            self.codex_sub.is_some().then_some("codex_sub"),
            self.anthropic_api.is_some().then_some("anthropic_api"),
            self.anthropic_sub.is_some().then_some("anthropic_sub"),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Every enabled backend, anthropic protocol first.
    pub fn enabled_backends(&self) -> Vec<&'static str> {
        let mut all = self.enabled_anthropic();
        for backend in self.enabled_openai_chat() {
            if !all.contains(&backend) {
                all.push(backend);
            }
        }
        all
    }
}

/// The `[notices]` table over the built-in entries, with the old
/// `[gates] notice_style` read as the `default` (see [`NoticesConfig`]).
fn resolve_notices(
    table: std::collections::BTreeMap<String, NoticeStyle>,
    old_default: Option<NoticeStyle>,
) -> anyhow::Result<NoticesConfig> {
    let mut notices = NoticesConfig::default();
    if let Some(style) = old_default {
        if table.contains_key("default") {
            bail!(
                "[gates] notice_style and [notices] default both set the default notice style: \
                 keep only [notices] default (notice_style is its old spelling)"
            );
        }
        notices.default = style;
    }
    for (name, style) in table {
        if name == "default" {
            notices.default = style;
        } else if is_frontend_name(&name) {
            notices.frontends.insert(name, style);
        } else {
            bail!(
                "[notices] {name:?} is not a frontend name: the /f/<frontend> prefix takes \
                 lowercase letters, digits, - and _"
            );
        }
    }
    Ok(notices)
}

/// A protocol's default backend: the named one when the file names it
/// (checked against the enabled set by [`check_default`]), else the only
/// enabled backend, else the historical default when it is among several
/// enabled ones. Several enabled backends and no way to choose leaves it
/// unset, and [`Config::validate`] refuses that rather than guess: bare
/// model names would otherwise route somewhere the operator never picked.
/// What to do with a stored value that cannot be used.
///
/// A load degrades and an edit refuses. Strictness sits at the edit because
/// that is where there is someone to tell: the dashboard reaches
/// configuration through the service, so a service that refused to start over
/// a row would leave no way to correct that row
/// (spec: DCONF#configuration-never-refuses-a-start).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strictness {
    /// Refuse the whole configuration, naming what could not be used.
    Refuse,
    /// Disable only what the value describes, and say so in the log.
    Degrade,
}

impl Strictness {
    fn usable<T>(
        self,
        setting: &str,
        resolved: anyhow::Result<Option<T>>,
    ) -> anyhow::Result<Option<T>> {
        match resolved {
            Ok(value) => Ok(value),
            Err(error) if self == Strictness::Refuse => {
                Err(error.context(format!("the {setting} setting")))
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    setting,
                    "stored configuration cannot be used: leaving it off"
                );
                Ok(None)
            }
        }
    }
}

/// The notice style a stored name spells. The store's own CHECK admits only
/// these, so a failure here is a row written around it.
fn parse_notice_style(name: &str) -> anyhow::Result<NoticeStyle> {
    serde_json::from_value(serde_json::Value::String(name.to_owned()))
        .with_context(|| format!("unknown notice style {name:?}"))
}

/// The listener port: a fixed default, overridden by `PORT`
/// (spec: DCONF#before-the-store).
fn resolve_port(environment: &impl Environment) -> anyhow::Result<u16> {
    match environment.var_os("PORT") {
        None => Ok(DEFAULT_PORT),
        Some(port) => port
            .into_string()
            .map_err(|_| anyhow::anyhow!("PORT is not valid UTF-8"))?
            .parse::<u16>()
            .context("PORT must be a port number"),
    }
}

/// The store's path: the user data directory's, overridden by `TOKER_DB`
/// (spec: DCONF#before-the-store).
fn resolve_db_path(environment: &impl Environment) -> anyhow::Result<PathBuf> {
    match environment.var_os("TOKER_DB") {
        Some(path) => Ok(PathBuf::from(path)),
        None => store::default_db_path().map_err(anyhow::Error::from),
    }
}

fn default_backend(
    named: Option<String>,
    enabled: &[&str],
    historical: Option<&str>,
) -> Option<String> {
    if named.is_some() {
        return named;
    }
    match enabled {
        [only] => Some((*only).to_owned()),
        several if historical.is_some_and(|name| several.contains(&name)) => {
            historical.map(str::to_owned)
        }
        _ => None,
    }
}

/// A protocol default must name one of the protocol's enabled backends.
fn check_default(
    key: &str,
    named: Option<&str>,
    enabled: &[&str],
    protocol: &[&str],
) -> anyhow::Result<()> {
    let Some(named) = named else {
        if !enabled.is_empty() {
            bail!(
                "{key} must name one of the enabled backends ({})",
                enabled.join(", ")
            );
        }
        return Ok(());
    };
    if enabled.contains(&named) {
        return Ok(());
    }
    if protocol.contains(&named) {
        bail!(
            "{key} = {named:?}, but [providers.{named}] is not in the config: \
             a backend is enabled by its block's presence"
        );
    }
    bail!(
        "{key} must name one of {}, not {named:?}",
        protocol.join(", ")
    )
}

/// The `toker.toml` wire shape, serde-side. `deny_unknown_fields` so a
/// typo'd key is an error, not a silent default — the strictness the
/// setup wizard's rewrite leans on (see [`Config::to_file`]): a key
/// this toker does not understand refuses the load instead of being
/// silently dropped by a rewrite. `Serialize` is the write side, with
/// every absent `Option` simply omitted. Field order matters there:
/// scalars before the `providers`/`gates` tables, because a TOML
/// table must be the last thing emitted in its own.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_header_names: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ping_header_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_backend_openai_chat: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_backend_anthropic: Option<String>,
    /// The idle-sleep lock toggle.
    #[serde(skip_serializing_if = "Option::is_none")]
    awake: Option<bool>,
    /// Extra transcript roots for the TUI's session labels, before `~`
    /// expansion (see [`Config::transcript_roots`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    transcript_roots: Option<Vec<String>>,
    providers: FileProviders,
    gates: FileGates,
    /// Frontend name (or `default`) → notice style; see [`NoticesConfig`].
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    notices: std::collections::BTreeMap<String, NoticeStyle>,
}

/// The `[gates]` block of `toker.toml`, serde-side.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileGates {
    #[serde(skip_serializing_if = "Option::is_none")]
    quota_enabled: Option<bool>,
    /// The old spelling of `[notices] default`, still read (see
    /// [`NoticesConfig`]) and never written. Parsed by [`NoticeStyle`]'s
    /// case-insensitive deserialiser; an unknown value fails the load,
    /// like a typo'd key would.
    #[serde(skip_serializing_if = "Option::is_none")]
    notice_style: Option<NoticeStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cold_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cold_outlook: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cold_min_tokens: Option<u64>,
    /// Minutes, fractional (a smoke test needs a floor it can wait
    /// out).
    #[serde(skip_serializing_if = "Option::is_none")]
    cold_idle_min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compact_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    force_newest: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileProviders {
    #[serde(skip_serializing_if = "Option::is_none")]
    openrouter: Option<FileOpenRouter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    openai_api: Option<FileOpenAiApi>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anthropic_sub: Option<FileAnthropicSub>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anthropic_api: Option<FileAnthropicApi>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_sub: Option<FileCodexSub>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileOpenAiApi {
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_keyring: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileOpenRouter {
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_env: Option<String>,
    /// The key is in the OS keyring; written only when true.
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_keyring: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    /// The picker rules, an array of tables, so last: TOML ends a block
    /// at its first table.
    #[serde(skip_serializing_if = "Option::is_none")]
    picker: Option<Vec<crate::picker::PickerRule>>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileAnthropicSub {
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth_token_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth_token_keyring: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_credentials_path: Option<String>,
    /// The model routing map: selector keys → target model ids, parsed by
    /// [`parse_model_map_table`] into the committed [`ModelMap`]. Written
    /// back by [`model_map_table`]; the table goes last because a TOML
    /// table must end its own block.
    #[serde(skip_serializing_if = "Option::is_none")]
    model_map: Option<toml::Table>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileAnthropicApi {
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_env: Option<String>,
    /// The key is in the OS keyring; written only when true.
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_keyring: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    /// Written last (see [`FileAnthropicSub::model_map`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    model_map: Option<toml::Table>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileCodexSub {
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    originator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_url: Option<String>,
    /// The codex client version to identify as (see
    /// [`CodexSubConfig::client_version`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    client_version: Option<String>,
    /// See [`CodexSubConfig::version_probe`].
    #[serde(skip_serializing_if = "Option::is_none")]
    version_probe: Option<bool>,
    /// Written last (see [`FileAnthropicSub::model_map`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    model_map: Option<toml::Table>,
}

/// `$TOKER_CONFIG`, else `$XDG_CONFIG_HOME/toker/toker.toml`, else
/// `~/.config/toker/toker.toml`.
/// The exit status a refusal over the configuration file leaves. Distinct so
/// the service unit can name it and the supervisor stop retrying: nothing
/// can change until the operator runs setup (spec: DLIFE#starting).
pub const EXIT_CONFIG_FILE_PRESENT: i32 = 78;

/// The configuration file, if a machine still has one.
///
/// Looked for where the environment names it and otherwise in the user
/// configuration directory (spec: DCONF#moving-a-machine-off-the-file). A
/// config home that cannot be worked out is not a file: there is nothing to
/// refuse over, and setup will say so in its own words.
pub fn config_file_present() -> Option<PathBuf> {
    config_file_present_with_env(&ProcessEnvironment)
}

fn config_file_present_with_env(environment: &impl Environment) -> Option<PathBuf> {
    config_path_with_env(environment)
        .ok()
        .filter(|path| path.is_file())
}

/// Refuse to run while a configuration file exists, naming it and saying
/// what moves the machine off it.
///
/// Every command except setup goes through here (spec: UCLI#before-anything-runs).
/// Refusing is what makes the move safe to interrupt: nothing can read the
/// store's configuration while the file is there, so no row can have been
/// edited between setup's import and the file's removal.
pub fn refuse_while_config_file_exists() -> anyhow::Result<()> {
    let Some(path) = config_file_present() else {
        return Ok(());
    };
    eprintln!(
        "toker: {} is still here, and configuration now lives in the store.\n\
         Run `toker setup` to import it; every other command waits until then.",
        path.display()
    );
    std::process::exit(EXIT_CONFIG_FILE_PRESENT);
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    config_path_with_env(&ProcessEnvironment)
}

fn config_path_with_env(environment: &impl Environment) -> anyhow::Result<PathBuf> {
    if let Some(path) = environment.var_os("TOKER_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let config_home = match environment
        .var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
    {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = environment
                .var_os("HOME")
                .filter(|home| !home.is_empty())
                .context("no config home: set XDG_CONFIG_HOME or HOME")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(config_home.join("toker").join("toker.toml"))
}

/// Parse and sanity-check an upstream base URL.
fn parse_upstream(upstream: &str) -> anyhow::Result<reqwest::Url> {
    let url =
        reqwest::Url::parse(upstream).with_context(|| format!("parsing upstream {upstream:?}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("upstream {upstream:?} must be http or https");
    }
    Ok(url)
}

/// Expand a leading `~/` against `$HOME` — the codex auth path's
/// default is the CLI's `~/.codex/auth.json`. A literal path, or `~`
/// without a home to expand against, passes through unchanged.
fn expand_tilde(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix("~/") else {
        return PathBuf::from(path);
    };
    match env::var_os("HOME").filter(|home| !home.is_empty()) {
        Some(home) => PathBuf::from(home).join(rest),
        None => PathBuf::from(path),
    }
}

fn default_claude_credentials_path() -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| expand_tilde("~/.claude"))
        .join(".credentials.json")
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_ANTHROPIC_API_KEY_ENV, DEFAULT_ANTHROPIC_UPSTREAM, DEFAULT_CODEX_AUTH_PATH,
        DEFAULT_CODEX_ORIGINATOR, DEFAULT_CODEX_REFRESH_URL, DEFAULT_CODEX_UPSTREAM,
        DEFAULT_OPENAI_API_KEY_ENV, DEFAULT_OPENAI_API_UPSTREAM, DEFAULT_OPENROUTER_UPSTREAM,
        DEFAULT_PORT, DEFAULT_SESSION_HEADERS, KeySources, OpenRouterConfig,
    };
    use crate::middleware::notice::NoticeStyle;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;

    #[derive(Default)]
    struct TestEnvironment(HashMap<String, OsString>);

    impl TestEnvironment {
        fn with(mut self, name: &str, value: impl Into<OsString>) -> Self {
            self.0.insert(name.to_owned(), value.into());
            self
        }
    }

    impl super::Environment for TestEnvironment {
        fn var_os(&self, name: &str) -> Option<OsString> {
            self.0.get(name).cloned()
        }
    }

    fn configured(
        rows: Vec<crate::store::ProviderConfigRow>,
        settings: crate::store::SettingsConfigRow,
    ) -> crate::store::Configuration {
        crate::store::Configuration {
            providers: rows,
            settings,
            notices: Default::default(),
        }
    }

    fn enabled(name: &str) -> crate::store::ProviderConfigRow {
        crate::store::ProviderConfigRow {
            provider: name.to_owned(),
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_configuration_file_is_found_where_the_environment_names_it() {
        let dir = crate::test_support::tempdir("gate-named-");
        let path = dir.join("elsewhere.toml");
        let environment = TestEnvironment::default().with("TOKER_CONFIG", &path);

        assert!(
            super::config_file_present_with_env(&environment).is_none(),
            "nothing to refuse over before the file exists"
        );
        std::fs::write(&path, b"port = 1\n").expect("write");
        assert_eq!(
            super::config_file_present_with_env(&environment),
            Some(path)
        );
    }

    #[test]
    fn a_configuration_file_is_otherwise_found_in_the_config_home() {
        let dir = crate::test_support::tempdir("gate-home-");
        let environment = TestEnvironment::default().with("XDG_CONFIG_HOME", &*dir);
        assert!(super::config_file_present_with_env(&environment).is_none());

        let path = dir.join("toker").join("toker.toml");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, b"port = 1\n").expect("write");
        assert_eq!(
            super::config_file_present_with_env(&environment),
            Some(path)
        );
    }

    #[test]
    fn a_directory_at_the_config_path_is_not_a_file_to_refuse_over() {
        let dir = crate::test_support::tempdir("gate-dir-");
        let path = dir.join("toker.toml");
        std::fs::create_dir_all(&path).expect("mkdir");
        let environment = TestEnvironment::default().with("TOKER_CONFIG", &path);
        assert!(super::config_file_present_with_env(&environment).is_none());
    }

    /// What a file resolves to must survive becoming rows and being read
    /// back: that round trip is the whole of setup's import.
    fn round_trip(toml: &str) -> (super::Config, super::Config) {
        let dir = test_dir("round-trip");
        let path = dir.join("toker.toml");
        fs::write(&path, toml).expect("write config");
        let from_file = super::Config::load_from(&path).expect("the file loads");

        let environment = TestEnvironment::default().with("TOKER_DB", "/tmp/toker-test.db");
        let from_rows = super::Config::from_configuration(
            &from_file.to_configuration(),
            &environment,
            super::Strictness::Refuse,
        )
        .expect("the rows resolve");
        (from_file, from_rows)
    }

    #[test]
    fn an_imported_file_resolves_back_to_what_it_configured() {
        let (file, rows) = round_trip(
            r#"
ping_header_name = "x-mine"
awake = false

[providers.openrouter]
upstream = "http://gateway.invalid/v1"
api_key_env = "MY_OPENROUTER_KEY"
api_key_keyring = true

[providers.codex_sub]
version_probe = false
client_version = "9.9.9"

[gates]
quota_enabled = false
cold_min_tokens = 42000
cold_idle_min = 7.5
"#,
        );

        assert_eq!(rows.ping_header_name, file.ping_header_name);
        assert_eq!(rows.awake, file.awake);
        assert_eq!(rows.gates.quota_enabled, file.gates.quota_enabled);
        assert_eq!(rows.gates.cold_min_tokens, file.gates.cold_min_tokens);
        assert_eq!(rows.gates.cold_idle_min, file.gates.cold_idle_min);
        assert_eq!(rows.gates.cold_enabled, file.gates.cold_enabled);

        let (file_or, rows_or) = (
            file.openrouter.as_ref().expect("on"),
            rows.openrouter.as_ref().expect("on"),
        );
        assert_eq!(rows_or.upstream, file_or.upstream);
        assert_eq!(rows_or.api_key_env, file_or.api_key_env);
        assert_eq!(rows_or.api_key_keyring, file_or.api_key_keyring);

        let (file_cx, rows_cx) = (
            file.codex_sub.as_ref().expect("on"),
            rows.codex_sub.as_ref().expect("on"),
        );
        assert_eq!(rows_cx.version_probe, file_cx.version_probe);
        assert_eq!(rows_cx.client_version, file_cx.client_version);

        // Not configured in the file, and so not enabled by the import.
        assert!(rows.anthropic_sub.is_none());
        assert!(rows.openai_api.is_none());
    }

    #[test]
    fn an_imported_default_is_stored_as_unset_rather_than_written_out() {
        let dir = test_dir("import-defaults");
        let path = dir.join("toker.toml");
        // Spelling the built-in value out loud must not pin it: the row
        // records that nothing was chosen, so a default that moves moves.
        fs::write(
            &path,
            format!("[providers.openrouter]\nupstream = \"{DEFAULT_OPENROUTER_UPSTREAM}\"\n"),
        )
        .expect("write");
        let configuration = super::Config::load_from(&path)
            .expect("loads")
            .to_configuration();

        let row = configuration
            .providers
            .iter()
            .find(|row| row.provider == "openrouter")
            .expect("a row");
        assert!(row.enabled);
        assert_eq!(row.upstream, None, "the default is not written out");
        assert_eq!(row.cred_env, None);
    }

    #[test]
    fn every_known_provider_gets_a_row_even_when_the_file_named_none() {
        let dir = test_dir("import-empty");
        let path = dir.join("toker.toml");
        fs::write(&path, "port = 19999\n").expect("write");
        let configuration = super::Config::load_from(&path)
            .expect("loads")
            .to_configuration();

        let names: Vec<&str> = configuration
            .providers
            .iter()
            .map(|row| row.provider.as_str())
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
        assert!(configuration.providers.iter().all(|row| !row.enabled));
    }

    #[test]
    fn an_imported_notice_table_carries_every_entry_it_kept() {
        let (file, rows) =
            round_trip("[notices]\ndefault = \"plain\"\nclaude = \"block\"\nmine = \"toker\"\n");
        assert_eq!(rows.notices.default, file.notices.default);
        assert_eq!(
            rows.notices.style_for(Some("claude")),
            file.notices.style_for(Some("claude"))
        );
        assert_eq!(
            rows.notices.style_for(Some("mine")),
            file.notices.style_for(Some("mine"))
        );
    }

    #[test]
    fn an_untouched_notice_table_stores_no_rows_at_all() {
        let dir = test_dir("import-notices-default");
        let path = dir.join("toker.toml");
        fs::write(&path, "port = 19999\n").expect("write");
        let configuration = super::Config::load_from(&path)
            .expect("loads")
            .to_configuration();
        assert!(configuration.notices.is_empty());
        assert_eq!(configuration.settings.notice_default, None);
    }

    #[test]
    fn an_empty_store_is_every_default_with_nothing_enabled() {
        let environment = TestEnvironment::default().with("TOKER_DB", "/tmp/toker-test.db");
        let config = super::Config::from_configuration(
            &Default::default(),
            &environment,
            super::Strictness::Refuse,
        )
        .expect("resolve an empty configuration");

        assert_eq!(config.port, DEFAULT_PORT);
        assert!(config.openrouter.is_none());
        assert!(config.anthropic_sub.is_none());
        assert!(config.codex_sub.is_none());
        assert_eq!(
            config.gates.cold_min_tokens,
            crate::middleware::cold::DEFAULT_MIN_TOKENS
        );
        assert!(config.gates.quota_enabled);
        assert!(config.awake);
        assert_eq!(config.notices, super::NoticesConfig::default());
    }

    #[test]
    fn a_disabled_row_leaves_the_provider_off() {
        let environment = TestEnvironment::default().with("TOKER_DB", "/tmp/toker-test.db");
        let off = crate::store::ProviderConfigRow {
            enabled: false,
            upstream: Some("https://example.invalid/v1".to_owned()),
            ..enabled("openrouter")
        };
        let config = super::Config::from_configuration(
            &configured(vec![off], Default::default()),
            &environment,
            super::Strictness::Refuse,
        )
        .expect("resolve");
        assert!(config.openrouter.is_none(), "a row saying off is off");
    }

    #[test]
    fn a_provider_row_resolves_its_endpoint_and_credential_source() {
        let environment = TestEnvironment::default().with("TOKER_DB", "/tmp/toker-test.db");
        let row = crate::store::ProviderConfigRow {
            upstream: Some("https://gateway.invalid/v1".to_owned()),
            cred_env: Some("MY_KEY".to_owned()),
            cred_keyring: Some(true),
            cred_literal: Some("sk-literal".to_owned()),
            ..enabled("openrouter")
        };
        let config = super::Config::from_configuration(
            &configured(vec![row], Default::default()),
            &environment,
            super::Strictness::Refuse,
        )
        .expect("resolve");

        let openrouter = config.openrouter.expect("enabled");
        assert_eq!(openrouter.upstream.as_str(), "https://gateway.invalid/v1");
        assert_eq!(openrouter.api_key_env, "MY_KEY");
        assert!(openrouter.api_key_keyring);
        assert_eq!(openrouter.api_key.as_deref(), Some("sk-literal"));
    }

    #[test]
    fn an_unset_column_takes_the_built_in_default_rather_than_a_frozen_one() {
        let environment = TestEnvironment::default().with("TOKER_DB", "/tmp/toker-test.db");
        let config = super::Config::from_configuration(
            &configured(vec![enabled("openrouter")], Default::default()),
            &environment,
            super::Strictness::Refuse,
        )
        .expect("resolve");

        let openrouter = config.openrouter.expect("enabled");
        assert_eq!(openrouter.upstream.as_str(), DEFAULT_OPENROUTER_UPSTREAM);
        assert_eq!(
            openrouter.api_key_env,
            super::DEFAULT_OPENROUTER_API_KEY_ENV
        );
        assert!(!openrouter.api_key_keyring);
    }

    #[test]
    fn the_port_is_the_default_until_port_names_another() {
        let base = TestEnvironment::default().with("TOKER_DB", "/tmp/toker-test.db");
        assert_eq!(
            super::Config::from_configuration(
                &Default::default(),
                &base,
                super::Strictness::Refuse
            )
            .expect("resolve")
            .port,
            DEFAULT_PORT
        );

        let overridden = TestEnvironment::default()
            .with("TOKER_DB", "/tmp/toker-test.db")
            .with("PORT", "19999");
        assert_eq!(
            super::Config::from_configuration(
                &Default::default(),
                &overridden,
                super::Strictness::Refuse,
            )
            .expect("resolve")
            .port,
            19_999
        );
    }

    #[test]
    fn a_port_that_is_not_a_port_is_an_error_rather_than_the_default() {
        let environment = TestEnvironment::default()
            .with("TOKER_DB", "/tmp/toker-test.db")
            .with("PORT", "http");
        let error = super::Config::from_configuration(
            &Default::default(),
            &environment,
            super::Strictness::Refuse,
        )
        .expect_err("a mistyped PORT must not silently read as the default");
        assert!(format!("{error:#}").contains("PORT"), "{error:#}");
    }

    #[test]
    fn toker_db_names_the_store() {
        let environment = TestEnvironment::default().with("TOKER_DB", "/tmp/elsewhere/toker.db");
        let config = super::Config::from_configuration(
            &Default::default(),
            &environment,
            super::Strictness::Refuse,
        )
        .expect("resolve");
        assert_eq!(config.db_path, PathBuf::from("/tmp/elsewhere/toker.db"));
    }

    #[test]
    fn stored_notice_styles_replace_the_built_in_table() {
        let environment = TestEnvironment::default().with("TOKER_DB", "/tmp/toker-test.db");
        let configuration = crate::store::Configuration {
            settings: crate::store::SettingsConfigRow {
                notice_default: Some("plain".to_owned()),
                ..Default::default()
            },
            notices: [("opencode".to_owned(), "toker".to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let config = super::Config::from_configuration(
            &configuration,
            &environment,
            super::Strictness::Refuse,
        )
        .expect("resolve");

        assert_eq!(config.notices.default, NoticeStyle::Plain);
        assert_eq!(
            config.notices.style_for(Some("opencode")),
            NoticeStyle::Toker
        );
        // The built-in claude entry is not merged under a stored table: the
        // rows are the whole of what the operator set.
        assert_eq!(config.notices.style_for(Some("claude")), NoticeStyle::Plain);
    }

    #[test]
    fn a_stored_value_that_cannot_be_used_disables_only_what_it_describes() {
        let environment = TestEnvironment::default().with("TOKER_DB", "/tmp/toker-test.db");
        let broken = crate::store::ProviderConfigRow {
            upstream: Some("not a url at all".to_owned()),
            ..enabled("openrouter")
        };
        let configuration = configured(vec![broken, enabled("anthropic_sub")], Default::default());

        // Loading degrades: the dashboard reaches configuration through the
        // service, so refusing to start would leave no way to fix the row.
        let config = super::Config::from_configuration(
            &configuration,
            &environment,
            super::Strictness::Degrade,
        )
        .expect("a bad row must not stop the service starting");
        assert!(config.openrouter.is_none(), "only the bad provider is off");
        assert!(config.anthropic_sub.is_some(), "its neighbour is untouched");

        // Editing refuses, because that is where there is someone to tell.
        let error = super::Config::from_configuration(
            &configuration,
            &environment,
            super::Strictness::Refuse,
        )
        .expect_err("an edit naming it must be refused");
        assert!(format!("{error:#}").contains("openrouter"), "{error:#}");
    }

    #[test]
    fn a_stored_notice_style_that_cannot_be_used_falls_back_rather_than_stopping_the_start() {
        let environment = TestEnvironment::default().with("TOKER_DB", "/tmp/toker-test.db");
        let configuration = crate::store::Configuration {
            settings: crate::store::SettingsConfigRow {
                notice_default: Some("sparkle".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = super::Config::from_configuration(
            &configuration,
            &environment,
            super::Strictness::Degrade,
        )
        .expect("loads");
        assert_eq!(
            config.notices.default,
            super::NoticesConfig::default().default
        );
    }

    #[test]
    fn the_routing_fields_are_empty_on_a_store_loaded_config() {
        let environment = TestEnvironment::default().with("TOKER_DB", "/tmp/toker-test.db");
        let config = super::Config::from_configuration(
            &configured(
                vec![enabled("anthropic_sub"), enabled("codex_sub")],
                Default::default(),
            ),
            &environment,
            super::Strictness::Refuse,
        )
        .expect("resolve");

        // Read only to author revision one, from a file setup imported
        // (spec: RMIG#the-routing-fields).
        assert!(
            config
                .anthropic_sub
                .as_ref()
                .expect("on")
                .model_map
                .is_none()
        );
        assert!(config.codex_sub.as_ref().expect("on").model_map.is_none());
        assert!(config.gates.compact_model.is_none());
        // A protocol default still falls out of which providers are on, so
        // a fresh machine's revision one routes somewhere.
        assert_eq!(
            config.default_backend_anthropic.as_deref(),
            Some("anthropic_sub")
        );
    }

    /// A fresh scratch directory under /tmp/opencode, unique per call.
    fn test_dir(name: &str) -> crate::test_support::TestDir {
        crate::test_support::tempdir(&format!("config-{name}-"))
    }

    /// Parse the scratch file. Nothing layers over it: the file is only
    /// ever read to import a machine off it.
    fn load_result(dir: &std::path::Path) -> anyhow::Result<super::Config> {
        super::Config::load_from(&dir.join("toker.toml"))
    }

    fn load_from(dir: &std::path::Path) -> super::Config {
        load_result(dir).expect("load config")
    }

    #[test]
    fn defaults_when_no_file_exists() {
        let config = load_from(&test_dir("defaults"));
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(
            config.session_header_names,
            DEFAULT_SESSION_HEADERS
                .iter()
                .map(|s| s.to_owned())
                .collect::<Vec<_>>()
        );
        assert!(
            DEFAULT_SESSION_HEADERS.contains(&"x-claude-code-session-id"),
            "claude identifies itself by its own header"
        );
        // No block, no backend: a fresh config enables nothing, and
        // neither protocol has a default to route to.
        assert_eq!(config.openrouter, None);
        assert_eq!(config.openai_api, None);
        assert_eq!(config.anthropic_sub, None);
        assert_eq!(config.anthropic_api, None);
        assert_eq!(config.codex_sub, None);
        assert_eq!(config.default_backend_openai_chat, None);
        assert_eq!(config.default_backend_anthropic, None);
        assert!(config.enabled_backends().is_empty());
        // The quota gate is on by default: a proxy that silently stopped
        // gating is a proxy that quietly spends overage.
        assert!(config.gates.quota_enabled);
        // Notices render per frontend: the block for claude, Workhorse's
        // alert for Workhorse, the generic GFM alert for everything else.
        // A change here would change the bytes of every notice overnight.
        assert_eq!(config.notices, super::NoticesConfig::default());
        assert_eq!(config.notices.style_for(None), NoticeStyle::Gfm);
        assert_eq!(config.notices.style_for(Some("claude")), NoticeStyle::Block);
        assert_eq!(
            config.notices.style_for(Some("workhorse")),
            NoticeStyle::Toker
        );
        assert_eq!(config.notices.style_for(Some("opencode")), NoticeStyle::Gfm);
        // The cold gate's defaults are the measured ones: on, outlook on,
        // the 175k
        // bar chosen against the log, the lane's own TTL tier as the idle
        // floor, and the sonnet family for the compaction retarget.
        assert!(config.gates.cold_enabled);
        assert!(config.gates.cold_outlook);
        assert_eq!(config.gates.cold_min_tokens, 175_000);
        assert_eq!(config.gates.cold_idle_min, None);
        assert_eq!(config.gates.compact_model, None);
        // The force-newest rewrite defaults ON
        // ("on unless disabled"): a proxy that silently stopped
        // upgrading is a proxy pinned below every newer model.
        assert!(config.gates.force_newest);
        // The idle-sleep lock defaults ON:
        // a proxy that silently stopped holding the machine awake is a
        // proxy whose sessions die to idle-suspend.
        assert!(config.awake);
        // No extra transcript roots by default: the labels look under
        // ~/.claude and $CLAUDE_CONFIG_DIR, and only a configured
        // harness adds more.
        assert!(config.transcript_roots.is_empty());
        // The db defaults to the store's default path.
        assert_eq!(
            config.db_path,
            crate::store::default_db_path().expect("default db path")
        );
    }

    #[test]
    fn an_empty_block_enables_its_backend_with_the_defaults() {
        let dir = test_dir("empty-blocks");
        fs::write(
            dir.join("toker.toml"),
            "[providers.openrouter]\n[providers.openai_api]\n[providers.anthropic_sub]\n\
             [providers.anthropic_api]\n[providers.codex_sub]\n",
        )
        .expect("write config");
        let config = load_from(&dir);
        assert_eq!(
            config.enabled_backends(),
            vec![
                "anthropic_sub",
                "anthropic_api",
                "codex_sub",
                "openrouter",
                "openai_api"
            ]
        );
        // Several anthropic backends and no named default: the historical
        // one, so a config written before backends were optional routes
        // where it always did.
        assert_eq!(
            config.default_backend_anthropic.as_deref(),
            Some("anthropic_sub")
        );
        // The historical OpenAI Chat default stays OpenRouter even though
        // cross-protocol Anthropic routes are now available.
        assert_eq!(
            config.default_backend_openai_chat.as_deref(),
            Some("openrouter")
        );

        // The codex defaults: the ChatGPT backend API's codex root, the
        // codex CLI's own originator and login/refresh endpoints, the
        // auth path tilde-expanded against $HOME.
        let codex = config.codex_sub.as_ref().expect("codex enabled");
        assert_eq!(codex.upstream.as_str(), DEFAULT_CODEX_UPSTREAM);
        assert_eq!(codex.originator, DEFAULT_CODEX_ORIGINATOR);
        assert_eq!(
            codex.auth_path,
            super::expand_tilde(DEFAULT_CODEX_AUTH_PATH),
            "the default auth path is the codex CLI's, tilde-expanded"
        );
        assert_eq!(codex.refresh_url.as_str(), DEFAULT_CODEX_REFRESH_URL);
        // No model map is configured anywhere by default: absence is the
        // disabled state, the same as every other optional block.
        let sub = config.anthropic_sub.as_ref().expect("sub enabled");
        let api = config.anthropic_api.as_ref().expect("api enabled");
        assert_eq!(sub.model_map, None);
        assert_eq!(api.model_map, None);
        assert_eq!(codex.model_map, None);
        // The default constant is a bare host; the resolved Url carries
        // its normalised trailing slash.
        let default_upstream =
            reqwest::Url::parse(DEFAULT_ANTHROPIC_UPSTREAM).expect("default upstream");
        assert_eq!(sub.upstream, default_upstream);
        assert_eq!(api.upstream, default_upstream);
        assert_eq!(api.api_key_env, DEFAULT_ANTHROPIC_API_KEY_ENV);
        assert_eq!(api.api_key, None);
        let openrouter = config.openrouter.as_ref().expect("openrouter enabled");
        assert_eq!(openrouter.upstream.as_str(), DEFAULT_OPENROUTER_UPSTREAM);
        assert_eq!(openrouter.api_key_env, "OPENROUTER_API_KEY");
        assert_eq!(openrouter.api_key, None);
        let openai = config.openai_api.as_ref().expect("openai api enabled");
        assert_eq!(openai.upstream.as_str(), DEFAULT_OPENAI_API_UPSTREAM);
        assert_eq!(openai.api_key_env, DEFAULT_OPENAI_API_KEY_ENV);
        assert_eq!(openai.api_key, None);

        // The Default impls the wizard enables a backend with are exactly
        // what an empty block resolves to.
        assert_eq!(*codex, super::CodexSubConfig::default());
        assert_eq!(*sub, super::AnthropicSubConfig::default());
        assert_eq!(*api, super::AnthropicApiConfig::default());
        assert_eq!(*openrouter, super::OpenRouterConfig::default());
        assert_eq!(*openai, super::OpenAiApiConfig::default());
    }

    #[test]
    fn anthropic_chat_binding_can_be_explicit_default_without_changing_legacy_inference() {
        let dir = test_dir("anthropic-chat-default");
        fs::write(
            dir.join("toker.toml"),
            "default_backend_openai_chat = \"anthropic_api\"\n[providers.anthropic_api]\n",
        )
        .expect("write config");
        let explicit = load_from(&dir);
        assert_eq!(
            explicit.default_backend_openai_chat.as_deref(),
            Some("anthropic_api")
        );
        fs::write(dir.join("toker.toml"), "[providers.anthropic_api]\n").expect("rewrite config");
        let legacy = load_from(&dir);
        assert_eq!(legacy.default_backend_openai_chat, None);
    }

    #[test]
    fn a_default_must_name_an_enabled_backend() {
        let dir = test_dir("default-enabled");
        let load = |text: &str| {
            fs::write(dir.join("toker.toml"), text).expect("write config");
            load_result(&dir).and_then(|config| config.validate().map(|()| config))
        };

        // A known backend whose block is absent: the error says the block
        // is what enables it.
        let error =
            load("default_backend_anthropic = \"anthropic_api\"\n[providers.anthropic_sub]\n")
                .expect_err("a default naming a disabled backend must fail");
        let message = format!("{error:#}");
        assert!(
            message.contains("default_backend_anthropic")
                && message.contains("[providers.anthropic_api]"),
            "{message}"
        );
        let error = load("default_backend_openai_chat = \"openrouter\"\n")
            .expect_err("no openrouter block, no openrouter default");
        assert!(format!("{error:#}").contains("default_backend_openai_chat"));
        // A backend of the other protocol is not a default for this one.
        let error = load("default_backend_anthropic = \"openrouter\"\n[providers.openrouter]\n")
            .expect_err("openrouter is not an anthropic backend");
        assert!(format!("{error:#}").contains("default_backend_anthropic"));

        // The only enabled backend is the inferred default.
        let config = load("[providers.codex_sub]\n").expect("one backend loads");
        assert_eq!(
            config.default_backend_anthropic.as_deref(),
            Some("codex_sub")
        );
        assert_eq!(
            config.default_backend_openai_chat.as_deref(),
            Some("codex_sub")
        );
        // Two enabled, neither the historical default, none named: an
        // error rather than a guess.
        let error = load("[providers.anthropic_api]\n[providers.codex_sub]\n")
            .expect_err("an ambiguous default must fail");
        assert!(format!("{error:#}").contains("must name one"));
        let config = load(
            "default_backend_anthropic = \"codex_sub\"\n\
             [providers.anthropic_api]\n[providers.codex_sub]\n",
        )
        .expect("a named default resolves the ambiguity");
        assert_eq!(
            config.default_backend_anthropic.as_deref(),
            Some("codex_sub")
        );
    }

    #[test]
    fn the_predecessor_shaped_config_keeps_its_behaviour() {
        // The shape of the config this machine ran on before backends
        // were optional: only the subscription block, the default named.
        let dir = test_dir("predecessor-shaped");
        fs::write(
            dir.join("toker.toml"),
            r#"
port = 18123
default_backend_anthropic = "anthropic_sub"
awake = true

[providers.anthropic_sub]
upstream = "https://api.anthropic.com/"

[gates]
quota_enabled = true
notice_style = "gfm"
"#,
        )
        .expect("write config");
        let config = load_from(&dir);
        assert_eq!(config.enabled_backends(), vec!["anthropic_sub"]);
        assert_eq!(
            config.default_backend_anthropic.as_deref(),
            Some("anthropic_sub")
        );
        assert_eq!(config.default_backend_openai_chat, None);
    }

    #[test]
    fn file_fields_are_read() {
        let dir = test_dir("file");
        fs::write(
            dir.join("toker.toml"),
            r#"
port = 19999
db_path = "scratch/toker.db"
session_header_names = ["x-toker-session"]
default_backend_openai_chat = "openrouter"

[providers.openrouter]
upstream = "http://localhost:9/v1"
api_key_env = "TOKER_TEST_KEY_FILE"
api_key = "literal-key"
"#,
        )
        .expect("write config");

        let config = load_from(&dir);
        assert_eq!(config.port, 19999);
        assert_eq!(config.db_path, PathBuf::from("scratch/toker.db"));
        assert_eq!(
            config.session_header_names,
            vec!["x-toker-session".to_owned()]
        );
        assert_eq!(
            config
                .openrouter
                .as_ref()
                .expect("enabled")
                .upstream
                .as_str(),
            "http://localhost:9/v1"
        );
        assert_eq!(
            config.openrouter.as_ref().expect("enabled").api_key_env,
            "TOKER_TEST_KEY_FILE"
        );
        assert_eq!(
            config
                .openrouter
                .as_ref()
                .expect("enabled")
                .api_key
                .as_deref(),
            Some("literal-key")
        );
    }

    #[test]
    fn anthropic_blocks_are_read_and_a_config_without_them_parses() {
        let dir = test_dir("anthropic");
        fs::write(
            dir.join("toker.toml"),
            r#"
default_backend_anthropic = "anthropic_api"

[providers.anthropic_sub]
upstream = "http://localhost:9"

[providers.anthropic_api]
upstream = "http://localhost:10"
api_key_env = "TOKER_TEST_ANTHROPIC_KEY_FILE"
api_key = "ak-literal-test"
"#,
        )
        .expect("write config");

        let config = load_result(&dir).expect("load config");
        assert_eq!(
            config.default_backend_anthropic.as_deref(),
            Some("anthropic_api")
        );
        assert_eq!(
            config
                .anthropic_sub
                .as_ref()
                .expect("enabled")
                .upstream
                .as_str(),
            "http://localhost:9/"
        );
        assert_eq!(
            config
                .anthropic_api
                .as_ref()
                .expect("enabled")
                .upstream
                .as_str(),
            "http://localhost:10/"
        );
        assert_eq!(
            config.anthropic_api.as_ref().expect("enabled").api_key_env,
            "TOKER_TEST_ANTHROPIC_KEY_FILE"
        );
        assert_eq!(
            config
                .anthropic_api
                .as_ref()
                .expect("enabled")
                .api_key
                .as_deref(),
            Some("ak-literal-test")
        );

        // The shared KeySources resolution, mirroring openrouter's.
        let api = config.anthropic_api.as_ref().expect("enabled");
        let environment =
            TestEnvironment::default().with("TOKER_TEST_ANTHROPIC_KEY_FILE", "ak-from-env");
        assert_eq!(
            api.api_key_with_env(&environment, || None).as_deref(),
            Some("ak-from-env")
        );
        assert_eq!(
            api.key_sources_with_env(&environment),
            KeySources {
                env_set: true,
                keyring_configured: false,
                literal_set: true
            }
        );

        // A config with no anthropic keys at all (a phase-1 toker.toml)
        // still parses, and enables no anthropic backend.
        fs::write(dir.join("toker.toml"), "port = 19999\n").expect("rewrite config");
        let config = load_result(&dir).expect("phase-1 config parses");
        assert_eq!(config.default_backend_anthropic, None);
        assert_eq!(config.anthropic_sub, None);
    }

    #[test]
    fn the_codex_block_is_read_and_a_config_without_it_parses() {
        let dir = test_dir("codex");
        fs::write(
            dir.join("toker.toml"),
            r#"
[providers.codex_sub]
upstream = "http://localhost:9/backend-api/codex"
originator = "my_tools_proxy"
auth_path = "/tmp/opencode/some-login.json"
refresh_url = "http://localhost:10/oauth/token"
"#,
        )
        .expect("write config");
        let config = load_from(&dir);
        assert_eq!(
            config
                .codex_sub
                .as_ref()
                .expect("enabled")
                .upstream
                .as_str(),
            "http://localhost:9/backend-api/codex"
        );
        assert_eq!(
            config.codex_sub.as_ref().expect("enabled").originator,
            "my_tools_proxy"
        );
        assert_eq!(
            config.codex_sub.as_ref().expect("enabled").auth_path,
            PathBuf::from("/tmp/opencode/some-login.json")
        );
        assert_eq!(
            config
                .codex_sub
                .as_ref()
                .expect("enabled")
                .refresh_url
                .as_str(),
            "http://localhost:10/oauth/token"
        );

        // An auth path relative to home: `~` expands at load.
        fs::write(
            dir.join("toker.toml"),
            r#"
[providers.codex_sub]
auth_path = "~/.codex/auth.json"
"#,
        )
        .expect("rewrite config");
        let config = load_result(&dir).expect("load config");
        assert_eq!(
            config.codex_sub.as_ref().expect("enabled").auth_path,
            super::expand_tilde("~/.codex/auth.json")
        );

        // A phase-1 toker.toml (no codex block at all) parses with codex
        // disabled, and a typo'd key is an error, not a silent default.
        fs::write(dir.join("toker.toml"), "port = 19999\n").expect("rewrite config");
        let config = load_result(&dir).expect("phase-1 config parses");
        assert_eq!(config.codex_sub, None);

        fs::write(
            dir.join("toker.toml"),
            "[providers.codex_sub]\nupstream = \"http://x\"\noriginater = \"typo\"\n",
        )
        .expect("rewrite config");
        assert!(
            load_result(&dir).is_err(),
            "a typo'd codex key must fail to load"
        );

        // An originator that cannot ride a header (a newline is never
        // legal in one) fails at load, not as a silently missing header
        // per request.
        fs::write(
            dir.join("toker.toml"),
            "[providers.codex_sub]\noriginator = \"codex\\ncli\"\n",
        )
        .expect("rewrite config");
        let error = load_result(&dir).expect_err("invalid originator must fail");
        assert!(format!("{error:#}").contains("originator"));
    }

    #[test]
    fn the_model_map_blocks_parse_per_provider() {
        // The `[providers.<id>.model_map]` table on each wired backend:
        // selector keys → opaque target ids, validated by the committed
        // parser (canonical folding, duplicate selectors, family rules).
        let dir = test_dir("model-map");
        fs::write(
            dir.join("toker.toml"),
            r#"
[providers.codex_sub.model_map]
"family:opus" = "gpt-5.6-sol"
"family:haiku" = "gpt-5.6-luna"
"model:claude-opus-4-5" = "special-opus"

[providers.anthropic_sub.model_map]
"family:opus" = "claude-opus-4-8"
"#,
        )
        .expect("write config");
        let config = load_from(&dir);

        // Parsed into the committed policy: exact identity ahead of
        // family, published snapshots folding to their identities.
        let codex_map = config
            .codex_sub
            .as_ref()
            .expect("enabled")
            .model_map
            .as_ref()
            .expect("codex map");
        assert_eq!(
            codex_map.entries().collect::<Vec<_>>(),
            vec![
                ("model:claude-opus-4-5", "special-opus"),
                ("family:haiku", "gpt-5.6-luna"),
                ("family:opus", "gpt-5.6-sol"),
            ]
        );
        let sub_map = config
            .anthropic_sub
            .as_ref()
            .expect("enabled")
            .model_map
            .as_ref()
            .expect("sub map");
        assert_eq!(
            sub_map.entries().collect::<Vec<_>>(),
            vec![("family:opus", "claude-opus-4-8")]
        );
        // A backend with no block carries no policy, and one backend's
        // map never answers for another's.
        assert_eq!(config.anthropic_api, None);

        // A present table with bad selector syntax fails the load naming
        // its provider block — a typo'd policy must not silently read as
        // "no mapping" while the operator believes their routing is on.
        let cases = [
            (
                "[providers.codex_sub.model_map]\n\"route:opus\" = \"x\"\n",
                "unknown selector type",
            ),
            (
                "[providers.anthropic_sub.model_map]\n\"family:opus-4\" = \"x\"\n",
                "must name a family",
            ),
            (
                "[providers.anthropic_api.model_map]\n\"family:opus\" = 4\n",
                "must be a string",
            ),
            (
                "[providers.codex_sub.model_map]\n\"family:opus\" = \"a\"\n\
                 \"family:OPUS\" = \"b\"\n",
                "duplicate canonical selector",
            ),
        ];
        for (text, message) in cases {
            fs::write(dir.join("toker.toml"), text).expect("rewrite config");
            let error = load_result(&dir).expect_err("bad policy must fail");
            let chain = format!("{error:#}");
            assert!(
                chain.contains("model_map") && chain.contains(message),
                "{text:?}: expected {message:?} in {chain}"
            );
        }

        // An empty table parses as an (empty) policy — explicitly nothing,
        // not an error.
        fs::write(dir.join("toker.toml"), "[providers.codex_sub.model_map]\n")
            .expect("rewrite config");
        let config = load_result(&dir).expect("empty table parses");
        assert!(
            config
                .codex_sub
                .as_ref()
                .expect("enabled")
                .model_map
                .is_some(),
            "an empty table is an explicitly empty policy"
        );
        assert_eq!(
            config
                .codex_sub
                .as_ref()
                .expect("enabled")
                .model_map
                .as_ref()
                .expect("empty policy")
                .entries()
                .count(),
            0
        );
    }

    #[test]
    fn codex_sub_is_a_valid_anthropic_protocol_default() {
        // The translated route: bare model names on the anthropic frontend
        // can default to the codex backend (unit C's routing).
        let dir = test_dir("codex-default");
        fs::write(
            dir.join("toker.toml"),
            "default_backend_anthropic = \"codex_sub\"\n[providers.codex_sub]\n",
        )
        .expect("write config");
        let config = load_from(&dir);
        assert_eq!(
            config.default_backend_anthropic.as_deref(),
            Some("codex_sub")
        );
    }

    #[test]
    fn the_gates_block_is_read_and_a_config_without_it_defaults_to_on() {
        let dir = test_dir("gates");
        fs::write(
            dir.join("toker.toml"),
            r#"
[gates]
quota_enabled = false
"#,
        )
        .expect("write config");
        let config = load_from(&dir);
        assert!(!config.gates.quota_enabled, "the file value is read");

        // A phase-1 toker.toml (no [gates] block at all) parses with the
        // default — and an unknown gates key is an error, not a silent
        // default, like every other block.
        fs::write(dir.join("toker.toml"), "port = 19999\n").expect("rewrite config");
        let config = load_result(&dir).expect("phase-1 config parses");
        assert!(config.gates.quota_enabled, "absent block = default = on");
        assert!(config.gates.cold_enabled, "the cold gate defaults on too");

        fs::write(dir.join("toker.toml"), "[gates]\nquota_on = true\n").expect("rewrite config");
        assert!(
            load_result(&dir).is_err(),
            "a typo'd gates key must fail to load"
        );
    }

    #[test]
    fn the_cold_gate_block_is_read_and_validated() {
        let dir = test_dir("gates-cold");
        let load = |text: &str| {
            fs::write(dir.join("toker.toml"), text).expect("write config");
            load_result(&dir)
        };

        // Every knob reads: the toggles, the bar, the fractional idle
        // floor, and the compact-model spec (family, pin, or "off").
        let config = load(
            "[gates]\ncold_enabled = false\ncold_outlook = false\n\
             cold_min_tokens = 50000\ncold_idle_min = 0.5\n\
             compact_model = \"claude-sonnet-5\"\nforce_newest = false\n",
        )
        .expect("cold gates load");
        assert!(!config.gates.cold_enabled);
        assert!(!config.gates.cold_outlook);
        assert_eq!(config.gates.cold_min_tokens, 50_000);
        assert_eq!(config.gates.cold_idle_min, Some(0.5));
        assert_eq!(
            config.gates.compact_model.as_deref(),
            Some("claude-sonnet-5")
        );
        // The force-newest toggle reads too — a false value disables it.
        assert!(!config.gates.force_newest);

        // A negative idle floor is a config error, not a gate that fires
        // on every request.
        assert!(
            load("[gates]\ncold_idle_min = -1\n").is_err(),
            "a negative idle floor must fail to load"
        );
        assert!(
            load("[gates]\ncold_idle_min = \"soon\"\n").is_err(),
            "the floor is a number of minutes"
        );
    }

    #[test]
    fn the_notices_table_is_read_over_the_defaults() {
        let dir = test_dir("notices");
        let load = |text: &str| {
            fs::write(dir.join("toker.toml"), text).expect("write config");
            load_result(&dir)
        };

        // Named styles load whatever their casing; unnamed frontends keep
        // their built-in entries.
        let config = load("[notices]\ndefault = \"Plain\"\nopencode = \"TOKER\"\n")
            .expect("the table loads");
        assert_eq!(config.notices.style_for(None), NoticeStyle::Plain);
        assert_eq!(
            config.notices.style_for(Some("opencode")),
            NoticeStyle::Toker
        );
        assert_eq!(config.notices.style_for(Some("claude")), NoticeStyle::Block);
        assert_eq!(
            config.notices.style_for(Some("elsewhere")),
            NoticeStyle::Plain
        );
        let config = load("[notices]\nclaude = \"gfm\"\n").expect("an override loads");
        assert_eq!(config.notices.style_for(Some("claude")), NoticeStyle::Gfm);

        // The old [gates] key is the default it always effectively was;
        // "insight" was the block's name then.
        let config = load("[gates]\nnotice_style = \"insight\"\n").expect("the old key loads");
        assert_eq!(config.notices.style_for(None), NoticeStyle::Block);
        assert_eq!(
            config.notices.style_for(Some("workhorse")),
            NoticeStyle::Toker
        );
        // Both spellings at once is an error, not a silent precedence.
        let error = load("[gates]\nnotice_style = \"gfm\"\n[notices]\ndefault = \"plain\"\n")
            .expect_err("both spellings");
        assert!(format!("{error:#}").contains("notice_style"));

        // An unknown value is a load error, not a silent default — the
        // deny_unknown_fields precedent, on the value side. The alternate
        // form carries the TOML error, whose snippet names the value.
        let error = load("[notices]\nclaude = \"fancy\"\n").expect_err("unknown style");
        assert!(format!("{error:#}").contains("fancy"));
        // A key the prefix could never carry is refused, not ignored.
        let error = load("[notices]\nClaude = \"plain\"\n").expect_err("not a frontend name");
        assert!(format!("{error:#}").contains("Claude"));

        // The rewrite writes the new spelling, defaults included, and
        // drops the old key.
        let config = load("[gates]\nnotice_style = \"plain\"\n").expect("loads");
        let text = toml::to_string(&config.to_file()).expect("serialise");
        assert!(!text.contains("notice_style"), "{text}");
        assert!(
            text.contains("[notices]")
                && text.contains("default = \"plain\"")
                && text.contains("claude = \"block\"")
                && text.contains("workhorse = \"toker\""),
            "{text}"
        );
        let reloaded = load(&text).expect("the rewrite loads");
        assert_eq!(reloaded.notices, config.notices);
    }

    #[test]
    fn an_unknown_default_backend_anthropic_is_an_error() {
        let dir = test_dir("bad-backend");
        fs::write(
            dir.join("toker.toml"),
            r#"default_backend_anthropic = "not-a-backend""#,
        )
        .expect("write config");
        let error = load_result(&dir)
            .and_then(|config| config.validate())
            .expect_err("unroutable default must fail");
        assert!(error.to_string().contains("default_backend_anthropic"));
    }

    #[test]
    fn the_awake_toggle_reads_the_file_and_defaults_on() {
        let dir = test_dir("awake-toggle");
        let load = |text: &str| {
            fs::write(dir.join("toker.toml"), text).expect("write config");
            load_result(&dir)
        };

        // Off is the only way to disable the idle-sleep lock.
        assert!(
            !load("awake = false\n").expect("off loads").awake,
            "the file value is read"
        );
        assert!(
            load("port = 19999\n").expect("absent key parses").awake,
            "absent awake = default = on"
        );
    }

    #[test]
    fn transcript_roots_are_read_and_a_config_without_them_parses() {
        // The harness-root list: the file names
        // the extra config directories whose transcripts the session
        // labels also look under. `~` is NOT expanded at load — it
        // expands at lookup, against whatever home is running the view.
        let dir = test_dir("transcript-roots");
        let load = |text: &str| {
            fs::write(dir.join("toker.toml"), text).expect("write config");
            load_result(&dir)
        };

        let config = load(r#"transcript_roots = ["/harness/repos/.claude", "~/elsewhere"]"#)
            .expect("transcript roots load");
        assert_eq!(
            config.transcript_roots,
            vec![
                PathBuf::from("/harness/repos/.claude"),
                PathBuf::from("~/elsewhere"),
            ],
            "~ expands at lookup, not at load"
        );

        // A config without the key parses with the empty default — the
        // pre-existing configs parse untouched.
        assert!(
            load("port = 19999\n")
                .expect("phase-1 config parses")
                .transcript_roots
                .is_empty()
        );
    }

    #[test]
    fn ping_header_name_reads_the_file_and_validates() {
        // Absent: the default (the predecessor's header, renamed for
        // toker).
        let config = load_from(&test_dir("ping-default"));
        assert_eq!(config.ping_header_name, super::DEFAULT_PING_HEADER);

        // Present: the file value is read.
        let dir = test_dir("ping-file");
        fs::write(dir.join("toker.toml"), r#"ping_header_name = "x-my-ping""#)
            .expect("write config");
        let config = load_from(&dir);
        assert_eq!(config.ping_header_name, "x-my-ping");

        // An unusable name is a load error, not a silent no-match.
        fs::write(
            dir.join("toker.toml"),
            r#"ping_header_name = "not a header!""#,
        )
        .expect("rewrite config");
        let error = load_result(&dir).expect_err("an invalid header name must fail");
        assert!(error.to_string().contains("ping_header_name"));

        fs::write(dir.join("toker.toml"), r#"ping_header_name = """#).expect("rewrite config");
        assert!(load_result(&dir).is_err(), "the empty name fails too");
    }

    #[test]
    fn unknown_file_keys_are_errors_not_silent_defaults() {
        let dir = test_dir("unknown-key");
        fs::write(dir.join("toker.toml"), "prot = 1\n").expect("write config");
        assert!(load_result(&dir).is_err(), "a typo'd key must fail to load");
    }

    #[test]
    fn load_leaves_the_routing_defaults_to_validate() {
        let dir = test_dir("owned-service-config");
        let path = dir.join("toker.toml");
        fs::write(
            &path,
            r#"
default_backend_anthropic = "codex_sub"
[providers.openrouter]
"#,
        )
        .unwrap();
        let config = super::Config::load_from(&path).unwrap();
        assert!(config.openrouter.is_some());
        assert_eq!(
            config.default_backend_anthropic.as_deref(),
            Some("codex_sub")
        );
        assert!(config.validate().is_err());
    }

    #[test]
    fn load_from_reads_exactly_what_the_file_says() {
        // The file is only ever read to import a machine off it, so it
        // reads exactly what is written there: nothing in the importing
        // operator's environment may reach the rows that import writes.
        let dir = test_dir("load-from");
        fs::write(
            dir.join("toker.toml"),
            "port = 19999\n[providers.openrouter]\n",
        )
        .expect("write config");

        let config = super::Config::load_from(&dir.join("toker.toml")).expect("loads");
        assert_eq!(config.port, 19_999);
        assert_eq!(
            config.db_path,
            crate::store::default_db_path().expect("default db path")
        );
        assert_eq!(
            config
                .openrouter
                .as_ref()
                .expect("enabled")
                .upstream
                .as_str(),
            DEFAULT_OPENROUTER_UPSTREAM
        );
    }

    #[test]
    fn api_key_env_wins_over_the_keyring_over_the_literal_then_none() {
        let mut config = OpenRouterConfig {
            upstream: super::parse_upstream(DEFAULT_OPENROUTER_UPSTREAM).unwrap(),
            api_key_env: "TOKER_TEST_KEY_PRECEDENCE".to_owned(),
            api_key_keyring: true,
            api_key: Some("literal-key".to_owned()),
            picker: None,
        };
        let keyring = || Some("from-keyring".to_owned());
        let no_entry = || None;
        let untouched = || -> Option<String> { panic!("the keyring must not be read") };

        let with_key = TestEnvironment::default().with("TOKER_TEST_KEY_PRECEDENCE", "from-env");
        assert_eq!(
            config.api_key_with_env(&with_key, untouched).as_deref(),
            Some("from-env")
        );
        assert_eq!(
            config.key_sources_with_env(&with_key),
            KeySources {
                env_set: true,
                keyring_configured: true,
                literal_set: true
            }
        );

        let empty = TestEnvironment::default();
        assert_eq!(
            config.api_key_with_env(&empty, keyring).as_deref(),
            Some("from-keyring")
        );
        // A keyring read that yields nothing falls to the literal.
        assert_eq!(
            config.api_key_with_env(&empty, no_entry).as_deref(),
            Some("literal-key")
        );

        // Not configured: never read, whatever it holds.
        config.api_key_keyring = false;
        assert_eq!(
            config.api_key_with_env(&empty, untouched).as_deref(),
            Some("literal-key")
        );
        assert_eq!(
            config.key_sources_with_env(&empty),
            KeySources {
                env_set: false,
                keyring_configured: false,
                literal_set: true
            }
        );

        config.api_key = None;
        assert_eq!(
            config.api_key_with_env(&empty, untouched),
            None,
            "no source set"
        );
        assert_eq!(
            config.key_sources_with_env(&empty),
            KeySources {
                env_set: false,
                keyring_configured: false,
                literal_set: false
            }
        );
    }

    #[test]
    fn the_keyring_flag_reads_and_round_trips() {
        let dir = test_dir("keyring-flag");
        fs::write(
            dir.join("toker.toml"),
            "[providers.openrouter]\napi_key_keyring = true\n\
             [providers.anthropic_api]\n",
        )
        .expect("write config");
        let config = load_from(&dir);
        assert!(config.openrouter.as_ref().expect("enabled").api_key_keyring);
        assert!(
            !config
                .anthropic_api
                .as_ref()
                .expect("enabled")
                .api_key_keyring
        );
        // Written only when on: the file keeps saying exactly what it
        // said.
        let text = toml::to_string(&config.to_file()).expect("serialise");
        assert_eq!(text.matches("api_key_keyring").count(), 1, "{text}");
    }
}
