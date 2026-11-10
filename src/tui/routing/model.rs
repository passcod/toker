//! Content-free routing snapshot, editing, and navigation state.
//!
//! The page is six tabs, in the order a request is resolved
//! (spec: UTUI#the-routing-page). Each lookup tab is a table of records, the
//! selected one's detail beneath it; the routes tab can open one route whole;
//! the models tab is a stack of panels with one focus among them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reqwest::Method;
use serde_json::{Value, json};

use super::data::{self, array, text};
use super::editing::{Editing, What};
use super::form::{Context, Form, Outcome, Purpose};
use super::history::{self, Armed};
use super::trace::{Picking, Subject, Trace};
use super::{models_tab, routes, rules, template};
use crate::tui::control::{Command, Reply, Response};

/// How many rows a page key moves a selection.
const PAGE: isize = 10;

/// The routing page's tabs. The first four are the lookup a request walks, in
/// order; models and history shape a route and record it rather than find
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    Presentations,
    Rewrites,
    Defaults,
    Routes,
    Models,
    History,
}

impl Tab {
    pub(super) const ALL: [Tab; 6] = [
        Tab::Presentations,
        Tab::Rewrites,
        Tab::Defaults,
        Tab::Routes,
        Tab::Models,
        Tab::History,
    ];

    pub(super) fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    /// The tab's label in the strip: lower case, like every other tab.
    pub(super) fn label(self) -> &'static str {
        match self {
            Tab::Presentations => "presentations",
            Tab::Rewrites => "rewrites",
            Tab::Defaults => "defaults",
            Tab::Routes => "routes",
            Tab::Models => "models",
            Tab::History => "history",
        }
    }

    /// The title of the tab's table panel.
    pub(super) fn title(self) -> &'static str {
        match self {
            Tab::Presentations => "PRESENTATIONS",
            Tab::Rewrites => "REWRITES",
            Tab::Defaults => "DEFAULTS",
            Tab::Routes => "ROUTES",
            Tab::Models => "MODELS",
            Tab::History => "HISTORY",
        }
    }

    /// What stands between the previous tab and this one: an arrow where a
    /// request goes on from the one before, a dot where this tab sits apart
    /// from the sequence.
    pub(super) fn joiner(self) -> &'static str {
        match self {
            Tab::Presentations | Tab::Rewrites | Tab::Defaults | Tab::Routes => " → ",
            Tab::Models | Tab::History => " · ",
        }
    }

    /// The tab a name in `first_refused_tab` stands for.
    fn named(name: &str) -> Option<Tab> {
        Self::ALL
            .into_iter()
            .find(|tab| tab.label() == name && !matches!(tab, Tab::Models | Tab::History))
    }

    fn next(self, delta: isize) -> Tab {
        let len = Self::ALL.len() as isize;
        Self::ALL[(self.index() as isize + delta).rem_euclid(len) as usize]
    }
}

/// Whether the routes tab shows its table or one route whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Table,
    /// The selected route, in the page of its own.
    Route,
}

/// The panels of the models tab that hold the focus, top to bottom. The
/// compaction line holds it only where there is a policy to hold it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Panel {
    Families,
    Upgrades,
    Compaction,
    Offers,
}

/// Where the models tab's focus and selections are. Only the focused panel
/// shows a selection, and the focus moves on past a panel's edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Models {
    pub(super) focus: Panel,
    pub(super) family: usize,
    pub(super) upgrade: usize,
    /// How far the offers window is scrolled: the first offer it shows.
    pub(super) offers_at: usize,
    /// Which binding's catalogue the offers show.
    pub(super) offers_binding: usize,
}

impl Default for Models {
    fn default() -> Self {
        Models {
            focus: Panel::Upgrades,
            family: 0,
            upgrade: 0,
            offers_at: 0,
            offers_binding: 0,
        }
    }
}

/// A small question the page asks over itself: a promotion's or a
/// description's one-line form, or the confirmation of a reactivation.
/// Records are edited in place, not here (see [`Editing`]).
#[derive(Debug, Clone, Default)]
pub(super) enum Modal {
    #[default]
    None,
    Form(Box<Form>),
    /// Which request to trace.
    Trace(Box<Picking>),
}

impl Modal {
    pub(super) fn active(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub(super) fn title(&self) -> String {
        match self {
            Self::None => String::new(),
            Self::Form(form) => form.title().to_uppercase(),
            Self::Trace(_) => "TRACE A REQUEST".to_owned(),
        }
    }
}

/// Data and selection the page preserves while the dashboard is visible.
#[derive(Debug)]
pub(super) struct State {
    tab: Tab,
    mode: Mode,
    /// Whether the page has chosen its first tab from the first snapshot.
    opened: bool,
    snapshot: Option<Value>,
    revisions: Vec<Value>,
    /// The revision the last snapshot said is live: what an edit is made
    /// against.
    active_revision_id: Option<i64>,
    /// The selected entry of each tab, by [`Tab::index`], so a tab keeps its
    /// place while another is looked at.
    selected: [usize; 6],
    models: Models,
    error: Option<String>,
    /// The outcome of the last post-activation Claude settings sync.
    settings_note: Option<String>,
    loading: bool,
    /// A save, a promotion or a reactivation is on its way.
    in_flight: bool,
    modal: Modal,
    /// The record, candidate or move being edited where it is shown.
    editing: Option<Editing>,
    /// The selected candidate of the open route.
    candidate: usize,
    /// Whether the last thing done was a save that landed.
    saved: bool,
    /// What the last promotion did, said once.
    notice: Option<String>,
    /// The snapshot is out of date: a save found another revision live.
    refresh_wanted: bool,
    /// The trace on show, over whichever tab is open.
    trace: Option<Trace>,
    /// The revision the invented request being filled in is traced under.
    tracing_under: Option<i64>,
    /// A move of the running lanes that has asked once and waits for the
    /// second confirmation.
    armed: Option<Armed>,
    /// The revision to read the next page of history before, where the
    /// daemon said there are older ones than those read.
    older_revisions: Option<i64>,
    /// The `before` of the page on its way, so one is not asked for twice.
    revisions_asked: Option<i64>,
}

impl Default for State {
    fn default() -> Self {
        State {
            tab: Tab::Presentations,
            mode: Mode::Table,
            opened: false,
            snapshot: None,
            revisions: Vec::new(),
            active_revision_id: None,
            selected: [0; 6],
            models: Models::default(),
            error: None,
            settings_note: None,
            loading: false,
            in_flight: false,
            modal: Modal::None,
            editing: None,
            candidate: 0,
            saved: false,
            notice: None,
            refresh_wanted: false,
            trace: None,
            tracing_under: None,
            older_revisions: None,
            revisions_asked: None,
            armed: None,
        }
    }
}

/// The record a selected row stands for, in the policy document: what an edit
/// or a removal addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Target {
    /// The document's collection: `rewrites`, `routes`, `defaults` and so on.
    pub(super) kind: &'static str,
    /// Its identifier; a default is addressed by its scope instead.
    pub(super) id: Option<String>,
    pub(super) scope: Option<Value>,
}

impl State {
    pub(super) fn tab(&self) -> Tab {
        self.tab
    }

    pub(super) fn mode(&self) -> Mode {
        self.mode
    }

    pub(super) fn models(&self) -> &Models {
        &self.models
    }

    /// The selected entry of `tab`.
    pub(super) fn selected_in(&self, tab: Tab) -> usize {
        self.selected[tab.index()]
    }

    /// The revision the daemon last reported live.
    pub(super) fn active_revision_id(&self) -> Option<i64> {
        self.active_revision_id
    }

    /// What the daemon last sent.
    pub(super) fn snapshot(&self) -> Option<&Value> {
        self.snapshot.as_ref()
    }

    /// The facts derived for the live revision, when it compiled.
    pub(super) fn active(&self) -> Option<&Value> {
        self.snapshot.as_ref().and_then(data::active)
    }

    /// The revisions, newest first.
    pub(super) fn revisions(&self) -> &[Value] {
        &self.revisions
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(super) fn settings_note(&self) -> Option<&str> {
        self.settings_note.as_deref()
    }

    pub(super) fn set_settings_note(&mut self, note: String) {
        self.settings_note = Some(note);
    }

    pub(super) fn loading(&self) -> bool {
        self.loading
    }

    pub(super) fn modal(&self) -> &Modal {
        &self.modal
    }

    pub(super) fn in_flight(&self) -> bool {
        self.in_flight
    }

    pub(super) fn editing(&self) -> Option<&Editing> {
        self.editing.as_ref()
    }

    /// The selected candidate of the open route.
    pub(super) fn candidate(&self) -> usize {
        self.candidate
    }

    /// Whether a form owns the keys, an edit in place or a small question.
    pub(super) fn capturing(&self) -> bool {
        self.editing.is_some() || self.modal.active()
    }

    /// What the page leads its header status with: that something is being
    /// edited, or that the last save landed.
    pub(super) fn lead(&self) -> Option<&'static str> {
        if self.editing.is_some() {
            Some("editing")
        } else if self.saved {
            Some("saved")
        } else {
            None
        }
    }

    /// The move of the running lanes that waits to be confirmed.
    pub(super) fn armed(&self) -> Option<&Armed> {
        self.armed.as_ref()
    }

    /// The trace on show.
    pub(super) fn trace(&self) -> Option<&Trace> {
        self.trace.as_ref()
    }

    pub(super) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Whether the snapshot should be read again, once.
    pub(super) fn take_refresh_wanted(&mut self) -> bool {
        std::mem::take(&mut self.refresh_wanted)
    }

    /// How many records a tab holds, for the strip. Models counts its
    /// families, upgrades and compaction together.
    pub(super) fn tab_count(&self, tab: Tab) -> Option<usize> {
        match tab {
            Tab::History => Some(self.revisions.len()),
            Tab::Models => self.active().map(models_tab::record_count),
            _ => self.active().map(|active| rules::record_count(active, tab)),
        }
    }

    // --- navigation -------------------------------------------------------

    /// Left or right: between the tabs, or, inside a route, from one route to
    /// the next, or, in the offers, from one catalogue to the next.
    pub(super) fn move_view(&mut self, delta: isize) {
        match (self.tab, self.mode) {
            (Tab::Routes, Mode::Route) => self.step_route(delta),
            (Tab::Models, _) if self.models.focus == Panel::Offers => {
                let count = self.offer_bindings().len();
                if count > 0 {
                    self.models.offers_binding = (self.models.offers_binding as isize + delta)
                        .rem_euclid(count as isize)
                        as usize;
                    self.models.offers_at = 0;
                }
            }
            _ => self.set_tab(self.tab.next(delta)),
        }
    }

    /// Show `binding`'s offers from the `at`th, for a test.
    #[cfg(test)]
    pub(super) fn show_offers(&mut self, binding: &str, at: usize) {
        if let Some(index) = self
            .offer_bindings()
            .iter()
            .position(|have| *have == binding)
        {
            self.models.offers_binding = index;
        }
        self.models.offers_at = at;
    }

    pub(super) fn set_view(&mut self, index: usize) {
        if let Some(tab) = Tab::ALL.get(index) {
            self.set_tab(*tab);
        }
    }

    pub(super) fn set_tab(&mut self, tab: Tab) {
        self.armed = None;
        self.tab = tab;
        self.mode = Mode::Table;
    }

    /// Up or down within the tab; in the models tab, on into the next panel
    /// past a panel's edge.
    pub(super) fn move_selection(&mut self, delta: isize) {
        self.armed = None;
        match (self.tab, self.mode) {
            (Tab::Routes, Mode::Route) => {
                let count = self.route_candidates();
                self.candidate = if count == 0 {
                    0
                } else {
                    self.candidate.saturating_add_signed(delta).min(count - 1)
                };
            }
            (_, Mode::Route) => {}
            (Tab::Models, _) => self.move_models(delta),
            (tab, _) => {
                let count = self.entry_count(tab);
                let at = &mut self.selected[tab.index()];
                *at = if count == 0 {
                    0
                } else {
                    at.saturating_add_signed(delta).min(count - 1)
                };
            }
        }
    }

    pub(super) fn page(&mut self, down: bool) {
        self.move_selection(if down { PAGE } else { -PAGE });
    }

    /// Select the entry at `index`, as a click does.
    pub(super) fn select(&mut self, index: usize) {
        if self.mode == Mode::Route || self.tab == Tab::Models {
            return;
        }
        if index < self.entry_count(self.tab) {
            self.selected[self.tab.index()] = index;
        }
    }

    /// Open the selected record's route: from the routes tab, the route
    /// itself; from a rule, a default or a presentation, the route it sends
    /// to, in the routes tab.
    pub(super) fn open(&mut self) {
        let Some(active) = self.active() else {
            return;
        };
        let route = match self.tab {
            Tab::Routes => Some(self.selected[Tab::Routes.index()]),
            Tab::Presentations | Tab::Rewrites | Tab::Defaults => rules::entries(active, self.tab)
                .get(self.selected[self.tab.index()])
                .and_then(|entry| entry.route_id())
                .and_then(|id| routes::position(active, id)),
            Tab::Models | Tab::History => None,
        };
        if let Some(route) = route {
            self.selected[Tab::Routes.index()] = route;
            self.candidate = 0;
            self.tab = Tab::Routes;
            self.mode = Mode::Route;
        }
    }

    /// Open the route named `id`, where the live revision holds one, as a
    /// provider's detail on the config page asks to.
    pub(super) fn open_route_named(&mut self, id: &str) {
        let Some(route) = self
            .active()
            .and_then(|active| routes::position(active, id))
        else {
            return;
        };
        self.selected[Tab::Routes.index()] = route;
        self.candidate = 0;
        self.tab = Tab::Routes;
        self.mode = Mode::Route;
    }

    /// Leave the route page for the table, the selection where it was.
    /// Returns whether there was a page to leave.
    pub(super) fn close(&mut self) -> bool {
        let was = self.mode == Mode::Route;
        self.mode = Mode::Table;
        was
    }

    fn step_route(&mut self, delta: isize) {
        let count = self.entry_count(Tab::Routes);
        if count == 0 {
            return;
        }
        let at = &mut self.selected[Tab::Routes.index()];
        *at = (*at as isize + delta).rem_euclid(count as isize) as usize;
        self.candidate = 0;
    }

