//! `toker setup` — the interactive wizard (plan: "Setup wizard"): the
//! flow that composes the library half into the command.
//!
//! Every effect sits behind a seam, so the whole wizard runs as a
//! script in tests:
//!
//! - [`Prompt`] — the questions. [`InquirePrompt`] is the real UI;
//!   tests script the answers and keep a transcript of everything
//!   asked, which doubles as a pin on the prompt sequence.
//! - [`SystemRunner`] — systemctl and the unit-file installs.
//!   [`ProcessRunner`] runs the real things; tests inject a recording
//!   fake. No test ever issues a real `systemctl --user` call, read-only
//!   or otherwise: this machine's live `toker.socket` is enabled and
//!   proxying traffic while the suite runs, and the suite must neither
//!   act on it nor depend on it.
//! - [`Paths`] — every file the wizard touches. [`Paths::real`]
//!   derives them from HOME/XDG (the wizard reads its environment; it
//!   never mutates it); tests mirror the same shape under a scratch
//!   root, so a scripted run never reads or writes anything real.
//!
//! The flow is [`Step`]'s plan ([`plan`]), and the plan's module docs
//! are the WHY
//! — the ordering rule: bind the socket, verify it answers, THEN point
//! clients at it (a frontend's env hot-reloads into running sessions,
//! so a client pointed at a listener that is not up is a client with
//! its sessions killed). The wizard never deviates: a failed unit
//! install or a failed verify means the frontends are NOT touched.
//!
//! Idempotence ("re-run to change anything") holds at every step: the
//! config write is the library's read-merge-rewrite
//! ([`write_config`]), the unit install is declarative
//! (the same contents again), and each frontend patch updates in place
//! ([`patchers`]). A fully-wired machine re-run detects everything,
//! changes nothing, and says so.
//!
//! Credentials (invariant 2): a pasted key goes only into the 0600
//! `toker.toml` — never into a prompt message, never into the wizard's
//! output, never into a unit file. Tests pin the rule with a scripted
//! key and an output scan.
//!
//! Deferred on purpose: the wake/hold/ping timers (plan: "Sleep lock,
//! wake, ping") are a following unit — the wizard says they are not
//! yet available rather than stubbing a toggle that controls nothing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::config::{
    Config, DEFAULT_ANTHROPIC_API_KEY_ENV, DEFAULT_OPENROUTER_API_KEY_ENV, DEFAULT_PORT,
};
use crate::import::{self, ImportOpts};
use crate::setup::atomic::atomic_write_bytes;
use crate::setup::config_writer::write_config;
use crate::setup::patchers::{self, Frontend};
use crate::setup::plugin;
use crate::setup::verify::{self, ServiceReady};
use crate::setup::{Step, plan};

/// The enabled unit (the socket). The service is never enabled
/// directly — the socket starts it on demand.
pub const SOCKET_UNIT: &str = "toker.socket";

/// The socket-activated service unit.
pub const SERVICE_UNIT: &str = "toker.service";

/// How long [`Step::VerifyService`] waits for the listener to answer
/// both usage paths: a cold socket-activated start (SQLite open,
/// version probe) plus the upstream round-trip, with margin.
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

// ── the seams ──────────────────────────────────────────────────────────

/// The wizard's question seam. Production is inquire; tests script
/// the answers and record every question.
pub trait Prompt {
    /// Pick one of `options`; `default` is the index offered as the
    /// preselected answer.
    fn select(&mut self, message: &str, options: &[&str], default: Option<usize>) -> Result<usize>;

    /// A yes/no, with `default` as the suggested answer.
    fn confirm(&mut self, message: &str, default: bool) -> Result<bool>;

    /// A free-text answer. An empty answer means "take the default"
    /// when one is given (the wizard resolves empties itself, so the
    /// real UI and the fake agree). `secret: true` is key entry: the
    /// real UI renders a password prompt and never echoes the answer;
    /// the wizard stores it but never prints it (invariant 2).
    fn text(&mut self, message: &str, default: Option<&str>, secret: bool) -> Result<String>;
}

/// The real prompt UI: inquire (plan: Dependencies).
pub struct InquirePrompt;

impl Prompt for InquirePrompt {
    fn select(&mut self, message: &str, options: &[&str], default: Option<usize>) -> Result<usize> {
        let mut question = inquire::Select::new(message, options.to_vec());
        if let Some(default) = default {
            question = question.with_starting_cursor(default);
        }
        let answer = question.prompt()?;
        Ok(options
            .iter()
            .position(|option| *option == answer)
            .expect("inquire returns one of the offered options"))
    }

    fn confirm(&mut self, message: &str, default: bool) -> Result<bool> {
        Ok(inquire::Confirm::new(message)
            .with_default(default)
            .prompt()?)
    }

    fn text(&mut self, message: &str, default: Option<&str>, secret: bool) -> Result<String> {
        if secret {
            // A key is entered masked; inquire's Password takes no
            // default (an echoed default would be the key-echo the
            // rule forbids).
            return Ok(inquire::Password::new(message).prompt()?);
        }
        let mut question = inquire::Text::new(message);
        if let Some(default) = default {
            question = question.with_default(default);
        }
        Ok(question.prompt()?)
    }
}

/// The wizard's systemd seam: every effect on the machine's units.
/// A non-zero systemctl exit is `Ok(Output)` — the caller reads the
/// status and stderr; `Err` is "the command could not run at all".
pub trait SystemRunner {
    /// Run `systemctl --user <args>` to completion.
    fn systemctl_user(&self, args: &[&str]) -> Result<Output>;

    /// Install a unit file into the user units dir, returning the path
    /// written. Declarative: the same contents install cleanly over an
    /// earlier install of the same unit.
    fn install_unit(&self, name: &str, contents: &str) -> Result<PathBuf>;
}

/// The real runner: `systemctl --user` via std::process, unit files
/// written atomically beside whatever they replace (the same
/// temp-write/rename primitive every config patch uses). Constructed
/// with the units dir in `cmds::setup`; never exercised by tests.
pub struct ProcessRunner {
    units_dir: PathBuf,
}

impl ProcessRunner {
    pub fn new(units_dir: PathBuf) -> ProcessRunner {
        ProcessRunner { units_dir }
    }
}

impl SystemRunner for ProcessRunner {
    fn systemctl_user(&self, args: &[&str]) -> Result<Output> {
        std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .with_context(|| format!("running systemctl --user {}", args.join(" ")))
    }

    fn install_unit(&self, name: &str, contents: &str) -> Result<PathBuf> {
        let path = self.units_dir.join(name);
        let bytes = contents.as_bytes();
        atomic_write_bytes(&path, bytes, Some(0o644), |_temp, written| {
            anyhow::ensure!(
                written == bytes,
                "the written unit does not round-trip byte-for-byte — refusing to rename"
            );
            Ok(())
        })?;
        Ok(path)
    }
}

/// Every file the wizard touches, resolved once and passed in: the
/// real HOME/XDG-derived paths in production ([`Paths::real`]), a
/// mirrored scratch root in tests ([`Paths::scratch`]) so a scripted
/// run never reads or writes anything real. The wizard itself reads
/// no environment variables — everything location-shaped comes from
/// here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `toker.toml` (the config loader's own resolution, so
    /// `TOKER_CONFIG` overrides the wizard too).
    pub config_toml: PathBuf,
    /// The systemd user units dir.
    pub units_dir: PathBuf,
    /// claude's user settings.
    pub claude_settings: PathBuf,
    /// The Workhorse repos root — the wizard offers the repo-scoped
    /// claude patch only when this directory exists.
    pub workhorse_repos: PathBuf,
    /// opencode's config.
    pub opencode_config: PathBuf,
    /// The shell rc candidates, in offer order.
    pub shell_rcs: Vec<PathBuf>,
    /// The predecessor proxy's usage log — the `toker import` source,
    /// offered when it
    /// exists.
    pub ctp_usage: PathBuf,
    /// The codex CLI's shared login — `codex_sub` is offered as an
    /// anthropic backend only when it exists.
    pub codex_auth: PathBuf,
    /// The state dir (the ledger's parent): the only path the
    /// hardened service unit may write.
    pub state_dir: PathBuf,
    /// opencode's plugin discovery dir — the toker-cost install target
    /// (`…/plugins/toker-cost/`; the offer is opt-out, detection-tied).
    pub opencode_plugins_dir: PathBuf,
}

impl Paths {
    /// This user's real paths: `$XDG_CONFIG_HOME` (else `~/.config`),
    /// `$XDG_DATA_HOME` (else `~/.local/share`), `$HOME` for the rest,
    /// and the config loader's own `toker.toml` resolution.
    pub fn real() -> Result<Paths> {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .context("no home: set HOME")?;
        let home = PathBuf::from(home);
        let config_home = xdg_dir("XDG_CONFIG_HOME", &home, ".config");
        let data_home = xdg_dir("XDG_DATA_HOME", &home, ".local/share");
        Ok(Paths {
            config_toml: crate::config::config_path()?,
            units_dir: config_home.join("systemd").join("user"),
            claude_settings: home.join(".claude").join("settings.json"),
            workhorse_repos: home.join(".workhorse").join("repos"),
            opencode_config: config_home.join("opencode").join("opencode.json"),
            shell_rcs: vec![home.join(".bashrc"), home.join(".zshrc")],
            ctp_usage: data_home.join("claude-token-proxy").join("usage.jsonl"),
            codex_auth: home.join(".codex").join("auth.json"),
            state_dir: data_home.join("toker"),
            opencode_plugins_dir: config_home.join("opencode").join("plugins"),
        })
    }

    /// The same shape mirrored under a scratch `root` — the test
    /// fixture world.
    pub fn scratch(root: &Path) -> Paths {
        Paths {
            config_toml: root.join(".config/toker/toker.toml"),
            units_dir: root.join(".config/systemd/user"),
            claude_settings: root.join(".claude/settings.json"),
            workhorse_repos: root.join(".workhorse/repos"),
            opencode_config: root.join(".config/opencode/opencode.json"),
            shell_rcs: vec![root.join(".bashrc"), root.join(".zshrc")],
            ctp_usage: root.join(".local/share/claude-token-proxy/usage.jsonl"),
            codex_auth: root.join(".codex/auth.json"),
            state_dir: root.join(".local/share/toker"),
            opencode_plugins_dir: root.join(".config/opencode/plugins"),
        }
    }

    /// The ledger path under the state dir.
    pub fn db_path(&self) -> PathBuf {
        self.state_dir.join("toker.db")
    }
}

/// `$XDG_*` resolution shared by [`Paths::real`]: the env var when set
/// and non-empty, else `home/.fallback`.
fn xdg_dir(env: &str, home: &Path, fallback: &str) -> PathBuf {
    match std::env::var_os(env).filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home.join(fallback),
    }
}

// ── the unit templates ─────────────────────────────────────────────────

/// `toker.socket` for a toker port — the enabled unit: loopback
/// listener, one long-lived server handed the socket (toker holds
/// per-lane state across requests). The equivalent of this machine's
/// hand-installed unit, as a function of the configured port instead
/// of a hardcoded one.
pub fn socket_unit(port: u16) -> String {
    format!(
        r#"[Unit]
Description=toker proxy socket (local proxy + measurement for AI coding traffic)

[Socket]
# One long-lived server process is handed the listening socket, rather than one
# process per connection: toker holds per-lane state across requests.
ListenStream=127.0.0.1:{port}
Accept=no

[Install]
WantedBy=sockets.target
"#
    )
}

