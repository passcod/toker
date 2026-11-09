//! Rendering the configuration pages.
//!
//! A pure function of the state, like every other view here, so the frames
//! can be asserted at several terminal sizes without a real terminal
//! (spec: UTUI#rendering).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};

use super::model::{Provider, Row, Safeguards, Settings, Snapshot, State, View};
use crate::tui::chrome::{self, Chrome, Noun, RowHit};

/// Where the last frame put things, for the mouse.
#[derive(Debug, Clone, Default)]
pub(super) struct Drawn {
    pub(super) header: chrome::Header,
    pub(super) tabs: Vec<Rect>,
    pub(super) rows: Vec<RowHit>,
}

/// The tabs, in the order `1` and `2` name them.
const TABS: [&str; 2] = ["providers", "settings"];

/// The page's keys, in its `?` overlay.
const KEYS: [(&str, &str); 9] = [
    ("↑/↓ j/k", "select"),
    ("←/→ h/l 1 2", "switch view"),
    ("space", "toggle"),
    ("e", "edit"),
    ("c", "clear a provider's server safeguards rejection marks"),
    ("r", "refresh"),
    ("esc", "back to the dashboard"),
    ("tab or ⇄", "next page"),
    ("q", "quit"),
];

pub(super) fn render(frame: &mut Frame, state: &State, overlay: bool) -> Drawn {
    // A refused value is reported on the page, under the header, for as
    // many rows as it takes (spec: UTUI#the-configuration-pages).
    let (header, body) = chrome::page(frame, state.error());
    let current = match state.view() {
        View::Providers => 0,
        View::Settings => 1,
    };
    let tabs = chrome::tabs(frame, header.rest, &TABS, current);

    let rows = if !state.reachable() {
        // The service is the only writer, so a form the operator can
        // fill in while it is down is a form whose edits go nowhere
        // (spec: UTUI#the-configuration-pages).
        let lines = vec![
            Line::from("the service is not answering"),
            Line::from("configuration is edited through it, so there is nothing to show").dim(),
        ];
        let inner = chrome::panel(
            frame,
            body,
            "CONFIGURATION",
            Chrome::fitting(lines.len(), body.height),
        );
        chrome::list(
            frame,
            inner,
            lines.into_iter().map(|line| vec![line]).collect(),
            None,
            NOUNS[current],
        );
        Vec::new()
    } else if let Some(snapshot) = state.snapshot() {
        let entries: Vec<Vec<Line<'static>>> = state
            .rows()
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                let mut lines = vec![line_for(row, snapshot)];
                if index == state.selected() {
                    lines.extend(entry_lines(state));
                }
                lines
            })
            .collect();
        let need = entries.iter().map(Vec::len).sum();
        let inner = chrome::panel(
            frame,
            body,
            title(state),
            Chrome::fitting(need, body.height),
        );
        chrome::list(
            frame,
            inner,
            entries,
            Some(state.selected()),
            NOUNS[current],
        )
    } else {
        Vec::new()
    };

    chrome::chips(frame, &header);
    if overlay {
        chrome::overlay(
            frame,
            "KEYS",
            &[chrome::Section {
                heading: None,
                rows: &KEYS,
            }],
        );
    }
    Drawn { header, tabs, rows }
}

/// What each view's counts call what they leave out.
const NOUNS: [Noun; 2] = [
    Noun {
        one: "row",
        many: "rows",
    },
    Noun {
        one: "setting",
        many: "settings",
    },
];

/// The panel title, upper case and unspaced against its rule, as every
/// other page renders its own.
fn title(state: &State) -> &'static str {
    match state.view() {
        View::Providers => "PROVIDERS",
        View::Settings => "SETTINGS",
    }
}

