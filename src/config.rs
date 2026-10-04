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
//! Invariant 2 (credentials): the resolved config holds the key sources
//! (env var name, optional literal), and `toker status` reports only which
//! sources are set — never the key values.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use crate::middleware::model_map::{self, ModelMap};
use crate::middleware::notice::NoticeStyle;
use crate::store;

/// The default listener port (plan: a new port, not 18082 — the
/// predecessor stays
/// running at work during migration).
pub const DEFAULT_PORT: u16 = 18_123;

/// Default session-attribution header names, read by name only (plan:
/// Attribution, headers-first; invariant 2 — request headers are never
/// captured wholesale). `x-claude-code-session-id` is claude's own
/// header; `x-toker-session` is what setup injects into opencode.
pub const DEFAULT_SESSION_HEADERS: &[&str] = &[
    "x-toker-session",
    "x-claude-code-session-id",
    "x-session-id",
];

/// The default ping-tagging header (plan: "Ping tagging — marks ping lanes
/// so they never hold the sleep lock"; the predecessor's was
/// `x-ctp-ping`). The window
/// pinger injects it via `ANTHROPIC_CUSTOM_HEADERS`, read by name only
/// (invariant 2).
pub const DEFAULT_PING_HEADER: &str = "x-toker-ping";

/// OpenRouter's upstream base, including the `/v1` prefix.
pub const DEFAULT_OPENROUTER_UPSTREAM: &str = "https://openrouter.ai/api/v1";

/// The env var holding the OpenRouter API key.
pub const DEFAULT_OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// The phase-1 default backend for the openai_chat protocol. Only
/// "openrouter" exists; [`crate::server::Server::new`] enforces it.
pub const DEFAULT_BACKEND_OPENAI_CHAT: &str = "openrouter";

/// Anthropic's upstream base — the API root, no `/v1` prefix: the
/// frontend's `/v1/messages…` paths are already the upstream's paths.
pub const DEFAULT_ANTHROPIC_UPSTREAM: &str = "https://api.anthropic.com";

/// The env var holding the Anthropic API key.
pub const DEFAULT_ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// The default backend for the anthropic protocol (plan: Routing —
/// configured default backend per frontend protocol; bare model names go
/// to the protocol default).
pub const DEFAULT_BACKEND_ANTHROPIC: &str = "anthropic_sub";

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

/// Resolved runtime configuration: file defaults ← `toker.toml` ← env.
/// `PartialEq` is the setup wizard's re-load-compare: a rewritten
/// `toker.toml` must resolve back to exactly the config that was meant
/// (see [`crate::setup::config_writer`]).
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
    /// The default backend for the openai_chat protocol.
    pub default_backend_openai_chat: String,
    /// The openrouter provider block.
    pub openrouter: OpenRouterConfig,
    /// The default backend for the anthropic protocol.
    pub default_backend_anthropic: String,
    /// The anthropic subscription provider block.
    pub anthropic_sub: AnthropicSubConfig,
    /// The anthropic API provider block.
    pub anthropic_api: AnthropicApiConfig,
    /// The codex subscription provider block.
    pub codex_sub: CodexSubConfig,
    /// The middleware gates block.
    pub gates: GatesConfig,
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
    /// reaches the ledger (invariant 1).
    pub transcript_roots: Vec<PathBuf>,
}

/// The `[gates]` block, resolved (plan: Middleware — route-scoped toggles,
/// enabled per route in config).
#[derive(Debug, Clone, PartialEq)]
pub struct GatesConfig {
    /// The quota gate (plan: "Quota gate + release marker"). Arms only on
    /// the anthropic_sub backend — today's only meter source — where it
    /// blocks a request whose quota window is spent, answering with a
    /// synthetic assistant turn; every other backend is a no-op. The
    /// marker *stripping* is the frozen marker rule's, not the toggle's:
    /// it runs on the `/v1/messages` path regardless of this setting (a
    /// toggled strip would change the cached prefix of every conversation
    /// carrying a marker).
    pub quota_enabled: bool,
    /// How the quota gate's notice is rendered (plan: "Native rendering
    /// for gate notices"). The config's stand-in for the per-frontend
    /// choice, until a client-selection mechanism exists: insight (the
    /// default) for claude, gfm for Workhorse-style frontends, plain to
    /// degrade.
    pub notice_style: NoticeStyle,
    /// The cold gate (plan: "Cold gate"): a session
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
            notice_style: NoticeStyle::default(),
            cold_enabled: true,
            cold_outlook: true,
            cold_min_tokens: crate::middleware::cold::DEFAULT_MIN_TOKENS,
            cold_idle_min: None,
            compact_model: None,
            force_newest: true,
        }
    }
}