/// `toker.service` for this binary — socket-activated, hardened like
/// the hand-installed unit (the pattern the earlier ledger proxy
/// established), with
/// `ExecStart` a function of the binary that ran the wizard
/// (`std::env::current_exe`, resolved by the caller) rather than a
/// hardcoded path, and `ReadWritePaths` the configured state dir
/// rather than `%h` (the state dir follows `$XDG_DATA_HOME`, which
/// `%h` cannot express).
pub fn service_unit(exe: &Path, state_dir: &Path) -> String {
    format!(
        r#"[Unit]
Description=toker proxy service (socket-activated)
Requires=toker.socket
After=toker.socket

[Service]
# ExecStart is the binary that ran `toker setup`, resolved at setup
# time (std::env::current_exe). Re-run `toker setup` after moving it
# or installing a new one; a running service keeps serving the old
# inode until restarted.
Type=exec
ExecStart="{exe}" serve
Restart=always
RestartSec=1
SyslogIdentifier=toker

# Hardening, following the established hand-installed unit pattern.
# MemoryDenyWriteExecute is safe for a Rust binary. The state dir is
# the only writable path; credentials pass through in transit and are
# never written to disk by the proxy itself.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths={state_dir}
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=true
LockPersonality=true
MemoryDenyWriteExecute=true

# No [Install] section: this unit is never enabled directly. toker.socket is
# the enabled unit and starts this on demand.
"#,
        exe = exe.display(),
        state_dir = state_dir.display(),
    )
}

// ── detection ───────────────────────────────────────────────────────────

/// What one frontend's config file says about its base URL. The
/// wizard's read side; the write side is [`crate::setup::patchers`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum UrlRead {
    /// The file does not exist (workhorse's settings, before the
    /// first patch creates the chain).
    Missing,
    /// Present but unreadable/unparseable — a patch would refuse
    /// (never clobber), so it is reported, not papered over.
    Unparseable,
    /// Present, no base URL set.
    NoBase,
    /// Present, carrying this URL.
    Base(String),
}

/// The toker port a base URL points at, when it points at toker's
/// listener shape at all. Both of the wizard's own URL shapes count
/// (the patchers' hand-done precedent): the bare listener claude
/// takes and the `/v1` form opencode takes.
fn toker_port_of(url: &str) -> Option<u16> {
    let rest = url.strip_prefix("http://127.0.0.1:")?;
    let rest = rest.strip_suffix("/v1").unwrap_or(rest);
    rest.parse().ok()
}

/// Walk `keys` through the JSON at `path` and read the string there —
/// a frontend's base URL. Nested keys absent anywhere along the chain
/// read as [`UrlRead::NoBase`]; a non-string at the end likewise (a
/// base URL that is not a string is not a base URL).
fn read_json_url(path: &Path, keys: &[&str]) -> UrlRead {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return UrlRead::Missing,
        Err(_) => return UrlRead::Unparseable,
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return UrlRead::Unparseable;
    };
    let mut current = &value;
    for key in keys {
        match current.get(key) {
            Some(next) => current = next,
            None => return UrlRead::NoBase,
        }
    }
    match current.as_str() {
        Some(url) => UrlRead::Base(url.to_owned()),
        None => UrlRead::NoBase,
    }
}

/// The `ANTHROPIC_BASE_URL` export a shell rc carries (first match
/// anywhere in the file — it is the effective value a sourced shell
/// applies; whether toker's marker governs it only matters to the
/// patch, which never touches unmarked lines).
fn read_rc_url(path: &Path) -> UrlRead {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return UrlRead::Missing,
        Err(_) => return UrlRead::Unparseable,
    };
    let needle = format!("export {}=\"", patchers::SHELL_VAR);
    for line in text.lines() {
        if let Some(rest) = line.trim_start().strip_prefix(&needle)
            && let Some(url) = rest.strip_suffix('"')
        {
            return UrlRead::Base(url.to_owned());
        }
    }
    UrlRead::NoBase
}

/// Everything the detect step found. The state summary is printed
/// from this before any question is asked (plan: the wizard reports
/// the machine's state first), and the later steps re-use it — the
/// frontend list and URL reads drive the offer-and-skip logic.
#[derive(Debug)]
struct Detected {
    /// The existing `toker.toml`, parsed and validated — `None` when
    /// absent or unparseable (see [`Detected::config_error`]).
    config: Option<Config>,
    /// A present-but-broken config, rendered. The wizard stops before
    /// changing anything rather than run over a file it does not
    /// understand (the config writer's own refusal rule).
    config_error: Option<String>,
    /// Whether the existing file carried an explicit `db_path` — the
    /// wizard only writes its own default when it did not, so an
    /// operator's custom db path is never stomped.
    db_explicit: bool,
    /// `toker.socket`'s is-active verdict; `None` when the query
    /// itself failed (non-fatal — the summary says "unknown").
    socket_active: Option<bool>,
    /// The failed socket query's message, for the summary.
    socket_error: Option<String>,
    /// The DETECTED frontends, in the fixed offer order (claude,
    /// workhorse, opencode, shell rc).
    frontends: Vec<FrontendDetected>,
    /// The predecessor proxy's usage log exists — the import candidate.
    ctp_usage: bool,
    /// The codex CLI's login exists — `codex_sub` is offered when it
    /// does (there is nothing to configure; the login is shared).
    codex_auth: bool,
}

/// One detected frontend and what its file currently says.
#[derive(Debug)]
struct FrontendDetected {
    frontend: Frontend,
    url: UrlRead,
}

// ── the wizard's config choices ────────────────────────────────────────

/// How one api-key backend authenticates. An env name is the default
/// offer — the key never touches a file; a literal is the explicit
/// alternative, stored in the 0600 `toker.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyChoice {
    Env(String),
    Literal(String),
}

/// The backends/defaults/toggles the wizard asked about (the plan's
/// steps 1-2), applied to the config at [`Step::WriteConfig`].
#[derive(Debug, Clone)]
struct Choices {
    port: u16,
    anthropic_backend: String,
    anthropic_api_key: Option<KeyChoice>,
    openrouter: bool,
    openrouter_key: Option<KeyChoice>,
    awake: bool,
}

// ── the report ─────────────────────────────────────────────────────────

/// What one wizard run did — the finish summary's data, returned for
/// tests and any future caller. Holds no secrets by construction:
/// credential fields appear only as key *sources*, never values
/// (invariant 2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunReport {
    /// The port this run configured (the written or kept config's).
    pub port: u16,
    /// `toker.toml` was written this run.
    pub config_written: bool,
    /// `toker.toml` existed and the operator chose to keep it.
    pub config_kept: bool,
    /// The unit names installed this run (installs that failed are
    /// absent; the manual commands carry them).
    pub units_installed: Vec<String>,
    /// Every unit install and systemctl step succeeded.
    pub units_ok: bool,
    /// The commands to finish by hand when [`RunReport::units_ok`] is
    /// false.
    pub units_manual: Vec<String>,
    /// The verify verdicts, when the run reached and passed verify.
    pub verified: Option<ServiceReady>,
    /// The frontends pointed at toker this run (their `describe`
    /// strings).
    pub patched: Vec<String>,
    /// The frontends detected already wired (or declined) — left as
    /// they were.
    pub left_unchanged: Vec<String>,
    /// The frontends whose patch refused (the file shape was not
    /// understood; nothing was clobbered).
    pub patch_failed: Vec<String>,
    /// The predecessor log imported this run, if any.
    pub import_ran: Option<PathBuf>,
    /// The opencode plugin installed (or confirmed) this run — its
    /// target dir; `None` when opencode was not detected or the
    /// operator declined the (opt-out) offer.
    pub plugin_installed: Option<PathBuf>,
    /// The plugin was offered and declined.
    pub plugin_declined: bool,
}

// ── the wizard ──────────────────────────────────────────────────────────

/// The wizard itself: the seams plus the captured output. `run` walks
/// [`plan`]'s order exactly once and returns the report.
pub struct Wizard<'a> {
    prompt: &'a mut dyn Prompt,
    runner: &'a dyn SystemRunner,
    paths: &'a Paths,
    out: &'a mut dyn Write,
    verify_timeout: Duration,
}

