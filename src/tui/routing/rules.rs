//! The three lookup tables a request walks before it reaches a route:
//! presentations, rewrites and defaults (spec: UTUI#reading-a-table).
//!
//! Each is a table grouped by scope, with the selected record's detail
//! beneath it. The daemon derived every fact in them; this lays them out.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde_json::Value;

use super::data::{array, counted, text};
use super::model::{Tab, Target};
use super::table::{BodyLine, Cells, Table, controls, field, heading, span, widest, width_of};
use super::words::{
    Tone, bold, dim, hits_phrase, hits_span, label, matcher_kind, matcher_text, no_reach,
    reach_reason, reach_spans, reach_text, toned,
};

/// A record the selection can rest on in a lookup table.
#[derive(Debug, Clone, Copy)]
pub(super) enum Entry<'a> {
    Presentation(&'a Value),
    Rewrite(&'a Value),
    /// A default, or the want of one where a protocol has none.
    Default(&'a Value),
}

impl<'a> Entry<'a> {
    pub(super) fn record(&self) -> &'a Value {
        match self {
            Entry::Presentation(record) | Entry::Rewrite(record) | Entry::Default(record) => record,
        }
    }

    /// The route the record sends to, where it names one.
    pub(super) fn route_id(&self) -> Option<&'a str> {
        let record = self.record();
        match self {
            Entry::Presentation(_) => array(record, "route_ids").first().and_then(Value::as_str),
            _ => Some(text(record, "route_id")).filter(|id| !id.is_empty()),
        }
    }

    /// The document's record this entry is, for an edit.
    pub(super) fn target(&self) -> Option<Target> {
        let record = self.record();
        match self {
            Entry::Presentation(_) => Some(Target {
                kind: "presentations",
                id: Some(text(record, "id").to_owned()),
                scope: None,
            }),
            Entry::Rewrite(_) => Some(Target {
                kind: "rewrites",
                id: Some(text(record, "id").to_owned()),
                scope: None,
            }),
            Entry::Default(_) => {
                (record.get("exists") == Some(&Value::Bool(true))).then(|| Target {
                    kind: "defaults",
                    id: None,
                    scope: record.get("scope").cloned(),
                })
            }
        }
    }
}

/// The entries of a lookup tab, in the order its table draws them.
pub(super) fn entries<'a>(active: &'a Value, tab: Tab) -> Vec<Entry<'a>> {
    match tab {
        Tab::Presentations => array(active, "presentation_records")
            .iter()
            .map(Entry::Presentation)
            .collect(),
        Tab::Rewrites => rewrite_groups(active)
            .into_iter()
            .flat_map(|group| {
                let rules = group.rules.into_iter().map(Entry::Rewrite);
                let ends = group.ends.into_iter().filter_map(|end| match end {
                    End::Default(record) => Some(Entry::Default(record)),
                    End::Pass(_) => None,
                });
                rules.chain(ends).collect::<Vec<_>>()
            })
            .collect(),
        Tab::Defaults => array(active, "defaults")
            .iter()
            .map(Entry::Default)
            .collect(),
        Tab::Routes | Tab::Models | Tab::History => Vec::new(),
    }
}

/// How many records a lookup tab holds, for the strip: a rewrite or a
/// presentation or a default that exists, not the want of one.
pub(super) fn record_count(active: &Value, tab: Tab) -> usize {
    match tab {
        Tab::Presentations => array(active, "presentation_records").len(),
        Tab::Rewrites => array(active, "rewrites").len(),
        Tab::Defaults => array(active, "defaults")
            .iter()
            .filter(|default| default.get("exists") == Some(&Value::Bool(true)))
            .count(),
        Tab::Routes => array(active, "routes").len(),
        Tab::Models | Tab::History => 0,
    }
}

