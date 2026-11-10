//! What the routing page shows beyond the policy document itself: live
//! availability, cost-boundary edges, pinned lanes, classifier examples, the
//! upgrade standing, per-presentation publication, and the facts about each
//! record that the tables and details read: its reach, how many requests it
//! caught, what shadows it, what refers to it and what depends on each
//! provider.
//!
//! The daemon computes it and the terminal renders it, because the page holds
//! no store, registry or learning view and a fact about policy is not the
//! page's to work out. Everything here is counts, ids and model names the
//! offers already carry; no content.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::catalog::offers::{OfferInventory, PolicyPublication, render_policy};
use crate::policy::shadow::Shape;
use crate::policy::{
    AvailabilitySnapshot, BindingAvailability, BindingId, CatalogueAvailability, CircuitSnapshot,
    CompiledClassifier, CompiledPolicy, CompiledPresentation, CostClass, EdgeSetting, Evidence,
    FrontendProtocol, FrontendScope, LearningView, MatcherSpec, PolicyDocument, Reach, RouteId,
    route_reach,
};
use crate::routing::{Capabilities, CapabilityEvidence};
use crate::server::RoutingLaneImpacts;
use crate::store::{HitCounts, HitCoverage, RoutingPublication};

mod models;
mod providers;
mod routes;
mod rules;

#[cfg(test)]
mod facts_tests;

/// The adapters a policy publishes through, by the frontend each serves.
const CLAUDE_ADAPTER: &str = "claude_settings";
const CHAT_ADAPTER: &str = "dynamic_chat_models";
const RESPONSES_ADAPTER: &str = "dynamic_responses_models";

/// Examples shown per classifier: parsed ones and ones the matcher accepts
/// but the parser rejects.
const PARSED_EXAMPLES: usize = 4;
const UNPARSED_EXAMPLES: usize = 2;

pub(crate) struct Inputs<'a, L> {
    pub availability: &'a AvailabilitySnapshot,
    pub inventory: &'a OfferInventory,
    pub learning: &'a L,
    pub circuits: &'a CircuitSnapshot,
    pub pinned: &'a BTreeMap<RouteId, RoutingLaneImpacts>,
    pub publications: &'a [RoutingPublication],
    /// The active policy, where it compiles.
    pub active: Option<ActivePolicy<'a>>,
    pub hits: &'a HitCounts,
    /// Lanes pinned to each revision, by revision.
    pub revision_lanes: &'a BTreeMap<i64, RoutingLaneImpacts>,
    /// Which lanes pinned to each revision hold a cache that moving them onto
    /// another revision would lose, by revision.
    pub revision_at_risk: &'a BTreeMap<i64, Vec<String>>,
}

/// A compiled policy with the document it was compiled from, which holds what
/// compilation drops: the defaults, and a matcher's own spelling. They travel
/// together so no caller can pair a policy with another revision's document.
#[derive(Clone, Copy)]
pub(crate) struct ActivePolicy<'a> {
    pub policy: &'a CompiledPolicy,
    pub document: &'a PolicyDocument,
}

/// What every fact about a record is worked out from: one policy, one
/// availability and one inventory, so no two facts disagree about the world.
pub(super) struct Context<'a> {
    pub policy: &'a CompiledPolicy,
    pub document: &'a PolicyDocument,
    pub availability: &'a AvailabilitySnapshot,
    pub inventory: &'a OfferInventory,
    pub hits: &'a HitCounts,
    /// The names a client could send, for judging a glob or a regular
    /// expression: each offer's own name, and under its provider's prefix, and
    /// every name a presentation publishes.
    pub names: BTreeSet<String>,
    /// The reach of each route walked from its first candidate.
    pub reach: BTreeMap<RouteId, Reach>,
}

