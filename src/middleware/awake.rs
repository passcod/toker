//! The idle-sleep lock — ctp's awake subsystem (plan: "Lane tracking +
//! sleep lock"; ctp awake.mjs + inhibit.mjs + the `evaluateAwake` wiring
//! in proxy.mjs), ported faithfully. Linux v1: ctp's `nextWake` is
//! macOS-only (`pmset repeat` re-arming) and is deliberately NOT ported.
//!
//! Why this exists: desktop idle-suspend kills running agent sessions.
//! While any lane is "live" — its cache would be: 5m or 1h past its last
//! response, per the lane's sticky TTL tier — plus anything in flight,
//! the proxy holds an **idle-only** sleep lock. Deliberately idle-only: a
//! logind `sleep` block would refuse the menu Suspend and is ignored by
//! lid-close anyway (`LidSwitchIgnoreInhibited=yes` is logind's default)
//! — both too strong and not strong enough, measured at work. The user's
//! own Suspend and the lid keep working.
//!
//! Sources, ported:
//!
//! - `decide_awake` — ctp `decideAwake`, awake.mjs:27-47 (pure: live
//!   lanes + in-flight → want-to-hold; ping lanes excluded — a ping
//!   opens a quota window on a timer, and its cache holding the machine
//!   up for an hour would keep a laptop that woke only to ping awake
//!   with nobody at it);
//! - `inhibit_command` / `on_path` — ctp `inhibitCommand` / `onPath`,
//!   inhibit.mjs:19-57 (the platform table; GNOME's idle suspend is
//!   gsd-power's, and gsd-power reads the session manager's inhibitors,
//!   so on GNOME the lock goes where gsd-power is known to look);
//! - the detached-child pattern — ctp inhibit.mjs:93: the child watches
//!   the proxy's PID (`tail --pid=OWNER -f /dev/null`) in its own
//!   process group with null stdio, so the inhibitor lives exactly as
//!   long as the proxy however it dies, and release can take the waiting
//!   child down with the wrapper;
//! - the 5-minute retry backoff — ctp `RETRY_MS`, inhibit.mjs:64:
//!   evaluation runs on every response, and without this a missing
//!   session bus would spawn a process per request;
//! - `AwakeState::evaluate` — ctp's `createInhibitor` hold/release (the
//!   backoff, the once-per-spell complaint, the kill-on-release) plus
//!   the proxy's flip bookkeeping (`evaluateAwake`, proxy.mjs:465-482):
//!   a row only on a held flip, because the row is what separates
//!   "released because the sessions went quiet" from "the lock quietly
//!   stopped working" — `want` differing from `held` is a lock that
//!   could not be taken.
//!
//! The real spawner takes a REAL idle-sleep lock; tests inject fakes
//! through the [`LockSpawner`] trait and never spawn systemd-inhibit or
//! gnome-session-inhibit.

use std::process::Command;

use crate::store::Lane;

use super::cold::ttl_of;

/// How long to wait before trying again after a lock failed to take or
/// died (ctp `RETRY_MS`, inhibit.mjs:64).
pub const RETRY_MS: i64 = 5 * 60_000;

/// How often the lock is re-evaluated on the wall clock (ctp
/// proxy.mjs:490: `setInterval(evaluateAwake, 60_000)`, unref'd).
pub const AWAKE_TICK_MS: u64 = 60_000;

/// The identity the lock shows up under in `systemd-inhibit --who=` /
/// `gnome-session-inhibit --app-id` (ctp: `who: "claude-token-proxy"`).
pub const INHIBIT_WHO: &str = "toker";

/// What the lock says it is for (ctp: `why: "Claude Code sessions are
/// live"`).
pub const INHIBIT_WHY: &str = "agent sessions are live";

// ── the decision (ctp decideAwake) ────────────────────────────────────

/// Whether the machine may idle-suspend right now (ctp `decideAwake`'s
/// `{hold, until, reason}`). Pure; the server owns the lock itself.
///
/// `until` is when the latest live lane's cache expires, epoch
/// milliseconds — or `None` where the hold rests on something without an
/// expiry (a request in flight) or there is no hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwakeDecision {
    /// A live lane or an in-flight request says: hold the lock.
    pub hold: bool,
    /// When the latest live lane expires; `None` for an in-flight-only
    /// hold (ctp: null — it rests on something without an expiry) and for
    /// no hold.
    pub until: Option<i64>,
    /// The count the hold rests on, ctp's exact wording (the fixture pins
    /// it: `"1 live lane"`, `"no live lanes"`).
    pub reason: String,
}

