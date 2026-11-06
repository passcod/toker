//! Typed, canonical documents for live model routing policy.
//!
//! This module deliberately contains no provider registry or live inventory
//! dependencies. Documents can therefore be parsed, validated, fingerprinted,
//! and stored before the runtime compiler is introduced.

pub mod canonical;
pub mod compile;
pub mod diagnostic;
pub mod document;
pub mod edit;
pub mod family;
pub mod learning;
pub mod legacy;
pub mod matcher;
pub mod pinning;
pub mod resolve;
pub mod runtime;
mod selector;
pub mod transition;
pub mod version;

pub use canonical::{
    CanonicalError, CanonicalLimits, FingerprintParseError, PolicyFingerprint, RouteFingerprint,
    RouteSemantics, decode_policy, policy_bytes, policy_fingerprint, route_fingerprint,
};
pub use compile::{
    ClassificationError, ClassifiedModel, Compilation, CompiledAliasRule, CompiledCandidate,
    CompiledCandidateModel, CompiledClassifier, CompiledCompaction, CompiledPolicy,
    CompiledPresentation, CompiledRewrite, CompiledRoute, CompiledRouteReference, CompiledSelector,
    CompiledUpgradePolicy, PolicyOrigin, PresentationRoute, compile,
};
pub use diagnostic::{
    Diagnostic, DiagnosticCode, DiagnosticField, DiagnosticPath, DiagnosticPathError, Severity,
};
pub use document::*;
pub use edit::{
    CandidateMoveDestination, MAX_POLICY_EDIT_OPERATIONS, PolicyEdit, PolicyEditError,
    PolicyEditOperation,
};
pub use learning::{
    BindingLearningView, LearnedEvidence, LearningSnapshot, LearningView, RecencyEvidence,
};
pub use matcher::{
    CaptureSchema, CompiledMatcher, MAX_MATCH_INPUT_BYTES, MAX_RENDERED_TEMPLATE_BYTES,
    MatcherCaptures, MatcherError, MatcherSpec, Template, TemplateError,
};
pub use pinning::{PinPersistence, persist_transition};
pub use resolve::{
    AttemptPlan, AvailabilitySnapshot, BindingAvailability, CandidateEvidence,
    CatalogueAvailability, CircuitSnapshot, CredentialSnapshot, Evidence, LaneFacts,
    LegacyCompactionFacts, NoLearning, OpenCircuitTarget, ProvisionalPin, RequestClass,
    RequestFacts, ResolveFailure, RouteProvenance, SkipReason, TraceEvent, resolve,
    resolve_after_failure, resolve_first_candidate, resolve_model_free, resolve_with_circuits,
};
pub use runtime::{HealthCircuitKey, PolicyRuntime, PolicyRuntimeError, PolicySnapshot};
pub use transition::{
    CacheTemperature, LaneState, PinIdentity, TransitionCause,
    TransitionDecision as LaneTransitionDecision, TransitionDenied, TransitionSource,
    decide_transition,
};
pub use version::{
    DateFormat, NumericVersion, ParsedVersion, ReleaseDate, VersionError, VersionParserSpec,
};
