//! What a revision changed from the one before it, in words.
//!
//! The history tab lists each revision with a one-line summary and opens it
//! as the records it added, changed, moved or removed
//! (spec: UTUI#history). Both are made here from the two documents, so the
//! terminal needs no policy documents of its own and the control surface
//! stays content-free: only record identifiers, matchers and routes appear,
//! as they do in the live snapshot.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::derived::scope_label;
use crate::policy::shadow::Shape;
use crate::policy::{
    ArgumentBinding, Candidate, PolicyDocument, REQUESTED_MODEL_PARAMETER, RewriteRule, Route,
    RouteReference,
};

/// One record's change between two revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Change {
    /// `added`, `removed`, `changed` or `moved`.
    pub(super) change: &'static str,
    /// The kind of record: `rewrite`, `presentation`, `default`, `route`,
    /// `candidate`, `family`, `upgrade`, `compaction` or `policy`.
    pub(super) kind: &'static str,
    pub(super) id: Option<String>,
    /// The change as a phrase for a list: `add rewrite claude-* →
    /// openrouter-messages`.
    pub(super) summary: String,
    /// The record as the detail reads it, after the sign that says what was
    /// done to it: `rewrite anthropic_messages 6  claude-* → …  as asked`.
    pub(super) text: String,
}

fn change(
    change: &'static str,
    kind: &'static str,
    id: Option<String>,
    summary: String,
    text: String,
) -> Change {
    Change {
        change,
        kind,
        id,
        summary,
        text,
    }
}

/// What a rule sends on, as the tables write it; `None` for a route that
/// takes no model.
fn sends_text(target: &RouteReference) -> Option<String> {
    let binding = target
        .parameters
        .iter()
        .find(|(name, _)| name.as_str() == REQUESTED_MODEL_PARAMETER)
        .or_else(|| target.parameters.iter().next())
        .map(|(_, binding)| binding)?;
    Some(match binding {
        ArgumentBinding::RequestedModel => "as asked".to_owned(),
        ArgumentBinding::RegexCaptureName { name } => format!("${{{name}}}"),
        ArgumentBinding::RegexCaptureIndex { index } => format!("${{{index}}}"),
        ArgumentBinding::Literal { value } => value.clone(),
    })
}

fn matcher_text(rule: &RewriteRule) -> String {
    Shape::of(&rule.model_matcher).text()
}

/// The rewrites of each scope in the order they are tried: ascending
/// priority, ties kept in document order.
fn rewrite_order(document: &PolicyDocument) -> BTreeMap<String, Vec<&RewriteRule>> {
    let mut by_scope: BTreeMap<String, Vec<&RewriteRule>> = BTreeMap::new();
    for rule in &document.rewrites {
        by_scope
            .entry(scope_label(&rule.scope))
            .or_default()
            .push(rule);
    }
    for rules in by_scope.values_mut() {
        rules.sort_by_key(|rule| rule.priority);
    }
    by_scope
}

