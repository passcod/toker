//! Content-free routing snapshot, draft editing, and navigation state.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reqwest::Method;
use serde_json::{Value, json};

use super::client::{Command, Reply, Response};

/// Lines a page key moves the detail pane.
const DETAIL_PAGE: i32 = 10;

/// The routing page's four master/detail views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum View {
    Presentations,
    Routes,
    Rewrites,
    Status,
}

impl View {
    const ALL: [View; 4] = [
        View::Presentations,
        View::Routes,
        View::Rewrites,
        View::Status,
    ];

    pub(super) fn title(self) -> &'static str {
        match self {
            View::Presentations => "PRESENTATIONS",
            View::Routes => "ROUTES, CANDIDATES & DEFAULTS",
            View::Rewrites => "REWRITES, FAMILIES, UPGRADES & COMPACTION",
            View::Status => "PREVIEW, REVISIONS & ACTIVATION",
        }
    }

    fn next(self, delta: isize) -> View {
        let here = Self::ALL.iter().position(|view| *view == self).unwrap_or(0) as isize;
        let len = Self::ALL.len() as isize;
        Self::ALL[(here + delta).rem_euclid(len) as usize]
    }
}

/// A modal editing or safety-confirmation state.
#[derive(Debug, Clone, Default)]
pub(super) enum Modal {
    #[default]
    None,
    Description {
        value: String,
    },
    /// Existing typed records serialize exactly as the daemon's typed DTOs.
    /// This avoids lossy, path-addressed edits while still allowing every
    /// current and future field to be changed from the terminal.
    ///
    /// `adding` marks a record that does not exist yet: it saves as an add
    /// rather than a replacement, and the daemon refuses an id already taken.
    Record {
        key: String,
        value: String,
        adding: bool,
    },
    /// Which kind of record to add, in a view that lists several.
    NewKind {
        kinds: &'static [&'static str],
        preselected: usize,
    },
    Remove {
        key: String,
        value: Value,
    },
    CandidateOrder {
        route: Value,
        selected: usize,
    },
    Preview {
        field: usize,
        values: Vec<String>,
    },
    PreviewResult(Value),
    Activate {
        scope: usize,
        force_cache_transition: bool,
    },
    Reactivate {
        revision_id: i64,
        scope: usize,
        force_cache_transition: bool,
    },
    Promote {
        field: usize,
        binding: String,
        model: String,
    },
    /// Replace the draft with the compiled legacy `toker.toml` routing.
    Import,
    Conflict {
        daemon_generation: Option<u64>,
        daemon_draft: Value,
    },
}

impl Modal {
    pub(super) fn active(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub(super) fn title(&self) -> &'static str {
        match self {
            Self::None => "",
            Self::Description { .. } => "EDIT DRAFT DESCRIPTION",
            Self::Record { adding: false, .. } => "EDIT TYPED POLICY RECORD",
            Self::Record { adding: true, .. } => "ADD TYPED POLICY RECORD",
            Self::NewKind { .. } => "ADD POLICY RECORD",
            Self::Remove { .. } => "REMOVE POLICY RECORD",
            Self::CandidateOrder { .. } => "ORDER ROUTE CANDIDATES",
            Self::Preview { .. } | Self::PreviewResult(_) => "ROUTING PREVIEW",
            Self::Activate { .. } => "ACTIVATE DRAFT",
            Self::Reactivate { .. } => "REACTIVATE REVISION",
            Self::Promote { .. } => "PROMOTE LEARNED MODEL",
            Self::Import => "IMPORT LEGACY CONFIG",
            Self::Conflict { .. } => "DRAFT CHANGED BY DAEMON",
        }
    }
}

/// Data and selection the page preserves while the dashboard is visible.
#[derive(Debug)]
pub(super) struct State {
    view: View,
    snapshot: Option<Value>,
    revisions: Vec<Value>,
    generation: Option<u64>,
    selected: usize,
    error: Option<String>,
    /// The outcome of the last post-activation Claude settings sync.
    settings_note: Option<String>,
    loading: bool,
    in_flight: bool,
    modal: Modal,
    /// How far the detail pane is scrolled, and how far it can be; the view
    /// owns the second because only it knows the pane's height and wrapping.
    detail_scroll: u16,
    detail_furthest: std::cell::Cell<u16>,
    /// The exact typed edit batch that received a 409.  It remains intact until
    /// the user explicitly reloads or reapplies it against the daemon copy.
    conflicted_edits: Vec<Value>,
    /// Compiles the legacy config into a policy document; replaceable so the
    /// page's tests need no config file.
    legacy_policy: fn() -> anyhow::Result<Value>,
}

impl Default for State {
    fn default() -> Self {
        State {
            view: View::Presentations,
            snapshot: None,
            revisions: Vec::new(),
            generation: None,
            selected: 0,
            error: None,
            settings_note: None,
            loading: false,
            in_flight: false,
            modal: Modal::None,
            detail_scroll: 0,
            detail_furthest: std::cell::Cell::new(0),
            conflicted_edits: Vec::new(),
            legacy_policy: super::import::legacy_policy,
        }
    }
}

impl State {
    pub(super) fn view(&self) -> View {
        self.view
    }

    pub(super) fn generation(&self) -> Option<u64> {
        self.generation
    }

    /// What the daemon last sent, for the detail pane to lay out.
    pub(super) fn snapshot(&self) -> Option<&Value> {
        self.snapshot.as_ref()
    }

    pub(super) fn detail_scroll(&self) -> u16 {
        self.detail_scroll
    }

    /// Scroll the detail pane. The view reports how far it can go, so the
    /// offset never runs past the last line and a key back always shows
    /// movement.
    pub(super) fn scroll_detail(&mut self, delta: i32) {
        let furthest = self.detail_furthest.get();
        self.detail_scroll =
            (i32::from(self.detail_scroll) + delta).clamp(0, i32::from(furthest)) as u16;
    }

