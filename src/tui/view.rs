//! Rendering: a [`Snapshot`] plus a frame in, pixels out.
//!
//! ratatui over crossterm, alternate screen handled by the parent module's
//! init/restore. [`render`] is a pure function of the snapshot, the
//! clock string, and the timezone — no I/O, no time reads — so the
//! TestBackend tests below assert real buffer contents at several
//! terminal sizes without ever touching a real terminal.
//!
//! Layout, phase 2:
//!
//! ```text
//! last 30m · 2 sessions · 17 requests       last req 12s ago · 12:34:56
//! ┌ SESSIONS ────────────────────────────────────────────────────────┐
//! │ table, sheds rightmost columns when the terminal narrows          │
//! └────────────────────────────────────────────────────────────────────┘
//! ┌ SPEND ─────────────┐ ┌ RATE & QUOTA ──────────────────────────────┐
//! │ billed             │ │ 0.6/min  ▁▂▃█  errors: 0 · drift: 0        │
//! │ breakdown          │ │ 5-hour   ██░░░░  12%  resets 14:53 · on track
//! │ no cost data: N    │ │ 7-day    ██████░  74%  resets Mon 05:00 · stops ~Thu 08:49
//! └─────────────────────┘ │ overage  ██████░  64%  resets 1 Oct · estimating
//!                         │ spent    today +3%  ·  30m <1%            │
//!                         │ binding  five_hour                         │
//!                         └────────────────────────────────────────────┘
//! ```
//! (the bottom strip holds SPEND and RATE & QUOTA side by side, and the
//! quota lines render only when the snapshot carries a quota section —
//! absence renders nothing, never zeros).
//!
//! Invariant 3 in rendering: every unknown renders as an explicit string —
//! `?` for unknown models and token counts, "no billed cost data",
//! "no cost data: N", "no requests in window", `estimating`/`window
//! rolled over`/`no data` for the meters, `no data` for the header's
//! freshness on an empty ledger — never as a confident zero.

use jiff::tz::TimeZone;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Cell, Paragraph, Row, Table},
};

use super::labels::short_dir;
use super::model::{HitRate, NO_SESSION, SessionAgg, Snapshot};
use super::quota::{MeterPanel, Spent};
use super::rebuilds::REBUILD_MIN;
use crate::catalog::windows::ContextWindow;
use crate::middleware::cold::{Verdict, alongside, reset_label};

/// Bottom panel row height while SPEND renders beside RATE: tall
/// enough for the total line, the never-dropped "no cost data" line,
/// and a few breakdown lines. The quota section grows it (see
/// [`bottom_height`]); without SPEND the strip is RATE's own height.
const BOTTOM_HEIGHT: u16 = 9;

/// The meter bar's width (22 cells), the widest
/// it ever renders — it shrinks as the panel narrows rather than
/// letting the line wrap or the verdict clip.
const BAR_WIDTH: u16 = 22;

/// The narrowest the meter bar shrinks to before the line sheds a
/// clause instead: below this a bar stops reading as a share, while
/// the reset clock is the meter's other answer.
const BAR_MIN_WIDTH: usize = 10;

/// Past this a session is not mid-turn; the context list goes quiet
/// about it.
const IDLE_SECS: i64 = 180;

/// The header's freshness stays green while the ledger's newest row is
/// younger than this, and turns yellow past it (the predecessor's
/// thresholds, kept so a glance reads the same).
const FRESH_SECS: i64 = 30;

/// Past this the proxy has been quiet long enough to look dead: the
/// freshness turns red and counts in minutes.
const STALE_SECS: i64 = 300;

/// The context panel lists sessions whose main lane holds at least this
/// many requests — occupancy is a claim about a conversation, and two
/// rows say nothing yet (a "fewer than three" check).
const CONTEXT_MIN_REQUESTS: usize = 3;

/// The least width a session NAME renders in (`labelW >= 12`):
/// whatever is left over after the table goes to
/// the label, and less than this is noise — the id renders instead.
const LABEL_MIN_W: u16 = 12;

/// The CONTEXT panel's name field: wide enough for the id forms and
/// short labels, a hard CAP for long titles — the bar is the panel's
/// point, so the name yields before it does (the SESSIONS table shows
/// the same name unclipped at its wider column). Correlation survives
/// a clip: the panels share the same PREFIX.
const CONTEXT_NAME_W: usize = 24;
/// The fixed gap between the name field and the bar — a name at the
/// cap must not touch the bar.
const CONTEXT_NAME_GAP: usize = 2;

/// The rebuild panel's localised detail lines: the newest
/// system-prompt changes, so a change is diagnosable at a glance
/// without leaving the dashboard for `toker report`.
const REBUILD_DETAIL_LINES: usize = 3;

/// The tokens panel's label and amount column widths
/// (`padEnd(15)` / `padStart(13)`).
const TOKENS_LABEL_W: usize = 15;
const TOKENS_AMOUNT_W: usize = 13;

/// The least width a tokens bar renders at — the shared
/// `max(6, …)` floor.
const TOKENS_BAR_MIN_W: usize = 6;

/// The share percentage's rendered width: `" NNN%"`.
const TOKENS_PCT_W: usize = 5;

/// A bucket row's bar width at a given inner width: the label, the
/// amount, the two gaps, the percentage, and a floor's note (its length
/// plus its gap) all come off the top; the floor of
/// [`TOKENS_BAR_MIN_W`] keeps a sliver of shape. A PURE function so
/// the budget is unit-testable directly — the render tests assert
/// structure, not glyph counts.
fn tokens_bar_width(width: usize, note_len: usize) -> usize {
    width
        .saturating_sub(46 + note_len + usize::from(note_len > 0) * 2)
        .max(TOKENS_BAR_MIN_W)
}

/// The hit-rate bar's width: its own fixed text is shorter (no
/// percentage column, the note rides inline), so the budget is
/// lighter — same floor.
fn tokens_rate_bar_width(width: usize, note_len: usize) -> usize {
    width.saturating_sub(32 + note_len).max(TOKENS_BAR_MIN_W)
}

/// The least inner width a bucket row's bar and percentage render in:
/// label, amount, gap, the least bar, and the percentage. Below this
/// the pair sheds whole — the counts are the information, the bar is
/// the shape, and a bar clipped mid-glyph is neither (the reference
/// let the final clamp cut the line; the panel here drops the column
/// pair cleanly instead, the quota meter's rule).
const TOKENS_SHARE_MIN_W: usize =
    TOKENS_LABEL_W + TOKENS_AMOUNT_W + 2 + TOKENS_BAR_MIN_W + TOKENS_PCT_W;

/// The least inner width the hit-rate bar renders in: indent, label,
/// amount, gap, bar. The rate itself rides in the amount column, so
/// the number survives the shed and only the shape goes.
const TOKENS_RATE_BAR_MIN_W: usize = TOKENS_LABEL_W + TOKENS_AMOUNT_W + 2 + TOKENS_BAR_MIN_W;

/// The sessions table's columns, left to right, with their base widths.
/// When the terminal is too narrow the *rightmost* columns shed first
/// (see [`session_plan`]) — columns never wrap and never squeeze.
const SESSION_COLUMNS: [(&str, u16); 10] = [
    ("SESSION", 18),
    ("CTX", 5),
    ("MODEL", 22),
    ("REQS", 5),
    ("IN NOW", 10),
    ("PEAK", 10),
    ("MSGS", 5),
    ("CMPCT", 6),
    ("OUT", 8),
    ("LAST", 8),
];

/// Sparkline blocks, low → high (▁▂▃▄▅▆▇█ style; a space is a flat minute).
const BLOCKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// The whole frame. `clock` is the preformatted HH:MM:SS string and
/// `tz` the zone the quota labels render in, both passed in so tests
/// stay deterministic.
///
/// The panels stack in the reference dashboard's order — SESSIONS /
/// CONTEXT /
/// TOKENS / CACHE REBUILDS, then the bottom strip holding RATE & QUOTA,
/// with the toker-only SPEND beside it when the window carries a cost
/// ([`SpendAgg::carries_cost`](super::model::SpendAgg::carries_cost))
/// — into a height budget
/// computed from the snapshot ([`panel_areas`]): lists grow into
/// slack, and a short terminal sheds panel rows from the top of the
/// middle (sessions first) rather than ever letting the quota block
/// scroll off the bottom, "the part worth watching".
pub(crate) fn render(frame: &mut Frame, snap: &Snapshot, clock: &str, tz: &TimeZone) {
    let [header, sessions, context, tokens, rebuilds, bottom] = panel_areas(snap, frame.area());

    render_header(frame, header, snap, clock);
    render_sessions(frame, sessions, snap);
    render_context(frame, context, snap);
    render_tokens(frame, tokens, snap);
    render_rebuilds(frame, rebuilds, snap);
    if snap.spend.carries_cost() {
        // Even halves: the meters' reset clocks need the width as much
        // as the breakdown lines do.
        let [spend, rate] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).areas(bottom);
        render_spend(frame, spend, snap);
        render_rate(frame, rate, snap, tz);
    } else {
        render_rate(frame, bottom, snap, tz);
    }
}

/// The frame's panel areas, from the snapshot's content and the frame's
/// height: the header and the bottom strip are pinned, the four middle
/// panels get their natural heights plus any slack (the sessions list
/// grows into it, as the reference's two lists did), and a deficit
/// sheds rows top-first among the middle panels — down to each panel's
/// scaffold (borders and a title row) before the scaffolds give way,
/// so the quota block never scrolls.
fn panel_areas(snap: &Snapshot, frame: Rect) -> [Rect; 6] {
    let bottom = bottom_height(snap);
    let room = frame.height.saturating_sub(1 + bottom);
    let [sessions, context, tokens, rebuilds] = middle_heights(snap, room);
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(sessions),
        Constraint::Length(context),
        Constraint::Length(tokens),
        Constraint::Length(rebuilds),
        Constraint::Length(bottom),
    ])
    .areas(frame)
}

/// The four middle panels' heights within `room` rows. Natural heights
/// come from the snapshot; a deficit sheds from the top (sessions
/// first, then context, tokens, rebuilds — the middle cut keeps
/// the tail), each panel keeping its scaffold while it can; slack goes
/// to the sessions list. The result always sums to exactly `room`, so
/// the layout never wraps and never leaves a gap.
fn middle_heights(snap: &Snapshot, room: u16) -> [u16; 4] {
    let natural = [
        sessions_height(snap),
        context_height(snap),
        tokens_height(snap),
        rebuilds_height(snap),
    ];
    // Borders plus a title row: the least a panel can be and still
    // say what it is.
    let scaffold = [3u16, 2, 2, 2];
    let mut out = natural;
    let mut over = out.iter().sum::<u16>().saturating_sub(room);
    for (height, floor) in out.iter_mut().zip(scaffold) {
        let cut = (*height).saturating_sub(floor).min(over);
        *height -= cut;
        over -= cut;
        if over == 0 {
            break;
        }
    }
    // Still short: the scaffolds give way bottom-first — the rebuild
    // and tokens panels fold before the context list, and the sessions
    // panel's title last, matching the guard that keeps the header
    // plus the SESSIONS scaffold over the middle.
    for (height, floor) in out.iter_mut().zip(scaffold).rev() {
        let cut = (*height).min(floor).min(over);
        *height -= cut;
        over -= cut;
        if over == 0 {
            break;
        }
    }
    debug_assert_eq!(over, 0, "the floors sum to more than any real room");
    // Slack: the sessions list takes what is left, like the
    // reference's lists sharing the page.
    let used: u16 = out.iter().sum();
    out[0] += room.saturating_sub(used);
    out
}

/// The sessions panel's natural height: borders, the table header, one
/// row per session — or its explicit empty-state line.
fn sessions_height(snap: &Snapshot) -> u16 {
    2 + 1 + snap.sessions.len().max(1) as u16
}

/// The CONTEXT panel's natural height.
fn context_height(snap: &Snapshot) -> u16 {
    2 + context_rows(snap).max(1) as u16
}

/// The TOKENS panel's natural height: the four input buckets, output,
/// the reasoning row when a provider reported thinking tokens (it is
/// rendered only then), and the hit-rate lines the data supports
/// (no hit-rate line at all when nothing was reused).
fn tokens_height(snap: &Snapshot) -> u16 {
    if snap.window_empty {
        return 2 + 1;
    }
    let mut lines = 4 + 1; // input buckets + output
    if snap.tokens.reasoning.value > 0
        || (snap.tokens.reasoning.unavailable > 0
            && snap.tokens.reasoning.unavailable < snap.tokens.requests)
    {
        lines += 1;
    }
    if !matches!(
        snap.tokens.hit_rate(),
        super::model::HitRate::NothingReusable
    ) {
        lines += 2; // hit rate + missed
    }
    2 + lines
}

/// The CACHE REBUILDS panel's natural height.
fn rebuilds_height(snap: &Snapshot) -> u16 {
    2 + rebuilds_lines(snap).max(1) as u16
}