/// Hold or release, from the lane table and the in-flight count (ctp
/// `decideAwake`, awake.mjs:27-47).
///
/// "In use" is read off the lane table rather than a request clock, for
/// the reason every other view here uses lanes: one session interleaves
/// several prefixes, and a two-token title summariser must not stand in
/// for the main agent. A lane is live for as long as its cache would be
/// — five minutes or an hour after its last response — which is also the
/// span within which the session behind it is plausibly coming back.
///
/// Ping lanes never count, and anything in flight holds, because a
/// lane's `at` moves only when a response finishes, and one long turn
/// can outlast a 5-minute tier.
pub fn decide_awake(lanes: &[Lane], in_flight: u64, now: i64) -> AwakeDecision {
    let mut until: Option<i64> = None;
    let mut live: u64 = 0;
    for lane in lanes {
        // A ping lane never counts: it opens a quota window on a timer,
        // and its cache holding the machine up for an hour would keep a
        // laptop that woke only to ping awake with nobody at it.
        if lane.ping == Some(true) {
            continue;
        }
        let at = lane.updated_ms;
        // A future timestamp is a clock that moved, and would hold
        // forever (ctp awake.mjs:37).
        if at > now {
            continue;
        }
        // ctp `ttlOf` (cold.mjs:44, via awake.mjs's import): anything not
        // the 5-minute tier reads as the hour — guessing short would
        // sleep the machine under lanes whose cache is still live.
        let expires = at.saturating_add(ttl_of(lane));
        if expires <= now {
            continue;
        }
        live += 1;
        if until.is_none_or(|current| expires > current) {
            until = Some(expires);
        }
    }

    if in_flight > 0 {
        return AwakeDecision {
            hold: true,
            until: None,
            reason: format!("{in_flight} in flight"),
        };
    }
    if live > 0 {
        return AwakeDecision {
            hold: true,
            until,
            reason: format!("{live} live lane{}", if live == 1 { "" } else { "s" }),
        };
    }
    AwakeDecision {
        hold: false,
        until: None,
        reason: "no live lanes".to_owned(),
    }
}

// ── the platform command table (ctp inhibitCommand / onPath) ───────────

/// One platform's lock-holding command (ctp `inhibitCommand`'s result):
/// the inhibitor plus the PID-watching child that makes the lock die
/// with its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InhibitCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// Whether `bin` is an executable on `PATH` (ctp `onPath`,
/// inhibit.mjs:19-25): each directory checked for an executable file of
/// that name; empty entries skipped, like ctp's `if (!dir) continue`.
pub fn on_path(bin: &str, path: Option<&str>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = path else {
        return false;
    };
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(std::path::Path::new(dir).join(bin)) else {
            continue;
        };
        // ctp checks access(X_OK); any execute bit is the same question
        // in mode form.
        if metadata.permissions().mode() & 0o111 != 0 {
            return true;
        }
    }
    false
}

/// The command that holds the lock until the process `owner_pid` exits,
/// or `None` where this platform offers none (ctp `inhibitCommand`,
/// inhibit.mjs:31-57). Pure: `exists` answers whether a binary is
/// available.
///
/// Linux only in toker v1 (the darwin/`caffeinate` branch is not ported).
/// The child waits on the process that owns it — `tail --pid=OWNER -f
/// /dev/null` — so the lock dies with its owner however that owner dies:
/// under systemd the unit's cgroup kill would cover a crash; run by
/// hand, nothing else would.
pub fn inhibit_command(
    platform: &str,
    xdg_current_desktop: Option<&str>,
    exists: impl Fn(&str) -> bool,
    who: &str,
    why: &str,
    owner_pid: u32,
) -> Option<InhibitCommand> {
    if platform != "linux" || !exists("tail") {
        return None;
    }
    let wait = [
        "tail".to_owned(),
        format!("--pid={owner_pid}"),
        "-f".to_owned(),
        "/dev/null".to_owned(),
    ];

    // GNOME's idle suspend is gsd-power's, and gsd-power reads the
    // session manager's inhibitors — flag 8, "suspend when idle", the
    // one the Caffeine extension takes. Whether it also honours a
    // logind idle lock is not something ctp was able to confirm, so on
    // GNOME the lock goes where gsd-power is known to look.
    // `XDG_CURRENT_DESKTOP` is a colon-separated list (a
    // "ubuntu:GNOME" style union), so membership is per component.
    let gnome = xdg_current_desktop
        .unwrap_or_default()
        .split(':')
        .any(|desktop| desktop == "GNOME");
    if gnome && exists("gnome-session-inhibit") {
        return Some(InhibitCommand {
            program: "gnome-session-inhibit".to_owned(),
            args: [
                "--app-id".to_owned(),
                who.to_owned(),
                "--reason".to_owned(),
                why.to_owned(),
                "--inhibit".to_owned(),
                "suspend".to_owned(),
            ]
            .into_iter()
            .chain(wait)
            .collect(),
        });
    }
    if exists("systemd-inhibit") {
        return Some(InhibitCommand {
            program: "systemd-inhibit".to_owned(),
            args: [
                "--what=idle".to_owned(),
                "--mode=block".to_owned(),
                format!("--who={who}"),
                format!("--why={why}"),
            ]
            .into_iter()
            .chain(wait)
            .collect(),
        });
    }
    None
}

/// The lock command this host offers, probed from the real environment
/// (what ctp's `createInhibitor` builds for itself, inhibit.mjs:70-74:
/// the platform, the desktop, PATH, and the owning PID).
pub fn platform_command(who: &str, why: &str) -> Option<InhibitCommand> {
    let path = std::env::var_os("PATH").map(|p| p.to_string_lossy().into_owned());
    inhibit_command(
        std::env::consts::OS,
        std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
        |bin| on_path(bin, path.as_deref()),
        who,
        why,
        std::process::id(),
    )
}

