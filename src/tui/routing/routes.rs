//! The routes tab and the route page (spec: UTUI#routes).
//!
//! A route is read in the direction a request travels: the candidates it is
//! sent to with the edges between them, the blocks that handle it, and what
//! sends a request to it.

use ratatui::text::{Line, Span};
use serde_json::Value;

use super::data::{self, array, counted, flag, text};
use super::table::{BodyLine, Cells, Table, controls, controls_in, field, heading, span, widest};
use super::words::{
    Tone, bold, cache_words, cost_word, dim, edge_parts, label, lane_words, lanes_phrase,
    matched_by_phrase, matcher_text, reach_spans, reach_text_in, reason_word, toned,
};
use unicode_width::UnicodeWidthStr;

/// The route at `id`'s place in the routes tab.
pub(super) fn position(active: &Value, id: &str) -> Option<usize> {
    array(active, "routes")
        .iter()
        .position(|route| text(route, "id") == id)
}

/// The routes table: each route with the providers its candidates reach in
/// order, what refers to it, the lanes it holds and its reach.
pub(super) fn routes_table(active: &Value, width: usize) -> Table {
    let routes = array(active, "routes");
    let sent_to = |route: &Value| {
        array(route, "providers")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" → ")
    };
    let matched =
        |route: &Value| matched_by_phrase(route.get("matched_by_counts").unwrap_or(&Value::Null));
    let lanes = |route: &Value| lanes_phrase(route.get("pinned_lanes").unwrap_or(&Value::Null));
    let natural = [
        widest(
            routes
                .iter()
                .map(|route| text(route, "id"))
                .chain(["route"]),
        ),
        widest(
            routes
                .iter()
                .map(sent_to)
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .chain(["sent to"]),
        ),
        widest(
            routes
                .iter()
                .map(matched)
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .chain(["matched by"]),
        ),
        widest(
            routes
                .iter()
                .map(lanes)
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .chain(["lanes"]),
        ),
    ];
    let reach_nat = widest(
        routes
            .iter()
            .map(|route| reach_text_in(route.get("reach").unwrap_or(&Value::Null), false))
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str)
            .chain(["reach"]),
    );
    // Two cells between columns; one where that is too much, and then the
    // columns the detail says again go, what refers to the route first and
    // the providers it reaches after (spec: UTUI#what-is-shed-first).
    let shown = [
        ([true; 4], 2),
        ([true; 4], 1),
        ([true, true, false, true], 1),
        ([true, false, false, true], 1),
    ];
    let (keep, gap) = shown
        .into_iter()
        .find(|(keep, gap)| {
            let used: usize = natural
                .iter()
                .zip(keep)
                .filter(|(_, keep)| **keep)
                .map(|(width, _)| width + gap)
                .sum();
            used + reach_nat <= width
        })
        .unwrap_or(([true, false, false, true], 1));
    let widths: Vec<usize> = natural
        .iter()
        .zip(keep)
        .map(|(width, keep)| if keep { width + gap } else { 0 })
        .collect();
    let reach_w = width.saturating_sub(widths.iter().sum());
    let heads = ["route", "sent to", "matched by", "lanes"];
    let mut header = Cells::new();
    for (head, width) in heads.iter().zip(&widths) {
        if *width > 0 {
            header = header.cell(heading(head), *width);
        }
    }
    let header = header.cell(heading("reach"), reach_w).line();
    let body = routes
        .iter()
        .enumerate()
        .map(|(entry, route)| {
            let lanes = lanes(route);
            let said = [
                vec![Span::raw(text(route, "id").to_owned())],
                vec![Span::raw(sent_to(route))],
                vec![Span::raw(matched(route))],
                vec![if lanes == "none" {
                    toned(lanes, Tone::Dim)
                } else {
                    Span::raw(lanes)
                }],
            ];
            let mut line = Cells::new();
            for (said, width) in said.into_iter().zip(&widths) {
                if *width > 0 {
                    line = line.cell(said, *width);
                }
            }
            let line = line
                .cell(
                    reach_spans(route.get("reach").unwrap_or(&Value::Null)),
                    reach_w,
                )
                .line();
            BodyLine::of(line, entry)
        })
        .collect();
    Table { header, body }
}