/// The middle panels' line counts (heights minus borders).
fn context_rows(snap: &Snapshot) -> usize {
    snap.sessions
        .iter()
        .filter(|session| session.lane_requests >= CONTEXT_MIN_REQUESTS)
        .count()
}

/// The rebuild panel's content lines: the summary, the cause table (or
/// its explicit none-line), and the newest localised detail lines.
fn rebuilds_lines(snap: &Snapshot) -> usize {
    let Some(rebuilds) = snap.rebuilds.as_ref() else {
        return 1;
    };
    if snap.window_empty {
        return 1;
    }
    let mut lines = 1 + rebuilds.causes.len().max(1);
    lines += rebuilds
        .events
        .iter()
        .filter(|event| event.cause == super::rebuilds::Cause::SystemPrompt)
        .count()
        .min(REBUILD_DETAIL_LINES);
    lines
}

/// The bottom row's height: RATE & QUOTA's own lines, floored at
/// [`BOTTOM_HEIGHT`] while SPEND renders beside it (its breakdown has
/// no line budget of its own) — the reference
/// dashboard drops middle rows
/// rather than let the quota block scroll off the bottom
/// ("the part worth watching"); here the middle
/// panels shed their rows first ([`middle_heights`]) and the bottom
/// strip is pinned at whatever it needs.
fn bottom_height(snap: &Snapshot) -> u16 {
    let mut lines = 3; // per-minute, sparkline, errors/drift
    if let Some(quota) = &snap.quota {
        lines += quota.meters.len();
        if quota.spent_today.is_some() {
            lines += 1;
        }
        if quota.binding.is_some() {
            lines += 1;
        }
    }
    let rate = (lines + 2) as u16; // + 2 border rows
    if snap.spend.carries_cost() {
        BOTTOM_HEIGHT.max(rate)
    } else {
        rate
    }
}

/// Header line: the window summary on the left, the freshness and the
/// clock at the right edge —
/// `last 60m · 3 sessions · 1 idle · 83 requests … last req 4s ago · 12:34:56`.
/// No title (the dashboard has one job), no "live" (there is no pause
/// to be not-live with), no ledger total (a lifetime row count answers
/// nothing the window does not). An empty window says "no requests in
/// window" — absence, not "0 requests".
///
/// The freshness is the age of the ledger's newest row of any kind
/// ([`Snapshot::latest_row_ts_ms`]), and it renders whether or not the
/// window holds rows: an empty window is exactly when it matters,
/// because a dead proxy looks like a quiet one everywhere else.
///
/// A narrow header sheds in this order: the window summary first (it
/// clips into whatever the right side leaves), then the clock (the
/// time is on every other screen; nothing else says the proxy went
/// quiet), then the "last req" label — the coloured age goes last.
fn render_header(frame: &mut Frame, area: Rect, snap: &Snapshot, clock: &str) {
    let (age, age_style) = freshness(snap);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let label = Span::styled("last req ", dim);
    let age = Span::styled(age, age_style);
    let levels = [
        Line::from(vec![
            label.clone(),
            age.clone(),
            Span::styled(" · ", dim),
            Span::raw(clock).bold(),
        ]),
        Line::from(vec![label, age.clone()]),
    ];
    // Each level needs one blank cell before it, so a clipped summary
    // never runs into the label; the age alone renders regardless.
    let right_line = levels
        .into_iter()
        .find(|line| line.width() < area.width as usize)
        .unwrap_or_else(|| Line::from(age));
    let right_width = (right_line.width() + 1).min(area.width as usize) as u16;
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(right_width)]).areas(area);

    let mut text = format!("last {}m", snap.window_mins);
    if snap.window_empty {
        text.push_str(" · no requests in window");
    } else {
        let sessions = snap.sessions.len();
        let idle = snap
            .sessions
            .iter()
            .filter(|session| snap.now_ms - session.latest_ts_ms >= IDLE_SECS * 1_000)
            .count();
        text.push_str(&format!(
            " · {sessions} session{}",
            if sessions == 1 { "" } else { "s" },
        ));
        if idle > 0 {
            text.push_str(&format!(" · {idle} idle"));
        }
        text.push_str(&format!(" · {} requests", snap.window_requests));
    }
    frame.render_widget(Paragraph::new(text.bold()), left);
    frame.render_widget(
        Paragraph::new(right_line).alignment(Alignment::Right),
        right,
    );
}

/// The freshness text and its colour, the predecessor's format exactly:
/// `Ns ago` green under [`FRESH_SECS`], yellow under [`STALE_SECS`],
/// then `Nm ago` in red; `no data` in red on an empty ledger. The age
/// is against the snapshot's `now_ms`, so it moves on the display tick
/// like every other figure in the frame.
fn freshness(snap: &Snapshot) -> (String, Style) {
    let Some(latest) = snap.latest_row_ts_ms else {
        return ("no data".to_owned(), Style::new().fg(Color::Red));
    };
    // Rounded to the nearest second, as the predecessor did; a row
    // stamped ahead of the frame (rows land slightly out of order, and
    // clocks drift) reads as 0s rather than a negative age.
    let age = ((snap.now_ms - latest).max(0) + 500) / 1_000;
    if age < FRESH_SECS {
        (format!("{age}s ago"), Style::new().fg(Color::Green))
    } else if age < STALE_SECS {
        (format!("{age}s ago"), Style::new().fg(Color::Yellow))
    } else {
        (
            format!("{}m ago", (age + 30) / 60),
            Style::new().fg(Color::Red),
        )
    }
}

/// The sessions table (the reference's SESSIONS block). Column shedding
/// is a
/// width-fallback chain: keep the leftmost columns that fit, give the
/// leftover width to SESSION.
fn render_sessions(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let block = Block::bordered().title_top("SESSIONS");
    if area.height == 0 {
        return;
    }
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
    // The SESSION column's width is the label's budget — the leftover
    // width the table plan hands that column.
    let session_w = match widths.first() {
        Some(ratatui::layout::Constraint::Length(width)) => *width,
        _ => 0,
    };
    let header = Row::new(headers.iter().map(|h| (*h).to_string())).style(Style::new().bold());
    let rows = snap
        .sessions
        .iter()
        .map(|s| Row::new(session_cells(s, snap.now_ms, headers.len(), session_w)));
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
///
/// The `↑` is bright while the conversation is being served upgraded
/// and dim once it has been at some point in the window but the latest
/// turn was not; the `$` marks a session released
/// past the armed quota gate for the window now running. Idle ages dim
/// past [`IDLE_SECS`], like the reference's idle column. `session_w` is
/// the
/// SESSION column's width — the label's budget (see [`session_cell`]).
fn session_cells(
    session: &SessionAgg,
    now_ms: i64,
    count: usize,
    session_w: u16,
) -> Vec<Cell<'static>> {
    let mut cells: Vec<Cell<'static>> = Vec::with_capacity(count);
    let mut push_if = |n: usize, cell: Cell<'static>| {
        if n < count {
            cells.push(cell);
        }
    };
    push_if(0, session_cell(session, session_w));
    push_if(1, ctx_cell(session.ctx));
    push_if(2, model_cell(session));
    push_if(3, Cell::new(session.requests.to_string()));
    push_if(4, Cell::new(unknown_or_grouped(session.input_now)));
    push_if(5, Cell::new(unknown_or_grouped(session.input_peak)));
    push_if(
        6,
        Cell::new(
            session
                .req_messages
                .map(|messages| messages.to_string())
                .unwrap_or_else(|| "-".into()),
        ),
    );
    push_if(
        7,
        Cell::new(
            // The latest generation,
            // and zero/absent renders as a dash (`String(gens || "-")`).
            session
                .compact_generations
                .filter(|generations| *generations > 0)
                .map(|generations| generations.to_string())
                .unwrap_or_else(|| "-".into()),
        ),
    );
    push_if(8, Cell::new(unknown_or_grouped(session.output_total)));
    let idle = now_ms - session.latest_ts_ms >= IDLE_SECS * 1_000;
    let last = rel_age(now_ms - session.latest_ts_ms);
    push_if(
        9,
        if idle {
            Cell::new(last).dim()
        } else {
            Cell::new(last)
        },
    );
    cells
}

/// The session's NAME spans, shared by every panel that names a
/// session — the working directory and the title the frontend gave it
/// (`shortDir(cwd) · title ?? prompt`, the directory cyan and the
/// separator dim) — so the SESSIONS table and the CONTEXT panel show
/// the same string and a row can be correlated across panels. `None`
/// when there is no labelled form (then the caller shows the id): an
/// absent label is the id, never an empty cell, and "unlabelled"
/// stays visibly different from "absent" (invariant 3).
fn session_name_spans(session: &SessionAgg) -> Option<Vec<Span<'static>>> {
    let label = session.label.as_ref()?;
    let dir = short_dir(label.cwd.as_deref());
    let what = label.title.clone().or_else(|| label.prompt.clone());
    if dir.is_none() && what.is_none() {
        return None;
    }
    let mut spans = Vec::with_capacity(3);
    if let Some(dir) = dir {
        spans.push(Span::styled(dir, Style::new().fg(Color::Cyan)));
    }
    if let Some(what) = what {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", Style::new().dim()));
        }
        spans.push(Span::raw(what));
    }
    Some(spans)
}

/// The SESSION cell: the shared name where the column is wide enough
/// for one ([`LABEL_MIN_W`], on the "too narrow a name is noise"
/// rule), else the session id. The cell clips at the column's width,
/// which grows with the terminal exactly like the reference's leftover
/// label column.
fn session_cell(session: &SessionAgg, width: u16) -> Cell<'static> {
    if width >= LABEL_MIN_W
        && let Some(spans) = session_name_spans(session)
    {
        return Cell::new(Line::from(spans));
    }
    Cell::new(session.session.clone())
}

/// The CTX cell: `1M`/`200k`/`872k` for a known ceiling, a dim `?` for
/// none — green and bright for a native exact 1M, yellow otherwise
/// (the reference's context colouring).
fn ctx_cell(ctx: ContextWindow) -> Cell<'static> {
    match ctx {
        ContextWindow::Unknown => Cell::new("?").dim(),
        ContextWindow::Exact { tokens } if tokens >= 1_000_000 => {
            Cell::new(short_tokens(tokens)).fg(Color::Green).bold()
        }
        ContextWindow::Exact { tokens } | ContextWindow::Declared { tokens } => {
            Cell::new(short_tokens(tokens)).fg(Color::Yellow)
        }
    }
}

/// The MODEL cell with its `↑`/`$` markers, coloured per marker.
fn model_cell(session: &SessionAgg) -> Cell<'static> {
    let model = session.model.clone().unwrap_or_else(|| "?".into());
    let mut line = vec![Span::raw(model)];
    if session.forced_any {
        let style = if session.forced_latest {
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Green).dim()
        };
        line.push(Span::styled(" ↑", style));
    }
    if session.released {
        line.push(Span::styled(" $", Style::new().fg(Color::Yellow)));
    }
    Cell::new(Line::from(line))
}

