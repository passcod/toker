//! Stable serialisable vocabulary for routing policy documents.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::matcher::MatcherSpec;
use super::version::VersionParserSpec;

pub const MAX_DOCUMENT_BYTES: usize = 1_048_576;
pub const MAX_DOCUMENT_DEPTH: usize = 64;
pub const MAX_COLLECTION_ITEMS: usize = 1_024;
pub const MAX_ROUTE_CANDIDATES: usize = 64;
pub const MAX_ROUTE_PARAMETERS: usize = 32;
pub const MAX_ID_BYTES: usize = 128;
pub const MAX_STRING_BYTES: usize = 1_024;
pub const MAX_DESCRIPTION_BYTES: usize = 4_096;
pub const MAX_MATCHER_BYTES: usize = 1_024;
pub const MAX_TEMPLATE_BYTES: usize = 1_024;

pub const CURRENT_FORMAT_VERSION: u32 = 1;
pub const REQUESTED_MODEL_PARAMETER: &str = "requested_model";

/// A stable policy identity with a deliberately narrow spelling.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StableId(String);

impl StableId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdError::Empty);
        }
        if value.len() > MAX_ID_BYTES {
            return Err(IdError::TooLong);
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        }) {
            return Err(IdError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StableId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for StableId {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for StableId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for StableId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IdError {
    #[error("identifier must not be empty")]
    Empty,
    #[error("identifier exceeds its byte limit")]
    TooLong,
    #[error("identifier contains a character outside [a-z0-9-_.]")]
    InvalidCharacter,
}

/// The complete independently parseable policy document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDocument {
    pub format_version: u32,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub defaults: Vec<DefaultRoute>,
    #[serde(default)]
    pub presentations: Vec<Presentation>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub rewrites: Vec<RewriteRule>,
    #[serde(default)]
    pub classifiers: Vec<FamilyClassifier>,
    #[serde(default)]
    pub upgrades: Vec<UpgradePolicy>,
    #[serde(default)]
    pub compaction: Option<CompactionPolicy>,
}

impl PolicyDocument {
    /// Validate intrinsic document bounds and identities. Cross-reference and
    /// topology checks belong to the later compiler.
    pub fn validate(&self) -> Result<(), DocumentError> {
        if self.format_version != CURRENT_FORMAT_VERSION {
            return Err(DocumentError::UnsupportedFormat(self.format_version));
        }
        check_optional_description(&self.description, "/description")?;
        check_collection(&self.defaults, "/defaults")?;
        check_collection(&self.presentations, "/presentations")?;
        check_collection(&self.routes, "/routes")?;
        check_collection(&self.rewrites, "/rewrites")?;
        check_collection(&self.classifiers, "/classifiers")?;
        check_collection(&self.upgrades, "/upgrades")?;
        unique_ids(&self.presentations, "/presentations")?;
        unique_ids(&self.routes, "/routes")?;
        unique_ids(&self.rewrites, "/rewrites")?;
        unique_ids(&self.classifiers, "/classifiers")?;
        unique_ids(&self.upgrades, "/upgrades")?;

        for (index, item) in self.defaults.iter().enumerate() {
            item.validate(&format!("/defaults/{index}"))?;
        }
        for (index, item) in self.presentations.iter().enumerate() {
            item.validate(&format!("/presentations/{index}"))?;
        }
        for (index, item) in self.routes.iter().enumerate() {
            item.validate(&format!("/routes/{index}"))?;
        }
        for (index, item) in self.rewrites.iter().enumerate() {
            item.validate(&format!("/rewrites/{index}"))?;
        }
        for (index, item) in self.classifiers.iter().enumerate() {
            item.validate(&format!("/classifiers/{index}"))?;
        }
        for (index, item) in self.upgrades.iter().enumerate() {
            item.validate(&format!("/upgrades/{index}"))?;
        }
        if let Some(compaction) = &self.compaction {
            compaction.validate("/compaction")?;
        }
        Ok(())
    }
}

trait Identified {
    fn id(&self) -> &StableId;
}

fn unique_ids<T: Identified>(items: &[T], path: &str) -> Result<(), DocumentError> {
    let mut seen = HashSet::new();
    for item in items {
        if !seen.insert(item.id()) {
            return Err(DocumentError::DuplicateId {
                path: path.to_owned(),
                id: item.id().clone(),
            });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendProtocol {
    AnthropicMessages,
    OpenaiChat,
    OpenaiResponses,
}

/// Profile policy wins over protocol policy for the same frontend.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrontendScope {
    Profile {
        profile: StableId,
        protocol: FrontendProtocol,
    },
    Protocol {
        protocol: FrontendProtocol,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteReference {
    pub route_id: StableId,
    #[serde(default)]
    pub parameters: BTreeMap<StableId, String>,
}

impl RouteReference {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        if self.parameters.len() > MAX_ROUTE_PARAMETERS {
            return Err(DocumentError::CollectionTooLarge(path.to_owned()));
        }
        for value in self.parameters.values() {
            check_string(value, path)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultRoute {
    pub scope: FrontendScope,
    pub target: RouteReference,
}

impl DefaultRoute {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        self.target.validate(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Presentation {
    pub id: StableId,
    pub scope: FrontendScope,
    pub slug: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub shadow_native: bool,
    pub source: PresentationSource,
    #[serde(default)]
    pub metadata: Option<AdapterMetadata>,
}

impl Identified for Presentation {
    fn id(&self) -> &StableId {
        &self.id
    }
}

impl Presentation {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_template(&self.slug, path)?;
        check_optional_string(&self.label, path)?;
        check_optional_description(&self.description, path)?;
        self.source.validate(path)?;
        if let Some(AdapterMetadata::Claude {
            behaves_as: Some(behaves_as),
        }) = &self.metadata
        {
            check_string(behaves_as, path)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PresentationSource {
    ExactOffer {
        binding_id: StableId,
        model_id: String,
    },
    OfferSelector {
        selector: OfferSelector,
        #[serde(default)]
        label_template: Option<String>,
    },
    Virtual {
        target: RouteReference,
    },
}

impl PresentationSource {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        match self {
            Self::ExactOffer { model_id, .. } => check_string(model_id, path),
            Self::OfferSelector {
                selector,
                label_template,
            } => {
                selector.validate(path)?;
                if let Some(template) = label_template {
                    check_template(template, path)?;
                }
                Ok(())
            }
            Self::Virtual { target } => target.validate(path),
        }
    }
}

/// Adapter-owned metadata remains typed rather than becoming unvalidated JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "adapter", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdapterMetadata {
    Claude {
        #[serde(default)]
        behaves_as: Option<String>,
    },
    Openai,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfferSelector {
    #[serde(default)]
    pub include: Vec<MatcherSpec>,
    #[serde(default)]
    pub exclude: Vec<MatcherSpec>,
    pub ordering: SelectorOrdering,
    pub keep: u16,
}

impl OfferSelector {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_collection(&self.include, path)?;
        check_collection(&self.exclude, path)?;
        if self.keep == 0 {
            return Err(DocumentError::InvalidValue(path.to_owned()));
        }
        for matcher in self.include.iter().chain(&self.exclude) {
            check_matcher(matcher, path)?;
        }
        if let SelectorOrdering::Explicit { model_ids } = &self.ordering {
            check_collection(model_ids, path)?;
            for model_id in model_ids {
                check_string(model_id, path)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SelectorOrdering {
    CatalogueChronology,
    ClassifiedVersion { classifier_id: StableId },
    Explicit { model_ids: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub id: StableId,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Vec<StableId>,
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub edges: Vec<CandidateEdge>,
    pub lane_transition: LaneTransitionPolicy,
}

impl Identified for Route {
    fn id(&self) -> &StableId {
        &self.id
    }
}

impl Route {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_string(&self.display_name, path)?;
        check_optional_description(&self.description, path)?;
        if self.parameters.len() > MAX_ROUTE_PARAMETERS {
            return Err(DocumentError::CollectionTooLarge(path.to_owned()));
        }
        let mut parameters = HashSet::new();
        for parameter in &self.parameters {
            if !parameters.insert(parameter) {
                return Err(DocumentError::DuplicateId {
                    path: path.to_owned(),
                    id: parameter.clone(),
                });
            }
        }
        if self.candidates.is_empty() || self.candidates.len() > MAX_ROUTE_CANDIDATES {
            return Err(DocumentError::CollectionTooLarge(path.to_owned()));
        }
        if self.edges.len() != self.candidates.len().saturating_sub(1) {
            return Err(DocumentError::InvalidEdgeCount(path.to_owned()));
        }
        unique_ids(&self.candidates, path)?;
        for candidate in &self.candidates {
            candidate.validate(path)?;
        }
        for edge in &self.edges {
            edge.validate(path)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub id: StableId,
    pub binding_id: StableId,
    pub model: CandidateModel,
    #[serde(default)]
    pub upgrade_policy_id: Option<StableId>,
    #[serde(default)]
    pub alias_policy: Option<AliasPolicy>,
    #[serde(default)]
    pub required_capabilities: CapabilityFlags,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub health_circuits: Vec<HealthCircuit>,
}

impl Identified for Candidate {
    fn id(&self) -> &StableId {
        &self.id
    }
}

impl Candidate {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        self.model.validate(path)?;
        if let Some(policy) = &self.alias_policy {
            policy.validate(path)?;
        }
        check_collection(&self.health_circuits, path)?;
        if self
            .health_circuits
            .iter()
            .any(|circuit| circuit.duration_seconds == 0)
        {
            return Err(DocumentError::InvalidValue(path.to_owned()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CandidateModel {
    Exact { model_id: String },
    RequestedModel,
    Template { template: String },
    OfferSelector { selector: OfferSelector },
}

impl CandidateModel {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        match self {
            Self::Exact { model_id } => check_string(model_id, path),
            Self::RequestedModel => Ok(()),
            Self::Template { template } => check_template(template, path),
            Self::OfferSelector { selector } => {
                selector.validate(path)?;
                if selector.keep != 1 {
                    return Err(DocumentError::InvalidValue(path.to_owned()));
                }
                Ok(())
            }
        }
    }
}

/// Aliases apply only after one candidate and its upgrade have been selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasPolicy {
    #[serde(default)]
    pub rules: Vec<AliasRule>,
}

impl AliasPolicy {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_collection(&self.rules, path)?;
        for rule in &self.rules {
            check_matcher(&rule.matcher, path)?;
            check_template(&rule.replacement, path)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasRule {
    pub matcher: MatcherSpec,
    pub replacement: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityFlags {
    #[serde(default)]
    pub sampling: bool,
    #[serde(default)]
    pub system_in_messages: bool,
    #[serde(default)]
    pub thinking_replay: bool,
    #[serde(default)]
    pub images: bool,
    #[serde(default)]
    pub tools: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateEdge {
    pub preflight: FallbackPermission,
    #[serde(default)]
    pub post_send: Vec<FailureClass>,
    pub billed_boundary: BoundaryConsent,
    pub ambiguous_transport: BoundaryConsent,
    pub cache_transition: CacheTransitionPolicy,
}

impl CandidateEdge {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_collection(&self.post_send, path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackPermission {
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Authentication,
    ModelNotFound,
    RateLimit,
    ProviderQuota,
    Overload,
    ServerError,
    TransportBeforeAccept,
    AmbiguousTransport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryConsent {
    Deny,
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostClass {
    Subscription,
    Billed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTransitionPolicy {
    PreserveWarm,
    AllowWhenCold,
    AllowCacheLoss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneTransitionPolicy {
    PreservePinned,
    MoveWhenCold,
    AllowImmediate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationScope {
    NewLanes,
    NewAndColdLanes,
    Immediate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCircuit {
    pub failure: FailureClass,
    pub duration_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewriteRule {
    pub id: StableId,
    pub scope: FrontendScope,
    pub priority: i32,
    pub model_matcher: MatcherSpec,
    pub target: RouteReference,
}

impl Identified for RewriteRule {
    fn id(&self) -> &StableId {
        &self.id
    }
}

impl RewriteRule {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_matcher(&self.model_matcher, path)?;
        self.target.validate(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FamilyClassifier {
    pub id: StableId,
    pub binding_id: StableId,
    pub model_matcher: MatcherSpec,
    pub family_template: String,
    pub version_template: String,
    pub version_parser: VersionParserSpec,
}

impl Identified for FamilyClassifier {
    fn id(&self) -> &StableId {
        &self.id
    }
}

impl FamilyClassifier {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_matcher(&self.model_matcher, path)?;
        check_template(&self.family_template, path)?;
        check_template(&self.version_template, path)?;
        self.version_parser
            .validate()
            .map_err(|_| DocumentError::InvalidValue(path.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpgradePolicy {
    pub id: StableId,
    pub classifier_id: StableId,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub minimum_learning: MinimumLearning,
    #[serde(default)]
    pub allow_manual_promotion: bool,
    #[serde(default)]
    pub inventory_sources: Vec<InventorySource>,
    pub lane_transition: LaneTransitionPolicy,
    #[serde(default)]
    pub allow_revision_one_legacy_learning: bool,
}

impl Identified for UpgradePolicy {
    fn id(&self) -> &StableId {
        &self.id
    }
}

impl UpgradePolicy {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_collection(&self.inventory_sources, path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MinimumLearning {
    None,
    DistinctDays { days: u16 },
    ExplicitPromotion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventorySource {
    Catalogue,
    Served,
    Learned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionPolicy {
    pub target: RouteReference,
    pub lane_transition: LaneTransitionPolicy,
    pub cache_transition: CacheTransitionPolicy,
}

impl CompactionPolicy {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        self.target.validate(path)
    }
}

fn default_true() -> bool {
    true
}

fn check_collection<T>(items: &[T], path: &str) -> Result<(), DocumentError> {
    if items.len() > MAX_COLLECTION_ITEMS {
        return Err(DocumentError::CollectionTooLarge(path.to_owned()));
    }
    Ok(())
}

fn check_string(value: &str, path: &str) -> Result<(), DocumentError> {
    if value.len() > MAX_STRING_BYTES {
        return Err(DocumentError::StringTooLong(path.to_owned()));
    }
    Ok(())
}

fn check_optional_string(value: &Option<String>, path: &str) -> Result<(), DocumentError> {
    if let Some(value) = value {
        check_string(value, path)?;
    }
    Ok(())
}

fn check_optional_description(value: &Option<String>, path: &str) -> Result<(), DocumentError> {
    if value
        .as_ref()
        .is_some_and(|value| value.len() > MAX_DESCRIPTION_BYTES)
    {
        return Err(DocumentError::DescriptionTooLong(path.to_owned()));
    }
    Ok(())
}

fn check_matcher(matcher: &MatcherSpec, path: &str) -> Result<(), DocumentError> {
    if matcher.value().len() > MAX_MATCHER_BYTES {
        return Err(DocumentError::MatcherTooLong(path.to_owned()));
    }
    Ok(())
}

fn check_template(value: &str, path: &str) -> Result<(), DocumentError> {
    if value.len() > MAX_TEMPLATE_BYTES {
        return Err(DocumentError::TemplateTooLong(path.to_owned()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DocumentError {
    #[error("unsupported policy format version {0}")]
    UnsupportedFormat(u32),
    #[error("collection exceeds its limit at {0}")]
    CollectionTooLarge(String),
    #[error("string exceeds its limit at {0}")]
    StringTooLong(String),
    #[error("description exceeds its limit at {0}")]
    DescriptionTooLong(String),
    #[error("matcher exceeds its limit at {0}")]
    MatcherTooLong(String),
    #[error("template exceeds its limit at {0}")]
    TemplateTooLong(String),
    #[error("duplicate id {id} in {path}")]
    DuplicateId { path: String, id: StableId },
    #[error("candidate edge count is invalid at {0}")]
    InvalidEdgeCount(String),
    #[error("invalid value at {0}")]
    InvalidValue(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_ids_accept_only_the_contract_spelling() {
        for valid in ["a", "model-1", "openrouter.messages", "a_b"] {
            assert_eq!(StableId::new(valid).unwrap().as_str(), valid);
        }
        for invalid in ["", "Upper", "has space", "slash/name", "é"] {
            assert!(StableId::new(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn all_struct_fields_are_known_during_deserialization() {
        let json = r#"{
            "format_version":1,
            "defaults":[],"presentations":[],"routes":[],"rewrites":[],
            "classifiers":[],"upgrades":[],"surprise":true
        }"#;
        assert!(serde_json::from_str::<PolicyDocument>(json).is_err());
    }

    #[test]
    fn route_requires_one_edge_between_adjacent_candidates() {
        let route = Route {
            id: StableId::new("route").unwrap(),
            display_name: "Route".to_owned(),
            description: None,
            parameters: vec![],
            candidates: vec![candidate("one"), candidate("two")],
            edges: vec![],
            lane_transition: LaneTransitionPolicy::PreservePinned,
        };
        assert!(matches!(
            route.validate("/routes/0"),
            Err(DocumentError::InvalidEdgeCount(_))
        ));
    }

    #[test]
    fn document_checks_collection_and_text_bounds() {
        let mut document = empty_document();
        document.description = Some("x".repeat(MAX_DESCRIPTION_BYTES + 1));
        assert!(matches!(
            document.validate(),
            Err(DocumentError::DescriptionTooLong(_))
        ));
        document.description = None;
        document.defaults = (0..=MAX_COLLECTION_ITEMS)
            .map(|_| DefaultRoute {
                scope: FrontendScope::Protocol {
                    protocol: FrontendProtocol::AnthropicMessages,
                },
                target: RouteReference {
                    route_id: StableId::new("r").unwrap(),
                    parameters: BTreeMap::new(),
                },
            })
            .collect();
        assert!(matches!(
            document.validate(),
            Err(DocumentError::CollectionTooLarge(_))
        ));
    }

    fn empty_document() -> PolicyDocument {
        PolicyDocument {
            format_version: CURRENT_FORMAT_VERSION,
            description: None,
            defaults: vec![],
            presentations: vec![],
            routes: vec![],
            rewrites: vec![],
            classifiers: vec![],
            upgrades: vec![],
            compaction: None,
        }
    }

    fn candidate(id: &str) -> Candidate {
        Candidate {
            id: StableId::new(id).unwrap(),
            binding_id: StableId::new("provider.dialect").unwrap(),
            model: CandidateModel::RequestedModel,
            upgrade_policy_id: None,
            alias_policy: None,
            required_capabilities: CapabilityFlags::default(),
            enabled: true,
            health_circuits: vec![],
        }
    }
}
