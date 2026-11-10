//! A record, a candidate or a move being edited where it is shown.
//!
//! An edit is a form of fields (or none, for a removal or a move) and the
//! typed operations it makes. Each time those change, the daemon is asked
//! whether they would compile, against the live revision, and what recent
//! traffic would do under them; what comes back is shown beside the fields,
//! and only an edit the daemon has accepted can be saved
//! (spec: UTUI#editing).

use serde_json::{Value, json};

use super::form::{Finding, Form, Severity, default_edge};

/// What is being edited, and so what its operations are.
#[derive(Debug, Clone)]
pub(super) enum What {
    /// A record's fields: replacing the one that was, or adding a new one.
    Record { kind: &'static str, adding: bool },
    /// Removing a record.
    Remove { kind: &'static str, record: Value },
    /// A fallback after `after`, a candidate of the route.
    AddCandidate { route_id: String, after: String },
    /// One candidate of the route, and the edge into it.
    EditCandidate { route: Value, index: usize },
    RemoveCandidate {
        route_id: String,
        candidate_id: String,
    },
    /// A candidate lifted from `from` to `to`; `edges` are the edges into
    /// each candidate after the first.
    MoveCandidate {
        route_id: String,
        ids: Vec<String>,
        edges: Vec<Value>,
        from: usize,
        to: usize,
    },
    /// A rewrite lifted within its scope: the scope's rules as they were, in
    /// order.
    MoveRule {
        scope_label: String,
        rules: Vec<Value>,
        from: usize,
        to: usize,
    },
}

/// The daemon's verdict on the operations as they were when it was asked.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Checked {
    pub(super) ops: Value,
    pub(super) valid: bool,
    /// Refusals that belong to other records than the one being edited.
    pub(super) elsewhere: Vec<Elsewhere>,
}

/// A refusal about a record the form is not for, which the form names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Elsewhere {
    pub(super) record: String,
    pub(super) sentence: String,
    pub(super) fix: Option<String>,
}

/// How the last 24 hours of requests would route under the operations.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Replay {
    pub(super) ops: Value,
    pub(super) replayed: u64,
    pub(super) unreplayable: u64,
    pub(super) truncated: bool,
    pub(super) changed_total: u64,
    pub(super) changed: Vec<Value>,
    pub(super) until_ms: i64,
}

#[derive(Debug, Clone)]
pub(super) struct Editing {
    pub(super) what: What,
    pub(super) form: Form,
    /// Where the record being edited sits in its collection of the document,
    /// so a finding about it can be told from one about another record.
    owner: Option<usize>,
    /// Where in its route's chain the candidate being edited sits, the first
    /// being 0.
    step: Option<usize>,
    /// The operations the daemon was last asked about, and whether it has
    /// answered: one check at a time.
    pub(super) requested: Option<Value>,
    pub(super) validating: bool,
    pub(super) checked: Option<Checked>,
    pub(super) replay_requested: Option<Value>,
    pub(super) replaying: bool,
    pub(super) replay: Option<Replay>,
    /// Why the checks could not be made, where the daemon did not answer.
    pub(super) unchecked: Option<String>,
    /// The revision that is live now, where a save or a check found that the
    /// one this edit was made against was not.
    pub(super) moved_to: Option<i64>,
}

impl Editing {
    pub(super) fn new(
        what: What,
        form: Form,
        owner: Option<usize>,
        step: Option<usize>,
    ) -> Editing {
        Editing {
            what,
            form,
            owner,
            step,
            requested: None,
            validating: false,
            checked: None,
            replay_requested: None,
            replaying: false,
            replay: None,
            unchecked: None,
            moved_to: None,
        }
    }

    /// What was asked of the daemon is stale: the live policy changed under
    /// the edit, so it is checked again.
    pub(super) fn recheck(&mut self) {
        self.requested = None;
        self.checked = None;
        self.replay_requested = None;
        self.replay = None;
    }

