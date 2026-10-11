//! Read-only, binding-keyed learning input for policy resolution.
//!
//! The interface does not expose mutation. Revision-one legacy policy may use
//! the adapter's unattributable rows; authored policy must use a future
//! binding-scoped implementation instead.

use std::sync::Arc;

use crate::middleware::models::{ModelStore, active_days_of, days_of};
use crate::store::{BindingLearning, ModelEntry, Store};

use super::BindingId;

/// Learned evidence for one exact backend model identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearnedEvidence {
    distinct_days: Vec<String>,
    max_prompt: Option<u64>,
    explicitly_promoted: bool,
}

impl LearnedEvidence {
    pub fn new(
        distinct_days: Vec<String>,
        max_prompt: Option<u64>,
        explicitly_promoted: bool,
    ) -> Self {
        Self {
            distinct_days,
            max_prompt,
            explicitly_promoted,
        }
    }

    pub fn distinct_days(&self) -> &[String] {
        &self.distinct_days
    }

    pub fn max_prompt(&self) -> Option<u64> {
        self.max_prompt
    }

    pub fn explicitly_promoted(&self) -> bool {
        self.explicitly_promoted
    }
}

/// Content-free read interface used by the eventual resolver.
pub trait LearningView {
    type Error;

    fn recently_sent(
        &self,
        binding_id: &BindingId,
        model_id: &str,
    ) -> Result<Option<i64>, Self::Error>;

    fn learned(
        &self,
        binding_id: &BindingId,
        model_id: &str,
    ) -> Result<Option<LearnedEvidence>, Self::Error>;

    fn known_models(&self, binding_id: &BindingId) -> Result<Vec<String>, Self::Error>;

    /// Number of distinct days carrying any evidence in this binding's
    /// learning pool. Revision-one's adapter intentionally returns the legacy
    /// table-wide pool because those rows cannot be attributed to bindings.
    fn active_days(&self, binding_id: &BindingId) -> Result<usize, Self::Error>;
}

/// Temporary revision-one adapter over the unattributable legacy model table.
pub struct LegacyLearningView {
    store: Arc<ModelStore>,
}

impl LegacyLearningView {
    pub fn new(store: Arc<ModelStore>) -> Self {
        Self { store }
    }
}

/// Binding-local learning over the non-authoritative routing state tables.
///
/// Recently-sent evidence has a separate write path and is not persisted in
/// this storage slice, so this view reports it as absent rather than borrowing
/// the unattributable legacy map.
pub struct BindingLearningView {
    store: Arc<Store>,
}

impl BindingLearningView {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

impl LearningView for BindingLearningView {
    type Error = crate::store::Error;

    fn recently_sent(
        &self,
        _binding_id: &BindingId,
        _model_id: &str,
    ) -> Result<Option<i64>, Self::Error> {
        Ok(None)
    }

    fn learned(
        &self,
        binding_id: &BindingId,
        model_id: &str,
    ) -> Result<Option<LearnedEvidence>, Self::Error> {
        Ok(self
            .store
            .load_binding_learning(binding_id, model_id)?
            .map(binding_evidence))
    }

    fn known_models(&self, binding_id: &BindingId) -> Result<Vec<String>, Self::Error> {
        Ok(self
            .store
            .load_binding_learnings(binding_id)?
            .into_iter()
            .map(|entry| entry.model_id)
            .collect())
    }

    fn active_days(&self, binding_id: &BindingId) -> Result<usize, Self::Error> {
        let mut days = std::collections::BTreeSet::new();
        for entry in self.store.load_binding_learnings(binding_id)? {
            days.extend(entry.days);
        }
        Ok(days.len())
    }
}

impl LearningView for LegacyLearningView {
    type Error = crate::store::Error;

    fn recently_sent(
        &self,
        _binding_id: &BindingId,
        model_id: &str,
    ) -> Result<Option<i64>, Self::Error> {
        Ok(self.store.last_served(model_id))
    }

    fn learned(
        &self,
        _binding_id: &BindingId,
        model_id: &str,
    ) -> Result<Option<LearnedEvidence>, Self::Error> {
        Ok(self.store.learned_entry(model_id)?.map(evidence))
    }

    fn known_models(&self, _binding_id: &BindingId) -> Result<Vec<String>, Self::Error> {
        self.store.known_models()
    }

