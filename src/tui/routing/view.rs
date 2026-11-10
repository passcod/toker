//! Routing page rendering, including content-free typed modal forms.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Clear, Paragraph, Wrap},
};
use serde_json::Value;

use super::data::array;
use super::form::Form;
use super::model::{Modal, Mode, State, Tab};
use super::rules::{self, Entry};
use super::table::{self, Table};
use super::template::singular;
use super::{history, models_tab, routes, status};
use crate::tui::chrome::{self, Chrome, RowHit};

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

pub(super) fn render(frame: &mut Frame, state: &State, overlay: bool) -> Drawn {
    let labels: Vec<chrome::Tab> = Tab::ALL
        .iter()
        .map(|tab| {
            let strip = chrome::Tab::new(tab.label()).after(tab.joiner());
            match state.tab_count(*tab) {
                Some(count) => strip.count(count),
                None => strip,
            }
        })
        .collect();
    let status = status::text_of(
        state.snapshot(),
        state.active_revision_id(),
        state.in_flight(),
        state.settings_note(),
    );
    // A refusal or a failed read is said over the page, which stays
    // usable read-only on the snapshot it last loaded.
    let chrome::Laid { header, tabs, body } = chrome::page(
        frame,
        crate::tui::Page::Routing,
        state.error(),
        &chrome::Strip {
            tabs: &labels,
            current: state.tab().index(),
            status: &status,
        },
    );

    let mut drawn = Drawn {
        header,
        tabs,
        ..Drawn::default()
    };
    match (state.snapshot(), state.active()) {
        (Some(snapshot), Some(active)) => match (state.tab(), state.mode()) {
            (Tab::Routes, Mode::Route) => route_page(frame, body, snapshot, active, state),
            (Tab::Models, _) => models_tab::render(frame, body, state, active),
            (Tab::History, _) => history_tab(frame, body, snapshot, state, &mut drawn),
            _ => lookup_tab(frame, body, snapshot, active, state, &mut drawn),
        },
        (Some(snapshot), None) => {
            let note = if state.tab() == Tab::History {
                history_tab(frame, body, snapshot, state, &mut drawn);
                None
            } else {
                Some("the live revision has no derived facts: it did not compile")
            };
            if let Some(note) = note {
                message(frame, body, state.tab().title(), note);
            }
        }
        (None, _) => {
            let note = if state.loading() {
                "loading routing snapshot…"
            } else {
                "no routing snapshot: the service is not answering"
            };
            message(frame, body, state.tab().title(), note);
        }
    }

    chrome::chips(frame, &drawn.header);
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
    drawn.form = if state.modal().active() {
        render_modal(frame, state)
    } else {
        None
    };
    drawn
}

/// A panel with one dim line in it, where there is nothing to draw.
fn message(frame: &mut Frame, body: Rect, title: &str, note: &str) {
    let framing = Chrome::fitting(1, body.height);
    let area = Rect {
        height: (1 + framing.cost()).min(body.height),
        ..body
    };
    table::draw_detail(
        frame,
        area,
        title,
        framing,
        vec![Line::styled(note.to_owned(), Style::new().dim())],
    );
}

/// The inner width a table's lines are laid to: the panel's, less a cell of
/// air either side.
fn content_width(body: Rect) -> usize {
    usize::from(body.width.saturating_sub(4))
}

