//! The configuration pages: providers, the schedule, and everything else.
//!
//! Configuration lives in the store and is edited through the service, so
//! this page is a view of what the service says plus the requests that
//! change it (spec: UTUI#the-configuration-pages). The schedule's slots and
//! its wake are the machine's instead, held in the installed units, so the
//! page reads and writes those itself, and hands the one step that needs
//! elevation to the dashboard loop, which gives the terminal up for it
//! (spec: UTUI#schedule).

mod check;
mod form;
mod model;
mod providers;
mod schedule;
mod settings;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;
mod view;

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use crossterm::event::{KeyCode, KeyEvent};

use crate::schedule::{self as units, Installed, ScheduledSlot};
use crate::setup::wizard::ProcessRunner;
use crate::store::Store;
use crate::tui::control::{Client, Command};
use model::{Machine, Op, Run, State};

/// The keys the configuration pages act on themselves. The dashboard loop
/// forwards exactly these, so a key the page handles but this list lacks is
/// silently dead.
pub(super) fn owns_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Char(
            'e' | 'c'
                | 'u'
                | 'f'
                | 'v'
                | 'p'
                | 'n'
                | 'd'
                | 'x'
                | ' '
                | '1'
                | '2'
                | '3'
                | 'h'
                | 'j'
                | 'k'
                | 'l'
        ) | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Enter
            | KeyCode::Backspace
    )
}

/// The user units dir and the state dir the schedule's units are written
/// for, which the dashboard and setup resolve alike.
struct Places {
    units_dir: PathBuf,
    state_dir: PathBuf,
}

impl Places {
    fn real(db_path: &Path) -> Option<Places> {
        let units_dir = crate::setup::wizard::Paths::real().ok()?.units_dir;
        let state_dir = db_path.parent()?.to_path_buf();
        Some(Places {
            units_dir,
            state_dir,
        })
    }
}

pub(super) struct Page {
    client: Client,
    /// The routing snapshot's reader, for what in routing depends on each
    /// provider (spec: UTUI#providers).
    routing: Client,
    state: State,
    drawn: std::cell::RefCell<view::Drawn>,
    /// Where the schedule's units live; `None` with no home to find them in.
    places: Option<Places>,
    /// The binary the installed units run, which a rewrite keeps.
    exe: Option<PathBuf>,
    /// The runs last read from the store.
    runs: Vec<Run>,
    /// A step waiting on elevation, and what follows it once it is done.
    elevation: Option<(String, Vec<Op>)>,
}

impl Page {
    pub(super) fn new(port: u16, db_path: &Path, store: &Store) -> Page {
        let mut page = Page {
            client: Client::new(port, "config", "config-snapshot"),
            routing: Client::new(port, "routing", "routing-snapshot"),
            state: State::default(),
            drawn: Default::default(),
            places: Places::real(db_path),
            exe: None,
            runs: Vec::new(),
            elevation: None,
        };
        page.refresh(store);
        page
    }

    pub(super) fn refresh(&mut self, store: &Store) {
        if !self.client.refresh() {
            self.state.command_refused("snapshot");
        }
        // A read that cannot be queued leaves what is known as it was: the
        // dependents are marked unread by the reply, not by a full queue.
        let _ = self.routing.refresh();
        self.runs = read_runs(store);
        self.read_machine();
    }

    /// Arrive at the page: read it again, and open on the provider that
    /// needs attention (spec: UTUI#providers).
    pub(super) fn arrive(&mut self, store: &Store) {
        self.state.reopen();
        self.refresh(store);
    }

    /// Read the schedule from the units and the system manager.
    fn read_machine(&mut self) {
        let (installed, wake) = match &self.places {
            Some(places) => (
                units::read_installed(&places.units_dir),
                units::wake_placed(),
            ),
            None => (Installed::default(), None),
        };
        self.exe = installed.exe;
        self.state.set_machine(Machine {
            slots: installed.slots,
            wake,
            runs: self.runs.clone(),
        });
    }

    /// Carry out what the last key asked of the machine.
    fn carry_out(&mut self) {
        for op in self.state.take_ops() {
            self.run(op);
        }
    }

    fn run(&mut self, op: Op) {
        match op {
            Op::Send(command) => self.send(command),
            Op::Write(slots) => {
                if let Err(error) = self.write(&slots) {
                    self.state.report(format!("the timers: {error:#}"));
                }
                self.read_machine();
            }
            // The loop gives the terminal up for it.
            Op::Elevate { script, then } => self.elevation = Some((script, then)),
        }
    }

    /// Rewrite the user timers at `slots`, keeping the binary the installed
    /// ones run, or this one where none are installed.
    fn write(&self, slots: &[ScheduledSlot]) -> anyhow::Result<()> {
        let Some(places) = &self.places else {
            anyhow::bail!("no home to find the user units in");
        };
        let exe = match &self.exe {
            Some(exe) => exe.clone(),
            None => std::env::current_exe()?,
        };
        let runner = ProcessRunner::new(places.units_dir.clone());
        units::apply_user(&runner, self.state.slots(), slots, &exe, &places.state_dir)
    }

