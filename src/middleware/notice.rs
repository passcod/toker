//! Native rendering for gate notices (see docs/internals/notices.md). A
//! blocked request is
//! answered with a synthetic assistant turn, and the client renders that
//! turn's text however it renders any assistant text — so a frontend with
//! a structured format of its own should see the notice in it.
//!
//! The frontend is named by the `/f/<frontend>` prefix its base URL
//! carries (the setup wizard writes `/f/claude` and `/f/workhorse`; see
//! [`crate::server`]), and the `[notices]` config table maps a frontend
//! to a [`NoticeStyle`] ([`crate::config::NoticesConfig`]): claude gets
//! the insight-style block, Workhorse its `> [!TOKER]` alert, every other
//! client (and an unprefixed base URL) the GFM alert.
//!
//! The [`NoticeLevel`] is the composer's, not the renderer's: the quota
//! block is a [`NoticeLevel::Caution`], the cold notice a
//! [`NoticeLevel::Warning`], decided where each notice is written.
//!
//! [`render`] is a pure function of (style, level, content), like every
//! byte the gate emits (invariant 8 in AGENTS.md): a
//! rendered notice enters replayed history, and the block's width is
//! FROZEN for exactly that reason — a width that varied with anything
//! (the content, the terminal, the version) would invalidate cache
//! prefixes and replay.

use serde::Deserializer;

/// toker's name as it first appears in a notice: a link to the project,
/// so a reader who has never heard of the proxy can find out what stopped
/// them.
pub const TOKER_LINK: &str = "[toker](https://github.com/passcod/toker)";

/// The block style's header: `★ Toker ` then dashes to 50 columns (42 of
/// them), wrapped in backticks. Claude Code renders its own explanatory
/// insight lines as inline code — the backticks are what make this one
/// look like those rather than like a line of prose dashes.
///
/// The width is FROZEN for byte-stability (invariant 4): the block
/// enters replayed history, and a width that varied with anything would
/// invalidate cache prefixes / replay. Do not compute it, trim it, or
/// fit it to content — it is a constant, byte-pinned by the tests below.
pub const BLOCK_HEADER: &str = "`★ Toker ──────────────────────────────────────────`";

/// The block style's footer: 50 columns of dashes, in backticks,
/// matching [`BLOCK_HEADER`]'s width.
///
/// FROZEN with the header — same invariant-4 rule, same byte pin.
pub const BLOCK_FOOTER: &str = "`──────────────────────────────────────────────────`";

/// How serious a notice is, which the GFM style shows as its alert kind.
/// The notice's composer decides it; the renderer only spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    /// A session stopped until the operator acts: the quota block.
    Caution,
    /// Advice the operator may act on or ignore: the cold notice.
    Warning,
}

impl NoticeLevel {
    /// The GFM alert kind.
    fn alert(self) -> &'static str {
        match self {
            NoticeLevel::Caution => "CAUTION",
            NoticeLevel::Warning => "WARNING",
        }
    }
}

/// How a gate notice is rendered for one frontend — a `[notices]` config
/// value. Deserialises case-insensitively (`"block"`, `"Gfm"`, `"TOKER"`,
/// …); an unknown value is a config load error, not a silent default (the
/// crate's deny_unknown_fields strictness).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NoticeStyle {
    /// A GFM alert of the notice's level — `> [!CAUTION]` or
    /// `> [!WARNING]`, then the content lines each prefixed `> `. The
    /// generic form: GitHub-ish renderers show the alert, anything that
    /// falls back to plain markdown shows a quote.
    #[default]
    Gfm,
    /// Workhorse's own alert, `> [!TOKER]`, whatever the level: Workhorse
    /// renders toker's notices as a kind of their own.
    Toker,
    /// The insight-style block claude renders: the frozen backticked
    /// header and footer around the content lines verbatim. **Claude
    /// renders this and nothing else does.** Spelled `"insight"` before
    /// it carried toker's name; that spelling still reads as this.
    Block,
    /// The content in brackets: opt-in, for frontends with no structured
    /// format of their own. The brackets are its only frame.
    Plain,
}

impl NoticeStyle {
    /// The canonical config spelling.
    pub fn name(self) -> &'static str {
        match self {
            NoticeStyle::Gfm => "gfm",
            NoticeStyle::Toker => "toker",
            NoticeStyle::Block => "block",
            NoticeStyle::Plain => "plain",
        }
    }
}

impl<'de> serde::Deserialize<'de> for NoticeStyle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        match text.to_ascii_lowercase().as_str() {
            "gfm" => Ok(NoticeStyle::Gfm),
            "toker" => Ok(NoticeStyle::Toker),
            // "insight" is the block's spelling from before the
            // per-frontend table; a config written then still loads.
            "block" | "insight" => Ok(NoticeStyle::Block),
            "plain" => Ok(NoticeStyle::Plain),
            _ => Err(serde::de::Error::unknown_variant(
                &text,
                &["gfm", "toker", "block", "plain"],
            )),
        }
    }
}