impl<'a> Context<'a> {
    fn new(
        policy: &'a CompiledPolicy,
        document: &'a PolicyDocument,
        availability: &'a AvailabilitySnapshot,
        inventory: &'a OfferInventory,
        hits: &'a HitCounts,
    ) -> Self {
        let mut names = BTreeSet::new();
        for offer in inventory.iter() {
            names.insert(offer.backend_model_id.clone());
            names.insert(format!("{}/{}", offer.provider, offer.backend_model_id));
        }
        for (_, slug, _) in policy.all_presentations() {
            names.insert(slug.to_owned());
        }
        let reach = policy
            .routes()
            .map(|route| {
                (
                    route.id().clone(),
                    route_reach(policy, route, availability, inventory),
                )
            })
            .collect();
        Self {
            policy,
            document,
            availability,
            inventory,
            hits,
            names,
            reach,
        }
    }

    /// The provider a binding belongs to: the availability's word for it,
    /// and failing that the binding identifier's own provider half.
    pub(super) fn provider_of(&self, binding: &BindingId) -> String {
        self.availability.get(binding).map_or_else(
            || {
                binding
                    .as_str()
                    .split_once('.')
                    .map_or(binding.as_str(), |(provider, _)| provider)
                    .to_owned()
            },
            |live| live.provider.clone(),
        )
    }

    pub(super) fn cost_of(&self, binding: &BindingId) -> CostClass {
        self.availability
            .get(binding)
            .map_or(CostClass::Unknown, |live| live.cost_class)
    }
}

/// The derived content for the active revision.
pub(crate) fn build<L: LearningView>(inputs: &Inputs<'_, L>) -> Value {
    json!({
        "availability": inputs
            .availability
            .iter()
            .map(|(id, availability)| availability_body(id, availability))
            .collect::<Vec<_>>(),
        "circuits": inputs
            .circuits
            .iter()
            .map(|circuit| json!({
                "route_id": circuit.route_id,
                "candidate_id": circuit.candidate_id,
                "binding_id": circuit.binding_id,
                "model_id": circuit.model_id,
                "failure": circuit.failure,
            }))
            .collect::<Vec<_>>(),
        "active": inputs.active.map(|active| policy_body(active, inputs)),
        "revisions": inputs
            .revision_lanes
            .iter()
            .map(|(revision_id, lanes)| {
                let mut body = lanes_body(Some(lanes));
                body["revision_id"] = json!(revision_id);
                body["at_risk"] = json!(
                    inputs
                        .revision_at_risk
                        .get(revision_id)
                        .cloned()
                        .unwrap_or_default()
                );
                body
            })
            .collect::<Vec<_>>(),
    })
}

fn policy_body<L: LearningView>(active: ActivePolicy<'_>, inputs: &Inputs<'_, L>) -> Value {
    let ActivePolicy { policy, document } = active;
    let ctx = Context::new(
        policy,
        document,
        inputs.availability,
        inputs.inventory,
        inputs.hits,
    );
    let slugs = presentations_body(policy, inputs);
    let rules = rules::build(&ctx, &slugs);
    let upgrades = models::upgrades(&ctx, inputs);
    let routes: Vec<Value> = policy
        .routes()
        .map(|route| routes::route_body(&ctx, route, inputs, &rules, &upgrades))
        .collect();
    let first_refused_tab = first_refused_tab(&rules, &routes);
    json!({
        "routes": routes,
        "classifiers": policy
            .classifiers()
            .map(|classifier| classifier_body(classifier, document, inputs.inventory))
            .collect::<Vec<_>>(),
        "presentations": slugs,
        "scopes": rules.scopes,
        "presentation_records": rules.presentations,
        "rewrites": rules.rewrites,
        "defaults": rules.defaults,
        "upgrades": upgrades.body(),
        "compaction": models::compaction(&ctx),
        "providers": providers::providers_body(&ctx, &rules),
        "first_refused_tab": first_refused_tab,
    })
}

