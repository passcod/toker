//! Rendering the configuration pages.
//!
//! A pure function of the state, like every other view here, so the frames
//! can be asserted at several terminal sizes without a real terminal
//! (spec: UTUI#rendering).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;

use super::model::{State, View};
use super::{providers, schedule, settings};
use crate::tui::chrome::{self, Chrome, RowHit, Section};
use crate::tui::table;

/// Where the last frame put things, for the mouse.
#[derive(Debug, Clone, Default)]
pub(super) struct Drawn {
    pub(super) header: chrome::Header,
    pub(super) tabs: Vec<ratatui::layout::Rect>,
    pub(super) rows: Vec<RowHit>,
}

/// The tabs, in the order `1` to `3` name them.
const TABS: [&str; 3] = ["providers", "settings", "schedule"];

/// Keys on every view.
pub(super) const EVERYWHERE: [(&str, &str); 6] = [
    ("↑/↓ j/k", "select a row"),
    ("←/→ h/l 1 2 3", "switch between the tabs"),
    ("r", "refresh"),
    ("esc", "back to the dashboard"),
    ("tab", "next page"),
    ("q", "quit"),
];

pub(super) const ON_PROVIDERS: [(&str, &str); 8] = [
    ("space", "turn the provider on or off"),
    ("e", "enter its key, which is never shown"),
    ("u", "set its endpoint"),
    ("f", "codex: set the refresh url"),
    ("v", "codex: pin a client version"),
    ("p", "codex: probe for the version, or not"),
    ("c", "clear its safeguards rejection marks"),
    ("enter", "open the first route that reaches it"),
];

pub(super) const ON_SETTINGS: [(&str, &str); 3] = [
    ("space", "switch a gate or the lock"),
    ("e", "edit the row in a form"),
    ("enter", "switch or edit, as the row takes"),
];

pub(super) const ON_SCHEDULE: [(&str, &str); 5] = [
    ("space", "off, while awake, wake then ping"),
    ("n", "add a slot, as 07:20 Mon..Fri"),
    ("e enter", "edit the slot"),
    ("d", "remove the slot"),
    ("x", "stop waking with nothing to ping"),
];

pub(super) const IN_A_FORM: [(&str, &str); 2] = [
    ("enter ctrl+s", "save, once nothing is refused"),
    ("esc", "cancel"),
];

/// What the page's marks and shorthand say.
pub(super) const READINGS: [(&str, &str); 8] = [
    ("none", "on, with no credential to answer with"),
    ("not configured", "the same, said in words"),
    ("marked unsupported", "a refusal marked the binding"),
    ("built-in", "unset: this is what it falls back to"),
    ("follows lane TTL", "unset: the lane's own cache tier"),
    ("175k  1M  1.5M", "token counts, in full or shorthand"),
    ("empty entry", "puts the setting back to unset"),
    ("-", "not read from the service"),
];

pub(super) fn render(frame: &mut Frame, state: &State, overlay: bool) -> Drawn {
    let current = View::ALL
        .iter()
        .position(|view| *view == state.view())
        .unwrap_or(0);
    // Providers are a list of records, so the tab says how many.
    let providers = state.snapshot().map(|snapshot| snapshot.providers.len());
    let strip = [
        match providers {
            Some(count) => chrome::Tab::new(TABS[0]).count(count),
            None => chrome::Tab::new(TABS[0]),
        },
        chrome::Tab::new(TABS[1]).after(" · "),
        // The slots are a list of records, as the providers are, once read.
        match state.machine() {
            Some(machine) => chrome::Tab::new(TABS[2])
                .count(machine.slots.len())
                .after(" · "),
            None => chrome::Tab::new(TABS[2]).after(" · "),
        },
    ];
    let status = status(state);
    let chrome::Laid { header, tabs, body } = chrome::page(
        frame,
        crate::tui::Page::Config,
        state.error(),
        &chrome::Strip {
            tabs: &strip,
            current,
            status: &status,
        },
    );

    let rows = if !state.reachable() {
        // The service is the only writer, so a form the operator can
        // fill in while it is down is a form whose edits go nowhere
        // (spec: UTUI#the-configuration-pages).
        let lines = vec![
            Line::from("the service is not answering"),
            Line::from("configuration is edited through it, so there is nothing to show").dim(),
        ];
        let framing = Chrome::fitting(lines.len(), body.height);
        let area = Rect {
            height: (lines.len() as u16 + framing.cost()).min(body.height),
            ..body
        };
        table::draw_detail(frame, area, "CONFIGURATION", framing, lines);
        Vec::new()
    } else if let Some(snapshot) = state.snapshot() {
        match state.view() {
            View::Providers => providers::draw(frame, body, state, snapshot),
            View::Settings => settings::draw(frame, body, state, snapshot),
            View::Schedule => schedule::draw(frame, body, state, snapshot),
        }
    } else {
        Vec::new()
    };

    chrome::chips(frame, &header);
    if overlay {
        // Under the header, so the buttons stay in view.
        let area = frame.area();
        let used = header.keys.height;
        chrome::overlay_in(
            frame,
            Rect {
                y: area.y + used,
                height: area.height - used,
                ..area
            },
            "KEYS",
            &[
                &[
                    Section {
                        heading: Some("ON EVERY TAB"),
                        rows: &EVERYWHERE,
                    },
                    Section {
                        heading: Some("ON PROVIDERS"),
                        rows: &ON_PROVIDERS,
                    },
                ],
                &[
                    Section {
                        heading: Some("ON SETTINGS"),
                        rows: &ON_SETTINGS,
                    },
                    Section {
                        heading: Some("ON SCHEDULE"),
                        rows: &ON_SCHEDULE,
                    },
                    Section {
                        heading: Some("IN A FORM"),
                        rows: &IN_A_FORM,
                    },
                    Section {
                        heading: Some("MARKS"),
                        rows: &READINGS,
                    },
                ],
            ],
            true,
        );
    }
    Drawn { header, tabs, rows }
}

/// The header's status: how many providers are on, and that a form is open.
fn status(state: &State) -> String {
    let Some(snapshot) = state.snapshot().filter(|_| state.reachable()) else {
        return String::new();
    };
    let on = snapshot
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .count();
    let on = format!("{on} {} on", if on == 1 { "provider" } else { "providers" });
    if state.editing().is_some() {
        format!("editing · {on}")
    } else {
        on
    }
}
