//! The settings page: the gates, the session and ping headers, the notice
//! styles, and the two values that are fixed before the store is opened,
//! each in the group it belongs to (spec: UTUI#settings).
//!
//! A value left unset says what it falls back to, in the row, so no row
//! reads as blank where it is doing something.

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::check;
use super::form;
use super::model::{Row, Snapshot, State, Target};
use crate::tui::chrome::RowHit;
use crate::tui::table::{self, BodyLine, Cells, Table, heading, span, widest};
use crate::tui::tone::{Tone, dim, toned};

fn label(row: Row, snapshot: &Snapshot) -> String {
    match row {
        Row::Quota => "quota gate".to_owned(),
        Row::Cold => "cold gate".to_owned(),
        Row::ColdOutlook => "cold outlook".to_owned(),
        Row::ColdMinTokens => "cold min tokens".to_owned(),
        Row::ColdIdleMin => "cold idle minutes".to_owned(),
        Row::SessionHeaders => "session headers".to_owned(),
        Row::PingHeader => "ping header".to_owned(),
        Row::Awake => "idle-sleep lock".to_owned(),
        Row::TranscriptRoots => "transcript roots".to_owned(),
        Row::NoticeDefault => "default notice style".to_owned(),
        Row::Notice(at) => snapshot.profiles().get(at).cloned().unwrap_or_default(),
        Row::Port => "listener port".to_owned(),
        Row::Store => "store".to_owned(),
        Row::Provider(_) => String::new(),
    }
}

/// A value, then what it falls back to in dim where it is not set.
fn set(value: impl Into<String>) -> Vec<Span<'static>> {
    vec![Span::raw(value.into())]
}

fn fallback(value: impl Into<String>, note: &str) -> Vec<Span<'static>> {
    vec![Span::raw(value.into()), span(format!("  {note}"), dim())]
}

fn flag(value: Option<bool>) -> Vec<Span<'static>> {
    match value {
        Some(true) => set("on"),
        Some(false) => set("off"),
        // Every switch here is on until it is set.
        None => fallback("on", "built-in"),
    }
}

fn list(value: &Option<Vec<String>>, built_in: Option<String>) -> Vec<Span<'static>> {
    match (value, built_in) {
        (Some(items), _) if !items.is_empty() => set(items.join(" · ")),
        (None, Some(built_in)) => fallback(built_in, "built-in"),
        _ => vec![toned("none", Tone::Dim)],
    }
}

/// What a row reads at rest.
fn value(row: Row, snapshot: &Snapshot) -> Vec<Span<'static>> {
    let settings = &snapshot.settings;
    match row {
        Row::Quota => flag(settings.quota_enabled),
        Row::Cold => flag(settings.cold_enabled),
        Row::ColdOutlook => flag(settings.cold_outlook),
        Row::Awake => flag(settings.awake),
        Row::ColdMinTokens => {
            let mut spans = match settings.cold_min_tokens {
                Some(tokens) => set(check::tokens_shown(tokens)),
                None => fallback(
                    check::tokens_shown(crate::middleware::cold::DEFAULT_MIN_TOKENS),
                    "built-in",
                ),
            };
            // Turning off the provider that supplied the largest window
            // strands a minimum typed when it was reachable, and an unset
            // one is never typed at all, so the page says it here rather
            // than waiting for an edit to refuse.
            if snapshot.cold_min_unreachable {
                let largest = snapshot
                    .largest_context_window
                    .map(check::tokens_shown)
                    .unwrap_or_else(|| "none".to_owned());
                spans.push(toned(
                    format!("  more than any model's context, {largest}: the gate never fires"),
                    Tone::Warn,
                ));
            }
            spans
        }
        Row::ColdIdleMin => match settings.cold_idle_min {
            Some(minutes) => set(minutes.to_string()),
            None => vec![toned("follows lane TTL", Tone::Dim)],
        },
        Row::SessionHeaders => list(
            &settings.session_headers,
            Some(crate::config::DEFAULT_SESSION_HEADERS.join(" · ")),
        ),
        Row::PingHeader => match &settings.ping_header {
            Some(name) => set(name.clone()),
            None => fallback(crate::config::DEFAULT_PING_HEADER, "built-in"),
        },
        Row::TranscriptRoots => list(&settings.transcript_roots, None),
        Row::NoticeDefault => match &settings.notice_default {
            Some(style) => set(style.clone()),
            None => fallback(snapshot.default_notice(), "built-in"),
        },
        Row::Notice(at) => {
            let Some(profile) = snapshot.profiles().get(at).cloned() else {
                return Vec::new();
            };
            match snapshot.notices.get(&profile) {
                Some(style) => set(style.clone()),
                None if crate::config::NoticesConfig::default()
                    .frontends
                    .contains_key(&profile) =>
                {
                    fallback(snapshot.notice_fallback(&profile), "built-in")
                }
                None => fallback(snapshot.default_notice(), "follows default"),
            }
        }
        Row::Port => set(snapshot.port.to_string()),
        Row::Store => set(snapshot.db_path.clone()),
        Row::Provider(_) => Vec::new(),
    }
}