// ── taking the lock (ctp createInhibitor) ─────────────────────────────

/// A taken lock, as the state machine sees it: a trait so tests inject
/// doubles; the real implementation is the detached inhibitor child.
pub trait InhibitLock: Send {
    /// Release the lock (ctp `release`, inhibit.mjs:106-113): SIGTERM the
    /// child's **whole process group** so the waiting `tail` goes down
    /// with the wrapper — gnome-session-inhibit kills its child on
    /// SIGTERM, and nothing promises the others do, so an orphaned tail
    /// per release would accumulate for as long as the proxy runs.
    fn kill(&mut self);

    /// Whether the child exited since the last look (ctp's `exit`
    /// listener, inhibit.mjs:99-100): a lock that ended on its own — the
    /// session bus went away, or someone killed it — is gone. A
    /// deliberate release clears the child first, so it never reads as
    /// an exit.
    fn exited(&mut self) -> bool;
}

/// How the lock is taken (ctp's `spawn`, inhibit.mjs:93: detached, stdio
/// ignored). The seam tests inject a fake through; the real one spawns a
/// process.
pub trait LockSpawner: Send {
    /// Spawn the inhibitor. `Err` carries the failure's message, ctp's
    /// `sleep lock unavailable: ${err.message}`.
    fn spawn(&mut self, command: &InhibitCommand) -> Result<Box<dyn InhibitLock>, String>;
}

/// The real spawner: the inhibitor in **its own process group** (ctp
/// `detached: true`) with all stdio nulled (ctp `stdio: "ignore"`) — its
/// own group so release can take the waiting child down with the wrapper,
/// null stdio so a chatty failure cannot write to the proxy's terminal
/// forever, and no shell anywhere in between.
pub struct ProcessSpawner;

impl LockSpawner for ProcessSpawner {
    fn spawn(&mut self, command: &InhibitCommand) -> Result<Box<dyn InhibitLock>, String> {
        use std::os::unix::process::CommandExt;
        let child = Command::new(&command.program)
            .args(&command.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // A new process group with the child as leader, so the group
            // kill below reaches the wrapper and everything it spawned.
            .process_group(0)
            .spawn()
            .map_err(|error| error.to_string())?;
        Ok(Box::new(ProcessLock { child }))
    }
}

/// The real taken lock: the inhibitor child, killed by group.
struct ProcessLock {
    child: std::process::Child,
}

impl InhibitLock for ProcessLock {
    fn kill(&mut self) {
        let pid = self.child.id() as i32;
        if nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        )
        .is_err()
        {
            // The group is already gone; take the wrapper alone down.
            let _ = self.child.kill();
        }
        // Reap, so a released lock does not linger as a zombie for the
        // rest of the proxy's life (Node reaps on the exit event; Rust
        // does not). The SIGTERM'd inhibitor exits promptly by design.
        let _ = self.child.wait();
    }

    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

// ── the state machine (ctp createInhibitor + evaluateAwake) ──────────

/// One held/want flip worth a row — ctp's awake row shape
/// `{held, want, until, reason}` (proxy.mjs:472-479), carried to the
/// ledger by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwakeTransition {
    /// Whether the lock is actually held now.
    pub held: bool,
    /// Whether the decision wanted it held — differing from `held` is a
    /// lock that could not be taken.
    pub want: bool,
    /// When the hold expires, epoch ms; `None` for an in-flight-only hold
    /// or no hold.
    pub until: Option<i64>,
    /// The count the hold rests on, ctp's exact wording.
    pub reason: String,
}

/// The lock's state: the taken child, the retry backoff, and the
/// last-logged held state (ctp `createInhibitor`'s closure plus the
/// proxy's `awakeHeld`). Never panics its way out of a request: a failure
/// is logged once per spell and the lock reads as not held.
pub struct AwakeState {
    /// The spawn/kill seam. The real one spawns the detached inhibitor;
    /// tests inject fakes that take no real lock.
    spawner: Box<dyn LockSpawner>,
    /// This platform's lock command, or `None` where none exists (ctp
    /// `command`: `available` is `command !== null`).
    command: Option<InhibitCommand>,
    /// The held lock, if any (ctp `child`).
    child: Option<Box<dyn InhibitLock>>,
    /// When the last spawn attempt failed or the last child died on its
    /// own (ctp `failedAt`) — the retry backoff's clock.
    failed_at: Option<i64>,
    /// Whether the current failure spell has already been logged (ctp
    /// `complained`: logged once per spell, re-armed by a release that
    /// lasted until we let go).
    complained: bool,
    /// The held state as last written to the ledger (ctp `awakeHeld`,
    /// starting false: the first evaluate of a live proxy flips it and
    /// writes the startup row).
    logged_held: bool,
    /// The latest decision's want (for inspection; the transitions carry
    /// it).
    want: bool,
}

impl AwakeState {
    /// The lock over `command`, taken through `spawner`. `command: None`
    /// is ctp's unavailable platform: `hold` is a permanent no-op and
    /// the caller warns once at startup.
    pub fn new(command: Option<InhibitCommand>, spawner: Box<dyn LockSpawner>) -> AwakeState {
        AwakeState {
            spawner,
            command,
            child: None,
            failed_at: None,
            complained: false,
            logged_held: false,
            want: false,
        }
    }