    /// The open route's derived record.
    fn open_route(&self) -> Option<&Value> {
        array(self.active()?, "routes").get(self.selected_in(Tab::Routes))
    }

    fn route_candidates(&self) -> usize {
        self.open_route()
            .map_or(0, |route| array(route, "candidates").len())
    }

    fn move_models(&mut self, delta: isize) {
        let Some(active) = self.active() else {
            return;
        };
        let families = models_tab::families(active).len();
        let upgrades = models_tab::upgrades(active).len();
        let compaction = models_tab::compaction(active).is_some();
        let offers = self.offers_shown().len();
        let models = &mut self.models;
        // Whether a step of `delta` stayed inside a list of `count`.
        let step = |at: &mut usize, count: usize| -> bool {
            match at.checked_add_signed(delta) {
                Some(next) if next < count => {
                    *at = next;
                    true
                }
                _ => false,
            }
        };
        // The panel past `models.focus`'s lower edge, and the one past its
        // upper edge: the focus moves on where there is one.
        let below_upgrades = if compaction {
            Panel::Compaction
        } else {
            Panel::Offers
        };
        let above_offers = if compaction {
            Panel::Compaction
        } else {
            Panel::Upgrades
        };
        match models.focus {
            Panel::Families => {
                if !step(&mut models.family, families) && delta > 0 {
                    models.focus = Panel::Upgrades;
                    models.upgrade = 0;
                }
            }
            Panel::Upgrades => {
                if !step(&mut models.upgrade, upgrades) {
                    if delta < 0 && families > 0 {
                        models.focus = Panel::Families;
                        models.family = families - 1;
                    } else if delta > 0 {
                        models.focus = below_upgrades;
                    }
                }
            }
            Panel::Compaction => {
                if delta < 0 && upgrades > 0 {
                    models.focus = Panel::Upgrades;
                    models.upgrade = upgrades - 1;
                } else if delta > 0 {
                    models.focus = Panel::Offers;
                }
            }
            Panel::Offers => {
                let at = models.offers_at;
                match at.checked_add_signed(delta) {
                    Some(next) if next < offers => models.offers_at = next,
                    Some(_) => models.offers_at = offers.saturating_sub(1),
                    None if at == 0 => {
                        models.focus = above_offers;
                        models.upgrade = upgrades.saturating_sub(1);
                    }
                    None => models.offers_at = 0,
                }
            }
        }
    }

    /// The bindings the catalogue holds offers for, sorted.
    pub(super) fn offer_bindings(&self) -> Vec<&str> {
        let mut bindings: Vec<&str> = self
            .snapshot
            .as_ref()
            .map(|snapshot| {
                array(snapshot, "offers")
                    .iter()
                    .map(|offer| text(offer, "binding_id"))
                    .collect()
            })
            .unwrap_or_default();
        bindings.sort_unstable();
        bindings.dedup();
        bindings
    }

    /// The offers of the binding the models tab is showing, by model.
    pub(super) fn offers_shown(&self) -> Vec<&Value> {
        let bindings = self.offer_bindings();
        let Some(binding) = bindings
            .get(
                self.models
                    .offers_binding
                    .min(bindings.len().saturating_sub(1)),
            )
            .copied()
        else {
            return Vec::new();
        };
        let mut offers: Vec<&Value> = self
            .snapshot
            .as_ref()
            .map(|snapshot| {
                array(snapshot, "offers")
                    .iter()
                    .filter(|offer| text(offer, "binding_id") == binding)
                    .collect()
            })
            .unwrap_or_default();
        offers.sort_by_key(|offer| text(offer, "model_id"));
        offers
    }

    /// The form that has the focus, whichever kind it is.
    fn form_mut(&mut self) -> Option<&mut Form> {
        match (&mut self.editing, &mut self.modal) {
            (Some(editing), _) => Some(&mut editing.form),
            (None, Modal::Form(form)) => Some(form),
            _ => None,
        }
    }

    /// Focus a field of the open form, as a click does.
    pub(super) fn focus_form_row(&mut self, row: usize) {
        if let Some(form) = self.form_mut() {
            form.focus_row(row);
        }
    }

    /// Move the open form's focus, as the wheel does.
    pub(super) fn step_form(&mut self, delta: isize) {
        if let Some(form) = self.form_mut() {
            form.step(delta);
        }
    }

    // --- entries ----------------------------------------------------------

    /// How many selectable entries `tab` has.
    pub(super) fn entry_count(&self, tab: Tab) -> usize {
        match tab {
            Tab::History => self.revisions.len(),
            Tab::Models => 0,
            Tab::Routes => self
                .active()
                .map_or(0, |active| array(active, "routes").len()),
            _ => self
                .active()
                .map_or(0, |active| rules::entries(active, tab).len()),
        }
    }

    /// The record the selection stands for, where it stands for one.
    pub(super) fn target(&self) -> Option<Target> {
        let active = self.active()?;
        match self.tab {
            Tab::Presentations | Tab::Rewrites | Tab::Defaults => {
                let entry = rules::entries(active, self.tab)
                    .into_iter()
                    .nth(self.selected_in(self.tab))?;
                entry.target()
            }
            Tab::Routes => {
                let route = array(active, "routes").get(self.selected_in(Tab::Routes))?;
                Some(Target {
                    kind: "routes",
                    id: Some(text(route, "id").to_owned()),
                    scope: None,
                })
            }
            Tab::Models => match self.models.focus {
                Panel::Families => {
                    let family = models_tab::families(active).get(self.models.family)?;
                    Some(Target {
                        kind: "classifiers",
                        id: Some(text(family, "id").to_owned()),
                        scope: None,
                    })
                }
                Panel::Upgrades => {
                    let row = models_tab::upgrades(active)
                        .into_iter()
                        .nth(self.models.upgrade)?;
                    Some(Target {
                        kind: "upgrades",
                        id: Some(row.policy_id.to_owned()),
                        scope: None,
                    })
                }
                Panel::Compaction => models_tab::compaction(active).map(|_| Target {
                    kind: "compaction",
                    id: None,
                    scope: None,
                }),
                Panel::Offers => None,
            },
            Tab::History => None,
        }
    }

    /// The document's record for `target`.
    pub(super) fn record(&self, target: &Target) -> Option<&Value> {
        let policy = data::policy(self.snapshot.as_ref()?)?;
        match (target.kind, &target.scope, &target.id) {
            // The one policy a document holds, if any.
            ("compaction", ..) => policy.get("compaction").filter(|record| !record.is_null()),
            ("defaults", Some(scope), _) => array(policy, "defaults")
                .iter()
                .find(|record| record.get("scope") == Some(scope)),
            (kind, _, Some(id)) => array(policy, kind)
                .iter()
                .find(|record| text(record, "id") == id),
            _ => None,
        }
    }

    // --- replies ----------------------------------------------------------

    /// Begin a refresh. The caller sends it only when this returns true.
    pub(super) fn begin_refresh(&mut self) -> bool {
        if self.loading || self.in_flight {
            return false;
        }
        self.loading = true;
        true
    }

    pub(super) fn receive(&mut self, reply: Reply) {
        match reply {
            Reply::Command(command, Ok(response)) => self.receive_response(command, response),
            Reply::Command(command, Err(error)) => {
                self.settle(command.name);
                if let Some(editing) = &mut self.editing {
                    match command.name {
                        "validate" => {
                            editing.validating = false;
                            editing.unchecked = Some(error.clone());
                        }
                        "preview" => editing.replaying = false,
                        _ => {}
                    }
                }
                match (command.name, &mut self.trace, &mut self.modal) {
                    ("trace", Some(trace), _) => {
                        trace.failed(format!("trace: {error}"));
                        return;
                    }
                    ("recent", _, Modal::Trace(picking)) => {
                        picking.receive(
                            &Response {
                                status: 0,
                                body: Value::Null,
                            },
                            format!("recent requests: {error}"),
                        );
                        return;
                    }
                    _ => {}
                }
                self.error = Some(format!("{}: {error}", command.name));
            }
        }
    }

    /// A reply ends only what its own command began: a snapshot or revisions
    /// read queued before an edit must not release the edit's busy guard, and
    /// a check belongs to the edit it was asked about.
    fn settle(&mut self, name: &str) {
        match name {
            "snapshot" | "revisions" => self.loading = false,
            "validate" | "preview" | "trace" | "recent" => {}
            _ => self.in_flight = false,
        }
    }

    /// The worker queue refused a refresh, so no reply will ever come.
    pub(super) fn refresh_refused(&mut self) {
        self.loading = false;
        self.error = Some("snapshot: routing command queue is busy".to_owned());
    }

    /// The worker queue refused an operation, so no reply will ever come.
    pub(super) fn command_refused(&mut self, name: &str) {
        match (name, &mut self.editing) {
            // A check that was not taken is asked again at the next change.
            ("validate", Some(editing)) => {
                editing.validating = false;
                editing.requested = None;
            }
            ("preview", Some(editing)) => {
                editing.replaying = false;
                editing.replay_requested = None;
            }
            // A trace or a listing is not an edit: its own panel says it
            // was not taken, and nothing else is waiting on it.
            ("trace" | "recent", _) => {
                self.error = Some(format!("{name}: routing command queue is busy"));
            }
            _ => {
                self.in_flight = false;
                self.error = Some(format!("{name}: routing command queue is busy"));
            }
        }
    }

    fn receive_response(&mut self, command: Command, response: Response) {
        self.settle(command.name);
        if command.name == "snapshot" && response.status / 100 == 2 {
            self.receive_snapshot(response.body);
            return;
        }
        if command.name == "revisions" && response.status / 100 == 2 {
            // A page asked for with `before` is older history added to what
            // is read; the newest page replaces it.
            if command
                .query
                .as_deref()
                .is_none_or(|query| !query.split('&').any(|pair| pair.starts_with("before=")))
            {
                self.revisions.clear();
            }
            self.revisions
                .extend(array(&response.body, "revisions").iter().cloned());
            // Newest first, whatever order they were sent in.
            self.revisions.sort_by_key(|revision| {
                std::cmp::Reverse(revision.get("revision_id").and_then(Value::as_i64))
            });
            self.revisions
                .dedup_by_key(|revision| revision.get("revision_id").and_then(Value::as_i64));
            // Where the daemon says there are older ones, the page reads them
            // when the operator reaches the oldest it has: a history that
            // silently stopped at the newest page hid the revision a lane
            // was pinned to.
            self.older_revisions = (response.body.get("has_more") == Some(&Value::Bool(true)))
                .then(|| response.body.get("next_before").and_then(Value::as_i64))
                .flatten();
            self.revisions_asked = None;
            self.error = None;
            self.clamp_selection();
            return;
        }
        if matches!(command.name, "validate" | "preview") {
            self.receive_check(&command, &response);
            return;
        }
        if command.name == "trace" {
            if let Some(trace) = &mut self.trace {
                let error = control_error(&command, &response);
                trace.receive(&command, &response, error);
            }
            return;
        }
        if command.name == "recent" {
            if let Modal::Trace(picking) = &mut self.modal {
                let error = control_error(&command, &response);
                picking.receive(&response, error);
            }
            return;
        }
        if command.name == "save"
            && response.status == 409
            && response.body.get("error").and_then(Value::as_str) == Some("stale_revision")
        {
            // The form stays as entered; it says which revision is live
            // now, and the page reads it.
            self.moved(&response.body);
            return;
        }
        let code = response.body.get("error").and_then(Value::as_str);
        if matches!(command.name, "reactivate" | "widen") && response.status == 409 {
            match code {
                // The live revision moved under the operation: read what is
                // live now, and let the operator ask again.
                Some("stale_revision") => {
                    self.refresh_wanted = true;
                    self.error = Some(control_error(&command, &response));
                }
                // The daemon asks for the consent the page did not know to
                // ask for, as unpinned lanes may hold caches too.
                Some("forced_cache_transition_required") => {
                    let impacts = response.body.get("lane_impacts").unwrap_or(&Value::Null);
                    let live = self.active_revision_id.unwrap_or_default();
                    let count = data::count(impacts, "at_risk");
                    self.armed = Some(Armed::new(Vec::new(), count, live));
                    self.error = None;
                }
                Some("not_a_widening") => {
                    self.notice = Some("the live revision already moves running lanes".to_owned());
                }
                _ => self.error = Some(control_error(&command, &response)),
            }
            return;
        }
        if response.status / 100 != 2 {
            self.error = Some(control_error(&command, &response));
            return;
        }
        match command.name {
            "save" => {
                self.merge_activation(response.body);
                self.editing = None;
                self.modal = Modal::None;
                self.saved = true;
            }
            "reactivate" => {
                let copied = command.body.as_ref().map_or(0, |body| {
                    body.get("revision_id")
                        .and_then(Value::as_i64)
                        .unwrap_or_default()
                });
                self.merge_activation(response.body);
                self.notice = Some(format!(
                    "reactivated rev {copied} as rev {}",
                    self.active_revision_id.unwrap_or_default()
                ));
                self.saved = true;
            }
            "widen" => {
                self.notice = Some(format!(
                    "moved running lanes onto rev {}",
                    self.active_revision_id.unwrap_or_default()
                ));
                self.error = None;
                // Where the lanes are now is the snapshot's to say.
                self.refresh_wanted = true;
            }
            "promote" => {
                self.notice = Some(format!(
                    "promoted {} on {}",
                    text_of(&command.body.clone().unwrap_or_default(), "model_id"),
                    text_of(&command.body.clone().unwrap_or_default(), "binding_id"),
                ));
                self.modal = Modal::None;
                self.error = None;
            }
            _ => self.error = None,
        }
    }

    /// A check came back. It is kept for the edit that is open if it is for
    /// the operations that edit makes now; an answer to an older form is
    /// dropped, and the next change asks again.
    fn receive_check(&mut self, command: &Command, response: &Response) {
        let ops = command
            .body
            .as_ref()
            .and_then(|body| body.get("edit"))
            .and_then(|edit| edit.get("operations"))
            .cloned()
            .unwrap_or(Value::Null);
        let stale = response.status == 409
            && response.body.get("error").and_then(Value::as_str) == Some("stale_revision");
        if stale {
            self.moved(&response.body);
        }
        let Some(editing) = &mut self.editing else {
            return;
        };
        match command.name {
            "validate" => {
                editing.validating = false;
                if response.status / 100 == 2 {
                    editing.checked_with(ops, &response.body);
                } else if !stale {
                    editing.unchecked = Some(control_error(command, response));
                }
            }
            _ => {
                editing.replaying = false;
                if response.status / 100 == 2 {
                    editing.replayed_with(ops, &response.body);
                }
            }
        }
    }

