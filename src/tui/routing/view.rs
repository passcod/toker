//! Routing page rendering, including content-free typed modal forms.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, Paragraph, Wrap},
};
use serde_json::Value;

use super::model::{Entry, Modal, State, View};
use super::template::singular;

/// The page areas that the loop needs for click handling.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Drawn {
    pub(crate) legend_button: Rect,
    pub(crate) routing_button: Rect,
    pub(super) tabs: Rect,
    pub(super) entries: Rect,
}

pub(super) fn render(frame: &mut Frame, state: &State, legend: bool) -> Drawn {
    let area = frame.area();
    let (legend_button, routing_button, content) = header_actions(area);
    let [tabs, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(content);
    render_header_actions(frame, legend_button, routing_button);
    frame.render_widget(Paragraph::new(StateTabs::line(state)), tabs);

    let [master, detail] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Fill(1)]).areas(body);
    let entries = state.entries();
    let rows: Vec<ListItem> = if entries.is_empty() {
        vec![ListItem::new("no routing data yet")]
    } else {
        entries
            .iter()
            .enumerate()
            .map(|(index, entry)| ListItem::new(entry_summary(entry)).style(selected(index, state)))
            .collect()
    };
    frame.render_widget(
        List::new(rows).block(Block::bordered().title(state.view().title())),
        master,
    );

    frame.render_widget(
        Paragraph::new(detail_lines(state, entries.get(state.selected())))
            .block(Block::bordered().title("DETAIL"))
            .wrap(Wrap { trim: false }),
        detail,
    );
    frame.render_widget(
        Paragraph::new(footer_text(state)).style(Style::new().dim()),
        footer,
    );
    if legend {
        render_legend(frame);
    }
    if state.modal().active() {
        render_modal(frame, state);
    }
    Drawn {
        legend_button,
        routing_button,
        tabs,
        entries: master,
    }
}

fn header_actions(area: Rect) -> (Rect, Rect, Rect) {
    let height = area.height.min(1);
    let legend_width = area.width.min(3);
    let routing_width = area.width.saturating_sub(legend_width).min(3);
    let legend = Rect::new(area.x, area.y, legend_width, height);
    let routing = Rect::new(area.x + legend_width, area.y, routing_width, height);
    let content = Rect::new(
        area.x,
        area.y + height,
        area.width,
        area.height.saturating_sub(height),
    );
    (legend, routing, content)
}

fn render_header_actions(frame: &mut Frame, legend: Rect, routing: Rect) {
    frame.render_widget(
        Paragraph::new(if legend.width >= 3 { " ? " } else { "?" })
            .style(Style::new().add_modifier(Modifier::REVERSED)),
        legend,
    );
    frame.render_widget(
        Paragraph::new(if routing.width >= 3 { " ⇄ " } else { "⇄" })
            .style(Style::new().add_modifier(Modifier::REVERSED)),
        routing,
    );
}

struct StateTabs;

impl StateTabs {
    fn line(state: &State) -> Line<'static> {
        let mut spans = Vec::new();
        for (index, view) in [
            View::Presentations,
            View::Routes,
            View::Rewrites,
            View::Status,
        ]
        .into_iter()
        .enumerate()
        {
            if index > 0 {
                spans.push(Span::raw("  "));
            }
            let label = format!("{} {}", index + 1, view.title());
            let style = if view == state.view() {
                Style::new().bold().add_modifier(Modifier::REVERSED)
            } else {
                Style::new().dim()
            };
            spans.push(Span::styled(label, style));
        }
        Line::from(spans)
    }
}

fn selected(index: usize, state: &State) -> Style {
    if index == state.selected() {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    }
}