/// The first lookup tab, in the order a request is resolved, holding a record
/// whose reach is refused (spec: UTUI#the-routing-page). A scope with no
/// default is not a record, so it does not count.
fn first_refused_tab(rules: &rules::Rules, routes: &[Value]) -> Value {
    let refused = |records: &[Value]| {
        records
            .iter()
            .any(|record| record["reach"]["refused"] == json!(true))
    };
    let existing_defaults: Vec<Value> = rules
        .defaults
        .iter()
        .filter(|default| default["exists"] == json!(true))
        .cloned()
        .collect();
    [
        ("presentations", refused(&rules.presentations)),
        ("rewrites", refused(&rules.rewrites)),
        ("defaults", refused(&existing_defaults)),
        ("routes", refused(routes)),
    ]
    .into_iter()
    .find_map(|(tab, any)| any.then(|| json!(tab)))
    .unwrap_or(Value::Null)
}

/// A matcher in the notation it is shown in (spec: UTUI#the-routing-page): a
/// prefix with the part it captures rather than the regular expression it
/// compiles to. `value` is the pattern as stored.
pub(super) fn matcher_body(spec: &MatcherSpec) -> Value {
    let shape = Shape::of(spec);
    let mut body = json!({
        "kind": shape.kind(),
        "text": shape.text(),
        "value": spec.value(),
    });
    if let Shape::Prefix {
        prefix, capture, ..
    } = &shape
    {
        body["prefix"] = json!(prefix);
        body["capture"] = json!(capture);
    }
    body
}

/// A scope as the page groups by it.
pub(super) fn scope_fields(scope: &FrontendScope) -> Value {
    json!({
        "scope": scope,
        "scope_label": scope_label(scope),
        "protocol": super::routing::protocol_name(protocol_of(scope)),
        "profile": match scope {
            FrontendScope::Profile { profile, .. } => Some(profile.as_str()),
            FrontendScope::Protocol { .. } => None,
        },
    })
}

/// How many requests a record caught in the last 24 hours, and how far to
/// trust it: `exact` where every row in the window recorded what matched,
/// `floor` where some predate that, and `not_recorded` where all do, in which
/// case there is no count to show (spec: UTUI#reading-a-table).
pub(super) fn hits_body(hits: &HitCounts, count: u64) -> Value {
    let (count, state) = match hits.coverage {
        HitCoverage::Complete => (Some(count), "exact"),
        HitCoverage::Floor => (Some(count), "floor"),
        HitCoverage::NotRecorded => (None, "not_recorded"),
    };
    json!({"count": count, "state": state})
}

fn cost_flag(cost: CostClass) -> Option<bool> {
    match cost {
        CostClass::Subscription => Some(false),
        CostClass::Billed => Some(true),
        CostClass::Unknown => None,
    }
}

/// A reach as the page reads it: the outcome, whether it is a refusal, what
/// serves it, which candidates were passed over and why, and the edge or
/// setting that stopped a refused walk.
pub(super) fn reach_body(ctx: &Context<'_>, reach: &Reach, route_id: Option<&RouteId>) -> Value {
    let serving = |serving: &crate::policy::Serving| {
        json!({
            "candidate_id": serving.candidate_id,
            "binding_id": serving.binding_id,
            "provider": ctx.provider_of(&serving.binding_id),
            "cost": cost_name(serving.cost),
            // `null` where the cost is not known, which is not the same as
            // not billed.
            "billed": cost_flag(serving.cost),
        })
    };
    let skipped = |skipped: &[crate::policy::Skipped]| {
        skipped
            .iter()
            .map(|skip| {
                json!({
                    "candidate_id": skip.candidate_id,
                    "binding_id": skip.binding_id,
                    "provider": ctx.provider_of(&skip.binding_id),
                    "reason": skip.reason.code(),
                })
            })
            .collect::<Vec<_>>()
    };
    let mut body = json!({
        "outcome": reach.outcome(),
        "refused": reach.is_refused(),
        "route_id": route_id,
        "serving": Value::Null,
        "skipped": [],
        "edge": Value::Null,
        "cause": Value::Null,
    });
    match reach {
        Reach::ServedFirst { serving: served } => body["serving"] = serving(served),
        Reach::ServedLater {
            serving: served,
            skipped: passed,
        } => {
            body["serving"] = serving(served);
            body["skipped"] = json!(skipped(passed));
        }
        Reach::RefusedEdge {
            skipped: passed,
            edge,
        } => {
            body["skipped"] = json!(skipped(passed));
            body["edge"] = json!({
                "from": edge.from,
                "to": edge.to,
                "setting": match edge.setting {
                    EdgeSetting::Preflight => "preflight",
                    EdgeSetting::BilledBoundary => "billed_boundary",
                },
                "value": match edge.setting {
                    EdgeSetting::Preflight => "disabled",
                    EdgeSetting::BilledBoundary => "deny",
                },
            });
        }
        Reach::RefusedUnavailable { skipped: passed } => body["skipped"] = json!(skipped(passed)),
        Reach::RefusedNoRoute(cause) => {
            body["cause"] = json!(match cause {
                crate::policy::NoRoute::NoDefault => "no_default",
                crate::policy::NoRoute::RouteMissing => "route_missing",
            });
        }
    }
    body
}

