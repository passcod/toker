//! Routing page rendering: the tabs, the routes, and the form that opens
//! under a row to edit it.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Clear, Paragraph, Wrap},
};
use serde_json::Value;

use super::data::array;
use super::edit_view;
use super::editing::{Editing, What};
use super::form::Form;
use super::model::{Modal, Mode, State, Tab};
use super::rules::{self, Entry};
use super::table::{self, BodyLine, Detail, Table};
use super::{history, keys, models_tab, routes, status};
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

/// An open form's visible fields: where each is, and the form's row it
/// focuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FormHit {
    pub(super) fields: Vec<(Rect, usize)>,
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
        state.lead(),
        state.settings_note().or(state.notice()),
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
            (Tab::Routes, Mode::Route) => {
                route_page(frame, body, snapshot, active, state, &mut drawn);
            }
            (Tab::Models, _) if state.editing().is_some() => {
                edit_page(frame, body, state, active, &mut drawn);
            }
            (Tab::Models, _) => {
                // A trace sits under the stack of panels, which gives up the
                // room it takes.
                let trace = state.trace().map(|trace| {
                    trace.lines(snapshot, state.active_revision_id(), content_width(body))
                });
                match trace {
                    Some(trace) => {
                        let split =
                            table::split(body, body.height.saturating_sub(2).into(), trace.len());
                        models_tab::render(frame, split.table, state, active);
                        if split.detail.height > 0 {
                            table::draw_detail(frame, split.detail, "TRACE", split.chrome, trace);
                        }
                    }
                    None => models_tab::render(frame, body, state, active),
                }
            }
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
        // Under the header, so the buttons stay in view.
        let area = frame.area();
        let used = drawn.header.keys.height;
        keys::render(
            frame,
            Rect {
                y: area.y + used,
                height: area.height - used,
                ..area
            },
        );
    }
    if state.modal().active() {
        drawn.form = render_modal(frame, state);
    }
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

/// The editing that belongs to the table the tab shows, when there is one.
fn editing_here(state: &State) -> Option<&Editing> {
    state.editing()
}

/// A lookup tab: its table, and the selected record's detail under it, or,
/// while the record is edited, its form under the row and the preview of
/// what the edit would do.
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
    let editing = editing_here(state);
    // A rewrite being moved is shown where it would land.
    let moved;
    let active = match editing.map(|editing| &editing.what) {
        Some(What::MoveRule {
            scope_label,
            from,
            to,
            ..
        }) if tab == Tab::Rewrites => {
            moved = rules::with_move(active, scope_label, *from, *to);
            &moved
        }
        _ => active,
    };
    let entries = rules::entries(active, tab);
    let selected = state.selected_in(tab);
    let entry = entries.get(selected).copied();
    let (mut table, mut detail, title): (Table, Detail, &str) = match tab {
        Tab::Presentations => (
            rules::presentations_table(active, width),
            Detail::whole(match entry {
                Some(Entry::Presentation(record)) => rules::presentation_detail(active, record),
                _ => Vec::new(),
            }),
            "PRESENTATION",
        ),
        Tab::Rewrites => (
            rules::rewrites_table(active, width),
            match entry {
                Some(Entry::Rewrite(rule)) => rules::with_reading(
                    rules::rule_detail(active, rule, &rules::rules_before(active, rule)),
                    rule.get("reach").unwrap_or(&Value::Null),
                    "[enter] open route",
                ),
                Some(Entry::Default(default)) => default_reading(active, default),
                _ => Detail::whole(Vec::new()),
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
                Some(Entry::Default(default)) => default_reading(active, default),
                _ => Detail::whole(Vec::new()),
            },
            "DEFAULT",
        ),
        Tab::Routes => (
            // A trace marks the route it walked, which takes a gutter.
            routes::routes_table(active, width - 2 * usize::from(state.trace().is_some())),
            Detail::whole(
                array(active, "routes")
                    .get(selected)
                    .map(|route| routes::route_detail(snapshot, route, width))
                    .unwrap_or_default(),
            ),
            "ROUTE",
        ),
        Tab::Models | Tab::History => unreachable!("drawn by their own tabs"),
    };
    let mut title = title;
    let live = state.active_revision_id();
    if let Some(trace) = state.trace() {
        let marks = trace.marks(active, tab, live);
        table::mark_rows(&mut table, &marks, tab == Tab::Routes);
        if editing.is_none() {
            detail = Detail::whole(trace.lines(snapshot, live, width));
            title = "TRACE";
        }
    }
    if let Some(editing) = editing {
        // The form opens under the row it belongs to, and the preview takes
        // the detail's place.
        let at = table
            .body
            .iter()
            .position(|line| line.entry == Some(selected))
            .map_or(table.body.len(), |at| at + 1);
        let form = edit_view::block(editing, width);
        table.body.splice(at..at, form);
        detail = Detail::whole(edit_view::preview(editing, active, width));
        title = "PREVIEW";
    }
    // A rule or a default sends to a route, so with none there is nothing to
    // add and nothing to read about what exists: the table says where to go.
    if editing.is_none()
        && matches!(tab, Tab::Rewrites | Tab::Defaults)
        && array(active, "routes").is_empty()
    {
        table.body.push(rules::needs_a_route());
        detail = Detail::whole(Vec::new());
    }
    if table.body.is_empty() {
        detail = Detail::whole(Vec::new());
    }
    let need = 1 + table.body.len().max(1);
    // A detail with a reading gives up its lines to keep the table whole.
    let split = if detail.has_reading() && editing.is_none() && state.trace().is_none() {
        table::split_reading(body, need, detail.len())
    } else {
        table::split(body, need, detail.len())
    };
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
    let hits = table::draw_table(
        frame,
        split.table,
        tab.title(),
        split.chrome,
        table,
        Some(selected),
    );
    drawn.rows = hits.rows;
    if editing.is_some() {
        drawn.form = Some(FormHit {
            fields: hits.fields,
        });
    }
    if split.detail.height > 0 {
        let lines = detail.shed(split.detail_rows, content_width(split.detail));
        if split.bare {
            table::draw_bare(frame, split.detail, lines);
        } else {
            table::draw_detail(frame, split.detail, title, split.chrome, lines);
        }
    }
}

