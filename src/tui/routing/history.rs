//! The history tab: the revisions, newest first, each with what it changed
//! from the one before it and the lanes pinned to it; the selected one as
//! the diff in words (spec: UTUI#history).

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::data::{array, count, text};
use super::table::{BodyLine, Cells, Table, heading, span, widest};
use super::words::{bold, dim, lanes_phrase, when};

/// The most changes the detail lists before it counts the rest.
const CHANGES_SHOWN: usize = 8;

/// The longest a lane's name is drawn.
const LANE_NAME: usize = 14;

fn id(revision: &Value) -> i64 {
    revision
        .get("revision_id")
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// The derived facts about the lanes pinned to revision `id`.
fn pinned(snapshot: &Value, id: i64) -> Option<&Value> {
    snapshot
        .get("derived")
        .map(|derived| array(derived, "revisions"))
        .unwrap_or(&[])
        .iter()
        .find(|lanes| lanes.get("revision_id").and_then(Value::as_i64) == Some(id))
        .filter(|lanes| count(lanes, "total") > 0)
}

fn lanes_of(snapshot: &Value, id: i64) -> String {
    pinned(snapshot, id).map_or_else(|| "none".to_owned(), lanes_phrase)
}

/// When the revision became live, or was made.
fn moment(revision: &Value) -> String {
    let at = revision
        .get("activated_ms")
        .and_then(Value::as_i64)
        .or_else(|| revision.get("created_ms").and_then(Value::as_i64));
    at.map_or_else(|| "-".to_owned(), when)
}

/// What the revision changed, in a line: the daemon's own summary, or, from
/// a daemon that does not send one, what its author wrote.
fn summary(revision: &Value) -> String {
    ["summary", "description"]
        .iter()
        .map(|key| text(revision, key))
        .find(|said| !said.is_empty())
        .unwrap_or("-")
        .to_owned()
}

/// The revisions as a table.
pub(super) fn table(
    snapshot: &Value,
    revisions: &[Value],
    live: Option<i64>,
    width: usize,
) -> Table {
    const REV: usize = 9;
    const WHEN: usize = 18;
    let lanes: Vec<String> = revisions
        .iter()
        .map(|revision| lanes_of(snapshot, id(revision)))
        .collect();
    let lanes_width = widest(lanes.iter().map(String::as_str).chain(["lanes"]));
    let said = width.saturating_sub(REV + WHEN + lanes_width + 1);
    let header = Cells::new()
        .cell(heading("rev"), REV)
        .cell(heading("when"), WHEN)
        .cell(heading("what changed"), said + 1)
        .cell(heading("lanes"), lanes_width)
        .line();
    let body = revisions
        .iter()
        .zip(&lanes)
        .enumerate()
        .map(|(entry, (revision, here))| {
            let at = id(revision);
            let name = format!("{at:>3}{}", if Some(at) == live { " live" } else { "" });
            let line = Cells::new()
                .cell(vec![Span::raw(name)], REV)
                .cell(vec![Span::raw(moment(revision))], WHEN)
                .cell(vec![Span::raw(summary(revision))], said)
                .raw(vec![Span::raw(" ")])
                .cell(
                    vec![if here == "none" {
                        span(here.clone(), dim())
                    } else {
                        Span::raw(here.clone())
                    }],
                    lanes_width,
                )
                .line();
            BodyLine::of(line, entry)
        })
        .collect();
    Table { header, body }
}

/// A move of the running lanes onto the live revision, asked for once and
/// waiting for the second confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Armed {
    /// The lanes whose cache it would lose, as far as they are named.
    pub(super) lanes: Vec<String>,
    /// How many lanes would lose it, named or not.
    pub(super) count: u64,
    /// The revision they would move to.
    pub(super) revision: i64,
}

impl Armed {
    pub(super) fn new(lanes: Vec<String>, count: u64, revision: i64) -> Armed {
        Armed {
            lanes,
            count,
            revision,
        }
    }

    fn sentence(&self) -> String {
        let names = shortened(&self.lanes);
        let named = if names.is_empty() {
            String::new()
        } else if (names.len() as u64) < self.count {
            format!(
                ": {} and {} more",
                names.join(", "),
                self.count - names.len() as u64
            )
        } else {
            format!(": {}", names.join(", "))
        };
        let lanes = if self.count == 1 { "lane" } else { "lanes" };
        let verb = if self.count == 1 { "loses" } else { "lose" };
        format!(
            "{} {lanes} {verb} {} cache{named}",
            self.count,
            if self.count == 1 { "its" } else { "their" },
        )
    }
}

/// The lane names as the prompt says them. A long name is cut in its
/// middle, keeping the end that tells lanes apart; where two cut names would
/// still read the same, the names are given whole, since the prompt exists
/// to say which caches are about to be lost.
fn shortened(lanes: &[String]) -> Vec<String> {
    let cut: Vec<String> = lanes.iter().map(|lane| short(lane)).collect();
    let clash = cut
        .iter()
        .enumerate()
        .any(|(at, name)| (0..at).any(|before| cut[before] == *name && lanes[before] != lanes[at]));
    if clash { lanes.to_vec() } else { cut }
}