    /// Whether the lock is held right now (ctp `held`: `child !== null`).
    pub fn held(&self) -> bool {
        self.child.is_some()
    }

    /// Whether this platform offers a lock at all (ctp `available`).
    pub fn available(&self) -> bool {
        self.command.is_some()
    }

    /// Take or drop the lock to match `decision`, and report the
    /// transition that needs a row — only on a held flip (ctp
    /// `evaluateAwake`, proxy.mjs:465-482). `want` differing from `held`
    /// is itself row-worthy: it is a lock that could not be taken.
    pub fn evaluate(&mut self, decision: &AwakeDecision, now: i64) -> Option<AwakeTransition> {
        // ctp's exit listener, polled at evaluation time: a child that
        // ended on its own while still wanted means the lock is gone.
        // Our own release clears `child` first, so it never lands here.
        if let Some(child) = self.child.as_mut()
            && child.exited()
        {
            self.child = None;
            self.failed_at = Some(now);
            // ctp names the signal or exit code; the trait does not carry
            // it, so the spell reads the same message every time.
            self.complain("sleep lock ended on its own");
        }

        self.want = decision.hold;
        if decision.hold {
            self.hold(now);
        } else {
            self.release();
        }

        let held = self.child.is_some();
        if held == self.logged_held {
            return None;
        }
        self.logged_held = held;
        Some(AwakeTransition {
            held,
            want: decision.hold,
            until: decision.until,
            reason: decision.reason.clone(),
        })
    }

    /// Take the lock (ctp `hold`, inhibit.mjs:85-105): a no-op while
    /// held or unavailable, and inside the retry backoff after a
    /// failure — evaluation runs on every response, and without the
    /// backoff a missing session bus would spawn a process per request.
    fn hold(&mut self, now: i64) {
        let Some(command) = self.command.as_ref() else {
            return;
        };
        if self.child.is_some() {
            return;
        }
        if let Some(failed_at) = self.failed_at
            && now.saturating_sub(failed_at) < RETRY_MS
        {
            return;
        }
        match self.spawner.spawn(command) {
            Ok(child) => {
                self.child = Some(child);
            }
            Err(message) => {
                self.failed_at = Some(now);
                self.complain(&format!("sleep lock unavailable: {message}"));
            }
        }
    }

    /// Release the lock (ctp `release`, inhibit.mjs:106-113). A lock that
    /// lasted until we let go re-arms the once-per-spell complaint, so
    /// the next failure is news again.
    fn release(&mut self) {
        if let Some(mut child) = self.child.take() {
            self.complained = false;
            child.kill();
        }
    }

    /// Log a failure once per spell (ctp `failed` + `complained`).
    fn complain(&mut self, message: &str) {
        if self.complained {
            return;
        }
        self.complained = true;
        tracing::warn!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AwakeDecision, AwakeState, AwakeTransition, INHIBIT_WHO, INHIBIT_WHY, InhibitCommand,
        LockSpawner, RETRY_MS, decide_awake, inhibit_command, on_path, platform_command,
    };
    use crate::middleware::lanes::Ttl;
    use crate::store::Lane;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const NOW: i64 = 2_000_000_000_000;
    const FIVE_MINUTES: i64 = 300_000;
    const HOUR: i64 = 3_600_000;

    /// A lane for the decision, ctp's shape: `at`, a TTL tier, a ping flag.
    fn lane(at_ms: i64, ttl: Option<i64>, ping: bool) -> Lane {
        Lane {
            key: format!("ses-{at_ms}|t"),
            session_id: Some(format!("ses-{at_ms}")),
            tools_hash: Some("t".to_owned()),
            updated_ms: at_ms,
            prompt_tokens: Some(200_000),
            ttl,
            ping: ping.then_some(true),
            noticed_at: None,
            forced_from: None,
            forced_to: None,
        }
    }

    /// A hold decision, for driving the state machine.
    fn hold(until: Option<i64>, reason: &str) -> AwakeDecision {
        AwakeDecision {
            hold: true,
            until,
            reason: reason.to_owned(),
        }
    }

    fn release() -> AwakeDecision {
        AwakeDecision {
            hold: false,
            until: None,
            reason: "no live lanes".to_owned(),
        }
    }

    // ── decide_awake (ctp decideAwake) ─────────────────────────────

