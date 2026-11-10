//! The frame every page draws in: the header's chips, the tab strip,
//! panel framing, list windows, the key overlay and the colour switch.
//!
//! The pages grew separately and disagreed on all of these, which an
//! operator reads as one product. Drawing them in one place is what keeps
//! them agreeing (spec: UTUI#rendering).

use ratatui::{
    Frame,
    layout::{Alignment, Position, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Clear, Padding, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use super::Page;

/// The `?` button's width: a key between two spaces.
const CHIP_WIDTH: u16 = 3;

/// How the panels are framed. Boxed panels cost two rows each (the
/// borders); a short terminal drops them for a `TITLE ───` rule that
/// costs one, because a border is the cheapest thing on the page to
/// lose — data goes only after every border has
/// (spec: UTUI#what-is-shed-first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Chrome {
    Boxed,
    Ruled,
}

impl Chrome {
    /// The rows a panel's frame takes.
    pub(super) fn cost(self) -> u16 {
        match self {
            Chrome::Boxed => 2,
            Chrome::Ruled => 1,
        }
    }

    /// Boxed while `rows` of content fit inside a box `height` tall, ruled
    /// otherwise.
    pub(super) fn fitting(rows: usize, height: u16) -> Chrome {
        if rows + usize::from(Chrome::Boxed.cost()) <= usize::from(height) {
            Chrome::Boxed
        } else {
            Chrome::Ruled
        }
    }
}

/// Draw a panel's frame — a bordered box, or the title rule — and
/// return the area its content goes in. The ruled form insets the
/// content by a column a side like the box does, so every width-driven
/// choice (column shedding, bar budgets) is the same in both.
pub(super) fn panel(frame: &mut Frame, area: Rect, title: &str, chrome: Chrome) -> Rect {
    match chrome {
        Chrome::Boxed => {
            let block = Block::bordered().title_top(title.to_owned());
            let inner = block.inner(area);
            frame.render_widget(block, area);
            inner
        }
        Chrome::Ruled => {
            if area.height == 0 {
                return area;
            }
            // The rule stops a cell short of the edge, so side-by-side
            // panels' titles never run into each other's rules.
            let rule = area.width.saturating_sub(title.width() as u16 + 2).into();
            frame.render_widget(
                Line::from(vec![
                    Span::styled(title.to_owned(), Style::new().bold()),
                    Span::raw(" "),
                    Span::styled("─".repeat(rule), Style::new().dim()),
                ]),
                Rect { height: 1, ..area },
            );
            Rect {
                x: area.x + 1.min(area.width),
                y: area.y + 1,
                width: area.width.saturating_sub(2),
                height: area.height - 1,
            }
        }
    }
}

/// Where the header's buttons landed, and the rest of the row for the
/// page's own tabs or status.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    /// The `?` button, which opens the page's overlay.
    pub(crate) keys: Rect,
    /// One button per page, indexed as [`Page::ALL`]. A click on one goes
    /// to that page. A button the header was too narrow to reach is
    /// zero-wide, so nothing is under it to click.
    pub(crate) pages: [Rect; Page::ALL.len()],
    /// The page being drawn, whose button reads as current.
    pub(crate) current: Page,
    /// What is left of the row.
    pub(crate) rest: Rect,
}

impl Header {
    /// The page whose button is under `at`.
    pub(crate) fn page_at(&self, at: Position) -> Option<Page> {
        let index = self.pages.iter().position(|button| button.contains(at))?;
        Some(Page::ALL[index])
    }
}

/// Lay out the header row: `?`, then a button for each page, each a cell
/// apart, and the rest after them. The buttons never shorten; a header
/// too narrow for all of them clips at its edge, the `?` surviving longest
/// because it is the way out to the overlay at any size.
pub(super) fn header(area: Rect, current: Page) -> Header {
    let height = area.height.min(1);
    let keys = Rect::new(area.x, area.y, area.width.min(CHIP_WIDTH), height);
    let mut pages = [Rect::default(); Page::ALL.len()];
    // One cell of gutter before every button, and after the last.
    let mut x = keys.right();
    for (button, page) in pages.iter_mut().zip(Page::ALL) {
        x = x.saturating_add(1).min(area.right());
        let width = (page.label().width() as u16 + 2).min(area.right() - x);
        *button = Rect::new(x, area.y, width, height);
        x += width;
    }
    let rest_x = x.saturating_add(1).min(area.right());
    let rest = Rect::new(rest_x, area.y, area.right() - rest_x, area.height);
    Header {
        keys,
        pages,
        current,
        rest,
    }
}

