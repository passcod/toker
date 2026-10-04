//! Frontend wiring patches — one pure-file function per frontend.
//!
//! Plan: "Setup wizard" step 4 — "Wire frontends: claude (user +
//! Workhorse repo settings — both required, directory-scoped settings
//! win), opencode (provider baseURL + session header if supported),
//! codex (`config.toml` via `toml_edit`, formatting-preserving), or
//! generic env vars into shell rc". This unit owns the file side of
//! that: each patcher takes a path and a base URL, rewrites exactly one
//! setting, and preserves every other byte the file had — no
//! prompting, no discovery; the interactive unit asks and then calls
//! one of these.
//!
//! The URL shapes are the hand-done precedent on the reference machine,
//! captured by [`anthropic_base_url`] and [`openai_base_url`]:
//!
//! - claude: `env.ANTHROPIC_BASE_URL` = `http://127.0.0.1:18123` — no
//!   `/v1`, because the client appends `/v1/messages` itself;
//! - opencode: `provider.openrouter.options.baseURL` =
//!   `http://127.0.0.1:18123/v1` — with `/v1`, the OpenAI client
//!   convention, because the client appends `/chat/completions`.
//!
//! The real files' shapes drive the fixtures below: a claude settings
//! carries `env` (all string values), `permissions`, `model`, `hooks`,
//! `enabledPlugins`, `spinnerVerbs`, `modelSettings`, `tui`,
//! `skipWorkflowUsageWarning`, `theme`; an opencode config carries
//! `$schema`, a `permissions` ARRAY, `provider`, `agents`. Every one of
//! those survives a patch byte-for-byte except the one key named.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Map, Value};

use crate::setup::atomic::{atomic_patch_json, atomic_write_bytes};

/// The marker comment governing toker's block in a shell rc. One
/// export line, updated in place when the marker exists — re-running
/// the wizard must never stack duplicates.
pub const SHELL_MARKER: &str = "# toker";

/// The env var of the generic-frontend option: every claude-protocol
/// tool reads `ANTHROPIC_BASE_URL` (claude, the CLIs built on its
/// protocol, and anything else anthropic-shaped), so the shell rc is
/// the frontend of last resort when a tool has no config file toker
/// knows how to patch.
pub const SHELL_VAR: &str = "ANTHROPIC_BASE_URL";

// ── the base-URL shapes (the hand-done precedent) ──────────────────────

/// The anthropic-protocol base URL for a toker port: the bare listener,
/// no `/v1` — claude appends `/v1/messages` itself. This is the value
/// this machine's `~/.claude/settings.json` carries.
pub fn anthropic_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// The openai-chat base URL for a toker port: with `/v1` — the OpenAI
/// client convention; opencode appends `/chat/completions` to it. This
/// is the value this machine's `~/.config/opencode/opencode.json`
/// carries under `provider.openrouter.options.baseURL`.
pub fn openai_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

// ── the patchers ────────────────────────────────────────────────────────

/// Point claude (or any claude-protocol client reading this settings
/// file) at toker: set `env.ANTHROPIC_BASE_URL`. Everything else in
/// the file — permissions, hooks, enabledPlugins, any other env var —
/// is preserved; a present-but-non-object `env` is refused, not
/// overwritten.
pub fn patch_claude(path: &Path, base_url: &str) -> anyhow::Result<()> {
    atomic_patch_json(path, |settings| {
        let env = object_slot(settings, "env", "claude settings")?;
        let env = env
            .as_object_mut()
            .expect("object_slot guarantees an object");
        env.insert(SHELL_VAR.to_owned(), Value::String(base_url.to_owned()));
        Ok(())
    })
    .with_context(|| format!("patching claude settings at {}", path.display()))
}