/// A default's detail, its reach the reading: a default that is not there
/// is added from the same place.
fn default_reading(active: &Value, default: &Value) -> Detail {
    let hint = if default.get("exists") == Some(&Value::Bool(true)) {
        "[enter] open route"
    } else {
        "[n] new"
    };
    rules::with_reading(
        rules::default_detail(active, default),
        default.get("reach").unwrap_or(&Value::Null),
        hint,
    )
}

/// One route, whole; with a candidate being edited, the candidates and the
/// form, and the preview under them.
fn route_page(
    frame: &mut Frame,
    body: Rect,
    snapshot: &Value,
    active: &Value,
    state: &State,
    drawn: &mut Drawn,
) {
    let routes = array(active, "routes");
    let at = state.selected_in(Tab::Routes);
    let Some(route) = routes.get(at) else {
        return;
    };
    let width = content_width(body);
    let editing = editing_here(state);
    let moved;
    let route = match editing.map(|editing| &editing.what) {
        Some(What::MoveCandidate { from, to, .. }) => {
            moved = routes::with_move(route, *from, *to);
            &moved
        }
        _ => route,
    };
    let top = routes::route_top(snapshot, route, at, routes.len(), width);
    // The selected candidate is the one the edit keys act on.
    let selected_line = routes::CANDIDATES_AT
        + routes::candidate_rows(route, width)
            .get(state.candidate())
            .copied()
            .unwrap_or(0);
    let mut lines: Vec<BodyLine> = top
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if index == selected_line && editing.is_none() {
                BodyLine::form(table::reversed(line, width), None, false)
            } else {
                BodyLine::plain(line)
            }
        })
        .collect();
    let Some(editing) = editing else {
        lines.extend(
            routes::route_rest(route, width)
                .into_iter()
                .map(BodyLine::plain),
        );
        if let Some(trace) = state.trace() {
            let trace = trace.lines(snapshot, state.active_revision_id(), width);
            let split = table::split(body, lines.len(), trace.len());
            drawn.entries = split.table;
            drawn.detail = split.detail;
            table::draw_page(frame, split.table, "ROUTE", split.chrome, lines);
            if split.detail.height > 0 {
                table::draw_detail(frame, split.detail, "TRACE", split.chrome, trace);
            }
            return;
        }
        let framing = Chrome::fitting(lines.len(), body.height);
        let area = Rect {
            height: (lines.len() as u16 + framing.cost()).min(body.height),
            ..body
        };
        table::draw_page(frame, area, "ROUTE", framing, lines);
        return;
    };
    lines.extend(edit_view::block(editing, width));
    let preview = edit_view::preview(editing, active, width);
    let split = table::split(body, lines.len(), preview.len());
    drawn.entries = split.table;
    drawn.detail = split.detail;
    let fields = table::draw_page(frame, split.table, "ROUTE", split.chrome, lines);
    drawn.form = Some(FormHit { fields });
    if split.detail.height > 0 {
        table::draw_detail(frame, split.detail, "PREVIEW", split.chrome, preview);
    }
}

