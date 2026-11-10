//! What a request a record catches gets now, by walking its route against a
//! snapshot's availability (spec: UTUI#reading-a-table).
//!
//! This is the resolver's walk with the request taken out. It uses the same
//! per-candidate and per-edge checks as [`super::resolve`], with the
//! availability a preview uses: no per-request credential evidence, no open
//! circuits (spec: RREV#preview), and a request that needs no capability,
//! names no binding and asks for no safeguards. What it reports is therefore
//! what a plain request gets, not what a particular request would.

use std::collections::BTreeMap;

use super::resolve::{candidate_evidence, edge_denial, exact_evidence, materialize_candidate};
use super::{
    AvailabilitySnapshot, BindingId, CandidateId, CompiledCandidate, CompiledCandidateModel,
    CompiledPolicy, CompiledRoute, CostClass, FrontendProtocol, RequestClass, RequestFacts,
    SkipReason,
};
use crate::catalog::offers::OfferInventory;

/// A candidate the walk passed over, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub candidate_id: CandidateId,
    pub binding_id: BindingId,
    pub reason: SkipReason,
}

/// The candidate that serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Serving {
    pub candidate_id: CandidateId,
    pub binding_id: BindingId,
    pub cost: CostClass,
}

/// The setting on an edge that ended the walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeSetting {
    /// Falling back before anything is sent is disabled.
    Preflight,
    /// Crossing from a subscription to a billed provider is not consented to.
    BilledBoundary,
}

/// The edge a walk was stopped at: the candidate it stood on and the one it
/// was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeStop {
    pub from: CandidateId,
    pub to: CandidateId,
    pub setting: EdgeSetting,
}

/// Why a request that no route was chosen for is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoRoute {
    /// The scope has no default, and nothing before it caught the request.
    NoDefault,
    /// The record names a route the policy does not hold.
    RouteMissing,
}

/// The five outcomes the dashboard shows for a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// Served by the route's first candidate.
    ServedFirst { serving: Serving },
    /// Served by a later candidate, after passing over the ones before it.
    ServedLater {
        serving: Serving,
        skipped: Vec<Skipped>,
    },
    /// Stopped at an edge, which ended the walk with candidates still ahead.
    RefusedEdge {
        skipped: Vec<Skipped>,
        edge: EdgeStop,
    },
    /// Every candidate was passed over.
    RefusedUnavailable { skipped: Vec<Skipped> },
    /// There is no route to walk.
    RefusedNoRoute(NoRoute),
}

impl Reach {
    /// The identifier the control surface and the page use.
    pub fn outcome(&self) -> &'static str {
        match self {
            Self::ServedFirst { .. } => "served_first",
            Self::ServedLater { .. } => "served_later",
            Self::RefusedEdge { .. } => "refused_edge",
            Self::RefusedUnavailable { .. } => "refused_unavailable",
            Self::RefusedNoRoute(_) => "refused_no_route",
        }
    }

    /// Whether a request is served at all.
    pub fn is_refused(&self) -> bool {
        !matches!(self, Self::ServedFirst { .. } | Self::ServedLater { .. })
    }

    /// How far from healthy: the worst of several records' reach is the one
    /// with the highest rank. A fallback that spends money outranks one that
    /// does not, and any refusal outranks both.
    pub fn rank(&self) -> u8 {
        match self {
            Self::ServedFirst { .. } => 0,
            Self::ServedLater { serving, .. } if serving.cost == CostClass::Subscription => 1,
            Self::ServedLater { .. } => 2,
            Self::RefusedEdge { .. } => 3,
            Self::RefusedUnavailable { .. } => 4,
            Self::RefusedNoRoute(_) => 5,
        }
    }
}

/// A request that asks for nothing: every check that reads the request passes.
fn neutral_facts() -> RequestFacts {
    RequestFacts {
        profile: None,
        protocol: FrontendProtocol::AnthropicMessages,
        requested_model: None,
        lane_reference: None,
        prompt_bound: 0,
        message_count: None,
        tool_count: 0,
        required_capabilities: Default::default(),
        asks_server_safeguards: false,
        binding_affinity: None,
        class: RequestClass::Inference,
        compaction_evidence: None,
        accounts: BTreeMap::new(),
        now_ms: 0,
    }
}

