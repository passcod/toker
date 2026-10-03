//! Rendering: a [`Snapshot`] plus a frame in, pixels out.
//!
//! ratatui over crossterm, alternate screen handled by the parent module's
//! init/restore. [`render`] is a pure function of the snapshot and the
//! clock string — no I/O, no time reads — so the TestBackend tests below
//! assert real buffer contents at several terminal sizes without ever
//! touching a real terminal.
//!
//! Layout, phase 1 (the grown panel set lands in phase 5 and slots into
//! the same frame):
//!
//! ```text
//! ─ toker · live · last 30m · 2 sessions · 17 requests in window   12:34:56
//! ┌ SESSIONS ────────────────────────┐ ┌ SPEND ─────┐ ┌ RATE ──────┐
//! │ table, sheds rightmost columns   │ │ billed     │ │ 0.6/min    │
//! │ when the terminal narrows         │ │ breakdown  │ │ ▁▂▃█  err  │
//! └───────────────────────────────────┘ └────────────┘ └────────────┘
//! ```
//!
//! Invariant 3 in rendering: every unknown renders as an explicit string —
//! `?` for unknown models and token counts, "no billed cost data",
//! "no cost data: N", "no requests in window" — never as a confident
//! zero.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Paragraph, Row, Table},
};

use super::model::{NO_SESSION, Snapshot};

/// Bottom panel row height: SPEND and RATE side by side. Tall enough for
/// the total line, the never-dropped "no cost data" line, and a few
/// breakdown lines.
const BOTTOM_HEIGHT: u16 = 9;

/// The sessions table's columns, left to right, with their base widths.
/// When the terminal is too narrow the *rightmost* columns shed first
/// (see [`session_plan`]) — columns never wrap and never squeeze.
const SESSION_COLUMNS: [(&str, u16); 7] = [
    ("SESSION", 18),
    ("MODEL", 22),
    ("REQS", 5),
    ("IN NOW", 8),
    ("PEAK", 8),
    ("OUT", 7),
    ("LAST", 8),
];

/// Sparkline blocks, low → high (▁▂▃▄▅▆▇█ style; a space is a flat minute).
const BLOCKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// The whole frame. `clock` is the preformatted HH:MM:SS string so tests
/// stay deterministic.
pub(crate) fn render(frame: &mut Frame, snap: &Snapshot, clock: &str) {
    let [header, sessions, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(BOTTOM_HEIGHT),
    ])
    .areas(frame.area());

    render_header(frame, header, snap, clock);
    render_sessions(frame, sessions, snap);
    let [spend, rate] =
        Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).areas(bottom);
    render_spend(frame, spend, snap);
    render_rate(frame, rate, snap);
}

/// Header line: `toker · live · last 30m` + window summary, clock at the
/// right edge. An empty window says "no requests in window" — absence,
/// not "0 requests".
fn render_header(frame: &mut Frame, area: Rect, snap: &Snapshot, clock: &str) {
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(8)]).areas(area);

    let mut text = format!("toker · live · last {}m", snap.window_mins);
    if snap.window_empty {
        text.push_str(" · no requests in window");
    } else {
        let sessions = snap.sessions.len();
        text.push_str(&format!(
            " · {sessions} session{} · {} requests in window",
            if sessions == 1 { "" } else { "s" },
            snap.window_requests
        ));
    }
    match snap.total_requests {
        0 => text.push_str(" · ledger empty"),
        total => text.push_str(&format!(" · {total} in ledger")),
    }
    frame.render_widget(Paragraph::new(text.bold()), left);
    frame.render_widget(
        Paragraph::new(clock).alignment(Alignment::Right).bold(),
        right,
    );
}