/// What a group of rewrites ends on.
#[derive(Debug, Clone, Copy)]
pub(super) enum End<'a> {
    /// A profile's group passes on to its protocol's, which its rules missed.
    Pass(&'a str),
    /// A default: what catches a request no rule did.
    Default(&'a Value),
}

/// One scope's rewrites in ascending priority and what they end on.
pub(super) struct Group<'a> {
    pub(super) scope: &'a Value,
    pub(super) rules: Vec<&'a Value>,
    pub(super) ends: Vec<End<'a>>,
}

/// The rewrites grouped as the table draws them: each profile ahead of its
/// protocol, a profile's group ending by passing to its protocol's, and a
/// protocol's by the defaults, each profile's own ahead of the protocol's
/// (spec: UTUI#reading-a-table). Every protocol has a group, so one with no
/// rules still says that it refuses.
pub(super) fn rewrite_groups(active: &Value) -> Vec<Group<'_>> {
    let rewrites = array(active, "rewrites");
    let defaults = array(active, "defaults");
    let default_of = |label: &str| {
        defaults
            .iter()
            .find(|default| text(default, "scope_label") == label)
    };
    let mut groups = Vec::new();
    for scope in array(active, "scopes") {
        let label = text(scope, "scope_label");
        let rules: Vec<&Value> = rewrites
            .iter()
            .filter(|rule| text(rule, "scope_label") == label)
            .collect();
        let protocol = text(scope, "protocol");
        if scope
            .get("profile")
            .is_some_and(|profile| !profile.is_null())
        {
            // A profile with no rule of its own has nothing to say here: its
            // default is listed with its protocol's.
            if !rules.is_empty() {
                groups.push(Group {
                    scope,
                    rules,
                    ends: vec![End::Pass(protocol)],
                });
            }
            continue;
        }
        let mut ends: Vec<End> = array(active, "scopes")
            .iter()
            .filter(|other| {
                text(other, "protocol") == protocol
                    && other
                        .get("profile")
                        .is_some_and(|profile| !profile.is_null())
            })
            .filter_map(|profile| default_of(text(profile, "scope_label")))
            .filter(|default| default.get("exists") == Some(&Value::Bool(true)))
            .map(End::Default)
            .collect();
        ends.extend(default_of(label).map(End::Default));
        groups.push(Group { scope, rules, ends });
    }
    groups
}

fn route_of<'a>(active: &'a Value, id: &str) -> Option<&'a Value> {
    array(active, "routes")
        .iter()
        .find(|route| text(route, "id") == id)
}

/// A route as `2 candidates · anthropic_sub → openrouter`.
pub(super) fn route_summary(active: &Value, id: &str) -> Vec<Span<'static>> {
    let Some(route) = route_of(active, id) else {
        return vec![toned("not in the policy", Tone::Warn)];
    };
    let providers: Vec<&str> = array(route, "providers")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let candidates = array(route, "candidates");
    let mut said = format!(
        "{} · {}",
        counted(candidates.len(), "candidate", "candidates"),
        providers.join(" → ")
    );
    // A route every candidate of which is billed says so; a mixed one is
    // read candidate by candidate.
    if !candidates.is_empty()
        && candidates
            .iter()
            .all(|candidate| text(candidate, "cost_class") == "billed")
    {
        said.push_str(" · billed");
    }
    vec![span(said, dim())]
}

// --- rewrites ---------------------------------------------------------------

/// The columns of the rewrites table, and what each is given.
struct Columns {
    kind: Option<usize>,
    matched: usize,
    route: usize,
    sends: Option<usize>,
    reach: usize,
}

const GUTTER: usize = 6;
const HITS: usize = 8;