/// An edit on the models tab, whose records are not rows of a table: the
/// form takes the page, with the preview under it.
fn edit_page(frame: &mut Frame, body: Rect, state: &State, active: &Value, drawn: &mut Drawn) {
    let Some(editing) = editing_here(state) else {
        return;
    };
    let width = content_width(body);
    let lines = edit_view::block(editing, width);
    let preview = edit_view::preview(editing, active, width);
    let split = table::split(body, lines.len(), preview.len());
    drawn.entries = split.table;
    drawn.detail = split.detail;
    let fields = table::draw_page(frame, split.table, "MODELS", split.chrome, lines);
    drawn.form = Some(FormHit { fields });
    if split.detail.height > 0 {
        table::draw_detail(frame, split.detail, "PREVIEW", split.chrome, preview);
    }
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
    let mut detail = history::detail(
        snapshot,
        &revisions[selected.min(revisions.len() - 1)],
        live,
        state.armed(),
    );
    let mut detail_title = "REVISION";
    if let Some(trace) = state.trace() {
        detail = trace.lines(snapshot, live, content_width(body));
        detail_title = "TRACE";
    }
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
    )
    .rows;
    if split.detail.height > 0 {
        table::draw_detail(frame, split.detail, detail_title, split.chrome, detail);
    }
}

