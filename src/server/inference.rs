//! Shared immutable canonical baseline and prepared inference attempts.
//!
//! Frontend parsing and request-global gates produce one [`InferenceRequest`].
//! Each candidate is prepared from a fresh clone, then backend-specific routing
//! transforms and rendering may mutate only that private [`InferenceAttempt`].

use std::sync::Arc;

use crate::ir::canonical::CanonicalRequest;
use crate::providers::Provider;
use crate::routing::{BackendBinding, ModelTarget};

#[derive(Clone)]
pub(crate) struct InferenceRequest {
    requested_model: Option<String>,
    canonical: Arc<CanonicalRequest>,
}

impl InferenceRequest {
    pub(crate) fn new(requested_model: Option<String>, canonical: CanonicalRequest) -> Self {
        Self {
            requested_model,
            canonical: Arc::new(canonical),
        }
    }

    /// Clone the unchanged request-global baseline for candidate-local policy.
    pub(crate) fn canonical(&self) -> CanonicalRequest {
        self.canonical.as_ref().clone()
    }

    pub(crate) fn prepare(
        &self,
        target: ModelTarget,
        effective_model: Option<String>,
        canonical: CanonicalRequest,
    ) -> InferenceAttempt {
        InferenceAttempt {
            target,
            requested_model: self.requested_model.clone(),
            effective_model,
            canonical,
        }
    }
}

pub(crate) struct InferenceAttempt {
    target: ModelTarget,
    requested_model: Option<String>,
    effective_model: Option<String>,
    canonical: CanonicalRequest,
}

impl InferenceAttempt {
    pub(crate) fn backend(&self) -> &Arc<dyn Provider> {
        self.target.provider()
    }

    pub(crate) fn binding(&self) -> &BackendBinding {
        self.target.binding()
    }

    pub(crate) fn requested_model(&self) -> Option<&str> {
        self.requested_model.as_deref()
    }

    pub(crate) fn effective_model(&self) -> Option<&str> {
        self.effective_model.as_deref()
    }

    /// Clone this candidate's private canonical IR for one backend renderer.
    pub(crate) fn canonical(&self) -> CanonicalRequest {
        let mut canonical = self.canonical.clone();
        canonical.model.clone_from(&self.effective_model);
        canonical
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::providers::{OpenRouter, Provider};
    use crate::routing::{ProtocolId, RouteRegistry};

    #[test]
    fn candidates_start_from_fresh_baseline_clones_with_separate_identities() {
        let provider: Arc<dyn Provider> = Arc::new(OpenRouter::new(
            "https://openrouter.ai/api/v1".parse().unwrap(),
            None,
        ));
        let registry = RouteRegistry::new(
            [provider],
            [(ProtocolId::OpenAiChat, "openrouter".to_owned())],
        );
        let target = registry
            .resolve(ProtocolId::OpenAiChat, Some("openrouter/invented-model"))
            .unwrap();
        let mut original = CanonicalRequest {
            model: Some("openrouter/invented-model".to_owned()),
            ..CanonicalRequest::default()
        };
        original
            .system
            .push(crate::ir::canonical::CanonSystemPart::text(
                "private fixture",
            ));
        let request = InferenceRequest::new(Some("openrouter/invented-model".to_owned()), original);

        let mut first = request.canonical();
        first.model = Some("mutated-by-first-candidate".to_owned());
        first.system.clear();
        let attempt = request.prepare(
            target,
            Some("invented-model".to_owned()),
            request.canonical(),
        );

        assert_eq!(attempt.backend().id(), "openrouter");
        assert_eq!(attempt.binding().provider(), "openrouter");
        assert_eq!(attempt.requested_model(), Some("openrouter/invented-model"));
        assert_eq!(attempt.effective_model(), Some("invented-model"));
        let second = attempt.canonical();
        assert_eq!(second.model.as_deref(), Some("invented-model"));
        assert_eq!(second.system.len(), 1);
    }
}