/// How wide each column is. The reach and the hits take what they read in;
/// the match and the route share the rest, three parts to four, and a column
/// that varies from row to row comes out of one of them, so a table where a
/// value is shared reads the same as one where it is not.
fn columns(width: usize, reach: usize, kind: bool, sends: bool) -> Columns {
    let rest = width.saturating_sub(GUTTER + reach + HITS);
    let matched = rest * 3 / 7;
    let route = rest * 4 / 7;
    let leftover = rest - matched - route;
    let kind = kind.then_some(7.min(matched));
    let sends = sends.then_some((route * 2 + 2) / 5);
    Columns {
        matched: matched - kind.unwrap_or(0),
        route: route - sends.unwrap_or(0),
        kind,
        sends,
        reach: reach + leftover,
    }
}

fn gutter(number: usize) -> Vec<Span<'static>> {
    vec![
        span("│", dim()),
        Span::raw(" "),
        span(format!("{number:>2}"), dim()),
        Span::raw("  "),
    ]
}

fn else_gutter(last: bool) -> Vec<Span<'static>> {
    vec![
        span(if last { "└─" } else { "├─" }, dim()),
        Span::raw(" "),
        span("else", dim()),
        Span::raw("  "),
    ]
}

/// `route` as the route cell reads: an arrow and the route.
fn route_cell(route: &str) -> Vec<Span<'static>> {
    vec![Span::raw(format!("→ {route}"))]
}

fn sends_text(record: &Value) -> String {
    record
        .get("sends")
        .and_then(|sends| sends.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_owned()
}

/// A group head: the scope, what its rows share, and a rule to the edge.
fn group_head(name: &str, clauses: &str, width: usize) -> BodyLine {
    let mut spans = vec![
        span("┌─ ", dim()),
        span(name.to_owned(), bold()),
        Span::raw(format!("  {clauses} ")),
    ];
    let used = width_of(&spans);
    spans.push(span("─".repeat(width.saturating_sub(used)), dim()));
    BodyLine::plain(Line::from(spans))
}

/// The rewrites table.
pub(super) fn rewrites_table(active: &Value, width: usize) -> Table {
    let groups = rewrite_groups(active);
    let all: Vec<&Value> = groups
        .iter()
        .flat_map(|group| group.rules.iter().copied())
        .collect();
    let kinds: std::collections::BTreeSet<&str> = all
        .iter()
        .map(|rule| rule.get("matcher").map_or("", matcher_kind))
        .collect();
    let sendses: std::collections::BTreeSet<String> =
        all.iter().map(|rule| sends_text(rule)).collect();
    let (kind_col, sends_col) = (kinds.len() > 1, sendses.len() > 1);
    let reach_nat = widest(
        all.iter()
            .map(|rule| reach_text(rule.get("reach").unwrap_or(&Value::Null)))
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str)
            .chain(["reach"]),
    );
    let cols = columns(width, reach_nat, kind_col, sends_col);

    let mut header = Cells::new().cell(Vec::new(), GUTTER);
    if let Some(kind) = cols.kind {
        header = header.cell(heading("kind"), kind);
    }
    header = header
        .cell(heading("match"), cols.matched)
        .cell(vec![Span::raw("  "), label("route")], cols.route);
    if let Some(sends) = cols.sends {
        header = header.cell(heading("sends"), sends);
    }
    let header = header
        .cell(heading("reach"), cols.reach)
        .right(heading("hits 24h"), HITS)
        .line();

    let mut body = Vec::new();
    let mut entry = 0;
    for group in &groups {
        let scope = text(group.scope, "scope_label");
        let rules = group.rules.len();
        let mut clauses = if rules == 0 {
            "no rules".to_owned()
        } else {
            counted(rules, "rule", "rules")
        };
        if !kind_col
            && rules > 0
            && let Some(kind) = kinds.iter().next()
        {
            clauses.push_str(&format!(" · {kind}"));
        }
        if !sends_col
            && rules > 0
            && let Some(sends) = sendses.iter().next()
        {
            clauses.push_str(&format!(" · sends {sends}"));
        }
        body.push(group_head(scope, &clauses, width));
        for rule in &group.rules {
            let matcher = rule.get("matcher").unwrap_or(&Value::Null);
            let mut cells =
                Cells::new().raw(gutter(rule["position"].as_u64().unwrap_or(0) as usize));
            if let Some(kind) = cols.kind {
                cells = cells.cell(vec![Span::raw(matcher_kind(matcher).to_owned())], kind);
            }
            cells = cells
                .cell(vec![Span::raw(matcher_text(matcher))], cols.matched)
                .cell(route_cell(text(rule, "route_id")), cols.route);
            if let Some(sends) = cols.sends {
                cells = cells.cell(vec![Span::raw(sends_text(rule))], sends);
            }
            let line = cells
                .cell(
                    reach_spans(rule.get("reach").unwrap_or(&Value::Null)),
                    cols.reach,
                )
                .right(vec![hits_span(rule.get("hits"))], HITS)
                .line();
            body.push(BodyLine::of(line, entry));
            entry += 1;
        }
        for (index, end) in group.ends.iter().enumerate() {
            let last = index + 1 == group.ends.len();
            let lead = GUTTER + cols.kind.unwrap_or(0) + cols.matched;
            let name_width = lead.saturating_sub(9);
            match end {
                End::Pass(protocol) => body.push(BodyLine::plain(
                    Cells::new()
                        .raw(else_gutter(last))
                        .raw(vec![span(format!("tries {protocol} next"), dim())])
                        .line(),
                )),
                End::Default(default) => {
                    body.push(BodyLine::of(
                        default_end_row(default, &cols, last, name_width),
                        entry,
                    ));
                    entry += 1;
                }
            }
        }
    }
    Table { header, body }
}

