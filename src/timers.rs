//! The wake/hold/ping timer subsystem (plan: "Sleep lock, wake, ping")
//! — the last parity gap with the predecessor proxy, as its Linux
//! shapes. The pieces:
//!
//! - **wake** — a SYSTEM timer with `WakeSystem=true` and one
//!   `OnCalendar=` per user-chosen slot: wakes the machine **from
//!   suspend only** (a powered-off machine stays off — that is
//!   rtcwake's job, not this one's). Root-level because the user
//!   manager lacks `CAP_WAKE_ALARM`; it is the only root-level piece
//!   toker has ([`crate::setup::wizard::wake_system_unit`] generates
//!   it and the `/bin/true` service it starts, the wizard enables them
//!   through sudo).
//! - **hold** — USER timers at the same slots running `toker hold
//!   --for=15m`: a timer that elapses while the machine is suspended
//!   fires on resume, and the hold then keeps the machine up those 15
//!   minutes so the ping can fire. The verb holds the idle-only sleep
//!   lock through the same detached-child machinery the daemon uses
//!   ([`crate::middleware::awake`]) — independently: daemon and verb
//!   each take their own inhibitor, each release their own, and the
//!   PID-watching child makes a verb that dies for any reason release
//!   with it.
//! - **ping** — USER timers ~11 minutes after each slot running `toker
//!   ping-window --slot=hh:mm`: opens a fresh 5-hour quota window by
//!   sending **one tiny request as a client** — `claude -p` with
//!   `ANTHROPIC_BASE_URL` pointed at toker and
//!   `ANTHROPIC_CUSTOM_HEADERS` carrying the configured ping header
//!   (`Name: 1`). The request is on-ledger like any other: the row
//!   carries `ping: true`, the lane is tagged, and ping lanes never
//!   hold the sleep lock ([`crate::middleware::lanes`]). Buys phase,
//!   not capacity. A window already open (any sub measurement reporting
//!   a 5-hour reset ahead of now) is a skip, as in the predecessor's
//!   `decidePing`.
//!
//! The 11-minute delay (not at the slot): the machine needs a moment
//! after waking for the network to settle, and the hold (15 m) covers
//! the slot→ping span with 4 m to spare. The lateness guard is the
//! other half of that timing: a ping more than
//! [`LATENESS_LIMIT_MINUTES`] past its scheduled fire time refuses to
//! run — systemd fires a timer that elapsed during suspend
//! immediately on resume (possibly hours later), and `Persistent=false`
//! (the timer units' deliberate setting) keeps a reboot from re-running
//! missed slots at all; the guard covers the resume case and any manual
//! re-run alike. A window opened hours into its 5 hours is mostly
//! spent, so the refusal is the honest answer.
//!
//! Units: all times are epoch milliseconds (the ledger's convention),
//! resolved in a caller-supplied [`jiff::tz::TimeZone`] — the timers
//! are local wall-clock (`OnCalendar`), so the verb resolves slots in
//! the system zone and the tests in fixed ones.
//!
//! Seams (the wizard's pattern): the spawner is the awake module's
//! ([`LockSpawner`] — tests inject fakes and never take a real lock),
//! the `claude` invocation goes through [`CommandRunner`] (tests
//! script it; no real `claude` call, and credentials are never touched
//! — the child inherits its own login, toker only adds the base URL
//! and the ping header), and the readback's retry pacing is a closure
//! so tests poll instantly.

use std::io::Write;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::tz::TimeZone;

use crate::middleware::awake::{InhibitCommand, LockSpawner};
use crate::setup::patchers::anthropic_base_url;
use crate::store::{PingAction, PingRecord, Store};

/// The ping timer's offset from its slot: 11 minutes after the wake
/// slot — the machine has woken and settled, and the hold still has
/// 4 m to run.
pub const PING_DELAY_MINUTES: i64 = 11;

/// The lateness guard: a ping more than this past its scheduled fire
/// time refuses to run. Covers systemd's fire-on-resume (a timer that
/// elapsed during suspend fires on resume, hours late) and any re-run
/// of missed slots.
pub const LATENESS_LIMIT_MINUTES: i64 = 10;

/// The quota window a ping opens: the anthropic 5-hour window.
pub const WINDOW_MS: i64 = 5 * 3_600_000;

/// The grid the quota windows' phase snaps to: a slot's window is
/// anchored at `floor(slot, 10 min)`.
pub const ANCHOR_STEP_MS: i64 = 10 * 60_000;

/// What the wizard-installed hold service runs: `hold --for=15m`. The
/// 15 m must exceed the ping delay plus settle margin, so the ping
/// (11 m after the slot) always fires inside the hold.
pub const HOLD_UNIT_FOR: &str = "15m";

/// The hold's inhibitor `--why` (the daemon's hold says "agent sessions
/// are live"; this one says what it is for instead).
pub const HOLD_WHY: &str = "wake hold timer";

/// What `toker wake-arm` says. The predecessor's primitive was a
/// macOS one-shot (`pmset schedule wake`); on Linux the systemd
/// `WakeSystem` timer replaces it, so the verb is a documented no-op —
/// wake is owned by the system timer, and the wizard never installs
/// anything for the verb itself.
pub const WAKE_ARM_NOTE: &str = "nothing to arm: wake is owned by the systemd \
     system timer toker-wake.timer (WakeSystem=true), installed and enabled by \
     `toker setup`. This verb was the predecessor's macOS one-shot \
     (pmset schedule wake); on Linux the timer replaces it.";

/// How many times the readback checks the ledger for the ping row: the
/// row lands when the response completes server-side, which can be a
/// moment after the client exits.
const READBACK_ATTEMPTS: usize = 10;

/// The pause between readback attempts.
const READBACK_INTERVAL: Duration = Duration::from_millis(500);

/// The model the ping asks for when `TOKER_PING_MODEL` does not say:
/// the cheapest. A ping buys a window's phase, and any model opens it.
pub const PING_MODEL: &str = "haiku";

/// What the ping says. Any prompt opens the window; the reply is never
/// read or kept (it is completion content).
const PING_PROMPT: &str = "Reply with the single word: ok";

/// How long the client may take before it is killed. The predecessor's
/// figure: a ping is one tiny request, and one hung past this has
/// already missed the bucket it was aiming at.
pub const PING_TIMEOUT: Duration = Duration::from_secs(120);

/// The anthropic subscription backend's provider id
/// ([`crate::providers::anthropic::AnthropicSub`]'s): the only meter
/// source, and the only backend whose requests open a 5-hour window. A
/// ping routed to any other backend opens nothing, so its rows must not
/// read as the window opening.
const SUB_PROVIDER: &str = "anthropic_sub";

// ── the slot clock ─────────────────────────────────────────────────────

/// A validated `hh:mm` slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub hour: u8,
    pub minute: u8,
}

impl Slot {
    /// The `hh:mm` spelling (the unit files' `OnCalendar=` value and the
    /// `--slot` argument's shape).
    pub fn hhmm(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }
}

/// Parse `hh:mm` **strictly**: exactly two digits, a colon, two digits,
/// `00:00`–`23:59`. Anything else — `9:00`, `09:0`, `0900`, `09:60`,
/// surrounding spaces — is not a slot.
pub fn parse_slot(text: &str) -> Option<Slot> {
    let (hours, minutes) = text.split_once(':')?;
    let hour = two_digits(hours)?;
    let minute = two_digits(minutes)?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(Slot { hour, minute })
}

/// Exactly two ASCII digits, as a number.
fn two_digits(text: &str) -> Option<u8> {
    let bytes = text.as_bytes();
    if bytes.len() != 2 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some((bytes[0] - b'0') * 10 + (bytes[1] - b'0'))
}

/// Parse the hold span, in minutes: a positive finite number with an
/// optional single trailing `m` — `15m` (what the unit passes),
/// `15`, `0.5`, `0.5m`. Strictly: no whitespace, no other suffix, and
/// a zero or negative span is not a span.
pub fn parse_hold_minutes(text: &str) -> Option<f64> {
    let text = text.strip_suffix('m').unwrap_or(text);
    let minutes: f64 = text.parse().ok()?;
    minutes.is_finite().then_some(minutes).filter(|m| *m > 0.0)
}

/// Parse a free-form slot list for the wizard: tokens separated by
/// commas and/or whitespace, each a strict `hh:mm`; duplicates collapse
/// (first wins, input order kept); an empty answer is no slots.
/// `Err` carries the first offending token, for the re-ask.
pub fn parse_slot_list(text: &str) -> std::result::Result<Vec<String>, String> {
    let mut slots: Vec<String> = Vec::new();
    for token in text.split(|c: char| c == ',' || c.is_whitespace()) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if parse_slot(token).is_none() {
            return Err(token.to_owned());
        }
        if !slots.iter().any(|slot| slot == token) {
            slots.push(token.to_owned());
        }
    }
    Ok(slots)
}

