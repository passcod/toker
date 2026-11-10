//! The ping schedule as the machine holds it: the slots and their days, the
//! units that fire them, and the wake that rouses a suspended machine for
//! them (spec: GPING#the-schedule).
//!
//! The installed units are the record. The hold timer carries one
//! `OnCalendar=` line per slot, its days and its time, and that is read back
//! as the schedule; the store keeps no copy, because the service that owns
//! the store is sandboxed away from the units dir and could never repair a
//! copy that drifted (spec: GPING#where-the-schedule-is-kept).
//!
//! Setup and the dashboard both write the user units through
//! [`apply_user`]. The wake belongs to the system manager, which only an
//! elevated step reaches: [`place_script`] and [`remove_script`] are the
//! whole of what runs as root, a shell script carrying the units inline, and
//! the dashboard hands it to `pkexec` with the terminal given back
//! (spec: SEC#toker-never-runs-with-elevation).

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};

use anyhow::{Context, Result, bail};

use crate::timers::{self, Days, HOLD_UNIT_FOR, PING_DELAY_MINUTES, Slot};

/// The wake SYSTEM timer's name, which needs `CAP_WAKE_ALARM` for
/// `WakeSystem=true`, and the user manager lacks it.
pub const WAKE_TIMER_UNIT: &str = "toker-wake.timer";

/// The service the wake timer starts. It does nothing, but systemd refuses
/// to start a timer whose unit does not exist: without it the first wake
/// timer never armed at all.
pub const WAKE_SERVICE_UNIT: &str = "toker-wake.service";

/// The hold USER timer's name (same name, timer + service pair).
pub const HOLD_TIMER_UNIT: &str = "toker-hold.timer";

/// The hold USER service's name.
pub const HOLD_SERVICE_UNIT: &str = "toker-hold.service";

/// Where the system manager's own units are kept, which is where the wake
/// is copied to. Copied rather than linked from home: a home mounted after
/// the system manager starts would leave a linked unit unloadable.
pub const SYSTEM_UNITS_DIR: &str = "/etc/systemd/system";

/// The program the elevated step runs under. The platform's own agent asks
/// for the authorisation, on the desktop where one is registered and on the
/// terminal otherwise.
pub const ELEVATE: &str = "pkexec";

/// The shell the elevated script runs in, by absolute path so nothing on
/// the operator's `PATH` stands in for it as root.
pub const SHELL: &str = "/bin/sh";

/// The five hours a ping's window runs, in minutes, which two slots' pings
/// must keep apart.
const WINDOW_MINUTES: i64 = timers::WINDOW_MS / 60_000;

/// Minutes in a day, and in a week.
const DAY_MINUTES: i64 = 24 * 60;
const WEEK_MINUTES: i64 = 7 * DAY_MINUTES;

// ── a slot ──────────────────────────────────────────────────────────────

/// One slot of the schedule: a clock time and the days it runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduledSlot {
    pub time: Slot,
    pub days: Days,
}

impl ScheduledSlot {
    /// When the slot's ping fires: [`PING_DELAY_MINUTES`] after the slot.
    pub fn fire(&self) -> Slot {
        timers::slot_plus_minutes(&self.time, PING_DELAY_MINUTES)
    }

    /// The slot's minute of the day.
    fn minute(&self) -> i64 {
        i64::from(self.time.hour) * 60 + i64::from(self.time.minute)
    }

    /// Whether the delay carries the ping past midnight.
    pub fn wraps(&self) -> bool {
        self.minute() + PING_DELAY_MINUTES >= DAY_MINUTES
    }

    /// The days the ping fires on: the slot's own, or the day after each
    /// where the delay carries it past midnight (spec: GPING#the-schedule).
    pub fn ping_days(&self) -> Days {
        if self.wraps() {
            self.days.next_day()
        } else {
            self.days
        }
    }

    /// The `OnCalendar=` value for the slot itself.
    pub fn calendar(&self) -> String {
        format!("{} {}", self.days.calendar(), self.time.hhmm())
    }

