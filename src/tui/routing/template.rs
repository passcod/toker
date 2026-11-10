//! Starting points for adding policy records from the routing page.
//!
//! Each template is the smallest record of its kind that the daemon accepts
//! as an addition: it parses, passes the document's own checks, and carries a
//! placeholder id. Anything that must name another record (a route behind a
//! default, a classifier behind an upgrade) can still dangle, which the form
//! marks and the validate step reports, so the user fills the template in
//! before saving.

use serde_json::{Value, json};

use super::model::{Panel, Tab};
use crate::policy::MAX_ID_BYTES;

/// The kind of record a tab adds. The models tab holds several, by the
/// panel the focus is in.
pub(super) fn kind_for(tab: Tab, focus: Panel) -> Option<&'static str> {
    match tab {
        Tab::Presentations => Some("presentations"),
        Tab::Rewrites => Some("rewrites"),
        Tab::Defaults => Some("defaults"),
        Tab::Routes => Some("routes"),
        Tab::Models => Some(match focus {
            Panel::Families => "classifiers",
            Panel::Upgrades | Panel::Offers => "upgrades",
            Panel::Compaction => "compaction",
        }),
        Tab::History => None,
    }
}

/// The template for a new record of `kind`. A new default points at
/// `route_id`, which is the first route of the live policy when it has one.
pub(super) fn template(kind: &str, route_id: Option<&str>) -> Value {
    let route_id = route_id.unwrap_or("route-id");
    match kind {
        "presentations" => json!({
            "id": "new-presentation",
            "scope": { "kind": "protocol", "protocol": "anthropic_messages" },
            "slug": "new-model",
            "label": "New model",
            "enabled": true,
            "source": {
                "kind": "exact_offer",
                "binding_id": "anthropic_sub.anthropic_messages",
                "model_id": "model-id",
            },
        }),
        "routes" => json!({
            "id": "new-route",
            "display_name": "New route",
            "chain": {
                "first": {
                    "id": "new-candidate",
                    "binding_id": "anthropic_sub.anthropic_messages",
                    "model": { "kind": "exact", "model_id": "model-id" },
                    "enabled": true,
                },
                "steps": [],
            },
            "lane_transition": "move_when_cold",
        }),
        "defaults" => json!({
            "scope": { "kind": "protocol", "protocol": "openai_chat" },
            "target": { "route_id": route_id },
        }),
        "rewrites" => {
            let matcher = json!({ "kind": "exact", "value": "model-name" });
            json!({
                "id": rewrite_id(&matcher, route_id, &[]),
                "scope": { "kind": "protocol", "protocol": "openai_chat" },
                "priority": 0,
                "model_matcher": matcher,
                "target": { "route_id": route_id },
            })
        }
        "classifiers" => json!({
            "id": "new-classifier",
            "binding_id": "anthropic_sub.anthropic_messages",
            "model_matcher": {
                "kind": "regex",
                "value": r"^(?P<family>[a-z]+)-(?P<version>[0-9]+\.[0-9]+)$",
            },
            "family_template": "${family}",
            "version_template": "${version}",
            "version_parser": {
                "kind": "numeric_tuple", "components": 2, "separator": ".",
            },
        }),
        "upgrades" => json!({
            "id": "new-upgrade",
            "classifier_id": "new-classifier",
            "enabled": true,
            "minimum_learning": { "kind": "explicit_promotion" },
            "allow_manual_promotion": true,
            "inventory_sources": ["catalogue", "served", "learned"],
            "lane_transition": "move_when_cold",
        }),
        "compaction" => json!({
            "target": { "route_id": route_id },
            "lane_transition": "move_when_cold",
            "cache_transition": "allow_when_cold",
        }),
        _ => json!({}),
    }
}

/// Room kept at the end of a made identifier for the suffix that tells it
/// apart from one already taken.
const SUFFIX_ROOM: usize = 8;

/// An identifier for a copy of the record `id` names, unlike any in `taken`
/// and within the length the document allows.
pub(super) fn fresh_id(id: &str, taken: &[&str]) -> String {
    let stem = clip(id, MAX_ID_BYTES - SUFFIX_ROOM - "-copy".len());
    unique(format!("{stem}-copy"), taken)
}

/// The identifier of a new rewrite, which the page never shows and so never
/// asks for (spec: UTUI#editing): what it matches and the route it sends to,
/// in the spelling an identifier allows (spec: RDOC#parsing), unlike any in
/// `taken`.
pub(super) fn rewrite_id(matcher: &Value, route_id: &str, taken: &[&str]) -> String {
    let part = (MAX_ID_BYTES - SUFFIX_ROOM - "-to-".len()) / 2;
    let matched = slug(matcher.get("value").and_then(Value::as_str).unwrap_or(""));
    let route = slug(route_id);
    let (matched, route) = (clip(&matched, part), clip(&route, part));
    let base = match (matched.is_empty(), route.is_empty()) {
        (true, true) => "rewrite".to_owned(),
        (false, true) => matched.to_owned(),
        (true, false) => format!("to-{route}"),
        (false, false) => format!("{matched}-to-{route}"),
    };
    unique(base, taken)
}