/// Where a fixed value is settled, since it is not edited here.
fn settled_by(row: Row) -> Option<&'static str> {
    match row {
        Row::Port => Some("set by toker setup"),
        Row::Store => Some("set in toker.toml"),
        _ => None,
    }
}

/// A group's head: its rail turning the corner, its name and how many rows
/// it holds, and the rule running to the edge.
fn head(name: &str, count: usize, width: usize) -> Line<'static> {
    let lead = "┌─ ";
    let tail = format!("  {count} ");
    let used = lead.width() + name.width() + tail.width();
    Line::from(vec![
        span(lead, dim()),
        span(name.to_owned(), crate::tui::tone::bold()),
        span(tail, dim()),
        span("─".repeat(width.saturating_sub(used)), dim()),
    ])
}

pub(super) fn draw(
    frame: &mut Frame,
    body: Rect,
    state: &State,
    snapshot: &Snapshot,
) -> Vec<RowHit> {
    let width = usize::from(body.width.saturating_sub(4));
    let groups = state.groups();
    let label_width = widest(
        groups
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .map(|row| label(*row, snapshot))
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str),
    ) + 2;
    // The fixed rows' values line up so what settled each is read down a
    // column.
    let fixed_width = widest(
        groups
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .filter(|row| settled_by(**row).is_some())
            .map(|row| {
                value(*row, snapshot)
                    .iter()
                    .map(|piece| piece.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str),
    ) + 3;

    // What settled a fixed value goes before the value is cut short: the
    // path is the reading, the note is its provenance
    // (spec: UTUI#what-is-shed-first).
    let note_width = widest(
        groups
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .filter_map(|row| settled_by(*row)),
    );
    let notes = 2 + label_width + fixed_width + note_width <= width;

    let rail = || span("│", dim());
    let mut lines: Vec<BodyLine> = Vec::new();
    let mut entry = 0;
    for (name, rows) in &groups {
        lines.push(BodyLine::plain(head(name, rows.len(), width)));
        for row in rows {
            let mut cells = Cells::new()
                .raw(vec![rail(), Span::raw(" ")])
                .cell(vec![Span::raw(label(*row, snapshot))], label_width);
            match settled_by(*row) {
                Some(by) if notes => {
                    cells = cells
                        .cell(value(*row, snapshot), fixed_width)
                        .raw(vec![span(by, dim())]);
                }
                _ => cells = cells.raw(value(*row, snapshot)),
            }
            lines.push(BodyLine::of(cells.line(), entry));
            if state.selected() == entry
                && let Some(editing) = state.editing()
                && matches!(editing.target, Target::Setting(_))
            {
                lines.extend(form::lines(
                    editing,
                    snapshot,
                    vec![rail(), Span::raw("     ")],
                    width,
                ));
            }
            entry += 1;
        }
    }
    let header = Cells::new()
        .raw(vec![Span::raw("  ")])
        .cell(heading("setting"), label_width)
        .raw(heading("value"))
        .line();
    let table = Table {
        header,
        body: lines,
    };
    let split = table::split(body, 1 + table.body.len(), 0);
    table::draw_table(
        frame,
        split.table,
        "SETTINGS",
        split.chrome,
        table,
        Some(state.selected()),
    )
    .rows
}
