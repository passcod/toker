//! The detail pane's text for each kind of item on the routing page.
//!
//! The daemon computes what is derived (see its `control::derived`); this
//! only lays it out beside the record. An item the page has no layout for
//! shows as JSON.

use serde_json::Value;

use super::model::{Entry, scope_label};

/// The lines for `entry`, from the snapshot the daemon last sent.
pub(super) fn lines(snapshot: &Value, entry: &Entry<'_>) -> Vec<String> {
    let mut lines = match entry.key {
        "presentations" => presentation(snapshot, entry.value),
        "routes" => route(snapshot, entry.value),
        "defaults" => default(snapshot, entry.value),
        "rewrites" => rewrite(entry.value),
        "classifiers" => classifier(snapshot, entry.value),
        "upgrades" => upgrade(snapshot, entry.value),
        "availability" => availability(snapshot, entry.value),
        "publications" => publication(entry.value),
        _ => json(entry.value),
    };
    if let Some(id) = entry.value.get("id").and_then(Value::as_str) {
        let diagnostics: Vec<String> = array(snapshot, "draft_diagnostics")
            .iter()
            .filter(|diagnostic| text(diagnostic, "record_id") == id)
            .map(|diagnostic| {
                format!(
                    "  {} {} at {}",
                    text(diagnostic, "severity"),
                    text(diagnostic, "code"),
                    text(diagnostic, "path")
                )
            })
            .collect();
        if !diagnostics.is_empty() {
            lines.push(String::new());
            lines.push("DIAGNOSTICS".to_owned());
            lines.extend(diagnostics);
        }
    }
    lines
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("-")
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn json(value: &Value) -> Vec<String> {
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|_| "<unrenderable>".to_owned())
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The derived content that matches what the list shows: the draft's when
/// there is one, else the active revision's. A draft that does not compile
/// has none, and the page says so rather than show the active revision's
/// facts against records it does not describe.
fn derived(snapshot: &Value) -> Result<&Value, &'static str> {
    let derived = snapshot
        .get("derived")
        .ok_or("derived facts not reported")?;
    let key = if snapshot.get("draft").is_some_and(|draft| !draft.is_null()) {
        "draft"
    } else {
        "active"
    };
    derived
        .get(key)
        .filter(|value| !value.is_null())
        .ok_or("no derived facts: the draft does not compile")
}

fn derived_note(snapshot: &Value, lines: &mut Vec<String>) {
    if let Err(note) = derived(snapshot) {
        lines.push(note.to_owned());
    }
}

fn find<'a>(items: &'a [Value], key: &str, wanted: &str) -> Option<&'a Value> {
    items.iter().find(|item| text(item, key) == wanted)
}

fn presentation(snapshot: &Value, record: &Value) -> Vec<String> {
    let mut lines = vec![
        format!("{}  {}", text(record, "slug"), text(record, "label")),
        format!("scope: {}", scope_label(record)),
        format!("source: {}", source(record.get("source"))),
        format!(
            "enabled: {}",
            if record.get("enabled").and_then(Value::as_bool) == Some(false) {
                "no"
            } else {
                "yes"
            }
        ),
        String::new(),
        "PUBLICATION".to_owned(),
    ];
    let Ok(derived) = derived(snapshot) else {
        derived_note(snapshot, &mut lines);
        return lines;
    };
    let slug = text(record, "slug");
    let scope = scope_label(record);
    let rows: Vec<&Value> = array(derived, "presentations")
        .iter()
        .filter(|row| text(row, "slug") == slug && text(row, "scope") == scope)
        .collect();
    if rows.is_empty() {
        lines.push("  not published by any adapter".to_owned());
    }
    for row in rows {
        let desired = match row.get("reason").and_then(Value::as_str) {
            Some(reason) => format!("left out ({})", reason.replace('_', " ")),
            None => "listed".to_owned(),
        };
        let applied = match row.get("applied").and_then(Value::as_bool) {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unknown",
        };
        lines.push(format!(
            "  {}: desired {desired}; applied {applied}",
            text(row, "adapter")
        ));
        if let Some(publication) = array(snapshot, "publications")
            .iter()
            .find(|publication| text(publication, "adapter") == text(row, "adapter"))
        {
            lines.push(format!("    {}", publication_summary(publication)));
        }
    }
    lines
}