/// The sessions table. Column shedding is a width-fallback chain: keep the
/// leftmost columns that fit, give the leftover width to SESSION.
fn render_sessions(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let block = Block::bordered().title_top("SESSIONS");
    if snap.sessions.is_empty() {
        let message = if snap.window_empty {
            "no requests in window"
        } else {
            "no sessions in window"
        };
        frame.render_widget(Paragraph::new(message).dim().block(block), area);
        return;
    }

    let inner = block.inner(area);
    let (headers, widths) = session_plan(inner.width);
    let header = Row::new(headers.iter().map(|h| (*h).to_string())).style(Style::new().bold());
    let rows = snap
        .sessions
        .iter()
        .map(|s| Row::new(session_cells(s, snap.now_ms, headers.len())));
    frame.render_widget(Table::new(rows, widths).header(header).block(block), area);
}

/// The width-fallback chain: the largest prefix of [`SESSION_COLUMNS`]
/// (plus inter-column spacing) that fits `available` cells, with every
/// leftover cell handed to the SESSION column. Always returns at least
/// one column, pinned to the available width when even that cannot fit.
fn session_plan(available: u16) -> (Vec<&'static str>, Vec<Constraint>) {
    let width_of = |count: usize| -> u16 {
        SESSION_COLUMNS[..count]
            .iter()
            .map(|(_, w)| *w)
            .sum::<u16>()
            .saturating_add(count.saturating_sub(1) as u16)
    };
    let mut count = SESSION_COLUMNS.len();
    while count > 1 && width_of(count) > available {
        count -= 1;
    }
    let mut widths: Vec<u16> = SESSION_COLUMNS[..count].iter().map(|(_, w)| *w).collect();
    let needed = width_of(count);
    if needed <= available {
        widths[0] += available - needed; // SESSION absorbs the slack
    } else {
        widths[0] = available.max(1); // degenerate width: one clipped column
    }
    (
        SESSION_COLUMNS[..count].iter().map(|(h, _)| *h).collect(),
        widths.into_iter().map(Constraint::Length).collect(),
    )
}

/// One table row for a session; cells are produced only for the surviving
/// columns so shedding never leaves stray data.
fn session_cells(session: &super::model::SessionAgg, now_ms: i64, count: usize) -> Vec<String> {
    let mut cells = Vec::with_capacity(count);
    let mut push_if = |n: usize, value: String| {
        if n < count {
            cells.push(value);
        }
    };
    push_if(0, session.session.clone());
    push_if(1, session.model.clone().unwrap_or_else(|| "?".into()));
    push_if(2, session.requests.to_string());
    push_if(3, unknown_or(session.input_now));
    push_if(4, unknown_or(session.input_peak));
    push_if(5, unknown_or(session.output_total));
    push_if(6, rel_age(now_ms - session.latest_ts_ms));
    cells
}

/// Unknown token counts render as `?`, never as a fake zero.
fn unknown_or(value: Option<i64>) -> String {
    value.map(|n| n.to_string()).unwrap_or_else(|| "?".into())
}

/// Rough relative age for the LAST column.
fn rel_age(delta_ms: i64) -> String {
    let secs = delta_ms / 1_000;
    if secs < 10 {
        "now".to_string()
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3_600)
    }
}