/// The model a candidate is checked against. Only a policy-owned model can be
/// disproved by a catalogue, and only a selector can fail to resolve; a
/// candidate sending what the request named has no model to check before one
/// is asked for.
fn checked_model(
    candidate: &CompiledCandidate,
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
) -> Option<String> {
    match candidate.model() {
        CompiledCandidateModel::Exact(model) => Some(model.clone()),
        CompiledCandidateModel::OfferSelector(_) => {
            materialize_candidate(candidate, &BTreeMap::new(), inventory, policy)
                .ok()
                .flatten()
        }
        CompiledCandidateModel::RequestedModel | CompiledCandidateModel::Template(_) => {
            Some(String::new())
        }
    }
}

fn cost_of(availability: &AvailabilitySnapshot, binding: &BindingId) -> CostClass {
    availability
        .get(binding)
        .map_or(CostClass::Unknown, |live| live.cost_class)
}

/// Walk `route` from its first candidate.
pub fn route_reach(
    policy: &CompiledPolicy,
    route: &CompiledRoute,
    availability: &AvailabilitySnapshot,
    inventory: &OfferInventory,
) -> Reach {
    let facts = neutral_facts();
    let candidates = route.candidates();
    let mut skipped = Vec::new();
    let mut prior_live = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let live = availability.get(candidate.binding_id());
        if index > 0
            && let Some(reason) = edge_denial(&route.edges()[index - 1], prior_live, live)
        {
            let setting = match reason {
                SkipReason::PreflightFallbackDisabled => EdgeSetting::Preflight,
                _ => EdgeSetting::BilledBoundary,
            };
            return Reach::RefusedEdge {
                skipped,
                edge: EdgeStop {
                    from: candidates[index - 1].id().clone(),
                    to: candidate.id().clone(),
                    setting,
                },
            };
        }
        prior_live = live;
        let model = checked_model(candidate, policy, inventory);
        let evidence =
            candidate_evidence(candidate, model.as_deref(), live, inventory, &facts, false);
        match evidence.result {
            Ok(()) => {
                let serving = Serving {
                    candidate_id: candidate.id().clone(),
                    binding_id: candidate.binding_id().clone(),
                    cost: cost_of(availability, candidate.binding_id()),
                };
                return if index == 0 {
                    Reach::ServedFirst { serving }
                } else {
                    Reach::ServedLater { serving, skipped }
                };
            }
            Err(reason) => {
                let ends_walk = reason == SkipReason::SelectorUnresolved;
                skipped.push(Skipped {
                    candidate_id: candidate.id().clone(),
                    binding_id: candidate.binding_id().clone(),
                    reason,
                });
                // A selector that resolves to no winner ends the walk: an
                // absent inventory is not evidence for falling back.
                if ends_walk {
                    break;
                }
            }
        }
    }
    Reach::RefusedUnavailable { skipped }
}

