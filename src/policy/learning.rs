//! Read-only, binding-keyed learning input for policy resolution.
//!
//! The interface does not expose mutation. Revision-one legacy policy may use
//! the adapter's unattributable rows; authored policy must use a future
//! binding-scoped implementation instead.

use crate::middleware::models::{ModelStore, days_of};
use crate::store::ModelEntry;

use super::BindingId;

/// Learned evidence for one exact backend model identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearnedEvidence {
    distinct_days: Vec<String>,
    max_prompt: Option<u64>,
}

impl LearnedEvidence {
    pub fn distinct_days(&self) -> &[String] {
        &self.distinct_days
    }

    pub fn max_prompt(&self) -> Option<u64> {
        self.max_prompt
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
}

/// Temporary revision-one adapter over the unattributable legacy model table.
pub struct LegacyLearningView<'a> {
    store: &'a ModelStore,
}

impl<'a> LegacyLearningView<'a> {
    pub fn new(store: &'a ModelStore) -> Self {
        Self { store }
    }
}

impl LearningView for LegacyLearningView<'_> {
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
}

impl LearningView for ModelStore {
    type Error = crate::store::Error;

    fn recently_sent(
        &self,
        _binding_id: &BindingId,
        model_id: &str,
    ) -> Result<Option<i64>, Self::Error> {
        Ok(self.last_served(model_id))
    }

    fn learned(
        &self,
        _binding_id: &BindingId,
        model_id: &str,
    ) -> Result<Option<LearnedEvidence>, Self::Error> {
        Ok(self.learned_entry(model_id)?.map(evidence))
    }

    fn known_models(&self, _binding_id: &BindingId) -> Result<Vec<String>, Self::Error> {
        ModelStore::known_models(self)
    }
}

fn evidence(entry: ModelEntry) -> LearnedEvidence {
    LearnedEvidence {
        distinct_days: days_of(&entry),
        max_prompt: entry.max_prompt.and_then(|value| u64::try_from(value).ok()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::middleware::models::ModelStore;
    use crate::store::{ModelEntry, RequestRow, Store};

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
        let view = LegacyLearningView::new(&models);
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
        let view = LegacyLearningView::new(&models);
        let binding = binding("openrouter.openrouter_messages");
        assert_eq!(view.recently_sent(&binding, "unknown-model").unwrap(), None);
        assert_eq!(view.learned(&binding, "unknown-model").unwrap(), None);
        assert!(view.known_models(&binding).unwrap().is_empty());
    }
}