/// `text` lower-cased, with each run of characters an identifier cannot
/// hold, or of separators, made one hyphen: `^claude-(.+)$` is `claude`.
fn slug(text: &str) -> String {
    let mut slug = String::new();
    for character in text.chars().map(|character| character.to_ascii_lowercase()) {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            slug.push(character);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_owned()
}

/// `base`, or `base-2`, `base-3` and so on: the first that is not taken.
fn unique(base: String, taken: &[&str]) -> String {
    if !taken.contains(&base.as_str()) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !taken.contains(&id.as_str()))
        .expect("an unused suffix exists")
}

/// The first `bytes` of an identifier, which is ASCII, without a separator
/// left dangling at the end.
fn clip(id: &str, bytes: usize) -> &str {
    id[..id.len().min(bytes)].trim_end_matches(['-', '_', '.'])
}

#[cfg(test)]
mod tests {
    use super::{fresh_id, kind_for, rewrite_id, template};
    use crate::policy::{PolicyDocument, PolicyEdit};
    use crate::tui::routing::editing::add_edit;
    use crate::tui::routing::model::{Panel, Tab};

    fn empty() -> PolicyDocument {
        serde_json::from_value(serde_json::json!({ "format_version": 3 })).expect("document")
    }

    /// The daemon's own edit vocabulary decides whether a template is a record
    /// it will add, so each one is pushed through it.
    #[test]
    fn every_template_is_an_addition_the_daemon_accepts() {
        for (tab, focus) in [
            (Tab::Presentations, Panel::Upgrades),
            (Tab::Rewrites, Panel::Upgrades),
            (Tab::Defaults, Panel::Upgrades),
            (Tab::Routes, Panel::Upgrades),
            (Tab::Models, Panel::Families),
            (Tab::Models, Panel::Upgrades),
            (Tab::Models, Panel::Compaction),
        ] {
            if let Some(kind) = kind_for(tab, focus) {
                let operation = add_edit(kind, template(kind, None));
                let edit: PolicyEdit =
                    serde_json::from_value(serde_json::json!({ "operations": [operation] }))
                        .unwrap_or_else(|error| panic!("{kind} template does not parse: {error}"));
                edit.apply(&empty())
                    .unwrap_or_else(|error| panic!("{kind} template is refused: {error}"));
            }
        }
    }

    #[test]
    fn a_new_default_points_at_the_given_route() {
        let default = template("defaults", Some("my-route"));
        assert_eq!(default["target"]["route_id"], "my-route");
        let default = template("defaults", None);
        assert_eq!(default["target"]["route_id"], "route-id");
    }

    #[test]
    fn a_rewrites_identifier_is_made_from_its_match_and_route() {
        let exact = serde_json::json!({"kind": "exact", "value": "Claude-Sonnet-4.5"});
        assert_eq!(rewrite_id(&exact, "fast", &[]), "claude-sonnet-4-5-to-fast");
        let regex = serde_json::json!({"kind": "regex", "value": "^claude-(?P<model>.+)$"});
        assert_eq!(rewrite_id(&regex, "fast", &[]), "claude-p-model-to-fast");
        let glob = serde_json::json!({"kind": "glob", "value": "*"});
        assert_eq!(rewrite_id(&glob, "fast", &[]), "to-fast");
        assert_eq!(rewrite_id(&glob, "", &[]), "rewrite");
        // One already taken is told apart.
        assert_eq!(
            rewrite_id(&exact, "fast", &["claude-sonnet-4-5-to-fast"]),
            "claude-sonnet-4-5-to-fast-2"
        );
        // However long what it is made from, it is an identifier the
        // document accepts.
        let long = serde_json::json!({"kind": "exact", "value": "x".repeat(1_000)});
        let route = "r".repeat(crate::policy::MAX_ID_BYTES);
        let id = rewrite_id(&long, &route, &[]);
        assert!(crate::policy::StableId::new(id.clone()).is_ok(), "{id}");
        let taken = [id.as_str()];
        let again = rewrite_id(&long, &route, &taken);
        assert!(
            crate::policy::StableId::new(again.clone()).is_ok(),
            "{again}"
        );
    }

    #[test]
    fn a_copy_gets_an_identifier_no_record_has() {
        assert_eq!(fresh_id("fast", &["fast"]), "fast-copy");
        assert_eq!(
            fresh_id("fast", &["fast", "fast-copy", "fast-copy-2"]),
            "fast-copy-3"
        );
        // A copy of a long identifier still fits the document's bound.
        let long = "a".repeat(crate::policy::MAX_ID_BYTES);
        let copy = fresh_id(&long, &[long.as_str()]);
        assert!(crate::policy::StableId::new(copy.clone()).is_ok(), "{copy}");
    }
}