/// The document's route, for what compilation drops from the derived one:
/// the model each candidate asks for.
fn document_route<'a>(snapshot: &'a Value, id: &str) -> Option<&'a Value> {
    array(data::policy(snapshot)?, "routes")
        .iter()
        .find(|route| text(route, "id") == id)
}

/// A candidate's model as the route page reads it: an alias where one
/// applies, otherwise what the candidate asks the provider for.
fn model_reading(candidate: &Value, document: Option<&Value>, handled: Option<&Value>) -> String {
    let id = text(candidate, "id");
    let alias = handled
        .and_then(|handled| handled.get("alias"))
        .map(|alias| array(alias, "candidates"))
        .unwrap_or(&[])
        .iter()
        .find(|alias| text(alias, "candidate_id") == id)
        .and_then(|alias| array(alias, "rules").first());
    if let Some(rule) = alias {
        return format!(
            "alias {} → {}",
            matcher_text(rule.get("match").unwrap_or(&Value::Null)),
            text(rule, "to")
        );
    }
    let model = document
        .and_then(|route| route.get("chain"))
        .map(|chain| {
            let mut candidates = vec![chain.get("first")];
            candidates.extend(
                array(chain, "steps")
                    .iter()
                    .map(|step| step.get("candidate")),
            );
            candidates
        })
        .unwrap_or_default()
        .into_iter()
        .flatten()
        .find(|document| text(document, "id") == id)
        .and_then(|document| document.get("model"));
    match model.map(|model| (text(model, "kind"), model)) {
        Some(("requested_model", _)) | None => "as asked".to_owned(),
        Some(("exact", model)) => text(model, "model_id").to_owned(),
        Some(("template", model)) => text(model, "template").to_owned(),
        Some(_) => "a model the selector picks".to_owned(),
    }
}

/// How a candidate stands: what the route's walk found of it, else what
/// the provider's availability says.
fn candidate_state(snapshot: &Value, route: &Value, candidate: &Value) -> (String, Tone) {
    let id = text(candidate, "id");
    let reach = route.get("reach").unwrap_or(&Value::Null);
    if !flag(candidate, "enabled") {
        return ("disabled".to_owned(), Tone::Dim);
    }
    if let Some(skip) = array(reach, "skipped")
        .iter()
        .find(|skip| text(skip, "candidate_id") == id)
    {
        let word = reason_word(text(skip, "reason"));
        let tone = if word == "off" { Tone::Dim } else { Tone::Warn };
        return (word.to_owned(), tone);
    }
    if reach
        .get("serving")
        .is_some_and(|serving| text(serving, "candidate_id") == id)
    {
        return ("ok".to_owned(), Tone::Plain);
    }
    let live = snapshot
        .get("derived")
        .map(|derived| array(derived, "availability"))
        .unwrap_or(&[])
        .iter()
        .find(|live| text(live, "binding_id") == text(candidate, "binding_id"));
    let state = |name: &str| {
        live.and_then(|live| live.get(name))
            .map(|evidence| text(evidence, "state"))
    };
    match (state("enabled"), state("configured"), state("credential")) {
        (None, ..) => ("-".to_owned(), Tone::Dim),
        (Some("no"), ..) => ("off".to_owned(), Tone::Dim),
        (_, Some("no"), _) => ("not configured".to_owned(), Tone::Warn),
        (_, _, Some("no")) => ("no cred".to_owned(), Tone::Warn),
        _ => ("ok".to_owned(), Tone::Plain),
    }
}