/// The slot's clock time plus `minutes`, wrapping past midnight (the
/// timers are weekday `OnCalendar` values, so a wrapped time fires
/// the NEXT day — a Friday 23:55 slot's ping fires Saturday 00:06,
/// which is why its timer's mask runs Tue..Sat).
pub fn slot_plus_minutes(slot: &Slot, minutes: i64) -> Slot {
    let total = (slot.hour as i64 * 60 + slot.minute as i64 + minutes).rem_euclid(24 * 60);
    Slot {
        hour: (total / 60) as u8,
        minute: (total % 60) as u8,
    }
}

/// The most recent WEEKDAY (Mon–Fri) occurrence of the slot's
/// wall-clock time at-or-before `now`, in `tz` — the schedule is
/// weekday-only, matching the work pattern it serves. A weekend day
/// has no occurrence of its own: Saturday and Sunday resolve to
/// Friday's slot, and the lateness guard then refuses a weekend run
/// as long past (weekends have no ping, by design). Before the slot's
/// first occurrence of the local day, the resolution steps to the
/// previous weekday — an early run for today's slot reads as very
/// late for Friday's, which the guard refuses.
///
/// A slot inside a DST gap or fold resolves by jiff's compatible
/// strategy (the gap folds forward); a weekday clock slot has no
/// meaningful finer answer.
pub fn last_occurrence_ms(now_ms: i64, slot: &Slot, tz: &TimeZone) -> Option<i64> {
    use jiff::civil::Weekday;
    let now = jiff::Timestamp::from_millisecond(now_ms).ok()?;
    let zoned = now.to_zoned(tz.clone());
    let time = jiff::civil::Time::constant(slot.hour as i8, slot.minute as i8, 0, 0);
    let at = |date: jiff::civil::Date| -> Option<i64> {
        Some(
            date.to_datetime(time)
                .to_zoned(tz.clone())
                .ok()?
                .timestamp()
                .as_millisecond(),
        )
    };
    // Start at today; step back until the weekday whose slot has
    // occurred. Weekends never resolve to themselves — the timers do
    // not fire Sat/Sun, so a weekend keeps walking to Friday.
    let mut date = zoned.date();
    if at(date)? > now_ms {
        date = date.yesterday().ok()?;
    }
    loop {
        match date.weekday() {
            Weekday::Saturday | Weekday::Sunday => date = date.yesterday().ok()?,
            _ => return at(date),
        }
    }
}

/// When a slot's ping is scheduled to fire: the occurrence plus
/// [`PING_DELAY_MINUTES`] (the timer units sit at exactly this offset).
pub fn scheduled_fire_ms(occurrence_ms: i64) -> i64 {
    occurrence_ms + PING_DELAY_MINUTES * 60_000
}

/// The boundary of the quota window a request at `at_ms` opens:
/// `floor(at, ANCHOR_STEP_MS) + WINDOW_MS` — a window is anchored to the
/// request that opens it, on the 10-minute grid, ending 5 h later
/// (measured across 38 windows in the predecessor's log). Pass the time
/// the ping actually fires, not its slot: the first version floored the
/// slot, which is 11 minutes earlier and so predicted a boundary 10
/// minutes early whenever the two straddled a grid line — 07:20's
/// window "ending 12:20" when the 07:31 ping opens one ending 12:30.
/// Floored on the epoch, not local clock fields, which is where the API
/// does it.
pub fn window_boundary_ms(at_ms: i64) -> i64 {
    at_ms - at_ms.rem_euclid(ANCHOR_STEP_MS) + WINDOW_MS
}

/// A wall-clock `hh:mm` rendering of an instant (the notices' frozen
/// `%H:%M` convention; a formatting failure names the raw instant
/// rather than hiding it).
fn hhmm_of(ms: i64, tz: &TimeZone) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|ts| ts.to_zoned(tz.clone()).strftime("%H:%M").to_string())
        .unwrap_or_else(|_| format!("{ms} (ms since epoch)"))
}

// ── hold ───────────────────────────────────────────────────────────────

/// `toker hold --for=<mins>`: hold the idle-sleep lock for the span,
/// then release — through the same detached-child machinery the daemon
/// uses, but as the VERB's own lock: the inhibitor watches this
/// process's PID, so it dies however the verb dies, and the daemon's
/// own lock (if it holds one) is a separate inhibitor held and
/// released separately.
///
/// `command` is this platform's lock (the caller probes
/// [`crate::middleware::awake::platform_command`]; tests pass a fixed
/// one); `sleep` paces the span (production sleeps, tests record).
/// The hold and release are printed to `out`.
pub fn hold_lock(
    out: &mut dyn Write,
    minutes: f64,
    command: Option<InhibitCommand>,
    spawner: &mut dyn LockSpawner,
    sleep: &mut dyn FnMut(Duration),
) -> Result<()> {
    let Some(command) = command else {
        bail!("no idle-sleep lock on this platform — nothing to hold");
    };
    let mut lock = spawner
        .spawn(&command)
        .map_err(|error| anyhow::anyhow!("sleep lock unavailable: {error}"))?;
    say(
        out,
        &format!("holding the idle-sleep lock for {minutes} minutes"),
    )?;
    // `as u64` saturates: a parser-valid span is positive and finite,
    // and a pub caller cannot panic the verb with a negative one.
    sleep(Duration::from_millis((minutes * 60_000.0) as u64));
    lock.kill();
    say(out, "released the idle-sleep lock")?;
    Ok(())
}

// ── ping-window ────────────────────────────────────────────────────────

/// What the loaded config resolves to for a ping: the ledger to read
/// back, the port toker listens on, and the configured ping header's
/// name. Grouped so the verb's core stays within the argument-count
/// the lints allow without suppressing anything.
pub struct PingConfig<'a> {
    /// The ledger the daemon writes — read back for the ping row.
    pub db_path: &'a Path,
    /// The port toker listens on (the client's `ANTHROPIC_BASE_URL`).
    pub port: u16,
    /// The configured ping header's name (the client's
    /// `ANTHROPIC_CUSTOM_HEADERS` entry).
    pub ping_header: &'a str,
    /// The claude CLI to run (`TOKER_PING_CLAUDE`, else `claude` on
    /// PATH).
    pub claude: &'a str,
    /// The model the ping asks for (`TOKER_PING_MODEL`, else
    /// [`PING_MODEL`]).
    pub model: &'a str,
}

/// One client run: what to run, where, and for how long.
pub struct Invocation<'a> {
    pub program: &'a str,
    pub args: &'a [&'a str],
    /// Overrides on top of the inherited environment.
    pub env: &'a [(&'a str, String)],
    /// The working directory: a fresh empty one, so no project
    /// `CLAUDE.md` is picked up and paid for.
    pub cwd: &'a Path,
    /// Past this the child is killed and the run is an `Err`.
    pub timeout: Duration,
}

