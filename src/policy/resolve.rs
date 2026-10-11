//! Pure, content-free policy resolution.
//!
//! Resolution consumes immutable snapshots and returns a description of one
//! preflight-selected attempt. It has no access to request bodies, providers,
//! credentials, clocks, lanes, or stores and cannot render or send traffic.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;

use thiserror::Error;

use crate::catalog::offers::OfferInventory;
use crate::middleware::force_newest::{NEW_CONVERSATION_MESSAGES, idle_for_ttl};
use crate::routing::Capabilities;

use super::{
    ArgumentBinding, BindingId, BoundaryConsent, CacheTransitionPolicy, CandidateEdge, CandidateId,
    CapabilityFlags, CompiledCandidate, CompiledCandidateModel, CompiledClassifier, CompiledPolicy,
    CompiledRoute, CompiledRouteReference, CostClass, FallbackPermission, FrontendProtocol,
    FrontendScope, InventorySource, LaneTransitionPolicy, LearningView, MatcherCaptures,
    MinimumLearning, ParsedVersion, PolicyFingerprint, PresentationId, PresentationRoute,
    ProfileId, REQUESTED_MODEL_PARAMETER, RewriteId, RouteFingerprint, RouteId, StableId,
};

/// The kind of request being resolved. This is derived from fixed markers;
/// neither the marker nor any prompt text enters policy resolution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RequestClass {
    #[default]
    Inference,
    Compaction,
}

/// Content-free facts derived by a frontend adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestFacts {
    pub profile: Option<ProfileId>,
    pub protocol: FrontendProtocol,
    pub requested_model: Option<String>,
    /// Raw lane identity or reference. This is deliberately not lineage.
    pub lane_reference: Option<String>,
    pub prompt_bound: u64,
    pub message_count: Option<u64>,
    pub tool_count: u64,
    pub required_capabilities: CapabilityFlags,
    pub binding_affinity: Option<BindingId>,
    pub class: RequestClass,
    /// The caller owns the clock. The resolver never reads one.
    pub now_ms: i64,
}

impl RequestFacts {
    pub fn protocol_scope(&self) -> FrontendScope {
        FrontendScope::Protocol {
            protocol: self.protocol,
        }
    }

    pub fn profile_scope(&self) -> Option<FrontendScope> {
        self.profile.clone().map(|profile| FrontendScope::Profile {
            profile,
            protocol: self.protocol,
        })
    }
}

/// Whether a live fact is known, with the source of that knowledge retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence<T> {
    Positive(T),
    Negative(T),
    Unknown,
}

/// Catalogue evidence for one binding at snapshot capture time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogueAvailability {
    Fresh { fetched_at_ms: i64 },
    Stale { fetched_at_ms: i64 },
    Absent,
}

/// Immutable live facts for one statically declared backend binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingAvailability {
    pub provider: String,
    pub configured: Evidence<&'static str>,
    pub enabled: Evidence<&'static str>,
    pub credential: Evidence<&'static str>,
    pub catalogue: CatalogueAvailability,
    pub capabilities: Capabilities,
    pub cost_class: CostClass,
    pub cache_domain: Option<String>,
}

/// One consistently captured availability snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AvailabilitySnapshot {
    bindings: BTreeMap<BindingId, BindingAvailability>,
}

impl AvailabilitySnapshot {
    pub fn new(bindings: BTreeMap<BindingId, BindingAvailability>) -> Self {
        Self { bindings }
    }

    pub fn get(&self, binding: &BindingId) -> Option<&BindingAvailability> {
        self.bindings.get(binding)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&BindingId, &BindingAvailability)> {
        self.bindings.iter()
    }

    /// Re-evaluate catalogue age from one caller-owned clock reading.
    pub fn at(&self, now_ms: i64, fresh_for_ms: i64) -> Self {
        let bindings = self
            .bindings
            .iter()
            .map(|(id, availability)| {
                let mut availability = availability.clone();
                if let CatalogueAvailability::Fresh { fetched_at_ms }
                | CatalogueAvailability::Stale { fetched_at_ms } = availability.catalogue
                {
                    availability.catalogue = if now_ms.saturating_sub(fetched_at_ms) <= fresh_for_ms
                    {
                        CatalogueAvailability::Fresh { fetched_at_ms }
                    } else {
                        CatalogueAvailability::Stale { fetched_at_ms }
                    };
                }
                (id.clone(), availability)
            })
            .collect();
        Self { bindings }
    }
}

/// Read-only lane facts needed for cache-safe resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneFacts {
    pub cold: bool,
    pub pinned: Option<ProvisionalPin>,
    pub forced_from: Option<String>,
    pub forced_to: Option<String>,
}