    #[test]
    fn a_lane_is_live_for_its_ttl_tier_past_its_last_response() {
        // 5-minute tier: live now, expired one tick past the window.
        let live_5m = lane(NOW - 1_000, Some(Ttl::FiveMinutes.as_ms()), false);
        assert_eq!(
            decide_awake(std::slice::from_ref(&live_5m), 0, NOW),
            AwakeDecision {
                hold: true,
                until: Some(NOW - 1_000 + FIVE_MINUTES),
                reason: "1 live lane".to_owned(),
            }
        );
        // The boundary: expires == now is gone (ctp `expires <= now`).
        let expired_5m = lane(NOW - FIVE_MINUTES, Some(Ttl::FiveMinutes.as_ms()), false);
        assert!(!decide_awake(&[expired_5m], 0, NOW).hold);
        // One tick before the boundary: still live.
        let almost = lane(
            NOW - FIVE_MINUTES + 1,
            Some(Ttl::FiveMinutes.as_ms()),
            false,
        );
        assert!(decide_awake(&[almost], 0, NOW).hold);

        // 1-hour tier, and the unrecorded tier reads as the hour (ctp
        // ttlOf: anything not "5m" is the long one — guessing short would
        // sleep the machine under lanes whose cache is still live).
        for ttl in [Some(Ttl::Hour.as_ms()), None, Some(123_456)] {
            let lane = lane(NOW - FIVE_MINUTES - 1_000, ttl, false);
            assert_eq!(
                decide_awake(&[lane], 0, NOW),
                AwakeDecision {
                    hold: true,
                    until: Some(NOW - FIVE_MINUTES - 1_000 + HOUR),
                    reason: "1 live lane".to_owned(),
                },
                "ttl {ttl:?} reads as the hour tier"
            );
        }
    }

    #[test]
    fn a_future_timestamp_is_a_clock_that_moved_and_never_holds() {
        let future = lane(NOW + 60_000, Some(Ttl::Hour.as_ms()), false);
        assert_eq!(
            decide_awake(&[future], 0, NOW),
            AwakeDecision {
                hold: false,
                until: None,
                reason: "no live lanes".to_owned(),
            },
            "ctp awake.mjs:37 — a future `at` would hold forever"
        );
    }

    #[test]
    fn ping_lanes_never_count() {
        let ping = lane(NOW - 1_000, Some(Ttl::Hour.as_ms()), true);
        assert_eq!(
            decide_awake(std::slice::from_ref(&ping), 0, NOW),
            AwakeDecision {
                hold: false,
                until: None,
                reason: "no live lanes".to_owned(),
            }
        );
        // A ping lane must not move `until` either: the hold rests on the
        // live lanes only.
        let live = lane(NOW - 2_000, Some(Ttl::FiveMinutes.as_ms()), false);
        let decision = decide_awake(&[ping, live.clone()], 0, NOW);
        assert_eq!(decision.until, Some(NOW - 2_000 + FIVE_MINUTES));
        assert_eq!(decision.reason, "1 live lane");
    }

    #[test]
    fn anything_in_flight_holds_even_with_no_live_lanes() {
        // ctp awake.mjs:44: in-flight is checked first, and its `until`
        // is null — the hold rests on something without an expiry, even
        // when live lanes exist alongside.
        let live = lane(NOW - 1_000, Some(Ttl::Hour.as_ms()), false);
        for (lanes, in_flight) in [(&[][..], 1), (&[live][..], 3)] {
            assert_eq!(
                decide_awake(lanes, in_flight, NOW),
                AwakeDecision {
                    hold: true,
                    until: None,
                    reason: format!("{in_flight} in flight"),
                }
            );
        }
        assert_eq!(
            decide_awake(&[], 0, NOW),
            AwakeDecision {
                hold: false,
                until: None,
                reason: "no live lanes".to_owned(),
            }
        );
    }

    #[test]
    fn the_latest_expiry_wins_and_the_reason_counts_lanes() {
        let early = lane(NOW - 1_000, Some(Ttl::FiveMinutes.as_ms()), false);
        let late = lane(NOW - 2_000, Some(Ttl::Hour.as_ms()), false);
        let expired = lane(NOW - HOUR - 5_000, Some(Ttl::Hour.as_ms()), false);
        let decision = decide_awake(&[early, late, expired], 0, NOW);
        assert!(decision.hold);
        assert_eq!(decision.until, Some(NOW - 2_000 + HOUR));
        assert_eq!(decision.reason, "2 live lanes");
    }

    // ── inhibit_command (the platform table) ──────────────────────

    fn everything_exists(bin: &str) -> bool {
        matches!(bin, "tail" | "gnome-session-inhibit" | "systemd-inhibit")
    }

    fn nothing_exists(_: &str) -> bool {
        false
    }

    #[test]
    fn gnome_gets_the_session_manager_inhibitor_when_present() {
        let command = inhibit_command(
            "linux",
            Some("GNOME"),
            everything_exists,
            "who-test",
            "why-test",
            4242,
        )
        .expect("GNOME with the binary present takes the lock");
        assert_eq!(command.program, "gnome-session-inhibit");
        assert_eq!(
            command.args,
            vec![
                "--app-id".to_owned(),
                "who-test".to_owned(),
                "--reason".to_owned(),
                "why-test".to_owned(),
                "--inhibit".to_owned(),
                "suspend".to_owned(),
                "tail".to_owned(),
                "--pid=4242".to_owned(),
                "-f".to_owned(),
                "/dev/null".to_owned(),
            ],
            "the inhibitor plus the PID-watching child, ctp inhibit.mjs:45-48"
        );
    }

