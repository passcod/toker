//! Shared immutable canonical baseline and prepared inference attempts.
//!
//! Frontend parsing and request-global gates produce one [`InferenceRequest`].
//! Each candidate is prepared from a fresh clone, then backend-specific routing
//! transforms and rendering may mutate only that private [`InferenceAttempt`].

use std::sync::Arc;

use crate::ir::canonical::CanonicalRequest;
use crate::middleware::cold::RetargetOutcome;
use crate::middleware::model_map::{MapTarget, ModelMap};
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
        baseline_model: Option<String>,
    ) -> InferenceAttempt {
        let mut canonical = self.canonical();
        canonical.model.clone_from(&baseline_model);
        InferenceAttempt {
            target,
            requested_model: self.requested_model.clone(),
            effective_model: baseline_model,
            mapped: None,
            canonical,
        }
    }

    pub(crate) fn prepare_compaction(
        &self,
        target: ModelTarget,
        compaction_model: Option<&str>,
        model_map: Option<&ModelMap>,
    ) -> Option<(InferenceAttempt, RetargetOutcome)> {
        let baseline_model = self.canonical.model.clone();
        let mut attempt = self.prepare(target, baseline_model);
        let outcome = attempt.apply_compaction(compaction_model, model_map)?;
        Some((attempt, outcome))
    }

    /// Temporary bridge for Messages while its remaining candidate-local
    /// transforms are moved behind `prepare`. The method name prevents a
    /// transformed request from masquerading as the immutable baseline.
    pub(crate) fn prepare_legacy_transformed(
        &self,
        target: ModelTarget,
        effective_model: Option<String>,
        canonical: CanonicalRequest,
    ) -> InferenceAttempt {
        self.prepare_legacy_transformed_with_map(target, effective_model, canonical, None)
    }

    pub(crate) fn prepare_legacy_transformed_with_map(
        &self,
        target: ModelTarget,
        baseline_model: Option<String>,
        mut canonical: CanonicalRequest,
        model_map: Option<&ModelMap>,
    ) -> InferenceAttempt {
        canonical.model.clone_from(&baseline_model);
        let mut attempt = InferenceAttempt {
            target,
            requested_model: self.requested_model.clone(),
            effective_model: baseline_model,
            mapped: None,
            canonical,
        };
        attempt.apply_model_map(model_map);
        attempt
    }
}

pub(crate) struct InferenceAttempt {
    target: ModelTarget,
    requested_model: Option<String>,
    effective_model: Option<String>,
    mapped: Option<MapTarget>,
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

    pub(crate) fn mapped(&self) -> Option<&MapTarget> {
        self.mapped.as_ref()
    }

    pub(crate) fn apply_compaction(
        &mut self,
        compaction_model: Option<&str>,
        model_map: Option<&ModelMap>,
    ) -> Option<RetargetOutcome> {
        let outcome = crate::middleware::canonical::retarget_compaction(
            &mut self.canonical,
            compaction_model,
            true,
            model_map,
        )?;
        self.effective_model.clone_from(&self.canonical.model);
        Some(outcome)
    }

    pub(crate) fn apply_model_map(&mut self, model_map: Option<&ModelMap>) {
        self.mapped = crate::middleware::canonical::map_model(&mut self.canonical, model_map);
        self.effective_model.clone_from(&self.canonical.model);
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
    use crate::middleware::model_map::parse_model_map;
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
        let attempt = request.prepare(target, Some("invented-model".to_owned()));

        assert_eq!(attempt.backend().id(), "openrouter");
        assert_eq!(attempt.binding().provider(), "openrouter");
        assert_eq!(attempt.requested_model(), Some("openrouter/invented-model"));
        assert_eq!(attempt.effective_model(), Some("invented-model"));
        let second = attempt.canonical();
        assert_eq!(second.model.as_deref(), Some("invented-model"));
        assert_eq!(second.system.len(), 1);
    }

    #[test]
    fn compaction_is_candidate_local_and_preserves_the_baseline() {
        let provider: Arc<dyn Provider> = Arc::new(OpenRouter::new(
            "https://openrouter.ai/api/v1".parse().unwrap(),
            None,
        ));
        let registry = RouteRegistry::new(
            [provider],
            [(ProtocolId::OpenAiChat, "openrouter".to_owned())],
        );
        let target = registry
            .resolve(ProtocolId::OpenAiChat, Some("openrouter/claude-opus-4-1"))
            .unwrap();
        let mut canonical = CanonicalRequest {
            model: Some("claude-opus-4-1".to_owned()),
            ..CanonicalRequest::default()
        };
        canonical.messages.push(crate::ir::canonical::CanonMessage {
            role: crate::ir::canonical::CanonRole::User,
            blocks: vec![crate::ir::canonical::CanonBlock::Text("prompt".to_owned())],
            extensions: vec![],
        });
        let request =
            InferenceRequest::new(Some("openrouter/claude-opus-4-1".to_owned()), canonical);
        let (attempt, outcome) = request
            .prepare_compaction(target, Some("claude-haiku-3-5"), None)
            .expect("retargeted");

        assert_eq!(outcome.from.as_deref(), Some("claude-opus-4-1"));
        assert_eq!(outcome.to.as_deref(), Some("claude-haiku-3-5"));
        assert_eq!(attempt.effective_model(), Some("claude-haiku-3-5"));
        assert_eq!(
            request.canonical().model.as_deref(),
            Some("claude-opus-4-1")
        );
    }

    #[test]
    fn provider_mapping_is_candidate_local_and_keeps_baseline_identity() {
        let provider: Arc<dyn Provider> = Arc::new(OpenRouter::new(
            "https://openrouter.ai/api/v1".parse().unwrap(),
            None,
        ));
        let registry = RouteRegistry::new(
            [provider],
            [(ProtocolId::OpenAiChat, "openrouter".to_owned())],
        );
        let target = registry
            .resolve(ProtocolId::OpenAiChat, Some("openrouter/asked-model"))
            .unwrap();
        let request = InferenceRequest::new(
            Some("openrouter/asked-model".to_owned()),
            CanonicalRequest {
                model: Some("openrouter/asked-model".to_owned()),
                ..CanonicalRequest::default()
            },
        );
        let map = parse_model_map(r#"{"model:asked-model":"served-model"}"#)
            .unwrap()
            .unwrap();
        let mut mapped = request.prepare(target.clone(), Some("asked-model".to_owned()));
        mapped.apply_model_map(Some(&map));
        let plain = request.prepare(target, Some("asked-model".to_owned()));

        assert_eq!(mapped.effective_model(), Some("served-model"));
        assert_eq!(mapped.canonical().model.as_deref(), Some("served-model"));
        assert_eq!(plain.effective_model(), Some("asked-model"));
        assert_eq!(
            request.canonical().model.as_deref(),
            Some("openrouter/asked-model")
        );
    }
}