/// The Workhorse variant of the same patch: the work machine's
/// `~/.workhorse/repos/.claude/settings.json` — the repo-root settings
/// Workhorse agents read because directory-scoped settings take
/// precedence over the user ones. Both are required: a Workhorse agent
/// wired only through `~/.claude/settings.json` reads its own
/// directory-scoped settings first and quietly bypasses the proxy —
/// traffic that is invisible to toker but still burns the very quota
/// the gate exists to protect. The wizard offers this patch only when
/// that directory exists; the patch itself is exactly [`patch_claude`]
/// applied at `<repo_root>/.claude/settings.json`.
pub fn patch_claude_workhorse(repo_root: &Path, base_url: &str) -> anyhow::Result<()> {
    patch_claude(&repo_root.join(".claude").join("settings.json"), base_url)
}

/// Point opencode at toker: set `provider.openrouter.options.baseURL`
/// (the shape on this machine — opencode's provider block; the
/// `baseURL` includes the `/v1`, see [`openai_base_url`]). Missing
/// `provider`/`openrouter`/`options` objects are created; a present
/// non-object at any of those keys is refused, not overwritten.
pub fn patch_opencode(path: &Path, base_url: &str) -> anyhow::Result<()> {
    atomic_patch_json(path, |config| {
        let provider = object_slot(config, "provider", "opencode config")?;
        let openrouter = object_slot(provider, "openrouter", "opencode config provider")?;
        let options = object_slot(openrouter, "options", "opencode config provider.openrouter")?;
        let options = options
            .as_object_mut()
            .expect("object_slot guarantees an object");
        options.insert("baseURL".to_owned(), Value::String(base_url.to_owned()));
        Ok(())
    })
    .with_context(|| format!("patching opencode config at {}", path.display()))
}

/// The generic-frontend option: toker's export block in a shell rc —
///
/// ```sh
/// # toker
/// export ANTHROPIC_BASE_URL="http://127.0.0.1:18123"
/// ```
///
/// Idempotent: when the [`SHELL_MARKER`] exists, the governed export
/// line (every `export ANTHROPIC_BASE_URL=` line AFTER the marker —
/// the marker is what made it toker's) is updated in place, first one
/// kept, any further duplicates dropped; re-running never stacks. When
/// no marker exists the block is appended at the end of the file (last
/// assignment wins in a shell, so toker's export is the effective one
/// whatever sits above it); lines BEFORE a marker are never touched —
/// an unmarked `ANTHROPIC_BASE_URL` export is not toker's to rewrite.
/// The write is atomic like every other patch here.
pub fn patch_shell_rc(path: &Path, base_url: &str) -> anyhow::Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
    };
    let updated = shell_block(&existing, base_url);
    if updated == existing {
        // Already wired: an idempotent no-op, not even an mtime churn.
        return Ok(());
    }
    let intended = updated.clone().into_bytes();
    atomic_write_bytes(path, &intended, None, |_temp, written| {
        if written != intended {
            bail!("the written shell rc does not round-trip byte-for-byte — refusing to rename");
        }
        Ok(())
    })
    .with_context(|| format!("patching the shell rc at {}", path.display()))
}

// ── the pieces the patchers share ──────────────────────────────────────

/// The object at `key` in `parent`, inserting `{}` when absent. A
/// present non-object at `key` is an error — patching through (or
/// around) a string where a settings file promises an object would
/// corrupt a shape we do not understand, so the whole patch refuses.
fn object_slot<'a>(parent: &'a mut Value, key: &str, what: &str) -> anyhow::Result<&'a mut Value> {
    let Some(map) = parent.as_object_mut() else {
        bail!("{what}: the JSON at this level is not an object — refusing to patch");
    };
    let slot = map
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        bail!("{what}: {key:?} is not an object — refusing to overwrite it");
    }
    Ok(slot)
}