    /// The minutes of the week, Monday 00:00 first, at which the slot's
    /// pings fire.
    fn ping_minutes(&self) -> impl Iterator<Item = i64> + '_ {
        let fire = self.minute() + PING_DELAY_MINUTES;
        self.days
            .indices()
            .map(move |day| (day as i64 * DAY_MINUTES + fire).rem_euclid(WEEK_MINUTES))
    }

    /// Parse a slot as the dashboard takes it: `hh:mm`, then its days, as
    /// in `07:20 Mon..Fri`. `Err` says what is wrong in words.
    pub fn parse(text: &str) -> std::result::Result<ScheduledSlot, String> {
        let text = text.trim();
        let (time, days) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let time = timers::parse_slot(time)
            .ok_or_else(|| format!("{time:?} is not a time: write it hh:mm, as 07:20"))?;
        let days = Days::parse(days)?;
        if days.is_empty() {
            return Err("name the days it runs on, as 07:20 Mon..Fri".to_owned());
        }
        Ok(ScheduledSlot { time, days })
    }
}

/// The schedule installed when none is: the predecessor's. 07:20's ping at
/// 07:31 anchors its window on 07:30, ending 12:30, and 12:20's at 12:31
/// anchors 12:30–17:30: a window open for the morning, re-opened after
/// lunch.
pub fn default_schedule() -> Vec<ScheduledSlot> {
    ["07:20", "12:20"]
        .into_iter()
        .map(|time| ScheduledSlot {
            time: timers::parse_slot(time).expect("the default slots parse"),
            days: Days::WEEKDAYS,
        })
        .collect()
}

// ── the rules ───────────────────────────────────────────────────────────

/// What the schedule's rules say of the slot at `at`, judged against the
/// others in `slots`: `None` where it is acceptable
/// (spec: GPING#the-schedule).
pub fn refusal(slots: &[ScheduledSlot], at: usize) -> Option<String> {
    let slot = slots.get(at)?;
    if slot.days.is_empty() {
        return Some("a slot needs at least one day".to_owned());
    }
    for (index, other) in slots.iter().enumerate() {
        if index == at {
            continue;
        }
        if other.time == slot.time {
            return Some(format!("another slot is at {}", other.time.hhmm()));
        }
    }
    for (index, other) in slots.iter().enumerate() {
        if index == at {
            continue;
        }
        // Around the week, so a late Sunday slot and an early Monday one
        // are measured across the midnight between them.
        let close = slot.ping_minutes().any(|mine| {
            other.ping_minutes().any(|theirs| {
                let apart = (mine - theirs).rem_euclid(WEEK_MINUTES);
                apart.min(WEEK_MINUTES - apart) < WINDOW_MINUTES
            })
        });
        if close {
            return Some(format!(
                "within 5 h of {}'s ping at {}, so one would always be skipped",
                other.time.hhmm(),
                other.fire().hhmm()
            ));
        }
    }
    None
}

/// Every slot's refusal, in order.
pub fn refusals(slots: &[ScheduledSlot]) -> Vec<Option<String>> {
    (0..slots.len()).map(|at| refusal(slots, at)).collect()
}

/// Sort slots by time, the order the units and the dashboard list them in.
pub fn sorted(mut slots: Vec<ScheduledSlot>) -> Vec<ScheduledSlot> {
    slots.sort_by_key(|slot| (slot.time.hour, slot.time.minute));
    slots
}

// ── the units ───────────────────────────────────────────────────────────