/// The openrouter provider block, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenRouterConfig {
    /// Upstream base including `/v1`, e.g. `https://openrouter.ai/api/v1`.
    pub upstream: reqwest::Url,
    /// The env var the API key is read from.
    pub api_key_env: String,
    /// An optional literal key, used only when the env var is unset.
    pub api_key: Option<String>,
}

impl OpenRouterConfig {
    /// The API key to use: the env var when set, else the configured
    /// literal. `None` when neither is set — requests then go upstream
    /// unauthenticated and openrouter's 401 body passes through, which
    /// verifies the wiring (ledger-proxy lesson).
    pub fn api_key(&self) -> Option<String> {
        resolve_api_key(&self.api_key_env, &self.api_key)
    }

    /// Which key sources are set, for status reporting (invariant 2: never
    /// the values).
    pub fn key_sources(&self) -> KeySources {
        key_sources_of(&self.api_key_env, &self.api_key)
    }
}

/// The anthropic subscription provider block, resolved. No key sources:
/// auth is pass-through-when-present and toker has no stored sub token yet
/// (a later credentials unit adds signing).
#[derive(Debug, Clone, PartialEq)]
pub struct AnthropicSubConfig {
    /// Upstream base — the API root, no `/v1` prefix.
    pub upstream: reqwest::Url,
    /// The model routing map (plan: Middleware — "Model routing map"):
    /// `[providers.anthropic_sub.model_map]`, an optional operator policy
    /// applied as the pipeline's final routing stage on this backend.
    /// `None` when the table is absent (the default — no mapping).
    pub model_map: Option<ModelMap>,
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
    /// An optional literal key, used only when the env var is unset.
    pub api_key: Option<String>,
}

impl AnthropicApiConfig {
    /// The API key to use: the env var when set, else the configured
    /// literal. `None` when neither is set — requests then go upstream
    /// unauthenticated and anthropic's 401 body passes through.
    pub fn api_key(&self) -> Option<String> {
        resolve_api_key(&self.api_key_env, &self.api_key)
    }

    /// Which key sources are set, for status reporting (invariant 2).
    pub fn key_sources(&self) -> KeySources {
        key_sources_of(&self.api_key_env, &self.api_key)
    }
}

/// The codex subscription provider block, resolved. No key sources:
/// auth is the shared `auth.json` login (see [`crate::providers::codex`]
/// — always toker-signed, never pass-through), reported by presence
/// only (invariant 2: never the values).
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
/// exact identities first then families, deterministic per invariant 4.
/// The round-trip is exact on canonical selectors (the parse side
/// folds non-canonical ones); see [`Config::to_file`].
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

/// Resolve an API key: the named env var when set, else the literal (the
/// shared KeySources resolution every api-key provider uses).
fn resolve_api_key(api_key_env: &str, literal: &Option<String>) -> Option<String> {
    if let Some(key) = env::var_os(api_key_env).filter(|key| !key.is_empty()) {
        return key.into_string().ok();
    }
    literal.clone()
}

/// Which key sources are set — the whole of what status may report about
/// credentials (invariant 2).
fn key_sources_of(api_key_env: &str, literal: &Option<String>) -> KeySources {
    KeySources {
        env_set: env::var_os(api_key_env).is_some_and(|key| !key.is_empty()),
        literal_set: literal.is_some(),
    }
}

/// Whether each key source is configured — the whole of what status may
/// report about credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySources {
    pub env_set: bool,
    pub literal_set: bool,
}

impl Config {
    /// Load the resolved configuration: defaults ← `toker.toml` ← env.
    /// A missing file is the defaults, not an error; a present file that
    /// does not parse is an error — a hand-edited typo must not silently
    /// read as default.
    pub fn load() -> anyhow::Result<Config> {
        let path = config_path()?;
        let mut config = Self::load_from(&path)?;

        // Env overrides (config file loses).
        if let Some(port) = env::var_os("TOKER_PORT") {
            let port = port
                .into_string()
                .map_err(|_| anyhow::anyhow!("TOKER_PORT is not valid UTF-8"))?
                .parse::<u16>()
                .context("TOKER_PORT must be a port number")?;
            config.port = port;
        }
        if let Some(db) = env::var_os("TOKER_DB") {
            config.db_path = PathBuf::from(db);
        }
        if let Some(upstream) = env::var_os("TOKER_UPSTREAM") {
            let upstream = upstream
                .into_string()
                .map_err(|_| anyhow::anyhow!("TOKER_UPSTREAM is not valid UTF-8"))?;
            config.openrouter.upstream = parse_upstream(&upstream).context("TOKER_UPSTREAM")?;
        }

        config.validate()?;
        Ok(config)
    }