/// The rc text with toker's block in place (see [`patch_shell_rc`] for
/// the semantics).
fn shell_block(existing: &str, base_url: &str) -> String {
    let export = format!("export {SHELL_VAR}=\"{base_url}\"\n");
    let marker_line = format!("{SHELL_MARKER}\n");
    let mut lines: Vec<String> = if existing.is_empty() {
        Vec::new()
    } else {
        existing.split_inclusive('\n').map(str::to_owned).collect()
    };

    let Some(marker) = lines.iter().position(|line| line.trim() == SHELL_MARKER) else {
        // No block yet: ensure the last line is newline-terminated, then
        // append the block at the end.
        if let Some(last) = lines.last_mut()
            && !last.ends_with('\n')
        {
            last.push('\n');
        }
        lines.push(marker_line);
        lines.push(export);
        return lines.concat();
    };

    // The block exists: update the governed export in place — the first
    // governed line becomes the new export where it sits, further ones
    // are dropped (normalised into it), and a marker whose export was
    // hand-removed gets it re-seated right after the marker.
    let mut result: Vec<String> = Vec::with_capacity(lines.len());
    let mut replaced = false;
    for (index, line) in lines.into_iter().enumerate() {
        if index > marker && is_governed_export(&line) {
            if !replaced {
                result.push(export.clone());
                replaced = true;
            }
        } else {
            result.push(line);
        }
    }
    if !replaced {
        result.insert(marker + 1, export);
    }
    result.concat()
}

/// Whether a line is toker's governed export: `export
/// ANTHROPIC_BASE_URL=…`, whitespace-tolerant on both sides of the
/// assignment (a hand-mangled `export  ANTHROPIC_BASE_URL=` is still
/// recognised, so it is updated rather than duplicated).
fn is_governed_export(line: &str) -> bool {
    let mut tokens = line.split_whitespace();
    tokens.next() == Some("export")
        && tokens.next().is_some_and(|rest| {
            rest.strip_prefix(SHELL_VAR)
                .is_some_and(|assign| assign.starts_with('='))
        })
}

// ── the enum the wizard composes ──────────────────────────────────────

/// A frontend toker can point at itself. The variants carry only the
/// file to patch; the URL is the wizard's choice of port shaped per
/// protocol via [`Frontend::base_url`], and [`Frontend::patch`] applies
/// the matching patcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frontend {
    /// claude's user settings (`~/.claude/settings.json`):
    /// `env.ANTHROPIC_BASE_URL` ([`patch_claude`]).
    Claude {
        /// The settings.json to patch.
        settings: PathBuf,
    },
    /// The Workhorse repo-scoped settings
    /// (`~/.workhorse/repos/.claude/settings.json`) — same patch as
    /// [`Frontend::Claude`], offered by the wizard only when the
    /// directory exists, because directory-scoped settings win over
    /// the user ones (see [`patch_claude_workhorse`]).
    ClaudeWorkhorse {
        /// The settings.json to patch.
        settings: PathBuf,
    },
    /// opencode's config (`~/.config/opencode/opencode.json`):
    /// `provider.openrouter.options.baseURL` ([`patch_opencode`]).
    Opencode {
        /// The opencode.json to patch.
        config: PathBuf,
    },
    /// A shell rc's `# toker` export block — the generic frontend
    /// option ([`patch_shell_rc`]).
    ShellRc {
        /// The rc file to patch.
        rc: PathBuf,
    },
}

impl Frontend {
    /// The Workhorse variant from its repo root: the settings file is
    /// `<repo_root>/.claude/settings.json` (on the work machine,
    /// `~/.workhorse/repos`). Offered by the wizard only when that
    /// directory exists.
    pub fn claude_workhorse(repo_root: impl AsRef<Path>) -> Frontend {
        Frontend::ClaudeWorkhorse {
            settings: repo_root.as_ref().join(".claude").join("settings.json"),
        }
    }

    /// The base URL this frontend wants for a toker port — the
    /// hand-done shapes: anthropic-protocol frontends take the bare
    /// listener, opencode takes the `/v1` form.
    pub fn base_url(&self, port: u16) -> String {
        match self {
            Frontend::Opencode { .. } => openai_base_url(port),
            _ => anthropic_base_url(port),
        }
    }