/// Unknown token counts render as `?`, never as a fake zero; known ones
/// comma-grouped.
fn unknown_or_grouped(value: Option<i64>) -> String {
    value
        .map(|n| super::rebuilds::grouped(Some(n)))
        .unwrap_or_else(|| "?".into())
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

/// CONTEXT: per-session occupancy against the known ceilings
/// (the reference's context block) — a bar and a percentage only
/// where both the prompt and the ceiling are known; an unknown prompt
/// or an unknown ceiling claims nothing (`? / 1M`, `136,260 / ?`),
/// and an idle session is dimmed, because a session that is not about
/// to do anything is not about to compact either. Past 80% a live
/// session's bar turns red with the marker that says why.
fn render_context(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let block = Block::bordered().title_top("CONTEXT");
    if area.height == 0 {
        return;
    }
    if snap.window_empty {
        frame.render_widget(Paragraph::new("no data in window").dim().block(block), area);
        return;
    }
    let width = block.inner(area).width as usize;

    let mut lines = Vec::new();
    let mut listed = 0;
    for session in &snap.sessions {
        if session.lane_requests < CONTEXT_MIN_REQUESTS {
            continue;
        }
        listed += 1;
        let idle = snap.now_ms - session.latest_ts_ms >= IDLE_SECS * 1_000;
        let idle_note = idle.then(|| {
            Line::from(Span::styled(
                format!("  idle {}", rel_age(snap.now_ms - session.latest_ts_ms)),
                Style::new().dim(),
            ))
        });

        let ctx = match session.ctx {
            ContextWindow::Unknown => None,
            known => Some((
                short_tokens(known.tokens().unwrap_or(0)),
                known.tokens().unwrap_or(1),
            )),
        };
        // The SAME name the sessions table shows (label or id) — the
        // panels correlate row-for-row — in a CAPPED field: a long
        // title clips (with an ellipsis) rather than eating the bar,
        // a short one pads, and both end with the fixed gap so the
        // name never touches the bar.
        let name_spans = context_name_spans(session);

        let Some(prompt) = session.input_now else {
            // Unknown prompt: no bar, no share — an explicit `?`.
            let mut spans = name_spans;
            spans.push(Span::raw(format!("{:>13} / ", "?")));
            match ctx {
                Some((label, _)) => spans.push(ctx_span(&label, session.ctx)),
                None => spans.push(Span::styled("?".to_owned(), Style::new().dim())),
            }
            if let Some(note) = idle_note {
                spans.extend(note.spans);
            }
            lines.push(Line::from(spans));
            continue;
        };
        let Some((ctx_label, ceiling)) = ctx else {
            // Known prompt, unknown ceiling: the number is real, the
            // share is not claimable.
            let mut spans = name_spans;
            spans.push(Span::raw(format!("{:>13} / ", grouped(Some(prompt)))));
            spans.push(Span::styled("?".to_owned(), Style::new().dim()));
            if let Some(note) = idle_note {
                spans.extend(note.spans);
            }
            lines.push(Line::from(spans));
            continue;
        };

        // Both known: the occupancy claim.
        let frac = (prompt as f64 / ceiling as f64).clamp(0.0, 1.0);
        let near = frac > 0.8 && !idle;
        // The bar budgets for the line's fixed text AND the trailing
        // note — the "← compacts soon" marker or the idle label —
        // against the capped name field: the historical WIDTH − 66
        // rule with the 30-wide field it now is.
        let bar_width = width.saturating_sub(66).max(10) as u16;
        let bar_style = if idle {
            Style::new().dim()
        } else if near {
            Style::new().fg(Color::Red)
        } else {
            Style::new().fg(Color::Cyan)
        };
        let mut spans = name_spans;
        spans.push(Span::styled(bar(frac, bar_width), bar_style));
        spans.push(Span::raw(format!(
            " {:>13} / {:<4} {:>3}%",
            grouped(Some(prompt)),
            ctx_label,
            (frac * 100.0).round() as i64
        )));
        if near {
            spans.push(Span::styled(
                "  ← compacts soon".to_owned(),
                Style::new().fg(Color::Red),
            ));
        } else if let Some(note) = idle_note {
            spans.extend(note.spans);
        }
        lines.push(Line::from(spans));
    }
    if listed == 0 {
        lines.push(Line::from(Span::styled(
            "no session with enough history yet".to_owned(),
            Style::new().dim(),
        )));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// The CONTEXT panel's name line: the same name the sessions table
/// shows (label or id), clipped to [`CONTEXT_NAME_W`] with an
/// ellipsis when the title is the part that had to go, padded to the
/// cap when short, and ended with the fixed gap so the name never
/// touches the bar.
fn context_name_spans(session: &SessionAgg) -> Vec<Span<'static>> {
    // Flush-left, matching the sessions table: a row's name starts at
    // the same column in both panels — correlation is alignment, not
    // just a shared string.
    let mut spans = Vec::with_capacity(4);
    let shown =
        session_name_spans(session).unwrap_or_else(|| vec![Span::raw(session.session.clone())]);
    let name_w: usize = shown.iter().map(|span| span.width()).sum();
    if name_w <= CONTEXT_NAME_W {
        spans.extend(shown);
        spans.push(Span::raw(
            " ".repeat(CONTEXT_NAME_W - name_w + CONTEXT_NAME_GAP),
        ));
        return spans;
    }
    // Over the cap: clip the LAST span (the title or prompt — the
    // directory is the correlation anchor, it stays whole).
    let keep = CONTEXT_NAME_W.saturating_sub(1); // room for the ellipsis
    let mut clipped = Vec::with_capacity(shown.len());
    let mut used = 0;
    let last = shown.len().saturating_sub(1);
    for (index, span) in shown.into_iter().enumerate() {
        if index == last {
            let room = keep.saturating_sub(used);
            let text = span.content.clone();
            let cut = text
                .char_indices()
                .take_while(|(byte, _)| *byte <= room.saturating_sub(1).min(text.len()))
                .last()
                .map(|(byte, _)| byte)
                .unwrap_or(0);
            let text = format!("{}…", &text[..cut]);
            clipped.push(Span::styled(text, span.style));
        } else {
            used += span.width();
            clipped.push(span);
        }
    }
    spans.extend(clipped);
    spans.push(Span::raw(" ".repeat(CONTEXT_NAME_GAP)));
    spans
}

/// The ceiling label's span, coloured like the sessions table's CTX
/// cell.
fn ctx_span(label: &str, ctx: ContextWindow) -> Span<'static> {
    let style = match ctx {
        ContextWindow::Unknown => Style::new().dim(),
        ContextWindow::Exact { tokens } if tokens >= 1_000_000 => {
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
        }
        _ => Style::new().fg(Color::Yellow),
    };
    Span::styled(label.to_owned(), style)
}

/// TOKENS: where the window's input went (the reference's TOKENS
/// block). Shares are computed only when no input bucket is missing
/// rows; a bucket with unavailable rows renders as a floor (`≥`) with
/// the reason, and the hit rate is over the REUSABLE prefix — hits
/// plus rewrites, never all input (fresh input was never going to be a
/// hit). Absence renders as absence throughout: `?` rates, `≥` floors,
/// "N req unknown" counts — never confident zeros.
fn render_tokens(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let block = Block::bordered().title_top("TOKENS");
    if area.height == 0 {
        return;
    }
    if snap.window_empty {
        frame.render_widget(Paragraph::new("no data in window").dim().block(block), area);
        return;
    }
    let width = block.inner(area).width as usize;
    let tokens = &snap.tokens;

    let mut lines = Vec::new();
    // Shares render per bucket from the KNOWN sums: a bucket with
    // unknown rows is a FLOOR (the `≥` marker) and keeps its share —
    // missing data on one row never blanks the others. The share's
    // denominator is the known input total (max 1, so an all-zero
    // window renders 0%, not a crash).
    let total = tokens.input_total().max(1);
    // The bar/percentage pair is the row's shape; shape sheds before
    // information, and the fixed-width counts always fit.
    let show_shares = width >= TOKENS_SHARE_MIN_W;
    let label = |text: &str| format!("{text:<TOKENS_LABEL_W$}");
    let amount = |bucket: &super::model::BucketAgg| {
        format!(
            "{:>TOKENS_AMOUNT_W$}",
            format!(
                "{}{}",
                if bucket.unavailable > 0 { "≥" } else { "" },
                grouped(Some(bucket.value))
            )
        )
    };
    for (name, bucket) in [
        ("fresh input", &tokens.fresh_input),
        ("cache read", &tokens.cache_read),
        ("cache write 1h", &tokens.write_1h),
        ("cache write 5m", &tokens.write_5m),
    ] {
        let note = (bucket.unavailable > 0).then(|| format!("{} req unknown", bucket.unavailable));
        let mut row = vec![Span::raw(label(name)), Span::raw(amount(bucket))];
        if show_shares {
            let frac = bucket.value as f64 / total as f64;
            // A floor's note rides the same row as the bar, so the bar
            // yields its width to it — otherwise the border clips the
            // note mid-word and the count the row exists to show is the
            // part that vanishes.
            let bar_w = tokens_bar_width(width, note.as_ref().map_or(0, String::len));
            row.push(Span::raw("  "));
            row.push(Span::styled(
                fill_bar(frac, bar_w, "▬", " "),
                Style::new().dim(),
            ));
            row.push(Span::raw(format!(" {:>3}%", (frac * 100.0).round() as i64)));
        }
        if let Some(note) = note {
            row.push(Span::raw("  "));
            row.push(Span::styled(note, Style::new().dim()));
        }
        lines.push(Line::from(row));
    }

    // Output: no shared denominator with the input buckets, so no bar
    // — just the total and the per-request average.
    let output_note = if tokens.output.unavailable > 0 {
        format!("{} req unknown", tokens.output.unavailable)
    } else {
        format!(
            "{}/req",
            if tokens.requests > 0 {
                (tokens.output.value as f64 / tokens.requests as f64).round() as i64
            } else {
                0
            }
        )
    };
    lines.push(Line::from(vec![
        Span::raw(label("output")),
        Span::raw(amount(&tokens.output)),
        Span::raw("  "),
        Span::styled(output_note, Style::new().dim()),
    ]));

    // Reasoning: rendered only when a provider reported thinking
    // tokens — an output-side bucket, so the note
    // carries the per-request average like the output row's. A window
    // where EVERY row lacks reasoning has nothing to say:
    // nothing renders, and rightly (absence across the board is
    // the provider's silence, not unknown data).
    let reasoning_partial =
        tokens.reasoning.unavailable > 0 && tokens.reasoning.unavailable < tokens.requests;
    if tokens.reasoning.value > 0 || reasoning_partial {
        let reasoning_note = if tokens.reasoning.unavailable > 0 {
            format!("{} req unknown", tokens.reasoning.unavailable)
        } else if tokens.requests > 0 {
            format!(
                "{}/req",
                (tokens.reasoning.value as f64 / tokens.requests as f64).round() as i64
            )
        } else {
            "0/req".to_owned()
        };
        lines.push(Line::from(vec![
            Span::raw(label("reasoning")),
            Span::raw(amount(&tokens.reasoning)),
            Span::raw("  "),
            Span::styled(reasoning_note, Style::new().dim()),
        ]));
    }

    // Hit rate over the reusable prefix, and the missed line beside it.
    match tokens.hit_rate() {
        HitRate::Unknown => {
            lines.push(Line::from(vec![
                Span::raw(label("hit rate")),
                Span::raw(format!("{:>TOKENS_AMOUNT_W$}", "?")),
                Span::raw("  "),
                Span::styled(
                    format!("cache metrics unavailable for {} req", tokens.cache_unknown),
                    Style::new().dim(),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw(label("missed")),
                Span::raw(format!(
                    "{:>TOKENS_AMOUNT_W$}",
                    format!("≥{}", grouped(Some(tokens.written())))
                )),
                Span::raw("  "),
                Span::styled("known cache writes only".to_owned(), Style::new().dim()),
            ]));
        }
        HitRate::Rate(hit) => {
            let style = if hit > 0.95 {
                Style::new().fg(Color::Green)
            } else if hit > 0.8 {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new().fg(Color::Red)
            };
            let note = if width >= 72 {
                " of reusable prefix"
            } else {
                ""
            };
            let mut row = vec![
                Span::raw(label("hit rate")),
                Span::raw(format!(
                    "{:>TOKENS_AMOUNT_W$}",
                    format!("{:.1}%", hit * 100.0)
                )),
            ];
            // The bar sheds below its least width; the rate rides in
            // the amount column and survives. The note annotates the
            // bar, so it sheds with it — and only ever renders at
            // widths where the bar does.
            if width >= TOKENS_RATE_BAR_MIN_W {
                row.push(Span::raw("  "));
                row.push(Span::styled(
                    bar(hit, tokens_rate_bar_width(width, note.len()) as u16),
                    style,
                ));
                row.push(Span::styled(note.to_owned(), Style::new().dim()));
            }
            // A rate over known sums with unknown rows riding: the
            // caveat travels with the rate instead of blanking it.
            if tokens.cache_unknown > 0 {
                row.push(Span::styled(
                    format!("  · {} req unknown", tokens.cache_unknown),
                    Style::new().dim(),
                ));
            }
            lines.push(Line::from(row));
            let per_req = if tokens.requests > 0 {
                (tokens.written() as f64 / tokens.requests as f64).round() as i64
            } else {
                0
            };
            lines.push(Line::from(vec![
                Span::raw(label("missed")),
                Span::raw(format!(
                    "{:>TOKENS_AMOUNT_W$}",
                    grouped(Some(tokens.written()))
                )),
                Span::raw("  "),
                Span::styled(
                    format!(
                        "rewritten · {}/req · {} of {} req reused nothing",
                        grouped(Some(per_req)),
                        tokens.cold,
                        tokens.requests
                    ),
                    Style::new().dim(),
                ),
            ]));
        }
        // Nothing was read or rewritten: no rate is claimable either
        // way, and no line renders at all.
        HitRate::NothingReusable => {}
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// CACHE REBUILDS: the panel to watch (the reference's rebuild
/// block) — rewrites over the threshold, by cause, and the newest
/// localised system-prompt changes. The panel never vanishes: no
/// rebuilds is a verdict ("none — every prefix held"), not an absence
/// of data, and unknown rewrites are counted, never guessed.
fn render_rebuilds(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let block = Block::bordered().title_top("CACHE REBUILDS");
    if area.height == 0 {
        return;
    }
    let Some(rebuilds) = snap.rebuilds.as_ref() else {
        // The loop computes the section before the first draw; this is
        // the pre-refresh placeholder's shape, and it says so.
        frame.render_widget(
            Paragraph::new("no rebuild data yet").dim().block(block),
            area,
        );
        return;
    };
    if snap.window_empty {
        frame.render_widget(Paragraph::new("no data in window").dim().block(block), area);
        return;
    }

    let mut lines = Vec::new();
    let threshold = grouped(Some(REBUILD_MIN));
    // The denominator is the walk's own window count (`measured` +
    // `unmeasured`), not the display aggregation's: the rebuild read
    // is capped separately, and a fraction must be honest about its
    // own denominator (the reference's own walked-row count, the same
    // rule).
    let walked = rebuilds.measured + rebuilds.unmeasured;
    if rebuilds.unmeasured > 0 {
        lines.push(Line::from(format!(
            "{} of {} measured requests rewrote ≥{} tokens · {} unknown",
            rebuilds.rebuilds, rebuilds.measured, threshold, rebuilds.unmeasured
        )));
    } else {
        lines.push(Line::from(format!(
            "{} of {} requests rewrote ≥{} tokens",
            rebuilds.rebuilds, walked, threshold
        )));
    }
    if rebuilds.causes.is_empty() {
        let verdict = if rebuilds.unmeasured > 0 {
            "none among measured requests"
        } else {
            "none — every prefix held"
        };
        lines.push(Line::from(Span::styled(
            verdict.to_owned(),
            Style::new().dim(),
        )));
    }
    for (cause, count) in &rebuilds.causes {
        lines.push(Line::from(vec![
            Span::raw(format!("{:<24}{:>4}  ", cause.label(), count)),
            Span::styled("▬".repeat((*count).min(30)), Style::new().dim()),
        ]));
    }
    // The newest localised system-prompt changes (the per-rebuild
    // line, the part that makes a change diagnosable).
    for event in rebuilds
        .events
        .iter()
        .filter(|event| event.cause == super::rebuilds::Cause::SystemPrompt)
        .take(REBUILD_DETAIL_LINES)
    {
        // The session's NAME, shared with the other panels (label or
        // id) — correlation everywhere a session is named, the same
        // builder. Clipped harder here: the detail line is the point.
        let name = snap
            .sessions
            .iter()
            .find(|session| session.session == event.session)
            .map(|session| {
                session_name_spans(session)
                    .unwrap_or_else(|| vec![Span::raw(session.session.clone())])
            })
            .unwrap_or_else(|| vec![Span::raw(event.session.clone())]);
        let name_text = name
            .iter()
            .map(|span| span.content.clone())
            .collect::<Vec<_>>()
            .join("");
        let detail = event.detail.as_deref().unwrap_or("");
        lines.push(Line::from(Span::styled(
            format!("· {name_text} — system prompt changed ({detail})"),
            Style::new().dim(),
        )));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Short tokens: round thousands and millions
/// abbreviate; anything else renders comma-grouped.
fn short_tokens(tokens: u64) -> String {
    // Human-rounded, deliberately not exact: the CONTEXT panel carries
    // the precise figure (occupancy bar + exact counts), so the table's
    // cell answers "which league is this window in". ≥1M rounds to the
    // nearest 0.1M (1,048,576 → `1M`, 1,050,000 → `1.1M`); ≥1k to the
    // nearest 1k (262,144 → `262k`); below that, grouped as-is.
    if tokens >= 1_000_000 {
        let tenths = ((tokens as f64 / 100_000.0).round()) as u64;
        let whole = tenths / 10;
        let frac = tenths % 10;
        if frac == 0 {
            format!("{whole}M")
        } else {
            format!("{whole}.{frac}M")
        }
    } else if tokens >= 1_000 {
        format!("{}k", ((tokens as f64 / 1_000.0).round()) as u64)
    } else {
        grouped(Some(tokens as i64))
    }
}

/// Comma-grouped counts (shared with the
/// rebuild panel's detail lines).
fn grouped(value: Option<i64>) -> String {
    super::rebuilds::grouped(value)
}

/// The TOKENS panel's bucket bars (`▬`
/// fill and space pad — a share bar, visually distinct from the
/// occupancy and meter bars).
fn fill_bar(frac: f64, width: usize, fill: &str, pad: &str) -> String {
    let frac = frac.clamp(0.0, 1.0);
    let filled = (frac * width as f64).round() as usize;
    let filled = filled.min(width);
    fill.repeat(filled) + &pad.repeat(width.saturating_sub(filled))
}

/// SPEND: billed total, per-provider·model breakdown, and the never-dropped
/// "no cost data" count (invariant 3: a request whose cost the provider
/// never reported is visible, not folded into the total). Rendered only
/// when [`SpendAgg::carries_cost`](super::model::SpendAgg::carries_cost),
/// so never over an empty window.
fn render_spend(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let block = Block::bordered().title_top("SPEND");
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

/// RATE & QUOTA: requests per minute, the per-minute sparkline (minutes
/// with errors flagged in red), the error/drift counters — and, when the
/// snapshot carries a quota section, the plan's own meters with their
/// reset clocks and forecasts, the `spent` line, and the `binding`
/// claim (the reference's RATE & QUOTA block).
fn render_rate(frame: &mut Frame, area: Rect, snap: &Snapshot, tz: &TimeZone) {
    let block = Block::bordered().title_top("RATE & QUOTA");
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

    // The quota lines: absent when the section is absent — an openai
    // window has no quota meters and no quota lines (the per-backend
    // panel rule).
    if let Some(quota) = &snap.quota {
        for meter in &quota.meters {
            lines.push(meter_line(
                meter,
                quota.gate_assumed,
                inner.width,
                snap.now_ms,
                tz,
            ));
        }
        if let (Some(today), Some(window)) = (quota.spent_today, quota.spent_window) {
            lines.push(spent_line(today, window, snap.window_mins));
        }
        if let Some(binding) = &quota.binding {
            lines.push(binding_line(binding, quota.overage_in_use));
        }
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// One meter line (the reference dashboard's exact line shape): the
/// label, a utilisation bar coloured by how much is left, and — never
/// wrapped — the status, the resets clock, and the forecast verdict.
/// As the panel narrows the bar shrinks to [`BAR_MIN_WIDTH`] first,
/// then the status goes, then the resets clock; the verdict is the
/// last thing to go.
fn meter_line(
    meter: &MeterPanel,
    gate_assumed: bool,
    width: u16,
    now_ms: i64,
    tz: &TimeZone,
) -> Line<'static> {
    let width = width as usize;

    // The verdict, with its clock: a runout lands at `at`, labelled so
    // it cannot be misread as belonging to the reset's day. The gate
    // is recoverable — the release marker spends past it — where real
    // exhaustion is not: different walls, different colours (`stops`
    // yellow, `out` red).
    let (verdict_text, verdict_style) = match meter.verdict {
        Verdict::Runout { at_ms } => {
            let other = meter
                .reset_s
                .map(|reset| reset as f64 * 1000.0)
                .unwrap_or(at_ms);
            let label = alongside(at_ms, other, now_ms, tz);
            if meter.target < 1.0 {
                (format!("stops ~{label}"), Style::new().fg(Color::Yellow))
            } else {
                (format!("out ~{label}"), Style::new().fg(Color::Red))
            }
        }
        Verdict::OnTrack => ("on track".to_owned(), Style::new().fg(Color::Green)),
        Verdict::Reached => ("spent".to_owned(), Style::new().fg(Color::Red)),
        // A rolled window and an unmeasurable one both say so in words
        // — never a silent guess (invariant 3).
        Verdict::Stale => ("window rolled over".to_owned(), Style::new().dim()),
        Verdict::Unknown => ("estimating".to_owned(), Style::new().dim()),
    };
    let gated_prefix = if meter.exhausted { "gated · " } else { "" };
    let assumed_suffix = if meter.gated && gate_assumed { "?" } else { "" };
    let verdict_full = format!("{gated_prefix}{verdict_text}{assumed_suffix}");

    // The resets clause goes with a live verdict only — "resets ?"
    // beside "window rolled over" would assert a present tense the
    // reading no longer has. An absent reset is named, never guessed.
    let resets = match meter.verdict {
        Verdict::Stale => None,
        _ => Some(match meter.reset_s {
            Some(reset) => format!(
                "resets {} · ",
                reset_label(reset as f64 * 1000.0, now_ms, tz)
            ),
            None => "resets ? · ".to_owned(),
        }),
    };
    let status = meter
        .status
        .as_deref()
        .filter(|status| *status != "allowed")
        .map(str::to_owned);

    // Compose the right side, shedding as the panel narrows: the bar
    // shrinks to its floor first, then the status goes, then the
    // resets clause, the verdict last. The reset clock is half of what
    // a meter answers, so it outlives the bar's last dozen cells.
    // Where the reference cuts the bar string mid-glyph at
    // this point, the bar here shrinks instead — every surviving piece
    // keeps its styling and a partial bar still reads as a bar.
    let len = |s: &str| s.chars().count();
    let with = |status: Option<&str>, resets: Option<&str>| {
        let mut right = String::new();
        if let Some(status) = status {
            right.push_str(status);
            right.push_str("  ");
        }
        if let Some(resets) = resets {
            right.push_str(resets);
        }
        right.push_str(&verdict_full);
        right
    };
    let pct = format!("{:>3}%", (meter.util * 100.0).round() as i64);
    let label = format!("  {:<9}", meter.label);
    // Everything on the line but the bar and the right side: the label
    // column, the space before the percentage, the percentage, and the
    // one space that always separates it from the right side — the gap
    // once dropped that space and printed "11%on track".
    let fixed = len(&label) + 1 + len(&pct) + 1;
    let bar_room = |right: &str| width.saturating_sub(fixed + len(right));
    let (use_status, use_resets) = [
        (status.as_deref(), resets.as_deref()),
        (None, resets.as_deref()),
    ]
    .into_iter()
    .find(|&(status, resets)| bar_room(&with(status, resets)) >= BAR_MIN_WIDTH)
    .unwrap_or((None, None));
    let right = with(use_status, use_resets);
    let bar_width = bar_room(&right).min(BAR_WIDTH as usize) as u16;

    // A reading from a window that has rolled is dimmed along with its
    // bar: the verdict says so in words, but a bright 95% next to it
    // is the thing the eye actually reads.
    let bar_style = match meter.verdict {
        Verdict::Stale => Style::new().dim(),
        _ if meter.util > 0.95 => Style::new().fg(Color::Red),
        _ if meter.util > 0.8 => Style::new().fg(Color::Yellow),
        _ => Style::new().fg(Color::Green),
    };
    let left = format!("{label}{} {pct}", bar(meter.util, bar_width));

    // The styled left side: the label plain, the bar and its
    // percentage in the bar's colour.
    let left_spans = || -> Vec<Span<'static>> {
        vec![
            Span::raw(label.clone()),
            Span::styled(bar(meter.util, bar_width), bar_style),
            Span::raw(" "),
            Span::styled(pct.clone(), bar_style),
        ]
    };

    // The styled right side, kept whole: the verdict is the last thing
    // to go, so it is never the thing that gets clipped.
    let right_spans = || -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        if let Some(status) = use_status {
            spans.push(Span::styled(status.to_owned(), Style::new().fg(Color::Red)));
            spans.push(Span::raw("  "));
        }
        if let Some(resets) = use_resets {
            spans.push(Span::styled(resets.to_owned(), Style::new().dim()));
        }
        if meter.exhausted {
            spans.push(Span::styled(gated_prefix, Style::new().fg(Color::Yellow)));
        }
        spans.push(Span::styled(verdict_text.clone(), verdict_style));
        if !assumed_suffix.is_empty() {
            spans.push(Span::styled(assumed_suffix, Style::new().dim()));
        }
        spans
    };

    if fixed + bar_width as usize + len(&right) <= width {
        // At least the one separating space, by construction of `fixed`.
        let gap = width - (fixed - 1) - bar_width as usize - len(&right);
        let mut spans = left_spans();
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right_spans());
        Line::from(spans)
    } else {
        // Degenerate width: cut the left so the verdict survives —
        // the reference's spread clamps the left side of the line, at
        // the price of the cut portion's styling.
        let keep = width.saturating_sub(len(&right) + 1);
        let mut spans = vec![
            Span::raw(left.chars().take(keep).collect::<String>()),
            Span::raw(" "),
        ];
        spans.extend(right_spans());
        Line::from(spans)
    }
}

/// The `spent` line: how much overage today and
/// this window actually cost — the thing the utilisation bar cannot
/// say, because a meter sitting at 64% got there at some point in the
/// past, not necessarily this span. "Today" is the user's local day.
fn spent_line(today: Spent, window: Spent, window_mins: u64) -> Line<'static> {
    // The five display states (the `spent` display table): a total,
    // a quantisation ceiling, a floor, idle, and no data — a busy span
    // that did not move the 1%-quantised figure must not print as a
    // measured "+0%".
    let show = |spent: Spent| -> (String, bool) {
        match spent {
            Spent::NoData => ("no data".to_owned(), true),
            Spent::Idle => ("idle".to_owned(), true),
            Spent::Measured { points, floor } => {
                let pct = (points * 100.0).round();
                if pct < 1.0 {
                    return ("<1%".to_owned(), true);
                }
                (
                    if floor {
                        format!("≥+{pct}%")
                    } else {
                        format!("+{pct}%")
                    },
                    false,
                )
            }
        }
    };
    let (today_text, today_dim) = show(today);
    let (window_text, window_dim) = show(window);
    let dim_of = |dim: bool| {
        if dim {
            Style::new().dim()
        } else {
            Style::new()
        }
    };
    Line::from(vec![
        Span::raw("  spent   "),
        Span::styled("today ", Style::new().dim()),
        Span::styled(today_text, dim_of(today_dim)),
        Span::styled("  ·  ", Style::new().dim()),
        Span::styled(format!("{window_mins}m "), Style::new().dim()),
        Span::styled(window_text, dim_of(window_dim)),
    ])
}

/// The `binding` line: the representative-claim
/// naming which limit is in force, with the overage flag beside it —
/// spend has shifted off plan quota, which is not a footnote.
fn binding_line(claim: &str, overage_in_use: bool) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("binding", Style::new().dim()),
        Span::raw(format!("  {claim}")),
    ];
    if overage_in_use {
        spans.push(Span::styled(
            "   overage IN USE",
            Style::new().fg(Color::Red),
        ));
    }
    Line::from(spans)
}

/// The meter bar: fill to `round(frac × w)`, pad with `░`.
fn bar(frac: f64, w: u16) -> String {
    let w = w as usize;
    let f = frac.clamp(0.0, 1.0);
    let fill = (f * w as f64).round() as usize;
    let fill = fill.min(w);
    "█".repeat(fill) + &"░".repeat(w.saturating_sub(fill))
}

/// Dollar formatting for the spend panel.
fn usd(value: f64) -> String {
    format!("${value:.6}")
}

#[cfg(test)]
mod short_tokens_tests {
    use super::short_tokens;

    #[test]
    fn large_windows_round_to_human_figures() {
        // The CONTEXT panel carries the exact occupancy; the table's
        // cell answers "which league". ≥1M rounds to the nearest 0.1M
        // (trailing `.0` dropped); ≥1k to the nearest k.
        for (tokens, shown) in [
            (1_000_000u64, "1M"),
            (1_048_576, "1M"),   // openrouter's binary megabyte
            (1_050_000, "1.1M"), // gpt-6-luna's declared window
            (1_048_576 + 40_000, "1.1M"),
            (872_000, "872k"),
            (262_144, "262k"),
            (200_000, "200k"),
            (104_857, "105k"),
            (102_400, "102k"),
            (999, "999"),
            (500, "500"),
        ] {
            assert_eq!(short_tokens(tokens), shown, "{tokens}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::model;
    use super::super::quota::Spent;
    use super::super::testrows::{
        as_display_rows, as_meter_rows, display_bare, display_billed, display_kind_row,
        metered_full,
    };
    use crate::catalog::fetched::FetchedCatalogs;
    use crate::store::RowKind;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use serde_json::json;
    use std::collections::{HashMap, HashSet};

    use super::super::labels::Label;

    const NOW: i64 = 1_769_000_000_000;
    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    /// The test frame's local-day start: 2026-01-21T12:53:20Z minus
    /// twelve hours, arbitrary but stable.
    const TODAY: i64 = NOW - 12 * HOUR;

    /// The tests' shared "no transcript labels" input — the absent
    /// state, never a fabricated one.
    fn no_labels() -> HashMap<String, Label> {
        HashMap::new()
    }

    /// A fixed zone keeps the pinned clock strings independent of the
    /// machine running the tests; UTC keeps them readable.
    fn utc() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get("UTC").expect("UTC is always present in the tzdb")
    }

    /// The shared synthetic window: two sessions plus a NULL-session group,
    /// a billed spread for the sparkline, a NULL-cost request, and one
    /// error row. Display rows in the narrow shape the tick reads; they
    /// carry no meter snapshots, so the loop's quota section over this
    /// window is None — the openai-route shape the no-quota-panel tests
    /// pin (absence, never zeros).
    fn snapshot() -> model::Snapshot {
        let mut rows = Vec::new();
        // Counts 1..=8 in the eight minutes ending 1 minute ago → the
        // sparkline shows a contiguous ▁▂▃▄▅▆▇█ ramp (the billed request
        // below joins the count-8 minute, the unpriced one the newest).
        for count in 1..=8 {
            for _ in 0..count {
                let mut row = display_bare(NOW - (9 - count as i64) * 60_000);
                row.session_id = Some("ses-abc".into());
                row.model = Some("z-ai/glm-5.3".into());
                row.provider = Some("openrouter".into());
                rows.push(row);
            }
        }
        // A billed request and an unpriced one from the dash session.
        rows.push(display_billed(
            NOW - 90_000,
            Some("ses-abc"),
            "z-ai/glm-5.3",
            "openrouter",
            12_345,
            100_000,
            678,
            0.00213,
        ));
        let mut unpriced = display_bare(NOW - 45_000);
        unpriced.session_id = None;
        unpriced.model = Some("z-ai/glm-5.3".into());
        unpriced.provider = Some("openrouter".into());
        unpriced.input = Some(500);
        rows.push(unpriced);
        rows.push(display_kind_row(NOW - 10_000, RowKind::Error));
        model::aggregate(
            &rows,
            None,
            &HashSet::new(),
            &no_labels(),
            &FetchedCatalogs::default(),
            None,
            30,
            NOW,
            523,
        )
    }

    /// The shared synthetic quota rows: real-shaped anthropic meter
    /// snapshots across three windows —
    ///
    /// - 5-hour: 0.10 → 0.30 → 0.32 over ninety minutes, resetting
    ///   three hours out (a measured burn that resets first → `on
    ///   track`), and `overageInUse` on the newest reading, so the
    ///   gate counts it spent → the `gated ·` prefix and a countdown
    ///   to exhaustion rather than to the gate;
    /// - 7-day: 0.10 → 0.80 over two days (the day-scale-span rule
    ///   demands at least a day), resetting four days out → a runout
    ///   before the reset, against the armed gate → `stops ~…` with
    ///   the alongside weekday;
    /// - overage: 0.58 → 0.64 across midnight and flat inside the
    ///   window → `estimating` (no span out-measures the quantisation
    ///   across the day-scale minimum), `spent today +6%` measured
    ///   from the pre-midnight baseline, `30m <1%` flat, a `rejected`
    ///   status, and the `binding` claim with overage in use.
    ///
    /// `gate_on` false builds the same readings from rows that predate
    /// the `gate_on` field — the assumed-gate fixture.
    fn quota_rows(gate_on: bool) -> Vec<crate::store::RequestRow> {
        let reset5h = (NOW + 3 * HOUR) / 1000;
        let reset7d = (NOW + 4 * DAY) / 1000;
        let reset_overage = (NOW + 20 * DAY) / 1000;
        let mut rows = vec![
            metered_full(NOW - 2 * DAY, json!({"util7d": 0.10, "reset7d": reset7d})),
            metered_full(NOW - DAY, json!({"util7d": 0.80, "reset7d": reset7d})),
            metered_full(
                NOW - 13 * HOUR,
                json!({"utilOverage": 0.58, "resetOverage": reset_overage}),
            ),
            metered_full(NOW - 90 * MIN, json!({"util5h": 0.10, "reset5h": reset5h})),
            metered_full(
                NOW - 45 * MIN,
                json!({
                    "util5h": 0.30, "reset5h": reset5h,
                    "utilOverage": 0.64, "resetOverage": reset_overage,
                }),
            ),
            metered_full(
                NOW - 10 * MIN,
                json!({
                    "util5h": 0.32, "reset5h": reset5h,
                    "util7d": 0.80, "reset7d": reset7d,
                    "utilOverage": 0.64, "resetOverage": reset_overage,
                    "status5h": "allowed", "status7d": "allowed",
                    "statusOverage": "rejected",
                    "claim": "five_hour", "overageInUse": true,
                }),
            ),
        ];
        if gate_on {
            rows.last_mut().expect("the newest row").gate_on = Some(true);
        }
        rows
    }

    fn quota_snapshot() -> model::Snapshot {
        let rows = quota_rows(true);
        {
            // The view tests build the quota section once, the way the
            // loop does (over the meter lookback's narrow shape), and
            // reuse it across sizes; the display aggregate runs over
            // the same rows' display projection.
            let quota = crate::tui::quota::aggregate(
                &as_meter_rows(&rows),
                NOW,
                TODAY,
                NOW.saturating_sub(30 * 60_000),
            );
            model::aggregate(
                &as_display_rows(&rows),
                quota.as_ref(),
                &HashSet::new(),
                &no_labels(),
                &FetchedCatalogs::default(),
                None,
                30,
                NOW,
                523,
            )
        }
    }

    /// The phase-5 panels' shared synthetic window: the anthropic-sub
    /// shape the new panels exist for — cache metrics on every row,
    /// prompts that carry their write share, known ceilings, a
    /// released session, a rewrite, an idle session — built the way
    /// the loop builds a frame (aggregate + precomputed rebuild
    /// section + released set).
    ///
    /// - `ses-hot`: claude-opus-5, native 1M, three turns a minute
    ///   apart, prompt 467,893 → the 47% occupancy bar; a system
    ///   rewrite on the latest turn (dim `↑`… bright, actually — the
    ///   latest row carries it).
    /// - `ses-crowded`: claude-haiku-4-5 (200k), idle ten minutes, its
    ///   prompt at 85% → the dimmed bar and the idle note.
    /// - `ses-mystery`: gpt-5.6-terra — outside the catalogue, ceiling
    ///   `?`, and its latest row reports no cache_read → the prompt is
    ///   unknown too: the `? / ?` context line.
    /// - `ses-free`: released past the gate → the `$`.
    fn full_snapshot() -> model::Snapshot {
        full_snapshot_with_labels(&no_labels())
    }

    /// full_snapshot's shape with the given labels (and released set)
    /// consumed by the aggregate.
    fn full_snapshot_with_labels(labels: &HashMap<String, Label>) -> model::Snapshot {
        let (rows, rebuilds, released) = full_snapshot_parts();
        model::aggregate(
            &rows,
            None,
            &released,
            labels,
            &FetchedCatalogs::default(),
            Some(rebuilds),
            30,
            NOW,
            523,
        )
    }

    /// The rows, the precomputed rebuild section, and the released
    /// set — full_snapshot's inputs, so label variants share them.
    fn full_snapshot_parts() -> (
        Vec<crate::store::DisplayRow>,
        crate::tui::rebuilds::RebuildAgg,
        HashSet<String>,
    ) {
        let mut rows = Vec::new();
        // ses-hot: three turns, cache metrics on every one — the last
        // pushed is the newest, so the latest turn is the lean one
        // (1 500 fresh + 449 393 read + 20 000 written = 470 893).
        for (input, read, write_1h, write_5m) in [
            (15_161, 400_000, 30_000, 10_000),
            (2_000, 440_000, 30_000, 0),
            (1_500, 449_393, 20_000, 0),
        ] {
            let mut row = display_bare(NOW - (3 - rows.len() as i64) * 60_000 - 30_000);
            row.session_id = Some("ses-hot".into());
            row.model = Some("claude-opus-5".into());
            row.provider = Some("anthropic_sub".into());
            row.input = Some(input);
            row.cache_read = Some(read);
            row.cache_write_1h = Some(write_1h);
            row.cache_write_5m = Some(write_5m);
            row.output = Some(768);
            row.req_messages = Some(75 + rows.len() as i64);
            row.compact_generations = Some(1);
            rows.push(row);
        }
        // The latest turn of ses-hot was served upgraded.
        rows.last_mut().expect("ses-hot").forced_to = Some("claude-opus-5-5".into());

        // ses-crowded: one idle turn at 85% of a 200k window.
        let mut crowded = display_bare(NOW - 10 * 60_000);
        crowded.session_id = Some("ses-crowded".into());
        crowded.model = Some("claude-haiku-4-5".into());
        crowded.provider = Some("anthropic_sub".into());
        crowded.input = Some(3_000);
        crowded.cache_read = Some(160_000);
        crowded.cache_write_1h = Some(7_000);
        crowded.cache_write_5m = Some(0);
        crowded.output = Some(100);
        rows.push(crowded);
        // …and two earlier turns, because occupancy is a claim about a
        // conversation (the three-request rule). The oldest read
        // nothing from cache — one cold request for the tokens panel.
        for (at, read) in [(11, 150_000), (12, 0)] {
            let mut row = display_bare(NOW - at * 60_000);
            row.session_id = Some("ses-crowded".into());
            row.model = Some("claude-haiku-4-5".into());
            row.provider = Some("anthropic_sub".into());
            row.input = Some(3_000);
            row.cache_read = Some(read);
            row.cache_write_1h = Some(0);
            row.cache_write_5m = Some(0);
            rows.push(row);
        }

        // ses-mystery: no catalogue entry — the ceiling is `?`, and the
        // context line renders the prompt against it, claiming no
        // share. Three turns, because occupancy is a claim about a
        // conversation.
        for (at, input) in [(4, 100), (3, 100), (2, 2_132)] {
            let mut row = display_bare(NOW - at * 60_000);
            row.session_id = Some("ses-mystery".into());
            row.model = Some("gpt-5.6-terra".into());
            row.provider = Some("anthropic_sub".into());
            row.input = Some(input);
            row.cache_read = Some(1_000);
            row.cache_write_1h = Some(0);
            row.cache_write_5m = Some(0);
            row.output = Some(if at == 2 { 40 } else { 0 });
            rows.push(row);
        }

        // ses-free: released past the gate for the window now running.
        for at in [6, 5, 1] {
            let mut row = display_bare(NOW - at * 60_000);
            row.session_id = Some("ses-free".into());
            row.model = Some("claude-sonnet-5".into());
            row.provider = Some("anthropic_sub".into());
            row.input = Some(500);
            row.cache_read = Some(2_000);
            row.cache_write_1h = Some(0);
            row.cache_write_5m = Some(0);
            row.output = Some(if at == 1 { 60 } else { 0 });
            rows.push(row);
        }

        // The precomputed rebuild section, the way the loop's quota
        // cadence builds it: two rewrites over the threshold, one of
        // them a localised system-prompt change.
        let rebuilds = crate::tui::rebuilds::RebuildAgg {
            rebuilds: 2,
            measured: 9,
            unmeasured: 0,
            causes: vec![
                (crate::tui::rebuilds::Cause::SystemPrompt, 1),
                (crate::tui::rebuilds::Cause::NewPrefix, 1),
            ],
            events: vec![crate::tui::rebuilds::RebuildEvent {
                ts_ms: NOW - 30_000,
                session: "ses-hot".into(),
                cause: crate::tui::rebuilds::Cause::SystemPrompt,
                detail: Some("43,696 → 43,801 chars; block 1, in the last 8 bytes".into()),
                rewritten: 20_000,
                system: None,
            }],
        };

        let mut released = HashSet::new();
        released.insert("ses-free".to_owned());
        (rows, rebuilds, released)
    }

    fn rendered(snap: &model::Snapshot, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        let tz = utc();
        terminal
            .draw(|frame| super::render(frame, snap, "12:34:56", &tz))
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
            "last 30m",
            "12:34:56",
            "2 sessions",
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
        let snap = model::aggregate(
            &[],
            None,
            &HashSet::new(),
            &no_labels(),
            &FetchedCatalogs::default(),
            None,
            30,
            NOW,
            523,
        );
        let text = rendered(&snap, 100, 30);
        assert!(text.contains("no requests in window"));
        assert!(text.contains("no data in window"));
        // The lifetime ledger count is deliberately absent from the
        // header: it answers nothing the window does not.
        assert!(!text.contains("in ledger"), "no lifetime count:\n{text}");
        assert!(
            !text.contains("toker ·"),
            "no title, live or otherwise:\n{text}"
        );
        assert!(!text.contains("$"), "no dollar figure is invented");
    }

    #[test]
    fn empty_ledger_renders_the_same_empty_window_shape() {
        // A zero-total ledger and a quiet window are indistinguishable
        // on the dashboard now — the header shows the window only, and
        // neither state invents anything.
        let snap = model::aggregate(
            &[],
            None,
            &HashSet::new(),
            &no_labels(),
            &FetchedCatalogs::default(),
            None,
            30,
            NOW,
            0,
        );
        let text = rendered(&snap, 80, 24);
        assert!(text.contains("no requests in window"));
        assert!(!text.contains("ledger"), "no lifetime count:\n{text}");
    }

    /// The header row as text plus the foreground colour of the cell
    /// where `needle` starts in it.
    fn header_fg(snap: &model::Snapshot, width: u16, needle: &str) -> (String, Color) {
        let mut terminal = Terminal::new(TestBackend::new(width, 30)).expect("terminal");
        let tz = utc();
        terminal
            .draw(|frame| super::render(frame, snap, "12:34:56", &tz))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let cells: Vec<&str> = (0..width).map(|x| buffer[(x, 0)].symbol()).collect();
        let row = cells.concat();
        // Every header symbol is one ASCII-or-`·` cell, so a cell index
        // is a char index of the joined row.
        let at = row
            .char_indices()
            .position(|(i, _)| row[i..].starts_with(needle))
            .unwrap_or_else(|| panic!("{needle:?} not in header {row:?}"));
        (row, buffer[(at as u16, 0)].fg)
    }

    fn with_latest(mut snap: model::Snapshot, latest: Option<i64>) -> model::Snapshot {
        snap.latest_row_ts_ms = latest;
        snap
    }

    #[test]
    fn freshness_is_green_while_fresh() {
        let snap = with_latest(snapshot(), Some(NOW - 4_000));
        let (row, fg) = header_fg(&snap, 100, "4s ago");
        assert_eq!(fg, Color::Green, "in {row:?}");
        assert!(row.contains("last req 4s ago · 12:34:56"), "{row:?}");
        assert!(row.contains("2 sessions"), "the summary stays: {row:?}");
    }

    #[test]
    fn freshness_is_yellow_while_stale() {
        let snap = with_latest(snapshot(), Some(NOW - 120_000));
        let (row, fg) = header_fg(&snap, 100, "120s ago");
        assert_eq!(fg, Color::Yellow, "in {row:?}");
    }

    #[test]
    fn freshness_is_red_in_minutes_once_dead() {
        let snap = with_latest(snapshot(), Some(NOW - 600_000));
        let (row, fg) = header_fg(&snap, 100, "10m ago");
        assert_eq!(fg, Color::Red, "in {row:?}");
    }

    #[test]
    fn freshness_says_no_data_on_an_empty_ledger() {
        let snap = with_latest(model::empty(30), None);
        let (row, fg) = header_fg(&snap, 100, "no data");
        assert_eq!(fg, Color::Red, "in {row:?}");
        assert!(row.contains("last req no data"), "{row:?}");
    }

    #[test]
    fn freshness_shows_when_the_window_is_empty() {
        // The dead-proxy case: nothing in the window, but the ledger's
        // newest row dates the last write.
        let snap = model::aggregate(
            &[],
            None,
            &HashSet::new(),
            &no_labels(),
            &FetchedCatalogs::default(),
            None,
            30,
            NOW,
            523,
        );
        let snap = with_latest(snap, Some(NOW - 2 * 3_600_000));
        let (row, fg) = header_fg(&snap, 100, "120m ago");
        assert_eq!(fg, Color::Red, "in {row:?}");
        assert!(row.contains("no requests in window"), "{row:?}");
    }

    #[test]
    fn freshness_rounds_and_switches_at_the_predecessor_thresholds() {
        let at = |age_ms: i64| {
            let mut snap = with_latest(model::empty(30), Some(NOW - age_ms));
            snap.now_ms = NOW;
            let (text, style) = super::freshness(&snap);
            (text, style.fg.expect("coloured"))
        };
        assert_eq!(at(29_400), ("29s ago".into(), Color::Green));
        assert_eq!(at(29_500), ("30s ago".into(), Color::Yellow));
        assert_eq!(at(299_400), ("299s ago".into(), Color::Yellow));
        assert_eq!(at(299_500), ("5m ago".into(), Color::Red));
        // 5.5 minutes rounds up, as `Math.round` did.
        assert_eq!(at(330_000), ("6m ago".into(), Color::Red));
        // A row stamped after the frame is not a negative age.
        assert_eq!(at(-5_000), ("0s ago".into(), Color::Green));
    }

    #[test]
    fn narrow_header_sheds_summary_then_clock_then_label() {
        let snap = with_latest(snapshot(), Some(NOW - 4_000));
        // "last req 4s ago · 12:34:56" is 26 cells; one more for the gap.
        let (row, _) = header_fg(&snap, 40, "4s ago");
        assert!(row.contains("last req 4s ago · 12:34:56"), "{row:?}");
        assert!(row.starts_with("last 30m"), "summary clips: {row:?}");
        assert!(!row.contains("2 sessions"), "summary clips: {row:?}");

        let (row, _) = header_fg(&snap, 20, "4s ago");
        assert!(row.contains("last req 4s ago"), "{row:?}");
        assert!(!row.contains("12:34:56"), "the clock goes next: {row:?}");

        let (row, fg) = header_fg(&snap, 10, "4s ago");
        assert!(!row.contains("last req"), "the label goes last: {row:?}");
        assert_eq!(fg, Color::Green, "the coloured age survives");
    }

    #[test]
    fn narrow_terminal_sheds_rightmost_columns() {
        let snap = snapshot();
        // 100 wide: the full column set, model included.
        let wide = rendered(&snap, 100, 36);
        assert!(wide.contains("z-ai/glm-5.3"));
        assert!(wide.contains("MODEL"));
        assert!(wide.contains("PEAK"));

        // 40 wide (sessions block ≈ 38 inner cells): the chain sheds
        // down to SESSION + CTX — the id and its ceiling survive, the
        // model and token columns do not (clipped, not wrapped).
        let narrow = rendered(&snap, 40, 36);
        assert!(narrow.contains("ses-abc"), "session ids always survive");
        assert!(!narrow.contains("z-ai/glm-5.3"), "model column is shed");
        assert!(!narrow.contains("PEAK"), "rightmost columns are shed");

        // 80 wide: an intermediate level — model kept, LAST shed.
        let mid = rendered(&snap, 80, 36);
        assert!(mid.contains("z-ai/glm-5.3"));
        assert!(mid.contains("MODEL"));
        assert!(!mid.contains("LAST"));

        // 20 rows tall: the height budget sheds the sessions LIST
        // before anything else — the panel keeps its scaffold (title
        // and header), the rows go, and nothing wraps or panics.
        let short = rendered(&snap, 40, 20);
        assert!(short.contains("SESSIONS"), "the scaffold survives");
        assert!(!short.contains("ses-abc"), "the list rows are shed");
    }

    #[test]
    fn tiny_terminal_does_not_panic_and_keeps_the_session_column() {
        let snap = snapshot();
        // Height 12 leaves the middle panels two rows between the
        // header and the fixed bottom row; the sessions scaffold keeps
        // them (the guard keeps its top — the SESSIONS scaffold and
        // the quota block — over everything else). Width 16 is barely
        // enough for the panel titles. The point: no panic, and the
        // two panels that matter survive.
        let text = rendered(&snap, 16, 12);
        assert!(text.contains("SESSIONS"));
        assert!(text.contains("RATE"));
    }

    #[test]
    fn column_chain_sheds_in_order() {
        let plan = |width: u16| super::session_plan(width).0.to_vec();
        // Full set: 97 + 9 spacing = 106 cells needed.
        assert_eq!(
            plan(106),
            [
                "SESSION", "CTX", "MODEL", "REQS", "IN NOW", "PEAK", "MSGS", "CMPCT", "OUT", "LAST"
            ]
        );
        assert_eq!(
            plan(120),
            [
                "SESSION", "CTX", "MODEL", "REQS", "IN NOW", "PEAK", "MSGS", "CMPCT", "OUT", "LAST"
            ]
        );
        // Each step down sheds exactly the rightmost surviving column.
        assert_eq!(
            plan(105),
            [
                "SESSION", "CTX", "MODEL", "REQS", "IN NOW", "PEAK", "MSGS", "CMPCT", "OUT"
            ]
        );
        assert_eq!(
            plan(96),
            [
                "SESSION", "CTX", "MODEL", "REQS", "IN NOW", "PEAK", "MSGS", "CMPCT"
            ]
        );
        assert_eq!(
            plan(87),
            ["SESSION", "CTX", "MODEL", "REQS", "IN NOW", "PEAK", "MSGS"]
        );
        assert_eq!(
            plan(80),
            ["SESSION", "CTX", "MODEL", "REQS", "IN NOW", "PEAK"]
        );
        assert_eq!(plan(70), ["SESSION", "CTX", "MODEL", "REQS", "IN NOW"]);
        assert_eq!(plan(59), ["SESSION", "CTX", "MODEL", "REQS"]);
        assert_eq!(plan(48), ["SESSION", "CTX", "MODEL"]);
        assert_eq!(plan(24), ["SESSION", "CTX"]);
        assert_eq!(plan(18), ["SESSION"]);
        // Degenerate: never empty, pinned to whatever exists.
        assert_eq!(plan(5), ["SESSION"]);
        // Leftover width lands on SESSION: at 110 cells, 106 are needed.
        let (_, widths) = super::session_plan(110);
        assert_eq!(widths[0], ratatui::layout::Constraint::Length(18 + 4));
    }

    // ── the rate & quota panel ────────────────────────────────────────

    #[test]
    fn quota_panel_renders_meters_spent_and_binding() {
        let snap = quota_snapshot();
        // 200 columns: the rate panel (alone: the fixture is unbilled)
        // holds every meter line at full width — bar, resets clock,
        // status, verdict.
        let text = rendered(&snap, 200, 30);
        assert!(text.contains("RATE & QUOTA"));
        // The 5-hour meter: measured burn that resets first, gated by
        // overageInUse, so the countdown is to exhaustion — but the
        // gate is why the prefix is there.
        assert!(text.contains("resets 15:53 · gated · on track"));
        // The 7-day meter: a day-scale burn (the day-scale-span rule)
        // reaching the armed gate's threshold before the reset, the
        // runout labelled with a weekday so it cannot be misread as
        // the reset's day.
        assert!(text.contains("resets Sun 12:53 · stops ~Thu 01:55"));
        // The overage meter: no span out-measures the quantisation at
        // the day-scale minimum — an explicit "estimating", never a
        // silent guess — plus a status that is not "allowed".
        assert!(text.contains("rejected  resets 10 Feb · estimating"));
        // The spent line: today measured from the pre-midnight
        // baseline, the window flat below the quantisation.
        assert!(text.contains("today +6%  ·  30m <1%"), "{text}");
        // The binding claim, with overage flagged.
        assert!(text.contains("binding  five_hour"), "{text}");
        assert!(text.contains("overage IN USE"), "{text}");
        // The bars render utilisation: 32% of a 22-cell bar is 7
        // filled, 80% is 18, 64% is 14.
        assert!(
            text.contains(&format!(
                "  5-hour   {}  32%",
                "█".repeat(7) + &"░".repeat(15)
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "  7-day    {}  80%",
                "█".repeat(18) + &"░".repeat(4)
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "  overage  {}  64%",
                "█".repeat(14) + &"░".repeat(8)
            )),
            "{text}"
        );
    }

    #[test]
    fn quota_panel_renders_nothing_without_meter_rows() {
        // The per-backend panel rule: an openai-shaped window (rows
        // with no rate-limit snapshots) gets the sparkline but no
        // meter bars, no spent line, no binding claim — absence, not
        // zeros.
        let snap = snapshot();
        let text = rendered(&snap, 100, 30);
        assert!(text.contains("RATE & QUOTA"), "the panel itself stays");
        assert!(!text.contains("5-hour"));
        assert!(!text.contains("7-day"));
        assert!(!text.contains("overage"));
        assert!(!text.contains("binding"));
        assert!(!text.contains("spent"));
        assert_eq!(snap.quota, None);
    }

    #[test]
    fn a_narrow_panel_sheds_resets_and_status_but_keeps_the_verdicts() {
        let snap = quota_snapshot();
        // 54 columns → the rate panel (alone: the fixture is unbilled)
        // has inner 52: no meter's resets clause leaves the bar its
        // floor, so the clauses and the status go and every verdict
        // survives — the verdict is the last thing to go.
        let text = rendered(&snap, 54, 30);
        assert!(!text.contains("resets"), "the resets clause is shed");
        assert!(!text.contains("rejected"), "the status is shed first");
        assert!(text.contains("gated · on track"));
        assert!(text.contains("stops ~Thu 01:55"));
        assert!(text.contains("estimating"));
        assert!(text.contains("today +6%  ·  30m <1%"), "spent stays");
    }

    /// The row index of the bottom strip's top border: the first row
    /// holding the RATE & QUOTA title.
    fn strip_top(text: &str) -> usize {
        text.lines()
            .position(|line| line.contains("RATE & QUOTA"))
            .expect("the rate panel renders")
    }

    #[test]
    fn an_unbilled_window_hides_spend_and_gives_quota_the_strip() {
        // The quota fixture is subscription-shaped: no row is billed,
        // so SPEND could only restate counts. RATE & QUOTA takes the
        // whole width, and the strip is its natural height — three
        // rate lines, three meters, spent, binding, and borders — not
        // the nine-row floor SPEND's breakdown needs.
        let snap = quota_snapshot();
        assert!(!snap.spend.carries_cost());
        let text = rendered(&snap, 100, 30);
        assert!(!text.contains("SPEND"), "{text}");
        assert!(!text.contains("no cost data"), "{text}");
        let top = text.lines().nth(strip_top(&text)).expect("the top row");
        assert!(top.starts_with("┌RATE & QUOTA"), "{top}");
        assert!(top.ends_with('┐'), "{top}");
        assert_eq!(text.lines().count() - strip_top(&text), 10, "{text}");

        // With no quota section either, the strip is the rate panel's
        // three lines and its borders.
        let mut unpriced = display_bare(NOW - 60_000);
        unpriced.session_id = Some("ses-x".into());
        unpriced.model = Some("claude-opus-5".into());
        unpriced.provider = Some("anthropic_sub".into());
        let snap = model::aggregate(
            &[unpriced],
            None,
            &HashSet::new(),
            &no_labels(),
            &FetchedCatalogs::default(),
            None,
            30,
            NOW,
            1,
        );
        assert_eq!(snap.spend.no_cost_data, 1);
        let text = rendered(&snap, 100, 30);
        assert!(!text.contains("SPEND"), "{text}");
        assert_eq!(text.lines().count() - strip_top(&text), 5, "{text}");
    }

    #[test]
    fn a_billed_window_splits_the_strip_evenly() {
        // The shared fixture carries one billed request: SPEND renders
        // on the left half and RATE & QUOTA on the right, at the
        // nine-row floor SPEND's breakdown needs.
        let snap = snapshot();
        assert!(snap.spend.carries_cost());
        let text = rendered(&snap, 100, 30);
        let top: Vec<char> = text
            .lines()
            .nth(strip_top(&text))
            .expect("the top row")
            .chars()
            .collect();
        assert_eq!(top.len(), 100);
        assert!(top[..50].iter().collect::<String>().starts_with("┌SPEND"));
        assert_eq!(top[49], '┐');
        assert!(
            top[50..]
                .iter()
                .collect::<String>()
                .starts_with("┌RATE & QUOTA"),
            "{text}"
        );
        assert_eq!(text.lines().count() - strip_top(&text), 9, "{text}");
    }

    /// One meter line's text at `width`, from the quota fixture's
    /// meter labelled `label`.
    fn meter_text(snap: &model::Snapshot, label: &str, width: u16) -> String {
        let quota = snap.quota.as_ref().expect("the fixture has meters");
        let meter = quota
            .meters
            .iter()
            .find(|meter| meter.label == label)
            .expect("the fixture has this meter");
        super::meter_line(meter, quota.gate_assumed, width, NOW, &utc())
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn a_meter_line_always_spaces_its_percentage_from_the_right_side() {
        // The gap once subtracted the separating space without
        // emitting it: at the width that left the bar exactly its
        // room, the line read "11%on track". Every width from the
        // degenerate (the left side cut so the verdict survives) to the
        // full line keeps the space, and the line never overruns its
        // width once the verdict itself fits.
        let snap = quota_snapshot();
        for label in ["5-hour", "7-day", "overage"] {
            for width in 24..=90u16 {
                let text = meter_text(&snap, label, width);
                assert!(
                    text.chars().count() <= width as usize,
                    "{label} at {width} overruns: {text:?}"
                );
                if let Some(at) = text.find('%') {
                    assert_eq!(
                        text[at + 1..].chars().next(),
                        Some(' '),
                        "{label} at {width}: {text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_bar_shrinks_before_the_reset_clock_drops() {
        let snap = quota_snapshot();
        // 60 cells: the full 22-cell bar beside the resets clause
        // needs 70, so the bar used to keep its width and the clock
        // went. Now the bar gives up cells first: the resets clause
        // stays and the bar takes the 12 cells left.
        let text = meter_text(&snap, "5-hour", 60);
        assert_eq!(
            text,
            format!(
                "  5-hour   {}  32% resets 15:53 · gated · on track",
                "█".repeat(4) + &"░".repeat(8)
            )
        );
        // The status still goes before the clock: with it, the bar
        // would fall under its floor.
        let text = meter_text(&snap, "overage", 60);
        assert_eq!(
            text,
            format!(
                "  overage  {}  64% resets 10 Feb · estimating",
                "█".repeat(11) + &"░".repeat(6)
            )
        );
        // Under the floor the clock goes and the bar grows back.
        let text = meter_text(&snap, "5-hour", 50);
        assert!(!text.contains("resets"), "{text:?}");
        assert!(text.ends_with(" gated · on track"), "{text:?}");
    }

    #[test]
    fn an_assumed_gate_marks_its_countdowns_with_a_question() {
        // The same readings from rows predating `gateOn`: the gate is
        // assumed (armed, by default) and every gate-aware
        // countdown says so — an assumed gate must not read the same
        // as an observed one.
        let rows = quota_rows(false);
        let snap = {
            // The view tests build the quota section once, the way the
            // loop does (over the meter lookback's narrow shape), and
            // reuse it across sizes; the display aggregate runs over
            // the same rows' display projection.
            let quota = crate::tui::quota::aggregate(
                &as_meter_rows(&rows),
                NOW,
                TODAY,
                NOW.saturating_sub(30 * 60_000),
            );
            model::aggregate(
                &as_display_rows(&rows),
                quota.as_ref(),
                &HashSet::new(),
                &no_labels(),
                &FetchedCatalogs::default(),
                None,
                30,
                NOW,
                523,
            )
        };
        assert!(snap.quota.as_ref().expect("readings exist").gate_assumed);
        let text = rendered(&snap, 200, 30);
        assert!(text.contains("gated · on track?"), "{text}");
        assert!(text.contains("stops ~Thu 01:55?"), "{text}");
        assert!(
            !text.contains("estimating?"),
            "the overage meter is not gate-aware, so no question mark"
        );
    }

    #[test]
    fn spent_line_renders_all_five_states() {
        // The README's `spent` table, one line each: a total, a
        // quantisation ceiling (<1%), a floor (≥+N%), idle, and no
        // data — the last three dim, and none of them a zero.
        let line = |today: Spent, window: Spent| {
            let mut text = String::new();
            let mut spans = super::spent_line(today, window, 30).spans.into_iter();
            for span in spans.by_ref() {
                text.push_str(span.content.as_ref());
            }
            text
        };
        assert_eq!(
            line(
                Spent::Measured {
                    points: 0.03,
                    floor: false
                },
                Spent::Measured {
                    points: 0.004,
                    floor: false
                }
            ),
            "  spent   today +3%  ·  30m <1%"
        );
        assert_eq!(
            line(
                Spent::Measured {
                    points: 0.06,
                    floor: true
                },
                Spent::Idle
            ),
            "  spent   today ≥+6%  ·  30m idle"
        );
        assert_eq!(
            line(Spent::NoData, Spent::NoData),
            "  spent   today no data  ·  30m no data"
        );
    }

    #[test]
    fn a_tiny_terminal_renders_the_quota_lines_without_panicking() {
        let snap = quota_snapshot();
        // The quota section grows the bottom row to ten; a 12-row
        // terminal still renders the panel titles and clips cleanly.
        let text = rendered(&snap, 40, 12);
        assert!(text.contains("RATE"));
        assert!(text.contains("SESSIONS"));
    }

    // ── the phase-5 panels ───────────────────────────────────────────

    #[test]
    fn sessions_table_renders_ctx_msgs_cmpct_and_the_markers() {
        let snap = full_snapshot();
        // 120 wide: every column survives; 40 tall: every panel fits.
        let text = rendered(&snap, 120, 40);
        for expected in [
            "CTX", "MSGS", "CMPCT", "1M",   // ses-hot's native ceiling, bright green
            "200k", // ses-crowded's fixed window
            "?",    // gpt-5.6-terra: outside the catalogue
            "77",   // ses-hot's latest message count
        ] {
            assert!(text.contains(expected), "expected {expected:?} in:\n{text}");
        }
        // The `↑` (ses-hot's latest turn was served upgraded) and the
        // `$` (ses-free is released past the gate) — the markers live
        // in the model column.
        assert!(text.contains("↑"), "the upgrade marker:\n{text}");
        assert!(text.contains("$"), "the released marker:\n{text}");
        // The prompt sums the write share: 1 500 + 449 393 + 20 000.
        assert!(text.contains("470,893"), "the grouped prompt:\n{text}");
        // `msgs` renders `-` where no row carried a count: ses-crowded
        // never did.
        assert!(text.contains("   -"), "a dash, never a zero:\n{text}");
    }

    /// The session NAME: working directory and title in the SESSION
    /// cell where the column fits a name
    /// (`shortDir(cwd) · title ?? prompt`, the directory cyan), the
    /// session id where it does not — too narrow (the
    /// 12-cell rule) or no label at all. An absent label is the id,
    /// never an empty cell.
    #[test]
    fn labeled_sessions_render_their_names_and_others_fall_back_to_the_id() {
        // One request each: under the CONTEXT panel's three-request
        // rule, so the ids appear nowhere but the SESSIONS table and
        // the wide render's "the labeled id is replaced" is provable.
        let mut rows = Vec::new();
        for (at, sid) in [
            (4, "ses-named"),
            (3, "ses-dir"),
            (2, "ses-prompt"),
            (1, "ses-bare"),
        ] {
            let mut row = display_bare(NOW - at * 60_000);
            row.session_id = Some(sid.into());
            row.model = Some("claude-opus-5".into());
            row.provider = Some("anthropic_sub".into());
            row.input = Some(1_000);
            row.cache_read = Some(2_000);
            row.cache_write_1h = Some(0);
            row.cache_write_5m = Some(0);
            rows.push(row);
        }
        let mut labels = HashMap::new();
        labels.insert(
            "ses-named".to_owned(),
            Label {
                cwd: Some("/home/u/code/toker".into()),
                title: Some("TUI session labels".into()),
                prompt: None,
            },
        );
        labels.insert(
            "ses-dir".to_owned(),
            // A worktree: `repo/.../worktrees/x` reads `repo/x`.
            Label {
                cwd: Some("/home/u/code/toker/worktrees/labels".into()),
                title: None,
                prompt: None,
            },
        );
        labels.insert(
            "ses-prompt".to_owned(),
            // No cwd, no title: the last prompt stands in
            // (`title ?? prompt`).
            Label {
                cwd: None,
                title: None,
                prompt: Some("the last prompt".into()),
            },
        );
        let snap = model::aggregate(
            &rows,
            None,
            &HashSet::new(),
            &labels,
            &FetchedCatalogs::default(),
            None,
            30,
            NOW,
            523,
        );

        // 120 wide: the SESSION column takes the slack — the names
        // render in full, the labeled ids are gone, and the session
        // with no label keeps its id.
        let wide = rendered(&snap, 120, 40);
        assert!(
            wide.contains("toker · TUI session labels"),
            "dir · title:\n{wide}"
        );
        assert!(
            wide.contains("toker/labels"),
            "a worktree names repo/tree:\n{wide}"
        );
        assert!(
            wide.contains("the last prompt"),
            "the prompt stands in:\n{wide}"
        );
        assert!(wide.contains("ses-bare"), "no label → the id:\n{wide}");
        assert!(
            !wide.contains("ses-named"),
            "the labeled id is replaced:\n{wide}"
        );
        assert!(
            !wide.contains("ses-dir"),
            "the labeled id is replaced:\n{wide}"
        );
        assert!(
            !wide.contains("ses-prompt"),
            "the labeled id is replaced:\n{wide}"
        );

        // 13 wide: the sessions panel keeps 11 inner cells, under the
        // 12 a name needs — the ids everywhere, the labels never, and
        // nothing panics.
        let narrow = rendered(&snap, 13, 40);
        assert!(
            narrow.contains("ses-named"),
            "too narrow → the id:\n{narrow}"
        );
        assert!(
            narrow.contains("ses-bare"),
            "too narrow → the id:\n{narrow}"
        );
        assert!(
            !narrow.contains("TUI session labels"),
            "no name at that width:\n{narrow}"
        );
        assert!(
            !narrow.contains("the last prompt"),
            "no name at that width:\n{narrow}"
        );
    }

    #[test]
    fn context_panel_renders_occupancy_ceilings_and_idle() {
        let snap = full_snapshot();
        let text = rendered(&snap, 120, 40);
        assert!(text.contains("CONTEXT"), "the panel stays:\n{text}");
        // The occupancy claim: the bar, the prompt, the ceiling, the
        // share. 470,893 of 1M is 47%.
        assert!(
            text.contains("470,893 / 1M"),
            "the known-ceiling line:\n{text}"
        );
        assert!(text.contains("47%"), "the share:\n{text}");
        assert!(
            text.contains("170,000 / 200k"),
            "the idle session still claims its occupancy:\n{text}"
        );
        assert!(text.contains("85%"), "its share:\n{text}");
        assert!(text.contains("idle 10m"), "the idle note:\n{text}");
        // Known prompt, unknown ceiling: the number is real, the share
        // is not claimable.
        assert!(
            text.contains("3,132 / ?"),
            "the unknown-ceiling line:\n{text}"
        );
        // The bar itself renders for the claimable sessions.
        assert!(text.contains("█"), "occupancy bars:\n{text}");
        assert!(text.contains("░"), "occupancy pads:\n{text}");
    }

    #[test]
    fn the_sessions_and_context_panels_show_the_same_session_name() {
        // Correlation is the point of the shared name builder: a
        // labeled session reads identically in both panels — the
        // cyan dir, the dim separator, the same title — and an
        // unlabeled one reads as its id in both.
        let mut labels = HashMap::new();
        labels.insert(
            "ses-hot".to_owned(),
            Label {
                cwd: Some("/home/u/code/toker".into()),
                title: Some("Correlated panels".into()),
                prompt: None,
            },
        );
        // ses-crowded stays UNlabeled: its id, in both panels.
        let snap = full_snapshot_with_labels(&labels);
        let text = rendered(&snap, 120, 40);
        // The labeled form appears TWICE — once per panel — and the
        // raw id never appears for that session.
        assert_eq!(
            text.matches("toker · Correlated panels").count(),
            2,
            "the label in both panels:\n{text}"
        );
        assert!(
            !text.contains("ses-hot"),
            "the labeled session never renders its id:\n{text}"
        );
        // The unlabeled one: its id, also twice.
        assert_eq!(text.matches("ses-crowded").count(), 2);
    }

    #[test]
    fn context_panel_says_so_when_no_session_has_enough_history() {
        // Occupancy is a claim about a conversation: a window whose
        // sessions never reach the three-request rule gets the explicit
        // line, never a vanishing panel (the reference's
        // "(no session with enough history yet)").
        let mut rows = Vec::new();
        for at in [2, 4] {
            let mut row = display_bare(NOW - at * 60_000);
            row.session_id = Some("ses-short".into());
            row.model = Some("claude-opus-5".into());
            row.provider = Some("anthropic_sub".into());
            row.input = Some(1_000);
            row.cache_read = Some(2_000);
            row.cache_write_1h = Some(0);
            row.cache_write_5m = Some(0);
            rows.push(row);
        }
        let snap = model::aggregate(
            &rows,
            None,
            &HashSet::new(),
            &no_labels(),
            &FetchedCatalogs::default(),
            None,
            30,
            NOW,
            523,
        );
        let text = rendered(&snap, 120, 40);
        assert!(
            text.contains("no session with enough history yet"),
            "the empty state:\n{text}"
        );
    }

    #[test]
    fn context_panel_renders_an_unknown_prompt_against_a_known_ceiling() {
        // The latest row reports no input: the prompt is unknown, so
        // the context line claims no occupancy — an explicit `?`
        // against the model's known 1M, never a zero (the ceiling is
        // not the thing in doubt; the size is).
        let mut rows = Vec::new();
        for at in [3, 2, 1] {
            let mut row = display_bare(NOW - at * 60_000);
            row.session_id = Some("ses-blank".into());
            row.model = Some("claude-opus-5".into());
            row.provider = Some("anthropic_sub".into());
            row.input = if at == 1 { None } else { Some(1_000) };
            row.cache_read = Some(2_000);
            row.cache_write_1h = Some(0);
            row.cache_write_5m = Some(0);
            rows.push(row);
        }
        let snap = model::aggregate(
            &rows,
            None,
            &HashSet::new(),
            &no_labels(),
            &FetchedCatalogs::default(),
            None,
            30,
            NOW,
            523,
        );
        let text = rendered(&snap, 120, 40);
        assert!(text.contains("? / 1M"), "the unknown-prompt line:\n{text}");
        // …and no share on that line: the `?` is the whole claim.
        let line = text
            .lines()
            .find(|line| line.contains("ses-blank"))
            .expect("the context line");
        assert!(
            !line.contains('%'),
            "no percentage beside an unknown prompt:\n{line}"
        );
    }

    #[test]
    fn rebuilds_panel_renders_counts_causes_and_localisations() {
        let snap = full_snapshot();
        let text = rendered(&snap, 120, 40);
        for expected in [
            "CACHE REBUILDS",
            "2 of 9 requests rewrote ≥50,000 tokens",
            "system prompt changed",
            "new prefix / first turn",
            "· ses-hot — system prompt changed (43,696 → 43,801 chars; block 1, in the last 8 bytes)",
        ] {
            assert!(text.contains(expected), "expected {expected:?} in:\n{text}");
        }
        // The cause rows' count bars.
        assert!(text.contains("▬"), "the cause bars:\n{text}");
    }

    #[test]
    fn rebuilds_panel_without_rebuilds_says_so_rather_than_vanishing() {
        // No rewrites over the threshold: the panel keeps its place and
        // its verdict. With unknown rewrites the verdict is scoped to
        // the measured requests, exactly the reference's two
        // none-lines.
        let none = crate::tui::rebuilds::RebuildAgg {
            rebuilds: 0,
            measured: 12,
            unmeasured: 0,
            causes: vec![],
            events: vec![],
        };
        let mut snap = full_snapshot();
        snap.rebuilds = Some(none);
        let text = rendered(&snap, 120, 40);
        assert!(text.contains("0 of 12 requests rewrote ≥50,000 tokens"));
        assert!(text.contains("none — every prefix held"), "{text}");

        let unknowns = crate::tui::rebuilds::RebuildAgg {
            rebuilds: 0,
            measured: 3,
            unmeasured: 9,
            causes: vec![],
            events: vec![],
        };
        snap.rebuilds = Some(unknowns);
        let text = rendered(&snap, 120, 40);
        assert!(
            text.contains("0 of 3 measured requests rewrote ≥50,000 tokens · 9 unknown"),
            "{text}"
        );
        assert!(text.contains("none among measured requests"), "{text}");

        // Before the quota cadence's first pass there is no section at
        // all — and that renders as its own state too.
        snap.rebuilds = None;
        let text = rendered(&snap, 120, 40);
        assert!(text.contains("no rebuild data yet"), "{text}");
        assert!(
            text.contains("CACHE REBUILDS"),
            "the panel never vanishes:\n{text}"
        );
    }

    #[test]
    fn the_height_budget_grows_the_sessions_list_and_sheds_from_the_top() {
        let snap = full_snapshot();
        // Comfortable: everything at natural height and the slack goes
        // to the sessions list (its rows render).
        let text = rendered(&snap, 120, 44);
        assert!(
            text.contains("ses-hot"),
            "sessions rows at natural height:\n{text}"
        );
        assert!(text.contains("CACHE REBUILDS"), "{text}");

        // Short: the middle sheds from the top — the sessions LIST
        // rows go first (the scaffold keeps its title), and the quota
        // block at the bottom never scrolls off.
        let text = rendered(&snap, 120, 24);
        assert!(text.contains("SESSIONS"), "the sessions scaffold:\n{text}");
        assert!(
            !text.contains("claude-opus-5"),
            "the list rows are shed (the id also rides the rebuild detail line, the model cell does not):\n{text}"
        );
        assert!(
            text.contains("RATE & QUOTA"),
            "the quota block stays:\n{text}"
        );
        assert!(
            text.contains("CACHE REBUILDS"),
            "the panel to watch stays:\n{text}"
        );
    }
    #[test]
    fn degenerate_terminal_sizes_render_without_panicking() {
        // The height budget's floor: a frame too short for the header
        // and the bottom strip still renders — panels clip, nothing
        // panics, and a one-row frame is not a crash.
        let mut rows = Vec::new();
        for at in 1..=4 {
            let mut row = display_bare(NOW - at * 60_000);
            row.session_id = Some("ses-x".into());
            row.model = Some("claude-opus-5".into());
            row.provider = Some("anthropic_sub".into());
            row.input = Some(1_000);
            row.cache_read = Some(2_000);
            row.cache_write_1h = Some(0);
            row.cache_write_5m = Some(0);
            rows.push(row);
        }
        rows.push(display_billed(
            NOW - 30_000,
            Some("ses-x"),
            "claude-opus-5",
            "anthropic_sub",
            10,
            20,
            5,
            0.1,
        ));
        let snap = model::aggregate(
            &rows,
            None,
            &HashSet::new(),
            &no_labels(),
            &FetchedCatalogs::default(),
            None,
            30,
            NOW,
            5,
        );
        for (width, height) in [
            (1u16, 1u16),
            (2, 2),
            (3, 3),
            (5, 4),
            (8, 6),
            (10, 3),
            (16, 2),
            (40, 5),
            (60, 8),
            (200, 1),
            (1, 40),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| super::render(frame, &snap, "12:34:56", &utc()))
                .expect("draw");
        }
    }
}
