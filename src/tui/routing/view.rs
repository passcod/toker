//! Routing page rendering, including content-free typed modal forms.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Clear, Paragraph, Wrap},
};
use serde_json::Value;

use super::form::Form;
use super::model::{Entry, Modal, State, View};
use super::template::singular;
use crate::tui::chrome::{self, Chrome, Noun, RowHit};

/// The page areas that the loop needs for click handling.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Drawn {
    pub(crate) header: chrome::Header,
    pub(super) tabs: Vec<Rect>,
    /// Where each entry's row landed.
    pub(super) rows: Vec<RowHit>,
    pub(super) entries: Rect,
    pub(super) detail: Rect,
    /// Where an open form's fields are, for clicks.
    pub(super) form: Option<FormHit>,
}

/// An open form's visible lines: the field each focuses, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FormHit {
    pub(super) body: Rect,
    pub(super) rows: Vec<Option<usize>>,
}

/// Below this width the list and the detail pane stack rather than sit
/// side by side, where each would be too narrow to read.
const SIDE_BY_SIDE_MIN_WIDTH: u16 = 100;

/// What the list's counts call the entries they leave out.
const NOUN: Noun = Noun {
    one: "record",
    many: "records",
};

pub(super) fn render(frame: &mut Frame, state: &State, overlay: bool) -> Drawn {
    // A refusal or a failed read is said over the page, which stays
    // usable read-only on the snapshot it last loaded.
    let (header, body) = chrome::page(frame, state.error());
    let labels = View::ALL.map(View::tab);
    let current = View::ALL
        .iter()
        .position(|view| *view == state.view())
        .unwrap_or(0);
    let tabs_min = chrome::tabs_min_width(&labels, current);
    let tabs_area = chrome::status(frame, header.rest, tabs_min, &status_text(state));
    let tabs = chrome::tabs(frame, tabs_area, &labels, current);

    let entries = state.entries();
    let list: Vec<Vec<Line<'static>>> = if entries.is_empty() {
        vec![vec![Line::raw("no routing data yet")]]
    } else {
        entries
            .iter()
            .map(|entry| vec![Line::raw(entry_summary(entry))])
            .collect()
    };
    let lines = detail_lines(state, entries.get(state.selected()));

    let stacked = body.width < SIDE_BY_SIDE_MIN_WIDTH;
    let (master, detail) = if stacked {
        // The list takes what it needs, up to a third of the body; the
        // detail pane the rest.
        let list_rows = (list.len() as u16 + Chrome::Boxed.cost())
            .min(body.height / 3)
            .max(Chrome::Boxed.cost() + 1)
            .min(body.height);
        let [master, detail] =
            Layout::vertical([Constraint::Length(list_rows), Constraint::Fill(1)]).areas(body);
        (master, detail)
    } else {
        let [master, detail] =
            Layout::horizontal([Constraint::Percentage(42), Constraint::Fill(1)]).areas(body);
        (master, detail)
    };
    // One framing for both panes, boxed only while neither loses content
    // to its border (spec: UTUI#what-is-shed-first).
    let detail_need = wrapped_rows(&lines, detail.width.saturating_sub(2));
    let chrome = if Chrome::fitting(list.len(), master.height) == Chrome::Boxed
        && Chrome::fitting(detail_need, detail.height) == Chrome::Boxed
    {
        Chrome::Boxed
    } else {
        Chrome::Ruled
    };

    let inner = chrome::panel(frame, master, state.view().title(), chrome);
    let selected = (!entries.is_empty()).then_some(state.selected());
    let rows = chrome::list(frame, inner, list, selected, NOUN);
    let rows = if selected.is_some() { rows } else { Vec::new() };

    let inner = chrome::panel(frame, detail, "DETAIL", chrome);
    // Re-measured at the width the pane really has, which the ruled form
    // shares with the boxed one.
    let wrapped = wrapped_rows(&lines, inner.width);
    let furthest = u16::try_from(wrapped)
        .unwrap_or(u16::MAX)
        .saturating_sub(inner.height);
    state.set_detail_furthest(furthest);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((state.detail_scroll().min(furthest), 0)),
        inner,
    );

    chrome::chips(frame, &header);
    if overlay {
        chrome::overlay(
            frame,
            "KEYS",
            &[
                chrome::Section {
                    heading: None,
                    rows: &KEYS,
                },
                chrome::Section {
                    heading: Some("IN A FORM"),
                    rows: &FORM_KEYS,
                },
            ],
        );
    }
    let form = if state.modal().active() {
        render_modal(frame, state)
    } else {
        None
    };
    Drawn {
        header,
        tabs,
        rows,
        entries: master,
        detail,
        form,
    }
}