impl<'a> Wizard<'a> {
    pub fn new(
        prompt: &'a mut dyn Prompt,
        runner: &'a dyn SystemRunner,
        paths: &'a Paths,
        out: &'a mut dyn Write,
        verify_timeout: Duration,
    ) -> Wizard<'a> {
        Wizard {
            prompt,
            runner,
            paths,
            out,
            verify_timeout,
        }
    }

    /// The flow, in [`plan`] order (the plan's docs are the WHY):
    /// detect and report, choose, write, install, verify, patch,
    /// toggles, done. The spine: a failed unit install skips verify
    /// and the frontends (reported, with the manual commands); a
    /// failed verify aborts the wizard (frontends NOT touched — the
    /// ordering rule's enforcement point).
    pub async fn run(mut self) -> Result<RunReport> {
        let detected = self.detect_state()?;
        if let Some(error) = &detected.config_error {
            bail!("the wizard stopped without changing anything: {error}");
        }
        let mut report = RunReport::default();

        let choices = self.choose_backends(&detected)?;
        let current = self.write_step(choices.as_ref(), &detected, &mut report)?;
        report.port = current.port;

        // A port change on a machine whose socket is already up: the
        // running socket keeps its old listener until it is restarted,
        // and `enable --now` is a no-op on an active unit — so verify
        // would fail against the new port with the old listener still
        // serving. The wizard deliberately never restarts a live
        // socket (a restart drops every live session through it), so
        // it says the one command that finishes the change instead.
        if report.config_written
            && detected.socket_active == Some(true)
            && detected
                .config
                .as_ref()
                .is_some_and(|old| old.port != current.port)
        {
            self.say("")?;
            self.say(&format!(
                "  note: the port changed ({} → {}) and a running {SOCKET_UNIT} keeps its \
                 old listener until restarted — `systemctl --user restart {SOCKET_UNIT}` \
                 picks the new one up (the wizard never restarts a live socket: that \
                 would drop every live session through it)",
                detected
                    .config
                    .as_ref()
                    .map(|old| old.port)
                    .expect("checked above"),
                current.port,
            ))?;
        }

        if self.units_step(current.port, &mut report)? {
            self.verify_step(current.port, &mut report).await?;
            self.frontends_step(&detected, current.port, &mut report)?;
            self.toggles_step(&detected, &current, &mut report)?;
        }
        self.finish(&report)?;
        Ok(report)
    }

    // ── output helpers ────────────────────────────────────────────

    /// One line to the wizard's output (stdout in production, the
    /// captured buffer in tests).
    fn say(&mut self, line: &str) -> Result<()> {
        writeln!(self.out, "{line}").context("writing the wizard's output")
    }

    /// A step's header: its position in the plan plus its label.
    fn step(&mut self, step: Step) -> Result<()> {
        let plan = plan();
        let n = plan.iter().position(|s| *s == step).expect("a plan step") + 1;
        self.say("")?;
        self.say(&format!("[{}/{}] {}", n, plan.len(), step.label()))
    }

    // ── detect & report ──────────────────────────────────────────

    /// Read the machine's state and print the summary — before any
    /// question is asked (the plan's wizard reports what exists
    /// first, so every answer is made knowing the state).
    fn detect_state(&mut self) -> Result<Detected> {
        let mut socket_active = None;
        let mut socket_error = None;
        match self.runner.systemctl_user(&["is-active", SOCKET_UNIT]) {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                socket_active = Some(stdout == "active");
            }
            // Non-fatal by design: an unqueryable systemd is a fresh
            // machine's normal state, and the socket's state is only
            // reported, never acted on here.
            Err(error) => socket_error = Some(format!("{error:#}")),
        }

        let (config, config_error, db_explicit) =
            match std::fs::read_to_string(&self.paths.config_toml) {
                Ok(text) => match Config::load_from(&self.paths.config_toml) {
                    Ok(config) => (Some(config), None, toml_has_db_path(&text)),
                    Err(error) => (None, Some(format!("{error:#}")), false),
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None, false),
                Err(error) => (
                    None,
                    Some(format!(
                        "reading {}: {error}",
                        self.paths.config_toml.display()
                    )),
                    false,
                ),
            };

        let mut frontends = Vec::new();
        if self.paths.claude_settings.exists() {
            let url = read_json_url(&self.paths.claude_settings, &["env", "ANTHROPIC_BASE_URL"]);
            frontends.push(FrontendDetected {
                frontend: Frontend::Claude {
                    settings: self.paths.claude_settings.clone(),
                },
                url,
            });
        }
        if self.paths.workhorse_repos.is_dir() {
            let settings = self
                .paths
                .workhorse_repos
                .join(".claude")
                .join("settings.json");
            let url = read_json_url(&settings, &["env", "ANTHROPIC_BASE_URL"]);
            frontends.push(FrontendDetected {
                frontend: Frontend::claude_workhorse(&self.paths.workhorse_repos),
                url,
            });
        }
        if self.paths.opencode_config.exists() {
            let url = read_json_url(
                &self.paths.opencode_config,
                &["provider", "openrouter", "options", "baseURL"],
            );
            frontends.push(FrontendDetected {
                frontend: Frontend::Opencode {
                    config: self.paths.opencode_config.clone(),
                },
                url,
            });
        }
        if let Some(rc) = self.paths.shell_rcs.iter().find(|rc| rc.exists()) {
            let url = read_rc_url(rc);
            frontends.push(FrontendDetected {
                frontend: Frontend::ShellRc { rc: rc.clone() },
                url,
            });
        }

        let detected = Detected {
            config,
            config_error,
            db_explicit,
            socket_active,
            socket_error,
            frontends,
            ctp_usage: self.paths.ctp_usage.exists(),
            codex_auth: self.paths.codex_auth.exists(),
        };
        self.state_summary(&detected)?;
        Ok(detected)
    }

    /// The state summary print (from [`Wizard::detect_state`], before
    /// any prompt): the config, the socket, each frontend (offered or
    /// not, and why), the codex login, the predecessor's usage log.
    fn state_summary(&mut self, detected: &Detected) -> Result<()> {
        self.say("toker setup — the state of this machine")?;
        let config_line = match (&detected.config, &detected.config_error) {
            (Some(config), _) => format!(
                "{} — port {}, anthropic → {}, openai_chat → {}, awake {}, db {}",
                self.paths.config_toml.display(),
                config.port,
                config.default_backend_anthropic,
                config.default_backend_openai_chat,
                on_off(config.awake),
                config.db_path.display(),
            ),
            (None, Some(error)) => {
                format!(
                    "{} — does not load: {error}",
                    self.paths.config_toml.display()
                )
            }
            (None, None) => "none (fresh setup)".to_owned(),
        };
        self.say(&format!("  config      : {config_line}"))?;
        match detected.socket_active {
            Some(true) => self.say(&format!("  systemd     : {SOCKET_UNIT} is active"))?,
            Some(false) => self.say(&format!("  systemd     : {SOCKET_UNIT} is not active"))?,
            None => self.say(&format!(
                "  systemd     : unknown (the query failed: {})",
                detected.socket_error.as_deref().unwrap_or("?")
            ))?,
        }

        self.say("  frontends:")?;
        self.frontend_lines(detected)?;

        self.say(&format!(
            "  codex login : {} — {}",
            self.paths.codex_auth.display(),
            if detected.codex_auth {
                "found (codex_sub is offered)"
            } else {
                "not found (codex_sub is not offered)"
            },
        ))?;
        match detected.ctp_usage {
            true => self.say(&format!(
                "  predecessor history : {} (an import candidate)",
                self.paths.ctp_usage.display()
            ))?,
            false => self.say("  predecessor history : none")?,
        }
        Ok(())
    }

    /// The frontend block of the state summary: each frontend's line,
    /// including the not-offered reasons (a frontend that is not
    /// detected is reported as such, not silently skipped).
    fn frontend_lines(&mut self, detected: &Detected) -> Result<()> {
        let claude = detected
            .frontends
            .iter()
            .find(|fd| matches!(fd.frontend, Frontend::Claude { .. }));
        match claude {
            Some(fd) => self.say(&format!("    {}", frontend_state(&fd.frontend, &fd.url)))?,
            None => self.say(&format!(
                "    claude    : no {} — not offered",
                self.paths.claude_settings.display()
            ))?,
        }
        let workhorse = detected
            .frontends
            .iter()
            .find(|fd| matches!(fd.frontend, Frontend::ClaudeWorkhorse { .. }));
        match workhorse {
            Some(fd) => self.say(&format!("    {}", frontend_state(&fd.frontend, &fd.url)))?,
            None => self.say(&format!(
                "    workhorse : no {} directory — not offered",
                self.paths.workhorse_repos.display()
            ))?,
        }
        let opencode = detected
            .frontends
            .iter()
            .find(|fd| matches!(fd.frontend, Frontend::Opencode { .. }));
        match opencode {
            Some(fd) => self.say(&format!("    {}", frontend_state(&fd.frontend, &fd.url)))?,
            None => self.say(&format!(
                "    opencode  : no {} — not offered",
                self.paths.opencode_config.display()
            ))?,
        }
        let shell = detected
            .frontends
            .iter()
            .find(|fd| matches!(fd.frontend, Frontend::ShellRc { .. }));
        match shell {
            Some(fd) => self.say(&format!("    {}", frontend_state(&fd.frontend, &fd.url)))?,
            None => {
                let candidates: Vec<String> = self
                    .paths
                    .shell_rcs
                    .iter()
                    .map(|rc| rc.display().to_string())
                    .collect();
                self.say(&format!(
                    "    shell rc  : none of {} exists — not offered",
                    candidates.join(", ")
                ))?
            }
        }
        Ok(())
    }

    // ── step 1: choose backends ───────────────────────────────────

    /// [`Step::ChooseBackends`] — the plan's "backends → defaults →
    /// toggles" questions: the anthropic protocol's backend, openrouter
    /// for openai-chat (and how each key is sourced), the awake
    /// toggle, and the port. An existing config is offered
    /// keep-vs-reconfigure first; `Ok(None)` means "keep" — the caller
    /// writes nothing. Every existing value preselects its own answer,
    /// so a re-run can be walked through with Enter.
    fn choose_backends(&mut self, detected: &Detected) -> Result<Option<Choices>> {
        self.step(Step::ChooseBackends)?;
        let existing = detected.config.as_ref();
        if existing.is_some() {
            let options = ["keep it as is", "reconfigure"];
            let keep = self.prompt.select(
                &format!(
                    "Found {} — what should the wizard do?",
                    self.paths.config_toml.display()
                ),
                &options,
                Some(0),
            )? == 0;
            if keep {
                return Ok(None);
            }
        }

        // The anthropic protocol's backend. anthropic_sub is the
        // default: claude brings its own credentials and there is
        // nothing to store. codex_sub is offered only when the CLI's
        // shared login exists (its auth is that login).
        let mut options = vec!["anthropic_sub", "anthropic_api"];
        if detected.codex_auth {
            options.push("codex_sub");
        }
        let current = existing
            .map(|config| config.default_backend_anthropic.as_str())
            .unwrap_or("anthropic_sub");
        let default = options.iter().position(|option| *option == current);
        let picked = options[self.prompt.select(
            "Default backend for the anthropic protocol (claude and friends)? \
             (anthropic_sub reuses claude's own OAuth — nothing to store; \
             codex_sub reuses the codex CLI's login)",
            &options,
            default,
        )?]
        .to_owned();
        let anthropic_api_key = if picked == "anthropic_api" {
            let default_env = existing
                .map(|config| config.anthropic_api.api_key_env.clone())
                .unwrap_or_else(|| DEFAULT_ANTHROPIC_API_KEY_ENV.to_owned());
            let existing_literal = existing.and_then(|config| config.anthropic_api.api_key.clone());
            Some(self.ask_api_key("anthropic_api", default_env, existing_literal)?)
        } else {
            None
        };
        if picked == "codex_sub" {
            self.say(
                "  note: the model routing that makes the translated route useful \
                 ([providers.codex_sub.model_map] in toker.toml) is a deliberate \
                 operator edit — none was written",
            )?;
        }

        // The openai-chat protocol's backend: openrouter is the only
        // one, so this is a yes/no, declined by leaving whatever the
        // config already says.
        let openrouter = self.prompt.confirm(
            "Use openrouter as the openai-chat backend (opencode)?",
            true,
        )?;
        let openrouter_key = if openrouter {
            let default_env = existing
                .map(|config| config.openrouter.api_key_env.clone())
                .unwrap_or_else(|| DEFAULT_OPENROUTER_API_KEY_ENV.to_owned());
            let existing_literal = existing.and_then(|config| config.openrouter.api_key.clone());
            Some(self.ask_api_key("openrouter", default_env, existing_literal)?)
        } else {
            None
        };

        let awake_default = existing.map(|config| config.awake).unwrap_or(true);
        let awake = self.prompt.confirm(
            "Hold an idle-sleep lock while agent sessions are live (awake)?",
            awake_default,
        )?;
        let port_default = existing.map(|config| config.port).unwrap_or(DEFAULT_PORT);
        let port = self.ask_port(port_default)?;

        Ok(Some(Choices {
            port,
            anthropic_backend: picked,
            anthropic_api_key,
            openrouter,
            openrouter_key,
            awake,
        }))
    }

    /// One api-key backend's source (plan: Credentials): an env var
    /// name — the default, the key never touching a file — or a
    /// literal into the 0600 `toker.toml`, the explicit alternative.
    /// The key itself is only ever the ANSWER (masked in the real UI);
    /// it appears in no message and no output.
    fn ask_api_key(
        &mut self,
        provider: &str,
        default_env: String,
        existing_literal: Option<String>,
    ) -> Result<KeyChoice> {
        let options = [
            "from an env var (recommended — the key never touches a file)",
            "a literal key in toker.toml (mode 0600)",
        ];
        if self.prompt.select(
            &format!("How should toker authenticate to {provider}?"),
            &options,
            Some(0),
        )? == 0
        {
            let answer = self.prompt.text(
                &format!("Env var holding the {provider} API key"),
                Some(&default_env),
                false,
            )?;
            Ok(KeyChoice::Env(if answer.is_empty() {
                default_env
            } else {
                answer
            }))
        } else {
            let hint = if existing_literal.is_some() {
                " (empty keeps the existing one)"
            } else {
                ""
            };
            for _ in 0..3 {
                let key = self.prompt.text(
                    &format!("The {provider} API key, stored in toker.toml at mode 0600{hint}"),
                    None,
                    true,
                )?;
                if key.is_empty() {
                    if let Some(key) = &existing_literal {
                        return Ok(KeyChoice::Literal(key.clone()));
                    }
                    continue;
                }
                return Ok(KeyChoice::Literal(key));
            }
            bail!("no {provider} key was given")
        }
    }

    /// The listener port (plan: "a new port, not 18082"): a text
    /// answer with the current-or-default value preselected; a
    /// non-number is complained about and re-asked, not fatal on the
    /// first typo.
    fn ask_port(&mut self, default: u16) -> Result<u16> {
        for _ in 0..3 {
            let answer = self
                .prompt
                .text("Listener port", Some(&default.to_string()), false)?;
            if answer.is_empty() {
                return Ok(default);
            }
            if let Ok(port) = answer.parse::<u16>() {
                return Ok(port);
            }
            self.say(&format!("  {answer:?} is not a port number — try again"))?;
        }
        bail!("no valid port was given")
    }

    // ── step 2: write toker.toml ──────────────────────────────────

    /// [`Step::WriteConfig`] — the library's read-merge-rewrite, then
    /// a summary of the resolved config. A kept config writes nothing
    /// (the operator chose to change nothing in it).
    fn write_step(
        &mut self,
        choices: Option<&Choices>,
        detected: &Detected,
        report: &mut RunReport,
    ) -> Result<Config> {
        self.step(Step::WriteConfig)?;
        let Some(choices) = choices else {
            let config = detected
                .config
                .clone()
                .expect("kept means an existing config");
            self.say(&format!(
                "kept {} as is (choose reconfigure to change anything in it)",
                self.paths.config_toml.display()
            ))?;
            report.config_kept = true;
            return Ok(config);
        };
        let db_explicit = detected.db_explicit;
        let mut written: Option<Config> = None;
        write_config(&self.paths.config_toml, |config| {
            apply_choices(config, choices, self.paths, db_explicit)?;
            written = Some(config.clone());
            Ok(())
        })?;
        let config = written.expect("the write ran");
        self.say(&format!("wrote {}", self.paths.config_toml.display()))?;
        self.say(&format!(
            "  port {}, awake {}",
            config.port,
            on_off(config.awake)
        ))?;
        self.say(&format!(
            "  anthropic → {}; openai_chat → {}",
            config.default_backend_anthropic, config.default_backend_openai_chat
        ))?;
        self.say(&key_line(
            "openrouter",
            &config.openrouter.api_key_env,
            &config.openrouter.api_key,
        ))?;
        if config.default_backend_anthropic == "anthropic_api" {
            self.say(&key_line(
                "anthropic_api",
                &config.anthropic_api.api_key_env,
                &config.anthropic_api.api_key,
            ))?;
        }
        report.config_written = true;
        Ok(config)
    }

    // ── step 3: install + start the units ─────────────────────────

    /// [`Step::InstallUnits`] — generate the units (functions of the
    /// configured port, this binary, and the state dir), install them,
    /// `daemon-reload`, `enable --now toker.socket`. Every failure is
    /// non-fatal and reported with the manual commands — but the
    /// return value is the spine: `false` means the run stops here
    /// (no verify, no frontends, no import).
    fn units_step(&mut self, port: u16, report: &mut RunReport) -> Result<bool> {
        self.step(Step::InstallUnits)?;
        let exe = std::env::current_exe().context("resolving the running binary's own path")?;
        let units = [
            (SOCKET_UNIT, socket_unit(port)),
            (SERVICE_UNIT, service_unit(&exe, &self.paths.state_dir)),
        ];

        let mut ok = true;
        let mut write_failures: Vec<String> = Vec::new();
        for (name, contents) in &units {
            match self.runner.install_unit(name, contents) {
                Ok(path) => {
                    self.say(&format!("installed {} ({})", name, path.display()))?;
                    report.units_installed.push((*name).to_owned());
                }
                Err(error) => {
                    ok = false;
                    self.say(&format!("installing {name} failed: {error:#}"))?;
                    self.say("  the unit contents, to place by hand:")?;
                    for line in contents.lines() {
                        self.say(&format!("    | {line}"))?;
                    }
                    write_failures.push(format!(
                        "write {name} into {}",
                        self.paths.units_dir.join(name).display()
                    ));
                }
            }
        }

        if ok {
            match self.runner.systemctl_user(&["daemon-reload"]) {
                Ok(output) if output.status.success() => {
                    self.say("systemctl --user daemon-reload — ok")?
                }
                other => {
                    ok = false;
                    self.say(&format!(
                        "systemctl --user daemon-reload failed: {}",
                        stderr_of(&other)
                    ))?;
                }
            }
        }
        if ok {
            match self
                .runner
                .systemctl_user(&["enable", "--now", SOCKET_UNIT])
            {
                Ok(output) if output.status.success() => {
                    self.say(&format!("systemctl --user enable --now {SOCKET_UNIT} — ok"))?
                }
                other => {
                    ok = false;
                    self.say(&format!(
                        "systemctl --user enable --now {SOCKET_UNIT} failed: {}",
                        stderr_of(&other)
                    ))?;
                }
            }
        }

        if !ok {
            report.units_manual = write_failures;
            report
                .units_manual
                .push("systemctl --user daemon-reload".to_owned());
            report
                .units_manual
                .push(format!("systemctl --user enable --now {SOCKET_UNIT}"));
            self.say(
                "the units did not come up cleanly — no frontend was touched, \
                 nothing was pointed at an unverified listener",
            )?;
            self.say("finish by hand, then re-run `toker setup`:")?;
            for command in &report.units_manual {
                self.say(&format!("  {command}"))?;
            }
        }
        report.units_ok = ok;
        Ok(ok)
    }

    // ── step 4: verify ────────────────────────────────────────────

    /// [`Step::VerifyService`] — the library's wiring check, the
    /// ordering rule's enforcement point. Failure aborts the wizard:
    /// the frontends are NOT touched, and the error says so.
    async fn verify_step(&mut self, port: u16, report: &mut RunReport) -> Result<()> {
        self.step(Step::VerifyService)?;
        match verify::await_service_ready(port, self.verify_timeout).await {
            Ok(ready) => {
                self.say(&format!(
                    "POST /v1/messages → {} (the upstream's verdict)",
                    ready.messages.expect("ready means both paths answered"),
                ))?;
                self.say(&format!(
                    "POST /v1/chat/completions → {}",
                    ready
                        .chat_completions
                        .expect("ready means both paths answered"),
                ))?;
                report.verified = Some(ready);
                Ok(())
            }
            Err(error) => {
                self.say("verification failed — the frontends were NOT touched:")?;
                self.say(&format!("{error:#}"))?;
                Err(error.context(
                    "the wizard stopped before the frontends step: \
                     no frontend config was changed",
                ))
            }
        }
    }

    // ── step 5: wire the frontends ────────────────────────────────

    /// [`Step::PatchFrontends`] — offer each DETECTED frontend, patch
    /// through the library, print exactly what changed in each file.
    /// A frontend already pointing at this run's port is offered
    /// "leave it as is?" (default yes — the idempotent re-run patches
    /// nothing); a patch that refuses (a file shape the library does
    /// not understand) is reported and skipped, never clobbered.
    fn frontends_step(
        &mut self,
        detected: &Detected,
        port: u16,
        report: &mut RunReport,
    ) -> Result<()> {
        self.step(Step::PatchFrontends)?;
        for detected in &detected.frontends {
            let frontend = &detected.frontend;
            let url = frontend.base_url(port);
            let name = frontend.describe();
            let wired =
                matches!(&detected.url, UrlRead::Base(found) if toker_port_of(found) == Some(port));
            let question = if wired {
                format!("{name} already points at toker — leave it as is?")
            } else {
                format!("Point {name} at toker ({url})?")
            };
            let answer = self.prompt.confirm(&question, true)?;
            if wired {
                if answer {
                    self.say(&format!("{name}: already wired, nothing to do"))?;
                    report.left_unchanged.push(name);
                } else {
                    // An explicit re-apply: the patch is idempotent, so
                    // this can only rewrite the same value in place.
                    self.patch_one(frontend, &url, &name, report)?;
                }
                continue;
            }
            if !answer {
                self.say(&format!("{name}: left as it was (declined)"))?;
                report.left_unchanged.push(format!("{name} (declined)"));
                continue;
            }
            self.patch_one(frontend, &url, &name, report)?;
        }
        if detected.frontends.is_empty() {
            self.say("no frontends detected — nothing to wire")?;
        }
        self.plugin_offer(detected, report)?;
        Ok(())
    }

    /// The opencode plugin offer (opt-out): rides the frontends step,
    /// offered only when opencode itself was detected. The bundled
    /// plugin is the repo's own `plugins/opencode/toker-cost/`,
    /// embedded at build time; a hand-edited install is never silently
    /// clobbered — a differing install asks explicitly.
    fn plugin_offer(&mut self, detected: &Detected, report: &mut RunReport) -> Result<()> {
        let Some(opencode) = detected
            .frontends
            .iter()
            .find(|fd| matches!(fd.frontend, Frontend::Opencode { .. }))
        else {
            return Ok(());
        };
        let _ = opencode;
        let plugins_dir = self.paths.opencode_plugins_dir.clone();
        let state = plugin::plugin_state(&plugins_dir);
        let target = plugin::plugin_target_dir(&plugins_dir);
        let (question, install) = match state {
            plugin::PluginState::Absent => (
                format!(
                    "install the opencode sidebar plugin ({})?",
                    target.display()
                ),
                true,
            ),
            plugin::PluginState::Installed => {
                self.say("opencode plugin: already installed, nothing to do")?;
                report.plugin_installed = Some(target);
                return Ok(());
            }
            plugin::PluginState::Different => (
                "the installed opencode plugin differs from this build —                  reinstall (overwrites the existing files)?"
                    .to_owned(),
                false,
            ),
        };
        if !self.prompt.confirm(&question, install)? {
            self.say("opencode plugin: declined")?;
            report.plugin_declined = true;
            return Ok(());
        }
        match plugin::install_plugin(&plugins_dir) {
            Ok(()) => {
                self.say(&format!(
                    "opencode plugin: installed to {}",
                    target.display()
                ))?;
                report.plugin_installed = Some(target);
            }
            Err(error) => {
                // Non-fatal: the dashboards still work; the plugin is
                // an enhancement, and the manual copy is one command.
                self.say(&format!(
                    "opencode plugin: install failed ({error:#}) — copy                      {}/plugins/opencode/toker-cost into {}",
                    "the toker checkout",
                    plugins_dir.display()
                ))?;
            }
        }
        Ok(())
    }

    /// Apply (or re-apply) one frontend patch and record the outcome.
    fn patch_one(
        &mut self,
        frontend: &Frontend,
        url: &str,
        name: &str,
        report: &mut RunReport,
    ) -> Result<()> {
        match frontend.patch(url) {
            Ok(()) => {
                self.say(&format!("{name}: {}", what_changed(frontend, url)))?;
                report.patched.push(name.to_owned());
            }
            Err(error) => {
                self.say(&format!("{name}: the patch refused: {error:#}"))?;
                report.patch_failed.push(name.to_owned());
            }
        }
        Ok(())
    }

    // ── the toggles ───────────────────────────────────────────────

    /// The plan's optional toggles, after the frontends: the awake
    /// report (asked at the backends step — it is config), the
    /// history import offer (when the source exists: the existing
    /// `toker
    /// import` logic, in-process, into the ledger the config names),
    /// and the deferred timers' note — the wake/hold/ping verbs are a
    /// following unit, so the wizard says they are not yet available
    /// rather than stubbing a toggle that controls nothing.
    fn toggles_step(
        &mut self,
        detected: &Detected,
        current: &Config,
        report: &mut RunReport,
    ) -> Result<()> {
        self.say("")?;
        self.say("optional toggles")?;
        self.say(&format!(
            "  awake (idle-sleep lock while sessions are live): {}",
            on_off(current.awake)
        ))?;
        self.say(
            "  wake timer, hold, ping windows: not yet available in toker \
             (a following unit)",
        )?;
        if !detected.ctp_usage {
            self.say("  predecessor history: none to import")?;
            return Ok(());
        }
        let question = format!(
            "Import the predecessor's usage history into the toker ledger now? (source: {})",
            self.paths.ctp_usage.display()
        );
        if !self.prompt.confirm(&question, true)? {
            self.say("  import: skipped")?;
            return Ok(());
        }
        let opts = ImportOpts {
            from: self.paths.ctp_usage.clone(),
            db: current.db_path.clone(),
            cost_kind: import::DEFAULT_COST_KIND,
            force: false,
            dry_run: false,
        };
        match import::run(opts) {
            Ok(()) => {
                // import::run prints its own report; the wizard adds
                // where it landed.
                self.say(&format!(
                    "  predecessor history imported into {}",
                    current.db_path.display()
                ))?;
                report.import_ran = Some(self.paths.ctp_usage.clone());
            }
            Err(error) => {
                self.say(&format!("  the import failed: {error:#}"))?;
                self.say(&format!(
                    "  by hand: toker import --from {}",
                    self.paths.ctp_usage.display()
                ))?;
            }
        }
        Ok(())
    }

    // ── step 6: done ──────────────────────────────────────────────

    /// The finish: everything done, the state files touched, and the
    /// re-run reminder (the plan's idempotence promise).
    fn finish(&mut self, report: &RunReport) -> Result<()> {
        self.step(Step::Done)?;
        let config = if report.config_written {
            format!("wrote {}", self.paths.config_toml.display())
        } else {
            format!("kept {} as is", self.paths.config_toml.display())
        };
        self.say(&format!("  config    : {config}"))?;
        let units = if report.units_ok {
            format!(
                "{}: {} installed; daemon-reload + enable --now ok",
                self.paths.units_dir.display(),
                report.units_installed.join(", ")
            )
        } else {
            format!(
                "{}: NOT up — finish by hand: {}",
                self.paths.units_dir.display(),
                report.units_manual.join(" && ")
            )
        };
        self.say(&format!("  units     : {units}"))?;
        let verified = match &report.verified {
            Some(ready) => format!(
                "answered ({}/{})",
                ready.messages.unwrap_or(0),
                ready.chat_completions.unwrap_or(0)
            ),
            None if !report.units_ok => "not attempted (the units failed)".to_owned(),
            None => "not reached".to_owned(),
        };
        self.say(&format!("  verified  : {verified}"))?;
        self.say(&format!(
            "  frontends : {} patched, {} left as is{}",
            report.patched.len(),
            report.left_unchanged.len(),
            if report.patch_failed.is_empty() {
                String::new()
            } else {
                format!(", {} REFUSED", report.patch_failed.len())
            },
        ))?;
        for name in &report.patched {
            self.say(&format!("    patched: {name}"))?;
        }
        for name in &report.left_unchanged {
            self.say(&format!("    as is : {name}"))?;
        }
        for name in &report.patch_failed {
            self.say(&format!("    refused: {name}"))?;
        }
        let ledger = match &report.import_ran {
            Some(from) => format!("predecessor history imported ({})", from.display()),
            None => "not imported".to_owned(),
        };
        self.say(&format!("  ledger    : {ledger}"))?;
        self.say("re-run `toker setup` any time to change anything.")?;
        Ok(())
    }
}

