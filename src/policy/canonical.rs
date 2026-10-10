//! Bounded canonical JSON and domain-separated policy fingerprints.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::document::{
    Candidate, CandidateEdge, LaneTransitionPolicy, MAX_COLLECTION_ITEMS, MAX_DOCUMENT_BYTES,
    MAX_DOCUMENT_DEPTH, MAX_STRING_BYTES, PolicyDocument, Route, StableId,
};

const POLICY_FINGERPRINT_DOMAIN: &[u8] = b"toker.policy.document.v1\0";
const ROUTE_FINGERPRINT_DOMAIN: &[u8] = b"toker.policy.route.v1\0";

/// Limits applied before a document enters the runtime compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalLimits {
    pub document_bytes: usize,
    pub depth: usize,
    pub collection_items: usize,
    pub string_bytes: usize,
}

impl Default for CanonicalLimits {
    fn default() -> Self {
        Self {
            document_bytes: MAX_DOCUMENT_BYTES,
            depth: MAX_DOCUMENT_DEPTH,
            collection_items: MAX_COLLECTION_ITEMS,
            string_bytes: MAX_STRING_BYTES,
        }
    }
}

/// Parse a bounded policy document while duplicate keys are still observable.
pub fn decode_policy(
    bytes: &[u8],
    limits: CanonicalLimits,
) -> Result<PolicyDocument, CanonicalError> {
    if bytes.len() > limits.document_bytes {
        return Err(CanonicalError::DocumentTooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| CanonicalError::InvalidUtf8)?;
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = BoundedValueSeed { limits, depth: 0 }
        .deserialize(&mut deserializer)
        .map_err(classify_json_error)?;
    deserializer.end().map_err(classify_json_error)?;
    let document = PolicyDocument::deserialize(value)
        .map_err(|error| CanonicalError::TypedDocument(error.to_string()))?;
    document
        .validate()
        .map_err(|error| CanonicalError::InvalidDocument(error.to_string()))?;
    Ok(document)
}

/// Render any serialisable policy value as compact JSON with recursively sorted
/// object keys. Array order is deliberately preserved.
pub fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, CanonicalError> {
    let value = serde_json::to_value(value)
        .map_err(|error| CanonicalError::Serialization(error.to_string()))?;
    let sorted = sort_value(value);
    serde_json::to_vec(&sorted).map_err(|error| CanonicalError::Serialization(error.to_string()))
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
    };
}

fingerprint_type!(PolicyFingerprint);
fingerprint_type!(RouteFingerprint);

pub fn policy_fingerprint(document: &PolicyDocument) -> Result<PolicyFingerprint, CanonicalError> {
    Ok(PolicyFingerprint(hash(
        POLICY_FINGERPRINT_DOMAIN,
        &canonical_bytes(document)?,
    )))
}

/// Fingerprint input for a route and its caller-supplied transitive semantic
/// closure. The route projection omits display name and description. Current
/// inventory winners are not document fields and must not be supplied in
/// `semantic_closure` by the compiler.
pub struct RouteSemanticFingerprintInput<'a, T: Serialize> {
    route: &'a Route,
    semantic_closure: &'a T,
}

impl<'a, T: Serialize> RouteSemanticFingerprintInput<'a, T> {
    pub fn new(route: &'a Route, semantic_closure: &'a T) -> Self {
        Self {
            route,
            semantic_closure,
        }
    }
}

impl<T: Serialize> Serialize for RouteSemanticFingerprintInput<'_, T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct SemanticRoute<'a, T> {
            id: &'a StableId,
            parameters: &'a [StableId],
            candidates: &'a [Candidate],
            edges: &'a [CandidateEdge],
            lane_transition: LaneTransitionPolicy,
            semantic_closure: &'a T,
        }

        SemanticRoute {
            id: &self.route.id,
            parameters: &self.route.parameters,
            candidates: &self.route.candidates,
            edges: &self.route.edges,
            lane_transition: self.route.lane_transition,
            semantic_closure: self.semantic_closure,
        }
        .serialize(serializer)
    }
}