    /// The live revision is not the one the open edit was made against.
    fn moved(&mut self, body: &Value) {
        let live = body.get("live_revision_id").and_then(Value::as_i64);
        if let Some(editing) = &mut self.editing {
            editing.moved_to = live;
        }
        self.refresh_wanted = true;
    }

    /// The next check to ask the daemon for, if the open edit needs one: its
    /// operations against the live revision, and once those are accepted,
    /// how recent requests would route under them. One of each at a time.
    /// The revision an older page of history should be read before, where
    /// there is one and the operator has reached the oldest revision read.
    /// `None` once it has been asked for, until the answer lands.
    pub(super) fn wants_older_revisions(&mut self) -> Option<i64> {
        let before = self.older_revisions?;
        if self.revisions_asked.is_some()
            || self.tab != Tab::History
            || self.selected_in(Tab::History) + 1 < self.revisions.len()
        {
            return None;
        }
        self.revisions_asked = Some(before);
        Some(before)
    }

    /// Whether the history read stops short of the oldest revision there is.
    pub(super) fn more_revisions(&self) -> bool {
        self.older_revisions.is_some()
    }

    pub(super) fn next_check(&mut self) -> Option<Command> {
        if self.in_flight {
            return None;
        }
        let revision = self.active_revision_id?;
        let editing = self.editing.as_mut()?;
        let ops = Value::Array(editing.operations());
        if ops.as_array().is_none_or(Vec::is_empty) {
            return None;
        }
        if !editing.validating && editing.requested.as_ref() != Some(&ops) {
            editing.requested = Some(ops.clone());
            editing.validating = true;
            return Some(command(
                "validate",
                Method::POST,
                "validate",
                "routing-validate",
                json!({ "expected_revision_id": revision, "edit": { "operations": ops } }),
            ));
        }
        if !editing.validating
            && !editing.replaying
            && editing.accepted(&ops)
            && editing.replay_requested.as_ref() != Some(&ops)
        {
            editing.replay_requested = Some(ops.clone());
            editing.replaying = true;
            return Some(command(
                "preview",
                Method::POST,
                "preview",
                "routing-preview",
                json!({
                    "mode": "replay_recent",
                    "expected_revision_id": revision,
                    "edit": { "operations": ops },
                }),
            ));
        }
        None
    }

    fn receive_snapshot(&mut self, snapshot: Value) {
        let revision = data::revision_id(&snapshot);
        if revision != self.active_revision_id
            && let Some(editing) = &mut self.editing
        {
            // What was checked was checked against a policy that is not live.
            editing.recheck();
        }
        self.active_revision_id = revision;
        // The page opens where something is refused, once: a later refresh
        // must not move an operator off the tab they chose
        // (spec: UTUI#the-routing-page).
        if !self.opened {
            self.opened = true;
            self.tab = data::active(&snapshot)
                .and_then(|active| active.get("first_refused_tab"))
                .and_then(Value::as_str)
                .and_then(Tab::named)
                .unwrap_or(Tab::Presentations);
        }
        self.snapshot = Some(snapshot);
        self.error = None;
        self.clamp_selection();
    }

    fn merge_activation(&mut self, body: Value) {
        if let Some(snapshot) = &mut self.snapshot {
            for key in [
                "active_revision",
                "activation",
                "lane_impacts",
                "publications",
            ] {
                if let Some(value) = body.get(key) {
                    snapshot[key] = value.clone();
                }
            }
            self.active_revision_id = data::revision_id(snapshot);
        }
        self.error = None;
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        for tab in Tab::ALL {
            let count = self.entry_count(tab);
            let at = &mut self.selected[tab.index()];
            *at = (*at).min(count.saturating_sub(1));
        }
        if let Some(active) = self.snapshot.as_ref().and_then(data::active) {
            self.models.family = self
                .models
                .family
                .min(models_tab::families(active).len().saturating_sub(1));
            self.models.upgrade = self
                .models
                .upgrade
                .min(models_tab::upgrades(active).len().saturating_sub(1));
        }
        let bindings = self.offer_bindings().len();
        self.models.offers_binding = self.models.offers_binding.min(bindings.saturating_sub(1));
        let offers = self.offers_shown().len();
        self.models.offers_at = self.models.offers_at.min(offers.saturating_sub(1));
        self.candidate = self
            .candidate
            .min(self.route_candidates().saturating_sub(1));
    }

    /// Handle a routing-page key.  Returned commands must be sent by the page's
    /// bounded worker; commands never perform synchronous I/O in this state.
    /// The checks an edit asks for follow from [`State::next_check`].
    pub(super) fn key(&mut self, key: KeyEvent) -> Option<Command> {
        self.saved = false;
        if self.in_flight {
            if key.code == KeyCode::Esc {
                self.modal = Modal::None;
                self.cancel_edit();
            }
            return None;
        }
        if self.editing.is_some() {
            return self.editing_key(key);
        }
        if self.modal.active() {
            return self.modal_key(key);
        }
        self.notice = None;
        let armed = self.armed.take();
        match key.code {
            KeyCode::Char('e') => {
                self.open_edit();
                None
            }
            KeyCode::Char('n') => {
                self.open_add();
                None
            }
            KeyCode::Char('y') => {
                self.open_duplicate();
                None
            }
            KeyCode::Char('d') => {
                self.open_remove();
                None
            }
            KeyCode::Char('m') if self.tab == Tab::History => self.move_lanes(armed),
            KeyCode::Char('m') => {
                self.open_move();
                None
            }
            KeyCode::Char('a') if self.tab == Tab::History => self.reactivate_selected(),
            KeyCode::Char('c') if self.tab == Tab::History => {
                let under = self
                    .selected_revision()
                    .filter(|revision| Some(*revision) != self.active_revision_id);
                self.open_trace(under)
            }
            // Only the key that armed a move can confirm it; esc is the
            // cancel, and stops there.
            KeyCode::Esc if armed.is_some() => None,
            KeyCode::Char('p') => {
                self.open_promote();
                None
            }
            KeyCode::Char('t') => self.open_trace(None),
            KeyCode::Enter => {
                if !self.follow_trace() {
                    self.open();
                }
                None
            }
            // The trace closes before the page it is over does.
            KeyCode::Esc => {
                if self.trace.take().is_none() {
                    self.close();
                }
                None
            }
            KeyCode::PageDown => {
                self.page(true);
                None
            }
            KeyCode::PageUp => {
                self.page(false);
                None
            }
            _ => None,
        }
    }