// ── the wizard's free helpers ───────────────────────────────────────────

/// Apply the wizard's choices to the config the config writer read
/// (its read-merge-rewrite preserves everything not changed here). A
/// chosen key source REPLACES the old one (picking the env var drops
/// a stale literal; picking a literal stores the given key), so the
/// file ends up saying exactly what was just answered. The db path is
/// written from the wizard's own paths only when the file did not
/// carry one — an operator's explicit db path is never stomped, and a
/// scratch-rooted run never pins the real machine's db into its
/// config.
fn apply_choices(
    config: &mut Config,
    choices: &Choices,
    paths: &Paths,
    db_explicit: bool,
) -> Result<()> {
    config.port = choices.port;
    config.default_backend_anthropic = choices.anthropic_backend.clone();
    if choices.anthropic_backend == "anthropic_api"
        && let Some(key) = &choices.anthropic_api_key
    {
        match key {
            KeyChoice::Env(name) => {
                config.anthropic_api.api_key_env = name.clone();
                config.anthropic_api.api_key = None;
            }
            KeyChoice::Literal(key) => config.anthropic_api.api_key = Some(key.clone()),
        }
    }
    if choices.openrouter {
        config.default_backend_openai_chat = "openrouter".to_owned();
        if let Some(key) = &choices.openrouter_key {
            match key {
                KeyChoice::Env(name) => {
                    config.openrouter.api_key_env = name.clone();
                    config.openrouter.api_key = None;
                }
                KeyChoice::Literal(key) => config.openrouter.api_key = Some(key.clone()),
            }
        }
    }
    config.awake = choices.awake;
    if !db_explicit {
        config.db_path = paths.db_path();
    }
    Ok(())
}

