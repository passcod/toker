//! Bounded canonical JSON and domain-separated policy fingerprints.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::document::{
    AliasPolicy, Candidate, CandidateEdge, CandidateId, ClassifierId, FamilyClassifier,
    HealthCircuit, InventorySource, LaneTransitionPolicy, MAX_COLLECTION_ITEMS,
    MAX_DESCRIPTION_BYTES, MAX_DOCUMENT_BYTES, MAX_DOCUMENT_DEPTH, PolicyDocument, Route, RouteId,
    RouteStep, StableId, UpgradePolicy, UpgradePolicyId, ValidatedPolicyDocument,
};

const POLICY_FINGERPRINT_DOMAIN_V1: &[u8] = b"toker.policy.document.v1\0";
const POLICY_FINGERPRINT_DOMAIN_V2: &[u8] = b"toker.policy.document.v2\0";
const POLICY_FINGERPRINT_DOMAIN_V3: &[u8] = b"toker.policy.document.v3\0";
const POLICY_FINGERPRINT_DOMAIN_V4: &[u8] = b"toker.policy.document.v4\0";
// Route semantics evolve independently from the document format. Change this
// domain whenever the closed projection below changes meaning or shape.
const ROUTE_FINGERPRINT_DOMAIN_V1: &[u8] = b"toker.policy.route-semantics.v1\0";
const ROUTE_FINGERPRINT_DOMAIN_V2: &[u8] = b"toker.policy.route-semantics.v2\0";
// A route that honours server safeguards has semantics v2 cannot express, so it
// takes this domain; every other route keeps v2 whatever its document version.
const ROUTE_FINGERPRINT_DOMAIN_V3: &[u8] = b"toker.policy.route-semantics.v3\0";

/// Limits applied before a document enters the runtime compiler.
///
/// Callers may tighten production limits, but values above the declared maxima
/// are clamped and can never weaken them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalLimits {
    pub document_bytes: usize,
    pub depth: usize,
    pub collection_items: usize,
    pub string_bytes: usize,
}

impl CanonicalLimits {
    fn hardened(self) -> Self {
        Self {
            document_bytes: self.document_bytes.min(MAX_DOCUMENT_BYTES),
            depth: self.depth.min(MAX_DOCUMENT_DEPTH),
            collection_items: self.collection_items.min(MAX_COLLECTION_ITEMS),
            // The parser ceiling must admit descriptions; narrower fields are
            // checked by typed document validation.
            string_bytes: self.string_bytes.min(MAX_DESCRIPTION_BYTES),
        }
    }
}

impl Default for CanonicalLimits {
    fn default() -> Self {
        Self {
            document_bytes: MAX_DOCUMENT_BYTES,
            depth: MAX_DOCUMENT_DEPTH,
            collection_items: MAX_COLLECTION_ITEMS,
            string_bytes: MAX_DESCRIPTION_BYTES,
        }
    }
}

/// Parse and validate a bounded policy while duplicate keys and number tokens
/// are still lexically observable.
pub fn decode_policy(
    bytes: &[u8],
    limits: CanonicalLimits,
) -> Result<ValidatedPolicyDocument, CanonicalError> {
    let limits = limits.hardened();
    if bytes.len() > limits.document_bytes {
        return Err(CanonicalError::DocumentTooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| CanonicalError::InvalidUtf8)?;
    JsonBoundsParser::new(text, limits).parse()?;
    let lexical: Value = serde_json::from_str(text)
        .map_err(|error| CanonicalError::TypedDocument(error.to_string()))?;
    let format_version = lexical.get("format_version").and_then(Value::as_u64);
    if format_version == Some(1) && uses_v2_capability_fields(&lexical) {
        return Err(CanonicalError::InvalidDocument(
            "tool-choice capability fields require policy format version 2".to_owned(),
        ));
    }
    if format_version.is_some_and(|version| version < 3)
        && lexical
            .pointer("/compaction/target/kind")
            .is_some_and(|kind| kind == "revision_one_legacy")
    {
        return Err(CanonicalError::InvalidDocument(
            "revision-one compaction target requires policy format version 3".to_owned(),
        ));
    }
    // Deserialize from the original token stream, not through `Value`:
    // arbitrary-precision `Value` uses a private sentinel map internally, and
    // replaying a user object through it would make that sentinel forgeable.
    let document: PolicyDocument = serde_json::from_str(text)
        .map_err(|error| CanonicalError::TypedDocument(error.to_string()))?;
    document
        .validate()
        .map_err(|error| CanonicalError::InvalidDocument(error.to_string()))
}

fn uses_v2_capability_fields(document: &Value) -> bool {
    const V2_FIELDS: [&str; 4] = [
        "tool_choice_none",
        "tool_choice_named",
        "tool_choice_unnamed",
        "tool_choice_other",
    ];
    document
        .get("routes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|route| route.get("chain"))
        .flat_map(|chain| {
            chain.get("first").into_iter().chain(
                chain
                    .get("steps")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|step| step.get("candidate")),
            )
        })
        .filter_map(|candidate| candidate.get("required_capabilities"))
        .any(|required| V2_FIELDS.iter().any(|field| required.get(field).is_some()))
}

fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, CanonicalError> {
    let value = serde_json::to_value(value)
        .map_err(|error| CanonicalError::Serialization(error.to_string()))?;
    let sorted = sort_value(value);
    let bytes = serde_json::to_vec(&sorted)
        .map_err(|error| CanonicalError::Serialization(error.to_string()))?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(CanonicalError::DocumentTooLarge);
    }
    Ok(bytes)
}

