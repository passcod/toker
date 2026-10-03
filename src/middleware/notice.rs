//! Native rendering for gate notices (plan: "Native rendering for gate
//! notices", docs/plans/toker-toolsuite.md:191). A blocked request is
//! answered with a synthetic assistant turn, and the client renders that
//! turn's text however it renders any assistant text — so a frontend with
//! a structured format of its own should see the notice in it: claude's
//! insight block, Workhorse's `> [!NOTE]` GFM alert, plain text
//! everywhere else.
//!
//! The choice is per-frontend-protocol in the plan's wording; until a
//! client-selection mechanism exists it is a config knob —
//! `[gates] notice_style` — threaded to the one place a notice is
//! composed ([`crate::middleware::quota::Blocking::notice`]).
//!
//! [`render`] is a pure function of (style, content), like every byte
//! the gate emits (invariant 4, docs/plans/toker-toolsuite.md:93): a
//! rendered notice enters replayed history, and the insight block's
//! width is FROZEN for exactly that reason — a width that varied with
//! anything (the content, the terminal, the version) would invalidate
//! cache prefixes and replay.

use serde::Deserializer;

/// The insight block's header line: `★ Insight ` then dashes to 50
/// columns (40 of them).
///
/// The width is FROZEN for byte-stability (invariant 4): the block
/// enters replayed history, and a width that varied with anything would
/// invalidate cache prefixes / replay. Do not compute it, trim it, or
/// fit it to content — it is a constant, byte-pinned by the tests below.
pub const INSIGHT_HEADER: &str = "★ Insight ────────────────────────────────────────";

/// The insight block's footer: 50 columns of dashes, matching
/// [`INSIGHT_HEADER`]'s total width.
///
/// FROZEN with the header — same invariant-4 rule, same byte pin.
pub const INSIGHT_FOOTER: &str = "──────────────────────────────────────────────────";

/// How a gate notice is rendered — the `[gates] notice_style` config
/// value. Deserialises case-insensitively (`"insight"`, `"Gfm"`,
/// `"PLAIN"`, …); an unknown value is a config load error, not a silent
/// default (the crate's deny_unknown_fields strictness).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NoticeStyle {
    /// Claude's insight block: the frozen header, the content lines
    /// verbatim, the frozen footer. **Claude renders this and nothing else
    /// does** — opt in per client only when every anthropic-frontend client
    /// in play is claude Code.
    Insight,
    /// A GFM alert — `> [!NOTE]` then the content lines each prefixed
    /// `> ` — the generic form: Workhorse, GitHub-ish renderers, and
    /// anything that falls back to plain markdown all show it sensibly.
    #[default]
    Gfm,
    /// The content verbatim: the degradation for frontends with no
    /// structured format of their own.
    Plain,
}

impl<'de> serde::Deserialize<'de> for NoticeStyle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        match text.to_ascii_lowercase().as_str() {
            "insight" => Ok(NoticeStyle::Insight),
            "gfm" => Ok(NoticeStyle::Gfm),
            "plain" => Ok(NoticeStyle::Plain),
            _ => Err(serde::de::Error::unknown_variant(
                &text,
                &["insight", "gfm", "plain"],
            )),
        }
    }
}