fn short(name: &str) -> String {
    if name.width() <= LANE_NAME {
        return name.to_owned();
    }
    // The head and the tail share what is left after the ellipsis.
    let tail_room = (LANE_NAME - 1) / 2;
    let head_room = LANE_NAME - 1 - tail_room;
    let mut head = String::new();
    let mut used = 0;
    for glyph in name.chars() {
        let wide = glyph.width().unwrap_or(0);
        if used + wide > head_room {
            break;
        }
        head.push(glyph);
        used += wide;
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for glyph in name.chars().rev() {
        let wide = glyph.width().unwrap_or(0);
        if used + wide > tail_room {
            break;
        }
        tail.push(glyph);
        used += wide;
    }
    tail.reverse();
    format!("{head}…{}", tail.into_iter().collect::<String>())
}

/// The lanes whose cache moving the running lanes onto revision `live` would
/// lose, and how many of them there are: those on an earlier revision that
/// are warm or not provably cold.
pub(super) fn at_risk(snapshot: &Value, live: i64) -> (Vec<String>, u64) {
    let earlier = || {
        snapshot
            .get("derived")
            .map(|derived| array(derived, "revisions"))
            .unwrap_or(&[])
            .iter()
            .filter(move |lanes| lanes.get("revision_id").and_then(Value::as_i64) != Some(live))
    };
    let names = earlier()
        .flat_map(|lanes| array(lanes, "at_risk"))
        .filter_map(|name| name.as_str().map(str::to_owned))
        .collect();
    let total = earlier()
        .map(|lanes| count(lanes, "warm") + count(lanes, "unknown"))
        .sum();
    (names, total)
}

/// How many lanes are pinned to a revision other than `live`.
pub(super) fn on_earlier(snapshot: &Value, live: i64) -> u64 {
    snapshot
        .get("derived")
        .map(|derived| array(derived, "revisions"))
        .unwrap_or(&[])
        .iter()
        .filter(|lanes| lanes.get("revision_id").and_then(Value::as_i64) != Some(live))
        .map(|lanes| count(lanes, "total"))
        .sum()
}

/// Where the lanes stand: on this revision and, where it is the live one,
/// still on earlier ones.
fn lanes_line(snapshot: &Value, at: i64, live: Option<i64>) -> Line<'static> {
    let here = lanes_of(snapshot, at);
    let mut said = if here == "none" {
        format!("none pinned to rev {at}")
    } else {
        format!("{here} on rev {at}")
    };
    if Some(at) == live {
        let earlier = snapshot
            .get("derived")
            .map(|derived| array(derived, "revisions"))
            .unwrap_or(&[])
            .iter()
            .filter(|lanes| lanes.get("revision_id").and_then(Value::as_i64) != Some(at))
            .filter(|lanes| count(lanes, "total") > 0);
        for lanes in earlier {
            said.push_str(&format!(
                " · {} still on rev {}",
                lanes_phrase(lanes),
                count(lanes, "revision_id")
            ));
        }
    }
    Line::from(vec![span("lanes  ", dim()), Span::raw(said)])
}

/// The sign a change is listed under.
fn sign(change: &str) -> &'static str {
    match change {
        "added" => "+",
        "removed" => "-",
        "moved" => "↕",
        _ => "~",
    }
}

/// The detail's controls, `[m]` reversed while a move is armed.
fn controls_line(at: i64, live: Option<i64>, armed: bool) -> Line<'static> {
    let mut spans = vec![Span::raw(format!(
        "[a] {}   ",
        if Some(at) == live {
            "already live"
        } else {
            "reactivate"
        }
    ))];
    let mover = "[m] move running lanes";
    spans.push(if armed {
        Span::styled(mover, Style::new().add_modifier(Modifier::REVERSED))
    } else {
        Span::raw(mover)
    });
    spans.push(Span::raw("   [c] trace a request"));
    Line::from(spans)
}

/// The selected revision's detail: what it changed, where its lanes stand
/// and what can be done with it.
pub(super) fn detail(
    snapshot: &Value,
    revision: &Value,
    live: Option<i64>,
    armed: Option<&Armed>,
) -> Vec<Line<'static>> {
    let at = id(revision);
    let mut head = vec![
        span(format!("rev {at}"), bold()),
        Span::raw(format!("  {}", moment(revision))),
    ];
    match (
        text(revision, "source"),
        revision.get("parent_revision_id").and_then(Value::as_i64),
    ) {
        ("reactivation", Some(parent)) => {
            head.push(span(format!(" · a copy of rev {parent}"), dim()));
        }
        (_, Some(parent)) => head.push(span(format!(" · from rev {parent}"), dim())),
        (_, None) => head.push(span(" · the first revision".to_owned(), dim())),
    }
    let mut lines = vec![Line::from(head)];
    let changes = array(revision, "changes");
    if changes.is_empty() {
        lines.push(Line::from(span(summary(revision), dim())));
    }
    for change in changes.iter().take(CHANGES_SHOWN) {
        lines.push(Line::from(vec![
            span(format!("{} ", sign(text(change, "change"))), bold()),
            Span::raw(text(change, "text").to_owned()),
        ]));
    }
    if changes.len() > CHANGES_SHOWN {
        lines.push(Line::from(span(
            format!("… and {} more", changes.len() - CHANGES_SHOWN),
            dim(),
        )));
    }
    lines.push(Line::raw(""));
    lines.push(lanes_line(snapshot, at, live));
    lines.push(controls_line(at, live, armed.is_some()));
    if let Some(armed) = armed {
        lines.push(Line::from(vec![
            Span::raw(armed.sentence()),
            span(
                format!(
                    " · [m] again to move them to rev {} · [esc] cancel",
                    armed.revision
                ),
                dim(),
            ),
        ]));
    }
    lines
}