/// `toker-wake.timer`, the SYSTEM unit: one `OnCalendar=` per slot plus
/// `WakeSystem=true`, which wakes the machine **from suspend only** (a
/// powered-off machine stays off). The only unit Toker places in the
/// system manager.
pub fn wake_system_unit(slots: &[ScheduledSlot]) -> String {
    let mut on_calendar = String::new();
    for slot in slots {
        on_calendar.push_str(&format!("OnCalendar={}\n", slot.calendar()));
    }
    format!(
        r#"[Unit]
Description=toker wake timer (wakes the machine at the chosen slots)

[Timer]
# The only root-level piece toker installs: WakeSystem=true needs
# CAP_WAKE_ALARM, which the user manager lacks, so this unit belongs to
# the system manager. Wakes from suspend only — a machine that is
# powered off stays off.
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

/// `toker-wake.service`, what [`wake_system_unit`] activates (by the
/// shared name; no `Unit=` needed). It does nothing: the timer's
/// `WakeSystem=true` is the whole job, and staying awake for the ping is
/// the hold USER unit's, so the root-owned half has nothing in it to audit
/// and never needs the user's session bus.
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

/// `toker-hold.timer` + `toker-hold.service`, the hold USER units: a timer
/// at the slots running the hold verb for the pinned 15 m. A timer that
/// elapses while the machine is suspended fires on resume, and the hold
/// then keeps the machine up those 15 minutes so the ping (11 m after the
/// slot) can fire; `Persistent=false` keeps a missed slot from re-running
/// hours late — the ping's own lateness guard refuses those anyway. The
/// service runs only while the marker in `state_dir` is there
/// (spec: GPING#scope).
///
/// The timer's `OnCalendar=` lines are also the schedule's record, which
/// [`read_installed`] reads back.
pub fn hold_user_units(slots: &[ScheduledSlot], exe: &Path, state_dir: &Path) -> (String, String) {
    let mut on_calendar = String::new();
    for slot in slots {
        on_calendar.push_str(&format!("OnCalendar={}\n", slot.calendar()));
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
# Only while the schedule is on and a provider that anchors its windows
# is enabled: toker keeps the marker, so the dashboard switches this
# without elevation. On the service, not the timer, whose condition is
# read once when it starts.
ConditionPathExists={marker}

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
        marker = timers::ping_marker(state_dir).display(),
    );
    (timer, service)
}

/// One slot's ping pair — [`ping_user_units`] returns one per slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingUnits {
    /// The slot this pair serves.
    pub slot: ScheduledSlot,
    /// The timer unit's name.
    pub timer_name: String,
    /// The timer unit's contents.
    pub timer: String,
    /// The service unit's name.
    pub service_name: String,
    /// The service unit's contents.
    pub service: String,
}

/// The ping USER units — **one timer+service pair per slot**, the timer at
/// slot+11 m and the service running the ping verb **for that slot and its
/// days**. Per-slot because the `--slot` argument must match its timer's
/// clock time: a shared service cannot know which slot fired. Each service
/// runs only while the marker in `state_dir` is there (spec: GPING#scope).
pub fn ping_user_units(slots: &[ScheduledSlot], exe: &Path, state_dir: &Path) -> Vec<PingUnits> {
    let marker = timers::ping_marker(state_dir);
    slots
        .iter()
        .map(|slot| {
            let time = slot.time.hhmm();
            let fire = slot.fire().hhmm();
            // A wrapped fire time lands the day AFTER the slot — Friday
            // 23:55's ping fires Saturday 00:06 — so its timer runs on the
            // day after each of the slot's days.
            let ping_days = slot.ping_days().calendar();
            let days = slot.days.calendar();
            let stem = ping_unit_stem(&slot.time);
            let timer = format!(
                r#"[Unit]
Description=toker ping timer (opens the {time} quota window, {PING_DELAY_MINUTES} m after the slot)

[Timer]
# {PING_DELAY_MINUTES} m after the slot: the machine has woken and settled, and
# the hold still has 4 m left to run. Persistent=false deliberately —
# a ping re-run hours late would open a mostly-spent window, and the
# verb's lateness guard refuses those anyway.
OnCalendar={ping_days} {fire}
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
Description=toker ping service (opens the {time} quota window)
# As for the hold: only while toker keeps the marker, and on the service
# rather than the timer.
ConditionPathExists={marker}

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
ExecStart="{exe}" ping-window --slot={time} --days={days}
"#,
                exe = exe.display(),
                marker = marker.display(),
            );
            PingUnits {
                slot: *slot,
                timer_name: format!("{stem}.timer"),
                timer,
                service_name: format!("{stem}.service"),
                service,
            }
        })
        .collect()
}

/// The per-slot ping unit stem: `toker-ping-0900` for the 09:00 slot (a
/// unit name cannot carry the colon).
pub fn ping_unit_stem(time: &Slot) -> String {
    format!("toker-ping-{:02}{:02}", time.hour, time.minute)
}

/// The user units for `slots`, in install order: the hold pair, then a ping
/// pair per slot. With the timers among them, in the order they are
/// enabled.
pub fn user_units(
    slots: &[ScheduledSlot],
    exe: &Path,
    state_dir: &Path,
) -> (Vec<(String, String)>, Vec<String>) {
    let (hold_timer, hold_service) = hold_user_units(slots, exe, state_dir);
    let mut units = vec![
        (HOLD_TIMER_UNIT.to_owned(), hold_timer),
        (HOLD_SERVICE_UNIT.to_owned(), hold_service),
    ];
    let mut timers = vec![HOLD_TIMER_UNIT.to_owned()];
    for pair in ping_user_units(slots, exe, state_dir) {
        units.push((pair.timer_name.clone(), pair.timer));
        units.push((pair.service_name, pair.service));
        timers.push(pair.timer_name);
    }
    (units, timers)
}

// ── reading what is installed ───────────────────────────────────────────

/// What the units dir holds of the schedule.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Installed {
    /// The slots, sorted by time; none where no hold timer is installed.
    pub slots: Vec<ScheduledSlot>,
    /// The binary the hold service runs, which later rewrites keep, so the
    /// timers go on running the binary setup installed rather than
    /// whichever one the dashboard happens to be.
    pub exe: Option<PathBuf>,
}