fn entry_summary(entry: &Entry<'_>) -> String {
    let value = entry.value;
    if entry.key == "defaults" {
        return format!(
            "default {}  {}",
            super::model::scope_label(value),
            short(value)
        );
    }
    let name = ["slug", "id", "name", "revision", "adapter", "provider"]
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
    if let Some(error) = state.error() {
        return vec![Line::styled(
            format!("unavailable: {error}"),
            Style::new().bold(),
        )];
    }
    let mut lines = match selected {
        Some(entry) => pretty(entry.value)
            .lines()
            .map(|line| Line::raw(line.to_owned()))
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

fn footer_text(state: &State) -> String {
    let generation = state
        .generation()
        .map(|generation| format!("draft {generation} · "))
        .unwrap_or_default();
    let busy = if state.in_flight() { "working · " } else { "" };
    let settings = state
        .settings_note()
        .map(|note| format!("{note} · "))
        .unwrap_or_default();
    format!(
        "{generation}{busy}{settings}←/→ or 1–4 view · ↑/↓ select · n new · e edit · d remove · v preview · V validate · a activate · p promote · r refresh · Tab dashboard · ? help"
    )
}

fn short(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unrenderable>".to_owned())
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "<unrenderable>".to_owned())
}

fn render_modal(frame: &mut Frame, state: &State) {
    let modal = state.modal();
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
}

/// The modal's own lines, the impact summary an activation confirms against,
/// and the last daemon refusal, which would otherwise hide behind the popup.
fn modal_text(state: &State) -> Vec<String> {
    let modal = state.modal();
    let mut lines = modal_lines(modal);
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
        Modal::Description { value } => vec![
            "Type a description; Enter saves, Esc cancels.".to_owned(),
            value.clone(),
        ],
        Modal::Record { key, value, adding } => vec![
            if *adding {
                format!("{key}: fill in the new typed JSON record; its id must be unused.")
            } else {
                format!("{key}: edit its typed JSON record.")
            },
            "Ctrl+Enter saves; Esc cancels. The daemon validates all fields.".to_owned(),
            value.clone(),
        ],
        Modal::NewKind { kinds, preselected } => {
            let mut lines = vec!["Add which kind of record? The number or Enter picks it.".to_owned()];
            lines.extend(kinds.iter().enumerate().map(|(index, kind)| {
                let mark = if index == *preselected { ">" } else { " " };
                format!("{mark} {} {}", index + 1, singular(kind))
            }));
            lines.push("Esc cancels".to_owned());
            lines
        }
        Modal::Remove { key, value } => vec![
            format!("Remove this {key} record? This changes only the draft."),
            short(value),
            "y/Enter confirms · n/Esc cancels".to_owned(),
        ],
        Modal::CandidateOrder { route, selected } => vec![
            "j/k choose candidate · h/l moves it · Enter saves typed route edit.".to_owned(),
            "Transition edges keep their positions; the daemon validates the result.".to_owned(),
            format!("selected candidate {}", selected + 1),
            pretty(route),
        ],
        Modal::Preview { field, values } => {
            let labels = ["protocol", "profile", "model", "lane key", "prompt bound", "message count", "tool count"];
            let mut lines = vec![
                "Content-free request facts only. Tab selects; Enter previews; Esc cancels.".to_owned(),
            ];
            lines.extend(labels.into_iter().zip(values).enumerate().map(|(index, (label, value))| {
                if index == *field { format!("> {label}: {value}") } else { format!("  {label}: {value}") }
            }));
            lines
        }
        Modal::PreviewResult(result) => vec![
            "Press any key to close.".to_owned(),
            pretty(result),
        ],
        Modal::Activate { scope, force_cache_transition } => activation_lines("Activate this draft?", *scope, *force_cache_transition),
        Modal::Reactivate { revision_id, scope, force_cache_transition } => {
            let mut lines = activation_lines(&format!("Create a forward revision from revision {revision_id}?"), *scope, *force_cache_transition);
            lines.insert(1, "The active pointer never moves backwards.".to_owned());
            lines
        }
        Modal::Promote { field, binding, model } => vec![
            "Promotion records a binding-specific learned model; it does not activate a draft.".to_owned(),
            if *field == 0 { format!("> binding: {binding}") } else { format!("  binding: {binding}") },
            if *field == 1 { format!("> model: {model}") } else { format!("  model: {model}") },
            "Tab selects · Ctrl+O completes from a daemon offer · Enter records · Esc cancels".to_owned(),
        ],
        Modal::Import => vec![
            "Replace the current draft with the routing in toker.toml?".to_owned(),
            "The active revision does not change until the draft is activated.".to_owned(),
            "y/Enter confirms · n/Esc cancels".to_owned(),
        ],
        Modal::Conflict { daemon_generation, daemon_draft } => vec![
            "The daemon draft changed while this terminal was editing.".to_owned(),
            format!("daemon generation: {}", daemon_generation.map_or_else(|| "unknown".to_owned(), |value| value.to_string())),
            "r reloads and discards local typed edits · a reapplies them to this daemon copy · Esc closes".to_owned(),
            pretty(daemon_draft),
        ],
    }
}

