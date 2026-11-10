//! The routing page's `?` overlay: the keys it acts on, grouped by where
//! each applies, beside every mark the page draws and what it means
//! (spec: UTUI#finding-the-keys, UTUI#marks).

use ratatui::{Frame, layout::Rect};

use crate::tui::chrome::{self, Section};

/// Keys that apply on a tab's table and on a route's page alike.
pub(super) const ANYWHERE: [(&str, &str); 16] = [
    ("↑/↓ j/k", "select a row; at a panel's edge, the next"),
    ("←/→ h/l", "previous or next tab, route or catalogue"),
    ("1–6", "jump to a tab"),
    ("pgup/pgdn", "move a page of rows"),
    ("e", "edit the row, or the candidate, in place"),
    ("n", "new row below; on a route, a fallback"),
    ("m", "lift a rewrite or candidate; ↑/↓ moves it"),
    ("", "enter drops it, esc puts it back"),
    ("d", "delete; ctrl+s confirms, esc keeps"),
    ("t", "trace a request, invented or recent"),
    ("p", "promote a learned model, on models"),
    ("enter", "open the route a row sends to"),
    ("r", "refresh"),
    ("esc", "close a trace, then a route, then leave"),
    ("tab", "next page"),
    ("q", "quit"),
];

pub(super) const IN_HISTORY: [(&str, &str); 3] = [
    ("a", "make the selected revision live"),
    ("m", "move running lanes; m again to confirm"),
    ("c", "trace a request under the revision"),
];

pub(super) const IN_A_TRACE: [(&str, &str); 2] = [
    ("enter", "go to the row the trace names"),
    ("t", "trace another"),
];

pub(super) const IN_AN_EDIT: [(&str, &str); 8] = [
    ("↑/↓", "move between fields"),
    ("←/→", "change the value"),
    ("tab", "complete a name"),
    ("space", "toggle"),
    ("alt+↑/↓", "move a list item or candidate"),
    ("ctrl+d", "remove a list item"),
    ("ctrl+s", "save, once nothing is refused"),
    ("esc", "cancel"),
];

/// What a row of a table, or a line of a trace, can carry.
pub(super) const ROWS: [(&str, &str); 7] = [
    (">", "caught here, or serving the request"),
    ("↓", "fell past; on a route, goes onward"),
    ("┌─ │ └─", "a group of rows and what they share"),
    ("else", "what catches a request no row did"),
    ("tries X next", "a profile's rules missed: X's next"),
    ("${model}", "the name the match captured"),
    ("as asked", "the name sent on unchanged"),
];

/// What a reach, a hit count and a cut list say.
pub(super) const READINGS: [(&str, &str); 9] = [
    ("ok", "the first candidate serves it"),
    ("no cred", "provider on, with no credential"),
    ("off", "provider turned off"),
    ("billed fallback", "a later one serves it, per token"),
    ("unbilled fallback", "a later one serves it, not billed"),
    ("refused", "an edge's setting stops the walk"),
    ("refused · no route", "no rule or default catches it"),
    ("- ≥N", "not recorded · a floor, some unknown"),
    ("… N more", "a list cut short; also … N above"),
];

/// What a presentation's row and detail say.
pub(super) const PRESENTATIONS: [(&str, &str); 5] = [
    ("slug", "the name a client sees it listed under"),
    ("${model_id}", "a listed model's provider id"),
    ("as", "the picker tier it is listed under"),
    ("published", "written to the client's settings"),
    ("unprefixed", "a client using no /f/<name> profile"),
];

/// The revisions, the lanes on them and the header's status.
pub(super) const REVISIONS: [(&str, &str); 9] = [
    ("+ ~ - ↕", "added, changed, removed, moved"),
    ("live", "the revision serving new lanes"),
    ("warm · cold", "a lane's prompt cache live · lapsed"),
    ("[m] reversed", "armed: press again to go ahead"),
    ("N lanes on rev M", "warm lanes left on an older revision"),
    ("claude pending", "settings lag: or failed, unsupported"),
    ("no policy yet", "the live revision holds no record"),
    ("→ ·", "lookup order; models, history apart"),
    ("‹ ›", "tabs scrolled out of the strip"),
];

/// The overlay, in `area`: the keys down the left, the marks down the right,
/// one over the other where `area` is too narrow for both. `area` starts
/// under the header, which keeps its buttons showing.
pub(super) fn render(frame: &mut Frame, area: Rect) {
    let keys = [
        Section {
            heading: Some("ON A TABLE OR A ROUTE"),
            rows: &ANYWHERE,
        },
        Section {
            heading: Some("IN HISTORY"),
            rows: &IN_HISTORY,
        },
        Section {
            heading: Some("IN A TRACE"),
            rows: &IN_A_TRACE,
        },
        Section {
            heading: Some("IN AN EDIT"),
            rows: &IN_AN_EDIT,
        },
    ];
    let marks = [
        Section {
            heading: Some("MARKS"),
            rows: &ROWS,
        },
        Section {
            heading: Some("READINGS"),
            rows: &READINGS,
        },
        Section {
            heading: Some("PRESENTATIONS"),
            rows: &PRESENTATIONS,
        },
        Section {
            heading: Some("REVISIONS AND THE HEADER"),
            rows: &REVISIONS,
        },
    ];
    chrome::overlay_in(frame, area, "KEYS", &[&keys, &marks], true);
}
