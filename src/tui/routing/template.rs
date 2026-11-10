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

/// What a kind of record is called, one of them.
pub(super) fn singular(kind: &str) -> &'static str {
    match kind {
        "presentations" => "presentation",
        "routes" => "route",
        "defaults" => "default",
        "rewrites" => "rewrite",
        "classifiers" => "classifier (family)",
        "upgrades" => "upgrade",
        "compaction" => "compaction policy",
        _ => "record",
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
        "rewrites" => json!({
            "id": "new-rewrite",
            "scope": { "kind": "protocol", "protocol": "openai_chat" },
            "priority": 0,
            "model_matcher": { "kind": "exact", "value": "model-name" },
            "target": { "route_id": route_id },
        }),
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

#[cfg(test)]
mod tests {
    use super::{kind_for, template};
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
}
