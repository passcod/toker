//! Content-free diagnostics shared by document validation and later compilers.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::document::{MAX_COLLECTION_ITEMS, StableId};

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

/// A static, policy-schema field name suitable for a diagnostic path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticField(&'static str);

impl DiagnosticField {
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }
}

/// A path into a policy document, encoded as escaped JSON Pointer segments.
/// Only static schema fields and bounded collection indices can be appended;
/// request and model strings cannot be injected as raw pointer text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DiagnosticPath(String);

impl DiagnosticPath {
    pub fn root() -> Self {
        Self(String::new())
    }

    pub fn field(mut self, field: DiagnosticField) -> Self {
        self.0.push('/');
        for character in field.0.chars() {
            match character {
                '~' => self.0.push_str("~0"),
                '/' => self.0.push_str("~1"),
                character => self.0.push(character),
            }
        }
        self
    }

    pub fn index(mut self, index: usize) -> Result<Self, DiagnosticPathError> {
        if index >= MAX_COLLECTION_ITEMS {
            return Err(DiagnosticPathError::IndexOutOfBounds);
        }
        self.0.push('/');
        self.0.push_str(&index.to_string());
        Ok(self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DiagnosticPathError {
    #[error("diagnostic path index exceeds the collection bound")]
    IndexOutOfBounds,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_pointer_escaped_and_indices_are_bounded() {
        let path = DiagnosticPath::root()
            .field(DiagnosticField::new("routes/~active"))
            .index(2)
            .unwrap();
        assert_eq!(path.as_str(), "/routes~1~0active/2");
        assert_eq!(
            DiagnosticPath::root().index(MAX_COLLECTION_ITEMS),
            Err(DiagnosticPathError::IndexOutOfBounds)
        );
    }
}
