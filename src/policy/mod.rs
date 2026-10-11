//! Typed, canonical documents for live model routing policy.
//!
//! This module deliberately contains no provider registry or live inventory
//! dependencies. Documents can therefore be parsed, validated, fingerprinted,
//! and stored before the runtime compiler is introduced.

pub mod canonical;
pub mod compile;
pub mod diagnostic;
pub mod document;
pub mod learning;
pub mod legacy;
pub mod matcher;
pub mod resolve;
mod selector;
pub mod shadow;
pub mod version;

pub use canonical::{
    CanonicalError, CanonicalLimits, FingerprintParseError, PolicyFingerprint, RouteFingerprint,
    RouteSemantics, decode_policy, policy_bytes, policy_fingerprint, route_fingerprint,
};
pub use compile::{
    Compilation, CompiledAliasRule, CompiledCandidate, CompiledCandidateModel, CompiledClassifier,
    CompiledCompaction, CompiledPolicy, CompiledPresentation, CompiledRewrite, CompiledRoute,
    CompiledRouteReference, CompiledSelector, CompiledUpgradePolicy, PolicyOrigin,
    PresentationRoute, compile,
};
pub use diagnostic::{
    Diagnostic, DiagnosticCode, DiagnosticField, DiagnosticPath, DiagnosticPathError, Severity,
};
pub use document::*;
pub use learning::{BindingLearningView, LearnedEvidence, LearningView, LegacyLearningView};
pub use matcher::{
    CaptureSchema, CompiledMatcher, MAX_MATCH_INPUT_BYTES, MAX_RENDERED_TEMPLATE_BYTES,
    MatcherCaptures, MatcherError, MatcherSpec, Template, TemplateError,
};
pub use resolve::{
    AttemptPlan, AvailabilitySnapshot, BindingAvailability, CandidateEvidence,
    CatalogueAvailability, Evidence, LaneFacts, NoLearning, ProvisionalPin, RequestClass,
    RequestFacts, ResolveFailure, RouteProvenance, SkipReason, TraceEvent, TransitionDecision,
    resolve,
};
pub use shadow::{
    CollectingObserver, LegacyOutcome, PolicyShadow, ShadowComparison, ShadowObserver,
    ShadowOutcome, ShadowRecord, TracingObserver,
};
pub use version::{
    DateFormat, NumericVersion, ParsedVersion, ReleaseDate, VersionError, VersionParserSpec,
};