/// The candidates of a route, each a line and then the edge to the next
/// drawn under it, in the direction a request travels. `indent` is how far
/// in the lines start.
///
/// An edge too long for `width` says what it consents to on a line of its own
/// under when it falls back.
pub(super) fn candidate_lines(snapshot: &Value, route: &Value, width: usize) -> Vec<Line<'static>> {
    let candidates = array(route, "candidates");
    let document = document_route(snapshot, text(route, "id"));
    let handled = route.get("handled_by");
    let readings: Vec<String> = candidates
        .iter()
        .map(|candidate| model_reading(candidate, document, handled))
        .collect();
    let states: Vec<(String, Tone)> = candidates
        .iter()
        .map(|candidate| candidate_state(snapshot, route, candidate))
        .collect();
    let binding_nat = widest(
        candidates
            .iter()
            .map(|candidate| text(candidate, "binding_id")),
    );
    let cost_nat = widest(
        candidates
            .iter()
            .map(|candidate| cost_word(text(candidate, "cost_class"))),
    );
    let model_nat = widest(readings.iter().map(String::as_str));
    let state_nat = widest(states.iter().map(|(state, _)| state.as_str()));
    // The widths read from the room there is: roomy, then tighter, then as
    // tight as a cell between columns allows, before anything is cut.
    let (binding_w, cost_w, model_w) = [(2, 16, 5), (2, cost_nat + 2, 2), (1, cost_nat + 1, 1)]
        .into_iter()
        .map(|(binding, cost, model)| (binding_nat + binding, cost, model_nat + model))
        .find(|(binding, cost, model)| 3 + binding + cost + model + state_nat <= width)
        .unwrap_or((binding_nat + 1, cost_nat + 1, model_nat + 1));
    let mut lines = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let (state, tone) = states[index].clone();
        let cost = cost_word(text(candidate, "cost_class")).to_owned();
        lines.push(
            Cells::new()
                .cell(vec![Span::raw(format!("{}", index + 1))], 3)
                .cell(
                    vec![Span::raw(text(candidate, "binding_id").to_owned())],
                    binding_w,
                )
                .cell(vec![Span::raw(cost)], cost_w)
                .cell(vec![Span::raw(readings[index].clone())], model_w)
                .raw(vec![toned(state, tone)])
                .line(),
        );
        if let Some(edge) = array(route, "edges").get(index) {
            for (at, said) in edge_lines(edge, width).into_iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::raw(if at == 0 { "   " } else { "     " }),
                    Span::raw(said),
                ]));
            }
        }
    }
    lines
}

/// The lines an edge is said in: one where it fits `width`, otherwise when
/// it falls back and what it consents to, one under the other.
fn edge_lines(edge: &Value, width: usize) -> Vec<String> {
    let (when, consents) = edge_parts(edge);
    if 3 + when.width() + 3 + consents.width() > width {
        vec![when, consents]
    } else {
        vec![format!("{when}   {consents}")]
    }
}

/// Where each candidate's line is, counted from the first's: an edge between
/// them takes the lines it is said in.
pub(super) fn candidate_rows(route: &Value, width: usize) -> Vec<usize> {
    let mut rows = Vec::new();
    let mut at = 0;
    for index in 0..array(route, "candidates").len() {
        rows.push(at);
        at += 1 + array(route, "edges")
            .get(index)
            .map_or(0, |edge| edge_lines(edge, width).len());
    }
    rows
}

const ROUTE_CONTROLS: [(&str, &str); 5] = [
    ("e", "edit"),
    ("n", "new"),
    ("d", "delete"),
    ("t", "trace"),
    ("enter", "open route"),
];

/// The detail beneath the routes table: the route's candidates with the
/// conditions on each edge between them.
pub(super) fn route_detail(snapshot: &Value, route: &Value, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(span(text(route, "id").to_owned(), bold()))];
    lines.extend(candidate_lines(snapshot, route, width));
    lines.push(controls(&ROUTE_CONTROLS));
    lines
}

// --- the route page -------------------------------------------------------

const PAGE_CONTROLS: [(&str, &str); 7] = [
    ("e", "edit"),
    ("n", "add fallback"),
    ("m", "move"),
    ("d", "remove"),
    ("t", "trace"),
    ("←/→", "other routes"),
    ("esc", "back"),
];

/// The same in fewer words, for where the full ones do not fit a line.
const PAGE_CONTROLS_BRIEF: [(&str, &str); 7] = [
    ("e", "edit"),
    ("n", "add"),
    ("m", "move"),
    ("d", "remove"),
    ("t", "trace"),
    ("←/→", "routes"),
    ("esc", "back"),
];