/// A default as a group's closing row.
fn default_end_row(
    default: &Value,
    cols: &Columns,
    last: bool,
    name_width: usize,
) -> Line<'static> {
    let exists = default.get("exists") == Some(&Value::Bool(true));
    let reach = default.get("reach").unwrap_or(&Value::Null);
    let scope_name = match default.get("profile").and_then(Value::as_str) {
        Some(profile) => format!("{profile} default"),
        None => "default".to_owned(),
    };
    let cells = Cells::new().raw(else_gutter(last));
    if exists {
        cells
            .cell(vec![Span::raw(scope_name)], name_width)
            .cell(
                route_cell(text(default, "route_id")),
                cols.route + cols.sends.unwrap_or(0),
            )
            .cell(reach_spans(reach), cols.reach)
            .right(vec![hits_span(default.get("hits"))], HITS)
            .line()
    } else {
        // Nothing catches a request here: the route cell says so, in the
        // reach's own words, and there is no reach column to read.
        let mut refused = vec![Span::raw("→ ")];
        refused.extend(reach_spans(reach));
        cells
            .cell(vec![Span::raw("no default")], name_width)
            .cell(refused, cols.route + cols.sends.unwrap_or(0) + cols.reach)
            .right(no_reach(), HITS)
            .line()
    }
}

/// What the `?` overlay and the controls line call a rule's controls.
const RULE_CONTROLS: [(&str, &str); 7] = [
    ("e", "edit"),
    ("n", "new"),
    ("m", "move"),
    ("space", "turn off"),
    ("d", "delete"),
    ("t", "trace"),
    ("enter", "open route"),
];

const DEFAULT_CONTROLS: [(&str, &str); 6] = [
    ("e", "edit"),
    ("n", "new"),
    ("space", "turn off"),
    ("d", "delete"),
    ("t", "trace"),
    ("enter", "open route"),
];

const LABELS: usize = 10;