fn publication_summary(publication: &Value) -> String {
    let revision = |key: &str| {
        publication
            .get(key)
            .and_then(Value::as_i64)
            .map_or_else(|| "none".to_owned(), |id| id.to_string())
    };
    let detail = publication
        .get("detail_code")
        .and_then(Value::as_str)
        .map(|code| format!(" ({code})"))
        .unwrap_or_default();
    format!(
        "state {}{detail}; revision {} applied, {} desired",
        text(publication, "state"),
        revision("applied_revision_id"),
        revision("desired_revision_id"),
    )
}

fn source(source: Option<&Value>) -> String {
    let Some(source) = source else {
        return "-".to_owned();
    };
    match text(source, "kind") {
        "exact_offer" => format!(
            "exact offer {} on {}",
            text(source, "model_id"),
            text(source, "binding_id")
        ),
        "virtual" => format!(
            "virtual, routes through {}",
            source
                .get("target")
                .map_or("-", |target| text(target, "route_id"))
        ),
        "offer_selector" => "offer selector".to_owned(),
        other => other.to_owned(),
    }
}

fn model_label(model: Option<&Value>) -> String {
    let Some(model) = model else {
        return "-".to_owned();
    };
    match text(model, "kind") {
        "exact" => format!("exact {}", text(model, "model_id")),
        "requested_model" => "the requested model".to_owned(),
        "template" => format!("template {}", text(model, "template")),
        "offer_selector" => "selected from the catalogue".to_owned(),
        other => other.to_owned(),
    }
}

fn route(snapshot: &Value, record: &Value) -> Vec<String> {
    let mut lines = vec![
        format!("{}  {}", text(record, "id"), text(record, "display_name")),
        format!("lane transition: {}", text(record, "lane_transition")),
    ];
    let derived = derived(snapshot);
    let facts = derived
        .ok()
        .and_then(|derived| find(array(derived, "routes"), "id", text(record, "id")));
    match (facts, derived) {
        (Some(facts), _) => {
            let lanes = &facts["pinned_lanes"];
            let count = |key: &str| lanes.get(key).and_then(Value::as_u64).unwrap_or(0);
            lines.push(format!(
                "pinned lanes: {} warm, {} cold, {} unknown",
                count("warm"),
                count("cold"),
                count("unknown")
            ));
        }
        (None, Err(note)) => lines.push(note.to_owned()),
        (None, Ok(_)) => lines.push("not in the compiled policy yet".to_owned()),
    }
    lines.push(String::new());
    lines.push("CANDIDATES".to_owned());
    let chain = &record["chain"];
    let mut steps: Vec<(&Value, Option<&Value>)> = vec![(&chain["first"], None)];
    steps.extend(
        array(chain, "steps")
            .iter()
            .map(|step| (&step["candidate"], Some(&step["edge"]))),
    );
    for (index, (candidate, edge)) in steps.into_iter().enumerate() {
        if let Some(edge) = edge {
            lines.extend(edge_lines(
                facts,
                steps_previous(record, index),
                candidate,
                edge,
            ));
        }
        lines.extend(candidate_lines(snapshot, facts, index, candidate));
    }
    lines
}

fn steps_previous(record: &Value, index: usize) -> &Value {
    let chain = &record["chain"];
    if index == 1 {
        &chain["first"]
    } else {
        &array(chain, "steps")[index - 2]["candidate"]
    }
}

fn candidate_lines(
    snapshot: &Value,
    facts: Option<&Value>,
    index: usize,
    candidate: &Value,
) -> Vec<String> {
    let id = text(candidate, "id");
    let derived_candidate = facts.and_then(|facts| find(array(facts, "candidates"), "id", id));
    let cost = derived_candidate.map_or("?", |derived| text(derived, "cost_class"));
    let binding = text(candidate, "binding_id");
    let enabled = if candidate.get("enabled").and_then(Value::as_bool) == Some(false) {
        "  DISABLED"
    } else {
        ""
    };
    let mut lines = vec![
        format!("{}. {id}  {binding}  [{cost}]{enabled}", index + 1),
        format!("     model: {}", model_label(candidate.get("model"))),
        format!("     now: {}", availability_summary(snapshot, binding, id)),
    ];
    if let Some(upgrade) = derived_candidate.and_then(|derived| derived.get("upgrade"))
        && !upgrade.is_null()
    {
        lines.push(format!(
            "     upgrade: {} -> {}",
            text(upgrade, "baseline"),
            upgrade
                .get("winner")
                .and_then(Value::as_str)
                .unwrap_or("no newer model")
        ));
    }
    lines
}