/// An immutable prospective pin. Slice five never persists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionalPin {
    pub policy_fingerprint: PolicyFingerprint,
    pub route_fingerprint: RouteFingerprint,
    pub candidate_id: CandidateId,
    pub binding_id: BindingId,
    pub baseline_model: String,
    pub effective_model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteProvenance {
    ProfilePresentation(PresentationId),
    ProtocolPresentation(PresentationId),
    ProfileRewrite(RewriteId),
    ProtocolRewrite(RewriteId),
    ProfileDefault,
    ProtocolDefault,
    Compaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionDecision {
    Preserve,
    MoveWhenCold,
    Move,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    Disabled,
    BindingUnknown,
    ProviderUnconfigured,
    BindingDisabled,
    CredentialUnavailable,
    MissingRequiredCapability,
    BindingAffinity,
    ModelMissingFromFreshCatalogue,
    ModelUnknown,
    SelectorUnresolved,
    PreflightFallbackDisabled,
    BilledBoundaryDenied,
    WarmCacheTransitionDenied,
    WarmLaneTransitionDenied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateEvidence {
    pub candidate_id: CandidateId,
    pub binding_id: BindingId,
    pub model: Option<String>,
    pub result: Result<(), SkipReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEvent {
    Lookup(RouteProvenance),
    Candidate(CandidateEvidence),
    Upgrade { from: String, to: String },
    Alias { from: String, to: String },
}

/// The complete, content-free result of pure preflight resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptPlan {
    pub policy_fingerprint: PolicyFingerprint,
    /// Legacy in-memory policy has no persisted revision yet.
    pub policy_revision: Option<u64>,
    pub route_id: Option<RouteId>,
    pub route_fingerprint: Option<RouteFingerprint>,
    pub provenance: RouteProvenance,
    pub candidate_id: Option<CandidateId>,
    pub binding_id: BindingId,
    pub provider: String,
    pub requested_model: Option<String>,
    pub baseline_model: String,
    pub upgraded_model: Option<String>,
    pub aliased_model: Option<String>,
    pub effective_model: String,
    pub compaction: bool,
    pub transition: TransitionDecision,
    pub trace: Vec<TraceEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResolveFailure {
    #[error("no presentation, rewrite, or default route matched")]
    NoRoute,
    #[error("the selected route is missing")]
    MissingRoute,
    #[error("a route argument could not be materialised")]
    InvalidArgument,
    #[error("a matcher or template could not be evaluated")]
    InvalidTemplate,
    #[error("learning evidence could not be read")]
    LearningUnavailable,
    #[error("no candidate is available before send")]
    RouteUnavailable { trace: Vec<TraceEvent> },
}

struct SelectedRoute<'a> {
    route: Option<&'a CompiledRoute>,
    exact: Option<(
        &'a BindingId,
        &'a str,
        &'a RouteId,
        &'a RouteFingerprint,
        &'a CandidateId,
    )>,
    parameters: BTreeMap<StableId, String>,
    provenance: RouteProvenance,
    lane_transition: Option<LaneTransitionPolicy>,
    cache_transition: Option<CacheTransitionPolicy>,
}

/// Resolve one request without mutation, I/O, clock reads, or persistence.
pub fn resolve<L: LearningView>(
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
    learning: &L,
    lane: Option<&LaneFacts>,
    availability: &AvailabilitySnapshot,
    facts: &RequestFacts,
) -> Result<AttemptPlan, ResolveFailure> {
    let mut selected = lookup(policy, facts)?;
    if facts.class == RequestClass::Compaction
        && let Some(compaction) = policy.compaction()
    {
        selected = from_reference(
            policy,
            compaction.target(),
            facts,
            None,
            RouteProvenance::Compaction,
        )?;
        selected.lane_transition = Some(compaction.lane_transition());
        selected.cache_transition = Some(compaction.cache_transition());
    }

    let mut trace = vec![TraceEvent::Lookup(selected.provenance.clone())];
    if let Some((binding_id, model, route_id, fingerprint, candidate_id)) = selected.exact {
        let live = availability.get(binding_id);
        let evidence = exact_evidence(binding_id, model, live, inventory, facts);
        trace.push(TraceEvent::Candidate(evidence.clone()));
        if evidence.result.is_err() {
            return Err(ResolveFailure::RouteUnavailable { trace });
        }
        let provider = live.map_or_else(String::new, |live| live.provider.clone());
        return Ok(AttemptPlan {
            policy_fingerprint: policy.fingerprint().clone(),
            policy_revision: None,
            route_id: Some(route_id.clone()),
            route_fingerprint: Some(fingerprint.clone()),
            provenance: selected.provenance,
            candidate_id: Some(candidate_id.clone()),
            binding_id: binding_id.clone(),
            provider,
            requested_model: facts.requested_model.clone(),
            baseline_model: model.to_owned(),
            upgraded_model: None,
            aliased_model: None,
            effective_model: model.to_owned(),
            compaction: facts.class == RequestClass::Compaction,
            transition: TransitionDecision::Preserve,
            trace,
        });
    }

    let route = selected.route.ok_or(ResolveFailure::MissingRoute)?;
    let pinned = lane
        .and_then(|lane| lane.pinned.as_ref())
        .filter(|pin| pin.policy_fingerprint == *policy.fingerprint())
        .filter(|pin| pin.route_fingerprint == *route.fingerprint());
    let start = pinned
        .and_then(|pin| {
            route
                .candidates()
                .iter()
                .position(|candidate| candidate.id() == &pin.candidate_id)
        })
        .unwrap_or(0);
    let mut prior_live: Option<&BindingAvailability> = None;
    for (index, candidate) in route.candidates().iter().enumerate().skip(start) {
        if index > start {
            let edge = &route.edges()[index - 1];
            if let Some(reason) = edge_denial(
                edge,
                prior_live,
                availability.get(candidate.binding_id()),
                lane,
            ) {
                trace.push(TraceEvent::Candidate(CandidateEvidence {
                    candidate_id: candidate.id().clone(),
                    binding_id: candidate.binding_id().clone(),
                    model: None,
                    result: Err(reason.clone()),
                }));
                if matches!(
                    reason,
                    SkipReason::PreflightFallbackDisabled
                        | SkipReason::BilledBoundaryDenied
                        | SkipReason::WarmCacheTransitionDenied
                ) {
                    break;
                }
                continue;
            }
        }
        let baseline = materialize_candidate(candidate, &selected.parameters, inventory, policy)?;
        let live = availability.get(candidate.binding_id());
        prior_live = live;
        let evidence = candidate_evidence(candidate, baseline.as_deref(), live, inventory, facts);
        trace.push(TraceEvent::Candidate(evidence.clone()));
        if evidence.result.is_err() {
            continue;
        }
        let baseline = baseline.ok_or(ResolveFailure::InvalidTemplate)?;
        if facts.class == RequestClass::Compaction
            && lane.is_some_and(|lane| !lane.cold)
            && (selected.cache_transition != Some(CacheTransitionPolicy::AllowCacheLoss)
                || selected.lane_transition != Some(LaneTransitionPolicy::AllowImmediate))
        {
            if let Some(last) = trace.last_mut()
                && let TraceEvent::Candidate(evidence) = last
            {
                evidence.result = Err(
                    if selected.cache_transition != Some(CacheTransitionPolicy::AllowCacheLoss) {
                        SkipReason::WarmCacheTransitionDenied
                    } else {
                        SkipReason::WarmLaneTransitionDenied
                    },
                );
            }
            return Err(ResolveFailure::RouteUnavailable { trace });
        }
        let upgraded = choose_upgrade(
            policy, inventory, learning, lane, facts, candidate, &baseline,
        )?;
        if let Some(to) = upgraded.as_deref() {
            trace.push(TraceEvent::Upgrade {
                from: baseline.clone(),
                to: to.to_owned(),
            });
        }
        let before_alias = upgraded.as_deref().unwrap_or(&baseline);
        let aliased = apply_alias(candidate, before_alias, &selected.parameters)?;
        if let Some(to) = aliased.as_deref() {
            trace.push(TraceEvent::Alias {
                from: before_alias.to_owned(),
                to: to.to_owned(),
            });
        }
        let effective = aliased
            .clone()
            .or_else(|| upgraded.clone())
            .unwrap_or_else(|| baseline.clone());
        let provider = live.map_or_else(String::new, |live| live.provider.clone());
        let mut lane_policy = selected
            .lane_transition
            .map_or(route.lane_transition(), |policy| {
                stricter_transition(route.lane_transition(), policy)
            });
        if upgraded.is_some()
            && let Some(upgrade) = candidate
                .upgrade_policy_id()
                .and_then(|id| policy.upgrade(id))
        {
            lane_policy = stricter_transition(lane_policy, upgrade.policy().lane_transition);
        }
        return Ok(AttemptPlan {
            policy_fingerprint: policy.fingerprint().clone(),
            policy_revision: None,
            route_id: Some(route.id().clone()),
            route_fingerprint: Some(route.fingerprint().clone()),
            provenance: selected.provenance,
            candidate_id: Some(candidate.id().clone()),
            binding_id: candidate.binding_id().clone(),
            provider,
            requested_model: facts.requested_model.clone(),
            baseline_model: baseline,
            upgraded_model: upgraded,
            aliased_model: aliased,
            effective_model: effective,
            compaction: facts.class == RequestClass::Compaction,
            transition: transition(lane_policy, lane),
            trace,
        });
    }
    Err(ResolveFailure::RouteUnavailable { trace })
}

fn lookup<'a>(
    policy: &'a CompiledPolicy,
    facts: &RequestFacts,
) -> Result<SelectedRoute<'a>, ResolveFailure> {
    let requested = facts.requested_model.as_deref();
    if let (Some(scope), Some(model)) = (facts.profile_scope(), requested)
        && let Some(presentation) = policy.presentation(&scope, model)
    {
        return from_presentation(
            policy,
            presentation.route(),
            facts,
            RouteProvenance::ProfilePresentation(presentation.id().clone()),
        );
    }
    let protocol = facts.protocol_scope();
    if let Some(model) = requested
        && let Some(presentation) = policy.presentation(&protocol, model)
    {
        return from_presentation(
            policy,
            presentation.route(),
            facts,
            RouteProvenance::ProtocolPresentation(presentation.id().clone()),
        );
    }
    if let (Some(scope), Some(model)) = (facts.profile_scope(), requested)
        && let Some(selected) = first_rewrite(policy, &scope, model, facts, true)?
    {
        return Ok(selected);
    }
    if let Some(model) = requested
        && let Some(selected) = first_rewrite(policy, &protocol, model, facts, false)?
    {
        return Ok(selected);
    }
    if let Some(scope) = facts.profile_scope()
        && let Some(reference) = policy.default_route(&scope)
    {
        return from_reference(
            policy,
            reference,
            facts,
            None,
            RouteProvenance::ProfileDefault,
        );
    }
    if let Some(reference) = policy.default_route(&protocol) {
        return from_reference(
            policy,
            reference,
            facts,
            None,
            RouteProvenance::ProtocolDefault,
        );
    }
    Err(ResolveFailure::NoRoute)
}

