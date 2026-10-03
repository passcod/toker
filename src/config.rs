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
use std::path::PathBuf;

use anyhow::{Context, bail};

use crate::store;

/// The default listener port (plan: a new port, not 18082 — ctp stays
/// running at work during migration).
pub const DEFAULT_PORT: u16 = 18_123;

/// Default session-attribution header names, read by name only (plan:
/// Attribution, headers-first; invariant 2 — request headers are never
/// captured wholesale). `x-claude-code-session-id` is claude's own
/// (ctp's header); `x-toker-session` is what setup injects into opencode.
pub const DEFAULT_SESSION_HEADERS: &[&str] = &[
    "x-toker-session",
    "x-claude-code-session-id",
    "x-session-id",
];

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

/// Resolved runtime configuration: file defaults ← `toker.toml` ← env.
#[derive(Debug, Clone)]
pub struct Config {
    /// The loopback listener port.
    pub port: u16,
    /// The SQLite ledger path.
    pub db_path: PathBuf,
    /// Session-attribution header names, in priority order.
    pub session_header_names: Vec<String>,
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
    /// The middleware gates block.
    pub gates: GatesConfig,
}

/// The `[gates]` block, resolved (plan: Middleware — route-scoped toggles,
/// enabled per route in config).
#[derive(Debug, Clone)]
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
}

impl Default for GatesConfig {
    fn default() -> Self {
        GatesConfig {
            quota_enabled: true,
        }
    }
}

/// The openrouter provider block, resolved.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub struct AnthropicSubConfig {
    /// Upstream base — the API root, no `/v1` prefix.
    pub upstream: reqwest::Url,
}

/// The anthropic API provider block, resolved — the same KeySources
/// pattern as openrouter.
#[derive(Debug, Clone)]
pub struct AnthropicApiConfig {
    /// Upstream base — the API root, no `/v1` prefix.
    pub upstream: reqwest::Url,
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
        let file = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<FileConfig>(&text)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };

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
        };
        let anthropic_api = AnthropicApiConfig {
            upstream: parse_upstream(
                file.providers
                    .anthropic_api
                    .as_ref()
                    .and_then(|p| p.upstream.as_deref())
                    .unwrap_or(DEFAULT_ANTHROPIC_UPSTREAM),
            )?,
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

        let mut config = Config {
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
            default_backend_openai_chat: file
                .default_backend_openai_chat
                .unwrap_or_else(|| DEFAULT_BACKEND_OPENAI_CHAT.to_owned()),
            openrouter,
            default_backend_anthropic: file
                .default_backend_anthropic
                .unwrap_or_else(|| DEFAULT_BACKEND_ANTHROPIC.to_owned()),
            anthropic_sub,
            anthropic_api,
            gates: GatesConfig {
                quota_enabled: file.gates.quota_enabled.unwrap_or(true),
            },
        };

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

    /// Cross-field validation the individual pieces cannot see.
    fn validate(&self) -> anyhow::Result<()> {
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
        // Both anthropic backends are wired regardless of enabled state
        // (routing resolves by name), but the protocol default must name
        // one of them — anything else cannot route anywhere.
        if !matches!(
            self.default_backend_anthropic.as_str(),
            "anthropic_sub" | "anthropic_api"
        ) {
            bail!(
                "default_backend_anthropic must be \"anthropic_sub\" or \
                 \"anthropic_api\", not {:?}",
                self.default_backend_anthropic
            );
        }
        Ok(())
    }
}

/// The `toker.toml` wire shape, serde-side. `deny_unknown_fields` so a
/// typo'd key is an error, not a silent default.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    port: Option<u16>,
    db_path: Option<String>,
    session_header_names: Option<Vec<String>>,
    default_backend_openai_chat: Option<String>,
    default_backend_anthropic: Option<String>,
    providers: FileProviders,
    gates: FileGates,
}

/// The `[gates]` block of `toker.toml`, serde-side.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileGates {
    quota_enabled: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileProviders {
    openrouter: Option<FileOpenRouter>,
    anthropic_sub: Option<FileAnthropicSub>,
    anthropic_api: Option<FileAnthropicApi>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileOpenRouter {
    upstream: Option<String>,
    api_key_env: Option<String>,
    api_key: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileAnthropicSub {
    upstream: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileAnthropicApi {
    upstream: Option<String>,
    api_key_env: Option<String>,
    api_key: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_ANTHROPIC_API_KEY_ENV, DEFAULT_ANTHROPIC_UPSTREAM, DEFAULT_BACKEND_ANTHROPIC,
        DEFAULT_OPENROUTER_UPSTREAM, DEFAULT_PORT, DEFAULT_SESSION_HEADERS, KeySources,
        OpenRouterConfig,
    };
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
            "claude identifies itself by its own header (ctp's)"
        );
        assert_eq!(config.default_backend_openai_chat, "openrouter");
        assert_eq!(config.default_backend_anthropic, DEFAULT_BACKEND_ANTHROPIC);
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

        fs::write(dir.join("toker.toml"), "[gates]\nquota_on = true\n").expect("rewrite config");
        assert!(
            super::Config::load().is_err(),
            "a typo'd gates key must fail to load"
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