    /// The script waiting on elevation, for the loop to run with the
    /// terminal given up.
    pub(super) fn take_elevation(&self) -> Option<String> {
        self.elevation.as_ref().map(|(script, _)| script.clone())
    }

    /// What the elevated step came to: what follows it runs only if it
    /// succeeded, and otherwise the schedule is as it was.
    pub(super) fn elevated(&mut self, outcome: std::io::Result<ExitStatus>) {
        let Some((_, then)) = self.elevation.take() else {
            return;
        };
        match units::elevation_outcome(&outcome) {
            Ok(()) => {
                for op in then {
                    self.run(op);
                }
            }
            Err(error) => self
                .state
                .report(format!("{error}: the schedule is as it was")),
        }
        self.read_machine();
    }

    pub(super) fn poll(&mut self) {
        while let Some(reply) = self.client.poll() {
            if let Some(next) = self.state.receive(reply) {
                self.send(next);
            }
        }
        while let Some(reply) = self.routing.poll() {
            self.state.receive_routing(reply);
        }
    }

    /// The route the operator asked to be taken to on the routing page.
    pub(super) fn take_goto(&mut self) -> Option<String> {
        self.state.take_goto()
    }

    /// Handle a key owned by this page. Returns whether a text entry was
    /// open, so the loop knows the key was swallowed by it.
    pub(super) fn key(&mut self, key: KeyEvent) -> bool {
        let editing = self.state.editing().is_some();
        if let Some(command) = self.state.key(key) {
            self.send(command);
        }
        self.carry_out();
        editing
    }

    pub(super) fn editing(&self) -> bool {
        self.state.editing().is_some()
    }

    fn send(&mut self, command: Command) {
        let name = command.name;
        if !self.client.send(command) {
            self.state.command_refused(name);
        }
    }

    /// Draw the page, with its key overlay over it when `overlay` is set,
    /// and return where the header's chips landed.
    pub(super) fn render(
        &self,
        frame: &mut ratatui::Frame,
        overlay: bool,
    ) -> crate::tui::chrome::Header {
        let drawn = view::render(frame, &self.state, overlay);
        let header = drawn.header;
        self.drawn.replace(drawn);
        header
    }

    pub(super) fn click(&mut self, at: ratatui::layout::Position) {
        let drawn = self.drawn.borrow().clone();
        if let Some(tab) = drawn.tabs.iter().position(|tab| tab.contains(at)) {
            if let Some(view) = model::View::ALL.get(tab) {
                self.state.set_view(*view);
            }
        } else if let Some(entry) = crate::tui::chrome::entry_at(&drawn.rows, at) {
            self.state.select(entry);
        }
    }

    pub(super) fn wheel(&mut self, delta: i32) {
        self.state.move_selection(delta as isize);
    }
}

/// Each slot's newest run for each account, from the store's ping runs, in
/// the local clock. The newest few hundred runs cover any schedule's last
/// week many times over. Unreadable is no runs: the page then says a slot
/// never ran rather than failing.
fn read_runs(store: &Store) -> Vec<Run> {
    let Ok(pings) = store.pings_since(0, 500) else {
        return Vec::new();
    };
    let emails: std::collections::HashMap<i64, String> = store
        .load_accounts()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|account| Some((account.number, account.email?)))
        .collect();
    let tz = jiff::tz::TimeZone::system();
    let now = jiff::Timestamp::now().as_millisecond();
    let mut seen = std::collections::HashSet::new();
    pings
        .iter()
        .rev()
        .filter_map(|ping| {
            let slot = ping.slot.clone()?;
            seen.insert((slot.clone(), ping.account_number))
                .then(|| Run {
                    slot,
                    when: when(ping.ts_ms, now, &tz),
                    account: ping.account_number.map(|number| {
                        emails
                            .get(&number)
                            .cloned()
                            .unwrap_or_else(|| format!("account {number}"))
                    }),
                    did: ping.action,
                    verified: ping.verified,
                    predicted: ping.boundary_ms.map(|ms| clock(ms, &tz)),
                    observed: ping.observed_ms.map(|ms| clock(ms, &tz)),
                })
        })
        .collect()
}

/// `hh:mm` in `tz`.
fn clock(ms: i64, tz: &jiff::tz::TimeZone) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|at| at.to_zoned(tz.clone()).strftime("%H:%M").to_string())
        .unwrap_or_default()
}

/// When a run happened, as a fixed reading rather than one relative to now
/// that would go stale while the page is open: the weekday within the last
/// week, the date before that.
fn when(ms: i64, now: i64, tz: &jiff::tz::TimeZone) -> String {
    let Ok(at) = jiff::Timestamp::from_millisecond(ms) else {
        return String::new();
    };
    let zoned = at.to_zoned(tz.clone());
    let format = if now - ms < 6 * 86_400_000 {
        "%a %H:%M"
    } else {
        "%-d %b %H:%M"
    };
    zoned.strftime(format).to_string()
}
