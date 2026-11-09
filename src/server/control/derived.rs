//! What the routing page shows beyond the policy document itself: live
//! availability, cost-boundary edges, pinned lanes, classifier examples, the
//! upgrade standing and per-presentation publication.
//!
//! The daemon computes it and the terminal renders it, because the page holds
//! no store, registry or learning view. Everything here is counts, ids and
//! model names the offers already carry; no content.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::catalog::offers::{OfferInventory, PolicyPublication, render_policy};
use crate::policy::{
    AvailabilitySnapshot, BindingAvailability, BindingId, CatalogueAvailability, CircuitSnapshot,
    CompiledCandidateModel, CompiledClassifier, CompiledPolicy, CompiledPresentation, CostClass,
    Evidence, FrontendProtocol, FrontendScope, LearningView, RouteId, upgrade_standing,
};
use crate::routing::{Capabilities, CapabilityEvidence};
use crate::server::RoutingLaneImpacts;
use crate::store::RoutingPublication;

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
    pub active: Option<&'a CompiledPolicy>,
    pub draft: Option<&'a CompiledPolicy>,
}

/// The derived content for the active revision and, when it compiles, the
/// draft. A draft that does not compile has no derived content: its
/// diagnostics say why.
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
        "active": inputs.active.map(|policy| policy_body(policy, inputs)),
        "draft": inputs.draft.map(|policy| policy_body(policy, inputs)),
    })
}

fn policy_body<L: LearningView>(policy: &CompiledPolicy, inputs: &Inputs<'_, L>) -> Value {
    json!({
        "routes": policy
            .routes()
            .map(|route| route_body(policy, route, inputs))
            .collect::<Vec<_>>(),
        "classifiers": policy
            .classifiers()
            .map(|classifier| classifier_body(classifier, inputs.inventory))
            .collect::<Vec<_>>(),
        "presentations": presentations_body(policy, inputs),
    })
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
    json!({"warm": impacts.warm, "cold": impacts.cold, "unknown": impacts.unknown})
}

fn route_body<L: LearningView>(
    policy: &CompiledPolicy,
    route: &crate::policy::CompiledRoute,
    inputs: &Inputs<'_, L>,
) -> Value {
    let cost_of = |binding: &BindingId| {
        inputs
            .availability
            .get(binding)
            .map_or(CostClass::Unknown, |availability| availability.cost_class)
    };
    let candidates: Vec<Value> = route
        .candidates()
        .iter()
        .map(|candidate| {
            // The baseline is only known up front for a fixed model; a
            // template, selector or requested-model candidate gets its
            // baseline from the request.
            let standing = match candidate.model() {
                CompiledCandidateModel::Exact(baseline) => upgrade_standing(
                    policy,
                    inputs.inventory,
                    inputs.learning,
                    candidate,
                    baseline,
                )
                .ok()
                .flatten()
                .map(|election| {
                    json!({
                        "baseline": baseline,
                        "winner": election.winner,
                        "rejected": election
                            .rejected
                            .iter()
                            .map(|(model, reason)| json!({
                                "model": model,
                                "reason": rejection_name(*reason),
                            }))
                            .collect::<Vec<_>>(),
                    })
                }),
                _ => None,
            };
            json!({
                "id": candidate.id(),
                "binding_id": candidate.binding_id(),
                "cost_class": cost_name(cost_of(candidate.binding_id())),
                "upgrade_policy_id": candidate.upgrade_policy_id(),
                "upgrade": standing,
            })
        })
        .collect();
    let edges: Vec<Value> = route
        .edges()
        .iter()
        .enumerate()
        .map(|(index, edge)| {
            let from = cost_of(route.candidates()[index].binding_id());
            let to = cost_of(route.candidates()[index + 1].binding_id());
            json!({
                "from": route.candidates()[index].id(),
                "to": route.candidates()[index + 1].id(),
                "from_cost": cost_name(from),
                "to_cost": cost_name(to),
                "crosses_cost_boundary": from == CostClass::Subscription
                    && matches!(to, CostClass::Billed | CostClass::Unknown),
                "billed_boundary": edge.billed_boundary,
                "cache_transition": edge.cache_transition,
                "preflight": edge.preflight,
            })
        })
        .collect();
    json!({
        "id": route.id(),
        "pinned_lanes": lanes_body(inputs.pinned.get(route.id())),
        "candidates": candidates,
        "edges": edges,
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

fn classifier_body(classifier: &CompiledClassifier, inventory: &OfferInventory) -> Value {
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
    json!({
        "id": classifier.id(),
        "binding_id": classifier.binding_id(),
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
fn presentations_body<L: LearningView>(policy: &CompiledPolicy, inputs: &Inputs<'_, L>) -> Value {
    let active = inputs.active;
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
            "scope": scope_label(&scope),
            "slug": slug,
            "adapter": adapter,
            "desired": desired,
            "reason": reason,
            "applied": active.and_then(|active| applied_state(
                active,
                inputs,
                profile,
                protocol,
                slug,
                adapter,
            )),
        }));
    }
    Value::Array(rows)
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
/// request, so they hold exactly what it lists. The claude picker is a
/// settings file, so it holds what the last applied sync wrote; `None` when
/// no sync has recorded its rows.
fn applied_state<L: LearningView>(
    active: &CompiledPolicy,
    inputs: &Inputs<'_, L>,
    profile: Option<&crate::policy::ProfileId>,
    protocol: FrontendProtocol,
    slug: &str,
    adapter: &str,
) -> Option<bool> {
    if adapter != CLAUDE_ADAPTER {
        let presentation = match profile {
            Some(profile) => active.presentation(
                &FrontendScope::Profile {
                    profile: profile.clone(),
                    protocol,
                },
                slug,
            ),
            None => active.presentation(&FrontendScope::Protocol { protocol }, slug),
        }?;
        return Some(
            desired_state(
                active,
                inputs.inventory,
                profile,
                protocol,
                slug,
                presentation,
            )
            .0 == "listed",
        );
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
            "format_version": 3,
            "classifiers": [{
                "id": "family",
                "binding_id": "anthropic_sub.anthropic_messages",
                "model_matcher": {
                    "kind": "regex",
                    "value": r"^claude-(?P<family>[a-z]+)-(?P<version>[0-9a-z-]+)$",
                },
                "family_template": "${family}",
                "version_template": "${version}",
                "version_parser": {"kind": "legacy_numeric_tuple", "separator": "-"},
            }],
        }))
        .expect("document");
        let topology =
            RouteRegistry::new(std::iter::empty::<Arc<dyn crate::providers::Provider>>());
        let policy = compile(document.validate().expect("valid"), &topology, &inventory)
            .into_policy()
            .expect("compiles");

        let body = classifier_body(policy.classifiers().next().unwrap(), &inventory);

        assert_eq!(
            body["parsed"],
            json!([
                {"model": "claude-opus-4-8", "family": "opus", "version": "4.8"},
                {"model": "claude-opus-5", "family": "opus", "version": "5"},
            ])
        );
        assert_eq!(body["unparsed"], json!([{"model": "claude-opus-x"}]));
    }
}
