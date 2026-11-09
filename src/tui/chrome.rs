//! The frame every page draws in: the header's chips, the tab strip,
//! panel framing, list windows, the key overlay and the colour switch.
//!
//! The pages grew separately and disagreed on all of these, which an
//! operator reads as one product. Drawing them in one place is what keeps
//! them agreeing (spec: UTUI#rendering).

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Clear, Padding, Paragraph},
};
use unicode_width::UnicodeWidthStr;

/// The `?` chip's width: a key between two spaces.
const CHIP_WIDTH: u16 = 3;

/// Below this width the header needs every cell, so the `⇄` chip goes and
/// `Tab` is the way across.
const SWITCH_MIN_WIDTH: u16 = 80;

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

/// Where the header's chips landed, and the rest of the row for the
/// page's own tabs or status.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    /// The `?` chip, which opens the page's overlay.
    pub(crate) keys: Rect,
    /// The `⇄` chip, which goes to the next page. Zero-wide when the
    /// header is too narrow to spare it.
    pub(crate) switch: Rect,
    /// What is left of the row.
    pub(crate) rest: Rect,
}

/// Lay out the header row: `?` at the far left, `⇄` in the one-cell
/// gutter after it, and the rest after that.
pub(super) fn header(area: Rect) -> Header {
    let height = area.height.min(1);
    let keys_width = area.width.min(CHIP_WIDTH);
    let keys = Rect::new(area.x, area.y, keys_width, height);
    let switch_width = u16::from(area.width >= SWITCH_MIN_WIDTH);
    let switch = Rect::new(area.x + keys_width, area.y, switch_width, height);
    let rest_x = area.x + keys_width + u16::from(area.width > keys_width);
    let rest = Rect::new(
        rest_x,
        area.y,
        area.width.saturating_sub(rest_x - area.x),
        area.height,
    );
    Header { keys, switch, rest }
}

/// Draw the chips. Drawn last on a page, so a panel that had to share the
/// header's row cannot paint over the way out to the overlay.
pub(super) fn chips(frame: &mut Frame, header: &Header) {
    let label = if header.keys.width >= CHIP_WIDTH {
        " ? "
    } else {
        "?"
    };
    frame.render_widget(
        Paragraph::new(label).style(
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::REVERSED),
        ),
        header.keys,
    );
    if header.switch.width > 0 {
        frame.render_widget(
            Paragraph::new("⇄").style(
                Style::new()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::REVERSED),
            ),
            header.switch,
        );
    }
}

/// The tab strip's gap between tabs.
const TAB_GAP: &str = "  ";

/// The tab strip's lines, from widest to narrowest: every label in full,
/// then the other tabs down to their number, then the current label
/// clipped to what is left.
fn tab_spans(labels: &[&str], current: usize, width: usize) -> Vec<(Span<'static>, Option<usize>)> {
    let tab = |index: usize, full: bool| -> String {
        if full {
            format!("{} {}", index + 1, labels[index])
        } else {
            format!("{}", index + 1)
        }
    };
    let build = |others_full: bool, current_text: String| {
        let mut spans: Vec<(Span<'static>, Option<usize>)> = vec![(Span::raw(" "), None)];
        for index in 0..labels.len() {
            if index > 0 {
                spans.push((Span::raw(TAB_GAP), None));
            }
            let span = if index == current {
                Span::styled(current_text.clone(), Style::new().bold().reversed())
            } else {
                Span::styled(tab(index, others_full), Style::new().dim())
            };
            spans.push((span, Some(index)));
        }
        spans
    };
    let fits = |spans: &[(Span, Option<usize>)]| {
        spans.iter().map(|(span, _)| span.width()).sum::<usize>() <= width
    };
    let full = build(true, tab(current, true));
    if fits(&full) {
        return full;
    }
    let collapsed = build(false, tab(current, true));
    if fits(&collapsed) {
        return collapsed;
    }
    let others: usize = build(false, String::new())
        .iter()
        .map(|(span, _)| span.width())
        .sum();
    build(
        false,
        clip_ellipsis(&tab(current, true), width.saturating_sub(others)),
    )
}

/// The narrowest the tab strip goes before its current label clips: every
/// other tab down to its number.
pub(super) fn tabs_min_width(labels: &[&str], current: usize) -> u16 {
    tab_spans(labels, current, 0)
        .iter()
        .filter(|(_, tab)| *tab != Some(current))
        .map(|(span, _)| span.width())
        .sum::<usize>()
        .saturating_add(format!("{} {}", current + 1, labels[current]).width()) as u16
}

/// Draw the tab strip into `area` and return each tab's rectangle, for
/// the mouse.
pub(super) fn tabs(frame: &mut Frame, area: Rect, labels: &[&str], current: usize) -> Vec<Rect> {
    let spans = tab_spans(labels, current, usize::from(area.width));
    let mut hits = Vec::new();
    let mut x = area.x;
    for (span, tab) in &spans {
        let width = span.width() as u16;
        if tab.is_some() {
            hits.push(Rect::new(x, area.y, width, area.height.min(1)).intersection(area));
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

/// Lay out a page other than the dashboard: the header row, then the
/// page's message under it for as many rows as it takes, then the body.
/// The chips are left to [`chips`], drawn once the body is down.
pub(super) fn page(frame: &mut Frame, message: Option<&str>) -> (Header, Rect) {
    let area = frame.area();
    let header = header(Rect {
        height: area.height.min(1),
        ..area
    });
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
    (header, body)
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
    fn of(self, count: usize) -> &'static str {
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
/// it renders as well, centred over the frame and clipped to it.
pub(super) fn overlay(frame: &mut Frame, title: &str, sections: &[Section]) {
    let key_w = sections
        .iter()
        .flat_map(|section| section.rows.iter())
        .map(|(key, _)| key.width())
        .max()
        .unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();
    for section in sections {
        if let Some(heading) = section.heading {
            if !lines.is_empty() {
                lines.push(Line::raw(""));
            }
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
    lines.push(Line::styled(hints(&[("? or esc", "close")]), Style::new().dim()));
    let text_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let area = frame.area();
    let width = text_w.saturating_add(4).min(area.width);
    let height = (lines.len() as u16).saturating_add(2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title_top(title.to_owned())
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

    #[test]
    fn the_tab_strip_collapses_the_other_tabs_before_clipping_the_current_one() {
        let labels = ["presentations", "routes", "rewrites"];
        let text = |width| {
            tab_spans(&labels, 1, width)
                .into_iter()
                .map(|(span, _)| span.content.into_owned())
                .collect::<String>()
        };
        assert_eq!(text(80), " 1 presentations  2 routes  3 rewrites");
        assert_eq!(text(20), " 1  2 routes  3");
        assert_eq!(text(12), " 1  2 ro…  3");
        assert_eq!(tabs_min_width(&labels, 1), " 1  2 routes  3".width() as u16);
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
        assert_eq!(
            text,
            ["the service", "refused the", "value it was", "sent"]
        );
        assert!(message_lines(None, 16).is_empty());
    }
}