/// The blocks that handle a request on a route, in the order the page names
/// them: each with whether it is on.
const BLOCKS: [(&str, &str); 7] = [
    ("upgrade", "upgrade"),
    ("lane transition", "lane_transition"),
    ("compaction", "compaction"),
    ("alias", "alias"),
    ("server safeguards", "server_safeguards"),
    ("required capabilities", "required_capabilities"),
    ("health circuits", "health_circuits"),
];

/// The top of the route page: its name and place, and the candidates it is
/// sent to, which is where a candidate is edited. Each candidate's line is
/// where [`candidate_rows`] puts it past [`CANDIDATES_AT`], the edge to the
/// next being said between them.
pub(super) fn route_top(
    snapshot: &Value,
    route: &Value,
    place: usize,
    total: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let id = text(route, "id");
    let mut lines = Vec::new();
    let place = format!("{} of {total}", place + 1);
    let gap = width.saturating_sub(id.chars().count() + place.chars().count());
    lines.push(Line::from(vec![
        span(id.to_owned(), bold()),
        Span::raw(" ".repeat(gap)),
        span(place, dim()),
    ]));
    lines.push(Line::raw(""));

    let candidates = array(route, "candidates").len();
    lines.push(field(
        "sent to",
        "sent to".len() + 2,
        vec![Span::raw(counted(candidates, "candidate", "candidates"))],
    ));
    lines.extend(candidate_lines(snapshot, route, width));
    lines
}

/// `route` with its candidate at `from` put at `to`, as a move in progress
/// shows it. The edges stay where they are: they lie between places, not
/// candidates.
pub(super) fn with_move(route: &Value, from: usize, to: usize) -> Value {
    let mut moved = route.clone();
    if let Some(candidates) = moved.get_mut("candidates").and_then(Value::as_array_mut)
        && from < candidates.len()
        && to < candidates.len()
    {
        let lifted = candidates.remove(from);
        candidates.insert(to, lifted);
    }
    moved
}

/// Where the first candidate's line is in [`route_top`].
pub(super) const CANDIDATES_AT: usize = 3;

/// The rest of the route page: what handles a request on the route and what
/// sends one to it, with the controls.
pub(super) fn route_rest(route: &Value, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::raw("")];
    let handled = route.get("handled_by").unwrap_or(&Value::Null);
    let on = |key: &str| handled.get(key).is_some_and(|block| flag(block, "enabled"));
    let (enabled, disabled): (Vec<_>, Vec<_>) = BLOCKS.iter().partition(|(_, key)| on(key));
    let mut head = vec![Span::raw(format!(
        "{} on · {} off",
        enabled.len(),
        disabled.len()
    ))];
    if !disabled.is_empty() {
        head.push(Span::raw(": "));
        for (index, (name, _)) in disabled.iter().enumerate() {
            if index > 0 {
                head.push(span(" · ", dim()));
            }
            head.push(span((*name).to_owned(), dim()));
        }
    }
    lines.push(field("handled by", "handled by".len() + 2, head));
    let name_w = 17;
    for (name, key) in &enabled {
        lines.push(Line::from(vec![
            Span::raw("  "),
            span(format!("{name:<name_w$}"), dim()),
            Span::raw(block_text(route, handled, key)),
        ]));
    }
    lines.push(Line::raw(""));

    let referrers = array(route, "matched_by");
    lines.push(field(
        "matched by",
        "matched by".len() + 2,
        vec![Span::raw(matched_by_phrase(
            route.get("matched_by_counts").unwrap_or(&Value::Null),
        ))],
    ));
    let names: Vec<String> = referrers.iter().map(referrer_name).collect();
    let kind_w = widest(referrers.iter().map(|referrer| text(referrer, "kind"))) + 2;
    let name_w = widest(names.iter().map(String::as_str)) + 3;
    for (referrer, name) in referrers.iter().zip(&names) {
        let what = match text(referrer, "kind") {
            "rewrite" => matcher_text(referrer.get("matcher").unwrap_or(&Value::Null)),
            "presentation" => array(referrer, "slugs")
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
        let mut cells = Cells::new()
            .cell(Vec::new(), 2)
            .cell(vec![label(text(referrer, "kind").to_owned())], kind_w);
        cells = if what.is_empty() {
            cells.raw(vec![Span::raw(name.clone())])
        } else {
            cells
                .cell(vec![Span::raw(name.clone())], name_w)
                .raw(vec![Span::raw(what)])
        };
        lines.push(cells.line());
    }
    let whole = controls_in(&PAGE_CONTROLS, width);
    lines.extend(if whole.len() == 1 {
        whole
    } else {
        controls_in(&PAGE_CONTROLS_BRIEF, width)
    });
    lines
}