fn availability_body(binding: &BindingId, availability: &BindingAvailability) -> Value {
    let catalogue = match availability.catalogue {
        CatalogueAvailability::Fresh { fetched_at_ms } => {
            json!({"state": "fresh", "fetched_at_ms": fetched_at_ms})
        }
        CatalogueAvailability::Stale { fetched_at_ms } => {
            json!({"state": "stale", "fetched_at_ms": fetched_at_ms})
        }
        CatalogueAvailability::Absent => json!({"state": "absent"}),
    };
    json!({
        "binding_id": binding,
        "provider": availability.provider,
        "configured": evidence_body(availability.configured),
        "enabled": evidence_body(availability.enabled),
        "credential": evidence_body(availability.credential),
        "catalogue": catalogue,
        "capabilities": capabilities_body(availability.capabilities),
        "cost_class": cost_name(availability.cost_class),
    })
}

fn evidence_body(evidence: Evidence<&'static str>) -> Value {
    match evidence {
        Evidence::Positive(reason) => json!({"state": "yes", "reason": reason}),
        Evidence::Negative(reason) => json!({"state": "no", "reason": reason}),
        Evidence::Unknown => json!({"state": "unknown"}),
    }
}

fn capabilities_body(capabilities: Capabilities) -> Value {
    let name = |evidence: CapabilityEvidence| match evidence {
        CapabilityEvidence::Supported => "supported",
        CapabilityEvidence::Unsupported => "unsupported",
        CapabilityEvidence::Unknown => "unknown",
    };
    json!({
        "sampling": name(capabilities.sampling),
        "system_in_messages": name(capabilities.system_in_messages),
        "thinking_replay": name(capabilities.thinking_replay),
        "images": name(capabilities.images),
        "tools": name(capabilities.tools),
        "tool_choice_none": name(capabilities.tool_choice_none),
        "tool_choice_named": name(capabilities.tool_choice_named),
        "tool_choice_unnamed": name(capabilities.tool_choice_unnamed),
        "tool_choice_other": name(capabilities.tool_choice_other),
    })
}

fn cost_name(cost: CostClass) -> &'static str {
    match cost {
        CostClass::Subscription => "subscription",
        CostClass::Billed => "billed",
        CostClass::Unknown => "unknown",
    }
}

fn lanes_body(impacts: Option<&RoutingLaneImpacts>) -> Value {
    let impacts = impacts.copied().unwrap_or_default();
    json!({
        "warm": impacts.warm,
        "cold": impacts.cold,
        "unknown": impacts.unknown,
        "total": impacts.total,
    })
}

fn rejection_name(reason: crate::policy::UpgradeRejection) -> &'static str {
    use crate::policy::UpgradeRejection;
    match reason {
        UpgradeRejection::FamilyMismatch => "family_mismatch",
        UpgradeRejection::NotNewer => "not_newer",
        UpgradeRejection::LearningMinimum => "learning_minimum",
        UpgradeRejection::ContextFit => "context_fit",
    }
}

