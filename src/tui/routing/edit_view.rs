//! Drawing an edit in place: the form that opens under a row, one line per
//! field with what the check says beside it, and the preview of what the
//! edit would do to recent requests (spec: UTUI#editing).

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::data::text;
use super::editing::{Editing, What};
use super::form::{FormLine, Note};
use super::table::{BodyLine, Cells, span};
use super::words::{Tone, dim, toned};
use crate::tui::chrome;

/// Where a form's lines are indented to, under the row they belong to.
const INDENT: usize = 4;

/// What the block is headed with.
fn heading(editing: &Editing) -> String {
    let id = |value: &Value| text(value, "id").to_owned();
    match &editing.what {
        What::Record { adding: true, .. } => editing.form.title().to_owned(),
        What::Record { .. } => {
            let value = editing.form.value();
            match value.get("id").and_then(Value::as_str) {
                Some(id) => format!("{} {id}", editing.form.title()),
                None => editing.form.title().to_owned(),
            }
        }
        What::Remove { kind, record } => match record.get("id").and_then(Value::as_str) {
            Some(id) => format!("remove {} {id}", noun(kind)),
            None => format!("remove the {}", noun(kind)),
        },
        What::AddCandidate { route_id, .. } => format!("add a fallback to {route_id}"),
        What::EditCandidate { route, index } => {
            format!("edit candidate {} of {}", index + 1, id(route))
        }
        What::RemoveCandidate {
            route_id,
            candidate_id,
        } => format!("remove {candidate_id} from {route_id}"),
        What::MoveCandidate { route_id, .. } => format!("move a candidate of {route_id}"),
        What::MoveRule { scope_label, .. } => format!("move a rewrite of {scope_label}"),
    }
}

fn noun(kind: &str) -> &str {
    match kind {
        "rewrites" => "rewrite",
        "routes" => "route",
        "presentations" => "presentation",
        "defaults" => "default",
        "classifiers" => "classifier",
        "upgrades" => "upgrade",
        other => other,
    }
}

/// What a move says of where the lifted row is.
fn move_reading(what: &What) -> Option<String> {
    match what {
        What::MoveRule {
            rules, from, to, ..
        } => Some(format!(
            "rule {} → {} of {}  [↑/↓] to put it where it goes",
            from + 1,
            to + 1,
            rules.len()
        )),
        What::MoveCandidate { ids, from, to, .. } => Some(format!(
            "candidate {} → {} of {}  [↑/↓] to put it where it goes",
            from + 1,
            to + 1,
            ids.len()
        )),
        _ => None,
    }
}

fn tone_of(note: Note) -> Tone {
    match note {
        Note::Reading => Tone::Plain,
        Note::Warning => Tone::Warn,
        Note::Refusal => Tone::Bad,
    }
}

/// `text` broken into lines of at most `width` cells, at spaces.
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.width() + 1 + word.width() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