impl serde::Serialize for NoticeStyle {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // The canonical lowercase name — the inverse of the
        // case-insensitive deserialiser above, so a config value
        // round-trips as its own example (`claude = "block"`). The setup
        // wizard's `toker.toml` rewrite writes through here (see
        // `crate::setup::config_writer`).
        serializer.serialize_str(self.name())
    }
}

/// Render `content` in the style at the level. Pure: the same (style,
/// level, content) renders the same bytes on every call, forever
/// (invariant 4 — a gate notice enters replayed history).
///
/// Multi-line content passes through verbatim between the block's header
/// and footer, and line by line under the alert prefixes.
pub fn render(style: NoticeStyle, level: NoticeLevel, content: &str) -> String {
    match style {
        NoticeStyle::Block => {
            let mut out =
                String::with_capacity(BLOCK_HEADER.len() + content.len() + BLOCK_FOOTER.len() + 2);
            out.push_str(BLOCK_HEADER);
            out.push('\n');
            out.push_str(content);
            out.push('\n');
            out.push_str(BLOCK_FOOTER);
            out
        }
        NoticeStyle::Gfm => alert(&format!("> [!{}]", level.alert()), content),
        NoticeStyle::Toker => alert("> [!TOKER]", content),
        // Unframed, the brackets are what mark the text as harness output
        // rather than the model's own words (the shape the client's own
        // injected notices take); every other style's frame does that job.
        NoticeStyle::Plain => format!("[{content}]"),
    }
}