/// One frontend's state-summary line: what its file says about its
/// base URL, or why there is nothing to read yet.
fn frontend_state(frontend: &Frontend, url: &UrlRead) -> String {
    let what = match frontend {
        Frontend::Opencode { .. } => "baseURL",
        _ => "ANTHROPIC_BASE_URL",
    };
    match url {
        UrlRead::Missing => format!(
            "{}: no settings file yet — the patch would create it",
            frontend.describe()
        ),
        UrlRead::Unparseable => format!(
            "{}: does not parse — a patch would refuse, never clobber",
            frontend.describe()
        ),
        UrlRead::NoBase => format!("{}: no {what} set", frontend.describe()),
        UrlRead::Base(url) => match toker_port_of(url) {
            Some(port) => format!("{}: points at toker (port {port})", frontend.describe()),
            None => format!("{}: points at {url}", frontend.describe()),
        },
    }
}

/// Exactly what one frontend patch changed in its file — the line the
/// wizard prints per patch.
fn what_changed(frontend: &Frontend, url: &str) -> String {
    match frontend {
        Frontend::Claude { .. } | Frontend::ClaudeWorkhorse { .. } => {
            format!("env.{} = {url}", patchers::SHELL_VAR)
        }
        Frontend::Opencode { .. } => format!("provider.openrouter.options.baseURL = {url}"),
        Frontend::ShellRc { .. } => {
            format!("export {}=\"{url}\"", patchers::SHELL_VAR)
        }
    }
}

/// Whether the raw `toker.toml` text carries an explicit `db_path` —
/// the read side of the never-stomp rule in [`apply_choices`]. (The
/// resolved [`Config`] cannot answer this: an absent key and a
/// defaulted one look the same there.)
fn toml_has_db_path(text: &str) -> bool {
    toml::from_str::<toml::Table>(text)
        .map(|table| table.contains_key("db_path"))
        .unwrap_or(false)
}

/// A provider's key line for the config summary — sources only, never
/// values (invariant 2).
fn key_line(provider: &str, env: &str, literal: &Option<String>) -> String {
    match literal {
        Some(_) => format!(
            "  {provider} key: env {env} when set, else the literal in toker.toml (mode 0600)"
        ),
        None => format!("  {provider} key: env {env}"),
    }
}