    /// Load the resolved configuration from an explicit file, defaults
    /// ← file, with **no env overrides**: the read side the setup
    /// wizard's read-merge-rewrite runs on (see
    /// [`crate::setup::config_writer`]), so a `TOKER_*` override
    /// active in the wizard's environment can never be pinned into the
    /// file by a rewrite. A missing file is the defaults; a present
    /// file that does not parse or validate is an error — a
    /// hand-edited typo must not be silently clobbered.
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
        config.validate()?;
        Ok(config)
    }

    /// The write-side wire mapping — the inverse of
    /// [`Config::resolve_file`]: this resolved config as the
    /// `toker.toml` shape, for the setup wizard's read-merge-rewrite
    /// (see [`crate::setup::config_writer`]). Every key the reader
    /// understands is written explicitly — the wizard writes the file
    /// it fully understands, defaults included. Round-trip
    /// normalisations, deliberate and asserted there: upstream URLs in
    /// their parsed normalised form (a bare host gains its trailing
    /// slash), `~`-leading paths already expanded at load, model-map
    /// selectors canonical; comments and hand formatting are not
    /// carried.
    pub(crate) fn to_file(&self) -> FileConfig {
        FileConfig {
            port: Some(self.port),
            db_path: Some(self.db_path.to_string_lossy().into_owned()),
            session_header_names: Some(self.session_header_names.clone()),
            ping_header_name: Some(self.ping_header_name.clone()),
            default_backend_openai_chat: Some(self.default_backend_openai_chat.clone()),
            default_backend_anthropic: Some(self.default_backend_anthropic.clone()),
            awake: Some(self.awake),
            transcript_roots: Some(
                self.transcript_roots
                    .iter()
                    .map(|root| root.to_string_lossy().into_owned())
                    .collect(),
            ),
            providers: FileProviders {
                openrouter: Some(FileOpenRouter {
                    upstream: Some(self.openrouter.upstream.to_string()),
                    api_key_env: Some(self.openrouter.api_key_env.clone()),
                    api_key: self.openrouter.api_key.clone(),
                }),
                anthropic_sub: Some(FileAnthropicSub {
                    upstream: Some(self.anthropic_sub.upstream.to_string()),
                    model_map: self.anthropic_sub.model_map.as_ref().map(model_map_table),
                }),
                anthropic_api: Some(FileAnthropicApi {
                    upstream: Some(self.anthropic_api.upstream.to_string()),
                    api_key_env: Some(self.anthropic_api.api_key_env.clone()),
                    api_key: self.anthropic_api.api_key.clone(),
                    model_map: self.anthropic_api.model_map.as_ref().map(model_map_table),
                }),
                codex_sub: Some(FileCodexSub {
                    upstream: Some(self.codex_sub.upstream.to_string()),
                    originator: Some(self.codex_sub.originator.clone()),
                    auth_path: Some(self.codex_sub.auth_path.to_string_lossy().into_owned()),
                    refresh_url: Some(self.codex_sub.refresh_url.to_string()),
                    client_version: self.codex_sub.client_version.clone(),
                    version_probe: Some(self.codex_sub.version_probe),
                    model_map: self.codex_sub.model_map.as_ref().map(model_map_table),
                }),
            },
            gates: FileGates {
                quota_enabled: Some(self.gates.quota_enabled),
                notice_style: Some(self.gates.notice_style),
                cold_enabled: Some(self.gates.cold_enabled),
                cold_outlook: Some(self.gates.cold_outlook),
                cold_min_tokens: Some(self.gates.cold_min_tokens),
                cold_idle_min: self.gates.cold_idle_min,
                compact_model: self.gates.compact_model.clone(),
                force_newest: Some(self.gates.force_newest),
            },
        }
    }

    /// The file→resolved mapping shared by [`Config::load`] and
    /// [`Config::load_from`]: every default applied, every block
    /// parsed. No env, no validation — both are the callers' side.
    fn resolve_file(file: FileConfig) -> anyhow::Result<Config> {
        let openrouter = OpenRouterConfig {
            upstream: parse_upstream(
                file.providers
                    .openrouter
                    .as_ref()
                    .and_then(|p| p.upstream.as_deref())
                    .unwrap_or(DEFAULT_OPENROUTER_UPSTREAM),
            )?,
            api_key_env: file
                .providers
                .openrouter
                .as_ref()
                .and_then(|p| p.api_key_env.as_deref())
                .unwrap_or(DEFAULT_OPENROUTER_API_KEY_ENV)
                .to_owned(),
            api_key: file
                .providers
                .openrouter
                .as_ref()
                .and_then(|p| p.api_key.clone()),
        };

        let anthropic_sub = AnthropicSubConfig {
            upstream: parse_upstream(
                file.providers
                    .anthropic_sub
                    .as_ref()
                    .and_then(|p| p.upstream.as_deref())
                    .unwrap_or(DEFAULT_ANTHROPIC_UPSTREAM),
            )?,
            model_map: match file.providers.anthropic_sub.as_ref() {
                Some(sub) => match &sub.model_map {
                    Some(table) => parse_model_map_table("anthropic_sub", table)?,
                    None => None,
                },
                None => None,
            },
        };
        let anthropic_api = AnthropicApiConfig {
            upstream: parse_upstream(
                file.providers
                    .anthropic_api
                    .as_ref()
                    .and_then(|p| p.upstream.as_deref())
                    .unwrap_or(DEFAULT_ANTHROPIC_UPSTREAM),
            )?,
            model_map: match file.providers.anthropic_api.as_ref() {
                Some(api) => match &api.model_map {
                    Some(table) => parse_model_map_table("anthropic_api", table)?,
                    None => None,
                },
                None => None,
            },
            api_key_env: file
                .providers
                .anthropic_api
                .as_ref()
                .and_then(|p| p.api_key_env.as_deref())
                .unwrap_or(DEFAULT_ANTHROPIC_API_KEY_ENV)
                .to_owned(),
            api_key: file
                .providers
                .anthropic_api
                .as_ref()
                .and_then(|p| p.api_key.clone()),
        };
        let codex_sub = CodexSubConfig {
            upstream: parse_upstream(
                file.providers
                    .codex_sub
                    .as_ref()
                    .and_then(|p| p.upstream.as_deref())
                    .unwrap_or(DEFAULT_CODEX_UPSTREAM),
            )?,
            originator: file
                .providers
                .codex_sub
                .as_ref()
                .and_then(|p| p.originator.as_deref())
                .unwrap_or(DEFAULT_CODEX_ORIGINATOR)
                .to_owned(),
            auth_path: expand_tilde(
                file.providers
                    .codex_sub
                    .as_ref()
                    .and_then(|p| p.auth_path.as_deref())
                    .unwrap_or(DEFAULT_CODEX_AUTH_PATH),
            ),
            refresh_url: parse_upstream(
                file.providers
                    .codex_sub
                    .as_ref()
                    .and_then(|p| p.refresh_url.as_deref())
                    .unwrap_or(DEFAULT_CODEX_REFRESH_URL),
            )?,
            model_map: match file.providers.codex_sub.as_ref() {
                Some(sub) => match &sub.model_map {
                    Some(table) => parse_model_map_table("codex_sub", table)?,
                    None => None,
                },
                None => None,
            },
            client_version: file
                .providers
                .codex_sub
                .as_ref()
                .and_then(|p| p.client_version.clone()),
            version_probe: file
                .providers
                .codex_sub
                .as_ref()
                .and_then(|p| p.version_probe)
                .unwrap_or(true),
        };

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
            default_backend_openai_chat: file
                .default_backend_openai_chat
                .unwrap_or_else(|| DEFAULT_BACKEND_OPENAI_CHAT.to_owned()),
            openrouter,
            default_backend_anthropic: file
                .default_backend_anthropic
                .unwrap_or_else(|| DEFAULT_BACKEND_ANTHROPIC.to_owned()),
            anthropic_sub,
            anthropic_api,
            codex_sub,
            gates: GatesConfig {
                quota_enabled: file.gates.quota_enabled.unwrap_or(true),
                notice_style: file.gates.notice_style.unwrap_or_default(),
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
    /// the wizard's gate: [`crate::setup::config_writer`] re-runs it
    /// over the wizard's changes before anything is written.
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
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
        // The anthropic protocol's backends are all wired (routing resolves
        // by name; codex_sub serves through the translation pipeline), but
        // the protocol default must name one of them — anything else cannot
        // route anywhere.
        if !matches!(
            self.default_backend_anthropic.as_str(),
            "anthropic_sub" | "anthropic_api" | "codex_sub"
        ) {
            bail!(
                "default_backend_anthropic must be \"anthropic_sub\", \
                 \"anthropic_api\", or \"codex_sub\", not {:?}",
                self.default_backend_anthropic
            );
        }
        // The codex originator rides on every request header: an
        // unusable value would silently never reach the upstream.
        if let Err(error) = axum::http::HeaderValue::from_str(&self.codex_sub.originator) {
            bail!(
                "providers.codex_sub originator {:?} is not a valid header value: {error}",
                self.codex_sub.originator
            );
        }
        Ok(())
    }
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
}

