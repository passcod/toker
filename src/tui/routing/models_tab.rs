//! The models tab: how a name is read, which routes move to a newer model
//! and on what evidence, where cold compactions go, and what each catalogue
//! offers (spec: UTUI#models).
//!
//! Four panels stacked, the offers taking the rows the others leave. One
//! panel holds the focus, and the focus moves on past a panel's edge.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::data::{array, count, counted, text};
use super::model::{Panel, State};
use super::routes::compaction_text;
use super::words::matcher_text;
use crate::tui::chrome::{self, Chrome};
use crate::tui::table::{Cells, controls, fit, heading, span, widest};
use crate::tui::tone::{Tone, dim, toned};

/// How many records the tab holds, for the strip: families, upgrades and the
/// compaction policy, if any.
pub(super) fn record_count(active: &Value) -> usize {
    families(active).len() + upgrades(active).len() + usize::from(compaction(active).is_some())
}

/// The families: what the classifiers read a name as.
pub(super) fn families(active: &Value) -> &[Value] {
    array(active, "classifiers")
}

pub(super) fn compaction(active: &Value) -> Option<&Value> {
    active.get("compaction").filter(|value| !value.is_null())
}

/// One row of the upgrades panel: an upgrade policy as one route's candidate
/// uses it.
pub(super) struct UpgradeRow<'a> {
    pub(super) policy_id: &'a str,
    pub(super) route_id: &'a str,
    pub(super) binding_id: &'a str,
    usage: &'a Value,
    policy: &'a Value,
}

impl UpgradeRow<'_> {
    /// The model the evidence bar is holding back, if it holds one.
    pub(super) fn held_back_model(&self) -> Option<&str> {
        array(self.usage, "held_back")
            .first()
            .map(|held| text(held, "model"))
            .filter(|model| !model.is_empty())
    }
}

/// The upgrades, a row for each place a policy is used.
pub(super) fn upgrades(active: &Value) -> Vec<UpgradeRow<'_>> {
    array(active, "upgrades")
        .iter()
        .flat_map(|policy| {
            array(policy, "used_by")
                .iter()
                .map(move |usage| UpgradeRow {
                    policy_id: text(policy, "id"),
                    route_id: text(usage, "route_id"),
                    binding_id: text(usage, "binding_id"),
                    usage,
                    policy,
                })
        })
        .collect()
}

/// The evidence a policy asks of a model before it moves a route there, with
/// what the page can say of how near the model is.
fn evidence(row: &UpgradeRow<'_>) -> String {
    let bar = row.usage.get("bar").unwrap_or(&Value::Null);
    let held = array(row.usage, "held_back").first();
    match text(bar, "kind") {
        "none" => "none".to_owned(),
        "distinct_days" => {
            let days = count(bar, "needs_days");
            match held {
                Some(held) => format!(
                    "at least {days} days, {} served {}",
                    text(held, "model"),
                    count(held, "distinct_days")
                ),
                None => format!("at least {days} days"),
            }
        }
        "explicit_promotion" => match held {
            Some(held) => format!("promotion, {} not promoted", text(held, "model")),
            None => "promotion".to_owned(),
        },
        "scaled" => match (
            bar.get("needs_days").and_then(Value::as_u64),
            bar.get("active_days").and_then(Value::as_u64),
        ) {
            (Some(needs), Some(active)) => format!("scaled: {needs} of {active} active days"),
            _ => "scaled".to_owned(),
        },
        _ => "-".to_owned(),
    }
}

/// What the upgrade would do now: from and to, or what holds it back.
fn move_now(row: &UpgradeRow<'_>) -> (String, Vec<Span<'static>>) {
    if let Some(mv) = array(row.usage, "moves").first() {
        let more = array(row.usage, "moves").len() - 1;
        let mut to = vec![Span::raw(format!("→ {}", text(mv, "to")))];
        if more > 0 {
            to.push(span(format!("  and {more} more"), dim()));
        }
        return (text(mv, "from").to_owned(), to);
    }
    if let Some(held) = array(row.usage, "held_back").first() {
        return (
            text(held, "model").to_owned(),
            vec![toned("held back", Tone::Warn)],
        );
    }
    (String::new(), vec![span("no newer model", dim())])
}