    /// A key in the open edit: the form's, or, in a move, the arrows that
    /// carry the lifted row.
    fn editing_key(&mut self, key: KeyEvent) -> Option<Command> {
        let moving = matches!(
            self.editing.as_ref()?.what,
            What::MoveRule { .. } | What::MoveCandidate { .. }
        );
        if moving {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.step_move(-1),
                KeyCode::Down | KeyCode::Char('j') => self.step_move(1),
                KeyCode::Esc => self.cancel_edit(),
                KeyCode::Enter => return self.save_edit(),
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return self.save_edit();
                }
                _ => {}
            }
            return None;
        }
        match self.editing.as_mut()?.form.key(key) {
            Outcome::Cancel => self.cancel_edit(),
            Outcome::Submit => return self.save_edit(),
            Outcome::Keep => {}
        }
        None
    }

    /// Abandon the open edit, putting back what a move displaced.
    fn cancel_edit(&mut self) {
        match self.editing.take().map(|editing| editing.what) {
            Some(What::MoveRule { from, to, .. }) => {
                let at = &mut self.selected[Tab::Rewrites.index()];
                *at = at.saturating_add_signed(from as isize - to as isize);
            }
            Some(What::MoveCandidate { from, .. }) => self.candidate = from,
            _ => {}
        }
    }

    /// Lift the moved row a place up or down, its scope or route being all
    /// it can move within.
    fn step_move(&mut self, delta: isize) {
        let Some(editing) = &mut self.editing else {
            return;
        };
        match &mut editing.what {
            What::MoveRule { rules, to, .. } => {
                let next = to.saturating_add_signed(delta).min(rules.len() - 1);
                let moved = next as isize - *to as isize;
                *to = next;
                let at = &mut self.selected[Tab::Rewrites.index()];
                *at = at.saturating_add_signed(moved);
            }
            What::MoveCandidate { ids, to, .. } => {
                *to = to.saturating_add_signed(delta).min(ids.len() - 1);
                self.candidate = *to;
            }
            _ => {}
        }
    }

    /// Save the open edit, if the daemon has accepted it as it stands.
    fn save_edit(&mut self) -> Option<Command> {
        let revision = self.active_revision_id?;
        let editing = self.editing.as_ref()?;
        if !editing.saveable() {
            return None;
        }
        let operations = editing.operations();
        self.in_flight = true;
        Some(command(
            "save",
            Method::POST,
            "save",
            "routing-save",
            json!({ "expected_revision_id": revision, "edit": { "operations": operations } }),
        ))
    }

    /// Where a record sits in its collection of the document.
    fn document_index(&self, target: &Target) -> Option<usize> {
        let policy = data::policy(self.snapshot.as_ref()?)?;
        match (target.kind, &target.scope, &target.id) {
            ("compaction", ..) => None,
            ("defaults", Some(scope), _) => array(policy, "defaults")
                .iter()
                .position(|record| record.get("scope") == Some(scope)),
            (kind, _, Some(id)) => array(policy, kind)
                .iter()
                .position(|record| text(record, "id") == id),
            _ => None,
        }
    }

    /// The document's route the open route page shows.
    fn document_route(&self) -> Option<(usize, &Value)> {
        let id = text(self.open_route()?, "id");
        let policy = data::policy(self.snapshot.as_ref()?)?;
        array(policy, "routes")
            .iter()
            .enumerate()
            .find(|(_, route)| text(route, "id") == id)
    }

    /// Where the open route is in the document, and how many steps its chain
    /// has: where a candidate added to it would be, for a test.
    #[cfg(test)]
    pub(super) fn document_route_for_tests(&self) -> (usize, usize) {
        let (index, route) = self.document_route().expect("a route is open");
        (index, array(&route["chain"], "steps").len())
    }

    /// Whether the open route page is the one in view.
    fn on_route_page(&self) -> bool {
        (self.tab, self.mode) == (Tab::Routes, Mode::Route)
    }

    fn open_edit(&mut self) {
        // The live revision's description is edited from the top of history.
        if self.tab == Tab::History && self.selected_in(Tab::History) == 0 {
            let description = self
                .snapshot
                .as_ref()
                .and_then(data::policy)
                .and_then(|policy| policy.get("description"))
                .cloned()
                .unwrap_or(Value::Null);
            self.modal = Modal::Form(Box::new(Form::new(
                Purpose::Description,
                json!({ "description": description }),
                Context::default(),
            )));
            return;
        }
        if self.on_route_page() {
            self.edit_candidate();
            return;
        }
        let Some(target) = self.target() else {
            return;
        };
        let Some(record) = self.record(&target).cloned() else {
            return;
        };
        let owner = self.document_index(&target);
        let form = Form::new(
            Purpose::Record {
                kind: target.kind,
                adding: false,
            },
            record,
            self.form_context(),
        );
        self.editing = Some(Editing::new(
            What::Record {
                kind: target.kind,
                adding: false,
            },
            form,
            owner,
            None,
        ));
    }

    /// The selected candidate of the open route, and the edge into it.
    fn edit_candidate(&mut self) {
        let Some((owner, route)) = self.document_route() else {
            return;
        };
        let route = route.clone();
        let index = self.candidate;
        let (edge, candidate) = if index == 0 {
            (Value::Null, route["chain"]["first"].clone())
        } else {
            let step = &route["chain"]["steps"][index - 1];
            (step["edge"].clone(), step["candidate"].clone())
        };
        let (value, with_edge) = if index == 0 {
            (json!({ "candidate": candidate }), false)
        } else {
            (json!({ "edge": edge, "candidate": candidate }), true)
        };
        let form = Form::new(Purpose::Candidate { with_edge }, value, self.form_context());
        self.editing = Some(Editing::new(
            What::EditCandidate { route, index },
            form,
            Some(owner),
            Some(index),
        ));
    }

    /// What a form completes from: the bindings toker implements, what the
    /// catalogues offer, the policy's record ids and what its routes are.
    fn form_context(&self) -> Context {
        let mut context = Context {
            bindings: crate::routing::BACKEND_BINDINGS
                .iter()
                .map(|binding| binding.id_str().to_owned())
                .collect(),
            ..Context::default()
        };
        let Some(snapshot) = &self.snapshot else {
            return context;
        };
        let text =
            |value: &Value, name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
        context.offers = array(snapshot, "offers")
            .iter()
            .filter_map(|offer| Some((text(offer, "binding_id")?, text(offer, "model_id")?)))
            .collect();
        if let Some(policy) = data::policy(snapshot) {
            for collection in ["routes", "classifiers", "upgrades"] {
                context.ids.insert(
                    collection,
                    array(policy, collection)
                        .iter()
                        .filter_map(|record| text(record, "id"))
                        .collect(),
                );
            }
            // What tells a classifier or an upgrade policy apart from the
            // others in a field's list.
            let about = |collection: &'static str, say: fn(&Value) -> String| {
                array(policy, collection)
                    .iter()
                    .filter_map(|record| Some((text(record, "id")?, say(record))))
                    .collect()
            };
            context.about.insert(
                "classifiers",
                about("classifiers", |classifier| {
                    format!(
                        "{} · {}",
                        data::text(classifier, "binding_id"),
                        data::text(&classifier["model_matcher"], "value")
                    )
                }),
            );
            context.about.insert(
                "upgrades",
                about("upgrades", |upgrade| {
                    format!("on {}", data::text(upgrade, "classifier_id"))
                }),
            );
        }
        if let Some(active) = data::active(snapshot) {
            // A route reads as the providers its candidates reach in order,
            // as the routes tab shows it.
            let routes = context.about.entry("routes").or_default();
            for route in array(active, "routes") {
                let id = data::text(route, "id");
                let said: String = rules::route_summary(active, id)
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                routes.insert(id.to_owned(), said);
            }
        }
        context
    }

    /// Start adding a record to the table that shows it, or a fallback to
    /// the open route.
    fn open_add(&mut self) {
        if self.on_route_page() {
            self.add_candidate();
            return;
        }
        let Some(kind) = template::kind_for(self.tab, self.models.focus) else {
            self.error = Some("there is nothing to add in this view".to_owned());
            return;
        };
        let routes = self
            .snapshot
            .as_ref()
            .and_then(data::policy)
            .map_or(&[][..], |policy| array(policy, "routes"));
        // A rule, a default and a compaction policy send somewhere, so
        // there is nowhere to start until there is a route.
        if routes.is_empty() && matches!(kind, "rewrites" | "defaults" | "compaction") {
            self.error = Some(format!(
                "a {} needs a route to send to: add one in routes first (press 4)",
                super::words::record_noun(kind)
            ));
            return;
        }
        if kind == "compaction"
            && self
                .snapshot
                .as_ref()
                .and_then(data::policy)
                .is_some_and(|policy| policy.get("compaction").is_some_and(|c| !c.is_null()))
        {
            self.error = Some("there is a compaction policy already: edit it".to_owned());
            return;
        }
        let route_id = routes.first().map(|route| text(route, "id").to_owned());
        let mut value = template::template(kind, route_id.as_deref());
        // A new record starts in the scope of the one under the selection.
        if matches!(kind, "rewrites" | "defaults" | "presentations")
            && let Some(active) = self.active()
            && let Some(scope) = rules::entries(active, self.tab)
                .get(self.selected_in(self.tab))
                .and_then(|entry| entry.record().get("scope"))
                .filter(|scope| !scope.is_null())
        {
            value["scope"] = scope.clone();
        }
        self.start_adding(kind, value);
    }

    /// Open the add form filled from the selected record (spec: UTUI#editing),
    /// as a record of its own: a fresh identifier and, for a rewrite, the
    /// next free priority in its scope, so the copy lands just after the
    /// rule it was copied from where there is room.
    fn open_duplicate(&mut self) {
        let Some(target) = self.target() else {
            self.error = Some("select a record to duplicate".to_owned());
            return;
        };
        if target.kind == "compaction" {
            self.error = Some("there is one compaction policy: edit it".to_owned());
            return;
        }
        let Some(mut value) = self.record(&target).cloned() else {
            self.error = Some("select a record to duplicate".to_owned());
            return;
        };
        let records = self
            .snapshot
            .as_ref()
            .and_then(data::policy)
            .map_or(&[][..], |policy| array(policy, target.kind));
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            let taken: Vec<&str> = records.iter().map(|record| text(record, "id")).collect();
            let fresh = template::fresh_id(id, &taken);
            value["id"] = Value::from(fresh);
        }
        if target.kind == "rewrites" {
            // A priority is unique within its scope (spec: RDOC#collections).
            let scope = value.get("scope");
            let taken: Vec<i64> = records
                .iter()
                .filter(|record| record.get("scope") == scope)
                .filter_map(|record| record.get("priority").and_then(Value::as_i64))
                .collect();
            let from = value.get("priority").and_then(Value::as_i64).unwrap_or(0);
            let next = (from + 1..)
                .find(|priority| !taken.contains(priority))
                .unwrap_or(from + 1);
            value["priority"] = Value::from(next);
        }
        // A route copied from its own page is added in the routes table.
        self.mode = Mode::Table;
        self.start_adding(target.kind, value);
    }

    /// Open the add form for a new record of `kind`, starting from `value`.
    fn start_adding(&mut self, kind: &'static str, value: Value) {
        let owner = (kind != "compaction").then(|| {
            self.snapshot
                .as_ref()
                .and_then(data::policy)
                .map_or(0, |policy| array(policy, kind).len())
        });
        let form = Form::new(
            Purpose::Record { kind, adding: true },
            value,
            self.form_context(),
        );
        self.editing = Some(Editing::new(
            What::Record { kind, adding: true },
            form,
            owner,
            None,
        ));
    }

    /// A new fallback at the end of the open route.
    fn add_candidate(&mut self) {
        let Some((owner, route)) = self.document_route() else {
            return;
        };
        let route_id = text(route, "id").to_owned();
        let ids = candidate_ids(route);
        let after = ids.last().cloned().unwrap_or_default();
        let unused = (1..)
            .map(|n| format!("fallback-{n}"))
            .find(|id| !ids.contains(id))
            .unwrap_or_default();
        let mut candidate = template::template("routes", None)["chain"]["first"].clone();
        candidate["id"] = Value::from(unused);
        let value = json!({ "edge": super::form::default_edge(), "candidate": candidate });
        let form = Form::new(
            Purpose::Candidate { with_edge: true },
            value,
            self.form_context(),
        );
        let step = ids.len();
        self.editing = Some(Editing::new(
            What::AddCandidate { route_id, after },
            form,
            Some(owner),
            Some(step),
        ));
    }

    /// Start removing the selected record, or the open route's selected
    /// candidate. A removal is checked like an edit: what still depends on
    /// the record refuses it.
    fn open_remove(&mut self) {
        if self.on_route_page() {
            let Some((owner, route)) = self.document_route() else {
                return;
            };
            let ids = candidate_ids(route);
            let Some(candidate_id) = ids.get(self.candidate).cloned() else {
                return;
            };
            let what = What::RemoveCandidate {
                route_id: text(route, "id").to_owned(),
                candidate_id,
            };
            self.editing = Some(Editing::new(
                what,
                Form::new(Purpose::Fieldless, Value::Null, Context::default()),
                Some(owner),
                None,
            ));
            return;
        }
        let Some(target) = self.target() else {
            return;
        };
        let Some(record) = self.record(&target).cloned() else {
            return;
        };
        let what = What::Remove {
            kind: target.kind,
            record,
        };
        self.editing = Some(Editing::new(
            what,
            Form::new(Purpose::Fieldless, Value::Null, Context::default()),
            None,
            None,
        ));
    }

    /// Lift the selected rewrite, or the open route's selected candidate, to
    /// be put somewhere else with the arrows.
    fn open_move(&mut self) {
        let what = if self.on_route_page() {
            let Some((_, route)) = self.document_route() else {
                return;
            };
            let ids = candidate_ids(route);
            let edges: Vec<Value> = array(&route["chain"], "steps")
                .iter()
                .map(|step| step["edge"].clone())
                .collect();
            let from = self.candidate.min(ids.len().saturating_sub(1));
            if ids.len() < 2 {
                self.error = Some("a route with one candidate has nothing to reorder".to_owned());
                return;
            }
            What::MoveCandidate {
                route_id: text(route, "id").to_owned(),
                ids,
                edges,
                from,
                to: from,
            }
        } else if self.tab == Tab::Rewrites {
            let Some(active) = self.active() else {
                return;
            };
            let Some(rules::Entry::Rewrite(rule)) = rules::entries(active, self.tab)
                .get(self.selected_in(self.tab))
                .copied()
            else {
                self.error = Some("only a rewrite can be moved: select one".to_owned());
                return;
            };
            let scope_label = text(rule, "scope_label").to_owned();
            let ids: Vec<&str> = array(active, "rewrites")
                .iter()
                .filter(|other| text(other, "scope_label") == scope_label)
                .map(|other| text(other, "id"))
                .collect();
            let from = ids
                .iter()
                .position(|id| *id == text(rule, "id"))
                .unwrap_or(0);
            let records: Vec<Value> = ids
                .iter()
                .filter_map(|id| {
                    self.record(&Target {
                        kind: "rewrites",
                        id: Some((*id).to_owned()),
                        scope: None,
                    })
                    .cloned()
                })
                .collect();
            if records.len() < 2 {
                self.error = Some("it is the only rewrite in its scope".to_owned());
                return;
            }
            What::MoveRule {
                scope_label,
                rules: records,
                from,
                to: from,
            }
        } else {
            self.error = Some("only a rewrite, or a route's candidate, can be moved".to_owned());
            return;
        };
        self.editing = Some(Editing::new(
            what,
            Form::new(Purpose::Fieldless, Value::Null, Context::default()),
            None,
            None,
        ));
    }

    /// Ask which request to trace, reading the recent ones to choose from.
    fn open_trace(&mut self, under: Option<i64>) -> Option<Command> {
        self.modal = Modal::Trace(Box::new(Picking::new(under)));
        Some(Picking::command())
    }

    /// What the invented request's form starts from: the request the
    /// selected record catches, where it catches one.
    fn invented_start(&self) -> Value {
        let mut start = json!({ "protocol": "anthropic_messages" });
        let record = self
            .active()
            .and_then(|active| {
                rules::entries(active, self.tab)
                    .into_iter()
                    .nth(self.selected_in(self.tab))
            })
            .map(|entry| entry.record().clone());
        if let Some(record) = record {
            if let Some(protocol) = record.get("protocol").filter(|value| value.is_string()) {
                start["protocol"] = protocol.clone();
            }
            if let Some(profile) = record.get("profile").filter(|value| value.is_string()) {
                start["profile"] = profile.clone();
            }
            let model = record
                .get("example")
                .and_then(|example| example.get("in"))
                .or_else(|| record.get("slug"))
                .filter(|value| value.is_string());
            if let Some(model) = model {
                start["model"] = model.clone();
            }
        }
        start
    }

    /// With a trace on show, `enter` follows it to the record that caught the
    /// request, in its own table; once there it opens the route as it does
    /// without a trace. Returns whether it went anywhere.
    fn follow_trace(&mut self) -> bool {
        let Some(trace) = &self.trace else {
            return false;
        };
        let Some(active) = self.active() else {
            return false;
        };
        let Some((tab, at)) = trace.catch(active, self.active_revision_id) else {
            return false;
        };
        if self.tab == tab && self.mode == Mode::Table && self.selected_in(tab) == at {
            return false;
        }
        self.set_tab(tab);
        self.selected[tab.index()] = at;
        true
    }

    /// Start tracing, the answer to follow.
    fn start_trace(&mut self, subject: Subject, under: Option<i64>) -> Option<Command> {
        let trace = Trace::new(subject, under);
        let command = trace.command();
        self.trace = Some(trace);
        Some(command)
    }

    /// The revision the history tab has selected.
    fn selected_revision(&self) -> Option<i64> {
        self.revisions
            .get(self.selected_in(Tab::History))
            .and_then(|revision| revision.get("revision_id"))
            .and_then(Value::as_i64)
    }

    /// Make the selected revision live again, as a new revision, for new
    /// and cold lanes as a save is (spec: RREV#history-and-reactivation).
    fn reactivate_selected(&mut self) -> Option<Command> {
        let Some(revision_id) = self.selected_revision() else {
            self.error = Some("refresh revisions, then choose a revision to reactivate".to_owned());
            return None;
        };
        if Some(revision_id) == self.active_revision_id {
            self.notice = Some(format!("rev {revision_id} is already live"));
            return None;
        }
        self.reactivate(revision_id)
    }

    /// Move the running lanes onto the live revision. A move that would lose
    /// a warm cache asks once, naming the lanes, and goes on a second `m`;
    /// one that loses nothing goes at once.
    fn move_lanes(&mut self, armed: Option<Armed>) -> Option<Command> {
        let live = self.active_revision_id?;
        // The consent was given for the lanes of the revision that was
        // named, so that is the one it is sent against: a revision that went
        // live since is refused as stale rather than moved onto unseen.
        if let Some(armed) = armed {
            return self.widen(armed.revision, true);
        }
        let snapshot = self.snapshot.as_ref()?;
        let activation = snapshot.get("activation").unwrap_or(&Value::Null);
        if text(activation, "scope") == "immediate" {
            self.notice = Some(format!(
                "rev {live} already moves running lanes at their next request"
            ));
            return None;
        }
        if history::on_earlier(snapshot) == 0 {
            self.notice = Some(format!("no lane is on an earlier revision than rev {live}"));
            return None;
        }
        let (names, count) = history::at_risk(snapshot);
        if count == 0 {
            return self.widen(live, false);
        }
        self.armed = Some(Armed::new(names, count, live));
        None
    }

    fn widen(&mut self, live: i64, force: bool) -> Option<Command> {
        self.in_flight = true;
        Some(command(
            "widen",
            Method::POST,
            "widen",
            "routing-widen",
            json!({
                "expected_revision_id": live,
                "scope": "immediate",
                "force_cache_transition": force,
            }),
        ))
    }

    /// Promote a learned model. With an upgrade selected, the form starts
    /// from the model its evidence bar is holding back.
    fn open_promote(&mut self) {
        let context = self.form_context();
        let held = (self.tab == Tab::Models && self.models.focus == Panel::Upgrades)
            .then(|| {
                let row = models_tab::upgrades(self.active()?)
                    .into_iter()
                    .nth(self.models.upgrade)?;
                Some((row.binding_id.to_owned(), row.held_back_model()?.to_owned()))
            })
            .flatten();
        let (binding, model) = held.unwrap_or_default();
        self.modal = Modal::Form(Box::new(Form::new(
            Purpose::Promote,
            json!({ "binding_id": binding, "model_id": model }),
            context,
        )));
    }

    fn modal_key(&mut self, key: KeyEvent) -> Option<Command> {
        let modal = std::mem::take(&mut self.modal);
        let (modal, action) = match modal {
            Modal::None => (Modal::None, ModalAction::None),
            Modal::Form(mut form) => match form.key(key) {
                Outcome::Cancel => return None,
                Outcome::Keep => (Modal::Form(form), ModalAction::None),
                Outcome::Submit => {
                    let action = form_action(&form);
                    (Modal::Form(form), action)
                }
            },
            Modal::Trace(mut picking) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    picking.step(-1);
                    (Modal::Trace(picking), ModalAction::None)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    picking.step(1);
                    (Modal::Trace(picking), ModalAction::None)
                }
                KeyCode::Enter => {
                    let under = picking.under;
                    match picking.chosen() {
                        Some(id) => (
                            Modal::None,
                            ModalAction::Trace(Subject::Recorded(id), under),
                        ),
                        None => (Modal::None, ModalAction::Invent(under)),
                    }
                }
                KeyCode::Esc => return None,
                _ => (Modal::Trace(picking), ModalAction::None),
            },
        };
        self.modal = modal;
        match action {
            ModalAction::None => None,
            ModalAction::Edit(edits) => self.edit(edits),
            ModalAction::Promote { binding, model } => self.promote(&binding, &model),
            ModalAction::Trace(subject, under) => {
                // The form that asked for it is done with.
                self.modal = Modal::None;
                let under = under.or_else(|| self.tracing_under.take());
                self.start_trace(subject, under)
            }
            ModalAction::Invent(under) => {
                self.tracing_under = under;
                self.modal = Modal::Form(Box::new(Form::new(
                    Purpose::Trace,
                    self.invented_start(),
                    Context::default(),
                )));
                None
            }
        }
    }

    /// Save typed edits against the revision the page last saw live. The
    /// daemon refuses a save made against a revision that has since moved.
    fn edit(&mut self, edits: Vec<Value>) -> Option<Command> {
        let revision = self.active_revision_id?;
        self.in_flight = true;
        Some(command(
            "save",
            Method::POST,
            "save",
            "routing-save",
            json!({ "expected_revision_id": revision, "edit": { "operations": edits } }),
        ))
    }

    fn reactivate(&mut self, revision_id: i64) -> Option<Command> {
        let revision = self.active_revision_id?;
        self.in_flight = true;
        Some(command(
            "reactivate",
            Method::POST,
            "reactivate",
            "routing-reactivate",
            json!({ "expected_revision_id": revision, "revision_id": revision_id }),
        ))
    }

    fn promote(&mut self, binding: &str, model: &str) -> Option<Command> {
        if binding.is_empty() || model.is_empty() {
            self.error = Some("binding and model are both required".to_owned());
            return None;
        }
        self.in_flight = true;
        Some(command(
            "promote",
            Method::POST,
            "promote",
            "routing-promote",
            json!({ "binding_id": binding, "model_id": model }),
        ))
    }
}