/// Draw the buttons, the current page's in bold blue and the rest dark
/// grey. Drawn last on a page, so a panel that had to share the header's
/// row cannot paint over the way out to the overlay.
pub(super) fn chips(frame: &mut Frame, header: &Header) {
    let grey = Style::new()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::REVERSED);
    let label = if header.keys.width >= CHIP_WIDTH {
        " ? "
    } else {
        "?"
    };
    frame.render_widget(Paragraph::new(label).style(grey), header.keys);
    for (button, page) in header.pages.iter().zip(Page::ALL) {
        if button.width == 0 {
            continue;
        }
        let style = if page == header.current {
            Style::new()
                .fg(Color::Blue)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            grey
        };
        frame.render_widget(
            Paragraph::new(format!(" {} ", page.label())).style(style),
            *button,
        );
    }
}

/// One tab of a page's strip. The strip numbers them itself, from one.
#[derive(Debug, Clone, Copy)]
pub(super) struct Tab<'a> {
    pub(super) label: &'a str,
    /// How many records the tab holds, where it is a list of them.
    pub(super) count: Option<usize>,
    /// What stands between the previous tab and this one. Unused on the
    /// first.
    pub(super) joiner: &'a str,
}

impl<'a> Tab<'a> {
    pub(super) const fn new(label: &'a str) -> Self {
        Tab {
            label,
            count: None,
            joiner: "  ",
        }
    }

    pub(super) const fn count(self, count: usize) -> Self {
        Tab {
            count: Some(count),
            ..self
        }
    }

    /// Set the text between the previous tab and this one: ` → ` for a
    /// tab a request passes through after it, ` · ` for one that sits
    /// apart.
    pub(super) const fn after(self, joiner: &'a str) -> Self {
        Tab { joiner, ..self }
    }
}

/// How much of the tabs other than the current one the strip shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Others {
    /// Label and count.
    Counted,
    /// Label alone.
    Named,
    /// Number alone.
    Numbered,
}