    /// Apply the patch: the file at the variant's path gets the base
    /// URL (which the wizard built via [`Frontend::base_url`]).
    pub fn patch(&self, base_url: &str) -> anyhow::Result<()> {
        match self {
            Frontend::Claude { settings } => patch_claude(settings, base_url),
            Frontend::ClaudeWorkhorse { settings } => patch_claude(settings, base_url),
            Frontend::Opencode { config } => patch_opencode(config, base_url),
            Frontend::ShellRc { rc } => patch_shell_rc(rc, base_url),
        }
    }

    /// A one-line description for the wizard's display.
    pub fn describe(&self) -> String {
        match self {
            Frontend::Claude { settings } => format!("claude ({})", settings.display()),
            Frontend::ClaudeWorkhorse { settings } => {
                format!("claude in the Workhorse repos ({})", settings.display())
            }
            Frontend::Opencode { config } => format!("opencode ({})", config.display()),
            Frontend::ShellRc { rc } => format!("shell rc ({})", rc.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::setup::test_dir;

    /// The claude settings fixture: the real machine's shape (every
    /// top-level key it has) with the base URL in its UNWIRED state, so
    /// a patch visibly rewires it.
    fn claude_settings() -> Value {
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                "ANTHROPIC_AUTH_TOKEN": "unused",
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "872000",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                "MCP_DISCOVERY_CACHE": "1",
                "CLAUDE_AFK_TIMEOUT_MS": "10000000"
            },
            "permissions": {
                "defaultMode": "auto",
                "allow": ["Bash(agent-repo get *)", "Bash(agent-repo list)"],
                "additionalDirectories": ["~/.cache/agent-repos"]
            },
            "model": "opus[1m]",
            "hooks": {
                "Notification": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": "\"/home/x/.claude/hooks/claude-notify.sh\"",
                                "async": true
                            }
                        ]
                    }
                ]
            },
            "enabledPlugins": {
                "rust-analyzer-lsp@claude-plugins-official": true,
                "playwright@claude-plugins-official": true,
                "remember@claude-plugins-official": true
            },
            "spinnerVerbs": {"mode": "replace", "verbs": ["Working"]},
            "modelSettings": {"opus": {"effortLevel": "medium"}},
            "tui": "fullscreen",
            "skipWorkflowUsageWarning": true,
            "theme": "dark"
        })
    }

    /// The opencode config fixture: the real machine's shape
    /// (permissions as an ARRAY of objects, $schema, agents) with the
    /// baseURL in its unwired state.
    fn opencode_config() -> Value {
        json!({
            "$schema": "https://opencode.ai/config.json",
            "permissions": [
                {"action": "external_directory", "resource": "~/.cache/agent-repos/*", "effect": "allow"},
                {"action": "read", "resource": "~/.cache/agent-repos/*", "effect": "allow"},
                {"action": "edit", "resource": "~/.cache/agent-repos/*", "effect": "deny"}
            ],
            "provider": {
                "openrouter": {"options": {"baseURL": "https://openrouter.ai/api/v1"}}
            },
            "agents": {"title": {"model": "openrouter/z-ai/glm-5.3-flash"}}
        })
    }

    fn write_pretty(path: &std::path::Path, value: &Value) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(value).expect("serialise fixture");
        bytes.push(b'\n');
        fs::write(path, &bytes).expect("write fixture");
        bytes
    }

    fn read_value(path: &std::path::Path) -> Value {
        serde_json::from_slice(&fs::read(path).expect("read back")).expect("parse")
    }

    #[test]
    fn claude_settings_patch_changes_exactly_the_base_url() {
        let path = test_dir("claude").join("settings.json");
        let original = claude_settings();
        write_pretty(&path, &original);

        patch_claude(&path, "http://127.0.0.1:18123").expect("patch");

        // Byte-for-byte: the fixture's own bytes with exactly the one
        // string changed — order, nesting, every other key preserved.
        let mut expected = original.clone();
        expected["env"]["ANTHROPIC_BASE_URL"] = json!("http://127.0.0.1:18123");
        let mut expected_bytes = serde_json::to_vec_pretty(&expected).expect("serialise");
        expected_bytes.push(b'\n');
        assert_eq!(fs::read(&path).expect("read back"), expected_bytes);

        // And re-patching to a different port updates the same line in
        // place — never a second key, never a moved key.
        patch_claude(&path, "http://127.0.0.1:19999").expect("re-patch");
        let mut re_expected = original;
        re_expected["env"]["ANTHROPIC_BASE_URL"] = json!("http://127.0.0.1:19999");
        let mut re_expected_bytes = serde_json::to_vec_pretty(&re_expected).expect("serialise");
        re_expected_bytes.push(b'\n');
        assert_eq!(
            fs::read(&path).expect("read back"),
            re_expected_bytes,
            "same shape, only the value changed"
        );
    }

    #[test]
    fn claude_settings_without_env_gains_it_at_the_end() {
        let path = test_dir("claude-no-env").join("settings.json");
        let original = json!({"model": "opus[1m]", "theme": "dark"});
        write_pretty(&path, &original);
        patch_claude(&path, "http://127.0.0.1:18123").expect("patch");
        let patched = read_value(&path);
        assert_eq!(patched["model"], "opus[1m]", "everything else preserved");
        assert_eq!(patched["theme"], "dark");
        assert_eq!(
            patched["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:18123"
        );
        // The new env key goes last — preserve_order, no re-sorting.
        assert_eq!(
            patched
                .as_object()
                .expect("object")
                .keys()
                .next_back()
                .map(String::as_str),
            Some("env")
        );
    }

    #[test]
    fn claude_settings_with_a_non_object_env_is_refused() {
        let path = test_dir("claude-bad-env").join("settings.json");
        write_pretty(&path, &json!({"env": "not an object", "model": "opus[1m]"}));
        let error = patch_claude(&path, "http://127.0.0.1:18123")
            .expect_err("a non-object env must be refused");
        let chain = format!("{error:#}");
        assert!(
            chain.contains("\"env\"") && chain.contains("refusing"),
            "{chain}"
        );
        assert_eq!(
            read_value(&path),
            json!({"env": "not an object", "model": "opus[1m]"}),
            "the file is untouched"
        );
    }

    #[test]
    fn a_non_object_root_is_refused_whatever_the_frontend() {
        for (name, patch) in [
            (
                "claude",
                patch_claude as fn(&Path, &str) -> anyhow::Result<()>,
            ),
            (
                "opencode",
                patch_opencode as fn(&Path, &str) -> anyhow::Result<()>,
            ),
        ] {
            let path = test_dir("root-array").join(format!("{name}.json"));
            fs::write(&path, b"[1, 2, 3]\n").expect("write fixture");
            let error = patch(&path, "http://127.0.0.1:18123")
                .expect_err("a JSON array root must be refused");
            assert!(format!("{error:#}").contains("not an object"), "{name}");
            assert_eq!(fs::read(&path).expect("read back"), b"[1, 2, 3]\n");
        }
    }

    #[test]
    fn the_workhorse_patch_is_claude_s_patch_under_the_repo_root() {
        let repo = test_dir("workhorse");
        let settings = repo.join(".claude").join("settings.json");
        // The directory does not exist yet — the wizard offers this
        // patch when it does, but the patch itself must create the
        // chain; the fixture seeds it the same way.
        let original = claude_settings();
        fs::create_dir_all(settings.parent().expect("the .claude dir"))
            .expect("create the settings dir");
        write_pretty(&settings, &original);

        patch_claude_workhorse(&repo, "http://127.0.0.1:18123").expect("patch");

        let patched = read_value(&settings);
        assert_eq!(
            patched["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:18123"
        );
        assert_eq!(patched["model"], original["model"], "the rest preserved");

        // The enum constructor names the same file, and its patch
        // dispatches to the same patcher.
        let frontend = Frontend::claude_workhorse(&repo);
        assert_eq!(
            frontend,
            Frontend::ClaudeWorkhorse {
                settings: settings.clone()
            }
        );
        frontend
            .patch("http://127.0.0.1:19999")
            .expect("patch via the enum");
        assert_eq!(
            read_value(&settings)["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:19999"
        );
    }

    #[test]
    fn opencode_patch_changes_exactly_the_base_url() {
        let path = test_dir("opencode").join("opencode.json");
        let original = opencode_config();
        write_pretty(&path, &original);

        patch_opencode(&path, "http://127.0.0.1:18123/v1").expect("patch");

        let mut expected = original.clone();
        expected["provider"]["openrouter"]["options"]["baseURL"] =
            json!("http://127.0.0.1:18123/v1");
        let mut expected_bytes = serde_json::to_vec_pretty(&expected).expect("serialise");
        expected_bytes.push(b'\n');
        assert_eq!(
            fs::read(&path).expect("read back"),
            expected_bytes,
            "$schema, the permissions array, agents — all preserved byte-for-byte"
        );
    }

    #[test]
    fn opencode_patch_builds_the_provider_chain_when_absent() {
        let path = test_dir("opencode-empty").join("opencode.json");
        let original = json!({"$schema": "https://opencode.ai/config.json"});
        write_pretty(&path, &original);
        patch_opencode(&path, "http://127.0.0.1:18123/v1").expect("patch");
        let patched = read_value(&path);
        assert_eq!(
            patched["provider"]["openrouter"]["options"]["baseURL"],
            "http://127.0.0.1:18123/v1"
        );
        assert_eq!(patched["$schema"], "https://opencode.ai/config.json");
    }

    #[test]
    fn a_fresh_opencode_config_starts_from_scratch() {
        let path = test_dir("opencode-fresh").join("opencode.json");
        patch_opencode(&path, "http://127.0.0.1:18123/v1").expect("patch");
        assert_eq!(
            read_value(&path),
            json!({"provider": {"openrouter": {"options": {"baseURL": "http://127.0.0.1:18123/v1"}}}})
        );
    }

    #[test]
    fn opencode_with_a_non_object_provider_is_refused() {
        let path = test_dir("opencode-bad").join("opencode.json");
        write_pretty(&path, &json!({"provider": "openrouter", "agents": {}}));
        let error = patch_opencode(&path, "http://127.0.0.1:18123/v1")
            .expect_err("a non-object provider must be refused");
        let chain = format!("{error:#}");
        assert!(
            chain.contains("\"provider\"") && chain.contains("refusing"),
            "{chain}"
        );
        assert_eq!(
            read_value(&path),
            json!({"provider": "openrouter", "agents": {}}),
            "the file is untouched"
        );
    }

    #[test]
    fn the_base_url_shapes_are_the_hand_done_precedent() {
        assert_eq!(
            anthropic_base_url(18_123),
            "http://127.0.0.1:18123",
            "claude's env var: the bare listener, no /v1"
        );
        assert_eq!(
            openai_base_url(18_123),
            "http://127.0.0.1:18123/v1",
            "opencode's provider baseURL: with /v1"
        );
        let claude = Frontend::Claude {
            settings: PathBuf::from("/x"),
        };
        let workhorse = Frontend::claude_workhorse("/repos");
        let opencode = Frontend::Opencode {
            config: PathBuf::from("/y"),
        };
        let shell = Frontend::ShellRc {
            rc: PathBuf::from("/z"),
        };
        assert_eq!(claude.base_url(18_123), "http://127.0.0.1:18123");
        assert_eq!(workhorse.base_url(18_123), "http://127.0.0.1:18123");
        assert_eq!(shell.base_url(18_123), "http://127.0.0.1:18123");
        assert_eq!(opencode.base_url(18_123), "http://127.0.0.1:18123/v1");
    }

    #[test]
    fn the_enum_dispatches_and_describes() {
        let dir = test_dir("dispatch");
        let settings = dir.join("settings.json");
        let config = dir.join("opencode.json");
        let rc = dir.join("bashrc");
        write_pretty(&settings, &claude_settings());
        write_pretty(&config, &opencode_config());
        fs::write(&rc, "# unrelated\n").expect("write rc");

        Frontend::Claude {
            settings: settings.clone(),
        }
        .patch("http://127.0.0.1:18123")
        .expect("dispatch");
        assert_eq!(
            read_value(&settings)["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:18123"
        );

        Frontend::Opencode {
            config: config.clone(),
        }
        .patch("http://127.0.0.1:18123/v1")
        .expect("dispatch");
        assert_eq!(
            read_value(&config)["provider"]["openrouter"]["options"]["baseURL"],
            "http://127.0.0.1:18123/v1"
        );

        Frontend::ShellRc { rc: rc.clone() }
            .patch("http://127.0.0.1:18123")
            .expect("dispatch");
        assert_eq!(
            fs::read_to_string(&rc).expect("read rc"),
            "# unrelated\n# toker\nexport ANTHROPIC_BASE_URL=\"http://127.0.0.1:18123\"\n"
        );

        for (frontend, label) in [
            (
                Frontend::Claude {
                    settings: PathBuf::from("/x"),
                },
                "claude",
            ),
            (
                Frontend::ClaudeWorkhorse {
                    settings: PathBuf::from("/x"),
                },
                "Workhorse",
            ),
            (
                Frontend::Opencode {
                    config: PathBuf::from("/x"),
                },
                "opencode",
            ),
            (
                Frontend::ShellRc {
                    rc: PathBuf::from("/x"),
                },
                "shell rc",
            ),
        ] {
            assert!(
                frontend.describe().contains(label),
                "{} missing from {}",
                label,
                frontend.describe()
            );
        }
    }

    // ── the shell rc ───────────────────────────────────────────────────

    #[test]
    fn a_fresh_rc_gets_the_block() {
        let path = test_dir("rc-fresh").join("bashrc");
        patch_shell_rc(&path, "http://127.0.0.1:18123").expect("patch");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "# toker\nexport ANTHROPIC_BASE_URL=\"http://127.0.0.1:18123\"\n"
        );
    }

    #[test]
    fn an_empty_existing_rc_gets_the_block_without_a_stray_blank_line() {
        let path = test_dir("rc-empty").join("bashrc");
        fs::write(&path, "").expect("write empty");
        patch_shell_rc(&path, "http://127.0.0.1:18123").expect("patch");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "# toker\nexport ANTHROPIC_BASE_URL=\"http://127.0.0.1:18123\"\n"
        );
    }

    #[test]
    fn repatching_the_same_url_is_a_byte_identical_no_op() {
        let path = test_dir("rc-idempotent").join("bashrc");
        fs::write(&path, "export PATH=$HOME/bin:$PATH\n").expect("write rc");
        patch_shell_rc(&path, "http://127.0.0.1:18123").expect("patch");
        let once = fs::read_to_string(&path).expect("read");
        patch_shell_rc(&path, "http://127.0.0.1:18123").expect("re-patch");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            once,
            "no churn at all"
        );
        assert_eq!(once.matches("export ANTHROPIC_BASE_URL").count(), 1);
    }

    #[test]
    fn repatching_a_new_url_updates_the_line_in_place() {
        let path = test_dir("rc-update").join("bashrc");
        fs::write(
            &path,
            "alias x=y\n# toker\nexport ANTHROPIC_BASE_URL=\"http://old\"\nalias z=w\n",
        )
        .expect("write rc");
        patch_shell_rc(&path, "http://127.0.0.1:18123").expect("patch");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "alias x=y\n# toker\nexport ANTHROPIC_BASE_URL=\"http://127.0.0.1:18123\"\nalias z=w\n",
            "one line changed in place: same count, same position, neighbours untouched"
        );
    }

    #[test]
    fn a_block_is_appended_after_ensuring_the_file_ends_with_a_newline() {
        let path = test_dir("rc-append").join("bashrc");
        fs::write(&path, "export PATH=$HOME/bin:$PATH").expect("write rc, no trailing newline");
        patch_shell_rc(&path, "http://127.0.0.1:18123").expect("patch");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "export PATH=$HOME/bin:$PATH\n# toker\nexport ANTHROPIC_BASE_URL=\"http://127.0.0.1:18123\"\n"
        );
    }

    #[test]
    fn a_mangled_block_is_normalised_not_duplicated() {
        // The marker with the export moved (or duplicated) below it:
        // toker's governed exports collapse into one, where the first
        // one sat.
        let path = test_dir("rc-mangled").join("bashrc");
        fs::write(
            &path,
            "# toker\necho hello\nexport ANTHROPIC_BASE_URL=\"http://a\"\nexport ANTHROPIC_BASE_URL=\"http://b\"\n",
        )
        .expect("write rc");
        patch_shell_rc(&path, "http://127.0.0.1:18123").expect("patch");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "# toker\necho hello\nexport ANTHROPIC_BASE_URL=\"http://127.0.0.1:18123\"\n",
            "the first governed line is updated in place, the duplicate dropped"
        );
    }

    #[test]
    fn a_marker_without_its_export_gets_it_re_seated() {
        let path = test_dir("rc-reseat").join("bashrc");
        fs::write(&path, "# toker\nalias x=y\n").expect("write rc");
        patch_shell_rc(&path, "http://127.0.0.1:18123").expect("patch");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "# toker\nexport ANTHROPIC_BASE_URL=\"http://127.0.0.1:18123\"\nalias x=y\n"
        );
    }

    #[test]
    fn hand_made_exports_before_the_marker_are_never_touched() {
        // An ANTHROPIC_BASE_URL export with no marker is not toker's;
        // it stays byte-for-byte (and toker's block, later in the file,
        // is the assignment a sourced shell actually applies last).
        let path = test_dir("rc-foreign").join("bashrc");
        fs::write(
            &path,
            "export ANTHROPIC_BASE_URL=\"https://my-own.example\"\nexport OTHER=1\n",
        )
        .expect("write rc");
        patch_shell_rc(&path, "http://127.0.0.1:18123").expect("patch");
        let patched = fs::read_to_string(&path).expect("read");
        assert!(
            patched.starts_with(
                "export ANTHROPIC_BASE_URL=\"https://my-own.example\"\nexport OTHER=1\n"
            ),
            "the hand-made lines are untouched: {patched:?}"
        );
        assert!(
            patched.ends_with("# toker\nexport ANTHROPIC_BASE_URL=\"http://127.0.0.1:18123\"\n")
        );

        // A later re-run still updates only toker's line.
        patch_shell_rc(&path, "http://127.0.0.1:19999").expect("re-patch");
        let patched = fs::read_to_string(&path).expect("read");
        assert!(patched.contains("https://my-own.example"));
        assert_eq!(patched.matches("export ANTHROPIC_BASE_URL").count(), 2);
        assert!(patched.contains("export ANTHROPIC_BASE_URL=\"http://127.0.0.1:19999\""));
    }

    #[test]
    fn a_non_utf8_rc_is_an_error_not_a_clobber() {
        let path = test_dir("rc-utf8").join("bashrc");
        fs::write(&path, b"export A=\xff").expect("write non-utf8 rc");
        let error =
            patch_shell_rc(&path, "http://127.0.0.1:18123").expect_err("non-utf8 must be refused");
        assert!(format!("{error:#}").contains("reading"));
        assert_eq!(fs::read(&path).expect("read back"), b"export A=\xff");
    }
}