/// A lookup tab: its table, and the selected record's detail under it.
fn lookup_tab(
    frame: &mut Frame,
    body: Rect,
    snapshot: &Value,
    active: &Value,
    state: &State,
    drawn: &mut Drawn,
) {
    let width = content_width(body);
    let tab = state.tab();
    let entries = rules::entries(active, tab);
    let selected = state.selected_in(tab);
    let entry = entries.get(selected).copied();
    let (table, mut detail, title): (Table, Vec<Line<'static>>, &str) = match tab {
        Tab::Presentations => (
            rules::presentations_table(active, width),
            match entry {
                Some(Entry::Presentation(record)) => rules::presentation_detail(active, record),
                _ => Vec::new(),
            },
            "PRESENTATION",
        ),
        Tab::Rewrites => (
            rules::rewrites_table(active, width),
            match entry {
                Some(Entry::Rewrite(rule)) => {
                    rules::rule_detail(active, rule, &rules::rules_before(active, rule))
                }
                Some(Entry::Default(default)) => rules::default_detail(active, default),
                _ => Vec::new(),
            },
            if matches!(entry, Some(Entry::Default(_))) {
                "DEFAULT"
            } else {
                "RULE"
            },
        ),
        Tab::Defaults => (
            rules::defaults_table(active, width),
            match entry {
                Some(Entry::Default(default)) => rules::default_detail(active, default),
                _ => Vec::new(),
            },
            "DEFAULT",
        ),
        Tab::Routes => (
            routes::routes_table(active, width),
            array(active, "routes")
                .get(selected)
                .map(|route| routes::route_detail(snapshot, route))
                .unwrap_or_default(),
            "ROUTE",
        ),
        Tab::Models | Tab::History => unreachable!("drawn by their own tabs"),
    };
    if table.body.is_empty() {
        detail = Vec::new();
    }
    let split = table::split(body, 1 + table.body.len().max(1), detail.len());
    drawn.entries = split.table;
    drawn.detail = split.detail;
    if table.body.is_empty() {
        table::draw_detail(
            frame,
            split.table,
            tab.title(),
            split.chrome,
            vec![Line::styled(
                format!("no {} in the live revision", tab.label()),
                Style::new().dim(),
            )],
        );
        return;
    }
    drawn.rows = table::draw_table(
        frame,
        split.table,
        tab.title(),
        split.chrome,
        table,
        Some(selected),
    );
    if split.detail.height > 0 {
        table::draw_detail(frame, split.detail, title, split.chrome, detail);
    }
}

/// One route, whole.
fn route_page(frame: &mut Frame, body: Rect, snapshot: &Value, active: &Value, state: &State) {
    let routes = array(active, "routes");
    let at = state.selected_in(Tab::Routes);
    let Some(route) = routes.get(at) else {
        return;
    };
    let lines = routes::route_page(snapshot, route, at, routes.len(), content_width(body));
    let framing = Chrome::fitting(lines.len(), body.height);
    let area = Rect {
        height: (lines.len() as u16 + framing.cost()).min(body.height),
        ..body
    };
    table::draw_detail(frame, area, "ROUTE", framing, lines);
}

/// The history tab: the revisions, and the selected one's detail.
fn history_tab(frame: &mut Frame, body: Rect, snapshot: &Value, state: &State, drawn: &mut Drawn) {
    let revisions = state.revisions();
    let live = state.active_revision_id();
    if revisions.is_empty() {
        message(frame, body, Tab::History.title(), "no revisions read yet");
        return;
    }
    let selected = state.selected_in(Tab::History);
    let table = history::table(snapshot, revisions, live, content_width(body));
    let detail = history::detail(
        snapshot,
        &revisions[selected.min(revisions.len() - 1)],
        live,
    );
    let split = table::split(body, 1 + table.body.len(), detail.len());
    drawn.entries = split.table;
    drawn.detail = split.detail;
    drawn.rows = table::draw_table(
        frame,
        split.table,
        Tab::History.title(),
        split.chrome,
        table,
        Some(selected),
    );
    if split.detail.height > 0 {
        table::draw_detail(frame, split.detail, "REVISION", split.chrome, detail);
    }
}

/// The page's keys, in its `?` overlay.
const KEYS: [(&str, &str); 14] = [
    ("↑/↓ j/k", "select a record"),
    ("←/→ h/l 1–6", "switch tab; ←/→ also step between routes"),
    ("enter", "open the route a record sends to"),
    ("esc", "back from a route, then to the dashboard"),
    ("pgup/pgdn", "move a page of records"),
    (
        "n",
        "add a record, asking which kind where a view lists several",
    ),
    ("e", "edit the selected record in a form"),
    ("d", "remove the selected record"),
    ("v", "preview a request"),
    ("a", "reactivate the selected revision"),
    ("p", "promote"),
    ("r", "refresh"),
    ("click", "select a record, or focus a form field"),
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
    // A pretty-printed result is one string of many lines, and the popup is
    // sized in lines drawn, not in strings.
    let lines: Vec<String> = modal_text(state)
        .iter()
        .flat_map(|text| text.split('\n').map(str::to_owned))
        .collect();
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
    if matches!(modal, Modal::Reactivate { .. }) {
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
            format!("Remove this {key} record? Saving it makes a new revision live."),
            short(value),
            confirm_hint(),
        ],
        Modal::PreviewResult(result) => {
            vec![chrome::hints(&[("any key", "close")]), pretty(result)]
        }
        Modal::Reactivate { revision_id } => vec![
            format!("Create a forward revision from revision {revision_id}?"),
            "The active pointer never moves backwards.".to_owned(),
            confirm_hint(),
        ],
    }
}