type Spans = Vec<(Span<'static>, Option<usize>)>;

/// The strip showing tabs `lo..=hi`, with a `‹` or `›` where tabs lie
/// beyond them.
fn strip(tabs: &[Tab], current: usize, lo: usize, hi: usize, others: Others) -> Spans {
    let dim = Style::new().dim();
    let mut spans: Spans = vec![(Span::raw(" "), None)];
    if lo > 0 {
        spans.push((Span::styled("‹ ", dim), None));
    }
    for (index, tab) in tabs.iter().enumerate().take(hi + 1).skip(lo) {
        if index > lo {
            spans.push((Span::styled(tab.joiner.to_owned(), dim), None));
        }
        let counted = |text: String| match tab.count {
            Some(count) => format!("{text} {count}"),
            None => text,
        };
        let named = format!("{} {}", index + 1, tab.label);
        let span = if index == current {
            Span::styled(counted(named), Style::new().bold().reversed())
        } else {
            let text = match others {
                Others::Counted => counted(named),
                Others::Named => named,
                Others::Numbered => format!("{}", index + 1),
            };
            Span::styled(text, dim)
        };
        spans.push((span, Some(index)));
    }
    if hi + 1 < tabs.len() {
        spans.push((Span::styled(" ›", dim), None));
    }
    spans
}

fn spans_width(spans: &Spans) -> usize {
    spans.iter().map(|(span, _)| span.width()).sum()
}

/// The tab strip's spans, from widest to narrowest: every tab in full;
/// the other tabs without their counts; a window of tabs around the
/// current one, as many as fit and at least three, marked where tabs are
/// hidden; every tab as its number but the current; and the current tab
/// alone, clipped to what is left. The current tab keeps its count
/// throughout.
///
/// A window of two reads as a pair rather than a place in a sequence, so
/// the numbers, which show every tab, are preferred to it.
fn tab_spans(tabs: &[Tab], current: usize, width: usize) -> Spans {
    let last = tabs.len().saturating_sub(1);
    for others in [Others::Counted, Others::Named] {
        let spans = strip(tabs, current, 0, last, others);
        if spans_width(&spans) <= width {
            return spans;
        }
    }
    let (mut lo, mut hi) = (current, current);
    loop {
        let mut grew = false;
        for left in [true, false] {
            let (next_lo, next_hi) = match left {
                true if lo > 0 => (lo - 1, hi),
                false if hi < last => (lo, hi + 1),
                _ => continue,
            };
            if spans_width(&strip(tabs, current, next_lo, next_hi, Others::Named)) <= width {
                (lo, hi) = (next_lo, next_hi);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    if hi - lo + 1 >= tabs.len().min(3) {
        return strip(tabs, current, lo, hi, Others::Named);
    }
    let numbers = strip(tabs, current, 0, last, Others::Numbered);
    if spans_width(&numbers) <= width {
        return numbers;
    }
    let mut alone = strip(tabs, current, current, current, Others::Named);
    let fixed: usize = alone
        .iter()
        .filter(|(_, tab)| *tab != Some(current))
        .map(|(span, _)| span.width())
        .sum();
    if let Some((span, _)) = alone.iter_mut().find(|(_, tab)| *tab == Some(current)) {
        span.content = clip_ellipsis(&span.content, width.saturating_sub(fixed)).into();
    }
    alone
}

/// The narrowest the tab strip goes before its current label clips: the
/// current tab alone, with the markers for what is hidden.
pub(super) fn tabs_min_width(tabs: &[Tab], current: usize) -> u16 {
    let spans = strip(tabs, current, current, current, Others::Named);
    u16::try_from(spans_width(&spans)).unwrap_or(u16::MAX)
}

/// Draw the tab strip into `area` and return each tab's rectangle, for
/// the mouse, indexed like `tabs`. A tab scrolled out of the strip gets
/// an empty one, which nothing is inside of.
pub(super) fn tabs(frame: &mut Frame, area: Rect, tabs: &[Tab], current: usize) -> Vec<Rect> {
    let spans = tab_spans(tabs, current, usize::from(area.width));
    let mut hits = vec![Rect::default(); tabs.len()];
    let mut x = area.x;
    for (span, tab) in &spans {
        let width = span.width() as u16;
        if let Some(index) = tab {
            hits[*index] = Rect::new(x, area.y, width, area.height.min(1)).intersection(area);
        }
        x = x.saturating_add(width);
    }
    frame.render_widget(
        Line::from(spans.into_iter().map(|(span, _)| span).collect::<Vec<_>>()),
        area,
    );
    hits
}

/// A status at the right of the header, after the tabs have had their
/// narrowest width: the tabs collapse before the status clips.
pub(super) fn status(frame: &mut Frame, area: Rect, tabs_min: u16, text: &str) -> Rect {
    if text.is_empty() {
        return area;
    }
    let room = area.width.saturating_sub(tabs_min + 1);
    let width = (text.width() as u16 + 1).min(room);
    if width < 2 {
        return area;
    }
    let right = Rect {
        x: area.right() - width,
        width,
        ..area
    };
    frame.render_widget(
        Paragraph::new(clip_ellipsis(text, usize::from(width - 1)))
            .dim()
            .alignment(Alignment::Right),
        right,
    );
    Rect {
        width: area.width - width,
        ..area
    }
}

/// What a page puts in its header row after the page buttons: its tabs,
/// which tab is current, and a status at the right. A page with neither
/// leaves the default.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Strip<'a> {
    pub(super) tabs: &'a [Tab<'a>],
    pub(super) current: usize,
    pub(super) status: &'a str,
}

/// A page laid out: where its header's buttons and tabs are, and the body
/// under them.
#[derive(Debug, Clone)]
pub(super) struct Laid {
    pub(super) header: Header,
    /// Each tab's rectangle, indexed like `Strip::tabs`; empty for one
    /// scrolled out of the strip.
    pub(super) tabs: Vec<Rect>,
    pub(super) body: Rect,
}

/// Lay out a page other than the dashboard: the header row with its
/// buttons, tabs and status, then the page's message under it for as many
/// rows as it takes, then the body. Every such page goes through here, so
/// they share one header. The buttons are left to [`chips`], drawn once the
/// body is down.
pub(super) fn page(frame: &mut Frame, current: Page, message: Option<&str>, strip: &Strip) -> Laid {
    let area = frame.area();
    let header = header(
        Rect {
            height: area.height.min(1),
            ..area
        },
        current,
    );
    let tabs_min = tabs_min_width(strip.tabs, strip.current);
    let tabs_area = status(frame, header.rest, tabs_min, strip.status);
    let tabs = tabs(frame, tabs_area, strip.tabs, strip.current);
    let below = area.height - header.keys.height;
    let lines = message_lines(message, area.width.saturating_sub(2));
    // The body keeps a row: a message never takes the whole page.
    let rows = (lines.len() as u16).min(below.saturating_sub(1));
    for (row, line) in lines.into_iter().take(usize::from(rows)).enumerate() {
        frame.render_widget(
            line,
            Rect::new(
                area.x + 1.min(area.width),
                area.y + header.keys.height + row as u16,
                area.width.saturating_sub(2),
                1,
            ),
        );
    }
    let body = Rect {
        y: area.y + header.keys.height + rows,
        height: below - rows,
        ..area
    };
    Laid { header, tabs, body }
}

/// A page's message: what went wrong with the last thing it tried,
/// wrapped across as many rows as `width` needs. Empty without one.
pub(super) fn message_lines(message: Option<&str>, width: u16) -> Vec<Line<'static>> {
    let Some(message) = message else {
        return Vec::new();
    };
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in message.split_whitespace() {
        if !line.is_empty() && line.width() + 1 + word.width() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines
        .into_iter()
        .map(|text| Line::styled(clip_ellipsis(&text, width), Style::new().fg(Color::Red)))
        .collect()
}

/// Which of a list's `total` entries show in `room` rows, scrolled
/// `offset` entries down: all of them when they fit; one entry alone at
/// one row (a row is better spent on an entry than on a count of none
/// shown); otherwise the entries from `offset`, under an "above" count
/// once scrolled and over a "more" count while any are left below. The
/// offset is clamped so the last entry, once reached, sits on the last
/// row. At two rows a scrolled list in the middle drops the "above"
/// count: an entry and the "more" line are worth more than two counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ListWindow {
    /// The furthest offset that still changes what shows.
    pub(super) max_offset: usize,
    /// The count line over the entries, when it renders.
    pub(super) above: Option<usize>,
    /// The entries that render.
    pub(super) shown: std::ops::Range<usize>,
    /// The count line under the entries, when it renders.
    pub(super) below: Option<usize>,
}

pub(super) fn list_window(total: usize, room: usize, offset: usize) -> ListWindow {
    if total <= room || room == 0 {
        return ListWindow {
            max_offset: 0,
            above: None,
            shown: 0..total.min(room),
            below: None,
        };
    }
    if room == 1 {
        let offset = offset.min(total - 1);
        return ListWindow {
            max_offset: total - 1,
            above: None,
            shown: offset..offset + 1,
            below: None,
        };
    }
    let max_offset = total - (room - 1);
    let offset = offset.min(max_offset);
    let above = (offset > 0 && (room >= 3 || offset == max_offset)).then_some(offset);
    let rows = room - usize::from(above.is_some());
    if offset + rows >= total {
        return ListWindow {
            max_offset,
            above,
            shown: offset..total,
            below: None,
        };
    }
    let end = offset + rows - 1;
    ListWindow {
        max_offset,
        above,
        shown: offset..end,
        below: Some(total - end),
    }
}

/// The window that shows `selected`: the nearest the top it can sit.
/// Computed from the selection alone, so a page holds no scroll offset
/// and the frame stays a function of its state.
pub(super) fn selection_window(total: usize, room: usize, selected: usize) -> ListWindow {
    let selected = selected.min(total.saturating_sub(1));
    let last = list_window(total, room, usize::MAX).max_offset;
    (0..=last)
        .map(|offset| list_window(total, room, offset))
        .find(|window| window.shown.contains(&selected))
        .unwrap_or_else(|| list_window(total, room, selected))
}

/// One clickable row a list drew: where it landed and the entry it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RowHit {
    pub(super) area: Rect,
    pub(super) entry: usize,
}

