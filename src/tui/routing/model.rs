//! Content-free routing snapshot and navigation state.

use serde_json::Value;

use super::client::Reply;

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

/// Data and selection the page preserves while the dashboard is visible.
#[derive(Debug)]
pub(super) struct State {
    view: View,
    snapshot: Option<Value>,
    generation: Option<u64>,
    selected: usize,
    error: Option<String>,
    loading: bool,
}

impl Default for State {
    fn default() -> Self {
        State {
            view: View::Presentations,
            snapshot: None,
            generation: None,
            selected: 0,
            error: None,
            loading: false,
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

    pub(super) fn loading(&self) -> bool {
        self.loading
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
        let active = snapshot.get("active_revision");
        match self.view {
            View::Presentations => active
                .into_iter()
                .flat_map(|revision| array(policy(revision), "presentations"))
                .map(|value| Entry {
                    key: "presentations",
                    value,
                })
                .collect(),
            View::Routes => active
                .into_iter()
                .flat_map(|revision| array(policy(revision), "routes"))
                .map(|value| Entry {
                    key: "routes",
                    value,
                })
                .collect(),
            View::Rewrites => active
                .into_iter()
                .flat_map(|revision| {
                    let document = policy(revision);
                    ["rewrites", "families", "upgrades"]
                        .into_iter()
                        .flat_map(move |key| {
                            array(document, key)
                                .iter()
                                .map(move |value| Entry { key, value })
                        })
                })
                .collect(),
            View::Status => {
                let revision = active.into_iter().map(|value| Entry {
                    key: "active_revision",
                    value,
                });
                let publications = array(snapshot, "publications").iter().map(|value| Entry {
                    key: "publications",
                    value,
                });
                revision.chain(publications).collect()
            }
        }
    }

    /// Begin a refresh.  The caller sends it only when this returns true.
    pub(super) fn begin_refresh(&mut self) -> bool {
        if self.loading {
            return false;
        }
        self.loading = true;
        true
    }

    pub(super) fn receive(&mut self, reply: Reply) {
        self.loading = false;
        match reply {
            Reply::Snapshot(Ok(snapshot)) => {
                self.generation = generation(&snapshot);
                self.snapshot = Some(snapshot);
                self.error = None;
                let count = self.entries().len();
                self.selected = self.selected.min(count.saturating_sub(1));
            }
            Reply::Snapshot(Err(error)) => self.error = Some(error),
        }
    }
}

/// One content-free item supplied by the snapshot.
#[derive(Debug, Clone, Copy)]
pub(super) struct Entry<'a> {
    pub(super) key: &'a str,
    pub(super) value: &'a Value,
}

fn array<'a>(snapshot: &'a Value, key: &str) -> &'a [Value] {
    snapshot
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn policy(revision: &Value) -> &Value {
    revision.get("policy").unwrap_or(&Value::Null)
}

fn generation(snapshot: &Value) -> Option<u64> {
    // Current snapshot replies do not carry a mutable draft generation. Keep
    // both documented locations for the draft-led control surface, and never
    // infer one from an immutable active revision.
    snapshot
        .get("draft")
        .and_then(|draft| draft.get("generation"))
        .and_then(Value::as_u64)
        .or_else(|| snapshot.get("generation").and_then(Value::as_u64))
}

#[cfg(test)]
mod tests {
    use super::{State, View};
    use crate::tui::routing::client::Reply;

    #[test]
    fn views_cycle_and_snapshots_keep_the_draft_generation() {
        let mut state = State::default();
        assert_eq!(state.view(), View::Presentations);
        state.move_view(-1);
        assert_eq!(state.view(), View::Status);
        state.move_view(1);
        assert_eq!(state.view(), View::Presentations);
        state.receive(Reply::Snapshot(Ok(serde_json::json!({
            "draft": {"generation": 7},
            "active_revision": {"policy": {"presentations": [{"slug": "friendly"}]}}
        }))));
        assert_eq!(state.generation(), Some(7));
        assert_eq!(state.entries().len(), 1);
    }
}
