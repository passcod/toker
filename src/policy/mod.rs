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
    CanonicalError, CanonicalLimits, PolicyFingerprint, RouteFingerprint,
    RouteSemanticFingerprintInput, canonical_bytes, decode_policy, policy_fingerprint,
    route_fingerprint,
};
pub use diagnostic::{Diagnostic, DiagnosticCode, DiagnosticPath, Severity};
pub use document::*;
pub use matcher::{
    CaptureSchema, CompiledMatcher, MatcherCaptures, MatcherError, MatcherSpec, Template,
    TemplateError,
};
pub use version::{
    DateFormat, NumericVersion, ParsedVersion, ReleaseDate, VersionError, VersionParserSpec,
};