pub fn route_fingerprint<T: Serialize>(
    input: &RouteSemanticFingerprintInput<'_, T>,
) -> Result<RouteFingerprint, CanonicalError> {
    Ok(RouteFingerprint(hash(
        ROUTE_FINGERPRINT_DOMAIN,
        &canonical_bytes(input)?,
    )))
}

fn hash(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    digest.finalize().into()
}

#[derive(Clone, Copy)]
struct BoundedValueSeed {
    limits: CanonicalLimits,
    depth: usize,
}

impl BoundedValueSeed {
    fn child(self) -> Result<Self, &'static str> {
        let depth = self.depth + 1;
        if depth > self.limits.depth {
            return Err(DEPTH_MARKER);
        }
        Ok(Self {
            limits: self.limits,
            depth,
        })
    }
}

const DUPLICATE_MARKER: &str = "policy duplicate object key";
const NUMBER_MARKER: &str = "policy numeric value must be an integer";
const DEPTH_MARKER: &str = "policy nesting depth exceeded";
const COLLECTION_MARKER: &str = "policy collection size exceeded";
const STRING_MARKER: &str = "policy string size exceeded";

impl<'de> DeserializeSeed<'de> for BoundedValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(BoundedValueVisitor(self))
    }
}

struct BoundedValueVisitor(BoundedValueSeed);

impl<'de> Visitor<'de> for BoundedValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded policy JSON")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Err(E::custom(NUMBER_MARKER))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_string(value.to_owned())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > self.0.limits.string_bytes {
            return Err(E::custom(STRING_MARKER));
        }
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.0.deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let child = self.0.child().map_err(serde::de::Error::custom)?;
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(child)? {
            if values.len() == self.0.limits.collection_items {
                return Err(serde::de::Error::custom(COLLECTION_MARKER));
            }
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let child = self.0.child().map_err(serde::de::Error::custom)?;
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            // With serde_json's arbitrary_precision feature, a number reaches
            // deserialize_any through this private one-field map. Inspecting
            // it here preserves overflow and fractional evidence.
            if key == "$serde_json::private::Number" && values.is_empty() {
                let number = object.next_value::<String>()?;
                if number.contains(['.', 'e', 'E']) {
                    return Err(serde::de::Error::custom(NUMBER_MARKER));
                }
                let number = number.parse::<Number>().map_err(serde::de::Error::custom)?;
                if object.next_key::<String>()?.is_some() {
                    return Err(serde::de::Error::custom(NUMBER_MARKER));
                }
                return Ok(Value::Number(number));
            }
            if key.len() > self.0.limits.string_bytes {
                return Err(serde::de::Error::custom(STRING_MARKER));
            }
            if values.len() == self.0.limits.collection_items {
                return Err(serde::de::Error::custom(COLLECTION_MARKER));
            }
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom(DUPLICATE_MARKER));
            }
            let value = object.next_value_seed(child)?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