/// An alert marker line, then each content line under `> `.
fn alert(marker: &str, content: &str) -> String {
    let mut out = String::from(marker);
    for line in content.lines() {
        out.push_str("\n> ");
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{BLOCK_FOOTER, BLOCK_HEADER, NoticeLevel, NoticeStyle, render};

    /// The plan's own example content.
    const CONTENT: &str = "You've hit the 5-hour limit for your current plan. It resets at 21:30.";

    const STYLES: [NoticeStyle; 4] = [
        NoticeStyle::Gfm,
        NoticeStyle::Toker,
        NoticeStyle::Block,
        NoticeStyle::Plain,
    ];
    const LEVELS: [NoticeLevel; 2] = [NoticeLevel::Caution, NoticeLevel::Warning];

    #[test]
    fn the_block_constants_are_frozen_at_50_columns_in_backticks() {
        // The exact strings, pinned — and their widths, stated: 8 columns
        // of label ("★ Toker ") + 42 of dashes = 50 inside the backticks;
        // the footer is 50 of dashes. FROZEN (invariant 4): the block
        // enters replayed history, so a width that varied with anything
        // would invalidate cache prefixes / replay.
        assert_eq!(
            BLOCK_HEADER,
            "`★ Toker ──────────────────────────────────────────`"
        );
        assert_eq!(BLOCK_HEADER, format!("`★ Toker {}`", "─".repeat(42)));
        assert_eq!(BLOCK_HEADER.trim_matches('`').chars().count(), 50);
        assert_eq!(BLOCK_FOOTER, format!("`{}`", "─".repeat(50)));
        assert_eq!(BLOCK_FOOTER.trim_matches('`').chars().count(), 50);
    }

    #[test]
    fn block_wraps_the_content_verbatim_between_header_and_footer() {
        // The plan's example, rendered whole — at either level, the block
        // has no level of its own.
        let [caution, warning] = LEVELS.map(|level| render(NoticeStyle::Block, level, CONTENT));
        assert_eq!(caution, warning, "the block has no level");
        // The invariant, stated outside the snapshot so accepting a new
        // snapshot cannot change it: the frozen header, the content
        // verbatim, the frozen footer.
        assert_eq!(
            caution,
            format!("{BLOCK_HEADER}\n{CONTENT}\n{BLOCK_FOOTER}")
        );
        insta::assert_snapshot!(caution, @r"
        `★ Toker ──────────────────────────────────────────`
        You've hit the 5-hour limit for your current plan. It resets at 21:30.
        `──────────────────────────────────────────────────`
        ");
        // Multi-line content passes through verbatim, untouched.
        assert_eq!(
            render(
                NoticeStyle::Block,
                NoticeLevel::Warning,
                "line one\nline two"
            ),
            format!("{BLOCK_HEADER}\nline one\nline two\n{BLOCK_FOOTER}")
        );
    }

    #[test]
    fn gfm_spells_the_level_and_prefixes_each_content_line() {
        insta::assert_snapshot!(render(NoticeStyle::Gfm, NoticeLevel::Caution, CONTENT), @r"
        > [!CAUTION]
        > You've hit the 5-hour limit for your current plan. It resets at 21:30.
        ");
        insta::assert_snapshot!(render(NoticeStyle::Gfm, NoticeLevel::Warning, "line one\nline two"), @r"
        > [!WARNING]
        > line one
        > line two
        ");
        // A trailing newline in the content is not a line, so it never
        // grows an empty `> ` continuation.
        assert_eq!(
            render(
                NoticeStyle::Gfm,
                NoticeLevel::Warning,
                "line one\nline two\n"
            ),
            render(NoticeStyle::Gfm, NoticeLevel::Warning, "line one\nline two"),
            "a trailing newline is not a content line"
        );
    }

    #[test]
    fn toker_is_workhorses_alert_whatever_the_level() {
        let [caution, warning] =
            LEVELS.map(|level| render(NoticeStyle::Toker, level, "line one\nline two"));
        assert_eq!(caution, warning, "the level does not show");
        insta::assert_snapshot!(caution, @r"
        > [!TOKER]
        > line one
        > line two
        ");
    }

    #[test]
    fn plain_is_the_content_in_brackets() {
        for level in LEVELS {
            assert_eq!(
                render(NoticeStyle::Plain, level, CONTENT),
                format!("[{CONTENT}]")
            );
            assert_eq!(render(NoticeStyle::Plain, level, "a\nb"), "[a\nb]");
        }
    }

    #[test]
    fn empty_content_renders_the_skeletons() {
        // Block: header, an empty line, footer — the content lines are
        // "none", not "missing", so the block keeps its shape.
        assert_eq!(
            render(NoticeStyle::Block, NoticeLevel::Caution, ""),
            format!("{BLOCK_HEADER}\n\n{BLOCK_FOOTER}")
        );
        // The alerts: the bare marker; Plain: its brackets.
        insta::assert_snapshot!(render(NoticeStyle::Gfm, NoticeLevel::Caution, ""), @"> [!CAUTION]");
        insta::assert_snapshot!(render(NoticeStyle::Toker, NoticeLevel::Caution, ""), @"> [!TOKER]");
        insta::assert_snapshot!(render(NoticeStyle::Plain, NoticeLevel::Caution, ""), @"[]");
    }

    #[test]
    fn no_frame_carries_an_em_dash() {
        // The notices hold no em dashes (the compacted wording dropped
        // them); every style's frame is held to the same rule, so a
        // frame change cannot bring one back unnoticed.
        for style in STYLES {
            for level in LEVELS {
                let rendered = render(style, level, "");
                assert!(!rendered.contains('\u{2014}'), "{style:?}: {rendered}");
            }
        }
    }

    #[test]
    fn the_default_style_is_the_generic_gfm_alert() {
        // The style every unknown client gets: the one every client
        // family shows sensibly.
        assert_eq!(NoticeStyle::default(), NoticeStyle::Gfm);
    }

    #[test]
    fn the_style_deserialises_case_insensitively_and_rejects_the_unknown() {
        let parse = |text: &str| serde_json::from_str::<NoticeStyle>(&format!("\"{text}\""));
        for (texts, style) in [
            (&["gfm", "GFM", "Gfm"][..], NoticeStyle::Gfm),
            (&["toker", "TOKER", "Toker"][..], NoticeStyle::Toker),
            (&["block", "BLOCK", "Block"][..], NoticeStyle::Block),
            // The block's earlier spelling still loads.
            (&["insight", "INSIGHT", "iNsIgHt"][..], NoticeStyle::Block),
            (&["plain", "PLAIN", "Plain"][..], NoticeStyle::Plain),
        ] {
            for text in texts {
                assert_eq!(parse(text).expect("parses"), style, "{text}");
            }
        }
        // An unknown value is an error, not a silent default — the
        // config's deny_unknown_fields strictness, on the value side.
        assert!(
            parse("fancy").is_err(),
            "an unknown style must fail to load"
        );
        // Each style serialises as its canonical name, which parses back.
        for style in STYLES {
            let text = serde_json::to_string(&style).expect("serialises");
            assert_eq!(text, format!("\"{}\"", style.name()));
            assert_eq!(
                serde_json::from_str::<NoticeStyle>(&text).expect("round-trips"),
                style
            );
        }
    }

    #[test]
    fn render_is_pure() {
        // Invariant 4: same (style, level, content) → same bytes, every
        // call.
        for _ in 0..3 {
            for style in STYLES {
                for level in LEVELS {
                    assert_eq!(render(style, level, CONTENT), render(style, level, CONTENT));
                }
            }
        }
    }
}
