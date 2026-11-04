//! Read-only routing page rendering.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, Paragraph},
};
use serde_json::Value;

use super::model::{Entry, State};

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
    let tab_line = StateTabs::line(state);
    frame.render_widget(Paragraph::new(tab_line), tabs);

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
    let master_block = Block::bordered().title(state.view().title());
    frame.render_widget(List::new(rows).block(master_block), master);

    let detail_lines = detail_lines(state, entries.get(state.selected()));
    frame.render_widget(
        Paragraph::new(detail_lines)
            .block(Block::bordered().title("DETAIL"))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        detail,
    );
    frame.render_widget(
        Paragraph::new(footer_text(state)).style(Style::new().dim()),
        footer,
    );
    if legend {
        render_legend(frame);
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
            super::model::View::Presentations,
            super::model::View::Routes,
            super::model::View::Rewrites,
            super::model::View::Status,
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
    let name = ["slug", "id", "name", "revision", "adapter", "provider"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str).map(str::to_owned))
        .or_else(|| {
            ["id", "revision"].into_iter().find_map(|key| {
                value
                    .get(key)
                    .and_then(Value::as_u64)
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
    format!("{generation}←/→ or 1–4 view · ↑/↓ or j/k select · r refresh · Tab dashboard · ? help")
}

fn short(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unrenderable>".to_owned())
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "<unrenderable>".to_owned())
}

fn render_legend(frame: &mut Frame) {
    let lines = [
        "ROUTING HELP",
        "Tab  return to dashboard",
        "1–4, ←/→  change routing view",
        "↑/↓, j/k  select an item",
        "r  reload routing snapshot",
        "Read-only: editing and activation are not available yet",
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

    use super::{State, render};
    use crate::tui::routing::client::Reply;

    #[test]
    fn page_names_all_four_read_only_views() {
        let mut state = State::default();
        state.receive(Reply::Snapshot(Ok(serde_json::json!({
            "draft": {"generation": 2},
            "active_revision": {"policy": {"presentations": [{"slug": "friendly"}]}}
        }))));
        let backend = TestBackend::new(110, 20);
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
        assert!(text.contains("PRESENTATIONS"));
        assert!(text.contains("ROUTES & CANDIDATES"));
        assert!(text.contains("REWRITES, FAMILIES & UPGRADES"));
        assert!(text.contains("PREVIEW, REVISIONS & ACTIVATION"));
    }
}