/// The ids of a document route's candidates, in order.
fn candidate_ids(route: &Value) -> Vec<String> {
    let mut ids = vec![text(&route["chain"]["first"], "id").to_owned()];
    ids.extend(
        array(&route["chain"], "steps")
            .iter()
            .map(|step| text(&step["candidate"], "id").to_owned()),
    );
    ids
}

#[derive(Debug)]
enum ModalAction {
    None,
    Edit(Vec<Value>),
    Promote {
        binding: String,
        model: String,
    },
    /// Trace this request, under this revision where not the live one.
    Trace(Subject, Option<i64>),
    /// Ask for an invented request to trace.
    Invent(Option<i64>),
}

fn command(
    name: &'static str,
    method: Method,
    path: &'static str,
    control: &'static str,
    body: Value,
) -> Command {
    Command {
        name,
        method,
        path,
        control,
        body: Some(body),
        query: None,
    }
}

/// The edit a submitted small form sends, or what it asks for.
fn form_action(form: &Form) -> ModalAction {
    let value = form.value();
    match form.purpose() {
        Purpose::Description => ModalAction::Edit(vec![json!({
            "kind": "set_description",
            "description": value.get("description").cloned().unwrap_or(Value::Null),
        })]),
        Purpose::Promote => ModalAction::Promote {
            binding: text_of(value, "binding_id"),
            model: text_of(value, "model_id"),
        },
        Purpose::Trace => ModalAction::Trace(Subject::Invented(invented(value)), None),
        // Records and candidates are edited in place, not by a small form.
        _ => ModalAction::None,
    }
}

/// The request a filled-in trace form describes: only what was said, so the
/// resolver reads nothing the operator did not ask about.
fn invented(form: &Value) -> Value {
    let mut request = json!({ "kind": "invented", "protocol": text_of(form, "protocol") });
    for key in ["profile", "model"] {
        let said = text_of(form, key);
        if !said.trim().is_empty() {
            request[key] = json!(said.trim());
        }
    }
    if let Some(tokens) = form.get("prompt_bound").and_then(Value::as_u64) {
        request["prompt_bound"] = json!(tokens);
    }
    if form.get("asks_server_safeguards") == Some(&Value::Bool(true)) {
        request["asks_server_safeguards"] = json!(true);
    }
    request
}