/// Read the schedule back from the hold units in `units_dir`. Unreadable is
/// nothing installed: the dir is created by the first install, so a fresh
/// machine has none. A line that does not read as a slot is passed over
/// rather than failing the whole read.
pub fn read_installed(units_dir: &Path) -> Installed {
    let timer = std::fs::read_to_string(units_dir.join(HOLD_TIMER_UNIT)).unwrap_or_default();
    let slots = timer
        .lines()
        .filter_map(|line| line.trim().strip_prefix("OnCalendar="))
        .filter_map(|value| {
            let (days, time) = value.trim().rsplit_once(char::is_whitespace)?;
            let time = timers::parse_slot(time)?;
            let days = Days::parse(days).ok().filter(|days| !days.is_empty())?;
            Some(ScheduledSlot { time, days })
        })
        .collect();
    let service = std::fs::read_to_string(units_dir.join(HOLD_SERVICE_UNIT)).unwrap_or_default();
    let exe = service
        .lines()
        .filter_map(|line| line.trim().strip_prefix("ExecStart=\""))
        .find_map(|rest| rest.split_once('"'))
        .map(|(exe, _)| PathBuf::from(exe));
    Installed {
        slots: sorted(slots),
        exe,
    }
}

// ── writing the user units ──────────────────────────────────────────────

/// The user manager and its units dir: every effect writing the user units
/// has. A non-zero systemctl exit is `Ok(Output)`, for the caller to read;
/// `Err` is "the command could not run at all".
pub trait UserManager {
    /// Run `systemctl --user <args>` to completion.
    fn systemctl_user(&self, args: &[&str]) -> Result<Output>;

    /// Install a unit file into the user units dir, returning the path
    /// written. Declarative: the same contents install cleanly over an
    /// earlier install of the same unit.
    fn install_unit(&self, name: &str, contents: &str) -> Result<PathBuf>;

    /// Remove a unit file from the user units dir. One already gone is
    /// removed.
    fn remove_unit(&self, name: &str) -> Result<()>;
}

