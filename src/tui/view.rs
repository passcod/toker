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
//! ─ toker · live · last 30m · 2 sessions · 17 requests in window   12:34:56
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
//! rolled over`/`no data` for the meters — never as a confident
//! zero.

use jiff::tz::TimeZone;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Paragraph, Row, Table},
};

use super::model::{NO_SESSION, Snapshot};
use super::quota::{MeterPanel, Spent};
use crate::middleware::cold::{Verdict, alongside, reset_label};

/// Bottom panel row height while no quota section exists: SPEND and
/// RATE side by side. Tall enough for the total line, the
/// never-dropped "no cost data" line, and a few breakdown lines. The
/// quota section grows it (see [`bottom_height`]).
const BOTTOM_HEIGHT: u16 = 9;

/// The meter bar's width (ctp live.mjs:574's `bar(v, 22)`), the widest
/// it ever renders — it shrinks as the panel narrows rather than
/// letting the line wrap or the verdict clip.
const BAR_WIDTH: u16 = 22;

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

/// The whole frame. `clock` is the preformatted HH:MM:SS string and
/// `tz` the zone the quota labels render in, both passed in so tests
/// stay deterministic.
pub(crate) fn render(frame: &mut Frame, snap: &Snapshot, clock: &str, tz: &TimeZone) {
    let [header, sessions, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(bottom_height(snap)),
    ])
    .areas(frame.area());

    render_header(frame, header, snap, clock);
    render_sessions(frame, sessions, snap);
    let [spend, rate] =
        Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).areas(bottom);
    render_spend(frame, spend, snap);
    render_rate(frame, rate, snap, tz);
}

/// The bottom row's height: the fixed [`BOTTOM_HEIGHT`] floor, grown to
/// fit the quota section's lines when one exists — ctp drops from the
/// middle rather than let the quota block scroll off the bottom
/// ("the part worth watching", live.mjs:660-679); here the sessions
/// panel yields the rows instead.
fn bottom_height(snap: &Snapshot) -> u16 {
    let Some(quota) = &snap.quota else {
        return BOTTOM_HEIGHT;
    };
    let mut lines = 3; // per-minute, sparkline, errors/drift
    lines += quota.meters.len();
    if quota.spent_today.is_some() {
        lines += 1;
    }
    if quota.binding.is_some() {
        lines += 1;
    }
    BOTTOM_HEIGHT.max((lines + 2) as u16) // + 2 border rows
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

/// RATE & QUOTA: requests per minute, the per-minute sparkline (minutes
/// with errors flagged in red), the error/drift counters — and, when the
/// snapshot carries a quota section, the plan's own meters with their
/// reset clocks and forecasts, the `spent` line, and the `binding`
/// claim (ctp live.mjs's RATE & QUOTA block, 524-626).
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

/// One meter line (ctp live.mjs's `q()`, the whole shape of it): the
/// label, a utilisation bar coloured by how much is left, and — shed
/// before anything else when the panel narrows, never wrapped — the
/// resets clock and the forecast verdict. The verdict is the last
/// thing to go; the status is the first.
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

    // Compose the right side, shedding as the panel narrows: the status
    // first, then the resets clause, the verdict last (ctp
    // live.mjs:584-589). Where ctp cuts the bar string mid-glyph at
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
    let mut use_status = status.as_deref();
    let mut use_resets = resets.as_deref();
    let mut right = with(use_status, use_resets);
    if 17 + len(&right) + BAR_WIDTH as usize > width {
        use_status = None;
        right = with(None, use_resets);
    }
    if 17 + len(&right) + BAR_WIDTH as usize > width {
        use_resets = None;
        right = with(None, None);
    }
    let bar_width = width
        .saturating_sub(17 + len(&right))
        .min(BAR_WIDTH as usize) as u16;

    // A reading from a window that has rolled is dimmed along with its
    // bar: the verdict says so in words, but a bright 95% next to it
    // is the thing the eye actually reads.
    let bar_style = match meter.verdict {
        Verdict::Stale => Style::new().dim(),
        _ if meter.util > 0.95 => Style::new().fg(Color::Red),
        _ if meter.util > 0.8 => Style::new().fg(Color::Yellow),
        _ => Style::new().fg(Color::Green),
    };
    let pct = format!("{:>3}%", (meter.util * 100.0).round() as i64);
    let left = format!("  {:<9}{} {}", meter.label, bar(meter.util, bar_width), pct);

    // The styled left side: the label plain, the bar and its
    // percentage in the bar's colour.
    let left_spans = || -> Vec<Span<'static>> {
        vec![
            Span::raw(format!("  {:<9}", meter.label)),
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

    if 16 + bar_width as usize + 1 + len(&right) <= width {
        let gap = width - 16 - bar_width as usize - 1 - len(&right);
        let mut spans = left_spans();
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right_spans());
        Line::from(spans)
    } else {
        // Degenerate width: cut the left so the verdict survives —
        // ctp's spread clamps the left side of the line, at the price
        // of the cut portion's styling.
        let keep = width.saturating_sub(len(&right) + 1);
        let mut spans = vec![
            Span::raw(left.chars().take(keep).collect::<String>()),
            Span::raw(" "),
        ];
        spans.extend(right_spans());
        Line::from(spans)
    }
}