/// The detail beneath the rewrites table for the selected rule: what it
/// matches with a worked example, what it sends on, the route and
/// candidates, its reach with the reason for it, anything earlier that
/// already catches it, and its recent catches (spec: UTUI#reading-a-table).
pub(super) fn rule_detail(active: &Value, rule: &Value, earlier: &[&Value]) -> Vec<Line<'static>> {
    let matcher = rule.get("matcher").unwrap_or(&Value::Null);
    let route = text(rule, "route_id");
    let reach = rule.get("reach").unwrap_or(&Value::Null);
    let mut lines = vec![Line::from(span(
        format!("{} → {route}", matcher_text(matcher)),
        bold(),
    ))];
    lines.push(field(
        "match",
        LABELS,
        vec![Span::raw(format!(
            "{}  {}",
            matcher_kind(matcher),
            matcher_text(matcher)
        ))],
    ));
    lines.push(field(
        "sends",
        LABELS,
        vec![match rule.pointer("/sends/kind").and_then(Value::as_str) {
            Some("none") | None => toned("-", Tone::Dim),
            Some(_) => Span::raw(sends_text(rule)),
        }],
    ));
    lines.push(field("example", LABELS, example_spans(rule.get("example"))));
    let mut sends_to = vec![Span::raw(route.to_owned()), Span::raw("  ")];
    sends_to.extend(route_summary(active, route));
    lines.push(field("sends to", LABELS, sends_to));
    lines.push(reach_line(reach));
    lines.push(field(
        "overlaps",
        LABELS,
        vec![Span::raw(overlap_text(rule, earlier))],
    ));
    lines.push(field(
        "recent",
        LABELS,
        vec![Span::raw(hits_phrase(rule.get("hits")))],
    ));
    lines.push(controls(&RULE_CONTROLS));
    lines
}

fn reach_line(reach: &Value) -> Line<'static> {
    let mut value = reach_spans(reach);
    let reason = reach_reason(reach);
    if !reason.is_empty() {
        value.push(span(format!("  {reason}"), Style::new()));
    }
    field("reach", LABELS, value)
}

fn example_spans(example: Option<&Value>) -> Vec<Span<'static>> {
    let Some(example) = example.filter(|example| !example.is_null()) else {
        return vec![toned("-", Tone::Dim)];
    };
    let mut spans = vec![Span::raw(format!(
        "{} → {}",
        text(example, "in"),
        example.get("out").and_then(Value::as_str).unwrap_or("-")
    ))];
    if example.get("invented") == Some(&Value::Bool(true)) {
        spans.push(span("  made up from the prefix", dim()));
    }
    spans
}

/// What already catches all or part of what a rule matches, in words, or
/// that nothing does and why.
fn overlap_text(rule: &Value, earlier: &[&Value]) -> String {
    let shadows = array(rule, "shadowed_by");
    if shadows.is_empty() {
        let matcher = rule.get("matcher").unwrap_or(&Value::Null);
        let prefixes = matcher_kind(matcher) == "prefix"
            && !earlier.is_empty()
            && earlier
                .iter()
                .all(|rule| rule.get("matcher").map(matcher_kind) == Some("prefix"));
        return if prefixes {
            "none: the prefixes differ".to_owned()
        } else {
            format!("none: no rule above catches {}", matcher_text(matcher))
        };
    }
    shadows
        .iter()
        .map(shadow_text)
        .collect::<Vec<_>>()
        .join("; ")
}

fn shadow_text(shadow: &Value) -> String {
    let how = if text(shadow, "catches") == "all" {
        "all of it"
    } else {
        "part of it"
    };
    let matcher = shadow.get("matcher").unwrap_or(&Value::Null);
    let at = shadow
        .get("position")
        .and_then(Value::as_u64)
        .map(|position| format!("rule {position} "))
        .unwrap_or_default();
    let who = match text(shadow, "by") {
        "presentation" => format!("presentation {}", text(shadow, "id")),
        "profile_rule" => format!(
            "{at}{} of {}",
            matcher_text(matcher),
            text(shadow, "scope_label")
        ),
        _ => format!("{at}{}", matcher_text(matcher)),
    };
    let examples: Vec<&str> = array(shadow, "examples")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let seen = if examples.is_empty() {
        String::new()
    } else {
        format!(" ({})", examples.join(", "))
    };
    format!("{how} is caught first by {who}{seen}")
}

