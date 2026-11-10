//! Drawing a table with its detail beneath it: the panel pair every lookup
//! tab is, and the cell and line helpers they build rows from.
//!
//! Rows are built here as lines of fixed-width cells, since a record's row
//! is hand-laid (spec: UTUI#the-routing-page); the selected one is reversed
//! across the panel. Nothing here knows what a rule is.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use super::words::{dim, label};
use crate::tui::chrome::{self, Chrome, Noun, RowHit};

/// A line of a table's body, and the entry it belongs to where it is one a
/// selection can rest on: a group's head or a pass-through row is not.
pub(super) struct BodyLine {
    pub(super) line: Line<'static>,
    pub(super) entry: Option<usize>,
}

impl BodyLine {
    pub(super) fn plain(line: Line<'static>) -> Self {
        BodyLine { line, entry: None }
    }

    pub(super) fn of(line: Line<'static>, entry: usize) -> Self {
        BodyLine {
            line,
            entry: Some(entry),
        }
    }
}

/// A table's column heading and its lines.
pub(super) struct Table {
    pub(super) header: Line<'static>,
    pub(super) body: Vec<BodyLine>,
}

/// What a table's rows are called in the count it leaves where it was cut.
pub(super) const NOUN: Noun = Noun {
    one: "row",
    many: "rows",
};

/// `spans` as exactly `width` cells: clipped with an ellipsis where too long,
/// padded where short.
pub(super) fn fit(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut spans = chrome::clip_spans(spans, width);
    let used: usize = spans.iter().map(Span::width).sum();
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    spans
}

/// `spans` as exactly `width` cells, right-aligned.
pub(super) fn fit_right(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let spans = chrome::clip_spans(spans, width);
    let used: usize = spans.iter().map(Span::width).sum();
    let mut out = vec![Span::raw(" ".repeat(width.saturating_sub(used)))];
    out.extend(spans);
    out
}

/// A line made of cells laid end to end.
#[derive(Default)]
pub(super) struct Cells {
    spans: Vec<Span<'static>>,
}

impl Cells {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// A cell of exactly `width`.
    pub(super) fn cell(mut self, spans: Vec<Span<'static>>, width: usize) -> Self {
        self.spans.extend(fit(spans, width));
        self
    }

    /// A right-aligned cell of exactly `width`.
    pub(super) fn right(mut self, spans: Vec<Span<'static>>, width: usize) -> Self {
        self.spans.extend(fit_right(spans, width));
        self
    }

    /// Spans as they are, taking what room they take.
    pub(super) fn raw(mut self, spans: Vec<Span<'static>>) -> Self {
        self.spans.extend(spans);
        self
    }

    pub(super) fn line(self) -> Line<'static> {
        Line::from(self.spans)
    }
}

/// A heading cell: dim.
pub(super) fn heading(text: &str) -> Vec<Span<'static>> {
    vec![label(text.to_owned())]
}

/// Spans from a string and a style.
pub(super) fn span(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

/// A line with every colour and emphasis taken off and reversed across
/// `width`: the selected row. What colour said is still said in words.
fn reversed(line: Line<'static>, width: usize) -> Line<'static> {
    let used = line.width();
    let mut spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|span| Span::styled(span.content, Style::new().add_modifier(Modifier::REVERSED)))
        .collect();
    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::new().add_modifier(Modifier::REVERSED),
        ));
    }
    Line::from(spans)
}

/// A line of the panel's inner width with a cell of air each side.
fn padded(line: Line<'static>, inner: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(chrome::clip_spans(line.spans, inner.saturating_sub(2)));
    Line::from(spans)
}

/// How a pair of panels, a table over its detail, shares the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Split {
    pub(super) table: Rect,
    pub(super) detail: Rect,
    pub(super) chrome: Chrome,
}

/// The least rows of table content, the column heading and a row, worth a
/// panel of its own.
const TABLE_FLOOR: usize = 2;

