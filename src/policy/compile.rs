//! Final in-memory compiler for routing policy documents.
//!
//! Compilation closes every document reference against static route topology
//! and one immutable offer snapshot. Provider configuration and catalogue
//! presence are evidence, not document validity, so they produce non-blocking
//! diagnostics only.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::catalog::offers::OfferInventory;
use crate::routing::{BackendBinding, Capabilities, ProtocolId, RouteRegistry};

use super::{
    AdapterMetadata, AliasPolicy, ArgumentBinding, BindingId, BoundaryConsent,
    CacheTransitionPolicy, Candidate, CandidateEdge, CandidateId, CandidateModel, CaptureSchema,
    ClassifierId, CompactionPolicy, CompiledMatcher, CostClass, Diagnostic, DiagnosticCode,
    DiagnosticField, DiagnosticPath, FailureClass, FamilyClassifier, FrontendProtocol,
    FrontendScope, LaneTransitionPolicy, MatcherSpec, ParsedVersion, PolicyFingerprint,
    PresentationId, PresentationSource, REQUESTED_MODEL_PARAMETER, RewriteId, Route,
    RouteFingerprint, RouteId, RouteReference, RouteSemantics, SelectorOrdering, Severity,
    StableId, Template, UpgradePolicy, UpgradePolicyId, ValidatedPolicyDocument, VersionParserSpec,
    policy_fingerprint, route_fingerprint,
};

/// Whether compilation is allowed to consume revision-one legacy learning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PolicyOrigin {
    /// Authored and edited policies must use binding-scoped learning.
    #[default]
    Authored,
    /// A policy generated directly from legacy configuration.
    GeneratedRevisionOne,
}

/// The result of compiling one document.
#[derive(Debug)]
pub struct Compilation {
    policy: Option<CompiledPolicy>,
    diagnostics: Vec<Diagnostic>,
}

impl Compilation {
    pub fn policy(&self) -> Option<&CompiledPolicy> {
        self.policy.as_ref()
    }

    pub fn into_policy(self) -> Result<CompiledPolicy, Vec<Diagnostic>> {
        self.policy.ok_or(self.diagnostics)
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}

/// A closed, immutable policy ready for an eventual resolver.
#[derive(Debug)]
pub struct CompiledPolicy {
    fingerprint: PolicyFingerprint,
    routes: BTreeMap<RouteId, CompiledRoute>,
    presentations: BTreeMap<ScopeKey, BTreeMap<String, CompiledPresentation>>,
    rewrites: BTreeMap<ScopeKey, Vec<CompiledRewrite>>,
    defaults: BTreeMap<ScopeKey, CompiledRouteReference>,
    classifiers: BTreeMap<ClassifierId, CompiledClassifier>,
    upgrades: BTreeMap<UpgradePolicyId, CompiledUpgradePolicy>,
    compaction: Option<CompiledCompaction>,
}

impl CompiledPolicy {
    pub fn fingerprint(&self) -> &PolicyFingerprint {
        &self.fingerprint
    }

    pub fn route(&self, id: &RouteId) -> Option<&CompiledRoute> {
        self.routes.get(id)
    }

    pub fn presentation(&self, scope: &FrontendScope, slug: &str) -> Option<&CompiledPresentation> {
        self.presentations
            .get(&ScopeKey::from(scope))
            .and_then(|items| items.get(slug))
    }