/// The prompt every confirmation closes on.
fn confirm_hint() -> String {
    chrome::hints(&[("y/enter", "confirm"), ("n/esc", "cancel")])
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::{Drawn, State, Tab, render};
    use crate::tui::control::{Command, Reply, Response};
    use crate::tui::routing::testing::{self, World};

    /// A page that has read the snapshot and revisions of `world`, on `tab`.
    fn page(world: World, tab: Tab) -> State {
        let mut state = State::default();
        let mut reply = |name: &'static str, body| {
            let mut command = Command::snapshot("routing-snapshot");
            command.name = name;
            state.receive(Reply::Command(command, Ok(Response { status: 200, body })));
        };
        reply("snapshot", testing::snapshot(world));
        reply("revisions", testing::revisions(world));
        state.set_tab(tab);
        state
    }

    /// The page at `width`×`height` as text, and where it put things.
    fn drawn_at(state: &State, width: u16, height: u16) -> (String, Drawn) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        let mut drawn = Drawn::default();
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

    fn frame(state: &State, width: u16, height: u16) -> String {
        drawn_at(state, width, height).0
    }

    /// Rewrites at rest, as drawn for a 120×40 terminal, the second rule
    /// selected.
    #[test]
    fn rewrites_at_rest_120x40() {
        let mut state = page(World::AT_REST, Tab::Rewrites);
        state.move_selection(1);
        insta::assert_snapshot!(frame(&state, 120, 40));
    }

    #[test]
    fn rewrites_at_rest_135x50() {
        let mut state = page(World::AT_REST, Tab::Rewrites);
        state.move_selection(1);
        insta::assert_snapshot!(frame(&state, 135, 50));
    }

    #[test]
    fn rewrites_after_a_save_120x40() {
        let mut state = page(World::SAVED, Tab::Rewrites);
        state.move_selection(5);
        insta::assert_snapshot!(frame(&state, 120, 40));
    }

    #[test]
    fn presentations_120x40() {
        let mut state = page(World::AT_REST, Tab::Presentations);
        state.move_selection(4);
        insta::assert_snapshot!(frame(&state, 120, 40));
    }

    #[test]
    fn defaults_120x40() {
        let state = page(World::FULL, Tab::Defaults);
        insta::assert_snapshot!(frame(&state, 120, 40));
    }

    #[test]
    fn routes_120x40() {
        let state = page(World::FULL, Tab::Routes);
        insta::assert_snapshot!(frame(&state, 120, 40));
    }

    #[test]
    fn the_route_page_120x40() {
        let mut state = page(World::FULL, Tab::Routes);
        state.open();
        insta::assert_snapshot!(frame(&state, 120, 40));
    }

    #[test]
    fn models_with_the_focus_in_upgrades_120x40() {
        let mut state = page(World::AT_REST, Tab::Models);
        state.show_offers(testing::OR_CHAT, 180);
        insta::assert_snapshot!(frame(&state, 120, 40));
    }

    #[test]
    fn models_with_the_focus_in_families_120x40() {
        let mut state = page(World::AT_REST, Tab::Models);
        state.show_offers(testing::OR_CHAT, 180);
        state.move_selection(-1);
        insta::assert_snapshot!(frame(&state, 120, 40));
    }

    #[test]
    fn history_120x40() {
        let state = page(World::SAVED, Tab::History);
        insta::assert_snapshot!(frame(&state, 120, 40));
    }
}