/// The `[gates]` block of `toker.toml`, serde-side.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileGates {
    #[serde(skip_serializing_if = "Option::is_none")]
    quota_enabled: Option<bool>,
    /// Parsed by [`NoticeStyle`]'s case-insensitive deserialiser; an
    /// unknown value fails the load, like a typo'd key would.
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
    anthropic_sub: Option<FileAnthropicSub>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anthropic_api: Option<FileAnthropicApi>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_sub: Option<FileCodexSub>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileOpenRouter {
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileAnthropicSub {
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream: Option<String>,
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
pub fn config_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = env::var_os("TOKER_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let config_home = match env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = env::var_os("HOME")
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

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_ANTHROPIC_API_KEY_ENV, DEFAULT_ANTHROPIC_UPSTREAM, DEFAULT_BACKEND_ANTHROPIC,
        DEFAULT_CODEX_AUTH_PATH, DEFAULT_CODEX_ORIGINATOR, DEFAULT_CODEX_REFRESH_URL,
        DEFAULT_CODEX_UPSTREAM, DEFAULT_OPENROUTER_UPSTREAM, DEFAULT_PORT, DEFAULT_SESSION_HEADERS,
        KeySources, OpenRouterConfig,
    };
    use crate::middleware::model_map;
    use crate::middleware::notice::NoticeStyle;
    use crate::store::Store;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    /// Env-reading tests share process-global env vars, so they serialise
    /// on this lock (RUST_TEST_THREADS-style parallelism would race
    /// otherwise).
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn set_env(name: &str, value: Option<&str>) {
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }

    /// A fresh scratch directory under /tmp/opencode, unique per call.
    fn test_dir(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = PathBuf::from("/tmp/opencode").join(format!(
            "config-{}-{}-{}",
            std::process::id(),
            name,
            n
        ));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// A config pointing at a scratch db, loadable without touching the
    /// user's real config or env.
    fn load_from(dir: &std::path::Path) -> super::Config {
        let _guard = env_lock().lock().unwrap();
        set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
        set_env("TOKER_PORT", None);
        set_env("TOKER_DB", None);
        set_env("TOKER_UPSTREAM", None);
        set_env("OPENROUTER_API_KEY", None);
        super::Config::load().expect("load config")
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
        assert_eq!(config.default_backend_openai_chat, "openrouter");
        assert_eq!(config.default_backend_anthropic, DEFAULT_BACKEND_ANTHROPIC);
        // The codex defaults: the ChatGPT backend API's codex root, the
        // codex CLI's own originator and login/refresh endpoints, the
        // auth path tilde-expanded against $HOME.
        assert_eq!(config.codex_sub.upstream.as_str(), DEFAULT_CODEX_UPSTREAM);
        assert_eq!(config.codex_sub.originator, DEFAULT_CODEX_ORIGINATOR);
        assert_eq!(
            config.codex_sub.auth_path,
            super::expand_tilde(DEFAULT_CODEX_AUTH_PATH),
            "the default auth path is the codex CLI's, tilde-expanded"
        );
        assert_eq!(
            config.codex_sub.refresh_url.as_str(),
            DEFAULT_CODEX_REFRESH_URL
        );
        // No model map is configured anywhere by default: absence is the
        // disabled state, the same as every other optional block.
        assert_eq!(config.anthropic_sub.model_map, None);
        assert_eq!(config.anthropic_api.model_map, None);
        assert_eq!(config.codex_sub.model_map, None);
        // The default constant is a bare host; the resolved Url carries
        // its normalised trailing slash.
        let default_upstream =
            reqwest::Url::parse(DEFAULT_ANTHROPIC_UPSTREAM).expect("default upstream");
        assert_eq!(config.anthropic_sub.upstream, default_upstream);
        assert_eq!(config.anthropic_api.upstream, default_upstream);
        assert_eq!(
            config.anthropic_api.api_key_env,
            DEFAULT_ANTHROPIC_API_KEY_ENV
        );
        assert_eq!(config.anthropic_api.api_key, None);
        assert_eq!(
            config.openrouter.upstream.as_str(),
            DEFAULT_OPENROUTER_UPSTREAM
        );
        // The quota gate is on by default: a proxy that silently stopped
        // gating is a proxy that quietly spends overage.
        assert!(config.gates.quota_enabled);
        // The notice renders in the default style (the generic GFM alert —
        // the insight block is claude-only): a change here would change the
        // bytes of every notice overnight.
        assert_eq!(config.gates.notice_style, NoticeStyle::Gfm);
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
        assert_eq!(config.openrouter.api_key_env, "OPENROUTER_API_KEY");
        assert_eq!(config.openrouter.api_key, None);
        // The db defaults to the store's default path.
        assert_eq!(
            config.db_path,
            crate::store::default_db_path().expect("default db path")
        );
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
        assert_eq!(config.openrouter.upstream.as_str(), "http://localhost:9/v1");
        assert_eq!(config.openrouter.api_key_env, "TOKER_TEST_KEY_FILE");
        assert_eq!(config.openrouter.api_key.as_deref(), Some("literal-key"));
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

        let _guard = env_lock().lock().unwrap();
        set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
        set_env("TOKER_TEST_ANTHROPIC_KEY_FILE", None);
        let config = super::Config::load().expect("load config");
        assert_eq!(config.default_backend_anthropic, "anthropic_api");
        assert_eq!(
            config.anthropic_sub.upstream.as_str(),
            "http://localhost:9/"
        );
        assert_eq!(
            config.anthropic_api.upstream.as_str(),
            "http://localhost:10/"
        );
        assert_eq!(
            config.anthropic_api.api_key_env,
            "TOKER_TEST_ANTHROPIC_KEY_FILE"
        );
        assert_eq!(
            config.anthropic_api.api_key.as_deref(),
            Some("ak-literal-test")
        );

        // The shared KeySources resolution, mirroring openrouter's.
        set_env("TOKER_TEST_ANTHROPIC_KEY_FILE", Some("ak-from-env"));
        assert_eq!(
            config.anthropic_api.api_key().as_deref(),
            Some("ak-from-env")
        );
        assert_eq!(
            config.anthropic_api.key_sources(),
            KeySources {
                env_set: true,
                literal_set: true
            }
        );
        set_env("TOKER_TEST_ANTHROPIC_KEY_FILE", None);

        // A config with no anthropic keys at all (a phase-1 toker.toml)
        // still parses and resolves to the defaults — the defaults test's
        // domain, here proven against a real file too.
        fs::write(dir.join("toker.toml"), "port = 19999\n").expect("rewrite config");
        let config = super::Config::load().expect("phase-1 config parses");
        assert_eq!(config.default_backend_anthropic, "anthropic_sub");
        assert_eq!(
            config.anthropic_sub.upstream,
            reqwest::Url::parse(DEFAULT_ANTHROPIC_UPSTREAM).expect("default upstream")
        );
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
        let _guard = env_lock().lock().unwrap();
        set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
        let config = super::Config::load().expect("load config");
        assert_eq!(
            config.codex_sub.upstream.as_str(),
            "http://localhost:9/backend-api/codex"
        );
        assert_eq!(config.codex_sub.originator, "my_tools_proxy");
        assert_eq!(
            config.codex_sub.auth_path,
            PathBuf::from("/tmp/opencode/some-login.json")
        );
        assert_eq!(
            config.codex_sub.refresh_url.as_str(),
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
        let config = super::Config::load().expect("load config");
        assert_eq!(
            config.codex_sub.auth_path,
            super::expand_tilde("~/.codex/auth.json")
        );

        // A phase-1 toker.toml (no codex block at all) parses with the
        // defaults, and a typo'd key is an error, not a silent default.
        fs::write(dir.join("toker.toml"), "port = 19999\n").expect("rewrite config");
        let config = super::Config::load().expect("phase-1 config parses");
        assert_eq!(config.codex_sub.originator, super::DEFAULT_CODEX_ORIGINATOR);

        fs::write(
            dir.join("toker.toml"),
            "[providers.codex_sub]\nupstream = \"http://x\"\noriginater = \"typo\"\n",
        )
        .expect("rewrite config");
        assert!(
            super::Config::load().is_err(),
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
        let error = super::Config::load().expect_err("invalid originator must fail");
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
        let _guard = env_lock().lock().unwrap();
        set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
        let config = super::Config::load().expect("load config");

        // Parsed into the committed policy: exact identity ahead of
        // family, published snapshots folding to their identities.
        let codex_map = config.codex_sub.model_map.as_ref().expect("codex map");
        assert_eq!(
            model_map::preview_mapped_model(Some(codex_map), "claude-opus-5"),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            model_map::preview_mapped_model(Some(codex_map), "claude-opus-4-5-20251101"),
            Some("special-opus"),
            "the exact selector beats the family one"
        );
        assert_eq!(
            model_map::preview_mapped_model(Some(codex_map), "claude-sonnet-5"),
            Some("claude-sonnet-5"),
            "an unmatched model is the identity"
        );
        let sub_map = config.anthropic_sub.model_map.as_ref().expect("sub map");
        assert_eq!(
            model_map::preview_mapped_model(Some(sub_map), "claude-opus-4-5"),
            Some("claude-opus-4-8")
        );
        // A backend with no table carries no policy, and one backend's
        // map never answers for another's.
        assert_eq!(config.anthropic_api.model_map, None);
        assert_eq!(
            model_map::preview_mapped_model(
                config.anthropic_api.model_map.as_ref(),
                "claude-opus-5"
            ),
            Some("claude-opus-5")
        );

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
            let error = super::Config::load().expect_err("bad policy must fail");
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
        let config = super::Config::load().expect("empty table parses");
        assert!(
            config.codex_sub.model_map.is_some(),
            "an empty table is an explicitly empty policy"
        );
        assert_eq!(
            model_map::preview_mapped_model(config.codex_sub.model_map.as_ref(), "claude-opus-5"),
            Some("claude-opus-5")
        );
    }

    #[test]
    fn codex_sub_is_a_valid_anthropic_protocol_default() {
        // The translated route: bare model names on the anthropic frontend
        // can default to the codex backend (unit C's routing).
        let dir = test_dir("codex-default");
        fs::write(
            dir.join("toker.toml"),
            r#"default_backend_anthropic = "codex_sub""#,
        )
        .expect("write config");
        let _guard = env_lock().lock().unwrap();
        set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
        let config = super::Config::load().expect("load config");
        assert_eq!(config.default_backend_anthropic, "codex_sub");
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
        let _guard = env_lock().lock().unwrap();
        set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
        let config = super::Config::load().expect("load config");
        assert!(!config.gates.quota_enabled, "the file value is read");

        // A phase-1 toker.toml (no [gates] block at all) parses with the
        // default — and an unknown gates key is an error, not a silent
        // default, like every other block.
        fs::write(dir.join("toker.toml"), "port = 19999\n").expect("rewrite config");
        let config = super::Config::load().expect("phase-1 config parses");
        assert!(config.gates.quota_enabled, "absent block = default = on");
        assert!(config.gates.cold_enabled, "the cold gate defaults on too");

        fs::write(dir.join("toker.toml"), "[gates]\nquota_on = true\n").expect("rewrite config");
        assert!(
            super::Config::load().is_err(),
            "a typo'd gates key must fail to load"
        );
    }

    #[test]
    fn the_cold_gate_block_is_read_and_validated() {
        let dir = test_dir("gates-cold");
        let load = |text: &str| {
            let _guard = env_lock().lock().unwrap();
            set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
            fs::write(dir.join("toker.toml"), text).expect("write config");
            super::Config::load()
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
    fn notice_style_is_read_case_insensitively_and_unknown_values_fail() {
        let dir = test_dir("notice-style");
        let load = |text: &str| {
            let _guard = env_lock().lock().unwrap();
            set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
            fs::write(dir.join("toker.toml"), text).expect("write config");
            super::Config::load()
        };

        // Explicitly named styles load, whatever their casing.
        assert_eq!(
            load("[gates]\nnotice_style = \"plain\"\n")
                .expect("plain loads")
                .gates
                .notice_style,
            NoticeStyle::Plain
        );
        assert_eq!(
            load("[gates]\nnotice_style = \"Gfm\"\n")
                .expect("gfm loads")
                .gates
                .notice_style,
            NoticeStyle::Gfm
        );
        assert_eq!(
            load("[gates]\nnotice_style = \"INSIGHT\"\n")
                .expect("insight loads")
                .gates
                .notice_style,
            NoticeStyle::Insight
        );

        // Absent: the default (the generic GFM alert) — never a silent downgrade.
        assert_eq!(
            load("port = 19999\n")
                .expect("absent block parses")
                .gates
                .notice_style,
            NoticeStyle::Gfm
        );

        // An unknown value is a load error, not a silent default — the
        // deny_unknown_fields precedent, on the value side. The plain
        // Display is only the "parsing <path>" context; the alternate
        // form carries the TOML error, whose snippet names the key.
        let error = load("[gates]\nnotice_style = \"fancy\"\n").expect_err("unknown style");
        let chain = format!("{error:#}");
        assert!(
            chain.contains("notice_style") && chain.contains("fancy"),
            "the error names the key and the value: {chain}"
        );
    }

    #[test]
    fn an_unknown_default_backend_anthropic_is_an_error() {
        let dir = test_dir("bad-backend");
        fs::write(
            dir.join("toker.toml"),
            r#"default_backend_anthropic = "not-a-backend""#,
        )
        .expect("write config");
        let _guard = env_lock().lock().unwrap();
        set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
        let error = super::Config::load().expect_err("unroutable default must fail");
        assert!(error.to_string().contains("default_backend_anthropic"));
    }

    #[test]
    fn the_awake_toggle_reads_the_file_and_defaults_on() {
        let dir = test_dir("awake-toggle");
        let load = |text: &str| {
            let _guard = env_lock().lock().unwrap();
            set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
            fs::write(dir.join("toker.toml"), text).expect("write config");
            super::Config::load()
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
            let _guard = env_lock().lock().unwrap();
            set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
            fs::write(dir.join("toker.toml"), text).expect("write config");
            super::Config::load()
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
        let _guard = env_lock().lock().unwrap();
        set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
        let config = super::Config::load().expect("load config");
        assert_eq!(config.ping_header_name, "x-my-ping");

        // An unusable name is a load error, not a silent no-match.
        fs::write(
            dir.join("toker.toml"),
            r#"ping_header_name = "not a header!""#,
        )
        .expect("rewrite config");
        let error = super::Config::load().expect_err("an invalid header name must fail");
        assert!(error.to_string().contains("ping_header_name"));

        fs::write(dir.join("toker.toml"), r#"ping_header_name = """#).expect("rewrite config");
        assert!(super::Config::load().is_err(), "the empty name fails too");
    }

    #[test]
    fn unknown_file_keys_are_errors_not_silent_defaults() {
        let dir = test_dir("unknown-key");
        fs::write(dir.join("toker.toml"), "prot = 1\n").expect("write config");
        let _guard = env_lock().lock().unwrap();
        set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
        assert!(
            super::Config::load().is_err(),
            "a typo'd key must fail to load"
        );
    }

    #[test]
    fn load_from_reads_a_path_without_env_overrides() {
        // The setup wizard's read-merge side: `load_from(path)` is
        // env-free, so a `TOKER_*` override active in the wizard's own
        // environment can never be read-merged and pinned into the file
        // by a rewrite (see `crate::setup::config_writer`) — while
        // `load()` still layers the env over the same file for the
        // running daemon.
        let _guard = env_lock().lock().unwrap();
        let dir = test_dir("load-from");
        fs::write(dir.join("toker.toml"), "port = 19999\n").expect("write config");
        set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
        set_env("TOKER_PORT", Some("20000"));
        set_env(
            "TOKER_DB",
            Some(&dir.join("elsewhere.db").to_string_lossy()),
        );
        set_env("TOKER_UPSTREAM", Some("http://from-env/v1"));

        let config = super::Config::load_from(&dir.join("toker.toml")).expect("loads");
        assert_eq!(config.port, 19_999, "no env override applies to load_from");
        assert_eq!(
            config.db_path,
            crate::store::default_db_path().expect("default db path"),
            "the db path is the file-or-default one, not TOKER_DB"
        );
        assert_ne!(config.openrouter.upstream.as_str(), "http://from-env/v1");

        let loaded = super::Config::load().expect("loads");
        assert_eq!(loaded.port, 20_000);
        assert_eq!(loaded.db_path, dir.join("elsewhere.db"));
        assert_eq!(loaded.openrouter.upstream.as_str(), "http://from-env/v1");

        set_env("TOKER_PORT", None);
        set_env("TOKER_DB", None);
        set_env("TOKER_UPSTREAM", None);
    }

    #[test]
    fn env_overrides_the_file() {
        let _guard = env_lock().lock().unwrap();
        let dir = test_dir("env");
        fs::write(
            dir.join("toker.toml"),
            "port = 19999\n[providers.openrouter]\nupstream = \"http://from-file/v1\"\n",
        )
        .expect("write config");
        set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
        set_env("TOKER_PORT", Some("20000"));
        set_env("TOKER_DB", Some(&dir.join("env.db").to_string_lossy()));
        set_env("TOKER_UPSTREAM", Some("http://from-env/v1"));
        set_env("OPENROUTER_API_KEY", None);

        let config = super::Config::load().expect("load config");
        assert_eq!(config.port, 20000);
        assert_eq!(config.db_path, dir.join("env.db"));
        assert_eq!(config.openrouter.upstream.as_str(), "http://from-env/v1");

        set_env("TOKER_PORT", None);
        set_env("TOKER_DB", None);
        set_env("TOKER_UPSTREAM", None);

        let _ = Store::open(&config.db_path).expect("the env db path is openable");
    }

    #[test]
    fn bad_env_port_is_an_error() {
        let _guard = env_lock().lock().unwrap();
        let dir = test_dir("bad-port");
        set_env("TOKER_CONFIG", dir.join("toker.toml").to_str());
        set_env("TOKER_PORT", Some("not-a-port"));
        let error = super::Config::load().expect_err("TOKER_PORT must be numeric");
        assert!(error.to_string().contains("TOKER_PORT"));
        set_env("TOKER_PORT", None);
    }

    #[test]
    fn api_key_env_wins_over_literal_then_none() {
        let _guard = env_lock().lock().unwrap();
        let config = OpenRouterConfig {
            upstream: super::parse_upstream(DEFAULT_OPENROUTER_UPSTREAM).unwrap(),
            api_key_env: "TOKER_TEST_KEY_PRECEDENCE".to_owned(),
            api_key: Some("literal-key".to_owned()),
        };
        set_env("TOKER_TEST_KEY_PRECEDENCE", Some("from-env"));
        assert_eq!(config.api_key().as_deref(), Some("from-env"));
        assert_eq!(
            config.key_sources(),
            KeySources {
                env_set: true,
                literal_set: true
            }
        );

        set_env("TOKER_TEST_KEY_PRECEDENCE", None);
        assert_eq!(config.api_key().as_deref(), Some("literal-key"));
        assert_eq!(
            config.key_sources(),
            KeySources {
                env_set: false,
                literal_set: true
            }
        );

        let no_literal = OpenRouterConfig {
            upstream: super::parse_upstream(DEFAULT_OPENROUTER_UPSTREAM).unwrap(),
            api_key_env: "TOKER_TEST_KEY_PRECEDENCE".to_owned(),
            api_key: None,
        };
        assert_eq!(no_literal.api_key(), None, "neither source set");
        assert_eq!(
            no_literal.key_sources(),
            KeySources {
                env_set: false,
                literal_set: false
            }
        );
    }
}
