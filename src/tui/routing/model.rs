//! Content-free routing snapshot, editing, and navigation state.
//!
//! The page is six tabs, in the order a request is resolved
//! (spec: UTUI#the-routing-page). Each lookup tab is a table of records, the
//! selected one's detail beneath it; the routes tab can open one route whole;
//! the models tab is a stack of panels with one focus among them.

use crossterm::event::{KeyCode, KeyEvent};
use reqwest::Method;
use serde_json::{Value, json};

use super::data::{self, array, text};
use super::form::{Context, Form, Outcome, Purpose};
use super::{models_tab, routes, rules};
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

/// A modal editing or safety-confirmation state.
#[derive(Debug, Clone, Default)]
pub(super) enum Modal {
    #[default]
    None,
    /// A typed form: a record's fields, the policy description, a promotion
    /// or a preview's request facts.
    Form(Box<Form>),
    /// Which kind of record to add, in a view that lists several.
    NewKind {
        kinds: &'static [&'static str],
        preselected: usize,
    },
    Remove {
        key: String,
        value: Value,
    },
    PreviewResult(Value),
    Reactivate {
        revision_id: i64,
    },
}

impl Modal {
    pub(super) fn active(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub(super) fn title(&self) -> &'static str {
        match self {
            Self::None => "",
            Self::Form(form) => form.title(),
            Self::NewKind { .. } => "ADD POLICY RECORD",
            Self::Remove { .. } => "REMOVE POLICY RECORD",
            Self::PreviewResult(_) => "ROUTING PREVIEW",
            Self::Reactivate { .. } => "REACTIVATE REVISION",
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
    in_flight: bool,
    modal: Modal,
    /// Whether the preview shown asked for server safeguards. The result of
    /// a route with no candidate left does not say, so the page remembers.
    preview_asked: bool,
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
            preview_asked: false,
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

    /// What a reactivation would do, from the snapshot the daemon last sent:
    /// the lane impact and where each publication stands. Shown before the
    /// user confirms.
    pub(super) fn activation_summary(&self) -> Vec<String> {
        let Some(snapshot) = &self.snapshot else {
            return vec!["no daemon snapshot loaded".to_owned()];
        };
        let mut lines = Vec::new();
        match snapshot.get("lane_impacts") {
            Some(impacts) => {
                let count = |key: &str| impacts.get(key).and_then(Value::as_u64).unwrap_or(0);
                lines.push(format!(
                    "lanes: {} total, {} unpinned, {} warm, {} cold, {} unknown",
                    count("total"),
                    count("unpinned"),
                    count("warm"),
                    count("cold"),
                    count("unknown"),
                ));
            }
            None => lines.push("lanes: impact not reported".to_owned()),
        }
        for publication in array(snapshot, "publications") {
            let text = |key: &str| publication.get(key).and_then(Value::as_str).unwrap_or("-");
            lines.push(format!(
                "publication {}: {}",
                text("adapter"),
                text("state")
            ));
        }
        lines
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

    pub(super) fn preview_asked(&self) -> bool {
        self.preview_asked
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
        self.tab = tab;
        self.mode = Mode::Table;
    }

    /// Up or down within the tab; in the models tab, on into the next panel
    /// past a panel's edge.
    pub(super) fn move_selection(&mut self, delta: isize) {
        match (self.tab, self.mode) {
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
            self.tab = Tab::Routes;
            self.mode = Mode::Route;
        }
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

    /// Focus a field of the open form, as a click does.
    pub(super) fn focus_form_row(&mut self, row: usize) {
        if let Modal::Form(form) = &mut self.modal {
            form.focus_row(row);
        }
    }

    /// Move the open form's focus, as the wheel does.
    pub(super) fn step_form(&mut self, delta: isize) {
        if let Modal::Form(form) = &mut self.modal {
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
                self.error = Some(format!("{}: {error}", command.name));
            }
        }
    }

    /// A reply ends only what its own command began: a snapshot or revisions
    /// read queued before an edit must not release the edit's busy guard.
    fn settle(&mut self, name: &str) {
        if matches!(name, "snapshot" | "revisions") {
            self.loading = false;
        } else {
            self.in_flight = false;
        }
    }

    /// The worker queue refused a refresh, so no reply will ever come.
    pub(super) fn refresh_refused(&mut self) {
        self.loading = false;
        self.error = Some("snapshot: routing command queue is busy".to_owned());
    }

    /// The worker queue refused an operation, so no reply will ever come.
    pub(super) fn command_refused(&mut self, name: &str) {
        self.in_flight = false;
        self.error = Some(format!("{name}: routing command queue is busy"));
    }

    fn receive_response(&mut self, command: Command, response: Response) {
        self.settle(command.name);
        if command.name == "snapshot" && response.status / 100 == 2 {
            self.receive_snapshot(response.body);
            return;
        }
        if command.name == "revisions" && response.status / 100 == 2 {
            self.revisions = array(&response.body, "revisions").to_vec();
            // Newest first, whatever order they were sent in.
            self.revisions.sort_by_key(|revision| {
                std::cmp::Reverse(revision.get("revision_id").and_then(Value::as_i64))
            });
            self.error = None;
            self.clamp_selection();
            return;
        }
        if response.status / 100 != 2 {
            self.error = Some(control_error(&command, &response));
            return;
        }
        match command.name {
            "preview" => {
                self.preview_asked = command.body.as_ref().is_some_and(|body| {
                    body.get("request")
                        .and_then(|request| request.get("asks_server_safeguards"))
                        == Some(&Value::Bool(true))
                });
                self.modal = Modal::PreviewResult(response.body);
            }
            "save" | "reactivate" => {
                self.merge_activation(response.body);
                self.modal = Modal::None;
            }
            "promote" => self.modal = Modal::PreviewResult(response.body),
            _ => self.error = None,
        }
    }

    fn receive_snapshot(&mut self, snapshot: Value) {
        self.active_revision_id = data::revision_id(&snapshot);
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
    }

    /// Handle a routing-page key.  Returned commands must be sent by the page's
    /// bounded worker; commands never perform synchronous I/O in this state.
    pub(super) fn key(&mut self, key: KeyEvent) -> Option<Command> {
        if self.in_flight {
            if key.code == KeyCode::Esc {
                self.modal = Modal::None;
            }
            return None;
        }
        if self.modal.active() {
            return self.modal_key(key);
        }
        match key.code {
            KeyCode::Char('e') => {
                self.open_edit();
                None
            }
            KeyCode::Char('n') => {
                self.open_add();
                None
            }
            KeyCode::Char('d') => {
                self.open_remove();
                None
            }
            KeyCode::Char('v') => {
                self.open_preview();
                None
            }
            KeyCode::Char('a') if self.tab == Tab::History => {
                self.open_reactivate();
                None
            }
            KeyCode::Char('p') => {
                self.open_promote();
                None
            }
            KeyCode::Enter => {
                self.open();
                None
            }
            KeyCode::Esc => {
                self.close();
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
        let selected = self
            .target()
            .and_then(|target| Some((target.kind, self.record(&target)?.clone())));
        if let Some((kind, value)) = selected {
            let context = self.form_context();
            self.modal = Modal::Form(Box::new(Form::new(
                Purpose::Record {
                    kind,
                    adding: false,
                },
                value,
                context,
            )));
        }
    }

    /// What a form completes from: the bindings toker implements, what the
    /// catalogues offer, the policy's record ids, and the daemon's findings on
    /// the record being edited, keyed by their path inside it.
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
        }
        context
    }

    /// Start adding a record. A tab that holds one kind goes straight to its
    /// template; one that holds several asks which kind first.
    fn open_add(&mut self) {
        let kinds = super::template::kinds(self.tab, self.models.focus);
        match kinds {
            [] => {
                self.error = Some("there is nothing to add in this view".to_owned());
            }
            [kind] => self.open_template(kind),
            _ => {
                let preselected = self
                    .target()
                    .and_then(|target| kinds.iter().position(|kind| *kind == target.kind))
                    .unwrap_or(0);
                self.modal = Modal::NewKind { kinds, preselected };
            }
        }
    }

    fn open_template(&mut self, kind: &'static str) {
        let route_id = self
            .snapshot
            .as_ref()
            .and_then(data::policy)
            .and_then(|policy| array(policy, "routes").first())
            .and_then(|route| route.get("id"))
            .and_then(Value::as_str);
        let context = self.form_context();
        self.modal = Modal::Form(Box::new(Form::new(
            Purpose::Record { kind, adding: true },
            super::template::template(kind, route_id),
            context,
        )));
    }

    fn open_remove(&mut self) {
        let selected = self
            .target()
            .and_then(|target| Some((target.kind, self.record(&target)?.clone())));
        if let Some((kind, value)) = selected {
            self.modal = Modal::Remove {
                key: kind.to_owned(),
                value,
            };
        }
    }

    fn open_preview(&mut self) {
        self.modal = Modal::Form(Box::new(Form::new(
            Purpose::Preview,
            json!({ "protocol": "anthropic_messages", "prompt_bound": 0, "tool_count": 0 }),
            Context::default(),
        )));
    }

    fn open_reactivate(&mut self) {
        let revision_id = self
            .revisions
            .get(self.selected_in(Tab::History))
            .and_then(|revision| revision.get("revision_id"))
            .and_then(Value::as_i64);
        if let Some(revision_id) = revision_id {
            self.modal = Modal::Reactivate { revision_id };
        } else {
            self.error = Some("refresh revisions, then choose a revision to reactivate".to_owned());
        }
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
            Modal::NewKind { kinds, preselected } => {
                let chosen = match key.code {
                    KeyCode::Esc => return None,
                    KeyCode::Enter => Some(preselected),
                    KeyCode::Char(digit) => digit
                        .to_digit(10)
                        .and_then(|digit| usize::try_from(digit).ok())
                        .and_then(|digit| digit.checked_sub(1))
                        .filter(|index| *index < kinds.len()),
                    _ => None,
                };
                match chosen.and_then(|index| kinds.get(index)) {
                    Some(kind) => {
                        self.open_template(kind);
                        return None;
                    }
                    None => (Modal::NewKind { kinds, preselected }, ModalAction::None),
                }
            }
            Modal::Remove {
                key: record_key,
                value,
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => (
                    Modal::Remove {
                        key: record_key.clone(),
                        value: value.clone(),
                    },
                    ModalAction::Edit(vec![remove_edit(&record_key, &value)]),
                ),
                KeyCode::Esc | KeyCode::Char('n') => (Modal::None, ModalAction::None),
                _ => (
                    Modal::Remove {
                        key: record_key,
                        value,
                    },
                    ModalAction::None,
                ),
            },
            Modal::PreviewResult(_) => (Modal::None, ModalAction::None),
            Modal::Reactivate { revision_id } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => (
                    Modal::Reactivate { revision_id },
                    ModalAction::Reactivate { revision_id },
                ),
                KeyCode::Esc | KeyCode::Char('n') => return None,
                _ => (Modal::Reactivate { revision_id }, ModalAction::None),
            },
        };
        self.modal = modal;
        match action {
            ModalAction::None => None,
            ModalAction::Edit(edits) => self.edit(edits),
            ModalAction::Preview(facts) => self.preview(&facts),
            ModalAction::Reactivate { revision_id } => self.reactivate(revision_id),
            ModalAction::Promote { binding, model } => self.promote(&binding, &model),
        }
    }

    /// Save the typed edits against the revision the page last saw live. The
    /// daemon refuses a save made against a revision that has since moved, and
    /// the form stays open as entered.
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

    /// The preview request from the form's facts: only what was filled in.
    fn preview(&mut self, facts: &Value) -> Option<Command> {
        self.in_flight = true;
        let mut request = json!({
            "kind": "invented",
            "protocol": facts.get("protocol").cloned().unwrap_or(Value::Null),
        });
        for key in ["profile", "model", "lane_key"] {
            if let Some(text) = facts
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                request[key] = Value::String(text.to_owned());
            }
        }
        for key in ["prompt_bound", "message_count", "tool_count"] {
            if let Some(number) = facts.get(key).and_then(Value::as_u64) {
                request[key] = Value::from(number);
            }
        }
        if facts.get("asks_server_safeguards") == Some(&Value::Bool(true)) {
            request["asks_server_safeguards"] = Value::Bool(true);
        }
        let body = json!({
            "mode": "resolve",
            "expected_revision_id": self.active_revision_id,
            "request": request,
        });
        Some(command(
            "preview",
            Method::POST,
            "preview",
            "routing-preview",
            body,
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

#[derive(Debug)]
enum ModalAction {
    None,
    Edit(Vec<Value>),
    Preview(Value),
    Reactivate { revision_id: i64 },
    Promote { binding: String, model: String },
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
    }
}

fn replace_edit(key: &str, value: Value) -> Value {
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    match key {
        "defaults" => {
            let scope = value.get("scope").cloned().unwrap_or(Value::Null);
            json!({ "kind": "replace_default", "scope": scope, "default": value })
        }
        "compaction" => json!({ "kind": "replace_compaction", "compaction": value }),
        "presentations" => {
            json!({ "kind": "replace_presentation", "id": id, "presentation": value })
        }
        "routes" => json!({ "kind": "replace_route", "id": id, "route": value }),
        "rewrites" => json!({ "kind": "replace_rewrite", "id": id, "rewrite": value }),
        "classifiers" => json!({ "kind": "replace_classifier", "id": id, "classifier": value }),
        "upgrades" => json!({ "kind": "replace_upgrade", "id": id, "upgrade": value }),
        _ => Value::Null,
    }
}

/// The typed addition of a new record. It carries no id of its own to
/// compare against: the daemon refuses an id or scope already taken.
pub(super) fn add_edit(key: &str, value: Value) -> Value {
    match key {
        "presentations" => json!({ "kind": "add_presentation", "presentation": value }),
        "routes" => json!({ "kind": "add_route", "route": value }),
        "defaults" => json!({ "kind": "add_default", "default": value }),
        "rewrites" => json!({ "kind": "add_rewrite", "rewrite": value }),
        "classifiers" => json!({ "kind": "add_classifier", "classifier": value }),
        "upgrades" => json!({ "kind": "add_upgrade", "upgrade": value }),
        "compaction" => json!({ "kind": "add_compaction", "compaction": value }),
        _ => Value::Null,
    }
}

fn remove_edit(key: &str, value: &Value) -> Value {
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    match key {
        "defaults" => {
            let scope = value.get("scope").cloned().unwrap_or(Value::Null);
            json!({ "kind": "remove_default", "scope": scope })
        }
        "compaction" => json!({ "kind": "remove_compaction" }),
        "presentations" => json!({ "kind": "remove_presentation", "id": id }),
        "routes" => json!({ "kind": "remove_route", "id": id }),
        "rewrites" => json!({ "kind": "remove_rewrite", "id": id }),
        "classifiers" => json!({ "kind": "remove_classifier", "id": id }),
        "upgrades" => json!({ "kind": "remove_upgrade", "id": id }),
        _ => Value::Null,
    }
}

/// The edit a submitted form sends, or what it asks for.
fn form_action(form: &Form) -> ModalAction {
    let value = form.value();
    match form.purpose() {
        Purpose::Record { kind, adding } => {
            let mut record = value.clone();
            // The flag is the one route field with its own operation, which
            // is what raises the document to the format that carries it, so
            // the record travels without it and the operation follows.
            let honours = (kind == "routes")
                .then(|| {
                    record
                        .as_object_mut()
                        .and_then(|route| route.remove("honours_server_safeguards"))
                })
                .flatten();
            let mut edits = vec![if adding {
                add_edit(kind, record)
            } else {
                replace_edit(kind, record)
            }];
            if kind == "routes" {
                edits.push(json!({
                    "kind": "set_route_server_safeguards",
                    "route_id": value.get("id").cloned().unwrap_or(Value::Null),
                    "honours": honours == Some(Value::Bool(true)),
                }));
            }
            ModalAction::Edit(edits)
        }
        Purpose::Description => ModalAction::Edit(vec![json!({
            "kind": "set_description",
            "description": value.get("description").cloned().unwrap_or(Value::Null),
        })]),
        Purpose::Promote => ModalAction::Promote {
            binding: text_of(value, "binding_id"),
            model: text_of(value, "model_id"),
        },
        Purpose::Preview => ModalAction::Preview(value.clone()),
    }
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
    use crate::tui::routing::form::Purpose;
    use crate::tui::routing::testing::{self, World};

    const CTRL_ENTER: KeyEvent = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);

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

    /// The open form's purpose, if a form is open.
    fn form_purpose(state: &State) -> Option<Purpose> {
        match state.modal() {
            Modal::Form(form) => Some(form.purpose()),
            _ => None,
        }
    }

    fn record_form(kind: &'static str, adding: bool) -> Option<Purpose> {
        Some(Purpose::Record { kind, adding })
    }

    fn form_value(state: &State) -> serde_json::Value {
        let Modal::Form(form) = state.modal() else {
            panic!("a form is open");
        };
        form.value().clone()
    }

    fn first_operation(command: Command) -> serde_json::Value {
        command.body.expect("body")["edit"]["operations"][0].clone()
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

    // --- editing ----------------------------------------------------------

    #[test]
    fn escape_cancels_every_confirmation_without_a_command() {
        let mut state = loaded(World::AT_REST);
        for key in ['d', 'p', 'v'] {
            press(&mut state, KeyCode::Char(key));
            assert!(state.modal().active(), "{key} opens a form");
            assert!(press(&mut state, KeyCode::Esc).is_none());
            assert!(!state.modal().active(), "{key} closes on Esc");
        }
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(form_purpose(&state), record_form("presentations", false));
        press(&mut state, KeyCode::Esc);
        assert!(!state.modal().active());
        state.set_tab(Tab::History);
        // The top of history is the live revision, whose description is
        // edited.
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(form_purpose(&state), Some(Purpose::Description));
        press(&mut state, KeyCode::Esc);
        assert!(
            !state.modal().active(),
            "the description form closes on Esc"
        );
    }

    #[test]
    fn removal_needs_a_yes_and_names_the_record() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('d'));
        assert!(
            press(&mut state, KeyCode::Char('x')).is_none(),
            "other keys do nothing"
        );
        let command = press(&mut state, KeyCode::Char('y')).expect("confirmed");
        let operation = first_operation(command);
        assert_eq!(operation["kind"], "remove_presentation");
        assert_eq!(operation["id"], "pick-1");
    }

    #[test]
    fn a_rule_is_edited_and_removed_by_its_id() {
        let mut state = on(Tab::Rewrites);
        state.move_selection(1);
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(form_purpose(&state), record_form("rewrites", false));
        assert_eq!(form_value(&state)["id"], "anthropic-messages-2");
        press(&mut state, KeyCode::Esc);
        press(&mut state, KeyCode::Char('d'));
        let operation = first_operation(press(&mut state, KeyCode::Char('y')).expect("sent"));
        assert_eq!(operation["kind"], "remove_rewrite");
        assert_eq!(operation["id"], "anthropic-messages-2");
    }

    #[test]
    fn a_record_edit_sends_the_typed_replacement() {
        let mut state = on(Tab::Routes);
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(form_purpose(&state), record_form("routes", false));
        assert_eq!(form_value(&state)["id"], "claude-sub");
        // The id names the record the edit replaces, so it is fixed.
        typed(&mut state, "x");
        assert_eq!(form_value(&state)["id"], "claude-sub");
        // Move to the route's name and change it.
        press(&mut state, KeyCode::Down);
        typed(&mut state, "named");
        let command = state.key(CTRL_ENTER).expect("edit command");
        assert_eq!(command.method, Method::POST);
        let operation = first_operation(command);
        assert_eq!(operation["kind"], "replace_route");
        assert_eq!(operation["id"], "claude-sub");
        assert_eq!(operation["route"]["display_name"], "claude-subnamed");
        // What the schema does not name is carried through as it was.
        assert_eq!(operation["route"]["chain"]["first"]["id"], "sub");
    }

    #[test]
    fn keys_are_ignored_while_a_command_is_in_flight() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('d'));
        press(&mut state, KeyCode::Char('y'));
        assert!(state.in_flight());
        assert!(press(&mut state, KeyCode::Char('y')).is_none());
        assert!(press(&mut state, KeyCode::Char('n')).is_none());
        state.receive(reply(
            "save",
            200,
            serde_json::json!({"active_revision": {"revision_id": 4}}),
        ));
        assert!(!state.in_flight());
        assert_eq!(state.active_revision_id(), Some(4));
    }

    #[test]
    fn the_summary_names_lane_impact_and_publications() {
        let state = loaded(World::AT_REST);
        let summary = state.activation_summary().join("\n");
        assert!(
            summary.contains("lanes: 10 total, 0 unpinned, 2 warm, 8 cold, 0 unknown"),
            "{summary}"
        );
        assert!(
            summary.contains("publication claude_settings: applied"),
            "{summary}"
        );
        assert_eq!(
            State::default().activation_summary(),
            ["no daemon snapshot loaded"]
        );
    }

    #[test]
    fn a_save_moved_past_names_the_live_revision_and_keeps_the_form() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('n'));
        let command = state.key(CTRL_ENTER).expect("add command");
        assert_eq!(command.body.expect("body")["expected_revision_id"], 9);
        state.receive(reply(
            "save",
            409,
            serde_json::json!({"error": "stale_revision", "live_revision_id": 12}),
        ));
        assert_eq!(form_purpose(&state), record_form("presentations", true));
        assert!(!state.in_flight());
        let error = state.error().expect("the refusal is shown");
        assert!(error.contains("revision 12 is now live"), "{error}");
    }

    #[test]
    fn an_invalid_save_names_its_diagnostics() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('d'));
        press(&mut state, KeyCode::Char('y'));
        state.receive(reply(
            "save",
            422,
            serde_json::json!({"error": "invalid_policy", "diagnostics": [
                {"code": "unknown_binding", "path": "/routes/0/chain/first"}
            ]}),
        ));
        let error = state.error().expect("error").to_owned();
        assert!(
            error.contains("unknown_binding at /routes/0/chain/first"),
            "{error}"
        );
    }

    #[test]
    fn reactivation_confirms_the_selected_revision() {
        let mut state = on(Tab::History);
        // Newest first, whatever order the daemon sent them in.
        state.receive(reply(
            "revisions",
            200,
            serde_json::json!({"revisions": [
                {"revision_id": 4}, {"revision_id": 9}, {"revision_id": 7},
            ]}),
        ));
        state.move_selection(2);
        assert!(press(&mut state, KeyCode::Char('a')).is_none());
        assert!(matches!(
            state.modal(),
            Modal::Reactivate { revision_id: 4 }
        ));
        let command = press(&mut state, KeyCode::Char('y')).expect("confirmed");
        let body = command.body.expect("body");
        assert_eq!(command.name, "reactivate");
        assert_eq!(body["revision_id"], 4);
        assert_eq!(body["expected_revision_id"], 9);
        assert!(body.get("scope").is_none(), "a reactivation names no scope");
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
        assert_eq!(form_purpose(&state), Some(Purpose::Promote));
        // A binding is typed, or completed from the ones toker implements;
        // a model completes from what that binding offers.
        typed(&mut state, testing::OR_CHAT);
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Tab);
        assert_eq!(form_value(&state)["model_id"], "anthropic/model-000");
        // A second Tab moves to the next offer.
        press(&mut state, KeyCode::Tab);
        let command = press(&mut state, KeyCode::Enter).expect("promotion");
        let body = command.body.expect("body");
        assert_eq!(body["binding_id"], testing::OR_CHAT);
        assert_eq!(body["model_id"], "anthropic/model-001");
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
        let value = form_value(&state);
        assert_eq!(value["binding_id"], testing::API);
        assert_eq!(value["model_id"], "claude-sonnet-5");
    }

    #[test]
    fn the_preview_form_sends_only_the_facts_that_were_filled_in() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('v'));
        assert_eq!(form_purpose(&state), Some(Purpose::Preview));
        // protocol, then profile, model, lane key, prompt bound.
        press(&mut state, KeyCode::Right);
        assert_eq!(form_value(&state)["protocol"], "openai_chat");
        press(&mut state, KeyCode::Down);
        typed(&mut state, "work");
        for _ in 0..3 {
            press(&mut state, KeyCode::Down);
        }
        typed(&mut state, "5000");
        let command = press(&mut state, KeyCode::Enter).expect("preview");
        let body = command.body.expect("body");
        assert_eq!(command.name, "preview");
        assert_eq!(body["mode"], "resolve");
        assert_eq!(body["request"]["kind"], "invented");
        assert_eq!(body["request"]["protocol"], "openai_chat");
        assert_eq!(body["request"]["profile"], "work");
        assert_eq!(body["request"]["prompt_bound"], 5000);
        assert!(body["request"].get("model").is_none());
        assert!(body["request"].get("message_count").is_none());
    }

    #[test]
    fn the_preview_form_can_ask_for_server_safeguards() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('v'));
        // Down to the last field: protocol, profile, model, lane key,
        // prompt bound, message count, tool count, then the switch.
        for _ in 0..7 {
            press(&mut state, KeyCode::Down);
        }
        press(&mut state, KeyCode::Char(' '));
        assert_eq!(form_value(&state)["asks_server_safeguards"], true);
        let command = state.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let body = command.expect("preview").body.expect("body");
        assert_eq!(body["request"]["asks_server_safeguards"], true);
        assert!(
            !state.preview_asked(),
            "only a reply settles what was shown"
        );

        // The page remembers the ask, since a result with no candidate
        // left does not say whether it was made.
        let mut ask = Command::snapshot("routing-snapshot");
        ask.name = "preview";
        ask.body = Some(body);
        state.receive(Reply::Command(
            ask,
            Ok(Response {
                status: 200,
                body: serde_json::json!({"result": {"outcome": "unavailable"}}),
            }),
        ));
        assert!(state.preview_asked());
    }

    #[test]
    fn a_preview_that_does_not_ask_sends_no_such_fact() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('v'));
        let body = state
            .key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .expect("preview")
            .body
            .expect("body");
        assert!(body["request"].get("asks_server_safeguards").is_none());
    }

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
        let command = state.key(CTRL_ENTER).expect("edit command");
        let operations = command.body.expect("body")["edit"]["operations"].clone();
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
        state.receive(reply("save", 200, serde_json::json!({})));

        // Left off, the same save clears it.
        press(&mut state, KeyCode::Char('e'));
        let command = state.key(CTRL_ENTER).expect("edit command");
        let operations = command.body.expect("body")["edit"]["operations"].clone();
        assert_eq!(operations[1]["honours"], false);
    }

    #[test]
    fn only_a_route_save_carries_the_honour_flag_operation() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('e'));
        let command = state.key(CTRL_ENTER).expect("edit command");
        let operations = command.body.expect("body")["edit"]["operations"].clone();
        assert_eq!(operations.as_array().expect("operations").len(), 1);
    }

    /// The terminal builds its edits by hand; the daemon's typed vocabulary is
    /// the authority on whether they are the edits it understands.
    #[test]
    fn removals_and_descriptions_are_the_daemons_typed_operations() {
        use crate::policy::PolicyEdit;
        let record = serde_json::json!({"id": "some-id"});
        let operations: Vec<_> = [
            "presentations",
            "routes",
            "rewrites",
            "classifiers",
            "upgrades",
        ]
        .into_iter()
        .map(|key| super::remove_edit(key, &record))
        .chain([serde_json::json!({"kind": "set_description", "description": null})])
        .collect();
        let edit: PolicyEdit =
            serde_json::from_value(serde_json::json!({ "operations": operations }))
                .expect("every emitted operation is a typed PolicyEdit operation");
        assert_eq!(edit.operations.len(), 6);
    }

    #[test]
    fn a_read_reply_does_not_release_a_pending_edit() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('d'));
        press(&mut state, KeyCode::Char('y'));
        assert!(state.in_flight());
        // The page's earlier snapshot and revisions reads land first.
        state.receive(reply(
            "revisions",
            200,
            serde_json::json!({"revisions": []}),
        ));
        state.receive(reply("snapshot", 200, testing::snapshot(World::AT_REST)));
        assert!(state.in_flight());
        assert!(press(&mut state, KeyCode::Char('y')).is_none());
        state.receive(reply(
            "save",
            200,
            serde_json::json!({"active_revision": {"revision_id": 4}}),
        ));
        assert!(!state.in_flight());
    }

    #[test]
    fn a_refused_queue_slot_releases_the_busy_guards() {
        let mut state = loaded(World::AT_REST);
        assert!(state.begin_refresh());
        state.refresh_refused();
        assert!(state.begin_refresh(), "a refused refresh can be retried");
        press(&mut state, KeyCode::Char('d'));
        press(&mut state, KeyCode::Char('y'));
        state.command_refused("save");
        assert!(!state.in_flight());
        assert!(state.error().is_some_and(|error| error.contains("busy")));
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
    fn a_default_is_replaced_and_removed_by_its_scope() {
        let mut state = loaded(World::FULL);
        state.set_tab(Tab::Defaults);
        // Unprefixed messages, messages, chat, unprefixed chat, responses.
        state.move_selection(3);
        press(&mut state, KeyCode::Char('e'));
        let command = state.key(CTRL_ENTER).expect("replace command");
        let operation = first_operation(command);
        assert_eq!(operation["kind"], "replace_default");
        assert_eq!(operation["scope"]["profile"], "unprefixed");
        assert_eq!(
            operation["default"]["target"]["route_id"],
            "openrouter-chat"
        );
        state.receive(reply("save", 200, serde_json::json!({})));

        press(&mut state, KeyCode::Char('d'));
        let operation = first_operation(press(&mut state, KeyCode::Char('y')).expect("confirmed"));
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
        assert!(!state.modal().active());
    }

    #[test]
    fn compaction_is_replaced_and_removed_without_an_id() {
        let mut state = on(Tab::Models);
        state.move_selection(5);
        assert_eq!(state.models().focus, Panel::Compaction);
        press(&mut state, KeyCode::Char('d'));
        let operation = first_operation(press(&mut state, KeyCode::Char('y')).expect("confirmed"));
        assert_eq!(operation["kind"], "remove_compaction");
        state.receive(reply("save", 200, serde_json::json!({})));
        press(&mut state, KeyCode::Char('e'));
        let operation = first_operation(state.key(CTRL_ENTER).expect("replace command"));
        assert_eq!(operation["kind"], "replace_compaction");
        assert_eq!(operation["compaction"]["target"]["kind"], "substitution");
    }

    /// The terminal builds default and compaction edits by hand as well; the
    /// daemon's typed vocabulary is the authority on what it understands.
    #[test]
    fn default_and_compaction_edits_are_the_daemons_typed_operations() {
        use crate::policy::PolicyEdit;
        let default = serde_json::json!({
            "scope": {"kind": "protocol", "protocol": "openai_chat"},
            "target": {"route_id": "route-a"},
        });
        let compaction = serde_json::json!({
            "target": {"route_id": "route-a"},
            "lane_transition": "move_when_cold",
            "cache_transition": "allow_when_cold",
        });
        let operations = [
            super::replace_edit("defaults", default.clone()),
            super::remove_edit("defaults", &default),
            super::replace_edit("compaction", compaction.clone()),
            super::remove_edit("compaction", &compaction),
        ];
        let edit: PolicyEdit =
            serde_json::from_value(serde_json::json!({ "operations": operations }))
                .expect("every emitted operation is a typed PolicyEdit operation");
        assert_eq!(edit.operations.len(), 4);
    }

    #[test]
    fn adding_a_presentation_sends_the_typed_addition_against_the_live_revision() {
        let mut state = loaded(World::AT_REST);
        assert!(press(&mut state, KeyCode::Char('n')).is_none());
        assert_eq!(
            form_purpose(&state),
            record_form("presentations", true),
            "a one-kind tab opens the template directly"
        );
        assert_eq!(form_value(&state)["id"], "new-presentation");
        let command = state.key(CTRL_ENTER).expect("add command");
        assert_eq!(command.name, "save");
        assert_eq!(command.method, Method::POST);
        let body = command.body.expect("body");
        assert_eq!(body["expected_revision_id"], 9);
        let operation = &body["edit"]["operations"][0];
        assert_eq!(operation["kind"], "add_presentation");
        assert_eq!(operation["presentation"]["id"], "new-presentation");
        assert!(state.in_flight());
        // The activation the daemon answers with closes the form.
        state.receive(reply("save", 200, serde_json::json!({})));
        assert!(!state.modal().active());
    }

    #[test]
    fn an_added_record_is_what_was_typed_into_the_form() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('n'));
        for _ in 0.."new-presentation".len() {
            press(&mut state, KeyCode::Backspace);
        }
        typed(&mut state, "mine");
        let command = state.key(CTRL_ENTER).expect("add command");
        let operation = first_operation(command);
        assert_eq!(operation["presentation"]["id"], "mine");
        assert_eq!(operation["presentation"]["slug"], "new-model");
    }

    #[test]
    fn a_blank_id_stays_open_and_sends_nothing() {
        let mut state = loaded(World::AT_REST);
        press(&mut state, KeyCode::Char('n'));
        for _ in 0.."new-presentation".len() {
            press(&mut state, KeyCode::Backspace);
        }
        assert!(state.key(CTRL_ENTER).is_none());
        let Modal::Form(form) = state.modal() else {
            panic!("the form stays open");
        };
        assert_eq!(form.note(), Some("id is empty"));
        assert!(!state.in_flight());
        press(&mut state, KeyCode::Esc);
        assert!(!state.modal().active());
    }

    #[test]
    fn adding_a_record_whose_id_exists_shows_the_daemons_refusal_in_the_form() {
        let mut state = on(Tab::Routes);
        press(&mut state, KeyCode::Char('n'));
        assert!(state.key(CTRL_ENTER).is_some());
        // The refusal text is the daemon's own: the template's route added
        // to a policy that already holds a route of that id.
        let record = crate::tui::routing::template::template("routes", None);
        let live: crate::policy::PolicyDocument = serde_json::from_value(serde_json::json!({
            "format_version": 3, "routes": [record.clone()],
        }))
        .expect("document");
        let duplicate: crate::policy::PolicyEdit = serde_json::from_value(
            serde_json::json!({ "operations": [super::add_edit("routes", record)] }),
        )
        .expect("edit");
        let refusal = duplicate.apply(&live).expect_err("the id is taken");
        state.receive(reply(
            "save",
            422,
            serde_json::json!({
                "error": "invalid_policy_edit",
                "detail": refusal.to_string(),
            }),
        ));
        assert_eq!(
            form_purpose(&state),
            record_form("routes", true),
            "the form stays open so the id can be changed"
        );
        assert!(!state.in_flight());
        let error = state.error().expect("the refusal is shown");
        assert!(error.contains("invalid_policy_edit"), "{error}");
        assert!(error.contains("already exists"), "{error}");
        // Fixing the id and saving again is an ordinary second attempt.
        assert!(state.key(CTRL_ENTER).is_some());
    }

    #[test]
    fn the_models_tab_asks_which_kind_to_add_by_the_panel_it_is_in() {
        let mut state = on(Tab::Models);
        // The focus is in upgrades: an upgrade is the preselected kind.
        press(&mut state, KeyCode::Char('n'));
        assert!(matches!(
            state.modal(),
            Modal::NewKind { preselected: 0, .. }
        ));
        assert!(press(&mut state, KeyCode::Char('x')).is_none());
        assert!(matches!(state.modal(), Modal::NewKind { .. }));
        press(&mut state, KeyCode::Enter);
        assert_eq!(form_purpose(&state), record_form("upgrades", true));
        press(&mut state, KeyCode::Esc);

        // In the compaction line, the compaction is.
        state.move_selection(5);
        press(&mut state, KeyCode::Char('n'));
        assert!(matches!(
            state.modal(),
            Modal::NewKind { preselected: 1, .. }
        ));
        // A number picks a kind whatever the focus is on.
        press(&mut state, KeyCode::Char('9'));
        assert!(matches!(state.modal(), Modal::NewKind { .. }));
        press(&mut state, KeyCode::Char('1'));
        assert_eq!(form_purpose(&state), record_form("upgrades", true));
    }

    #[test]
    fn every_kind_of_record_can_be_added_from_the_tab_that_shows_it() {
        for (tab, digits, kind, operation) in [
            (Tab::Presentations, "", "presentations", "add_presentation"),
            (Tab::Rewrites, "", "rewrites", "add_rewrite"),
            (Tab::Defaults, "", "defaults", "add_default"),
            (Tab::Routes, "", "routes", "add_route"),
            (Tab::Models, "1", "upgrades", "add_upgrade"),
            (Tab::Models, "2", "compaction", "add_compaction"),
        ] {
            let mut state = on(tab);
            press(&mut state, KeyCode::Char('n'));
            for digit in digits.chars() {
                press(&mut state, KeyCode::Char(digit));
            }
            assert_eq!(form_purpose(&state), record_form(kind, true), "{tab:?}");
            let operation_sent = first_operation(state.key(CTRL_ENTER).expect("add command"));
            assert_eq!(operation_sent["kind"], operation, "{tab:?}");
        }
        // A family is added with the focus in families.
        let mut state = on(Tab::Models);
        state.move_selection(-1);
        press(&mut state, KeyCode::Char('n'));
        assert_eq!(form_purpose(&state), record_form("classifiers", true));
    }

    #[test]
    fn a_new_default_starts_from_the_first_route_and_adds_by_scope() {
        let mut state = on(Tab::Defaults);
        press(&mut state, KeyCode::Char('n'));
        let template = form_value(&state);
        assert_eq!(template["target"]["route_id"], "claude-sub");
        let command = state.key(CTRL_ENTER).expect("add command");
        let operation = first_operation(command);
        assert_eq!(operation["kind"], "add_default");
        assert_eq!(operation["default"], template);
    }

    #[test]
    fn adding_is_not_offered_in_history_or_while_busy() {
        let mut state = on(Tab::History);
        assert!(press(&mut state, KeyCode::Char('n')).is_none());
        assert!(!state.modal().active());
        assert!(state.error().is_some());
        state.set_tab(Tab::Presentations);
        press(&mut state, KeyCode::Char('d'));
        press(&mut state, KeyCode::Char('y'));
        assert!(state.in_flight());
        press(&mut state, KeyCode::Char('n'));
        assert!(form_purpose(&state).is_none());
    }
}