    #[test]
    fn the_gnome_detector_reads_the_desktop_list_not_the_whole_string() {
        // `XDG_CURRENT_DESKTOP` is colon-separated ("ubuntu:GNOME");
        // membership is per component.
        for desktop in ["GNOME", "ubuntu:GNOME", "GNOME:XFCE", "GNOME:Cinnamon:UKUI"] {
            assert!(
                inhibit_command("linux", Some(desktop), everything_exists, "w", "y", 1)
                    .is_some_and(|command| command.program == "gnome-session-inhibit"),
                "{desktop:?} includes GNOME"
            );
        }
        // A different casing, a different component, an empty or absent
        // value: not GNOME, so the logind idle lock instead.
        for desktop in ["", "XFCE", "KDE", "gnome", "unity:GNOME3"] {
            let command = inhibit_command("linux", Some(desktop), everything_exists, "w", "y", 1)
                .expect("systemd-inhibit is the fallback");
            assert_eq!(command.program, "systemd-inhibit", "{desktop:?}");
        }
        let command = inhibit_command("linux", None, everything_exists, "w", "y", 1).unwrap();
        assert_eq!(command.program, "systemd-inhibit");
    }

    #[test]
    fn other_linux_gets_systemd_inhibit_idle_block() {
        let command = inhibit_command(
            "linux",
            Some("XFCE"),
            everything_exists,
            "who-test",
            "why-test",
            7,
        )
        .expect("the fallback lock exists");
        assert_eq!(command.program, "systemd-inhibit");
        assert_eq!(
            command.args,
            vec![
                "--what=idle".to_owned(),
                "--mode=block".to_owned(),
                "--who=who-test".to_owned(),
                "--why=why-test".to_owned(),
                "tail".to_owned(),
                "--pid=7".to_owned(),
                "-f".to_owned(),
                "/dev/null".to_owned(),
            ],
            "idle-only, block mode, ctp inhibit.mjs:50-55"
        );

        // GNOME without the binary falls through to the same command.
        let command = inhibit_command(
            "linux",
            Some("GNOME"),
            |bin| bin == "tail" || bin == "systemd-inhibit",
            "w",
            "y",
            1,
        )
        .expect("fallback");
        assert_eq!(command.program, "systemd-inhibit");
    }

    #[test]
    fn no_tail_no_lock_and_no_darwin_in_v1() {
        // The PID-watching child is the mechanism; without tail there is
        // no lock to take (ctp inhibit.mjs:35).
        assert_eq!(
            inhibit_command(
                "linux",
                Some("GNOME"),
                |bin| bin == "gnome-session-inhibit",
                "w",
                "y",
                1
            ),
            None
        );
        assert_eq!(
            inhibit_command("linux", None, nothing_exists, "w", "y", 1),
            None
        );
        // ctp's darwin/caffeinate branch is macOS-only and not ported.
        assert_eq!(
            inhibit_command("darwin", Some("GNOME"), everything_exists, "w", "y", 1),
            None,
            "toker v1 is Linux-only: no caffeinate"
        );
    }

    // ── on_path (ctp onPath) ──────────────────────────────────────