fn classifier_body(
    classifier: &CompiledClassifier,
    document: &PolicyDocument,
    inventory: &OfferInventory,
) -> Value {
    let mut parsed = Vec::new();
    let mut unparsed = Vec::new();
    let mut models: Vec<&str> = inventory
        .iter()
        .filter(|offer| offer.binding_id == *classifier.binding_id())
        .map(|offer| offer.backend_model_id.as_str())
        .collect();
    models.sort_unstable();
    for model in models {
        match classifier.classify(model) {
            Ok(Some(classified)) => {
                if parsed.len() < PARSED_EXAMPLES {
                    parsed.push(json!({
                        "model": model,
                        "family": classified.family,
                        "version": classified.version.to_string(),
                    }));
                }
            }
            // A classifier that does not parse a model either never matched
            // it, which is no example, or matched and could not read a
            // version, which is the one worth seeing.
            Ok(None) if classifier.matches(model).unwrap_or(false) => {
                if unparsed.len() < UNPARSED_EXAMPLES {
                    unparsed.push(json!({"model": model}));
                }
            }
            Ok(None) | Err(_) => {}
        }
    }
    // The matcher as written, in its own notation: the compiled one holds only
    // the expression it became (spec: UTUI#models).
    let matcher = document
        .classifiers
        .iter()
        .find(|written| written.id == *classifier.id())
        .map_or(Value::Null, |written| matcher_body(&written.model_matcher));
    json!({
        "id": classifier.id(),
        "binding_id": classifier.binding_id(),
        "matcher": matcher,
        "parsed": parsed,
        "unparsed": unparsed,
    })
}

fn adapter_of(protocol: FrontendProtocol) -> &'static str {
    match protocol {
        FrontendProtocol::AnthropicMessages => CLAUDE_ADAPTER,
        FrontendProtocol::OpenaiChat => CHAT_ADAPTER,
        FrontendProtocol::OpenaiResponses => RESPONSES_ADAPTER,
    }
}

fn protocol_of(scope: &FrontendScope) -> FrontendProtocol {
    match scope {
        FrontendScope::Profile { protocol, .. } | FrontendScope::Protocol { protocol } => *protocol,
    }
}

fn scope_label(scope: &FrontendScope) -> String {
    match scope {
        FrontendScope::Profile { profile, protocol } => {
            format!(
                "{}/{}",
                profile.as_str(),
                super::routing::protocol_name(*protocol)
            )
        }
        FrontendScope::Protocol { protocol } => super::routing::protocol_name(*protocol).to_owned(),
    }
}

/// Whether the slug is listed to its frontend, and if not why. The listing
/// is whatever `render_policy` would publish, so this cannot disagree with
/// `/v1/models`.
fn presentations_body<L: LearningView>(
    policy: &CompiledPolicy,
    inputs: &Inputs<'_, L>,
) -> Vec<Value> {
    let mut rows = Vec::new();
    for (scope, slug, presentation) in policy.all_presentations() {
        let protocol = protocol_of(&scope);
        let profile = match &scope {
            FrontendScope::Profile { profile, .. } => Some(profile),
            FrontendScope::Protocol { .. } => None,
        };
        let adapter = adapter_of(protocol);
        let (desired, reason) = desired_state(
            policy,
            inputs.inventory,
            profile,
            protocol,
            slug,
            presentation,
        );
        rows.push(json!({
            "id": presentation.id(),
            "scope": scope_label(&scope),
            "slug": slug,
            "adapter": adapter,
            "desired": desired,
            "reason": reason,
            "applied": applied_state(inputs, desired, slug, adapter),
        }));
    }
    rows
}

