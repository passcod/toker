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

/// What a finding means and how to make it acceptable, for the form to show
/// beside the field it concerns (spec: UTUI#editing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Explanation {
    /// What is wrong, or what is worth knowing, in a sentence.
    pub sentence: &'static str,
    /// What to change, or what to expect where nothing needs changing.
    pub fix: &'static str,
}

/// The explanation for a diagnostic code, or `None` for a code the compiler
/// does not raise. Every code it raises has one; a test reads the compiler's
/// source to keep it so.
pub fn explain(code: &str) -> Option<Explanation> {
    let (sentence, fix) = match code {
        // Refusals.
        "ambiguous-transport-consent-required" => (
            "This edge falls back after an ambiguous failure, which may mean the request was already sent, but does not allow a retry then.",
            "Allow the ambiguous retry on the edge, or stop the edge falling back on an ambiguous failure.",
        ),
        "billed-boundary-consent-required" => (
            "This edge moves from a subscription to a billed provider without allowing the spend.",
            "Allow the billed boundary on the edge, or fall back to a provider on the same subscription.",
        ),
        "binding-not-routable-from-scope" => (
            "The binding does not serve this protocol, so a request on it could never reach the offer.",
            "Choose a binding that serves the protocol of the presentation's scope.",
        ),
        "cache-loss-consent-required" => (
            "This edge promises to keep a warm cache, but the candidate after it is on a cache the first cannot share.",
            "Let the edge lose the cache, when cold or always, or put both candidates on one cache.",
        ),
        "capability-unavailable" => (
            "The candidate requires something its binding is known not to do.",
            "Remove the required capability, or choose a binding that supports it.",
        ),
        "capture-binding-not-regex" => (
            "The route takes a capture from the match, and this match is not a regular expression.",
            "Match with a regular expression, or give the route the requested model or a fixed name instead.",
        ),
        "classifier-binding-mismatch" => (
            "The upgrade policy's classifier reads a different binding from the candidate it is set on.",
            "Use an upgrade policy whose classifier is for this candidate's binding.",
        ),
        "duplicate-default-scope" => (
            "Another default already answers for this scope.",
            "Edit the existing default instead, or remove it first.",
        ),
        "duplicate-presentation-slug" => (
            "Another presentation already publishes this name in the same scope.",
            "Give the presentation a different name, or remove the other.",
        ),
        "duplicate-rewrite-priority" => (
            "Another rewrite in this scope already has this priority, so their order would be a guess.",
            "Move the rewrite to a priority of its own.",
        ),
        "invalid-matcher" => (
            "The pattern is not a valid glob or regular expression.",
            "Correct the pattern; a regular expression must compile and a glob must close its brackets.",
        ),
        "invalid-template" => (
            "The text names a value that is not a parameter of the route or a capture of the match.",
            "Name only parameters the route declares and captures the match produces, as ${name}.",
        ),
        "missing-binding" => (
            "No provider endpoint has this identifier.",
            "Choose a binding from the list of those Toker knows.",
        ),
        "missing-classifier" => (
            "The record names a classifier the policy does not hold.",
            "Choose an existing classifier, or add the one it names.",
        ),
        "missing-regex-capture" => (
            "The route takes a capture the match does not have.",
            "Name a capture the regular expression defines, or add the capture to it.",
        ),
        "missing-route" => (
            "The record sends to a route the policy does not hold.",
            "Choose an existing route, or add the one it names.",
        ),
        "missing-upgrade" => (
            "The candidate names an upgrade policy the policy does not hold.",
            "Choose an existing upgrade policy, or add the one it names.",
        ),
        "native-presentation-shadow-consent-required" => (
            "This name is also the provider's own name for a model, so presenting it hides that model.",
            "Allow the presentation to shadow the native name, or publish it under another name.",
        ),
        "requested-model-undeclared" => (
            "The candidate sends the model the client asked for, and its route does not declare that parameter.",
            "Declare the requested model as a parameter of the route, or send a fixed model.",
        ),
        "route-arguments-mismatch" => (
            "The record gives the route different values from the parameters it declares.",
            "Supply exactly one value for each parameter the route declares.",
        ),
        "route-semantics-invalid" => (
            "The route could not be fingerprinted, so lanes could not be pinned to it.",
            "Simplify the route; this is not expected from a route made in the editor.",
        ),
        // Findings that do not block a save.
        "binding-unconfigured" => (
            "The candidate's provider is not configured, so it is skipped until it is.",
            "Nothing to change to save; configure the provider to have the candidate serve.",
        ),
        "offer-unknown" => (
            "The catalogue does not list this model for the binding, so a fresh catalogue will skip the candidate.",
            "Check the model's spelling, or leave it if the catalogue is simply out of date.",
        ),
        "selector-unresolved" => (
            "The selector picks no offer from the catalogue as it stands.",
            "Widen the selector, or wait for the catalogue to list a match.",
        ),
        "virtual-presentation-unlisted-in-responses" => (
            "Codex lists a model only from a record of its own, and this presentation has none.",
            "Add a Codex record to the presentation to have it listed; it still routes without one.",
        ),
        "codex-model-record-unused" => (
            "The Codex record is used only for a virtual presentation in a Responses scope.",
            "Remove the record, or move the presentation to a Responses scope.",
        ),
        _ => return None,
    };
    Some(Explanation { sentence, fix })
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

    /// Every code the compiler raises explains itself, so the form never shows
    /// a bare code. The codes are read from its source rather than listed
    /// here, so a new one without an explanation fails this.
    #[test]
    fn every_code_the_compiler_raises_has_an_explanation() {
        let source = include_str!("compile.rs");
        let raised = regex::Regex::new(r#"\.(?:error|info|warning)\(\s*"([a-z0-9-]+)""#).unwrap();
        let codes: std::collections::BTreeSet<_> = raised
            .captures_iter(source)
            .map(|captures| captures[1].to_owned())
            .collect();
        assert!(codes.len() >= 20, "found {codes:?}");
        for code in &codes {
            let explanation = explain(code).unwrap_or_else(|| panic!("no explanation for {code}"));
            assert!(explanation.sentence.ends_with('.'), "{code}");
            assert!(explanation.fix.ends_with('.'), "{code}");
        }
        assert_eq!(explain("not-a-code"), None);
    }
}
