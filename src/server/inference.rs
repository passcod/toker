//! Shared identity and clone boundary for prepared inference attempts.
//!
//! This does not yet own the earlier post-gate canonical baseline: Messages
//! still applies its licensed routing transforms before constructing the value.
//! It does ensure backend renderers receive a private canonical clone while the
//! frontend's requested identity remains separate from the effective identity.

use std::sync::Arc;

use crate::ir::canonical::CanonicalRequest;
use crate::providers::Provider;
use crate::routing::{BackendBinding, ModelTarget};

#[derive(Clone)]
pub(crate) struct InferenceAttempt {
    target: ModelTarget,
    requested_model: Option<String>,
    effective_model: Option<String>,
    canonical: Arc<CanonicalRequest>,
}

impl InferenceAttempt {
    pub(crate) fn new(
        target: ModelTarget,
        requested_model: Option<String>,
        effective_model: Option<String>,
        canonical: CanonicalRequest,
    ) -> Self {
        Self {
            target,
            requested_model,
            effective_model,
            canonical: Arc::new(canonical),
        }
    }

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

    /// Build one isolated renderer input. Renderer-local mutations never alter
    /// the stored prepared request or either explicit model identity.
    pub(crate) fn canonical(&self) -> CanonicalRequest {
        let mut request = self.canonical.as_ref().clone();
        request.model.clone_from(&self.effective_model);
        request
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::providers::{OpenRouter, Provider};
    use crate::routing::{ProtocolId, RouteRegistry};

    #[test]
    fn every_attempt_gets_a_fresh_canonical_request() {
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
        let attempt = InferenceAttempt::new(
            target,
            Some("openrouter/invented-model".to_owned()),
            Some("invented-model".to_owned()),
            original,
        );

        let mut first = attempt.canonical();
        first.model = Some("mutated-by-first-adapter".to_owned());
        first.system.clear();
        let second = attempt.canonical();

        assert_eq!(second.model.as_deref(), Some("invented-model"));
        assert_eq!(second.system.len(), 1);
        assert_eq!(attempt.backend().id(), "openrouter");
        assert_eq!(attempt.binding().provider(), "openrouter");
        assert_eq!(attempt.requested_model(), Some("openrouter/invented-model"));
        assert_eq!(attempt.effective_model(), Some("invented-model"));
    }
}