/// The entry whose row holds `at`, if any.
pub(super) fn entry_at(hits: &[RowHit], at: ratatui::layout::Position) -> Option<usize> {
    hits.iter()
        .find(|hit| hit.area.contains(at))
        .map(|hit| hit.entry)
}

/// What a list calls its entries in its counts: `… 3 more providers`.
#[derive(Debug, Clone, Copy)]
pub(super) struct Noun {
    pub(super) one: &'static str,
    pub(super) many: &'static str,
}

impl Noun {
    pub(super) fn of(self, count: usize) -> &'static str {
        if count == 1 { self.one } else { self.many }
    }
}

/// Draw a selectable list of entries into `area`, each entry its head
/// line and any further lines under it, and return where each row landed.
///
/// Every entry shows in full while they all fit. Otherwise the entries
/// other than the selected one shed to their head line, and the list
/// windows around the selection with a count of what it left out on each
/// side (spec: UTUI#what-is-shed-first). The selected entry's head is
/// reversed across the whole row, and every line clips with `…` rather
/// than at the edge.
pub(super) fn list(
    frame: &mut Frame,
    area: Rect,
    entries: Vec<Vec<Line<'static>>>,
    selected: Option<usize>,
    noun: Noun,
) -> Vec<RowHit> {
    let room = usize::from(area.height);
    let width = usize::from(area.width);
    let full: usize = entries.iter().map(Vec::len).sum();
    let mut rows: Vec<(Line<'static>, Option<usize>)> = Vec::new();
    let push = |rows: &mut Vec<_>, index: usize, entry: &[Line<'static>], whole: bool| {
        for (line_no, line) in entry.iter().enumerate() {
            if line_no > 0 && !whole {
                break;
            }
            let mut line = clip_line(line.clone(), width);
            if line_no == 0 && selected == Some(index) {
                let pad = width.saturating_sub(line.width());
                line.spans.push(Span::raw(" ".repeat(pad)));
                for span in &mut line.spans {
                    span.style = span.style.add_modifier(Modifier::REVERSED);
                }
            }
            rows.push((line, Some(index)));
        }
    };
    if full <= room {
        for (index, entry) in entries.iter().enumerate() {
            push(&mut rows, index, entry, true);
        }
    } else {
        // The selected entry keeps its own lines; the others share what
        // is left a row each.
        let extra = selected
            .and_then(|index| entries.get(index))
            .map_or(0, |entry| entry.len().saturating_sub(1));
        let heads = room.saturating_sub(extra).max(1);
        let window = selection_window(entries.len(), heads, selected.unwrap_or(0));
        let dim = Style::new().dim();
        if let Some(above) = window.above {
            rows.push((
                Line::styled(format!("… {above} {} above", noun.of(above)), dim),
                None,
            ));
        }
        for index in window.shown.clone() {
            push(&mut rows, index, &entries[index], selected == Some(index));
        }
        if let Some(below) = window.below {
            rows.push((
                Line::styled(format!("… {below} more {}", noun.of(below)), dim),
                None,
            ));
        }
    }
    let mut hits = Vec::new();
    for (row, (line, entry)) in rows.into_iter().take(room).enumerate() {
        let at = Rect {
            y: area.y + row as u16,
            height: 1,
            ..area
        };
        frame.render_widget(line, at);
        if let Some(entry) = entry {
            hits.push(RowHit { area: at, entry });
        }
    }
    hits
}

/// A section of the key overlay: a heading, where it has one, over rows
/// of a key or mark and what it does or means.
pub(super) struct Section<'a> {
    pub(super) heading: Option<&'a str>,
    pub(super) rows: &'a [(&'a str, &'a str)],
}

