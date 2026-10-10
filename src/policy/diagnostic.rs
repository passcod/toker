//! Content-free diagnostics shared by document validation and later compilers.

use serde::{Deserialize, Serialize};

use super::document::StableId;

/// A stable machine-readable diagnostic identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DiagnosticCode(pub StableId);

/// Whether a finding prevents policy activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// A path into a policy document, encoded as JSON Pointer segments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DiagnosticPath(String);

impl DiagnosticPath {
    pub fn root() -> Self {
        Self(String::new())
    }

    pub fn new(pointer: impl Into<String>) -> Self {
        Self(pointer.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A diagnostic that identifies policy structure without carrying request,
/// response, credential, or upstream error content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub record_id: Option<StableId>,
    pub path: DiagnosticPath,
}