    fn active_days(&self, _binding_id: &BindingId) -> Result<usize, Self::Error> {
        Ok(active_days_of(&self.store.learned_entries()?))
    }
}

fn binding_evidence(entry: BindingLearning) -> LearnedEvidence {
    LearnedEvidence {
        distinct_days: entry.days.into_iter().collect(),
        max_prompt: entry.max_prompt.and_then(|value| u64::try_from(value).ok()),
        explicitly_promoted: entry.explicitly_promoted,
    }
}

fn evidence(entry: ModelEntry) -> LearnedEvidence {
    LearnedEvidence {
        distinct_days: days_of(&entry),
        max_prompt: entry.max_prompt.and_then(|value| u64::try_from(value).ok()),
        // Legacy grants are encoded by adding ordinary days, so the legacy
        // table cannot distinguish an explicit grant from observations.
        explicitly_promoted: false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::middleware::models::ModelStore;
    use crate::store::{BindingLearningUpdate, ModelEntry, RequestRow, Store};

    fn binding(value: &str) -> BindingId {
        BindingId::new(value).unwrap()
    }

    fn row(model: &str, at: i64) -> RequestRow {
        RequestRow {
            id: None,
            ts_ms: at,
            duration_ms: None,
            kind: None,
            frontend: None,
            provider: None,
            route: None,
            session_id: None,
            ping: None,
            model: None,
            raw_model: Some(model.to_owned()),
            requested_model: None,
            effective_model: None,
            input: None,
            cache_read: None,
            cache_write_total: None,
            cache_write_5m: None,
            cache_write_1h: None,
            output: None,
            reasoning: None,
            iterations: None,
            web_searches: None,
            code_execs: None,
            ttl_split_known: None,
            usage_presence: None,
            usage_raw: None,
            cost_usd: None,
            cost_kind: None,
            rate_limits: None,
            req_bytes: None,
            req_messages: None,
            req_tools: None,
            tools_hash: None,
            system_chars: None,
            system_hash: None,
            system_blocks: None,
            system_messages: None,
            compact_generations: None,
            summarising: None,
            system_change: None,
            system_ladder: None,
            system_tail: None,
            gate_on: None,
            cold_on: None,
            forced_from: None,
            forced_to: None,
            downgraded_from: None,
            downgraded_to: None,
            cache_stripped: None,
            system_merged: None,
            model_mappings: None,
            drift_digest: None,
            status: None,
            error_type: None,
            retry_after_ms: None,
            extra: None,
            betas: None,
            geo: None,
            fast: None,
            policy_revision_id: None,
            presentation_id: None,
            rewrite_id: None,
            policy_route_id: None,
            route_fingerprint: None,
            candidate_id: None,
            binding_id: None,
            baseline_model: None,
            attempt_group_id: None,
            attempt_index: None,
            fallback_reason: None,
            routing_trace: None,
            lineage: None,
        }
    }

    #[test]
    fn legacy_adapter_accepts_binding_keys_but_reads_revision_one_rows() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        store
            .upsert_model(&ModelEntry {
                model_id: "claude-sonnet-5".to_owned(),
                days_json: Some(json!(["2026-10-01", "2026-10-01", "2026-10-02"])),
                max_prompt: Some(123_456),
                context_window_json: None,
            })
            .unwrap();
        let models = ModelStore::seeded(store, &[row("claude-sonnet-5", 9_000)], Some(5_000));
        let view = LegacyLearningView::new(Arc::new(models));
        let first = binding("anthropic_sub.anthropic_messages");
        let second = binding("anthropic_api.anthropic_messages");

        assert_eq!(
            view.recently_sent(&first, "claude-sonnet-5").unwrap(),
            Some(9_000)
        );
        assert_eq!(
            view.recently_sent(&second, "claude-sonnet-5").unwrap(),
            Some(9_000)
        );
        let learned = view.learned(&first, "claude-sonnet-5").unwrap().unwrap();
        assert_eq!(learned.distinct_days(), ["2026-10-01", "2026-10-02"]);
        assert_eq!(learned.max_prompt(), Some(123_456));
        assert_eq!(view.known_models(&second).unwrap(), ["claude-sonnet-5"]);
    }

    #[test]
    fn reads_do_not_create_unknown_learning() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let models = ModelStore::seeded(store, &[], None);
        let view = LegacyLearningView::new(Arc::new(models));
        let binding = binding("openrouter.openrouter_messages");
        assert_eq!(view.recently_sent(&binding, "unknown-model").unwrap(), None);
        assert_eq!(view.learned(&binding, "unknown-model").unwrap(), None);
        assert!(view.known_models(&binding).unwrap().is_empty());
    }
    #[test]
    fn binding_adapter_isolates_bindings_and_never_falls_back_to_legacy_rows() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let first = binding("anthropic_sub.anthropic_messages");
        let second = binding("anthropic_api.anthropic_messages");
        store
            .upsert_model(&ModelEntry {
                model_id: "legacy-only".to_owned(),
                days_json: Some(json!(["2026-10-01"])),
                max_prompt: Some(999),
                context_window_json: None,
            })
            .unwrap();
        store
            .merge_binding_learning(&BindingLearningUpdate {
                binding_id: first.clone(),
                model_id: "shared-name".to_owned(),
                days: ["2026-10-01".to_owned(), "2026-10-02".to_owned()]
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
                max_prompt: Some(100),
                context_window: None,
                explicitly_promoted: true,
                updated_ms: 10,
            })
            .unwrap();
        store
            .merge_binding_learning(&BindingLearningUpdate {
                binding_id: second.clone(),
                model_id: "other".to_owned(),
                days: ["2026-10-03".to_owned()]
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
                max_prompt: Some(200),
                context_window: None,
                explicitly_promoted: false,
                updated_ms: 11,
            })
            .unwrap();
        let view = BindingLearningView::new(store);

        let learned = view.learned(&first, "shared-name").unwrap().unwrap();
        assert_eq!(learned.distinct_days(), ["2026-10-01", "2026-10-02"]);
        assert_eq!(learned.max_prompt(), Some(100));
        assert!(learned.explicitly_promoted());
        assert_eq!(view.learned(&second, "shared-name").unwrap(), None);
        assert_eq!(view.learned(&first, "legacy-only").unwrap(), None);
        assert_eq!(view.known_models(&first).unwrap(), ["shared-name"]);
        assert_eq!(view.known_models(&second).unwrap(), ["other"]);
        assert_eq!(view.active_days(&first).unwrap(), 2);
        assert_eq!(view.active_days(&second).unwrap(), 1);
        assert_eq!(view.recently_sent(&first, "shared-name").unwrap(), None);
    }
}