/// Share `body` between a table needing `table` rows of content and a
/// detail needing `detail`. A panel is no taller than its content; both are
/// boxed while neither loses content to its border, ruled when they would,
/// and the table gives up rows before the detail does
/// (spec: UTUI#what-is-shed-first).
pub(super) fn split(body: Rect, table: usize, detail: usize) -> Split {
    let height = usize::from(body.height);
    let framing = if table + detail + 2 * usize::from(Chrome::Boxed.cost()) <= height {
        Chrome::Boxed
    } else {
        Chrome::Ruled
    };
    let cost = usize::from(framing.cost());
    let detail_rows = if detail == 0 {
        0
    } else {
        (detail + cost).min(height.saturating_sub(TABLE_FLOOR + cost))
    };
    let table_rows = (table + cost).min(height - detail_rows);
    let table_area = Rect {
        height: table_rows as u16,
        ..body
    };
    let detail_area = Rect {
        y: body.y + table_rows as u16,
        height: detail_rows as u16,
        ..body
    };
    Split {
        table: table_area,
        detail: detail_area,
        chrome: framing,
    }
}

/// Draw a table into `area`: its heading, then its lines windowed around the
/// selected entry with a count of what was cut on each side. Returns where
/// each entry's row landed.
pub(super) fn draw_table(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    framing: Chrome,
    table: Table,
    selected: Option<usize>,
) -> Vec<RowHit> {
    let inner = chrome::panel(frame, area, title, framing);
    if inner.height == 0 {
        return Vec::new();
    }
    let width = usize::from(inner.width);
    frame.render_widget(padded(table.header, width), Rect { height: 1, ..inner });
    let room = usize::from(inner.height) - 1;
    let on = |line: &BodyLine| selected.is_some() && line.entry == selected;
    let at = table.body.iter().position(on).unwrap_or(0);
    let window = chrome::selection_window(table.body.len(), room, at);
    let mut rows: Vec<(Line<'static>, Option<usize>)> = Vec::new();
    if let Some(above) = window.above {
        rows.push((
            Line::styled(format!("… {above} {} above", NOUN.of(above)), dim()),
            None,
        ));
    }
    let shown = window.shown.clone();
    for (index, body) in table.body.into_iter().enumerate() {
        if !shown.contains(&index) {
            continue;
        }
        let line = if selected.is_some() && body.entry == selected {
            reversed(padded(body.line, width), width)
        } else {
            padded(body.line, width)
        };
        rows.push((line, body.entry));
    }
    if let Some(below) = window.below {
        rows.push((
            Line::styled(format!("… {below} more {}", NOUN.of(below)), dim()),
            None,
        ));
    }
    let mut hits = Vec::new();
    for (row, (line, entry)) in rows.into_iter().take(room).enumerate() {
        let at = Rect {
            y: inner.y + 1 + row as u16,
            height: 1,
            ..inner
        };
        frame.render_widget(Paragraph::new(line), at);
        if let Some(entry) = entry {
            hits.push(RowHit { area: at, entry });
        }
    }
    hits
}

/// Draw a detail panel: its lines, each with a cell of air, the last of
/// them the controls.
pub(super) fn draw_detail(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    framing: Chrome,
    lines: Vec<Line<'static>>,
) {
    let inner = chrome::panel(frame, area, title, framing);
    let width = usize::from(inner.width);
    for (row, line) in lines
        .into_iter()
        .take(usize::from(inner.height))
        .enumerate()
    {
        frame.render_widget(
            Paragraph::new(padded(line, width)),
            Rect {
                y: inner.y + row as u16,
                height: 1,
                ..inner
            },
        );
    }
}

/// A label and value row of a detail: the label dim in a column of
/// `label_width`, then the value's spans.
pub(super) fn field(name: &str, label_width: usize, value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![span(format!("{name:<label_width$}"), dim())];
    spans.extend(value);
    Line::from(spans)
}

/// A detail's controls, written `[key] label`, three spaces apart.
pub(super) fn controls(items: &[(&str, &str)]) -> Line<'static> {
    Line::raw(
        items
            .iter()
            .map(|(key, what)| format!("[{key}] {what}"))
            .collect::<Vec<_>>()
            .join("   "),
    )
}

/// The width of `spans` in cells.
pub(super) fn width_of(spans: &[Span<'static>]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// The width of the widest of `texts`.
pub(super) fn widest<'a>(texts: impl IntoIterator<Item = &'a str>) -> usize {
    texts
        .into_iter()
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
}