    #[test]
    fn path_probe_finds_executables_and_skips_the_rest() {
        let dir = std::env::temp_dir().join(format!("toker-awake-{}-path", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create probe dir");
        let exe = dir.join("fake-inhibitor");
        std::fs::write(&exe, b"#!/bin/sh\n").expect("write probe");
        set_mode(&exe, 0o755);
        let plain = dir.join("plain-file");
        std::fs::write(&plain, b"x").expect("write probe");
        set_mode(&plain, 0o644);

        let path = dir.to_string_lossy().into_owned();
        assert!(on_path("fake-inhibitor", Some(&path)));
        // An existing but non-executable file is not on PATH (ctp checks
        // X_OK).
        assert!(!on_path("plain-file", Some(&path)));
        assert!(!on_path("missing", Some(&path)));
        // Empty entries are skipped, ctp `if (!dir) continue`.
        let sparse = format!(":{path}::");
        assert!(on_path("fake-inhibitor", Some(&sparse)));
        assert!(!on_path("fake-inhibitor", None), "no PATH is no lock");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn set_mode(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("set probe mode");
    }

    #[test]
    fn the_real_environment_probe_answers_a_question() {
        // platform_command is the real-environment probe; what it answers
        // depends on this host, so the assertion is only that it never
        // panics and, when it finds a lock, it names the PID-watching
        // child this process owns.
        let command = platform_command(INHIBIT_WHO, INHIBIT_WHY);
        if let Some(command) = command {
            assert!(
                command
                    .args
                    .contains(&format!("--pid={}", std::process::id()))
            );
        }
    }

    // ── the state machine (ctp createInhibitor + evaluateAwake) ───

    /// A spawner whose results the test scripts, with its counters
    /// shared so the test keeps handles while the state owns the box.
    #[derive(Clone, Default)]
    struct FakeSpawner {
        attempts: Arc<AtomicUsize>,
        failures: Arc<AtomicUsize>,
        kills: Arc<AtomicUsize>,
        dead: Arc<AtomicBool>,
    }

    impl FakeSpawner {
        /// Make the next spawn fail (consumed once).
        fn queue_failure(&self) {
            self.failures.fetch_add(1, Ordering::SeqCst);
        }

        /// Kill the current fake child where it stands: the next
        /// `exited` poll sees it, like ctp's exit listener firing.
        fn die(&self) {
            self.dead.store(true, Ordering::SeqCst);
        }

        fn attempts(&self) -> usize {
            self.attempts.load(Ordering::SeqCst)
        }

        fn kills(&self) -> usize {
            self.kills.load(Ordering::SeqCst)
        }
    }

    impl LockSpawner for FakeSpawner {
        fn spawn(
            &mut self,
            _command: &InhibitCommand,
        ) -> Result<Box<dyn super::InhibitLock>, String> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let failures = self.failures.load(Ordering::SeqCst);
            if failures > 0 {
                self.failures.store(failures - 1, Ordering::SeqCst);
                return Err("no session bus".to_owned());
            }
            Ok(Box::new(FakeLock {
                kills: self.kills.clone(),
                dead: self.dead.clone(),
            }))
        }
    }

    /// A taken lock whose kills and death the test controls.
    struct FakeLock {
        kills: Arc<AtomicUsize>,
        dead: Arc<AtomicBool>,
    }

    impl super::InhibitLock for FakeLock {
        fn kill(&mut self) {
            self.kills.fetch_add(1, Ordering::SeqCst);
        }
        fn exited(&mut self) -> bool {
            self.dead.swap(false, Ordering::SeqCst)
        }
    }

    /// The command the state machine tests run over — the table's shape;
    /// nothing real is ever spawned.
    fn command() -> InhibitCommand {
        inhibit_command("linux", None, everything_exists, "toker", "why", 1)
            .expect("the test platform table")
    }

    #[test]
    fn a_row_only_on_a_held_flip_and_no_double_spawn_while_held() {
        let fake = FakeSpawner::default();
        let mut state = AwakeState::new(Some(command()), Box::new(fake.clone()));
        // Nothing wanted, nothing held: no row (ctp's awakeHeld starts
        // false and stays there).
        assert_eq!(state.evaluate(&release(), NOW), None);
        assert_eq!(state.evaluate(&release(), NOW + 1), None);
        assert_eq!(fake.attempts(), 0);

        // Wanted: spawn once, one row.
        let first = state
            .evaluate(&hold(Some(NOW + HOUR), "1 live lane"), NOW)
            .expect("the flip to held is a row");
        assert_eq!(
            first,
            AwakeTransition {
                held: true,
                want: true,
                until: Some(NOW + HOUR),
                reason: "1 live lane".to_owned(),
            }
        );
        assert_eq!(fake.attempts(), 1, "the hold spawns exactly one child");

        // Still wanted: no second spawn (ctp `if (child || !command)
        // return`), no second row.
        assert_eq!(
            state.evaluate(&hold(Some(NOW + HOUR), "1 live lane"), NOW + 30_000),
            None
        );
        assert_eq!(fake.attempts(), 1);
        assert_eq!(fake.kills(), 0, "nothing was released");

        // Released: the child is killed, one row.
        let second = state
            .evaluate(&release(), NOW + 60_000)
            .expect("the flip back is a row");
        assert!(!second.held);
        assert!(!second.want);
        assert_eq!(fake.kills(), 1, "release kills the child's group");
        // No further evaluation writes anything until a flip.
        assert_eq!(state.evaluate(&release(), NOW + 61_000), None);
        assert_eq!(fake.attempts(), 1);
    }

    #[test]
    fn a_failed_spawn_retries_at_five_minutes_not_before() {
        let fake = FakeSpawner::default();
        fake.queue_failure();
        let mut state = AwakeState::new(Some(command()), Box::new(fake.clone()));

        // The spawn fails: held never flipped off its initial false, so
        // no row — ctp logs the flip of the ACTUAL lock, and an untaken
        // lock reads as not held. (A `want` ≠ `held` row exists only
        // where a previously-held lock was lost and could not be retaken
        // — the child-death test covers that one.)
        assert_eq!(
            state.evaluate(&hold(Some(NOW + HOUR), "1 live lane"), NOW),
            None,
            "a failed first take writes no row"
        );
        assert_eq!(fake.attempts(), 1);
        assert_eq!(state.failed_at, Some(NOW));

        // Evaluation runs on every response; the backoff keeps a missing
        // session bus from becoming a process per request (ctp RETRY_MS).
        assert_eq!(
            state.evaluate(&hold(Some(NOW + HOUR), "1 live lane"), NOW + 60_000),
            None,
            "inside the backoff: no row, no attempt"
        );
        assert_eq!(
            state.evaluate(&hold(Some(NOW + HOUR), "1 live lane"), NOW + RETRY_MS - 1),
            None
        );
        assert_eq!(fake.attempts(), 1, "not one retry inside five minutes");

        // At five minutes the retry goes ahead and takes the lock — and
        // THAT flip is the row.
        let row = state
            .evaluate(&hold(Some(NOW + HOUR), "1 live lane"), NOW + RETRY_MS)
            .expect("the retry takes the lock, a flip off never-held");
        assert!(row.held);
        assert!(row.want);
        assert_eq!(fake.attempts(), 2);
        assert_eq!(
            state.failed_at,
            Some(NOW),
            "a success does not clear the spell — only a release does"
        );
    }

    #[test]
    fn a_child_that_died_on_its_own_is_a_failure_with_a_backoff() {
        let fake = FakeSpawner::default();
        let mut state = AwakeState::new(Some(command()), Box::new(fake.clone()));

        let row = state
            .evaluate(&hold(Some(NOW + HOUR), "1 live lane"), NOW)
            .expect("held");
        assert!(row.held);

        // The session bus goes away; the child exits. The next
        // evaluation sees it (ctp's exit listener, polled here) and the
        // lock reads as lost — with the same five-minute backoff as a
        // failed spawn, and the dead child is NOT killed (it is already
        // gone; release was never called).
        fake.die();
        let row = state
            .evaluate(&hold(Some(NOW + HOUR), "1 live lane"), NOW + 10_000)
            .expect("losing the lock is a flip");
        assert!(!row.held);
        assert!(row.want);
        assert_eq!(fake.kills(), 0);
        assert_eq!(state.failed_at, Some(NOW + 10_000));

        // Still wanted: the re-spawn waits out the backoff, then takes
        // the lock back.
        assert_eq!(
            state.evaluate(
                &hold(Some(NOW + HOUR), "1 live lane"),
                NOW + 10_000 + RETRY_MS - 1
            ),
            None
        );
        let row = state
            .evaluate(
                &hold(Some(NOW + HOUR), "1 live lane"),
                NOW + 10_000 + RETRY_MS,
            )
            .expect("re-spawned after the backoff");
        assert!(row.held);
        assert_eq!(fake.attempts(), 2);
    }

    #[test]
    fn the_complaint_is_once_per_spell_and_a_lasting_release_re_arms_it() {
        let fake = FakeSpawner::default();
        let mut state = AwakeState::new(Some(command()), Box::new(fake.clone()));

        fake.queue_failure();
        state.evaluate(&hold(Some(NOW + HOUR), "1 live lane"), NOW);
        assert!(state.complained, "the first failure logs");

        // The next failure in the same spell logs nothing new — the
        // latch is what makes the complaint once per spell.
        fake.queue_failure();
        state.evaluate(&hold(Some(NOW + HOUR), "1 live lane"), NOW + RETRY_MS);
        assert!(state.complained, "still the same spell");

        // A lock that lasts until we let go re-arms the complaint, so the
        // next failure is news again (ctp: release sets complained=false).
        let row = state
            .evaluate(&hold(Some(NOW + HOUR), "1 live lane"), NOW + 2 * RETRY_MS)
            .expect("takes the lock");
        assert!(row.held);
        state.evaluate(&release(), NOW + 2 * RETRY_MS + 1);
        assert!(!state.complained, "a lasting release re-arms the complaint");

        fake.queue_failure();
        state.evaluate(
            &hold(Some(NOW + HOUR), "1 live lane"),
            NOW + 2 * RETRY_MS + 2,
        );
        assert!(state.complained, "and the next failure complains again");
    }

    #[test]
    fn an_unavailable_platform_holds_nothing_and_writes_no_rows() {
        let fake = FakeSpawner::default();
        let mut state = AwakeState::new(None, Box::new(fake.clone()));
        assert!(!state.available());
        // ctp: `if (child || !command) return` — wanted forever, the lock
        // never exists, and held never flips off its initial false, so no
        // row ever lands.
        for offset in [0, RETRY_MS, 10 * RETRY_MS] {
            assert_eq!(
                state.evaluate(&hold(Some(NOW + HOUR), "1 live lane"), NOW + offset),
                None
            );
        }
        assert_eq!(fake.attempts(), 0);
        assert!(!state.held());
    }

    // ── the real spawner, manual verification only ─────────────────
    //
    // This takes a REAL idle-sleep lock on the host that runs it, so it
    // is #[ignore]-gated: `cargo test -- --ignored` on a machine you are
    // sitting at. CI and the test suite never touch it.

    #[test]
    #[ignore = "takes a REAL idle-sleep lock — manual verification on the host only"]
    fn the_real_spawner_takes_a_real_lock_and_release_takes_it_down() {
        use std::time::Duration;
        let Some(command) = platform_command(INHIBIT_WHO, INHIBIT_WHY) else {
            eprintln!("no idle-sleep lock on this platform; nothing to verify");
            return;
        };
        let mut state = AwakeState::new(Some(command), Box::new(super::ProcessSpawner));
        let now = jiff::Timestamp::now().as_millisecond();
        let row = state
            .evaluate(&hold(Some(now + 60_000), "manual verification"), now)
            .expect("the lock was taken");
        assert!(row.held);

        // Give a broken child time to die (a missing session bus kills
        // systemd-inhibit within milliseconds), then re-evaluate: no flip
        // means the lock is still standing.
        std::thread::sleep(Duration::from_millis(500));
        let now = jiff::Timestamp::now().as_millisecond();
        assert_eq!(
            state.evaluate(&hold(Some(now + 60_000), "manual verification"), now),
            None,
            "the lock survived half a second"
        );

        let now = jiff::Timestamp::now().as_millisecond();
        let row = state
            .evaluate(&release(), now)
            .expect("the release is a flip");
        assert!(!row.held);
    }
}