/// Lines of a panel, each marked where the selection rests on it.
type Lines = Vec<(Line<'static>, bool)>;

/// Draw a panel of lines: reversed where selected, each with a cell of air.
fn draw(frame: &mut Frame, area: Rect, title: &str, framing: Chrome, lines: Lines) {
    let inner = chrome::panel(frame, area, title, framing);
    let width = usize::from(inner.width);
    for (row, (line, selected)) in lines
        .into_iter()
        .take(usize::from(inner.height))
        .enumerate()
    {
        let mut spans = vec![Span::raw(" ")];
        spans.extend(chrome::clip_spans(line.spans, width.saturating_sub(2)));
        let used: usize = spans.iter().map(Span::width).sum();
        if selected {
            let reversed = Style::new().add_modifier(Modifier::REVERSED);
            spans = spans
                .into_iter()
                .map(|span| Span::styled(span.content, reversed))
                .collect();
            spans.push(Span::styled(
                " ".repeat(width.saturating_sub(used)),
                reversed,
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                y: inner.y + row as u16,
                height: 1,
                ..inner
            },
        );
    }
}

/// `rows` windowed to `room` around the selected one, with the counts of what
/// was left out.
fn windowed(rows: Lines, room: usize, selected: Option<usize>) -> Lines {
    let selected_at = selected
        .and_then(|selected| rows.iter().position(|(_, on)| *on).or(Some(selected)))
        .unwrap_or(0);
    let window = chrome::selection_window(rows.len(), room, selected_at);
    let mut out: Lines = Vec::new();
    if let Some(above) = window.above {
        out.push((Line::styled(format!("… {above} above"), dim()), false));
    }
    let shown = window.shown.clone();
    out.extend(
        rows.into_iter()
            .enumerate()
            .filter(|(index, _)| shown.contains(index))
            .map(|(_, row)| row),
    );
    if let Some(below) = window.below {
        out.push((Line::styled(format!("… {below} more"), dim()), false));
    }
    out
}

fn families_lines(active: &Value, focus: Option<usize>, width: usize) -> Lines {
    let families = families(active);
    let name_w = widest(
        families
            .iter()
            .map(|family| text(family, "id"))
            .chain(["family"]),
    ) + 4;
    let matches: Vec<String> = families
        .iter()
        .map(|family| matcher_text(family.get("matcher").unwrap_or(&Value::Null)))
        .collect();
    let examples: Vec<&str> = families
        .iter()
        .map(|family| {
            array(family, "parsed")
                .first()
                .map_or("-", |parsed| text(parsed, "model"))
        })
        .collect();
    let matches_w = (widest(matches.iter().map(String::as_str).chain(["matches"])) + 12).max(32);
    let example_w = (widest(examples.iter().copied().chain(["example"])) + 12).max(30);
    let _ = width;
    let mut lines: Lines = vec![(
        Cells::new()
            .cell(heading("family"), name_w)
            .cell(heading("matches"), matches_w)
            .cell(heading("example"), example_w)
            .raw(heading("reads as"))
            .line(),
        false,
    )];
    for (index, family) in families.iter().enumerate() {
        let reads = array(family, "parsed")
            .first()
            .map(|parsed| format!("{} · {}", text(parsed, "family"), text(parsed, "version")))
            .unwrap_or_else(|| "-".to_owned());
        lines.push((
            Cells::new()
                .cell(vec![Span::raw(text(family, "id").to_owned())], name_w)
                .cell(vec![Span::raw(matches[index].clone())], matches_w)
                .cell(vec![Span::raw(examples[index].to_owned())], example_w)
                .raw(vec![Span::raw(reads)])
                .line(),
            focus == Some(index),
        ));
    }
    lines
}

fn upgrades_lines(active: &Value, focus: Option<usize>) -> Lines {
    let rows = upgrades(active);
    let moves: Vec<(String, Vec<Span<'static>>)> = rows.iter().map(move_now).collect();
    let route_w = widest(rows.iter().map(|row| row.route_id)) + 2;
    let from_w = widest(moves.iter().map(|(from, _)| from.as_str())) + 2;
    let to_w = moves
        .iter()
        .map(|(_, to)| to.iter().map(Span::width).sum::<usize>())
        .max()
        .unwrap_or(0)
        + 5;
    let policies: std::collections::BTreeSet<&str> = rows.iter().map(|row| row.policy_id).collect();
    let head = match (policies.len(), policies.iter().next()) {
        (1, Some(policy)) => format!("{policy}  {}", counted(rows.len(), "upgrade", "upgrades")),
        _ => counted(rows.len(), "upgrade", "upgrades"),
    };
    let mut lines: Lines = vec![(
        Cells::new()
            .cell(vec![Span::raw(head)], route_w + from_w + to_w)
            .raw(heading("evidence bar"))
            .line(),
        false,
    )];
    for (index, row) in rows.iter().enumerate() {
        let (from, to) = &moves[index];
        lines.push((
            Cells::new()
                .cell(vec![Span::raw(row.route_id.to_owned())], route_w)
                .cell(vec![Span::raw(from.clone())], from_w)
                .cell(to.clone(), to_w)
                .raw(vec![Span::raw(evidence(row))])
                .line(),
            focus == Some(index),
        ));
    }
    let _ = rows.iter().map(|row| row.policy);
    lines
}

fn compaction_line(active: &Value, focused: bool) -> Lines {
    let Some(compaction) = compaction(active) else {
        return vec![(Line::from(span("no compaction policy", dim())), false)];
    };
    let on = match text(compaction, "kind") {
        "substitution" => {
            // The providers the bindings are on, each once: the line says
            // where cold compactions are moved, not which endpoint.
            let mut providers: Vec<&str> = Vec::new();
            for binding in array(compaction, "bindings") {
                let provider = text(binding, "provider");
                if !providers.contains(&provider) {
                    providers.push(provider);
                }
            }
            format!("on {}", providers.join(" · "))
        }
        _ => format!("on {}", text(compaction, "route_id")),
    };
    let said = compaction_text(compaction);
    // The sentence the route page uses, split into what it sends and how it
    // treats the cache.
    let (target, cache) = said.split_once(" · cache ").unwrap_or((&said, ""));
    vec![(
        Cells::new()
            .cell(vec![Span::raw("cold compactions")], 22)
            .cell(
                vec![Span::raw(
                    target
                        .strip_prefix("cold compactions ")
                        .unwrap_or(target)
                        .to_owned(),
                )],
                29,
            )
            .cell(vec![Span::raw(on.clone())], (on.width() + 2).max(32))
            .raw(vec![Span::raw(format!("cache {cache}"))])
            .line(),
        focused,
    )]
}

fn offer_lines(state: &State) -> (String, Vec<Line<'static>>) {
    let offers = state.offers_shown();
    let bindings = state.offer_bindings();
    let binding = bindings
        .get(
            state
                .models()
                .offers_binding
                .min(bindings.len().saturating_sub(1)),
        )
        .copied()
        .unwrap_or("-");
    let head = format!("{binding}  {}", counted(offers.len(), "offer", "offers"));
    let lines = offers
        .iter()
        .map(|offer| {
            let window = offer
                .get("context_window")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".to_owned(), crate::tui::view::short_tokens);
            let since = offer
                .get("chronology_ms")
                .and_then(Value::as_i64)
                .map_or_else(String::new, month);
            Cells::new()
                .cell(vec![Span::raw(text(offer, "model_id").to_owned())], 43)
                .right(vec![Span::raw(window)], 6)
                .cell(Vec::new(), 3)
                .raw(vec![Span::raw(since)])
                .line()
        })
        .collect();
    (head, lines)
}

/// A moment as `2024-01`: when a model was released.
fn month(unix_ms: i64) -> String {
    let days = unix_ms.div_euclid(86_400_000);
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year}-{month:02}")
}

const FAMILY_CONTROLS: [(&str, &str); 3] = [("e", "edit"), ("n", "new"), ("d", "delete")];
const COMPACTION_CONTROLS: [(&str, &str); 3] = [("e", "edit"), ("n", "new"), ("d", "delete")];
const UPGRADE_CONTROLS: [(&str, &str); 4] = [
    ("e", "edit"),
    ("n", "new"),
    ("d", "delete"),
    ("p", "promote"),
];

/// The fewest rows the offers get: the head and a couple of offers.
const OFFERS_FLOOR: usize = 4;

/// Draw the tab into `body`.
pub(super) fn render(frame: &mut Frame, body: Rect, state: &State, active: &Value) {
    let models = state.models();
    let width = usize::from(body.width.saturating_sub(4));
    let in_families = models.focus == Panel::Families;
    let in_upgrades = models.focus == Panel::Upgrades;
    let mut family = families_lines(active, in_families.then_some(models.family), width);
    let mut upgrade = upgrades_lines(active, in_upgrades.then_some(models.upgrade));
    if in_families {
        family.push((controls(&FAMILY_CONTROLS), false));
    }
    if in_upgrades {
        upgrade.push((controls(&UPGRADE_CONTROLS), false));
    }
    let in_compaction = models.focus == Panel::Compaction;
    let mut compaction = compaction_line(active, in_compaction);
    if in_compaction {
        compaction.push((controls(&COMPACTION_CONTROLS), false));
    }

    // Boxed while every panel keeps its content inside its border.
    let height = usize::from(body.height);
    let content = family.len() + upgrade.len() + compaction.len();
    let framing = if content + OFFERS_FLOOR + 8 <= height {
        Chrome::Boxed
    } else {
        Chrome::Ruled
    };
    let cost = usize::from(framing.cost());
    let room = height.saturating_sub(4 * cost + OFFERS_FLOOR + compaction.len());
    // The first two panels give up rows, past the least, before the offers do.
    let (family_room, upgrade_room) = if family.len() + upgrade.len() <= room {
        (family.len(), upgrade.len())
    } else {
        let family_room = family.len().min(room / 2).max(2);
        (family_room, room.saturating_sub(family_room).max(2))
    };
    let mut y = body.y;
    let mut place = |rows: usize| {
        let rows = (rows + cost).min(usize::from(body.bottom().saturating_sub(y))) as u16;
        let area = Rect {
            y,
            height: rows,
            ..body
        };
        y += rows;
        area
    };
    let family_area = place(family_room);
    let upgrade_area = place(upgrade_room);
    let compaction_area = place(compaction.len());
    let offers_area = Rect {
        y,
        height: body.bottom().saturating_sub(y),
        ..body
    };

    // Panels keep a heading row and the controls row when they are cut; the
    // heading first, the entries after.
    let cut = |mut rows: Lines, room: usize, keep_last: bool| -> Lines {
        if rows.len() <= room {
            return rows;
        }
        let head = rows.remove(0);
        let last = keep_last.then(|| rows.pop()).flatten();
        let entries_room = room.saturating_sub(1 + usize::from(last.is_some()));
        let mut out = vec![head];
        out.extend(windowed(rows, entries_room, None));
        out.extend(last);
        out
    };
    draw(
        frame,
        family_area,
        "FAMILIES",
        framing,
        cut(family, family_room, in_families),
    );
    draw(
        frame,
        upgrade_area,
        "UPGRADES",
        framing,
        cut(upgrade, upgrade_room, in_upgrades),
    );
    draw(frame, compaction_area, "COMPACTION", framing, compaction);

    let (head, offers) = offer_lines(state);
    let inner_rows = usize::from(offers_area.height).saturating_sub(cost);
    let list_room = inner_rows.saturating_sub(1);
    let window = chrome::list_window(offers.len(), list_room, models.offers_at);
    let mut lines: Lines = vec![(Line::from(span(head, Style::new())), false)];
    if let Some(above) = window.above {
        lines.push((Line::styled(format!("… {above} above"), dim()), false));
    }
    lines.extend(
        offers
            .into_iter()
            .enumerate()
            .filter(|(index, _)| window.shown.contains(index))
            .map(|(_, line)| (line, false)),
    );
    if let Some(below) = window.below {
        lines.push((Line::styled(format!("… {below} more"), dim()), false));
    }
    if models.focus == Panel::Offers && state.offer_bindings().len() > 1 {
        // The controls take the last row, ahead of the count of what is left.
        if lines.len() >= inner_rows && inner_rows > 0 {
            lines.truncate(inner_rows - 1);
        }
        lines.push((controls(&[("←/→", "other catalogues")]), false));
    }
    draw(frame, offers_area, "OFFERS", framing, lines);
    let _ = fit;
}

#[cfg(test)]
mod tests {
    use super::month;

    #[test]
    fn a_release_is_dated_to_its_month() {
        // 2024-01-15T00:00:00Z and the first and last days either side.
        assert_eq!(month(1_705_276_800_000), "2024-01");
        assert_eq!(month(1_704_067_200_000), "2024-01");
        assert_eq!(month(1_706_745_599_000), "2024-01");
        assert_eq!(month(1_706_745_600_000), "2024-02");
        assert_eq!(month(0), "1970-01");
    }
}