/// The line between two candidates: what a fallback across it is allowed to
/// do, and a mark where it crosses from a subscription to billed spend.
fn edge_lines(facts: Option<&Value>, from: &Value, to: &Value, edge: &Value) -> Vec<String> {
    let derived = facts.and_then(|facts| {
        array(facts, "edges").iter().find(|derived| {
            text(derived, "from") == text(from, "id") && text(derived, "to") == text(to, "id")
        })
    });
    let post_send = array(edge, "post_send")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let line = format!(
        "   v fallback {}; after send: {}; cache: {}",
        if text(edge, "preflight") == "disabled" {
            "off"
        } else {
            "on"
        },
        if post_send.is_empty() {
            "none".to_owned()
        } else {
            post_send.join(", ")
        },
        text(edge, "cache_transition").replace('_', " "),
    );
    let mut lines = vec![line];
    if let Some(derived) = derived
        && derived["crosses_cost_boundary"] == true
    {
        lines.push(format!(
            "     $ COST BOUNDARY {} -> {}, consent {}",
            text(derived, "from_cost"),
            text(derived, "to_cost"),
            text(derived, "billed_boundary"),
        ));
    }
    lines
}

/// One line on whether a binding can take a request now.
fn availability_summary(snapshot: &Value, binding: &str, candidate: &str) -> String {
    let Some(derived) = snapshot.get("derived") else {
        return "not reported".to_owned();
    };
    let Some(live) = find(array(derived, "availability"), "binding_id", binding) else {
        return "unknown binding".to_owned();
    };
    let state = |key: &str| live[key]["state"].as_str().unwrap_or("unknown");
    let mut parts = Vec::new();
    if state("configured") == "no" {
        parts.push("not configured".to_owned());
    } else if state("enabled") == "no" {
        parts.push("disabled".to_owned());
    } else if state("credential") == "no" {
        parts.push("no credential".to_owned());
    } else {
        parts.push("available".to_owned());
    }
    parts.push(format!(
        "catalogue {}",
        live["catalogue"]["state"].as_str().unwrap_or("?")
    ));
    let open = array(derived, "circuits")
        .iter()
        .filter(|circuit| text(circuit, "candidate_id") == candidate)
        .count();
    if open > 0 {
        parts.push(format!("{open} open circuit(s)"));
    }
    parts.join("; ")
}

fn default(snapshot: &Value, record: &Value) -> Vec<String> {
    let target = record
        .get("target")
        .map_or("-", |target| text(target, "route_id"));
    let mut lines = vec![
        format!("default for {}", scope_label(record)),
        format!("route: {target}"),
    ];
    let route = snapshot
        .get("draft")
        .filter(|draft| !draft.is_null())
        .or_else(|| snapshot.get("active_revision"))
        .map(|revision| revision.get("policy").unwrap_or(revision))
        .and_then(|policy| find(array(policy, "routes"), "id", target));
    if let Some(route) = route {
        lines.push(String::new());
        lines.extend(self::route(snapshot, route));
    }
    lines
}

fn rewrite(record: &Value) -> Vec<String> {
    let matcher = record.get("model_matcher");
    vec![
        format!("{}  priority {}", text(record, "id"), record["priority"]),
        format!("scope: {}", scope_label(record)),
        format!(
            "matches: {} {}",
            matcher.map_or("-", |matcher| text(matcher, "kind")),
            matcher.map_or("-", |matcher| text(matcher, "value"))
        ),
        format!(
            "routes to: {}",
            record
                .get("target")
                .map_or("-", |target| text(target, "route_id"))
        ),
    ]
}

fn classifier(snapshot: &Value, record: &Value) -> Vec<String> {
    let matcher = record.get("model_matcher");
    let mut lines = vec![
        format!("{}  on {}", text(record, "id"), text(record, "binding_id")),
        format!(
            "matches: {} {}",
            matcher.map_or("-", |matcher| text(matcher, "kind")),
            matcher.map_or("-", |matcher| text(matcher, "value"))
        ),
        format!(
            "family {}, version {} ({})",
            text(record, "family_template"),
            text(record, "version_template"),
            record
                .get("version_parser")
                .map_or("-", |parser| text(parser, "kind"))
        ),
        String::new(),
        "EXAMPLES FROM THE CATALOGUE".to_owned(),
    ];
    let Ok(derived) = derived(snapshot) else {
        derived_note(snapshot, &mut lines);
        return lines;
    };
    let Some(facts) = find(array(derived, "classifiers"), "id", text(record, "id")) else {
        lines.push("  not in the compiled policy yet".to_owned());
        return lines;
    };
    for example in array(facts, "parsed") {
        lines.push(format!(
            "  {} -> family {}, version {}",
            text(example, "model"),
            text(example, "family"),
            text(example, "version")
        ));
    }
    for example in array(facts, "unparsed") {
        lines.push(format!(
            "  {} -> matched, version not parsed",
            text(example, "model")
        ));
    }
    if array(facts, "parsed").is_empty() && array(facts, "unparsed").is_empty() {
        lines.push("  no catalogue model for this binding matches".to_owned());
    }
    lines
}