fn classify_json_error(error: serde_json::Error) -> CanonicalError {
    let message = error.to_string();
    if message.contains(DUPLICATE_MARKER) {
        CanonicalError::DuplicateKey
    } else if message.contains(NUMBER_MARKER) {
        CanonicalError::NonIntegerNumber
    } else if message.contains(DEPTH_MARKER) {
        CanonicalError::DepthLimit
    } else if message.contains(COLLECTION_MARKER) {
        CanonicalError::CollectionLimit
    } else if message.contains(STRING_MARKER) {
        CanonicalError::StringLimit
    } else {
        CanonicalError::InvalidJson(message)
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
    #[error("cannot serialise policy document: {0}")]
    Serialization(String),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::policy::document::{
        CURRENT_FORMAT_VERSION, CacheTransitionPolicy, CandidateModel, CapabilityFlags,
    };

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

    #[test]
    fn valid_document_roundtrips_through_canonical_form() {
        let document = decode_policy(minimal_json(), CanonicalLimits::default()).unwrap();
        let bytes = canonical_bytes(&document).unwrap();
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
    fn rejects_duplicate_unknown_noninteger_and_invalid_utf8() {
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
        let decimal = br#"{"format_version":1.0}"#;
        assert_eq!(
            decode_policy(decimal, CanonicalLimits::default()),
            Err(CanonicalError::NonIntegerNumber)
        );
        assert_eq!(
            decode_policy(&[0xff], CanonicalLimits::default()),
            Err(CanonicalError::InvalidUtf8)
        );
    }

    #[test]
    fn rejects_configured_byte_depth_collection_and_string_limits() {
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

        let collection = CanonicalLimits {
            collection_items: 1,
            ..CanonicalLimits::default()
        };
        assert_eq!(
            decode_policy(br#"{"format_version":1,"routes":[]}"#, collection),
            Err(CanonicalError::CollectionLimit)
        );

        let strings = CanonicalLimits {
            string_bytes: 3,
            ..CanonicalLimits::default()
        };
        assert_eq!(
            decode_policy(br#"{"format_version":1}"#, strings),
            Err(CanonicalError::StringLimit)
        );
    }

    #[test]
    fn canonical_objects_ignore_input_key_order_but_arrays_do_not() {
        let a = json!({"z": 1, "a": {"y": 2, "x": 3}});
        let b = json!({"a": {"x": 3, "y": 2}, "z": 1});
        assert_eq!(canonical_bytes(&a).unwrap(), canonical_bytes(&b).unwrap());
        assert_ne!(
            canonical_bytes(&json!([1, 2])).unwrap(),
            canonical_bytes(&json!([2, 1])).unwrap()
        );
    }

    #[test]
    fn policy_fingerprint_is_stable_and_domain_separated() {
        let document = empty_document();
        let first = policy_fingerprint(&document).unwrap();
        let second = policy_fingerprint(&document).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.to_string().len(), 64);
        assert_ne!(
            first.as_bytes(),
            route_fingerprint(&RouteSemanticFingerprintInput::new(
                &route("display", None),
                &BTreeMap::<String, String>::new(),
            ))
            .unwrap()
            .as_bytes()
        );
    }

    #[test]
    fn route_fingerprint_ignores_display_changes_but_tracks_semantics() {
        let route_a = route("First", Some("description"));
        let route_b = route("Renamed", Some("other"));
        let closure = BTreeMap::from([("classifier", "alpha")]);
        let a = route_fingerprint(&RouteSemanticFingerprintInput::new(&route_a, &closure)).unwrap();
        let b = route_fingerprint(&RouteSemanticFingerprintInput::new(&route_b, &closure)).unwrap();
        assert_eq!(a, b);

        let other_closure = BTreeMap::from([("classifier", "beta")]);
        let changed = route_fingerprint(&RouteSemanticFingerprintInput::new(
            &route_b,
            &other_closure,
        ))
        .unwrap();
        assert_ne!(a, changed);
    }

    fn route(display_name: &str, description: Option<&str>) -> Route {
        Route {
            id: StableId::new("route").unwrap(),
            display_name: display_name.to_owned(),
            description: description.map(str::to_owned),
            parameters: vec![],
            candidates: vec![Candidate {
                id: StableId::new("candidate").unwrap(),
                binding_id: StableId::new("provider.dialect").unwrap(),
                model: CandidateModel::Exact {
                    model_id: "model".to_owned(),
                },
                upgrade_policy_id: None,
                alias_policy: None,
                required_capabilities: CapabilityFlags::default(),
                enabled: true,
                health_circuits: vec![],
            }],
            edges: vec![],
            lane_transition: LaneTransitionPolicy::PreservePinned,
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
