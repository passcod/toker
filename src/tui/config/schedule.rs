//! The schedule page: where the ping schedule stands, and each slot with
//! its days, when its ping fires, and its newest run for each account it
//! pinged (spec: UTUI#schedule).
//!
//! The slots and the wake are the machine's, read from the installed units
//! and the system manager rather than from the service, so the page draws
//! them from [`super::model::Machine`] beside the service's snapshot.

use ratatui::{Frame, layout::Rect, text::Span};

use super::form;
use super::model::{Position, Run, Snapshot, State, Target};
use super::settings::head;
use crate::store::PingAction;
use crate::tui::chrome::RowHit;
use crate::tui::table::{self, BodyLine, Cells, Table, heading, span, widest};
use crate::tui::tone::{Tone, dim, toned};

/// What the position row reads: where the schedule stands, and why nothing
/// follows where that is so, with the way out of a wake that leads nowhere.
fn position(state: &State) -> Vec<Span<'static>> {
    let at = state.position();
    let mut spans = vec![Span::raw(at.label())];
    if let Some(why) = state.idle() {
        let said = match at {
            Position::Wake => format!("  wakes with nothing to ping: {why}"),
            _ => format!("  nothing to ping: {why}"),
        };
        spans.push(toned(said, Tone::Warn));
        if at == Position::Wake {
            spans.push(span("  x stops waking", dim()));
        }
    }
    spans
}

/// What a run did, and whether the boundary it predicted held.
fn outcome(run: &Run) -> Vec<Span<'static>> {
    let mut spans = vec![match run.did {
        Some(PingAction::Ping) => Span::raw("sent"),
        Some(PingAction::Skip) => Span::raw("skipped"),
        Some(PingAction::Failed) => toned("failed", Tone::Bad),
        None => span("-", dim()),
    }];
    match (run.verified, &run.predicted, &run.observed) {
        (Some(true), _, Some(observed)) => spans.push(Span::raw(format!("  matched {observed}"))),
        (Some(false), Some(predicted), Some(observed)) => spans.push(toned(
            format!("  missed {predicted} → {observed}"),
            Tone::Warn,
        )),
        _ => {}
    }
    spans
}

/// The columns a slot row carries, the optional ones shed as the page
/// narrows (spec: UTUI#what-is-shed-first).
struct Columns {
    days: usize,
    account: usize,
    /// The ping's fire time, which the slot's own time and the delay say
    /// anyway, so it goes first.
    ping: bool,
    /// When each run happened, which goes before what the run found.
    when: bool,
}

const PING: &str = "ping 00:00";

impl Columns {
    fn fit(width: usize, label: usize, days: usize, runs: &[Run]) -> Columns {
        let account = widest(runs.iter().map(|run| run.account.as_deref().unwrap_or("-")));
        let when = widest(runs.iter().map(|run| run.when.as_str()));
        let outcome = runs
            .iter()
            .map(|run| outcome(run).iter().map(Span::width).sum::<usize>())
            .max()
            .unwrap_or(0);
        let base = 2 + label + days + 2 + account + 2 + outcome;
        let mut columns = Columns {
            days,
            account,
            ping: true,
            when: true,
        };
        let ping = PING.len() + 3;
        let when = when + 2;
        if base + ping + when > width {
            columns.ping = false;
        }
        if base + when > width {
            columns.when = false;
        }
        columns
    }

    /// Where a run starts on its slot's row, so the runs under it line up.
    fn indent(&self, label: usize) -> usize {
        2 + label + self.days + 2 + if self.ping { PING.len() + 3 } else { 0 }
    }

    /// One run, as it reads beside its slot: when, whose window, and its
    /// outcome.
    fn run(&self, run: &Run) -> Vec<Span<'static>> {
        let account = run.account.clone().unwrap_or_else(|| "-".to_owned());
        let mut spans = Vec::new();
        if self.when {
            spans.push(Span::raw(format!("{}  ", run.when)));
        }
        spans.push(Span::raw(format!(
            "{account:<width$}  ",
            width = self.account
        )));
        spans.extend(outcome(run));
        spans
    }
}

pub(super) fn draw(
    frame: &mut Frame,
    body: Rect,
    state: &State,
    snapshot: &Snapshot,
) -> Vec<RowHit> {
    let width = usize::from(body.width.saturating_sub(4));
    let slots = state.slots();
    let runs = state
        .machine()
        .map(|machine| machine.runs.as_slice())
        .unwrap_or_default();
    let label_width = widest(["position", "00:00"]) + 2;
    let days = slots
        .iter()
        .map(|slot| slot.days.calendar())
        .collect::<Vec<_>>();
    let days_width = widest(days.iter().map(String::as_str));
    let columns = Columns::fit(width, label_width, days_width, runs);

    let rail = || span("│", dim());
    let form_under = |entry: usize, lines: &mut Vec<BodyLine>, target: Target| {
        if state.selected() == entry
            && let Some(editing) = state.editing()
            && editing.target == target
        {
            lines.extend(form::lines(
                editing,
                snapshot,
                vec![rail(), Span::raw("     ")],
                width,
            ));
        }
    };

    let mut lines: Vec<BodyLine> = Vec::new();
    lines.push(BodyLine::plain(head("schedule", 1, width)));
    let cells = Cells::new()
        .raw(vec![rail(), Span::raw(" ")])
        .cell(vec![Span::raw("position")], label_width)
        .raw(position(state));
    lines.push(BodyLine::of(cells.line(), 0));

    lines.push(BodyLine::plain(head("slots", slots.len(), width)));
    for (at, slot) in slots.iter().enumerate() {
        let entry = at + 1;
        let time = slot.time.hhmm();
        let mut mine = runs.iter().filter(|run| run.slot == time);
        let mut cells = Cells::new()
            .raw(vec![rail(), Span::raw(" ")])
            .cell(vec![Span::raw(time.clone())], label_width)
            .cell(vec![Span::raw(slot.days.calendar())], days_width + 2);
        if columns.ping {
            cells = cells.cell(
                vec![span(format!("ping {}", slot.fire().hhmm()), dim())],
                PING.len() + 3,
            );
        }
        cells = match mine.next() {
            Some(first) => cells.raw(columns.run(first)),
            None => cells.raw(vec![span("never run", dim())]),
        };
        lines.push(BodyLine::of(cells.line(), entry));
        // Under the slot, each other account's newest run, lined up with
        // the first.
        for other in mine {
            let mut spans = vec![
                rail(),
                Span::raw(" ".repeat(columns.indent(label_width) - 1)),
            ];
            spans.extend(columns.run(other));
            lines.push(BodyLine::plain(spans.into()));
        }
        form_under(entry, &mut lines, Target::Slot(Some(at)));
    }
    // A new slot's form opens at the foot of the list, where it will land
    // once sorted in.
    if let Some(editing) = state.editing()
        && editing.target == Target::Slot(None)
    {
        lines.extend(form::lines(
            editing,
            snapshot,
            vec![rail(), Span::raw("     ")],
            width,
        ));
    }
    if slots.is_empty() {
        lines.push(BodyLine::plain(
            vec![rail(), Span::raw(" "), span("none: n adds one", dim())].into(),
        ));
    }

    let header = Cells::new()
        .raw(vec![Span::raw("  ")])
        .cell(heading("setting"), label_width)
        .raw(heading("value"))
        .line();
    let table = Table {
        header,
        body: lines,
    };
    let split = table::split(body, 1 + table.body.len(), 0);
    table::draw_table(
        frame,
        split.table,
        "SCHEDULE",
        split.chrome,
        table,
        Some(state.selected()),
    )
    .rows
}
