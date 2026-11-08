//! Rendering the configuration pages.
//!
//! A pure function of the state, like every other view here, so the frames
//! can be asserted at several terminal sizes without a real terminal
//! (spec: UTUI#rendering).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use super::model::{Provider, Row, Settings, Snapshot, State, View};

/// Where the last frame put things, for the mouse.
#[derive(Debug, Clone, Default)]
pub(super) struct Drawn {
    pub(super) entries: Rect,
}

pub(super) fn render(frame: &mut Frame, state: &State) -> Drawn {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(tabs(state)), header);

    let drawn = match state.snapshot() {
        _ if !state.reachable() => {
            // The service is the only writer, so a form the operator can
            // fill in while it is down is a form whose edits go nowhere
            // (spec: UTUI#the-configuration-pages).
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(""),
                    Line::from("  the service is not answering"),
                    Line::from("  configuration is edited through it, so there is nothing to show"),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("CONFIGURATION"),
                ),
                body,
            );
            Drawn::default()
        }
        None => Drawn::default(),
        Some(snapshot) => {
            let rows = state.rows();
            let items: Vec<ListItem> = rows
                .iter()
                .map(|row| ListItem::new(line_for(*row, snapshot)))
                .collect();
            let mut list_state = ListState::default();
            list_state.select(Some(state.selected()));
            frame.render_stateful_widget(
                List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(title(state)))
                    .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
                body,
                &mut list_state,
            );
            Drawn { entries: body }
        }
    };

    frame.render_widget(Paragraph::new(footer_lines(state)), footer);
    drawn
}

fn tabs(state: &State) -> Line<'static> {
    let mark = |view: View, label: &'static str| {
        let style = if state.view() == view {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Span::styled(label, style)
    };
    Line::from(vec![
        Span::raw(" "),
        mark(View::Providers, "1 providers"),
        Span::raw("   "),
        mark(View::Settings, "2 settings"),
    ])
}

/// The panel title, upper case and unspaced against its rule, as the
/// dashboard and the routing page both render theirs.
fn title(state: &State) -> &'static str {
    match state.view() {
        View::Providers => "PROVIDERS",
        View::Settings => "SETTINGS",
    }
}

fn footer_lines(state: &State) -> Vec<Line<'static>> {
    if let Some(editing) = state.editing() {
        let shown = if editing.secret {
            // A credential is never echoed, here or anywhere
            // (spec: SEC#a-credential-is-never-logged-or-displayed).
            "·".repeat(editing.buffer.chars().count())
        } else {
            editing.buffer.clone()
        };
        return vec![
            Line::from(format!("  > {shown}")),
            Line::from("  enter accepts, empty clears, esc cancels"),
        ];
    }
    let mut lines = vec![Line::from(
        "  space toggles  e edits  1/2 switches  q quits",
    )];
    lines.push(Line::from(match state.error() {
        Some(error) => format!("  {error}"),
        None => String::new(),
    }));
    lines
}

fn line_for(row: Row, snapshot: &Snapshot) -> Line<'static> {
    match row {
        Row::Provider(index) => match snapshot.providers.get(index) {
            Some(provider) => Line::from(provider_line(provider)),
            None => Line::from(""),
        },
        Row::Upstream(index) => field(
            "    endpoint",
            snapshot
                .providers
                .get(index)
                .and_then(|provider| provider.upstream.clone())
                .unwrap_or_else(|| "(the built-in one)".to_owned()),
        ),
        Row::Credential(index) => match snapshot.providers.get(index) {
            Some(provider) => field("    credential", credential_line(provider)),
            None => Line::from(""),
        },
        Row::Quota => field("quota gate", on_off(snapshot.settings.quota_enabled, true)),
        Row::Cold => field("cold gate", on_off(snapshot.settings.cold_enabled, true)),
        Row::ColdOutlook => field("cold outlook", on_off(snapshot.settings.cold_outlook, true)),
        Row::ColdMinTokens => field("cold min tokens", number(&snapshot.settings)),
        Row::ColdIdleMin => field(
            "cold idle minutes",
            match snapshot.settings.cold_idle_min {
                Some(minutes) => minutes.to_string(),
                None => "(the lane's own TTL tier)".to_owned(),
            },
        ),
        Row::Awake => field("idle-sleep lock", on_off(snapshot.settings.awake, true)),
        Row::PingHeader => field("ping header", unset(snapshot.settings.ping_header.clone())),
        Row::NoticeDefault => field(
            "default notice style",
            unset(snapshot.settings.notice_default.clone()),
        ),
        // Settled before the store is opened, so shown and not offered
        // (spec: UTUI#settings).
        Row::Port => field("listener port", snapshot.port.to_string()),
        Row::Store => field("store", snapshot.db_path.clone()),
    }
}

