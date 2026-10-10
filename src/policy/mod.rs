//! Typed, canonical documents for live model routing policy.
//!
//! This module deliberately contains no provider registry or live inventory
//! dependencies. Documents can therefore be parsed, validated, fingerprinted,
//! and stored before the runtime compiler is introduced.

pub mod canonical;
pub mod diagnostic;
pub mod document;
pub mod matcher;
pub mod version;

pub use canonical::{
    CanonicalError, CanonicalLimits, FingerprintParseError, PolicyFingerprint, RouteFingerprint,
    RouteSemantics, decode_policy, policy_bytes, policy_fingerprint, route_fingerprint,
};
pub use diagnostic::{
    Diagnostic, DiagnosticCode, DiagnosticField, DiagnosticPath, DiagnosticPathError, Severity,
};
pub use document::*;
pub use matcher::{
    CaptureSchema, CompiledMatcher, MAX_MATCH_INPUT_BYTES, MAX_RENDERED_TEMPLATE_BYTES,
    MatcherCaptures, MatcherError, MatcherSpec, Template, TemplateError,
};
pub use version::{
    DateFormat, NumericVersion, ParsedVersion, ReleaseDate, VersionError, VersionParserSpec,
};
