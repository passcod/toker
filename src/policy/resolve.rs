//! Pure, content-free policy resolution.
//!
//! Resolution consumes immutable snapshots and returns a description of one
//! preflight-selected attempt. It has no access to request bodies, providers,
//! credentials, clocks, lanes, or stores and cannot render or send traffic.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;

use thiserror::Error;

use super::force_newest::{NEW_CONVERSATION_MESSAGES, idle_for_ttl};
use crate::catalog::offers::OfferInventory;
use crate::routing::Capabilities;

use super::{
    ArgumentBinding, BindingId, BoundaryConsent, CacheTransitionPolicy, CandidateEdge, CandidateId,
    CapabilityFlags, CompiledCandidate, CompiledCandidateModel, CompiledClassifier,
    CompiledCompaction, CompiledPolicy, CompiledRoute, CompiledRouteReference, CostClass,
    FailureClass, FallbackPermission, FrontendProtocol, FrontendScope, InventorySource,
    LaneTransitionPolicy, LearningView, MatcherCaptures, MinimumLearning, ParsedVersion,
    PolicyFingerprint, PresentationId, PresentationRoute, ProfileId, REQUESTED_MODEL_PARAMETER,
    RewriteId, RouteFingerprint, RouteId, StableId, UpgradePolicy,
};

/// The kind of request being resolved. This is derived from fixed markers;
/// neither the marker nor any prompt text enters policy resolution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RequestClass {
    #[default]
    Inference,
    Compaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyCompactionFacts {
    pub lane_cold: bool,
    pub prompt_tokens: u64,
    /// Whether the authoritative all-or-nothing body transform succeeded.
    pub transform_applied: bool,
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
    /// Whether the request asks for server safeguards. It binds a route only
    /// where the route honours them.
    pub asks_server_safeguards: bool,
    pub binding_affinity: Option<BindingId>,
    pub class: RequestClass,
    /// Exact revision-one compaction evidence, separate from generic cache
    /// temperature so it cannot authorize unrelated transitions.
    pub legacy_compaction: Option<LegacyCompactionFacts>,
    /// The account each provider's credential names for this request, by
    /// provider id. A provider with no entry is one whose accounts Toker
    /// cannot tell apart, or whose credential no source has named yet.
    pub accounts: BTreeMap<String, i64>,
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
    /// Declared capabilities, with server safeguards narrowed to
    /// unsupported where a rejection mark is held.
    pub capabilities: Capabilities,
    /// Whether a rejection mark is held against the binding's server
    /// safeguards.
    pub safeguards_marked: bool,
    pub cost_class: CostClass,
}

/// One consistently captured availability snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AvailabilitySnapshot {
    bindings: BTreeMap<BindingId, BindingAvailability>,
}

pub type CredentialSnapshot = BTreeMap<BindingId, Evidence<&'static str>>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct OpenCircuitTarget {
    pub route_id: RouteId,
    pub candidate_id: CandidateId,
    pub binding_id: BindingId,
    pub model_id: String,
    pub failure: super::FailureClass,
}

pub type CircuitSnapshot = std::collections::BTreeSet<OpenCircuitTarget>;

impl AvailabilitySnapshot {
    pub fn new(bindings: BTreeMap<BindingId, BindingAvailability>) -> Self {
        Self { bindings }
    }

    pub fn get(&self, binding: &BindingId) -> Option<&BindingAvailability> {
        self.bindings.get(binding)
    }

    pub fn with_credentials(&self, credentials: &CredentialSnapshot) -> Self {
        let mut snapshot = self.clone();
        for (binding, credential) in credentials {
            if let Some(availability) = snapshot.bindings.get_mut(binding) {
                availability.credential = *credential;
            }
        }
        snapshot
    }

    /// Narrow server safeguards to unsupported on every marked binding.
    pub fn with_safeguards_marks(&self, marked: &BTreeSet<BindingId>) -> Self {
        let mut snapshot = self.clone();
        for binding in marked {
            if let Some(availability) = snapshot.bindings.get_mut(binding) {
                availability.safeguards_marked = true;
                availability.capabilities.server_safeguards =
                    crate::routing::CapabilityEvidence::Unsupported;
            }
        }
        snapshot
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
    pub temperature: super::CacheTemperature,
    pub pinned: Option<ProvisionalPin>,
    pub forced_from: Option<String>,
    pub forced_to: Option<String>,
}

/// An immutable prospective pin. Slice five never persists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionalPin {
    pub policy_revision_id: Option<i64>,
    pub policy_fingerprint: Option<PolicyFingerprint>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    Disabled,
    BindingUnknown,
    ProviderUnconfigured,
    BindingDisabled,
    CredentialUnavailable,
    MissingRequiredCapability,
    /// The route honours server safeguards, the request asks for them, and the
    /// binding's effective support is unsupported.
    LacksServerSafeguards,
    BindingAffinity,
    ModelMissingFromFreshCatalogue,
    ModelUnknown,
    SelectorUnresolved,
    HealthCircuitOpen,
    PreflightFallbackDisabled,
    BilledBoundaryDenied,
    WarmCacheTransitionDenied,
    WarmLaneTransitionDenied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateEvidence {
    pub candidate_id: CandidateId,
    pub crossed_from: Option<CandidateId>,
    pub binding_id: BindingId,
    pub model: Option<String>,
    pub result: Result<(), SkipReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEvent {
    Lookup(RouteProvenance),
    Candidate(CandidateEvidence),
    Upgrade {
        from: String,
        to: String,
    },
    Alias {
        from: String,
        to: String,
    },
    /// A route honouring server safeguards ended without a candidate and was
    /// walked again without the requirement.
    SafeguardsWalkedAgain,
}

/// The complete, content-free result of pure preflight resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptPlan {
    pub policy_fingerprint: PolicyFingerprint,
    /// The stored revision this plan resolved under; an in-memory policy
    /// resolved directly has none.
    pub policy_revision: Option<i64>,
    pub route_id: Option<RouteId>,
    pub route_fingerprint: Option<RouteFingerprint>,
    /// The account this attempt is expected to serve on, attached by the
    /// server after resolution. Routing does not select an account
    /// (spec: ACCT#routing-does-not-select-an-account), so the resolver
    /// never fills this. `None` where the provider has no accounts Toker can
    /// tell apart, which is why it is not required for a plan to be pinnable.
    pub account_number: Option<i64>,
    pub provenance: RouteProvenance,
    pub candidate_id: Option<CandidateId>,
    pub binding_id: BindingId,
    pub provider: String,
    pub requested_model: Option<String>,
    pub baseline_model: String,
    /// Whether the client's own model spelling decided the baseline, as
    /// opposed to a policy-owned exact model or an offer selector's winner.
    /// A changed baseline on the same candidate is the client's doing only
    /// when this holds; otherwise the policy moved it.
    pub baseline_from_request: bool,
    pub upgraded_model: Option<String>,
    pub retargeted_model: Option<String>,
    pub aliased_model: Option<String>,
    pub effective_model: String,
    pub compaction: bool,
    pub lane_transition: LaneTransitionPolicy,
    pub cache_transition: Option<CacheTransitionPolicy>,
    pub fallback_cache_transition: Option<CacheTransitionPolicy>,
    pub trace: Vec<TraceEvent>,
    /// What server safeguards meant for this attempt, content-free.
    pub safeguards: SafeguardsPlan,
    continuation: Option<PostSendContinuation>,
}

/// What a request's ask for server safeguards came to for one attempt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SafeguardsPlan {
    /// The request asked.
    pub asked: bool,
    /// The serving binding's effective support, when it is known to the
    /// resolver's snapshot.
    pub effective: Option<crate::routing::CapabilityEvidence>,
    /// A rejection mark was held against the serving binding.
    pub marked: bool,
    /// Honouring the ask skipped at least one candidate.
    pub skipped_candidate: bool,
    /// The route was walked again without the requirement.
    pub walked_again: bool,
}

impl AttemptPlan {
    pub fn with_policy_revision(mut self, revision_id: i64) -> Self {
        self.policy_revision = Some(revision_id);
        if let Some(continuation) = self.continuation.as_mut() {
            continuation.policy_revision = Some(revision_id);
        }
        self
    }

    /// The upgrade this plan made, as the `(from, to)` pair a row and its
    /// lane record. The lane keeps it while its cache is warm, which is what
    /// makes the next turn resolve to the same model instead of the one the
    /// client keeps asking for.
    pub fn forced_pair(&self) -> Option<(String, String)> {
        self.upgraded_model
            .as_ref()
            .map(|to| (self.baseline_model.clone(), to.clone()))
    }

    /// The model a compaction was moved off and onto, as the `(from, to)`
    /// pair a row records as its downgrade.
    ///
    /// Policy picks the cheaper model before the compaction body transform
    /// runs, so the transform sees the request already on it and cannot report
    /// the move; the plan still holds both ends. `None` when the plan made no
    /// move: a compaction that kept its model only strips cache writes.
    pub fn compaction_move(&self) -> Option<(String, String)> {
        let from = if self.provenance == RouteProvenance::Compaction {
            self.requested_model.clone()?
        } else if self.retargeted_model.is_some() {
            self.upgraded_model
                .clone()
                .unwrap_or_else(|| self.baseline_model.clone())
        } else {
            return None;
        };
        (from != self.effective_model).then(|| (from, self.effective_model.clone()))
    }

    pub fn pin_identity(&self) -> Option<super::PinIdentity> {
        Some(super::PinIdentity {
            policy_revision_id: self.policy_revision?,
            policy_route_id: self.route_id.clone()?,
            route_fingerprint: self.route_fingerprint.clone()?,
            candidate_id: self.candidate_id.clone()?,
            binding_id: self.binding_id.clone(),
            account_number: self.account_number,
            baseline_model: self.baseline_model.clone(),
            effective_model: self.effective_model.clone(),
        })
    }
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
    RouteUnavailable {
        route_id: RouteId,
        route_fingerprint: RouteFingerprint,
        provenance: RouteProvenance,
        trace: Vec<TraceEvent>,
    },
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
    resolve_with_circuits(
        policy,
        inventory,
        learning,
        lane,
        availability,
        &CircuitSnapshot::new(),
        facts,
    )
}

pub fn resolve_with_circuits<L: LearningView>(
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
    learning: &L,
    lane: Option<&LaneFacts>,
    availability: &AvailabilitySnapshot,
    circuits: &CircuitSnapshot,
    facts: &RequestFacts,
) -> Result<AttemptPlan, ResolveFailure> {
    resolve_with_options(
        policy,
        inventory,
        learning,
        lane,
        availability,
        circuits,
        facts,
        ResolveOptions::default(),
    )
}