/// The open text entry, under the row it edits: what has been typed, then
/// the keys it takes. It takes every key, `?` included, so it says them
/// itself rather than leaving them to the overlay.
fn entry_lines(state: &State) -> Vec<Line<'static>> {
    let Some(editing) = state.editing() else {
        return Vec::new();
    };
    let shown = if editing.secret {
        // A credential is never echoed, here or anywhere
        // (spec: SEC#a-credential-is-never-logged-or-displayed).
        "·".repeat(editing.buffer.chars().count())
    } else {
        editing.buffer.clone()
    };
    vec![
        Line::from(vec![
            Span::raw(format!("    {:<18} {shown}", "new value")),
            Span::styled("▏", Style::new().fg(Color::Cyan)),
        ]),
        Line::styled(
            format!(
                "    {:<18} {}",
                "",
                chrome::hints(&[("enter", "save, empty clears"), ("esc", "cancel")])
            ),
            Style::new().dim(),
        ),
    ]
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
        Row::Safeguards(index) => match snapshot.safeguards.get(index) {
            Some(binding) => field("    server safeguards", safeguards_line(binding)),
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

/// A binding's declared support, and that a refusal has marked it, which
/// reads as unsupported to the router until the mark is cleared
/// (spec: WSAFE#a-rejection-mark).
fn safeguards_line(binding: &Safeguards) -> String {
    let marked = if binding.marked_ms.is_some() {
        ", marked unsupported"
    } else {
        ""
    };
    format!("{}  {}{marked}", binding.binding_id, binding.declared)
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
                super::render(f, state, false);
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

    /// Too short for its rows boxed, the page sheds its border before any
    /// row, then windows the list around the selection and counts the cut
    /// (spec: UTUI#what-is-shed-first).
    #[test]
    fn a_short_page_goes_ruled_and_counts_what_it_cut() {
        let mut state = loaded();
        state.set_view(View::Settings);
        let rows = state.rows().len();
        let roomy = frame(&state, 100, rows as u16 + 3);
        assert!(
            roomy.contains("\u{250c}SETTINGS"),
            "boxed while it fits:\n{roomy}"
        );

        let short = frame(&state, 100, 6);
        assert!(short.contains("SETTINGS \u{2500}"), "ruled:\n{short}");
        assert!(
            short.contains("more settings"),
            "the cut is counted:\n{short}"
        );
    }

    /// A click names the row drawn where it lands, not one counted from
    /// the top of a list that has scrolled.
    #[test]
    fn a_click_selects_the_row_drawn_there_once_the_list_has_scrolled() {
        let mut state = loaded();
        state.set_view(View::Settings);
        let last = state.rows().len() - 1;
        state.select(last);
        let mut terminal = Terminal::new(TestBackend::new(100, 6)).expect("terminal");
        let mut drawn = super::Drawn::default();
        terminal
            .draw(|f| drawn = super::render(f, &state, false))
            .expect("draw");
        let first = drawn.rows.first().cloned().expect("a row was drawn");
        assert!(first.entry > 0, "the list scrolled: {:?}", drawn.rows);
        let at = first.area.as_position();
        assert_eq!(
            crate::tui::chrome::entry_at(&drawn.rows, at),
            Some(first.entry)
        );
    }

    /// The page's keys are in its overlay, every key it takes named there,
    /// and there is no footer (spec: UTUI#finding-the-keys).
    #[test]
    fn the_overlay_names_every_key_the_page_takes() {
        use crossterm::event::KeyCode;
        for key in ['e', 'c', ' ', '1', '2', 'h', 'j', 'k', 'l'] {
            assert!(super::super::owns_key(KeyCode::Char(key)));
            let token = if key == ' ' {
                "space".to_owned()
            } else {
                key.to_string()
            };
            assert!(
                super::KEYS
                    .iter()
                    .any(|(keys, _)| keys.split([' ', '/']).any(|t| t == token)),
                "{token:?} is taken but not named"
            );
        }
        let state = loaded();
        assert!(!frame(&state, 100, 24).contains("quits"), "no footer");
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|f| {
                super::render(f, &state, true);
            })
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("KEYS") && text.contains("switch view"),
            "{text}"
        );
    }

    /// A refused value is said under the header, with the page still
    /// standing below it (spec: UTUI#the-configuration-pages).
    #[test]
    fn a_message_sits_under_the_header_over_the_page() {
        let mut state = loaded();
        state.set_error("quota_enabled: refused by the service");
        let text = frame(&state, 100, 24);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains("refused by the service"), "{text}");
        assert!(lines[2].contains("PROVIDERS"), "{text}");
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