/// Run one `systemctl --user` call, failing with what it said where it
/// exits non-zero.
fn systemctl(manager: &dyn UserManager, args: &[&str]) -> Result<()> {
    let output = manager.systemctl_user(args)?;
    if !output.status.success() {
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Bring the user units from the `installed` slots to `slots`: retire the
/// ping pairs of the slots removed, write the hold pair and a ping pair per
/// slot, and have the user manager pick them up. Timers are restarted so a
/// changed `OnCalendar=` is the one they wait on. The first failure stops
/// the rest and is returned.
pub fn apply_user(
    manager: &dyn UserManager,
    installed: &[ScheduledSlot],
    slots: &[ScheduledSlot],
    exe: &Path,
    state_dir: &Path,
) -> Result<()> {
    let removed: Vec<String> = installed
        .iter()
        .filter(|old| !slots.iter().any(|slot| slot.time == old.time))
        .map(|old| ping_unit_stem(&old.time))
        .collect();
    if !removed.is_empty() {
        let timers: Vec<String> = removed.iter().map(|stem| format!("{stem}.timer")).collect();
        let mut args = vec!["disable", "--now"];
        args.extend(timers.iter().map(String::as_str));
        systemctl(manager, &args)?;
    }
    let (units, timers) = user_units(slots, exe, state_dir);
    for (name, contents) in &units {
        manager
            .install_unit(name, contents)
            .with_context(|| format!("writing {name}"))?;
    }
    for stem in &removed {
        for name in [format!("{stem}.timer"), format!("{stem}.service")] {
            manager
                .remove_unit(&name)
                .with_context(|| format!("removing {name}"))?;
        }
    }
    systemctl(manager, &["daemon-reload"])?;
    let names: Vec<&str> = timers.iter().map(String::as_str).collect();
    let mut enable = vec!["enable"];
    enable.extend(&names);
    systemctl(manager, &enable)?;
    let mut restart = vec!["restart"];
    restart.extend(&names);
    systemctl(manager, &restart)
}

// ── the wake ────────────────────────────────────────────────────────────

/// A heredoc delimiter that no line of `contents` matches, so the unit
/// cannot end its own quoting.
fn delimiter(contents: &str) -> String {
    let mut delimiter = "TOKER_UNIT_END".to_owned();
    while contents.lines().any(|line| line == delimiter) {
        delimiter.push('_');
    }
    delimiter
}

/// One unit written into the system units dir from a quoted heredoc.
fn write_unit(script: &mut String, name: &str, contents: &str) {
    let end = delimiter(contents);
    script.push_str(&format!(
        "cat > {SYSTEM_UNITS_DIR}/{name} <<'{end}'\n{contents}{}{end}\n",
        if contents.ends_with('\n') { "" } else { "\n" }
    ));
}

/// The elevated script that places the wake at `slots`: the two units
/// written whole into the system units dir, the system manager reloaded,
/// and the timer enabled and restarted so it waits on these slots. The
/// units are in the script rather than read from a file, so the command the
/// platform asks to authorise is everything that runs as root, and nothing
/// it reads can change between the prompt and the write
/// (spec: GPING#where-the-schedule-is-kept).
pub fn place_script(slots: &[ScheduledSlot]) -> String {
    let mut script = "set -eu\numask 022\n".to_owned();
    write_unit(&mut script, WAKE_SERVICE_UNIT, &wake_system_service());
    write_unit(&mut script, WAKE_TIMER_UNIT, &wake_system_unit(slots));
    script.push_str(&format!(
        "systemctl daemon-reload\nsystemctl enable {WAKE_TIMER_UNIT}\nsystemctl restart {WAKE_TIMER_UNIT}\n"
    ));
    script
}

/// The elevated script that removes the wake: the timer stopped and
/// disabled where it is loaded, both units deleted, the system manager
/// reloaded.
pub fn remove_script() -> String {
    format!(
        "set -eu\n\
         systemctl disable --now {WAKE_TIMER_UNIT} 2>/dev/null || true\n\
         rm -f {SYSTEM_UNITS_DIR}/{WAKE_TIMER_UNIT} {SYSTEM_UNITS_DIR}/{WAKE_SERVICE_UNIT}\n\
         systemctl daemon-reload\n"
    )
}

/// Whether the system manager has the wake enabled, which needs no
/// elevation to ask. `None` where systemctl could not be run at all.
pub fn wake_placed() -> Option<bool> {
    let output = std::process::Command::new("systemctl")
        .args(["is-enabled", WAKE_TIMER_UNIT])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim() == "enabled")
}

/// Run an elevated script through the platform's authorisation, with the
/// terminal inherited so its agent can ask there. The caller gives the
/// terminal up first.
pub fn elevate(script: &str) -> std::io::Result<ExitStatus> {
    std::process::Command::new(ELEVATE)
        .args([SHELL, "-c", script])
        .status()
}

/// What an elevated run's exit says, in words for the page. pkexec exits
/// 126 when the authorisation was dismissed or refused, and 127 when it
/// could not be asked.
pub fn elevation_outcome(status: &std::io::Result<ExitStatus>) -> std::result::Result<(), String> {
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(match status.code() {
            Some(126) => "elevation was refused".to_owned(),
            Some(127) => "elevation could not be asked for".to_owned(),
            Some(code) => format!("the elevated step failed (exit {code})"),
            None => "the elevated step was stopped".to_owned(),
        }),
        Err(error) => Err(format!("{ELEVATE} could not be run: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    fn slot(text: &str) -> ScheduledSlot {
        ScheduledSlot::parse(text).expect(text)
    }

    #[test]
    fn a_slot_parses_as_a_time_then_its_days() {
        let parsed = slot("07:20 Mon..Fri");
        assert_eq!(parsed.time.hhmm(), "07:20");
        assert_eq!(parsed.days, Days::WEEKDAYS);
        assert_eq!(slot("  09:40   sat sun ").calendar(), "Sat,Sun 09:40");
        assert!(ScheduledSlot::parse("07:20").unwrap_err().contains("days"));
        assert!(
            ScheduledSlot::parse("7:20 Mon")
                .unwrap_err()
                .contains("hh:mm")
        );
        assert!(ScheduledSlot::parse("07:20 Someday").is_err());
    }

    #[test]
    fn a_ping_carried_past_midnight_runs_the_next_day() {
        let late = slot("23:55 Mon..Fri");
        assert!(late.wraps());
        assert_eq!(late.fire().hhmm(), "00:06");
        assert_eq!(late.ping_days().calendar(), "Tue..Sat");
        let sunday = slot("23:50 Sun");
        assert_eq!(sunday.ping_days().calendar(), "Mon");
        assert!(!slot("23:48 Mon").wraps());
    }

    /// The rules (spec: GPING#the-schedule): a day at least, no two slots at
    /// one time, and no two pings within five hours on a day both run.
    #[test]
    fn slots_are_refused_by_the_schedules_rules() {
        let defaults = default_schedule();
        assert_eq!(refusals(&defaults), vec![None, None]);

        let clash = vec![slot("07:20 Mon..Fri"), slot("07:20 Sat")];
        assert!(
            refusal(&clash, 1)
                .expect("refused")
                .contains("another slot is at 07:20")
        );

        let near = vec![slot("07:20 Mon..Fri"), slot("12:19 Mon..Fri")];
        let said = refusal(&near, 1).expect("refused");
        assert!(
            said.contains("within 5 h of 07:20's ping at 07:31"),
            "{said}"
        );
        assert!(refusal(&near, 0).is_some(), "both are refused");

        // Exactly five hours apart: the first window has closed.
        let apart = vec![slot("07:20 Mon..Fri"), slot("12:20 Mon..Fri")];
        assert_eq!(refusal(&apart, 1), None);

        // No shared day, no clash.
        let split = vec![slot("07:20 Mon..Fri"), slot("09:00 Sat,Sun")];
        assert_eq!(refusals(&split), vec![None, None]);

        // Across midnight: Friday 22:00's ping and Saturday 01:00's.
        let midnight = vec![slot("22:00 Fri"), slot("01:00 Sat")];
        assert!(refusal(&midnight, 1).is_some());
        // And across the week: Sunday 23:00 and Monday 02:00.
        let week = vec![slot("23:00 Sun"), slot("02:00 Mon")];
        assert!(refusal(&week, 0).is_some());

        let empty = vec![ScheduledSlot {
            time: timers::parse_slot("07:20").expect("time"),
            days: Days::NONE,
        }];
        assert!(
            refusal(&empty, 0)
                .expect("refused")
                .contains("at least one day")
        );
    }

    #[test]
    fn the_units_carry_each_slots_days() {
        let exe = Path::new("/opt/toker/toker");
        let state_dir = Path::new("/home/user/.local/share/toker");
        let slots = vec![slot("09:00 Mon..Fri"), slot("23:55 Sat,Sun")];
        let wake = wake_system_unit(&slots);
        assert!(wake.contains("OnCalendar=Mon..Fri 09:00\nOnCalendar=Sat,Sun 23:55\n"));
        let (hold, _) = hold_user_units(&slots, exe, state_dir);
        assert!(hold.contains("OnCalendar=Mon..Fri 09:00\nOnCalendar=Sat,Sun 23:55\n"));
        let pings = ping_user_units(&slots, exe, state_dir);
        assert_eq!(pings[0].timer_name, "toker-ping-0900.timer");
        assert!(pings[0].timer.contains("OnCalendar=Mon..Fri 09:11"));
        assert!(
            pings[0]
                .service
                .contains("ping-window --slot=09:00 --days=Mon..Fri")
        );
        // The wrapped weekend slot pings Sunday and Monday, spelled Monday
        // first.
        assert!(pings[1].timer.contains("OnCalendar=Mon,Sun 00:06"));
        assert!(pings[1].service.contains("--slot=23:55 --days=Sat,Sun"));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("toker-schedule-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn the_schedule_reads_back_from_the_hold_units() {
        let dir = scratch("read");
        assert_eq!(read_installed(&dir), Installed::default());
        let exe = Path::new("/opt/toker/toker");
        let slots = vec![slot("12:20 Mon..Fri"), slot("09:40 Sat,Sun")];
        let (timer, service) = hold_user_units(&slots, exe, &dir);
        std::fs::write(dir.join(HOLD_TIMER_UNIT), timer).expect("timer");
        std::fs::write(dir.join(HOLD_SERVICE_UNIT), service).expect("service");
        let installed = read_installed(&dir);
        assert_eq!(installed.slots, sorted(slots));
        assert_eq!(installed.exe.as_deref(), Some(exe));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What an earlier version wrote reads as the weekday slots it meant.
    #[test]
    fn a_hold_timer_written_before_days_reads_as_weekdays() {
        let dir = scratch("legacy");
        std::fs::write(
            dir.join(HOLD_TIMER_UNIT),
            "[Timer]\nOnCalendar=Mon..Fri 07:20\nOnCalendar=Mon..Fri 12:20\nPersistent=false\n",
        )
        .expect("timer");
        assert_eq!(read_installed(&dir).slots, default_schedule());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A user manager that records what it was asked and answers every call.
    #[derive(Default)]
    struct Recorder {
        calls: RefCell<Vec<String>>,
        failing: Option<&'static str>,
    }

    impl UserManager for Recorder {
        fn systemctl_user(&self, args: &[&str]) -> Result<Output> {
            let call = args.join(" ");
            let code = if self
                .failing
                .is_some_and(|failing| call.starts_with(failing))
            {
                1
            } else {
                0
            };
            self.calls.borrow_mut().push(call);
            Ok(Output {
                status: ExitStatus::from_raw(code << 8),
                stdout: Vec::new(),
                stderr: b"no".to_vec(),
            })
        }

        fn install_unit(&self, name: &str, _contents: &str) -> Result<PathBuf> {
            self.calls.borrow_mut().push(format!("write {name}"));
            Ok(PathBuf::from(name))
        }

        fn remove_unit(&self, name: &str) -> Result<()> {
            self.calls.borrow_mut().push(format!("remove {name}"));
            Ok(())
        }
    }

    #[test]
    fn applying_retires_the_removed_slots_and_restarts_the_rest() {
        let recorder = Recorder::default();
        let installed = vec![slot("07:20 Mon..Fri"), slot("12:20 Mon..Fri")];
        let slots = vec![slot("07:20 Mon..Fri"), slot("18:00 Sat")];
        apply_user(
            &recorder,
            &installed,
            &slots,
            Path::new("/opt/toker/toker"),
            Path::new("/state"),
        )
        .expect("applied");
        assert_eq!(
            recorder.calls.into_inner(),
            [
                "disable --now toker-ping-1220.timer",
                "write toker-hold.timer",
                "write toker-hold.service",
                "write toker-ping-0720.timer",
                "write toker-ping-0720.service",
                "write toker-ping-1800.timer",
                "write toker-ping-1800.service",
                "remove toker-ping-1220.timer",
                "remove toker-ping-1220.service",
                "daemon-reload",
                "enable toker-hold.timer toker-ping-0720.timer toker-ping-1800.timer",
                "restart toker-hold.timer toker-ping-0720.timer toker-ping-1800.timer",
            ]
        );
    }

    #[test]
    fn a_failed_call_stops_the_apply_and_says_why() {
        let recorder = Recorder {
            failing: Some("daemon-reload"),
            ..Recorder::default()
        };
        let slots = default_schedule();
        let error = apply_user(&recorder, &slots, &slots, Path::new("/x"), Path::new("/s"))
            .expect_err("the reload failed");
        assert!(
            error.to_string().contains("daemon-reload failed: no"),
            "{error}"
        );
        assert!(
            !recorder
                .calls
                .into_inner()
                .iter()
                .any(|call| call.starts_with("enable"))
        );
    }

    /// The elevated script holds only the writes and the system manager's
    /// reload, with the units whole in it (spec: GPING#where-the-schedule-is-kept).
    #[test]
    fn the_place_script_carries_the_units_whole() {
        let slots = default_schedule();
        let script = place_script(&slots);
        assert!(script.starts_with("set -eu\n"));
        assert!(script.contains(&format!(
            "cat > /etc/systemd/system/toker-wake.timer <<'TOKER_UNIT_END'\n{}TOKER_UNIT_END\n",
            wake_system_unit(&slots)
        )));
        assert!(script.contains(&format!(
            "cat > /etc/systemd/system/toker-wake.service <<'TOKER_UNIT_END'\n{}TOKER_UNIT_END\n",
            wake_system_service()
        )));
        assert!(script.ends_with(
            "systemctl daemon-reload\nsystemctl enable toker-wake.timer\nsystemctl restart toker-wake.timer\n"
        ));
        assert!(!script.contains("toker hold") && !script.contains("/opt/toker"));
        let removal = remove_script();
        assert!(removal.contains("rm -f /etc/systemd/system/toker-wake.timer"));
        assert!(removal.ends_with("systemctl daemon-reload\n"));
    }

    #[test]
    fn a_unit_line_cannot_close_its_heredoc() {
        assert_eq!(delimiter("a\nb\n"), "TOKER_UNIT_END");
        assert_eq!(delimiter("a\nTOKER_UNIT_END\n"), "TOKER_UNIT_END_");
    }

    /// The scripts run as written under a real shell, against a scratch
    /// directory standing in for the system units dir.
    #[test]
    fn the_place_script_writes_the_units_byte_for_byte() {
        let dir = scratch("place");
        let slots = default_schedule();
        let script = place_script(&slots)
            .replace(SYSTEM_UNITS_DIR, &dir.display().to_string())
            .replace("systemctl", "true");
        let status = std::process::Command::new(SHELL)
            .args(["-c", &script])
            .status()
            .expect("sh runs");
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(dir.join(WAKE_TIMER_UNIT)).expect("timer"),
            wake_system_unit(&slots)
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(WAKE_SERVICE_UNIT)).expect("service"),
            wake_system_service()
        );
        let removal = remove_script()
            .replace(SYSTEM_UNITS_DIR, &dir.display().to_string())
            .replace("systemctl", "true");
        let status = std::process::Command::new(SHELL)
            .args(["-c", &removal])
            .status()
            .expect("sh runs");
        assert!(status.success());
        assert!(!dir.join(WAKE_TIMER_UNIT).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_elevation_outcome_says_what_happened() {
        let exit = |code: i32| Ok(ExitStatus::from_raw(code << 8));
        assert_eq!(elevation_outcome(&exit(0)), Ok(()));
        assert_eq!(
            elevation_outcome(&exit(126)),
            Err("elevation was refused".to_owned())
        );
        assert!(elevation_outcome(&exit(3)).unwrap_err().contains("exit 3"));
        let missing = Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(
            elevation_outcome(&missing)
                .unwrap_err()
                .contains("could not be run")
        );
    }
}