fn guttered(spans: Vec<Span<'static>>, bar: &str) -> Vec<Span<'static>> {
    let mut line = vec![span(format!("{}{bar} ", " ".repeat(INDENT)), dim())];
    line.extend(spans);
    line
}

/// The form's lines, to sit in the table under its row, `width` cells wide.
pub(super) fn block(editing: &Editing, width: usize) -> Vec<BodyLine> {
    let mut lines = Vec::new();
    let line = |spans: Vec<Span<'static>>, bar: &str| {
        BodyLine::form(Line::from(guttered(spans, bar)), None, false)
    };
    lines.push(BodyLine::form(
        Line::from(vec![
            span(format!("{}┌ ", " ".repeat(INDENT)), dim()),
            span(heading(editing), Style::new().bold()),
        ]),
        None,
        false,
    ));
    let room = width.saturating_sub(INDENT + 2);
    if let Some(live) = editing.moved_to {
        lines.push(line(
            vec![toned(
                format!("revision {live} is now live; your edit is kept as you entered it"),
                Tone::Bad,
            )],
            "│",
        ));
    }
    if let Some(reading) = move_reading(&editing.what) {
        lines.push(line(vec![span(reading, dim())], "│"));
    }
    if matches!(
        editing.what,
        What::Remove { .. } | What::RemoveCandidate { .. }
    ) {
        lines.push(line(
            vec![span(
                "saving takes it out of the policy and makes a new revision live",
                dim(),
            )],
            "│",
        ));
    }

    let form_lines = editing.form.lines();
    let left = form_lines
        .iter()
        .filter(|form_line| form_line.row.is_some())
        .map(|form_line| form_line.text.width())
        .max()
        .unwrap_or(0)
        .clamp(20, (room * 55 / 100).max(20));
    for form_line in &form_lines {
        lines.extend(field_lines(form_line, left, room));
    }

    // What belongs to no field: the edit the daemon could not apply, and the
    // records it concerns.
    let mut loose: Vec<(String, Option<String>)> = Vec::new();
    for finding in editing.form.loose() {
        loose.push((finding.sentence, finding.fix));
    }
    if let Some(checked) = &editing.checked {
        loose.extend(checked.refusal.iter().map(|text| (text.clone(), None)));
        loose.extend(checked.elsewhere.iter().map(|other| {
            (
                format!("{}: {}", other.record, other.sentence),
                other.fix.clone(),
            )
        }));
    }
    if let Some(unchecked) = &editing.unchecked {
        loose.push((format!("it could not be checked: {unchecked}"), None));
    }
    for (sentence, fix) in loose {
        for text in wrap(&sentence, room.saturating_sub(9)) {
            lines.push(line(
                vec![toned("refused ", Tone::Bad), toned(text, Tone::Bad)],
                "│",
            ));
        }
        for text in fix.iter().flat_map(|fix| wrap(fix, room.saturating_sub(9))) {
            lines.push(line(
                vec![Span::raw("        "), toned(text, Tone::Bad)],
                "│",
            ));
        }
    }

    let moving = matches!(
        editing.what,
        What::MoveRule { .. } | What::MoveCandidate { .. }
    );
    let mut footer = vec![span(format!("{}└ ", " ".repeat(INDENT)), dim())];
    if moving {
        footer.push(Span::raw("[↑/↓] move   "));
    }
    footer.push(Span::raw(format!("[ctrl+s] {}", editing.save_label())));
    footer.push(Span::raw("   [esc] cancel"));
    lines.push(BodyLine::form(Line::from(footer), None, false));
    lines
}

/// One field of the form, with its notes beside it, wrapped under one another
/// where they take more than a line.
fn field_lines(form_line: &FormLine, left: usize, room: usize) -> Vec<BodyLine> {
    let reading_width = room.saturating_sub(left + 2);
    let mut readings: Vec<(Tone, String)> = Vec::new();
    for (note, text) in &form_line.notes {
        for piece in wrap(text, reading_width.max(12)) {
            readings.push((tone_of(*note), piece));
        }
    }
    let mut out = Vec::new();
    let first = readings.first().cloned();
    let cell = |text: &str| Cells::new().cell(vec![Span::raw(text.to_owned())], left);
    let mut spans = cell(&form_line.text).line().spans;
    if let Some((tone, text)) = first {
        spans.push(Span::raw("  "));
        spans.push(toned(text, tone));
    }
    out.push(BodyLine::form(
        Line::from(guttered(spans, "│")),
        form_line.row,
        form_line.focused,
    ));
    for (tone, text) in readings.into_iter().skip(1) {
        let mut spans = cell("").line().spans;
        spans.push(Span::raw("  "));
        spans.push(toned(text, tone));
        out.push(BodyLine::form(
            Line::from(guttered(spans, "│")),
            form_line.row,
            false,
        ));
    }
    out
}

// --- the preview ------------------------------------------------------------

/// How long before the window's end a request was made.
fn age(until_ms: i64, ts_ms: i64) -> String {
    let minutes = (until_ms - ts_ms).max(0) / 60_000;
    match minutes {
        0 => "now".to_owned(),
        1..=59 => format!("{minutes}m ago"),
        _ => format!("{}h ago", minutes / 60),
    }
}