    /// Called by the view each draw with the last offset that still shows a
    /// line.
    pub(super) fn set_detail_furthest(&self, furthest: u16) {
        self.detail_furthest.set(furthest);
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// What an activation would do, from the snapshot the daemon last sent:
    /// the records the draft changes, the lane impact, any draft diagnostics,
    /// and where each publication stands. Shown before the user confirms.
    pub(super) fn activation_summary(&self) -> Vec<String> {
        let Some(snapshot) = &self.snapshot else {
            return vec!["no daemon snapshot loaded".to_owned()];
        };
        let mut lines = Vec::new();
        let changes = changed_records(snapshot);
        if changes.is_empty() {
            lines.push("changed records: none".to_owned());
        } else {
            lines.push(format!("changed records: {}", changes.join(", ")));
        }
        lines.extend(cost_boundary_changes(snapshot));
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
        let diagnostics = array(snapshot, "draft_diagnostics");
        if !diagnostics.is_empty() {
            lines.push(format!("draft diagnostics: {}", diagnostics.len()));
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

    pub(super) fn move_view(&mut self, delta: isize) {
        self.set_view(self.view.next(delta));
    }

    pub(super) fn set_view(&mut self, view: View) {
        self.view = view;
        self.selected = 0;
        self.detail_scroll = 0;
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        let count = self.entries().len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.saturating_add_signed(delta).min(count - 1);
        }
        self.detail_scroll = 0;
    }

    pub(super) fn selected(&self) -> usize {
        self.selected
    }

    pub(super) fn entries(&self) -> Vec<Entry<'_>> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let policy = draft_policy(snapshot).or_else(|| active_policy(snapshot));
        match self.view {
            View::Presentations => policy
                .into_iter()
                .flat_map(|document| array(document, "presentations"))
                .map(|value| Entry {
                    key: "presentations",
                    value,
                })
                .collect(),
            View::Routes => policy
                .into_iter()
                .flat_map(|document| {
                    ["routes", "defaults"].into_iter().flat_map(move |key| {
                        array(document, key)
                            .iter()
                            .map(move |value| Entry { key, value })
                    })
                })
                .collect(),
            View::Rewrites => policy
                .into_iter()
                .flat_map(|document| {
                    let records = ["rewrites", "classifiers", "upgrades"]
                        .into_iter()
                        .flat_map(move |key| {
                            array(document, key)
                                .iter()
                                .map(move |value| Entry { key, value })
                        });
                    // The compaction policy is one optional record, not a list.
                    let compaction = document
                        .get("compaction")
                        .filter(|value| !value.is_null())
                        .map(|value| Entry {
                            key: "compaction",
                            value,
                        });
                    records.chain(compaction)
                })
                .collect(),
            View::Status => {
                let active = snapshot
                    .get("active_revision")
                    .into_iter()
                    .map(|value| Entry {
                        key: "active_revision",
                        value,
                    });
                let draft = snapshot.get("draft").into_iter().map(|value| Entry {
                    key: "draft",
                    value,
                });
                let revisions = self.revisions.iter().map(|value| Entry {
                    key: "revision",
                    value,
                });
                let publications = array(snapshot, "publications").iter().map(|value| Entry {
                    key: "publications",
                    value,
                });
                let availability = snapshot
                    .get("derived")
                    .map(|derived| array(derived, "availability"))
                    .unwrap_or_default()
                    .iter()
                    .map(|value| Entry {
                        key: "availability",
                        value,
                    });
                let lane_impacts = snapshot.get("lane_impacts").into_iter().map(|value| Entry {
                    key: "lane_impacts",
                    value,
                });
                let diagnostics = array(snapshot, "draft_diagnostics")
                    .iter()
                    .map(|value| Entry {
                        key: "diagnostic",
                        value,
                    });
                let offers = array(snapshot, "offers").iter().map(|value| Entry {
                    key: "offer",
                    value,
                });
                active
                    .chain(draft)
                    .chain(revisions)
                    .chain(publications)
                    .chain(availability)
                    .chain(lane_impacts)
                    .chain(diagnostics)
                    .chain(offers)
                    .collect()
            }
        }
    }

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
            self.error = None;
            return;
        }
        if response.status == 409
            && response.body.get("error").and_then(Value::as_str) == Some("stale_draft")
        {
            let daemon_draft = response.body.get("draft").cloned().unwrap_or(Value::Null);
            let daemon_generation = response.body.get("generation").and_then(Value::as_u64);
            self.set_daemon_draft(daemon_generation, daemon_draft.clone());
            self.modal = Modal::Conflict {
                daemon_generation,
                daemon_draft,
            };
            self.error = None;
            return;
        }
        if response.status / 100 != 2 {
            // A refused edit is not a conflicted one: a later, unrelated
            // stale-draft refusal must not offer to reapply it.
            if command.name == "draft-edit" {
                self.conflicted_edits.clear();
            }
            self.error = Some(control_error(&command, &response));
            return;
        }
        match command.name {
            "draft-edit" | "draft-replace" | "validate" => {
                let validation = if command.name == "validate" {
                    Some(json!({
                        "kind": "validation",
                        "valid": response.body.get("valid"),
                        "diagnostics": response.body.get("diagnostics"),
                    }))
                } else {
                    None
                };
                self.receive_draft_reply(response.body);
                if let Some(validation) = validation {
                    self.modal = Modal::PreviewResult(validation);
                } else {
                    self.conflicted_edits.clear();
                    self.modal = Modal::None;
                }
            }
            "preview" => self.modal = Modal::PreviewResult(response.body),
            "activate" | "reactivate" => {
                self.merge_activation(response.body);
                self.conflicted_edits.clear();
                self.modal = Modal::None;
            }
            "promote" => self.modal = Modal::PreviewResult(response.body),
            _ => self.error = None,
        }
    }

    fn receive_snapshot(&mut self, snapshot: Value) {
        self.generation = generation(&snapshot);
        self.snapshot = Some(snapshot);
        self.error = None;
        self.clamp_selection();
    }

    fn receive_draft_reply(&mut self, body: Value) {
        let generation = body.get("generation").and_then(Value::as_u64);
        let draft = body.get("draft").cloned();
        if let Some(draft) = draft {
            self.set_daemon_draft(generation, draft);
        } else {
            self.generation = generation.or(self.generation);
        }
        self.error = None;
        self.clamp_selection();
    }

    fn merge_activation(&mut self, body: Value) {
        if let Some(snapshot) = &mut self.snapshot {
            for key in ["active_revision", "draft", "lane_impacts", "publications"] {
                if let Some(value) = body.get(key) {
                    snapshot[key] = value.clone();
                }
            }
            self.generation = body
                .get("generation")
                .and_then(Value::as_u64)
                .or(self.generation);
        }
        self.error = None;
    }

    fn set_daemon_draft(&mut self, generation: Option<u64>, draft: Value) {
        self.generation = generation.or(self.generation);
        if let Some(snapshot) = &mut self.snapshot {
            snapshot["draft"] = draft;
            if let Some(generation) = self.generation {
                snapshot["draft_generation"] = Value::from(generation);
            }
        } else {
            self.snapshot = Some(json!({
                "draft_generation": self.generation,
                "draft": draft,
            }));
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        let count = self.entries().len();
        self.selected = self.selected.min(count.saturating_sub(1));
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
            KeyCode::Char('m') if self.view == View::Routes => {
                self.open_candidate_order();
                None
            }
            KeyCode::Char('v') => {
                self.open_preview();
                None
            }
            KeyCode::Char('V') => self.validate(),
            KeyCode::Char('a') => {
                self.open_activate();
                None
            }
            KeyCode::Char('x') if self.view == View::Status => {
                self.open_reactivate();
                None
            }
            KeyCode::Char('p') => {
                self.open_promote();
                None
            }
            KeyCode::Char('i') if self.view == View::Status => {
                self.modal = Modal::Import;
                None
            }
            KeyCode::PageDown => {
                self.scroll_detail(DETAIL_PAGE);
                None
            }
            KeyCode::PageUp => {
                self.scroll_detail(-DETAIL_PAGE);
                None
            }
            _ => None,
        }
    }

    fn open_edit(&mut self) {
        let on_draft = self
            .entries()
            .get(self.selected)
            .is_some_and(|entry| entry.key == "draft");
        if on_draft {
            let description = draft_policy(self.snapshot.as_ref().unwrap_or(&Value::Null))
                .and_then(|policy| policy.get("description"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            self.modal = Modal::Description { value: description };
            return;
        }
        let selected = self
            .entries()
            .get(self.selected)
            .map(|entry| (entry.key.to_owned(), entry.value.clone()));
        if let Some((record_key, value)) = selected {
            if editable_key(&record_key) {
                self.modal = Modal::Record {
                    key: record_key,
                    value: pretty(&value),
                    adding: false,
                };
            } else {
                self.error = Some("this item is not a draft policy record".to_owned());
            }
        }
    }

    /// Start adding a record. A view that lists one kind goes straight to its
    /// template; one that lists several asks which kind first, defaulting to
    /// the kind under the cursor.
    fn open_add(&mut self) {
        let kinds = super::template::kinds(self.view);
        match kinds {
            [] => {
                self.error = Some("there is nothing to add in this view".to_owned());
            }
            [kind] => self.open_template(kind),
            _ => {
                let preselected = self
                    .entries()
                    .get(self.selected)
                    .and_then(|entry| kinds.iter().position(|kind| *kind == entry.key))
                    .unwrap_or(0);
                self.modal = Modal::NewKind { kinds, preselected };
            }
        }
    }

    fn open_template(&mut self, kind: &'static str) {
        let route_id = self
            .snapshot
            .as_ref()
            .and_then(draft_policy)
            .and_then(|policy| array(policy, "routes").first())
            .and_then(|route| route.get("id"))
            .and_then(Value::as_str);
        self.modal = Modal::Record {
            key: kind.to_owned(),
            value: pretty(&super::template::template(kind, route_id)),
            adding: true,
        };
    }

    fn open_remove(&mut self) {
        let selected = self
            .entries()
            .get(self.selected)
            .map(|entry| (entry.key.to_owned(), entry.value.clone()));
        if let Some((record_key, value)) = selected.filter(|(key, _)| editable_key(key)) {
            self.modal = Modal::Remove {
                key: record_key,
                value,
            };
        }
    }

    fn open_candidate_order(&mut self) {
        let selected = self
            .entries()
            .get(self.selected)
            .filter(|entry| entry.key == "routes")
            .map(|entry| entry.value.clone());
        if let Some(route) = selected {
            let count = candidate_count(&route);
            if count > 1 {
                self.modal = Modal::CandidateOrder { route, selected: 0 };
            } else {
                self.error = Some("a route needs at least two candidates to reorder".to_owned());
            }
        }
    }

    fn open_preview(&mut self) {
        self.modal = Modal::Preview {
            field: 0,
            values: vec![
                "anthropic_messages".to_owned(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ],
        };
    }

    fn open_activate(&mut self) {
        self.modal = Modal::Activate {
            scope: 0,
            force_cache_transition: false,
        };
    }

    fn open_reactivate(&mut self) {
        let revision_id = self
            .entries()
            .get(self.selected)
            .and_then(|entry| entry.value.get("revision_id"))
            .and_then(Value::as_i64)
            .or_else(|| {
                self.revisions
                    .first()
                    .and_then(|revision| revision.get("revision_id"))
                    .and_then(Value::as_i64)
            });
        if let Some(revision_id) = revision_id {
            self.modal = Modal::Reactivate {
                revision_id,
                scope: 0,
                force_cache_transition: false,
            };
        } else {
            self.error = Some("refresh revisions, then choose a revision to reactivate".to_owned());
        }
    }

    /// Fill the binding and model from the next daemon offer after the one
    /// already entered (or the first), so a promotion names an offer that
    /// exists rather than a typed guess.
    fn complete_offer(&self, binding: &mut String, model: &mut String) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let offers = array(snapshot, "offers");
        let current = offers.iter().position(|offer| {
            offer.get("binding_id").and_then(Value::as_str) == Some(binding.as_str())
                && offer.get("model_id").and_then(Value::as_str) == Some(model.as_str())
        });
        let next = current.map_or(0, |index| index + 1);
        if let Some(offer) = offers.get(next % offers.len().max(1)) {
            let text = |key: &str| offer.get(key).and_then(Value::as_str).unwrap_or_default();
            *binding = text("binding_id").to_owned();
            *model = text("model_id").to_owned();
        }
    }

    fn open_promote(&mut self) {
        self.modal = Modal::Promote {
            field: 0,
            binding: String::new(),
            model: String::new(),
        };
    }

    fn modal_key(&mut self, key: KeyEvent) -> Option<Command> {
        let modal = std::mem::take(&mut self.modal);
        let (modal, action) = match modal {
            Modal::None => (Modal::None, ModalAction::None),
            Modal::Description { mut value } => {
                if key.code == KeyCode::Esc {
                    return None;
                }
                if text_key(&mut value, key) {
                    let description = if value.is_empty() {
                        Value::Null
                    } else {
                        Value::String(value.clone())
                    };
                    (
                        Modal::Description { value },
                        ModalAction::Edit(vec![json!({
                            "kind": "set_description", "description": description,
                        })]),
                    )
                } else {
                    (Modal::Description { value }, ModalAction::None)
                }
            }
            Modal::Record {
                key: record_key,
                mut value,
                adding,
            } => {
                let mut action = ModalAction::None;
                if key.code == KeyCode::Esc {
                    return None;
                } else if key.code == KeyCode::Enter
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    match serde_json::from_str::<Value>(&value) {
                        Ok(record) => {
                            action = ModalAction::Edit(vec![if adding {
                                add_edit(&record_key, record)
                            } else {
                                replace_edit(&record_key, record)
                            }]);
                        }
                        Err(error) => self.error = Some(format!("record JSON: {error}")),
                    }
                } else {
                    text_key(&mut value, key);
                }
                (
                    Modal::Record {
                        key: record_key,
                        value,
                        adding,
                    },
                    action,
                )
            }
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
            Modal::CandidateOrder {
                mut route,
                mut selected,
            } => {
                let count = candidate_count(&route);
                let action = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                        ModalAction::None
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(count.saturating_sub(1));
                        ModalAction::None
                    }
                    KeyCode::Char('h') => {
                        move_candidate(&mut route, selected, -1);
                        ModalAction::None
                    }
                    KeyCode::Char('l') => {
                        move_candidate(&mut route, selected, 1);
                        ModalAction::None
                    }
                    KeyCode::Enter | KeyCode::Char('y') => {
                        ModalAction::Edit(vec![replace_edit("routes", route.clone())])
                    }
                    KeyCode::Esc => return None,
                    _ => ModalAction::None,
                };
                (Modal::CandidateOrder { route, selected }, action)
            }
            Modal::Preview {
                mut field,
                mut values,
            } => {
                let action = match key.code {
                    KeyCode::Tab | KeyCode::Down => {
                        field = (field + 1) % values.len();
                        ModalAction::None
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        field = (field + values.len() - 1) % values.len();
                        ModalAction::None
                    }
                    KeyCode::Enter => ModalAction::Preview(values.clone()),
                    KeyCode::Esc => return None,
                    _ => {
                        text_key(&mut values[field], key);
                        ModalAction::None
                    }
                };
                (Modal::Preview { field, values }, action)
            }
            Modal::PreviewResult(_) => (Modal::None, ModalAction::None),
            Modal::Activate {
                mut scope,
                mut force_cache_transition,
            } => {
                let action = match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        scope = scope.saturating_sub(1);
                        ModalAction::None
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        scope = (scope + 1).min(2);
                        ModalAction::None
                    }
                    KeyCode::Char('f') => {
                        force_cache_transition = !force_cache_transition;
                        ModalAction::None
                    }
                    KeyCode::Char('y') | KeyCode::Enter => ModalAction::Activate {
                        scope,
                        force: force_cache_transition,
                    },
                    KeyCode::Esc | KeyCode::Char('n') => return None,
                    _ => ModalAction::None,
                };
                (
                    Modal::Activate {
                        scope,
                        force_cache_transition,
                    },
                    action,
                )
            }
            Modal::Reactivate {
                revision_id,
                mut scope,
                mut force_cache_transition,
            } => {
                let action = match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        scope = scope.saturating_sub(1);
                        ModalAction::None
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        scope = (scope + 1).min(2);
                        ModalAction::None
                    }
                    KeyCode::Char('f') => {
                        force_cache_transition = !force_cache_transition;
                        ModalAction::None
                    }
                    KeyCode::Char('y') | KeyCode::Enter => ModalAction::Reactivate {
                        revision_id,
                        scope,
                        force: force_cache_transition,
                    },
                    KeyCode::Esc | KeyCode::Char('n') => return None,
                    _ => ModalAction::None,
                };
                (
                    Modal::Reactivate {
                        revision_id,
                        scope,
                        force_cache_transition,
                    },
                    action,
                )
            }
            Modal::Promote {
                mut field,
                mut binding,
                mut model,
            } => {
                let action = match key.code {
                    KeyCode::Tab | KeyCode::Down => {
                        field = (field + 1) % 2;
                        ModalAction::None
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        field = (field + 1) % 2;
                        ModalAction::None
                    }
                    KeyCode::Enter => ModalAction::Promote {
                        binding: binding.clone(),
                        model: model.clone(),
                    },
                    KeyCode::Esc => return None,
                    KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.complete_offer(&mut binding, &mut model);
                        ModalAction::None
                    }
                    _ => {
                        text_key(if field == 0 { &mut binding } else { &mut model }, key);
                        ModalAction::None
                    }
                };
                (
                    Modal::Promote {
                        field,
                        binding,
                        model,
                    },
                    action,
                )
            }
            Modal::Import => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => (Modal::Import, ModalAction::Import),
                KeyCode::Esc | KeyCode::Char('n') => return None,
                _ => (Modal::Import, ModalAction::None),
            },
            Modal::Conflict {
                daemon_generation,
                daemon_draft,
            } => match key.code {
                KeyCode::Char('r') => {
                    self.conflicted_edits.clear();
                    self.modal = Modal::None;
                    return Some(Command::snapshot());
                }
                KeyCode::Char('a') => (
                    Modal::Conflict {
                        daemon_generation,
                        daemon_draft,
                    },
                    ModalAction::Reapply,
                ),
                KeyCode::Esc => return None,
                _ => (
                    Modal::Conflict {
                        daemon_generation,
                        daemon_draft,
                    },
                    ModalAction::None,
                ),
            },
        };
        self.modal = modal;
        match action {
            ModalAction::None => None,
            ModalAction::Edit(edits) => self.edit(edits),
            ModalAction::Preview(values) => self.preview(&values),
            ModalAction::Activate { scope, force } => self.activate(scope, force),
            ModalAction::Reactivate {
                revision_id,
                scope,
                force,
            } => self.reactivate(revision_id, scope, force),
            ModalAction::Promote { binding, model } => self.promote(&binding, &model),
            ModalAction::Import => self.import(),
            ModalAction::Reapply => self.reapply_conflict(),
        }
    }

    /// Replace the draft with the compiled legacy config. A config that does
    /// not load or compile is reported and leaves the draft alone.
    fn import(&mut self) -> Option<Command> {
        let generation = self.generation?;
        match (self.legacy_policy)() {
            Ok(policy) => {
                self.in_flight = true;
                Some(command(
                    "draft-replace",
                    Method::PUT,
                    "draft",
                    "routing-draft-replace",
                    json!({ "expected_generation": generation, "policy": policy }),
                ))
            }
            Err(error) => {
                self.modal = Modal::None;
                self.error = Some(format!("import: {error:#}"));
                None
            }
        }
    }

    fn edit(&mut self, edits: Vec<Value>) -> Option<Command> {
        let generation = self.generation?;
        self.conflicted_edits = edits.clone();
        self.in_flight = true;
        Some(command(
            "draft-edit",
            Method::PATCH,
            "draft",
            "routing-draft-edit",
            json!({ "expected_generation": generation, "edit": { "operations": edits } }),
        ))
    }

    fn reapply_conflict(&mut self) -> Option<Command> {
        if self.conflicted_edits.is_empty() {
            self.error = Some("there is no local edit to reapply".to_owned());
            return None;
        }
        let generation = self.generation?;
        self.in_flight = true;
        Some(command(
            "draft-edit",
            Method::PATCH,
            "draft",
            "routing-draft-edit",
            json!({ "expected_generation": generation, "edit": { "operations": self.conflicted_edits } }),
        ))
    }

    fn validate(&mut self) -> Option<Command> {
        self.in_flight = true;
        Some(command(
            "validate",
            Method::POST,
            "validate",
            "routing-validate",
            json!({ "expected_generation": self.generation }),
        ))
    }

    fn preview(&mut self, values: &[String]) -> Option<Command> {
        self.in_flight = true;
        let optional_number = |index: usize| values[index].parse::<u64>().ok();
        let mut body = json!({
            "expected_generation": self.generation,
            "protocol": values[0],
        });
        for (key, index) in [("profile", 1), ("model", 2), ("lane_key", 3)] {
            if !values[index].is_empty() {
                body[key] = Value::String(values[index].clone());
            }
        }
        for (key, index) in [("prompt_bound", 4), ("message_count", 5), ("tool_count", 6)] {
            if let Some(value) = optional_number(index) {
                body[key] = Value::from(value);
            }
        }
        Some(command(
            "preview",
            Method::POST,
            "preview",
            "routing-preview",
            body,
        ))
    }

    fn activate(&mut self, scope: usize, force: bool) -> Option<Command> {
        let generation = self.generation?;
        self.in_flight = true;
        Some(command(
            "activate",
            Method::POST,
            "activate",
            "routing-activate",
            json!({
                "expected_generation": generation,
                "scope": activation_scope(scope),
                "force_cache_transition": force,
            }),
        ))
    }

    fn reactivate(&mut self, revision_id: i64, scope: usize, force: bool) -> Option<Command> {
        let generation = self.generation?;
        self.in_flight = true;
        Some(command(
            "reactivate",
            Method::POST,
            "reactivate",
            "routing-reactivate",
            json!({
                "expected_generation": generation,
                "revision_id": revision_id,
                "scope": activation_scope(scope),
                "force_cache_transition": force,
            }),
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
    Preview(Vec<String>),
    Activate {
        scope: usize,
        force: bool,
    },
    Reactivate {
        revision_id: i64,
        scope: usize,
        force: bool,
    },
    Promote {
        binding: String,
        model: String,
    },
    Import,
    Reapply,
}

/// One content-free item supplied by the snapshot.
#[derive(Debug, Clone, Copy)]
pub(super) struct Entry<'a> {
    pub(super) key: &'a str,
    pub(super) value: &'a Value,
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

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn active_policy(snapshot: &Value) -> Option<&Value> {
    snapshot
        .get("active_revision")
        .and_then(|revision| revision.get("policy"))
}

fn draft_policy(snapshot: &Value) -> Option<&Value> {
    let draft = snapshot.get("draft")?;
    Some(draft.get("policy").unwrap_or(draft))
}

fn generation(snapshot: &Value) -> Option<u64> {
    snapshot
        .get("draft_generation")
        .and_then(Value::as_u64)
        .or_else(|| {
            snapshot
                .get("draft")
                .and_then(|draft| draft.get("generation"))
                .and_then(Value::as_u64)
        })
        .or_else(|| snapshot.get("generation").and_then(Value::as_u64))
}

fn editable_key(key: &str) -> bool {
    matches!(
        key,
        "presentations"
            | "routes"
            | "defaults"
            | "rewrites"
            | "classifiers"
            | "upgrades"
            | "compaction"
    )
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

/// A default's identity is the scope it covers, as the daemon addresses it.
pub(super) fn scope_label(record: &Value) -> String {
    let scope = record.get("scope");
    let text = |key: &str| {
        scope
            .and_then(|scope| scope.get(key))
            .and_then(Value::as_str)
            .unwrap_or("?")
    };
    match scope
        .and_then(|scope| scope.get("kind"))
        .and_then(Value::as_str)
    {
        Some("profile") => format!("{}/{}", text("profile"), text("protocol")),
        _ => text("protocol").to_owned(),
    }
}

/// The route edges whose crossing from subscription to billed spend, or the
/// consent that allows it, differ between the draft and the active revision.
/// Spend is the one thing an activation can change that the changed-record
/// list does not make plain.
fn cost_boundary_changes(snapshot: &Value) -> Vec<String> {
    let Some(derived) = snapshot.get("derived") else {
        return Vec::new();
    };
    let edges = |key: &str| -> Vec<(String, &Value)> {
        derived
            .get(key)
            .map(|policy| array(policy, "routes"))
            .unwrap_or_default()
            .iter()
            .flat_map(|route| {
                let route_id = route.get("id").and_then(Value::as_str).unwrap_or("?");
                array(route, "edges").iter().map(move |edge| {
                    let text = |name: &str| edge.get(name).and_then(Value::as_str).unwrap_or("?");
                    (
                        format!("{route_id}: {} -> {}", text("from"), text("to")),
                        edge,
                    )
                })
            })
            .collect()
    };
    let (active, draft) = (edges("active"), edges("draft"));
    let crosses = |edge: &Value| edge.get("crosses_cost_boundary") == Some(&Value::Bool(true));
    let consent = |edge: &Value| {
        edge.get("billed_boundary")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned()
    };
    let mut lines = Vec::new();
    for (name, edge) in &draft {
        let before = active
            .iter()
            .find(|(active_name, _)| active_name == name)
            .map(|(_, edge)| *edge);
        match (before.is_some_and(crosses), crosses(edge)) {
            (false, true) => lines.push(format!(
                "cost boundary: {name} now crosses {} to {} (consent {})",
                edge["from_cost"].as_str().unwrap_or("?"),
                edge["to_cost"].as_str().unwrap_or("?"),
                consent(edge),
            )),
            (true, true) if before.map(consent) != Some(consent(edge)) => {
                lines.push(format!(
                    "cost boundary: {name} consent {} (was {})",
                    consent(edge),
                    before.map(consent).unwrap_or_default(),
                ));
            }
            (true, false) => lines.push(format!("cost boundary: {name} no longer crosses")),
            _ => {}
        }
    }
    for (name, edge) in &active {
        if crosses(edge) && !draft.iter().any(|(draft_name, _)| draft_name == name) {
            lines.push(format!("cost boundary: {name} removed"));
        }
    }
    lines
}

/// The records the draft adds (`+`), removes (`-`) or changes (`~`) relative
/// to the active revision, by kind and id.
fn changed_records(snapshot: &Value) -> Vec<String> {
    let (Some(draft), active) = (draft_policy(snapshot), active_policy(snapshot)) else {
        return Vec::new();
    };
    let mut changes = Vec::new();
    for key in [
        "presentations",
        "routes",
        "defaults",
        "rewrites",
        "classifiers",
        "upgrades",
        "compaction",
    ] {
        let by_id = |document: Option<&Value>| -> Vec<(String, Value)> {
            // Compaction is one optional record, so it has no id to match by.
            if key == "compaction" {
                return document
                    .and_then(|document| document.get("compaction"))
                    .filter(|record| !record.is_null())
                    .map(|record| (String::new(), record.clone()))
                    .into_iter()
                    .collect();
            }
            document
                .map(|document| array(document, key))
                .unwrap_or(&[])
                .iter()
                .map(|record| {
                    if key == "defaults" {
                        return (scope_label(record), record.clone());
                    }
                    let id = record
                        .get("id")
                        .or_else(|| record.get("slug"))
                        .map(|id| id.as_str().map_or_else(|| id.to_string(), str::to_owned))
                        .unwrap_or_default();
                    (id, record.clone())
                })
                .collect()
        };
        let before = by_id(active);
        let after = by_id(Some(draft));
        let kind = if key == "compaction" {
            key
        } else {
            key.trim_end_matches('s')
        };
        for (id, record) in &after {
            match before.iter().find(|(prior, _)| prior == id) {
                None => changes.push(format!("+{kind} {id}").trim_end().to_owned()),
                Some((_, prior)) if prior != record => {
                    changes.push(format!("~{kind} {id}").trim_end().to_owned())
                }
                Some(_) => {}
            }
        }
        for (id, _) in &before {
            if !after.iter().any(|(next, _)| next == id) {
                changes.push(format!("-{kind} {id}").trim_end().to_owned());
            }
        }
    }
    changes
}

fn candidate_count(route: &Value) -> usize {
    route
        .get("chain")
        .map(|chain| 1 + array(chain, "steps").len())
        .unwrap_or(0)
}

/// Reorders the candidates while every transition edge keeps its position in
/// the chain.  The first candidate has no incoming edge, so a candidate moved
/// to or from the front cannot carry one with it; keeping edges positional
/// means every step always has the edge the daemon's step record requires,
/// and the daemon stays the validator of whether the result is acceptable.
fn move_candidate(route: &mut Value, selected: usize, delta: isize) {
    let Some(chain) = route.get_mut("chain").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(steps) = chain.get("steps").and_then(Value::as_array) else {
        return;
    };
    let Some(first) = chain.get("first") else {
        return;
    };
    let mut candidates = vec![first.clone()];
    let mut edges = Vec::with_capacity(steps.len());
    for step in steps {
        edges.push(step.get("edge").cloned().unwrap_or(Value::Null));
        candidates.push(step.get("candidate").cloned().unwrap_or(Value::Null));
    }
    let destination = selected
        .saturating_add_signed(delta)
        .min(candidates.len().saturating_sub(1));
    if selected >= candidates.len() || destination == selected {
        return;
    }
    let candidate = candidates.remove(selected);
    candidates.insert(destination, candidate);
    let mut candidates = candidates.into_iter();
    let first = candidates.next().unwrap_or(Value::Null);
    let steps = edges
        .into_iter()
        .zip(candidates)
        .map(|(edge, candidate)| json!({ "edge": edge, "candidate": candidate }))
        .collect();
    chain.insert("first".to_owned(), first);
    chain.insert("steps".to_owned(), Value::Array(steps));
}

fn text_key(value: &mut String, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => true,
        KeyCode::Esc => false,
        KeyCode::Backspace => {
            value.pop();
            false
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            value.push(character);
            false
        }
        _ => false,
    }
}

fn activation_scope(index: usize) -> &'static str {
    ["new_lanes", "new_and_cold_lanes", "immediate"][index.min(2)]
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned())
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
    use super::{Modal, State, View, candidate_count, move_candidate};
    use crate::tui::routing::client::{Command, Reply, Response};
    use reqwest::Method;

    fn snapshot() -> serde_json::Value {
        serde_json::json!({
            "draft_generation": 7,
            "draft": {"policy": {"description": "draft", "routes": [{
                "id": "route", "chain": {"first": {"id": "one"}, "steps": [
                    {"edge": {"preflight": "enabled"}, "candidate": {"id": "two"}}
                ]}
            }]}},
            "active_revision": {"policy": {"routes": []}}
        })
    }

    #[test]
    fn draft_entries_and_generation_win_over_the_active_revision() {
        let mut state = State::default();
        state.receive(Reply::Command(
            Command::snapshot(),
            Ok(Response {
                status: 200,
                body: snapshot(),
            }),
        ));
        state.set_view(View::Routes);
        assert_eq!(state.generation(), Some(7));
        assert_eq!(state.entries()[0].value["id"], "route");
    }

    #[test]
    fn conflict_preserves_edits_and_offers_reapply() {
        let mut state = State::default();
        state.receive(Reply::Command(
            Command::snapshot(),
            Ok(Response {
                status: 200,
                body: snapshot(),
            }),
        ));
        let command = state.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('V'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(command.expect("validation command").name, "validate");
        state.conflicted_edits =
            vec![serde_json::json!({"kind": "set_description", "description": "mine"})];
        state.receive(reply(
            "draft-edit",
            409,
            serde_json::json!({"error": "stale_draft", "generation": 8, "draft": {"policy": {"description": "theirs"}}}),
        ));
        assert!(matches!(state.modal(), Modal::Conflict { .. }));
        let command = state.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let body = command.expect("reapply command").body.expect("body");
        assert_eq!(body["expected_generation"], 8);
        assert_eq!(body["edit"]["operations"][0]["description"], "mine");
    }

    #[test]
    fn candidate_reordering_keeps_every_explicit_transition_record() {
        let mut route = snapshot()["draft"]["policy"]["routes"][0].clone();
        assert_eq!(candidate_count(&route), 2);
        move_candidate(&mut route, 1, -1);
        assert_eq!(route["chain"]["first"]["id"], "two");
        assert_eq!(route["chain"]["steps"][0]["candidate"]["id"], "one");
        assert_eq!(
            route["chain"]["steps"][0]["edge"],
            serde_json::json!({"preflight": "enabled"}),
            "every step keeps an edge, so the daemon can read the route back"
        );
    }

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(state: &mut State, code: KeyCode) -> Option<Command> {
        state.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn typed(state: &mut State, text: &str) {
        for character in text.chars() {
            assert!(press(state, KeyCode::Char(character)).is_none());
        }
    }

    fn loaded(body: serde_json::Value) -> State {
        let mut state = State::default();
        state.receive(Reply::Command(
            Command::snapshot(),
            Ok(Response { status: 200, body }),
        ));
        state
    }

    fn reply(name: &'static str, status: u16, body: serde_json::Value) -> Reply {
        let mut command = Command::snapshot();
        command.name = name;
        Reply::Command(command, Ok(Response { status, body }))
    }

    fn full_snapshot() -> serde_json::Value {
        serde_json::json!({
            "draft_generation": 3,
            "draft": {"policy": {
                "description": "draft",
                "presentations": [{"id": "kept", "slug": "k"}, {"id": "added", "slug": "a"}],
                "routes": [{"id": "changed", "chain": {"first": {"id": "b"}, "steps": []}}],
            }},
            "active_revision": {"policy": {
                "presentations": [{"id": "kept", "slug": "k"}, {"id": "gone", "slug": "g"}],
                "routes": [{"id": "changed", "chain": {"first": {"id": "a"}, "steps": []}}],
            }},
            "draft_diagnostics": [{"code": "unused_binding", "severity": "warning", "path": "/routes/0"}],
            "lane_impacts": {"total": 5, "unpinned": 1, "warm": 2, "cold": 1, "unknown": 1},
            "offers": [
                {"binding_id": "openrouter.chat", "model_id": "vendor/one"},
                {"binding_id": "openrouter.chat", "model_id": "vendor/two"},
            ],
            "publications": [{"adapter": "claude", "state": "applied"}],
        })
    }

    #[test]
    fn views_wrap_and_selection_clamps_without_a_snapshot() {
        let mut state = State::default();
        state.move_selection(5);
        assert_eq!(state.selected(), 0);
        state.move_view(-1);
        assert_eq!(state.view(), View::Status);
        state.move_view(1);
        assert_eq!(state.view(), View::Presentations);
        let mut state = loaded(full_snapshot());
        state.move_selection(9);
        assert_eq!(state.selected(), 1, "clamped to the last presentation");
        state.set_view(View::Routes);
        assert_eq!(state.selected(), 0, "a view change resets the selection");
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_snapshot_readable() {
        let mut state = loaded(full_snapshot());
        assert!(state.begin_refresh());
        assert!(!state.begin_refresh(), "one refresh at a time");
        state.receive(Reply::Command(Command::snapshot(), Err("down".to_owned())));
        assert!(state.error().is_some_and(|error| error.contains("down")));
        assert_eq!(state.entries().len(), 2);
        assert!(state.begin_refresh(), "refresh can resume once it settled");
    }

    #[test]
    fn status_view_lists_diagnostics_lane_impact_and_offers() {
        let mut state = loaded(full_snapshot());
        state.set_view(View::Status);
        let keys: Vec<_> = state.entries().iter().map(|entry| entry.key).collect();
        assert_eq!(
            keys,
            [
                "active_revision",
                "draft",
                "publications",
                "lane_impacts",
                "diagnostic",
                "offer",
                "offer"
            ]
        );
    }

    #[test]
    fn activation_waits_for_confirmation_and_carries_scope_and_force() {
        let mut state = loaded(full_snapshot());
        assert!(press(&mut state, KeyCode::Char('a')).is_none());
        assert!(matches!(state.modal(), Modal::Activate { .. }));
        press(&mut state, KeyCode::Char('l'));
        press(&mut state, KeyCode::Char('f'));
        let command = press(&mut state, KeyCode::Char('y')).expect("confirmed");
        assert_eq!(
            (command.name, command.control),
            ("activate", "routing-activate")
        );
        let body = command.body.expect("body");
        assert_eq!(body["expected_generation"], 3);
        assert_eq!(body["scope"], "new_and_cold_lanes");
        assert_eq!(body["force_cache_transition"], true);
        assert!(state.in_flight());
    }

    #[test]
    fn import_waits_for_confirmation_then_replaces_the_draft_at_the_loaded_generation() {
        let mut state = loaded(full_snapshot());
        state.legacy_policy = || Ok(serde_json::json!({"format_version": 3, "marker": "legacy"}));
        state.set_view(View::Status);
        assert!(press(&mut state, KeyCode::Char('i')).is_none());
        assert!(matches!(state.modal(), Modal::Import));
        let command = press(&mut state, KeyCode::Char('y')).expect("confirmed");
        assert_eq!(
            (command.name, command.control),
            ("draft-replace", "routing-draft-replace")
        );
        assert_eq!(command.method, Method::PUT);
        let body = command.body.expect("body");
        assert_eq!(body["expected_generation"], 3);
        assert_eq!(body["policy"]["marker"], "legacy");
        assert!(state.in_flight());
    }

    #[test]
    fn import_is_offered_only_in_the_status_view_and_cancels_cleanly() {
        let mut state = loaded(full_snapshot());
        press(&mut state, KeyCode::Char('i'));
        assert!(!state.modal().active(), "not in the presentations view");
        state.set_view(View::Status);
        press(&mut state, KeyCode::Char('i'));
        assert!(press(&mut state, KeyCode::Esc).is_none());
        assert!(!state.modal().active());
    }

    #[test]
    fn a_config_that_cannot_be_imported_leaves_the_draft_alone() {
        let mut state = loaded(full_snapshot());
        state.legacy_policy = || Err(anyhow::anyhow!("toker.toml does not load"));
        state.set_view(View::Status);
        press(&mut state, KeyCode::Char('i'));
        assert!(press(&mut state, KeyCode::Char('y')).is_none());
        assert!(!state.modal().active());
        assert!(!state.in_flight());
        assert!(
            state
                .error()
                .is_some_and(|error| error.contains("toker.toml does not load")),
            "{:?}",
            state.error()
        );
        assert_eq!(state.generation(), Some(3));
    }

    #[test]
    fn an_import_against_a_stale_draft_takes_the_conflict_path() {
        let mut state = loaded(full_snapshot());
        state.legacy_policy = || Ok(serde_json::json!({}));
        state.set_view(View::Status);
        press(&mut state, KeyCode::Char('i'));
        press(&mut state, KeyCode::Char('y'));
        state.receive(reply(
            "draft-replace",
            409,
            serde_json::json!({"error": "stale_draft", "generation": 4, "draft": {"policy": {"description": "theirs"}}}),
        ));
        assert!(matches!(state.modal(), Modal::Conflict { .. }));
        assert_eq!(state.generation(), Some(4), "the daemon draft is kept");
    }

    #[test]
    fn escape_cancels_every_confirmation_without_a_command() {
        let mut state = loaded(full_snapshot());
        for key in ['a', 'd', 'p', 'v'] {
            press(&mut state, KeyCode::Char(key));
            assert!(state.modal().active(), "{key} opens a form");
            assert!(press(&mut state, KeyCode::Esc).is_none());
            assert!(!state.modal().active(), "{key} closes on Esc");
        }
        press(&mut state, KeyCode::Char('e'));
        assert!(matches!(state.modal(), Modal::Record { .. }));
        press(&mut state, KeyCode::Esc);
        assert!(!state.modal().active());
        state.set_view(View::Status);
        // Entry 0 is the active revision; the draft follows it.
        state.move_selection(1);
        press(&mut state, KeyCode::Char('e'));
        assert!(matches!(state.modal(), Modal::Description { .. }));
        press(&mut state, KeyCode::Esc);
        assert!(
            !state.modal().active(),
            "the description form closes on Esc"
        );
    }

    #[test]
    fn removal_needs_a_yes_and_names_the_record() {
        let mut state = loaded(full_snapshot());
        press(&mut state, KeyCode::Char('d'));
        assert!(
            press(&mut state, KeyCode::Char('x')).is_none(),
            "other keys do nothing"
        );
        let command = press(&mut state, KeyCode::Char('y')).expect("confirmed");
        let operation = &command.body.expect("body")["edit"]["operations"][0];
        assert_eq!(operation["kind"], "remove_presentation");
        assert_eq!(operation["id"], "kept");
    }

    #[test]
    fn a_record_edit_sends_the_typed_replacement_and_rejects_bad_json() {
        let mut state = loaded(full_snapshot());
        state.set_view(View::Routes);
        press(&mut state, KeyCode::Char('e'));
        let Modal::Record { value, .. } = state.modal().clone() else {
            panic!("record form");
        };
        assert!(value.contains("\"changed\""));
        // Corrupt the JSON: the form stays open, shows why, sends nothing.
        typed(&mut state, "}");
        let ctrl_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        assert!(state.key(ctrl_enter).is_none());
        assert!(
            state
                .error()
                .is_some_and(|error| error.starts_with("record JSON"))
        );
        assert!(matches!(state.modal(), Modal::Record { .. }));
        // A backspace restores valid JSON, which saves as a typed replace.
        press(&mut state, KeyCode::Backspace);
        let command = state.key(ctrl_enter).expect("edit command");
        assert_eq!(command.method, reqwest::Method::PATCH);
        let operation = &command.body.expect("body")["edit"]["operations"][0];
        assert_eq!(operation["kind"], "replace_route");
        assert_eq!(operation["id"], "changed");
    }

    #[test]
    fn keys_are_ignored_while_a_command_is_in_flight() {
        let mut state = loaded(full_snapshot());
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Char('y'));
        assert!(state.in_flight());
        assert!(press(&mut state, KeyCode::Char('y')).is_none());
        assert!(press(&mut state, KeyCode::Char('V')).is_none());
        state.receive(reply("activate", 200, serde_json::json!({"generation": 4})));
        assert!(!state.in_flight());
    }

    #[test]
    fn activation_summary_names_changes_lane_impact_and_publications() {
        let state = loaded(full_snapshot());
        let summary = state.activation_summary().join("\n");
        assert!(summary.contains("+presentation added"), "{summary}");
        assert!(summary.contains("-presentation gone"), "{summary}");
        assert!(summary.contains("~route changed"), "{summary}");
        assert!(!summary.contains("kept"), "{summary}");
        assert!(
            summary.contains("lanes: 5 total, 1 unpinned, 2 warm, 1 cold, 1 unknown"),
            "{summary}"
        );
        assert!(summary.contains("draft diagnostics: 1"), "{summary}");
        assert!(summary.contains("publication claude: applied"), "{summary}");
        assert_eq!(
            State::default().activation_summary(),
            ["no daemon snapshot loaded"]
        );
    }

    #[test]
    fn a_refused_activation_keeps_the_form_open_with_the_reason() {
        let mut state = loaded(full_snapshot());
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Char('y'));
        state.receive(reply(
            "activate",
            409,
            serde_json::json!({"error": "forced_cache_transition_required"}),
        ));
        assert!(matches!(state.modal(), Modal::Activate { .. }));
        assert!(
            state
                .error()
                .is_some_and(|error| error.contains("forced_cache_transition_required"))
        );
        // Toggling the force and confirming again resubmits.
        press(&mut state, KeyCode::Char('f'));
        let command = press(&mut state, KeyCode::Char('y')).expect("resubmitted");
        assert_eq!(command.body.expect("body")["force_cache_transition"], true);
    }

    #[test]
    fn an_invalid_activation_names_its_diagnostics() {
        let mut state = loaded(full_snapshot());
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Char('y'));
        state.receive(reply(
            "activate",
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
        let mut state = loaded(full_snapshot());
        state.receive(reply(
            "revisions",
            200,
            serde_json::json!({"revisions": [{"revision_id": 9}, {"revision_id": 4}]}),
        ));
        state.set_view(View::Status);
        // Entries: active, draft, then the revisions.
        state.move_selection(3);
        assert!(press(&mut state, KeyCode::Char('x')).is_none());
        assert!(matches!(
            state.modal(),
            Modal::Reactivate { revision_id: 4, .. }
        ));
        let command = press(&mut state, KeyCode::Char('y')).expect("confirmed");
        let body = command.body.expect("body");
        assert_eq!(command.name, "reactivate");
        assert_eq!(body["revision_id"], 4);
        assert_eq!(body["force_cache_transition"], false);
    }

    #[test]
    fn promotion_needs_a_binding_and_a_model_and_completes_from_offers() {
        let mut state = loaded(full_snapshot());
        press(&mut state, KeyCode::Char('p'));
        assert!(press(&mut state, KeyCode::Enter).is_none());
        assert!(
            state
                .error()
                .is_some_and(|error| error.contains("both required"))
        );
        let complete = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(state.key(complete).is_none());
        let Modal::Promote { binding, model, .. } = state.modal().clone() else {
            panic!("promote form");
        };
        assert_eq!(
            (binding.as_str(), model.as_str()),
            ("openrouter.chat", "vendor/one")
        );
        // A second completion moves to the next offer.
        state.key(complete);
        let command = press(&mut state, KeyCode::Enter).expect("promotion");
        let body = command.body.expect("body");
        assert_eq!(body["binding_id"], "openrouter.chat");
        assert_eq!(body["model_id"], "vendor/two");
    }

    #[test]
    fn a_conflict_can_reload_and_discards_the_local_batch() {
        let mut state = loaded(full_snapshot());
        state.conflicted_edits =
            vec![serde_json::json!({"kind": "set_description", "description": "mine"})];
        state.receive(reply(
            "draft-edit",
            409,
            serde_json::json!({"error": "stale_draft", "generation": 4,
                "draft": {"policy": {"description": "theirs"}}}),
        ));
        assert_eq!(state.generation(), Some(4), "the daemon copy is shown");
        let command = press(&mut state, KeyCode::Char('r')).expect("reload");
        assert_eq!(command.name, "snapshot");
        assert!(state.conflicted_edits.is_empty());
        assert!(!state.modal().active());
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
        let mut state = loaded(full_snapshot());
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Char('y'));
        assert!(state.in_flight());
        // The page's earlier snapshot and revisions reads land first.
        state.receive(reply(
            "revisions",
            200,
            serde_json::json!({"revisions": []}),
        ));
        state.receive(reply("snapshot", 200, full_snapshot()));
        assert!(state.in_flight());
        assert!(press(&mut state, KeyCode::Char('y')).is_none());
        state.receive(reply("activate", 200, serde_json::json!({"generation": 4})));
        assert!(!state.in_flight());
    }

    #[test]
    fn a_refused_queue_slot_releases_the_busy_guards() {
        let mut state = loaded(full_snapshot());
        assert!(state.begin_refresh());
        state.refresh_refused();
        assert!(state.begin_refresh(), "a refused refresh can be retried");
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Char('y'));
        state.command_refused("activate");
        assert!(!state.in_flight());
        assert!(state.error().is_some_and(|error| error.contains("busy")));
    }

    fn defaults_snapshot() -> serde_json::Value {
        serde_json::json!({
            "draft_generation": 5,
            "draft": {"policy": {
                "routes": [{"id": "route-a"}],
                "defaults": [
                    {"scope": {"kind": "protocol", "protocol": "openai_chat"},
                     "target": {"route_id": "route-a"}},
                    {"scope": {"kind": "profile", "profile": "unprefixed", "protocol": "openai_chat"},
                     "target": {"route_id": "route-a"}},
                ],
                "compaction": {
                    "target": {"route_id": "route-a"},
                    "lane_transition": "move_when_cold",
                    "cache_transition": "allow_when_cold",
                },
            }},
            "active_revision": {"policy": {"routes": [{"id": "route-a"}], "defaults": [
                {"scope": {"kind": "protocol", "protocol": "openai_chat"},
                 "target": {"route_id": "route-b"}},
                {"scope": {"kind": "protocol", "protocol": "openai_responses"},
                 "target": {"route_id": "route-b"}},
            ]}},
        })
    }

    #[test]
    fn defaults_and_compaction_are_listed_where_they_are_edited() {
        let mut state = loaded(defaults_snapshot());
        state.set_view(View::Routes);
        let keys: Vec<_> = state.entries().iter().map(|entry| entry.key).collect();
        assert_eq!(keys, ["routes", "defaults", "defaults"]);
        state.set_view(View::Rewrites);
        let keys: Vec<_> = state.entries().iter().map(|entry| entry.key).collect();
        assert_eq!(keys, ["compaction"]);
    }

    #[test]
    fn a_default_is_replaced_and_removed_by_its_scope() {
        let mut state = loaded(defaults_snapshot());
        state.set_view(View::Routes);
        state.move_selection(2);
        press(&mut state, KeyCode::Char('e'));
        let ctrl_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        let command = state.key(ctrl_enter).expect("replace command");
        let operation = &command.body.expect("body")["edit"]["operations"][0];
        assert_eq!(operation["kind"], "replace_default");
        assert_eq!(operation["scope"]["profile"], "unprefixed");
        assert_eq!(operation["default"]["target"]["route_id"], "route-a");
        state.receive(reply(
            "draft-edit",
            200,
            serde_json::json!({"generation": 6}),
        ));

        press(&mut state, KeyCode::Char('d'));
        let command = press(&mut state, KeyCode::Char('y')).expect("confirmed");
        let operation = &command.body.expect("body")["edit"]["operations"][0];
        assert_eq!(operation["kind"], "remove_default");
        assert_eq!(operation["scope"]["profile"], "unprefixed");
    }

    #[test]
    fn compaction_is_replaced_and_removed_without_an_id() {
        let mut state = loaded(defaults_snapshot());
        state.set_view(View::Rewrites);
        press(&mut state, KeyCode::Char('d'));
        let command = press(&mut state, KeyCode::Char('y')).expect("confirmed");
        let operation = &command.body.expect("body")["edit"]["operations"][0];
        assert_eq!(operation["kind"], "remove_compaction");
        state.receive(reply(
            "draft-edit",
            200,
            serde_json::json!({"generation": 6}),
        ));
        press(&mut state, KeyCode::Char('e'));
        let ctrl_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        let command = state.key(ctrl_enter).expect("replace command");
        let operation = &command.body.expect("body")["edit"]["operations"][0];
        assert_eq!(operation["kind"], "replace_compaction");
        assert_eq!(operation["compaction"]["target"]["route_id"], "route-a");
    }

    #[test]
    fn the_activation_summary_counts_default_and_compaction_changes() {
        let state = loaded(defaults_snapshot());
        let summary = state.activation_summary().join("\n");
        assert!(summary.contains("~default openai_chat"), "{summary}");
        assert!(
            summary.contains("+default unprefixed/openai_chat"),
            "{summary}"
        );
        assert!(summary.contains("-default openai_responses"), "{summary}");
        assert!(summary.contains("+compaction"), "{summary}");
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

    const CTRL_ENTER: KeyEvent = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);

    #[test]
    fn adding_a_presentation_sends_the_typed_addition_at_the_loaded_generation() {
        let mut state = loaded(full_snapshot());
        assert!(press(&mut state, KeyCode::Char('n')).is_none());
        let Modal::Record { key, value, adding } = state.modal().clone() else {
            panic!("a one-kind view opens the template directly");
        };
        assert_eq!((key.as_str(), adding), ("presentations", true));
        assert!(value.contains("new-presentation"), "{value}");
        let command = state.key(CTRL_ENTER).expect("add command");
        assert_eq!(command.name, "draft-edit");
        assert_eq!(command.method, Method::PATCH);
        let body = command.body.expect("body");
        assert_eq!(body["expected_generation"], 3);
        let operation = &body["edit"]["operations"][0];
        assert_eq!(operation["kind"], "add_presentation");
        assert_eq!(operation["presentation"]["id"], "new-presentation");
        assert!(state.in_flight());
        // The draft the daemon answers with closes the form.
        state.receive(reply(
            "draft-edit",
            200,
            serde_json::json!({"generation": 4}),
        ));
        assert!(!state.modal().active());
    }

    #[test]
    fn an_added_record_is_the_text_that_was_edited_not_the_template() {
        let mut state = loaded(full_snapshot());
        press(&mut state, KeyCode::Char('n'));
        let Modal::Record { value, .. } = state.modal().clone() else {
            panic!("record form");
        };
        // Clear the template, then type a record of our own.
        for _ in 0..value.chars().count() {
            press(&mut state, KeyCode::Backspace);
        }
        typed(&mut state, r#"{"id":"mine","slug":"m"}"#);
        let command = state.key(CTRL_ENTER).expect("add command");
        let operation = &command.body.expect("body")["edit"]["operations"][0];
        assert_eq!(operation["presentation"]["id"], "mine");
    }

    #[test]
    fn a_bad_new_record_stays_open_and_sends_nothing() {
        let mut state = loaded(full_snapshot());
        press(&mut state, KeyCode::Char('n'));
        typed(&mut state, "}");
        assert!(state.key(CTRL_ENTER).is_none());
        assert!(
            state
                .error()
                .is_some_and(|error| error.starts_with("record JSON"))
        );
        assert!(matches!(state.modal(), Modal::Record { adding: true, .. }));
        assert!(!state.in_flight());
        press(&mut state, KeyCode::Esc);
        assert!(!state.modal().active());
    }

    #[test]
    fn adding_a_record_whose_id_exists_shows_the_daemons_refusal_in_the_form() {
        let mut state = loaded(full_snapshot());
        state.set_view(View::Routes);
        press(&mut state, KeyCode::Char('n'));
        press(&mut state, KeyCode::Char('1'));
        assert!(state.key(CTRL_ENTER).is_some());
        // The refusal text is the daemon's own: the template's route added
        // to a draft that already holds a route of that id.
        let record = crate::tui::routing::template::template("routes", None);
        let draft: crate::policy::PolicyDocument = serde_json::from_value(serde_json::json!({
            "format_version": 3, "routes": [record.clone()],
        }))
        .expect("document");
        let duplicate: crate::policy::PolicyEdit = serde_json::from_value(
            serde_json::json!({ "operations": [super::add_edit("routes", record)] }),
        )
        .expect("edit");
        let refusal = duplicate.apply(&draft).expect_err("the id is taken");
        state.receive(reply(
            "draft-edit",
            422,
            serde_json::json!({
                "error": "invalid_policy_edit",
                "detail": refusal.to_string(),
            }),
        ));
        assert!(
            matches!(state.modal(), Modal::Record { adding: true, .. }),
            "the form stays open so the id can be changed"
        );
        assert!(!state.in_flight());
        let error = state.error().expect("the refusal is shown");
        assert!(error.contains("invalid_policy_edit"), "{error}");
        assert!(error.contains("already exists"), "{error}");
        assert!(
            state.conflicted_edits.is_empty(),
            "a refused edit is not offered for reapply"
        );
        // Fixing the id and saving again is an ordinary second attempt.
        assert!(state.key(CTRL_ENTER).is_some());
    }

    #[test]
    fn a_conflict_on_an_add_reapplies_the_addition_to_the_daemon_copy() {
        let mut state = loaded(full_snapshot());
        press(&mut state, KeyCode::Char('n'));
        state.key(CTRL_ENTER).expect("add command");
        state.receive(reply(
            "draft-edit",
            409,
            serde_json::json!({"error": "stale_draft", "generation": 9,
                "draft": {"policy": {"description": "theirs"}}}),
        ));
        assert!(matches!(state.modal(), Modal::Conflict { .. }));
        let command = press(&mut state, KeyCode::Char('a')).expect("reapply command");
        let body = command.body.expect("body");
        assert_eq!(body["expected_generation"], 9);
        assert_eq!(body["edit"]["operations"][0]["kind"], "add_presentation");
    }

    #[test]
    fn a_view_with_several_kinds_asks_which_to_add() {
        let mut state = loaded(defaults_snapshot());
        state.set_view(View::Routes);
        // The cursor is on a route, so a route is the preselected kind.
        press(&mut state, KeyCode::Char('n'));
        assert!(matches!(
            state.modal(),
            Modal::NewKind { preselected: 0, .. }
        ));
        assert!(press(&mut state, KeyCode::Char('x')).is_none());
        assert!(matches!(state.modal(), Modal::NewKind { .. }));
        press(&mut state, KeyCode::Enter);
        let Modal::Record { key, adding, .. } = state.modal().clone() else {
            panic!("Enter picks the preselected kind");
        };
        assert_eq!((key.as_str(), adding), ("routes", true));
        press(&mut state, KeyCode::Esc);

        // With the cursor on a default, a default is the preselected kind.
        state.move_selection(1);
        press(&mut state, KeyCode::Char('n'));
        assert!(matches!(
            state.modal(),
            Modal::NewKind { preselected: 1, .. }
        ));
        press(&mut state, KeyCode::Esc);
        assert!(!state.modal().active());

        // A number picks a kind whatever the cursor is on.
        press(&mut state, KeyCode::Char('n'));
        press(&mut state, KeyCode::Char('9'));
        assert!(matches!(state.modal(), Modal::NewKind { .. }));
        press(&mut state, KeyCode::Char('2'));
        assert!(matches!(state.modal(), Modal::Record { key, .. } if key == "defaults"));
    }

    #[test]
    fn every_kind_in_the_rewrites_view_can_be_added() {
        let mut state = loaded(defaults_snapshot());
        state.set_view(View::Rewrites);
        for (digit, kind, operation) in [
            ('1', "rewrites", "add_rewrite"),
            ('2', "classifiers", "add_classifier"),
            ('3', "upgrades", "add_upgrade"),
            ('4', "compaction", "add_compaction"),
        ] {
            press(&mut state, KeyCode::Char('n'));
            press(&mut state, KeyCode::Char(digit));
            assert!(matches!(state.modal(), Modal::Record { key, .. } if key == kind));
            let command = state.key(CTRL_ENTER).expect("add command");
            assert_eq!(
                command.body.expect("body")["edit"]["operations"][0]["kind"],
                operation
            );
            state.receive(reply(
                "draft-edit",
                200,
                serde_json::json!({"generation": 6}),
            ));
        }
    }

    #[test]
    fn a_new_default_starts_from_the_drafts_first_route_and_adds_by_scope() {
        let mut state = loaded(defaults_snapshot());
        state.set_view(View::Routes);
        press(&mut state, KeyCode::Char('n'));
        press(&mut state, KeyCode::Char('2'));
        let Modal::Record { value, .. } = state.modal().clone() else {
            panic!("record form");
        };
        let template: serde_json::Value = serde_json::from_str(&value).expect("JSON template");
        assert_eq!(template["target"]["route_id"], "route-a");
        let command = state.key(CTRL_ENTER).expect("add command");
        let operation = &command.body.expect("body")["edit"]["operations"][0];
        assert_eq!(operation["kind"], "add_default");
        assert_eq!(operation["default"], template);
    }

    #[test]
    fn adding_is_not_offered_in_the_status_view_or_while_busy() {
        let mut state = loaded(full_snapshot());
        state.set_view(View::Status);
        assert!(press(&mut state, KeyCode::Char('n')).is_none());
        assert!(!state.modal().active());
        assert!(state.error().is_some());
        state.set_view(View::Presentations);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Char('y'));
        assert!(state.in_flight());
        press(&mut state, KeyCode::Char('n'));
        assert!(!matches!(state.modal(), Modal::Record { .. }));
    }
}