fn activation_lines(prompt: &str, scope: usize, force_cache_transition: bool) -> Vec<String> {
    let scope = ["new lanes", "new and cold lanes", "immediate"][scope.min(2)];
    vec![
        prompt.to_owned(),
        format!("scope: {scope} (h/l changes)"),
        format!(
            "forced warm-cache transition: {} (f toggles)",
            if force_cache_transition { "YES" } else { "no" }
        ),
        "y/Enter confirms · n/Esc cancels".to_owned(),
    ]
}

fn render_legend(frame: &mut Frame) {
    let lines = [
        "ROUTING HELP",
        "Tab  return to dashboard",
        "1–4, ←/→  change routing view",
        "↑/↓, j/k  select an item",
        "n  add a record from a template (asks which kind where a view has several)",
        "e  edit typed record; Ctrl+Enter saves JSON, in an add too",
        "d  confirm removal; m orders route candidates",
        "defaults (routes view) and compaction (rewrites view) take n, e and d",
        "v preview; V validate; a activate; x reactivate; p promote",
        "i  import legacy toker.toml routing into the draft (status view)",
        "A missing daemon endpoint leaves the loaded read-only view usable",
        "? or Esc closes",
    ];
    let width = lines.iter().map(|line| line.len()).max().unwrap_or(0) as u16 + 4;
    let area = frame.area();
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width.min(area.width)) / 2,
        area.y + area.height.saturating_sub((lines.len() + 2) as u16) / 2,
        width.min(area.width),
        (lines.len() as u16 + 2).min(area.height),
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines.join("\n")).block(Block::bordered()),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::{State, modal_text, render};
    use crate::tui::routing::client::{Command, Reply, Response};

    #[test]
    fn page_names_edit_and_activation_controls() {
        let mut state = State::default();
        state.receive(Reply::Command(
            Command::snapshot(),
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
        assert!(text.contains("e edit"));
        assert!(text.contains("a activate"));
    }

    #[test]
    fn the_activation_form_shows_its_impact_and_the_last_refusal() {
        let mut state = State::default();
        state.receive(Reply::Command(
            Command::snapshot(),
            Ok(Response {
                status: 200,
                body: serde_json::json!({
                    "draft_generation": 2,
                    "draft": {"policy": {"routes": [{"id": "new-route"}]}},
                    "active_revision": {"policy": {"routes": []}},
                    "lane_impacts": {"total": 4, "unpinned": 1, "warm": 2, "cold": 1, "unknown": 0},
                    "publications": [{"adapter": "claude", "state": "failed"}],
                }),
            }),
        ));
        state.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let mut command = Command::snapshot();
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
        assert!(text.contains("publication claude: failed"), "{text}");
        assert!(text.contains("forced_cache_transition_required"), "{text}");
        assert!(
            text.lines()
                .rev()
                .nth(1)
                .is_some_and(|line| line.contains("confirms")),
            "the confirmation prompt stays above the refusal: {text}"
        );
    }

    /// Every key the page acts on is named in the legend, so a key that was
    /// added cannot stay undocumented.
    #[test]
    fn the_legend_names_every_record_key() {
        let backend = TestBackend::new(140, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
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
        for key_help in [
            "n  add a record",
            "e  edit typed record",
            "d  confirm removal",
            "defaults (routes view)",
            "i  import",
        ] {
            assert!(text.contains(key_help), "{key_help}: {text}");
        }
    }

    #[test]
    fn the_add_forms_say_what_they_do() {
        let mut state = State::default();
        state.receive(Reply::Command(
            Command::snapshot(),
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
}