/// The `?` overlay: each page's own keys, and on the dashboard the marks
/// it renders as well, centred over the frame. How to close it rides in
/// the bottom border, so it shows at any size the border does; rows that
/// do not fit are counted rather than cut at the edge.
pub(super) fn overlay(frame: &mut Frame, title: &str, sections: &[Section]) {
    overlay_in(frame, frame.area(), title, &[sections], false);
}

/// One column of an overlay: its sections' lines, the key column set to the
/// widest key, and a blank line between sections where `spaced`.
fn overlay_lines(sections: &[Section], spaced: bool) -> Vec<Line<'static>> {
    let key_w = sections
        .iter()
        .flat_map(|section| section.rows.iter())
        .map(|(key, _)| key.width())
        .max()
        .unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();
    for (at, section) in sections.iter().enumerate() {
        if spaced && at > 0 {
            lines.push(Line::raw(""));
        }
        if let Some(heading) = section.heading {
            lines.push(Line::styled(heading.to_owned(), Style::new().bold()));
        }
        lines.extend(section.rows.iter().map(|(key, meaning)| {
            Line::from(vec![
                Span::styled(format!("{key:<key_w$}"), Style::new().bold()),
                Span::raw("  "),
                Span::raw(meaning.to_string()),
            ])
        }));
    }
    lines
}