    pub fn rewrites(&self, scope: &FrontendScope) -> &[CompiledRewrite] {
        self.rewrites
            .get(&ScopeKey::from(scope))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn default_route(&self, scope: &FrontendScope) -> Option<&CompiledRouteReference> {
        self.defaults.get(&ScopeKey::from(scope))
    }

    pub fn classifier(&self, id: &ClassifierId) -> Option<&CompiledClassifier> {
        self.classifiers.get(id)
    }

    pub fn upgrade(&self, id: &UpgradePolicyId) -> Option<&CompiledUpgradePolicy> {
        self.upgrades.get(id)
    }

    pub fn compaction(&self) -> Option<&CompiledCompaction> {
        self.compaction.as_ref()
    }

    pub fn presentations(
        &self,
        scope: &FrontendScope,
    ) -> impl Iterator<Item = (&str, &CompiledPresentation)> {
        self.presentations
            .get(&ScopeKey::from(scope))
            .into_iter()
            .flat_map(|items| {
                items
                    .iter()
                    .map(|(slug, presentation)| (slug.as_str(), presentation))
            })
    }
}

#[derive(Debug)]
pub struct CompiledRoute {
    id: RouteId,
    parameters: BTreeSet<StableId>,
    candidates: Vec<CompiledCandidate>,
    edges: Vec<CandidateEdge>,
    fingerprint: RouteFingerprint,
    lane_transition: LaneTransitionPolicy,
}

impl CompiledRoute {
    pub fn id(&self) -> &RouteId {
        &self.id
    }
    pub fn parameters(&self) -> impl ExactSizeIterator<Item = &StableId> {
        self.parameters.iter()
    }
    pub fn candidates(&self) -> &[CompiledCandidate] {
        &self.candidates
    }
    pub fn edges(&self) -> &[CandidateEdge] {
        &self.edges
    }
    pub fn fingerprint(&self) -> &RouteFingerprint {
        &self.fingerprint
    }
    pub fn lane_transition(&self) -> LaneTransitionPolicy {
        self.lane_transition
    }
}

#[derive(Debug)]
pub struct CompiledCandidate {
    id: CandidateId,
    binding_id: BindingId,
    model: CompiledCandidateModel,
    upgrade_policy_id: Option<UpgradePolicyId>,
    alias_rules: Vec<CompiledAliasRule>,
    enabled: bool,
    required_capabilities: super::CapabilityFlags,
}

impl CompiledCandidate {
    pub fn id(&self) -> &CandidateId {
        &self.id
    }
    pub fn binding_id(&self) -> &BindingId {
        &self.binding_id
    }
    pub fn model(&self) -> &CompiledCandidateModel {
        &self.model
    }
    pub fn upgrade_policy_id(&self) -> Option<&UpgradePolicyId> {
        self.upgrade_policy_id.as_ref()
    }
    pub fn alias_rules(&self) -> &[CompiledAliasRule] {
        &self.alias_rules
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn required_capabilities(&self) -> super::CapabilityFlags {
        self.required_capabilities
    }
}

#[derive(Debug)]
pub enum CompiledCandidateModel {
    Exact(String),
    RequestedModel,
    Template(Template),
    OfferSelector(CompiledSelector),
}

#[derive(Debug)]
pub struct CompiledAliasRule {
    matcher: CompiledMatcher,
    exclude: Vec<CompiledMatcher>,
    replacement: Template,
}

impl CompiledAliasRule {
    pub fn matcher(&self) -> &CompiledMatcher {
        &self.matcher
    }
    pub fn exclude(&self) -> &[CompiledMatcher] {
        &self.exclude
    }
    pub fn replacement(&self) -> &Template {
        &self.replacement
    }
}

#[derive(Debug)]
pub struct CompiledSelector {
    include: Vec<CompiledMatcher>,
    exclude: Vec<CompiledMatcher>,
    ordering: SelectorOrdering,
}

impl CompiledSelector {
    pub fn include(&self) -> &[CompiledMatcher] {
        &self.include
    }
    pub fn exclude(&self) -> &[CompiledMatcher] {
        &self.exclude
    }
    pub fn ordering(&self) -> &SelectorOrdering {
        &self.ordering
    }
}

#[derive(Debug)]
pub struct CompiledPresentation {
    id: PresentationId,
    route: PresentationRoute,
    metadata: Option<AdapterMetadata>,
}

impl CompiledPresentation {
    pub fn id(&self) -> &PresentationId {
        &self.id
    }
    pub fn route(&self) -> &PresentationRoute {
        &self.route
    }
    pub fn metadata(&self) -> Option<&AdapterMetadata> {
        self.metadata.as_ref()
    }
}

#[derive(Debug)]
pub enum PresentationRoute {
    Reference(CompiledRouteReference),
    Exact {
        binding_id: BindingId,
        model_id: String,
    },
}

#[derive(Debug)]
pub struct CompiledRouteReference {
    route_id: RouteId,
    parameters: BTreeMap<StableId, ArgumentBinding>,
}

impl CompiledRouteReference {
    pub fn route_id(&self) -> &RouteId {
        &self.route_id
    }
    pub fn parameters(&self) -> &BTreeMap<StableId, ArgumentBinding> {
        &self.parameters
    }
}

#[derive(Debug)]
pub struct CompiledRewrite {
    id: RewriteId,
    priority: i32,
    matcher: CompiledMatcher,
    target: CompiledRouteReference,
}

impl CompiledRewrite {
    pub fn id(&self) -> &RewriteId {
        &self.id
    }
    pub fn priority(&self) -> i32 {
        self.priority
    }
    pub fn matcher(&self) -> &CompiledMatcher {
        &self.matcher
    }
    pub fn target(&self) -> &CompiledRouteReference {
        &self.target
    }
}

#[derive(Debug)]
pub struct CompiledClassifier {
    id: ClassifierId,
    binding_id: BindingId,
    matcher: CompiledMatcher,
    family_template: Template,
    version_template: Template,
    parser: VersionParserSpec,
}

impl CompiledClassifier {
    pub fn id(&self) -> &ClassifierId {
        &self.id
    }
    pub fn binding_id(&self) -> &BindingId {
        &self.binding_id
    }
    pub fn matcher(&self) -> &CompiledMatcher {
        &self.matcher
    }
    pub fn family_template(&self) -> &Template {
        &self.family_template
    }
    pub fn version_template(&self) -> &Template {
        &self.version_template
    }
    pub fn parser(&self) -> &VersionParserSpec {
        &self.parser
    }
}

#[derive(Debug, Clone)]
pub struct CompiledUpgradePolicy {
    policy: UpgradePolicy,
}

impl CompiledUpgradePolicy {
    pub fn policy(&self) -> &UpgradePolicy {
        &self.policy
    }
}

#[derive(Debug)]
pub struct CompiledCompaction {
    target: CompiledRouteReference,
    lane_transition: LaneTransitionPolicy,
    cache_transition: CacheTransitionPolicy,
}

impl CompiledCompaction {
    pub fn target(&self) -> &CompiledRouteReference {
        &self.target
    }
    pub fn lane_transition(&self) -> LaneTransitionPolicy {
        self.lane_transition
    }
    pub fn cache_transition(&self) -> CacheTransitionPolicy {
        self.cache_transition
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ScopeKey {
    profile: Option<String>,
    protocol: FrontendProtocol,
}

impl From<&FrontendScope> for ScopeKey {
    fn from(scope: &FrontendScope) -> Self {
        match scope {
            FrontendScope::Profile { profile, protocol } => Self {
                profile: Some(profile.to_string()),
                protocol: *protocol,
            },
            FrontendScope::Protocol { protocol } => Self {
                profile: None,
                protocol: *protocol,
            },
        }
    }
}

/// Compile an intrinsically validated policy against immutable topology and
/// offer inventory inputs.
pub fn compile(
    document: ValidatedPolicyDocument,
    topology: &RouteRegistry,
    inventory: &OfferInventory,
    origin: PolicyOrigin,
) -> Compilation {
    Compiler::new(document, topology, inventory, origin).run()
}

struct Compiler<'a> {
    document: ValidatedPolicyDocument,
    topology: &'a RouteRegistry,
    inventory: &'a OfferInventory,
    origin: PolicyOrigin,
    diagnostics: Vec<Diagnostic>,
    failed: bool,
}

impl<'a> Compiler<'a> {
    fn new(
        document: ValidatedPolicyDocument,
        topology: &'a RouteRegistry,
        inventory: &'a OfferInventory,
        origin: PolicyOrigin,
    ) -> Self {
        Self {
            document,
            topology,
            inventory,
            origin,
            diagnostics: vec![],
            failed: false,
        }
    }

    fn run(mut self) -> Compilation {
        let raw = self.document.document();
        let route_positions: HashMap<_, _> = raw
            .routes
            .iter()
            .enumerate()
            .map(|(index, route)| (route.id.clone(), index))
            .collect();
        let classifier_positions: HashMap<_, _> = raw
            .classifiers
            .iter()
            .enumerate()
            .map(|(index, item)| (item.id.clone(), index))
            .collect();
        let upgrade_positions: HashMap<_, _> = raw
            .upgrades
            .iter()
            .enumerate()
            .map(|(index, item)| (item.id.clone(), index))
            .collect();

        let classifiers = self.compile_classifiers();
        let upgrades = self.compile_upgrades(&classifier_positions);
        let routes = self.compile_routes(&classifier_positions, &upgrade_positions);
        let defaults = self.compile_defaults(&route_positions, &routes);
        let rewrites = self.compile_rewrites(&route_positions, &routes);
        let presentations = self.compile_presentations(&route_positions, &routes, &classifiers);
        let compaction = self.compile_compaction(&route_positions, &routes);

        if self.failed {
            return Compilation {
                policy: None,
                diagnostics: self.diagnostics,
            };
        }
        let fingerprint =
            policy_fingerprint(&self.document).expect("validated bounded policy canonicalises");
        Compilation {
            policy: Some(CompiledPolicy {
                fingerprint,
                routes,
                presentations,
                rewrites,
                defaults,
                classifiers,
                upgrades,
                compaction,
            }),
            diagnostics: self.diagnostics,
        }
    }

    fn compile_classifiers(&mut self) -> BTreeMap<ClassifierId, CompiledClassifier> {
        let items = self.document.document().classifiers.clone();
        let mut out = BTreeMap::new();
        for (index, item) in items.iter().enumerate() {
            let base = path("classifiers", index);
            let Some(_) = self.binding(
                &item.binding_id,
                base.clone().field(f("binding_id")),
                item.id.as_stable_id(),
            ) else {
                continue;
            };
            let Some(matcher) = self.matcher(
                &item.model_matcher,
                base.clone().field(f("model_matcher")),
                item.id.as_stable_id(),
            ) else {
                continue;
            };
            let captures = matcher.capture_schema();
            let empty = BTreeSet::new();
            let Some(family_template) = self.template(
                &item.family_template,
                &empty,
                &captures,
                base.clone().field(f("family_template")),
                item.id.as_stable_id(),
            ) else {
                continue;
            };
            let Some(version_template) = self.template(
                &item.version_template,
                &empty,
                &captures,
                base.clone().field(f("version_template")),
                item.id.as_stable_id(),
            ) else {
                continue;
            };
            out.insert(
                item.id.clone(),
                CompiledClassifier {
                    id: item.id.clone(),
                    binding_id: item.binding_id.clone(),
                    matcher,
                    family_template,
                    version_template,
                    parser: item.version_parser.clone(),
                },
            );
        }
        out
    }

