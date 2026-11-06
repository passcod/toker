//! Content-free routing snapshot, draft editing, and navigation state.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reqwest::Method;
use serde_json::{Value, json};

use super::client::{Command, Reply, Response};

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
            View::Routes => "ROUTES & CANDIDATES",
            View::Rewrites => "REWRITES, FAMILIES & UPGRADES",
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
    Record {
        key: String,
        value: String,
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
            Self::Record { .. } => "EDIT TYPED POLICY RECORD",
            Self::Remove { .. } => "REMOVE POLICY RECORD",
            Self::CandidateOrder { .. } => "ORDER ROUTE CANDIDATES",
            Self::Preview { .. } | Self::PreviewResult(_) => "ROUTING PREVIEW",
            Self::Activate { .. } => "ACTIVATE DRAFT",
            Self::Reactivate { .. } => "REACTIVATE REVISION",
            Self::Promote { .. } => "PROMOTE LEARNED MODEL",
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
    /// The exact typed edit batch that received a 409.  It remains intact until
    /// the user explicitly reloads or reapplies it against the daemon copy.
    conflicted_edits: Vec<Value>,
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
            conflicted_edits: Vec::new(),
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
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        let count = self.entries().len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.saturating_add_signed(delta).min(count - 1);
        }
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
                .flat_map(|document| array(document, "routes"))
                .map(|value| Entry {
                    key: "routes",
                    value,
                })
                .collect(),
            View::Rewrites => policy
                .into_iter()
                .flat_map(|document| {
                    ["rewrites", "classifiers", "upgrades"]
                        .into_iter()
                        .flat_map(move |key| {
                            array(document, key)
                                .iter()
                                .map(move |value| Entry { key, value })
                        })
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
                };
            } else {
                self.error = Some("this item is not a draft policy record".to_owned());
            }
        }
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
            } => {
                if key.code == KeyCode::Esc {
                    (Modal::None, ModalAction::None)
                } else if key.code == KeyCode::Enter
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    match serde_json::from_str::<Value>(&value) {
                        Ok(record) => (
                            Modal::Record {
                                key: record_key.clone(),
                                value,
                            },
                            ModalAction::Edit(vec![replace_edit(&record_key, record)]),
                        ),
                        Err(error) => {
                            self.error = Some(format!("record JSON: {error}"));
                            (
                                Modal::Record {
                                    key: record_key,
                                    value,
                                },
                                ModalAction::None,
                            )
                        }
                    }
                } else {
                    text_key(&mut value, key);
                    (
                        Modal::Record {
                            key: record_key,
                            value,
                        },
                        ModalAction::None,
                    )
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
            ModalAction::Reapply => self.reapply_conflict(),
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
        "presentations" | "routes" | "rewrites" | "classifiers" | "upgrades"
    )
}

fn replace_edit(key: &str, value: Value) -> Value {
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    match key {
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

fn remove_edit(key: &str, value: &Value) -> Value {
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    match key {
        "presentations" => json!({ "kind": "remove_presentation", "id": id }),
        "routes" => json!({ "kind": "remove_route", "id": id }),
        "rewrites" => json!({ "kind": "remove_rewrite", "id": id }),
        "classifiers" => json!({ "kind": "remove_classifier", "id": id }),
        "upgrades" => json!({ "kind": "remove_upgrade", "id": id }),
        _ => Value::Null,
    }
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
        "rewrites",
        "classifiers",
        "upgrades",
    ] {
        let by_id = |document: Option<&Value>| -> Vec<(String, Value)> {
            document
                .map(|document| array(document, key))
                .unwrap_or(&[])
                .iter()
                .map(|record| {
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
        let kind = key.trim_end_matches('s');
        for (id, record) in &after {
            match before.iter().find(|(prior, _)| prior == id) {
                None => changes.push(format!("+{kind} {id}")),
                Some((_, prior)) if prior != record => changes.push(format!("~{kind} {id}")),
                Some(_) => {}
            }
        }
        for (id, _) in &before {
            if !after.iter().any(|(next, _)| next == id) {
                changes.push(format!("-{kind} {id}"));
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
}
