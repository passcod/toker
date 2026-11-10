//! Final in-memory compiler for routing policy documents.
//!
//! Compilation closes every document reference against static route topology
//! and one immutable offer snapshot. Provider configuration and catalogue
//! presence are evidence, not document validity, so they produce non-blocking
//! diagnostics only.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use sha2::{Digest, Sha256};

use crate::catalog::model_identity;
use crate::catalog::offers::{OfferInventory, derivable_offers};
use crate::routing::{BackendBinding, Capabilities, ProtocolId, RouteRegistry};

use super::canonical::generated_route_fingerprint;
use super::{
    AdapterMetadata, AliasPolicy, ArgumentBinding, BindingId, BoundaryConsent,
    CacheTransitionPolicy, Candidate, CandidateEdge, CandidateId, CandidateModel, CaptureSchema,
    ClassifierId, CompactionPolicy, CompactionTarget, CompiledMatcher, CostClass, Diagnostic,
    DiagnosticCode, DiagnosticField, DiagnosticPath, FailureClass, FamilyClassifier,
    FrontendProtocol, FrontendScope, LaneTransitionPolicy, MatcherError, MatcherSpec,
    ParsedVersion, PolicyFingerprint, Presentation, PresentationId, PresentationSource, ProfileId,
    REQUESTED_MODEL_PARAMETER, RewriteId, Route, RouteFingerprint, RouteId, RouteReference,
    RouteSemantics, SelectorOrdering, Severity, StableId, SubstituteModel, Template, TemplateError,
    UpgradePolicy, UpgradePolicyId, ValidatedPolicyDocument, VersionParserSpec, policy_fingerprint,
    route_fingerprint,
};

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
    publication: BTreeMap<ScopeKey, BTreeSet<String>>,
    /// The Chat and Responses listings derived from the inventory: slug to
    /// the offer it names. These are published by `/v1/models` and never
    /// matched when routing a request.
    derived_offers: BTreeMap<ScopeKey, BTreeMap<String, (BindingId, String)>>,
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

    pub fn routes(&self) -> impl Iterator<Item = &CompiledRoute> {
        self.routes.values()
    }

    pub fn classifiers(&self) -> impl Iterator<Item = &CompiledClassifier> {
        self.classifiers.values()
    }

    /// Every presentation of every scope, in scope then slug order.
    pub fn all_presentations(
        &self,
    ) -> impl Iterator<Item = (FrontendScope, &str, &CompiledPresentation)> {
        self.presentations.iter().flat_map(|(key, items)| {
            let scope = match &key.profile {
                // Every ScopeKey profile came from a validated ProfileId.
                Some(profile) => FrontendScope::Profile {
                    profile: ProfileId::new(profile).expect("compiled profile id stays valid"),
                    protocol: key.protocol,
                },
                None => FrontendScope::Protocol {
                    protocol: key.protocol,
                },
            };
            items
                .iter()
                .map(move |(slug, presentation)| (scope.clone(), slug.as_str(), presentation))
        })
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

    pub fn upgrades(&self) -> impl Iterator<Item = &CompiledUpgradePolicy> {
        self.upgrades.values()
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

    /// Expand the publication visible to one frontend. Protocol-scoped rows
    /// are inherited, while an exact profile row with the same slug replaces
    /// the inherited row, matching resolver scope precedence.
    pub fn publication(
        &self,
        profile: Option<&ProfileId>,
        protocol: FrontendProtocol,
    ) -> BTreeMap<&str, &CompiledPresentation> {
        let protocol_key = ScopeKey {
            profile: None,
            protocol,
        };
        let mut expanded = self
            .presentations
            .get(&protocol_key)
            .into_iter()
            .flatten()
            .map(|(slug, presentation)| (slug.as_str(), presentation))
            .collect::<BTreeMap<_, _>>();
        if let Some(profile) = profile {
            let profile_key = ScopeKey {
                profile: Some(profile.to_string()),
                protocol,
            };
            if let Some(items) = self.presentations.get(&profile_key) {
                expanded.extend(
                    items
                        .iter()
                        .map(|(slug, presentation)| (slug.as_str(), presentation)),
                );
            }
        }
        expanded.retain(|slug, presentation| {
            let published = self
                .publication
                .get(&protocol_key)
                .is_some_and(|items| items.contains(*slug))
                || profile.is_some_and(|profile| {
                    self.publication
                        .get(&ScopeKey {
                            profile: Some(profile.to_string()),
                            protocol,
                        })
                        .is_some_and(|items| items.contains(*slug))
                });
            presentation.enabled() && published
        });
        expanded
    }

    /// Every named profile with an Anthropic Messages presentation or
    /// publication. The Claude settings adapter compares each of these with
    /// the shared protocol projection before writing its one settings file.
    pub fn anthropic_profiles(&self) -> impl Iterator<Item = ProfileId> + '_ {
        self.presentations
            .keys()
            .chain(self.publication.keys())
            .filter(|scope| scope.protocol == FrontendProtocol::AnthropicMessages)
            .filter_map(|scope| scope.profile.as_deref())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|profile| {
                // Every ScopeKey profile came from a validated ProfileId.
                ProfileId::new(profile).expect("compiled profile id stays valid")
            })
    }

    pub fn publication_slugs(
        &self,
        profile: Option<&ProfileId>,
        protocol: FrontendProtocol,
    ) -> BTreeSet<&str> {
        let protocol_key = ScopeKey {
            profile: None,
            protocol,
        };
        let mut expanded = self
            .publication
            .get(&protocol_key)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if let Some(profile) = profile {
            let profile_key = ScopeKey {
                profile: Some(profile.to_string()),
                protocol,
            };
            if let Some(items) = self.publication.get(&profile_key) {
                expanded.extend(items.iter().map(String::as_str));
            }
        }
        expanded
    }

    /// The derived listing for one protocol. A slug a presentation also
    /// publishes is the presentation's to render.
    pub fn derived_publication(
        &self,
        protocol: FrontendProtocol,
    ) -> impl Iterator<Item = (&str, &BindingId, &str)> {
        self.derived_offers
            .get(&ScopeKey {
                profile: None,
                protocol,
            })
            .into_iter()
            .flatten()
            .map(|(slug, (binding_id, model_id))| (slug.as_str(), binding_id, model_id.as_str()))
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
    honours_server_safeguards: bool,
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
    /// Whether a request that asks for server safeguards requires them of
    /// this route's candidates.
    pub fn honours_server_safeguards(&self) -> bool {
        self.honours_server_safeguards
    }
}