    fn compile_upgrades(
        &mut self,
        classifiers: &HashMap<ClassifierId, usize>,
    ) -> BTreeMap<UpgradePolicyId, CompiledUpgradePolicy> {
        let items = self.document.document().upgrades.clone();
        let mut out = BTreeMap::new();
        for (index, item) in items.iter().enumerate() {
            let base = path("upgrades", index);
            if !classifiers.contains_key(&item.classifier_id) {
                self.error(
                    "missing-classifier",
                    base.clone().field(f("classifier_id")),
                    item.id.as_stable_id(),
                );
                continue;
            }
            if item.allow_revision_one_legacy_learning
                && self.origin != PolicyOrigin::GeneratedRevisionOne
            {
                self.error(
                    "legacy-learning-forbidden",
                    base.clone().field(f("allow_revision_one_legacy_learning")),
                    item.id.as_stable_id(),
                );
                continue;
            }
            if item.minimum_learning == super::MinimumLearning::RevisionOneAdaptive
                && self.origin != PolicyOrigin::GeneratedRevisionOne
            {
                self.error(
                    "legacy-adaptive-learning-forbidden",
                    base.clone().field(f("minimum_learning")),
                    item.id.as_stable_id(),
                );
                continue;
            }
            out.insert(
                item.id.clone(),
                CompiledUpgradePolicy {
                    policy: item.clone(),
                },
            );
        }
        out
    }

