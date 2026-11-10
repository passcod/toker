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
    /// The field of an open form this line is, which a click on it focuses.
    pub(super) field: Option<usize>,
    /// Whether the line is the focused field of an open form: the one the
    /// window follows, and the one drawn reversed.
    pub(super) focused: bool,
    /// A line that must stay in view, as an open form's save control: drawn
    /// in the panel's last row when the window has scrolled past it.
    pub(super) pinned: bool,
}

impl BodyLine {
    pub(super) fn plain(line: Line<'static>) -> Self {
        BodyLine {
            line,
            entry: None,
            field: None,
            focused: false,
            pinned: false,
        }
    }

    pub(super) fn of(line: Line<'static>, entry: usize) -> Self {
        BodyLine {
            line,
            entry: Some(entry),
            field: None,
            focused: false,
            pinned: false,
        }
    }

    /// A line that stays in view whatever the window.
    pub(super) fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }

    /// A line of an open form, a field or what sits with one.
    pub(super) fn form(line: Line<'static>, field: Option<usize>, focused: bool) -> Self {
        BodyLine {
            line,
            entry: None,
            field,
            focused,
            pinned: false,
        }
    }
}

/// Where a drawn table put its lines: those a selection can rest on, and the
/// fields of an open form.
#[derive(Debug, Default)]
pub(super) struct Hits {
    pub(super) rows: Vec<RowHit>,
    pub(super) fields: Vec<(Rect, usize)>,
}

/// A table's column heading and its lines.
pub(super) struct Table {
    pub(super) header: Line<'static>,
    pub(super) body: Vec<BodyLine>,
}

/// What a table's rows are called in the count it leaves where it was cut.
pub(super) const NOUN: Noun = Noun {
    one: "record",
    many: "records",
};

/// How many of the records a table leaves out of the lines `range` hides,
/// which count lines instead where no record is among them: a group's head
/// is not a record, and a count of nothing would say nothing.
fn hidden(body: &[BodyLine], range: std::ops::Range<usize>) -> (usize, Noun) {
    let records = body[range.clone()]
        .iter()
        .filter(|line| line.entry.is_some())
        .count();
    if records > 0 {
        (records, NOUN)
    } else {
        (
            range.len(),
            Noun {
                one: "line",
                many: "lines",
            },
        )
    }
}

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
pub(super) fn reversed(line: Line<'static>, width: usize) -> Line<'static> {
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
    /// How many of the detail's lines its area holds, the framing aside.
    pub(super) detail_rows: usize,
    /// Whether the detail is drawn without a frame of its own, as the one
    /// line of a reading is.
    pub(super) bare: bool,
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
        detail_rows: usize::from(detail_area.height).saturating_sub(cost),
        bare: false,
    }
}

/// Share `body` between a table and a detail with a reading in it, the
/// table keeping its rows and the detail giving up its own: first its
/// frame, then its lines down to the reading, which is all that is left of
/// it in the last row (spec: UTUI#what-is-shed-first). Where even that leaves
/// the table too few rows, the table is cut instead.
pub(super) fn split_reading(body: Rect, table: usize, detail: usize) -> Split {
    let whole = split(body, table, detail);
    let height = usize::from(body.height);
    let ruled = usize::from(Chrome::Ruled.cost());
    if detail == 0 || usize::from(whole.table.height) >= table + usize::from(whole.chrome.cost()) {
        return whole;
    }
    let rest = height.saturating_sub(table + ruled);
    if rest >= 2 {
        return Split {
            table: Rect {
                height: (table + ruled) as u16,
                ..body
            },
            detail: Rect {
                y: body.y + (table + ruled) as u16,
                height: rest as u16,
                ..body
            },
            chrome: Chrome::Ruled,
            detail_rows: rest - ruled,
            bare: false,
        };
    }
    // The reading alone, in the last row; with no room for it beside the
    // least table worth a panel, the detail goes.
    let reading = usize::from(height > TABLE_FLOOR + ruled);
    Split {
        table: Rect {
            height: (height - reading) as u16,
            ..body
        },
        detail: Rect {
            y: body.y + (height - reading) as u16,
            height: reading as u16,
            ..body
        },
        chrome: Chrome::Ruled,
        detail_rows: reading,
        bare: true,
    }
}

/// A detail that can be cut to the room it is given: its lines, and the one
/// of them that is the reading the panel exists to give.
pub(super) struct Detail {
    pub(super) lines: Vec<Line<'static>>,
    reading: Option<Reading>,
}