/// The detail for a default.
pub(super) fn default_detail(active: &Value, default: &Value) -> Vec<Line<'static>> {
    let exists = default.get("exists") == Some(&Value::Bool(true));
    let reach = default.get("reach").unwrap_or(&Value::Null);
    let mut lines = vec![Line::from(span(
        text(default, "scope_label").to_owned(),
        bold(),
    ))];
    if exists {
        let route = text(default, "route_id");
        let mut sends_to = vec![Span::raw(route.to_owned()), Span::raw("  ")];
        sends_to.extend(route_summary(active, route));
        lines.push(field("sends to", LABELS, sends_to));
        lines.push(reach_line(reach));
        lines.push(field(
            "recent",
            LABELS,
            vec![Span::raw(hits_phrase(default.get("hits")))],
        ));
        lines.push(controls(&DEFAULT_CONTROLS));
    } else {
        lines.push(field(
            "sends to",
            LABELS,
            vec![toned("nowhere: there is no default", Tone::Warn)],
        ));
        lines.push(reach_line(reach));
        lines.push(controls(&[("n", "new"), ("t", "trace")]));
    }
    lines
}

/// The rules that come before `rule` in its own group, for the detail's
/// word on overlap.
pub(super) fn rules_before<'a>(active: &'a Value, rule: &Value) -> Vec<&'a Value> {
    let label = text(rule, "scope_label");
    let position = rule.get("position").and_then(Value::as_u64).unwrap_or(0);
    array(active, "rewrites")
        .iter()
        .filter(|other| {
            text(other, "scope_label") == label
                && other.get("position").and_then(Value::as_u64).unwrap_or(0) < position
        })
        .collect()
}

// --- defaults ---------------------------------------------------------------

/// The defaults table: one row for each scope that has a default, and one for
/// a protocol that has none.
pub(super) fn defaults_table(active: &Value, width: usize) -> Table {
    let defaults = array(active, "defaults");
    let cols = default_columns(width, defaults);
    let header = Cells::new()
        .cell(Vec::new(), 2)
        .cell(heading("default for"), cols.0)
        .cell(vec![Span::raw("  "), label("route")], cols.1)
        .cell(heading("reach"), cols.2)
        .right(heading("hits 24h"), HITS)
        .line();
    let body = defaults
        .iter()
        .enumerate()
        .map(|(entry, default)| {
            let exists = default.get("exists") == Some(&Value::Bool(true));
            let reach = default.get("reach").unwrap_or(&Value::Null);
            let name = if exists {
                vec![Span::raw(text(default, "scope_label").to_owned())]
            } else {
                vec![
                    Span::raw(format!("{}  ", text(default, "scope_label"))),
                    toned("no default", Tone::Warn),
                ]
            };
            let cells = Cells::new().cell(Vec::new(), 2).cell(name, cols.0);
            let line = if exists {
                cells
                    .cell(route_cell(text(default, "route_id")), cols.1)
                    .cell(reach_spans(reach), cols.2)
                    .right(vec![hits_span(default.get("hits"))], HITS)
                    .line()
            } else {
                let mut refused = vec![Span::raw("→ ")];
                refused.extend(reach_spans(reach));
                cells
                    .cell(refused, cols.1 + cols.2)
                    .right(no_reach(), HITS)
                    .line()
            };
            BodyLine::of(line, entry)
        })
        .collect();
    Table { header, body }
}

/// The widths of the defaults table's name, route and reach columns: the
/// name and the route each read in their longest and a little more, and the
/// reach has the rest.
fn default_columns(width: usize, defaults: &[Value]) -> (usize, usize, usize) {
    let name = widest(
        defaults
            .iter()
            .map(|default| text(default, "scope_label"))
            .chain(["default for"]),
    ) + 6;
    let route = widest(
        defaults
            .iter()
            .map(|default| text(default, "route_id"))
            .chain([""]),
    ) + 2
        + 9;
    let reach = width.saturating_sub(2 + name + route + HITS);
    (name, route, reach)
}