#[derive(Debug)]
pub struct CompiledCandidate {
    id: CandidateId,
    binding_id: BindingId,
    model: CompiledCandidateModel,
    upgrade_policy_id: Option<UpgradePolicyId>,
    alias_rules: Vec<CompiledAliasRule>,
    health_circuits: BTreeMap<FailureClass, u64>,
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
    pub fn health_circuit_seconds(&self, failure: FailureClass) -> Option<u64> {
        self.health_circuits.get(&failure).copied()
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
    slug: String,
    label: Option<String>,
    description: Option<String>,
    enabled: bool,
    shadow_native: bool,
    route: PresentationRoute,
    metadata: Option<AdapterMetadata>,
    /// Where a selector put this offer in its own ordering, from the first,
    /// which the routing page reads as the newest it lists. Zero for a
    /// presentation that is not one of several a selector expanded to.
    rank: usize,
}

impl CompiledPresentation {
    pub fn id(&self) -> &PresentationId {
        &self.id
    }
    pub fn slug(&self) -> &str {
        &self.slug
    }
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn shadow_native(&self) -> bool {
        self.shadow_native
    }
    pub fn route(&self) -> &PresentationRoute {
        &self.route
    }
    pub fn metadata(&self) -> Option<&AdapterMetadata> {
        self.metadata.as_ref()
    }
    pub fn rank(&self) -> usize {
        self.rank
    }
}

#[derive(Debug)]
pub enum PresentationRoute {
    Reference(CompiledRouteReference),
    Exact {
        route_id: RouteId,
        fingerprint: RouteFingerprint,
        candidate_id: CandidateId,
        binding_id: BindingId,
        model_id: String,
    },
}

impl PresentationRoute {
    pub fn exact_offer(&self) -> Option<(&BindingId, &str)> {
        match self {
            Self::Exact {
                binding_id,
                model_id,
                ..
            } => Some((binding_id, model_id)),
            Self::Reference(_) => None,
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClassificationError {
    #[error(transparent)]
    Matcher(#[from] MatcherError),
    #[error(transparent)]
    Template(#[from] TemplateError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedModel {
    pub family: String,
    pub version: ParsedVersion,
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

    /// Whether the matcher accepts `model`'s canonical identity, whether or
    /// not its version then parses.
    pub fn matches(&self, model: &str) -> Result<bool, MatcherError> {
        model_identity(model).map_or(Ok(false), |identity| self.matcher.is_match(&identity))
    }

    /// Classify `model` by its canonical identity, so that a published
    /// dated snapshot takes the family and version of its undated form.
    // spec: RMODEL#classifying-an-identity
    pub fn classify(&self, model: &str) -> Result<Option<ClassifiedModel>, ClassificationError> {
        let Some(identity) = model_identity(model) else {
            return Ok(None);
        };
        let Some(captures) = self.matcher.captures(&identity)? else {
            return Ok(None);
        };
        let empty = BTreeMap::new();
        let family = self.family_template.render(&empty, &captures)?;
        let version = self.version_template.render(&empty, &captures)?;
        Ok(self
            .parser
            .parse(&version)
            .ok()
            .map(|version| ClassifiedModel { family, version }))
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
pub enum CompiledCompaction {
    Route {
        target: CompiledRouteReference,
        lane_transition: LaneTransitionPolicy,
        cache_transition: CacheTransitionPolicy,
    },
    Substitution {
        model: Option<SubstituteModel>,
        bindings: BTreeSet<BindingId>,
    },
}

impl CompiledCompaction {
    pub fn route(
        &self,
    ) -> Option<(
        &CompiledRouteReference,
        LaneTransitionPolicy,
        CacheTransitionPolicy,
    )> {
        match self {
            Self::Route {
                target,
                lane_transition,
                cache_transition,
            } => Some((target, *lane_transition, *cache_transition)),
            Self::Substitution { .. } => None,
        }
    }

    pub fn substitution(&self) -> Option<(Option<&SubstituteModel>, &BTreeSet<BindingId>)> {
        match self {
            Self::Substitution { model, bindings } => Some((model.as_ref(), bindings)),
            Self::Route { .. } => None,
        }
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
) -> Compilation {
    Compiler::new(document, topology, inventory).run()
}

struct Compiler<'a> {
    document: ValidatedPolicyDocument,
    topology: &'a RouteRegistry,
    inventory: &'a OfferInventory,
    diagnostics: Vec<Diagnostic>,
    failed: bool,
}

impl<'a> Compiler<'a> {
    fn new(
        document: ValidatedPolicyDocument,
        topology: &'a RouteRegistry,
        inventory: &'a OfferInventory,
    ) -> Self {
        Self {
            document,
            topology,
            inventory,
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
        let mut publication: BTreeMap<ScopeKey, BTreeSet<String>> = presentations
            .iter()
            .map(|(scope, items)| (scope.clone(), items.keys().cloned().collect()))
            .collect();
        let derived_offers = self.derive_publication(&rewrites, &defaults, &routes);
        for (scope, offers) in &derived_offers {
            publication
                .entry(scope.clone())
                .or_default()
                .extend(offers.keys().cloned());
        }
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
                publication,
                derived_offers,
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
                        honours_server_safeguards: route.honours_server_safeguards,
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
        let health_circuits = candidate
            .health_circuits
            .iter()
            .map(|circuit| (circuit.failure, circuit.duration_seconds))
            .collect();
        Some(CompiledCandidate {
            id: candidate.id.clone(),
            binding_id: candidate.binding_id.clone(),
            model,
            upgrade_policy_id: candidate.upgrade_policy_id.clone(),
            alias_rules,
            health_circuits,
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
            self.codex_listing_diagnostics(item, &base);
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
                    let slug = item.slug.clone();
                    if !self.validate_shadow_native(item, &slug, &base) {
                        continue;
                    }
                    if let Some(presentation) = self.exact_presentation(
                        item,
                        slug.clone(),
                        item.label.clone(),
                        binding_id.clone(),
                        model_id.clone(),
                        &base,
                    ) {
                        self.insert_presentation(
                            &mut out,
                            scope,
                            slug,
                            presentation,
                            &base,
                            item.id.as_stable_id(),
                        );
                    }
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
                    if !self.validate_shadow_native(item, &item.slug, &base) {
                        continue;
                    }
                    self.insert_presentation(
                        &mut out,
                        scope,
                        item.slug.clone(),
                        CompiledPresentation {
                            id: item.id.clone(),
                            slug: item.slug.clone(),
                            label: item.label.clone(),
                            description: item.description.clone(),
                            enabled: item.enabled,
                            shadow_native: item.shadow_native,
                            route: PresentationRoute::Reference(target),
                            metadata: item.metadata.clone(),
                            rank: 0,
                        },
                        &base,
                        item.id.as_stable_id(),
                    );
                }
                PresentationSource::OfferSelector {
                    selector,
                    label_template,
                } => {
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
                    for (rank, offer) in selected.into_iter().enumerate() {
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
                        if !self.validate_shadow_native(item, &slug, &base) {
                            continue;
                        }
                        let label = if let Some(source) = label_template {
                            let Some(template) = self.template(
                                source,
                                &template_parameters,
                                &captures,
                                base.clone().field(f("source")).field(f("label_template")),
                                item.id.as_stable_id(),
                            ) else {
                                break;
                            };
                            let Some(label) = template.render(&values, &empty_captures).ok() else {
                                self.error(
                                    "invalid-template",
                                    base.clone().field(f("source")).field(f("label_template")),
                                    item.id.as_stable_id(),
                                );
                                break;
                            };
                            Some(label)
                        } else {
                            item.label.clone()
                        };
                        if let Some(mut presentation) = self.exact_presentation(
                            item,
                            slug.clone(),
                            label,
                            offer.binding_id.clone(),
                            offer.backend_model_id.clone(),
                            &base,
                        ) {
                            presentation.rank = rank;
                            self.insert_presentation(
                                &mut out,
                                scope.clone(),
                                slug,
                                presentation,
                                &base,
                                item.id.as_stable_id(),
                            );
                        }
                    }
                }
            }
        }
        out
    }

    fn validate_shadow_native(
        &mut self,
        item: &super::Presentation,
        slug: &str,
        path: &DiagnosticPath,
    ) -> bool {
        let collision = self.inventory.iter().any(|offer| {
            self.route_exists(protocol(&item.scope), &offer.binding)
                && (offer.backend_model_id == slug
                    || format!("{}/{}", offer.provider, offer.backend_model_id) == slug)
        });
        if collision && !item.shadow_native {
            self.error(
                "native-presentation-shadow-consent-required",
                path.clone().field(f("shadow_native")),
                item.id.as_stable_id(),
            );
            false
        } else {
            true
        }
    }

    fn exact_presentation(
        &mut self,
        item: &super::Presentation,
        slug: String,
        label: Option<String>,
        binding_id: BindingId,
        model_id: String,
        path: &DiagnosticPath,
    ) -> Option<CompiledPresentation> {
        let suffix = generated_id_suffix(&item.id, &binding_id, &model_id);
        let route_id = RouteId::new(format!("presentation-{suffix}"))
            .expect("hash-derived generated route id");
        let candidate_id = CandidateId::new(format!("candidate-{suffix}"))
            .expect("hash-derived generated candidate id");
        let fingerprint = match generated_route_fingerprint(
            self.document.document().format_version,
            &route_id,
            &candidate_id,
            &binding_id,
            &model_id,
        ) {
            Ok(fingerprint) => fingerprint,
            Err(_) => {
                self.error(
                    "route-semantics-invalid",
                    path.clone().field(f("source")),
                    item.id.as_stable_id(),
                );
                return None;
            }
        };
        Some(CompiledPresentation {
            id: item.id.clone(),
            slug,
            label,
            description: item.description.clone(),
            enabled: item.enabled,
            shadow_native: item.shadow_native,
            route: PresentationRoute::Exact {
                route_id,
                fingerprint,
                candidate_id,
                binding_id,
                model_id,
            },
            metadata: item.metadata.clone(),
            rank: 0,
        })
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
        match item.target {
            CompactionTarget::Route(target) => {
                let target = self.route_reference(
                    &target,
                    None,
                    &base.clone().field(f("target")),
                    routes,
                    compiled_routes,
                )?;
                Some(CompiledCompaction::Route {
                    target,
                    lane_transition: item.lane_transition,
                    cache_transition: item.cache_transition,
                })
            }
            CompactionTarget::Substitution { model, bindings } => {
                let mut compiled = BTreeSet::new();
                for (index, binding_id) in bindings.into_iter().enumerate() {
                    if self
                        .binding(
                            &binding_id,
                            base.clone()
                                .field(f("target"))
                                .field(f("bindings"))
                                .index(index)
                                .expect("validated collection bound"),
                            &StableId::new("compaction").expect("static id"),
                        )
                        .is_some()
                    {
                        compiled.insert(binding_id);
                    }
                }
                Some(CompiledCompaction::Substitution {
                    model,
                    bindings: compiled,
                })
            }
        }
    }

    /// The Chat and Responses slugs this revision derives from the
    /// inventory, each kept only where the revision's own rewrites and
    /// default route it to the binding holding the offer, so a revision lists
    /// what it routes.
    // spec: RPUB#derived-listings
    fn derive_publication(
        &self,
        rewrites: &BTreeMap<ScopeKey, Vec<CompiledRewrite>>,
        defaults: &BTreeMap<ScopeKey, CompiledRouteReference>,
        routes: &BTreeMap<RouteId, CompiledRoute>,
    ) -> BTreeMap<ScopeKey, BTreeMap<String, (BindingId, String)>> {
        let mut out = BTreeMap::new();
        for protocol in [ProtocolId::OpenAiChat, ProtocolId::OpenAiResponses] {
            let key = ScopeKey {
                profile: None,
                protocol: protocol.into(),
            };
            let scope_rewrites = rewrites.get(&key).map(Vec::as_slice).unwrap_or_default();
            let mut listed = BTreeMap::new();
            for offer in derivable_offers(self.topology, self.inventory, protocol) {
                let routed = routed_binding(
                    &offer.routed_model_id,
                    scope_rewrites,
                    defaults.get(&key),
                    routes,
                );
                if routed == Some(offer.binding.id()) {
                    listed.insert(
                        offer.routed_model_id,
                        (offer.binding.id().clone(), offer.backend_model_id),
                    );
                }
            }
            if !listed.is_empty() {
                out.insert(key, listed);
            }
        }
        out
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
        (!required.sampling || !actual.sampling.is_unsupported())
            && (!required.system_in_messages || !actual.system_in_messages.is_unsupported())
            && (!required.thinking_replay || !actual.thinking_replay.is_unsupported())
            && (!required.images || !actual.images.is_unsupported())
            && (!required.tools || !actual.tools.is_unsupported())
            && (!required.tool_choice_none || !actual.tool_choice_none.is_unsupported())
            && (!required.tool_choice_named || !actual.tool_choice_named.is_unsupported())
            && (!required.tool_choice_unnamed || !actual.tool_choice_unnamed.is_unsupported())
            && (!required.tool_choice_other || !actual.tool_choice_other.is_unsupported())
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

    fn warning(&mut self, code: &str, path: DiagnosticPath, record: &StableId) {
        self.diagnostics
            .push(diagnostic(code, Severity::Warning, path, Some(record)));
    }

    /// The Codex client lists a model only from a catalogue entry carrying
    /// fields toker cannot invent. A virtual presentation in a Responses scope
    /// is listed from its `codex` metadata and left out without it; the record
    /// is used for nothing else, so anywhere it cannot be used it is noise.
    fn codex_listing_diagnostics(&mut self, item: &Presentation, base: &DiagnosticPath) {
        let responses = protocol(&item.scope) == ProtocolId::OpenAiResponses;
        let virtual_source = matches!(item.source, PresentationSource::Virtual { .. });
        let record = matches!(
            item.metadata,
            Some(AdapterMetadata::Codex { model: Some(_) })
        );
        if responses && virtual_source && !record {
            self.info(
                "virtual-presentation-unlisted-in-responses",
                base.clone().field(f("metadata")),
                item.id.as_stable_id(),
            );
        } else if record && !(responses && virtual_source) {
            self.warning(
                "codex-model-record-unused",
                base.clone().field(f("metadata")),
                item.id.as_stable_id(),
            );
        }
    }
}

/// The binding a request naming `slug` reaches under one protocol scope's
/// rewrites and default: the first candidate of the route it selects.
fn routed_binding<'p>(
    slug: &str,
    rewrites: &[CompiledRewrite],
    default: Option<&CompiledRouteReference>,
    routes: &'p BTreeMap<RouteId, CompiledRoute>,
) -> Option<&'p BindingId> {
    let reference = match rewrites
        .iter()
        .find(|rewrite| rewrite.matcher().is_match(slug).unwrap_or(false))
    {
        Some(rewrite) => rewrite.target(),
        None => default?,
    };
    routes
        .get(reference.route_id())?
        .candidates()
        .first()
        .map(CompiledCandidate::binding_id)
}

fn generated_id_suffix(
    presentation_id: &PresentationId,
    binding_id: &BindingId,
    model_id: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"toker.presentation-route.v1\0");
    digest.update(presentation_id.as_str().as_bytes());
    digest.update([0]);
    digest.update(binding_id.as_str().as_bytes());
    digest.update([0]);
    digest.update(model_id.as_bytes());
    let digest = digest.finalize();
    let mut suffix = String::with_capacity(24);
    for byte in &digest[..12] {
        use std::fmt::Write as _;
        write!(suffix, "{byte:02x}").expect("writing to string");
    }
    suffix
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
        RouteRegistry::new(std::iter::empty::<Arc<dyn Provider>>())
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
                honours_server_safeguards: false,
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

    fn compile_document(document: PolicyDocument) -> Compilation {
        compile(
            document.validate().unwrap(),
            &topology(),
            &empty_inventory(),
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
        let compilation = compile_document(document());
        let policy = compilation.policy().expect("compiled");
        let route = policy.route(&route_id("route")).expect("route");
        assert_eq!(
            route.candidates()[0].health_circuit_seconds(FailureClass::Overload),
            None
        );
        assert!(policy.presentation(&scope(), "friendly").is_some());
        assert!(policy.default_route(&scope()).is_some());
        assert_eq!(
            codes(&compilation),
            ["binding-unconfigured", "offer-unknown"]
        );
    }

    #[test]
    fn candidate_health_circuit_durations_survive_compilation() {
        let mut document = document();
        document.routes[0].chain.first.health_circuits = vec![HealthCircuit {
            failure: FailureClass::Overload,
            duration_seconds: 17,
        }];
        let compilation = compile_document(document);
        let candidate = &compilation
            .policy()
            .expect("compiled")
            .route(&route_id("route"))
            .expect("route")
            .candidates()[0];
        assert_eq!(
            candidate.health_circuit_seconds(FailureClass::Overload),
            Some(17)
        );
        assert_eq!(
            candidate.health_circuit_seconds(FailureClass::ServerError),
            None
        );
    }

    #[test]
    fn anthropic_profiles_include_presentation_and_publication_scopes_once() {
        let mut document = document();
        document.presentations[0].scope = FrontendScope::Profile {
            profile: ProfileId::new("claude").unwrap(),
            protocol: FrontendProtocol::AnthropicMessages,
        };
        document.presentations.push(Presentation {
            id: PresentationId::new("workhorse-presented").unwrap(),
            scope: FrontendScope::Profile {
                profile: ProfileId::new("workhorse").unwrap(),
                protocol: FrontendProtocol::AnthropicMessages,
            },
            slug: "workhorse-friendly".to_owned(),
            label: None,
            description: None,
            enabled: true,
            shadow_native: false,
            source: PresentationSource::Virtual {
                target: reference("route"),
            },
            metadata: None,
        });
        let policy = compile_document(document).into_policy().unwrap();
        assert_eq!(
            policy
                .anthropic_profiles()
                .map(|profile| profile.to_string())
                .collect::<Vec<_>>(),
            ["claude", "workhorse"]
        );
    }

    #[test]
    fn publication_inherits_protocol_rows_and_deduplicates_profile_collisions() {
        let mut document = document();
        let mut inherited = document.presentations[0].clone();
        inherited.id = PresentationId::new("inherited").unwrap();
        inherited.slug = "shared".to_owned();
        let mut profile = inherited.clone();
        profile.id = PresentationId::new("profile").unwrap();
        profile.scope = FrontendScope::Profile {
            profile: ProfileId::new("opencode").unwrap(),
            protocol: FrontendProtocol::AnthropicMessages,
        };
        let mut profile_only = profile.clone();
        profile_only.id = PresentationId::new("profile-only").unwrap();
        profile_only.slug = "profile-only".to_owned();
        document.presentations = vec![inherited, profile, profile_only];

        let compilation = compile_document(document);
        let policy = compilation.policy().expect("compiled policy");
        let profile = ProfileId::new("opencode").unwrap();
        assert_eq!(
            policy.publication_slugs(Some(&profile), FrontendProtocol::AnthropicMessages),
            BTreeSet::from(["profile-only", "shared"])
        );
        assert_eq!(
            policy.publication_slugs(None, FrontendProtocol::AnthropicMessages),
            BTreeSet::from(["shared"])
        );
        assert_eq!(
            policy
                .publication(Some(&profile), FrontendProtocol::AnthropicMessages)
                .get("shared")
                .map(|presentation| presentation.id().as_str()),
            Some("profile"),
            "the exact-profile record replaces the inherited record"
        );
    }

    fn responses_document() -> PolicyDocument {
        let mut document = document();
        let scope = FrontendScope::Protocol {
            protocol: FrontendProtocol::OpenaiResponses,
        };
        document.defaults[0].scope = scope.clone();
        document.presentations[0].scope = scope;
        document.routes[0].chain.first = candidate(&CODEX_SUB_RESPONSES);
        document
    }

    fn codex_record() -> CodexModelRecord {
        CodexModelRecord {
            description: Some("A model toker routes".to_owned()),
            supported_reasoning_levels: vec![CodexReasoningLevel {
                effort: "low".to_owned(),
                description: "Fast".to_owned(),
            }],
            default_reasoning_level: Some("low".to_owned()),
            visibility: CodexVisibility::List,
            shell_type: CodexShellType::UnifiedExec,
            supported_in_api: true,
            support_verbosity: false,
            priority: 3,
            context_window: Some(100_000),
            max_context_window: None,
        }
    }

    fn codex_offer(model: &str) -> crate::catalog::offers::BackendOffer {
        crate::catalog::offers::BackendOffer {
            binding_id: CODEX_SUB_RESPONSES.id().clone(),
            provider: "codex_sub".to_owned(),
            backend_model_id: model.to_owned(),
            binding: CODEX_SUB_RESPONSES.clone(),
            context_window: Some(272_000),
            chronology: None,
            fetched_at_ms: 1,
            raw: serde_json::json!({"slug": model, "display_name": "Provider's own"}),
        }
    }

    fn responses_listing(
        policy: &CompiledPolicy,
        inventory: &OfferInventory,
    ) -> Option<Vec<serde_json::Value>> {
        match crate::catalog::offers::render_policy(
            policy,
            inventory,
            None,
            FrontendProtocol::OpenaiResponses,
        ) {
            crate::catalog::offers::PolicyPublication::Ready(body) => {
                Some(body["models"].as_array().cloned().unwrap_or_default())
            }
            crate::catalog::offers::PolicyPublication::Unresolved => None,
        }
    }

    #[test]
    fn a_virtual_responses_presentation_is_listed_from_its_codex_record() {
        let mut document = responses_document();
        document.presentations[0].label = Some("Friendly".to_owned());
        document.presentations[0].metadata = Some(AdapterMetadata::Codex {
            model: Some(codex_record()),
        });
        let compilation = compile_document(document);
        assert!(
            !codes(&compilation).contains(&"virtual-presentation-unlisted-in-responses"),
            "{:?}",
            codes(&compilation)
        );
        let policy = compilation.policy().expect("compiled");
        let models = responses_listing(policy, &empty_inventory()).expect("a ready listing");
        assert_eq!(models.len(), 1);
        let entry = &models[0];
        assert_eq!(entry["slug"], "friendly");
        assert_eq!(entry["display_name"], "Friendly");
        // The fields the Codex client's `ModelInfo` requires, so it parses the
        // catalogue instead of discarding it.
        for required in [
            "slug",
            "display_name",
            "supported_reasoning_levels",
            "shell_type",
            "visibility",
            "supported_in_api",
            "priority",
            "support_verbosity",
            "truncation_policy",
            "experimental_supported_tools",
        ] {
            assert!(entry.get(required).is_some(), "missing {required}: {entry}");
        }
        assert_eq!(entry["shell_type"], "unified_exec");
        assert_eq!(entry["visibility"], "list");
        assert_eq!(entry["context_window"], 100_000);
    }

    #[test]
    fn a_virtual_responses_presentation_without_a_record_is_left_out_not_a_failure() {
        let mut document = responses_document();
        let mut exact = document.presentations[0].clone();
        exact.id = PresentationId::new("exact").unwrap();
        exact.slug = "exact-slug".to_owned();
        exact.source = PresentationSource::ExactOffer {
            binding_id: CODEX_SUB_RESPONSES.id().clone(),
            model_id: "gpt-6-sol".to_owned(),
        };
        document.presentations.push(exact);
        let compilation = compile_document(document);
        assert!(codes(&compilation).contains(&"virtual-presentation-unlisted-in-responses"));
        let policy = compilation.policy().expect("compiled");

        let inventory = OfferInventory::from_offers([codex_offer("gpt-6-sol")]);
        let models = responses_listing(policy, &inventory).expect("the list is still answered");
        let slugs: Vec<_> = models
            .iter()
            .map(|entry| entry["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs, ["exact-slug"], "the virtual slug is not advertised");
        assert_eq!(
            models[0]["display_name"], "Provider's own",
            "an exact offer keeps its provider's entry"
        );

        // A missing exact offer is still the listing's one failure.
        assert!(responses_listing(policy, &empty_inventory()).is_none());
    }

    #[test]
    fn a_codex_record_where_it_cannot_be_used_is_a_warning() {
        let mut messages = document();
        messages.presentations[0].metadata = Some(AdapterMetadata::Codex {
            model: Some(codex_record()),
        });
        let mut exact = responses_document();
        exact.presentations[0].source = PresentationSource::ExactOffer {
            binding_id: CODEX_SUB_RESPONSES.id().clone(),
            model_id: "gpt-6-sol".to_owned(),
        };
        exact.presentations[0].metadata = Some(AdapterMetadata::Codex {
            model: Some(codex_record()),
        });
        for document in [messages, exact] {
            let compilation = compile_document(document);
            let warning = compilation
                .diagnostics()
                .iter()
                .find(|item| item.code.0.as_str() == "codex-model-record-unused")
                .expect("the unused record is named");
            assert_eq!(warning.severity, Severity::Warning);
            assert_eq!(warning.path.as_str(), "/presentations/0/metadata");
        }
    }

    #[test]
    fn a_codex_adapter_without_a_record_is_unchanged_and_says_nothing() {
        let mut document = responses_document();
        document.presentations[0].metadata = Some(AdapterMetadata::Codex { model: None });
        let bytes = serde_json::to_string(&document.presentations[0].metadata).unwrap();
        assert_eq!(bytes, r#"{"adapter":"codex"}"#);
        let decoded: Option<AdapterMetadata> = serde_json::from_str(&bytes).unwrap();
        assert_eq!(decoded, Some(AdapterMetadata::Codex { model: None }));
    }

    #[test]
    fn an_invalid_codex_record_is_refused_at_its_path() {
        let mut document = responses_document();
        let mut record = codex_record();
        record.default_reasoning_level = Some("high".to_owned());
        document.presentations[0].metadata = Some(AdapterMetadata::Codex {
            model: Some(record),
        });
        let error = document.validate().unwrap_err().to_string();
        assert!(
            error.contains("/presentations/0/metadata/model/default_reasoning_level"),
            "{error}"
        );
    }

    #[test]
    fn a_virtual_chat_presentation_is_listed_by_slug() {
        let mut document = document();
        document.presentations[0].scope = FrontendScope::Protocol {
            protocol: FrontendProtocol::OpenaiChat,
        };
        document.defaults[0].scope = document.presentations[0].scope.clone();
        document.routes[0].chain.first = candidate(&OPENAI_API_CHAT);
        let compilation = compile_document(document);
        let policy = compilation.policy().expect("compiled");
        let crate::catalog::offers::PolicyPublication::Ready(listing) =
            crate::catalog::offers::render_policy(
                policy,
                &empty_inventory(),
                None,
                FrontendProtocol::OpenaiChat,
            )
        else {
            panic!("a virtual slug needs no inventory");
        };
        assert_eq!(
            listing["data"],
            serde_json::json!([{"id": "friendly", "object": "model", "owned_by": "toker"}])
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
            let result = compile_document(document);
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
    fn reference_errors_are_rejected_and_authored_vocabulary_compiles() {
        let mut matcher = document();
        matcher.rewrites.push(RewriteRule {
            id: RewriteId::new("bad-matcher").unwrap(),
            scope: scope(),
            priority: 1,
            model_matcher: MatcherSpec::Regex("(".to_owned()),
            target: reference("route"),
        });
        assert!(codes(&compile_document(matcher)).contains(&"invalid-matcher"));

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
            codes(&compile_document(duplicate_priority)).contains(&"duplicate-rewrite-priority")
        );

        let mut alias = document();
        alias.routes[0].chain.first.alias_policy = Some(AliasPolicy {
            rules: vec![AliasRule {
                matcher: MatcherSpec::Regex("(".to_owned()),
                exclude: vec![],
                replacement: "target".to_owned(),
            }],
        });
        assert!(codes(&compile_document(alias)).contains(&"invalid-matcher"));

        let mut upgrade = document();
        upgrade.upgrades.push(UpgradePolicy {
            id: UpgradePolicyId::new("upgrade").unwrap(),
            classifier_id: ClassifierId::new("absent").unwrap(),
            enabled: true,
            minimum_learning: MinimumLearning::None,
            allow_manual_promotion: false,
            inventory_sources: vec![],
            lane_transition: LaneTransitionPolicy::PreservePinned,
        });
        assert!(codes(&compile_document(upgrade)).contains(&"missing-classifier"));

        let mut scaled = document();
        scaled.classifiers.push(classifier(&ANTHROPIC_SUB_MESSAGES));
        scaled.upgrades.push(UpgradePolicy {
            id: UpgradePolicyId::new("upgrade").unwrap(),
            classifier_id: ClassifierId::new("classifier").unwrap(),
            enabled: true,
            minimum_learning: MinimumLearning::Scaled,
            allow_manual_promotion: true,
            inventory_sources: vec![],
            lane_transition: LaneTransitionPolicy::PreservePinned,
        });
        scaled.classifiers[0].version_parser = VersionParserSpec::NumericTuple {
            components: None,
            separator: "-".to_owned(),
        };
        scaled.compaction = Some(CompactionPolicy {
            target: CompactionTarget::Substitution {
                model: Some(SubstituteModel::Family("sonnet".to_owned())),
                bindings: vec![ANTHROPIC_SUB_MESSAGES.id().clone()],
            },
            lane_transition: LaneTransitionPolicy::MoveWhenCold,
            cache_transition: CacheTransitionPolicy::AllowWhenCold,
        });
        let scaled = compile_document(scaled);
        assert!(scaled.policy().is_some(), "{:?}", scaled.diagnostics());
    }

    #[test]
    fn classifiers_read_the_canonical_identity() {
        let mut document = document();
        let mut anthropic = classifier(&ANTHROPIC_SUB_MESSAGES);
        anthropic.model_matcher = MatcherSpec::Regex(
            r"^claude-(?P<f>[a-z]+)-(?P<v>[0-9]{1,4}(?:-[0-9]{1,4})*)$".to_owned(),
        );
        anthropic.version_parser = VersionParserSpec::NumericTuple {
            components: None,
            separator: "-".to_owned(),
        };
        document.classifiers.push(anthropic);
        let policy = compile_document(document).into_policy().unwrap();
        let classifier = policy.classifiers().next().unwrap();
        let family = |model: &str| {
            classifier
                .classify(model)
                .unwrap()
                .map(|classified| (classified.family, classified.version.to_string()))
        };
        assert_eq!(
            family("claude-opus-4-5"),
            Some(("opus".into(), "4.5".into()))
        );
        // A published snapshot and a bracketed context suffix fold to the
        // undated identity before matching.
        assert_eq!(
            family("claude-opus-4-5-20251101"),
            Some(("opus".into(), "4.5".into()))
        );
        assert_eq!(
            family("CLAUDE-SONNET-4-5[1m]"),
            Some(("sonnet".into(), "4.5".into()))
        );
        // An unpublished snapshot still carries its date, so it has none.
        assert_eq!(family("claude-opus-4-5-20990101"), None);
        assert!(!classifier.matches("claude-opus-4-5-20990101").unwrap());
        assert!(classifier.matches("claude-opus-4-5-20251101").unwrap());
    }

    fn classifier(binding: &BackendBinding) -> FamilyClassifier {
        FamilyClassifier {
            id: ClassifierId::new("classifier").unwrap(),
            binding_id: binding.id().clone(),
            model_matcher: MatcherSpec::Regex(r"^(?P<f>[a-z]+)-(?P<v>[0-9]+)$".to_owned()),
            family_template: "${f}".to_owned(),
            version_template: "${v}".to_owned(),
            version_parser: VersionParserSpec::NumericTuple {
                components: Some(1),
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

        let billed = compile_document(build(&ANTHROPIC_API_MESSAGES, edge()));
        assert!(codes(&billed).contains(&"billed-boundary-consent-required"));

        let mut ambiguous_edge = edge();
        ambiguous_edge.billed_boundary = BoundaryConsent::Allow;
        ambiguous_edge
            .post_send
            .push(FailureClass::AmbiguousTransport);
        let ambiguous = compile_document(build(&ANTHROPIC_API_MESSAGES, ambiguous_edge));
        assert!(codes(&ambiguous).contains(&"ambiguous-transport-consent-required"));

        let mut cache_edge = edge();
        cache_edge.billed_boundary = BoundaryConsent::Allow;
        cache_edge.cache_transition = CacheTransitionPolicy::PreserveWarm;
        let cache = compile_document(build(&ANTHROPIC_API_MESSAGES, cache_edge));
        assert!(codes(&cache).contains(&"cache-loss-consent-required"));

        let mut allowed = edge();
        allowed.billed_boundary = BoundaryConsent::Allow;
        let allowed = compile_document(build(&ANTHROPIC_API_MESSAGES, allowed));
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
        });
        classifier_mismatch.routes[0].chain.first.upgrade_policy_id =
            Some(UpgradePolicyId::new("upgrade").unwrap());
        assert!(
            codes(&compile_document(classifier_mismatch)).contains(&"classifier-binding-mismatch")
        );

        let mut capability = document();
        capability.routes[0].chain.first.binding_id = CODEX_SUB_RESPONSES.id().clone();
        capability.routes[0]
            .chain
            .first
            .required_capabilities
            .sampling = true;
        assert!(codes(&compile_document(capability)).contains(&"capability-unavailable"));

        let mut unknown_capability = document();
        unknown_capability.routes[0].chain.first.binding_id = OPENAI_API_CHAT.id().clone();
        unknown_capability.routes[0]
            .chain
            .first
            .required_capabilities
            .sampling = true;
        assert!(!codes(&compile_document(unknown_capability)).contains(&"capability-unavailable"));

        let mut defaults = document();
        defaults.defaults.push(defaults.defaults[0].clone());
        assert!(codes(&compile_document(defaults)).contains(&"duplicate-default-scope"));

        let mut presentations = document();
        let mut duplicate = presentations.presentations[0].clone();
        duplicate.id = PresentationId::new("other").unwrap();
        presentations.presentations.push(duplicate);
        assert!(codes(&compile_document(presentations)).contains(&"duplicate-presentation-slug"));
    }

    #[test]
    fn diagnostics_are_deterministic_and_do_not_copy_model_content() {
        let first = compile_document(document());
        let second = compile_document(document());
        let first = serde_json::to_string(first.diagnostics()).unwrap();
        let second = serde_json::to_string(second.diagnostics()).unwrap();
        assert_eq!(first, second);
        assert!(!first.contains("model-not-content"));
    }
}