fn first_rewrite<'a>(
    policy: &'a CompiledPolicy,
    scope: &FrontendScope,
    model: &str,
    facts: &RequestFacts,
    profile: bool,
) -> Result<Option<SelectedRoute<'a>>, ResolveFailure> {
    for rewrite in policy.rewrites(scope) {
        let captures = rewrite
            .matcher()
            .captures(model)
            .map_err(|_| ResolveFailure::InvalidTemplate)?;
        let Some(captures) = captures else { continue };
        let provenance = if profile {
            RouteProvenance::ProfileRewrite(rewrite.id().clone())
        } else {
            RouteProvenance::ProtocolRewrite(rewrite.id().clone())
        };
        return from_reference(policy, rewrite.target(), facts, Some(&captures), provenance)
            .map(Some);
    }
    Ok(None)
}

fn from_presentation<'a>(
    policy: &'a CompiledPolicy,
    route: &'a PresentationRoute,
    facts: &RequestFacts,
    provenance: RouteProvenance,
) -> Result<SelectedRoute<'a>, ResolveFailure> {
    match route {
        PresentationRoute::Reference(reference) => {
            from_reference(policy, reference, facts, None, provenance)
        }
        PresentationRoute::Exact {
            route_id,
            fingerprint,
            candidate_id,
            binding_id,
            model_id,
        } => Ok(SelectedRoute {
            route: None,
            exact: Some((binding_id, model_id, route_id, fingerprint, candidate_id)),
            parameters: BTreeMap::new(),
            provenance,
            lane_transition: None,
            cache_transition: None,
        }),
    }
}

fn from_reference<'a>(
    policy: &'a CompiledPolicy,
    reference: &'a CompiledRouteReference,
    facts: &RequestFacts,
    captures: Option<&MatcherCaptures<'_>>,
    provenance: RouteProvenance,
) -> Result<SelectedRoute<'a>, ResolveFailure> {
    let route = policy
        .route(reference.route_id())
        .ok_or(ResolveFailure::MissingRoute)?;
    let mut parameters = BTreeMap::new();
    for (name, source) in reference.parameters() {
        let value = match source {
            ArgumentBinding::Literal { value } => value.clone(),
            ArgumentBinding::RequestedModel => facts
                .requested_model
                .clone()
                .ok_or(ResolveFailure::InvalidArgument)?,
            ArgumentBinding::RegexCaptureName { name } => captures
                .and_then(|captures| captures.get(name))
                .map(str::to_owned)
                .ok_or(ResolveFailure::InvalidArgument)?,
            ArgumentBinding::RegexCaptureIndex { index } => captures
                .and_then(|captures| captures.get(&index.to_string()))
                .map(str::to_owned)
                .ok_or(ResolveFailure::InvalidArgument)?,
        };
        parameters.insert(name.clone(), value);
    }
    Ok(SelectedRoute {
        route: Some(route),
        exact: None,
        parameters,
        provenance,
        lane_transition: None,
        cache_transition: None,
    })
}

fn materialize_candidate(
    candidate: &CompiledCandidate,
    parameters: &BTreeMap<StableId, String>,
    inventory: &OfferInventory,
    policy: &CompiledPolicy,
) -> Result<Option<String>, ResolveFailure> {
    match candidate.model() {
        CompiledCandidateModel::Exact(model) => Ok(Some(model.clone())),
        CompiledCandidateModel::RequestedModel => Ok(parameters
            .iter()
            .find(|(id, _)| id.as_str() == REQUESTED_MODEL_PARAMETER)
            .map(|(_, value)| value.clone())),
        CompiledCandidateModel::Template(template) => template
            .render(parameters, &empty_captures()?)
            .map(Some)
            .map_err(|_| ResolveFailure::InvalidTemplate),
        CompiledCandidateModel::OfferSelector(selector) => Ok(super::selector::winner(
            inventory,
            candidate.binding_id(),
            selector,
            |classifier_id, offer| {
                policy
                    .classifier(classifier_id)
                    .and_then(|classifier| classify(classifier, &offer.backend_model_id).ok())
                    .flatten()
                    .map(|(_, version)| version)
            },
        )
        .map(|offer| offer.backend_model_id.clone())),
    }
}