// --- presentations ----------------------------------------------------------

fn matchers(source: &Value, key: &str) -> Vec<String> {
    array(source, key).iter().map(matcher_text).collect()
}

fn include_text(record: &Value) -> String {
    let source = record.get("source").unwrap_or(&Value::Null);
    match text(source, "kind") {
        "offer_selector" => matchers(source, "include").join(" "),
        "exact_offer" => text(source, "model_id").to_owned(),
        _ => text(record, "slug").to_owned(),
    }
}

fn except_text(record: &Value) -> Option<String> {
    let source = record.get("source").unwrap_or(&Value::Null);
    let except = matchers(source, "exclude");
    (!except.is_empty()).then(|| except.join(" "))
}

/// Where a presentation's names go: a route, or the binding of an offer.
fn goes_to(record: &Value) -> String {
    if let Some(route) = array(record, "route_ids").first().and_then(Value::as_str) {
        return route.to_owned();
    }
    array(record, "bindings")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The presentations table: each scope's presentations, with the slug they
/// share said once in the group's head.
pub(super) fn presentations_table(active: &Value, width: usize) -> Table {
    let records = array(active, "presentation_records");
    let name_w = widest(
        records
            .iter()
            .map(include_text)
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str)
            .chain(["include"]),
    ) + 4;
    let except_w = widest(
        records
            .iter()
            .filter_map(except_text)
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str)
            .chain(["except"]),
    ) + 2;
    let as_w = 8;
    let lists_w = 8;
    let cols = (
        name_w.min(width / 3),
        except_w.min(width / 3),
        as_w,
        lists_w,
    );
    let header = Cells::new()
        .cell(Vec::new(), GUTTER)
        .cell(heading("include"), cols.0)
        .cell(heading("except"), cols.1)
        .cell(heading("as"), cols.2)
        .cell(heading("lists"), cols.3)
        .cell(
            heading("newest listed"),
            width.saturating_sub(GUTTER + cols.0 + cols.1 + cols.2 + cols.3),
        )
        .line();
    let mut body = Vec::new();
    let mut entry = 0;
    for scope in array(active, "scopes") {
        let label = text(scope, "scope_label");
        let group: Vec<&Value> = records
            .iter()
            .filter(|record| text(record, "scope_label") == label)
            .collect();
        if group.is_empty() {
            continue;
        }
        let mut clauses = counted(group.len(), "presentation", "presentations");
        let slugs: std::collections::BTreeSet<&str> =
            group.iter().map(|record| text(record, "slug")).collect();
        let targets: std::collections::BTreeSet<String> =
            group.iter().map(|record| goes_to(record)).collect();
        if let (1, Some(slug)) = (slugs.len(), slugs.iter().next()) {
            clauses.push_str(&format!(" · slug {slug}"));
        }
        if let (1, Some(target)) = (targets.len(), targets.iter().next())
            && !target.is_empty()
        {
            clauses.push_str(&format!(" → {target}"));
        }
        body.push(group_head(label, &clauses, width));
        for record in &group {
            let lists = record.get("lists").filter(|lists| !lists.is_null());
            let line = Cells::new()
                .raw(gutter(record["position"].as_u64().unwrap_or(0) as usize))
                .cell(vec![Span::raw(include_text(record))], cols.0)
                .cell(
                    vec![match except_text(record) {
                        Some(except) => Span::raw(except),
                        None => toned("-", Tone::Dim),
                    }],
                    cols.1,
                )
                .cell(
                    vec![match record.get("behaves_as").and_then(Value::as_str) {
                        Some(tier) => Span::raw(tier.to_owned()),
                        None => toned("-", Tone::Dim),
                    }],
                    cols.2,
                )
                .right(
                    vec![match lists {
                        Some(lists) => Span::raw(super::data::count(lists, "count").to_string()),
                        None => toned("-", Tone::Dim),
                    }],
                    cols.3 - 3,
                )
                .cell(Vec::new(), 3)
                .raw(vec![match lists {
                    Some(lists) => Span::raw(text(lists, "newest").to_owned()),
                    None => toned("-", Tone::Dim),
                }])
                .line();
            body.push(BodyLine::of(line, entry));
            entry += 1;
        }
        body.push(BodyLine::plain(
            Cells::new()
                .raw(vec![
                    span("└─", dim()),
                    Span::raw(" "),
                    span("else", dim()),
                    Span::raw("  "),
                ])
                .cell(vec![Span::raw("no slug equals it")], GUTTER + cols.0 - 7)
                .raw(vec![span("→ rewrites", dim())])
                .line(),
        ));
    }
    Table { header, body }
}