/// "on"/"off" for the summary lines.
fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// A systemctl outcome's readable failure: stderr when there is any,
/// the status otherwise; "could not run" for an Err.
fn stderr_of(outcome: &Result<Output>) -> String {
    match outcome {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if stderr.is_empty() {
                format!("exit {}", output.status.code().unwrap_or(-1))
            } else {
                stderr
            }
        }
        Err(error) => format!("{error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::test_dir;
    use crate::store::Store;
    use axum::http::StatusCode;
    use axum::routing::post;
    use serde_json::{Value, json};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tokio::task::JoinHandle;

    // ── the scripted prompt ───────────────────────────────────────

    /// One scripted answer.
    #[derive(Debug, Clone)]
    enum Answer {
        Select(usize),
        Confirm(bool),
        Text(String),
    }

    fn select(index: usize) -> Answer {
        Answer::Select(index)
    }

    fn confirm(yes: bool) -> Answer {
        Answer::Confirm(yes)
    }

    fn text(answer: &str) -> Answer {
        Answer::Text(answer.to_owned())
    }

    /// Everything one prompt call asked.
    #[derive(Debug, Clone)]
    struct Asked {
        kind: &'static str,
        message: String,
        options: Vec<String>,
        secret: bool,
    }

    /// The fake prompt: scripted answers in order, every question
    /// recorded. Running out of answers panics — a test that scripts
    /// too few answers is a test that pinned the prompt sequence
    /// wrong, and the panic names the question that broke it.
    struct ScriptedPrompt {
        answers: VecDeque<Answer>,
        asked: Vec<Asked>,
    }

    impl ScriptedPrompt {
        fn new(answers: Vec<Answer>) -> ScriptedPrompt {
            ScriptedPrompt {
                answers: answers.into(),
                asked: Vec::new(),
            }
        }

        /// The transcript, for sequence and content assertions.
        fn asked(&self) -> &[Asked] {
            &self.asked
        }

        fn next(
            &mut self,
            kind: &'static str,
            message: &str,
            options: &[&str],
            secret: bool,
        ) -> Answer {
            self.asked.push(Asked {
                kind,
                message: message.to_owned(),
                options: options.iter().map(|option| option.to_string()).collect(),
                secret,
            });
            self.answers.pop_front().unwrap_or_else(|| {
                panic!("no scripted answer for {kind} {message:?} — script the run fully")
            })
        }
    }

    impl Prompt for ScriptedPrompt {
        fn select(
            &mut self,
            message: &str,
            options: &[&str],
            _default: Option<usize>,
        ) -> Result<usize> {
            match self.next("select", message, options, false) {
                Answer::Select(index) => Ok(index),
                other => panic!("select {message:?} was scripted {other:?}"),
            }
        }

        fn confirm(&mut self, message: &str, _default: bool) -> Result<bool> {
            match self.next("confirm", message, &[], false) {
                Answer::Confirm(yes) => Ok(yes),
                other => panic!("confirm {message:?} was scripted {other:?}"),
            }
        }

        fn text(&mut self, message: &str, _default: Option<&str>, secret: bool) -> Result<String> {
            match self.next("text", message, &[], secret) {
                Answer::Text(answer) => Ok(answer),
                other => panic!("text {message:?} was scripted {other:?}"),
            }
        }
    }

    // ── the recording fake runner ─────────────────────────────────

    /// The fake runner: scripted systemctl outcomes in call order,
    /// every call and every unit install recorded. Install writes the
    /// scratch units dir so a run's files exist like they would for
    /// real. NO systemctl is ever executed — the machine's live
    /// toker.socket is enabled and serving while these tests run.
    #[derive(Clone, Default)]
    struct FakeRunner {
        units_dir: PathBuf,
        outcomes: Arc<Mutex<VecDeque<Result<Output>>>>,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        installed: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl FakeRunner {
        fn new(units_dir: PathBuf, outcomes: Vec<Result<Output>>) -> FakeRunner {
            FakeRunner {
                units_dir,
                outcomes: Arc::new(Mutex::new(outcomes.into())),
                calls: Arc::new(Mutex::new(Vec::new())),
                installed: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }

        fn installed(&self) -> Vec<(String, String)> {
            self.installed.lock().unwrap().clone()
        }
    }

    impl SystemRunner for FakeRunner {
        fn systemctl_user(&self, args: &[&str]) -> Result<Output> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| arg.to_string()).collect());
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| panic!("no scripted systemctl outcome for {args:?}"))
        }

        fn install_unit(&self, name: &str, contents: &str) -> Result<PathBuf> {
            let path = self.units_dir.join(name);
            std::fs::create_dir_all(&self.units_dir).expect("create the scratch units dir");
            std::fs::write(&path, contents).expect("write the scratch unit");
            self.installed
                .lock()
                .unwrap()
                .push((name.to_owned(), contents.to_owned()));
            Ok(path)
        }
    }

    /// A systemctl outcome: success with stdout, or failure with
    /// stderr.
    fn outcome(success: bool, stdout: &str, stderr: &str) -> Result<Output> {
        use std::os::unix::process::ExitStatusExt;
        Ok(Output {
            status: std::process::ExitStatus::from_raw(if success { 0 } else { 1 }),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    fn ok_empty() -> Result<Output> {
        outcome(true, "", "")
    }

    fn active() -> Result<Output> {
        outcome(true, "active\n", "")
    }

    fn inactive() -> Result<Output> {
        outcome(true, "inactive\n", "")
    }

    fn reload_fails() -> Result<Output> {
        outcome(false, "", "Failed to connect to bus: No medium found\n")
    }

    // ── the scratch world ─────────────────────────────────────────

    /// The wizard rig: a scratch root, a scripted prompt, a recording
    /// runner, and the captured output.
    struct Rig {
        root: PathBuf,
        prompt: ScriptedPrompt,
        runner: FakeRunner,
        out: Vec<u8>,
    }

    impl Rig {
        fn new(name: &str, answers: Vec<Answer>, systemctl: Vec<Result<Output>>) -> Rig {
            Rig::at(test_dir(name), answers, systemctl)
        }

        fn at(root: PathBuf, answers: Vec<Answer>, systemctl: Vec<Result<Output>>) -> Rig {
            let units_dir = Paths::scratch(&root).units_dir;
            Rig {
                root,
                prompt: ScriptedPrompt::new(answers),
                runner: FakeRunner::new(units_dir, systemctl),
                out: Vec::new(),
            }
        }

        async fn run(&mut self, timeout: Duration) -> Result<RunReport> {
            let paths = Paths::scratch(&self.root);
            Wizard::new(
                &mut self.prompt,
                &self.runner,
                &paths,
                &mut self.out,
                timeout,
            )
            .run()
            .await
        }

        fn out(&self) -> String {
            String::from_utf8_lossy(&self.out).into_owned()
        }

        fn paths(&self) -> Paths {
            Paths::scratch(&self.root)
        }

        fn toml_path(&self) -> PathBuf {
            self.paths().config_toml
        }
    }

    /// A scratch upstream stand-in answering both usage paths with
    /// fixed verdicts — bound to an ephemeral loopback port, never the
    /// machine's live listener.
    async fn serve(messages: StatusCode, chat: StatusCode) -> (u16, JoinHandle<()>) {
        let app = axum::Router::new()
            .route("/v1/messages", post(move || async move { messages }))
            .route("/v1/chat/completions", post(move || async move { chat }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scratch");
        let port = listener.local_addr().expect("local addr").port();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("the scratch server serves");
        });
        (port, handle)
    }

    /// A loopback port with nothing on it.
    fn dropped_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    }

    // ── the frontend fixtures ─────────────────────────────────────

    /// claude's settings, the real machine's shape, unwired.
    fn claude_fixture() -> Value {
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                "ANTHROPIC_AUTH_TOKEN": "unused",
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "872000",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"
            },
            "permissions": {"defaultMode": "auto", "allow": ["Bash(agent-repo list)"]},
            "model": "opus[1m]",
            "spinnerVerbs": {"mode": "replace", "verbs": ["Working"]},
            "theme": "dark"
        })
    }

    /// opencode's config, the real machine's shape, unwired.
    fn opencode_fixture() -> Value {
        json!({
            "$schema": "https://opencode.ai/config.json",
            "permissions": [
                {"action": "read", "resource": "~/.cache/agent-repos/*", "effect": "allow"}
            ],
            "provider": {
                "openrouter": {"options": {"baseURL": "https://openrouter.ai/api/v1"}}
            },
            "agents": {"title": {"model": "openrouter/z-ai/glm-5.3-flash"}}
        })
    }

    fn write_pretty(path: &Path, value: &Value) -> Vec<u8> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the fixture dir");
        }
        let mut bytes = serde_json::to_vec_pretty(value).expect("serialise fixture");
        bytes.push(b'\n');
        std::fs::write(path, &bytes).expect("write fixture");
        bytes
    }

    /// The fixture with one nested string key set — the exact bytes a
    /// patch should leave on disk.
    fn with_key(mut value: Value, keys: &[&str], url: &str) -> Vec<u8> {
        let mut current = &mut value;
        for key in &keys[..keys.len() - 1] {
            current = current.get_mut(*key).expect("the fixture has the chain");
        }
        current[keys[keys.len() - 1]] = json!(url);
        let mut bytes = serde_json::to_vec_pretty(&value).expect("serialise expected");
        bytes.push(b'\n');
        bytes
    }

    fn seed_claude(root: &Path) -> Vec<u8> {
        write_pretty(
            &root.join(".claude").join("settings.json"),
            &claude_fixture(),
        )
    }

    fn seed_opencode(root: &Path) -> Vec<u8> {
        write_pretty(
            &root.join(".config").join("opencode").join("opencode.json"),
            &opencode_fixture(),
        )
    }

    fn seed_rc(root: &Path) {
        std::fs::write(root.join(".bashrc"), "export PATH=$HOME/bin:$PATH\n").expect("seed the rc");
    }

    fn claude_wired(port: u16) -> Vec<u8> {
        with_key(
            claude_fixture(),
            &["env", "ANTHROPIC_BASE_URL"],
            &patchers::anthropic_base_url(port),
        )
    }

    fn opencode_wired(port: u16) -> Vec<u8> {
        with_key(
            opencode_fixture(),
            &["provider", "openrouter", "options", "baseURL"],
            &patchers::openai_base_url(port),
        )
    }

    /// The real machine's hand-written `toker.toml`, shape and prose
    /// verbatim — the existing-config fixture (the config writer's
    /// own tests use the same text).
    const EXISTING_TOML: &str = r#"# toker — local config (this machine). Claude drives the codex sub
# through toker's translation; opencode drives openrouter unchanged.
# The family map (inherited from the predecessor): claude's model names → codex slugs.
default_backend_anthropic = "codex_sub"