    /// The typed operations the edit makes as it stands. None where there is
    /// nothing to do yet, as with a move that has not moved.
    pub(super) fn operations(&self) -> Vec<Value> {
        match &self.what {
            What::Record { kind, adding } => record_operations(kind, *adding, self.form.value()),
            What::Remove { kind, record } => vec![remove_edit(kind, record)],
            What::AddCandidate { route_id, after } => {
                let value = self.form.value();
                vec![json!({
                    "kind": "add_candidate",
                    "route_id": route_id,
                    "after": after,
                    "edge": value.get("edge").cloned().unwrap_or(Value::Null),
                    "candidate": value.get("candidate").cloned().unwrap_or(Value::Null),
                })]
            }
            What::EditCandidate { route, index } => {
                let value = self.form.value();
                let mut route = route.clone();
                let candidate = value.get("candidate").cloned().unwrap_or(Value::Null);
                if *index == 0 {
                    route["chain"]["first"] = candidate;
                } else if let Some(step) = route["chain"]["steps"].get_mut(index - 1) {
                    step["candidate"] = candidate;
                    step["edge"] = value.get("edge").cloned().unwrap_or(Value::Null);
                }
                route_operations(route, false)
            }
            What::RemoveCandidate {
                route_id,
                candidate_id,
            } => vec![json!({
                "kind": "remove_candidate",
                "route_id": route_id,
                "candidate_id": candidate_id,
            })],
            What::MoveCandidate {
                route_id,
                ids,
                edges,
                from,
                to,
            } => {
                if from == to {
                    return Vec::new();
                }
                let mut order = ids.clone();
                let moved = order.remove(*from);
                order.insert(*to, moved.clone());
                // The edge that led into it travels with it; the first has
                // none, so the one it gets is the default.
                let edge = from
                    .checked_sub(1)
                    .and_then(|at| edges.get(at).cloned())
                    .unwrap_or_else(default_edge);
                let destination = if *to == 0 {
                    json!({ "kind": "first", "edge": edge })
                } else {
                    json!({
                        "kind": "after",
                        "candidate_id": order[to - 1],
                        "edge": edge,
                    })
                };
                vec![json!({
                    "kind": "move_candidate",
                    "route_id": route_id,
                    "candidate_id": moved,
                    "destination": destination,
                })]
            }
            What::MoveRule {
                rules, from, to, ..
            } => rule_moves(rules, *from, *to),
        }
    }

    /// Whether the daemon has accepted exactly these operations.
    pub(super) fn accepted(&self, ops: &Value) -> bool {
        self.checked
            .as_ref()
            .is_some_and(|checked| &checked.ops == ops && checked.valid)
    }

    /// Whether the edit can be saved as it stands.
    pub(super) fn saveable(&self) -> bool {
        let ops = Value::Array(self.operations());
        ops.as_array().is_some_and(|ops| !ops.is_empty())
            && self.accepted(&ops)
            && self.form.refused() == (Vec::new(), false)
    }

    /// What the save control says: `save`, or what is in the way of it.
    pub(super) fn save_label(&self) -> String {
        if let Some(live) = self.moved_to {
            return format!("save onto rev {live}");
        }
        let ops = Value::Array(self.operations());
        if ops.as_array().is_some_and(Vec::is_empty) {
            return match self.what {
                What::MoveRule { .. } | What::MoveCandidate { .. } => "move it to save".to_owned(),
                _ => "change something to save".to_owned(),
            };
        }
        let (fields, loose) = self.form.refused();
        let checked = self.checked.as_ref().filter(|checked| checked.ops == ops);
        let elsewhere = checked.map_or(0, |checked| checked.elsewhere.len());
        let others = elsewhere + usize::from(loose);
        match (fields.len(), others) {
            (0, 0) => match checked {
                Some(checked) if checked.valid => "save".to_owned(),
                Some(_) => "fix the refusal to save".to_owned(),
                None if self.unchecked.is_some() => "the check failed".to_owned(),
                None => "checking".to_owned(),
            },
            (1, 0) => format!("fix {} to save", fields[0]),
            (count, 0) => format!("fix {count} fields to save"),
            (0, 1) if elsewhere == 1 => {
                let record = checked
                    .and_then(|checked| checked.elsewhere.first())
                    .map_or("the record", |other| other.record.as_str());
                format!("fix {record} to save")
            }
            (0, _) if elsewhere == others => format!("fix {others} records to save"),
            (0, 1) => "fix the refusal to save".to_owned(),
            (fields, others) => format!("fix {} things to save", fields + others),
        }
    }