/// How the ping shells out to the claude CLI. Production runs the real
/// `claude -p`; tests script the invocation and record its shape.
/// Credentials are never touched: the child inherits its own
/// environment — claude's own login is what authenticates the request,
/// through toker, to the subscription — and toker adds exactly two
/// variables: the base URL and the ping header.
pub trait CommandRunner {
    /// Run the invocation to completion. A non-zero exit is
    /// `Ok(status)` — the caller reads it; `Err` is "the command could
    /// not run at all, or was killed at its timeout". Only the status
    /// comes back: the child's output is a reply, which is completion
    /// content and never read.
    fn run(&self, invocation: &Invocation<'_>) -> Result<ExitStatus>;
}

/// The real runner: std::process, inheriting the parent's environment
/// (so the child keeps whatever login it has) plus the overrides, with
/// its output discarded.
pub struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, invocation: &Invocation<'_>) -> Result<ExitStatus> {
        use std::process::Stdio;
        let what = || format!("{} {}", invocation.program, invocation.args.join(" "));
        // Discarded rather than piped: nothing reads it, and an unread
        // pipe that fills would stall the child until the timeout.
        let mut child = std::process::Command::new(invocation.program)
            .args(invocation.args)
            .envs(
                invocation
                    .env
                    .iter()
                    .map(|(name, value)| (*name, value.as_str())),
            )
            .current_dir(invocation.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("running {}", what()))?;
        let deadline = std::time::Instant::now() + invocation.timeout;
        loop {
            if let Some(status) = child
                .try_wait()
                .with_context(|| format!("waiting on {}", what()))?
            {
                return Ok(status);
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "{} timed out after {} s and was killed",
                    what(),
                    invocation.timeout.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// A fresh, empty working directory for the client, removed on drop.
/// Fresh rather than the temp root itself, so whatever lands in the
/// temp root can never be read as a project for the ping to load.
struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    fn new() -> Result<ScratchDir> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "toker-ping-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path)
            .with_context(|| format!("creating the ping's working dir {}", path.display()))?;
        Ok(ScratchDir(path))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `toker ping-window --slot=hh:mm`: resolve the slot, refuse if more
/// than [`LATENESS_LIMIT_MINUTES`] past its fire time (non-zero exit,
/// the reason said), skip if the ledger shows a window already open
/// (recorded, clean exit), then send one tiny request as a client — `claude
/// -p` with `ANTHROPIC_BASE_URL` at toker's port and
/// `ANTHROPIC_CUSTOM_HEADERS` carrying `ping_header: 1` — and read the
/// ledger back: did a `ping: true` row land on the anthropic sub, and
/// which 5-hour reset did it report. The row's absence is a failure
/// report, not a crash.
///
/// The run (never a refusal) is recorded in the `pings` table with its
/// slot, action, exit code, duration, the boundary predicted from the
/// fire time, the boundary observed, and whether they matched. `now_ms`
/// and `tz` resolve the slot; `sleep` paces the readback's retries.
pub fn ping_window(
    out: &mut dyn Write,
    config: &PingConfig<'_>,
    slot_text: &str,
    now_ms: i64,
    tz: &TimeZone,
    runner: &dyn CommandRunner,
    sleep: &mut dyn FnMut(Duration),
) -> Result<()> {
    let Some(slot) = parse_slot(slot_text) else {
        bail!("--slot must be hh:mm — got {slot_text:?}");
    };
    let Some(occurrence) = last_occurrence_ms(now_ms, &slot, tz) else {
        bail!("the slot could not be resolved on this clock");
    };
    let fire = scheduled_fire_ms(occurrence);
    let lateness = now_ms - fire;
    let slot_hhmm = slot.hhmm();

    if lateness > LATENESS_LIMIT_MINUTES * 60_000 {
        let minutes = (lateness + 59_999) / 60_000;
        let fire_hhmm = hhmm_of(fire, tz);
        let reason = format!(
            "slot {slot_hhmm}: refusing — {minutes} min past the {fire_hhmm} fire time \
             (guard {LATENESS_LIMIT_MINUTES} min): the window would be mostly spent"
        );
        say(out, &reason)?;
        bail!("{reason}");
    }

    // A ping into a window that is already running is a no-op costing
    // a few tokens, but it is not nothing, and it is recorded as if it
    // had opened one. So skip while any measurement says a window runs
    // past now. The decision is lopsided the other way, though: a skip
    // that was wrong forfeits the boundary, so anything the ledger
    // cannot answer resolves to "ping" and records itself as assumed.
    let store = Store::open(config.db_path);
    // A reading logged more than one window ago cannot name a reset
    // still ahead (a window ends at most 5 h after any request in it);
    // the extra step is slack for the two clocks disagreeing.
    let lookback = now_ms - WINDOW_MS - ANCHOR_STEP_MS;
    let open_until = match &store {
        Ok(store) => store
            .furthest_reset5h(SUB_PROVIDER, lookback, false)
            .map_err(anyhow::Error::from),
        Err(error) => Err(anyhow::anyhow!("opening it: {error}")),
    };
    let assumed = match open_until {
        Ok(Some(reset_s)) if reset_s.saturating_mul(1000) > now_ms => {
            let reset_ms = reset_s.saturating_mul(1000);
            say(
                out,
                &format!(
                    "slot {slot_hhmm}: no ping — a window is already open until {}",
                    hhmm_of(reset_ms, tz)
                ),
            )?;
            if let Ok(store) = &store
                && let Err(error) = store.record_ping(&PingRecord {
                    id: None,
                    ts_ms: now_ms,
                    exit_code: None,
                    duration_ms: None,
                    boundary_ms: None,
                    slot: Some(slot_hhmm.clone()),
                    action: Some(PingAction::Skip),
                    observed_ms: Some(reset_ms),
                    verified: None,
                    assumed: None,
                })
            {
                say(
                    out,
                    &format!("slot {slot_hhmm}: recording the skip failed: {error}"),
                )?;
            }
            return Ok(());
        }
        Ok(Some(reset_s)) => {
            say(
                out,
                &format!(
                    "slot {slot_hhmm}: the last window ended at {}",
                    hhmm_of(reset_s.saturating_mul(1000), tz)
                ),
            )?;
            false
        }
        Ok(None) => {
            say(
                out,
                &format!(
                    "slot {slot_hhmm}: no window reading in the ledger, assuming none is open"
                ),
            )?;
            true
        }
        Err(error) => {
            say(
                out,
                &format!(
                    "slot {slot_hhmm}: the ledger could not be read ({error:#}), \
                     assuming no window is open"
                ),
            )?;
            true
        }
    };

    // Predicted from when the ping fires, which is when the request
    // that anchors the window goes out — not from the slot.
    let boundary = window_boundary_ms(now_ms);
    say(
        out,
        &format!(
            "slot {slot_hhmm}: opening the quota window (scheduled {}, expecting it to end {})",
            hhmm_of(fire, tz),
            hhmm_of(boundary, tz),
        ),
    )?;
    if lateness > 0 {
        say(
            out,
            &format!(
                "slot {slot_hhmm}: running {} min late (within the {LATENESS_LIMIT_MINUTES} min guard)",
                (lateness + 59_999) / 60_000,
            ),
        )?;
    }

    // The client: claude's own login authenticates; toker adds the base
    // URL and the ping header (the frozen `Name: 1` literal is_ping
    // reads) and nothing else.
    let env: Vec<(&str, String)> = vec![
        ("ANTHROPIC_BASE_URL", anthropic_base_url(config.port)),
        (
            "ANTHROPIC_CUSTOM_HEADERS",
            format!("{}: 1", config.ping_header),
        ),
    ];
    // The cheapest request that opens a window: the smallest model, and
    // --strict-mcp-config to skip the configured MCP servers, whose tool
    // definitions the ping has no use for (measured by the predecessor:
    // 56,583 fresh tokens down to 45,096). The rest is Claude Code's own
    // system prompt and global memory, which a ping cannot shed — the
    // credentials it needs live in the config dir it would have to
    // abandon to do so.
    let args = [
        "-p",
        PING_PROMPT,
        "--model",
        config.model,
        "--strict-mcp-config",
    ];
    let started = std::time::Instant::now();
    let output = ScratchDir::new().and_then(|cwd| {
        runner.run(&Invocation {
            program: config.claude,
            args: &args,
            env: &env,
            cwd: &cwd.0,
            timeout: PING_TIMEOUT,
        })
    });
    let duration_ms = started.elapsed().as_millis() as i64;
    let exit_code = output.as_ref().ok().and_then(|status| status.code());
    let completed = matches!(&output, Ok(status) if status.success());
    // Deliberately only the exit code, never the output: a reply is
    // completion content, which nothing toker writes may hold.
    match &output {
        Ok(status) => say(
            out,
            &format!(
                "slot {slot_hhmm}: claude exited {}",
                status.code().unwrap_or(-1)
            ),
        )?,
        Err(error) => say(
            out,
            &format!("slot {slot_hhmm}: claude could not run: {error:#}"),
        )?,
    }

    // The readback: the row lands when the response completes
    // server-side, which can be a moment after the client exits — poll
    // briefly rather than declare failure on a race. A store that
    // cannot be read at all is a real error. Only the sub's rows count:
    // a ping the router sent anywhere else opened no window.
    let store =
        store.with_context(|| format!("opening the ledger at {}", config.db_path.display()))?;
    let mut landed = false;
    for attempt in 0..READBACK_ATTEMPTS {
        if attempt > 0 {
            sleep(READBACK_INTERVAL);
        }
        if let Ok(true) = store.ping_landed(SUB_PROVIDER, now_ms) {
            landed = true;
            break;
        }
    }
    // The boundary the API gave, read off the rows the ping produced:
    // a prediction printed as a fact is the predecessor's recurring
    // failure mode. `since` is what keeps it honest — the ledger already
    // holds the previous window's reset, and without the cutoff a ping
    // that changed nothing would report that as its own.
    let observed = if landed {
        store
            .furthest_reset5h(SUB_PROVIDER, now_ms, true)
            .ok()
            .flatten()
            .map(|reset_s| reset_s.saturating_mul(1000))
    } else {
        None
    };
    let verified = observed.map(|observed| observed == boundary);

    if let Err(error) = store.record_ping(&PingRecord {
        id: None,
        ts_ms: now_ms,
        exit_code: exit_code.map(i64::from),
        duration_ms: Some(duration_ms),
        boundary_ms: Some(boundary),
        slot: Some(slot_hhmm.clone()),
        action: Some(if completed {
            PingAction::Ping
        } else {
            PingAction::Failed
        }),
        observed_ms: observed,
        verified,
        assumed: Some(assumed),
    }) {
        say(
            out,
            &format!("slot {slot_hhmm}: recording the ping run failed: {error}"),
        )?;
    }

    if !landed {
        say(
            out,
            &format!(
                "slot {slot_hhmm}: no ping row landed in the ledger — the window did not open"
            ),
        )?;
        bail!("no ping row landed in the ledger — the window did not open");
    }
    match observed {
        Some(observed) if observed == boundary => say(
            out,
            &format!(
                "slot {slot_hhmm}: the ledger confirms the window is open until {}",
                hhmm_of(observed, tz)
            ),
        )?,
        // A mismatch means the measured rule has moved. Say so rather
        // than quietly reporting the reading and leaving the prediction
        // wrong forever.
        Some(observed) => say(
            out,
            &format!(
                "slot {slot_hhmm}: the window is open until {}, not the expected {} — \
                 the floor-to-10-minutes rule may no longer hold",
                hhmm_of(observed, tz),
                hhmm_of(boundary, tz),
            ),
        )?,
        None => say(
            out,
            &format!(
                "slot {slot_hhmm}: the ping row landed without a 5-hour reset — \
                 boundary unverified, expected {}",
                hhmm_of(boundary, tz)
            ),
        )?,
    }
    if !completed {
        bail!("claude did not exit cleanly, though its ping row landed");
    }
    Ok(())
}

/// One line to the verb's output (stdout in production, the captured
/// buffer in tests).
fn say(out: &mut dyn Write, line: &str) -> Result<()> {
    writeln!(out, "{line}").context("writing the verb's output")
}

#[cfg(test)]
mod tests {
    use super::{
        ANCHOR_STEP_MS, CommandRunner, HOLD_UNIT_FOR, LATENESS_LIMIT_MINUTES, PING_DELAY_MINUTES,
        WAKE_ARM_NOTE, WINDOW_MS, hold_lock, last_occurrence_ms, parse_hold_minutes, parse_slot,
        parse_slot_list, scheduled_fire_ms, slot_plus_minutes, window_boundary_ms,
    };
    use crate::middleware::awake::{InhibitCommand, InhibitLock, LockSpawner};
    use crate::setup::test_dir;
    use crate::store::{PingAction, PingRecord, RequestRow, Store};
    use jiff::tz::TimeZone;
    use std::process::ExitStatus;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    fn utc() -> TimeZone {
        TimeZone::get("UTC").expect("UTC is always present in the tzdb")
    }

    /// A fixed UTC day's clock time, as epoch ms.
    fn utc_ms(hour: i8, minute: i8) -> i64 {
        jiff::civil::date(2026, 10, 5)
            .at(hour, minute, 0, 0)
            .to_zoned(utc())
            .expect("a valid civil datetime")
            .timestamp()
            .as_millisecond()
    }

    // ── the parsers ───────────────────────────────────────────────

    #[test]
    fn slots_parse_strictly() {
        for good in ["00:00", "09:00", "23:59", "12:30"] {
            assert_eq!(parse_slot(good).expect(good).hhmm(), good);
        }
        // Two digits each, colon-separated, in range — nothing else.
        for bad in [
            "9:00", "09:0", "0900", "09:60", "24:00", "09:00:", ":09:00", " 09:00", "09:00 ", "",
            "09-00", "aa:bb", "09:0a",
        ] {
            assert_eq!(parse_slot(bad), None, "{bad:?} is not a slot");
        }
        assert_eq!(
            parse_slot("09:07"),
            Some(super::Slot { hour: 9, minute: 7 })
        );
    }

    #[test]
    fn hold_minutes_parse_strictly() {
        assert_eq!(parse_hold_minutes("15m"), Some(15.0));
        assert_eq!(parse_hold_minutes("15"), Some(15.0));
        assert_eq!(parse_hold_minutes("0.5"), Some(0.5));
        assert_eq!(parse_hold_minutes("0.5m"), Some(0.5));
        // The unit's pinned span parses.
        assert_eq!(parse_hold_minutes(HOLD_UNIT_FOR), Some(15.0));
        for bad in [
            "", "m", "15 m", "15M", "15min", "-1", "-1m", "0", "0m", "abc", "1e", "NaN", "inf",
        ] {
            assert_eq!(parse_hold_minutes(bad), None, "{bad:?} is not a span");
        }
    }

    #[test]
    fn slot_lists_split_dedupe_and_reject() {
        assert_eq!(
            parse_slot_list("09:00, 12:30").expect("comma and space"),
            vec!["09:00".to_owned(), "12:30".to_owned()]
        );
        assert_eq!(
            parse_slot_list("09:00 12:30\t18:45").expect("any whitespace"),
            vec!["09:00", "12:30", "18:45"]
        );
        // Duplicates collapse, first position wins.
        assert_eq!(
            parse_slot_list("12:30, 09:00, 12:30").expect("dedupe"),
            vec!["12:30".to_owned(), "09:00".to_owned()]
        );
        // Empty — however padded — is no slots (the wizard's silent skip).
        for empty in ["", "  ", " , "] {
            assert_eq!(parse_slot_list(empty).expect("empty"), Vec::<String>::new());
        }
        // The first offending token is named, for the re-ask.
        assert_eq!(parse_slot_list("09:00, 9:30").unwrap_err(), "9:30");
        assert_eq!(parse_slot_list("09:00;12:30").unwrap_err(), "09:00;12:30");
    }

    #[test]
    fn slot_plus_wraps_past_midnight() {
        let at = |hh, mm| super::Slot {
            hour: hh,
            minute: mm,
        };
        assert_eq!(
            slot_plus_minutes(&at(9, 0), PING_DELAY_MINUTES).hhmm(),
            "09:11"
        );
        assert_eq!(slot_plus_minutes(&at(9, 55), 11).hhmm(), "10:06");
        // The wrap: 23:55 + 11 m is 00:06 — daily timers make that the
        // next day, exactly 11 m after the slot.
        assert_eq!(slot_plus_minutes(&at(23, 55), 11).hhmm(), "00:06");
        assert_eq!(slot_plus_minutes(&at(0, 0), 0).hhmm(), "00:00");
    }

    // ── occurrence, fire, boundary ─────────────────────────────────

    #[test]
    fn the_most_recent_occurrence_wins_and_weekdays_fill_before_the_slot() {
        // The fixed day is 2026-10-05, a MONDAY — the weekday walk
        // shows in every before-the-slot case.
        let slot = parse_slot("09:00").expect("slot");
        // After today's slot: today's.
        assert_eq!(
            last_occurrence_ms(utc_ms(9, 5), &slot, &utc()),
            Some(utc_ms(9, 0))
        );
        // Exactly at the slot: today's (at-or-before).
        assert_eq!(
            last_occurrence_ms(utc_ms(9, 0), &slot, &utc()),
            Some(utc_ms(9, 0))
        );
        // Before today's slot: the previous WEEKDAY's — Sunday and
        // Saturday are skipped (the timers never fire then).
        assert_eq!(
            last_occurrence_ms(utc_ms(8, 59), &slot, &utc()),
            Some(utc_ms(9, 0) - 3 * 86_400_000),
            "Monday 08:59 resolves FRIDAY's 09:00"
        );
        // Midnight edge: the day's first minute.
        let midnight = parse_slot("00:00").expect("slot");
        assert_eq!(
            last_occurrence_ms(utc_ms(0, 0), &midnight, &utc()),
            Some(utc_ms(0, 0))
        );
    }

    #[test]
    fn weekends_resolve_to_fridays_slot() {
        // 2026-10-03 is a Saturday, 2026-10-04 a Sunday, 2026-10-02 the
        // Friday they walk back to. A weekend run has no occurrence of
        // its own — the guard reads it as long past for Friday's
        // slot, which is exactly right: weekends have no ping.
        let slot = parse_slot("09:00").expect("slot");
        let day = |d: i8, h: i8, m: i8| {
            jiff::civil::date(2026, 10, d)
                .at(h, m, 0, 0)
                .to_zoned(utc())
                .expect("valid")
                .timestamp()
                .as_millisecond()
        };
        assert_eq!(
            last_occurrence_ms(day(3, 10, 0), &slot, &utc()),
            Some(day(2, 9, 0))
        );
        assert_eq!(
            last_occurrence_ms(day(4, 23, 0), &slot, &utc()),
            Some(day(2, 9, 0))
        );
        // Friday itself: its own slot, once past.
        assert_eq!(
            last_occurrence_ms(day(2, 10, 0), &slot, &utc()),
            Some(day(2, 9, 0))
        );
        // Early Friday (before the slot): THURSDAY's.
        assert_eq!(
            last_occurrence_ms(day(2, 8, 0), &slot, &utc()),
            Some(day(1, 9, 0))
        );
    }

    #[test]
    fn the_fire_time_is_the_occurrence_plus_the_delay() {
        assert_eq!(scheduled_fire_ms(utc_ms(9, 0)), utc_ms(9, 11));
        assert_eq!(
            scheduled_fire_ms(0),
            PING_DELAY_MINUTES * 60_000,
            "pure arithmetic on the occurrence"
        );
    }

    #[test]
    fn the_boundary_floors_the_fire_time_to_ten_minutes_and_adds_five_hours() {
        // An on-grid time: its own time + 5 h.
        assert_eq!(window_boundary_ms(utc_ms(9, 0)), utc_ms(14, 0));
        // An off-grid time: floored to the grid first.
        assert_eq!(window_boundary_ms(utc_ms(9, 7)), utc_ms(14, 0));
        // The default schedule: the 07:20 slot's ping fires at 07:31 and
        // opens a window ending 12:30, and the 12:20 slot's fires at
        // 12:31, ending 17:30. Flooring the SLOT instead would say 12:20
        // and 17:20 — the bug this replaced.
        let morning = scheduled_fire_ms(utc_ms(7, 20));
        assert_eq!(morning, utc_ms(7, 31));
        assert_eq!(window_boundary_ms(morning), utc_ms(12, 30));
        let afternoon = scheduled_fire_ms(utc_ms(12, 20));
        assert_eq!(window_boundary_ms(afternoon), utc_ms(17, 30));
        assert_eq!(window_boundary_ms(utc_ms(9, 59)), utc_ms(14, 50));
        assert_eq!(
            window_boundary_ms(utc_ms(23, 59)),
            utc_ms(4, 50) + 86_400_000
        );
        // The constants are the measured ones.
        assert_eq!(WINDOW_MS, 5 * 3_600_000);
        assert_eq!(ANCHOR_STEP_MS, 10 * 60_000);
        assert_eq!(PING_DELAY_MINUTES, 11);
        assert_eq!(LATENESS_LIMIT_MINUTES, 10);
        assert_eq!(HOLD_UNIT_FOR, "15m");
    }

    #[test]
    fn the_occurrence_follows_the_zone_not_utc() {
        // 2026-10-05 20:30 in Auckland (NZDT, +13) is 07:30 UTC: the
        // 09:00 slot's most recent occurrence is Auckland's 09:00 this
        // morning — which was 20:00 UTC the previous day.
        let auckland = TimeZone::get("Pacific/Auckland").expect("IANA zone");
        let auckland_ms = |h: i8, m: i8| {
            jiff::civil::date(2026, 10, 5)
                .at(h, m, 0, 0)
                .to_zoned(auckland.clone())
                .expect("valid")
                .timestamp()
                .as_millisecond()
        };
        let slot = parse_slot("09:00").expect("slot");
        let now = auckland_ms(20, 30);
        assert_eq!(
            last_occurrence_ms(now, &slot, &auckland),
            Some(auckland_ms(9, 0))
        );
        // The same instant in UTC resolves the same wall-clock slot to
        // a different occurrence — the zone is a real input. In UTC it
        // is still Monday morning, before the slot, so the walk goes
        // to the previous weekday: FRIDAY's 09:00.
        let now_utc = jiff::Timestamp::from_millisecond(now)
            .expect("valid")
            .to_zoned(utc())
            .timestamp()
            .as_millisecond();
        assert_eq!(
            last_occurrence_ms(now_utc, &slot, &utc()),
            Some(utc_ms(9, 0) - 3 * 86_400_000)
        );
    }

    #[test]
    fn wake_arm_is_a_documented_no_op() {
        // The verb's whole contract: it says where wake is owned, and
        // never arms anything.
        assert!(WAKE_ARM_NOTE.contains("toker-wake.timer"));
        assert!(WAKE_ARM_NOTE.contains("nothing to arm"));
    }

    // ── hold ───────────────────────────────────────────────────────

    /// A spawner whose takes and kills the test counts (the awake
    /// module's fake, minimal): no real lock is ever taken.
    #[derive(Default, Clone)]
    struct FakeSpawner {
        attempts: Arc<AtomicUsize>,
        kills: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
    }

    impl LockSpawner for FakeSpawner {
        fn spawn(&mut self, _command: &InhibitCommand) -> Result<Box<dyn InhibitLock>, String> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err("no session bus".to_owned());
            }
            Ok(Box::new(FakeLock {
                kills: self.kills.clone(),
            }))
        }
    }

    struct FakeLock {
        kills: Arc<AtomicUsize>,
    }

    impl InhibitLock for FakeLock {
        fn kill(&mut self) {
            self.kills.fetch_add(1, Ordering::SeqCst);
        }
        fn exited(&mut self) -> bool {
            false
        }
    }

    /// A platform command for the hold tests — the table's shape;
    /// nothing real is ever spawned.
    fn hold_command() -> Option<InhibitCommand> {
        crate::middleware::awake::inhibit_command("linux", None, |_| true, "toker", "why", 4242)
    }

    #[test]
    fn hold_takes_the_lock_for_the_span_then_releases_it() {
        let fake = FakeSpawner::default();
        let slept: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
        let mut out = Vec::new();
        let mut sleep = |span: Duration| slept.lock().unwrap().push(span);

        hold_lock(&mut out, 0.5, hold_command(), &mut fake.clone(), &mut sleep)
            .expect("the hold takes and releases");

        assert_eq!(
            fake.attempts.load(Ordering::SeqCst),
            1,
            "one inhibitor taken"
        );
        assert_eq!(fake.kills.load(Ordering::SeqCst), 1, "released by kill");
        // The span is exactly the parsed minutes (fractional minutes
        // are the seam the tests and short manual holds use).
        assert_eq!(*slept.lock().unwrap(), vec![Duration::from_millis(30_000)]);
        let out = String::from_utf8(out).expect("utf-8");
        assert_eq!(
            out, "holding the idle-sleep lock for 0.5 minutes\nreleased the idle-sleep lock\n",
            "the hold and the unhold are printed"
        );
    }

    #[test]
    fn hold_without_a_platform_lock_or_with_a_failed_spawn_reports_it() {
        let mut out = Vec::new();
        let error = hold_lock(
            &mut out,
            15.0,
            None,
            &mut FakeSpawner::default(),
            &mut |_| {},
        )
        .expect_err("no platform lock");
        assert!(
            format!("{error:#}").contains("no idle-sleep lock"),
            "{error:#}"
        );

        let fake = FakeSpawner::default();
        fake.fail.store(true, Ordering::SeqCst);
        let mut out = Vec::new();
        let error = hold_lock(
            &mut out,
            15.0,
            hold_command(),
            &mut fake.clone(),
            &mut |_| {},
        )
        .expect_err("the spawn failed");
        assert!(
            format!("{error:#}").contains("sleep lock unavailable: no session bus"),
            "{error:#}"
        );
        // Nothing was held, so nothing is printed as held.
        assert_eq!(fake.kills.load(Ordering::SeqCst), 0);
        assert!(out.is_empty());
    }

    // ── ping-window ────────────────────────────────────────────────

    /// One recorded client invocation: the program, its args, the env
    /// overrides it was given, its working dir (and whether that was an
    /// existing, empty directory when it ran), and its timeout.
    #[derive(Clone)]
    struct ClientCall {
        program: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd: std::path::PathBuf,
        cwd_fresh: bool,
        timeout: Duration,
    }

    /// The scripted client: records every invocation's shape, answers
    /// with a fixed exit code, and lands the rows its request would have
    /// produced — while it runs, as the daemon would, so the pre-ping
    /// skip decision never sees them. No claude is ever run.
    struct ScriptedClient {
        calls: Mutex<Vec<ClientCall>>,
        exit_code: i32,
        fail: bool,
        lands: Option<(std::path::PathBuf, Vec<RequestRow>)>,
    }

    impl ScriptedClient {
        fn new(exit_code: i32) -> ScriptedClient {
            ScriptedClient {
                calls: Mutex::new(Vec::new()),
                exit_code,
                fail: false,
                lands: None,
            }
        }

        /// A client whose request lands `rows` on the ledger at `db`.
        fn landing(exit_code: i32, db: &std::path::Path, rows: Vec<RequestRow>) -> ScriptedClient {
            ScriptedClient {
                lands: Some((db.to_owned(), rows)),
                ..ScriptedClient::new(exit_code)
            }
        }

        /// A client whose request lands one sub ping row at `ts_ms`
        /// reporting `reset_ms`.
        fn landing_ping(db: &std::path::Path, ts_ms: i64, reset_ms: i64) -> ScriptedClient {
            ScriptedClient::landing(0, db, vec![ping_row(ts_ms, reset_ms)])
        }

        fn calls(&self) -> Vec<ClientCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for ScriptedClient {
        fn run(&self, invocation: &super::Invocation<'_>) -> anyhow::Result<ExitStatus> {
            self.calls.lock().unwrap().push(ClientCall {
                program: invocation.program.to_owned(),
                args: invocation.args.iter().map(|arg| arg.to_string()).collect(),
                env: invocation
                    .env
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), value.clone()))
                    .collect(),
                cwd: invocation.cwd.to_owned(),
                cwd_fresh: std::fs::read_dir(invocation.cwd)
                    .is_ok_and(|mut entries| entries.next().is_none()),
                timeout: invocation.timeout,
            });
            if self.fail {
                anyhow::bail!("claude: command not found");
            }
            if let Some((db, rows)) = &self.lands {
                for row in rows {
                    seed(db, row);
                }
            }
            use std::os::unix::process::ExitStatusExt;
            // A wait status, not a code: the code rides the high byte.
            Ok(ExitStatus::from_raw(self.exit_code << 8))
        }
    }

    /// A ledger row with only `ts_ms` — every other column NULL (the
    /// store tests' shape).
    fn bare_row() -> RequestRow {
        RequestRow {
            id: None,
            ts_ms: 0,
            duration_ms: None,
            kind: None,
            frontend: None,
            provider: None,
            route: None,
            session_id: None,
            ping: None,
            model: None,
            raw_model: None,
            requested_model: None,
            effective_model: None,
            input: None,
            cache_read: None,
            cache_write_total: None,
            cache_write_5m: None,
            cache_write_1h: None,
            output: None,
            reasoning: None,
            iterations: None,
            web_searches: None,
            code_execs: None,
            ttl_split_known: None,
            usage_presence: None,
            usage_raw: None,
            cost_usd: None,
            cost_kind: None,
            rate_limits: None,
            req_bytes: None,
            req_messages: None,
            req_tools: None,
            tools_hash: None,
            system_chars: None,
            system_hash: None,
            system_blocks: None,
            system_messages: None,
            compact_generations: None,
            summarising: None,
            system_change: None,
            system_ladder: None,
            system_tail: None,
            gate_on: None,
            cold_on: None,
            forced_from: None,
            forced_to: None,
            downgraded_from: None,
            downgraded_to: None,
            cache_stripped: None,
            system_merged: None,
            model_mappings: None,
            drift_digest: None,
            status: None,
            error_type: None,
            retry_after_ms: None,
            extra: None,
            betas: None,
            geo: None,
            fast: None,
        }
    }

    /// A `ping: true` row at `ts_ms`, served by the anthropic sub and
    /// reporting a 5-hour reset at `reset_ms`.
    fn ping_row(ts_ms: i64, reset_ms: i64) -> RequestRow {
        let mut row = sub_row(ts_ms, reset_ms);
        row.ping = Some(true);
        row
    }

    /// An ordinary session's sub row at `ts_ms` reporting a 5-hour reset
    /// at `reset_ms`.
    fn sub_row(ts_ms: i64, reset_ms: i64) -> RequestRow {
        let mut row = bare_row();
        row.ts_ms = ts_ms;
        row.provider = Some("anthropic_sub".to_owned());
        row.rate_limits = Some(serde_json::json!({"util5h": 0.0, "reset5h": reset_ms / 1000}));
        row
    }

    /// A scratch ledger with one sub ping row (see [`ping_row`]).
    fn seeded_ping_row(db: &std::path::Path, ts_ms: i64, reset_ms: i64) {
        seed(db, &ping_row(ts_ms, reset_ms));
    }

    fn seed(db: &std::path::Path, row: &RequestRow) {
        Store::open(db)
            .expect("open the scratch ledger")
            .record_request(row)
            .expect("seed the row");
    }

    fn no_sleep() -> impl FnMut(Duration) {
        |_| {}
    }

    /// The ping verb's config view for a scratch ledger and the given
    /// header name (the port is the fixed test port).
    fn ping_config<'a>(db: &'a std::path::Path, header: &'a str) -> super::PingConfig<'a> {
        super::PingConfig {
            db_path: db,
            port: PORT,
            ping_header: header,
            claude: "claude",
            model: super::PING_MODEL,
        }
    }

    /// The slot resolved for a `now` in the fixed UTC day, so the tests
    /// share one set of clock facts (fire = 09:11, boundary = 14:00).
    const PORT: u16 = 18_199;

    #[test]
    fn a_ping_runs_the_client_with_the_configured_env_and_confirms_the_row() {
        let dir = test_dir("ping-lands");
        let db = dir.join("toker.db");
        let now = utc_ms(9, 12); // One minute past the 09:11 fire time.
        // The row lands a moment after the run starts, reporting the
        // reset the rule predicts from 09:12: floor to 09:10, plus 5 h.
        let client = ScriptedClient::landing_ping(&db, now + 1_000, utc_ms(14, 10));
        let mut out = Vec::new();
        let mut sleep = no_sleep();
        super::ping_window(
            &mut out,
            &ping_config(&db, "x-my-ping"),
            "09:00",
            now,
            &utc(),
            &client,
            &mut sleep,
        )
        .expect("the ping lands");

        // The invocation's shape: claude -p with a tiny prompt, on the
        // cheapest model with no MCP servers loaded, from a fresh empty
        // directory that is gone afterwards, bounded at 120 s — and the
        // two env vars: the base URL from the port, the ping header by
        // the CONFIGURED name carrying the frozen literal 1.
        let calls = client.calls();
        assert_eq!(calls.len(), 1, "exactly one tiny request");
        assert_eq!(calls[0].program, "claude");
        assert_eq!(
            calls[0].args,
            vec![
                "-p",
                "Reply with the single word: ok",
                "--model",
                "haiku",
                "--strict-mcp-config"
            ]
        );
        assert!(calls[0].cwd_fresh, "an empty dir: no project CLAUDE.md");
        assert!(calls[0].cwd.starts_with(std::env::temp_dir()));
        assert_ne!(calls[0].cwd, std::env::temp_dir(), "a dir of its own");
        assert!(!calls[0].cwd.exists(), "removed after the run");
        assert_eq!(calls[0].timeout, Duration::from_secs(120));
        let env: std::collections::BTreeMap<&str, &str> = calls[0]
            .env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL").map(|v| v.to_string()),
            Some(format!("http://127.0.0.1:{PORT}")),
            "the base URL points the client at toker"
        );
        assert_eq!(
            env.get("ANTHROPIC_CUSTOM_HEADERS").map(|v| v.to_string()),
            Some("x-my-ping: 1".to_owned()),
            "the configured ping header, in claude's Name: Value form"
        );
        assert_eq!(
            env.len(),
            2,
            "nothing else is set — credentials are never touched"
        );

        // The outcome is printed, and the run is recorded with the
        // boundary it predicted from the FIRE time (09:12 → 14:10, not
        // the slot's 14:00) and the one the ledger reported back.
        let out = String::from_utf8(out).expect("utf-8");
        assert!(
            out.contains(
                "slot 09:00: opening the quota window (scheduled 09:11, expecting it to end 14:10)"
            ),
            "{out}"
        );
        assert!(out.contains("slot 09:00: running 1 min late"), "{out}");
        assert!(out.contains("claude exited 0"), "{out}");
        assert!(
            out.contains("the ledger confirms the window is open until 14:10"),
            "{out}"
        );
        let pings: Vec<PingRecord> = Store::open(&db)
            .expect("open")
            .pings_since(0, 10)
            .expect("pings");
        assert_eq!(pings.len(), 1);
        assert_eq!(pings[0].exit_code, Some(0));
        assert_eq!(pings[0].boundary_ms, Some(utc_ms(14, 10)));
        assert_eq!(pings[0].observed_ms, Some(utc_ms(14, 10)));
        assert_eq!(pings[0].verified, Some(true));
        assert_eq!(pings[0].action, Some(PingAction::Ping));
        assert_eq!(pings[0].slot.as_deref(), Some("09:00"));
        assert_eq!(pings[0].ts_ms, now);
    }

    #[test]
    fn a_boundary_other_than_predicted_is_recorded_unverified_and_said() {
        let dir = test_dir("ping-mismatch");
        let db = dir.join("toker.db");
        let now = utc_ms(9, 12);
        // The API anchored somewhere else: the rule has moved.
        let client = ScriptedClient::landing_ping(&db, now + 1_000, utc_ms(14, 0));

        let mut out = Vec::new();
        let mut sleep = no_sleep();
        super::ping_window(
            &mut out,
            &ping_config(&db, "x-toker-ping"),
            "09:00",
            now,
            &utc(),
            &client,
            &mut sleep,
        )
        .expect("the window did open, just not where predicted");

        let out = String::from_utf8(out).expect("utf-8");
        assert!(
            out.contains("the window is open until 14:00, not the expected 14:10"),
            "{out}"
        );
        let pings = Store::open(&db)
            .expect("open")
            .pings_since(0, 10)
            .expect("pings");
        assert_eq!(pings[0].boundary_ms, Some(utc_ms(14, 10)), "the prediction");
        assert_eq!(pings[0].observed_ms, Some(utc_ms(14, 0)), "the reading");
        assert_eq!(pings[0].verified, Some(false));
    }

    #[test]
    fn the_readback_reads_only_what_this_ping_produced_on_the_sub() {
        let dir = test_dir("ping-readback-scope");
        let db = dir.join("toker.db");
        let now = utc_ms(9, 12);
        // What the request produced, none of which is the sub opening
        // the window for this ping: a ping row routed to the plain API
        // (it opened no window), a sub row that is not a measurement (a
        // gate's copy of the meters), and an untagged session's row.
        let mut api = ping_row(now + 1_000, utc_ms(14, 10));
        api.provider = Some("anthropic_api".to_owned());
        let mut blocked = ping_row(now + 1_000, utc_ms(14, 10));
        blocked.kind = Some(crate::store::RowKind::Blocked);
        let session = sub_row(now + 1_000, utc_ms(14, 10));
        let client = ScriptedClient::landing(0, &db, vec![api, blocked, session]);

        let mut out = Vec::new();
        let mut sleep = no_sleep();
        let error = super::ping_window(
            &mut out,
            &ping_config(&db, "x-toker-ping"),
            "09:00",
            now,
            &utc(),
            &client,
            &mut sleep,
        )
        .expect_err("nothing the ping produced on the sub landed");
        assert!(format!("{error:#}").contains("did not open"), "{error:#}");
        let pings = Store::open(&db)
            .expect("open")
            .pings_since(0, 10)
            .expect("pings");
        assert_eq!(pings[0].observed_ms, None, "no reading was invented");
        assert_eq!(pings[0].verified, None, "nothing to compare");
    }

    #[test]
    fn a_ping_row_without_a_reset_is_unverified_not_confirmed() {
        let dir = test_dir("ping-no-reset");
        let db = dir.join("toker.db");
        let now = utc_ms(9, 12);
        // The morning's ping, whose window has ended: its reading is
        // already in the ledger, and must not pass for this ping's.
        seeded_ping_row(&db, utc_ms(4, 22), utc_ms(9, 10));
        let mut row = ping_row(now + 1_000, 0);
        row.rate_limits = None;
        let client = ScriptedClient::landing(0, &db, vec![row]);

        let mut out = Vec::new();
        let mut sleep = no_sleep();
        super::ping_window(
            &mut out,
            &ping_config(&db, "x-toker-ping"),
            "09:00",
            now,
            &utc(),
            &client,
            &mut sleep,
        )
        .expect("the row landed");
        let out = String::from_utf8(out).expect("utf-8");
        assert!(
            out.contains("slot 09:00: the last window ended at 09:10"),
            "{out}"
        );
        assert!(out.contains("boundary unverified, expected 14:10"), "{out}");
        let pings = Store::open(&db)
            .expect("open")
            .pings_since(0, 10)
            .expect("pings");
        assert_eq!(pings[0].observed_ms, None);
        assert_eq!(pings[0].verified, None);
    }

    #[test]
    fn a_late_ping_is_refused_without_running_the_client() {
        let dir = test_dir("ping-late");
        let db = dir.join("toker.db");
        // 09:23 is 12 minutes past the 09:11 fire — beyond the guard.
        let now = utc_ms(9, 23);

        let client = ScriptedClient::new(0);
        let mut out = Vec::new();
        let mut sleep = no_sleep();
        let error = super::ping_window(
            &mut out,
            &ping_config(&db, "x-toker-ping"),
            "09:00",
            now,
            &utc(),
            &client,
            &mut sleep,
        )
        .expect_err("the guard refuses");

        // The reason is the refusal's message, and nothing ran: no
        // client call, no ledger open (the db was never created), no
        // recorded ping.
        let reason = format!("{error:#}");
        assert!(reason.contains("refusing"), "{reason}");
        assert!(
            reason.contains("12 min past the 09:11 fire time"),
            "{reason}"
        );
        assert!(reason.contains("guard 10 min"), "{reason}");
        assert!(client.calls().is_empty(), "the client never ran");
        assert!(!db.exists(), "the ledger was never even opened");
        let out = String::from_utf8(out).expect("utf-8");
        assert!(out.contains("slot 09:00: refusing"), "{out}");
    }

    #[test]
    fn the_guard_admits_the_fire_time_and_refuses_only_past_ten_minutes() {
        let dir = test_dir("ping-guard-edges");
        let db = dir.join("toker.db");
        let slot_fire = utc_ms(9, 11);
        // Exactly at the fire time, and exactly at the 10-minute guard:
        // admitted (the guard is strictly MORE than).
        for now in [slot_fire, slot_fire + LATENESS_LIMIT_MINUTES * 60_000] {
            // A fresh ledger each time, or the first run's window would
            // make the second a skip.
            let db = dir.join(format!("toker-{now}.db"));
            let client = ScriptedClient::landing_ping(&db, now + 1_000, window_boundary_ms(now));
            let mut out = Vec::new();
            let mut sleep = no_sleep();
            super::ping_window(
                &mut out,
                &ping_config(&db, "x-toker-ping"),
                "09:00",
                now,
                &utc(),
                &client,
                &mut sleep,
            )
            .expect("within the guard runs");
            assert_eq!(client.calls().len(), 1, "the client ran");
        }
        // One tick past: refused.
        let mut out = Vec::new();
        let mut sleep = no_sleep();
        let error = super::ping_window(
            &mut out,
            &ping_config(&db, "x-toker-ping"),
            "09:00",
            slot_fire + LATENESS_LIMIT_MINUTES * 60_000 + 1,
            &utc(),
            &ScriptedClient::new(0),
            &mut sleep,
        )
        .expect_err("past the guard refuses");
        assert!(format!("{error:#}").contains("refusing"));
    }

    #[test]
    fn an_early_run_reads_as_yesterdays_slot_and_is_refused() {
        // 08:59 is before today's 09:00, so the most recent occurrence
        // is yesterday's — a fire time ~24 h past, far beyond the guard.
        let now = utc_ms(8, 59);
        let mut out = Vec::new();
        let mut sleep = no_sleep();
        let error = super::ping_window(
            &mut out,
            &ping_config(&test_dir("ping-early").join("toker.db"), "x-toker-ping"),
            "09:00",
            now,
            &utc(),
            &ScriptedClient::new(0),
            &mut sleep,
        )
        .expect_err("an early run is a late run for yesterday's slot");
        assert!(format!("{error:#}").contains("refusing"), "{error:#}");
    }

    #[test]
    fn a_missing_row_is_a_failure_report_not_a_crash() {
        let dir = test_dir("ping-no-row");
        let db = dir.join("toker.db");
        let now = utc_ms(9, 12);
        // A ledger with a NON-ping row in the window: not the row the
        // readback wants.
        let mut row = bare_row();
        row.ts_ms = now + 1_000;
        row.ping = None;
        row.provider = Some("anthropic_sub".to_owned());
        seed(&db, &row);

        let mut out = Vec::new();
        let mut sleep = no_sleep();
        let error = super::ping_window(
            &mut out,
            &ping_config(&db, "x-toker-ping"),
            "09:00",
            now,
            &utc(),
            &ScriptedClient::new(1),
            &mut sleep,
        )
        .expect_err("the row never lands");

        // The report names the exit code and the absence; the error is
        // the failure, not a panic.
        let out = String::from_utf8(out).expect("utf-8");
        assert!(out.contains("claude exited 1"), "{out}");
        assert!(
            out.contains("no ping row landed in the ledger — the window did not open"),
            "{out}"
        );
        assert!(format!("{error:#}").contains("did not open"), "{error:#}");
        // The failed run is still recorded.
        let pings: Vec<PingRecord> = Store::open(&db)
            .expect("open")
            .pings_since(0, 10)
            .expect("pings");
        assert_eq!(pings.len(), 1);
        assert_eq!(pings[0].exit_code, Some(1));
        assert_eq!(pings[0].action, Some(PingAction::Failed));
    }

    #[test]
    fn a_client_that_cannot_run_reports_and_still_reads_the_ledger_back() {
        let dir = test_dir("ping-no-client");
        let db = dir.join("toker.db");
        let now = utc_ms(9, 12);
        let mut client = ScriptedClient::new(0);
        client.fail = true;

        let mut out = Vec::new();
        let mut sleep = no_sleep();
        let error = super::ping_window(
            &mut out,
            &ping_config(&db, "x-toker-ping"),
            "09:00",
            now,
            &utc(),
            &client,
            &mut sleep,
        )
        .expect_err("nothing was sent");

        let out = String::from_utf8(out).expect("utf-8");
        assert!(
            out.contains("claude could not run: claude: command not found"),
            "{out}"
        );
        assert!(out.contains("no ping row landed"), "{out}");
        assert!(format!("{error:#}").contains("did not open"), "{error:#}");
    }

    #[test]
    fn the_readback_polls_until_the_row_lands() {
        let dir = test_dir("ping-polls");
        let db = dir.join("toker.db");
        let now = utc_ms(9, 12);
        // The row lands only on the second readback attempt — the
        // daemon writes it a moment after the client exits.
        let db_late = db.clone();
        let mut attempts = 0;
        let mut sleep = move |_span: Duration| {
            attempts += 1;
            if attempts == 1 {
                seeded_ping_row(&db_late, now + 1_000, utc_ms(14, 10));
            }
        };

        let mut out = Vec::new();
        super::ping_window(
            &mut out,
            &ping_config(&db, "x-toker-ping"),
            "09:00",
            now,
            &utc(),
            &ScriptedClient::new(0),
            &mut sleep,
        )
        .expect("the row lands on the retry");

        let out = String::from_utf8(out).expect("utf-8");
        assert!(
            out.contains("the ledger confirms the window is open until 14:10"),
            "{out}"
        );
    }

    #[test]
    fn the_configured_claude_and_model_are_what_runs() {
        let dir = test_dir("ping-overrides");
        let db = dir.join("toker.db");
        let now = utc_ms(9, 12);
        let client = ScriptedClient::landing_ping(&db, now + 1_000, utc_ms(14, 10));
        let mut config = ping_config(&db, "x-toker-ping");
        config.claude = "/opt/claude/bin/claude";
        config.model = "sonnet";
        let mut sleep = no_sleep();
        super::ping_window(
            &mut Vec::new(),
            &config,
            "09:00",
            now,
            &utc(),
            &client,
            &mut sleep,
        )
        .expect("the ping lands");
        let calls = client.calls();
        assert_eq!(calls[0].program, "/opt/claude/bin/claude");
        assert_eq!(calls[0].args[2..4], ["--model", "sonnet"]);
    }

    #[test]
    fn the_process_runner_kills_a_child_at_its_timeout() {
        // A real child, but never claude: a shell that outlives its
        // timeout, and one that exits on its own.
        let cwd = test_dir("ping-runner");
        let slow = super::ProcessCommandRunner
            .run(&super::Invocation {
                program: "/bin/sh",
                args: &["-c", "sleep 5"],
                env: &[],
                cwd: &cwd,
                timeout: Duration::from_millis(200),
            })
            .expect_err("killed at the timeout");
        assert!(format!("{slow:#}").contains("timed out"), "{slow:#}");

        let status = super::ProcessCommandRunner
            .run(&super::Invocation {
                program: "/bin/sh",
                args: &["-c", "test \"$PWD\" = \"$EXPECTED\" && exit 3"],
                env: &[("EXPECTED", cwd.display().to_string())],
                cwd: &cwd,
                timeout: Duration::from_secs(10),
            })
            .expect("it ran");
        assert_eq!(status.code(), Some(3), "ran in the given dir, status back");
    }

    // ── the skip ───────────────────────────────────────────────────

    #[test]
    fn an_open_window_skips_the_ping_and_records_the_skip() {
        let dir = test_dir("ping-skip");
        let db = dir.join("toker.db");
        let now = utc_ms(9, 12);
        // A session opened a window at 08:03, ending 13:00 — and a
        // later row from the same window reports it too, out of order
        // with an earlier, nearer reading. The furthest counts.
        seed(&db, &sub_row(utc_ms(8, 3), utc_ms(13, 0)));
        seed(&db, &sub_row(utc_ms(8, 50), utc_ms(12, 0)));

        let client = ScriptedClient::new(0);
        let mut out = Vec::new();
        let mut sleep = no_sleep();
        super::ping_window(
            &mut out,
            &ping_config(&db, "x-toker-ping"),
            "09:00",
            now,
            &utc(),
            &client,
            &mut sleep,
        )
        .expect("a skip is a clean exit");

        assert!(client.calls().is_empty(), "nothing was sent");
        let out = String::from_utf8(out).expect("utf-8");
        assert!(
            out.contains("slot 09:00: no ping — a window is already open until 13:00"),
            "{out}"
        );
        let pings = Store::open(&db)
            .expect("open")
            .pings_since(0, 10)
            .expect("pings");
        assert_eq!(pings.len(), 1);
        assert_eq!(pings[0].action, Some(PingAction::Skip));
        assert_eq!(pings[0].observed_ms, Some(utc_ms(13, 0)));
        assert_eq!(pings[0].slot.as_deref(), Some("09:00"));
        assert_eq!(pings[0].exit_code, None);
        assert_eq!(pings[0].boundary_ms, None, "nothing was predicted");
    }

    #[test]
    fn readings_that_are_not_the_subs_measurements_never_cause_a_skip() {
        let dir = test_dir("ping-skip-scope");
        let db = dir.join("toker.db");
        let now = utc_ms(9, 12);
        // Every one of these names a reset ahead of now, and none of
        // them is the sub's API telling us a window is open: another
        // backend's row, the gate's stale copy, and a non-numeric
        // reset. The stay-quiet half of the decision.
        let mut api = sub_row(utc_ms(8, 3), utc_ms(13, 0));
        api.provider = Some("anthropic_api".to_owned());
        seed(&db, &api);
        let mut blocked = sub_row(utc_ms(8, 3), utc_ms(13, 0));
        blocked.kind = Some(crate::store::RowKind::Blocked);
        seed(&db, &blocked);
        let mut garbled = sub_row(utc_ms(8, 3), 0);
        garbled.rate_limits = Some(serde_json::json!({"reset5h": "1790000000"}));
        seed(&db, &garbled);

        let client = ScriptedClient::landing_ping(&db, now + 1_000, utc_ms(14, 10));
        let mut out = Vec::new();
        let mut sleep = no_sleep();
        super::ping_window(
            &mut out,
            &ping_config(&db, "x-toker-ping"),
            "09:00",
            now,
            &utc(),
            &client,
            &mut sleep,
        )
        .expect("the ping goes out");
        assert_eq!(client.calls().len(), 1, "the ping was sent");
        let out = String::from_utf8(out).expect("utf-8");
        assert!(
            out.contains("no window reading in the ledger, assuming none is open"),
            "{out}"
        );
        let pings = Store::open(&db)
            .expect("open")
            .pings_since(0, 10)
            .expect("pings");
        assert_eq!(pings[0].action, Some(PingAction::Ping));
        assert_eq!(
            pings[0].assumed,
            Some(true),
            "the decision says it was assumed"
        );
    }

    #[test]
    fn an_ended_window_pings_and_is_not_assumed() {
        let dir = test_dir("ping-ended");
        let db = dir.join("toker.db");
        let now = utc_ms(9, 12);
        seed(&db, &sub_row(utc_ms(4, 5), utc_ms(9, 0)));

        let client = ScriptedClient::landing_ping(&db, now + 1_000, utc_ms(14, 10));
        let mut out = Vec::new();
        let mut sleep = no_sleep();
        super::ping_window(
            &mut out,
            &ping_config(&db, "x-toker-ping"),
            "09:00",
            now,
            &utc(),
            &client,
            &mut sleep,
        )
        .expect("the ping goes out");
        assert_eq!(client.calls().len(), 1);
        let out = String::from_utf8(out).expect("utf-8");
        assert!(out.contains("the last window ended at 09:00"), "{out}");
        let pings = Store::open(&db)
            .expect("open")
            .pings_since(0, 10)
            .expect("pings");
        assert_eq!(pings[0].assumed, Some(false));
        assert_eq!(pings[0].verified, Some(true));
    }

    #[test]
    fn a_bad_slot_argument_is_refused_before_anything_runs() {
        let mut out = Vec::new();
        let mut sleep = no_sleep();
        let client = ScriptedClient::new(0);
        let error = super::ping_window(
            &mut out,
            &ping_config(&test_dir("ping-bad-slot").join("toker.db"), "x-toker-ping"),
            "9:30",
            utc_ms(9, 12),
            &utc(),
            &client,
            &mut sleep,
        )
        .expect_err("the slot argument is validated");
        assert!(
            format!("{error:#}").contains("--slot must be hh:mm"),
            "{error:#}"
        );
        assert!(client.calls().is_empty());
    }
}