fn upgrade(snapshot: &Value, record: &Value) -> Vec<String> {
    let id = text(record, "id");
    let mut lines = vec![
        format!(
            "{}  classifier {}{}",
            id,
            text(record, "classifier_id"),
            if record.get("enabled").and_then(Value::as_bool) == Some(false) {
                "  DISABLED"
            } else {
                ""
            }
        ),
        format!("lane transition: {}", text(record, "lane_transition")),
        String::new(),
        "STANDING FOR A NEW LANE".to_owned(),
    ];
    let Ok(derived) = derived(snapshot) else {
        derived_note(snapshot, &mut lines);
        return lines;
    };
    let mut any = false;
    for route in array(derived, "routes") {
        for candidate in array(route, "candidates") {
            if text(candidate, "upgrade_policy_id") != id {
                continue;
            }
            any = true;
            lines.push(format!("{} / {}", text(route, "id"), text(candidate, "id")));
            let Some(standing) = candidate.get("upgrade").filter(|value| !value.is_null()) else {
                lines.push("  baseline depends on the request".to_owned());
                continue;
            };
            lines.push(format!(
                "  baseline {} -> {}",
                text(standing, "baseline"),
                standing
                    .get("winner")
                    .and_then(Value::as_str)
                    .unwrap_or("no newer model")
            ));
            for rejected in array(standing, "rejected") {
                lines.push(format!(
                    "    not {}: {}",
                    text(rejected, "model"),
                    text(rejected, "reason").replace('_', " ")
                ));
            }
        }
    }
    if !any {
        lines.push("  no route candidate uses this upgrade".to_owned());
    }
    lines
}

fn availability(snapshot: &Value, record: &Value) -> Vec<String> {
    let state = |key: &str| {
        let evidence = &record[key];
        match evidence.get("reason").and_then(Value::as_str) {
            Some(reason) => format!("{} ({reason})", text(evidence, "state")),
            None => text(evidence, "state").to_owned(),
        }
    };
    let mut lines = vec![
        format!(
            "{}  {}",
            text(record, "binding_id"),
            text(record, "provider")
        ),
        format!("cost class: {}", text(record, "cost_class")),
        format!("configured: {}", state("configured")),
        format!("enabled: {}", state("enabled")),
        format!("credential: {}", state("credential")),
        format!("catalogue: {}", text(&record["catalogue"], "state")),
        String::new(),
        "CAPABILITIES".to_owned(),
    ];
    if let Some(capabilities) = record["capabilities"].as_object() {
        for (name, value) in capabilities {
            lines.push(format!(
                "  {}: {}",
                name.replace('_', " "),
                value.as_str().unwrap_or("?")
            ));
        }
    }
    let circuits: Vec<&Value> = snapshot
        .get("derived")
        .map(|derived| array(derived, "circuits"))
        .unwrap_or_default()
        .iter()
        .filter(|circuit| text(circuit, "binding_id") == text(record, "binding_id"))
        .collect();
    if !circuits.is_empty() {
        lines.push(String::new());
        lines.push("OPEN CIRCUITS".to_owned());
        for circuit in circuits {
            lines.push(format!(
                "  {} / {} on {}: {}",
                text(circuit, "route_id"),
                text(circuit, "candidate_id"),
                text(circuit, "model_id"),
                text(circuit, "failure")
            ));
        }
    }
    lines
}