/// The reading of a detail: its line, said in full, in less and in its
/// fewest words, and the keys to close the line with where it is all that is
/// shown.
struct Reading {
    at: usize,
    /// From the fullest to the briefest; the last always fits something.
    forms: Vec<Vec<Span<'static>>>,
    hint: &'static str,
}

impl Detail {
    /// A detail with nothing in it to prefer: it is cut from the bottom.
    pub(super) fn whole(lines: Vec<Line<'static>>) -> Self {
        Detail {
            lines,
            reading: None,
        }
    }

    /// A detail whose line `at` is its reading, in the `forms` it can be
    /// said in from the fullest to the briefest, with `hint` after it.
    pub(super) fn reading(
        lines: Vec<Line<'static>>,
        at: usize,
        forms: Vec<Vec<Span<'static>>>,
        hint: &'static str,
    ) -> Self {
        Detail {
            lines,
            reading: Some(Reading { at, forms, hint }),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.lines.len()
    }

    pub(super) fn has_reading(&self) -> bool {
        self.reading.is_some()
    }

    /// The lines that fit in `rows` of `width` cells. The reading goes last
    /// of all: the controls, then the title, then the rest, from the top.
    pub(super) fn shed(self, rows: usize, width: usize) -> Vec<Line<'static>> {
        if rows >= self.lines.len() {
            return self.lines;
        }
        let Some(reading) = self.reading else {
            return self.lines.into_iter().take(rows).collect();
        };
        if rows == 1 {
            let keys = format!("   {}", reading.hint);
            let room = width.saturating_sub(keys.width() + 2);
            let mut forms = reading.forms;
            let at = forms
                .iter()
                .position(|form| width_of(form) <= room)
                .unwrap_or(forms.len().saturating_sub(1));
            let mut spans = if forms.is_empty() {
                Vec::new()
            } else {
                forms.swap_remove(at)
            };
            spans.push(span(keys, dim()));
            return vec![Line::from(spans)];
        }
        let last = self.lines.len() - 1;
        let mut order: Vec<usize> = Vec::new();
        for at in [reading.at, last, 0].into_iter().chain(1..last) {
            if !order.contains(&at) {
                order.push(at);
            }
        }
        let mut kept: Vec<usize> = order.into_iter().take(rows).collect();
        kept.sort_unstable();
        self.lines
            .into_iter()
            .enumerate()
            .filter(|(at, _)| kept.contains(at))
            .map(|(_, line)| line)
            .collect()
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
) -> Hits {
    let inner = chrome::panel(frame, area, title, framing);
    if inner.height == 0 {
        return Hits::default();
    }
    let width = usize::from(inner.width);
    frame.render_widget(padded(table.header, width), Rect { height: 1, ..inner });
    let room = usize::from(inner.height) - 1;
    let on = |line: &BodyLine| selected.is_some() && line.entry == selected;
    // The window follows the focused field of an open form, and the selected
    // row otherwise.
    let at = table
        .body
        .iter()
        .position(|line| line.focused)
        .or_else(|| table.body.iter().position(on))
        .unwrap_or(0);
    let window = chrome::selection_window(table.body.len(), room, at);
    let mut rows: Vec<(Line<'static>, Option<usize>, Option<usize>)> = Vec::new();
    // The counts say how many records are out of view, not how many lines.
    let first = window.shown.start.min(table.body.len());
    let last = window.shown.end.min(table.body.len());
    if window.above.is_some() {
        let (above, noun) = hidden(&table.body, 0..first);
        rows.push((
            Line::styled(format!("… {above} {} above", noun.of(above)), dim()),
            None,
            None,
        ));
    }
    let below = window
        .below
        .map(|_| hidden(&table.body, last..table.body.len()));
    let shown = window.shown.clone();
    let pinned_out = table
        .body
        .iter()
        .position(|line| line.pinned)
        .filter(|at| !shown.contains(at));
    let mut pinned_line = None;
    for (index, body) in table.body.into_iter().enumerate() {
        if Some(index) == pinned_out {
            pinned_line = Some(body);
            continue;
        }
        if !shown.contains(&index) {
            continue;
        }
        let line = if (selected.is_some() && body.entry == selected) || body.focused {
            reversed(padded(body.line, width), width)
        } else {
            padded(body.line, width)
        };
        rows.push((line, body.entry, body.field));
    }
    if let Some((below, noun)) = below {
        rows.push((
            Line::styled(format!("… {below} more {}", noun.of(below)), dim()),
            None,
            None,
        ));
    }
    // The save control stays in view: it takes the last row.
    if let Some(pinned) = pinned_line {
        rows.truncate(room.saturating_sub(1));
        rows.push((padded(pinned.line, width), None, None));
    }
    let mut hits = Hits::default();
    for (row, (line, entry, field)) in rows.into_iter().take(room).enumerate() {
        let at = Rect {
            y: inner.y + 1 + row as u16,
            height: 1,
            ..inner
        };
        frame.render_widget(Paragraph::new(line), at);
        if let Some(entry) = entry {
            hits.rows.push(RowHit { area: at, entry });
        }
        if let Some(field) = field {
            hits.fields.push((at, field));
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

/// Draw lines with no frame of their own, where a panel has shed it: each in
/// a row, with the cell of air a panel's content has.
pub(super) fn draw_bare(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    let inner = Rect {
        x: area.x + 1.min(area.width),
        width: area.width.saturating_sub(2),
        ..area
    };
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

/// Draw a panel of lines that may hold an open form: the lines from the top,
/// scrolled only as far as keeps the focused field in view. Returns where
/// each field landed.
pub(super) fn draw_page(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    framing: Chrome,
    lines: Vec<BodyLine>,
) -> Vec<(Rect, usize)> {
    let inner = chrome::panel(frame, area, title, framing);
    let room = usize::from(inner.height);
    let width = usize::from(inner.width);
    let mut lines = lines;
    // Where the lines do not all fit, the pinned one takes the last row and
    // the rest scroll only as far as keeps the focused field in view.
    let pinned = (lines.len() > room)
        .then(|| lines.iter().position(|line| line.pinned))
        .flatten()
        .map(|at| lines.remove(at));
    let room_left = room.saturating_sub(usize::from(pinned.is_some()));
    let focus = lines.iter().position(|line| line.focused).unwrap_or(0);
    let start = (focus + 1)
        .saturating_sub(room_left)
        .min(lines.len().saturating_sub(room_left));
    let mut fields = Vec::new();
    let mut shown: Vec<BodyLine> = lines.into_iter().skip(start).take(room_left).collect();
    shown.extend(pinned);
    for (row, body) in shown.into_iter().enumerate() {
        let at = Rect {
            y: inner.y + row as u16,
            height: 1,
            ..inner
        };
        let line = if body.focused {
            reversed(padded(body.line, width), width)
        } else {
            padded(body.line, width)
        };
        frame.render_widget(Paragraph::new(line), at);
        if let Some(field) = body.field {
            fields.push((at, field));
        }
    }
    fields
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

/// The controls laid in lines of at most `width` cells: as one line where
/// they fit, and otherwise as many as they take, each control whole.
pub(super) fn controls_in(items: &[(&str, &str)], width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<String> = Vec::new();
    for (key, what) in items {
        let control = format!("[{key}] {what}");
        match lines.last_mut() {
            Some(line) if line.width() + 3 + control.width() <= width => {
                line.push_str("   ");
                line.push_str(&control);
            }
            _ => lines.push(control),
        }
    }
    lines.into_iter().map(Line::raw).collect()
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

/// Mark the rows a trace passed through, in the gutter each draws at its
/// left: the catching row `>`, the rows fallen past `↓`. A table with no
/// gutter of its own is given one, where `gutter` says so.
pub(super) fn mark_rows(
    table: &mut Table,
    marks: &std::collections::BTreeMap<usize, char>,
    gutter: bool,
) {
    let mark = |line: &mut Line<'static>, mark: Option<char>| {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        if gutter {
            let cell = match mark {
                Some(mark) => span(format!("{mark} "), bold),
                None => Span::raw("  "),
            };
            line.spans.insert(0, cell);
        } else if let Some(mark) = mark
            && let Some(first) = line.spans.first_mut()
        {
            let mut rest = first.content.chars();
            rest.next();
            let rest = rest.as_str().to_owned();
            first.content = rest.into();
            line.spans.insert(0, span(mark.to_string(), bold));
        }
    };
    if gutter {
        mark(&mut table.header, None);
    }
    for body in &mut table.body {
        let mark_of = body.entry.and_then(|entry| marks.get(&entry).copied());
        mark(&mut body.line, mark_of);
    }
}