fn empty_captures() -> Result<MatcherCaptures<'static>, ResolveFailure> {
    super::MatcherSpec::Exact(String::new())
        .compile()
        .map_err(|_| ResolveFailure::InvalidTemplate)?
        .captures("")
        .map_err(|_| ResolveFailure::InvalidTemplate)?
        .ok_or(ResolveFailure::InvalidTemplate)
}

fn candidate_evidence(
    candidate: &CompiledCandidate,
    model: Option<&str>,
    live: Option<&BindingAvailability>,
    inventory: &OfferInventory,
    facts: &RequestFacts,
) -> CandidateEvidence {
    let reason = if !candidate.enabled() {
        Some(SkipReason::Disabled)
    } else if facts
        .binding_affinity
        .as_ref()
        .is_some_and(|affinity| affinity != candidate.binding_id())
    {
        Some(SkipReason::BindingAffinity)
    } else if let Some(live) = live {
        common_denial(
            live,
            merge_capabilities(
                candidate.required_capabilities(),
                facts.required_capabilities,
            ),
        )
        .or_else(|| match candidate.model() {
            // A fresh listing can disprove a policy-owned exact offer. Models
            // supplied by a compatibility default or template remain
            // provider-owned strings, as they are on the legacy route: an
            // incomplete catalogue must not turn the listing into an
            // upstream allowlist.
            CompiledCandidateModel::Exact(_) => {
                model_availability(candidate.binding_id(), model, live.catalogue, inventory)
            }
            CompiledCandidateModel::OfferSelector(_) if model.is_none() => {
                Some(SkipReason::SelectorUnresolved)
            }
            CompiledCandidateModel::RequestedModel
            | CompiledCandidateModel::Template(_)
            | CompiledCandidateModel::OfferSelector(_) => None,
        })
    } else {
        Some(SkipReason::BindingUnknown)
    };
    CandidateEvidence {
        candidate_id: candidate.id().clone(),
        binding_id: candidate.binding_id().clone(),
        model: model.map(str::to_owned),
        result: reason.map_or(Ok(()), Err),
    }
}

fn exact_evidence(
    binding: &BindingId,
    model: &str,
    live: Option<&BindingAvailability>,
    inventory: &OfferInventory,
    facts: &RequestFacts,
) -> CandidateEvidence {
    let reason = if facts
        .binding_affinity
        .as_ref()
        .is_some_and(|affinity| affinity != binding)
    {
        Some(SkipReason::BindingAffinity)
    } else if let Some(live) = live {
        common_denial(live, facts.required_capabilities)
            .or_else(|| model_availability(binding, Some(model), live.catalogue, inventory))
    } else {
        Some(SkipReason::BindingUnknown)
    };
    CandidateEvidence {
        candidate_id: CandidateId::new("presentation-exact").expect("static id"),
        binding_id: binding.clone(),
        model: Some(model.to_owned()),
        result: reason.map_or(Ok(()), Err),
    }
}

fn common_denial(live: &BindingAvailability, required: CapabilityFlags) -> Option<SkipReason> {
    if matches!(live.configured, Evidence::Negative(_)) {
        return Some(SkipReason::ProviderUnconfigured);
    }
    if matches!(live.enabled, Evidence::Negative(_)) {
        return Some(SkipReason::BindingDisabled);
    }
    if matches!(live.credential, Evidence::Negative(_)) {
        return Some(SkipReason::CredentialUnavailable);
    }
    let actual = live.capabilities;
    if (required.sampling && !actual.sampling)
        || (required.system_in_messages && !actual.system_in_messages)
        || (required.thinking_replay && !actual.thinking_replay)
        || (required.images && !actual.images)
        || (required.tools && !actual.tools)
    {
        return Some(SkipReason::MissingRequiredCapability);
    }
    None
}

fn model_availability(
    binding: &BindingId,
    model: Option<&str>,
    catalogue: CatalogueAvailability,
    inventory: &OfferInventory,
) -> Option<SkipReason> {
    let Some(model) = model else {
        return Some(SkipReason::SelectorUnresolved);
    };
    if inventory.get(binding, model).is_some() {
        return None;
    }
    match catalogue {
        CatalogueAvailability::Fresh { .. } => Some(SkipReason::ModelMissingFromFreshCatalogue),
        CatalogueAvailability::Stale { .. } | CatalogueAvailability::Absent => None,
    }
}

fn edge_denial(
    edge: &CandidateEdge,
    from: Option<&BindingAvailability>,
    to: Option<&BindingAvailability>,
    lane: Option<&LaneFacts>,
) -> Option<SkipReason> {
    if edge.preflight == FallbackPermission::Disabled {
        return Some(SkipReason::PreflightFallbackDisabled);
    }
    if matches!(
        (from.map(|v| v.cost_class), to.map(|v| v.cost_class)),
        (
            Some(CostClass::Subscription),
            Some(CostClass::Billed | CostClass::Unknown)
        )
    ) && edge.billed_boundary == BoundaryConsent::Deny
    {
        return Some(SkipReason::BilledBoundaryDenied);
    }
    if lane.is_some_and(|lane| !lane.cold)
        && edge.cache_transition != CacheTransitionPolicy::AllowCacheLoss
        && from.and_then(|v| v.cache_domain.as_deref())
            != to.and_then(|v| v.cache_domain.as_deref())
    {
        return Some(SkipReason::WarmCacheTransitionDenied);
    }
    None
}

fn stricter_transition(
    left: LaneTransitionPolicy,
    right: LaneTransitionPolicy,
) -> LaneTransitionPolicy {
    use LaneTransitionPolicy::{AllowImmediate, MoveWhenCold, PreservePinned};
    match (left, right) {
        (PreservePinned, _) | (_, PreservePinned) => PreservePinned,
        (MoveWhenCold, _) | (_, MoveWhenCold) => MoveWhenCold,
        (AllowImmediate, AllowImmediate) => AllowImmediate,
    }
}