    fn compile_routes(
        &mut self,
        classifiers: &HashMap<ClassifierId, usize>,
        upgrades: &HashMap<UpgradePolicyId, usize>,
    ) -> BTreeMap<RouteId, CompiledRoute> {
        let items = self.document.document().routes.clone();
        let classifier_by_id: HashMap<_, _> = self
            .document
            .document()
            .classifiers
            .iter()
            .map(|item| (item.id.clone(), item.clone()))
            .collect();
        let upgrade_by_id: HashMap<_, _> = self
            .document
            .document()
            .upgrades
            .iter()
            .map(|item| (item.id.clone(), item.clone()))
            .collect();
        let mut out = BTreeMap::new();
        for (route_index, route) in items.iter().enumerate() {
            let base = path("routes", route_index);
            let parameters: BTreeSet<_> = route.parameters.iter().cloned().collect();
            let mut candidates = Vec::new();
            for (candidate_index, candidate) in route.chain.candidates().enumerate() {
                let candidate_path = if candidate_index == 0 {
                    base.clone().field(f("chain")).field(f("first"))
                } else {
                    base.clone()
                        .field(f("chain"))
                        .field(f("steps"))
                        .index(candidate_index - 1)
                        .expect("bounded")
                        .field(f("candidate"))
                };
                let Some(compiled) = self.compile_candidate(
                    candidate,
                    &parameters,
                    &candidate_path,
                    classifiers,
                    upgrades,
                    &classifier_by_id,
                    &upgrade_by_id,
                ) else {
                    continue;
                };
                candidates.push(compiled);
            }
            let edges: Vec<_> = route
                .chain
                .steps
                .iter()
                .map(|step| step.edge.clone())
                .collect();
            self.validate_edges(route_index, route, &edges);
            let fingerprint = match RouteSemantics::from_document(&self.document, &route.id)
                .and_then(|semantics| route_fingerprint(&semantics))
            {
                Ok(value) => value,
                Err(_) => {
                    self.error("route-semantics-invalid", base, route.id.as_stable_id());
                    continue;
                }
            };
            if candidates.len() == route.chain.candidates().count() {
                out.insert(
                    route.id.clone(),
                    CompiledRoute {
                        id: route.id.clone(),
                        parameters,
                        candidates,
                        edges,
                        fingerprint,
                        lane_transition: route.lane_transition,
                    },
                );
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_candidate(
        &mut self,
        candidate: &Candidate,
        parameters: &BTreeSet<StableId>,
        path: &DiagnosticPath,
        classifiers: &HashMap<ClassifierId, usize>,
        upgrades: &HashMap<UpgradePolicyId, usize>,
        classifier_by_id: &HashMap<ClassifierId, FamilyClassifier>,
        upgrade_by_id: &HashMap<UpgradePolicyId, UpgradePolicy>,
    ) -> Option<CompiledCandidate> {
        let binding = self.binding(
            &candidate.binding_id,
            path.clone().field(f("binding_id")),
            candidate.id.as_stable_id(),
        )?;
        if !self.capabilities_satisfy(binding, candidate) {
            self.error(
                "capability-unavailable",
                path.clone().field(f("required_capabilities")),
                candidate.id.as_stable_id(),
            );
        }
        if !self.topology.binding_is_configured(binding) {
            self.info(
                "binding-unconfigured",
                path.clone().field(f("binding_id")),
                candidate.id.as_stable_id(),
            );
        }
        if let Some(upgrade_id) = &candidate.upgrade_policy_id {
            if !upgrades.contains_key(upgrade_id) {
                self.error(
                    "missing-upgrade",
                    path.clone().field(f("upgrade_policy_id")),
                    candidate.id.as_stable_id(),
                );
            } else if let Some(upgrade) = upgrade_by_id.get(upgrade_id)
                && let Some(classifier) = classifier_by_id.get(&upgrade.classifier_id)
                && classifier.binding_id != candidate.binding_id
            {
                self.error(
                    "classifier-binding-mismatch",
                    path.clone().field(f("upgrade_policy_id")),
                    candidate.id.as_stable_id(),
                );
            }
        }
        let model_path = path.clone().field(f("model"));
        let model = match &candidate.model {
            CandidateModel::Exact { model_id } => {
                if self
                    .inventory
                    .get(&candidate.binding_id, model_id)
                    .is_none()
                {
                    self.info(
                        "offer-unknown",
                        model_path.clone().field(f("model_id")),
                        candidate.id.as_stable_id(),
                    );
                }
                CompiledCandidateModel::Exact(model_id.clone())
            }
            CandidateModel::RequestedModel => {
                if !parameters
                    .iter()
                    .any(|parameter| parameter.as_str() == REQUESTED_MODEL_PARAMETER)
                {
                    self.error(
                        "requested-model-undeclared",
                        model_path,
                        candidate.id.as_stable_id(),
                    );
                }
                CompiledCandidateModel::RequestedModel
            }
            CandidateModel::Template { template } => {
                let empty = Default::default();
                let template = self.template(
                    template,
                    parameters,
                    &empty,
                    model_path.clone().field(f("template")),
                    candidate.id.as_stable_id(),
                )?;
                CompiledCandidateModel::Template(template)
            }
            CandidateModel::OfferSelector { selector } => {
                self.validate_ordering(
                    &selector.ordering,
                    classifiers,
                    &candidate.binding_id,
                    &model_path,
                    candidate.id.as_stable_id(),
                    classifier_by_id,
                );
                let compiled = self.selector(
                    &selector.include,
                    &selector.exclude,
                    &selector.ordering,
                    &model_path,
                    candidate.id.as_stable_id(),
                )?;
                if super::selector::winner(
                    self.inventory,
                    &candidate.binding_id,
                    &compiled,
                    |classifier_id, offer| {
                        classifier_by_id
                            .get(classifier_id)
                            .filter(|classifier| classifier.binding_id == offer.binding_id)
                            .and_then(|classifier| {
                                classify_version(classifier, &offer.backend_model_id)
                            })
                    },
                )
                .is_none()
                {
                    self.info(
                        "selector-unresolved",
                        model_path.clone().field(f("selector")),
                        candidate.id.as_stable_id(),
                    );
                }
                CompiledCandidateModel::OfferSelector(compiled)
            }
        };
        let alias_rules = self.aliases(
            candidate.alias_policy.as_ref(),
            path,
            candidate.id.as_stable_id(),
        )?;
        Some(CompiledCandidate {
            id: candidate.id.clone(),
            binding_id: candidate.binding_id.clone(),
            model,
            upgrade_policy_id: candidate.upgrade_policy_id.clone(),
            alias_rules,
            enabled: candidate.enabled,
            required_capabilities: candidate.required_capabilities,
        })
    }

    fn validate_edges(&mut self, route_index: usize, route: &Route, edges: &[CandidateEdge]) {
        let candidates: Vec<_> = route.chain.candidates().collect();
        for (index, edge) in edges.iter().enumerate() {
            let Some(from) = self.topology.binding(&candidates[index].binding_id) else {
                continue;
            };
            let Some(to) = self.topology.binding(&candidates[index + 1].binding_id) else {
                continue;
            };
            let base = path("routes", route_index)
                .field(f("chain"))
                .field(f("steps"))
                .index(index)
                .expect("bounded")
                .field(f("edge"));
            if from.cost_class() == CostClass::Subscription
                && matches!(to.cost_class(), CostClass::Billed | CostClass::Unknown)
                && edge.billed_boundary != BoundaryConsent::Allow
            {
                self.error(
                    "billed-boundary-consent-required",
                    base.clone().field(f("billed_boundary")),
                    route.id.as_stable_id(),
                );
            }
            if edge.post_send.contains(&FailureClass::AmbiguousTransport)
                && edge.ambiguous_transport != BoundaryConsent::Allow
            {
                self.error(
                    "ambiguous-transport-consent-required",
                    base.clone().field(f("ambiguous_transport")),
                    route.id.as_stable_id(),
                );
            }
            let same_cache = from.id() == to.id()
                || from
                    .cache_domain()
                    .zip(to.cache_domain())
                    .is_some_and(|(left, right)| left.id() == right.id());
            if !same_cache && edge.cache_transition == CacheTransitionPolicy::PreserveWarm {
                self.error(
                    "cache-loss-consent-required",
                    base.field(f("cache_transition")),
                    route.id.as_stable_id(),
                );
            }
        }
    }

    fn compile_defaults(
        &mut self,
        routes: &HashMap<RouteId, usize>,
        compiled_routes: &BTreeMap<RouteId, CompiledRoute>,
    ) -> BTreeMap<ScopeKey, CompiledRouteReference> {
        let items = self.document.document().defaults.clone();
        let mut out = BTreeMap::new();
        for (index, item) in items.iter().enumerate() {
            let base = path("defaults", index);
            let key = ScopeKey::from(&item.scope);
            if out.contains_key(&key) {
                self.error(
                    "duplicate-default-scope",
                    base.clone().field(f("scope")),
                    &item.target.route_id.as_stable_id().clone(),
                );
                continue;
            }
            let Some(target) = self.route_reference(
                &item.target,
                None,
                &base.clone().field(f("target")),
                routes,
                compiled_routes,
            ) else {
                continue;
            };
            self.validate_route_scope(
                &item.scope,
                &target.route_id,
                compiled_routes,
                base.clone().field(f("target")),
            );
            out.insert(key, target);
        }
        out
    }

    fn compile_rewrites(
        &mut self,
        routes: &HashMap<RouteId, usize>,
        compiled_routes: &BTreeMap<RouteId, CompiledRoute>,
    ) -> BTreeMap<ScopeKey, Vec<CompiledRewrite>> {
        let items = self.document.document().rewrites.clone();
        let mut priorities = HashSet::new();
        let mut out: BTreeMap<ScopeKey, Vec<CompiledRewrite>> = BTreeMap::new();
        for (index, item) in items.iter().enumerate() {
            let base = path("rewrites", index);
            let key = ScopeKey::from(&item.scope);
            if !priorities.insert((key.clone(), item.priority)) {
                self.error(
                    "duplicate-rewrite-priority",
                    base.clone().field(f("priority")),
                    item.id.as_stable_id(),
                );
                continue;
            }
            let Some(matcher) = self.matcher(
                &item.model_matcher,
                base.clone().field(f("model_matcher")),
                item.id.as_stable_id(),
            ) else {
                continue;
            };
            let captures = matcher.capture_schema();
            let Some(target) = self.route_reference(
                &item.target,
                Some((&item.model_matcher, &captures)),
                &base.clone().field(f("target")),
                routes,
                compiled_routes,
            ) else {
                continue;
            };
            self.validate_route_scope(
                &item.scope,
                &target.route_id,
                compiled_routes,
                base.clone().field(f("target")),
            );
            out.entry(key).or_default().push(CompiledRewrite {
                id: item.id.clone(),
                priority: item.priority,
                matcher,
                target,
            });
        }
        for rules in out.values_mut() {
            rules.sort_by_key(|rule| rule.priority);
        }
        out
    }

    fn compile_presentations(
        &mut self,
        routes: &HashMap<RouteId, usize>,
        compiled_routes: &BTreeMap<RouteId, CompiledRoute>,
        classifiers: &BTreeMap<ClassifierId, CompiledClassifier>,
    ) -> BTreeMap<ScopeKey, BTreeMap<String, CompiledPresentation>> {
        let items = self.document.document().presentations.clone();
        let classifier_raw: HashMap<_, _> = self
            .document
            .document()
            .classifiers
            .iter()
            .map(|item| (item.id.clone(), item.clone()))
            .collect();
        let mut out: BTreeMap<ScopeKey, BTreeMap<String, CompiledPresentation>> = BTreeMap::new();
        for (index, item) in items.iter().enumerate() {
            if !item.enabled {
                continue;
            }
            let base = path("presentations", index);
            let scope = ScopeKey::from(&item.scope);
            match &item.source {
                PresentationSource::ExactOffer {
                    binding_id,
                    model_id,
                } => {
                    let Some(binding) = self.binding(
                        binding_id,
                        base.clone().field(f("source")).field(f("binding_id")),
                        item.id.as_stable_id(),
                    ) else {
                        continue;
                    };
                    if !self.route_exists(protocol(&item.scope), binding) {
                        self.error(
                            "binding-not-routable-from-scope",
                            base.clone().field(f("source")).field(f("binding_id")),
                            item.id.as_stable_id(),
                        );
                        continue;
                    }
                    if self.inventory.get(binding_id, model_id).is_none() {
                        self.info(
                            "offer-unknown",
                            base.clone().field(f("source")).field(f("model_id")),
                            item.id.as_stable_id(),
                        );
                    }
                    self.insert_presentation(
                        &mut out,
                        scope,
                        item.slug.clone(),
                        CompiledPresentation {
                            id: item.id.clone(),
                            route: PresentationRoute::Exact {
                                binding_id: binding_id.clone(),
                                model_id: model_id.clone(),
                            },
                            metadata: item.metadata.clone(),
                        },
                        &base,
                        item.id.as_stable_id(),
                    );
                }
                PresentationSource::Virtual { target } => {
                    let Some(target) = self.route_reference(
                        target,
                        None,
                        &base.clone().field(f("source")).field(f("target")),
                        routes,
                        compiled_routes,
                    ) else {
                        continue;
                    };
                    self.validate_route_scope(
                        &item.scope,
                        &target.route_id,
                        compiled_routes,
                        base.clone().field(f("source")),
                    );
                    self.insert_presentation(
                        &mut out,
                        scope,
                        item.slug.clone(),
                        CompiledPresentation {
                            id: item.id.clone(),
                            route: PresentationRoute::Reference(target),
                            metadata: item.metadata.clone(),
                        },
                        &base,
                        item.id.as_stable_id(),
                    );
                }
                PresentationSource::OfferSelector { selector, .. } => {
                    let selector_path = base.clone().field(f("source")).field(f("selector"));
                    let Some(compiled) = self.selector(
                        &selector.include,
                        &selector.exclude,
                        &selector.ordering,
                        &selector_path,
                        item.id.as_stable_id(),
                    ) else {
                        continue;
                    };
                    if let SelectorOrdering::ClassifiedVersion { classifier_id } =
                        &selector.ordering
                        && !classifiers.contains_key(classifier_id)
                    {
                        self.error(
                            "missing-classifier",
                            selector_path.clone().field(f("ordering")),
                            item.id.as_stable_id(),
                        );
                        continue;
                    }
                    let Some(binding) = self.binding(
                        &selector.binding_id,
                        selector_path.clone().field(f("binding_id")),
                        item.id.as_stable_id(),
                    ) else {
                        continue;
                    };
                    if !self.route_exists(protocol(&item.scope), binding) {
                        self.error(
                            "binding-not-routable-from-scope",
                            selector_path.clone().field(f("binding_id")),
                            item.id.as_stable_id(),
                        );
                        continue;
                    }
                    if let SelectorOrdering::ClassifiedVersion { classifier_id } =
                        &selector.ordering
                        && classifier_raw
                            .get(classifier_id)
                            .is_some_and(|item| item.binding_id != selector.binding_id)
                    {
                        self.error(
                            "classifier-binding-mismatch",
                            selector_path.clone().field(f("ordering")),
                            item.id.as_stable_id(),
                        );
                        continue;
                    }
                    let selected = super::selector::select(
                        self.inventory,
                        Some(&selector.binding_id),
                        &compiled,
                        usize::from(selector.keep),
                        |classifier_id, offer| {
                            classifier_raw
                                .get(classifier_id)
                                .filter(|classifier| classifier.binding_id == offer.binding_id)
                                .and_then(|classifier| {
                                    classify_version(classifier, &offer.backend_model_id)
                                })
                        },
                    );
                    if selected.is_empty() {
                        self.info("selector-unresolved", selector_path, item.id.as_stable_id());
                    }
                    for offer in selected {
                        if !self.route_exists(protocol(&item.scope), &offer.binding) {
                            continue;
                        }
                        let template_parameters: BTreeSet<_> = [
                            StableId::new("model_id").expect("static id"),
                            StableId::new("provider_id").expect("static id"),
                            StableId::new("binding_id").expect("static id"),
                        ]
                        .into_iter()
                        .collect();
                        let captures = Default::default();
                        let Some(slug_template) = self.template(
                            &item.slug,
                            &template_parameters,
                            &captures,
                            base.clone().field(f("slug")),
                            item.id.as_stable_id(),
                        ) else {
                            break;
                        };
                        let values = [
                            (
                                StableId::new("model_id").expect("static id"),
                                offer.backend_model_id.clone(),
                            ),
                            (
                                StableId::new("provider_id").expect("static id"),
                                offer.provider.clone(),
                            ),
                            (
                                StableId::new("binding_id").expect("static id"),
                                offer.binding_id.to_string(),
                            ),
                        ]
                        .into_iter()
                        .collect();
                        let empty_captures = CompiledMatcher::Exact(String::new())
                            .captures("")
                            .expect("bounded")
                            .expect("exact match");
                        let Some(slug) = slug_template.render(&values, &empty_captures).ok() else {
                            self.error(
                                "invalid-template",
                                base.clone().field(f("slug")),
                                item.id.as_stable_id(),
                            );
                            break;
                        };
                        self.insert_presentation(
                            &mut out,
                            scope.clone(),
                            slug,
                            CompiledPresentation {
                                id: item.id.clone(),
                                route: PresentationRoute::Exact {
                                    binding_id: offer.binding_id.clone(),
                                    model_id: offer.backend_model_id.clone(),
                                },
                                metadata: item.metadata.clone(),
                            },
                            &base,
                            item.id.as_stable_id(),
                        );
                    }
                }
            }
        }
        out
    }

    fn insert_presentation(
        &mut self,
        out: &mut BTreeMap<ScopeKey, BTreeMap<String, CompiledPresentation>>,
        scope: ScopeKey,
        slug: String,
        presentation: CompiledPresentation,
        path: &DiagnosticPath,
        id: &StableId,
    ) {
        if out
            .entry(scope)
            .or_default()
            .insert(slug, presentation)
            .is_some()
        {
            self.error(
                "duplicate-presentation-slug",
                path.clone().field(f("slug")),
                id,
            );
        }
    }

    fn compile_compaction(
        &mut self,
        routes: &HashMap<RouteId, usize>,
        compiled_routes: &BTreeMap<RouteId, CompiledRoute>,
    ) -> Option<CompiledCompaction> {
        let item: CompactionPolicy = self.document.document().compaction.clone()?;
        let base = DiagnosticPath::root().field(f("compaction"));
        let target = self.route_reference(
            &item.target,
            None,
            &base.clone().field(f("target")),
            routes,
            compiled_routes,
        )?;
        Some(CompiledCompaction {
            target,
            lane_transition: item.lane_transition,
            cache_transition: item.cache_transition,
        })
    }

    fn route_reference(
        &mut self,
        reference: &RouteReference,
        rewrite: Option<(&MatcherSpec, &CaptureSchema)>,
        path: &DiagnosticPath,
        routes: &HashMap<RouteId, usize>,
        compiled_routes: &BTreeMap<RouteId, CompiledRoute>,
    ) -> Option<CompiledRouteReference> {
        if !routes.contains_key(&reference.route_id) {
            self.error(
                "missing-route",
                path.clone().field(f("route_id")),
                reference.route_id.as_stable_id(),
            );
            return None;
        }
        let route = compiled_routes.get(&reference.route_id)?;
        let supplied: BTreeSet<_> = reference.parameters.keys().cloned().collect();
        if supplied != route.parameters {
            self.error(
                "route-arguments-mismatch",
                path.clone().field(f("parameters")),
                reference.route_id.as_stable_id(),
            );
            return None;
        }
        for (parameter, binding) in &reference.parameters {
            match binding {
                ArgumentBinding::RegexCaptureName { name } => {
                    let Some((MatcherSpec::Regex(_), captures)) = rewrite else {
                        self.error(
                            "capture-binding-not-regex",
                            path.clone().field(f("parameters")),
                            parameter,
                        );
                        continue;
                    };
                    if !captures.named().any(|capture| capture == name) {
                        self.error(
                            "missing-regex-capture",
                            path.clone().field(f("parameters")),
                            parameter,
                        );
                    }
                }
                ArgumentBinding::RegexCaptureIndex { index } => {
                    let Some((MatcherSpec::Regex(_), captures)) = rewrite else {
                        self.error(
                            "capture-binding-not-regex",
                            path.clone().field(f("parameters")),
                            parameter,
                        );
                        continue;
                    };
                    if usize::try_from(*index)
                        .ok()
                        .is_none_or(|index| !captures.numbered().any(|capture| capture == index))
                    {
                        self.error(
                            "missing-regex-capture",
                            path.clone().field(f("parameters")),
                            parameter,
                        );
                    }
                }
                ArgumentBinding::Literal { .. } | ArgumentBinding::RequestedModel => {}
            }
        }
        Some(CompiledRouteReference {
            route_id: reference.route_id.clone(),
            parameters: reference.parameters.clone(),
        })
    }

    fn validate_route_scope(
        &mut self,
        scope: &FrontendScope,
        route_id: &RouteId,
        routes: &BTreeMap<RouteId, CompiledRoute>,
        path: DiagnosticPath,
    ) {
        let Some(route) = routes.get(route_id) else {
            return;
        };
        for candidate in &route.candidates {
            let Some(binding) = self.topology.binding(&candidate.binding_id) else {
                continue;
            };
            if !self.route_exists(protocol(scope), binding) {
                self.error(
                    "binding-not-routable-from-scope",
                    path.clone(),
                    route_id.as_stable_id(),
                );
            }
        }
    }

    fn validate_ordering(
        &mut self,
        ordering: &SelectorOrdering,
        classifiers: &HashMap<ClassifierId, usize>,
        binding_id: &BindingId,
        path: &DiagnosticPath,
        record: &StableId,
        classifier_by_id: &HashMap<ClassifierId, FamilyClassifier>,
    ) {
        if let SelectorOrdering::ClassifiedVersion { classifier_id } = ordering {
            if !classifiers.contains_key(classifier_id) {
                self.error(
                    "missing-classifier",
                    path.clone().field(f("ordering")),
                    record,
                );
            } else if classifier_by_id
                .get(classifier_id)
                .is_some_and(|item| &item.binding_id != binding_id)
            {
                self.error(
                    "classifier-binding-mismatch",
                    path.clone().field(f("ordering")),
                    record,
                );
            }
        }
    }

    fn selector(
        &mut self,
        include: &[MatcherSpec],
        exclude: &[MatcherSpec],
        ordering: &SelectorOrdering,
        path: &DiagnosticPath,
        record: &StableId,
    ) -> Option<CompiledSelector> {
        let include = include
            .iter()
            .enumerate()
            .map(|(index, matcher)| {
                self.matcher(
                    matcher,
                    path.clone()
                        .field(f("include"))
                        .index(index)
                        .expect("bounded"),
                    record,
                )
            })
            .collect::<Option<Vec<_>>>()?;
        let exclude = exclude
            .iter()
            .enumerate()
            .map(|(index, matcher)| {
                self.matcher(
                    matcher,
                    path.clone()
                        .field(f("exclude"))
                        .index(index)
                        .expect("bounded"),
                    record,
                )
            })
            .collect::<Option<Vec<_>>>()?;
        Some(CompiledSelector {
            include,
            exclude,
            ordering: ordering.clone(),
        })
    }

    fn aliases(
        &mut self,
        alias: Option<&AliasPolicy>,
        path: &DiagnosticPath,
        record: &StableId,
    ) -> Option<Vec<CompiledAliasRule>> {
        let Some(alias) = alias else {
            return Some(vec![]);
        };
        let mut out = Vec::new();
        for (index, rule) in alias.rules.iter().enumerate() {
            let base = path
                .clone()
                .field(f("alias_policy"))
                .field(f("rules"))
                .index(index)
                .expect("bounded");
            let matcher = self.matcher(&rule.matcher, base.clone().field(f("matcher")), record)?;
            let exclude = rule
                .exclude
                .iter()
                .enumerate()
                .map(|(exclude_index, matcher)| {
                    self.matcher(
                        matcher,
                        base.clone()
                            .field(f("exclude"))
                            .index(exclude_index)
                            .expect("bounded"),
                        record,
                    )
                })
                .collect::<Option<Vec<_>>>()?;
            let replacement = self.template(
                &rule.replacement,
                &Default::default(),
                &matcher.capture_schema(),
                base.field(f("replacement")),
                record,
            )?;
            out.push(CompiledAliasRule {
                matcher,
                exclude,
                replacement,
            });
        }
        Some(out)
    }

    fn matcher(
        &mut self,
        spec: &MatcherSpec,
        path: DiagnosticPath,
        record: &StableId,
    ) -> Option<CompiledMatcher> {
        match spec.compile() {
            Ok(value) => Some(value),
            Err(_) => {
                self.error("invalid-matcher", path, record);
                None
            }
        }
    }

    fn template(
        &mut self,
        source: &str,
        parameters: &BTreeSet<StableId>,
        captures: &CaptureSchema,
        path: DiagnosticPath,
        record: &StableId,
    ) -> Option<Template> {
        match Template::parse(source, parameters, captures) {
            Ok(value) => Some(value),
            Err(_) => {
                self.error("invalid-template", path, record);
                None
            }
        }
    }

    fn binding(
        &mut self,
        id: &BindingId,
        path: DiagnosticPath,
        record: &StableId,
    ) -> Option<&'static BackendBinding> {
        match self.topology.binding(id) {
            Some(binding) => Some(binding),
            None => {
                self.error("missing-binding", path, record);
                None
            }
        }
    }

    fn capabilities_satisfy(&self, binding: &BackendBinding, candidate: &Candidate) -> bool {
        let Some(backend) = binding.canonical_backend() else {
            return false;
        };
        let actual: Capabilities = backend.capabilities();
        let required = candidate.required_capabilities;
        (!required.sampling || actual.sampling)
            && (!required.system_in_messages || actual.system_in_messages)
            && (!required.thinking_replay || actual.thinking_replay)
            && (!required.images || actual.images)
            && (!required.tools || actual.tools)
    }

    fn route_exists(&self, frontend: ProtocolId, binding: &BackendBinding) -> bool {
        self.topology
            .routes()
            .any(|route| route.frontend() == frontend && route.binding().id() == binding.id())
    }

    fn error(&mut self, code: &str, path: DiagnosticPath, record: &StableId) {
        self.failed = true;
        self.diagnostics
            .push(diagnostic(code, Severity::Error, path, Some(record)));
    }

    fn info(&mut self, code: &str, path: DiagnosticPath, record: &StableId) {
        self.diagnostics
            .push(diagnostic(code, Severity::Info, path, Some(record)));
    }
}

fn classify_version(classifier: &FamilyClassifier, model: &str) -> Option<ParsedVersion> {
    let matcher = classifier.model_matcher.compile().ok()?;
    let captures = matcher.captures(model).ok()??;
    let empty = BTreeMap::new();
    let family = Template::parse(
        &classifier.family_template,
        &Default::default(),
        &matcher.capture_schema(),
    )
    .ok()?
    .render(&empty, &captures)
    .ok()?;
    if family.is_empty() {
        return None;
    }
    let version = Template::parse(
        &classifier.version_template,
        &Default::default(),
        &matcher.capture_schema(),
    )
    .ok()?
    .render(&empty, &captures)
    .ok()?;
    classifier.version_parser.parse(&version).ok()
}

fn protocol(scope: &FrontendScope) -> ProtocolId {
    match scope {
        FrontendScope::Profile { protocol, .. } | FrontendScope::Protocol { protocol } => {
            (*protocol).into()
        }
    }
}

fn diagnostic(
    code: &str,
    severity: Severity,
    path: DiagnosticPath,
    record: Option<&StableId>,
) -> Diagnostic {
    Diagnostic {
        code: DiagnosticCode(
            StableId::new(code).expect("compiler diagnostic codes are stable ids"),
        ),
        severity,
        record_id: record.cloned(),
        path,
    }
}

fn f(name: &'static str) -> DiagnosticField {
    DiagnosticField::new(name)
}
fn path(collection: &'static str, index: usize) -> DiagnosticPath {
    DiagnosticPath::root()
        .field(f(collection))
        .index(index)
        .expect("validated collection bound")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::policy::*;
    use crate::providers::Provider;
    use crate::routing::{
        ANTHROPIC_API_MESSAGES, ANTHROPIC_SUB_MESSAGES, CODEX_SUB_RESPONSES, OPENAI_API_CHAT,
    };

    fn topology() -> RouteRegistry {
        RouteRegistry::new(
            std::iter::empty::<Arc<dyn Provider>>(),
            std::iter::empty::<(ProtocolId, String)>(),
        )
    }

    fn empty_inventory() -> OfferInventory {
        OfferInventory::default()
    }

    fn id(value: &str) -> StableId {
        StableId::new(value).unwrap()
    }
    fn route_id(value: &str) -> RouteId {
        RouteId::new(value).unwrap()
    }
    fn candidate_id(value: &str) -> CandidateId {
        CandidateId::new(value).unwrap()
    }

    fn scope() -> FrontendScope {
        FrontendScope::Protocol {
            protocol: FrontendProtocol::AnthropicMessages,
        }
    }

    fn reference(route: &str) -> RouteReference {
        RouteReference {
            route_id: route_id(route),
            parameters: BTreeMap::from([(
                id(REQUESTED_MODEL_PARAMETER),
                ArgumentBinding::RequestedModel,
            )]),
        }
    }

    fn candidate(binding: &BackendBinding) -> Candidate {
        Candidate {
            id: candidate_id("primary"),
            binding_id: binding.id().clone(),
            model: CandidateModel::Exact {
                model_id: "model-not-content".to_owned(),
            },
            upgrade_policy_id: None,
            alias_policy: None,
            required_capabilities: Default::default(),
            enabled: true,
            health_circuits: vec![],
        }
    }

    fn edge() -> CandidateEdge {
        CandidateEdge {
            preflight: FallbackPermission::Enabled,
            post_send: vec![],
            billed_boundary: BoundaryConsent::Deny,
            ambiguous_transport: BoundaryConsent::Deny,
            cache_transition: CacheTransitionPolicy::AllowCacheLoss,
        }
    }

    fn document() -> PolicyDocument {
        PolicyDocument {
            format_version: CURRENT_FORMAT_VERSION,
            description: None,
            defaults: vec![DefaultRoute {
                scope: scope(),
                target: reference("route"),
            }],
            presentations: vec![Presentation {
                id: PresentationId::new("presented").unwrap(),
                scope: scope(),
                slug: "friendly".to_owned(),
                label: None,
                description: None,
                enabled: true,
                shadow_native: false,
                source: PresentationSource::Virtual {
                    target: reference("route"),
                },
                metadata: None,
            }],
            routes: vec![Route {
                id: route_id("route"),
                display_name: "Route".to_owned(),
                description: None,
                parameters: vec![id(REQUESTED_MODEL_PARAMETER)],
                chain: RouteChain {
                    first: candidate(&ANTHROPIC_SUB_MESSAGES),
                    steps: vec![],
                },
                lane_transition: LaneTransitionPolicy::PreservePinned,
            }],
            rewrites: vec![],
            classifiers: vec![],
            upgrades: vec![],
            compaction: None,
        }
    }

    fn compile_document(document: PolicyDocument, origin: PolicyOrigin) -> Compilation {
        compile(
            document.validate().unwrap(),
            &topology(),
            &empty_inventory(),
            origin,
        )
    }

    fn codes(compilation: &Compilation) -> Vec<&str> {
        compilation
            .diagnostics()
            .iter()
            .map(|item| item.code.0.as_str())
            .collect()
    }

    #[test]
    fn authored_document_compiles_to_closed_lookup_indexes() {
        let compilation = compile_document(document(), PolicyOrigin::Authored);
        let policy = compilation.policy().expect("compiled");
        assert!(policy.route(&route_id("route")).is_some());
        assert!(policy.presentation(&scope(), "friendly").is_some());
        assert!(policy.default_route(&scope()).is_some());
        assert_eq!(
            codes(&compilation),
            ["binding-unconfigured", "offer-unknown"]
        );
    }

    #[test]
    fn cross_reference_scope_template_and_argument_errors_are_structured() {
        let mut cases: Vec<(&str, PolicyDocument)> = Vec::new();

        let mut missing_route = document();
        missing_route.defaults[0].target.route_id = route_id("absent");
        cases.push(("missing-route", missing_route));

        let mut arguments = document();
        arguments.defaults[0].target.parameters.clear();
        cases.push(("route-arguments-mismatch", arguments));

        let mut capture = document();
        capture.defaults[0].target.parameters.insert(
            id(REQUESTED_MODEL_PARAMETER),
            ArgumentBinding::RegexCaptureIndex { index: 1 },
        );
        cases.push(("capture-binding-not-regex", capture));

        let mut template = document();
        template.routes[0].chain.first.model = CandidateModel::Template {
            template: "${undeclared}".to_owned(),
        };
        cases.push(("invalid-template", template));

        let mut binding = document();
        binding.routes[0].chain.first.binding_id = BindingId::new("missing.binding").unwrap();
        cases.push(("missing-binding", binding));

        let mut incompatible = document();
        incompatible.routes[0].chain.first.binding_id = OPENAI_API_CHAT.id().clone();
        cases.push(("binding-not-routable-from-scope", incompatible));

        for (expected, document) in cases {
            let result = compile_document(document, PolicyOrigin::Authored);
            assert!(
                result.policy().is_none(),
                "{expected} unexpectedly compiled"
            );
            assert!(
                codes(&result).contains(&expected),
                "missing {expected}: {:?}",
                codes(&result)
            );
        }
    }

    #[test]
    fn matcher_alias_classifier_upgrade_and_legacy_learning_errors_are_rejected() {
        let mut matcher = document();
        matcher.rewrites.push(RewriteRule {
            id: RewriteId::new("bad-matcher").unwrap(),
            scope: scope(),
            priority: 1,
            model_matcher: MatcherSpec::Regex("(".to_owned()),
            target: reference("route"),
        });
        assert!(
            codes(&compile_document(matcher, PolicyOrigin::Authored)).contains(&"invalid-matcher")
        );

        let mut duplicate_priority = document();
        for name in ["one", "two"] {
            duplicate_priority.rewrites.push(RewriteRule {
                id: RewriteId::new(name).unwrap(),
                scope: scope(),
                priority: 7,
                model_matcher: MatcherSpec::Exact(name.to_owned()),
                target: reference("route"),
            });
        }
        assert!(
            codes(&compile_document(
                duplicate_priority,
                PolicyOrigin::Authored
            ))
            .contains(&"duplicate-rewrite-priority")
        );

        let mut alias = document();
        alias.routes[0].chain.first.alias_policy = Some(AliasPolicy {
            rules: vec![AliasRule {
                matcher: MatcherSpec::Regex("(".to_owned()),
                exclude: vec![],
                replacement: "target".to_owned(),
            }],
        });
        assert!(
            codes(&compile_document(alias, PolicyOrigin::Authored)).contains(&"invalid-matcher")
        );

        let mut upgrade = document();
        upgrade.upgrades.push(UpgradePolicy {
            id: UpgradePolicyId::new("upgrade").unwrap(),
            classifier_id: ClassifierId::new("absent").unwrap(),
            enabled: true,
            minimum_learning: MinimumLearning::None,
            allow_manual_promotion: false,
            inventory_sources: vec![],
            lane_transition: LaneTransitionPolicy::PreservePinned,
            allow_revision_one_legacy_learning: false,
        });
        assert!(
            codes(&compile_document(upgrade, PolicyOrigin::Authored))
                .contains(&"missing-classifier")
        );

        let mut legacy = document();
        legacy.classifiers.push(classifier(&ANTHROPIC_SUB_MESSAGES));
        legacy.upgrades.push(UpgradePolicy {
            id: UpgradePolicyId::new("upgrade").unwrap(),
            classifier_id: ClassifierId::new("classifier").unwrap(),
            enabled: true,
            minimum_learning: MinimumLearning::None,
            allow_manual_promotion: false,
            inventory_sources: vec![],
            lane_transition: LaneTransitionPolicy::PreservePinned,
            allow_revision_one_legacy_learning: true,
        });
        assert!(
            codes(&compile_document(legacy.clone(), PolicyOrigin::Authored))
                .contains(&"legacy-learning-forbidden")
        );
        assert!(
            compile_document(legacy, PolicyOrigin::GeneratedRevisionOne)
                .policy()
                .is_some()
        );
    }

    fn classifier(binding: &BackendBinding) -> FamilyClassifier {
        FamilyClassifier {
            id: ClassifierId::new("classifier").unwrap(),
            binding_id: binding.id().clone(),
            model_matcher: MatcherSpec::Regex(r"^(?P<f>[a-z]+)-(?P<v>[0-9]+)$".to_owned()),
            family_template: "${f}".to_owned(),
            version_template: "${v}".to_owned(),
            version_parser: VersionParserSpec::NumericTuple {
                components: 1,
                separator: "-".to_owned(),
            },
        }
    }

    #[test]
    fn route_edges_enforce_cost_ambiguous_transport_and_cache_consent() {
        let build = |to: &BackendBinding, edge: CandidateEdge| {
            let mut document = document();
            let mut backup = candidate(to);
            backup.id = candidate_id("backup");
            document.routes[0].chain.steps.push(RouteStep {
                edge,
                candidate: backup,
            });
            document
        };

        let billed = compile_document(
            build(&ANTHROPIC_API_MESSAGES, edge()),
            PolicyOrigin::Authored,
        );
        assert!(codes(&billed).contains(&"billed-boundary-consent-required"));

        let mut ambiguous_edge = edge();
        ambiguous_edge.billed_boundary = BoundaryConsent::Allow;
        ambiguous_edge
            .post_send
            .push(FailureClass::AmbiguousTransport);
        let ambiguous = compile_document(
            build(&ANTHROPIC_API_MESSAGES, ambiguous_edge),
            PolicyOrigin::Authored,
        );
        assert!(codes(&ambiguous).contains(&"ambiguous-transport-consent-required"));

        let mut cache_edge = edge();
        cache_edge.billed_boundary = BoundaryConsent::Allow;
        cache_edge.cache_transition = CacheTransitionPolicy::PreserveWarm;
        let cache = compile_document(
            build(&ANTHROPIC_API_MESSAGES, cache_edge),
            PolicyOrigin::Authored,
        );
        assert!(codes(&cache).contains(&"cache-loss-consent-required"));

        let mut allowed = edge();
        allowed.billed_boundary = BoundaryConsent::Allow;
        let allowed = compile_document(
            build(&ANTHROPIC_API_MESSAGES, allowed),
            PolicyOrigin::Authored,
        );
        assert!(allowed.policy().is_some());
    }

    #[test]
    fn classifier_binding_capabilities_and_duplicate_scopes_are_checked() {
        let mut classifier_mismatch = document();
        classifier_mismatch
            .classifiers
            .push(classifier(&ANTHROPIC_API_MESSAGES));
        classifier_mismatch.upgrades.push(UpgradePolicy {
            id: UpgradePolicyId::new("upgrade").unwrap(),
            classifier_id: ClassifierId::new("classifier").unwrap(),
            enabled: true,
            minimum_learning: MinimumLearning::None,
            allow_manual_promotion: false,
            inventory_sources: vec![],
            lane_transition: LaneTransitionPolicy::PreservePinned,
            allow_revision_one_legacy_learning: false,
        });
        classifier_mismatch.routes[0].chain.first.upgrade_policy_id =
            Some(UpgradePolicyId::new("upgrade").unwrap());
        assert!(
            codes(&compile_document(
                classifier_mismatch,
                PolicyOrigin::Authored
            ))
            .contains(&"classifier-binding-mismatch")
        );

        let mut capability = document();
        capability.routes[0].chain.first.binding_id = CODEX_SUB_RESPONSES.id().clone();
        capability.routes[0]
            .chain
            .first
            .required_capabilities
            .sampling = true;
        assert!(
            codes(&compile_document(capability, PolicyOrigin::Authored))
                .contains(&"capability-unavailable")
        );

        let mut defaults = document();
        defaults.defaults.push(defaults.defaults[0].clone());
        assert!(
            codes(&compile_document(defaults, PolicyOrigin::Authored))
                .contains(&"duplicate-default-scope")
        );

        let mut presentations = document();
        let mut duplicate = presentations.presentations[0].clone();
        duplicate.id = PresentationId::new("other").unwrap();
        presentations.presentations.push(duplicate);
        assert!(
            codes(&compile_document(presentations, PolicyOrigin::Authored))
                .contains(&"duplicate-presentation-slug")
        );
    }

    #[test]
    fn diagnostics_are_deterministic_and_do_not_copy_model_content() {
        let first = compile_document(document(), PolicyOrigin::Authored);
        let second = compile_document(document(), PolicyOrigin::Authored);
        let first = serde_json::to_string(first.diagnostics()).unwrap();
        let second = serde_json::to_string(second.diagnostics()).unwrap();
        assert_eq!(first, second);
        assert!(!first.contains("model-not-content"));
    }
}