/// The `spent` line (ctp live.mjs:602-621): how much overage today and
/// this window actually cost — the thing the utilisation bar cannot
/// say, because a meter sitting at 64% got there at some point in the
/// past, not necessarily this span. "Today" is the user's local day.
fn spent_line(today: Spent, window: Spent, window_mins: u64) -> Line<'static> {
    // The five display states (the README's `spent` table): a total,
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

/// The `binding` line (ctp live.mjs:622-625): the representative-claim
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

/// The meter bar: fill to `round(frac × w)`, pad with `░` (ctp's `bar`).
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
mod tests {
    use super::super::model;
    use super::super::quota::Spent;
    use super::super::testrows::{bare, billed, kind_row, metered};
    use crate::store::RowKind;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    const NOW: i64 = 1_769_000_000_000;
    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    /// The test frame's local-day start: 2026-01-21T12:53:20Z minus
    /// twelve hours, arbitrary but stable.
    const TODAY: i64 = NOW - 12 * HOUR;

    /// A fixed zone keeps the pinned clock strings independent of the
    /// machine running the tests; UTC keeps them readable.
    fn utc() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get("UTC").expect("UTC is always present in the tzdb")
    }

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
        model::aggregate(&rows, &rows, 30, NOW, 523, TODAY)
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
            metered(NOW - 2 * DAY, json!({"util7d": 0.10, "reset7d": reset7d})),
            metered(NOW - DAY, json!({"util7d": 0.80, "reset7d": reset7d})),
            metered(
                NOW - 13 * HOUR,
                json!({"utilOverage": 0.58, "resetOverage": reset_overage}),
            ),
            metered(NOW - 90 * MIN, json!({"util5h": 0.10, "reset5h": reset5h})),
            metered(
                NOW - 45 * MIN,
                json!({
                    "util5h": 0.30, "reset5h": reset5h,
                    "utilOverage": 0.64, "resetOverage": reset_overage,
                }),
            ),
            metered(
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
        model::aggregate(&rows, &rows, 30, NOW, 523, TODAY)
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
        let snap = model::aggregate(&[], &[], 30, NOW, 523, TODAY);
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
        let snap = model::aggregate(&[], &[], 30, NOW, 0, TODAY);
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

    // ── the rate & quota panel ────────────────────────────────────────

    #[test]
    fn quota_panel_renders_meters_spent_and_binding() {
        let snap = quota_snapshot();
        // 200 columns: the rate panel's inner 78 hold every meter line
        // at full width — bar, resets clock, status, verdict.
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
        // 100 columns → the rate panel's inner 38: the resets clause
        // and the status go, the bar shrinks, every verdict survives —
        // the verdict is the last thing to go (ctp live.mjs:585).
        let text = rendered(&snap, 100, 30);
        assert!(!text.contains("resets"), "the resets clause is shed");
        assert!(!text.contains("rejected"), "the status is shed first");
        assert!(text.contains("gated · on track"));
        assert!(text.contains("stops ~Thu 01:55"));
        assert!(text.contains("estimating"));
        assert!(text.contains("today +6%  ·  30m <1%"), "spent stays");
    }

    #[test]
    fn an_assumed_gate_marks_its_countdowns_with_a_question() {
        // The same readings from rows predating `gateOn`: the gate is
        // assumed (ctp's default of armed) and every gate-aware
        // countdown says so — an assumed gate must not read the same
        // as an observed one.
        let rows = quota_rows(false);
        let snap = model::aggregate(&rows, &rows, 30, NOW, 523, TODAY);
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
}