/// SPEND: billed total, per-provider·model breakdown, and the never-dropped
/// "no cost data" count (invariant 3: a request whose cost the provider
/// never reported is visible, not folded into the total).
fn render_spend(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let block = Block::bordered().title_top("SPEND");
    if snap.window_empty {
        frame.render_widget(Paragraph::new("no data in window").dim().block(block), area);
        return;
    }
    let spend = &snap.spend;
    let mut lines = Vec::with_capacity(3 + spend.breakdown.len());
    match spend.billed_total {
        Some(total) => lines.push(Line::from(format!(
            "billed: {} ({} reqs)",
            usd(total),
            spend.billed_requests
        ))),
        None => lines.push(Line::from("billed: no billed cost data")),
    }
    // Positioned third-from-last at worst and second here: this line is
    // the invariant-3 counter, so it must never scroll out of view.
    lines.push(Line::from(format!(
        "no cost data: {} reqs",
        spend.no_cost_data
    )));
    for entry in &spend.breakdown {
        lines.push(Line::from(format!(
            "{} · {}  {} · {} reqs",
            entry.provider.as_deref().unwrap_or(NO_SESSION),
            entry.model.as_deref().unwrap_or(NO_SESSION),
            usd(entry.billed),
            entry.requests
        )));
    }
    if spend.other_cost_kinds > 0 {
        lines.push(Line::from(format!(
            "{} reqs with non-billed cost",
            spend.other_cost_kinds
        )));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// RATE: requests per minute, the per-minute sparkline (minutes with
/// errors flagged in red), and the error/drift counters.
fn render_rate(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let block = Block::bordered().title_top("RATE");
    let inner = block.inner(area);
    let mut lines = vec![Line::from(format!("{:.1}/min", snap.rate.per_minute))];

    let buckets = &snap.rate.buckets;
    let max = buckets.iter().map(|b| b.requests).max().unwrap_or(0);
    // Keep the newest buckets when the terminal is too narrow for the
    // whole window.
    let shown = buckets.len().min(inner.width as usize);
    let spans: Vec<Span> = buckets[buckets.len() - shown..]
        .iter()
        .map(|bucket| {
            let block_char = if bucket.requests == 0 {
                " "
            } else {
                let level = (bucket.requests * BLOCKS.len()).div_ceil(max);
                BLOCKS[(level - 1).min(BLOCKS.len() - 1)]
            };
            if bucket.errors > 0 {
                Span::styled(
                    block_char,
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(block_char)
            }
        })
        .collect();
    lines.push(Line::from(spans));
    lines.push(Line::from(format!(
        "errors: {} · drift: {}",
        snap.errors, snap.drift
    )));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Dollar formatting for the spend panel.
fn usd(value: f64) -> String {
    format!("${value:.6}")
}

#[cfg(test)]
mod tests {
    use super::super::model;
    use super::super::testrows::{bare, billed, kind_row};
    use crate::store::RowKind;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const NOW: i64 = 1_769_000_000_000;

    /// The shared synthetic window: two sessions plus a NULL-session group,
    /// a billed spread for the sparkline, a NULL-cost request, and one
    /// error row.
    fn snapshot() -> model::Snapshot {
        let mut rows = Vec::new();
        // Counts 1..=8 in the eight minutes ending 1 minute ago → the
        // sparkline shows a contiguous ▁▂▃▄▅▆▇█ ramp (the billed request
        // below joins the count-8 minute, the unpriced one the newest).
        for count in 1..=8 {
            for _ in 0..count {
                let mut row = bare(NOW - (9 - count as i64) * 60_000);
                row.session_id = Some("ses-abc".into());
                row.model = Some("z-ai/glm-5.3".into());
                row.provider = Some("openrouter".into());
                rows.push(row);
            }
        }
        // A billed request and an unpriced one from the dash session.
        rows.push(billed(
            NOW - 90_000,
            Some("ses-abc"),
            "z-ai/glm-5.3",
            "openrouter",
            12_345,
            100_000,
            678,
            0.00213,
        ));
        let mut unpriced = bare(NOW - 45_000);
        unpriced.session_id = None;
        unpriced.model = Some("z-ai/glm-5.3".into());
        unpriced.provider = Some("openrouter".into());
        unpriced.input = Some(500);
        rows.push(unpriced);
        rows.push(kind_row(NOW - 10_000, RowKind::Error));
        model::aggregate(&rows, 30, NOW, 523)
    }

    fn rendered(snap: &model::Snapshot, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| super::render(frame, snap, "12:34:56"))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn renders_all_panels_at_a_comfortable_size() {
        let text = rendered(&snapshot(), 100, 30);
        for expected in [
            "toker · live · last 30m",
            "12:34:56",
            "2 sessions",
            "523 in ledger",
            "SESSIONS",
            "ses-abc",
            "z-ai/glm-5.3",
            "no cost data: 37 reqs",
            "$0.002130",
            "openrouter · z-ai/glm-5.3",
            "errors: 1",
            "drift: 0",
        ] {
            assert!(text.contains(expected), "expected {expected:?} in:\n{text}");
        }
        // The per-minute ramp: counts 1..8 in consecutive minutes render
        // as the full block ladder.
        assert!(text.contains("▁▂▃▄▅▆▇█"), "sparkline ramp in:\n{text}");
    }

    #[test]
    fn empty_window_renders_absence_not_zero() {
        let snap = model::aggregate(&[], 30, NOW, 523);
        let text = rendered(&snap, 100, 30);
        assert!(text.contains("no requests in window"));
        assert!(text.contains("no data in window"));
        assert!(
            text.contains("523 in ledger"),
            "the count query is still real"
        );
        assert!(!text.contains("$"), "no dollar figure is invented");
    }

    #[test]
    fn truly_empty_ledger_says_ledger_empty() {
        let snap = model::aggregate(&[], 30, NOW, 0);
        let text = rendered(&snap, 80, 24);
        assert!(text.contains("ledger empty"));
        assert!(text.contains("no requests in window"));
    }

    #[test]
    fn narrow_terminal_sheds_rightmost_columns() {
        let snap = snapshot();
        // 100 wide: the full column set, model included.
        let wide = rendered(&snap, 100, 30);
        assert!(wide.contains("z-ai/glm-5.3"));
        assert!(wide.contains("MODEL"));
        assert!(wide.contains("PEAK"));

        // 40 wide (sessions block ≈ 24 inner cells): the chain sheds down
        // to the SESSION column alone — the id survives, the model and
        // token columns do not (clipped, not wrapped).
        let narrow = rendered(&snap, 40, 20);
        assert!(narrow.contains("ses-abc"), "session ids always survive");
        assert!(!narrow.contains("z-ai/glm-5.3"), "model column is shed");
        assert!(!narrow.contains("PEAK"), "rightmost columns are shed");

        // 80 wide: an intermediate level — model kept, LAST shed.
        let mid = rendered(&snap, 80, 24);
        assert!(mid.contains("z-ai/glm-5.3"));
        assert!(mid.contains("MODEL"));
        assert!(!mid.contains("LAST"));
    }

    #[test]
    fn tiny_terminal_does_not_panic_and_keeps_the_session_column() {
        let snap = snapshot();
        // Height 12 leaves the sessions panel only its borders below the
        // header and the fixed bottom row; width 16 is barely enough for
        // the panel titles. The point: no panic, panel titles survive.
        let text = rendered(&snap, 16, 12);
        assert!(text.contains("SESSIONS"));
        assert!(text.contains("RATE"));
    }

    #[test]
    fn column_chain_sheds_in_order() {
        let plan = |width: u16| super::session_plan(width).0.to_vec();
        // Full set: 76 + 6 spacing = 82 cells needed.
        assert_eq!(
            plan(82),
            ["SESSION", "MODEL", "REQS", "IN NOW", "PEAK", "OUT", "LAST"]
        );
        assert_eq!(
            plan(100),
            ["SESSION", "MODEL", "REQS", "IN NOW", "PEAK", "OUT", "LAST"]
        );
        // Each step down sheds exactly the rightmost surviving column.
        assert_eq!(
            plan(81),
            ["SESSION", "MODEL", "REQS", "IN NOW", "PEAK", "OUT"]
        );
        assert_eq!(
            plan(73),
            ["SESSION", "MODEL", "REQS", "IN NOW", "PEAK", "OUT"]
        );
        assert_eq!(plan(72), ["SESSION", "MODEL", "REQS", "IN NOW", "PEAK"]);
        assert_eq!(plan(64), ["SESSION", "MODEL", "REQS", "IN NOW"]);
        assert_eq!(plan(55), ["SESSION", "MODEL", "REQS"]);
        assert_eq!(plan(45), ["SESSION", "MODEL"]);
        assert_eq!(plan(18), ["SESSION"]);
        // Degenerate: never empty, pinned to whatever exists.
        assert_eq!(plan(5), ["SESSION"]);
        // Leftover width lands on SESSION: at 90 cells, 82 are needed.
        let (_, widths) = super::session_plan(90);
        assert_eq!(widths[0], ratatui::layout::Constraint::Length(18 + 8));
    }
}
