//! The history tab: the revisions, newest first, each with what it was made
//! from and the lanes pinned to it (spec: UTUI#history).
//!
//! The diff in words, and the controls that act on a revision, belong to the
//! tab's editing; this is the list they hang off.

use ratatui::text::{Line, Span};
use serde_json::Value;

use super::data::{array, count, text};
use super::table::{BodyLine, Cells, Table, controls, field, heading, span};
use super::words::{bold, dim, lanes_phrase};

fn id(revision: &Value) -> i64 {
    revision
        .get("revision_id")
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// The lanes pinned to revision `id`, from the derived facts.
fn lanes(snapshot: &Value, id: i64) -> Option<&Value> {
    snapshot
        .get("derived")
        .map(|derived| array(derived, "revisions"))
        .unwrap_or(&[])
        .iter()
        .find(|lanes| lanes.get("revision_id").and_then(Value::as_i64) == Some(id))
        .filter(|lanes| count(lanes, "total") > 0)
}

fn source(revision: &Value) -> String {
    text(revision, "source").replace('_', " ")
}

/// The revisions as a table.
pub(super) fn table(
    snapshot: &Value,
    revisions: &[Value],
    live: Option<i64>,
    width: usize,
) -> Table {
    let header = Cells::new()
        .cell(Vec::new(), 2)
        .cell(heading("revision"), 14)
        .cell(heading("from"), 16)
        .cell(heading("lanes pinned"), 22)
        .raw(heading("description"))
        .line();
    let body = revisions
        .iter()
        .enumerate()
        .map(|(entry, revision)| {
            let at = id(revision);
            let here = lanes(snapshot, at).map_or_else(|| "none".to_owned(), lanes_phrase);
            let name = if Some(at) == live {
                format!("rev {at} live")
            } else {
                format!("rev {at}")
            };
            let line = Cells::new()
                .cell(Vec::new(), 2)
                .cell(vec![Span::raw(name)], 14)
                .cell(vec![Span::raw(source(revision))], 16)
                .cell(
                    vec![if here == "none" {
                        span(here, dim())
                    } else {
                        Span::raw(here)
                    }],
                    22,
                )
                .cell(
                    vec![Span::raw(text(revision, "description").to_owned())],
                    width.saturating_sub(54),
                )
                .line();
            BodyLine::of(line, entry)
        })
        .collect();
    Table { header, body }
}

/// The selected revision's detail.
pub(super) fn detail(snapshot: &Value, revision: &Value, live: Option<i64>) -> Vec<Line<'static>> {
    let at = id(revision);
    let mut lines = vec![Line::from(span(
        if Some(at) == live {
            format!("rev {at} · live")
        } else {
            format!("rev {at}")
        },
        bold(),
    ))];
    lines.push(field("from", 14, vec![Span::raw(source(revision))]));
    if let Some(parent) = revision.get("parent_revision_id").and_then(Value::as_i64) {
        lines.push(field(
            "made from",
            14,
            vec![Span::raw(format!("rev {parent}"))],
        ));
    }
    lines.push(field(
        "lanes pinned",
        14,
        vec![Span::raw(
            lanes(snapshot, at).map_or_else(|| "none".to_owned(), lanes_phrase),
        )],
    ));
    lines.push(controls(&[("a", "reactivate")]));
    lines
}