/// The names of the top-level fields whose values differ, leaving out
/// `ignore`.
fn differing(before: &impl Serialize, after: &impl Serialize, ignore: &[&str]) -> Vec<String> {
    let (Ok(Value::Object(before)), Ok(Value::Object(after))) =
        (serde_json::to_value(before), serde_json::to_value(after))
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = before
        .keys()
        .chain(after.keys())
        .filter(|key| !ignore.contains(&key.as_str()) && before.get(*key) != after.get(*key))
        .cloned()
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The ids that changed place among the ones both orders hold, each with the
/// id that follows it now (`None` at the end).
///
/// The ones left where they were are the longest run that kept its order, so
/// moving one record past three others reads as one move, not three.
fn moved(before: &[String], after: &[String]) -> Vec<(String, Option<String>)> {
    let kept_after: Vec<&String> = after.iter().filter(|id| before.contains(id)).collect();
    let kept_before: Vec<&String> = before.iter().filter(|id| after.contains(id)).collect();
    if kept_before == kept_after {
        return Vec::new();
    }
    // Positions in the old order, in the new order; the longest increasing
    // run of them is what stayed.
    let places: Vec<usize> = kept_after
        .iter()
        .filter_map(|id| kept_before.iter().position(|other| other == id))
        .collect();
    let mut best: Vec<Vec<usize>> = Vec::new();
    for (at, place) in places.iter().enumerate() {
        let mut run = best[..at]
            .iter()
            .zip(&places)
            .filter(|(_, earlier)| *earlier < place)
            .map(|(run, _)| run)
            .max_by_key(|run| run.len())
            .cloned()
            .unwrap_or_default();
        run.push(at);
        best.push(run);
    }
    let stayed = best
        .into_iter()
        .max_by_key(|run| run.len())
        .unwrap_or_default();
    kept_after
        .iter()
        .enumerate()
        .filter(|(at, _)| !stayed.contains(at))
        .map(|(at, id)| {
            (
                (*id).clone(),
                kept_after.get(at + 1).map(|id| (*id).clone()),
            )
        })
        .collect()
}

fn candidates(route: &Route) -> Vec<&Candidate> {
    std::iter::once(&route.chain.first)
        .chain(route.chain.steps.iter().map(|step| &step.candidate))
        .collect()
}

fn describe_route(route: &Route) -> String {
    let chain: Vec<String> = candidates(route)
        .iter()
        .map(|candidate| candidate.binding_id.to_string())
        .collect();
    format!("route {}  {}", route.id, chain.join(" → "))
}

fn route_changes(before: &Route, after: &Route, out: &mut Vec<Change>) {
    let id = after.id.to_string();
    let (was, now) = (candidates(before), candidates(after));
    let was_ids: Vec<String> = was.iter().map(|c| c.id.to_string()).collect();
    let now_ids: Vec<String> = now.iter().map(|c| c.id.to_string()).collect();
    for candidate in &now {
        let name = candidate.id.to_string();
        if !was_ids.contains(&name) {
            let fallback = now_ids.first() != Some(&name);
            let what = if fallback {
                "a fallback"
            } else {
                "a first candidate"
            };
            out.push(change(
                "added",
                "candidate",
                Some(name),
                format!("add {what} to {id}"),
                format!(
                    "candidate {} on route {id}  {}",
                    candidate.id, candidate.binding_id
                ),
            ));
        }
    }
    for candidate in &was {
        let name = candidate.id.to_string();
        if !now_ids.contains(&name) {
            out.push(change(
                "removed",
                "candidate",
                Some(name),
                format!("remove {} from {id}", candidate.id),
                format!(
                    "candidate {} on route {id}  {}",
                    candidate.id, candidate.binding_id
                ),
            ));
        }
    }
    for (name, next) in moved(&was_ids, &now_ids) {
        let placed = match next {
            Some(next) => format!("above {next}"),
            None => "to the end".to_owned(),
        };
        out.push(change(
            "moved",
            "candidate",
            Some(name.clone()),
            format!("move {name} {placed} in {id}"),
            format!("candidate {name} {placed} on route {id}"),
        ));
    }
    for candidate in &now {
        let Some(old) = was.iter().find(|old| old.id == candidate.id) else {
            continue;
        };
        let fields = differing(*old, *candidate, &[]);
        if !fields.is_empty() {
            out.push(change(
                "changed",
                "candidate",
                Some(candidate.id.to_string()),
                format!("change {} on {id}", candidate.id),
                format!(
                    "candidate {} on route {id}: {}",
                    candidate.id,
                    fields.join(", ")
                ),
            ));
        }
    }
    // The edge into a candidate that stayed: what makes a route fall back.
    for step in &after.chain.steps {
        let Some(old) = before
            .chain
            .steps
            .iter()
            .find(|old| old.candidate.id == step.candidate.id)
        else {
            continue;
        };
        let fields = differing(&old.edge, &step.edge, &[]);
        if !fields.is_empty() {
            out.push(change(
                "changed",
                "route",
                Some(id.clone()),
                format!("change when {id} falls back to {}", step.candidate.id),
                format!(
                    "route {id}, the edge into {}: {}",
                    step.candidate.id,
                    fields.join(", ")
                ),
            ));
        }
    }
    let fields = differing(before, after, &["chain"]);
    if !fields.is_empty() {
        out.push(change(
            "changed",
            "route",
            Some(id.clone()),
            format!("change route {id}"),
            format!("route {id}: {}", fields.join(", ")),
        ));
    }
}

/// Added, removed and changed records of one kind, matched by `key_of`.
///
/// Records are walked in `after`'s order, reporting each as added or passing
/// it with its old self to `changed`, then the ones only in `before` as
/// removed. What each is called is the caller's to say, so a kind with its own
/// wording or its own notion of changed does not need a loop of its own.
fn matched<T>(
    before: &[T],
    after: &[T],
    key_of: impl Fn(&T) -> String,
    added: impl Fn(&T) -> Change,
    changed: impl Fn(&T, &T, &mut Vec<Change>),
    removed: impl Fn(&T) -> Change,
    out: &mut Vec<Change>,
) {
    for record in after {
        let key = key_of(record);
        match before.iter().find(|old| key_of(old) == key) {
            None => out.push(added(record)),
            Some(old) => changed(old, record, out),
        }
    }
    for old in before {
        let key = key_of(old);
        if !after.iter().any(|record| key_of(record) == key) {
            out.push(removed(old));
        }
    }
}

/// Records of one kind, by identifier, that are described by a summary and
/// one line of text and changed where any field differs.
fn records<T: Serialize>(
    kind: &'static str,
    before: &[T],
    after: &[T],
    id_of: impl Fn(&T) -> String,
    summary_of: impl Fn(&T) -> String,
    text_of: impl Fn(&T) -> String,
    out: &mut Vec<Change>,
) {
    matched(
        before,
        after,
        &id_of,
        |record| {
            change(
                "added",
                kind,
                Some(id_of(record)),
                format!("add {}", summary_of(record)),
                text_of(record),
            )
        },
        |old, record, out| {
            let fields = differing(old, record, &[]);
            if !fields.is_empty() {
                out.push(change(
                    "changed",
                    kind,
                    Some(id_of(record)),
                    format!("change {}", summary_of(record)),
                    format!("{}: {}", text_of(record), fields.join(", ")),
                ));
            }
        },
        |old| {
            change(
                "removed",
                kind,
                Some(id_of(old)),
                format!("remove {}", summary_of(old)),
                text_of(old),
            )
        },
        out,
    );
}

/// Every record that differs between `before` and `after`, in the order a
/// request looks them up and then the records that shape a route.
pub(super) fn changes(before: &PolicyDocument, after: &PolicyDocument) -> Vec<Change> {
    let mut out = Vec::new();

    records(
        "presentation",
        &before.presentations,
        &after.presentations,
        |p| p.id.to_string(),
        |p| format!("presentation {}", p.slug),
        |p| format!("presentation {}  {}", scope_label(&p.scope), p.slug),
        &mut out,
    );

    let (was, now) = (rewrite_order(before), rewrite_order(after));
    let position = |order: &BTreeMap<String, Vec<&RewriteRule>>, rule: &RewriteRule| {
        order
            .get(&scope_label(&rule.scope))
            .and_then(|rules| rules.iter().position(|other| other.id == rule.id))
            .map_or(0, |at| at + 1)
    };
    let route_of = |rule: &RewriteRule| rule.target.route_id.to_string();
    let rewrite_summary =
        |rule: &RewriteRule| format!("rewrite {} → {}", matcher_text(rule), route_of(rule));
    // The position a rewrite holds is not part of its record: moving one
    // renumbers its neighbours, and that is reported as the move.
    let text = |rule: &RewriteRule, order: &BTreeMap<String, Vec<&RewriteRule>>| {
        format!(
            "rewrite {} {}  {} → {}",
            scope_label(&rule.scope),
            position(order, rule),
            matcher_text(rule),
            route_of(rule)
        )
    };
    // What a rewrite that is present now says, with what it sends. One that
    // is gone is described as it stood, with no more than where it sat.
    let text_now = |rule: &RewriteRule| {
        let mut line = text(rule, &now);
        if let Some(sends) = sends_text(&rule.target) {
            line.push_str(&format!("  {sends}"));
        }
        line
    };
    matched(
        &before.rewrites,
        &after.rewrites,
        |rule| rule.id.to_string(),
        |rule| {
            let text = text_now(rule);
            change(
                "added",
                "rewrite",
                Some(rule.id.to_string()),
                format!("add {}", rewrite_summary(rule)),
                text,
            )
        },
        |old, rule, out| {
            let fields = differing(old, rule, &["priority"]);
            if !fields.is_empty() {
                let text = text_now(rule);
                out.push(change(
                    "changed",
                    "rewrite",
                    Some(rule.id.to_string()),
                    format!("change {}", rewrite_summary(rule)),
                    format!("{text}: {}", fields.join(", ")),
                ));
            }
        },
        |old| {
            change(
                "removed",
                "rewrite",
                Some(old.id.to_string()),
                format!("remove {}", rewrite_summary(old)),
                text(old, &was),
            )
        },
        &mut out,
    );
    for (scope, rules) in &now {
        let ids = |rules: &[&RewriteRule]| -> Vec<String> {
            rules.iter().map(|rule| rule.id.to_string()).collect()
        };
        let old_ids = was.get(scope).map(|rules| ids(rules)).unwrap_or_default();
        let new_ids = ids(rules);
        for (id, next) in moved(&old_ids, &new_ids) {
            let Some(rule) = rules.iter().find(|rule| rule.id.as_str() == id) else {
                continue;
            };
            let placed = match next
                .as_deref()
                .and_then(|next| rules.iter().find(|rule| rule.id.as_str() == next))
            {
                Some(next) => format!("above {}", matcher_text(next)),
                None => "to the end".to_owned(),
            };
            out.push(change(
                "moved",
                "rewrite",
                Some(id),
                format!("move {} {placed} in {scope}", matcher_text(rule)),
                format!(
                    "rewrite {scope} {}  {} → {} {placed}",
                    position(&now, rule),
                    matcher_text(rule),
                    route_of(rule)
                ),
            ));
        }
    }

    matched(
        &before.defaults,
        &after.defaults,
        |default| scope_label(&default.scope),
        |default| {
            let label = scope_label(&default.scope);
            change(
                "added",
                "default",
                Some(label.clone()),
                format!("add the default for {label} → {}", default.target.route_id),
                format!("default {label}  → {}", default.target.route_id),
            )
        },
        |old, default, out| {
            if old.target != default.target {
                let label = scope_label(&default.scope);
                out.push(change(
                    "changed",
                    "default",
                    Some(label.clone()),
                    format!(
                        "send the default for {label} to {}",
                        default.target.route_id
                    ),
                    format!(
                        "default {label}  {} → {}",
                        old.target.route_id, default.target.route_id
                    ),
                ));
            }
        },
        |old| {
            let label = scope_label(&old.scope);
            change(
                "removed",
                "default",
                Some(label.clone()),
                format!("remove the default for {label}"),
                format!("default {label}  → {}", old.target.route_id),
            )
        },
        &mut out,
    );

    matched(
        &before.routes,
        &after.routes,
        |route| route.id.to_string(),
        |route| {
            change(
                "added",
                "route",
                Some(route.id.to_string()),
                format!("add route {}", route.id),
                describe_route(route),
            )
        },
        route_changes,
        |old| {
            change(
                "removed",
                "route",
                Some(old.id.to_string()),
                format!("remove route {}", old.id),
                describe_route(old),
            )
        },
        &mut out,
    );

    records(
        "family",
        &before.classifiers,
        &after.classifiers,
        |c| c.id.to_string(),
        |c| format!("family {}", c.id),
        |c| format!("family {}  on {}", c.id, c.binding_id),
        &mut out,
    );
    records(
        "upgrade",
        &before.upgrades,
        &after.upgrades,
        |u| u.id.to_string(),
        |u| format!("upgrade {}", u.id),
        |u| format!("upgrade {}  on family {}", u.id, u.classifier_id),
        &mut out,
    );

    match (&before.compaction, &after.compaction) {
        (None, Some(_)) => out.push(change(
            "added",
            "compaction",
            None,
            "turn on compaction".to_owned(),
            "compaction".to_owned(),
        )),
        (Some(_), None) => out.push(change(
            "removed",
            "compaction",
            None,
            "turn off compaction".to_owned(),
            "compaction".to_owned(),
        )),
        (Some(old), Some(new)) => {
            let fields = differing(old, new, &[]);
            if !fields.is_empty() {
                out.push(change(
                    "changed",
                    "compaction",
                    None,
                    "change compaction".to_owned(),
                    format!("compaction: {}", fields.join(", ")),
                ));
            }
        }
        (None, None) => {}
    }

    if before.description != after.description {
        out.push(change(
            "changed",
            "policy",
            None,
            "describe the policy".to_owned(),
            match &after.description {
                Some(description) => format!("description: {description}"),
                None => "description removed".to_owned(),
            },
        ));
    }
    out
}

/// What a revision with nothing before it holds, as `setup: 7 routes, 4
/// defaults`.
pub(super) fn first_summary(document: &PolicyDocument) -> String {
    let counts = [
        (document.routes.len(), "route", "routes"),
        (document.defaults.len(), "default", "defaults"),
        (document.rewrites.len(), "rewrite", "rewrites"),
        (
            document.presentations.len(),
            "presentation",
            "presentations",
        ),
        (document.classifiers.len(), "family", "families"),
        (document.upgrades.len(), "upgrade", "upgrades"),
    ];
    let parts: Vec<String> = counts
        .iter()
        .filter(|(count, ..)| *count > 0)
        .map(|(count, one, many)| format!("{count} {}", if *count == 1 { one } else { many }))
        .collect();
    if parts.is_empty() {
        "setup: an empty policy".to_owned()
    } else {
        format!("setup: {}", parts.join(", "))
    }
}

/// A revision's one line: its first change, with how many more follow.
pub(super) fn summary(changes: &[Change]) -> String {
    match changes {
        [] => "no change to the policy".to_owned(),
        [only] => only.summary.clone(),
        [first, rest @ ..] => format!("{} and {} more", first.summary, rest.len()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn document(value: Value) -> PolicyDocument {
        serde_json::from_value(value).expect("a document")
    }

    fn route(id: &str, candidates: &[&str]) -> Value {
        let candidate = |name: &str| {
            json!({
                "id": name,
                "binding_id": format!("{name}.anthropic_messages"),
                "model": {"kind": "requested_model"},
            })
        };
        let steps: Vec<Value> = candidates[1..]
            .iter()
            .map(|name| {
                json!({
                    "edge": {
                        "preflight": "enabled",
                        "billed_boundary": "allow",
                        "ambiguous_transport": "deny",
                        "cache_transition": "allow_when_cold",
                    },
                    "candidate": candidate(name),
                })
            })
            .collect();
        json!({
            "id": id,
            "display_name": id,
            "parameters": ["requested_model"],
            "chain": {"first": candidate(candidates[0]), "steps": steps},
            "lane_transition": "move_when_cold",
        })
    }

    fn rewrite(id: &str, prefix: &str, priority: i32, to: &str) -> Value {
        json!({
            "id": id,
            "scope": {"kind": "protocol", "protocol": "anthropic_messages"},
            "priority": priority,
            "model_matcher": {"kind": "regex", "value": format!("^{prefix}/(?P<model>.+)$")},
            "target": {
                "route_id": to,
                "parameters": {"requested_model": {"kind": "regex_capture_name", "name": "model"}},
            },
        })
    }

    fn base() -> Value {
        json!({
            "format_version": 5,
            "routes": [route("alpha", &["one"]), route("beta", &["two"])],
            "rewrites": [
                rewrite("r1", "a", 10, "alpha"),
                rewrite("r2", "b", 20, "beta"),
                rewrite("r3", "c", 30, "alpha"),
            ],
        })
    }

    fn summaries(before: &Value, after: &Value) -> Vec<String> {
        changes(&document(before.clone()), &document(after.clone()))
            .into_iter()
            .map(|change| format!("{} {}", change.change, change.summary))
            .collect()
    }

    #[test]
    fn nothing_changed_says_so() {
        assert!(summaries(&base(), &base()).is_empty());
        assert_eq!(summary(&[]), "no change to the policy");
    }

    #[test]
    fn an_added_rule_reads_with_its_match_route_and_what_it_sends() {
        let mut after = base();
        after["rewrites"]
            .as_array_mut()
            .unwrap()
            .push(rewrite("r4", "d", 40, "beta"));
        let found = changes(&document(base()), &document(after));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].change, "added");
        assert_eq!(found[0].summary, "add rewrite d/${model} → beta");
        assert_eq!(
            found[0].text,
            "rewrite anthropic_messages 4  d/${model} → beta  ${model}"
        );
    }

    #[test]
    fn lifting_a_rule_is_one_move_not_a_change_to_each_it_passed() {
        // r3 above r1: the priorities of r1 and r2 are untouched, r3's falls.
        let mut after = base();
        after["rewrites"][2]["priority"] = json!(5);
        assert_eq!(
            summaries(&base(), &after),
            ["moved move c/${model} above a/${model} in anthropic_messages"]
        );
        // Renumbering that keeps the order is no change at all.
        let mut renumbered = base();
        for (at, rule) in renumbered["rewrites"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            rule["priority"] = json!((at + 1) * 100);
        }
        assert!(summaries(&base(), &renumbered).is_empty());
    }

    #[test]
    fn a_rule_sent_elsewhere_is_a_change_naming_the_field() {
        let mut after = base();
        after["rewrites"][0]["target"]["route_id"] = json!("beta");
        let found = changes(&document(base()), &document(after));
        assert_eq!(found[0].change, "changed");
        assert!(found[0].text.ends_with(": target"), "{}", found[0].text);
    }

    #[test]
    fn a_default_is_added_redirected_and_removed_by_its_scope() {
        let default = |to: &str| {
            json!({
                "scope": {"kind": "protocol", "protocol": "anthropic_messages"},
                "target": {"route_id": to, "parameters": {}},
            })
        };
        let mut with_alpha = base();
        with_alpha["defaults"] = json!([default("alpha")]);
        let mut with_beta = base();
        with_beta["defaults"] = json!([default("beta")]);
        let texts = |before: &Value, after: &Value| -> Vec<(String, String, String)> {
            changes(&document(before.clone()), &document(after.clone()))
                .into_iter()
                .map(|change| (change.change.to_owned(), change.summary, change.text))
                .collect()
        };
        assert_eq!(
            texts(&base(), &with_alpha),
            [(
                "added".to_owned(),
                "add the default for anthropic_messages → alpha".to_owned(),
                "default anthropic_messages  → alpha".to_owned()
            )]
        );
        assert_eq!(
            texts(&with_alpha, &with_beta),
            [(
                "changed".to_owned(),
                "send the default for anthropic_messages to beta".to_owned(),
                "default anthropic_messages  alpha → beta".to_owned()
            )]
        );
        assert_eq!(
            texts(&with_beta, &base()),
            [(
                "removed".to_owned(),
                "remove the default for anthropic_messages".to_owned(),
                "default anthropic_messages  → beta".to_owned()
            )]
        );
    }

    #[test]
    fn a_route_gaining_a_fallback_and_losing_a_rule() {
        let mut after = base();
        after["routes"][0] = route("alpha", &["one", "three"]);
        after["rewrites"].as_array_mut().unwrap().remove(1);
        assert_eq!(
            summaries(&base(), &after),
            [
                "removed remove rewrite b/${model} → beta",
                "added add a fallback to alpha",
            ]
        );
    }

    #[test]
    fn compaction_and_the_description_are_named_plainly() {
        let mut after = base();
        after["compaction"] = json!({
            "target": {"kind": "substitution", "bindings": ["one.anthropic_messages"]},
            "lane_transition": "move_when_cold",
            "cache_transition": "allow_when_cold",
        });
        after["description"] = json!("tidied");
        assert_eq!(
            summaries(&base(), &after),
            ["added turn on compaction", "changed describe the policy"]
        );
    }

    #[test]
    fn a_revision_with_many_changes_is_summarised_by_its_first() {
        let mut after = base();
        after["rewrites"] = json!([]);
        let found = changes(&document(base()), &document(after));
        assert_eq!(found.len(), 3);
        assert_eq!(
            summary(&found),
            "remove rewrite a/${model} → alpha and 2 more"
        );
    }

    #[test]
    fn the_first_revision_is_counted_not_diffed() {
        assert_eq!(
            first_summary(&document(base())),
            "setup: 2 routes, 3 rewrites"
        );
    }
}