fn transition(policy: LaneTransitionPolicy, lane: Option<&LaneFacts>) -> TransitionDecision {
    match policy {
        LaneTransitionPolicy::PreservePinned => TransitionDecision::Preserve,
        LaneTransitionPolicy::MoveWhenCold if lane.is_some_and(|lane| lane.cold) => {
            TransitionDecision::Move
        }
        LaneTransitionPolicy::MoveWhenCold => TransitionDecision::MoveWhenCold,
        LaneTransitionPolicy::AllowImmediate => TransitionDecision::Move,
    }
}

fn merge_capabilities(left: CapabilityFlags, right: CapabilityFlags) -> CapabilityFlags {
    CapabilityFlags {
        sampling: left.sampling || right.sampling,
        system_in_messages: left.system_in_messages || right.system_in_messages,
        thinking_replay: left.thinking_replay || right.thinking_replay,
        images: left.images || right.images,
        tools: left.tools || right.tools,
    }
}

fn choose_upgrade<L: LearningView>(
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
    learning: &L,
    lane: Option<&LaneFacts>,
    facts: &RequestFacts,
    candidate: &CompiledCandidate,
    baseline: &str,
) -> Result<Option<String>, ResolveFailure> {
    let Some(upgrade_id) = candidate.upgrade_policy_id() else {
        return Ok(None);
    };
    let Some(upgrade) = policy.upgrade(upgrade_id).map(|upgrade| upgrade.policy()) else {
        return Ok(None);
    };
    if !upgrade.enabled {
        return Ok(None);
    }
    let classifier = policy
        .classifier(&upgrade.classifier_id)
        .ok_or(ResolveFailure::InvalidTemplate)?;

    if let Some(lane) = lane
        && !lane.cold
        && lane.forced_from.as_deref() == Some(baseline)
        && let Some(to) = lane.forced_to.as_ref()
    {
        return Ok(Some(to.clone()));
    }
    if facts.class == RequestClass::Compaction {
        return Ok(None);
    }
    let eligible = lane.map_or_else(
        || {
            facts.message_count.is_some_and(|count| {
                count <= NEW_CONVERSATION_MESSAGES
                    || learning
                        .recently_sent(candidate.binding_id(), baseline)
                        .ok()
                        .is_some_and(|recency| match recency {
                            super::RecencyEvidence::Unknown => false,
                            super::RecencyEvidence::Covered {
                                last_sent,
                                covered_since,
                            } => idle_for_ttl(last_sent, covered_since, facts.now_ms),
                        })
            })
        },
        |lane| lane.cold,
    );
    if !eligible {
        return Ok(None);
    }

    let baseline_class = classify(classifier, baseline)?;
    let Some((family, baseline_version)) = baseline_class else {
        return Ok(None);
    };
    let mut models = BTreeSet::new();
    for source in &upgrade.inventory_sources {
        match source {
            InventorySource::Catalogue => {
                models.extend(
                    inventory
                        .iter()
                        .filter(|offer| offer.binding_id == *candidate.binding_id())
                        .map(|offer| offer.backend_model_id.clone()),
                );
            }
            InventorySource::Served | InventorySource::Learned => {
                models.extend(
                    learning
                        .known_models(candidate.binding_id())
                        .map_err(|_| ResolveFailure::LearningUnavailable)?,
                );
            }
        }
    }
    let mut best: Option<(String, ParsedVersion)> = None;
    for model in models {
        let Some((model_family, version)) = classify(classifier, &model)? else {
            continue;
        };
        if model_family != family
            || version
                .partial_cmp(&baseline_version)
                .is_none_or(|order| !order.is_gt())
        {
            continue;
        }
        if !learning_minimum(
            upgrade.minimum_learning,
            learning,
            candidate.binding_id(),
            &model,
        )? {
            continue;
        }
        if !fits_context(
            inventory,
            learning,
            candidate.binding_id(),
            &model,
            facts.prompt_bound,
        )? {
            continue;
        }
        let replace = best.as_ref().is_none_or(|(_, current)| {
            version
                .partial_cmp(current)
                .is_some_and(|order| order.is_gt())
        });
        if replace {
            best = Some((model, version));
        }
    }
    Ok(best.map(|(model, _)| model))
}

fn learning_minimum<L: LearningView>(
    minimum: MinimumLearning,
    learning: &L,
    binding: &BindingId,
    model: &str,
) -> Result<bool, ResolveFailure> {
    match minimum {
        MinimumLearning::None => Ok(true),
        MinimumLearning::DistinctDays { days } => Ok(learning
            .learned(binding, model)
            .map_err(|_| ResolveFailure::LearningUnavailable)?
            .is_some_and(|evidence| evidence.distinct_days().len() >= usize::from(days))),
        MinimumLearning::ExplicitPromotion => Ok(learning
            .learned(binding, model)
            .map_err(|_| ResolveFailure::LearningUnavailable)?
            .is_some_and(|evidence| evidence.explicitly_promoted())),
        MinimumLearning::RevisionOneAdaptive => {
            let active_days = learning
                .active_days(binding)
                .map_err(|_| ResolveFailure::LearningUnavailable)?;
            let needed = crate::middleware::models::requirement(active_days);
            Ok(learning
                .learned(binding, model)
                .map_err(|_| ResolveFailure::LearningUnavailable)?
                .is_some_and(|evidence| evidence.distinct_days().len() as f64 > needed))
        }
    }
}

fn fits_context<L: LearningView>(
    inventory: &OfferInventory,
    learning: &L,
    binding: &BindingId,
    model: &str,
    prompt: u64,
) -> Result<bool, ResolveFailure> {
    if let Some(window) = inventory
        .get(binding, model)
        .and_then(|offer| offer.context_window)
    {
        return Ok(prompt <= window);
    }
    Ok(learning
        .learned(binding, model)
        .map_err(|_| ResolveFailure::LearningUnavailable)?
        .and_then(|evidence| {
            evidence
                .declared_window()
                .map(|window| window.max_tokens())
                .or_else(|| evidence.max_prompt())
        })
        .is_some_and(|max| prompt <= max))
}

fn classify(
    classifier: &CompiledClassifier,
    model: &str,
) -> Result<Option<(String, ParsedVersion)>, ResolveFailure> {
    let Some(captures) = classifier
        .matcher()
        .captures(model)
        .map_err(|_| ResolveFailure::InvalidTemplate)?
    else {
        return Ok(None);
    };
    let empty = BTreeMap::new();
    let family = classifier
        .family_template()
        .render(&empty, &captures)
        .map_err(|_| ResolveFailure::InvalidTemplate)?;
    let version = classifier
        .version_template()
        .render(&empty, &captures)
        .map_err(|_| ResolveFailure::InvalidTemplate)?;
    let version = classifier
        .parser()
        .parse(&version)
        .map_err(|_| ResolveFailure::InvalidTemplate)?;
    Ok(Some((family, version)))
}

