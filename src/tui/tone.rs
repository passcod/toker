//! The tones and styles the pages draw in: how loudly a clause reads, and the
//! spans that carry it. Nothing here knows which page is drawing.

use ratatui::{
    style::{Color, Style},
    text::Span,
};

/// How loudly a clause reads. Colour only ever draws the eye to a problem:
/// healthy is plain, what is merely off is dim, and a refusal is red
/// (spec: UTUI#distinguishing-a-reading; it never carries a reading alone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    Plain,
    Dim,
    Warn,
    Bad,
}

impl Tone {
    pub(super) fn style(self) -> Style {
        match self {
            Tone::Plain => Style::new(),
            Tone::Dim => Style::new().dim(),
            Tone::Warn => Style::new().fg(Color::Yellow),
            Tone::Bad => Style::new().fg(Color::Red),
        }
    }
}

pub(super) fn dim() -> Style {
    Style::new().dim()
}

pub(super) fn bold() -> Style {
    Style::new().bold()
}

/// A span in a tone.
pub(super) fn toned(content: impl Into<String>, tone: Tone) -> Span<'static> {
    Span::styled(content.into(), tone.style())
}

/// A dim label, for the left of a label and value row.
pub(super) fn label(content: impl Into<String>) -> Span<'static> {
    Span::styled(content.into(), dim())
}