/// How many rows `lines` take wrapped to `width`. `Paragraph` does not
/// report its wrapped height, so this estimates it: a line wider than the
/// pane takes its ceiling in rows, plus one for the word wrap that can
/// push a word onto the next row. Over-counting only lets the pane scroll
/// a row past the end.
fn wrapped_rows(lines: &[Line], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines
        .iter()
        .map(|line| match line.width() {
            columns if columns <= width => 1,
            columns => columns.div_ceil(width) + 1,
        })
        .sum()
}

/// What the page is in the middle of, at the right of its header.
fn status_text(state: &State) -> String {
    let mut parts = Vec::new();
    if let Some(generation) = state.generation() {
        parts.push(format!("draft {generation}"));
    }
    if state.in_flight() {
        parts.push("working".to_owned());
    }
    if let Some(note) = state.settings_note() {
        parts.push(note.to_owned());
    }
    parts.join(" · ")
}

/// The page's keys, in its `?` overlay.
const KEYS: [(&str, &str); 18] = [
    ("↑/↓ j/k", "select a record"),
    ("←/→ h/l 1–4", "switch view"),
    ("pgup/pgdn", "scroll the detail"),
    (
        "n",
        "add a record, asking which kind where a view lists several",
    ),
    ("e", "edit the selected record in a form"),
    ("d", "remove the selected record"),
    ("v", "preview a request"),
    ("V", "validate the draft"),
    ("a", "activate the draft"),
    ("x", "reactivate the selected revision"),
    ("p", "promote"),
    (
        "i",
        "import toker.toml's routing into the draft, from the status view",
    ),
    ("r", "refresh"),
    ("click", "select a record, or focus a form field"),
    ("wheel", "scroll what is under the pointer"),
    ("esc", "back to the dashboard"),
    ("tab or ⇄", "next page"),
    ("q", "quit"),
];

/// A form's keys. A form takes every key, `?` included, so it also says
/// the common ones on its own last line.
const FORM_KEYS: [(&str, &str); 8] = [
    ("↑/↓", "move between fields"),
    ("←/→", "change the value"),
    ("tab", "complete"),
    ("space", "toggle"),
    ("alt+↑/↓", "move a list item or candidate"),
    ("ctrl+d", "remove a list item"),
    ("ctrl+s", "save"),
    ("esc", "cancel"),
];

fn entry_summary(entry: &Entry<'_>) -> String {
    let value = entry.value;
    if entry.key == "defaults" {
        return format!(
            "default {}  {}",
            super::model::scope_label(value),
            short(value)
        );
    }
    let name = [
        "slug",
        "id",
        "name",
        "revision",
        "adapter",
        "binding_id",
        "provider",
    ]
    .into_iter()
    .find_map(|key| value.get(key).and_then(Value::as_str).map(str::to_owned))
    .or_else(|| {
        ["id", "revision", "revision_id"]
            .into_iter()
            .find_map(|key| {
                value
                    .get(key)
                    .and_then(Value::as_i64)
                    .map(|number| number.to_string())
            })
    })
    .unwrap_or_else(|| entry.key.to_owned());
    format!("{name}  {}", short(value))
}

fn detail_lines(state: &State, selected: Option<&Entry<'_>>) -> Vec<Line<'static>> {
    let mut lines = match selected {
        Some(entry) => match state.snapshot() {
            Some(snapshot) => super::detail::lines(snapshot, entry),
            None => pretty(entry.value).lines().map(str::to_owned).collect(),
        }
        .into_iter()
        .map(Line::raw)
        .collect(),
        None if state.loading() => vec![Line::raw("loading routing snapshot…")],
        None => vec![Line::raw("routing snapshot has no entries for this view")],
    };
    if let Some(generation) = state.generation() {
        lines.insert(
            0,
            Line::styled(
                format!("draft generation {generation}"),
                Style::new().bold(),
            ),
        );
    }
    lines
}

fn short(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unrenderable>".to_owned())
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "<unrenderable>".to_owned())
}