/// A referrer's scope and, where it has a place in it, its place.
fn referrer_name(referrer: &Value) -> String {
    match referrer.get("position").and_then(Value::as_u64) {
        Some(position) => format!("{} {position}", text(referrer, "scope_label")),
        None => text(referrer, "scope_label").to_owned(),
    }
}

/// What an enabled block says on the route page.
fn block_text(route: &Value, handled: &Value, key: &str) -> String {
    let block = handled.get(key).unwrap_or(&Value::Null);
    match key {
        "upgrade" => {
            let policies: Vec<&str> = array(block, "candidates")
                .iter()
                .filter(|candidate| flag(candidate, "enabled"))
                .map(|candidate| text(candidate, "policy_id"))
                .collect();
            let mut said = format!("{} on {}", policies.join(" · "), text(route, "id"));
            let first = array(block, "moves").iter().find_map(|usage| {
                array(usage, "moves")
                    .first()
                    .or_else(|| array(usage, "held_back").first())
            });
            if let Some(mv) = first {
                if mv.get("to").is_some() {
                    said.push_str(&format!(" · {} → {}", text(mv, "from"), text(mv, "to")));
                } else {
                    said.push_str(&format!(
                        " · {} held back by its evidence bar",
                        text(mv, "model")
                    ));
                }
            }
            said
        }
        "lane_transition" => {
            let lanes = lanes_phrase(route.get("pinned_lanes").unwrap_or(&Value::Null));
            let words = lane_words(text(block, "value"));
            if lanes == "none" {
                words.to_owned()
            } else {
                format!("{words} · {lanes}")
            }
        }
        "compaction" => compaction_text(block),
        "alias" => array(block, "candidates")
            .iter()
            .map(|candidate| {
                let rules = array(candidate, "rules")
                    .iter()
                    .map(|rule| {
                        format!(
                            "{} → {}",
                            matcher_text(rule.get("match").unwrap_or(&Value::Null)),
                            text(rule, "to")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                format!("{rules} · on {}", text(candidate, "provider"))
            })
            .collect::<Vec<_>>()
            .join("; "),
        "server_safeguards" => array(block, "candidates")
            .iter()
            .map(|candidate| {
                format!(
                    "{} {}",
                    text(candidate, "binding_id"),
                    candidate
                        .get("support")
                        .and_then(Value::as_str)
                        .unwrap_or("unreported")
                )
            })
            .collect::<Vec<_>>()
            .join(" · "),
        "required_capabilities" => array(block, "candidates")
            .iter()
            .map(|candidate| {
                format!(
                    "{}: {}",
                    text(candidate, "candidate_id"),
                    array(candidate, "capabilities")
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|name| name.replace('_', " "))
                        .collect::<Vec<_>>()
                        .join(" · ")
                )
            })
            .collect::<Vec<_>>()
            .join("; "),
        "health_circuits" => array(block, "candidates")
            .iter()
            .flat_map(|candidate| {
                array(candidate, "circuits").iter().map(move |circuit| {
                    format!(
                        "{}: {} opens for {}s",
                        text(candidate, "candidate_id"),
                        text(circuit, "failure").replace('_', " "),
                        circuit.get("seconds").and_then(Value::as_u64).unwrap_or(0)
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("; "),
        _ => String::new(),
    }
}

/// Where cold compactions go, with how their cache is treated.
pub(super) fn compaction_text(block: &Value) -> String {
    let target = match text(block, "kind") {
        "route" => "→ their own route".to_owned(),
        _ => match block.get("model") {
            Some(model) if !model.is_null() => {
                format!("cold compactions → {}", text(model, "value"))
            }
            _ => "cold compactions keep their model".to_owned(),
        },
    };
    format!(
        "{target} · cache {}",
        cache_words(text(block, "cache_transition"))
    )
}