fn provider_line(provider: &Provider) -> String {
    let state = if !provider.enabled {
        "off"
    } else if provider.holds_a_credential() && !provider.has_credential() {
        // The answer a request reaching it gets, so the page must not say
        // anything else (spec: UTUI#providers).
        "on, not configured"
    } else {
        "on"
    };
    format!("{:<16} {state}", provider.name)
}

fn credential_line(provider: &Provider) -> String {
    let mut sources = Vec::new();
    if provider.env_set {
        sources.push(match &provider.cred_env {
            Some(name) => format!("env {name}"),
            None => "env".to_owned(),
        });
    }
    if provider.keyring {
        sources.push("keyring".to_owned());
    }
    if provider.literal_set {
        sources.push("stored".to_owned());
    }
    if sources.is_empty() {
        match &provider.cred_env {
            Some(name) => format!("none ({name} is unset)"),
            None => "none".to_owned(),
        }
    } else {
        sources.join(", ")
    }
}

fn field(label: &str, value: String) -> Line<'static> {
    Line::from(format!("{label:<22} {value}"))
}

fn on_off(value: Option<bool>, default: bool) -> String {
    let on = value.unwrap_or(default);
    let mark = if on { "on" } else { "off" };
    match value {
        Some(_) => mark.to_owned(),
        None => format!("{mark} (default)"),
    }
}

fn number(settings: &Settings) -> String {
    match settings.cold_min_tokens {
        Some(count) => count.to_string(),
        None => "(the built-in floor)".to_owned(),
    }
}

fn unset(value: Option<String>) -> String {
    value.unwrap_or_else(|| "(the built-in one)".to_owned())
}

#[cfg(test)]
mod tests {
    use super::super::model::{State, View, testing::loaded};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// The frame as the screen reads it, trailing blanks trimmed.
    fn frame(state: &State, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|f| {
                super::render(f, state);
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

    #[test]
    fn the_providers_page_renders() {
        insta::assert_snapshot!("providers_100x24", frame(&loaded(), 100, 24));
    }

    #[test]
    fn the_settings_page_renders() {
        let mut state = loaded();
        state.set_view(View::Settings);
        insta::assert_snapshot!("settings_100x24", frame(&state, 100, 24));
    }

    #[test]
    fn an_unreachable_service_says_so_rather_than_offering_values() {
        // The service is the only writer, so a form the operator can fill
        // in while it is down is a form whose edits go nowhere
        // (spec: UTUI#the-configuration-pages).
        let mut state = loaded();
        state.command_refused("config-snapshot");
        insta::assert_snapshot!("unreachable_100x24", frame(&state, 100, 24));
    }

    /// The panel title is upper case and runs into its rule, as the
    /// dashboard's and the routing page's do.
    #[test]
    fn the_panel_title_matches_the_other_pages() {
        for (state, want) in [
            (loaded(), "\u{250c}PROVIDERS\u{2500}"),
            (
                {
                    let mut state = loaded();
                    state.set_view(View::Settings);
                    state
                },
                "\u{250c}SETTINGS\u{2500}",
            ),
        ] {
            let text = frame(&state, 100, 24);
            assert!(text.contains(want), "expected {want:?} in:\n{text}");
        }
    }
}