/// An overlay in `area`: columns side by side where `area` is wide enough
/// for them, one over the next where it is not. Under a page's header, an
/// `area` that starts below it leaves the header's buttons showing.
pub(super) fn overlay_in(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    columns: &[&[Section]],
    spaced: bool,
) {
    let built: Vec<Vec<Line>> = columns
        .iter()
        .map(|sections| overlay_lines(sections, spaced))
        .collect();
    let widest = |lines: &[Line]| lines.iter().map(Line::width).max().unwrap_or(0);
    let close = hints(&[("? or esc", "close")]);
    let floor = close.width().max(title.width());
    const GAP: usize = 3;
    let side_by_side = built.iter().map(|lines| widest(lines)).sum::<usize>()
        + GAP * built.len().saturating_sub(1)
        + 4
        <= usize::from(area.width);
    let mut lines: Vec<Line> = if side_by_side {
        let widths: Vec<usize> = built.iter().map(|lines| widest(lines)).collect();
        let rows = built.iter().map(Vec::len).max().unwrap_or(0);
        (0..rows)
            .map(|row| {
                let mut spans: Vec<Span> = Vec::new();
                for (column, lines) in built.iter().enumerate() {
                    let used = lines.get(row).map_or(0, Line::width);
                    if let Some(line) = lines.get(row) {
                        spans.extend(line.spans.iter().cloned());
                    }
                    if column + 1 < built.len() {
                        spans.push(Span::raw(" ".repeat(widths[column] - used + GAP)));
                    }
                }
                Line::from(spans)
            })
            .collect()
    } else {
        let mut stacked: Vec<Line> = Vec::new();
        for (at, lines) in built.into_iter().enumerate() {
            if at > 0 && spaced {
                stacked.push(Line::raw(""));
            }
            stacked.extend(lines);
        }
        stacked
    };
    let text_w = lines
        .iter()
        .map(Line::width)
        .chain([floor])
        .max()
        .unwrap_or(0) as u16;
    let width = text_w.saturating_add(4).min(area.width);
    let height = (lines.len() as u16).saturating_add(2).min(area.height);
    let room = usize::from(height.saturating_sub(2));
    if lines.len() > room && room > 0 {
        let hidden = lines.len() - (room - 1);
        lines.truncate(room - 1);
        lines.push(Line::styled(format!("… {hidden} more"), Style::new().dim()));
    }
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title_top(title.to_owned())
        .title_bottom(Line::styled(close, Style::new().dim()))
        .padding(Padding::horizontal(1));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Key hints on one line, the way every prompt writes them: a key, the
/// bare verb, and `·` between them — `y confirm · esc cancel`.
pub(super) fn hints(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, verb)| format!("{key} {verb}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Whether `NO_COLOR`'s value asks for no colour: set and non-empty, the
/// convention's own rule (an empty value is the same as unset).
pub(crate) fn no_color(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// Every colour off the finished frame, modifiers kept: `NO_COLOR` is
/// about colour, and bold and dim still carry meaning without it (a dim
/// `-`, an idle age). Done once over the buffer of whichever page drew,
/// rather than at each style site, so no panel and no page can forget it
/// (spec: UTUI#distinguishing-a-reading).
pub(super) fn strip_colour(frame: &mut Frame) {
    for cell in frame.buffer_mut().content.iter_mut() {
        cell.fg = Color::Reset;
        cell.bg = Color::Reset;
    }
}

/// `text` cut to at most `width` cells, whole characters only — a wide
/// character that would straddle the edge goes rather than half-renders.
pub(super) fn clip(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        out.push(ch);
    }
    out
}

/// `text` cut to `width` cells with an ellipsis when anything had to go.
pub(super) fn clip_ellipsis(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", clip(text, width - 1))
}

/// `spans` cut to `width` cells with an ellipsis when anything had to
/// go, each surviving span keeping its style — the directory stays
/// cyan up to the cut.
pub(super) fn clip_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= width {
        return spans;
    }
    let mut room = width.saturating_sub(1); // the ellipsis
    let mut out = Vec::with_capacity(spans.len() + 1);
    let mut style = Style::new();
    for span in spans {
        if room == 0 {
            break;
        }
        let text = clip(&span.content, room);
        room -= text.width();
        style = span.style;
        out.push(Span::styled(text, span.style));
    }
    if width > 0 {
        out.push(Span::styled("…", style));
    }
    out
}

/// A line clipped like [`clip_spans`], keeping its own style.
pub(super) fn clip_line(line: Line<'static>, width: usize) -> Line<'static> {
    let style = line.style;
    Line::from(clip_spans(line.spans, width)).style(style)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_window_counts_what_it_scrolled_past() {
        let window = |above, shown, below, max_offset| ListWindow {
            max_offset,
            above,
            shown,
            below,
        };
        // Fits: everything, nothing to scroll.
        assert_eq!(list_window(2, 3, 5), window(None, 0..2, None, 0));
        // Cut, at the top: the "more" count closes it.
        assert_eq!(list_window(5, 3, 0), window(None, 0..2, Some(3), 3));
        // Scrolled into the middle: a count on each side.
        assert_eq!(list_window(5, 3, 1), window(Some(1), 1..2, Some(3), 3));
        // At the end, and past it: the last entry on the last row.
        assert_eq!(list_window(5, 3, 3), window(Some(3), 3..5, None, 3));
        assert_eq!(list_window(5, 3, 9), window(Some(3), 3..5, None, 3));
        // One row: an entry, not a count of none shown.
        assert_eq!(list_window(5, 1, 0), window(None, 0..1, None, 4));
        assert_eq!(list_window(5, 1, 2), window(None, 2..3, None, 4));
        // Two rows in the middle: the entry and the "more" line.
        assert_eq!(list_window(5, 2, 2), window(None, 2..3, Some(2), 4));
        // No rows: nothing, and nothing to scroll.
        assert_eq!(list_window(5, 0, 2), window(None, 0..0, None, 0));
    }

    #[test]
    fn a_selection_window_always_shows_the_selection() {
        for total in 0..12 {
            for room in 1..8 {
                for selected in 0..total {
                    let window = selection_window(total, room, selected);
                    assert!(
                        window.shown.contains(&selected),
                        "{selected} of {total} in {room} rows: {window:?}"
                    );
                }
            }
        }
        // As near the top as it can sit: nothing scrolled while the
        // selection is on the first screen.
        assert_eq!(selection_window(10, 4, 2).shown, 0..3);
    }

    fn strip_text(tabs: &[Tab], current: usize, width: usize) -> String {
        tab_spans(tabs, current, width)
            .into_iter()
            .map(|(span, _)| span.content.into_owned())
            .collect()
    }

    /// The routing page's strip, as the design draws it.
    fn lookup_tabs() -> [Tab<'static>; 6] {
        [
            Tab::new("presentations").count(15),
            Tab::new("rewrites").count(14).after(" → "),
            Tab::new("defaults").count(4).after(" → "),
            Tab::new("routes").count(7).after(" → "),
            Tab::new("models").count(11).after(" · "),
            Tab::new("history").count(9).after(" · "),
        ]
    }

    #[test]
    fn the_tab_strip_drops_counts_then_scrolls_then_numbers_then_stands_alone() {
        let tabs = lookup_tabs();
        assert_eq!(
            strip_text(&tabs, 1, 120),
            " 1 presentations 15 → 2 rewrites 14 → 3 defaults 4 → 4 routes 7 · 5 models 11 · 6 history 9"
        );
        assert_eq!(
            strip_text(&tabs, 1, 90),
            " 1 presentations → 2 rewrites 14 → 3 defaults → 4 routes · 5 models · 6 history"
        );
        // A window of neighbours around the current tab, marked where
        // tabs are hidden.
        assert_eq!(
            strip_text(&tabs, 1, 60),
            " 1 presentations → 2 rewrites 14 → 3 defaults → 4 routes ›"
        );
        assert_eq!(
            strip_text(&tabs, 3, 41),
            " ‹ 3 defaults → 4 routes 7 · 5 models ›"
        );
        // A window of two would not fit, but every tab's number does.
        let wide = [
            Tab::new("presentations"),
            Tab::new("x"),
            Tab::new("presentations"),
        ];
        assert_eq!(strip_text(&wide, 1, 10), " 1  2 x  3");
        // Neither fits: the current tab alone.
        assert_eq!(strip_text(&wide, 1, 9), " ‹ 2 x ›");
        assert_eq!(strip_text(&tabs, 1, 21), " ‹ 2 rewrites 14 ›");
        assert_eq!(strip_text(&tabs, 0, 21), " 1 presentations 15 ›");
        assert_eq!(tabs_min_width(&tabs, 1), 18);
        assert_eq!(strip_text(&tabs, 1, 12), " ‹ 2 rewr… ›");
    }

    /// The routing page's header at `width` columns: the page buttons and
    /// tab strip on the first row, the status on the right, as text rows,
    /// with the buffer for styles.
    fn header_frame(
        width: u16,
        current: Page,
        strip: &Strip,
    ) -> (String, ratatui::buffer::Buffer, Laid) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 2)).expect("terminal");
        let mut laid = None;
        terminal
            .draw(|frame| {
                let drawn = page(frame, current, None, strip);
                chips(frame, &drawn.header);
                laid = Some(drawn);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let text = (0..width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned();
        (text, buffer, laid.expect("drawn"))
    }

    fn routing_strip<'a>(tabs: &'a [Tab<'a>], current: usize) -> Strip<'a> {
        Strip {
            tabs,
            current,
            status: "rev 9 live",
        }
    }

    // The header at the three widths the design draws it at: whole at
    // 120, scrolled to a window of the tabs at 100, and the current tab
    // alone at 80, with the buttons whole throughout.
    #[test]
    fn the_header_at_120_columns() {
        let tabs = lookup_tabs();
        let (text, ..) = header_frame(120, Page::Routing, &routing_strip(&tabs, 1));
        insta::assert_snapshot!("header_120", text);
    }

    #[test]
    fn the_header_at_100_columns() {
        let tabs = lookup_tabs();
        let (text, ..) = header_frame(100, Page::Routing, &routing_strip(&tabs, 3));
        insta::assert_snapshot!("header_100", text);
    }

    #[test]
    fn the_header_at_80_columns() {
        let tabs = lookup_tabs();
        let (text, ..) = header_frame(80, Page::Routing, &routing_strip(&tabs, 1));
        insta::assert_snapshot!("header_80", text);
    }

    #[test]
    fn a_page_with_no_tabs_has_the_buttons_alone() {
        let (text, _, laid) = header_frame(80, Page::Accounts, &Strip::default());
        insta::assert_snapshot!("header_accounts_80", text);
        assert!(laid.tabs.is_empty());
    }

    #[test]
    fn the_current_button_is_bold_blue_and_the_others_dark_grey() {
        let tabs = lookup_tabs();
        let (_, buffer, laid) = header_frame(100, Page::Config, &routing_strip(&tabs, 0));
        let style_at = |button: Rect| {
            let cell = &buffer[(button.x + 1, button.y)];
            (cell.fg, cell.modifier.contains(Modifier::BOLD))
        };
        for (button, page) in laid.header.pages.iter().zip(Page::ALL) {
            let (fg, bold) = style_at(*button);
            if page == Page::Config {
                assert_eq!((fg, bold), (Color::Blue, true), "{page:?}");
            } else {
                assert_eq!((fg, bold), (Color::DarkGray, false), "{page:?}");
            }
        }
        // A cell between two buttons is bare.
        assert_eq!(buffer[(laid.header.pages[0].right(), 0)].symbol(), " ");
    }

    #[test]
    fn a_window_of_two_gives_way_to_the_numbers() {
        let tabs = lookup_tabs();
        // The first two tabs would fit, but the numbers show all six.
        assert_eq!(
            strip_text(&tabs, 1, 35),
            " 1 → 2 rewrites 14 → 3 → 4 · 5 · 6"
        );
    }

    #[test]
    fn a_scrolled_tab_has_no_click_target() {
        let tabs = lookup_tabs();
        let backend = ratatui::backend::TestBackend::new(30, 1);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        let mut hits = Vec::new();
        terminal
            .draw(|frame| hits = super::tabs(frame, frame.area(), &tabs, 1))
            .expect("draw");
        assert_eq!(hits.len(), 6);
        assert_eq!(hits[1].width, "2 rewrites 14".width() as u16);
        assert!(hits[0].is_empty() && hits[2].is_empty());
    }

    #[test]
    fn the_buttons_never_shorten_and_clip_only_at_the_edge() {
        let header = |width| header(Rect::new(0, 0, width, 24), Page::Config);
        let wide = header(100);
        assert_eq!(wide.keys, Rect::new(0, 0, 3, 1));
        assert_eq!(
            wide.pages,
            [
                Rect::new(4, 0, 11, 1),
                Rect::new(16, 0, 9, 1),
                Rect::new(26, 0, 8, 1),
                Rect::new(35, 0, 10, 1),
            ]
        );
        assert_eq!(wide.rest, Rect::new(46, 0, 54, 24));
        assert_eq!(wide.page_at(Position::new(20, 0)), Some(Page::Routing));
        assert_eq!(wide.page_at(Position::new(15, 0)), None, "the gap");
        // The narrowest the design draws: the strip has nothing left.
        assert_eq!(header(46).rest.width, 0);
        // Narrower than the buttons, the later ones clip and then go.
        let tiny = header(20);
        assert_eq!(tiny.pages[1], Rect::new(16, 0, 4, 1));
        assert_eq!(tiny.pages[2].width, 0);
        assert_eq!(tiny.page_at(Position::new(22, 0)), None);
        assert_eq!(header(2).keys.width, 2);
    }

    #[test]
    fn hints_read_as_bare_verbs_between_dots() {
        assert_eq!(
            hints(&[("y", "confirm"), ("esc", "cancel")]),
            "y confirm · esc cancel"
        );
    }

    #[test]
    fn a_message_wraps_rather_than_clipping() {
        let lines = message_lines(Some("the service refused the value it was sent"), 16);
        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(text, ["the service", "refused the", "value it was", "sent"]);
        assert!(message_lines(None, 16).is_empty());
    }
}
