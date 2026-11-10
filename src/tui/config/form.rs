//! Drawing the form that opens under a row: its field, with what the check
//! says beside it, and the save control under it
//! (spec: UTUI#the-configuration-pages).

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::model::{Editing, Snapshot, Verdict};
use crate::tui::table::{BodyLine, span, wrap};
use crate::tui::tone::{Tone, dim, toned};

/// The cells the field's name takes, so values line up across forms.
const NAME: usize = 16;
/// The least a value cell shows, so a short entry still reads as a field.
const VALUE: usize = 12;

/// The end of `text` that fits `cells`, with a leading `…` where the start was
/// cut.
fn tail_in(text: &str, cells: usize) -> String {
    if text.width() <= cells {
        return text.to_owned();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 1;
    for glyph in text.chars().rev() {
        let wide = glyph.width().unwrap_or(0);
        if used + wide > cells {
            break;
        }
        kept.push(glyph);
        used += wide;
    }
    kept.push('…');
    kept.iter().rev().collect()
}

/// The form's lines, to sit under the row it edits. `prefix` is what the
/// page puts before every line of one: its indent, and its rail where it has
/// one. `width` is the cells the table's lines are laid to.
pub(super) fn lines(
    editing: &Editing,
    snapshot: &Snapshot,
    prefix: Vec<Span<'static>>,
    width: usize,
) -> Vec<BodyLine> {
    let prefix_width: usize = prefix.iter().map(Span::width).sum();
    let shown = if editing.secret {
        // A credential is never echoed, here or anywhere
        // (spec: SEC#a-credential-is-never-logged-or-displayed).
        "·".repeat(editing.buffer.chars().count())
    } else {
        editing.buffer.clone()
    };
    // The value is the tail of what was typed where it is longer than the
    // cell: the end is where the typing is, and a leading `…` says the start
    // is cut. Widths are in cells, as a wide character takes two.
    let room = width.saturating_sub(prefix_width + 2 + NAME);
    let cell = (shown.width() + 2).max(VALUE).min((room / 2).max(VALUE));
    let tail = tail_in(&shown, cell.saturating_sub(1));
    let typed = format!("{tail}▏");
    let padding = cell.saturating_sub(typed.width());
    let reversed = Style::new().add_modifier(Modifier::REVERSED);

    // What sits beside the field: the service's refusal of this very value,
    // else the check's, else what the value will do.
    let verdict = editing.verdict(snapshot);
    let (tone, words) = match (&editing.refusal, verdict) {
        (Some(refused), _) => (Tone::Bad, Some(refused.clone())),
        (None, Verdict::Refused(refused)) => (Tone::Bad, Some(refused)),
        (None, Verdict::Accepted(reading)) => (Tone::Plain, reading),
    };
    let beside = room.saturating_sub(cell + 2).max(12);
    let mut notes = words
        .map(|words| wrap(&words, beside))
        .unwrap_or_default()
        .into_iter();

    let with_prefix = |spans: Vec<Span<'static>>| {
        let mut line = prefix.clone();
        line.extend(spans);
        Line::from(line)
    };
    let note_spans = |note: Option<String>| -> Vec<Span<'static>> {
        match note {
            Some(note) if tone == Tone::Bad => vec![
                Span::raw("  "),
                Span::styled(note, Style::new().fg(ratatui::style::Color::Red).bold()),
            ],
            Some(note) => vec![Span::raw("  "), toned(note, tone)],
            None => Vec::new(),
        }
    };

    let mut first = vec![
        span("┌ ", dim()),
        span(format!("{:<NAME$}", editing.target.field()), dim()),
        span(format!("{typed}{}", " ".repeat(padding)), reversed),
    ];
    first.extend(note_spans(notes.next()));
    let mut out = vec![BodyLine::form(with_prefix(first), None, false)];
    // Where the words take more than a line, the rest sit under the first.
    for note in notes {
        let mut more = vec![span("│ ", dim()), Span::raw(" ".repeat(NAME + cell))];
        more.extend(note_spans(Some(note)));
        out.push(BodyLine::form(with_prefix(more), None, false));
    }

    let saveable = editing.saveable(snapshot);
    let save = if editing.is_sent() {
        span("saving…", dim())
    } else if saveable {
        Span::raw("save")
    } else {
        span("fix value to save", dim())
    };
    let footer = vec![
        span("└ ", dim()),
        Span::raw("[ctrl+s] "),
        save,
        Span::raw("   [esc] cancel"),
    ];
    // The save control stays in view whatever the window.
    out.push(BodyLine::form(with_prefix(footer), None, false).pinned());
    out
}