/// Render `content` in the style. Pure: the same (style, content) pair
/// renders the same bytes on every call, forever (invariant 4 — a gate
/// notice enters replayed history).
///
/// Multi-line content passes through verbatim between the insight
/// header and footer, and line by line under the GFM prefixes.
pub fn render(style: NoticeStyle, content: &str) -> String {
    match style {
        NoticeStyle::Insight => {
            let mut out = String::with_capacity(
                INSIGHT_HEADER.len() + content.len() + INSIGHT_FOOTER.len() + 2,
            );
            out.push_str(INSIGHT_HEADER);
            out.push('\n');
            out.push_str(content);
            out.push('\n');
            out.push_str(INSIGHT_FOOTER);
            out
        }
        NoticeStyle::Gfm => {
            let mut out = String::from("> [!NOTE]");
            for line in content.lines() {
                out.push_str("\n> ");
                out.push_str(line);
            }
            out
        }
        NoticeStyle::Plain => content.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{INSIGHT_FOOTER, INSIGHT_HEADER, NoticeStyle, render};

    /// The plan's own example content.
    const CONTENT: &str = "You've hit the 5-hour limit for your current plan. It resets at 21:30.";

    #[test]
    fn the_insight_constants_are_frozen_at_50_columns() {
        // The exact strings, pinned — and their widths, stated: 10
        // columns of label ("★ Insight ") + 40 of dashes = 50; the
        // footer is 50 of dashes. FROZEN (invariant 4): the block
        // enters replayed history, so a width that varied with anything
        // would invalidate cache prefixes / replay.
        assert_eq!(
            INSIGHT_HEADER,
            "★ Insight ────────────────────────────────────────"
        );
        assert_eq!(INSIGHT_HEADER, format!("★ Insight {}", "─".repeat(40)));
        assert_eq!(INSIGHT_HEADER.chars().count(), 50);
        assert_eq!(INSIGHT_FOOTER, "─".repeat(50));
        assert_eq!(INSIGHT_FOOTER.chars().count(), 50);
        assert!(
            INSIGHT_HEADER.starts_with("★ Insight "),
            "the label is frozen too"
        );
    }

    #[test]
    fn insight_wraps_the_content_verbatim_between_header_and_footer() {
        // Byte-pinned: the plan's example, rendered whole.
        assert_eq!(
            render(NoticeStyle::Insight, CONTENT),
            "★ Insight ────────────────────────────────────────\n\
             You've hit the 5-hour limit for your current plan. It resets at 21:30.\n\
             ──────────────────────────────────────────────────"
        );
        // Multi-line content passes through verbatim, untouched.
        assert_eq!(
            render(NoticeStyle::Insight, "line one\nline two"),
            format!("{INSIGHT_HEADER}\nline one\nline two\n{INSIGHT_FOOTER}")
        );
    }

    #[test]
    fn gfm_prefixes_each_content_line_under_the_alert_marker() {
        assert_eq!(
            render(NoticeStyle::Gfm, CONTENT),
            "> [!NOTE]\n> You've hit the 5-hour limit for your current plan. It resets at 21:30."
        );
        // Every line gets the prefix; a trailing newline in the content
        // is not a line, so it never grows an empty `> ` continuation.
        assert_eq!(
            render(NoticeStyle::Gfm, "line one\nline two"),
            "> [!NOTE]\n> line one\n> line two"
        );
        assert_eq!(
            render(NoticeStyle::Gfm, "line one\nline two\n"),
            "> [!NOTE]\n> line one\n> line two",
            "a trailing newline is not a content line"
        );
    }

    #[test]
    fn plain_is_the_content_verbatim() {
        assert_eq!(render(NoticeStyle::Plain, CONTENT), CONTENT);
        assert_eq!(render(NoticeStyle::Plain, "a\nb"), "a\nb");
    }

    #[test]
    fn empty_content_renders_the_block_skeletons() {
        // Insight: header, an empty line, footer — the content lines
        // are "none", not "missing", so the block keeps its shape.
        assert_eq!(
            render(NoticeStyle::Insight, ""),
            format!("{INSIGHT_HEADER}\n\n{INSIGHT_FOOTER}")
        );
        // Gfm: the bare alert marker; Plain: nothing.
        assert_eq!(render(NoticeStyle::Gfm, ""), "> [!NOTE]");
        assert_eq!(render(NoticeStyle::Plain, ""), "");
    }

    #[test]
    fn the_default_style_is_the_generic_gfm_alert() {
        // Insight is claude-only rendering; GFM is the one every client
        // family shows sensibly, and toker cannot yet tell clients apart.
        assert_eq!(NoticeStyle::default(), NoticeStyle::Gfm);
    }

    #[test]
    fn the_style_deserialises_case_insensitively_and_rejects_the_unknown() {
        // Case-insensitive variant names, every casing pattern the
        // config side can meet (the value is JSON-quoted by hand —
        // from_str takes a document, not a bare word).
        for text in ["insight", "INSIGHT", "Insight", "iNsIgHt"] {
            assert_eq!(
                serde_json::from_str::<NoticeStyle>(&format!("\"{text}\"")).expect("parses"),
                NoticeStyle::Insight
            );
        }
        for text in ["gfm", "GFM", "Gfm"] {
            assert_eq!(
                serde_json::from_str::<NoticeStyle>(&format!("\"{text}\"")).expect("parses"),
                NoticeStyle::Gfm
            );
        }
        for text in ["plain", "PLAIN", "Plain"] {
            assert_eq!(
                serde_json::from_str::<NoticeStyle>(&format!("\"{text}\"")).expect("parses"),
                NoticeStyle::Plain
            );
        }
        // An unknown value is an error, not a silent default — the
        // config's deny_unknown_fields strictness, on the value side.
        assert!(
            serde_json::from_str::<NoticeStyle>("\"fancy\"").is_err(),
            "an unknown style must fail to load"
        );
    }

    #[test]
    fn render_is_pure() {
        // Invariant 4: same (style, content) → same bytes, every call.
        for _ in 0..3 {
            for style in [NoticeStyle::Insight, NoticeStyle::Gfm, NoticeStyle::Plain] {
                assert_eq!(render(style, CONTENT), render(style, CONTENT));
            }
        }
    }
}