pub fn resolve_first_candidate<L: LearningView>(
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
    learning: &L,
    availability: &AvailabilitySnapshot,
    circuits: &CircuitSnapshot,
    facts: &RequestFacts,
) -> Result<AttemptPlan, ResolveFailure> {
    resolve_with_options(
        policy,
        inventory,
        learning,
        None,
        availability,
        circuits,
        facts,
        ResolveOptions {
            activation_allows_cache_loss: false,
            traverse_candidates: false,
            post_send: None,
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PostSendContinuation {
    policy_fingerprint: PolicyFingerprint,
    policy_revision: Option<i64>,
    route_id: RouteId,
    route_fingerprint: RouteFingerprint,
    provenance: RouteProvenance,
    parameters: BTreeMap<StableId, String>,
    lane_transition: Option<LaneTransitionPolicy>,
    cache_transition: Option<CacheTransitionPolicy>,
    facts: RequestFacts,
    lane: Option<LaneFacts>,
    route_start_index: usize,
    failed_candidate_index: usize,
    trace: Vec<TraceEvent>,
}

#[derive(Debug, Clone)]
struct ResolveOptions {
    activation_allows_cache_loss: bool,
    traverse_candidates: bool,
    post_send: Option<PostSendContinuation>,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            activation_allows_cache_loss: false,
            traverse_candidates: true,
            post_send: None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_with_activation_cache_consent<L: LearningView>(
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
    learning: &L,
    lane: Option<&LaneFacts>,
    availability: &AvailabilitySnapshot,
    circuits: &CircuitSnapshot,
    facts: &RequestFacts,
    activation_allows_cache_loss: bool,
) -> Result<AttemptPlan, ResolveFailure> {
    resolve_with_options(
        policy,
        inventory,
        learning,
        lane,
        availability,
        circuits,
        facts,
        ResolveOptions {
            activation_allows_cache_loss,
            traverse_candidates: true,
            post_send: None,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_after_failure<L: LearningView>(
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
    learning: &L,
    availability: &AvailabilitySnapshot,
    circuits: &CircuitSnapshot,
    failed: &AttemptPlan,
    failure: FailureClass,
) -> Result<Option<AttemptPlan>, ResolveFailure> {
    let Some(continuation) = failed.continuation.as_ref() else {
        return Ok(None);
    };
    if continuation.policy_fingerprint != *policy.fingerprint() {
        return Err(ResolveFailure::MissingRoute);
    }
    let Some(route) = policy.route(&continuation.route_id) else {
        return Err(ResolveFailure::MissingRoute);
    };
    if continuation.route_fingerprint != *route.fingerprint()
        || continuation.failed_candidate_index >= route.edges().len()
        || !route.edges()[continuation.failed_candidate_index]
            .post_send
            .contains(&failure)
    {
        return Ok(None);
    }
    let selected = SelectedRoute {
        route: Some(route),
        exact: None,
        parameters: continuation.parameters.clone(),
        provenance: continuation.provenance.clone(),
        lane_transition: continuation.lane_transition,
        cache_transition: continuation.cache_transition,
    };
    let mut resolved = resolve_selected(
        policy,
        inventory,
        learning,
        continuation.lane.as_ref(),
        availability,
        circuits,
        &continuation.facts,
        selected,
        ResolveOptions {
            activation_allows_cache_loss: false,
            traverse_candidates: true,
            post_send: Some(continuation.clone()),
        },
    )?;
    resolved.policy_revision = continuation.policy_revision;
    if let Some(next) = resolved.continuation.as_mut() {
        next.policy_revision = continuation.policy_revision;
    }
    Ok(Some(resolved))
}

#[allow(clippy::too_many_arguments)]
fn resolve_with_options<L: LearningView>(
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
    learning: &L,
    lane: Option<&LaneFacts>,
    availability: &AvailabilitySnapshot,
    circuits: &CircuitSnapshot,
    facts: &RequestFacts,
    options: ResolveOptions,
) -> Result<AttemptPlan, ResolveFailure> {
    let mut selected = lookup(policy, facts)?;
    if facts.class == RequestClass::Compaction
        && lane.is_some_and(|lane| lane.temperature == super::CacheTemperature::Cold)
        && let Some((target, lane_transition, cache_transition)) =
            policy.compaction().and_then(CompiledCompaction::route)
    {
        selected = from_reference(policy, target, facts, None, RouteProvenance::Compaction)?;
        selected.lane_transition = Some(lane_transition);
        selected.cache_transition = Some(cache_transition);
    }
    resolve_selected(
        policy,
        inventory,
        learning,
        lane,
        availability,
        circuits,
        facts,
        selected,
        options,
    )
}

/// Walk the selected route, then, where honouring server safeguards left no
/// candidate, walk it again without them. The plan records what the ask came
/// to.
#[allow(clippy::too_many_arguments)]
fn resolve_selected<L: LearningView>(
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
    learning: &L,
    lane: Option<&LaneFacts>,
    availability: &AvailabilitySnapshot,
    circuits: &CircuitSnapshot,
    facts: &RequestFacts,
    selected: SelectedRoute<'_>,
    options: ResolveOptions,
) -> Result<AttemptPlan, ResolveFailure> {
    let honour = facts.asks_server_safeguards
        && selected
            .route
            .is_some_and(CompiledRoute::honours_server_safeguards);
    let walk = |honour: bool, trace: Option<Vec<TraceEvent>>| {
        walk_selected(
            policy,
            inventory,
            learning,
            lane,
            availability,
            circuits,
            facts,
            &selected,
            options.clone(),
            honour,
            trace,
        )
    };
    let mut walked_again = false;
    let result = match walk(honour, None) {
        Err(ResolveFailure::RouteUnavailable { mut trace, .. }) if honour => {
            walked_again = true;
            trace.push(TraceEvent::SafeguardsWalkedAgain);
            walk(false, Some(trace))
        }
        other => other,
    };
    result.map(|mut plan| {
        let live = availability.get(&plan.binding_id);
        plan.safeguards = SafeguardsPlan {
            asked: facts.asks_server_safeguards,
            effective: live.map(|live| live.capabilities.server_safeguards),
            marked: live.is_some_and(|live| live.safeguards_marked),
            skipped_candidate: plan.trace.iter().any(|event| {
                matches!(
                    event,
                    TraceEvent::Candidate(CandidateEvidence {
                        result: Err(SkipReason::LacksServerSafeguards),
                        ..
                    })
                )
            }),
            walked_again,
        };
        plan
    })
}

#[allow(clippy::too_many_arguments)]
fn walk_selected<L: LearningView>(
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
    learning: &L,
    lane: Option<&LaneFacts>,
    availability: &AvailabilitySnapshot,
    circuits: &CircuitSnapshot,
    facts: &RequestFacts,
    selected: &SelectedRoute<'_>,
    options: ResolveOptions,
    honour_server_safeguards: bool,
    initial_trace: Option<Vec<TraceEvent>>,
) -> Result<AttemptPlan, ResolveFailure> {
    let activation_allows_cache_loss = options.activation_allows_cache_loss;
    let mut trace = initial_trace.unwrap_or_else(|| {
        options.post_send.as_ref().map_or_else(
            || vec![TraceEvent::Lookup(selected.provenance.clone())],
            |next| next.trace.clone(),
        )
    });
    if let Some((binding_id, model, route_id, fingerprint, candidate_id)) = selected.exact {
        if options.post_send.is_some() {
            return Err(ResolveFailure::MissingRoute);
        }
        let live = availability.get(binding_id);
        let mut evidence = exact_evidence(candidate_id, binding_id, model, live, inventory, facts);
        if target_has_open_circuit(circuits, route_id, candidate_id, binding_id, model) {
            evidence.result = Err(SkipReason::HealthCircuitOpen);
        }
        trace.push(TraceEvent::Candidate(evidence.clone()));
        if evidence.result.is_err() {
            return Err(ResolveFailure::RouteUnavailable {
                route_id: route_id.clone(),
                route_fingerprint: fingerprint.clone(),
                provenance: selected.provenance.clone(),
                trace,
            });
        }
        let provider = live.map_or_else(String::new, |live| live.provider.clone());
        return Ok(AttemptPlan {
            policy_fingerprint: policy.fingerprint().clone(),
            policy_revision: None,
            route_id: Some(route_id.clone()),
            route_fingerprint: Some(fingerprint.clone()),
            account_number: None,
            provenance: selected.provenance.clone(),
            candidate_id: Some(candidate_id.clone()),
            binding_id: binding_id.clone(),
            provider,
            requested_model: facts.requested_model.clone(),
            baseline_model: model.to_owned(),
            baseline_from_request: false,
            upgraded_model: None,
            retargeted_model: None,
            aliased_model: None,
            effective_model: model.to_owned(),
            compaction: selected.provenance == RouteProvenance::Compaction,
            lane_transition: LaneTransitionPolicy::PreservePinned,
            cache_transition: selected.cache_transition,
            fallback_cache_transition: None,
            trace,
            safeguards: SafeguardsPlan::default(),
            continuation: None,
        });
    }

    let route = selected.route.ok_or(ResolveFailure::MissingRoute)?;
    if options.post_send.as_ref().is_some_and(|next| {
        next.route_start_index >= route.candidates().len()
            || next.failed_candidate_index < next.route_start_index
            || next.failed_candidate_index >= route.candidates().len()
    }) {
        return Err(ResolveFailure::MissingRoute);
    }
    let pinned = lane
        .and_then(|lane| lane.pinned.as_ref())
        .filter(|pin| pin.policy_fingerprint.as_ref() == Some(policy.fingerprint()))
        .filter(|pin| pin.route_fingerprint == *route.fingerprint());
    let pinned_start = pinned
        .and_then(|pin| {
            route
                .candidates()
                .iter()
                .position(|candidate| candidate.id() == &pin.candidate_id)
        })
        .unwrap_or(0);
    let route_start = options
        .post_send
        .as_ref()
        .map_or(pinned_start, |next| next.route_start_index);
    let start = options
        .post_send
        .as_ref()
        .map_or(route_start, |next| next.failed_candidate_index + 1);
    let mut prior_live = (start > route_start)
        .then(|| route.candidates()[start - 1].binding_id())
        .and_then(|binding| availability.get(binding));
    let end = if options.traverse_candidates {
        route.candidates().len()
    } else {
        (start + 1).min(route.candidates().len())
    };
    for (index, candidate) in route.candidates().iter().enumerate().take(end).skip(start) {
        if index > route_start {
            let edge = &route.edges()[index - 1];
            let edge_failure = if index == start && options.post_send.is_some() {
                post_send_edge_denial(edge, prior_live, availability.get(candidate.binding_id()))
            } else {
                edge_denial(edge, prior_live, availability.get(candidate.binding_id()))
            };
            if let Some(reason) = edge_failure {
                trace.push(TraceEvent::Candidate(CandidateEvidence {
                    candidate_id: candidate.id().clone(),
                    crossed_from: None,
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
        let mut evidence = candidate_evidence(
            candidate,
            baseline.as_deref(),
            live,
            inventory,
            facts,
            honour_server_safeguards,
        );
        if index > route_start {
            evidence.crossed_from = Some(route.candidates()[index - 1].id().clone());
        }
        let candidate_trace_index = trace.len();
        trace.push(TraceEvent::Candidate(evidence.clone()));
        if evidence.result == Err(SkipReason::SelectorUnresolved) {
            break;
        }
        if evidence.result.is_err() {
            continue;
        }
        let baseline = baseline.ok_or(ResolveFailure::InvalidTemplate)?;
        if selected.provenance == RouteProvenance::Compaction
            && lane.is_some_and(|lane| lane.temperature != super::CacheTemperature::Cold)
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
            return Err(route_unavailable(route, &selected.provenance, trace));
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
        let before_compaction = upgraded.as_deref().unwrap_or(&baseline);
        let retargeted = choose_revision_one_compaction(
            policy,
            inventory,
            learning,
            facts,
            candidate,
            &selected.parameters,
            before_compaction,
        )?;
        let before_alias = retargeted.as_deref().unwrap_or(before_compaction);
        let aliased = apply_alias(candidate, before_alias, &selected.parameters)?;
        if let Some(to) = aliased.as_deref() {
            trace.push(TraceEvent::Alias {
                from: before_alias.to_owned(),
                to: to.to_owned(),
            });
        }
        let effective = aliased
            .clone()
            .or_else(|| retargeted.clone())
            .or_else(|| upgraded.clone())
            .unwrap_or_else(|| baseline.clone());
        if index > route_start
            && !activation_allows_cache_loss
            && let Some(reason) = cache_edge_denial(
                &route.edges()[route_start..index],
                candidate.binding_id(),
                &effective,
                lane,
            )
        {
            let TraceEvent::Candidate(evidence) = &mut trace[candidate_trace_index] else {
                unreachable!("the recorded candidate event keeps its index")
            };
            evidence.crossed_from = None;
            evidence.model = Some(effective);
            evidence.result = Err(reason);
            break;
        }
        if target_has_open_circuit(
            circuits,
            route.id(),
            candidate.id(),
            candidate.binding_id(),
            &effective,
        ) {
            let TraceEvent::Candidate(evidence) = &mut trace[candidate_trace_index] else {
                unreachable!("the recorded candidate event keeps its index")
            };
            evidence.model = Some(effective);
            evidence.result = Err(SkipReason::HealthCircuitOpen);
            continue;
        }
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
            account_number: None,
            provenance: selected.provenance.clone(),
            candidate_id: Some(candidate.id().clone()),
            binding_id: candidate.binding_id().clone(),
            provider,
            requested_model: facts.requested_model.clone(),
            baseline_model: baseline,
            baseline_from_request: matches!(
                candidate.model(),
                CompiledCandidateModel::RequestedModel | CompiledCandidateModel::Template(_)
            ),
            upgraded_model: upgraded,
            retargeted_model: retargeted,
            aliased_model: aliased,
            effective_model: effective,
            compaction: selected.provenance == RouteProvenance::Compaction
                || (facts
                    .legacy_compaction
                    .is_some_and(|facts| facts.transform_applied)
                    && policy
                        .compaction()
                        .and_then(CompiledCompaction::revision_one_legacy)
                        .is_some_and(|(_, bindings)| bindings.contains(candidate.binding_id()))),
            lane_transition: lane_policy,
            cache_transition: selected.cache_transition,
            fallback_cache_transition: (index > route_start).then(|| {
                route.edges()[route_start..index]
                    .iter()
                    .fold(CacheTransitionPolicy::AllowCacheLoss, |policy, edge| {
                        stricter_cache_transition(policy, edge.cache_transition)
                    })
            }),
            safeguards: SafeguardsPlan::default(),
            continuation: Some(PostSendContinuation {
                policy_fingerprint: policy.fingerprint().clone(),
                policy_revision: None,
                route_id: route.id().clone(),
                route_fingerprint: route.fingerprint().clone(),
                provenance: selected.provenance.clone(),
                parameters: selected.parameters.clone(),
                lane_transition: selected.lane_transition,
                cache_transition: selected.cache_transition,
                facts: facts.clone(),
                lane: lane.cloned(),
                route_start_index: route_start,
                failed_candidate_index: index,
                trace: trace.clone(),
            }),
            trace,
        });
    }
    Err(route_unavailable(route, &selected.provenance, trace))
}

fn route_unavailable(
    route: &CompiledRoute,
    provenance: &RouteProvenance,
    trace: Vec<TraceEvent>,
) -> ResolveFailure {
    ResolveFailure::RouteUnavailable {
        route_id: route.id().clone(),
        route_fingerprint: route.fingerprint().clone(),
        provenance: provenance.clone(),
        trace,
    }
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
    let (reference, provenance) =
        default_reference(policy, facts).ok_or(ResolveFailure::NoRoute)?;
    from_reference(policy, reference, facts, None, provenance)
}

/// The exact-profile default, else the protocol default.
fn default_reference<'a>(
    policy: &'a CompiledPolicy,
    facts: &RequestFacts,
) -> Option<(&'a CompiledRouteReference, RouteProvenance)> {
    facts
        .profile_scope()
        .and_then(|scope| policy.default_route(&scope))
        .map(|reference| (reference, RouteProvenance::ProfileDefault))
        .or_else(|| {
            policy
                .default_route(&facts.protocol_scope())
                .map(|reference| (reference, RouteProvenance::ProtocolDefault))
        })
}

/// Resolve a request that carries no model, such as a batch result fetch or
/// any Anthropic path toker does not itself serve, to the one binding it is
/// forwarded to.
///
/// Only the default route can answer it: there is no model for a
/// presentation or rewrite to match. The route's first candidate is the
/// whole answer, with no fallback and no model, so none of the candidate's
/// model-dependent steps (offer selection, upgrade, alias, circuits) apply.
/// This is deliberately not a mode of [`resolve`]: an inference request
/// without a model has nothing to send and still fails there, and a request
/// that does carry a model is refused here rather than silently routed
/// without it.
pub fn resolve_model_free(
    policy: &CompiledPolicy,
    availability: &AvailabilitySnapshot,
    facts: &RequestFacts,
) -> Result<BindingId, ResolveFailure> {
    if facts.requested_model.is_some() {
        return Err(ResolveFailure::InvalidArgument);
    }
    let (reference, provenance) =
        default_reference(policy, facts).ok_or(ResolveFailure::NoRoute)?;
    let route = policy
        .route(reference.route_id())
        .ok_or(ResolveFailure::MissingRoute)?;
    let candidate = route
        .candidates()
        .first()
        .ok_or(ResolveFailure::MissingRoute)?;
    let reason = if !candidate.enabled() {
        Some(SkipReason::Disabled)
    } else {
        availability.get(candidate.binding_id()).and_then(|live| {
            common_denial(
                live,
                merge_capabilities(
                    candidate.required_capabilities(),
                    facts.required_capabilities,
                ),
            )
        })
    };
    match reason {
        None => Ok(candidate.binding_id().clone()),
        Some(reason) => Err(ResolveFailure::RouteUnavailable {
            route_id: route.id().clone(),
            route_fingerprint: route.fingerprint().clone(),
            provenance: provenance.clone(),
            trace: vec![
                TraceEvent::Lookup(provenance),
                TraceEvent::Candidate(CandidateEvidence {
                    candidate_id: candidate.id().clone(),
                    crossed_from: None,
                    binding_id: candidate.binding_id().clone(),
                    model: None,
                    result: Err(reason),
                }),
            ],
        }),
    }
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
                    .and_then(|classifier| classifier.classify(&offer.backend_model_id).ok())
                    .flatten()
                    .map(|classified| classified.version)
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
    honour_server_safeguards: bool,
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
        .or_else(|| {
            (honour_server_safeguards && live.capabilities.server_safeguards.is_unsupported())
                .then_some(SkipReason::LacksServerSafeguards)
        })
        .or_else(|| match candidate.model() {
            // A fresh listing can disprove a policy-owned exact offer. Models
            // supplied by a compatibility default or template remain
            // provider-owned strings: an incomplete catalogue must not turn
            // the listing into an upstream allowlist.
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
        None
    };
    CandidateEvidence {
        candidate_id: candidate.id().clone(),
        crossed_from: None,
        binding_id: candidate.binding_id().clone(),
        model: model.map(str::to_owned),
        result: reason.map_or(Ok(()), Err),
    }
}

fn target_has_open_circuit(
    circuits: &CircuitSnapshot,
    route_id: &RouteId,
    candidate_id: &CandidateId,
    binding_id: &BindingId,
    model_id: &str,
) -> bool {
    circuits.iter().any(|circuit| {
        circuit.route_id == *route_id
            && circuit.candidate_id == *candidate_id
            && circuit.binding_id == *binding_id
            && circuit.model_id == model_id
    })
}

fn exact_evidence(
    candidate_id: &CandidateId,
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
        None
    };
    CandidateEvidence {
        candidate_id: candidate_id.clone(),
        crossed_from: None,
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
    if (required.sampling && actual.sampling.is_unsupported())
        || (required.system_in_messages && actual.system_in_messages.is_unsupported())
        || (required.thinking_replay && actual.thinking_replay.is_unsupported())
        || (required.images && actual.images.is_unsupported())
        || (required.tools && actual.tools.is_unsupported())
        || (required.tool_choice_none && actual.tool_choice_none.is_unsupported())
        || (required.tool_choice_named && actual.tool_choice_named.is_unsupported())
        || (required.tool_choice_unnamed && actual.tool_choice_unnamed.is_unsupported())
        || (required.tool_choice_other && actual.tool_choice_other.is_unsupported())
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
    None
}

fn post_send_edge_denial(
    edge: &CandidateEdge,
    from: Option<&BindingAvailability>,
    to: Option<&BindingAvailability>,
) -> Option<SkipReason> {
    if matches!(
        (from.map(|v| v.cost_class), to.map(|v| v.cost_class)),
        (
            Some(CostClass::Subscription),
            Some(CostClass::Billed | CostClass::Unknown)
        )
    ) && edge.billed_boundary == BoundaryConsent::Deny
    {
        Some(SkipReason::BilledBoundaryDenied)
    } else {
        None
    }
}

fn cache_edge_denial(
    edges: &[CandidateEdge],
    binding_id: &BindingId,
    effective_model: &str,
    lane: Option<&LaneFacts>,
) -> Option<SkipReason> {
    let lane = lane?;
    if lane.temperature == super::CacheTemperature::Cold {
        return None;
    }
    let cache_changes = lane
        .pinned
        .as_ref()
        .is_none_or(|pin| pin.binding_id != *binding_id || pin.effective_model != effective_model);
    if cache_changes
        && edges
            .iter()
            .any(|edge| edge.cache_transition != CacheTransitionPolicy::AllowCacheLoss)
    {
        Some(SkipReason::WarmCacheTransitionDenied)
    } else {
        None
    }
}

fn stricter_cache_transition(
    left: CacheTransitionPolicy,
    right: CacheTransitionPolicy,
) -> CacheTransitionPolicy {
    use CacheTransitionPolicy::{AllowCacheLoss, AllowWhenCold, PreserveWarm};
    match (left, right) {
        (PreserveWarm, _) | (_, PreserveWarm) => PreserveWarm,
        (AllowWhenCold, _) | (_, AllowWhenCold) => AllowWhenCold,
        (AllowCacheLoss, AllowCacheLoss) => AllowCacheLoss,
    }
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

fn merge_capabilities(left: CapabilityFlags, right: CapabilityFlags) -> CapabilityFlags {
    CapabilityFlags {
        sampling: left.sampling || right.sampling,
        system_in_messages: left.system_in_messages || right.system_in_messages,
        thinking_replay: left.thinking_replay || right.thinking_replay,
        images: left.images || right.images,
        tools: left.tools || right.tools,
        tool_choice_none: left.tool_choice_none || right.tool_choice_none,
        tool_choice_named: left.tool_choice_named || right.tool_choice_named,
        tool_choice_unnamed: left.tool_choice_unnamed || right.tool_choice_unnamed,
        tool_choice_other: left.tool_choice_other || right.tool_choice_other,
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
        && lane.temperature != super::CacheTemperature::Cold
        && !facts
            .legacy_compaction
            .is_some_and(|compaction| compaction.lane_cold)
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
        |lane| lane.temperature == super::CacheTemperature::Cold,
    );
    if !eligible {
        return Ok(None);
    }

    Ok(elect_upgrade(
        upgrade,
        classifier,
        inventory,
        learning,
        candidate.binding_id(),
        baseline,
        facts.prompt_bound,
    )?
    .winner)
}

/// Why a model in an upgrade policy's inventory did not win the election.
/// A model the classifier does not parse is not a contender and is not
/// listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeRejection {
    FamilyMismatch,
    NotNewer,
    LearningMinimum,
    ContextFit,
}

/// The newest model that may replace a baseline, with the contenders that
/// lost and why.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpgradeElection {
    pub winner: Option<String>,
    pub rejected: Vec<(String, UpgradeRejection)>,
}

/// The election as a new lane with no prompt bound would see it for a
/// candidate whose baseline is `baseline`. `None` when the candidate has no
/// enabled upgrade policy. This is what the routing page shows; the resolver
/// elects through the same function.
pub fn upgrade_standing<L: LearningView>(
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
    learning: &L,
    candidate: &CompiledCandidate,
    baseline: &str,
) -> Result<Option<UpgradeElection>, ResolveFailure> {
    let Some(upgrade) = candidate
        .upgrade_policy_id()
        .and_then(|id| policy.upgrade(id))
        .map(|upgrade| upgrade.policy())
        .filter(|upgrade| upgrade.enabled)
    else {
        return Ok(None);
    };
    let classifier = policy
        .classifier(&upgrade.classifier_id)
        .ok_or(ResolveFailure::InvalidTemplate)?;
    elect_upgrade(
        upgrade,
        classifier,
        inventory,
        learning,
        candidate.binding_id(),
        baseline,
        0,
    )
    .map(Some)
}

fn elect_upgrade<L: LearningView>(
    upgrade: &UpgradePolicy,
    classifier: &CompiledClassifier,
    inventory: &OfferInventory,
    learning: &L,
    binding: &BindingId,
    baseline: &str,
    prompt_bound: u64,
) -> Result<UpgradeElection, ResolveFailure> {
    let Some(baseline_class) = classifier
        .classify(baseline)
        .map_err(|_| ResolveFailure::InvalidTemplate)?
    else {
        return Ok(UpgradeElection::default());
    };
    let mut models = BTreeSet::new();
    for source in &upgrade.inventory_sources {
        match source {
            InventorySource::Catalogue => {
                models.extend(
                    inventory
                        .iter()
                        .filter(|offer| offer.binding_id == *binding)
                        .map(|offer| offer.backend_model_id.clone()),
                );
            }
            InventorySource::Served | InventorySource::Learned => {
                models.extend(
                    learning
                        .known_models(binding)
                        .map_err(|_| ResolveFailure::LearningUnavailable)?,
                );
            }
        }
    }
    let mut best: Option<(String, ParsedVersion)> = None;
    let mut rejected = Vec::new();
    for model in models {
        let Some(classified) = classifier
            .classify(&model)
            .map_err(|_| ResolveFailure::InvalidTemplate)?
        else {
            continue;
        };
        if classified.family != baseline_class.family {
            rejected.push((model, UpgradeRejection::FamilyMismatch));
            continue;
        }
        if classified
            .version
            .partial_cmp(&baseline_class.version)
            .is_none_or(|order| !order.is_gt())
        {
            rejected.push((model, UpgradeRejection::NotNewer));
            continue;
        }
        if !learning_minimum(
            upgrade.minimum_learning,
            upgrade.allow_manual_promotion,
            learning,
            binding,
            &model,
        )? {
            rejected.push((model, UpgradeRejection::LearningMinimum));
            continue;
        }
        if !fits_context(inventory, learning, binding, &model, prompt_bound)? {
            rejected.push((model, UpgradeRejection::ContextFit));
            continue;
        }
        let replace = best.as_ref().is_none_or(|(_, current)| {
            classified
                .version
                .partial_cmp(current)
                .is_some_and(|order| order.is_gt())
        });
        if replace {
            if let Some((beaten, _)) = best.replace((model, classified.version)) {
                rejected.push((beaten, UpgradeRejection::NotNewer));
            }
        } else {
            rejected.push((model, UpgradeRejection::NotNewer));
        }
    }
    Ok(UpgradeElection {
        winner: best.map(|(model, _)| model),
        rejected,
    })
}

fn choose_revision_one_compaction<L: LearningView>(
    policy: &CompiledPolicy,
    inventory: &OfferInventory,
    learning: &L,
    facts: &RequestFacts,
    candidate: &CompiledCandidate,
    parameters: &BTreeMap<StableId, String>,
    source: &str,
) -> Result<Option<String>, ResolveFailure> {
    let Some(compaction_facts) = facts
        .legacy_compaction
        .filter(|facts| facts.lane_cold && facts.transform_applied)
    else {
        return Ok(None);
    };
    let Some((spec, eligible_bindings)) = policy
        .compaction()
        .and_then(CompiledCompaction::revision_one_legacy)
    else {
        return Ok(None);
    };
    if facts.class != RequestClass::Compaction
        || !eligible_bindings.contains(candidate.binding_id())
    {
        return Ok(None);
    }

    // The binding's generated classifier splits the ids, so compaction reads
    // the same family and version the upgrade election does.
    let Some(classifier) = policy.family_split_for(candidate.binding_id()) else {
        return Ok(None);
    };
    let target = match classifier.family_split(spec) {
        Some(family) if family.version.is_empty() => {
            let mut best: Option<(String, super::family::Family)> = None;
            let Ok(models) = learning.known_models(candidate.binding_id()) else {
                return Ok(None);
            };
            for model in models {
                let Some(member) = classifier
                    .family_split(&model)
                    .filter(|member| member.name == family.name)
                else {
                    continue;
                };
                if !learning_minimum(
                    MinimumLearning::RevisionOneAdaptive,
                    false,
                    learning,
                    candidate.binding_id(),
                    &model,
                )
                .unwrap_or(false)
                {
                    continue;
                }
                let mapped =
                    apply_alias(candidate, &model, parameters)?.unwrap_or_else(|| model.clone());
                if crate::catalog::pricing::price(&mapped, false, None).is_none() {
                    continue;
                }
                if best
                    .as_ref()
                    .is_none_or(|(_, current)| member.is_newer_than(current))
                {
                    best = Some((model, member));
                }
            }
            best.map(|(model, _)| model)
        }
        Some(_) => crate::catalog::model_identity(spec),
        None => None,
    };
    let Some(target) = target else {
        return Ok(None);
    };
    let mapped_target =
        apply_alias(candidate, &target, parameters)?.unwrap_or_else(|| target.clone());
    let fits = if let Some(window) = inventory
        .get(candidate.binding_id(), &mapped_target)
        .and_then(|offer| offer.context_window)
    {
        compaction_facts.prompt_tokens <= window
    } else {
        learning
            .learned(candidate.binding_id(), &target)
            .ok()
            .flatten()
            .and_then(|evidence| evidence.max_prompt())
            .is_some_and(|max| compaction_facts.prompt_tokens <= max)
    };
    if !fits {
        return Ok(None);
    }

    let mapped_source =
        apply_alias(candidate, source, parameters)?.unwrap_or_else(|| source.to_owned());
    let target_price =
        crate::catalog::pricing::price(&mapped_target, false, None).map(|price| price.rates.input);
    let source_price =
        crate::catalog::pricing::price(&mapped_source, false, None).map(|price| price.rates.input);
    Ok(
        matches!((target_price, source_price), (Some(target), Some(source)) if target < source)
            .then_some(target),
    )
}

fn learning_minimum<L: LearningView>(
    minimum: MinimumLearning,
    allow_manual_promotion: bool,
    learning: &L,
    binding: &BindingId,
    model: &str,
) -> Result<bool, ResolveFailure> {
    if minimum == MinimumLearning::None {
        return Ok(true);
    }
    let evidence = learning
        .learned(binding, model)
        .map_err(|_| ResolveFailure::LearningUnavailable)?;
    if allow_manual_promotion
        && evidence
            .as_ref()
            .is_some_and(|evidence| evidence.explicitly_promoted())
    {
        return Ok(true);
    }
    match minimum {
        MinimumLearning::None => Ok(true),
        MinimumLearning::DistinctDays { days } => Ok(
            evidence.is_some_and(|evidence| evidence.distinct_days().len() >= usize::from(days))
        ),
        MinimumLearning::ExplicitPromotion => {
            Ok(evidence.is_some_and(|evidence| evidence.explicitly_promoted()))
        }
        MinimumLearning::RevisionOneAdaptive => {
            let active_days = learning
                .active_days(binding)
                .map_err(|_| ResolveFailure::LearningUnavailable)?;
            let needed = super::learning::requirement(active_days);
            Ok(evidence.is_some_and(|evidence| evidence.distinct_days().len() as f64 > needed))
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
        .and_then(|evidence| evidence.max_prompt())
        .is_some_and(|max| prompt <= max))
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
            .map(|replacement| (replacement != model).then_some(replacement))
            .map_err(|_| ResolveFailure::InvalidTemplate);
    }
    Ok(None)
}

/// Empty learning input for publication and tests that require no learning.
#[derive(Debug, Default)]
pub struct NoLearning;

impl LearningView for NoLearning {
    type Error = Infallible;

    fn snapshot(&self) -> super::LearningSnapshot {
        super::LearningSnapshot::default()
    }

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
    use std::convert::Infallible;
    use std::sync::Arc;

    use super::*;
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
            honours_server_safeguards: false,
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
        compile_origin(document, inventory, PolicyOrigin::Authored)
    }

    fn compile_origin(
        document: PolicyDocument,
        inventory: &OfferInventory,
        origin: PolicyOrigin,
    ) -> CompiledPolicy {
        let topology =
            RouteRegistry::new(std::iter::empty::<Arc<dyn crate::providers::Provider>>());
        crate::policy::compile(document.validate().unwrap(), &topology, inventory, origin)
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
            legacy_hyphenated: false,
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
                    capabilities: Capabilities::ANTHROPIC_MESSAGES,
                    safeguards_marked: false,
                    cost_class: CostClass::Subscription,
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
                    capabilities: Capabilities::ANTHROPIC_MESSAGES,
                    safeguards_marked: false,
                    cost_class: CostClass::Billed,
                },
            ),
        ]))
    }

    #[derive(Default)]
    struct TestLearning {
        models: BTreeMap<String, LearnedEvidence>,
        active_days: usize,
    }

    impl LearningView for TestLearning {
        type Error = Infallible;

        fn snapshot(&self) -> LearningSnapshot {
            LearningSnapshot::default()
        }

        fn recently_sent(&self, _: &BindingId, _: &str) -> Result<RecencyEvidence, Self::Error> {
            Ok(RecencyEvidence::Unknown)
        }

        fn learned(
            &self,
            _: &BindingId,
            model: &str,
        ) -> Result<Option<LearnedEvidence>, Self::Error> {
            Ok(self.models.get(model).cloned())
        }

        fn known_models(&self, _: &BindingId) -> Result<Vec<String>, Self::Error> {
            Ok(self.models.keys().cloned().collect())
        }

        fn active_days(&self, _: &BindingId) -> Result<usize, Self::Error> {
            Ok(self.active_days)
        }
    }

    fn learned(days: usize, max_prompt: u64) -> LearnedEvidence {
        LearnedEvidence::new(
            (0..days).map(|day| format!("2026-10-{day:02}")).collect(),
            Some(max_prompt),
            false,
        )
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
            asks_server_safeguards: false,
            binding_affinity: None,
            class: RequestClass::Inference,
            legacy_compaction: None,
            accounts: BTreeMap::new(),
            now_ms: 100,
        }
    }

    fn lane(
        policy: &CompiledPolicy,
        candidate_id: &str,
        binding_id: &BindingId,
        effective_model: &str,
        temperature: CacheTemperature,
    ) -> LaneFacts {
        let route = policy.route(&id("route")).unwrap();
        LaneFacts {
            temperature,
            pinned: Some(ProvisionalPin {
                policy_revision_id: Some(1),
                policy_fingerprint: Some(policy.fingerprint().clone()),
                route_fingerprint: route.fingerprint().clone(),
                candidate_id: id(candidate_id),
                binding_id: binding_id.clone(),
                baseline_model: effective_model.to_owned(),
                effective_model: effective_model.to_owned(),
            }),
            forced_from: None,
            forced_to: None,
        }
    }

    fn open_circuit(
        route_id: &RouteId,
        candidate_id: &CandidateId,
        binding_id: &BindingId,
        model_id: &str,
    ) -> OpenCircuitTarget {
        OpenCircuitTarget {
            route_id: route_id.clone(),
            candidate_id: candidate_id.clone(),
            binding_id: binding_id.clone(),
            model_id: model_id.to_owned(),
            failure: FailureClass::Overload,
        }
    }

    #[test]
    fn persisted_revision_becomes_attempt_provenance() {
        let inventory = OfferInventory::default();
        let binding = ANTHROPIC_SUB_MESSAGES.clone();
        let policy = compile(
            document(route(
                candidate("first", &binding, CandidateModel::RequestedModel),
                vec![],
            )),
            &inventory,
        );
        let plan = resolve(
            &policy,
            &inventory,
            &NoLearning,
            None,
            &availability(true, false),
            &facts("claude-opus-5"),
        )
        .unwrap()
        .with_policy_revision(7);
        let stored = crate::store::PolicyRouteProvenance::from_attempt_plan(&plan)
            .unwrap()
            .expect("authoritative provenance");
        assert_eq!(stored.policy_revision_id, 7);
        assert_eq!(stored.route.as_ref().unwrap().route_id, id("route"));
        assert_eq!(
            stored.outcome,
            crate::store::PolicyRoutingOutcome::Attempt {
                candidate_id: id("first"),
                baseline_model: "claude-opus-5".to_owned(),
            }
        );
        assert_eq!(stored.trace.events.len(), plan.trace.len());
    }

    fn standing_inventory() -> OfferInventory {
        let offer = |model: &str, window: Option<u64>| BackendOffer {
            binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
            provider: "anthropic_sub".to_owned(),
            backend_model_id: model.to_owned(),
            binding: ANTHROPIC_SUB_MESSAGES.clone(),
            context_window: window,
            chronology: None,
            fetched_at_ms: 1,
            raw: serde_json::Value::Null,
        };
        OfferInventory::from_offers([
            offer("claude-opus-4-1", Some(1_000_000)),
            offer("claude-opus-4-8", Some(1_000_000)),
            offer("claude-opus-4-9", Some(1_000_000)),
            offer("claude-opus-5", None),
            offer("claude-sonnet-9", Some(1_000_000)),
        ])
    }

    /// The routing page shows why each rival lost, and the winner it names is
    /// the one the resolver would elect for a new lane.
    #[test]
    fn the_upgrade_standing_says_why_each_rival_lost() {
        let inventory = standing_inventory();
        let policy = compile(upgrade_document(), &inventory);
        let route = policy.route(&id("route")).unwrap();

        let election = upgrade_standing(
            &policy,
            &inventory,
            &NoLearning,
            &route.candidates()[0],
            "claude-opus-4-8",
        )
        .unwrap()
        .expect("an enabled upgrade policy");

        assert_eq!(election.winner.as_deref(), Some("claude-opus-4-9"));
        let reason = |model: &str| {
            election
                .rejected
                .iter()
                .find(|(rejected, _)| rejected == model)
                .map(|(_, reason)| *reason)
        };
        assert_eq!(
            reason("claude-sonnet-9"),
            Some(UpgradeRejection::FamilyMismatch)
        );
        assert_eq!(reason("claude-opus-4-1"), Some(UpgradeRejection::NotNewer));
        assert_eq!(reason("claude-opus-4-8"), Some(UpgradeRejection::NotNewer));
        // The newest model has no window the election can trust.
        assert_eq!(reason("claude-opus-5"), Some(UpgradeRejection::ContextFit));
        assert_eq!(reason("claude-opus-4-9"), None);
    }

    #[test]
    fn the_upgrade_standing_names_the_learning_minimum() {
        let inventory = standing_inventory();
        let mut document = upgrade_document();
        document.upgrades[0].minimum_learning = MinimumLearning::ExplicitPromotion;
        let policy = compile(document, &inventory);
        let route = policy.route(&id("route")).unwrap();

        let election = upgrade_standing(
            &policy,
            &inventory,
            &NoLearning,
            &route.candidates()[0],
            "claude-opus-4-8",
        )
        .unwrap()
        .unwrap();

        assert_eq!(election.winner, None);
        assert!(election.rejected.contains(&(
            "claude-opus-4-9".to_owned(),
            UpgradeRejection::LearningMinimum
        )));
    }

    #[test]
    fn a_disabled_upgrade_policy_has_no_standing() {
        let inventory = standing_inventory();
        let mut document = upgrade_document();
        document.upgrades[0].enabled = false;
        let policy = compile(document, &inventory);
        let route = policy.route(&id("route")).unwrap();

        assert_eq!(
            upgrade_standing(
                &policy,
                &inventory,
                &NoLearning,
                &route.candidates()[0],
                "claude-opus-4-8",
            )
            .unwrap(),
            None
        );
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
    fn legacy_cold_compaction_suppresses_sticky_upgrade_with_unknown_temperature() {
        let inventory = upgrade_inventory();
        let policy = compile(upgrade_document(), &inventory);
        let mut request = facts("claude-opus-4-8");
        request.class = RequestClass::Compaction;
        request.legacy_compaction = Some(LegacyCompactionFacts {
            lane_cold: true,
            prompt_tokens: 100,
            transform_applied: true,
        });
        let lane = LaneFacts {
            temperature: CacheTemperature::Unknown,
            pinned: None,
            forced_from: Some("claude-opus-4-8".to_owned()),
            forced_to: Some("claude-opus-5".to_owned()),
        };

        let plan = resolve(
            &policy,
            &inventory,
            &NoLearning,
            Some(&lane),
            &availability(true, false),
            &request,
        )
        .unwrap();
        assert_eq!(plan.upgraded_model, None);
        assert_eq!(plan.effective_model, "claude-opus-4-8");
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
    fn manual_promotion_satisfies_a_distinct_day_minimum_only_when_allowed() {
        let binding = ANTHROPIC_SUB_MESSAGES.id().clone();
        let store = Arc::new(Store::open(":memory:").unwrap());
        store
            .merge_binding_learning(&crate::store::BindingLearningUpdate {
                binding_id: binding.clone(),
                model_id: "claude-opus-5".to_owned(),
                days: Default::default(),
                max_prompt: Some(100),
                explicitly_promoted: true,
                updated_ms: 1,
            })
            .unwrap();
        let learning = BindingLearningView::new(store);

        assert!(
            learning_minimum(
                MinimumLearning::DistinctDays { days: 2 },
                true,
                &learning,
                &binding,
                "claude-opus-5",
            )
            .unwrap()
        );
        assert!(
            !learning_minimum(
                MinimumLearning::DistinctDays { days: 2 },
                false,
                &learning,
                &binding,
                "claude-opus-5",
            )
            .unwrap()
        );
    }

    #[test]
    fn context_fit_prefers_inventory_then_empirical_maximum() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        store
            .merge_binding_learning(&crate::store::BindingLearningUpdate {
                binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
                model_id: "model".to_owned(),
                days: Default::default(),
                max_prompt: Some(300),
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
                350,
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
    fn post_send_failure_continues_from_the_selected_candidate() {
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
        let edge = CandidateEdge {
            preflight: FallbackPermission::Disabled,
            post_send: vec![FailureClass::Overload],
            billed_boundary: BoundaryConsent::Allow,
            ambiguous_transport: BoundaryConsent::Deny,
            cache_transition: CacheTransitionPolicy::AllowCacheLoss,
        };
        let policy = compile(
            document(route(
                first,
                vec![RouteStep {
                    edge,
                    candidate: second,
                }],
            )),
            &OfferInventory::default(),
        );
        let availability = availability(true, false);
        let request = facts("asked");
        let first = resolve_with_circuits(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability,
            &CircuitSnapshot::default(),
            &request,
        )
        .unwrap();

        let second = resolve_after_failure(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            &availability,
            &CircuitSnapshot::default(),
            &first,
            FailureClass::Overload,
        )
        .unwrap()
        .expect("the edge authorizes overload fallback");

        assert_eq!(second.candidate_id, Some(id("second")));
        assert_eq!(second.binding_id, *ANTHROPIC_API_MESSAGES.id());
        assert_eq!(second.baseline_model, "two");
        assert_eq!(
            second.fallback_cache_transition,
            Some(CacheTransitionPolicy::AllowCacheLoss)
        );
        assert!(matches!(
            second.trace.last(),
            Some(TraceEvent::Candidate(CandidateEvidence {
                candidate_id,
                crossed_from: Some(crossed_from),
                result: Ok(()),
                ..
            })) if candidate_id.as_str() == "second" && crossed_from.as_str() == "first"
        ));
    }

    #[test]
    fn post_send_continuation_uses_the_captured_route_and_inputs() {
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
            CandidateModel::RequestedModel,
        );
        let edge = CandidateEdge {
            preflight: FallbackPermission::Enabled,
            post_send: vec![FailureClass::Overload],
            billed_boundary: BoundaryConsent::Allow,
            ambiguous_transport: BoundaryConsent::Deny,
            cache_transition: CacheTransitionPolicy::AllowCacheLoss,
        };
        let policy = compile(
            document(route(
                first,
                vec![RouteStep {
                    edge,
                    candidate: second,
                }],
            )),
            &OfferInventory::default(),
        );
        let availability = availability(true, false);
        let request = facts("captured");
        let mut first = resolve_with_circuits(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability,
            &CircuitSnapshot::default(),
            &request,
        )
        .unwrap();
        first.route_id = Some(id("invented-public-view"));
        first.candidate_id = Some(id("invented-public-view"));
        first.trace.clear();

        let second = resolve_after_failure(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            &availability,
            &CircuitSnapshot::default(),
            &first,
            FailureClass::Overload,
        )
        .unwrap()
        .expect("captured continuation remains authoritative");

        assert_eq!(second.route_id, Some(id("route")));
        assert_eq!(second.candidate_id, Some(id("second")));
        assert_eq!(second.baseline_model, "captured");
        assert!(matches!(second.trace.first(), Some(TraceEvent::Lookup(_))));
    }

    #[test]
    fn exact_presentations_have_no_post_send_continuation() {
        let mut doc = document(route(
            candidate(
                "unused",
                &ANTHROPIC_SUB_MESSAGES,
                CandidateModel::RequestedModel,
            ),
            vec![],
        ));
        doc.presentations.push(Presentation {
            id: id("exact"),
            scope: scope(Some("claude")),
            slug: "friendly".to_owned(),
            label: None,
            description: None,
            enabled: true,
            shadow_native: false,
            source: PresentationSource::ExactOffer {
                binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
                model_id: "exact-model".to_owned(),
            },
            metadata: None,
        });
        let policy = compile(doc, &OfferInventory::default());
        let availability = availability(true, false);
        let plan = resolve_with_circuits(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability,
            &CircuitSnapshot::default(),
            &facts("friendly"),
        )
        .unwrap();

        assert!(
            resolve_after_failure(
                &policy,
                &OfferInventory::default(),
                &NoLearning,
                &availability,
                &CircuitSnapshot::default(),
                &plan,
                FailureClass::Overload,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn post_send_failure_stops_when_the_outgoing_edge_does_not_list_it() {
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
        let edge = CandidateEdge {
            preflight: FallbackPermission::Enabled,
            post_send: vec![FailureClass::RateLimit],
            billed_boundary: BoundaryConsent::Allow,
            ambiguous_transport: BoundaryConsent::Deny,
            cache_transition: CacheTransitionPolicy::AllowCacheLoss,
        };
        let policy = compile(
            document(route(
                first,
                vec![RouteStep {
                    edge,
                    candidate: second,
                }],
            )),
            &OfferInventory::default(),
        );
        let availability = availability(true, false);
        let request = facts("asked");
        let first = resolve_with_circuits(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability,
            &CircuitSnapshot::default(),
            &request,
        )
        .unwrap();

        assert!(
            resolve_after_failure(
                &policy,
                &OfferInventory::default(),
                &NoLearning,
                &availability,
                &CircuitSnapshot::default(),
                &first,
                FailureClass::Overload,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn an_open_circuit_skips_only_its_exact_candidate_target() {
        let first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "one".to_owned(),
            },
        );
        let second = candidate(
            "second",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "two".to_owned(),
            },
        );
        let edge = CandidateEdge {
            preflight: FallbackPermission::Enabled,
            post_send: vec![],
            billed_boundary: BoundaryConsent::Deny,
            ambiguous_transport: BoundaryConsent::Deny,
            cache_transition: CacheTransitionPolicy::AllowCacheLoss,
        };
        let policy = compile(
            document(route(
                first,
                vec![RouteStep {
                    edge,
                    candidate: second,
                }],
            )),
            &OfferInventory::default(),
        );
        let route = policy.route(&id("route")).unwrap();
        let first = &route.candidates()[0];
        let circuit = open_circuit(route.id(), first.id(), first.binding_id(), "one");
        let selected = resolve_with_circuits(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &CircuitSnapshot::from([circuit.clone()]),
            &facts("asked"),
        )
        .unwrap();
        assert_eq!(selected.candidate_id, Some(id("second")));
        assert!(matches!(
            &selected.trace[1],
            TraceEvent::Candidate(evidence)
                if evidence.candidate_id == *first.id()
                    && evidence.model.as_deref() == Some("one")
                    && evidence.result == Err(SkipReason::HealthCircuitOpen)
        ));

        let near_misses = [
            OpenCircuitTarget {
                route_id: id("other-route"),
                ..circuit.clone()
            },
            OpenCircuitTarget {
                candidate_id: id("other-candidate"),
                ..circuit.clone()
            },
            OpenCircuitTarget {
                binding_id: ANTHROPIC_API_MESSAGES.id().clone(),
                ..circuit.clone()
            },
            OpenCircuitTarget {
                model_id: "other-model".to_owned(),
                ..circuit
            },
        ];
        for near_miss in near_misses {
            let selected = resolve_with_circuits(
                &policy,
                &OfferInventory::default(),
                &NoLearning,
                None,
                &availability(true, false),
                &CircuitSnapshot::from([near_miss]),
                &facts("asked"),
            )
            .unwrap();
            assert_eq!(selected.candidate_id, Some(id("first")));
        }
    }

    #[test]
    fn open_circuits_match_the_concrete_upgraded_and_aliased_models() {
        let inventory = upgrade_inventory();
        let policy = compile(upgrade_document(), &inventory);
        let compiled_route = policy.route(&id("route")).unwrap();
        let compiled_candidate = &compiled_route.candidates()[0];
        let failure = resolve_with_circuits(
            &policy,
            &inventory,
            &NoLearning,
            None,
            &availability(true, false),
            &CircuitSnapshot::from([open_circuit(
                compiled_route.id(),
                compiled_candidate.id(),
                compiled_candidate.binding_id(),
                "claude-opus-5",
            )]),
            &facts("claude-opus-4-8"),
        )
        .unwrap_err();
        let ResolveFailure::RouteUnavailable { trace, .. } = failure else {
            panic!()
        };
        assert!(matches!(
            &trace[1],
            TraceEvent::Candidate(evidence)
                if evidence.model.as_deref() == Some("claude-opus-5")
                    && evidence.result == Err(SkipReason::HealthCircuitOpen)
        ));
        assert!(matches!(&trace[2], TraceEvent::Upgrade { to, .. } if to == "claude-opus-5"));

        let mut aliased = candidate(
            "aliased",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::RequestedModel,
        );
        aliased.alias_policy = Some(AliasPolicy {
            rules: vec![AliasRule {
                matcher: MatcherSpec::Exact("source".to_owned()),
                exclude: vec![],
                replacement: "destination".to_owned(),
            }],
        });
        let policy = compile(document(route(aliased, vec![])), &OfferInventory::default());
        let route = policy.route(&id("route")).unwrap();
        let candidate = &route.candidates()[0];
        let failure = resolve_with_circuits(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &CircuitSnapshot::from([open_circuit(
                route.id(),
                candidate.id(),
                candidate.binding_id(),
                "destination",
            )]),
            &facts("source"),
        )
        .unwrap_err();
        let ResolveFailure::RouteUnavailable { trace, .. } = failure else {
            panic!()
        };
        assert!(matches!(
            &trace[1],
            TraceEvent::Candidate(evidence)
                if evidence.model.as_deref() == Some("destination")
                    && evidence.result == Err(SkipReason::HealthCircuitOpen)
        ));
        assert!(matches!(&trace[2], TraceEvent::Alias { to, .. } if to == "destination"));
    }

    #[test]
    fn exact_presentations_observe_the_same_circuit_key() {
        let inventory = OfferInventory::from_offers([BackendOffer {
            binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
            provider: "anthropic_sub".to_owned(),
            backend_model_id: "exact-model".to_owned(),
            binding: ANTHROPIC_SUB_MESSAGES.clone(),
            context_window: None,
            chronology: None,
            fetched_at_ms: 1,
            raw: serde_json::Value::Null,
        }]);
        let mut document = document(route(
            candidate(
                "unused",
                &ANTHROPIC_SUB_MESSAGES,
                CandidateModel::RequestedModel,
            ),
            vec![],
        ));
        document.presentations.push(Presentation {
            id: id("exact"),
            scope: scope(Some("claude")),
            slug: "friendly".to_owned(),
            label: None,
            description: None,
            enabled: true,
            shadow_native: false,
            source: PresentationSource::ExactOffer {
                binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
                model_id: "exact-model".to_owned(),
            },
            metadata: None,
        });
        let policy = compile(document, &inventory);
        let presentation = policy
            .presentation(&scope(Some("claude")), "friendly")
            .unwrap();
        let PresentationRoute::Exact {
            route_id,
            candidate_id,
            binding_id,
            model_id,
            ..
        } = presentation.route()
        else {
            panic!()
        };
        let failure = resolve_with_circuits(
            &policy,
            &inventory,
            &NoLearning,
            None,
            &availability(true, false),
            &CircuitSnapshot::from([open_circuit(route_id, candidate_id, binding_id, model_id)]),
            &facts("friendly"),
        )
        .unwrap_err();
        assert!(matches!(
            failure,
            ResolveFailure::RouteUnavailable { trace, .. }
                if matches!(&trace[1], TraceEvent::Candidate(evidence)
                    if evidence.candidate_id == *candidate_id
                        && evidence.result == Err(SkipReason::HealthCircuitOpen))
        ));
    }

    #[test]
    fn missing_binding_evidence_remains_unknown_and_does_not_fallback() {
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
        let policy = compile(
            document(route(
                first,
                vec![RouteStep {
                    edge: CandidateEdge {
                        preflight: FallbackPermission::Enabled,
                        post_send: vec![],
                        billed_boundary: BoundaryConsent::Allow,
                        ambiguous_transport: BoundaryConsent::Deny,
                        cache_transition: CacheTransitionPolicy::AllowCacheLoss,
                    },
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
            &AvailabilitySnapshot::default(),
            &facts("asked"),
        )
        .unwrap();
        assert_eq!(selected.candidate_id, Some(id("first")));
        assert!(matches!(
            &selected.trace[1],
            TraceEvent::Candidate(evidence) if evidence.result.is_ok()
        ));
    }

    #[test]
    fn disabled_candidate_is_positive_preflight_evidence() {
        let mut disabled = candidate(
            "disabled",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "disabled".to_owned(),
            },
        );
        disabled.enabled = false;
        let fallback = candidate(
            "fallback",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "fallback".to_owned(),
            },
        );
        let edge = CandidateEdge {
            preflight: FallbackPermission::Enabled,
            post_send: vec![],
            billed_boundary: BoundaryConsent::Deny,
            ambiguous_transport: BoundaryConsent::Deny,
            cache_transition: CacheTransitionPolicy::AllowCacheLoss,
        };
        let policy = compile(
            document(route(
                disabled.clone(),
                vec![RouteStep {
                    edge,
                    candidate: fallback,
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
            &facts("asked"),
        )
        .unwrap();
        assert_eq!(selected.candidate_id, Some(id("fallback")));
        assert_eq!(
            selected.fallback_cache_transition,
            Some(CacheTransitionPolicy::AllowCacheLoss)
        );
        assert!(matches!(
            &selected.trace[1],
            TraceEvent::Candidate(evidence)
                if evidence.crossed_from.is_none()
                    && evidence.result == Err(SkipReason::Disabled)
        ));
        assert!(matches!(
            &selected.trace[2],
            TraceEvent::Candidate(evidence)
                if evidence.crossed_from.as_ref().is_some_and(|id| id.as_str() == "disabled")
                    && evidence.result.is_ok()
        ));

        let policy = compile(
            document(route(disabled, vec![])),
            &OfferInventory::default(),
        );
        let failure = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &facts("asked"),
        )
        .unwrap_err();
        assert!(matches!(
            failure,
            ResolveFailure::RouteUnavailable { trace, .. }
                if matches!(&trace[1], TraceEvent::Candidate(evidence)
                    if evidence.result == Err(SkipReason::Disabled))
        ));
    }

    #[test]
    fn unresolved_selector_does_not_authorize_preflight_fallback() {
        let first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::OfferSelector {
                selector: CandidateSelector {
                    include: vec![],
                    exclude: vec![],
                    ordering: SelectorOrdering::Explicit { model_ids: vec![] },
                },
            },
        );
        let second = candidate(
            "second",
            &ANTHROPIC_API_MESSAGES,
            CandidateModel::Exact {
                model_id: "fallback".to_owned(),
            },
        );
        let policy = compile(
            document(route(
                first,
                vec![RouteStep {
                    edge: CandidateEdge {
                        preflight: FallbackPermission::Enabled,
                        post_send: vec![],
                        billed_boundary: BoundaryConsent::Allow,
                        ambiguous_transport: BoundaryConsent::Deny,
                        cache_transition: CacheTransitionPolicy::AllowCacheLoss,
                    },
                    candidate: second,
                }],
            )),
            &OfferInventory::default(),
        );
        let failure = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &facts("asked"),
        )
        .unwrap_err();
        let ResolveFailure::RouteUnavailable { trace, .. } = failure else {
            panic!()
        };
        assert_eq!(trace.len(), 2, "the fallback candidate is never considered");
        assert!(matches!(
            &trace[1],
            TraceEvent::Candidate(evidence)
                if evidence.result == Err(SkipReason::SelectorUnresolved)
        ));
    }

    #[test]
    fn fallback_cache_consent_uses_the_final_binding_and_effective_model() {
        let mut first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "one".to_owned(),
            },
        );
        first.enabled = false;
        let second = candidate(
            "second",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "two".to_owned(),
            },
        );
        let policy = compile(
            document(route(
                first,
                vec![RouteStep {
                    edge: CandidateEdge {
                        preflight: FallbackPermission::Enabled,
                        post_send: vec![],
                        billed_boundary: BoundaryConsent::Deny,
                        ambiguous_transport: BoundaryConsent::Deny,
                        cache_transition: CacheTransitionPolicy::PreserveWarm,
                    },
                    candidate: second,
                }],
            )),
            &OfferInventory::default(),
        );
        let warm = lane(
            &policy,
            "first",
            ANTHROPIC_SUB_MESSAGES.id(),
            "one",
            CacheTemperature::Warm,
        );
        let failure = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            Some(&warm),
            &availability(true, false),
            &facts("asked"),
        )
        .unwrap_err();
        assert!(matches!(
            failure,
            ResolveFailure::RouteUnavailable { trace, .. }
                if matches!(&trace[2], TraceEvent::Candidate(evidence)
                    if evidence.model.as_deref() == Some("two")
                        && evidence.result == Err(SkipReason::WarmCacheTransitionDenied))
        ));

        let cold = lane(
            &policy,
            "first",
            ANTHROPIC_SUB_MESSAGES.id(),
            "one",
            CacheTemperature::Cold,
        );
        assert_eq!(
            resolve(
                &policy,
                &OfferInventory::default(),
                &NoLearning,
                Some(&cold),
                &availability(true, false),
                &facts("asked"),
            )
            .unwrap()
            .effective_model,
            "two"
        );

        let unknown = lane(
            &policy,
            "first",
            ANTHROPIC_SUB_MESSAGES.id(),
            "one",
            CacheTemperature::Unknown,
        );
        assert!(matches!(
            resolve(
                &policy,
                &OfferInventory::default(),
                &NoLearning,
                Some(&unknown),
                &availability(true, false),
                &facts("asked"),
            ),
            Err(ResolveFailure::RouteUnavailable { trace, .. })
                if matches!(&trace[2], TraceEvent::Candidate(evidence)
                    if evidence.result == Err(SkipReason::WarmCacheTransitionDenied))
        ));
    }

    #[test]
    fn fallback_without_a_pin_requires_explicit_cache_loss_consent() {
        let mut first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "one".to_owned(),
            },
        );
        first.enabled = false;
        let second = candidate(
            "second",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "two".to_owned(),
            },
        );
        let policy = compile(
            document(route(
                first,
                vec![RouteStep {
                    edge: CandidateEdge {
                        preflight: FallbackPermission::Enabled,
                        post_send: vec![],
                        billed_boundary: BoundaryConsent::Deny,
                        ambiguous_transport: BoundaryConsent::Deny,
                        cache_transition: CacheTransitionPolicy::PreserveWarm,
                    },
                    candidate: second,
                }],
            )),
            &OfferInventory::default(),
        );
        let unpinned = LaneFacts {
            temperature: CacheTemperature::Warm,
            pinned: None,
            forced_from: None,
            forced_to: None,
        };
        assert!(matches!(
            resolve(
                &policy,
                &OfferInventory::default(),
                &NoLearning,
                Some(&unpinned),
                &availability(true, false),
                &facts("asked"),
            ),
            Err(ResolveFailure::RouteUnavailable { trace, .. })
                if matches!(&trace[2], TraceEvent::Candidate(evidence)
                    if evidence.result == Err(SkipReason::WarmCacheTransitionDenied))
        ));
    }

    #[test]
    fn every_crossed_edge_must_allow_warm_cache_loss() {
        let mut first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "one".to_owned(),
            },
        );
        first.enabled = false;
        let mut second = candidate(
            "second",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "two".to_owned(),
            },
        );
        second.enabled = false;
        let third = candidate(
            "third",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "three".to_owned(),
            },
        );
        let edge = |cache_transition| CandidateEdge {
            preflight: FallbackPermission::Enabled,
            post_send: vec![],
            billed_boundary: BoundaryConsent::Deny,
            ambiguous_transport: BoundaryConsent::Deny,
            cache_transition,
        };
        let policy = compile(
            document(route(
                first,
                vec![
                    RouteStep {
                        edge: edge(CacheTransitionPolicy::PreserveWarm),
                        candidate: second,
                    },
                    RouteStep {
                        edge: edge(CacheTransitionPolicy::AllowCacheLoss),
                        candidate: third,
                    },
                ],
            )),
            &OfferInventory::default(),
        );
        let warm = lane(
            &policy,
            "first",
            ANTHROPIC_SUB_MESSAGES.id(),
            "one",
            CacheTemperature::Warm,
        );
        let failure = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            Some(&warm),
            &availability(true, false),
            &facts("asked"),
        )
        .unwrap_err();
        assert!(matches!(
            failure,
            ResolveFailure::RouteUnavailable { trace, .. }
                if matches!(&trace[2], TraceEvent::Candidate(evidence)
                    if evidence.crossed_from.as_ref().is_some_and(|id| id.as_str() == "first"))
                    && matches!(&trace[3], TraceEvent::Candidate(evidence)
                        if evidence.crossed_from.is_none()
                            && evidence.model.as_deref() == Some("three")
                            && evidence.result == Err(SkipReason::WarmCacheTransitionDenied))
        ));
    }

    #[test]
    fn successful_fallback_retains_the_strictest_crossed_cache_consent() {
        let mut first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "one".to_owned(),
            },
        );
        first.enabled = false;
        let mut second = candidate(
            "second",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "two".to_owned(),
            },
        );
        second.enabled = false;
        let third = candidate(
            "third",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "three".to_owned(),
            },
        );
        let edge = |cache_transition| CandidateEdge {
            preflight: FallbackPermission::Enabled,
            post_send: vec![],
            billed_boundary: BoundaryConsent::Deny,
            ambiguous_transport: BoundaryConsent::Deny,
            cache_transition,
        };
        let policy = compile(
            document(route(
                first,
                vec![
                    RouteStep {
                        edge: edge(CacheTransitionPolicy::AllowWhenCold),
                        candidate: second,
                    },
                    RouteStep {
                        edge: edge(CacheTransitionPolicy::AllowCacheLoss),
                        candidate: third,
                    },
                ],
            )),
            &OfferInventory::default(),
        );
        let selected = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &facts("asked"),
        )
        .unwrap();
        assert_eq!(selected.candidate_id, Some(id("third")));
        assert_eq!(
            selected.fallback_cache_transition,
            Some(CacheTransitionPolicy::AllowWhenCold)
        );
    }

    #[test]
    fn administrative_resolution_never_crosses_the_first_candidate_edge() {
        let mut first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::Exact {
                model_id: "one".to_owned(),
            },
        );
        first.enabled = false;
        let second = candidate(
            "second",
            &ANTHROPIC_API_MESSAGES,
            CandidateModel::Exact {
                model_id: "two".to_owned(),
            },
        );
        let policy = compile(
            document(route(
                first,
                vec![RouteStep {
                    edge: CandidateEdge {
                        preflight: FallbackPermission::Enabled,
                        post_send: vec![],
                        billed_boundary: BoundaryConsent::Allow,
                        ambiguous_transport: BoundaryConsent::Deny,
                        cache_transition: CacheTransitionPolicy::AllowCacheLoss,
                    },
                    candidate: second,
                }],
            )),
            &OfferInventory::default(),
        );
        let failure = resolve_first_candidate(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            &availability(true, false),
            &CircuitSnapshot::new(),
            &facts("asked"),
        )
        .unwrap_err();
        assert!(matches!(
            failure,
            ResolveFailure::RouteUnavailable { trace, .. }
                if trace.len() == 2
                    && matches!(&trace[1], TraceEvent::Candidate(evidence)
                        if evidence.candidate_id == id("first")
                            && evidence.result == Err(SkipReason::Disabled))
        ));
    }

    /// Two Anthropic candidates, the first on the subscription binding, joined
    /// by an edge that permits preflight fallback; `honours` is the route flag.
    fn safeguards_policy(honours: bool, second_enabled_edge: bool) -> CompiledPolicy {
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
        let edge = CandidateEdge {
            preflight: if second_enabled_edge {
                FallbackPermission::Enabled
            } else {
                FallbackPermission::Disabled
            },
            post_send: vec![FailureClass::Overload],
            billed_boundary: BoundaryConsent::Allow,
            ambiguous_transport: BoundaryConsent::Deny,
            cache_transition: CacheTransitionPolicy::AllowCacheLoss,
        };
        let mut route = route(
            first,
            vec![RouteStep {
                edge,
                candidate: second,
            }],
        );
        route.honours_server_safeguards = honours;
        compile(document(route), &OfferInventory::default())
    }

    fn marked(binding: &crate::routing::BackendBinding) -> AvailabilitySnapshot {
        availability(true, false).with_safeguards_marks(&BTreeSet::from([binding.id().clone()]))
    }

    fn resolve_asking(
        policy: &CompiledPolicy,
        availability: &AvailabilitySnapshot,
        asks: bool,
    ) -> Result<AttemptPlan, ResolveFailure> {
        let mut request = facts("asked");
        request.asks_server_safeguards = asks;
        resolve(
            policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            availability,
            &request,
        )
    }

    #[test]
    fn an_honouring_route_skips_a_candidate_whose_effective_support_is_unsupported() {
        let policy = safeguards_policy(true, true);
        let plan = resolve_asking(&policy, &marked(&ANTHROPIC_SUB_MESSAGES), true).unwrap();
        assert_eq!(plan.binding_id, *ANTHROPIC_API_MESSAGES.id());
        assert!(plan.trace.iter().any(|event| matches!(
            event,
            TraceEvent::Candidate(e)
                if e.candidate_id.as_str() == "first"
                    && e.result == Err(SkipReason::LacksServerSafeguards)
        )));
        assert_eq!(
            plan.safeguards,
            SafeguardsPlan {
                asked: true,
                effective: Some(crate::routing::CapabilityEvidence::Supported),
                marked: false,
                skipped_candidate: true,
                walked_again: false,
            }
        );
    }

    #[test]
    fn an_unknown_effective_support_never_skips_a_candidate() {
        let policy = safeguards_policy(true, true);
        let mut snapshot = availability(true, false);
        let mut bindings: BTreeMap<_, _> = snapshot
            .iter()
            .map(|(id, live)| (id.clone(), live.clone()))
            .collect();
        bindings
            .get_mut(ANTHROPIC_SUB_MESSAGES.id())
            .unwrap()
            .capabilities
            .server_safeguards = crate::routing::CapabilityEvidence::Unknown;
        snapshot = AvailabilitySnapshot::new(bindings);
        let plan = resolve_asking(&policy, &snapshot, true).unwrap();
        assert_eq!(plan.binding_id, *ANTHROPIC_SUB_MESSAGES.id());
        assert!(!plan.safeguards.skipped_candidate);
    }

    #[test]
    fn an_honouring_route_with_no_capable_candidate_is_walked_again_without_the_requirement() {
        // Preflight fallback is disabled, so the first candidate is the only
        // one reachable, and it is marked.
        let policy = safeguards_policy(true, false);
        let plan = resolve_asking(&policy, &marked(&ANTHROPIC_SUB_MESSAGES), true).unwrap();
        assert_eq!(plan.binding_id, *ANTHROPIC_SUB_MESSAGES.id());
        assert!(plan.safeguards.walked_again);
        assert!(plan.safeguards.skipped_candidate);
        assert!(plan.safeguards.marked);
        assert_eq!(
            plan.safeguards.effective,
            Some(crate::routing::CapabilityEvidence::Unsupported)
        );
        let kinds: Vec<_> = plan
            .trace
            .iter()
            .filter(|event| matches!(event, TraceEvent::SafeguardsWalkedAgain))
            .collect();
        assert_eq!(kinds.len(), 1, "the second walk is recorded once");
    }

    #[test]
    fn a_route_that_does_not_honour_the_ask_routes_as_though_it_had_not_asked() {
        let policy = safeguards_policy(false, true);
        let plan = resolve_asking(&policy, &marked(&ANTHROPIC_SUB_MESSAGES), true).unwrap();
        assert_eq!(plan.binding_id, *ANTHROPIC_SUB_MESSAGES.id());
        assert!(!plan.safeguards.skipped_candidate);
        assert!(!plan.safeguards.walked_again);
        assert!(plan.safeguards.asked);
    }

    #[test]
    fn a_request_that_does_not_ask_is_never_held_to_the_requirement() {
        let policy = safeguards_policy(true, true);
        let plan = resolve_asking(&policy, &marked(&ANTHROPIC_SUB_MESSAGES), false).unwrap();
        assert_eq!(plan.binding_id, *ANTHROPIC_SUB_MESSAGES.id());
        assert!(!plan.safeguards.asked);
        assert!(!plan.safeguards.skipped_candidate);
    }

    #[test]
    fn a_fall_after_send_continues_under_the_same_rule() {
        let policy = safeguards_policy(true, true);
        // Both bindings are reachable and capable at first.
        let first = resolve_asking(&policy, &availability(true, false), true).unwrap();
        assert_eq!(first.binding_id, *ANTHROPIC_SUB_MESSAGES.id());
        // By the time it falls, the second binding has been marked: nothing
        // capable is left, so the continuation walks again and still serves it.
        let next = resolve_after_failure(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            &marked(&ANTHROPIC_API_MESSAGES),
            &CircuitSnapshot::default(),
            &first,
            FailureClass::Overload,
        )
        .unwrap()
        .expect("the edge authorizes overload fallback");
        assert_eq!(next.binding_id, *ANTHROPIC_API_MESSAGES.id());
        assert!(next.safeguards.walked_again);
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
        let ResolveFailure::RouteUnavailable { trace, .. } = failure else {
            panic!()
        };
        assert!(
            trace
                .iter()
                .any(|event| matches!(event, TraceEvent::Candidate(e)
            if e.crossed_from.is_none()
                && e.result == Err(SkipReason::PreflightFallbackDisabled)))
        );

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
            .thinking_replay = crate::routing::CapabilityEvidence::Unsupported;
        availability
            .bindings
            .get_mut(ANTHROPIC_API_MESSAGES.id())
            .unwrap()
            .capabilities
            .thinking_replay = crate::routing::CapabilityEvidence::Unsupported;
        let failure = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability,
            &request,
        )
        .unwrap_err();
        let ResolveFailure::RouteUnavailable { trace, .. } = failure else {
            panic!()
        };
        assert!(trace.iter().any(|event| matches!(event, TraceEvent::Candidate(e) if e.result == Err(SkipReason::MissingRequiredCapability))));
    }

    #[test]
    fn tool_choice_requirements_use_exact_verified_binding_evidence() {
        let policy = compile(
            document(route(
                candidate(
                    "first",
                    &ANTHROPIC_SUB_MESSAGES,
                    CandidateModel::RequestedModel,
                ),
                vec![],
            )),
            &OfferInventory::default(),
        );
        type Requirement = fn(&mut CapabilityFlags);
        type Denial = fn(&mut Capabilities);
        let cases: [(Requirement, Denial); 4] = [
            (
                |required| required.tool_choice_none = true,
                |actual| {
                    actual.tool_choice_none = crate::routing::CapabilityEvidence::Unsupported;
                },
            ),
            (
                |required| required.tool_choice_named = true,
                |actual| {
                    actual.tool_choice_named = crate::routing::CapabilityEvidence::Unsupported;
                },
            ),
            (
                |required| required.tool_choice_unnamed = true,
                |actual| {
                    actual.tool_choice_unnamed = crate::routing::CapabilityEvidence::Unsupported;
                },
            ),
            (
                |required| required.tool_choice_other = true,
                |actual| {
                    actual.tool_choice_other = crate::routing::CapabilityEvidence::Unsupported;
                },
            ),
        ];

        for (require, deny) in cases {
            let mut request = facts("claude-opus-5");
            require(&mut request.required_capabilities);
            let mut live = availability(true, false);
            deny(
                &mut live
                    .bindings
                    .get_mut(ANTHROPIC_SUB_MESSAGES.id())
                    .unwrap()
                    .capabilities,
            );
            let failure = resolve(
                &policy,
                &OfferInventory::default(),
                &NoLearning,
                None,
                &live,
                &request,
            )
            .unwrap_err();
            let ResolveFailure::RouteUnavailable { trace, .. } = failure else {
                panic!()
            };
            assert!(trace.iter().any(|event| matches!(
                event,
                TraceEvent::Candidate(e)
                    if e.result == Err(SkipReason::MissingRequiredCapability)
            )));

            let actual = &mut live
                .bindings
                .get_mut(ANTHROPIC_SUB_MESSAGES.id())
                .unwrap()
                .capabilities;
            *actual = Capabilities::UNKNOWN;
            assert!(
                resolve(
                    &policy,
                    &OfferInventory::default(),
                    &NoLearning,
                    None,
                    &live,
                    &request,
                )
                .is_ok(),
                "unknown evidence remains viable"
            );
        }
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
    fn a_plan_builds_a_complete_pin_only_with_a_stored_revision() {
        let first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::RequestedModel,
        );
        let policy = compile(document(route(first, vec![])), &OfferInventory::default());
        let mut plan = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &facts("asked"),
        )
        .unwrap();
        assert_eq!(plan.pin_identity(), None);
        plan.policy_revision = Some(7);
        plan.account_number = Some(3);
        let pin = plan.pin_identity().expect("complete pin");
        assert_eq!(pin.policy_revision_id, 7);
        assert_eq!(pin.account_number, Some(3));
        assert_eq!(pin.policy_route_id, plan.route_id.clone().unwrap());
        assert_eq!(
            pin.route_fingerprint,
            plan.route_fingerprint.clone().unwrap()
        );
        assert_eq!(pin.candidate_id, plan.candidate_id.clone().unwrap());
        assert_eq!(pin.binding_id, plan.binding_id);
        assert_eq!(pin.baseline_model, plan.baseline_model);
        assert_eq!(pin.effective_model, plan.effective_model);
    }

    #[test]
    fn revision_one_compaction_overlays_the_selected_binding_and_legacy_guards() {
        let mut document = document(route(
            candidate(
                "first",
                &ANTHROPIC_SUB_MESSAGES,
                CandidateModel::RequestedModel,
            ),
            vec![],
        ));
        document.classifiers.push(FamilyClassifier {
            id: ClassifierId::new("family").unwrap(),
            binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
            model_matcher: MatcherSpec::Regex(
                r"^(?:claude-)?(?P<family>[a-z0-9][a-z0-9-]*?)-(?P<version>[0-9]+(?:-[0-9]+)*)$"
                    .to_owned(),
            ),
            family_template: "${family}".to_owned(),
            version_template: "${version}".to_owned(),
            version_parser: VersionParserSpec::LegacyNumericTuple {
                separator: "-".to_owned(),
            },
            legacy_hyphenated: true,
        });
        document.compaction = Some(CompactionPolicy {
            target: CompactionTarget::RevisionOneLegacy {
                model_spec: "sonnet".to_owned(),
                eligible_bindings: vec![ANTHROPIC_SUB_MESSAGES.id().clone()],
            },
            lane_transition: LaneTransitionPolicy::MoveWhenCold,
            cache_transition: CacheTransitionPolicy::AllowWhenCold,
        });
        let policy = compile_origin(
            document,
            &OfferInventory::default(),
            PolicyOrigin::GeneratedRevisionOne,
        );
        let learning = TestLearning {
            models: BTreeMap::from([
                ("claude-sonnet-5".to_owned(), learned(8, 500_000)),
                ("claude-sonnet-5-5".to_owned(), learned(1, 500_000)),
            ]),
            active_days: 8,
        };
        let mut request = facts("claude-opus-5");
        request.class = RequestClass::Compaction;
        request.legacy_compaction = Some(LegacyCompactionFacts {
            lane_cold: true,
            prompt_tokens: 200_000,
            transform_applied: true,
        });
        let plan = resolve(
            &policy,
            &OfferInventory::default(),
            &learning,
            None,
            &availability(true, false),
            &request,
        )
        .unwrap();
        assert_eq!(plan.binding_id, *ANTHROPIC_SUB_MESSAGES.id());
        assert_eq!(plan.provenance, RouteProvenance::ProtocolDefault);
        assert_eq!(plan.baseline_model, "claude-opus-5");
        assert_eq!(plan.retargeted_model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(plan.effective_model, "claude-sonnet-5");
        assert!(plan.compaction);

        let mut warm = request.clone();
        warm.legacy_compaction.as_mut().unwrap().lane_cold = false;
        warm.legacy_compaction.as_mut().unwrap().transform_applied = false;
        let plan = resolve(
            &policy,
            &OfferInventory::default(),
            &learning,
            None,
            &availability(true, false),
            &warm,
        )
        .unwrap();
        assert_eq!(plan.retargeted_model, None);
        assert!(!plan.compaction);

        let mut declined = request.clone();
        declined
            .legacy_compaction
            .as_mut()
            .unwrap()
            .transform_applied = false;
        let plan = resolve(
            &policy,
            &OfferInventory::default(),
            &learning,
            None,
            &availability(true, false),
            &declined,
        )
        .unwrap();
        assert_eq!(plan.retargeted_model, None);
        assert_eq!(plan.effective_model, "claude-opus-5");
        assert!(!plan.compaction);

        let too_large = TestLearning {
            models: BTreeMap::from([("claude-sonnet-5".to_owned(), learned(8, 100_000))]),
            active_days: 8,
        };
        let plan = resolve(
            &policy,
            &OfferInventory::default(),
            &too_large,
            None,
            &availability(true, false),
            &request,
        )
        .unwrap();
        assert_eq!(plan.retargeted_model, None);
        assert!(
            plan.compaction,
            "cold same-model transform remains licensed"
        );
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
            honours_server_safeguards: false,
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
            target: CompactionTarget::Route(RouteReference {
                route_id: id("compact-route"),
                parameters: BTreeMap::new(),
            }),
            lane_transition: LaneTransitionPolicy::MoveWhenCold,
            cache_transition: CacheTransitionPolicy::AllowWhenCold,
        });
        let policy = compile(doc, &OfferInventory::default());
        let mut request = facts("original");
        request.class = RequestClass::Compaction;
        let cold = LaneFacts {
            temperature: CacheTemperature::Cold,
            pinned: None,
            forced_from: None,
            forced_to: None,
        };
        let plan = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            Some(&cold),
            &availability(true, false),
            &request,
        )
        .unwrap();
        assert_eq!(plan.baseline_model, "compact-model");
        assert!(plan.compaction);
        assert_eq!(plan.provenance, RouteProvenance::Compaction);

        for lane in [
            None,
            Some(LaneFacts {
                temperature: CacheTemperature::Warm,
                pinned: None,
                forced_from: None,
                forced_to: None,
            }),
            Some(LaneFacts {
                temperature: CacheTemperature::Unknown,
                pinned: None,
                forced_from: None,
                forced_to: None,
            }),
        ] {
            let plan = resolve(
                &policy,
                &OfferInventory::default(),
                &NoLearning,
                lane.as_ref(),
                &availability(true, false),
                &request,
            )
            .unwrap();
            assert_eq!(plan.baseline_model, "original");
            assert_ne!(plan.provenance, RouteProvenance::Compaction);
        }
    }

    fn model_free_facts() -> RequestFacts {
        RequestFacts {
            requested_model: None,
            ..facts("unused")
        }
    }

    fn model_free_document() -> PolicyDocument {
        let first = candidate(
            "first",
            &ANTHROPIC_SUB_MESSAGES,
            CandidateModel::RequestedModel,
        );
        let second = candidate(
            "second",
            &ANTHROPIC_API_MESSAGES,
            CandidateModel::RequestedModel,
        );
        document(route(
            first,
            vec![RouteStep {
                edge: CandidateEdge {
                    preflight: FallbackPermission::Enabled,
                    post_send: vec![],
                    billed_boundary: BoundaryConsent::Allow,
                    ambiguous_transport: BoundaryConsent::Deny,
                    cache_transition: CacheTransitionPolicy::AllowCacheLoss,
                },
                candidate: second,
            }],
        ))
    }

    #[test]
    fn a_model_free_request_takes_the_default_routes_first_binding() {
        let policy = compile(model_free_document(), &OfferInventory::default());
        let binding =
            resolve_model_free(&policy, &availability(true, false), &model_free_facts()).unwrap();
        assert_eq!(&binding, ANTHROPIC_SUB_MESSAGES.id());
    }

    #[test]
    fn a_model_free_request_prefers_the_profile_default_to_the_protocol_one() {
        let mut document = model_free_document();
        let mut profile_route = route(
            candidate(
                "profile-first",
                &ANTHROPIC_API_MESSAGES,
                CandidateModel::RequestedModel,
            ),
            vec![],
        );
        profile_route.id = id("profile-route");
        document.routes.push(profile_route);
        document.defaults.push(DefaultRoute {
            scope: scope(Some("claude")),
            target: reference("profile-route", ArgumentBinding::RequestedModel),
        });
        let policy = compile(document, &OfferInventory::default());
        let binding =
            resolve_model_free(&policy, &availability(true, false), &model_free_facts()).unwrap();
        assert_eq!(&binding, ANTHROPIC_API_MESSAGES.id());
    }

    #[test]
    fn a_model_free_request_does_not_fall_back_past_an_unavailable_first_binding() {
        let policy = compile(model_free_document(), &OfferInventory::default());
        let failure = resolve_model_free(&policy, &availability(false, false), &model_free_facts())
            .unwrap_err();
        assert!(matches!(
            failure,
            ResolveFailure::RouteUnavailable { trace, .. }
                if matches!(&trace[1], TraceEvent::Candidate(evidence)
                    if evidence.candidate_id == id("first")
                        && evidence.result == Err(SkipReason::ProviderUnconfigured))
        ));
    }

    #[test]
    fn model_free_resolution_refuses_a_request_that_names_a_model() {
        let policy = compile(model_free_document(), &OfferInventory::default());
        assert!(matches!(
            resolve_model_free(&policy, &availability(true, false), &facts("asked")),
            Err(ResolveFailure::InvalidArgument)
        ));
    }

    #[test]
    fn an_inference_request_without_a_model_still_fails_ordinary_resolution() {
        let policy = compile(model_free_document(), &OfferInventory::default());
        let failure = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &model_free_facts(),
        )
        .unwrap_err();
        assert!(matches!(failure, ResolveFailure::InvalidArgument));
    }

    #[test]
    fn a_compaction_move_is_read_off_the_plan_and_only_when_there_is_one() {
        let policy = compile(
            document(route(
                candidate(
                    "first",
                    &ANTHROPIC_SUB_MESSAGES,
                    CandidateModel::RequestedModel,
                ),
                vec![],
            )),
            &OfferInventory::default(),
        );
        let mut plan = resolve(
            &policy,
            &OfferInventory::default(),
            &NoLearning,
            None,
            &availability(true, false),
            &facts("opus"),
        )
        .unwrap();
        assert_eq!(
            plan.compaction_move(),
            None,
            "an ordinary plan moves nothing"
        );

        plan.retargeted_model = Some("sonnet".to_owned());
        plan.effective_model = "sonnet".to_owned();
        assert_eq!(
            plan.compaction_move(),
            Some(("opus".to_owned(), "sonnet".to_owned()))
        );

        plan.upgraded_model = Some("opus-2".to_owned());
        assert_eq!(
            plan.compaction_move(),
            Some(("opus-2".to_owned(), "sonnet".to_owned())),
            "the lane's upgraded model is what it moved off"
        );

        plan.upgraded_model = None;
        plan.effective_model = "opus".to_owned();
        assert_eq!(
            plan.compaction_move(),
            None,
            "a retarget that lands on the model it started on is not a move"
        );

        plan.provenance = RouteProvenance::Compaction;
        plan.retargeted_model = None;
        plan.effective_model = "haiku".to_owned();
        assert_eq!(
            plan.compaction_move(),
            Some(("opus".to_owned(), "haiku".to_owned())),
            "an explicit compaction route moves off what the client asked for"
        );
    }
}