fn render_modal(frame: &mut Frame, state: &State) -> Option<FormHit> {
    let modal = state.modal();
    if let Modal::Form(form) = modal {
        return Some(render_form(frame, state, form));
    }
    let lines = modal_text(state);
    let width = lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(20)
        .min(100) as u16
        + 4;
    let height = (lines.len() as u16 + 2).min(frame.area().height);
    let area = frame.area();
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width.min(area.width)) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines.join("\n"))
            .block(Block::bordered().title(modal.title()))
            .wrap(Wrap { trim: false }),
        popup,
    );
    None
}

/// A form fills most of the screen: its fields in a window that follows the
/// focus, then its key help and the last refusal.
fn render_form(frame: &mut Frame, state: &State, form: &Form) -> FormHit {
    let area = frame.area();
    let width = area.width.saturating_sub(4).clamp(1, 110).min(area.width);
    let height = area.height.saturating_sub(2).max(1).min(area.height);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(form.title());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);

    let lines = form.lines();
    let visible = usize::from(body.height);
    let focused = lines.iter().position(|line| line.focused).unwrap_or(0);
    let start = focused
        .saturating_sub(visible / 2)
        .min(lines.len().saturating_sub(visible));
    let shown: Vec<_> = lines.iter().skip(start).take(visible).collect();
    let rendered: Vec<Line> = shown
        .iter()
        .map(|line| {
            let style = if line.focused {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            Line::styled(line.text.clone(), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(rendered), body);

    let mut notes = vec![Line::styled(form.help(), Style::new().dim())];
    if let Some(note) = form.note().or_else(|| state.error()) {
        notes.push(Line::styled(format!("! {note}"), Style::new().bold()));
    }
    frame.render_widget(Paragraph::new(notes), footer);
    FormHit {
        body,
        rows: shown.iter().map(|line| line.row).collect(),
    }
}

/// The modal's own lines, the impact summary an activation confirms against,
/// and the last daemon refusal, which would otherwise hide behind the popup.
fn modal_text(state: &State) -> Vec<String> {
    let modal = state.modal();
    let mut lines = modal_lines(modal);
    if let Modal::PreviewResult(result) = modal
        && state.preview_asked()
    {
        // Under the close hint, over the raw result it summarises.
        let summary = super::detail::preview_safeguards(result);
        lines.splice(1..1, summary);
    }
    if matches!(modal, Modal::Activate { .. } | Modal::Reactivate { .. }) {
        // Before the closing "confirms" line, so the prompt stays last.
        let confirm = lines.pop();
        lines.extend(state.activation_summary());
        lines.extend(confirm);
    }
    if let Some(error) = state.error() {
        lines.push(format!("! {error}"));
    }
    lines
}

fn modal_lines(modal: &Modal) -> Vec<String> {
    match modal {
        Modal::None => Vec::new(),
        // A form draws itself.
        Modal::Form(_) => Vec::new(),
        Modal::NewKind { kinds, preselected } => {
            let mut lines = vec!["Add which kind of record?".to_owned()];
            lines.extend(kinds.iter().enumerate().map(|(index, kind)| {
                let mark = if index == *preselected { ">" } else { " " };
                format!("{mark} {} {}", index + 1, singular(kind))
            }));
            let pick = format!("1–{} or enter", kinds.len());
            lines.push(chrome::hints(&[(&pick, "pick"), ("esc", "cancel")]));
            lines
        }
        Modal::Remove { key, value } => vec![
            format!("Remove this {key} record? This changes only the draft."),
            short(value),
            confirm_hint(),
        ],
        Modal::PreviewResult(result) => {
            vec![chrome::hints(&[("any key", "close")]), pretty(result)]
        }
        Modal::Activate {
            scope,
            force_cache_transition,
        } => activation_lines("Activate this draft?", *scope, *force_cache_transition),
        Modal::Reactivate {
            revision_id,
            scope,
            force_cache_transition,
        } => {
            let mut lines = activation_lines(
                &format!("Create a forward revision from revision {revision_id}?"),
                *scope,
                *force_cache_transition,
            );
            lines.insert(1, "The active pointer never moves backwards.".to_owned());
            lines
        }
        Modal::Import => vec![
            "Replace the current draft with the routing in toker.toml?".to_owned(),
            "The active revision does not change until the draft is activated.".to_owned(),
            confirm_hint(),
        ],
        Modal::Conflict {
            daemon_generation,
            daemon_draft,
        } => vec![
            "The daemon draft changed while this terminal was editing.".to_owned(),
            format!(
                "daemon generation: {}",
                daemon_generation.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
            ),
            chrome::hints(&[
                ("r", "reload, discarding local typed edits"),
                ("a", "reapply them to this daemon copy"),
                ("esc", "close"),
            ]),
            pretty(daemon_draft),
        ],
    }
}

/// The prompt every confirmation closes on.
fn confirm_hint() -> String {
    chrome::hints(&[("y/enter", "confirm"), ("n/esc", "cancel")])
}

fn activation_lines(prompt: &str, scope: usize, force_cache_transition: bool) -> Vec<String> {
    let scope = ["new lanes", "new and cold lanes", "immediate"][scope.min(2)];
    vec![
        prompt.to_owned(),
        format!("scope: {scope} · {}", chrome::hints(&[("h/l", "change")])),
        format!(
            "forced warm-cache transition: {} · {}",
            if force_cache_transition { "YES" } else { "no" },
            chrome::hints(&[("f", "toggle")])
        ),
        confirm_hint(),
    ]
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::{State, modal_text, render};
    use crate::tui::control::{Command, Reply, Response};

    #[test]
    fn page_names_edit_and_activation_controls() {
        let mut state = State::default();
        state.receive(Reply::Command(
            Command::snapshot("routing-snapshot"),
            Ok(Response {
                status: 200,
                body: serde_json::json!({
                    "draft_generation": 2,
                    "draft": {"policy": {"presentations": [{"slug": "friendly"}]}},
                    "active_revision": {"policy": {"presentations": []}}
                }),
            }),
        ));
        let backend = TestBackend::new(140, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render(frame, &state, false);
            })
            .expect("draw");
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!text.contains("n new"), "no page carries a footer: {text}");
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).expect("terminal");
        terminal
            .draw(|frame| {
                render(frame, &state, true);
            })
            .expect("draw");
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("edit the selected record"), "{text}");
        assert!(text.contains("activate the draft"), "{text}");
    }

    #[test]
    fn the_activation_form_shows_its_impact_and_the_last_refusal() {
        let mut state = State::default();
        state.receive(Reply::Command(
            Command::snapshot("routing-snapshot"),
            Ok(Response {
                status: 200,
                body: serde_json::json!({
                    "draft_generation": 2,
                    "draft": {"policy": {"routes": [{"id": "new-route"}]}},
                    "active_revision": {"policy": {"routes": []}},
                    "lane_impacts": {"total": 4, "unpinned": 1, "warm": 2, "cold": 1, "unknown": 0},
                    "publications": [{"adapter": "claude", "state": "failed"}],
                    "derived": {
                        "active": {"routes": [
                            {"id": "kept", "edges": [{"from": "a", "to": "b", "crosses_cost_boundary": true, "billed_boundary": "deny", "from_cost": "subscription", "to_cost": "billed"}]},
                            {"id": "gone", "edges": [{"from": "a", "to": "b", "crosses_cost_boundary": true, "billed_boundary": "allow"}]},
                        ]},
                        "draft": {"routes": [
                            {"id": "kept", "edges": [{"from": "a", "to": "b", "crosses_cost_boundary": true, "billed_boundary": "allow", "from_cost": "subscription", "to_cost": "billed"}]},
                            {"id": "new-route", "edges": [{"from": "a", "to": "b", "crosses_cost_boundary": true, "billed_boundary": "allow", "from_cost": "subscription", "to_cost": "unknown"}]},
                        ]},
                    },
                }),
            }),
        ));
        state.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let mut command = Command::snapshot("routing-snapshot");
        command.name = "activate";
        state.receive(Reply::Command(
            command,
            Ok(Response {
                status: 409,
                body: serde_json::json!({"error": "forced_cache_transition_required"}),
            }),
        ));
        let text = modal_text(&state).join("\n");
        assert!(text.contains("+route new-route"), "{text}");
        assert!(text.contains("2 warm"), "{text}");
        assert!(
            text.contains("cost boundary: kept: a -> b consent allow (was deny)"),
            "{text}"
        );
        assert!(
            text.contains(
                "cost boundary: new-route: a -> b now crosses subscription to unknown (consent allow)"
            ),
            "{text}"
        );
        assert!(
            text.contains("cost boundary: gone: a -> b removed"),
            "{text}"
        );
        assert!(text.contains("publication claude: failed"), "{text}");
        assert!(text.contains("forced_cache_transition_required"), "{text}");
        assert!(
            text.lines()
                .rev()
                .nth(1)
                .is_some_and(|line| line.contains("y/enter confirm · n/esc cancel")),
            "the confirmation prompt stays above the refusal: {text}"
        );
    }

    /// Every key the page acts on is named in its overlay, so a key that
    /// was added cannot stay undocumented (spec: UTUI#finding-the-keys).
    #[test]
    fn the_overlay_names_every_key_the_page_takes() {
        use crossterm::event::KeyCode;
        let named = |key: &str| {
            super::KEYS
                .iter()
                .any(|(keys, _)| keys.split([' ', '/']).any(|token| token == key))
        };
        for key in [
            'e', 'n', 'd', 'v', 'V', 'a', 'x', 'p', 'i', 'r', 'h', 'j', 'k', 'l',
        ] {
            assert!(
                !super::super::owns_key(KeyCode::Char(key)) || named(&key.to_string()),
                "{key} is taken but not named"
            );
        }
        assert!(named("pgup") && named("pgdn"));
        for key in ["tab", "alt+↑", "ctrl+d", "ctrl+s", "space"] {
            assert!(
                super::FORM_KEYS
                    .iter()
                    .any(|(keys, _)| keys.split([' ', '/']).any(|token| token == key)),
                "{key} in a form"
            );
        }

        let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("terminal");
        terminal
            .draw(|frame| {
                render(frame, &State::default(), true);
            })
            .expect("draw");
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for heading in ["KEYS", "IN A FORM", "? or esc close"] {
            assert!(text.contains(heading), "{heading}: {text}");
        }
    }

    #[test]
    fn the_add_forms_say_what_they_do() {
        let mut state = State::default();
        state.receive(Reply::Command(
            Command::snapshot("routing-snapshot"),
            Ok(Response {
                status: 200,
                body: serde_json::json!({
                    "draft_generation": 2,
                    "draft": {"policy": {"routes": []}},
                }),
            }),
        ));
        state.set_view(super::View::Routes);
        state.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('n'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let text = modal_text(&state).join("\n");
        assert!(text.contains("1 route"), "{text}");
        assert!(text.contains("2 default"), "{text}");
    }

    /// A snapshot with every kind of record and the derived facts for them,
    /// so each view's frame shows the page as it reads with a real policy.
    fn page_snapshot() -> serde_json::Value {
        let route = serde_json::json!({
            "id": "mixed", "display_name": "Subscription then billed",
            "lane_transition": "move_when_cold",
            "chain": {
                "first": {"id": "sub", "binding_id": "codex_sub.codex_responses",
                          "model": {"kind": "exact", "model_id": "gpt-example"}, "enabled": true},
                "steps": [{
                    "edge": {"preflight": "enabled", "post_send": ["rate_limit"],
                             "billed_boundary": "allow", "ambiguous_transport": "deny",
                             "cache_transition": "allow_cache_loss"},
                    "candidate": {"id": "billed", "binding_id": "openrouter.openrouter_responses",
                                  "model": {"kind": "requested_model"}, "enabled": true},
                }],
            },
        });
        let policy = serde_json::json!({
            "description": "example draft",
            "presentations": [
                {"id": "p-mixed", "slug": "mixed", "label": "Mixed",
                 "scope": {"kind": "protocol", "protocol": "openai_responses"},
                 "source": {"kind": "virtual", "target": {"route_id": "mixed"}}, "enabled": true},
            ],
            "routes": [route],
            "defaults": [{"scope": {"kind": "protocol", "protocol": "openai_responses"},
                          "target": {"route_id": "mixed"}}],
            "rewrites": [{"id": "r-sonnet", "scope": {"kind": "protocol", "protocol": "openai_responses"},
                          "priority": 5, "model_matcher": {"kind": "glob", "value": "gpt-*"},
                          "target": {"route_id": "mixed"}}],
            "classifiers": [{"id": "gpt-family", "binding_id": "codex_sub.codex_responses",
                             "model_matcher": {"kind": "regex", "value": "^gpt-(?P<family>[a-z]+)-(?P<version>[0-9.]+)$"},
                             "family_template": "${family}", "version_template": "${version}",
                             "version_parser": {"kind": "numeric_tuple", "components": 2, "separator": "."}}],
            "upgrades": [{"id": "newest", "classifier_id": "gpt-family", "enabled": true,
                          "lane_transition": "move_when_cold"}],
        });
        let derived_policy = serde_json::json!({
            "routes": [{
                "id": "mixed",
                "pinned_lanes": {"warm": 2, "cold": 1, "unknown": 0},
                "candidates": [
                    {"id": "sub", "binding_id": "codex_sub.codex_responses", "cost_class": "subscription",
                     "upgrade_policy_id": "newest",
                     "upgrade": {"baseline": "gpt-example-1.0", "winner": "gpt-example-1.2",
                                 "rejected": [{"model": "gpt-example-1.1", "reason": "not_newer"},
                                              {"model": "gpt-other-2.0", "reason": "family_mismatch"}]}},
                    {"id": "billed", "binding_id": "openrouter.openrouter_responses", "cost_class": "billed",
                     "upgrade_policy_id": null, "upgrade": null},
                ],
                "edges": [{"from": "sub", "to": "billed", "from_cost": "subscription", "to_cost": "billed",
                           "crosses_cost_boundary": true, "billed_boundary": "allow"}],
            }],
            "classifiers": [{"id": "gpt-family", "binding_id": "codex_sub.codex_responses",
                             "parsed": [{"model": "gpt-example-1.0", "family": "example", "version": "1.0"}],
                             "unparsed": [{"model": "gpt-example-latest"}]}],
            "presentations": [{"scope": "openai_responses", "slug": "mixed",
                               "adapter": "dynamic_responses_models",
                               "desired": "left_out", "reason": "no_codex_record", "applied": null}],
        });
        serde_json::json!({
            "draft_generation": 4,
            "draft": {"policy": policy},
            "active_revision": {"revision_id": 3, "policy": {}},
            "draft_diagnostics": [
                {"code": "virtual-presentation-unlisted-in-responses", "severity": "info",
                 "record_id": "p-mixed", "path": "/presentations/0/metadata"},
            ],
            "lane_impacts": {"total": 4, "unpinned": 1, "warm": 2, "cold": 1, "unknown": 0},
            "publications": [{"adapter": "dynamic_responses_models", "state": "applied",
                              "scope": {"kind": "protocol", "protocol": "openai_responses"},
                              "applied_revision_id": 3, "desired_revision_id": 3,
                              "applied_inventory_generation": 7, "desired_inventory_generation": 7}],
            "derived": {
                "availability": [{
                    "binding_id": "codex_sub.codex_responses", "provider": "codex_sub",
                    "configured": {"state": "yes", "reason": "configured"},
                    "enabled": {"state": "yes", "reason": "enabled"},
                    "credential": {"state": "unknown"}, "catalogue": {"state": "fresh"},
                    "capabilities": {"sampling": "supported", "tools": "supported", "images": "unknown"},
                    "cost_class": "subscription",
                }, {
                    "binding_id": "openrouter.openrouter_responses", "provider": "openrouter",
                    "configured": {"state": "no", "reason": "unconfigured"},
                    "enabled": {"state": "no", "reason": "disabled"},
                    "credential": {"state": "no", "reason": "provider-unconfigured"},
                    "catalogue": {"state": "absent"}, "capabilities": {}, "cost_class": "billed",
                }],
                "circuits": [],
                "draft": derived_policy,
                "active": null,
            },
        })
    }

    fn frame(state: &State) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).expect("terminal");
        terminal
            .draw(|frame| {
                render(frame, state, false);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn page_in(view: super::View) -> State {
        let mut state = State::default();
        state.receive(Reply::Command(
            Command::snapshot("routing-snapshot"),
            Ok(Response {
                status: 200,
                body: page_snapshot(),
            }),
        ));
        state.set_view(view);
        state
    }

    /// The page at `width`×`height`, and where it put things.
    fn drawn_at(state: &State, width: u16, height: u16) -> (String, super::Drawn) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        let mut drawn = super::Drawn::default();
        terminal
            .draw(|frame| drawn = render(frame, state, false))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let text = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, drawn)
    }

    /// Below 100 columns the list sits over the detail pane rather than
    /// beside it, where both would be too narrow to read.
    #[test]
    fn a_narrow_page_stacks_the_list_over_the_detail() {
        let state = page_in(super::View::Routes);
        let (_, wide) = drawn_at(&state, 100, 32);
        assert_eq!(wide.entries.y, wide.detail.y, "side by side from 100");
        assert!(wide.entries.right() <= wide.detail.x);
        let (text, narrow) = drawn_at(&state, 99, 32);
        assert_eq!(narrow.entries.x, narrow.detail.x, "stacked below:\n{text}");
        assert!(narrow.entries.bottom() <= narrow.detail.y, "{text}");
        assert!(narrow.entries.height <= 32 / 3, "{text}");
    }

    /// The other tabs collapse to their number before the current label
    /// clips, and every tab stays clickable.
    #[test]
    fn a_narrow_header_collapses_the_other_tabs() {
        let state = page_in(super::View::Routes);
        let (text, drawn) = drawn_at(&state, 90, 32);
        let header = text.lines().next().unwrap_or_default();
        assert!(
            header.contains("1  2 routes, candidates & defaults  3  4"),
            "{header}"
        );
        assert_eq!(drawn.tabs.len(), 4);
    }

    /// Too short for the detail boxed, both panes give up their borders
    /// before any line of content (spec: UTUI#what-is-shed-first).
    #[test]
    fn a_short_page_goes_ruled() {
        let state = page_in(super::View::Routes);
        let (tall, _) = drawn_at(&state, 120, 40);
        assert!(
            tall.contains("\u{250c}DETAIL"),
            "boxed while it fits:\n{tall}"
        );
        let (short, _) = drawn_at(&state, 120, 12);
        assert!(short.contains("DETAIL \u{2500}"), "ruled:\n{short}");
        assert!(!short.contains('\u{250c}'), "{short}");
    }

    #[test]
    fn the_presentations_page() {
        insta::assert_snapshot!(frame(&page_in(super::View::Presentations)));
    }

    #[test]
    fn the_routes_page() {
        insta::assert_snapshot!(frame(&page_in(super::View::Routes)));
    }

    #[test]
    fn the_rewrites_page_on_a_classifier() {
        let mut state = page_in(super::View::Rewrites);
        state.move_selection(1);
        insta::assert_snapshot!(frame(&state));
    }

    #[test]
    fn the_rewrites_page_on_an_upgrade() {
        let mut state = page_in(super::View::Rewrites);
        state.move_selection(2);
        insta::assert_snapshot!(frame(&state));
    }

    #[test]
    fn the_status_page_on_a_binding() {
        let mut state = page_in(super::View::Status);
        // active revision, draft, publication, then the bindings
        state.move_selection(3);
        insta::assert_snapshot!(frame(&state));
    }

    #[test]
    fn the_activation_form() {
        let mut state = page_in(super::View::Routes);
        state.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        ));
        insta::assert_snapshot!(frame(&state));
    }

    #[test]
    fn the_detail_pane_scrolls_to_its_last_line_and_no_further() {
        let mut state = page_in(super::View::Routes);
        // A short terminal so the route's detail does not fit.
        let mut terminal = Terminal::new(TestBackend::new(120, 14)).expect("terminal");
        let draw = |terminal: &mut Terminal<TestBackend>, state: &State| {
            terminal
                .draw(|frame| {
                    render(frame, state, false);
                })
                .expect("draw");
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        assert!(!draw(&mut terminal, &state).contains("model: the requested model"));
        for _ in 0..5 {
            state.key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageDown,
                crossterm::event::KeyModifiers::NONE,
            ));
            draw(&mut terminal, &state);
        }
        let scrolled = state.detail_scroll();
        assert!(draw(&mut terminal, &state).contains("model: the requested model"));
        assert!(
            scrolled > 0 && scrolled < 50,
            "stops at the end: {scrolled}"
        );
        state.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::PageUp,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(state.detail_scroll() < scrolled);
        // Moving to another item starts it at the top.
        state.move_selection(1);
        assert_eq!(state.detail_scroll(), 0);
    }

    fn press_in(state: &mut State, code: crossterm::event::KeyCode) {
        state.key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ));
    }

    /// Open the add form for the `digit`th kind of `view`.
    fn adding(view: super::View, digit: char) -> State {
        let mut state = page_in(view);
        press_in(&mut state, crossterm::event::KeyCode::Char('n'));
        if state.modal().title() == "ADD POLICY RECORD" {
            press_in(&mut state, crossterm::event::KeyCode::Char(digit));
        }
        state
    }

    #[test]
    fn the_route_form() {
        let mut state = page_in(super::View::Routes);
        press_in(&mut state, crossterm::event::KeyCode::Char('e'));
        insta::assert_snapshot!(frame(&state));
    }

    #[test]
    fn the_route_form_scrolled_to_a_later_field() {
        let mut state = page_in(super::View::Routes);
        press_in(&mut state, crossterm::event::KeyCode::Char('e'));
        for _ in 0..24 {
            press_in(&mut state, crossterm::event::KeyCode::Down);
        }
        insta::assert_snapshot!(frame(&state));
    }

    #[test]
    fn the_presentation_form() {
        insta::assert_snapshot!(frame(&adding(super::View::Presentations, '1')));
    }

    #[test]
    fn the_default_form() {
        insta::assert_snapshot!(frame(&adding(super::View::Routes, '2')));
    }

    #[test]
    fn the_rewrite_form() {
        insta::assert_snapshot!(frame(&adding(super::View::Rewrites, '1')));
    }

    #[test]
    fn the_classifier_form() {
        insta::assert_snapshot!(frame(&adding(super::View::Rewrites, '2')));
    }

    #[test]
    fn the_upgrade_form() {
        insta::assert_snapshot!(frame(&adding(super::View::Rewrites, '3')));
    }

    #[test]
    fn the_compaction_form() {
        insta::assert_snapshot!(frame(&adding(super::View::Rewrites, '4')));
    }

    #[test]
    fn the_promotion_and_preview_forms() {
        let mut state = page_in(super::View::Status);
        press_in(&mut state, crossterm::event::KeyCode::Char('p'));
        insta::assert_snapshot!("the_promotion_form", frame(&state));
        press_in(&mut state, crossterm::event::KeyCode::Esc);
        press_in(&mut state, crossterm::event::KeyCode::Char('v'));
        insta::assert_snapshot!("the_preview_form", frame(&state));
    }

    #[test]
    fn a_form_with_completions_and_a_refusal() {
        let mut state = page_in(super::View::Rewrites);
        press_in(&mut state, crossterm::event::KeyCode::Char('n'));
        press_in(&mut state, crossterm::event::KeyCode::Char('2'));
        // The classifier's binding, searched for.
        press_in(&mut state, crossterm::event::KeyCode::Down);
        for _ in 0..40 {
            press_in(&mut state, crossterm::event::KeyCode::Backspace);
        }
        for character in "codex".chars() {
            press_in(&mut state, crossterm::event::KeyCode::Char(character));
        }
        let mut command = Command::snapshot("routing-snapshot");
        command.name = "draft-edit";
        state.receive(Reply::Command(
            command,
            Ok(Response {
                status: 422,
                body: serde_json::json!({"error": "invalid_policy_edit", "detail": "id already exists"}),
            }),
        ));
        insta::assert_snapshot!(frame(&state));
    }

    /// A click on a form line focuses that field, and a click on a line that
    /// is only a heading or a completion focuses nothing.
    #[test]
    fn a_click_focuses_the_field_on_its_line() {
        let mut state = adding(super::View::Presentations, '1');
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).expect("terminal");
        let mut hit = None;
        terminal
            .draw(|frame| hit = render(frame, &state, false).form)
            .expect("draw");
        let hit = hit.expect("a form is open");
        let lines = frame(&state);
        let rows: Vec<&str> = lines.lines().skip(usize::from(hit.body.y)).collect();
        let on = |needle: &str| {
            rows.iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle:?}"))
        };
        let slug = hit.rows[on("slug:")].expect("the slug line is a field");
        assert!(hit.rows[on("SCOPE")].is_none(), "a heading is not a field");
        state.focus_form_row(slug);
        let focused: Vec<String> = match state.modal() {
            super::Modal::Form(form) => form
                .lines()
                .into_iter()
                .filter(|line| line.focused)
                .map(|line| line.text)
                .collect(),
            _ => panic!("a form is open"),
        };
        assert_eq!(focused.len(), 1);
        assert!(focused[0].contains("slug"), "{focused:?}");
    }

    #[test]
    fn clicking_an_entry_selects_it_and_the_list_scrolls_to_the_selection() {
        let mut state = page_in(super::View::Status);
        state.select(2);
        assert_eq!(state.selected(), 2);
        // Out of range is ignored rather than clamped to a different entry.
        state.select(99);
        assert_eq!(state.selected(), 2);
        // A short list keeps the selection in view, and a click on a row
        // names the entry drawn there rather than one counted from the top.
        let mut terminal = Terminal::new(TestBackend::new(120, 8)).expect("terminal");
        let mut drawn = super::Drawn::default();
        state.select(5);
        terminal
            .draw(|frame| drawn = render(frame, &state, false))
            .expect("draw");
        let selected = drawn
            .rows
            .iter()
            .find(|hit| hit.entry == 5)
            .expect("row 5 is drawn in an 8-line terminal");
        assert!(
            drawn.rows[0].entry > 0,
            "the list scrolled to reach row 5: {:?}",
            drawn.rows
        );
        state.select(0);
        assert_eq!(
            crate::tui::chrome::entry_at(&drawn.rows, selected.area.as_position()),
            Some(5)
        );
    }
}