/// The reach of a presentation that names one offer: there is no route to
/// walk, only whether the offer can be served.
pub fn offer_reach(
    candidate_id: &CandidateId,
    binding: &BindingId,
    model: &str,
    availability: &AvailabilitySnapshot,
    inventory: &OfferInventory,
) -> Reach {
    let evidence = exact_evidence(
        candidate_id,
        binding,
        model,
        availability.get(binding),
        inventory,
        &neutral_facts(),
    );
    match evidence.result {
        Ok(()) => Reach::ServedFirst {
            serving: Serving {
                candidate_id: candidate_id.clone(),
                binding_id: binding.clone(),
                cost: cost_of(availability, binding),
            },
        },
        Err(reason) => Reach::RefusedUnavailable {
            skipped: vec![Skipped {
                candidate_id: candidate_id.clone(),
                binding_id: binding.clone(),
                reason,
            }],
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::catalog::offers::{BackendOffer, OfferInventory};
    use crate::policy::{
        BindingAvailability, CatalogueAvailability, Evidence, PolicyDocument, RouteId, compile,
    };
    use crate::routing::{
        ANTHROPIC_API_MESSAGES, ANTHROPIC_SUB_MESSAGES, Capabilities, RouteRegistry,
    };

    const SUB: &str = "anthropic_sub.anthropic_messages";
    const API: &str = "anthropic_api.anthropic_messages";

    fn candidate(id: &str, binding: &str, enabled: bool) -> serde_json::Value {
        json!({
            "id": id,
            "binding_id": binding,
            "model": {"kind": "requested_model"},
            "enabled": enabled,
        })
    }

    fn edge(preflight: &str, billed: &str) -> serde_json::Value {
        json!({
            "preflight": preflight,
            "post_send": [],
            "billed_boundary": billed,
            "ambiguous_transport": "deny",
            "cache_transition": "allow_cache_loss",
        })
    }

    /// A route over the subscription then the API binding, and its policy.
    fn policy_with(first_enabled: bool, preflight: &str, billed: &str) -> CompiledPolicy {
        policy_over((SUB, API), first_enabled, preflight, billed)
    }

    fn policy_over(
        bindings: (&str, &str),
        first_enabled: bool,
        preflight: &str,
        billed: &str,
    ) -> CompiledPolicy {
        let document: PolicyDocument = serde_json::from_value(json!({
            "format_version": 5,
            "defaults": [{
                "scope": {"kind": "protocol", "protocol": "anthropic_messages"},
                "target": {"route_id": "r", "parameters": {"requested_model": {"kind": "requested_model"}}},
            }],
            "routes": [{
                "id": "r",
                "display_name": "r",
                "parameters": ["requested_model"],
                "chain": {
                    "first": candidate("first", bindings.0, first_enabled),
                    "steps": [{
                        "edge": edge(preflight, billed),
                        "candidate": candidate("second", bindings.1, true),
                    }],
                },
                "lane_transition": "move_when_cold",
            }],
        }))
        .expect("document");
        let topology =
            RouteRegistry::new(std::iter::empty::<Arc<dyn crate::providers::Provider>>());
        compile(
            document.validate().expect("valid"),
            &topology,
            &OfferInventory::default(),
        )
        .into_policy()
        .expect("compiles")
    }

    fn live(
        provider: &str,
        enabled: bool,
        credential: bool,
        cost: CostClass,
    ) -> BindingAvailability {
        BindingAvailability {
            provider: provider.to_owned(),
            configured: Evidence::Positive("configured"),
            enabled: if enabled {
                Evidence::Positive("enabled")
            } else {
                Evidence::Negative("disabled")
            },
            credential: if credential {
                Evidence::Positive("present")
            } else {
                Evidence::Negative("absent")
            },
            catalogue: CatalogueAvailability::Absent,
            capabilities: Capabilities::ANTHROPIC_MESSAGES,
            safeguards_marked: false,
            cost_class: cost,
        }
    }

    fn availability(sub: BindingAvailability, api: BindingAvailability) -> AvailabilitySnapshot {
        AvailabilitySnapshot::new(BTreeMap::from([
            (ANTHROPIC_SUB_MESSAGES.id().clone(), sub),
            (ANTHROPIC_API_MESSAGES.id().clone(), api),
        ]))
    }

    fn reach_of(policy: &CompiledPolicy, availability: &AvailabilitySnapshot) -> Reach {
        let route = policy.route(&"r".parse::<RouteId>().unwrap()).unwrap();
        route_reach(policy, route, availability, &OfferInventory::default())
    }

    fn healthy() -> AvailabilitySnapshot {
        availability(
            live("anthropic_sub", true, true, CostClass::Subscription),
            live("anthropic_api", true, true, CostClass::Billed),
        )
    }

    #[test]
    fn a_healthy_first_candidate_serves() {
        let reach = reach_of(&policy_with(true, "enabled", "allow"), &healthy());
        assert_eq!(reach.outcome(), "served_first");
        assert!(!reach.is_refused());
    }

    #[test]
    fn a_missing_credential_falls_to_a_later_candidate_and_says_it_is_billed() {
        let availability = availability(
            live("anthropic_sub", true, false, CostClass::Subscription),
            live("anthropic_api", true, true, CostClass::Billed),
        );
        let Reach::ServedLater { serving, skipped } =
            reach_of(&policy_with(true, "enabled", "allow"), &availability)
        else {
            panic!("served by the fallback");
        };
        assert_eq!(serving.binding_id.as_str(), API);
        assert_eq!(serving.cost, CostClass::Billed);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, SkipReason::CredentialUnavailable);
    }

    /// The compiler already requires consent to cross from a subscription to
    /// a billed binding as the topology declares them, so a denial can only
    /// bite where the live cost class says something else. The route here
    /// crosses nothing by its topology, and the live classes make it cross.
    #[test]
    fn a_denied_billed_boundary_stops_the_walk_at_the_edge() {
        let availability = availability(
            live("anthropic_sub", true, true, CostClass::Billed),
            live("anthropic_api", false, true, CostClass::Subscription),
        );
        let policy = policy_over((API, SUB), true, "enabled", "deny");
        let Reach::RefusedEdge { skipped, edge } = reach_of(&policy, &availability) else {
            panic!("refused at the edge");
        };
        assert_eq!(edge.setting, EdgeSetting::BilledBoundary);
        assert_eq!(edge.from.as_str(), "first");
        assert_eq!(edge.to.as_str(), "second");
        assert_eq!(skipped[0].reason, SkipReason::BindingDisabled);
    }

    #[test]
    fn disabled_fallback_before_send_stops_the_walk_at_the_edge() {
        let availability = availability(
            live("anthropic_sub", true, false, CostClass::Subscription),
            live("anthropic_api", true, true, CostClass::Billed),
        );
        let Reach::RefusedEdge { edge, .. } =
            reach_of(&policy_with(true, "disabled", "allow"), &availability)
        else {
            panic!("refused at the edge");
        };
        assert_eq!(edge.setting, EdgeSetting::Preflight);
    }

    #[test]
    fn a_billed_boundary_only_applies_from_a_subscription() {
        // The first candidate is billed too, so there is no boundary to deny.
        let availability = availability(
            live("anthropic_sub", true, true, CostClass::Billed),
            live("anthropic_api", true, false, CostClass::Billed),
        );
        let policy = policy_over((API, SUB), true, "enabled", "deny");
        assert_eq!(reach_of(&policy, &availability).outcome(), "served_later");
    }

    #[test]
    fn every_candidate_unavailable_is_refused_with_each_reason() {
        let availability = availability(
            live("anthropic_sub", false, true, CostClass::Subscription),
            live("anthropic_api", true, false, CostClass::Subscription),
        );
        let Reach::RefusedUnavailable { skipped } =
            reach_of(&policy_with(true, "enabled", "allow"), &availability)
        else {
            panic!("refused: nothing available");
        };
        let reasons: Vec<_> = skipped.iter().map(|skip| skip.reason.code()).collect();
        assert_eq!(reasons, ["provider_off", "no_credential"]);
    }

    #[test]
    fn a_disabled_candidate_is_passed_over() {
        let reach = reach_of(&policy_with(false, "enabled", "allow"), &healthy());
        let Reach::ServedLater { skipped, .. } = reach else {
            panic!("the second serves");
        };
        assert_eq!(skipped[0].reason, SkipReason::Disabled);
    }

    #[test]
    fn an_offer_the_fresh_catalogue_lacks_is_refused() {
        let mut live = live("anthropic_sub", true, true, CostClass::Subscription);
        live.catalogue = CatalogueAvailability::Fresh { fetched_at_ms: 1 };
        let availability = availability(
            live,
            self::live("anthropic_api", true, true, CostClass::Billed),
        );
        let inventory = OfferInventory::from_offers([BackendOffer {
            binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
            provider: "anthropic_sub".to_owned(),
            backend_model_id: "present".to_owned(),
            binding: ANTHROPIC_SUB_MESSAGES.clone(),
            context_window: None,
            chronology: None,
            fetched_at_ms: 1,
            raw: serde_json::Value::Null,
        }]);
        let candidate: CandidateId = "c".parse().unwrap();
        let present = offer_reach(
            &candidate,
            ANTHROPIC_SUB_MESSAGES.id(),
            "present",
            &availability,
            &inventory,
        );
        assert_eq!(present.outcome(), "served_first");
        let absent = offer_reach(
            &candidate,
            ANTHROPIC_SUB_MESSAGES.id(),
            "absent",
            &availability,
            &inventory,
        );
        let Reach::RefusedUnavailable { skipped } = absent else {
            panic!("refused");
        };
        assert_eq!(
            skipped[0].reason,
            SkipReason::ModelMissingFromFreshCatalogue
        );
    }

    #[test]
    fn refusals_outrank_a_billed_fallback_which_outranks_a_healthy_route() {
        let healthy = reach_of(&policy_with(true, "enabled", "allow"), &healthy());
        let billed = reach_of(
            &policy_with(true, "enabled", "allow"),
            &availability(
                live("anthropic_sub", true, false, CostClass::Subscription),
                live("anthropic_api", true, true, CostClass::Billed),
            ),
        );
        let refused = Reach::RefusedNoRoute(NoRoute::NoDefault);
        assert!(healthy.rank() < billed.rank());
        assert!(billed.rank() < refused.rank());
        assert!(refused.is_refused());
    }
}