fn text_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn control_error(command: &Command, response: &Response) -> String {
    let code = response
        .body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("request_failed");
    let diagnostics = array(&response.body, "diagnostics")
        .iter()
        .take(5)
        .map(|diagnostic| {
            let text = |key: &str| diagnostic.get(key).and_then(Value::as_str).unwrap_or("?");
            format!("{} at {}", text("code"), text("path"))
        })
        .collect::<Vec<_>>();
    let mut message = format!("{}: {code} (HTTP {})", command.name, response.status);
    // A save or a check keeps its form and says this there; what else was
    // made against the live revision says it here.
    if code == "stale_revision"
        && let Some(live) = response
            .body
            .get("live_revision_id")
            .and_then(Value::as_i64)
    {
        message.push_str(&format!(
            ": revision {live} is now live; reload, then save again"
        ));
    }
    if let Some(detail) = response.body.get("detail").and_then(Value::as_str) {
        message.push_str(&format!(": {detail}"));
    }
    if !diagnostics.is_empty() {
        message.push_str(&format!("; {}", diagnostics.join("; ")));
    }
    message
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use reqwest::Method;

    use super::{Modal, Mode, Panel, State, Tab};
    use crate::tui::control::{Command, Reply, Response};
    use crate::tui::routing::data::{self, array};
    use crate::tui::routing::editing::{What, add_edit, remove_edit, replace_edit};
    use crate::tui::routing::form::Purpose;
    use crate::tui::routing::testing::{self, World};

    const CTRL_S: KeyEvent = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);

    fn reply(name: &'static str, status: u16, body: serde_json::Value) -> Reply {
        let mut command = Command::snapshot("routing-snapshot");
        command.name = name;
        Reply::Command(command, Ok(Response { status, body }))
    }

    /// A page that has read the snapshot and the revisions of `world`.
    fn loaded(world: World) -> State {
        let mut state = State::default();
        state.receive(reply("snapshot", 200, testing::snapshot(world)));
        state.receive(reply("revisions", 200, testing::revisions(world)));
        state
    }

    fn on(tab: Tab) -> State {
        let mut state = loaded(World::AT_REST);
        state.set_tab(tab);
        state
    }

    fn press(state: &mut State, code: KeyCode) -> Option<Command> {
        state.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn typed(state: &mut State, text: &str) {
        for character in text.chars() {
            assert!(press(state, KeyCode::Char(character)).is_none());
        }
    }

    /// The open edit's form value.
    fn form_value(state: &State) -> serde_json::Value {
        state
            .editing()
            .expect("an edit is open")
            .form
            .value()
            .clone()
    }

    /// The typed operations of a command's edit.
    fn ops(command: &Command) -> serde_json::Value {
        command.body.as_ref().expect("body")["edit"]["operations"].clone()
    }

    /// Answer `command` as the daemon would, with `status` and `body`.
    fn answer(state: &mut State, command: Command, status: u16, body: serde_json::Value) {
        state.receive(Reply::Command(command, Ok(Response { status, body })));
    }

    /// What the daemon says of an edit it finds nothing wrong with.
    fn fine() -> serde_json::Value {
        serde_json::json!({"valid": true, "diagnostics": []})
    }

    /// Ask for the check the open edit is due, and have the daemon accept the
    /// edit. Returns the check that was sent.
    fn accepted(state: &mut State) -> Command {
        let check = state.next_check().expect("a check is due");
        assert_eq!(check.name, "validate");
        answer(state, check.clone(), 200, fine());
        check
    }

    /// Open the edit by `key`, have it accepted, and save it.
    fn saved_by(state: &mut State, key: char) -> Command {
        press(state, KeyCode::Char(key));
        accepted(state);
        state.key(CTRL_S).expect("a save")
    }

    // --- navigation -------------------------------------------------------

    #[test]
    fn the_page_opens_on_the_first_tab_holding_a_refused_reach_once() {
        let mut snapshot = testing::snapshot(World::AT_REST);
        snapshot["derived"]["active"]["first_refused_tab"] = serde_json::json!("defaults");
        let mut state = State::default();
        state.receive(reply("snapshot", 200, snapshot.clone()));
        assert_eq!(state.tab(), Tab::Defaults);
        // A refresh does not move an operator off the tab they chose.
        state.set_tab(Tab::Routes);
        state.receive(reply("snapshot", 200, snapshot));
        assert_eq!(state.tab(), Tab::Routes);
        // With nothing refused, the first tab.
        assert_eq!(loaded(World::AT_REST).tab(), Tab::Presentations);
    }

    #[test]
    fn each_tab_counts_the_records_it_holds() {
        let state = loaded(World::FULL);
        let counts: Vec<_> = Tab::ALL.iter().map(|tab| state.tab_count(*tab)).collect();
        // Presentations, rewrites, existing defaults, routes, then the
        // models tab's families, upgrades and compaction, and the revisions.
        assert_eq!(
            counts,
            [Some(15), Some(14), Some(4), Some(7), Some(11), Some(9)]
        );
        assert_eq!(State::default().tab_count(Tab::Rewrites), None);
    }

    #[test]
    fn left_and_right_move_between_tabs_and_wrap() {
        let mut state = loaded(World::AT_REST);
        state.move_view(-1);
        assert_eq!(state.tab(), Tab::History);
        state.move_view(1);
        assert_eq!(state.tab(), Tab::Presentations);
        state.set_view(3);
        assert_eq!(state.tab(), Tab::Routes);
        state.set_view(9);
        assert_eq!(state.tab(), Tab::Routes, "no tab is nine");
    }

    #[test]
    fn a_tab_keeps_its_selection_while_another_is_looked_at() {
        let mut state = on(Tab::Rewrites);
        state.move_selection(3);
        assert_eq!(state.selected_in(Tab::Rewrites), 3);
        state.set_tab(Tab::Routes);
        assert_eq!(state.selected_in(Tab::Routes), 0);
        state.set_tab(Tab::Rewrites);
        assert_eq!(state.selected_in(Tab::Rewrites), 3);
        // The selection stops at the last entry: fourteen rules and the
        // three defaults that end the groups (one of them the want of one).
        state.move_selection(99);
        assert_eq!(state.selected_in(Tab::Rewrites), 14 + 3 - 1);
    }

    #[test]
    fn enter_opens_the_route_a_rule_sends_to_and_esc_returns_with_the_selection_kept() {
        let mut state = on(Tab::Rewrites);
        state.move_selection(1);
        press(&mut state, KeyCode::Enter);
        assert_eq!((state.tab(), state.mode()), (Tab::Routes, Mode::Route));
        assert_eq!(state.selected_in(Tab::Routes), 0, "claude-sub, the first");
        // Left and right step between routes without returning, and wrap.
        state.move_view(1);
        assert_eq!(state.selected_in(Tab::Routes), 1);
        state.move_view(-2);
        assert_eq!(state.selected_in(Tab::Routes), 6);
        assert_eq!(state.mode(), Mode::Route);
        assert!(state.close());
        assert_eq!((state.tab(), state.mode()), (Tab::Routes, Mode::Table));
        assert_eq!(
            state.selected_in(Tab::Routes),
            6,
            "the route last looked at"
        );
        assert!(!state.close(), "nothing left to close");
    }

    #[test]
    fn a_default_with_no_route_opens_nothing() {
        let mut state = on(Tab::Defaults);
        state.move_selection(99);
        press(&mut state, KeyCode::Enter);
        assert_eq!((state.tab(), state.mode()), (Tab::Defaults, Mode::Table));
    }

    #[test]
    fn the_models_focus_moves_on_past_a_panels_edge() {
        let mut state = on(Tab::Models);
        assert_eq!(state.models().focus, Panel::Upgrades);
        state.move_selection(-1);
        assert_eq!(state.models().focus, Panel::Families);
        assert_eq!(state.models().family, 4, "the last family, not the first");
        state.move_selection(-1);
        assert_eq!(state.models().family, 3);
        state.move_selection(99);
        assert_eq!(
            (state.models().focus, state.models().upgrade),
            (Panel::Upgrades, 0),
            "past the last family: the first upgrade"
        );
        state.move_selection(4);
        assert_eq!(state.models().upgrade, 4);
        state.move_selection(1);
        assert_eq!(state.models().focus, Panel::Compaction);
        state.move_selection(1);
        assert_eq!(state.models().focus, Panel::Offers);
        state.show_offers(testing::OR_CHAT, 0);
        state.move_selection(3);
        assert_eq!(state.models().offers_at, 3, "the offers scroll");
        state.move_selection(-3);
        state.move_selection(-1);
        assert_eq!(state.models().focus, Panel::Compaction);
        state.move_selection(-1);
        assert_eq!(
            (state.models().focus, state.models().upgrade),
            (Panel::Upgrades, 4)
        );
    }

    #[test]
    fn left_and_right_in_the_offers_change_the_catalogue_not_the_tab() {
        let mut state = on(Tab::Models);
        state.move_selection(99);
        state.move_selection(1);
        state.move_selection(1);
        assert_eq!(state.models().focus, Panel::Offers);
        state.show_offers(testing::OR_CHAT, 0);
        let first = state.offers_shown()[0]["binding_id"].clone();
        state.move_selection(5);
        state.move_view(1);
        assert_eq!(state.tab(), Tab::Models);
        assert_ne!(state.offers_shown()[0]["binding_id"], first);
        assert_eq!(state.models().offers_at, 0, "a catalogue starts at its top");
    }

    // --- editing in place -----------------------------------------------------

    #[test]
    fn a_row_is_edited_where_it_is_with_every_key_going_to_its_form() {
        let mut state = loaded(World::AT_REST);
        assert!(!state.capturing());
        press(&mut state, KeyCode::Char('e'));
        assert!(state.capturing(), "the form owns the keys");
        assert!(!state.modal().active(), "it is not a popup");
        assert_eq!(form_value(&state)["id"], "pick-1");
        assert!(matches!(
            state.editing().expect("editing").what,
            What::Record {
                kind: "presentations",
                adding: false
            }
        ));
        assert_eq!(state.lead(), Some("editing"));
        // Escape abandons it, and sends nothing.
        assert!(press(&mut state, KeyCode::Esc).is_none());
        assert!(!state.capturing());
        assert_eq!(state.lead(), None);
    }

    /// One check at a time, always of the form as it stands: a change made
    /// while one is out is asked about when it comes back.
    #[test]
    fn a_change_is_checked_one_at_a_time_and_the_newest_form_wins() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('e'));
        let first = state.next_check().expect("a check on opening");
        assert_eq!(first.name, "validate");
        assert_eq!(first.path, "validate");
        let body = first.body.clone().expect("body");
        assert_eq!(body["expected_revision_id"], 9);
        assert_eq!(
            body["edit"]["operations"][0]["kind"],
            "replace_presentation"
        );
        assert!(state.next_check().is_none(), "one in flight");

        // Down to the slug: the id, then the scope's kind, profile and
        // protocol.
        for _ in 0..4 {
            press(&mut state, KeyCode::Down);
        }
        typed(&mut state, "x");
        assert!(state.next_check().is_none(), "still the first one's turn");
        // Its answer is for a form that has moved on: not accepted.
        answer(&mut state, first, 200, fine());
        assert!(!state.editing().expect("editing").saveable());
        let second = state.next_check().expect("the changed form is asked about");
        assert_eq!(second.name, "validate");
        assert_ne!(ops(&second), body["edit"]["operations"]);
        answer(&mut state, second, 200, fine());
        assert!(state.editing().expect("editing").saveable());
        // Nothing changed since: nothing to ask but what recent traffic did.
        assert_eq!(state.next_check().expect("replay").name, "preview");
    }

    #[test]
    fn nothing_can_be_saved_until_the_daemon_has_accepted_it() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('e'));
        assert!(state.key(CTRL_S).is_none(), "not yet checked");
        assert_eq!(state.editing().expect("editing").save_label(), "checking");
        let check = state.next_check().expect("check");
        assert!(state.key(CTRL_S).is_none(), "still being checked");
        answer(&mut state, check, 200, fine());
        assert_eq!(state.editing().expect("editing").save_label(), "save");
        let save = state.key(CTRL_S).expect("saveable");
        assert_eq!(save.name, "save");
        assert!(state.in_flight());
    }

    #[test]
    fn a_refusal_stops_the_save_and_says_which_field_to_fix() {
        let mut state = on(Tab::Rewrites);
        state.move_selection(1);
        press(&mut state, KeyCode::Char('e'));
        let index = state
            .document_index(&state.target().expect("target"))
            .expect("the rule's place");
        let check = state.next_check().expect("check");
        answer(
            &mut state,
            check,
            200,
            serde_json::json!({
                "valid": false,
                "diagnostics": [{
                    "code": "missing-route", "severity": "error",
                    "record_id": "anthropic-messages-2",
                    "path": format!("/rewrites/{index}/target/route_id"),
                    "sentence": "The route does not exist.",
                    "fix": "Choose a route in the policy.",
                }],
            }),
        );
        assert!(state.key(CTRL_S).is_none());
        let editing = state.editing().expect("editing");
        assert!(!editing.saveable());
        assert_eq!(editing.save_label(), "fix route to save");
        let route = editing
            .form
            .lines()
            .into_iter()
            .find(|line| line.text.contains("route:"))
            .expect("the route field");
        assert_eq!(route.notes.len(), 2, "the sentence and the fix: {route:?}");
        // The form is still open: it is fixed in place.
        assert!(state.capturing());
    }

    /// A finding about a record the form is not for names that record, and
    /// still stops the save.
    #[test]
    fn a_refusal_that_belongs_to_another_record_names_it() {
        let mut state = on(Tab::Routes);
        press(&mut state, KeyCode::Char('d'));
        let check = state.next_check().expect("check");
        assert_eq!(ops(&check)[0]["kind"], "remove_route");
        answer(
            &mut state,
            check,
            200,
            serde_json::json!({
                "valid": false,
                "diagnostics": [{
                    "code": "missing-route", "severity": "error",
                    "record_id": "anthropic-messages-1",
                    "path": "/rewrites/0/target/route_id",
                    "sentence": "The route it sends to is not in the policy.",
                    "fix": "Send it to another route first.",
                }],
            }),
        );
        let editing = state.editing().expect("editing");
        assert_eq!(
            editing.save_label(),
            "fix rewrite anthropic-messages-1 to save"
        );
        assert!(!editing.saveable());
        let checked = editing.checked.as_ref().expect("checked");
        assert_eq!(checked.elsewhere.len(), 1);
        assert!(state.key(CTRL_S).is_none());
    }

    #[test]
    fn a_warning_is_shown_beside_the_field_and_does_not_block_the_save() {
        let mut state = on(Tab::Routes);
        press(&mut state, KeyCode::Char('e'));
        let index = state
            .document_index(&state.target().expect("target"))
            .expect("place");
        let check = state.next_check().expect("check");
        answer(
            &mut state,
            check,
            200,
            serde_json::json!({
                "valid": true,
                "diagnostics": [
                    {"code": "binding-unconfigured", "severity": "warning",
                     "record_id": "claude-sub",
                     "path": format!("/routes/{index}/chain/first/binding_id"),
                     "sentence": "The provider is not configured.",
                     "fix": "Configure it, or leave it: it is skipped until it is."},
                    // Another record's warning is not this form's business.
                    {"code": "binding-unconfigured", "severity": "warning",
                     "record_id": "codex", "path": "/routes/3/chain/first/binding_id",
                     "sentence": "elsewhere", "fix": "elsewhere"},
                ],
            }),
        );
        let editing = state.editing().expect("editing");
        assert!(editing.saveable());
        let warned: Vec<_> = editing
            .form
            .lines()
            .into_iter()
            .filter(|line| !line.notes.is_empty())
            .collect();
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].text.contains("binding"));
        assert!(state.key(CTRL_S).is_some());
    }

    #[test]
    fn an_edit_the_daemon_cannot_apply_is_refused_with_its_words() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('n'));
        let check = state.next_check().expect("check");
        // The same shape `save` and `preview` refuse an unappliable edit in.
        answer(
            &mut state,
            check,
            422,
            serde_json::json!({
                "toker": "routing-validate", "ok": false,
                "error": "invalid_policy_edit",
                "detail": "presentation `new-presentation` already exists",
            }),
        );
        let editing = state.editing().expect("the form stays");
        assert_eq!(editing.save_label(), "the check failed");
        let said = editing.unchecked.as_deref().expect("the daemon's words");
        assert!(said.contains("already exists"), "{said}");
        assert!(state.key(CTRL_S).is_none());
    }

    #[test]
    fn a_blank_id_is_refused_by_the_form_before_the_daemon_is_asked() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('n'));
        for _ in 0.."new-presentation".len() {
            press(&mut state, KeyCode::Backspace);
        }
        let check = state.next_check().expect("check");
        answer(&mut state, check, 200, fine());
        let editing = state.editing().expect("editing");
        assert_eq!(editing.form.refused().0, ["id"]);
        assert_eq!(editing.save_label(), "fix id to save");
        assert!(state.key(CTRL_S).is_none(), "even though the daemon agreed");
        press(&mut state, KeyCode::Esc);
        assert!(!state.capturing());
    }

    #[test]
    fn a_record_is_saved_as_the_typed_replacement_against_the_live_revision() {
        let mut state = on(Tab::Routes);
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(form_value(&state)["id"], "claude-sub");
        // The id names the record the edit replaces, so it is fixed.
        typed(&mut state, "x");
        assert_eq!(form_value(&state)["id"], "claude-sub");
        press(&mut state, KeyCode::Down);
        typed(&mut state, "named");
        accepted(&mut state);
        let save = state.key(CTRL_S).expect("save");
        assert_eq!(save.method, Method::POST);
        assert_eq!(save.path, "save");
        assert_eq!(save.body.as_ref().expect("body")["expected_revision_id"], 9);
        let operations = ops(&save);
        assert_eq!(operations[0]["kind"], "replace_route");
        assert_eq!(operations[0]["id"], "claude-sub");
        assert_eq!(operations[0]["route"]["display_name"], "claude-subnamed");
        // What the schema does not name is carried through as it was.
        assert_eq!(operations[0]["route"]["chain"]["first"]["id"], "sub");
    }

    #[test]
    fn a_save_that_lands_closes_the_form_and_says_so() {
        let mut state = loaded(World::AT_REST);
        let save = saved_by(&mut state, 'e');
        assert!(state.in_flight());
        assert!(press(&mut state, KeyCode::Char('x')).is_none());
        answer(
            &mut state,
            save,
            200,
            serde_json::json!({"active_revision": {"revision_id": 10}}),
        );
        assert!(!state.in_flight());
        assert!(!state.capturing());
        assert_eq!(state.active_revision_id(), Some(10));
        assert_eq!(state.lead(), Some("saved"));
        press(&mut state, KeyCode::Down);
        assert_eq!(state.lead(), None, "said once");
    }

    /// A save made against a revision that is not live any more keeps the
    /// form as entered and says which revision is live; the page reads the
    /// new one and the form is checked against it.
    #[test]
    fn a_save_moved_past_keeps_the_form_and_names_the_live_revision() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('n'));
        accepted(&mut state);
        let save = state.key(CTRL_S).expect("add");
        assert_eq!(save.body.as_ref().expect("body")["expected_revision_id"], 9);
        answer(
            &mut state,
            save,
            409,
            serde_json::json!({"error": "stale_revision", "live_revision_id": 12}),
        );
        assert!(!state.in_flight());
        let editing = state.editing().expect("the form is kept");
        assert_eq!(editing.moved_to, Some(12));
        assert_eq!(editing.save_label(), "save onto rev 12");
        assert_eq!(form_value(&state)["id"], "new-presentation");
        assert!(state.error().is_none(), "the form says it, not the banner");
        assert!(state.take_refresh_wanted());
        assert!(!state.take_refresh_wanted(), "once");

        // The snapshot of the live revision arrives: the form is checked
        // against it, and saved onto it.
        let mut snapshot = testing::snapshot(World::AT_REST);
        snapshot["active_revision"]["revision_id"] = serde_json::json!(12);
        answer(
            &mut state,
            Command::snapshot("routing-snapshot"),
            200,
            snapshot,
        );
        let check = state.next_check().expect("asked again");
        assert_eq!(
            check.body.as_ref().expect("body")["expected_revision_id"],
            12
        );
        answer(&mut state, check, 200, fine());
        let save = state.key(CTRL_S).expect("save");
        assert_eq!(
            save.body.as_ref().expect("body")["expected_revision_id"],
            12
        );
    }

    #[test]
    fn a_save_the_daemon_refuses_leaves_the_form_open() {
        let mut state = loaded(World::AT_REST);
        let save = saved_by(&mut state, 'e');
        answer(
            &mut state,
            save,
            422,
            serde_json::json!({"error": "invalid_policy", "diagnostics": [
                {"code": "unknown_binding", "path": "/routes/0/chain/first"}
            ]}),
        );
        assert!(state.capturing());
        assert!(!state.in_flight());
        let error = state.error().expect("error").to_owned();
        assert!(
            error.contains("unknown_binding at /routes/0/chain/first"),
            "{error}"
        );
    }

    #[test]
    fn a_read_reply_does_not_release_a_pending_save() {
        let mut state = loaded(World::AT_REST);
        let save = saved_by(&mut state, 'e');
        assert!(state.in_flight());
        // The page's earlier reads land first, and a check's answer too.
        answer(
            &mut state,
            Command::snapshot("x"),
            200,
            testing::revisions(World::AT_REST),
        );
        state.receive(reply(
            "revisions",
            200,
            serde_json::json!({"revisions": []}),
        ));
        state.receive(reply("snapshot", 200, testing::snapshot(World::AT_REST)));
        state.receive(reply("validate", 200, fine()));
        assert!(state.in_flight());
        assert!(state.next_check().is_none(), "no checks while it saves");
        answer(
            &mut state,
            save,
            200,
            serde_json::json!({"active_revision": {"revision_id": 4}}),
        );
        assert!(!state.in_flight());
    }

    #[test]
    fn a_refused_queue_slot_releases_the_busy_guards_and_asks_the_check_again() {
        let mut state = loaded(World::AT_REST);
        assert!(state.begin_refresh());
        state.refresh_refused();
        assert!(state.begin_refresh(), "a refused refresh can be retried");
        press(&mut state, KeyCode::Char('e'));
        let check = state.next_check().expect("check");
        state.command_refused(check.name);
        assert!(
            state
                .error()
                .is_some_and(|error| !error.contains("validate")),
            "a check is not worth a banner: {:?}",
            state.error()
        );
        assert!(state.next_check().is_some(), "asked again");
        let save = {
            accepted_after_retry(&mut state);
            state.key(CTRL_S).expect("save")
        };
        state.command_refused(save.name);
        assert!(!state.in_flight());
        assert!(state.error().is_some_and(|error| error.contains("busy")));
    }

    /// Answer whichever check is outstanding as fine.
    fn accepted_after_retry(state: &mut State) {
        let ops = serde_json::Value::Array(state.editing().expect("editing").operations());
        let mut check = Command::snapshot("routing-snapshot");
        check.name = "validate";
        check.body = Some(serde_json::json!({"edit": {"operations": ops}}));
        answer(state, check, 200, fine());
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_snapshot_readable() {
        let mut state = loaded(World::AT_REST);
        assert!(state.begin_refresh());
        assert!(!state.begin_refresh(), "one refresh at a time");
        state.receive(Reply::Command(
            Command::snapshot("routing-snapshot"),
            Err("down".to_owned()),
        ));
        assert!(state.error().is_some_and(|error| error.contains("down")));
        assert_eq!(state.entry_count(Tab::Rewrites), 14 + 3);
        assert!(state.begin_refresh(), "refresh can resume once it settled");
    }

    #[test]
    fn what_recent_requests_would_do_is_asked_once_the_edit_is_accepted() {
        let mut state = on(Tab::Rewrites);
        press(&mut state, KeyCode::Char('n'));
        let check = accepted(&mut state);
        let replay = state.next_check().expect("the replay");
        assert_eq!(replay.name, "preview");
        let body = replay.body.clone().expect("body");
        assert_eq!(body["mode"], "replay_recent");
        assert_eq!(body["expected_revision_id"], 9);
        assert_eq!(body["edit"]["operations"], ops(&check));
        assert!(state.next_check().is_none(), "one at a time");
        answer(
            &mut state,
            replay,
            200,
            serde_json::json!({
                "replayed": 5, "unreplayable": 2, "truncated": false, "changed_total": 3,
                "window": {"since_ms": 0, "until_ms": 1_000_000},
                "changed": [{"request_id": 1, "ts_ms": 940_000,
                    "request": {"model": "claude-sonnet-5"},
                    "under_active": {"outcome": "selected", "route_id": "claude-sub",
                                     "matched": {"kind": "default", "scope": "protocol", "id": null}},
                    "under_edit": {"outcome": "selected", "route_id": "openrouter-messages",
                                   "matched": {"kind": "rewrite", "scope": "protocol", "id": "new-rewrite"}}}],
            }),
        );
        let replay = state
            .editing()
            .expect("editing")
            .replay
            .clone()
            .expect("kept");
        assert_eq!((replay.replayed, replay.unreplayable), (5, 2));
        assert_eq!(replay.changed_total, 3);
        // A change asks about the form again, not about traffic first.
        typed(&mut state, "9");
        assert_eq!(state.next_check().expect("check").name, "validate");
    }

    // --- adding, removing, moving ------------------------------------------

    #[test]
    fn adding_a_presentation_sends_the_typed_addition_against_the_live_revision() {
        let mut state = loaded(World::AT_REST);
        assert!(press(&mut state, KeyCode::Char('n')).is_none());
        assert!(matches!(
            state.editing().expect("editing").what,
            What::Record {
                kind: "presentations",
                adding: true
            }
        ));
        assert_eq!(form_value(&state)["id"], "new-presentation");
        accepted(&mut state);
        let save = state.key(CTRL_S).expect("add command");
        assert_eq!(save.body.as_ref().expect("body")["expected_revision_id"], 9);
        let operations = ops(&save);
        assert_eq!(operations[0]["kind"], "add_presentation");
        assert_eq!(operations[0]["presentation"]["id"], "new-presentation");
    }

    #[test]
    fn an_added_record_is_what_was_typed_into_the_form() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('n'));
        for _ in 0.."new-presentation".len() {
            press(&mut state, KeyCode::Backspace);
        }
        typed(&mut state, "mine");
        accepted(&mut state);
        let operation = ops(&state.key(CTRL_S).expect("add"))[0].clone();
        assert_eq!(operation["presentation"]["id"], "mine");
        assert_eq!(operation["presentation"]["slug"], "new-model");
    }

    #[test]
    fn every_kind_of_record_is_added_from_the_tab_that_shows_it() {
        for (tab, focus, kind, operation) in [
            (Tab::Presentations, 0, "presentations", "add_presentation"),
            (Tab::Rewrites, 0, "rewrites", "add_rewrite"),
            (Tab::Defaults, 0, "defaults", "add_default"),
            (Tab::Routes, 0, "routes", "add_route"),
            (Tab::Models, 0, "upgrades", "add_upgrade"),
        ] {
            let mut state = on(tab);
            let _ = focus;
            press(&mut state, KeyCode::Char('n'));
            assert!(
                matches!(&state.editing().expect("editing").what, What::Record { kind: k, adding: true } if *k == kind),
                "{tab:?}"
            );
            accepted(&mut state);
            assert_eq!(
                ops(&state.key(CTRL_S).expect("save"))[0]["kind"],
                operation,
                "{tab:?}"
            );
        }
        // The kind follows the panel the models focus is in.
        let mut state = on(Tab::Models);
        state.move_selection(-1);
        press(&mut state, KeyCode::Char('n'));
        assert!(matches!(
            state.editing().expect("editing").what,
            What::Record {
                kind: "classifiers",
                ..
            }
        ));
    }

    #[test]
    fn a_new_rewrite_starts_in_the_scope_of_the_row_it_is_added_under() {
        let mut state = on(Tab::Rewrites);
        state.move_selection(8);
        let scope = rules_scope(&state);
        press(&mut state, KeyCode::Char('n'));
        assert_eq!(form_value(&state)["scope"], scope);
    }

    fn rules_scope(state: &State) -> serde_json::Value {
        let active = state.active().expect("active");
        crate::tui::routing::rules::entries(active, state.tab())[state.selected_in(state.tab())]
            .record()["scope"]
            .clone()
    }

    #[test]
    fn a_new_default_starts_from_the_first_route() {
        let mut state = on(Tab::Defaults);
        press(&mut state, KeyCode::Char('n'));
        assert_eq!(form_value(&state)["target"]["route_id"], "claude-sub");
    }

    /// A duplicate is the add form filled from the selected rewrite, with an
    /// id no rewrite has and the next priority its scope leaves free.
    #[test]
    fn y_duplicates_a_rewrite_into_the_add_form_after_it() {
        let mut state = on(Tab::Rewrites);
        state.move_selection(1);
        let original = state
            .record(&state.target().expect("a rewrite"))
            .cloned()
            .expect("its record");
        assert!(press(&mut state, KeyCode::Char('y')).is_none());
        assert!(matches!(
            state.editing().expect("editing").what,
            What::Record {
                kind: "rewrites",
                adding: true
            }
        ));
        let copy = form_value(&state);
        for name in ["scope", "model_matcher", "target"] {
            assert_eq!(copy[name], original[name], "{name}");
        }
        assert_ne!(copy["id"], original["id"]);
        let rewrites = array(
            data::policy(state.snapshot().expect("snapshot")).expect("policy"),
            "rewrites",
        );
        assert!(
            rewrites.iter().all(|rule| rule["id"] != copy["id"]),
            "{copy}"
        );
        // The world's priorities step by ten, so the next one is free.
        let priority = original["priority"].as_i64().expect("a priority");
        assert_eq!(copy["priority"], priority + 1);
        accepted(&mut state);
        assert_eq!(
            ops(&state.key(CTRL_S).expect("save"))[0]["kind"],
            "add_rewrite"
        );
    }

    #[test]
    fn a_duplicate_takes_the_next_priority_its_scope_leaves_free() {
        let mut snapshot = testing::snapshot(World::AT_REST);
        let policy = &mut snapshot["active_revision"]["policy"];
        let scope = policy["rewrites"][0]["scope"].clone();
        let first = policy["rewrites"][0]["priority"]
            .as_i64()
            .expect("priority");
        // Its scope has the next two priorities taken.
        for (at, priority) in [(1, first + 1), (2, first + 2)] {
            policy["rewrites"][at]["scope"] = scope.clone();
            policy["rewrites"][at]["priority"] = priority.into();
        }
        let mut state = State::default();
        state.receive(reply("snapshot", 200, snapshot));
        state.set_tab(Tab::Rewrites);
        let at = (0..state.entry_count(Tab::Rewrites))
            .find(|at| {
                state.selected[Tab::Rewrites.index()] = *at;
                state
                    .target()
                    .and_then(|target| state.record(&target).cloned())
                    .is_some_and(|record| record["priority"] == first && record["scope"] == scope)
            })
            .expect("the first rule's row");
        state.selected[Tab::Rewrites.index()] = at;
        press(&mut state, KeyCode::Char('y'));
        assert_eq!(form_value(&state)["priority"], first + 3);
    }

    #[test]
    fn y_duplicates_a_route_from_its_table_or_its_own_page() {
        let mut state = on(Tab::Routes);
        press(&mut state, KeyCode::Char('y'));
        assert_eq!(form_value(&state)["id"], "claude-sub-copy");
        assert_eq!(
            form_value(&state)["chain"],
            state
                .record(&state.target().expect("route"))
                .expect("record")["chain"]
        );
        accepted(&mut state);
        assert_eq!(
            ops(&state.key(CTRL_S).expect("save"))[0]["kind"],
            "add_route"
        );

        let mut state = on(Tab::Routes);
        press(&mut state, KeyCode::Enter);
        assert_eq!(state.mode(), Mode::Route);
        press(&mut state, KeyCode::Char('y'));
        assert_eq!(state.mode(), Mode::Table, "added in the routes table");
        assert!(matches!(
            state.editing().expect("editing").what,
            What::Record {
                kind: "routes",
                adding: true
            }
        ));
    }

    #[test]
    fn there_is_nothing_to_duplicate_in_history_or_the_compaction_line() {
        let mut state = on(Tab::History);
        press(&mut state, KeyCode::Char('y'));
        assert!(!state.capturing());
        assert!(state.error().is_some());

        let mut state = on(Tab::Models);
        state.move_selection(99);
        assert_eq!(state.models().focus, Panel::Compaction);
        press(&mut state, KeyCode::Char('y'));
        assert!(!state.capturing());
        assert!(state.error().is_some_and(|error| error.contains("edit it")));
    }

    /// Adding a rule needs somewhere to send it, so with no routes the page
    /// points at routes first.
    #[test]
    fn adding_where_there_is_no_route_points_at_routes_first() {
        let mut snapshot = testing::snapshot(World::AT_REST);
        snapshot["active_revision"]["policy"]["routes"] = serde_json::json!([]);
        let mut state = State::default();
        state.receive(reply("snapshot", 200, snapshot));
        for tab in [Tab::Rewrites, Tab::Defaults] {
            state.set_tab(tab);
            press(&mut state, KeyCode::Char('n'));
            assert!(!state.capturing(), "{tab:?}");
            let error = state.error().expect("said");
            assert!(error.contains("needs a route"), "{error}");
            assert!(error.contains("press 4"), "{error}");
        }
        // A presentation sends nowhere, and a route is the thing wanted.
        state.set_tab(Tab::Routes);
        press(&mut state, KeyCode::Char('n'));
        assert!(state.capturing());
    }

    #[test]
    fn adding_is_not_offered_in_history_or_while_busy() {
        let mut state = on(Tab::History);
        assert!(press(&mut state, KeyCode::Char('n')).is_none());
        assert!(!state.capturing());
        assert!(state.error().is_some());
        state.set_tab(Tab::Presentations);
        let _save = saved_by(&mut state, 'e');
        assert!(state.in_flight());
        press(&mut state, KeyCode::Char('n'));
        assert!(
            state
                .editing()
                .is_some_and(|editing| matches!(editing.what, What::Record { adding: false, .. }))
        );
    }

    #[test]
    fn a_removal_is_checked_like_an_edit_and_saved_by_the_records_id() {
        let mut state = on(Tab::Rewrites);
        state.move_selection(1);
        press(&mut state, KeyCode::Char('d'));
        assert!(matches!(
            state.editing().expect("editing").what,
            What::Remove {
                kind: "rewrites",
                ..
            }
        ));
        let check = accepted(&mut state);
        let operation = ops(&check)[0].clone();
        assert_eq!(operation["kind"], "remove_rewrite");
        assert_eq!(operation["id"], "anthropic-messages-2");
        let save = state.key(CTRL_S).expect("save");
        assert_eq!(ops(&save), ops(&check));
    }

    #[test]
    fn a_default_is_replaced_and_removed_by_its_scope() {
        let mut state = loaded(World::FULL);
        state.set_tab(Tab::Defaults);
        // Unprefixed messages, messages, chat, unprefixed chat, responses.
        state.move_selection(3);
        let save = saved_by(&mut state, 'e');
        let operation = ops(&save)[0].clone();
        assert_eq!(operation["kind"], "replace_default");
        assert_eq!(operation["scope"]["profile"], "unprefixed");
        assert_eq!(
            operation["default"]["target"]["route_id"],
            "openrouter-chat"
        );
        answer(&mut state, save, 200, serde_json::json!({}));

        press(&mut state, KeyCode::Char('d'));
        let operation = ops(&accepted(&mut state))[0].clone();
        assert_eq!(operation["kind"], "remove_default");
        assert_eq!(operation["scope"]["profile"], "unprefixed");
    }

    #[test]
    fn a_protocol_with_no_default_has_none_to_edit_or_remove() {
        let mut state = loaded(World::AT_REST);
        state.set_tab(Tab::Defaults);
        state.move_selection(2);
        press(&mut state, KeyCode::Char('e'));
        press(&mut state, KeyCode::Char('d'));
        assert!(!state.capturing());
    }

    #[test]
    fn compaction_is_replaced_and_removed_without_an_id() {
        let mut state = on(Tab::Models);
        state.move_selection(5);
        assert_eq!(state.models().focus, Panel::Compaction);
        press(&mut state, KeyCode::Char('d'));
        let operation = ops(&accepted(&mut state))[0].clone();
        assert_eq!(operation["kind"], "remove_compaction");
        press(&mut state, KeyCode::Esc);
        let save = saved_by(&mut state, 'e');
        let operation = ops(&save)[0].clone();
        assert_eq!(operation["kind"], "replace_compaction");
        assert_eq!(operation["compaction"]["target"]["kind"], "substitution");
    }

    /// A rewrite is lifted with `m`, carried with the arrows, and saved as
    /// the priorities that put it there.
    #[test]
    fn a_rewrite_is_moved_within_its_scope_as_new_priorities() {
        let mut state = on(Tab::Rewrites);
        state.move_selection(1);
        press(&mut state, KeyCode::Char('m'));
        assert!(
            state.next_check().is_none(),
            "a move that has not moved is nothing to check"
        );
        assert_eq!(
            state.editing().expect("editing").save_label(),
            "move it to save"
        );
        press(&mut state, KeyCode::Down);
        assert_eq!(state.selected_in(Tab::Rewrites), 2, "the selection follows");
        let check = accepted(&mut state);
        // Priorities 10, 20, 30...: the two that swapped swap theirs.
        let operations = ops(&check);
        assert_eq!(operations.as_array().expect("array").len(), 2);
        assert_eq!(operations[0]["kind"], "replace_rewrite");
        assert_eq!(operations[0]["id"], "anthropic-messages-3");
        assert_eq!(operations[0]["rewrite"]["priority"], 20);
        assert_eq!(operations[1]["id"], "anthropic-messages-2");
        assert_eq!(operations[1]["rewrite"]["priority"], 30);
        // It stops at the edge of its scope.
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Down);
        assert_eq!(state.selected_in(Tab::Rewrites), 4, "the last of five");
        let save = state.key(CTRL_S);
        assert!(save.is_none(), "this form has not been checked yet");
        // Abandoning puts the selection back.
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.selected_in(Tab::Rewrites), 1);
        assert!(!state.capturing());
    }

    #[test]
    fn a_move_of_rules_that_share_a_priority_tells_them_apart() {
        let rule = |id: &str, priority: i64| serde_json::json!({"id": id, "priority": priority});
        let rules = [rule("a", 5), rule("b", 5), rule("c", 5)];
        let operations = super::super::editing::Editing::new(
            What::MoveRule {
                scope_label: "x".to_owned(),
                rules: rules.to_vec(),
                from: 0,
                to: 2,
            },
            crate::tui::routing::form::Form::new(
                Purpose::Fieldless,
                serde_json::Value::Null,
                Default::default(),
            ),
            None,
            None,
        )
        .operations();
        let priorities: Vec<_> = operations
            .iter()
            .map(|operation| {
                (
                    operation["id"].as_str().unwrap().to_owned(),
                    operation["rewrite"]["priority"].as_i64().unwrap(),
                )
            })
            .collect();
        // b, c, a in that order, with priorities that are strictly rising.
        assert_eq!(priorities, [("c".to_owned(), 6), ("a".to_owned(), 7)]);
    }

    #[test]
    fn only_a_rewrite_can_be_moved_in_a_table() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('m'));
        assert!(!state.capturing());
        assert!(state.error().is_some());
    }

    // --- a route's candidates ------------------------------------------------

    /// The first route opened whole.
    fn route_open() -> State {
        let mut state = on(Tab::Routes);
        press(&mut state, KeyCode::Enter);
        assert_eq!((state.tab(), state.mode()), (Tab::Routes, Mode::Route));
        state
    }

    #[test]
    fn the_arrows_choose_a_candidate_on_the_route_page() {
        let mut state = route_open();
        assert_eq!(state.candidate(), 0);
        state.move_selection(1);
        assert_eq!(state.candidate(), 1);
        state.move_selection(9);
        assert_eq!(state.candidate(), 1, "the last of two");
        // Another route starts at its first candidate.
        state.move_view(1);
        assert_eq!(state.candidate(), 0);
    }

    #[test]
    fn a_fallback_is_added_after_the_last_candidate_with_its_edge() {
        let mut state = route_open();
        press(&mut state, KeyCode::Char('n'));
        let editing = state.editing().expect("editing");
        let What::AddCandidate { route_id, after } = &editing.what else {
            panic!("a fallback is being added: {:?}", editing.what);
        };
        assert_eq!(route_id, "claude-sub");
        let candidate = form_value(&state)["candidate"]["id"].clone();
        assert_eq!(candidate, "fallback-1");
        let last = state.document_route().expect("route").1["chain"]["steps"][0]["candidate"]["id"]
            .clone();
        assert_eq!(&serde_json::Value::from(after.clone()), &last);
        let check = accepted(&mut state);
        let operation = ops(&check)[0].clone();
        assert_eq!(operation["kind"], "add_candidate");
        assert_eq!(operation["route_id"], "claude-sub");
        assert_eq!(operation["after"], last);
        assert_eq!(operation["candidate"]["id"], "fallback-1");
        assert!(
            operation["edge"]["billed_boundary"].is_string(),
            "{operation}"
        );
        assert!(state.key(CTRL_S).is_some());
    }

    #[test]
    fn a_candidates_own_findings_land_beside_its_fields() {
        let mut state = route_open();
        press(&mut state, KeyCode::Char('n'));
        let (index, step) = (
            state.document_route().expect("route").0,
            state.document_route().expect("route").1["chain"]["steps"]
                .as_array()
                .expect("steps")
                .len(),
        );
        let check = state.next_check().expect("check");
        answer(
            &mut state,
            check,
            200,
            serde_json::json!({"valid": false, "diagnostics": [{
                "code": "model-not-offered", "severity": "error", "record_id": "claude-sub",
                "path": format!("/routes/{index}/chain/steps/{step}/candidate/model/model_id"),
                "sentence": "The provider does not offer the model.",
                "fix": "Choose a model it offers.",
            }]}),
        );
        let editing = state.editing().expect("editing");
        assert_eq!(editing.save_label(), "fix model to save");
        assert!(
            editing
                .checked
                .as_ref()
                .expect("checked")
                .elsewhere
                .is_empty()
        );
        // The same finding on another candidate is not this form's.
        let other = serde_json::json!({"valid": false, "diagnostics": [{
            "code": "model-not-offered", "severity": "error", "record_id": "claude-sub",
            "path": format!("/routes/{index}/chain/first/model/model_id"),
            "sentence": "x", "fix": "y",
        }]});
        let check = {
            // The first field is a choice: another value is a change.
            press(&mut state, KeyCode::Right);
            state.next_check().expect("check")
        };
        answer(&mut state, check, 200, other);
        let checked = state
            .editing()
            .expect("editing")
            .checked
            .clone()
            .expect("checked");
        assert_eq!(checked.elsewhere.len(), 1);
    }

    #[test]
    fn a_candidate_is_removed_by_its_id() {
        let mut state = route_open();
        state.move_selection(1);
        press(&mut state, KeyCode::Char('d'));
        let operation = ops(&accepted(&mut state))[0].clone();
        assert_eq!(operation["kind"], "remove_candidate");
        assert_eq!(operation["route_id"], "claude-sub");
        let second =
            state.document_route().expect("route").1["chain"]["steps"][0]["candidate"]["id"]
                .clone();
        assert_eq!(operation["candidate_id"], second);
    }

    #[test]
    fn a_candidate_is_reordered_with_the_edge_made_explicit() {
        let mut state = route_open();
        press(&mut state, KeyCode::Char('m'));
        press(&mut state, KeyCode::Down);
        assert_eq!(
            state.candidate(),
            1,
            "the lifted candidate is the selection"
        );
        let operation = ops(&accepted(&mut state))[0].clone();
        assert_eq!(operation["kind"], "move_candidate");
        assert_eq!(operation["candidate_id"], "sub");
        assert_eq!(operation["destination"]["kind"], "after");
        assert!(operation["destination"]["edge"]["billed_boundary"].is_string());
        // Moving it back is nothing to do.
        press(&mut state, KeyCode::Up);
        assert!(ops_of(&state).is_empty());
        // And another way: the second to the front.
        press(&mut state, KeyCode::Esc);
        state.move_selection(1);
        press(&mut state, KeyCode::Char('m'));
        press(&mut state, KeyCode::Up);
        let operation = ops_of(&state)[0].clone();
        assert_eq!(operation["destination"]["kind"], "first");
    }

    fn ops_of(state: &State) -> Vec<serde_json::Value> {
        state.editing().expect("editing").operations()
    }

    #[test]
    fn a_candidate_is_edited_as_the_route_with_it_replaced() {
        let mut state = route_open();
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(form_value(&state)["candidate"]["id"], "sub");
        assert!(
            form_value(&state).get("edge").is_none(),
            "the first has no edge"
        );
        typed(&mut state, "x");
        let operations = ops(&accepted(&mut state));
        assert_eq!(operations[0]["kind"], "replace_route");
        assert_eq!(operations[0]["route"]["chain"]["first"]["id"], "subx");
        assert_eq!(
            operations.as_array().expect("array").len(),
            1,
            "no safeguards flag to carry"
        );

        // A later candidate comes with the edge into it.
        press(&mut state, KeyCode::Esc);
        state.move_selection(1);
        press(&mut state, KeyCode::Char('e'));
        assert!(form_value(&state)["edge"]["billed_boundary"].is_string());
        assert!(matches!(
            state.editing().expect("editing").what,
            What::EditCandidate { index: 1, .. }
        ));
    }

    // --- the route's safeguards flag and the daemon's vocabulary -------------

    #[test]
    fn a_route_save_sets_or_clears_its_honour_flag_by_its_own_operation() {
        let mut state = on(Tab::Routes);
        press(&mut state, KeyCode::Char('e'));
        // Down to the last field and switch it on.
        for _ in 0..200 {
            press(&mut state, KeyCode::Down);
        }
        press(&mut state, KeyCode::Char(' '));
        assert_eq!(form_value(&state)["honours_server_safeguards"], true);
        accepted(&mut state);
        let save = state.key(CTRL_S).expect("save");
        let operations = ops(&save);
        assert_eq!(operations[0]["kind"], "replace_route");
        // The flag does not travel inside the route: its own operation
        // carries it, and raises the document's format with it.
        assert!(
            operations[0]["route"]
                .get("honours_server_safeguards")
                .is_none()
        );
        assert_eq!(operations[1]["kind"], "set_route_server_safeguards");
        assert_eq!(operations[1]["route_id"], "claude-sub");
        assert_eq!(operations[1]["honours"], true);
        answer(&mut state, save, 200, serde_json::json!({}));

        // Left off, the same save clears it.
        press(&mut state, KeyCode::Char('e'));
        let operations = ops(&accepted(&mut state));
        assert_eq!(operations[1]["honours"], false);
    }

    #[test]
    fn only_a_route_save_carries_the_honour_flag_operation() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('e'));
        let operations = ops(&accepted(&mut state));
        assert_eq!(operations.as_array().expect("operations").len(), 1);
    }

    /// The terminal builds its edits by hand; the daemon's typed vocabulary is
    /// the authority on whether they are the edits it understands.
    #[test]
    fn every_operation_the_page_builds_is_one_the_daemon_understands() {
        use crate::policy::PolicyEdit;
        let record = serde_json::json!({"id": "some-id"});
        let default = serde_json::json!({
            "scope": {"kind": "protocol", "protocol": "openai_chat"},
            "target": {"route_id": "route-a"},
        });
        let compaction = serde_json::json!({
            "target": {"route_id": "route-a"},
            "lane_transition": "move_when_cold",
            "cache_transition": "allow_when_cold",
        });
        let edge = crate::tui::routing::form::default_edge();
        let mut operations: Vec<_> = [
            "presentations",
            "routes",
            "rewrites",
            "classifiers",
            "upgrades",
        ]
        .into_iter()
        .map(|key| remove_edit(key, &record))
        .collect();
        operations.extend([
            serde_json::json!({"kind": "set_description", "description": null}),
            replace_edit("defaults", default.clone()),
            remove_edit("defaults", &default),
            replace_edit("compaction", compaction.clone()),
            remove_edit("compaction", &compaction),
            serde_json::json!({"kind": "remove_candidate", "route_id": "r", "candidate_id": "c"}),
            serde_json::json!({"kind": "add_candidate", "route_id": "r", "after": "c",
                "edge": edge, "candidate": {"id": "d", "binding_id": "x.y",
                "model": {"kind": "requested_model"}}}),
            serde_json::json!({"kind": "move_candidate", "route_id": "r", "candidate_id": "c",
                "destination": {"kind": "first", "edge": edge}}),
            serde_json::json!({"kind": "move_candidate", "route_id": "r", "candidate_id": "c",
                "destination": {"kind": "after", "candidate_id": "d", "edge": edge}}),
        ]);
        let count = operations.len();
        let edit: PolicyEdit =
            serde_json::from_value(serde_json::json!({ "operations": operations }))
                .expect("every emitted operation is a typed PolicyEdit operation");
        assert_eq!(edit.operations.len(), count);
        // And the addition of each kind, which the templates exercise.
        let _ = add_edit("routes", serde_json::json!({}));
    }

    // --- the small questions that remain ----------------------------------

    #[test]
    fn escape_cancels_each_small_question_without_a_command() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('p'));
        assert!(state.modal().active());
        assert!(press(&mut state, KeyCode::Esc).is_none());
        assert!(!state.modal().active());
        state.set_tab(Tab::History);
        // The top of history is the live revision, whose description is
        // edited.
        press(&mut state, KeyCode::Char('e'));
        assert!(matches!(
            state.modal(),
            Modal::Form(form) if form.purpose() == Purpose::Description
        ));
        press(&mut state, KeyCode::Esc);
        assert!(!state.modal().active());
    }

    #[test]
    fn keys_are_ignored_while_a_save_is_in_flight() {
        let mut state = loaded(World::AT_REST);
        let _save = saved_by(&mut state, 'e');
        assert!(state.in_flight());
        assert!(press(&mut state, KeyCode::Char('y')).is_none());
        assert!(state.key(CTRL_S).is_none());
        state.receive(reply(
            "save",
            200,
            serde_json::json!({"active_revision": {"revision_id": 4}}),
        ));
        assert!(!state.in_flight());
        assert_eq!(state.active_revision_id(), Some(4));
    }

    #[test]
    fn promotion_needs_a_binding_and_a_model_and_completes_from_offers() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('p'));
        assert!(press(&mut state, KeyCode::Enter).is_none());
        assert!(
            state
                .error()
                .is_some_and(|error| error.contains("both required"))
        );
        // A binding is typed, or completed from the ones toker implements;
        // a model completes from what that binding offers.
        typed(&mut state, testing::OR_CHAT);
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Tab);
        let Modal::Form(form) = state.modal() else {
            panic!("a form is open");
        };
        assert_eq!(form.value()["model_id"], "anthropic/model-000");
        // A second Tab moves to the next offer.
        press(&mut state, KeyCode::Tab);
        let command = press(&mut state, KeyCode::Enter).expect("promotion");
        let body = command.body.clone().expect("body");
        assert_eq!(body["binding_id"], testing::OR_CHAT);
        assert_eq!(body["model_id"], "anthropic/model-001");
        // It is said once it lands, not shown as a document.
        answer(&mut state, command, 200, serde_json::json!({"ok": true}));
        assert!(!state.modal().active());
        assert!(
            state
                .notice()
                .is_some_and(|notice| notice.contains("anthropic/model-001")),
            "{:?}",
            state.notice()
        );
    }

    /// An upgrade whose evidence bar is holding a model back starts the
    /// promotion form from that model and its binding.
    #[test]
    fn promoting_from_an_upgrade_starts_from_the_model_it_holds_back() {
        let mut snapshot = testing::snapshot(World::AT_REST);
        snapshot["derived"]["active"]["upgrades"][0]["used_by"][1]["held_back"] = serde_json::json!(
            [{"family": "sonnet", "model": "claude-sonnet-5", "distinct_days": 1, "promoted": false}]
        );
        let mut state = State::default();
        state.receive(reply("snapshot", 200, snapshot));
        state.set_tab(Tab::Models);
        state.move_selection(1);
        press(&mut state, KeyCode::Char('p'));
        let Modal::Form(form) = state.modal() else {
            panic!("a form is open");
        };
        assert_eq!(form.value()["binding_id"], testing::API);
        assert_eq!(form.value()["model_id"], "claude-sonnet-5");
    }

    /// The keys of the pages this one replaced open nothing: there is no
    /// preview popup to ask for and no import to start.
    #[test]
    fn the_keys_of_the_removed_popups_open_nothing() {
        for key in ['v', 'i'] {
            let mut state = loaded(World::AT_REST);
            assert!(press(&mut state, KeyCode::Char(key)).is_none());
            assert!(!state.capturing(), "{key} opens nothing");
        }
    }
}