fn apply_alias(
    candidate: &CompiledCandidate,
    model: &str,
    parameters: &BTreeMap<StableId, String>,
) -> Result<Option<String>, ResolveFailure> {
    for rule in candidate.alias_rules() {
        if rule
            .exclude()
            .iter()
            .any(|matcher| matcher.is_match(model).unwrap_or(false))
        {
            continue;
        }
        let Some(captures) = rule
            .matcher()
            .captures(model)
            .map_err(|_| ResolveFailure::InvalidTemplate)?
        else {
            continue;
        };
        return rule
            .replacement()
            .render(parameters, &captures)
            .map(Some)
            .map_err(|_| ResolveFailure::InvalidTemplate);
    }
    Ok(None)
}

/// Empty learning input for publication and tests that require no learning.
#[derive(Debug, Default)]
pub struct NoLearning;

impl LearningView for NoLearning {
    type Error = Infallible;

    fn recently_sent(&self, _: &BindingId, _: &str) -> Result<super::RecencyEvidence, Self::Error> {
        Ok(super::RecencyEvidence::Unknown)
    }
    fn learned(
        &self,
        _: &BindingId,
        _: &str,
    ) -> Result<Option<super::LearnedEvidence>, Self::Error> {
        Ok(None)
    }
    fn known_models(&self, _: &BindingId) -> Result<Vec<String>, Self::Error> {
        Ok(vec![])
    }
    fn active_days(&self, _: &BindingId) -> Result<usize, Self::Error> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::catalog::DeclaredWindow;
    use crate::catalog::offers::{BackendOffer, OfferInventory};
    use crate::policy::*;
    use crate::routing::{ANTHROPIC_API_MESSAGES, ANTHROPIC_SUB_MESSAGES, RouteRegistry};
    use crate::store::Store;

    fn id<T: std::str::FromStr>(value: &str) -> T
    where
        T::Err: std::fmt::Debug,
    {
        value.parse().unwrap()
    }

    fn reference(route: &str, source: ArgumentBinding) -> RouteReference {
        RouteReference {
            route_id: id(route),
            parameters: BTreeMap::from([(id(REQUESTED_MODEL_PARAMETER), source)]),
        }
    }

    fn candidate(
        name: &str,
        binding: &crate::routing::BackendBinding,
        model: CandidateModel,
    ) -> Candidate {
        Candidate {
            id: id(name),
            binding_id: binding.id().clone(),
            model,
            upgrade_policy_id: None,
            alias_policy: None,
            required_capabilities: Default::default(),
            enabled: true,
            health_circuits: vec![],
        }
    }

    fn route(first: Candidate, steps: Vec<RouteStep>) -> Route {
        Route {
            id: id("route"),
            display_name: "route".to_owned(),
            description: None,
            parameters: vec![id(REQUESTED_MODEL_PARAMETER)],
            chain: RouteChain { first, steps },
            lane_transition: LaneTransitionPolicy::PreservePinned,
        }
    }

    fn scope(profile: Option<&str>) -> FrontendScope {
        profile.map_or(
            FrontendScope::Protocol {
                protocol: FrontendProtocol::AnthropicMessages,
            },
            |profile| FrontendScope::Profile {
                profile: id(profile),
                protocol: FrontendProtocol::AnthropicMessages,
            },
        )
    }

    fn document(route: Route) -> PolicyDocument {
        PolicyDocument {
            format_version: CURRENT_FORMAT_VERSION,
            description: None,
            defaults: vec![DefaultRoute {
                scope: scope(None),
                target: reference("route", ArgumentBinding::RequestedModel),
            }],
            presentations: vec![],
            routes: vec![route],
            rewrites: vec![],
            classifiers: vec![],
            upgrades: vec![],
            compaction: None,
        }
    }

    fn compile(document: PolicyDocument, inventory: &OfferInventory) -> CompiledPolicy {
        let topology = RouteRegistry::new(
            std::iter::empty::<Arc<dyn crate::providers::Provider>>(),
            std::iter::empty::<(crate::routing::ProtocolId, String)>(),
        );
        crate::policy::compile(
            document.validate().unwrap(),
            &topology,
            inventory,
            PolicyOrigin::Authored,
        )
        .into_policy()
        .unwrap()
    }

    fn upgrade_inventory() -> OfferInventory {
        OfferInventory::from_offers(["claude-opus-4-8", "claude-opus-5"].map(|model| {
            BackendOffer {
                binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
                provider: "anthropic_sub".to_owned(),
                backend_model_id: model.to_owned(),
                binding: ANTHROPIC_SUB_MESSAGES.clone(),
                context_window: Some(1_000_000),
                chronology: None,
                fetched_at_ms: 1,
                raw: serde_json::Value::Null,
            }
        }))
    }