[providers.codex_sub.model_map]
"family:opus" = "gpt-5.6-sol"
"family:fable" = "gpt-5.6-sol"
"family:sonnet" = "gpt-5.6-terra"
"family:haiku" = "gpt-5.6-luna"
"#;

    // ── the scripted answers ──────────────────────────────────────

    /// A full fresh-machine run: anthropic_sub, openrouter via env,
    /// awake on, the given port, and yes to every detected frontend
    /// (claude, [workhorse,] opencode, shell rc — the caller splices
    /// the workhorse confirm in where detection found it).
    fn answers_fresh(port: u16) -> Vec<Answer> {
        vec![
            select(0),               // anthropic backend: anthropic_sub
            confirm(true),           // openrouter on
            select(0),               // key source: env
            text(""),                // env name: keep the default
            confirm(true),           // awake
            text(&port.to_string()), // the listener port
            confirm(true),           // claude
            confirm(true),           // opencode
            confirm(true),           // shell rc
            confirm(true),           // the opencode plugin (opt-out, on)
        ]
    }

    /// The fresh script plus the workhorse confirm, between the
    /// claude and opencode confirms.
    fn answers_fresh_with_workhorse(port: u16) -> Vec<Answer> {
        let mut answers = answers_fresh(port);
        answers.insert(7, confirm(true));
        answers
    }

    /// The fresh script with openrouter's key stored as a literal.
    fn answers_literal_key(port: u16, key: &str) -> Vec<Answer> {
        let mut answers = answers_fresh(port);
        answers[2] = select(1); // key source: literal in toker.toml
        answers[3] = text(key); // the key itself, masked in the real UI
        answers
    }

    // ── the tests ──────────────────────────────────────────────────

    #[test]
    fn the_unit_templates_are_pinned() {
        // The socket: the hand-installed unit as a function of the
        // configured port (nothing machine-specific survives).
        assert_eq!(
            socket_unit(18_123),
            r#"[Unit]
Description=toker proxy socket (local proxy + measurement for AI coding traffic)

[Socket]
# One long-lived server process is handed the listening socket, rather than one
# process per connection: toker holds per-lane state across requests.
ListenStream=127.0.0.1:18123
Accept=no

[Install]
WantedBy=sockets.target
"#
        );
        assert!(
            socket_unit(20_000).contains("ListenStream=127.0.0.1:20000"),
            "the port substitutes"
        );

        // The service: the binary path and state dir substitute, and
        // every hardening line from the hand-installed unit survives.
        let service = service_unit(Path::new("/opt/toker/toker"), Path::new("/srv/state/toker"));
        assert!(service.contains("ExecStart=\"/opt/toker/toker\" serve"));
        for line in [
            "Requires=toker.socket",
            "After=toker.socket",
            "Type=exec",
            "Restart=always",
            "RestartSec=1",
            "SyslogIdentifier=toker",
            "NoNewPrivileges=true",
            "PrivateTmp=true",
            "ProtectSystem=strict",
            "ProtectHome=read-only",
            "ReadWritePaths=/srv/state/toker",
            "ProtectKernelTunables=true",
            "ProtectKernelModules=true",
            "ProtectControlGroups=true",
            "RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX",
            "RestrictNamespaces=true",
            "LockPersonality=true",
            "MemoryDenyWriteExecute=true",
        ] {
            assert!(service.contains(line), "{line} missing from:\n{service}");
        }
        assert!(
            !service.contains("18082") && !service.contains("18123"),
            "no hardcoded port in the service unit"
        );
    }

    #[tokio::test]
    async fn a_fresh_machine_gets_config_units_and_frontends() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "fresh",
            answers_fresh(port),
            // The socket query FAILS (no systemd reachable): non-fatal
            // by design, reported as "unknown" — the state summary
            // must say so, not stop.
            vec![
                Err(anyhow::anyhow!("systemctl: command not found")),
                ok_empty(),
                ok_empty(),
            ],
        );
        let unwired_claude = seed_claude(&rig.root);
        let unwired_opencode = seed_opencode(&rig.root);
        seed_rc(&rig.root);

        let report = rig
            .run(VERIFY_TIMEOUT)
            .await
            .expect("the fresh run completes");

        // The config: every choice applied, the db pinned under the
        // wizard's own root (the raw file had no db_path), 0600,
        // and no literal key.
        let text = std::fs::read_to_string(rig.toml_path()).expect("read the toml");
        assert!(text.contains(&format!("port = {port}")));
        assert!(text.contains("default_backend_anthropic = \"anthropic_sub\""));
        assert!(text.contains("default_backend_openai_chat = \"openrouter\""));
        assert!(text.contains("awake = true"));
        assert!(text.contains(&format!(
            "db_path = \"{}\"",
            rig.paths().db_path().display()
        )));
        assert!(text.contains("api_key_env = \"OPENROUTER_API_KEY\""));
        assert!(!text.contains("api_key ="), "no literal key was stored");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(rig.toml_path())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        // The opencode plugin: installed from the embedded set, byte
        // for byte, into the scratch plugins dir.
        let plugin_dir = rig.paths().opencode_plugins_dir.join("toker-cost");
        for (name, bytes) in plugin::PLUGIN_FILES {
            assert_eq!(
                std::fs::read(plugin_dir.join(name)).expect("the plugin file"),
                bytes.as_bytes(),
                "{name} installed byte-identically"
            );
        }
        assert!(report.plugin_installed.is_some());

        // The units: exactly the templates, as functions of this
        // binary and the scratch state dir; the systemctl calls in
        // the wizard's exact order.
        let exe = std::env::current_exe().expect("this test binary's path");
        assert_eq!(
            rig.runner.installed(),
            vec![
                (SOCKET_UNIT.to_owned(), socket_unit(port)),
                (
                    SERVICE_UNIT.to_owned(),
                    service_unit(&exe, &rig.paths().state_dir)
                ),
            ],
            "the units generated are functions of current_exe and the state dir"
        );
        assert_eq!(
            rig.runner.calls(),
            vec![
                vec!["is-active", SOCKET_UNIT],
                vec!["daemon-reload"],
                vec!["enable", "--now", SOCKET_UNIT],
            ]
            .into_iter()
            .map(|call| call.into_iter().map(str::to_owned).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        );

        // Verify passed with the upstream verdicts.
        assert_eq!(
            report.verified,
            Some(ServiceReady {
                messages: Some(401),
                chat_completions: Some(401),
            })
        );
        assert!(report.config_written);
        assert!(!report.config_kept);
        assert!(report.units_ok);
        assert_eq!(report.port, port);

        // The frontends: patched to this run's port, byte-for-byte.
        assert_eq!(
            std::fs::read(rig.root.join(".claude/settings.json")).expect("read claude"),
            claude_wired(port),
        );
        assert_eq!(
            std::fs::read(rig.root.join(".config/opencode/opencode.json")).expect("read opencode"),
            opencode_wired(port),
        );
        assert_eq!(
            std::fs::read_to_string(rig.root.join(".bashrc")).expect("read the rc"),
            format!(
                "export PATH=$HOME/bin:$PATH\n# toker\nexport ANTHROPIC_BASE_URL=\"http://127.0.0.1:{port}\"\n"
            )
        );
        assert_eq!(report.patched.len(), 3);
        assert_eq!(report.left_unchanged.len(), 0);

        // The output: the state summary said fresh/unknown, and every
        // step of the plan reported.
        let out = rig.out();
        assert!(out.contains("none (fresh setup)"), "the config line: {out}");
        assert!(out.contains("unknown"), "the failed socket query: {out}");
        assert!(out.contains("codex_sub is not offered"), "{out}");
        assert!(out.contains("[1/6] choose backends"), "{out}");
        assert!(out.contains("[2/6] write toker.toml"), "{out}");
        assert!(out.contains("[3/6] install + start the units"), "{out}");
        assert!(out.contains("[4/6] verify the service answers"), "{out}");
        assert!(out.contains("[5/6] wire the frontends"), "{out}");
        assert!(out.contains("[6/6] done"), "{out}");
        assert!(
            out.contains("not yet available in toker"),
            "the deferred timers' note: {out}"
        );

        // The fixture seeds were what the files said before the run —
        // i.e. the run really rewired them.
        assert_ne!(
            unwired_claude,
            std::fs::read(rig.root.join(".claude/settings.json")).expect("read claude")
        );
        assert_ne!(unwired_opencode, opencode_wired(port));

        // codex_sub was not offered (no login file): the backend
        // question carried exactly the two other options.
        assert_eq!(
            rig.prompt.asked()[0].options,
            ["anthropic_sub", "anthropic_api"],
        );
    }

    #[tokio::test]
    async fn a_rerun_detects_everything_and_changes_nothing() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        // Run one: the full fresh script.
        let root = test_dir("rerun");
        let mut rig = Rig::at(
            root.clone(),
            answers_fresh(port),
            vec![inactive(), ok_empty(), ok_empty()],
        );
        seed_claude(&root);
        seed_opencode(&root);
        seed_rc(&root);
        rig.run(VERIFY_TIMEOUT)
            .await
            .expect("the first run completes");

        let toml_once = std::fs::read(rig.toml_path()).expect("read the toml");
        let claude_once = std::fs::read(root.join(".claude/settings.json")).expect("read claude");
        let opencode_once =
            std::fs::read(root.join(".config/opencode/opencode.json")).expect("read opencode");
        let rc_once = std::fs::read(root.join(".bashrc")).expect("read the rc");
        let units_once = rig.runner.installed();

        // Run two, same root: keep the config, leave every frontend.
        let mut rerun = Rig::at(
            root.clone(),
            vec![select(0), confirm(true), confirm(true), confirm(true)],
            vec![active(), ok_empty(), ok_empty()],
        );
        let report = rerun
            .run(VERIFY_TIMEOUT)
            .await
            .expect("the re-run completes");

        // Nothing changed on disk: no config rewrite, no frontend
        // byte moved.
        assert_eq!(
            std::fs::read(rig.toml_path()).expect("read the toml"),
            toml_once,
            "a kept config is not even rewritten"
        );
        assert_eq!(
            std::fs::read(root.join(".claude/settings.json")).expect("read claude"),
            claude_once
        );
        assert_eq!(
            std::fs::read(root.join(".config/opencode/opencode.json")).expect("read opencode"),
            opencode_once
        );
        assert_eq!(
            std::fs::read(root.join(".bashrc")).expect("read the rc"),
            rc_once
        );

        // The units re-installed the same bytes (declarative).
        assert_eq!(rerun.runner.installed(), units_once);

        assert!(report.config_kept);
        assert!(!report.config_written);
        assert_eq!(report.patched.len(), 0);
        assert_eq!(report.left_unchanged.len(), 3);
        assert!(report.verified.is_some());
        assert!(report.units_ok);
        // The transcript pin: run two's first question was the
        // keep-vs-reconfigure select, and the frontend questions were
        // confirms.
        assert_eq!(rerun.prompt.asked()[0].kind, "select");
        assert!(
            rerun.prompt.asked()[1..]
                .iter()
                .all(|asked| asked.kind == "confirm")
        );

        // The re-run detected everything and said so.
        let out = rerun.out();
        assert!(out.contains("is active"), "{out}");
        assert!(out.contains("points at toker"), "{out}");
        assert!(out.contains("already wired, nothing to do"), "{out}");
        assert!(out.contains("kept"), "{out}");
        assert!(
            out.contains("0 patched, 3 left as is"),
            "the finish summary reports the no-op: {out}"
        );
    }

    #[tokio::test]
    async fn a_failed_verify_stops_the_wizard_before_the_frontends() {
        let port = dropped_port();
        let mut rig = Rig::new(
            "verify-fail",
            // The fresh script up to the port answer — the wizard
            // never reaches a frontend question.
            vec![
                select(0),
                confirm(true),
                select(0),
                text(""),
                confirm(true),
                text(&port.to_string()),
            ],
            vec![inactive(), ok_empty(), ok_empty()],
        );
        let before_claude = seed_claude(&rig.root);
        let before_opencode = seed_opencode(&rig.root);

        let error = rig
            .run(Duration::from_millis(300))
            .await
            .expect_err("nothing is listening on the dropped port");

        // The error says the ordering rule stopped the run: the
        // library's refusal, under the wizard's own words.
        let chain = format!("{error:#}");
        assert!(
            chain.contains("NOT being patched"),
            "the library's refusal is in the chain: {chain}"
        );
        assert!(
            chain.contains("the wizard stopped before the frontends step"),
            "the wizard's own words: {chain}"
        );
        assert!(rig.out().contains("the frontends were NOT touched"));

        // The frontends were untouched, byte-for-byte.
        assert_eq!(
            std::fs::read(rig.root.join(".claude/settings.json")).expect("read claude"),
            before_claude
        );
        assert_eq!(
            std::fs::read(rig.root.join(".config/opencode/opencode.json")).expect("read opencode"),
            before_opencode
        );

        // Everything BEFORE verify did happen (the steps are
        // sequential, not all-or-nothing): the config and the units.
        assert!(rig.toml_path().exists());
        assert_eq!(rig.runner.installed().len(), 2);
        // And nothing was asked past the port.
        assert_eq!(rig.prompt.asked().len(), 6);
    }

    #[tokio::test]
    async fn an_existing_config_can_be_reconfigured_without_losing_hand_edits() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "reconfigure",
            vec![
                select(1),               // reconfigure
                select(2),               // codex_sub — offered, the login exists
                confirm(false),          // openrouter declined
                confirm(true),           // awake
                text("not-a-port"),      // a bad port is re-asked, not fatal
                text(&port.to_string()), // the scratch port
                confirm(true),           // claude
            ],
            vec![active(), ok_empty(), ok_empty()],
        );
        std::fs::create_dir_all(rig.root.join(".config/toker")).expect("create the config dir");
        std::fs::write(rig.toml_path(), EXISTING_TOML).expect("seed the existing toml");
        std::fs::create_dir_all(rig.root.join(".codex")).expect("create the codex dir");
        std::fs::write(rig.root.join(".codex/auth.json"), "{}").expect("seed the codex login");
        seed_claude(&rig.root);

        let report = rig
            .run(VERIFY_TIMEOUT)
            .await
            .expect("the reconfigure run completes");

        // The reconfigured choices applied…
        let text = std::fs::read_to_string(rig.toml_path()).expect("read the toml");
        assert!(text.contains("default_backend_anthropic = \"codex_sub\""));
        assert!(text.contains(&format!("port = {port}")));
        assert!(text.contains("awake = true"));
        // …and the hand-set model map survived the read-merge-rewrite.
        assert!(text.contains("[providers.codex_sub.model_map]"));
        assert!(text.contains("\"family:opus\" = \"gpt-5.6-sol\""));
        assert!(text.contains("\"family:haiku\" = \"gpt-5.6-luna\""));

        // codex_sub WAS offered (the login exists) — and the note
        // about the deliberate model-map edit was printed.
        assert_eq!(
            rig.prompt.asked()[1].options,
            ["anthropic_sub", "anthropic_api", "codex_sub"],
        );
        assert!(
            rig.out().contains("providers.codex_sub.model_map"),
            "the codex_sub model-map note: {}",
            rig.out()
        );

        assert!(report.config_written);
        assert_eq!(report.patched.len(), 1, "only claude was detected");
        // The existing config (no port key → the default 18123) was
        // reconfigured onto the scratch port with the socket detected
        // ACTIVE — the wizard printed the restart note instead of
        // ever restarting the live socket itself.
        assert!(
            rig.out().contains("the port changed (18123 → "),
            "the port-change note: {}",
            rig.out()
        );
        assert!(
            rig.out()
                .contains("the wizard never restarts a live socket")
        );
        assert_eq!(
            std::fs::read(rig.root.join(".claude/settings.json")).expect("read claude"),
            claude_wired(port),
        );
    }

    #[tokio::test]
    async fn workhorse_is_offered_only_when_the_repos_dir_exists() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;

        // With the repos dir: offered (and, answered yes, patched) —
        // directory-scoped settings beat the user ones, so this is
        // the patch that actually wires Workhorse agents.
        let with_dir = test_dir("workhorse-with");
        std::fs::create_dir_all(with_dir.join(".workhorse/repos/.claude"))
            .expect("create the workhorse repos dir");
        write_pretty(
            &with_dir.join(".workhorse/repos/.claude/settings.json"),
            &claude_fixture(),
        );
        let mut with = Rig::at(
            with_dir.clone(),
            answers_fresh_with_workhorse(port),
            vec![inactive(), ok_empty(), ok_empty()],
        );
        let report = with
            .run(VERIFY_TIMEOUT)
            .await
            .expect("the with-workhorse run completes");
        assert_eq!(
            std::fs::read(with_dir.join(".workhorse/repos/.claude/settings.json"))
                .expect("read the workhorse settings"),
            claude_wired(port),
        );
        assert!(
            report.patched.iter().any(|name| name.contains("Workhorse")),
            "the workhorse patch is in the report: {:?}",
            report.patched
        );
        assert!(
            with.prompt
                .asked()
                .iter()
                .any(|asked| asked.message.contains("Workhorse")),
        );

        // Without the dir: never asked about, never patched.
        let mut without = Rig::new(
            "workhorse-without",
            answers_fresh(port),
            vec![inactive(), ok_empty(), ok_empty()],
        );
        seed_claude(&without.root);
        seed_opencode(&without.root);
        seed_rc(&without.root);
        let report = without
            .run(VERIFY_TIMEOUT)
            .await
            .expect("the without-workhorse run completes");
        assert!(
            !without
                .prompt
                .asked()
                .iter()
                .any(|asked| asked.message.contains("Workhorse")),
            "no workhorse question was asked: {:?}",
            without.prompt.asked()
        );
        assert!(!report.patched.iter().any(|name| name.contains("Workhorse")),);
        assert!(
            without
                .out()
                .contains(".workhorse/repos directory — not offered"),
            "{}",
            without.out()
        );
    }

    #[tokio::test]
    async fn an_existing_predecessor_log_is_offered_and_imported_in_process() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "import",
            // The fresh script without frontend confirms (nothing
            // detected), plus the import yes.
            vec![
                select(0),
                confirm(true),
                select(0),
                text(""),
                confirm(true),
                text(&port.to_string()),
                confirm(true), // import the predecessor's history
            ],
            vec![inactive(), ok_empty(), ok_empty()],
        );
        let ctp = rig.paths().ctp_usage;
        if let Some(parent) = ctp.parent() {
            std::fs::create_dir_all(parent).expect("create the predecessor dir");
        }
        std::fs::write(
            &ctp,
            concat!(
                r#"{"ts":"2026-10-01T10:00:00Z","model":"claude-opus-4-5","input":100,"output":50}"#,
                "\n",
            ),
        )
        .expect("seed the predecessor log");

        let report = rig
            .run(VERIFY_TIMEOUT)
            .await
            .expect("the run with an import completes");

        assert_eq!(report.import_ran, Some(ctp.clone()));
        assert!(rig.out().contains("imported into"), "{0}", rig.out());
        // The import landed in THIS run's ledger — the db pinned
        // under the wizard's own paths, never the real machine's.
        let db = rig.paths().db_path();
        assert!(db.exists(), "the scratch ledger exists");
        assert_eq!(
            Store::open(&db)
                .expect("open")
                .count_requests()
                .expect("count"),
            1
        );
        let text = std::fs::read_to_string(rig.toml_path()).expect("read the toml");
        assert!(
            text.contains(&format!("db_path = \"{}\"", db.display())),
            "the config names this run's ledger: {text}"
        );
    }

    #[tokio::test]
    async fn a_pasted_key_is_stored_but_never_echoed() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        const KEY: &str = "sk-or-v1-this-must-never-appear-in-output";
        let mut rig = Rig::new(
            "secret",
            answers_literal_key(port, KEY),
            vec![inactive(), ok_empty(), ok_empty()],
        );

        rig.run(VERIFY_TIMEOUT).await.expect("the run completes");

        // The key appears in NO output string the wizard produces…
        let out = rig.out();
        assert!(!out.contains(KEY), "the wizard's output:\n{out}");
        // …and in no question it asked (messages only, never answers).
        for asked in rig.prompt.asked() {
            assert!(
                !asked.message.contains(KEY),
                "the key leaked into a prompt: {:?}",
                asked.message
            );
        }
        // …and in no unit it installed.
        for (_name, contents) in rig.runner.installed() {
            assert!(!contents.contains(KEY), "the key leaked into a unit");
        }

        // The key IS stored — its one sanctioned home — in a 0600 file.
        let text = std::fs::read_to_string(rig.toml_path()).expect("read the toml");
        assert!(
            text.contains(&format!("api_key = \"{KEY}\"")),
            "the key is stored in the 0600 toker.toml: {text}"
        );
        // And it was asked as a SECRET question — the seam masks the
        // answer in the real UI, which is half of the never-echo rule.
        let key_question = rig
            .prompt
            .asked()
            .iter()
            .find(|asked| asked.secret)
            .expect("the key was asked as a secret question");
        assert!(
            key_question.message.contains("openrouter API key"),
            "the secret question names the provider, not the key"
        );
        assert_eq!(key_question.kind, "text");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(rig.toml_path())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        // And the summary named only the SOURCE, never the value.
        assert!(out.contains("the literal in toker.toml"), "{out}");
    }

    #[tokio::test]
    async fn a_failed_unit_install_reports_manual_commands_and_touches_no_frontend() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "units-fail",
            vec![
                select(0),
                confirm(true),
                select(0),
                text(""),
                confirm(true),
                text(&port.to_string()),
            ],
            // is-active fine; daemon-reload FAILS — so enable must
            // never be called (only two scripted outcomes remain
            // before the fake would panic).
            vec![inactive(), reload_fails()],
        );
        let before_claude = seed_claude(&rig.root);

        let report = rig
            .run(Duration::from_millis(300))
            .await
            .expect("a failed units step is reported, not an error");

        assert!(!report.units_ok);
        assert_eq!(report.units_installed.len(), 2, "the files installed fine");
        assert_eq!(report.verified, None, "verify was never attempted");
        assert_eq!(report.patched.len(), 0, "no frontend was touched");
        assert_eq!(
            std::fs::read(rig.root.join(".claude/settings.json")).expect("read claude"),
            before_claude
        );
        assert_eq!(
            rig.runner.calls(),
            vec![vec!["is-active", SOCKET_UNIT], vec!["daemon-reload"],]
                .into_iter()
                .map(|call| call.into_iter().map(str::to_owned).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            "enable was never called after the reload failed"
        );
        let out = rig.out();
        assert!(out.contains("no frontend was touched"), "{out}");
        assert!(
            out.contains(&format!("systemctl --user enable --now {SOCKET_UNIT}")),
            "the manual commands are printed: {out}"
        );
        assert!(out.contains("not attempted (the units failed)"), "{out}");
        // The verify step was never printed: the spine stopped early.
        assert!(!out.contains("verify the service answers"), "{out}");
    }

    #[tokio::test]
    async fn an_unparseable_config_stops_the_wizard_before_any_change() {
        let mut rig = Rig::new("broken", vec![], vec![inactive()]);
        std::fs::create_dir_all(rig.toml_path().parent().expect("the config dir"))
            .expect("create the config dir");
        std::fs::write(rig.toml_path(), "port = not-a-number\n").expect("seed the broken toml");

        let error = rig
            .run(VERIFY_TIMEOUT)
            .await
            .expect_err("the wizard refuses to run over a config it cannot read");

        let chain = format!("{error:#}");
        assert!(
            chain.contains("stopped without changing anything"),
            "{chain}"
        );
        assert!(
            chain.contains("toker.toml") && chain.contains("parsing"),
            "the refusal names the file: {chain}"
        );
        // Nothing was asked, nothing was written, nothing was started.
        assert_eq!(
            std::fs::read(rig.toml_path()).expect("read back"),
            b"port = not-a-number\n"
        );
        assert!(rig.prompt.asked().is_empty(), "no question was asked");
        assert!(rig.runner.installed().is_empty());
        assert_eq!(rig.runner.calls().len(), 1, "only the socket query ran");
        assert!(!rig.root.join(".claude").exists());
    }

    #[tokio::test]
    async fn the_opencode_plugin_is_offered_with_an_opt_out() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        // Declined: the offer was asked, nothing installed.
        let mut declined = Rig::new(
            "plugin-declined",
            {
                let mut answers = answers_fresh(port);
                answers[9] = confirm(false); // the opt-out
                answers
            },
            vec![inactive(), ok_empty(), ok_empty()],
        );
        seed_claude(&declined.root);
        seed_opencode(&declined.root);
        seed_rc(&declined.root);
        let report = declined
            .run(VERIFY_TIMEOUT)
            .await
            .expect("the declined run completes");
        assert!(
            declined
                .prompt
                .asked()
                .iter()
                .any(|asked| asked.message.contains("sidebar plugin")),
            "the offer was made: {:?}",
            declined.prompt.asked()
        );
        assert!(report.plugin_installed.is_none());
        assert!(report.plugin_declined);
        assert!(
            !declined
                .paths()
                .opencode_plugins_dir
                .join("toker-cost")
                .exists(),
            "nothing was installed"
        );

        // Accepted (the fresh-machine test already pins the happy
        // path); here: a DIFFERING install asks before overwriting,
        // and saying no leaves the files untouched.
        let mut guarded = Rig::new(
            "plugin-guarded",
            {
                let mut answers = answers_fresh(port);
                answers[9] = confirm(false); // the reinstall refusal
                answers
            },
            vec![inactive(), ok_empty(), ok_empty()],
        );
        seed_claude(&guarded.root);
        seed_opencode(&guarded.root);
        seed_rc(&guarded.root);
        let dir = guarded.paths().opencode_plugins_dir.join("toker-cost");
        std::fs::create_dir_all(&dir).expect("make the plugin dir");
        std::fs::write(dir.join("package.json"), "{\"name\":\"hand-tuned\"}").expect("a hand edit");
        let report = guarded
            .run(VERIFY_TIMEOUT)
            .await
            .expect("the guarded run completes");
        assert!(
            guarded
                .prompt
                .asked()
                .iter()
                .any(|asked| asked.message.contains("differs")),
            "the differing install asked before overwriting: {:?}",
            guarded.prompt.asked()
        );
        assert!(report.plugin_installed.is_none(), "the refusal held");
        assert_eq!(
            std::fs::read_to_string(dir.join("package.json")).expect("read back"),
            "{\"name\":\"hand-tuned\"}",
            "the hand edit survived"
        );
    }
}