/// A small question over the page: a promotion's or a description's form,
/// or the confirmation of a reactivation.
fn render_modal(frame: &mut Frame, state: &State) -> Option<FormHit> {
    let modal = state.modal();
    if let Modal::Form(form) = modal {
        return Some(render_form(frame, state, form));
    }
    // The popup is sized in lines drawn, not in strings.
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

/// A one-line form is a popup: its fields in a window that follows the
/// focus, then its key help and the last refusal.
fn render_form(frame: &mut Frame, state: &State, form: &Form) -> FormHit {
    let area = frame.area();
    let width = area.width.saturating_sub(4).clamp(1, 110).min(area.width);
    let height = area.height.saturating_sub(2).clamp(1, 14).min(area.height);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(Modal::Form(Box::new(form.clone())).title());
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
            let said: Vec<&str> = line.notes.iter().map(|(_, text)| text.as_str()).collect();
            Line::styled(format!("{}  {}", line.text, said.join(" · ")), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(rendered), body);

    let mut notes = vec![Line::styled(form.help(), Style::new().dim())];
    if let Some(note) = form.note().or_else(|| state.error()) {
        notes.push(Line::styled(format!("! {note}"), Style::new().bold()));
    }
    frame.render_widget(Paragraph::new(notes), footer);
    FormHit {
        fields: shown
            .iter()
            .enumerate()
            .filter_map(|(at, line)| {
                Some((
                    Rect::new(body.x, body.y + at as u16, body.width, 1),
                    line.row?,
                ))
            })
            .collect(),
    }
}

/// The modal's own lines, the impact summary an activation confirms against,
/// and the last daemon refusal, which would otherwise hide behind the popup.
fn modal_text(state: &State) -> Vec<String> {
    let modal = state.modal();
    let mut lines = modal_lines(modal);
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
        Modal::Trace(picking) => {
            let mut lines = picking.text();
            lines.push(chrome::hints(&[
                ("↑/↓", "choose"),
                ("enter", "trace"),
                ("esc", "cancel"),
            ]));
            lines
        }
    }
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
    fn models_with_the_focus_in_the_compaction_line_120x40() {
        let mut state = page(World::AT_REST, Tab::Models);
        state.show_offers(testing::OR_CHAT, 180);
        state.move_selection(5);
        insta::assert_snapshot!(frame(&state, 120, 40));
    }

    #[test]
    fn models_with_the_focus_in_the_offers_120x40() {
        let mut state = page(World::AT_REST, Tab::Models);
        state.move_selection(7);
        state.show_offers(testing::OR_CHAT, 180);
        insta::assert_snapshot!(frame(&state, 120, 40));
    }

    fn press(state: &mut State, code: crossterm::event::KeyCode) {
        state.key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ));
    }

    /// The tabs scroll around the current one before the strip clips, and
    /// every tab stays indexed for the mouse.
    #[test]
    fn a_narrow_header_scrolls_the_tabs() {
        let state = page(World::AT_REST, Tab::Routes);
        let (text, drawn) = drawn_at(&state, 90, 32);
        let header = text.lines().next().unwrap_or_default();
        // The other tabs give way to their numbers, the current one keeps
        // its name and count, and the arrows between them stay.
        assert!(
            header.contains("1 → 2 → 3 → 4 routes 7 · 5 · 6"),
            "{header}"
        );
        assert_eq!(drawn.tabs.len(), 6);
        assert!(drawn.tabs.iter().all(|tab| !tab.is_empty()));
        let (text, drawn) = drawn_at(&state, 70, 32);
        let header = text.lines().next().unwrap_or_default();
        assert!(header.contains("4 routes 7"), "{header}");
        assert!(drawn.tabs[0].is_empty(), "scrolled out of the strip");
        assert!(!drawn.tabs[3].is_empty());
    }

    /// Too short for the detail boxed, both panels give up their borders
    /// before any line of content (spec: UTUI#what-is-shed-first).
    #[test]
    fn a_short_page_goes_ruled() {
        let state = page(World::AT_REST, Tab::Routes);
        let (tall, _) = drawn_at(&state, 120, 40);
        assert!(
            tall.contains("\u{250c}ROUTE"),
            "boxed while it fits:\n{tall}"
        );
        let (short, _) = drawn_at(&state, 120, 14);
        assert!(short.contains("ROUTE \u{2500}"), "ruled:\n{short}");
        assert!(!short.contains('\u{250c}'), "{short}");
    }

    /// A panel is no taller than its content, so the rows left over stay
    /// bare rather than being boxed (spec: UTUI#the-routing-page).
    #[test]
    fn a_panel_is_no_taller_than_its_content() {
        let state = page(World::AT_REST, Tab::Routes);
        let (_, drawn) = drawn_at(&state, 120, 50);
        assert_eq!(
            drawn.entries.height,
            1 + 7 + 2,
            "heading, seven routes, borders"
        );
        assert_eq!(drawn.entries.bottom(), drawn.detail.y);
        assert!(
            drawn.detail.bottom() < 50,
            "the page does not fill the terminal"
        );
    }

    /// A table cut short counts what it left out, and the selection stays in
    /// view.
    #[test]
    fn a_table_too_tall_for_its_room_windows_around_the_selection() {
        let mut state = page(World::AT_REST, Tab::Rewrites);
        state.move_selection(12);
        let (text, drawn) = drawn_at(&state, 120, 16);
        // The counts are of records, not of the lines groups take: eight
        // records fill the room between the two counts, in eleven lines.
        assert!(text.contains("… 5 records above"), "{text}");
        assert!(text.contains("… 4 more records"), "{text}");
        assert!(text.contains("codex_sub/${model}"), "{text}");
        let selected = state.selected_in(Tab::Rewrites);
        assert!(
            drawn.rows.iter().any(|hit| hit.entry == selected),
            "the selected row is drawn: {text}"
        );
    }

    #[test]
    fn clicking_an_entry_selects_it_and_a_click_on_a_row_names_the_entry_drawn_there() {
        let mut state = page(World::AT_REST, Tab::Rewrites);
        state.select(9);
        assert_eq!(state.selected_in(Tab::Rewrites), 9);
        // Out of range is ignored rather than clamped to a different entry.
        state.select(99);
        assert_eq!(state.selected_in(Tab::Rewrites), 9);
        let (_, drawn) = drawn_at(&state, 120, 14);
        let selected = drawn
            .rows
            .iter()
            .find(|hit| hit.entry == 9)
            .expect("row 9 is drawn in a 14-line terminal");
        assert!(
            drawn.rows[0].entry > 0,
            "the table scrolled to reach row 9: {:?}",
            drawn.rows
        );
        state.select(0);
        assert_eq!(
            crate::tui::chrome::entry_at(&drawn.rows, selected.area.as_position()),
            Some(9)
        );
    }

    /// Every tab draws, and its route page and its focus in each models
    /// panel, in any room a terminal can be: a frame too small for a table
    /// is cut, never a panic.
    #[test]
    fn every_tab_draws_in_any_size() {
        for world in [World::AT_REST, World::FULL, World::SAVED] {
            for tab in Tab::ALL {
                let mut state = page(world, tab);
                for width in [1, 2, 8, 20, 50, 80, 100, 135, 200] {
                    for height in [1, 2, 3, 5, 8, 12, 24, 50] {
                        drawn_at(&state, width, height);
                    }
                }
                if tab == Tab::Routes {
                    state.open();
                    drawn_at(&state, 80, 24);
                    drawn_at(&state, 30, 5);
                }
                if tab == Tab::Models {
                    for _ in 0..6 {
                        state.move_selection(7);
                        drawn_at(&state, 100, 24);
                        drawn_at(&state, 60, 10);
                    }
                }
            }
        }
    }

    // --- states of the page ----------------------------------------------

    #[test]
    fn a_page_that_has_read_nothing_says_so() {
        let state = State::default();
        let text = frame(&state, 100, 12);
        assert!(text.contains("no routing snapshot"), "{text}");
        let mut state = State::default();
        assert!(state.begin_refresh());
        assert!(frame(&state, 100, 12).contains("loading routing snapshot"));
    }

    #[test]
    fn a_revision_that_did_not_compile_has_no_facts_to_draw() {
        let mut state = page(World::AT_REST, Tab::Rewrites);
        let mut snapshot = testing::snapshot(World::AT_REST);
        snapshot["derived"]["active"] = serde_json::Value::Null;
        let mut command = Command::snapshot("routing-snapshot");
        command.name = "snapshot";
        state.receive(Reply::Command(
            command,
            Ok(Response {
                status: 200,
                body: snapshot,
            }),
        ));
        let text = frame(&state, 100, 12);
        assert!(text.contains("did not compile"), "{text}");
    }

    /// The header says where each client's settings stand, with the detail
    /// the row records, whether pending, failed or unsupported.
    #[test]
    fn the_header_names_a_publication_that_is_not_applied_with_its_detail() {
        let mut snapshot = testing::snapshot(World::AT_REST);
        snapshot["publications"][0]["state"] = serde_json::json!("failed");
        snapshot["publications"][0]["detail_code"] = serde_json::json!("settings_not_writable");
        snapshot["publications"][1]["state"] = serde_json::json!("unsupported");
        let mut state = State::default();
        let mut command = Command::snapshot("routing-snapshot");
        command.name = "snapshot";
        state.receive(Reply::Command(
            command,
            Ok(Response {
                status: 200,
                body: snapshot,
            }),
        ));
        let (text, _) = drawn_at(&state, 200, 12);
        let header = text.lines().next().unwrap_or_default();
        assert!(
            header.contains("rev 9 live · claude failed: settings not writable · chat unsupported"),
            "{header}"
        );
        // A page that has applied everything says only which revision is live.
        let applied = frame(&page(World::AT_REST, Tab::Rewrites), 200, 12);
        let header = applied.lines().next().unwrap_or_default();
        assert!(header.trim_end().ends_with("rev 9 live"), "{header}");
    }

    #[test]
    fn lanes_left_on_earlier_revisions_are_counted_in_the_header() {
        let state = page(World::SAVED, Tab::Rewrites);
        let (text, _) = drawn_at(&state, 200, 12);
        let header = text.lines().next().unwrap_or_default();
        assert!(
            header.contains("rev 10 live · 2 lanes on rev 9 · claude pending"),
            "{header}"
        );
    }

    /// A rule that is shadowed says by what, in words.
    #[test]
    fn a_shadowed_rule_says_what_catches_it_first() {
        let mut snapshot = testing::snapshot(World::AT_REST);
        snapshot["derived"]["active"]["rewrites"][1]["shadowed_by"] = serde_json::json!([{
            "scope_label": "anthropic_messages", "by": "earlier_rule", "id": "anthropic-messages-1",
            "position": 1, "matcher": {"kind": "glob", "text": "anthropic*", "value": "anthropic*"},
            "catches": "part", "basis": "names", "compared": 3,
            "examples": ["anthropic_sub/claude-sonnet-5"], "scope_limited": false,
        }]);
        let mut state = State::default();
        let mut command = Command::snapshot("routing-snapshot");
        command.name = "snapshot";
        state.receive(Reply::Command(
            command,
            Ok(Response {
                status: 200,
                body: snapshot,
            }),
        ));
        state.set_tab(Tab::Rewrites);
        state.move_selection(1);
        let text = frame(&state, 140, 40);
        assert!(
            text.contains(
                "part of it is caught first by rule 1 anthropic* (anthropic_sub/claude-sonnet-5)"
            ),
            "{text}"
        );
    }

    /// A profile's default sits ahead of its protocol's at the end of the
    /// protocol's group, and a profile's own group passes on to it.
    #[test]
    fn profile_defaults_come_ahead_of_the_protocols_at_the_end_of_its_group() {
        let state = page(World::FULL, Tab::Rewrites);
        insta::assert_snapshot!(frame(&state, 120, 40));
    }

    // --- forms ------------------------------------------------------------

    #[test]
    fn the_promotion_form() {
        let mut state = page(World::AT_REST, Tab::Models);
        press(&mut state, crossterm::event::KeyCode::Char('p'));
        insta::assert_snapshot!("the_promotion_form", frame(&state, 120, 32));
    }

    // --- editing in place -------------------------------------------------

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn send(state: &mut State, code: KeyCode) {
        state.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// Answer `command` as the daemon would.
    fn answer(state: &mut State, command: Command, status: u16, body: serde_json::Value) {
        state.receive(Reply::Command(command, Ok(Response { status, body })));
    }

    /// Answer the check the open edit is due with `body`.
    fn checked(state: &mut State, body: serde_json::Value) {
        let check = state.next_check().expect("a check is due");
        answer(state, check, 200, body);
    }

    fn fine() -> serde_json::Value {
        serde_json::json!({"valid": true, "diagnostics": [], "refusal": null})
    }

    /// What replaying the last 24 hours under the edit came to.
    fn replayed(state: &mut State) {
        let replay = state.next_check().expect("the replay");
        assert_eq!(replay.name, "preview");
        let request = |model: &str, ts: i64| {
            serde_json::json!({
                "request_id": ts, "ts_ms": ts, "request": {"model": model},
                "under_active": {"outcome": "selected", "route_id": "claude-sub",
                    "matched": {"kind": "default", "scope": "protocol", "id": null}},
                "under_edit": {"outcome": "selected", "route_id": "openrouter-messages",
                    "matched": {"kind": "rewrite", "scope": "protocol", "id": "new-rewrite"}},
            })
        };
        answer(
            state,
            replay,
            200,
            serde_json::json!({
                "replayed": 5, "unreplayable": 2, "truncated": false, "changed_total": 3,
                "window": {"since_ms": 0, "until_ms": 86_400_000},
                "changed": [
                    request("claude-sonnet-5", 86_340_000),
                    request("claude-opus-5", 85_800_000),
                    request("claude-haiku-4-5", 82_000_000),
                ],
            }),
        );
    }

    /// A new rewrite under the second rule of the messages protocol, with
    /// the daemon's answer on it.
    fn adding_a_rewrite(answer_with: serde_json::Value) -> State {
        let mut state = page(World::AT_REST, Tab::Rewrites);
        state.move_selection(1);
        send(&mut state, KeyCode::Char('n'));
        checked(&mut state, answer_with);
        state
    }

    #[test]
    fn a_rewrite_being_added_opens_under_its_row_with_the_recent_traffic_under_it() {
        let mut state = adding_a_rewrite(fine());
        replayed(&mut state);
        let text = frame(&state, 120, 50);
        insta::assert_snapshot!(text);
        assert!(text.contains("editing · rev 9 live"), "{text}");
        assert!(text.contains("[ctrl+s] save   [esc] cancel"), "{text}");
        assert!(
            text.contains("3 of 5 recent requests would change route"),
            "{text}"
        );
        assert!(text.contains("2 requests could not be replayed"), "{text}");
    }

    #[test]
    fn a_refusal_is_said_in_red_beside_the_field_with_its_fix() {
        let index = 14;
        let mut state = adding_a_rewrite(serde_json::json!({
            "valid": false, "refusal": null,
            "diagnostics": [{
                "code": "capture-binding-not-regex", "severity": "error",
                "record_id": "new-rewrite", "path": format!("/rewrites/{index}/target"),
                "sentence": "The route takes a capture from the match, and this match is not a regular expression.",
                "fix": "Match with a regular expression, or give the route the requested model instead.",
            }],
        }));
        let text = frame(&state, 120, 50);
        insta::assert_snapshot!(text);
        assert!(text.contains("[ctrl+s] fix route to save"), "{text}");
        // The refusal is red: it is the one thing on the page that is.
        let mut terminal = Terminal::new(TestBackend::new(120, 50)).expect("terminal");
        terminal
            .draw(|frame| {
                render(frame, &state, false);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let red = buffer
            .content
            .iter()
            .filter(|cell| cell.fg == ratatui::style::Color::Red)
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(red.contains("The route takes a capture"), "{red}");
        assert!(red.contains("Match with a regular expression"), "{red}");
        // And nothing about it blocks reading the rest.
        send(&mut state, KeyCode::Esc);
    }

    #[test]
    fn a_warning_is_beside_the_field_and_the_save_stays_open() {
        let mut state = page(World::AT_REST, Tab::Routes);
        send(&mut state, KeyCode::Enter);
        send(&mut state, KeyCode::Char('n'));
        let (index, steps) = {
            let route = state.document_route_for_tests();
            (route.0, route.1)
        };
        checked(
            &mut state,
            serde_json::json!({"valid": true, "refusal": null, "diagnostics": [{
                "code": "binding-unconfigured", "severity": "warning", "record_id": "claude-sub",
                "path": format!("/routes/{index}/chain/steps/{steps}/candidate/binding_id"),
                "sentence": "The provider is off: the candidate is skipped until it is on.",
                "fix": "Turn the provider on, or leave it.",
            }]}),
        );
        replayed(&mut state);
        let text = frame(&state, 120, 50);
        insta::assert_snapshot!(text);
        assert!(text.contains("[ctrl+s] save"), "{text}");
    }

    #[test]
    fn a_save_moved_past_names_the_live_revision_in_the_form() {
        let mut state = adding_a_rewrite(fine());
        state.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let mut save = Command::snapshot("routing-snapshot");
        save.name = "save";
        answer(
            &mut state,
            save,
            409,
            serde_json::json!({"error": "stale_revision", "live_revision_id": 12}),
        );
        let text = frame(&state, 120, 50);
        insta::assert_snapshot!(text);
        assert!(
            text.contains("revision 12 is now live; your edit is kept as you entered it"),
            "{text}"
        );
        assert!(text.contains("[ctrl+s] save onto rev 12"), "{text}");
    }

    #[test]
    fn a_removal_the_rest_of_the_policy_refuses_names_the_records() {
        let mut state = page(World::AT_REST, Tab::Routes);
        send(&mut state, KeyCode::Char('d'));
        checked(
            &mut state,
            serde_json::json!({
                "valid": false, "refusal": null,
                "diagnostics": [
                    {"code": "missing-route", "severity": "error", "record_id": "anthropic-messages-1",
                     "path": "/rewrites/0/target/route_id",
                     "sentence": "The route it sends to is not in the policy.",
                     "fix": "Send it to another route, or remove it too."},
                    {"code": "missing-route", "severity": "error", "record_id": "anthropic-messages-2",
                     "path": "/rewrites/1/target/route_id",
                     "sentence": "The route it sends to is not in the policy.",
                     "fix": "Send it to another route, or remove it too."},
                ],
            }),
        );
        let text = frame(&state, 120, 40);
        insta::assert_snapshot!(text);
        assert!(text.contains("rewrite anthropic-messages-1"), "{text}");
        assert!(text.contains("rewrite anthropic-messages-2"), "{text}");
        assert!(text.contains("[ctrl+s] fix 2 records to save"), "{text}");
    }

    #[test]
    fn a_rewrite_being_moved_is_shown_where_it_would_land() {
        let mut state = page(World::AT_REST, Tab::Rewrites);
        state.move_selection(1);
        send(&mut state, KeyCode::Char('m'));
        send(&mut state, KeyCode::Down);
        checked(&mut state, fine());
        let text = frame(&state, 120, 40);
        insta::assert_snapshot!(text);
        assert!(text.contains("rule 2 → 3 of 5"), "{text}");
    }

    #[test]
    fn a_fallback_added_to_a_route_is_a_form_among_its_candidates() {
        let mut state = page(World::AT_REST, Tab::Routes);
        send(&mut state, KeyCode::Enter);
        send(&mut state, KeyCode::Char('n'));
        checked(&mut state, fine());
        replayed(&mut state);
        let text = frame(&state, 120, 50);
        insta::assert_snapshot!(text);
        assert!(text.contains("add a fallback to claude-sub"), "{text}");
    }

    #[test]
    fn a_candidate_being_reordered_is_shown_in_its_new_place() {
        let mut state = page(World::AT_REST, Tab::Routes);
        send(&mut state, KeyCode::Enter);
        send(&mut state, KeyCode::Char('m'));
        send(&mut state, KeyCode::Down);
        checked(&mut state, fine());
        let text = frame(&state, 120, 50);
        insta::assert_snapshot!(text);
        assert!(text.contains("candidate 1 → 2 of 2"), "{text}");
    }

    /// A click on a form line focuses that field, and a click on a line that
    /// is only a heading or a completion focuses nothing.
    #[test]
    fn a_click_focuses_the_field_on_its_line() {
        let mut state = page(World::AT_REST, Tab::Presentations);
        send(&mut state, KeyCode::Char('n'));
        let (text, drawn) = drawn_at(&state, 120, 50);
        let hit = drawn.form.expect("a form is open");
        let lines: Vec<&str> = text.lines().collect();
        let on = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle:?}: {text}"))
        };
        let at = |y: usize| {
            hit.fields
                .iter()
                .find(|(area, _)| usize::from(area.y) == y)
                .map(|(_, row)| *row)
        };
        let slug = at(on("slug:")).expect("the slug line is a field");
        assert!(at(on("SCOPE")).is_none(), "a heading is not a field");
        state.focus_form_row(slug);
        let focused: Vec<String> = state
            .editing()
            .expect("editing")
            .form
            .lines()
            .into_iter()
            .filter(|line| line.focused)
            .map(|line| line.text)
            .collect();
        assert_eq!(focused.len(), 1);
        assert!(focused[0].contains("slug"), "{focused:?}");
    }

    /// An edit draws in any room a terminal can be.
    #[test]
    fn every_edit_draws_in_any_size() {
        let edits: [(Tab, &[char]); 6] = [
            (Tab::Rewrites, &['n']),
            (Tab::Rewrites, &['e']),
            (Tab::Rewrites, &['m']),
            (Tab::Presentations, &['d']),
            (Tab::Models, &['e']),
            (Tab::Defaults, &['n']),
        ];
        for (tab, keys) in edits {
            let mut state = page(World::FULL, tab);
            for key in keys {
                send(&mut state, KeyCode::Char(*key));
            }
            // A move that has not moved has nothing to check.
            if let Some(check) = state.next_check() {
                answer(&mut state, check, 200, fine());
            }
            for width in [1, 2, 8, 20, 50, 80, 135] {
                for height in [1, 2, 3, 5, 8, 12, 24, 50] {
                    drawn_at(&state, width, height);
                }
            }
        }
        let mut state = page(World::FULL, Tab::Routes);
        send(&mut state, KeyCode::Enter);
        for key in ['n', 'e', 'd', 'm'] {
            send(&mut state, KeyCode::Char(key));
            for width in [1, 20, 80, 135] {
                for height in [1, 3, 12, 24, 50] {
                    drawn_at(&state, width, height);
                }
            }
            send(&mut state, KeyCode::Esc);
        }
    }
}