    fn upgrade_document() -> PolicyDocument {
        let mut first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::RequestedModel,
        );
        first.upgrade_policy_id = Some(id("upgrade"));
        let mut document = document(route(first, vec![]));
        document.classifiers.push(FamilyClassifier {
            id: id("classifier"),
            binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
            model_matcher: MatcherSpec::Regex(
                r"^claude-(?P<family>[a-z]+)-(?P<version>[0-9-]+)$".to_owned(),
            ),
            family_template: "${family}".to_owned(),
            version_template: "${version}".to_owned(),
            version_parser: VersionParserSpec::LegacyNumericTuple {
                separator: "-".to_owned(),
            },
        });
        document.upgrades.push(UpgradePolicy {
            id: id("upgrade"),
            classifier_id: id("classifier"),
            enabled: true,
            minimum_learning: MinimumLearning::None,
            allow_manual_promotion: false,
            inventory_sources: vec![InventorySource::Catalogue],
            lane_transition: LaneTransitionPolicy::MoveWhenCold,
            allow_revision_one_legacy_learning: false,
        });
        document
    }

    fn availability(configured: bool, fresh: bool) -> AvailabilitySnapshot {
        AvailabilitySnapshot::new(BTreeMap::from([
            (
                ANTHROPIC_SUB_MESSAGES.id().clone(),
                BindingAvailability {
                    provider: "anthropic_sub".to_owned(),
                    configured: if configured {
                        Evidence::Positive("configured")
                    } else {
                        Evidence::Negative("unconfigured")
                    },
                    enabled: Evidence::Positive("enabled"),
                    credential: Evidence::Unknown,
                    catalogue: if fresh {
                        CatalogueAvailability::Fresh { fetched_at_ms: 1 }
                    } else {
                        CatalogueAvailability::Absent
                    },
                    capabilities: Capabilities::MESSAGES,
                    cost_class: CostClass::Subscription,
                    cache_domain: Some("messages".to_owned()),
                },
            ),
            (
                ANTHROPIC_API_MESSAGES.id().clone(),
                BindingAvailability {
                    provider: "anthropic_api".to_owned(),
                    configured: Evidence::Positive("configured"),
                    enabled: Evidence::Positive("enabled"),
                    credential: Evidence::Unknown,
                    catalogue: CatalogueAvailability::Absent,
                    capabilities: Capabilities::MESSAGES,
                    cost_class: CostClass::Billed,
                    cache_domain: Some("api".to_owned()),
                },
            ),
        ]))
    }

    fn facts(model: &str) -> RequestFacts {
        RequestFacts {
            profile: Some(id("claude")),
            protocol: FrontendProtocol::AnthropicMessages,
            requested_model: Some(model.to_owned()),
            lane_reference: Some("opaque".to_owned()),
            prompt_bound: 10,
            message_count: Some(1),
            tool_count: 0,
            required_capabilities: Default::default(),
            binding_affinity: None,
            class: RequestClass::Inference,
            now_ms: 100,
        }
    }

    #[test]
    fn unknown_recency_cannot_prove_an_unknown_lane_cold() {
        let inventory = upgrade_inventory();
        let policy = compile(upgrade_document(), &inventory);
        let mut request = facts("claude-opus-4-8");
        request.message_count = Some(NEW_CONVERSATION_MESSAGES + 1);
        request.now_ms = 4_000_000;

        let plan = resolve(
            &policy,
            &inventory,
            &NoLearning,
            None,
            &availability(true, false),
            &request,
        )
        .unwrap();

        assert_eq!(plan.effective_model, "claude-opus-4-8");
        assert_eq!(plan.upgraded_model, None);
    }

    #[test]
    fn binding_learning_without_recency_cannot_prove_an_unknown_lane_cold() {
        let inventory = upgrade_inventory();
        let policy = compile(upgrade_document(), &inventory);
        let learning = BindingLearningView::new(Arc::new(Store::open(":memory:").unwrap()));
        let mut request = facts("claude-opus-4-8");
        request.message_count = Some(NEW_CONVERSATION_MESSAGES + 1);
        request.now_ms = 4_000_000;

        let plan = resolve(
            &policy,
            &inventory,
            &learning,
            None,
            &availability(true, false),
            &request,
        )
        .unwrap();

        assert_eq!(plan.effective_model, "claude-opus-4-8");
        assert_eq!(plan.upgraded_model, None);
    }

    #[test]
    fn context_fit_prefers_inventory_then_declaration_then_empirical_maximum() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        store
            .merge_binding_learning(&crate::store::BindingLearningUpdate {
                binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
                model_id: "model".to_owned(),
                days: Default::default(),
                max_prompt: Some(300),
                context_window: DeclaredWindow::new(100, 200),
                explicitly_promoted: false,
                updated_ms: 1,
            })
            .unwrap();
        let learning = BindingLearningView::new(store);
        let inventory = OfferInventory::from_offers([BackendOffer {
            binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
            provider: "anthropic_sub".to_owned(),
            backend_model_id: "model".to_owned(),
            binding: ANTHROPIC_SUB_MESSAGES.clone(),
            context_window: Some(100),
            chronology: None,
            fetched_at_ms: 1,
            raw: serde_json::Value::Null,
        }]);

        assert!(
            !fits_context(
                &inventory,
                &learning,
                ANTHROPIC_SUB_MESSAGES.id(),
                "model",
                150,
            )
            .unwrap()
        );
        assert!(
            fits_context(
                &OfferInventory::default(),
                &learning,
                ANTHROPIC_SUB_MESSAGES.id(),
                "model",
                150,
            )
            .unwrap()
        );
        assert!(
            !fits_context(
                &OfferInventory::default(),
                &learning,
                ANTHROPIC_SUB_MESSAGES.id(),
                "model",
                250,
            )
            .unwrap()
        );
    }

    #[test]
    fn context_fit_keeps_learned_evidence_binding_local() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        store
            .merge_binding_learning(&crate::store::BindingLearningUpdate {
                binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
                model_id: "same-model".to_owned(),
                days: Default::default(),
                max_prompt: Some(300),
                context_window: None,
                explicitly_promoted: false,
                updated_ms: 1,
            })
            .unwrap();
        let learning = BindingLearningView::new(store);

        assert!(
            fits_context(
                &OfferInventory::default(),
                &learning,
                ANTHROPIC_SUB_MESSAGES.id(),
                "same-model",
                250,
            )
            .unwrap()
        );
        assert!(
            !fits_context(
                &OfferInventory::default(),
                &learning,
                ANTHROPIC_API_MESSAGES.id(),
                "same-model",
                250,
            )
            .unwrap()
        );
    }

    #[test]
    fn lookup_precedence_and_regex_capture_are_exact() {
        let first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::RequestedModel,
        );
        let mut doc = document(route(first, vec![]));
        doc.presentations.push(Presentation {
            id: id("protocol-presentation"),
            scope: scope(None),
            slug: "friendly".to_owned(),
            label: None,
            description: None,
            enabled: true,
            shadow_native: false,
            source: PresentationSource::Virtual {
                target: reference(
                    "route",
                    ArgumentBinding::Literal {
                        value: "protocol".to_owned(),
                    },
                ),
            },
            metadata: None,
        });
        doc.presentations.push(Presentation {
            id: id("profile-presentation"),
            scope: scope(Some("claude")),
            slug: "friendly".to_owned(),
            label: None,
            description: None,
            enabled: true,
            shadow_native: false,
            source: PresentationSource::Virtual {
                target: reference(
                    "route",
                    ArgumentBinding::Literal {
                        value: "profile".to_owned(),
                    },
                ),
            },
            metadata: None,
        });
        doc.rewrites.push(RewriteRule {
            id: id("capture"),
            scope: scope(None),
            priority: 1,
            model_matcher: MatcherSpec::Regex(r"^vendor/(?P<model>.+)$".to_owned()),
            target: reference(
                "route",
                ArgumentBinding::RegexCaptureName {
                    name: "model".to_owned(),
                },
            ),
        });
        let policy = compile(doc, &OfferInventory::default());
        let plan = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &facts("friendly"),
        )
        .unwrap();
        assert_eq!(plan.baseline_model, "profile");
        assert!(matches!(
            plan.provenance,
            RouteProvenance::ProfilePresentation(_)
        ));

        let mut capture = facts("vendor/model-x");
        capture.profile = None;
        let plan = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &capture,
        )
        .unwrap();
        assert_eq!(plan.baseline_model, "model-x");
        assert!(matches!(
            plan.provenance,
            RouteProvenance::ProtocolRewrite(_)
        ));
    }

    #[test]
    fn fresh_catalogue_does_not_reject_a_compatibility_default_model() {
        let requested = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::RequestedModel,
        );
        let policy = compile(
            document(route(requested, vec![])),
            &OfferInventory::default(),
        );
        let plan = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, true),
            &facts("provider-owned-model"),
        )
        .unwrap();
        assert_eq!(plan.effective_model, "provider-owned-model");
    }

    #[test]
    fn fresh_missing_offer_skips_but_absent_catalogue_is_unknown_and_viable() {
        let first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "missing".to_owned(),
            },
        );
        let policy = compile(document(route(first, vec![])), &OfferInventory::default());
        assert!(matches!(
            resolve(
                &policy,
                &OfferInventory::default(),
                &NoLearning,
                None,
                &availability(true, true),
                &facts("asked"),
            ),
            Err(ResolveFailure::RouteUnavailable { .. })
        ));
        assert!(
            resolve(
                &policy,
                &OfferInventory::default(),
                &NoLearning,
                None,
                &availability(true, false),
                &facts("asked"),
            )
            .is_ok()
        );
    }

    #[test]
    fn preflight_edges_enforce_disabled_cost_cache_capability_and_affinity() {
        let first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "one".to_owned(),
            },
        );
        let second = candidate(
            "second",
            &ANTHROPIC_API_MESSAGES,
            CandidateModel::Exact {
                model_id: "two".to_owned(),
            },
        );
        let mut edge = CandidateEdge {
            preflight: FallbackPermission::Disabled,
            post_send: vec![],
            // Compilation requires explicit consent for a subscription-to-billed
            // edge. Runtime denial cases are exercised below with immutable
            // availability/cache evidence.
            billed_boundary: BoundaryConsent::Allow,
            ambiguous_transport: BoundaryConsent::Deny,
            cache_transition: CacheTransitionPolicy::AllowCacheLoss,
        };
        let policy = compile(
            document(route(
                first,
                vec![RouteStep {
                    edge: edge.clone(),
                    candidate: second.clone(),
                }],
            )),
            &OfferInventory::default(),
        );
        let mut request = facts("asked");
        request.binding_affinity = Some(ANTHROPIC_API_MESSAGES.id().clone());
        let failure = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &request,
        )
        .unwrap_err();
        let ResolveFailure::RouteUnavailable { trace } = failure else {
            panic!()
        };
        assert!(trace.iter().any(|event| matches!(event, TraceEvent::Candidate(e) if e.result == Err(SkipReason::PreflightFallbackDisabled))));

        edge.preflight = FallbackPermission::Enabled;
        let policy = compile(
            document(route(
                candidate(
                    "first",
                    &ANTHROPIC_SUB_MESSAGES,
                    CandidateModel::Exact {
                        model_id: "one".to_owned(),
                    },
                ),
                vec![RouteStep {
                    edge,
                    candidate: second,
                }],
            )),
            &OfferInventory::default(),
        );
        let selected = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &request,
        )
        .unwrap();
        assert_eq!(selected.binding_id, *ANTHROPIC_API_MESSAGES.id());

        request.binding_affinity = None;
        request.required_capabilities.thinking_replay = true;
        let mut availability = availability(true, false);
        availability
            .bindings
            .get_mut(ANTHROPIC_SUB_MESSAGES.id())
            .unwrap()
            .capabilities
            .thinking_replay = false;
        availability
            .bindings
            .get_mut(ANTHROPIC_API_MESSAGES.id())
            .unwrap()
            .capabilities
            .thinking_replay = false;
        let failure = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability,
            &request,
        )
        .unwrap_err();
        let ResolveFailure::RouteUnavailable { trace } = failure else {
            panic!()
        };
        assert!(trace.iter().any(|event| matches!(event, TraceEvent::Candidate(e) if e.result == Err(SkipReason::MissingRequiredCapability))));
    }

    #[test]
    fn alias_is_applied_after_baseline_and_trace_is_deterministic_without_mutation() {
        let mut first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::RequestedModel,
        );
        first.alias_policy = Some(AliasPolicy {
            rules: vec![AliasRule {
                matcher: MatcherSpec::Regex(
                    r"^claude-(?P<family>[a-z]+)-(?P<version>.+)$".to_owned(),
                ),
                exclude: vec![],
                replacement: "mapped-${family}-${version}".to_owned(),
            }],
        });
        let policy = compile(document(route(first, vec![])), &OfferInventory::default());
        let request = facts("claude-opus-5");
        let before = request.clone();
        let first = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &request,
        )
        .unwrap();
        let second = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &request,
        )
        .unwrap();
        assert_eq!(request, before);
        assert_eq!(first, second);
        assert_eq!(first.baseline_model, "claude-opus-5");
        assert_eq!(first.aliased_model.as_deref(), Some("mapped-opus-5"));
        assert_eq!(first.effective_model, "mapped-opus-5");
    }

    #[test]
    fn compaction_uses_the_explicit_route() {
        let first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::RequestedModel,
        );
        let compact = candidate(
            "compact",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "compact-model".to_owned(),
            },
        );
        let mut doc = document(route(first, vec![]));
        doc.routes.push(Route {
            id: id("compact-route"),
            display_name: "compact".to_owned(),
            description: None,
            parameters: vec![],
            chain: RouteChain {
                first: compact,
                steps: vec![],
            },
            lane_transition: LaneTransitionPolicy::MoveWhenCold,
        });
        doc.compaction = Some(CompactionPolicy {
            target: RouteReference {
                route_id: id("compact-route"),
                parameters: BTreeMap::new(),
            },
            lane_transition: LaneTransitionPolicy::MoveWhenCold,
            cache_transition: CacheTransitionPolicy::AllowWhenCold,
        });
        let policy = compile(doc, &OfferInventory::default());
        let mut request = facts("original");
        request.class = RequestClass::Compaction;
        let plan = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &request,
        )
        .unwrap();
        assert_eq!(plan.baseline_model, "compact-model");
        assert!(plan.compaction);
        assert_eq!(plan.provenance, RouteProvenance::Compaction);
    }
}