fn sort_value(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(sort_value).collect()),
        Value::Object(values) => {
            let sorted: BTreeMap<_, _> = values
                .into_iter()
                .map(|(key, value)| (key, sort_value(value)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        other => other,
    }
}

macro_rules! fingerprint_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name([u8; 32]);

        impl $name {
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(formatter, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl FromStr for $name {
            type Err = FingerprintParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                if value.len() != 64 {
                    return Err(FingerprintParseError);
                }
                let mut bytes = [0; 32];
                for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
                    let high = lower_hex(pair[0]).ok_or(FingerprintParseError)?;
                    let low = lower_hex(pair[1]).ok_or(FingerprintParseError)?;
                    bytes[index] = (high << 4) | low;
                }
                Ok(Self(bytes))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

fn lower_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("fingerprint must be exactly 64 lowercase hexadecimal characters")]
pub struct FingerprintParseError;

fingerprint_type!(PolicyFingerprint);
fingerprint_type!(RouteFingerprint);

pub fn policy_bytes(document: &ValidatedPolicyDocument) -> Result<Vec<u8>, CanonicalError> {
    canonical_bytes(document)
}

pub fn policy_fingerprint(
    document: &ValidatedPolicyDocument,
) -> Result<PolicyFingerprint, CanonicalError> {
    let domain = match document.document().format_version {
        1 => POLICY_FINGERPRINT_DOMAIN_V1,
        2 => POLICY_FINGERPRINT_DOMAIN_V2,
        3 => POLICY_FINGERPRINT_DOMAIN_V3,
        4 => POLICY_FINGERPRINT_DOMAIN_V4,
        version => return Err(CanonicalError::UnsupportedFormat(version)),
    };
    Ok(PolicyFingerprint(hash(domain, &policy_bytes(document)?)))
}

/// Closed, versioned projection of all semantics that can affect one route.
/// Display fields and inventory-selected winners are absent by construction.
#[derive(Debug, Clone, Serialize)]
pub struct RouteSemantics {
    #[serde(skip)]
    format_version: u32,
    route: SemanticRoute,
    aliases: BTreeMap<CandidateId, AliasPolicy>,
    upgrades: Vec<SemanticUpgrade>,
    classifiers: Vec<FamilyClassifier>,
}

impl RouteSemantics {
    /// This is compiler-owned until the compiler slice lands. It takes a
    /// validated document so no unbounded or structurally invalid route can be
    /// fingerprinted.
    pub fn from_document(
        document: &ValidatedPolicyDocument,
        route_id: &RouteId,
    ) -> Result<Self, CanonicalError> {
        let document = document.document();
        let route = document
            .routes
            .iter()
            .find(|route| &route.id == route_id)
            .ok_or_else(|| CanonicalError::MissingSemanticDependency(route_id.to_string()))?;

        let mut aliases = BTreeMap::new();
        let mut upgrade_ids = HashSet::new();
        let mut classifier_ids = HashSet::new();
        for candidate in route.chain.candidates() {
            if let Some(alias) = &candidate.alias_policy {
                aliases.insert(candidate.id.clone(), alias.clone());
            }
            if let Some(upgrade_id) = &candidate.upgrade_policy_id {
                upgrade_ids.insert(upgrade_id.clone());
            }
            if let super::document::CandidateModel::OfferSelector { selector } = &candidate.model
                && let super::document::SelectorOrdering::ClassifiedVersion { classifier_id } =
                    &selector.ordering
            {
                classifier_ids.insert(classifier_id.clone());
            }
        }

        let mut upgrades = Vec::new();
        for id in upgrade_ids {
            let policy = document
                .upgrades
                .iter()
                .find(|policy| policy.id == id)
                .ok_or_else(|| CanonicalError::MissingSemanticDependency(id.to_string()))?;
            classifier_ids.insert(policy.classifier_id.clone());
            upgrades.push(SemanticUpgrade::from(policy));
        }
        upgrades.sort_by(|left, right| left.id.cmp(&right.id));

        let mut classifiers = Vec::new();
        for id in classifier_ids {
            let classifier = document
                .classifiers
                .iter()
                .find(|classifier| classifier.id == id)
                .ok_or_else(|| CanonicalError::MissingSemanticDependency(id.to_string()))?;
            classifiers.push(classifier.clone());
        }
        classifiers.sort_by(|left, right| left.id.cmp(&right.id));

        Ok(Self {
            format_version: document.format_version,
            route: SemanticRoute::from(route),
            aliases,
            upgrades,
            classifiers,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
struct SemanticRoute {
    id: RouteId,
    parameters: Vec<StableId>,
    first: SemanticCandidate,
    steps: Vec<SemanticStep>,
    lane_transition: LaneTransitionPolicy,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    honours_server_safeguards: bool,
}

impl From<&Route> for SemanticRoute {
    fn from(route: &Route) -> Self {
        let mut parameters = route.parameters.clone();
        parameters.sort();
        Self {
            id: route.id.clone(),
            parameters,
            first: SemanticCandidate::from(&route.chain.first),
            steps: route.chain.steps.iter().map(SemanticStep::from).collect(),
            lane_transition: route.lane_transition,
            honours_server_safeguards: route.honours_server_safeguards,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SemanticStep {
    edge: SemanticEdge,
    candidate: SemanticCandidate,
}

impl From<&RouteStep> for SemanticStep {
    fn from(step: &RouteStep) -> Self {
        Self {
            edge: SemanticEdge::from(&step.edge),
            candidate: SemanticCandidate::from(&step.candidate),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SemanticCandidate {
    id: CandidateId,
    binding_id: super::document::BindingId,
    model: super::document::CandidateModel,
    upgrade_policy_id: Option<UpgradePolicyId>,
    required_capabilities: super::document::CapabilityFlags,
    enabled: bool,
    health_circuits: Vec<HealthCircuit>,
}

impl From<&Candidate> for SemanticCandidate {
    fn from(candidate: &Candidate) -> Self {
        let mut health_circuits = candidate.health_circuits.clone();
        health_circuits.sort_by_key(|circuit| (circuit.failure, circuit.duration_seconds));
        Self {
            id: candidate.id.clone(),
            binding_id: candidate.binding_id.clone(),
            model: candidate.model.clone(),
            upgrade_policy_id: candidate.upgrade_policy_id.clone(),
            required_capabilities: candidate.required_capabilities,
            enabled: candidate.enabled,
            health_circuits,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SemanticEdge {
    preflight: super::document::FallbackPermission,
    post_send: Vec<super::document::FailureClass>,
    billed_boundary: super::document::BoundaryConsent,
    ambiguous_transport: super::document::BoundaryConsent,
    cache_transition: super::document::CacheTransitionPolicy,
}

impl From<&CandidateEdge> for SemanticEdge {
    fn from(edge: &CandidateEdge) -> Self {
        let mut post_send = edge.post_send.clone();
        post_send.sort();
        post_send.dedup();
        Self {
            preflight: edge.preflight,
            post_send,
            billed_boundary: edge.billed_boundary,
            ambiguous_transport: edge.ambiguous_transport,
            cache_transition: edge.cache_transition,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SemanticUpgrade {
    id: UpgradePolicyId,
    classifier_id: ClassifierId,
    enabled: bool,
    minimum_learning: super::document::MinimumLearning,
    allow_manual_promotion: bool,
    inventory_sources: Vec<InventorySource>,
    lane_transition: LaneTransitionPolicy,
    allow_revision_one_legacy_learning: bool,
}

impl From<&UpgradePolicy> for SemanticUpgrade {
    fn from(policy: &UpgradePolicy) -> Self {
        let mut inventory_sources = policy.inventory_sources.clone();
        inventory_sources.sort();
        inventory_sources.dedup();
        Self {
            id: policy.id.clone(),
            classifier_id: policy.classifier_id.clone(),
            enabled: policy.enabled,
            minimum_learning: policy.minimum_learning,
            allow_manual_promotion: policy.allow_manual_promotion,
            inventory_sources,
            lane_transition: policy.lane_transition,
            allow_revision_one_legacy_learning: policy.allow_revision_one_legacy_learning,
        }
    }
}

pub fn route_fingerprint(semantics: &RouteSemantics) -> Result<RouteFingerprint, CanonicalError> {
    let domain = if semantics.route.honours_server_safeguards {
        ROUTE_FINGERPRINT_DOMAIN_V3
    } else {
        route_domain(semantics.format_version)?
    };
    Ok(RouteFingerprint(hash(domain, &canonical_bytes(semantics)?)))
}

fn route_domain(format_version: u32) -> Result<&'static [u8], CanonicalError> {
    match format_version {
        1 => Ok(ROUTE_FINGERPRINT_DOMAIN_V1),
        2..=4 => Ok(ROUTE_FINGERPRINT_DOMAIN_V2),
        version => Err(CanonicalError::UnsupportedFormat(version)),
    }
}

/// Fingerprint a compiler-generated one-candidate presentation route.
///
/// The binding, model, and stable generated identities close the complete
/// semantics; presentation display metadata is deliberately absent.
pub(crate) fn generated_route_fingerprint(
    format_version: u32,
    route_id: &RouteId,
    candidate_id: &CandidateId,
    binding_id: &super::BindingId,
    model_id: &str,
) -> Result<RouteFingerprint, CanonicalError> {
    #[derive(Serialize)]
    struct Generated<'a> {
        route_id: &'a RouteId,
        candidate_id: &'a CandidateId,
        binding_id: &'a super::BindingId,
        model_id: &'a str,
    }
    Ok(RouteFingerprint(hash(
        route_domain(format_version)?,
        &canonical_bytes(&Generated {
            route_id,
            candidate_id,
            binding_id,
            model_id,
        })?,
    )))
}

fn hash(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    digest.finalize().into()
}

/// A small lexical JSON parser used before serde. This is where duplicate keys,
/// exact number spellings, and allocation bounds remain observable. It never
/// interprets an object key as serde_json's private arbitrary-precision marker.
struct JsonBoundsParser<'a> {
    input: &'a [u8],
    position: usize,
    limits: CanonicalLimits,
}

impl<'a> JsonBoundsParser<'a> {
    fn new(input: &'a str, limits: CanonicalLimits) -> Self {
        Self {
            input: input.as_bytes(),
            position: 0,
            limits,
        }
    }

    fn parse(mut self) -> Result<(), CanonicalError> {
        self.skip_whitespace();
        self.value(0)?;
        self.skip_whitespace();
        if self.position != self.input.len() {
            return Err(self.invalid("trailing characters"));
        }
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<(), CanonicalError> {
        match self.peek() {
            Some(b'{') => self.object(depth + 1),
            Some(b'[') => self.array(depth + 1),
            Some(b'\"') => self.string().map(|_| ()),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.invalid("expected a JSON value")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<(), CanonicalError> {
        self.check_depth(depth)?;
        self.position += 1;
        self.skip_whitespace();
        let mut keys = HashSet::new();
        if self.take(b'}') {
            return Ok(());
        }
        loop {
            if keys.len() == self.limits.collection_items {
                return Err(CanonicalError::CollectionLimit);
            }
            let key = self.string()?;
            if !keys.insert(key) {
                return Err(CanonicalError::DuplicateKey);
            }
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();
            self.value(depth)?;
            self.skip_whitespace();
            if self.take(b'}') {
                return Ok(());
            }
            self.expect(b',')?;
            self.skip_whitespace();
        }
    }

    fn array(&mut self, depth: usize) -> Result<(), CanonicalError> {
        self.check_depth(depth)?;
        self.position += 1;
        self.skip_whitespace();
        if self.take(b']') {
            return Ok(());
        }
        let mut count = 0;
        loop {
            if count == self.limits.collection_items {
                return Err(CanonicalError::CollectionLimit);
            }
            self.value(depth)?;
            count += 1;
            self.skip_whitespace();
            if self.take(b']') {
                return Ok(());
            }
            self.expect(b',')?;
            self.skip_whitespace();
        }
    }

    fn string(&mut self) -> Result<String, CanonicalError> {
        if self.peek() != Some(b'\"') {
            return Err(self.invalid("expected an object key string"));
        }
        let start = self.position;
        self.position += 1;
        let mut escaped = false;
        while let Some(byte) = self.peek() {
            self.position += 1;
            match byte {
                b'\"' if !escaped => {
                    let token = std::str::from_utf8(&self.input[start..self.position])
                        .map_err(|_| CanonicalError::InvalidUtf8)?;
                    let decoded: String = serde_json::from_str(token)
                        .map_err(|error| CanonicalError::InvalidJson(error.to_string()))?;
                    if decoded.len() > self.limits.string_bytes {
                        return Err(CanonicalError::StringLimit);
                    }
                    return Ok(decoded);
                }
                b'\\' if !escaped => escaped = true,
                _ => escaped = false,
            }
        }
        Err(self.invalid("unterminated string"))
    }

    fn number(&mut self) -> Result<(), CanonicalError> {
        let start = self.position;
        if self.take(b'-') && !self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            return Err(self.invalid("invalid number"));
        }
        if self.take(b'0') {
            if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(self.invalid("leading zero in number"));
            }
        } else {
            let before = self.position;
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.position += 1;
            }
            if self.position == before {
                return Err(self.invalid("invalid number"));
            }
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err(CanonicalError::NonIntegerNumber);
        }
        if self.position == start {
            return Err(self.invalid("invalid number"));
        }
        Ok(())
    }

    fn literal(&mut self, literal: &[u8]) -> Result<(), CanonicalError> {
        if self.input.get(self.position..self.position + literal.len()) != Some(literal) {
            return Err(self.invalid("invalid literal"));
        }
        self.position += literal.len();
        Ok(())
    }

    fn check_depth(&self, depth: usize) -> Result<(), CanonicalError> {
        if depth > self.limits.depth {
            Err(CanonicalError::DepthLimit)
        } else {
            Ok(())
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), CanonicalError> {
        if self.take(byte) {
            Ok(())
        } else {
            Err(self.invalid("unexpected JSON token"))
        }
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }

    fn skip_whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.position += 1;
        }
    }

    fn invalid(&self, reason: &str) -> CanonicalError {
        CanonicalError::InvalidJson(format!("{reason} at byte {}", self.position))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CanonicalError {
    #[error("policy document exceeds its byte limit")]
    DocumentTooLarge,
    #[error("policy document is not valid UTF-8")]
    InvalidUtf8,
    #[error("policy document contains a duplicate object key")]
    DuplicateKey,
    #[error("policy numeric values must be integers")]
    NonIntegerNumber,
    #[error("policy document exceeds its nesting depth limit")]
    DepthLimit,
    #[error("policy collection exceeds its item limit")]
    CollectionLimit,
    #[error("policy string exceeds its byte limit")]
    StringLimit,
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("invalid typed policy document: {0}")]
    TypedDocument(String),
    #[error("invalid policy document: {0}")]
    InvalidDocument(String),
    #[error("unsupported policy format version {0}")]
    UnsupportedFormat(u32),
    #[error("missing route semantic dependency {0}")]
    MissingSemanticDependency(String),
    #[error("cannot serialise policy document: {0}")]
    Serialization(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::document::{
        AliasRule, BindingId, BoundaryConsent, CURRENT_FORMAT_VERSION, CacheTransitionPolicy,
        CandidateModel, CapabilityFlags, ClassifierId, FailureClass, FallbackPermission,
        MinimumLearning, RouteChain,
    };
    use crate::policy::matcher::MatcherSpec;
    use crate::policy::version::VersionParserSpec;

    fn minimal_json() -> &'static [u8] {
        br#"{
          "format_version": 1,
          "defaults": [],
          "presentations": [],
          "routes": [],
          "rewrites": [],
          "classifiers": [],
          "upgrades": []
        }"#
    }

    #[test]
    fn valid_document_roundtrips_through_canonical_form() {
        let document = decode_policy(minimal_json(), CanonicalLimits::default()).unwrap();
        let bytes = policy_bytes(&document).unwrap();
        assert_eq!(
            decode_policy(&bytes, CanonicalLimits::default()).unwrap(),
            document
        );
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            r#"{"classifiers":[],"compaction":null,"defaults":[],"description":null,"format_version":1,"presentations":[],"rewrites":[],"routes":[],"upgrades":[]}"#
        );
    }

    #[test]
    fn private_number_marker_is_never_treated_as_a_number() {
        let forged = br#"{
          "format_version":{"$serde_json::private::Number":"1"},
          "defaults":[],"presentations":[],"routes":[],"rewrites":[],
          "classifiers":[],"upgrades":[]
        }"#;
        assert!(matches!(
            decode_policy(forged, CanonicalLimits::default()),
            Err(CanonicalError::TypedDocument(_))
        ));
    }

    #[test]
    fn numeric_positions_accept_integer_tokens_only_even_on_overflow() {
        for token in ["1.0", "1e0", "-1E2"] {
            let json = format!(r#"{{"format_version":{token}}}"#);
            assert_eq!(
                decode_policy(json.as_bytes(), CanonicalLimits::default()),
                Err(CanonicalError::NonIntegerNumber)
            );
        }
        for token in ["4294967296", "18446744073709551616"] {
            let json = format!(r#"{{"format_version":{token}}}"#);
            assert!(matches!(
                decode_policy(json.as_bytes(), CanonicalLimits::default()),
                Err(CanonicalError::TypedDocument(_))
            ));
        }
    }

    #[test]
    fn rejects_duplicate_unknown_and_invalid_utf8() {
        let duplicate = br#"{"format_version":1,"format_version":1}"#;
        assert_eq!(
            decode_policy(duplicate, CanonicalLimits::default()),
            Err(CanonicalError::DuplicateKey)
        );
        let unknown = br#"{"format_version":1,"unknown":0}"#;
        assert!(matches!(
            decode_policy(unknown, CanonicalLimits::default()),
            Err(CanonicalError::TypedDocument(_))
        ));
        assert_eq!(
            decode_policy(&[0xff], CanonicalLimits::default()),
            Err(CanonicalError::InvalidUtf8)
        );
    }

    #[test]
    fn configured_limits_tighten_but_never_weaken_hard_bounds() {
        let tiny = CanonicalLimits {
            document_bytes: 2,
            ..CanonicalLimits::default()
        };
        assert_eq!(
            decode_policy(minimal_json(), tiny),
            Err(CanonicalError::DocumentTooLarge)
        );

        let depth = CanonicalLimits {
            depth: 2,
            ..CanonicalLimits::default()
        };
        assert_eq!(
            decode_policy(br#"{"format_version":1,"routes":[[]]}"#, depth),
            Err(CanonicalError::DepthLimit)
        );

        let permissive = CanonicalLimits {
            document_bytes: usize::MAX,
            depth: usize::MAX,
            collection_items: usize::MAX,
            string_bytes: usize::MAX,
        };
        let oversized = format!(
            r#"{{"format_version":1,"description":"{}"}}"#,
            "x".repeat(MAX_DESCRIPTION_BYTES + 1)
        );
        assert_eq!(
            decode_policy(oversized.as_bytes(), permissive),
            Err(CanonicalError::StringLimit)
        );
    }

    #[test]
    fn parser_admits_description_boundary_above_normal_string_limit() {
        for length in [1024, 1025, 4096] {
            let json = format!(
                r#"{{"format_version":1,"description":"{}"}}"#,
                "x".repeat(length)
            );
            assert!(decode_policy(json.as_bytes(), CanonicalLimits::default()).is_ok());
        }
        let json = format!(
            r#"{{"format_version":1,"description":"{}"}}"#,
            "x".repeat(4097)
        );
        assert_eq!(
            decode_policy(json.as_bytes(), CanonicalLimits::default()),
            Err(CanonicalError::StringLimit)
        );
    }

    #[test]
    fn policy_fingerprint_is_checked_and_domain_separated() {
        let document = full_document(route("display", None)).validate().unwrap();
        let first = policy_fingerprint(&document).unwrap();
        let parsed: PolicyFingerprint = first.to_string().parse().unwrap();
        assert_eq!(first, parsed);
        assert!(
            first
                .to_string()
                .to_uppercase()
                .parse::<PolicyFingerprint>()
                .is_err()
        );
        assert!("00".parse::<PolicyFingerprint>().is_err());

        let semantics =
            RouteSemantics::from_document(&document, &RouteId::new("route").unwrap()).unwrap();
        assert_ne!(
            first.as_bytes(),
            route_fingerprint(&semantics).unwrap().as_bytes()
        );
        let encoded = serde_json::to_string(&first).unwrap();
        assert_eq!(
            serde_json::from_str::<PolicyFingerprint>(&encoded).unwrap(),
            first
        );
    }

    #[test]
    fn route_fingerprint_excludes_display_and_normalizes_sets() {
        let mut first = full_document(route("First", Some("description")));
        let mut second = full_document(route("Renamed", Some("other")));
        first.upgrades[0].inventory_sources =
            vec![InventorySource::Served, InventorySource::Catalogue];
        second.upgrades[0].inventory_sources =
            vec![InventorySource::Catalogue, InventorySource::Served];
        first.routes[0].chain.steps[0].edge.post_send =
            vec![FailureClass::Overload, FailureClass::RateLimit];
        second.routes[0].chain.steps[0].edge.post_send =
            vec![FailureClass::RateLimit, FailureClass::Overload];
        assert_eq!(digest(first), digest(second));
    }

    #[test]
    fn policy_format_domains_preserve_prior_versions_and_separate_v3() {
        let mut v1 = full_document(route("display", None));
        v1.format_version = 1;
        let v1 = v1.validate().unwrap();
        let v1_policy = policy_fingerprint(&v1).unwrap();
        let v1_route = RouteSemantics::from_document(&v1, &RouteId::new("route").unwrap())
            .and_then(|semantics| route_fingerprint(&semantics))
            .unwrap();

        let mut v2 = full_document(route("display", None));
        v2.format_version = 2;
        v2.routes[0]
            .chain
            .first
            .required_capabilities
            .tool_choice_named = true;
        let v2 = v2.validate().unwrap();
        assert_ne!(v1_policy, policy_fingerprint(&v2).unwrap());
        let v2_route = RouteSemantics::from_document(&v2, &RouteId::new("route").unwrap())
            .and_then(|semantics| route_fingerprint(&semantics))
            .unwrap();
        assert_ne!(v1_route, v2_route);

        let mut v3_document = v2.document().clone();
        v3_document.format_version = 3;
        let v3 = v3_document.validate().unwrap();
        assert_ne!(
            policy_fingerprint(&v2).unwrap(),
            policy_fingerprint(&v3).unwrap()
        );
        assert_eq!(
            v2_route,
            RouteSemantics::from_document(&v3, &RouteId::new("route").unwrap())
                .and_then(|semantics| route_fingerprint(&semantics))
                .unwrap(),
            "document-global compaction vocabulary does not change route semantics"
        );

        let mut invalid_v1 = v1.into_document();
        invalid_v1.routes[0]
            .chain
            .first
            .required_capabilities
            .tool_choice_none = true;
        assert!(matches!(
            invalid_v1.validate(),
            Err(crate::policy::DocumentError::FeatureRequiresFormat { required: 2, .. })
        ));

        let old_reader_input = serde_json::to_vec(&serde_json::json!({
            "format_version": 1,
            "routes": [{
                "id": "route",
                "display_name": "route",
                "parameters": [],
                "chain": {
                    "first": {
                        "id": "candidate",
                        "binding_id": "provider.dialect",
                        "model": {"kind": "requested_model"},
                        "required_capabilities": {"tool_choice_none": false},
                        "enabled": true
                    },
                    "steps": []
                },
                "lane_transition": "preserve_pinned"
            }]
        }))
        .unwrap();
        assert!(matches!(
            decode_policy(&old_reader_input, CanonicalLimits::default()),
            Err(CanonicalError::InvalidDocument(_))
        ));

        let mut invalid_v2 = v2.into_document();
        invalid_v2.compaction = Some(crate::policy::CompactionPolicy {
            target: crate::policy::CompactionTarget::RevisionOneLegacy {
                model_spec: "sonnet".to_owned(),
                eligible_bindings: vec![BindingId::new("provider.dialect").unwrap()],
            },
            lane_transition: LaneTransitionPolicy::MoveWhenCold,
            cache_transition: CacheTransitionPolicy::AllowWhenCold,
        });
        assert!(matches!(
            invalid_v2.validate(),
            Err(crate::policy::DocumentError::FeatureRequiresFormat { required: 3, .. })
        ));
    }

    #[test]
    fn every_route_semantic_class_changes_the_digest() {
        let base = full_document(route("display", None));
        let expected = digest(base.clone());

        let mut capability_change = base.clone();
        capability_change.routes[0]
            .chain
            .first
            .required_capabilities
            .tool_choice_named = true;
        assert_ne!(expected, digest(capability_change));

        let mut route_change = base.clone();
        route_change.routes[0].lane_transition = LaneTransitionPolicy::AllowImmediate;
        assert_ne!(expected, digest(route_change));

        let mut alias_change = base.clone();
        alias_change.routes[0]
            .chain
            .first
            .alias_policy
            .as_mut()
            .unwrap()
            .rules[0]
            .replacement = "other".to_owned();
        assert_ne!(expected, digest(alias_change));

        let mut upgrade_change = base.clone();
        upgrade_change.upgrades[0].allow_manual_promotion = true;
        assert_ne!(expected, digest(upgrade_change));

        let mut classifier_change = base;
        classifier_change.classifiers[0].family_template = "other".to_owned();
        assert_ne!(expected, digest(classifier_change));
    }

    fn digest(document: PolicyDocument) -> RouteFingerprint {
        let document = document.validate().unwrap();
        let semantics =
            RouteSemantics::from_document(&document, &RouteId::new("route").unwrap()).unwrap();
        route_fingerprint(&semantics).unwrap()
    }

    fn full_document(route: Route) -> PolicyDocument {
        PolicyDocument {
            format_version: CURRENT_FORMAT_VERSION,
            description: None,
            defaults: vec![],
            presentations: vec![],
            routes: vec![route],
            rewrites: vec![],
            classifiers: vec![FamilyClassifier {
                id: ClassifierId::new("classifier").unwrap(),
                binding_id: BindingId::new("provider.dialect").unwrap(),
                model_matcher: MatcherSpec::Regex("^(.*)$".to_owned()),
                family_template: "${1}".to_owned(),
                version_template: "${1}".to_owned(),
                version_parser: VersionParserSpec::Semver,
                legacy_hyphenated: false,
            }],
            upgrades: vec![UpgradePolicy {
                id: UpgradePolicyId::new("upgrade").unwrap(),
                classifier_id: ClassifierId::new("classifier").unwrap(),
                enabled: true,
                minimum_learning: MinimumLearning::None,
                allow_manual_promotion: false,
                inventory_sources: vec![InventorySource::Catalogue],
                lane_transition: LaneTransitionPolicy::PreservePinned,
                allow_revision_one_legacy_learning: false,
            }],
            compaction: None,
        }
    }

    #[test]
    fn honouring_server_safeguards_is_route_semantics_and_a_quiet_route_keeps_its_fingerprint() {
        let quiet = |version: u32| {
            let mut document = full_document(route("Route", None));
            document.format_version = version;
            digest(document)
        };
        assert_eq!(
            quiet(3),
            quiet(4),
            "a route that never opts in keeps its fingerprint across versions"
        );
        let mut honouring = route("Route", None);
        honouring.honours_server_safeguards = true;
        assert_ne!(quiet(4), digest(full_document(honouring)));
    }

    fn route(display_name: &str, description: Option<&str>) -> Route {
        let mut first = candidate("candidate");
        first.upgrade_policy_id = Some(UpgradePolicyId::new("upgrade").unwrap());
        first.alias_policy = Some(AliasPolicy {
            rules: vec![AliasRule {
                matcher: MatcherSpec::Exact("old".to_owned()),
                exclude: vec![],
                replacement: "new".to_owned(),
            }],
        });
        Route {
            honours_server_safeguards: false,
            id: RouteId::new("route").unwrap(),
            display_name: display_name.to_owned(),
            description: description.map(str::to_owned),
            parameters: vec![],
            chain: RouteChain {
                first,
                steps: vec![RouteStep {
                    edge: CandidateEdge {
                        preflight: FallbackPermission::Enabled,
                        post_send: vec![],
                        billed_boundary: BoundaryConsent::Deny,
                        ambiguous_transport: BoundaryConsent::Deny,
                        cache_transition: CacheTransitionPolicy::PreserveWarm,
                    },
                    candidate: candidate("backup"),
                }],
            },
            lane_transition: LaneTransitionPolicy::PreservePinned,
        }
    }

    fn candidate(id: &str) -> Candidate {
        Candidate {
            id: CandidateId::new(id).unwrap(),
            binding_id: BindingId::new("provider.dialect").unwrap(),
            model: CandidateModel::Exact {
                model_id: "model".to_owned(),
            },
            upgrade_policy_id: None,
            alias_policy: None,
            required_capabilities: CapabilityFlags::default(),
            enabled: true,
            health_circuits: vec![],
        }
    }

    #[test]
    fn imported_cache_transition_spelling_stays_stable() {
        assert_eq!(
            serde_json::to_string(&CacheTransitionPolicy::AllowWhenCold).unwrap(),
            r#""allow_when_cold""#
        );
    }
}
