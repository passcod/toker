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
//! Credentials (invariant 2): a pasted key goes only into the OS keyring
//! ([`SecretStore`] — tests inject a map) or, when there is none, the
//! 0600 `toker.toml` — never into a prompt message, never into the
//! wizard's output, never into a unit file. Tests pin the rule with a
//! scripted key and an output scan.
//!
//! The wake/hold/ping timers (plan: "Sleep lock, wake, ping") are a
//! real offer in the toggles step: a yes/no defaulting to what is
//! installed, then a free-form `hh:mm` slot list (strictly validated;
//! the default is the installed slots, else [`DEFAULT_SLOTS`]), the hold
//! and per-slot ping USER units through the ordinary user-manager
//! path, and the wake SYSTEM timer with the do-nothing service it
//! starts — the only root-level pieces — staged into the units dir and
//! linked and enabled by path through
//! [`SystemRunner::systemctl_system`], which production runs as
//! `sudo systemctl` (it says so first: sudo will be asked). Every
//! timers failure is non-fatal with manual commands printed — a
//! machine that never sleeps-or-wakes still pings fine while up. A no
//! removes whatever an earlier run installed, and a changed slot list
//! retires the ping timers of the slots it dropped.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::config::{
    ANTHROPIC_BACKENDS, Config, DEFAULT_ANTHROPIC_API_KEY_ENV, DEFAULT_OPENROUTER_API_KEY_ENV,
    DEFAULT_PORT,
};
use crate::import::{self, ImportOpts};
use crate::secrets::SecretStore;
use crate::setup::atomic::atomic_write_bytes;
use crate::setup::config_writer::write_config;
use crate::setup::patchers::{self, Frontend};
use crate::setup::plugin;
use crate::setup::verify::{self, ServiceReady};
use crate::setup::{Step, plan};
use crate::timers::{self, HOLD_UNIT_FOR, PING_DELAY_MINUTES};

/// The enabled unit (the socket). The service is never enabled
/// directly — the socket starts it on demand.
pub const SOCKET_UNIT: &str = "toker.socket";

/// The socket-activated service unit.
pub const SERVICE_UNIT: &str = "toker.service";

/// The wake SYSTEM timer's name (plan: "Sleep lock, wake, ping") —
/// the only root-level piece. Staged into the user units dir (the one
/// place the wizard can write without root) and enabled by path into
/// the system manager through sudo; `WakeSystem=true` needs
/// `CAP_WAKE_ALARM`, which the user manager lacks.
pub const WAKE_TIMER_UNIT: &str = "toker-wake.timer";

/// The service the wake timer starts — staged and linked beside it.
/// A timer with no unit to activate is refused by systemd, so without
/// this the wake timer never armed at all: the first install shipped
/// the timer alone.
pub const WAKE_SERVICE_UNIT: &str = "toker-wake.service";

/// The hold USER timer's name (same name, timer + service pair).
pub const HOLD_TIMER_UNIT: &str = "toker-hold.timer";

/// The hold USER service's name.
pub const HOLD_SERVICE_UNIT: &str = "toker-hold.service";

/// The slots offered when no earlier run left any: the predecessor's
/// schedule. A slot is the wake and hold, and its ping fires
/// [`PING_DELAY_MINUTES`] later: 07:20's ping at 07:31 anchors its
/// window on floor(07:31, 10 min) = 07:30, ending 12:30, and 12:20's
/// at 12:31 anchors 12:30–17:30 (README: a window open for the
/// morning, re-opened after lunch).
pub const DEFAULT_SLOTS: &str = "07:20, 12:20";

/// The wake/hold/ping units an earlier run left in the units dir — the
/// wake pair is staged there too, so its presence stands for the
/// system install.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct InstalledTimers {
    hold: bool,
    /// The slots of the per-slot ping timers, `hh:mm`, sorted.
    ping_slots: Vec<String>,
    wake_timer: bool,
    wake_service: bool,
}

impl InstalledTimers {
    /// Read the units dir. Unreadable is none installed: the dir is
    /// created by the first install, so a fresh machine has none.
    fn read(units_dir: &Path) -> InstalledTimers {
        let mut found = InstalledTimers::default();
        let Ok(entries) = std::fs::read_dir(units_dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            match name {
                HOLD_TIMER_UNIT => found.hold = true,
                WAKE_TIMER_UNIT => found.wake_timer = true,
                WAKE_SERVICE_UNIT => found.wake_service = true,
                _ => {
                    // toker-ping-HHMM.timer, the stem's colon munged
                    // away (see ping_unit_stem).
                    let slot = name
                        .strip_prefix("toker-ping-")
                        .and_then(|rest| rest.strip_suffix(".timer"))
                        .filter(|hhmm| hhmm.len() == 4 && hhmm.is_char_boundary(2))
                        .map(|hhmm| format!("{}:{}", &hhmm[..2], &hhmm[2..]))
                        .filter(|slot| timers::parse_slot(slot).is_some());
                    if let Some(slot) = slot {
                        found.ping_slots.push(slot);
                    }
                }
            }
        }
        found.ping_slots.sort();
        found
    }