/// Where a request went, and what caught it.
fn outcome(outcome: &Value) -> String {
    let matched = outcome.get("matched").unwrap_or(&Value::Null);
    let caught = match (text(matched, "kind"), text(matched, "id")) {
        ("", _) => String::new(),
        (kind, "") => kind.to_owned(),
        (kind, id) => format!("{kind} {id}"),
    };
    match text(outcome, "outcome") {
        "selected" => format!("{caught} → {}", text(outcome, "route_id")),
        "unavailable" => format!("{caught} → {} · unavailable", text(outcome, "route_id")),
        _ => format!("refused · {}", text(outcome, "failure").replace('_', " ")),
    }
}

/// The preview panel's lines: the changed requests, now and with the edit,
/// and how many could not be replayed.
pub(super) fn preview(editing: &Editing, width: usize) -> Vec<Line<'static>> {
    let Some(replay) = &editing.replay else {
        let waiting = if editing.validating || editing.checked.is_none() {
            "checking the edit, then replaying the last 24 hours of requests under it"
        } else {
            "recent requests are replayed once the edit is one the daemon accepts"
        };
        return vec![Line::styled(waiting.to_owned(), dim())];
    };
    let current = Value::Array(editing.operations());
    let mut lines = Vec::new();
    let mut head = if replay.changed_total == 0 {
        format!(
            "no recent request would change route ({} replayed)",
            replay.replayed
        )
    } else {
        format!(
            "{} of {} recent requests would change route",
            replay.changed_total, replay.replayed
        )
    };
    if replay.ops != current {
        head.push_str(" · for the last edit the daemon accepted");
    }
    lines.push(Line::raw(head));
    if !replay.changed.is_empty() {
        let request_w = 30.min(width / 4).max(12);
        let when_w = 9;
        let side = width.saturating_sub(request_w + when_w + 2) / 2;
        lines.push(
            Cells::new()
                .cell(vec![span("request", dim())], request_w)
                .cell(vec![span("when", dim())], when_w)
                .cell(vec![span("now", dim())], side)
                .cell(vec![span("with this change", dim())], side)
                .line(),
        );
        const SHOWN: usize = 6;
        for change in replay.changed.iter().take(SHOWN) {
            let request = change.get("request").unwrap_or(&Value::Null);
            let model = match text(request, "model") {
                "" => "-",
                model => model,
            };
            lines.push(
                Cells::new()
                    .cell(vec![Span::raw(model.to_owned())], request_w)
                    .cell(
                        vec![span(
                            age(
                                replay.until_ms,
                                change.get("ts_ms").and_then(Value::as_i64).unwrap_or(0),
                            ),
                            dim(),
                        )],
                        when_w,
                    )
                    .cell(
                        vec![Span::raw(outcome(
                            change.get("under_active").unwrap_or(&Value::Null),
                        ))],
                        side,
                    )
                    .cell(
                        vec![Span::raw(outcome(
                            change.get("under_edit").unwrap_or(&Value::Null),
                        ))],
                        side,
                    )
                    .line(),
            );
        }
        let hidden =
            (replay.changed_total as usize).saturating_sub(SHOWN.min(replay.changed.len()));
        if hidden > 0 {
            lines.push(Line::styled(format!("… {hidden} more"), dim()));
        }
    }
    if replay.unreplayable > 0 {
        lines.push(Line::styled(
            format!(
                "{} {} could not be replayed: the ledger did not record what routing was given",
                replay.unreplayable,
                if replay.unreplayable == 1 {
                    "request"
                } else {
                    "requests"
                }
            ),
            dim(),
        ));
    }
    if replay.truncated {
        lines.push(Line::styled(
            "older requests in the 24 hours were not read".to_owned(),
            dim(),
        ));
    }
    lines.into_iter().map(|line| clip(line, width)).collect()
}

fn clip(line: Line<'static>, width: usize) -> Line<'static> {
    Line::from(chrome::clip_spans(line.spans, width))
}