const PRESENTATION_CONTROLS: [(&str, &str); 7] = RULE_CONTROLS;

/// The detail for a presentation.
pub(super) fn presentation_detail(active: &Value, record: &Value) -> Vec<Line<'static>> {
    let source = record.get("source").unwrap_or(&Value::Null);
    let mut lines = vec![Line::from(span(include_text(record), bold()))];
    match text(source, "kind") {
        "offer_selector" => {
            lines.push(field(
                "include",
                LABELS,
                vec![Span::raw(matchers(source, "include").join("  "))],
            ));
            lines.push(field(
                "except",
                LABELS,
                vec![match except_text(record) {
                    Some(_) => Span::raw(matchers(source, "exclude").join("  ")),
                    None => toned("-", Tone::Dim),
                }],
            ));
            lines.push(field("lists", LABELS, vec![Span::raw(lists_text(record))]));
        }
        "exact_offer" => {
            lines.push(field(
                "offer",
                LABELS,
                vec![Span::raw(format!(
                    "{}  {}",
                    text(source, "binding_id"),
                    text(source, "model_id")
                ))],
            ));
        }
        _ => {
            let route = goes_to(record);
            let mut sends_to = vec![Span::raw(route.clone())];
            if !route.is_empty() {
                sends_to.push(Span::raw("  "));
                sends_to.extend(route_summary(active, &route));
            }
            lines.push(field("sends to", LABELS, sends_to));
        }
    }
    let shown = array(record, "slugs")
        .first()
        .and_then(Value::as_str)
        .map(|slug| format!("  shown as {slug}"))
        .unwrap_or_default();
    lines.push(field(
        "as",
        LABELS,
        vec![
            match record.get("behaves_as").and_then(Value::as_str) {
                Some(tier) => Span::raw(tier.to_owned()),
                None => toned("-", Tone::Dim),
            },
            span(shown, dim()),
        ],
    ));
    lines.push(field(
        "overlaps",
        LABELS,
        vec![Span::raw(presentation_overlap(record))],
    ));
    lines.push(field(
        "published",
        LABELS,
        vec![Span::raw(
            record
                .get("published")
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_owned(),
        )],
    ));
    lines.push(controls(&PRESENTATION_CONTROLS));
    lines
}

fn lists_text(record: &Value) -> String {
    let Some(lists) = record.get("lists").filter(|lists| !lists.is_null()) else {
        return "-".to_owned();
    };
    let count = super::data::count(lists, "count") as usize;
    let models: Vec<&str> = array(lists, "models")
        .iter()
        .filter_map(Value::as_str)
        .take(3)
        .collect();
    let mut text = format!(
        "{} · newest {}",
        counted(count, "model", "models"),
        models.join(" · ")
    );
    if count > models.len() {
        text.push_str(&format!(" · and {} more", count - models.len()));
    }
    text
}

fn presentation_overlap(record: &Value) -> String {
    let shadows = array(record, "shadowed_by");
    if shadows.is_empty() {
        return "none".to_owned();
    }
    shadows
        .iter()
        .map(shadow_text)
        .collect::<Vec<_>>()
        .join("; ")
}