    fn any(&self) -> bool {
        self.hold || self.wake_timer || self.wake_service || !self.ping_slots.is_empty()
    }
}

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

    /// Pick any of `options`; `defaults` are the indices offered
    /// ticked. Returns the ticked indices, ascending.
    fn multi_select(
        &mut self,
        message: &str,
        options: &[&str],
        defaults: &[usize],
    ) -> Result<Vec<usize>>;

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

    fn multi_select(
        &mut self,
        message: &str,
        options: &[&str],
        defaults: &[usize],
    ) -> Result<Vec<usize>> {
        let answer = inquire::MultiSelect::new(message, options.to_vec())
            .with_default(defaults)
            .raw_prompt()?;
        let mut ticked: Vec<usize> = answer.iter().map(|option| option.index).collect();
        ticked.sort_unstable();
        Ok(ticked)
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

    /// Run `systemctl <args>` against the SYSTEM manager, as root —
    /// production goes through `sudo systemctl` (say so before
    /// calling: sudo will ask). The wake pair's link, enable and
    /// disable are the only calls the wizard makes here.
    fn systemctl_system(&self, args: &[&str]) -> Result<Output>;

    /// Install a unit file into the user units dir, returning the path
    /// written. Declarative: the same contents install cleanly over an
    /// earlier install of the same unit.
    fn install_unit(&self, name: &str, contents: &str) -> Result<PathBuf>;

    /// Remove a unit file from the user units dir. A file already gone
    /// is success: the goal is its absence.
    fn remove_unit(&self, name: &str) -> Result<()>;
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

    fn systemctl_system(&self, args: &[&str]) -> Result<Output> {
        // sudo, so the wizard says so before every call: the wake
        // pair is the only root-level thing toker touches.
        std::process::Command::new("sudo")
            .arg("systemctl")
            .args(args)
            .output()
            .with_context(|| format!("running sudo systemctl {}", args.join(" ")))
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

    fn remove_unit(&self, name: &str) -> Result<()> {
        let path = self.units_dir.join(name);
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        }
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

/// `toker-wake.timer` — the SYSTEM unit (plan: "Sleep lock, wake,
/// ping"): one `OnCalendar=` per user-chosen slot plus
/// `WakeSystem=true`, which wakes the machine **from suspend only**
/// (a powered-off machine stays off). Root-level on purpose: the user
/// manager lacks `CAP_WAKE_ALARM`, so this is the only unit toker
/// installs into the system manager. `toker wake-arm` is the
/// documented no-op pointing here. The schedule is **weekday-only**
/// (Mon–Fri): the pattern this serves is a work machine that wakes on
/// work days.
pub fn wake_system_unit(slots: &[String]) -> String {
    let mut on_calendar = String::new();
    for slot in slots {
        on_calendar.push_str(&format!("OnCalendar=Mon..Fri {slot}\n"));
    }
    format!(
        r#"[Unit]
Description=toker wake timer (wakes the machine at the chosen slots)

[Timer]
# The only root-level piece toker installs: WakeSystem=true needs
# CAP_WAKE_ALARM, which the user manager lacks, so this unit belongs to
# the system manager and `toker setup` enables it through sudo. Wakes
# from suspend only — a machine that is powered off stays off.
WakeSystem=true
{on_calendar}# A missed wake is not worth honouring late: the ping would refuse its
# slot anyway, and a wake at an arbitrary hour is what this avoids. To
# the second, because the default accuracy lets systemd fire up to a
# minute late, which eats into the hold's margin before the ping.
Persistent=false
AccuracySec=1s

[Install]
WantedBy=timers.target
"#
    )
}

/// `toker-wake.service` — what [`wake_system_unit`] activates (by the
/// shared name; no `Unit=` needed). It does nothing: the timer's
/// `WakeSystem=true` is the whole job, and staying awake for the ping
/// is the hold USER unit's, so the root-owned half has nothing in it to
/// audit and never needs the user's session bus. systemd still refuses
/// to start a timer whose unit does not exist, so it must be there.
pub fn wake_system_service() -> String {
    r#"[Unit]
Description=toker wake service (the wake timer's unit; does nothing itself)

[Service]
# The wake is the whole job and the timer's WakeSystem does it; holding
# the machine up afterwards is the toker-hold user unit's.
Type=oneshot
ExecStart=/bin/true
"#
    .to_owned()
}

/// `toker-hold.timer` + `toker-hold.service` — the hold USER units
/// (plan: "Sleep lock, wake, ping"): a timer at the wake slots running
/// the hold verb for the pinned 15 m. A timer that elapses while the
/// machine is suspended fires on resume, and the hold then keeps the
/// machine up those 15 minutes so the ping (11 m after the slot) can
/// fire; `Persistent=false` keeps a missed slot from re-running hours
/// late — the ping's own lateness guard refuses those anyway.
pub fn hold_user_units(slots: &[String], exe: &Path) -> (String, String) {
    let mut on_calendar = String::new();
    for slot in slots {
        on_calendar.push_str(&format!("OnCalendar=Mon..Fri {slot}\n"));
    }
    let timer = format!(
        r#"[Unit]
Description=toker hold timer (holds the idle-sleep lock {HOLD_UNIT_FOR} after each wake slot)

[Timer]
# At the wake slots themselves: a timer that elapses while the machine
# is suspended fires on resume, and the hold then keeps the machine up
# for its span so the ping ({PING_DELAY_MINUTES} m after the slot) can fire.
{on_calendar}
# A hold re-run hours after a missed slot would hold the machine up for
# nothing — the ping it protects never fires that late either.
Persistent=false
# To the second, beside the wake: systemd's default accuracy of a
# minute could start the hold after the machine has dozed off again.
AccuracySec=1s

[Install]
WantedBy=timers.target
"#,
    );
    let service = format!(
        r#"[Unit]
Description=toker hold service (holds the idle-sleep lock for its span)

[Service]
Type=oneshot
# The verb takes the lock independently of the daemon's own — both
# hold, both release on their own. The start timeout is off: a hold is
# exactly as long as its --for, and must not be killed at the 90 s
# default.
TimeoutStartSec=0
ExecStart="{exe}" hold --for={HOLD_UNIT_FOR}
"#,
        exe = exe.display(),
    );
    (timer, service)
}

/// One slot's ping pair — [`ping_user_units`] returns one per slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingUnits {
    /// The slot this pair serves, `hh:mm`.
    pub slot: String,
    /// The timer unit's name.
    pub timer_name: String,
    /// The timer unit's contents.
    pub timer: String,
    /// The service unit's name.
    pub service_name: String,
    /// The service unit's contents.
    pub service: String,
}

/// The ping USER units (plan: "Sleep lock, wake, ping") — **one
/// timer+service pair per slot**, the timer at slot+11 m and the
/// service running the ping verb **for that slot**. Per-slot because
/// the `--slot` argument must match its timer's clock time: a shared
/// service cannot know which slot fired. The slots must already be
/// validated `hh:mm` (the wizard validates before calling).
pub fn ping_user_units(slots: &[String], exe: &Path) -> Vec<PingUnits> {
    slots
        .iter()
        .filter_map(|slot| {
            let parsed = timers::parse_slot(slot)?;
            let fire = timers::slot_plus_minutes(&parsed, PING_DELAY_MINUTES).hhmm();
            // A wrapped fire time lands the day AFTER a weekday slot —
            // Friday 23:55's ping fires Saturday 00:06 — so its mask
            // covers the following days (Tue..Sat) rather than the
            // slot's own (Mon..Fri).
            let wrapped =
                (parsed.hour as i64 * 60 + parsed.minute as i64 + PING_DELAY_MINUTES) >= 24 * 60;
            let mask = if wrapped { "Tue..Sat" } else { "Mon..Fri" };
            let stem = ping_unit_stem(slot);
            let timer = format!(
                r#"[Unit]
Description=toker ping timer (opens the {slot} quota window, {PING_DELAY_MINUTES} m after the slot)

[Timer]
# {PING_DELAY_MINUTES} m after the slot: the machine has woken and settled, and
# the hold still has 4 m left to run. Persistent=false deliberately —
# a ping re-run hours late would open a mostly-spent window, and the
# verb's lateness guard refuses those anyway.
OnCalendar={mask} {fire}
Persistent=false
# The window anchors on the 10-minute grid, so the fire time is the
# boundary: systemd's default accuracy of a minute could push a ping
# across a grid line and move the boundary with it.
AccuracySec=10s

[Install]
WantedBy=timers.target
"#,
            );
            let service = format!(
                r#"[Unit]
Description=toker ping service (opens the {slot} quota window)

[Service]
Type=oneshot
# claude -p as a CLIENT of toker: the request lands on the ledger with
# ping: true — on-ledger, and never holding the sleep lock. User
# services start with a minimal environment, so the common user-local
# bin dirs ride along on PATH (TOKER_PING_CLAUDE names one that is
# not). The verb kills claude at 120 s itself; the start timeout is only
# a backstop above that and the readback, so a hung verb cannot
# suppress the next slot's ping.
Environment="PATH=%h/.local/bin:%h/.npm-global/bin:/usr/local/bin:/usr/bin:/bin"
TimeoutStartSec=3m
ExecStart="{exe}" ping-window --slot={slot}
"#,
                exe = exe.display(),
            );
            Some(PingUnits {
                slot: slot.clone(),
                timer_name: format!("{stem}.timer"),
                timer,
                service_name: format!("{stem}.service"),
                service,
            })
        })
        .collect()
}

/// The per-slot ping unit stem: `toker-ping-0900` for the 09:00 slot
/// (a unit name cannot carry the colon).
pub fn ping_unit_stem(slot: &str) -> String {
    format!("toker-ping-{}", slot.replace(':', ""))
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

/// The predecessor proxy's (claude-token-proxy's) listener port. A
/// frontend still wired to it reads as loopback in the same shape as
/// toker's, so without naming it the state summary called ctp's
/// listener toker's — toker's own port is the configured one
/// ([`DEFAULT_PORT`] unless changed), never this.
const CTP_PORT: u16 = 18_082;

/// The loopback port a base URL points at, when it is in one of the
/// wizard's own URL shapes at all (the patchers' hand-done precedent):
/// the bare listener claude takes and the `/v1` form opencode takes.
/// The port alone does not say whose listener it is; compare it with
/// the configured port before calling it toker's. The one place a
/// frontend base URL's shape is parsed, so a new shape is taught here.
fn loopback_port_of(url: &str) -> Option<u16> {
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

/// How one api-key backend authenticates (plan: Credentials).
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyChoice {
    /// Store nothing: the frontend sends its own key, which toker passes
    /// through (pass-through-when-present).
    Frontend,
    /// A key given to toker, before it is placed: the keyring when there
    /// is one, else the 0600 `toker.toml` ([`Wizard::place_keys`]).
    Store(String),
    /// "Give it to toker" with an empty answer over a key toker already
    /// stores: keep it where it is.
    KeepStored,
    /// Placed in the keyring.
    Keyring,
    /// Placed in the 0600 `toker.toml`: no secret service was available.
    Literal(String),
    /// An env var, which must be in the systemd user manager's
    /// environment (the service never sees the shell's).
    Env(String),
}

/// The backends/defaults/toggles the wizard asked about (the plan's
/// steps 1-2), applied to the config at [`Step::WriteConfig`].
#[derive(Debug, Clone)]
struct Choices {
    port: u16,
    /// The ticked backends, in [`BACKENDS`] order: exactly the set the
    /// written config enables.
    backends: Vec<&'static str>,
    /// The anthropic protocol's default, among the ticked; `None` when
    /// no anthropic backend is ticked.
    anthropic_default: Option<&'static str>,
    /// Asked only when anthropic_api is ticked.
    anthropic_api_key: Option<KeyChoice>,
    /// Asked only when openrouter is ticked.
    openrouter_key: Option<KeyChoice>,
    awake: bool,
}

/// Every backend the wizard offers, in the multi-select's order, with
/// the line that says what it is.
const BACKENDS: &[(&str, &str)] = &[
    (
        "anthropic_sub",
        "anthropic_sub — the Claude subscription (claude's own login, nothing to store)",
    ),
    (
        "anthropic_api",
        "anthropic_api — the Anthropic API, by API key",
    ),
    (
        "codex_sub",
        "codex_sub — the ChatGPT subscription, through the codex CLI's login",
    ),
    (
        "openrouter",
        "openrouter — the openai-chat protocol (opencode), by API key",
    ),
];

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
    /// The state dir was created (or already existed), and every unit
    /// install and systemctl step succeeded.
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
    /// The wake/hold/ping slots chosen this run (plan: "Sleep lock,
    /// wake, ping"); empty means the timers were declined, in which
    /// case nothing was installed and any an earlier run left were
    /// removed ([`RunReport::timers_removed`]).
    pub timer_slots: Vec<String>,
    /// The timer units disabled and removed this run: all of them when
    /// the timers were declined, else the ping pairs of slots no longer
    /// chosen.
    pub timers_removed: Vec<String>,
    /// The timer units installed this run: the hold pair, the per-slot
    /// ping pairs, and the staged wake system timer.
    pub timers_installed: Vec<String>,
    /// Every user-timer install, daemon-reload and enable succeeded.
    pub timers_ok: bool,
    /// Manual commands for the user timers when [`RunReport::timers_ok`]
    /// is false.
    pub timers_manual: Vec<String>,
    /// The wake SYSTEM timer was enabled through sudo this run.
    pub wake_enabled: bool,
    /// Manual commands for the wake timer when not enabled.
    pub wake_manual: Vec<String>,
}

// ── the wizard ──────────────────────────────────────────────────────────

/// The wizard itself: the seams plus the captured output. `run` walks
/// [`plan`]'s order exactly once and returns the report.
pub struct Wizard<'a> {
    prompt: &'a mut dyn Prompt,
    runner: &'a dyn SystemRunner,
    /// Where a key given to toker is kept ([`Wizard::place_keys`]).
    secrets: &'a dyn SecretStore,
    paths: &'a Paths,
    out: &'a mut dyn Write,
    verify_timeout: Duration,
}

impl<'a> Wizard<'a> {
    pub fn new(
        prompt: &'a mut dyn Prompt,
        runner: &'a dyn SystemRunner,
        secrets: &'a dyn SecretStore,
        paths: &'a Paths,
        out: &'a mut dyn Write,
        verify_timeout: Duration,
    ) -> Wizard<'a> {
        Wizard {
            prompt,
            runner,
            secrets,
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
                "{} — port {}, backends {}, anthropic → {}, openai_chat → {}, awake {}, db {}",
                self.paths.config_toml.display(),
                config.port,
                backends_list(config),
                config
                    .default_backend_anthropic
                    .as_deref()
                    .unwrap_or("none"),
                config
                    .default_backend_openai_chat
                    .as_deref()
                    .unwrap_or("none"),
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
                "found (codex_sub is pre-ticked)"
            } else {
                "not found"
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
        let toker_port = detected
            .config
            .as_ref()
            .map(|config| config.port)
            .unwrap_or(DEFAULT_PORT);
        let claude = detected
            .frontends
            .iter()
            .find(|fd| matches!(fd.frontend, Frontend::Claude { .. }));
        match claude {
            Some(fd) => self.say(&format!(
                "    {}",
                frontend_state(&fd.frontend, &fd.url, toker_port)
            ))?,
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
            Some(fd) => self.say(&format!(
                "    {}",
                frontend_state(&fd.frontend, &fd.url, toker_port)
            ))?,
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
            Some(fd) => self.say(&format!(
                "    {}",
                frontend_state(&fd.frontend, &fd.url, toker_port)
            ))?,
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
            Some(fd) => self.say(&format!(
                "    {}",
                frontend_state(&fd.frontend, &fd.url, toker_port)
            ))?,
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
    /// toggles" questions: which backends toker serves (one
    /// multi-select, pre-ticked from what the machine has), the auth of
    /// each ticked key backend, the default per protocol among the
    /// ticked ones (asked only when there is a choice), the awake toggle,
    /// and the port. An existing config is offered keep-vs-reconfigure
    /// first; `Ok(None)` means "keep" — the caller writes nothing. Every
    /// existing value preselects its own answer, so a re-run can be
    /// walked through with Enter.
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

        let backends = self.ask_backends(detected)?;
        let ticked = |name: &str| backends.contains(&name);

        let anthropic_api_key = if ticked("anthropic_api") {
            let api = existing.and_then(|config| config.anthropic_api.as_ref());
            Some(self.ask_api_key(
                "anthropic_api",
                DEFAULT_ANTHROPIC_API_KEY_ENV,
                api.map(|api| {
                    (
                        api.api_key_env.as_str(),
                        api.api_key_keyring,
                        api.api_key.is_some(),
                    )
                }),
            )?)
        } else {
            None
        };
        if ticked("codex_sub") {
            if !detected.codex_auth {
                self.say(&format!(
                    "  note: there is no codex login at {} yet — `codex login` creates it, \
                     and codex_sub answers 401s until then",
                    self.paths.codex_auth.display()
                ))?;
            }
            self.say(
                "  note: the model routing that makes the translated route useful \
                 ([providers.codex_sub.model_map] in toker.toml) is a deliberate \
                 operator edit — the wizard writes none",
            )?;
        }
        let openrouter_key = if ticked("openrouter") {
            let openrouter = existing.and_then(|config| config.openrouter.as_ref());
            Some(self.ask_api_key(
                "openrouter",
                DEFAULT_OPENROUTER_API_KEY_ENV,
                openrouter.map(|openrouter| {
                    (
                        openrouter.api_key_env.as_str(),
                        openrouter.api_key_keyring,
                        openrouter.api_key.is_some(),
                    )
                }),
            )?)
        } else {
            None
        };

        // The anthropic protocol's default, among the ticked anthropic
        // backends. The openai protocol has one backend, so it never
        // asks.
        let candidates: Vec<&'static str> = ANTHROPIC_BACKENDS
            .iter()
            .copied()
            .filter(|name| ticked(name))
            .collect();
        let anthropic_default = match candidates.as_slice() {
            [] => None,
            [only] => Some(*only),
            several => {
                let current = existing
                    .and_then(|config| config.default_backend_anthropic.as_deref())
                    .unwrap_or(ANTHROPIC_BACKENDS[0]);
                let default = several.iter().position(|name| *name == current);
                Some(
                    several[self.prompt.select(
                        "Default backend for the anthropic protocol (claude and friends)? \
                     (bare model names go here; a provider/ prefix picks another per request)",
                        several,
                        default.or(Some(0)),
                    )?],
                )
            }
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
            backends,
            anthropic_default,
            anthropic_api_key,
            openrouter_key,
            awake,
        }))
    }

    /// The backend multi-select. Pre-ticked from the existing config's
    /// enabled set when there is one, else from what the machine has: a
    /// claude settings file or the Workhorse repos ticks anthropic_sub,
    /// the codex CLI's login ticks codex_sub, an opencode config ticks
    /// openrouter. An empty answer is asked again: a toker with no
    /// backend answers every request with a not-configured error.
    fn ask_backends(&mut self, detected: &Detected) -> Result<Vec<&'static str>> {
        let ticked_before: Vec<&str> = match &detected.config {
            Some(config) => config.enabled_backends(),
            None => {
                let has = |pick: fn(&Frontend) -> bool| {
                    detected.frontends.iter().any(|fd| pick(&fd.frontend))
                };
                let mut found = Vec::new();
                if has(|frontend| {
                    matches!(
                        frontend,
                        Frontend::Claude { .. } | Frontend::ClaudeWorkhorse { .. }
                    )
                }) {
                    found.push("anthropic_sub");
                }
                if detected.codex_auth {
                    found.push("codex_sub");
                }
                if has(|frontend| matches!(frontend, Frontend::Opencode { .. })) {
                    found.push("openrouter");
                }
                found
            }
        };
        let defaults: Vec<usize> = BACKENDS
            .iter()
            .enumerate()
            .filter(|(_, (name, _))| ticked_before.contains(name))
            .map(|(index, _)| index)
            .collect();
        let labels: Vec<&str> = BACKENDS.iter().map(|(_, label)| *label).collect();
        for _ in 0..3 {
            let picked = self.prompt.multi_select(
                "Which backends should toker serve? (space toggles, enter accepts)",
                &labels,
                &defaults,
            )?;
            if picked.is_empty() {
                self.say("  pick at least one backend — try again")?;
                continue;
            }
            return Ok(picked.into_iter().map(|index| BACKENDS[index].0).collect());
        }
        bail!("no backend was picked")
    }

    /// One api-key backend's source (plan: Credentials), in this order:
    /// the frontend brings its own (toker stores nothing), give it to
    /// toker (the OS keyring, else the 0600 `toker.toml`), or an env var
    /// in the systemd user environment. The existing config's choice is
    /// preselected. The key itself is only ever the ANSWER (masked in the
    /// real UI); it appears in no message and no output.
    fn ask_api_key(
        &mut self,
        provider: &str,
        default_env: &str,
        existing: Option<(&str, bool, bool)>,
    ) -> Result<KeyChoice> {
        let options = [
            "the frontend brings its own — toker stores nothing and passes the frontend's key through",
            "give it to toker — kept in the OS keyring (toker.toml at mode 0600 when there is none)",
            "an env var — set in the systemd user environment, not your shell's",
        ];
        // The existing choice, as far as the config can tell: a stored
        // key is option 1, a renamed env var option 2. The default env
        // name with nothing stored reads as the frontend bringing its
        // own, which is what it does at runtime when the var is unset.
        let stored = existing.is_some_and(|(_, keyring, literal)| keyring || literal);
        let preselect = match existing {
            Some(_) if stored => 1,
            Some((env, _, _)) if env != default_env => 2,
            _ => 0,
        };
        match self.prompt.select(
            &format!("How should toker authenticate to {provider}?"),
            &options,
            Some(preselect),
        )? {
            0 => {
                self.say(&format!(
                    "  {provider}: toker stores no key; the frontend's own is passed through \
                     (a request without one goes upstream unauthenticated)"
                ))?;
                Ok(KeyChoice::Frontend)
            }
            1 => {
                let hint = if stored {
                    " (empty keeps the stored one)"
                } else {
                    ""
                };
                for _ in 0..3 {
                    let key = self.prompt.text(
                        &format!("The {provider} API key, for the OS keyring{hint}"),
                        None,
                        true,
                    )?;
                    if !key.is_empty() {
                        return Ok(KeyChoice::Store(key));
                    }
                    if stored {
                        return Ok(KeyChoice::KeepStored);
                    }
                }
                bail!("no {provider} key was given")
            }
            _ => {
                let current = existing.map_or(default_env, |(env, _, _)| env);
                let answer = self.prompt.text(
                    &format!("Env var holding the {provider} API key"),
                    Some(current),
                    false,
                )?;
                let name = if answer.is_empty() {
                    current.to_owned()
                } else {
                    answer
                };
                // The plain statement of where the variable must live:
                // a variable exported in a shell rc is invisible to the
                // service, and the symptom (401s) does not say why.
                self.say(&format!(
                    "  note: toker runs as a socket-activated systemd user service, so it sees \
                     only the user manager's environment, not your shell's. Put {name} in \
                     ~/.config/environment.d/*.conf (read at login), or run \
                     `systemctl --user set-environment {name}=…`; then restart toker.service \
                     for a running service to pick it up"
                ))?;
                Ok(KeyChoice::Env(name))
            }
        }
    }

    /// Place every key given to toker: the OS keyring when it takes the
    /// write, else the 0600 `toker.toml` with a line saying so. Runs
    /// before the config write, so the config records where each key
    /// actually went.
    fn place_keys(&mut self, choices: &mut Choices) -> Result<()> {
        for (provider, slot) in [
            ("anthropic_api", &mut choices.anthropic_api_key),
            ("openrouter", &mut choices.openrouter_key),
        ] {
            let Some(KeyChoice::Store(key)) = slot.as_ref() else {
                continue;
            };
            let key = key.clone();
            *slot = Some(match self.secrets.set(provider, &key) {
                Ok(()) => {
                    writeln!(
                        self.out,
                        "  {provider} key: stored in the OS keyring ({}/{provider})",
                        crate::secrets::KEYRING_SERVICE
                    )
                    .context("writing the wizard's output")?;
                    KeyChoice::Keyring
                }
                Err(error) => {
                    // The error names the entry, never the key.
                    writeln!(
                        self.out,
                        "  {provider} key: no OS keyring ({error:#}) — stored in toker.toml \
                         at mode 0600 instead"
                    )
                    .context("writing the wizard's output")?;
                    KeyChoice::Literal(key)
                }
            });
        }
        Ok(())
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
        let mut choices = choices.clone();
        self.place_keys(&mut choices)?;
        let db_explicit = detected.db_explicit;
        let mut written: Option<Config> = None;
        write_config(&self.paths.config_toml, |config| {
            apply_choices(config, &choices, self.paths, db_explicit)?;
            written = Some(config.clone());
            Ok(())
        })?;
        let config = written.expect("the write ran");
        self.say(&format!("wrote {}", self.paths.config_toml.display()))?;
        self.say(&format!(
            "  port {}, awake {}, backends {}",
            config.port,
            on_off(config.awake),
            backends_list(&config),
        ))?;
        self.say(&format!(
            "  anthropic → {}; openai_chat → {}",
            config
                .default_backend_anthropic
                .as_deref()
                .unwrap_or("none"),
            config
                .default_backend_openai_chat
                .as_deref()
                .unwrap_or("none")
        ))?;
        if let Some(api) = &config.anthropic_api {
            self.say(&key_line(
                "anthropic_api",
                api.key_sources(),
                &api.api_key_env,
            ))?;
        }
        if let Some(openrouter) = &config.openrouter {
            self.say(&key_line(
                "openrouter",
                openrouter.key_sources(),
                &openrouter.api_key_env,
            ))?;
        }
        report.config_written = true;
        Ok(config)
    }

    // ── step 3: install + start the units ─────────────────────────

    /// [`Step::InstallUnits`] — create the state dir, generate the
    /// units (functions of the configured port, this binary, and the
    /// state dir), install them, `daemon-reload`, `enable --now
    /// toker.socket`. Every failure is
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
        let mut manual: Vec<String> = Vec::new();

        // The service's ReadWritePaths names the state dir, and systemd
        // fails the namespace setup (226/NAMESPACE) when it is missing,
        // before the binary's own `Store::open` could create it — and
        // under ProtectHome=read-only the service could not create it
        // anyway. So the wizard does, before anything can start.
        if let Err(error) = std::fs::create_dir_all(&self.paths.state_dir) {
            ok = false;
            self.say(&format!(
                "creating the state dir {} failed: {error}",
                self.paths.state_dir.display()
            ))?;
            manual.push(format!("mkdir -p {}", self.paths.state_dir.display()));
        }

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
                    manual.push(format!(
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
            report.units_manual = manual;
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
            let wired = matches!(&detected.url, UrlRead::Base(found) if loopback_port_of(found) == Some(port));
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
    /// wake/hold/ping timers (a yes/no, then the slots: they install
    /// the hold and ping user timers and attempt the wake system timer
    /// through sudo; a no removes any an earlier run installed), and
    /// the history import offer (when
    /// the source exists: the existing `toker import` logic,
    /// in-process, into the ledger the config names).
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
        // Asked as a yes/no first, defaulting to what is installed: the
        // slots used to be one free-text question whose "clear for none"
        // could not work, because the prompt hands back its default for
        // an empty answer — declining installed the default timers.
        let installed = InstalledTimers::read(&self.paths.units_dir);
        let wanted = self.prompt.confirm(
            "Wake the machine on weekdays to open quota windows on a schedule (wake/hold/ping timers)?",
            installed.any(),
        )?;
        if wanted {
            let slots = self.ask_slots(&installed)?;
            self.timers_step(&slots, &installed, report)?;
        } else if installed.any() {
            self.remove_timers_step(&installed, report)?;
        }
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

    /// The wake/hold/ping slot question, once the timers are wanted: a
    /// free-form `hh:mm` list, strictly validated (a bad token is named
    /// and re-asked). The default is the slots an earlier run installed,
    /// else [`DEFAULT_SLOTS`]; an empty answer takes it.
    fn ask_slots(&mut self, installed: &InstalledTimers) -> Result<Vec<String>> {
        let default = if installed.ping_slots.is_empty() {
            DEFAULT_SLOTS.to_owned()
        } else {
            installed.ping_slots.join(", ")
        };
        for _ in 0..3 {
            let answer = self.prompt.text(
                &format!(
                    "Weekday (Mon..Fri) wake/hold/ping slots (hh:mm, comma- or space-separated; \
                     each pings {PING_DELAY_MINUTES} minutes later)"
                ),
                Some(&default),
                false,
            )?;
            // The seam's contract: the wizard resolves an empty answer
            // to the default itself, so the real UI and the fake agree.
            let answer = if answer.trim().is_empty() {
                default.as_str()
            } else {
                answer.as_str()
            };
            match timers::parse_slot_list(answer) {
                Ok(slots) if !slots.is_empty() => return Ok(slots),
                Ok(_) => self.say("  no slots given — try again")?,
                Err(bad) => self.say(&format!("  {bad:?} is not a hh:mm slot — try again"))?,
            }
        }
        bail!("no valid slots were given")
    }

    /// The timers proper, once slots are chosen (plan: "Sleep lock,
    /// wake, ping"). The hold and per-slot ping units go through the
    /// ordinary user-manager path (`install_unit`, `daemon-reload`,
    /// `enable --now` per timer); the wake SYSTEM timer follows in
    /// [`Wizard::wake_step`]. The ping timers of slots an earlier run
    /// chose and this one did not are disabled and removed, not left
    /// firing for a schedule nobody asked for. Every failure here is
    /// non-fatal with the manual commands printed.
    fn timers_step(
        &mut self,
        slots: &[String],
        installed: &InstalledTimers,
        report: &mut RunReport,
    ) -> Result<()> {
        let exe = std::env::current_exe().context("resolving the running binary's own path")?;
        report.timer_slots = slots.to_vec();

        // The user units: one hold pair, plus one ping pair per slot.
        let (hold_timer, hold_service) = hold_user_units(slots, &exe);
        let mut units = vec![
            (HOLD_TIMER_UNIT.to_owned(), hold_timer),
            (HOLD_SERVICE_UNIT.to_owned(), hold_service),
        ];
        let pings = ping_user_units(slots, &exe);
        for pair in &pings {
            units.push((pair.timer_name.clone(), pair.timer.clone()));
            units.push((pair.service_name.clone(), pair.service.clone()));
        }

        let mut ok = true;
        for (name, contents) in &units {
            match self.runner.install_unit(name, contents) {
                Ok(path) => {
                    self.say(&format!("installed {} ({})", name, path.display()))?;
                    report.timers_installed.push(name.clone());
                }
                Err(error) => {
                    ok = false;
                    self.say(&format!("installing {name} failed: {error:#}"))?;
                    self.say("  the unit contents, to place by hand:")?;
                    for line in contents.lines() {
                        self.say(&format!("    | {line}"))?;
                    }
                    report.timers_manual.push(format!(
                        "write {name} into {}",
                        self.paths.units_dir.join(name).display()
                    ));
                }
            }
        }

        let stale: Vec<String> = installed
            .ping_slots
            .iter()
            .filter(|slot| !slots.contains(slot))
            .map(|slot| format!("{}.timer", ping_unit_stem(slot)))
            .collect();
        // Not gating the enables below: a stale timer that will not go
        // is no reason to leave the chosen ones off.
        let retired = self.retire_user_timers(&stale, report)?;

        let mut timer_names = vec![HOLD_TIMER_UNIT.to_owned()];
        timer_names.extend(pings.iter().map(|pair| pair.timer_name.clone()));
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
            for timer in &timer_names {
                match self.runner.systemctl_user(&["enable", "--now", timer]) {
                    Ok(output) if output.status.success() => {
                        self.say(&format!("systemctl --user enable --now {timer} — ok"))?
                    }
                    other => {
                        ok = false;
                        self.say(&format!(
                            "systemctl --user enable --now {timer} failed: {}",
                            stderr_of(&other)
                        ))?;
                    }
                }
            }
        }
        if !ok {
            report
                .timers_manual
                .push("systemctl --user daemon-reload".to_owned());
            for timer in &timer_names {
                report
                    .timers_manual
                    .push(format!("systemctl --user enable --now {timer}"));
            }
            self.say("the user timers did not all come up — the wake timer is still attempted")?;
        } else if !retired {
            self.say("the timers of slots no longer chosen were not all removed")?;
        }
        if !(ok && retired) {
            self.say("finish the user timers by hand:")?;
            for command in &report.timers_manual {
                self.say(&format!("  {command}"))?;
            }
        }
        report.timers_ok = ok && retired;

        // The wake SYSTEM timer: independent of the user timers' fate
        // (a machine that never wakes still holds and pings fine while
        // it is up).
        self.wake_step(slots, report)
    }

    /// Disable (`--now`) and remove each user timer and the service of
    /// the same stem. A timer that will not disable keeps its files, so
    /// the manual command still has something to act on. Returns whether
    /// every one went.
    fn retire_user_timers(&mut self, timers: &[String], report: &mut RunReport) -> Result<bool> {
        let mut ok = true;
        for timer in timers {
            let service = timer.replace(".timer", ".service");
            match self.runner.systemctl_user(&["disable", "--now", timer]) {
                Ok(output) if output.status.success() => {
                    self.say(&format!("systemctl --user disable --now {timer} — ok"))?;
                }
                other => {
                    ok = false;
                    self.say(&format!(
                        "systemctl --user disable --now {timer} failed: {}",
                        stderr_of(&other)
                    ))?;
                    report
                        .timers_manual
                        .push(format!("systemctl --user disable --now {timer}"));
                    continue;
                }
            }
            for name in [timer, &service] {
                match self.runner.remove_unit(name) {
                    Ok(()) => {
                        self.say(&format!("removed {name}"))?;
                        report.timers_removed.push(name.clone());
                    }
                    Err(error) => {
                        ok = false;
                        self.say(&format!("removing {name} failed: {error:#}"))?;
                        report
                            .timers_manual
                            .push(format!("rm {}", self.paths.units_dir.join(name).display()));
                    }
                }
            }
        }
        Ok(ok)
    }

    /// The timers were declined while an earlier run's are installed:
    /// disable and remove them all — the user units through the user
    /// manager, the wake pair through the same sudo path that enabled
    /// it. Non-fatal like the install, with the manual commands printed.
    fn remove_timers_step(
        &mut self,
        installed: &InstalledTimers,
        report: &mut RunReport,
    ) -> Result<()> {
        self.say("removing the wake/hold/ping timers an earlier run installed")?;
        let mut timers = Vec::new();
        if installed.hold {
            timers.push(HOLD_TIMER_UNIT.to_owned());
        }
        timers.extend(
            installed
                .ping_slots
                .iter()
                .map(|slot| format!("{}.timer", ping_unit_stem(slot))),
        );
        let mut ok = self.retire_user_timers(&timers, report)?;
        if !timers.is_empty() {
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
                    report
                        .timers_manual
                        .push("systemctl --user daemon-reload".to_owned());
                }
            }
        }
        report.timers_ok = ok;
        if !ok {
            self.say("the user timers were not all removed; finish by hand:")?;
            for command in &report.timers_manual {
                self.say(&format!("  {command}"))?;
            }
        }

        if !(installed.wake_timer || installed.wake_service) {
            return Ok(());
        }
        // Only the units that exist: an install from before the wake
        // service was written has the timer alone, and disabling a unit
        // systemd has never heard of fails the whole call.
        let mut wake = Vec::new();
        if installed.wake_timer {
            wake.push(WAKE_TIMER_UNIT);
        }
        if installed.wake_service {
            wake.push(WAKE_SERVICE_UNIT);
        }
        self.say("disabling the wake system timer — sudo will be asked")?;
        let mut args = vec!["disable", "--now"];
        args.extend(&wake);
        match self.runner.systemctl_system(&args) {
            Ok(output) if output.status.success() => {
                self.say(&format!("sudo systemctl {} — ok", args.join(" ")))?;
                for name in &wake {
                    match self.runner.remove_unit(name) {
                        Ok(()) => {
                            self.say(&format!("removed the staged {name}"))?;
                            report.timers_removed.push((*name).to_owned());
                        }
                        Err(error) => {
                            self.say(&format!("removing the staged {name} failed: {error:#}"))?;
                            report
                                .wake_manual
                                .push(format!("rm {}", self.paths.units_dir.join(name).display()));
                        }
                    }
                }
            }
            other => {
                self.say(&format!(
                    "disabling the wake system timer failed: {}",
                    stderr_of(&other)
                ))?;
                // The staged files stay: the system manager may still
                // link to them, and the manual disable needs them.
                report.wake_manual = vec![
                    format!("sudo systemctl {}", args.join(" ")),
                    format!(
                        "sudo rm -f /etc/systemd/system/{WAKE_TIMER_UNIT} \
                         /etc/systemd/system/{WAKE_SERVICE_UNIT} && sudo systemctl daemon-reload"
                    ),
                ];
                self.say("the wake timer is still enabled — it will keep waking the machine;")?;
                self.say("finish by hand:")?;
                for command in &report.wake_manual {
                    self.say(&format!("  {command}"))?;
                }
            }
        }
        Ok(())
    }

    /// The wake SYSTEM timer — the only root-level piece. The timer and
    /// the do-nothing service it activates are staged where the wizard
    /// can write (the user units dir), the service is linked into the
    /// system manager by path (`sudo systemctl link`), and the timer is
    /// enabled by path (`sudo systemctl enable --now`): systemctl links
    /// a unit file outside the search paths into `/etc/systemd/system`
    /// itself. The service is linked rather than enabled because it has
    /// no `[Install]` — the timer is what starts it. Where the staging
    /// filesystem is one the system manager refuses to link from (or
    /// sudo is declined), the failure is non-fatal and the manual
    /// commands — a plain copy of both into `/etc/systemd/system`, then
    /// enable by name — are printed.
    fn wake_step(&mut self, slots: &[String], report: &mut RunReport) -> Result<()> {
        let units = [
            (WAKE_SERVICE_UNIT, wake_system_service()),
            (WAKE_TIMER_UNIT, wake_system_unit(slots)),
        ];
        let mut staged = Vec::new();
        for (name, contents) in &units {
            match self.runner.install_unit(name, contents) {
                Ok(path) => {
                    self.say(&format!("staged {} ({})", name, path.display()))?;
                    report.timers_installed.push((*name).to_owned());
                    staged.push(path.display().to_string());
                }
                Err(error) => {
                    report.wake_manual.push(format!(
                        "write {name} into /etc/systemd/system (contents below)"
                    ));
                    self.say(&format!("staging {name} failed: {error:#}"))?;
                    self.say("  the unit contents, to place by hand into /etc/systemd/system:")?;
                    for line in contents.lines() {
                        self.say(&format!("    | {line}"))?;
                    }
                }
            }
        }
        if staged.len() != units.len() {
            report.wake_manual.push(format!(
                "sudo systemctl daemon-reload && sudo systemctl enable --now {WAKE_TIMER_UNIT}"
            ));
            self.say_wake_manual(report)?;
            return Ok(());
        }
        let (service_path, timer_path) = (&staged[0], &staged[1]);

        self.say(
            "enabling the wake system timer — sudo will be asked \
             (WakeSystem=true needs the system manager: the user manager lacks CAP_WAKE_ALARM)",
        )?;
        let link = self.runner.systemctl_system(&["link", service_path]);
        let linked = match &link {
            Ok(output) if output.status.success() => {
                self.say(&format!("sudo systemctl link {service_path} — ok"))?;
                true
            }
            other => {
                self.say(&format!(
                    "linking the wake service failed: {}",
                    stderr_of(other)
                ))?;
                false
            }
        };
        let enabled = linked
            && match self
                .runner
                .systemctl_system(&["enable", "--now", timer_path])
            {
                Ok(output) if output.status.success() => {
                    self.say(&format!("sudo systemctl enable --now {timer_path} — ok"))?;
                    true
                }
                other => {
                    self.say(&format!(
                        "enabling the wake system timer failed: {}",
                        stderr_of(&other)
                    ))?;
                    false
                }
            };
        if enabled {
            report.wake_enabled = true;
        } else {
            report.wake_manual = vec![
                format!("sudo cp {service_path} /etc/systemd/system/{WAKE_SERVICE_UNIT}"),
                format!("sudo cp {timer_path} /etc/systemd/system/{WAKE_TIMER_UNIT}"),
                format!(
                    "sudo systemctl daemon-reload && sudo systemctl enable --now {WAKE_TIMER_UNIT}"
                ),
            ];
            self.say_wake_manual(report)?;
        }
        Ok(())
    }

    /// The wake failure's report: what is not working and the one or
    /// two commands that finish it by hand.
    fn say_wake_manual(&mut self, report: &RunReport) -> Result<()> {
        self.say("the wake timer is NOT enabled — the machine will not wake for its slots;")?;
        self.say("finish by hand:")?;
        for command in &report.wake_manual {
            self.say(&format!("  {command}"))?;
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
        let mut manual = Vec::new();
        manual.extend(report.timers_manual.iter().cloned());
        manual.extend(report.wake_manual.iter().cloned());
        let removed = if report.timers_removed.is_empty() {
            String::new()
        } else {
            format!("; removed {}", report.timers_removed.join(", "))
        };
        let timers = if report.timer_slots.is_empty() {
            if manual.is_empty() {
                format!("none{removed}")
            } else {
                format!(
                    "none — NOT fully removed{removed} — finish by hand: {}",
                    manual.join(" && ")
                )
            }
        } else if report.timers_ok && report.wake_enabled {
            format!(
                "slots {} — hold+ping user timers installed and enabled; \
                 wake system timer enabled{removed}",
                report.timer_slots.join(", ")
            )
        } else {
            format!(
                "slots {} — NOT fully up{removed} — finish by hand: {}",
                report.timer_slots.join(", "),
                manual.join(" && ")
            )
        };
        self.say(&format!("  timers    : {timers}"))?;
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
    // The ticked set is exactly the enabled set: a ticked backend gets
    // its block (an existing block keeps its hand edits), an unticked one
    // loses it, because a block's presence is what enables a backend.
    let ticked = |name: &str| choices.backends.contains(&name);
    fn keep_or_create<T: Default>(slot: &mut Option<T>, on: bool) {
        if on {
            slot.get_or_insert_with(T::default);
        } else {
            *slot = None;
        }
    }
    keep_or_create(&mut config.anthropic_sub, ticked("anthropic_sub"));
    keep_or_create(&mut config.anthropic_api, ticked("anthropic_api"));
    keep_or_create(&mut config.codex_sub, ticked("codex_sub"));
    keep_or_create(&mut config.openrouter, ticked("openrouter"));
    config.default_backend_anthropic = choices.anthropic_default.map(str::to_owned);
    config.default_backend_openai_chat = ticked("openrouter").then(|| "openrouter".to_owned());
    if let (Some(api), Some(key)) = (config.anthropic_api.as_mut(), &choices.anthropic_api_key) {
        apply_key(
            key,
            &mut api.api_key_env,
            &mut api.api_key_keyring,
            &mut api.api_key,
        )?;
    }
    if let (Some(openrouter), Some(key)) = (config.openrouter.as_mut(), &choices.openrouter_key) {
        apply_key(
            key,
            &mut openrouter.api_key_env,
            &mut openrouter.api_key_keyring,
            &mut openrouter.api_key,
        )?;
    }
    config.awake = choices.awake;
    if !db_explicit {
        config.db_path = paths.db_path();
    }
    Ok(())
}

/// One key choice onto a provider block's key sources. A chosen source
/// REPLACES the others, so the file says exactly what was just answered
/// (the env var name is left as it is unless the choice names one: an
/// unset variable is no source).
fn apply_key(
    key: &KeyChoice,
    env: &mut String,
    keyring: &mut bool,
    literal: &mut Option<String>,
) -> Result<()> {
    match key {
        KeyChoice::Frontend => {
            *keyring = false;
            *literal = None;
        }
        KeyChoice::Env(name) => {
            *env = name.clone();
            *keyring = false;
            *literal = None;
        }
        KeyChoice::Keyring => {
            *keyring = true;
            *literal = None;
        }
        KeyChoice::Literal(key) => {
            *keyring = false;
            *literal = Some(key.clone());
        }
        KeyChoice::KeepStored => {}
        KeyChoice::Store(_) => bail!("a key given to toker was never placed"),
    }
    Ok(())
}

/// One frontend's state-summary line: what its file says about its
/// base URL, or why there is nothing to read yet.
///
/// `toker_port` is the port toker is configured on before this run's
/// answers (the existing config's, else [`DEFAULT_PORT`]): a loopback
/// URL is toker only on that port. On [`CTP_PORT`] it is the
/// predecessor; on any other it is shown as the URL it is.
fn frontend_state(frontend: &Frontend, url: &UrlRead, toker_port: u16) -> String {
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
        UrlRead::Base(url) => match loopback_port_of(url) {
            Some(port) if port == toker_port => {
                format!("{}: points at toker (port {port})", frontend.describe())
            }
            Some(CTP_PORT) => format!(
                "{}: points at claude-token-proxy (port {CTP_PORT})",
                frontend.describe()
            ),
            _ => format!("{}: points at {url}", frontend.describe()),
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
fn key_line(provider: &str, sources: crate::config::KeySources, env: &str) -> String {
    let stored = if sources.keyring_configured {
        ", else the OS keyring"
    } else if sources.literal_set {
        ", else the literal in toker.toml (mode 0600)"
    } else {
        ", else the frontend's own key"
    };
    format!("  {provider} key: env {env} (systemd user environment) when set{stored}")
}

/// The enabled backends for the summary lines, or "none".
fn backends_list(config: &Config) -> String {
    let enabled = config.enabled_backends();
    if enabled.is_empty() {
        "none".to_owned()
    } else {
        enabled.join(", ")
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
    use crate::secrets::MemoryStore;
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
        /// The ticked indices; `None` accepts the offered defaults.
        MultiSelect(Option<Vec<usize>>),
        Confirm(bool),
        Text(String),
    }

    fn select(index: usize) -> Answer {
        Answer::Select(index)
    }

    fn multi(ticked: &[usize]) -> Answer {
        Answer::MultiSelect(Some(ticked.to_vec()))
    }

    /// Accept the multi-select's pre-ticked defaults (the Enter key).
    fn multi_defaults() -> Answer {
        Answer::MultiSelect(None)
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
        /// The default offered, rendered (`None` when none was).
        default: Option<String>,
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
            default: Option<String>,
        ) -> Answer {
            self.asked.push(Asked {
                kind,
                message: message.to_owned(),
                options: options.iter().map(|option| option.to_string()).collect(),
                secret,
                default,
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
            default: Option<usize>,
        ) -> Result<usize> {
            match self.next(
                "select",
                message,
                options,
                false,
                default.map(|index| index.to_string()),
            ) {
                Answer::Select(index) => Ok(index),
                other => panic!("select {message:?} was scripted {other:?}"),
            }
        }

        fn multi_select(
            &mut self,
            message: &str,
            options: &[&str],
            defaults: &[usize],
        ) -> Result<Vec<usize>> {
            match self.next(
                "multi_select",
                message,
                options,
                false,
                Some(format!("{defaults:?}")),
            ) {
                Answer::MultiSelect(Some(ticked)) => Ok(ticked),
                Answer::MultiSelect(None) => Ok(defaults.to_vec()),
                other => panic!("multi_select {message:?} was scripted {other:?}"),
            }
        }

        fn confirm(&mut self, message: &str, default: bool) -> Result<bool> {
            match self.next("confirm", message, &[], false, Some(default.to_string())) {
                Answer::Confirm(yes) => Ok(yes),
                other => panic!("confirm {message:?} was scripted {other:?}"),
            }
        }

        fn text(&mut self, message: &str, default: Option<&str>, secret: bool) -> Result<String> {
            match self.next("text", message, &[], secret, default.map(str::to_owned)) {
                Answer::Text(answer) => Ok(answer),
                other => panic!("text {message:?} was scripted {other:?}"),
            }
        }
    }

    // ── the recording fake runner ─────────────────────────────────

    /// The fake runner: scripted systemctl outcomes in call order,
    /// every call and every unit install recorded (user and sudo
    /// systemctl recorded separately). Install writes the scratch
    /// units dir so a run's files exist like they would for real.
    /// NO systemctl is ever executed — user or system — the machine's
    /// live toker.socket is enabled and serving while these tests run.
    #[derive(Clone, Default)]
    struct FakeRunner {
        units_dir: PathBuf,
        outcomes: Arc<Mutex<VecDeque<Result<Output>>>>,
        system_outcomes: Arc<Mutex<VecDeque<Result<Output>>>>,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        system_calls: Arc<Mutex<Vec<Vec<String>>>>,
        installed: Arc<Mutex<Vec<(String, String)>>>,
        removed: Arc<Mutex<Vec<String>>>,
    }

    impl FakeRunner {
        fn new(units_dir: PathBuf, outcomes: Vec<Result<Output>>) -> FakeRunner {
            FakeRunner {
                units_dir,
                outcomes: Arc::new(Mutex::new(outcomes.into())),
                system_outcomes: Arc::new(Mutex::new(VecDeque::new())),
                calls: Arc::new(Mutex::new(Vec::new())),
                system_calls: Arc::new(Mutex::new(Vec::new())),
                installed: Arc::new(Mutex::new(Vec::new())),
                removed: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn removed(&self) -> Vec<String> {
            self.removed.lock().unwrap().clone()
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }

        fn system_calls(&self) -> Vec<Vec<String>> {
            self.system_calls.lock().unwrap().clone()
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

        fn systemctl_system(&self, args: &[&str]) -> Result<Output> {
            self.system_calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| arg.to_string()).collect());
            self.system_outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| panic!("no scripted sudo systemctl outcome for {args:?}"))
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

        fn remove_unit(&self, name: &str) -> Result<()> {
            let _ = std::fs::remove_file(self.units_dir.join(name));
            self.removed.lock().unwrap().push(name.to_owned());
            Ok(())
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
        /// Never the real keyring: a working map unless a test swaps
        /// in [`MemoryStore::unavailable`].
        secrets: MemoryStore,
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
                secrets: MemoryStore::new(),
                out: Vec::new(),
            }
        }

        /// Script the sudo outcomes (the wake timer's enables). A run
        /// without any scripted sudo outcome panics if it reaches a
        /// `systemctl_system` call — the wake leg must be scripted
        /// deliberately.
        fn with_system(self, system: Vec<Result<Output>>) -> Rig {
            *self.runner.system_outcomes.lock().unwrap() = system.into();
            self
        }

        async fn run(&mut self, timeout: Duration) -> Result<RunReport> {
            let paths = Paths::scratch(&self.root);
            Wizard::new(
                &mut self.prompt,
                &self.runner,
                &self.secrets,
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

    /// A full fresh-machine run: anthropic_sub and openrouter ticked
    /// (whatever detection pre-ticked), openrouter's key via env,
    /// awake on, the given port, yes to every detected frontend
    /// (claude, [workhorse,] opencode, shell rc — the caller splices
    /// the workhorse confirm in where detection found it), and no to
    /// the wake/hold/ping timers (the default with none installed).
    fn answers_fresh(port: u16) -> Vec<Answer> {
        vec![
            multi(&[0, 3]),          // backends: anthropic_sub + openrouter
            select(2),               // key source: env var
            text(""),                // env name: keep the default
            confirm(true),           // awake
            text(&port.to_string()), // the listener port
            confirm(true),           // claude
            confirm(true),           // opencode
            confirm(true),           // shell rc
            confirm(true),           // the opencode plugin (opt-out, on)
            confirm(false),          // wake/hold/ping timers: no
        ]
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

    #[test]
    fn the_state_summary_names_whose_listener_a_frontend_points_at() {
        let claude = Frontend::Claude {
            settings: PathBuf::from("/home/u/.claude/settings.json"),
        };
        let opencode = Frontend::Opencode {
            config: PathBuf::from("/home/u/.config/opencode/opencode.json"),
        };
        let base = |url: &str| UrlRead::Base(url.to_owned());

        // toker only on the configured port, in either of its shapes.
        assert_eq!(
            frontend_state(&claude, &base("http://127.0.0.1:18123"), 18_123),
            "claude (/home/u/.claude/settings.json): points at toker (port 18123)"
        );
        assert_eq!(
            frontend_state(&opencode, &base("http://127.0.0.1:20000/v1"), 20_000),
            "opencode (/home/u/.config/opencode/opencode.json): points at toker (port 20000)"
        );

        // The predecessor's listener is named as such, not as toker.
        assert_eq!(
            frontend_state(&claude, &base("http://127.0.0.1:18082"), 18_123),
            "claude (/home/u/.claude/settings.json): points at claude-token-proxy (port 18082)"
        );

        // Any other loopback port — a stale toker port included — is
        // just the URL; so is anything not loopback.
        assert_eq!(
            frontend_state(&claude, &base("http://127.0.0.1:18123"), 20_000),
            "claude (/home/u/.claude/settings.json): points at http://127.0.0.1:18123"
        );
        assert_eq!(
            frontend_state(&claude, &base("https://api.anthropic.com"), 18_123),
            "claude (/home/u/.claude/settings.json): points at https://api.anthropic.com"
        );
    }

    #[test]
    fn the_timer_unit_templates_are_pinned() {
        let exe = Path::new("/opt/toker/toker");
        let slots = ["09:00".to_owned(), "23:55".to_owned()];

        // The wake SYSTEM unit: one OnCalendar= per slot, WakeSystem,
        // no catch-up, to the second, timers.target.
        let wake = wake_system_unit(&slots);
        assert_eq!(
            wake,
            r#"[Unit]
Description=toker wake timer (wakes the machine at the chosen slots)

[Timer]
# The only root-level piece toker installs: WakeSystem=true needs
# CAP_WAKE_ALARM, which the user manager lacks, so this unit belongs to
# the system manager and `toker setup` enables it through sudo. Wakes
# from suspend only — a machine that is powered off stays off.
WakeSystem=true
OnCalendar=Mon..Fri 09:00
OnCalendar=Mon..Fri 23:55
# A missed wake is not worth honouring late: the ping would refuse its
# slot anyway, and a wake at an arbitrary hour is what this avoids. To
# the second, because the default accuracy lets systemd fire up to a
# minute late, which eats into the hold's margin before the ping.
Persistent=false
AccuracySec=1s

[Install]
WantedBy=timers.target
"#
        );

        // The wake service: the timer's unit by the shared name, and
        // nothing in it — the timer's WakeSystem is the whole job.
        assert_eq!(
            wake_system_service(),
            r#"[Unit]
Description=toker wake service (the wake timer's unit; does nothing itself)

[Service]
# The wake is the whole job and the timer's WakeSystem does it; holding
# the machine up afterwards is the toker-hold user unit's.
Type=oneshot
ExecStart=/bin/true
"#
        );
        assert_eq!(
            WAKE_SERVICE_UNIT.strip_suffix(".service"),
            WAKE_TIMER_UNIT.strip_suffix(".timer"),
            "the timer activates the service by name, with no Unit= line"
        );

        // The hold pair: the slots verbatim, Persistent=false, and the
        // verb with the pinned 15 m span.
        let (hold_timer, hold_service) = hold_user_units(&slots, exe);
        assert_eq!(
            hold_timer,
            r#"[Unit]
Description=toker hold timer (holds the idle-sleep lock 15m after each wake slot)

[Timer]
# At the wake slots themselves: a timer that elapses while the machine
# is suspended fires on resume, and the hold then keeps the machine up
# for its span so the ping (11 m after the slot) can fire.
OnCalendar=Mon..Fri 09:00
OnCalendar=Mon..Fri 23:55

# A hold re-run hours after a missed slot would hold the machine up for
# nothing — the ping it protects never fires that late either.
Persistent=false
# To the second, beside the wake: systemd's default accuracy of a
# minute could start the hold after the machine has dozed off again.
AccuracySec=1s

[Install]
WantedBy=timers.target
"#
        );
        assert_eq!(
            hold_service,
            r#"[Unit]
Description=toker hold service (holds the idle-sleep lock for its span)

[Service]
Type=oneshot
# The verb takes the lock independently of the daemon's own — both
# hold, both release on their own. The start timeout is off: a hold is
# exactly as long as its --for, and must not be killed at the 90 s
# default.
TimeoutStartSec=0
ExecStart="/opt/toker/toker" hold --for=15m
"#
        );

        // The ping pairs: ONE per slot, the timer at slot+11 m — the
        // 23:55 slot wraps to 00:06, which a daily OnCalendar reads as
        // the next day, exactly 11 m later — and the service carrying
        // ITS OWN slot. Per-slot unit names (no colon in unit names).
        let pings = ping_user_units(&slots, exe);
        assert_eq!(pings.len(), 2, "one timer+service pair per slot");
        assert_eq!(pings[0].timer_name, "toker-ping-0900.timer");
        assert_eq!(pings[0].service_name, "toker-ping-0900.service");
        assert_eq!(
            pings[0].timer,
            r#"[Unit]
Description=toker ping timer (opens the 09:00 quota window, 11 m after the slot)

[Timer]
# 11 m after the slot: the machine has woken and settled, and
# the hold still has 4 m left to run. Persistent=false deliberately —
# a ping re-run hours late would open a mostly-spent window, and the
# verb's lateness guard refuses those anyway.
OnCalendar=Mon..Fri 09:11
Persistent=false
# The window anchors on the 10-minute grid, so the fire time is the
# boundary: systemd's default accuracy of a minute could push a ping
# across a grid line and move the boundary with it.
AccuracySec=10s

[Install]
WantedBy=timers.target
"#
        );
        assert_eq!(
            pings[0].service,
            r#"[Unit]
Description=toker ping service (opens the 09:00 quota window)

[Service]
Type=oneshot
# claude -p as a CLIENT of toker: the request lands on the ledger with
# ping: true — on-ledger, and never holding the sleep lock. User
# services start with a minimal environment, so the common user-local
# bin dirs ride along on PATH (TOKER_PING_CLAUDE names one that is
# not). The verb kills claude at 120 s itself; the start timeout is only
# a backstop above that and the readback, so a hung verb cannot
# suppress the next slot's ping.
Environment="PATH=%h/.local/bin:%h/.npm-global/bin:/usr/local/bin:/usr/bin:/bin"
TimeoutStartSec=3m
ExecStart="/opt/toker/toker" ping-window --slot=09:00
"#
        );
        // The wrapped slot: 23:55 + 11 m = 00:06, still paired with
        // --slot=23:55.
        assert_eq!(pings[1].timer_name, "toker-ping-2355.timer");
        assert!(pings[1].timer.contains("OnCalendar=Tue..Sat 00:06"));
        assert!(pings[1].timer.contains("opens the 23:55 quota window"));
        assert!(pings[1].service.contains("--slot=23:55"));

        // The stems munge the colon away.
        assert_eq!(ping_unit_stem("09:00"), "toker-ping-0900");
        assert_eq!(ping_unit_stem("23:55"), "toker-ping-2355");
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
        assert!(
            rig.paths().state_dir.is_dir(),
            "the state dir exists before the service could need it"
        );
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
        // The env-var choice says plainly where the variable must live.
        assert!(
            out.contains("socket-activated systemd user service")
                && out.contains("environment.d")
                && out.contains("systemctl --user set-environment OPENROUTER_API_KEY="),
            "the systemd environment note: {out}"
        );
        assert!(out.contains("unknown"), "the failed socket query: {out}");
        assert!(
            out.contains("codex login : ") && out.contains("not found"),
            "{out}"
        );
        assert!(out.contains("[1/6] choose backends"), "{out}");
        assert!(out.contains("[2/6] write toker.toml"), "{out}");
        assert!(out.contains("[3/6] install + start the units"), "{out}");
        assert!(out.contains("[4/6] verify the service answers"), "{out}");
        assert!(out.contains("[5/6] wire the frontends"), "{out}");
        assert!(out.contains("[6/6] done"), "{out}");
        // The timers question was asked (the last question of the
        // run), defaulting to no with none installed, and the no
        // installed nothing: no slots asked, no enable attempted,
        // nothing printed about wake.
        let asked = rig.prompt.asked();
        assert!(
            asked.last().is_some_and(|asked| asked.kind == "confirm"
                && asked.message.contains("wake/hold/ping timers")
                && asked.default.as_deref() == Some("false")),
            "the timers question: {asked:?}"
        );
        assert!(!out.contains("toker-hold"), "{out}");
        assert!(!out.contains("toker-ping"), "{out}");
        assert!(!out.contains("toker-wake"), "{out}");
        assert!(!out.contains("sudo"), "{out}");
        assert!(rig.runner.removed().is_empty(), "nothing to remove");
        assert!(rig.runner.system_calls().is_empty());
        assert!(
            out.contains("timers    : none\n"),
            "the summary names the decline: {out}"
        );

        // The fixture seeds were what the files said before the run —
        // i.e. the run really rewired them.
        assert_ne!(
            unwired_claude,
            std::fs::read(rig.root.join(".claude/settings.json")).expect("read claude")
        );
        assert_ne!(unwired_opencode, opencode_wired(port));

        // One multi-select offered every backend, pre-ticked from
        // detection: claude's settings tick anthropic_sub, opencode's
        // config ticks openrouter, and with no codex login codex_sub is
        // offered unticked. One anthropic backend ticked: no default
        // question.
        let first = &rig.prompt.asked()[0];
        assert_eq!(first.kind, "multi_select");
        assert_eq!(first.options.len(), 4);
        for (option, name) in first
            .options
            .iter()
            .zip(BACKENDS.iter().map(|(name, _)| name))
        {
            assert!(option.starts_with(name), "{option} offers {name}");
        }
        assert_eq!(first.default.as_deref(), Some("[0, 3]"));
        assert!(
            !rig.prompt
                .asked()
                .iter()
                .any(|asked| asked.message.contains("Default backend")),
            "one candidate per protocol, no default asked"
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

        // Run two, same root: keep the config, leave every frontend,
        // and no slots (the silent default).
        let mut rerun = Rig::at(
            root.clone(),
            vec![
                select(0),      // keep the config
                confirm(true),  // claude: already wired
                confirm(true),  // opencode: already wired
                confirm(true),  // the shell rc: already wired
                confirm(false), // wake/hold/ping timers: no
            ],
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
        // keep-vs-reconfigure select, the frontend questions were
        // confirms, and the last question was the timers confirm (no —
        // the default with none installed).
        let kinds: Vec<&str> = rerun.prompt.asked().iter().map(|a| a.kind).collect();
        assert_eq!(
            kinds,
            ["select", "confirm", "confirm", "confirm", "confirm"]
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
                multi(&[0, 3]), // backends: anthropic_sub + openrouter
                select(2),      // openrouter key: an env var
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
        assert_eq!(rig.prompt.asked().len(), 5);
    }

    #[tokio::test]
    async fn an_existing_config_can_be_reconfigured_without_losing_hand_edits() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "reconfigure",
            vec![
                select(1),               // reconfigure
                multi_defaults(),        // backends: the config's own (codex_sub)
                confirm(true),           // awake
                text("not-a-port"),      // a bad port is re-asked, not fatal
                text(&port.to_string()), // the scratch port
                confirm(true),           // claude
                confirm(false),          // wake/hold/ping timers: no
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

        // The existing config's enabled set was pre-ticked (codex_sub
        // alone, though claude's settings exist: the config wins over
        // detection) — and the note about the deliberate model-map edit
        // was printed.
        assert_eq!(rig.prompt.asked()[1].kind, "multi_select");
        assert_eq!(rig.prompt.asked()[1].default.as_deref(), Some("[2]"));
        assert!(
            !text.contains("[providers.openrouter]"),
            "not ticked, not enabled"
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
            // The fresh script up to the port, the workhorse confirm
            // (the only frontend detected here — no user-level claude,
            // no opencode, no shell rc, so no plugin offer), then no
            // slots.
            vec![
                multi(&[0, 3]),          // backends: anthropic_sub + openrouter
                select(2),               // key source: env var
                text(""),                // env name: keep the default
                confirm(true),           // awake
                text(&port.to_string()), // the listener port
                confirm(true),           // workhorse
                confirm(false),          // wake/hold/ping timers: no
            ],
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
            // detected), no slots, plus the import yes.
            vec![
                multi(&[0, 3]), // backends: anthropic_sub + openrouter
                select(2),      // openrouter key: an env var
                text(""),
                confirm(true),
                text(&port.to_string()),
                confirm(false), // wake/hold/ping timers: no
                confirm(true),  // import the predecessor's history
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
            // The literal-key script with no frontends detected: the
            // questions end at the port, then the slots text.
            {
                let mut answers = answers_fresh(port);
                answers[1] = select(1); // key source: literal in toker.toml
                answers[2] = text(KEY); // the key itself, masked in the real UI
                answers.truncate(5); // nothing past the port is asked
                answers.push(confirm(false)); // wake/hold/ping timers: no
                answers
            },
            vec![inactive(), ok_empty(), ok_empty()],
        );
        // No secret service on this machine: the key falls back to the
        // 0600 toml.
        rig.secrets = MemoryStore::unavailable();

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

        // And the summary named only the SOURCE, never the value, and
        // said why the key is not in the keyring.
        assert!(out.contains("the literal in toker.toml"), "{out}");
        assert!(out.contains("no OS keyring"), "{out}");
    }

    #[tokio::test]
    async fn a_key_given_to_toker_goes_to_the_keyring_and_the_other_choices_store_nothing() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        const KEY: &str = "sk-ant-this-must-never-appear-in-output";
        let mut rig = Rig::new(
            "keyring",
            vec![
                multi(&[1, 3]), // anthropic_api + openrouter
                select(1),      // anthropic_api: give it to toker
                text(KEY),      // the key, masked in the real UI
                select(0),      // openrouter: the frontend brings its own
                confirm(true),  // awake
                text(&port.to_string()),
                confirm(false), // wake/hold/ping timers: no
            ],
            vec![inactive(), ok_empty(), ok_empty()],
        );
        rig.run(VERIFY_TIMEOUT).await.expect("the run completes");

        // The key is in the (fake) keyring and nowhere else.
        assert_eq!(rig.secrets.peek("anthropic_api").as_deref(), Some(KEY));
        assert_eq!(rig.secrets.peek("openrouter"), None);
        let toml = std::fs::read_to_string(rig.toml_path()).expect("read the toml");
        assert!(!toml.contains(KEY), "{toml}");
        assert!(!toml.contains("api_key ="), "no literal: {toml}");
        let config = Config::load_from(&rig.toml_path()).expect("loads");
        let api = config.anthropic_api.as_ref().expect("enabled");
        assert!(api.api_key_keyring);
        let openrouter = config.openrouter.as_ref().expect("enabled");
        assert!(!openrouter.api_key_keyring && openrouter.api_key.is_none());
        let out = rig.out();
        assert!(!out.contains(KEY), "{out}");
        assert!(out.contains("stored in the OS keyring"), "{out}");
        assert!(
            out.contains("the frontend's own is passed through"),
            "{out}"
        );
        // The auth question offered the three choices in the agreed
        // order, the frontend's own first.
        let auth = &rig.prompt.asked()[1];
        assert!(auth.options[0].starts_with("the frontend brings its own"));
        assert!(auth.options[1].starts_with("give it to toker"));
        assert!(auth.options[2].starts_with("an env var"));

        // A re-run over the stored key, answered with Enter: it stays
        // where it is, and the stored choice is preselected.
        let mut rerun = Rig::at(
            rig.root.clone(),
            vec![
                select(1),        // reconfigure
                multi_defaults(), // the same backends
                select(1),        // anthropic_api: still toker's
                text(""),         // empty keeps the stored key
                select(0),        // openrouter: the frontend's own
                confirm(true),
                text(&port.to_string()),
                confirm(false),
            ],
            vec![inactive(), ok_empty(), ok_empty()],
        );
        rerun
            .run(VERIFY_TIMEOUT)
            .await
            .expect("the rerun completes");
        assert_eq!(rerun.prompt.asked()[2].default.as_deref(), Some("1"));
        let config = Config::load_from(&rerun.toml_path()).expect("loads");
        assert!(
            config
                .anthropic_api
                .as_ref()
                .expect("enabled")
                .api_key_keyring
        );
    }

    #[tokio::test]
    async fn the_ticked_set_is_the_enabled_set_and_a_default_is_asked_among_several() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "multi-select",
            vec![
                select(1),               // reconfigure
                multi(&[]),              // nothing ticked: asked again
                multi(&[0, 2]),          // anthropic_sub + codex_sub
                select(1),               // the anthropic default: codex_sub
                confirm(true),           // awake
                text(&port.to_string()), // the port
                confirm(false),          // wake/hold/ping timers: no
            ],
            vec![inactive(), ok_empty(), ok_empty()],
        );
        std::fs::create_dir_all(rig.root.join(".config/toker")).expect("create the config dir");
        std::fs::write(
            rig.toml_path(),
            "[providers.anthropic_sub]\n[providers.openrouter]\napi_key_env = \"MY_KEY\"\n",
        )
        .expect("seed the existing toml");

        rig.run(VERIFY_TIMEOUT).await.expect("the run completes");

        let asked = rig.prompt.asked();
        // The existing config's enabled set was pre-ticked.
        assert_eq!(asked[1].default.as_deref(), Some("[0, 3]"));
        assert!(rig.out().contains("pick at least one backend"));
        // The default question offered exactly the ticked anthropic
        // backends, the existing default preselected.
        assert_eq!(asked[3].kind, "select");
        assert_eq!(asked[3].options, ["anthropic_sub", "codex_sub"]);
        assert_eq!(asked[3].default.as_deref(), Some("0"));
        // No openrouter key was asked: it is no longer ticked.
        assert!(
            !asked
                .iter()
                .any(|asked| asked.message.contains("openrouter"))
        );

        let config = Config::load_from(&rig.toml_path()).expect("the written config loads");
        assert_eq!(
            config.enabled_backends(),
            vec!["anthropic_sub", "codex_sub"]
        );
        assert_eq!(
            config.default_backend_anthropic.as_deref(),
            Some("codex_sub")
        );
        assert_eq!(config.default_backend_openai_chat, None);
        // codex_sub ticked without a login: said so.
        assert!(rig.out().contains("no codex login"), "{}", rig.out());
    }

    #[tokio::test]
    async fn a_state_dir_that_cannot_be_created_is_reported_with_manual_commands() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "state-dir-fail",
            vec![
                multi(&[0, 3]), // backends: anthropic_sub + openrouter
                select(2),      // openrouter key: an env var
                text(""),
                confirm(true),
                text(&port.to_string()),
            ],
            // is-active only: with the state dir missing, nothing may
            // start, so reload and enable must never be called.
            vec![inactive()],
        );
        let before_claude = seed_claude(&rig.root);
        // A file where the state dir belongs: create_dir_all fails.
        let state_dir = rig.paths().state_dir;
        std::fs::create_dir_all(state_dir.parent().expect("the data dir"))
            .expect("create the data dir");
        std::fs::write(&state_dir, "").expect("block the state dir");

        let report = rig
            .run(Duration::from_millis(300))
            .await
            .expect("a failed state dir is reported, not an error");

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
            vec![vec!["is-active".to_owned(), SOCKET_UNIT.to_owned()]],
            "no systemctl step ran after the state dir failed"
        );
        let mkdir = format!("mkdir -p {}", state_dir.display());
        assert_eq!(
            report.units_manual.first(),
            Some(&mkdir),
            "the state dir comes first in the manual commands"
        );
        let out = rig.out();
        assert!(
            out.contains(&format!(
                "creating the state dir {} failed",
                state_dir.display()
            )),
            "{out}"
        );
        assert!(out.contains(&format!("  {mkdir}")), "{out}");
        assert!(out.contains("no frontend was touched"), "{out}");
    }

    #[tokio::test]
    async fn a_failed_unit_install_reports_manual_commands_and_touches_no_frontend() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "units-fail",
            vec![
                multi(&[0, 3]), // backends: anthropic_sub + openrouter
                select(2),      // openrouter key: an env var
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
                answers[8] = confirm(false); // the opt-out
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
                answers[8] = confirm(false); // the reinstall refusal
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

    // ── the wake/hold/ping timers ─────────────────────────────────

    /// The full slots leg: the hold pair and both ping pairs install
    /// and enable through the user manager, the wake unit is staged
    /// into the units dir and enabled by path through sudo.
    #[tokio::test]
    async fn slots_install_and_enable_the_hold_ping_and_wake_timers() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "timers-slots",
            {
                let mut answers = answers_fresh(port);
                answers.splice(9..10, [confirm(true), text("09:00, 12:30")]);
                answers
            },
            vec![
                inactive(), // is-active
                ok_empty(), // daemon-reload (units)
                ok_empty(), // enable --now socket
                ok_empty(), // daemon-reload (timers)
                ok_empty(), // enable --now toker-hold.timer
                ok_empty(), // enable --now toker-ping-0900.timer
                ok_empty(), // enable --now toker-ping-1230.timer
            ],
        )
        .with_system(vec![ok_empty(), ok_empty()]);
        seed_claude(&rig.root);
        seed_opencode(&rig.root);
        seed_rc(&rig.root);

        let report = rig
            .run(VERIFY_TIMEOUT)
            .await
            .expect("the slots run completes");

        let exe = std::env::current_exe().expect("this test binary's path");
        let slots = ["09:00".to_owned(), "12:30".to_owned()];
        let pings = ping_user_units(&slots, &exe);
        let (hold_timer, hold_service) = hold_user_units(&slots, &exe);

        // The units installed, in order, byte-for-byte the generators'
        // output — including the staged wake system timer.
        assert_eq!(
            rig.runner.installed(),
            vec![
                (SOCKET_UNIT.to_owned(), socket_unit(port)),
                (
                    SERVICE_UNIT.to_owned(),
                    service_unit(&exe, &rig.paths().state_dir)
                ),
                (HOLD_TIMER_UNIT.to_owned(), hold_timer),
                (HOLD_SERVICE_UNIT.to_owned(), hold_service),
                (pings[0].timer_name.clone(), pings[0].timer.clone()),
                (pings[0].service_name.clone(), pings[0].service.clone()),
                (pings[1].timer_name.clone(), pings[1].timer.clone()),
                (pings[1].service_name.clone(), pings[1].service.clone()),
                (WAKE_SERVICE_UNIT.to_owned(), wake_system_service()),
                (WAKE_TIMER_UNIT.to_owned(), wake_system_unit(&slots)),
            ],
            "socket, service, hold pair, one ping pair per slot, staged wake pair"
        );

        // The user-manager calls, in the wizard's exact order.
        assert_eq!(
            rig.runner.calls(),
            vec![
                vec!["is-active", SOCKET_UNIT],
                vec!["daemon-reload"],
                vec!["enable", "--now", SOCKET_UNIT],
                vec!["daemon-reload"],
                vec!["enable", "--now", HOLD_TIMER_UNIT],
                vec!["enable", "--now", "toker-ping-0900.timer"],
                vec!["enable", "--now", "toker-ping-1230.timer"],
            ]
            .into_iter()
            .map(|call| call.into_iter().map(str::to_owned).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        );

        // The wake leg: two sudo calls, both by path — link the
        // service the timer starts (it has no [Install] to enable),
        // then enable the timer. A timer whose unit is missing is
        // refused, which is how the first install never armed.
        let staged = rig.paths().units_dir.join(WAKE_TIMER_UNIT);
        let staged_service = rig.paths().units_dir.join(WAKE_SERVICE_UNIT);
        assert_eq!(
            rig.runner.system_calls(),
            vec![
                vec!["link".to_owned(), staged_service.display().to_string()],
                vec![
                    "enable".to_owned(),
                    "--now".to_owned(),
                    staged.display().to_string(),
                ],
            ],
            "sudo systemctl link <service>, then enable --now <timer>"
        );

        assert_eq!(report.timer_slots, slots);
        assert!(report.timers_ok, "the user timers came up");
        assert!(report.wake_enabled, "the wake timer was sudo-enabled");
        assert!(report.units_ok, "the main spine is unaffected");

        // The transcript: the sudo warning appears before the call,
        // and the summary reports all three timers.
        let out = rig.out();
        assert!(
            out.contains("sudo will be asked"),
            "the sudo prompt is in the transcript: {out}"
        );
        assert!(
            out.contains("CAP_WAKE_ALARM"),
            "the sudo warning says why: {out}"
        );
        assert!(
            out.contains(&format!("sudo systemctl enable --now {}", staged.display())),
            "{out}"
        );
        assert!(
            out.contains("timers    : slots 09:00, 12:30 — hold+ping user timers installed and enabled; wake system timer enabled"),
            "the summary's timers line: {out}"
        );
    }

    /// The sudo-failure leg: the wake enable fails, and the user
    /// timers it leaves standing are still installed and enabled —
    /// the manual commands carry the copy into /etc and the enable by
    /// name.
    #[tokio::test]
    async fn a_failed_sudo_keeps_the_user_timers_and_prints_manual_commands() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "timers-sudo-fail",
            {
                let mut answers = answers_fresh(port);
                answers.splice(9..10, [confirm(true), text("09:00")]); // one slot
                answers
            },
            vec![
                inactive(),
                ok_empty(), // daemon-reload (units)
                ok_empty(), // enable --now socket
                ok_empty(), // daemon-reload (timers)
                ok_empty(), // enable --now toker-hold.timer
                ok_empty(), // enable --now toker-ping-0900.timer
            ],
        )
        // sudo cannot run at all on this leg.
        .with_system(vec![Err(anyhow::anyhow!("sudo: command not found"))]);
        seed_claude(&rig.root);
        seed_opencode(&rig.root);
        seed_rc(&rig.root);

        let report = rig
            .run(VERIFY_TIMEOUT)
            .await
            .expect("a failed wake enable is non-fatal");

        // The user timers stand on their own.
        assert!(report.timers_ok, "the user timers came up");
        assert_eq!(
            report
                .timers_installed
                .iter()
                .filter(|name| name.starts_with("toker-ping"))
                .count(),
            2,
            "the ping pair is installed: {:?}",
            report.timers_installed
        );
        assert!(!report.wake_enabled);
        let out = rig.out();
        assert!(
            out.contains("linking the wake service failed: sudo: command not found"),
            "{out}"
        );
        assert_eq!(
            rig.runner.system_calls().len(),
            1,
            "the timer is not enabled once its service failed to link"
        );
        assert!(
            out.contains("the wake timer is NOT enabled — the machine will not wake for its slots"),
            "{out}"
        );
        // The manual commands: the copy into /etc, then the enable by
        // name — and they ride the summary too.
        let staged = rig.paths().units_dir.join(WAKE_TIMER_UNIT);
        let cp = format!(
            "sudo cp {} /etc/systemd/system/{WAKE_TIMER_UNIT}",
            staged.display()
        );
        assert!(out.contains(&cp), "the copy command: {out}");
        let staged_service = rig.paths().units_dir.join(WAKE_SERVICE_UNIT);
        let cp_service = format!(
            "sudo cp {} /etc/systemd/system/{WAKE_SERVICE_UNIT}",
            staged_service.display()
        );
        assert!(out.contains(&cp_service), "the service copy: {out}");
        assert!(
            out.contains(&format!(
                "sudo systemctl daemon-reload && sudo systemctl enable --now {WAKE_TIMER_UNIT}"
            )),
            "the enable-by-name command: {out}"
        );
        assert!(
            out.contains("NOT fully up"),
            "the summary carries it: {out}"
        );
        assert!(report.wake_manual.len() == 3, "{:?}", report.wake_manual);
    }

    /// The link succeeds but the enable is refused: the wake is still
    /// reported down, with every manual command, not half-done.
    #[tokio::test]
    async fn a_refused_wake_enable_after_the_link_prints_manual_commands() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "timers-enable-refused",
            {
                let mut answers = answers_fresh(port);
                answers.splice(9..10, [confirm(true), text("09:00")]); // one slot
                answers
            },
            vec![
                inactive(),
                ok_empty(), // daemon-reload (units)
                ok_empty(), // enable --now socket
                ok_empty(), // daemon-reload (timers)
                ok_empty(), // enable --now toker-hold.timer
                ok_empty(), // enable --now toker-ping-0900.timer
            ],
        )
        .with_system(vec![
            ok_empty(),
            outcome(false, "", "Failed to enable unit: Unit file is masked.\n"),
        ]);
        seed_claude(&rig.root);
        seed_opencode(&rig.root);
        seed_rc(&rig.root);

        let report = rig
            .run(VERIFY_TIMEOUT)
            .await
            .expect("a refused wake enable is non-fatal");

        assert!(!report.wake_enabled);
        assert_eq!(rig.runner.system_calls().len(), 2);
        let out = rig.out();
        assert!(
            out.contains("enabling the wake system timer failed: Failed to enable unit"),
            "{out}"
        );
        assert_eq!(report.wake_manual.len(), 3, "{:?}", report.wake_manual);
        assert!(
            report.wake_manual[0].ends_with(&format!("/etc/systemd/system/{WAKE_SERVICE_UNIT}")),
            "{:?}",
            report.wake_manual
        );
    }

    /// The user-timers-failure leg: a failed daemon-reload after the
    /// timer installs leaves the enables unrun, the manual commands
    /// printed — and the wake attempt still happens (independent
    /// pieces, independent failures).
    #[tokio::test]
    async fn a_failed_user_timer_step_still_attempts_the_wake_timer() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "timers-user-fail",
            {
                let mut answers = answers_fresh(port);
                answers.splice(9..10, [confirm(true), text("09:00")]); // one slot
                answers
            },
            vec![
                inactive(),
                ok_empty(),     // daemon-reload (units)
                ok_empty(),     // enable --now socket
                reload_fails(), // daemon-reload (timers) — the user manager is gone
            ],
        )
        .with_system(vec![ok_empty(), ok_empty()]);
        seed_claude(&rig.root);
        seed_opencode(&rig.root);
        seed_rc(&rig.root);

        let report = rig
            .run(VERIFY_TIMEOUT)
            .await
            .expect("a failed timers step is non-fatal");

        assert!(!report.timers_ok, "the user timers did not come up");
        // The enables were never called after the reload failed
        // (no further scripted user outcomes remain).
        assert_eq!(
            rig.runner.calls().last().map(|call| call.join(" ")),
            Some("daemon-reload".to_owned()),
            "enable was never called after the reload failed"
        );
        assert!(
            report
                .timers_manual
                .contains(&"systemctl --user enable --now toker-hold.timer".to_owned()),
            "the manual commands: {:?}",
            report.timers_manual
        );
        assert!(
            rig.out().contains(
                "the user timers did not all come up — the wake timer is still attempted"
            ),
            "{}",
            rig.out()
        );
        // The wake attempt still ran, and succeeded.
        assert!(report.wake_enabled);
        assert_eq!(rig.runner.system_calls().len(), 2);
    }

    /// Seed the scratch units dir with what an earlier run left there.
    fn seed_units(rig: &Rig, names: &[&str]) {
        let dir = rig.paths().units_dir;
        std::fs::create_dir_all(&dir).expect("create the scratch units dir");
        for name in names {
            std::fs::write(dir.join(name), "[Unit]\n").expect("seed a unit");
        }
    }

    fn strings(calls: Vec<Vec<&str>>) -> Vec<Vec<String>> {
        calls
            .into_iter()
            .map(|call| call.into_iter().map(str::to_owned).collect())
            .collect()
    }

    /// A yes with an empty slot answer takes the default slots — the
    /// predecessor's schedule — rather than meaning none.
    #[tokio::test]
    async fn accepting_with_an_empty_answer_installs_the_default_slots() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "timers-default-slots",
            {
                let mut answers = answers_fresh(port);
                answers.splice(9..10, [confirm(true), text("")]);
                answers
            },
            vec![
                inactive(),
                ok_empty(), // daemon-reload (units)
                ok_empty(), // enable --now socket
                ok_empty(), // daemon-reload (timers)
                ok_empty(), // enable --now toker-hold.timer
                ok_empty(), // enable --now toker-ping-0720.timer
                ok_empty(), // enable --now toker-ping-1220.timer
            ],
        )
        .with_system(vec![ok_empty(), ok_empty()]);
        seed_claude(&rig.root);
        seed_opencode(&rig.root);
        seed_rc(&rig.root);

        let report = rig.run(VERIFY_TIMEOUT).await.expect("the run completes");

        assert_eq!(report.timer_slots, ["07:20", "12:20"]);
        assert!(report.timers_ok && report.wake_enabled);
        let asked = rig.prompt.asked();
        let n = asked.len();
        assert_eq!(asked[n - 2].kind, "confirm");
        assert_eq!(
            asked[n - 2].default.as_deref(),
            Some("false"),
            "none installed"
        );
        assert_eq!(asked[n - 1].kind, "text");
        assert_eq!(asked[n - 1].default.as_deref(), Some(DEFAULT_SLOTS));
        assert!(
            !asked[n - 1].message.contains("clear"),
            "an empty answer is the default, so the prompt must not offer it as none"
        );
        let calls = rig.runner.calls();
        assert_eq!(
            calls[calls.len() - 2..],
            strings(vec![
                vec!["enable", "--now", "toker-ping-0720.timer"],
                vec!["enable", "--now", "toker-ping-1220.timer"],
            ])
        );
        assert!(rig.runner.removed().is_empty());
    }

    /// A no while an earlier run's timers are installed takes them all
    /// out: the user pairs through the user manager, the wake timer
    /// through sudo — here an install from before the wake service
    /// existed, so only the timer is named to systemd.
    #[tokio::test]
    async fn declining_removes_the_timers_an_earlier_run_installed() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "timers-decline-installed",
            answers_fresh(port),
            vec![
                inactive(),
                ok_empty(), // daemon-reload (units)
                ok_empty(), // enable --now socket
                ok_empty(), // disable --now toker-hold.timer
                ok_empty(), // disable --now toker-ping-0730.timer
                ok_empty(), // daemon-reload (removal)
            ],
        )
        .with_system(vec![ok_empty()]);
        seed_units(
            &rig,
            &[
                HOLD_TIMER_UNIT,
                HOLD_SERVICE_UNIT,
                "toker-ping-0730.timer",
                "toker-ping-0730.service",
                WAKE_TIMER_UNIT,
            ],
        );
        seed_claude(&rig.root);
        seed_opencode(&rig.root);
        seed_rc(&rig.root);

        let report = rig.run(VERIFY_TIMEOUT).await.expect("the run completes");

        let asked = rig.prompt.asked();
        assert_eq!(
            asked.last().and_then(|asked| asked.default.as_deref()),
            Some("true"),
            "installed timers make yes the default"
        );
        assert!(report.timer_slots.is_empty());
        assert_eq!(
            rig.runner.calls()[3..],
            strings(vec![
                vec!["disable", "--now", HOLD_TIMER_UNIT],
                vec!["disable", "--now", "toker-ping-0730.timer"],
                vec!["daemon-reload"],
            ])
        );
        assert_eq!(
            rig.runner.system_calls(),
            strings(vec![vec!["disable", "--now", WAKE_TIMER_UNIT]])
        );
        assert_eq!(
            report.timers_removed,
            [
                HOLD_TIMER_UNIT,
                HOLD_SERVICE_UNIT,
                "toker-ping-0730.timer",
                "toker-ping-0730.service",
                WAKE_TIMER_UNIT,
            ]
        );
        let left: Vec<_> = std::fs::read_dir(rig.paths().units_dir)
            .expect("the units dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| {
                name.starts_with("toker-") && name != SOCKET_UNIT && name != SERVICE_UNIT
            })
            .collect();
        assert!(left.is_empty(), "nothing of the timers is left: {left:?}");
        let out = rig.out();
        assert!(
            out.contains("timers    : none; removed toker-hold.timer"),
            "{out}"
        );
    }

    /// A refused sudo on the decline leaves the wake pair staged, since
    /// the system manager may still link to it, and says how to finish.
    #[tokio::test]
    async fn a_refused_wake_disable_keeps_the_staged_pair_and_says_so() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "timers-decline-sudo-fail",
            answers_fresh(port),
            vec![inactive(), ok_empty(), ok_empty()],
        )
        .with_system(vec![Err(anyhow::anyhow!("sudo: a password is required"))]);
        seed_units(&rig, &[WAKE_TIMER_UNIT, WAKE_SERVICE_UNIT]);
        seed_claude(&rig.root);
        seed_opencode(&rig.root);
        seed_rc(&rig.root);

        let report = rig.run(VERIFY_TIMEOUT).await.expect("non-fatal");

        assert_eq!(
            rig.runner.system_calls(),
            strings(vec![vec![
                "disable",
                "--now",
                WAKE_TIMER_UNIT,
                WAKE_SERVICE_UNIT
            ]])
        );
        assert!(rig.runner.removed().is_empty(), "the staged pair stays");
        assert!(rig.paths().units_dir.join(WAKE_TIMER_UNIT).exists());
        assert_eq!(report.wake_manual.len(), 2, "{:?}", report.wake_manual);
        let out = rig.out();
        assert!(
            out.contains("the wake timer is still enabled — it will keep waking the machine"),
            "{out}"
        );
        assert!(out.contains("NOT fully removed"), "{out}");
    }

    /// A yes with a different slot list retires the ping pairs of the
    /// slots it dropped — the old default's 07:30 here — and offers
    /// the installed slots as the default.
    #[tokio::test]
    async fn a_changed_slot_list_retires_the_dropped_ping_timers() {
        let (port, _server) = serve(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let mut rig = Rig::new(
            "timers-retire-stale",
            {
                let mut answers = answers_fresh(port);
                answers.splice(9..10, [confirm(true), text("07:20, 12:20")]);
                answers
            },
            vec![
                inactive(),
                ok_empty(), // daemon-reload (units)
                ok_empty(), // enable --now socket
                ok_empty(), // disable --now toker-ping-0730.timer
                ok_empty(), // daemon-reload (timers)
                ok_empty(), // enable --now toker-hold.timer
                ok_empty(), // enable --now toker-ping-0720.timer
                ok_empty(), // enable --now toker-ping-1220.timer
            ],
        )
        .with_system(vec![ok_empty(), ok_empty()]);
        seed_units(
            &rig,
            &[
                HOLD_TIMER_UNIT,
                HOLD_SERVICE_UNIT,
                "toker-ping-0730.timer",
                "toker-ping-0730.service",
                "toker-ping-1220.timer",
                "toker-ping-1220.service",
            ],
        );
        seed_claude(&rig.root);
        seed_opencode(&rig.root);
        seed_rc(&rig.root);

        let report = rig.run(VERIFY_TIMEOUT).await.expect("the run completes");

        let asked = rig.prompt.asked();
        let n = asked.len();
        assert_eq!(asked[n - 2].default.as_deref(), Some("true"));
        assert_eq!(
            asked[n - 1].default.as_deref(),
            Some("07:30, 12:20"),
            "the installed slots are the default"
        );
        assert_eq!(
            rig.runner.calls()[3],
            ["disable", "--now", "toker-ping-0730.timer"]
        );
        assert_eq!(
            report.timers_removed,
            ["toker-ping-0730.timer", "toker-ping-0730.service"]
        );
        assert!(!rig.paths().units_dir.join("toker-ping-0730.timer").exists());
        assert!(rig.paths().units_dir.join("toker-ping-1220.timer").exists());
        assert!(report.timers_ok && report.wake_enabled);
        assert!(
            rig.out()
                .contains("wake system timer enabled; removed toker-ping-0730.timer"),
            "{}",
            rig.out()
        );
    }
}
