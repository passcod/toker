//! Stable serialisable vocabulary for routing policy documents.
//!
//! The JSON structs are editable DTOs. Only [`ValidatedPolicyDocument`] may
//! cross into fingerprinting or, later, compilation. Because every struct uses
//! `deny_unknown_fields`, adding even an optional field changes which inputs
//! are accepted and therefore requires an explicit format-version evolution.

use std::borrow::Cow;
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

pub const MINIMUM_FORMAT_VERSION: u32 = 1;
pub const CURRENT_FORMAT_VERSION: u32 = 3;
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
    #[error("binding identifier must be `<provider-id>.<dialect-id>`")]
    InvalidBinding,
}

macro_rules! category_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(transparent)]
        pub struct $name(StableId);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                StableId::new(value).map(Self)
            }

            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }

            pub fn as_stable_id(&self) -> &StableId {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                self.0.serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                StableId::deserialize(deserializer).map(Self)
            }
        }
    };
}

/// A closed backend binding identity.
///
/// Runtime topology constructs these from the verified provider and dialect.
/// Policy documents still parse the same stable spelling, but reject ids that
/// cannot denote exactly one `<provider-id>.<dialect-id>` binding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct BindingId(Cow<'static, str>);

impl BindingId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        Self::validate(value.into()).map(Cow::Owned).map(Self)
    }

    pub const fn from_static(value: &'static str) -> Self {
        Self(Cow::Borrowed(value))
    }

    pub fn from_parts(provider_id: &str, dialect_id: &str) -> Result<Self, IdError> {
        Self::new(format!("{provider_id}.{dialect_id}"))
    }

    fn validate(value: String) -> Result<String, IdError> {
        let mut parts = value.split('.');
        let provider = parts.next().unwrap_or_default();
        let dialect = parts.next().unwrap_or_default();
        if provider.is_empty() || dialect.is_empty() || parts.next().is_some() {
            return Err(IdError::InvalidBinding);
        }
        StableId::new(value.clone())?;
        Ok(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BindingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for BindingId {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for BindingId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BindingId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

category_id!(RouteId);
category_id!(CandidateId);
category_id!(ClassifierId);
category_id!(UpgradePolicyId);
category_id!(PresentationId);
category_id!(RewriteId);
category_id!(ProfileId);

/// Editable policy DTO. Validate it before any authoritative use.
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

/// A policy whose complete intrinsic shape and bounds have been checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ValidatedPolicyDocument(PolicyDocument);

impl ValidatedPolicyDocument {
    pub fn document(&self) -> &PolicyDocument {
        &self.0
    }

    pub fn into_document(self) -> PolicyDocument {
        self.0
    }
}

impl PolicyDocument {
    /// Validate intrinsic bounds and identities. Cross-reference and topology
    /// checks that need external binding data belong to the later compiler.
    pub fn validate(&self) -> Result<ValidatedPolicyDocument, DocumentError> {
        self.validate_inner()?;
        Ok(ValidatedPolicyDocument(self.clone()))
    }

    fn validate_inner(&self) -> Result<(), DocumentError> {
        if !(MINIMUM_FORMAT_VERSION..=CURRENT_FORMAT_VERSION).contains(&self.format_version) {
            return Err(DocumentError::UnsupportedFormat(self.format_version));
        }
        if self.format_version < 2
            && self
                .routes
                .iter()
                .flat_map(|route| route.chain.candidates())
                .any(|candidate| candidate.required_capabilities.uses_tool_choice_shape())
        {
            return Err(DocumentError::FeatureRequiresFormat {
                path: "/routes/*/chain/*/required_capabilities".to_owned(),
                required: 2,
            });
        }
        if self.format_version < 3
            && self.compaction.as_ref().is_some_and(|compaction| {
                matches!(
                    compaction.target,
                    CompactionTarget::RevisionOneLegacy { .. }
                )
            })
        {
            return Err(DocumentError::FeatureRequiresFormat {
                path: "/compaction/target".to_owned(),
                required: 3,
            });
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
            item.validate(&indexed("/defaults", index))?;
        }
        for (index, item) in self.presentations.iter().enumerate() {
            item.validate(&indexed("/presentations", index))?;
        }
        for (index, item) in self.routes.iter().enumerate() {
            item.validate(&indexed("/routes", index))?;
        }
        for (index, item) in self.rewrites.iter().enumerate() {
            item.validate(&indexed("/rewrites", index))?;
        }
        for (index, item) in self.classifiers.iter().enumerate() {
            item.validate(&indexed("/classifiers", index))?;
        }
        for (index, item) in self.upgrades.iter().enumerate() {
            item.validate(&indexed("/upgrades", index))?;
        }
        if let Some(compaction) = &self.compaction {
            compaction.validate("/compaction")?;
        }
        Ok(())
    }
}

trait Identified {
    type Id: Eq + std::hash::Hash + Clone + fmt::Display;
    fn id(&self) -> &Self::Id;
}

fn unique_ids<T: Identified>(items: &[T], path: &str) -> Result<(), DocumentError> {
    let mut seen = HashSet::new();
    for item in items {
        if !seen.insert(item.id()) {
            return Err(DocumentError::DuplicateId {
                path: path.to_owned(),
                id: item.id().to_string(),
            });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendProtocol {
    AnthropicMessages,
    OpenaiChat,
    OpenaiResponses,
}

impl From<crate::routing::ProtocolId> for FrontendProtocol {
    fn from(protocol: crate::routing::ProtocolId) -> Self {
        match protocol {
            crate::routing::ProtocolId::AnthropicMessages => Self::AnthropicMessages,
            crate::routing::ProtocolId::OpenAiChat => Self::OpenaiChat,
            crate::routing::ProtocolId::OpenAiResponses => Self::OpenaiResponses,
        }
    }
}

impl From<FrontendProtocol> for crate::routing::ProtocolId {
    fn from(protocol: FrontendProtocol) -> Self {
        match protocol {
            FrontendProtocol::AnthropicMessages => Self::AnthropicMessages,
            FrontendProtocol::OpenaiChat => Self::OpenAiChat,
            FrontendProtocol::OpenaiResponses => Self::OpenAiResponses,
        }
    }
}

/// Profile policy wins over protocol policy for the same frontend.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrontendScope {
    Profile {
        profile: ProfileId,
        protocol: FrontendProtocol,
    },
    Protocol {
        protocol: FrontendProtocol,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteReference {
    pub route_id: RouteId,
    #[serde(default)]
    pub parameters: BTreeMap<StableId, ArgumentBinding>,
}

impl RouteReference {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        let parameters_path = field(path, "parameters");
        if self.parameters.len() > MAX_ROUTE_PARAMETERS {
            return Err(DocumentError::CollectionTooLarge(parameters_path));
        }
        for (parameter, binding) in &self.parameters {
            binding.validate(&keyed(&parameters_path, parameter.as_str()))?;
        }
        Ok(())
    }
}

/// One typed source for a declared route parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArgumentBinding {
    Literal { value: String },
    RequestedModel,
    RegexCaptureName { name: String },
    RegexCaptureIndex { index: u32 },
}

impl ArgumentBinding {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        match self {
            Self::Literal { value } => check_string(value, &field(path, "value")),
            Self::RegexCaptureName { name } => check_string(name, &field(path, "name")),
            Self::RegexCaptureIndex { index: 0 } => {
                Err(DocumentError::InvalidValue(field(path, "index")))
            }
            Self::RequestedModel | Self::RegexCaptureIndex { .. } => Ok(()),
        }
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
        self.target.validate(&field(path, "target"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Presentation {
    pub id: PresentationId,
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
    type Id = PresentationId;

    fn id(&self) -> &Self::Id {
        &self.id
    }
}

impl Presentation {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_template(&self.slug, &field(path, "slug"))?;
        check_optional_string(&self.label, &field(path, "label"))?;
        check_optional_description(&self.description, &field(path, "description"))?;
        self.source.validate(&field(path, "source"))?;
        if let Some(AdapterMetadata::Claude {
            behaves_as: Some(behaves_as),
        }) = &self.metadata
        {
            check_string(behaves_as, &field(&field(path, "metadata"), "behaves_as"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PresentationSource {
    ExactOffer {
        binding_id: BindingId,
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
            Self::ExactOffer { model_id, .. } => check_string(model_id, &field(path, "model_id")),
            Self::OfferSelector {
                selector,
                label_template,
            } => {
                selector.validate(&field(path, "selector"))?;
                if let Some(template) = label_template {
                    check_template(template, &field(path, "label_template"))?;
                }
                Ok(())
            }
            Self::Virtual { target } => target.validate(&field(path, "target")),
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
    /// Restrict selection to one stable backend namespace.
    pub binding_id: BindingId,
    #[serde(default)]
    pub include: Vec<MatcherSpec>,
    #[serde(default)]
    pub exclude: Vec<MatcherSpec>,
    pub ordering: SelectorOrdering,
    pub keep: u16,
}

impl OfferSelector {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        validate_selector(&self.include, &self.exclude, &self.ordering, path)?;
        if self.keep == 0 {
            return Err(DocumentError::InvalidValue(field(path, "keep")));
        }
        Ok(())
    }
}

/// A selector whose type guarantees that exactly one offer is retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateSelector {
    #[serde(default)]
    pub include: Vec<MatcherSpec>,
    #[serde(default)]
    pub exclude: Vec<MatcherSpec>,
    pub ordering: SelectorOrdering,
}

impl CandidateSelector {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        validate_selector(&self.include, &self.exclude, &self.ordering, path)
    }
}

fn validate_selector(
    include: &[MatcherSpec],
    exclude: &[MatcherSpec],
    ordering: &SelectorOrdering,
    path: &str,
) -> Result<(), DocumentError> {
    let include_path = field(path, "include");
    let exclude_path = field(path, "exclude");
    check_collection(include, &include_path)?;
    check_collection(exclude, &exclude_path)?;
    for (index, matcher) in include.iter().enumerate() {
        check_matcher(matcher, &indexed(&include_path, index))?;
    }
    for (index, matcher) in exclude.iter().enumerate() {
        check_matcher(matcher, &indexed(&exclude_path, index))?;
    }
    if let SelectorOrdering::Explicit { model_ids } = ordering {
        let model_ids_path = field(&field(path, "ordering"), "model_ids");
        check_collection(model_ids, &model_ids_path)?;
        for (index, model_id) in model_ids.iter().enumerate() {
            check_string(model_id, &indexed(&model_ids_path, index))?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SelectorOrdering {
    CatalogueChronology,
    ClassifiedVersion { classifier_id: ClassifierId },
    Explicit { model_ids: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub id: RouteId,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Vec<StableId>,
    pub chain: RouteChain,
    pub lane_transition: LaneTransitionPolicy,
}

impl Identified for Route {
    type Id = RouteId;

    fn id(&self) -> &Self::Id {
        &self.id
    }
}

impl Route {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_string(&self.display_name, &field(path, "display_name"))?;
        check_optional_description(&self.description, &field(path, "description"))?;
        let parameters_path = field(path, "parameters");
        if self.parameters.len() > MAX_ROUTE_PARAMETERS {
            return Err(DocumentError::CollectionTooLarge(parameters_path));
        }
        let mut parameters = HashSet::new();
        for (index, parameter) in self.parameters.iter().enumerate() {
            if !parameters.insert(parameter) {
                return Err(DocumentError::DuplicateId {
                    path: indexed(&parameters_path, index),
                    id: parameter.to_string(),
                });
            }
        }
        self.chain.validate(&field(path, "chain"))
    }
}

/// A non-empty candidate chain. Every candidate after the first is paired with
/// the edge that grants transition to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteChain {
    pub first: Candidate,
    #[serde(default)]
    pub steps: Vec<RouteStep>,
}

impl RouteChain {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        if self.steps.len() >= MAX_ROUTE_CANDIDATES {
            return Err(DocumentError::CollectionTooLarge(field(path, "steps")));
        }
        let mut ids = HashSet::new();
        if !ids.insert(&self.first.id) {
            unreachable!("the first insertion cannot be a duplicate");
        }
        self.first.validate(&field(path, "first"))?;
        let steps_path = field(path, "steps");
        for (index, step) in self.steps.iter().enumerate() {
            let step_path = indexed(&steps_path, index);
            if !ids.insert(&step.candidate.id) {
                return Err(DocumentError::DuplicateId {
                    path: field(&step_path, "candidate"),
                    id: step.candidate.id.to_string(),
                });
            }
            step.validate(&step_path)?;
        }
        Ok(())
    }

    pub fn candidates(&self) -> impl Iterator<Item = &Candidate> {
        std::iter::once(&self.first).chain(self.steps.iter().map(|step| &step.candidate))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteStep {
    pub edge: CandidateEdge,
    pub candidate: Candidate,
}

impl RouteStep {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        self.edge.validate(&field(path, "edge"))?;
        self.candidate.validate(&field(path, "candidate"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub id: CandidateId,
    pub binding_id: BindingId,
    pub model: CandidateModel,
    #[serde(default)]
    pub upgrade_policy_id: Option<UpgradePolicyId>,
    #[serde(default)]
    pub alias_policy: Option<AliasPolicy>,
    #[serde(default)]
    pub required_capabilities: CapabilityFlags,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub health_circuits: Vec<HealthCircuit>,
}

impl Candidate {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        self.model.validate(&field(path, "model"))?;
        if let Some(policy) = &self.alias_policy {
            policy.validate(&field(path, "alias_policy"))?;
        }
        let circuits_path = field(path, "health_circuits");
        check_collection(&self.health_circuits, &circuits_path)?;
        let mut failures = HashSet::new();
        for (index, circuit) in self.health_circuits.iter().enumerate() {
            if !failures.insert(circuit.failure) {
                return Err(DocumentError::DuplicateId {
                    path: circuits_path,
                    id: format!("{:?}", circuit.failure).to_ascii_lowercase(),
                });
            }
            if circuit.duration_seconds == 0 {
                return Err(DocumentError::InvalidValue(field(
                    &indexed(&circuits_path, index),
                    "duration_seconds",
                )));
            }
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
    OfferSelector { selector: CandidateSelector },
}

impl CandidateModel {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        match self {
            Self::Exact { model_id } => check_string(model_id, &field(path, "model_id")),
            Self::RequestedModel => Ok(()),
            Self::Template { template } => check_template(template, &field(path, "template")),
            Self::OfferSelector { selector } => selector.validate(&field(path, "selector")),
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
        let rules_path = field(path, "rules");
        check_collection(&self.rules, &rules_path)?;
        for (index, rule) in self.rules.iter().enumerate() {
            let rule_path = indexed(&rules_path, index);
            check_matcher(&rule.matcher, &field(&rule_path, "matcher"))?;
            let exclude_path = field(&rule_path, "exclude");
            check_collection(&rule.exclude, &exclude_path)?;
            for (exclude_index, matcher) in rule.exclude.iter().enumerate() {
                check_matcher(matcher, &indexed(&exclude_path, exclude_index))?;
            }
            check_template(&rule.replacement, &field(&rule_path, "replacement"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasRule {
    pub matcher: MatcherSpec,
    #[serde(default)]
    pub exclude: Vec<MatcherSpec>,
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
    #[serde(default, skip_serializing_if = "is_false")]
    pub tool_choice_none: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub tool_choice_named: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub tool_choice_unnamed: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub tool_choice_other: bool,
}

impl CapabilityFlags {
    fn uses_tool_choice_shape(self) -> bool {
        self.tool_choice_none
            || self.tool_choice_named
            || self.tool_choice_unnamed
            || self.tool_choice_other
    }
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
        check_collection(&self.post_send, &field(path, "post_send"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackPermission {
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    pub id: RewriteId,
    pub scope: FrontendScope,
    pub priority: i32,
    pub model_matcher: MatcherSpec,
    pub target: RouteReference,
}

impl Identified for RewriteRule {
    type Id = RewriteId;

    fn id(&self) -> &Self::Id {
        &self.id
    }
}

impl RewriteRule {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_matcher(&self.model_matcher, &field(path, "model_matcher"))?;
        self.target.validate(&field(path, "target"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FamilyClassifier {
    pub id: ClassifierId,
    pub binding_id: BindingId,
    pub model_matcher: MatcherSpec,
    pub family_template: String,
    pub version_template: String,
    pub version_parser: VersionParserSpec,
    /// Generated revision-one compatibility for legacy ids whose family and
    /// numeric version segments may be interleaved.
    #[serde(default, skip_serializing_if = "is_false")]
    pub legacy_hyphenated: bool,
}

impl Identified for FamilyClassifier {
    type Id = ClassifierId;

    fn id(&self) -> &Self::Id {
        &self.id
    }
}

impl FamilyClassifier {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_matcher(&self.model_matcher, &field(path, "model_matcher"))?;
        check_template(&self.family_template, &field(path, "family_template"))?;
        check_template(&self.version_template, &field(path, "version_template"))?;
        if let VersionParserSpec::NumericTuple { separator, .. }
        | VersionParserSpec::LegacyNumericTuple { separator } = &self.version_parser
        {
            check_string(
                separator,
                &field(&field(path, "version_parser"), "separator"),
            )?;
        }
        self.version_parser
            .validate()
            .map_err(|_| DocumentError::InvalidValue(field(path, "version_parser")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpgradePolicy {
    pub id: UpgradePolicyId,
    pub classifier_id: ClassifierId,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub minimum_learning: MinimumLearning,
    #[serde(default)]
    pub allow_manual_promotion: bool,
    #[serde(default)]
    pub inventory_sources: Vec<InventorySource>,
    pub lane_transition: LaneTransitionPolicy,
    /// Nothing reads this and revision one no longer sets it. It stays so a
    /// stored revision one that carries it still decodes and keeps the
    /// fingerprint it was activated under; the compiler refuses it from an
    /// authored document.
    #[serde(default)]
    pub allow_revision_one_legacy_learning: bool,
}

impl Identified for UpgradePolicy {
    type Id = UpgradePolicyId;

    fn id(&self) -> &Self::Id {
        &self.id
    }
}

impl UpgradePolicy {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        check_collection(&self.inventory_sources, &field(path, "inventory_sources"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MinimumLearning {
    None,
    DistinctDays {
        days: u16,
    },
    ExplicitPromotion,
    /// The revision-one election bar: strictly more than
    /// `min(7, active_days / 2)` distinct days.
    ///
    /// This exists only to preserve revision one's adaptive election bar over
    /// binding-local learning. Authored policy cannot select it.
    RevisionOneAdaptive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventorySource {
    Catalogue,
    Served,
    Learned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionPolicy {
    pub target: CompactionTarget,
    pub lane_transition: LaneTransitionPolicy,
    pub cache_transition: CacheTransitionPolicy,
}

impl CompactionPolicy {
    fn validate(&self, path: &str) -> Result<(), DocumentError> {
        match &self.target {
            CompactionTarget::Route(target) => target.validate(&field(path, "target")),
            CompactionTarget::RevisionOneLegacy {
                model_spec,
                eligible_bindings,
            } => {
                check_string(model_spec, &field(path, "model_spec"))?;
                check_collection(eligible_bindings, &field(path, "eligible_bindings"))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionTarget {
    Route(RouteReference),
    /// Revision-one compatibility overlay. Authored policy cannot select it.
    RevisionOneLegacy {
        model_spec: String,
        eligible_bindings: Vec<BindingId>,
    },
}

impl Serialize for CompactionTarget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Route(target) => target.serialize(serializer),
            Self::RevisionOneLegacy {
                model_spec,
                eligible_bindings,
            } => RevisionOneCompactionTarget {
                kind: RevisionOneCompactionKind::RevisionOneLegacy,
                model_spec: model_spec.clone(),
                eligible_bindings: eligible_bindings.clone(),
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for CompactionTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("kind").is_some() {
            let legacy: RevisionOneCompactionTarget =
                serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            return Ok(Self::RevisionOneLegacy {
                model_spec: legacy.model_spec,
                eligible_bindings: legacy.eligible_bindings,
            });
        }
        serde_json::from_value(value)
            .map(Self::Route)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionOneCompactionTarget {
    kind: RevisionOneCompactionKind,
    model_spec: String,
    eligible_bindings: Vec<BindingId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RevisionOneCompactionKind {
    RevisionOneLegacy,
}

fn default_true() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !value
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

fn field(path: &str, name: &str) -> String {
    format!("{path}/{name}")
}

fn indexed(path: &str, index: usize) -> String {
    format!("{path}/{index}")
}

fn keyed(path: &str, key: &str) -> String {
    let escaped = key.replace('~', "~0").replace('/', "~1");
    format!("{path}/{escaped}")
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DocumentError {
    #[error("unsupported policy format version {0}")]
    UnsupportedFormat(u32),
    #[error("policy feature at {path} requires format version {required}")]
    FeatureRequiresFormat { path: String, required: u32 },
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
    #[error("duplicate id {id} at {path}")]
    DuplicateId { path: String, id: String },
    #[error("invalid value at {0}")]
    InvalidValue(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_and_category_ids_preserve_validated_spelling() {
        for valid in ["a", "model-1", "openrouter.messages", "a_b"] {
            assert_eq!(StableId::new(valid).unwrap().as_str(), valid);
            assert_eq!(RouteId::new(valid).unwrap().as_str(), valid);
        }
        for invalid in ["", "Upper", "has space", "slash/name", "é"] {
            assert!(StableId::new(invalid).is_err(), "accepted {invalid:?}");
        }
        let route = RouteId::new("same").unwrap();
        let candidate = CandidateId::new("same").unwrap();
        assert_eq!(serde_json::to_string(&route).unwrap(), r#""same""#);
        assert_eq!(serde_json::to_string(&candidate).unwrap(), r#""same""#);
    }

    #[test]
    fn binding_ids_require_exactly_provider_and_dialect() {
        let id = BindingId::new("openrouter.openrouter_messages").unwrap();
        assert_eq!(id.as_str(), "openrouter.openrouter_messages");
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            r#""openrouter.openrouter_messages""#
        );
        assert_eq!(
            serde_json::from_str::<BindingId>(r#""openrouter.openrouter_messages""#).unwrap(),
            id
        );
        for invalid in ["openrouter", ".messages", "openrouter.", "a.b.c"] {
            assert!(BindingId::new(invalid).is_err(), "accepted {invalid:?}");
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
    fn route_chain_is_nonempty_and_pairs_edges_with_destinations() {
        let route = Route {
            id: RouteId::new("route").unwrap(),
            display_name: "Route".to_owned(),
            description: None,
            parameters: vec![],
            chain: RouteChain {
                first: candidate("one"),
                steps: vec![RouteStep {
                    edge: edge(),
                    candidate: candidate("two"),
                }],
            },
            lane_transition: LaneTransitionPolicy::PreservePinned,
        };
        assert_eq!(route.chain.candidates().count(), 2);
        assert!(route.validate("/routes/0").is_ok());
    }

    #[test]
    fn document_description_has_its_wider_exact_bound() {
        let mut document = empty_document();
        document.description = Some("x".repeat(MAX_STRING_BYTES));
        assert!(document.validate().is_ok());
        document.description = Some("x".repeat(MAX_STRING_BYTES + 1));
        assert!(document.validate().is_ok());
        document.description = Some("x".repeat(MAX_DESCRIPTION_BYTES));
        assert!(document.validate().is_ok());
        document.description = Some("x".repeat(MAX_DESCRIPTION_BYTES + 1));
        assert!(matches!(
            document.validate(),
            Err(DocumentError::DescriptionTooLong(path)) if path == "/description"
        ));
    }

    #[test]
    fn nested_string_errors_report_the_specific_path() {
        let mut document = empty_document();
        document.routes.push(Route {
            id: RouteId::new("route").unwrap(),
            display_name: "Route".to_owned(),
            description: None,
            parameters: vec![],
            chain: RouteChain {
                first: Candidate {
                    model: CandidateModel::Exact {
                        model_id: "x".repeat(MAX_STRING_BYTES + 1),
                    },
                    ..candidate("one")
                },
                steps: vec![],
            },
            lane_transition: LaneTransitionPolicy::PreservePinned,
        });
        assert!(matches!(
            document.validate(),
            Err(DocumentError::StringTooLong(path))
                if path == "/routes/0/chain/first/model/model_id"
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
            id: CandidateId::new(id).unwrap(),
            binding_id: BindingId::new("provider.dialect").unwrap(),
            model: CandidateModel::RequestedModel,
            upgrade_policy_id: None,
            alias_policy: None,
            required_capabilities: CapabilityFlags::default(),
            enabled: true,
            health_circuits: vec![],
        }
    }

    #[test]
    fn candidate_health_circuits_require_one_positive_duration_per_failure() {
        let mut candidate = candidate("candidate");
        candidate.health_circuits = vec![
            HealthCircuit {
                failure: FailureClass::Overload,
                duration_seconds: 10,
            },
            HealthCircuit {
                failure: FailureClass::Overload,
                duration_seconds: 20,
            },
        ];
        let mut document = empty_document();
        document.routes.push(Route {
            id: RouteId::new("route").unwrap(),
            display_name: "route".to_owned(),
            description: None,
            parameters: vec![],
            chain: RouteChain {
                first: candidate,
                steps: vec![],
            },
            lane_transition: LaneTransitionPolicy::PreservePinned,
        });
        assert!(matches!(
            document.validate(),
            Err(DocumentError::DuplicateId { path, .. })
                if path == "/routes/0/chain/first/health_circuits"
        ));
        document.routes[0].chain.first.health_circuits[1].failure = FailureClass::ServerError;
        document.routes[0].chain.first.health_circuits[1].duration_seconds = 0;
        assert!(matches!(
            document.validate(),
            Err(DocumentError::InvalidValue(path))
                if path == "/routes/0/chain/first/health_circuits/1/duration_seconds"
        ));
    }

    fn edge() -> CandidateEdge {
        CandidateEdge {
            preflight: FallbackPermission::Enabled,
            post_send: vec![],
            billed_boundary: BoundaryConsent::Deny,
            ambiguous_transport: BoundaryConsent::Deny,
            cache_transition: CacheTransitionPolicy::PreserveWarm,
        }
    }
}