    /// The form-relative path of a document path that concerns the record
    /// being edited, or none where it concerns another.
    fn form_path(&self, path: &[String]) -> Option<Vec<String>> {
        let (collection, rest) = path.split_first()?;
        match &self.what {
            What::Record { kind, .. } => {
                if collection != kind {
                    return None;
                }
                if *kind == "compaction" {
                    return Some(rest.to_vec());
                }
                let (index, rest) = rest.split_first()?;
                (self.owner.map(|owner| owner.to_string()) == Some(index.clone()))
                    .then(|| rest.to_vec())
            }
            What::AddCandidate { .. } | What::EditCandidate { .. } => {
                if collection != "routes" {
                    return None;
                }
                let (index, rest) = rest.split_first()?;
                if self.owner.map(|owner| owner.to_string()) != Some(index.clone()) {
                    return None;
                }
                let step = self.step?;
                // `chain/first/...` is step 0; `chain/steps/k/edge|candidate`
                // is step k + 1.
                let rest = rest.strip_prefix(&["chain".to_owned()])?;
                match rest.split_first()? {
                    (first, rest) if first == "first" => {
                        (step == 0).then(|| [vec!["candidate".to_owned()], rest.to_vec()].concat())
                    }
                    (steps, rest) if steps == "steps" => {
                        let (at, rest) = rest.split_first()?;
                        if at.parse::<usize>().ok()? + 1 != step {
                            return None;
                        }
                        Some(rest.to_vec())
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// A route's operations: the route as a replacement, and the safeguards flag
/// on its own operation, which raises the document's format with it. A route
/// form sets the flag either way; an edit of a candidate leaves it as it was.
fn route_operations(mut route: Value, always_set_flag: bool) -> Vec<Value> {
    let id = route.get("id").cloned().unwrap_or(Value::Null);
    let honours = route
        .as_object_mut()
        .and_then(|route| route.remove("honours_server_safeguards"));
    let mut operations = vec![json!({ "kind": "replace_route", "id": id, "route": route })];
    if always_set_flag || honours == Some(Value::Bool(true)) {
        operations.push(json!({
            "kind": "set_route_server_safeguards",
            "route_id": id,
            "honours": honours == Some(Value::Bool(true)),
        }));
    }
    operations
}

/// The operations a record's form makes.
fn record_operations(kind: &str, adding: bool, value: &Value) -> Vec<Value> {
    if kind == "routes" {
        // The flag is the one route field with its own operation, which is
        // what raises the document to the format that carries it, so the
        // route travels without it and the operation follows.
        let mut operations = route_operations(value.clone(), true);
        if adding {
            let route = operations.remove(0)["route"].clone();
            operations.insert(0, add_edit(kind, route));
        }
        return operations;
    }
    vec![if adding {
        add_edit(kind, value.clone())
    } else {
        replace_edit(kind, value.clone())
    }]
}

/// Priorities that put `rules` in the order with the rule at `from` moved to
/// `to`: the same priorities the scope already used, dealt out in the new
/// order, so no rule outside the move changes unless two shared one. The
/// operations replace only the rules whose priority changed.
fn rule_moves(rules: &[Value], from: usize, to: usize) -> Vec<Value> {
    if from == to {
        return Vec::new();
    }
    let priority = |rule: &Value| rule.get("priority").and_then(Value::as_i64).unwrap_or(0);
    let mut order: Vec<&Value> = rules.iter().collect();
    let moved = order.remove(from);
    order.insert(to, moved);
    let mut slots: Vec<i64> = rules.iter().map(priority).collect();
    slots.sort_unstable();
    // A scope whose rules shared a priority has no room to put one between
    // them, so they are told apart.
    for at in 1..slots.len() {
        if slots[at] <= slots[at - 1] {
            slots[at] = slots[at - 1] + 1;
        }
    }
    order
        .into_iter()
        .zip(slots)
        .filter(|(rule, slot)| priority(rule) != *slot)
        .map(|(rule, slot)| {
            let mut record = rule.clone();
            record["priority"] = Value::from(slot);
            replace_edit("rewrites", record)
        })
        .collect()
}

pub(super) fn replace_edit(key: &str, value: Value) -> Value {
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

pub(super) fn remove_edit(key: &str, value: &Value) -> Value {
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

/// What a collection's records are called one at a time.
fn record_noun(collection: &str) -> &str {
    match collection {
        "rewrites" => "rewrite",
        "routes" => "route",
        "presentations" => "presentation",
        "defaults" => "default",
        "classifiers" => "classifier",
        "upgrades" => "upgrade",
        other => other,
    }
}

/// A document path as its segments, with the pointer's escapes undone.
fn segments(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
        .collect()
}

impl Editing {
    /// Take what `routing/validate` answered for `ops`.
    pub(super) fn checked_with(&mut self, ops: Value, body: &Value) {
        let mut own = Vec::new();
        let mut elsewhere = Vec::new();
        for diagnostic in body
            .get("diagnostics")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
        {
            let severity = diagnostic.get("severity").and_then(Value::as_str);
            let path = segments(diagnostic.get("path").and_then(Value::as_str).unwrap_or(""));
            let sentence = diagnostic
                .get("sentence")
                .and_then(Value::as_str)
                .or_else(|| diagnostic.get("code").and_then(Value::as_str))
                .unwrap_or("refused")
                .to_owned();
            let fix = diagnostic
                .get("fix")
                .and_then(Value::as_str)
                .map(str::to_owned);
            match (self.form_path(&path), severity) {
                (Some(path), Some("error")) => own.push(Finding {
                    path,
                    severity: Severity::Refusal,
                    sentence,
                    fix,
                }),
                (Some(path), Some("warning")) => own.push(Finding {
                    path,
                    severity: Severity::Warning,
                    sentence,
                    fix,
                }),
                (None, Some("error")) => {
                    let collection = path.first().map_or("", String::as_str);
                    let record = match diagnostic.get("record_id").and_then(Value::as_str) {
                        Some(id) => format!("{} {id}", record_noun(collection)),
                        None => record_noun(collection).to_owned(),
                    };
                    elsewhere.push(Elsewhere {
                        record,
                        sentence,
                        fix,
                    });
                }
                _ => {}
            }
        }
        self.form.set_findings(own);
        self.checked = Some(Checked {
            ops,
            valid: body.get("valid") == Some(&Value::Bool(true)),
            elsewhere,
        });
        self.unchecked = None;
    }

    /// Take what `routing/preview` answered for `ops`.
    pub(super) fn replayed_with(&mut self, ops: Value, body: &Value) {
        let count = |key: &str| body.get(key).and_then(Value::as_u64).unwrap_or(0);
        self.replay = Some(Replay {
            ops,
            replayed: count("replayed"),
            unreplayable: count("unreplayable"),
            truncated: body.get("truncated") == Some(&Value::Bool(true)),
            changed_total: count("changed_total"),
            changed: body
                .get("changed")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            until_ms: body
                .get("window")
                .and_then(|window| window.get("until_ms"))
                .and_then(Value::as_i64)
                .unwrap_or(0),
        });
    }
}