fn desired_state(
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
    profile: Option<&crate::policy::ProfileId>,
    protocol: FrontendProtocol,
    slug: &str,
    presentation: &CompiledPresentation,
) -> (&'static str, Option<&'static str>) {
    if !presentation.enabled() {
        return ("left_out", Some("disabled"));
    }
    if !policy.publication_slugs(profile, protocol).contains(slug) {
        return ("left_out", Some("not_published"));
    }
    match render_policy(policy, inventory, profile, protocol) {
        PolicyPublication::Unresolved => ("left_out", Some("offer_missing")),
        PolicyPublication::Ready(listing) => {
            let listed = listing_slugs(&listing).any(|listed| listed == slug);
            if listed {
                ("listed", None)
            } else if protocol == FrontendProtocol::OpenaiResponses {
                ("left_out", Some("no_codex_record"))
            } else {
                ("left_out", Some("not_listed"))
            }
        }
    }
}

fn listing_slugs(listing: &Value) -> impl Iterator<Item = &str> {
    ["data", "models"]
        .into_iter()
        .filter_map(|key| listing.get(key).and_then(Value::as_array))
        .flatten()
        .filter_map(|entry| {
            entry
                .get("id")
                .or_else(|| entry.get("slug"))
                .and_then(Value::as_str)
        })
}

/// Whether the active revision's publication holds the slug right now. The
/// chat and responses lists are rendered from the active revision on every
/// request, so they hold exactly what it desires. The claude picker is a
/// settings file, so it holds what the last applied sync wrote; `None` when
/// no sync has recorded its rows.
fn applied_state<L: LearningView>(
    inputs: &Inputs<'_, L>,
    desired: &str,
    slug: &str,
    adapter: &str,
) -> Option<bool> {
    if adapter != CLAUDE_ADAPTER {
        return Some(desired == "listed");
    }
    inputs
        .publications
        .iter()
        .find(|publication| publication.key.adapter().as_str() == adapter)
        .and_then(|publication| publication.applied_identities.as_ref())
        .map(|identities| identities.contains(slug))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::classifier_body;
    use crate::catalog::offers::{BackendOffer, OfferInventory};
    use crate::policy::{PolicyDocument, compile};
    use crate::routing::{ANTHROPIC_SUB_MESSAGES, RouteRegistry};

    fn offer(model: &str) -> BackendOffer {
        BackendOffer {
            binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
            provider: "anthropic_sub".to_owned(),
            backend_model_id: model.to_owned(),
            binding: ANTHROPIC_SUB_MESSAGES.clone(),
            context_window: None,
            chronology: None,
            fetched_at_ms: 1,
            raw: serde_json::Value::Null,
        }
    }

    /// Examples are the models a classifier reads, and the ones its matcher
    /// accepts but whose version it cannot parse; a model it never matches
    /// is no example of anything.
    #[test]
    fn classifier_examples_split_parsed_from_unparsed_and_skip_the_unmatched() {
        let inventory = OfferInventory::from_offers(
            ["claude-opus-4-8", "claude-opus-5", "claude-opus-x", "gpt-5"].map(offer),
        );
        let document: PolicyDocument = serde_json::from_value(json!({
            "format_version": 5,
            "classifiers": [{
                "id": "family",
                "binding_id": "anthropic_sub.anthropic_messages",
                "model_matcher": {
                    "kind": "regex",
                    "value": r"^claude-(?P<family>[a-z]+)-(?P<version>[0-9a-z-]+)$",
                },
                "family_template": "${family}",
                "version_template": "${version}",
                "version_parser": {"kind": "numeric_tuple", "separator": "-"},
            }],
        }))
        .expect("document");
        let topology =
            RouteRegistry::new(std::iter::empty::<Arc<dyn crate::providers::Provider>>());
        let policy = compile(
            document.clone().validate().expect("valid"),
            &topology,
            &inventory,
        )
        .into_policy()
        .expect("compiles");

        let body = classifier_body(policy.classifiers().next().unwrap(), &document, &inventory);

        assert_eq!(
            body["parsed"],
            json!([
                {"model": "claude-opus-4-8", "family": "opus", "version": "4.8"},
                {"model": "claude-opus-5", "family": "opus", "version": "5"},
            ])
        );
        assert_eq!(body["unparsed"], json!([{"model": "claude-opus-x"}]));
        assert_eq!(body["matcher"]["kind"], "regex");
    }
}