fn publication(record: &Value) -> Vec<String> {
    let scope = &record["scope"];
    vec![
        format!(
            "{}  {}",
            text(record, "adapter"),
            match text(scope, "kind") {
                "profile" => format!("{}/{}", text(scope, "profile"), text(scope, "protocol")),
                _ => text(scope, "protocol").to_owned(),
            }
        ),
        publication_summary(record),
        format!(
            "inventory generation {} applied, {} desired",
            record
                .get("applied_inventory_generation")
                .map_or("none".to_owned(), Value::to_string),
            record["desired_inventory_generation"]
        ),
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Entry, lines};

    fn snapshot() -> serde_json::Value {
        json!({
            "draft": {"policy": {}},
            "draft_diagnostics": [
                {"code": "binding-unconfigured", "severity": "info", "record_id": "mixed", "path": "/routes/0"},
            ],
            "publications": [
                {"adapter": "dynamic_responses_models", "state": "applied",
                 "applied_revision_id": 2, "desired_revision_id": 3,
                 "scope": {"kind": "protocol", "protocol": "openai_responses"}},
            ],
            "derived": {
                "availability": [{
                    "binding_id": "codex_sub.codex_responses", "provider": "codex_sub",
                    "configured": {"state": "yes"}, "enabled": {"state": "yes"},
                    "credential": {"state": "unknown"}, "catalogue": {"state": "fresh"},
                    "capabilities": {}, "cost_class": "subscription",
                }],
                "circuits": [],
                "draft": {
                    "routes": [{
                        "id": "mixed",
                        "pinned_lanes": {"warm": 2, "cold": 1, "unknown": 0},
                        "candidates": [
                            {"id": "sub", "binding_id": "codex_sub.codex_responses",
                             "cost_class": "subscription", "upgrade_policy_id": null, "upgrade": null},
                            {"id": "billed", "binding_id": "openrouter.openrouter_responses",
                             "cost_class": "billed", "upgrade_policy_id": null, "upgrade": null},
                        ],
                        "edges": [{"from": "sub", "to": "billed", "from_cost": "subscription",
                                   "to_cost": "billed", "crosses_cost_boundary": true,
                                   "billed_boundary": "allow"}],
                    }],
                    "classifiers": [],
                    "presentations": [{
                        "scope": "openai_responses", "slug": "mixed",
                        "adapter": "dynamic_responses_models",
                        "desired": "left_out", "reason": "no_codex_record", "applied": null,
                    }],
                },
                "active": null,
            },
        })
    }

    fn route() -> serde_json::Value {
        json!({
            "id": "mixed", "display_name": "Mixed", "lane_transition": "move_when_cold",
            "chain": {
                "first": {"id": "sub", "binding_id": "codex_sub.codex_responses",
                          "model": {"kind": "exact", "model_id": "gpt-model"}},
                "steps": [{
                    "edge": {"preflight": "enabled", "post_send": ["rate_limit"],
                             "billed_boundary": "allow", "cache_transition": "allow_cache_loss"},
                    "candidate": {"id": "billed", "binding_id": "openrouter.openrouter_responses",
                                  "model": {"kind": "requested_model"}},
                }],
            },
        })
    }

    #[test]
    fn a_route_shows_its_cost_boundary_pinned_lanes_and_diagnostics() {
        let snapshot = snapshot();
        let record = route();
        let text = lines(
            &snapshot,
            &Entry {
                key: "routes",
                value: &record,
            },
        )
        .join("\n");

        assert!(
            text.contains("pinned lanes: 2 warm, 1 cold, 0 unknown"),
            "{text}"
        );
        assert!(
            text.contains("1. sub  codex_sub.codex_responses  [subscription]"),
            "{text}"
        );
        assert!(
            text.contains("$ COST BOUNDARY subscription -> billed, consent allow"),
            "{text}"
        );
        assert!(text.contains("now: available; catalogue fresh"), "{text}");
        assert!(
            text.contains("info binding-unconfigured at /routes/0"),
            "{text}"
        );
    }

    #[test]
    fn a_presentation_says_what_each_adapter_lists_and_holds() {
        let snapshot = snapshot();
        let record = json!({
            "id": "p", "slug": "mixed", "label": "Mixed",
            "scope": {"kind": "protocol", "protocol": "openai_responses"},
            "source": {"kind": "virtual", "target": {"route_id": "mixed"}},
        });
        let text = lines(
            &snapshot,
            &Entry {
                key: "presentations",
                value: &record,
            },
        )
        .join("\n");

        assert!(text.contains("virtual, routes through mixed"), "{text}");
        assert!(
            text.contains(
                "dynamic_responses_models: desired left out (no codex record); applied unknown"
            ),
            "{text}"
        );
        assert!(
            text.contains("state applied; revision 2 applied, 3 desired"),
            "{text}"
        );
    }

    #[test]
    fn a_draft_that_does_not_compile_has_no_derived_facts_to_show() {
        let mut snapshot = snapshot();
        snapshot["derived"]["draft"] = serde_json::Value::Null;
        let record = route();
        let text = lines(
            &snapshot,
            &Entry {
                key: "routes",
                value: &record,
            },
        )
        .join("\n");

        assert!(
            text.contains("no derived facts: the draft does not compile"),
            "{text}"
        );
        assert!(!text.contains("COST BOUNDARY"), "{text}");
    }
}
