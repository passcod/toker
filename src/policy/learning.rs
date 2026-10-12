//! Read-only, binding-keyed learning input for policy resolution.
//!
//! The interface does not expose mutation. Revision-one legacy policy may use
//! the adapter's unattributable rows; authored policy must use a future
//! binding-scoped implementation instead.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use crate::catalog::DeclaredWindow;
use crate::middleware::models::{ModelStore, active_days_of, days_of};
use crate::store::{BindingLearning, ModelEntry, Store};

use super::BindingId;

/// Learned evidence for one exact backend model identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearnedEvidence {
    distinct_days: Vec<String>,
    max_prompt: Option<u64>,
    declared_window: Option<DeclaredWindow>,
    explicitly_promoted: bool,
}

impl LearnedEvidence {
    pub fn new(
        distinct_days: Vec<String>,
        max_prompt: Option<u64>,
        declared_window: Option<DeclaredWindow>,
        explicitly_promoted: bool,
    ) -> Self {
        Self {
            distinct_days,
            max_prompt,
            declared_window,
            explicitly_promoted,
        }
    }

    pub fn distinct_days(&self) -> &[String] {
        &self.distinct_days
    }

    pub fn max_prompt(&self) -> Option<u64> {
        self.max_prompt
    }

    pub fn declared_window(&self) -> Option<DeclaredWindow> {
        self.declared_window
    }

    pub fn explicitly_promoted(&self) -> bool {
        self.explicitly_promoted
    }
}

#[derive(Debug, Clone, Default)]
pub struct LearningSnapshot {
    learned: BTreeMap<(Option<BindingId>, String), LearnedEvidence>,
    recent: BTreeMap<(Option<BindingId>, String), i64>,
    covered_since: BTreeMap<Option<BindingId>, Option<i64>>,
    active_days: BTreeMap<Option<BindingId>, usize>,
}

impl LearningView for LearningSnapshot {
    type Error = std::convert::Infallible;

    fn snapshot(&self) -> Result<LearningSnapshot, Self::Error> {
        Ok(self.clone())
    }

    fn recently_sent(
        &self,
        binding_id: &BindingId,
        model_id: &str,
    ) -> Result<RecencyEvidence, Self::Error> {
        let scope = if self.covered_since.contains_key(&Some(binding_id.clone())) {
            Some(binding_id.clone())
        } else if self.covered_since.contains_key(&None) {
            return Ok(RecencyEvidence::Covered {
                last_sent: self.recent.get(&(None, model_id.to_owned())).copied(),
                covered_since: self.covered_since.get(&None).copied().flatten(),
            });
        } else {
            return Ok(RecencyEvidence::Unknown);
        };
        Ok(RecencyEvidence::Covered {
            last_sent: self
                .recent
                .get(&(scope.clone(), model_id.to_owned()))
                .copied(),
            covered_since: self.covered_since.get(&scope).copied().flatten(),
        })
    }

    fn learned(
        &self,
        binding_id: &BindingId,
        model_id: &str,
    ) -> Result<Option<LearnedEvidence>, Self::Error> {
        Ok(self
            .learned
            .get(&(Some(binding_id.clone()), model_id.to_owned()))
            .or_else(|| self.learned.get(&(None, model_id.to_owned())))
            .cloned())
    }

    fn known_models(&self, binding_id: &BindingId) -> Result<Vec<String>, Self::Error> {
        let scoped = self
            .learned
            .keys()
            .filter_map(|(binding, model)| (binding.as_ref() == Some(binding_id)).then_some(model));
        let legacy = self
            .learned
            .keys()
            .filter_map(|(binding, model)| binding.is_none().then_some(model));
        Ok(scoped.chain(legacy).cloned().collect())
    }

    fn active_days(&self, binding_id: &BindingId) -> Result<usize, Self::Error> {
        Ok(self
            .active_days
            .get(&Some(binding_id.clone()))
            .or_else(|| self.active_days.get(&None))
            .copied()
            .unwrap_or_default())
    }
}

/// Recency evidence and the span over which absence is meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecencyEvidence {
    /// This view has no recency record for the requested binding.
    Unknown,
    /// `last_sent` was observed within a record that reaches back to
    /// `covered_since`. `None` coverage means the whole relevant record was
    /// read; binding-attributed recency normally carries that binding's first
    /// attributable send instead.
    Covered {
        last_sent: Option<i64>,
        covered_since: Option<i64>,
    },
}

/// Content-free read interface used by the eventual resolver.
pub trait LearningView {
    type Error;

    /// Capture an owned immutable view for deferred resolution. Synchronous
    /// callers may continue reading `self`; shadow workers must enqueue this
    /// snapshot so later observations cannot change an in-flight decision.
    fn snapshot(&self) -> Result<LearningSnapshot, Self::Error>;

    fn recently_sent(
        &self,
        binding_id: &BindingId,
        model_id: &str,
    ) -> Result<RecencyEvidence, Self::Error>;

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

/// Binding-local observation and recency, separate from revision-one's
/// unattributable [`ModelStore`].
///
/// The recently-sent map is keyed by the selected binding and exact effective
/// target. It is marked immediately before an inference send. Durable learning
/// is merged only after a completed response names its raw provider model.
pub struct BindingLearningView {
    store: Arc<Store>,
    recency: Mutex<BindingRecency>,
}

#[derive(Default)]
struct BindingRecency {
    sent_on: BTreeMap<(BindingId, String), i64>,
    /// Earliest attributable send known for each binding. Unattributed ledger
    /// history cannot prove a binding-local absence before this point.
    covered_since: BTreeMap<BindingId, i64>,
}

impl BindingRecency {
    fn note_sent(&mut self, binding_id: &BindingId, model_id: String, at_ms: i64) {
        self.covered_since
            .entry(binding_id.clone())
            .and_modify(|since| *since = (*since).min(at_ms))
            .or_insert(at_ms);
        self.sent_on
            .entry((binding_id.clone(), model_id))
            .and_modify(|sent_at| *sent_at = (*sent_at).max(at_ms))
            .or_insert(at_ms);
    }
}

impl BindingLearningView {
    /// Build an unseeded view. Runtime construction should use [`Self::seeded`]
    /// so absence retains its ledger coverage.
    pub fn new(store: Arc<Store>) -> Self {
        Self::seeded(store, &[])
    }

    /// Seed recency only from attributable inference rows. A row without both
    /// `binding_id` and `effective_model` proves nothing for this view.
    pub fn seeded(store: Arc<Store>, rows: &[crate::store::RequestRow]) -> Self {
        let mut recency = BindingRecency::default();
        for row in rows {
            // An attributed error row may be a provider response, so it must
            // remain conservatively warm after restart. Local pre-transport
            // failures share this shape; retaining those can only postpone a
            // move, while dropping an actual send can lose a live cache.
            if row
                .kind
                .is_some_and(|kind| kind != crate::store::RowKind::Error)
            {
                continue;
            }
            let (Some(binding_id), Some(model_id)) =
                (row.binding_id.as_ref(), row.effective_model.as_deref())
            else {
                continue;
            };
            let Some(model_id) = exact_effective_model(Some(model_id)) else {
                continue;
            };
            recency.note_sent(binding_id, model_id, row.ts_ms);
        }
        Self {
            store,
            recency: Mutex::new(recency),
        }
    }

    /// Remember an actual inference send before transport starts.
    pub fn note_sent(&self, binding_id: &BindingId, model_id: Option<&str>, at_ms: i64) {
        let Some(model_id) = exact_effective_model(model_id) else {
            return;
        };
        if let Ok(mut recency) = self.recency.lock() {
            recency.note_sent(binding_id, model_id, at_ms);
        }
    }

    /// Merge one completed provider-reported observation into durable,
    /// binding-local learning. Missing response models are rejected by type:
    /// callers cannot invoke this method without naming one.
    pub fn note_observed(
        &self,
        binding_id: &BindingId,
        reported_model: &str,
        at_ms: i64,
        prompt: Option<u64>,
        tz: &jiff::tz::TimeZone,
    ) -> crate::store::Result<()> {
        let Some(model_id) = exact_reported_model(reported_model) else {
            return Ok(());
        };
        let Some(day) = crate::middleware::models::local_day(at_ms, tz) else {
            return Ok(());
        };
        self.store
            .merge_binding_learning(&crate::store::BindingLearningUpdate {
                binding_id: binding_id.clone(),
                model_id,
                days: BTreeSet::from([day]),
                max_prompt: prompt.map(|prompt| i64::try_from(prompt).unwrap_or(i64::MAX)),
                context_window: None,
                explicitly_promoted: false,
                updated_ms: at_ms,
            })?;
        Ok(())
    }
}

fn exact_effective_model(model: Option<&str>) -> Option<String> {
    let model = model?;
    (!model.is_empty()).then(|| model.to_owned())
}

fn exact_reported_model(model: &str) -> Option<String> {
    (!model.trim().is_empty()).then(|| model.to_owned())
}

impl LearningView for BindingLearningView {
    type Error = crate::store::Error;

    fn snapshot(&self) -> Result<LearningSnapshot, Self::Error> {
        let recency = self
            .recency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let recent = recency
            .sent_on
            .iter()
            .map(|((binding, model), last_sent)| {
                ((Some(binding.clone()), model.clone()), *last_sent)
            })
            .collect();
        let covered_since = recency
            .covered_since
            .iter()
            .map(|(binding, since)| (Some(binding.clone()), Some(*since)))
            .collect();
        let mut learned = BTreeMap::new();
        let mut active_days = BTreeMap::new();
        for binding in recency.covered_since.keys() {
            let entries = self.store.load_binding_learnings(binding)?;
            let days = entries
                .iter()
                .flat_map(|entry| entry.days.iter().cloned())
                .collect::<BTreeSet<_>>()
                .len();
            active_days.insert(Some(binding.clone()), days);
            for entry in entries {
                learned.insert(
                    (Some(binding.clone()), entry.model_id.clone()),
                    binding_evidence(entry),
                );
            }
        }
        Ok(LearningSnapshot {
            learned,
            recent,
            covered_since,
            active_days,
        })
    }

    fn recently_sent(
        &self,
        binding_id: &BindingId,
        model_id: &str,
    ) -> Result<RecencyEvidence, Self::Error> {
        let Some(model_id) = exact_effective_model(Some(model_id)) else {
            return Ok(RecencyEvidence::Unknown);
        };
        let recency = match self.recency.lock() {
            Ok(recency) => recency,
            Err(_) => return Ok(RecencyEvidence::Unknown),
        };
        let Some(covered_since) = recency.covered_since.get(binding_id).copied() else {
            return Ok(RecencyEvidence::Unknown);
        };
        Ok(RecencyEvidence::Covered {
            last_sent: recency
                .sent_on
                .get(&(binding_id.clone(), model_id))
                .copied(),
            covered_since: Some(covered_since),
        })
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

    fn snapshot(&self) -> Result<LearningSnapshot, Self::Error> {
        let entries = self.store.learned_entries()?;
        let active_days = active_days_of(&entries);
        let learned = entries
            .into_iter()
            .map(|entry| ((None, entry.model_id.clone()), evidence(entry)))
            .collect();
        let covered_since = self.store.covered_since();
        let recent = self
            .store
            .recently_served()
            .into_iter()
            .map(|(model, at)| ((None, model), at))
            .collect();
        Ok(LearningSnapshot {
            learned,
            recent,
            covered_since: BTreeMap::from([(None, covered_since)]),
            active_days: BTreeMap::from([(None, active_days)]),
        })
    }

    fn recently_sent(
        &self,
        _binding_id: &BindingId,
        model_id: &str,
    ) -> Result<RecencyEvidence, Self::Error> {
        Ok(self.store.last_served_record(model_id).map_or(
            RecencyEvidence::Unknown,
            |(last_sent, covered_since)| RecencyEvidence::Covered {
                last_sent,
                covered_since,
            },
        ))
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
        declared_window: entry.context_window,
        explicitly_promoted: entry.explicitly_promoted,
    }
}

fn evidence(entry: ModelEntry) -> LearnedEvidence {
    LearnedEvidence {
        distinct_days: days_of(&entry),
        max_prompt: entry.max_prompt.and_then(|value| u64::try_from(value).ok()),
        declared_window: None,
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
            binding_id: None,
            policy_routing: None,
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
            RecencyEvidence::Covered {
                last_sent: Some(9_000),
                covered_since: Some(5_000),
            }
        );
        assert_eq!(
            view.recently_sent(&second, "claude-sonnet-5").unwrap(),
            RecencyEvidence::Covered {
                last_sent: Some(9_000),
                covered_since: Some(5_000),
            }
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
        assert_eq!(
            view.recently_sent(&binding, "unknown-model").unwrap(),
            RecencyEvidence::Covered {
                last_sent: None,
                covered_since: None,
            }
        );
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
                context_window: DeclaredWindow::new(100, 200),
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
        let view = BindingLearningView::seeded(store, &[]);

        let learned = view.learned(&first, "shared-name").unwrap().unwrap();
        assert_eq!(learned.distinct_days(), ["2026-10-01", "2026-10-02"]);
        assert_eq!(learned.max_prompt(), Some(100));
        assert_eq!(learned.declared_window(), DeclaredWindow::new(100, 200));
        assert!(learned.explicitly_promoted());
        assert_eq!(view.learned(&second, "shared-name").unwrap(), None);
        assert_eq!(view.learned(&first, "legacy-only").unwrap(), None);
        assert_eq!(view.known_models(&first).unwrap(), ["shared-name"]);
        assert_eq!(view.known_models(&second).unwrap(), ["other"]);
        assert_eq!(view.active_days(&first).unwrap(), 2);
        assert_eq!(view.active_days(&second).unwrap(), 1);
        assert_eq!(
            view.recently_sent(&first, "shared-name").unwrap(),
            RecencyEvidence::Unknown
        );
    }

    #[test]
    fn binding_recency_seeds_only_attributable_effective_targets() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let first = binding("anthropic_sub.anthropic_messages");
        let second = binding("anthropic_api.anthropic_messages");
        let mut first_row = row("provider-reported-is-not-the-send-target", 9_000);
        first_row.binding_id = Some(first.clone());
        first_row.effective_model = Some("same-model".to_owned());
        let mut second_row = row("other-provider-report", 10_000);
        second_row.binding_id = Some(second.clone());
        second_row.effective_model = Some("same-model".to_owned());
        let mut error_row = row("not-reported", 13_000);
        error_row.kind = Some(crate::store::RowKind::Error);
        error_row.binding_id = Some(first.clone());
        error_row.effective_model = Some("errored-effective".to_owned());
        let missing_binding = row("same-model", 11_000);
        let mut missing_effective = row("same-model", 12_000);
        missing_effective.binding_id = Some(first.clone());

        let view = BindingLearningView::seeded(
            store,
            &[
                first_row,
                second_row,
                error_row,
                missing_binding,
                missing_effective,
            ],
        );

        assert_eq!(
            view.recently_sent(&first, "same-model").unwrap(),
            RecencyEvidence::Covered {
                last_sent: Some(9_000),
                covered_since: Some(9_000),
            }
        );
        assert_eq!(
            view.recently_sent(&second, "same-model").unwrap(),
            RecencyEvidence::Covered {
                last_sent: Some(10_000),
                covered_since: Some(10_000),
            }
        );
        assert_eq!(
            view.recently_sent(&first, "provider-reported-is-not-the-send-target")
                .unwrap(),
            RecencyEvidence::Covered {
                last_sent: None,
                covered_since: Some(9_000),
            }
        );
        assert_eq!(
            view.recently_sent(&first, "errored-effective").unwrap(),
            RecencyEvidence::Covered {
                last_sent: Some(13_000),
                covered_since: Some(9_000),
            },
            "a possible provider send must remain warm after restart",
        );
        let snapshot = view.snapshot().unwrap();
        assert_eq!(
            snapshot.recently_sent(&first, "same-model").unwrap(),
            RecencyEvidence::Covered {
                last_sent: Some(9_000),
                covered_since: Some(9_000),
            }
        );
        view.note_sent(&first, Some("same-model"), 20_000);
        assert_eq!(
            snapshot.recently_sent(&first, "same-model").unwrap(),
            RecencyEvidence::Covered {
                last_sent: Some(9_000),
                covered_since: Some(9_000),
            },
            "owned snapshots cannot observe later sends"
        );
    }

    #[test]
    fn unattributed_history_cannot_establish_another_effective_target_cold() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let binding = binding("anthropic_api.anthropic_messages");
        let old_unattributed = row("old-model", 1_000);
        let mut first_attributed = row("reported", 10_000);
        first_attributed.binding_id = Some(binding.clone());
        first_attributed.effective_model = Some("recent-model".to_owned());
        let view = BindingLearningView::seeded(store, &[old_unattributed, first_attributed]);

        assert_eq!(
            view.recently_sent(&binding, "another-model").unwrap(),
            RecencyEvidence::Covered {
                last_sent: None,
                covered_since: Some(10_000),
            },
            "coverage starts at this binding's first attributable send",
        );
    }

    #[test]
    fn note_sent_is_binding_local_and_visible_before_completion() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let first = binding("anthropic_sub.anthropic_messages");
        let second = binding("anthropic_api.anthropic_messages");
        let view = BindingLearningView::seeded(store, &[]);

        view.note_sent(&first, Some("same-model"), 20_000);

        assert_eq!(
            view.recently_sent(&first, "same-model").unwrap(),
            RecencyEvidence::Covered {
                last_sent: Some(20_000),
                covered_since: Some(20_000),
            }
        );
        assert_eq!(
            view.recently_sent(&second, "same-model").unwrap(),
            RecencyEvidence::Unknown
        );
        assert_eq!(view.learned(&first, "same-model").unwrap(), None);
    }

    #[test]
    fn exact_effective_aliases_are_separate_cache_identities() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let binding = binding("anthropic_api.anthropic_messages");
        let view = BindingLearningView::seeded(store, &[]);

        view.note_sent(&binding, Some("claude-haiku-4-5-20251001"), 30_000);
        view.note_sent(&binding, Some("Claude-Haiku-4-5"), 31_000);
        view.note_sent(&binding, Some("vendor/[claude-haiku-4-5]"), 32_000);
        view.note_sent(&binding, Some(" spaced-model "), 33_000);

        assert_eq!(
            view.recently_sent(&binding, "claude-haiku-4-5-20251001")
                .unwrap(),
            RecencyEvidence::Covered {
                last_sent: Some(30_000),
                covered_since: Some(30_000),
            }
        );
        for (model, sent_at) in [
            ("Claude-Haiku-4-5", 31_000),
            ("vendor/[claude-haiku-4-5]", 32_000),
            (" spaced-model ", 33_000),
        ] {
            assert_eq!(
                view.recently_sent(&binding, model).unwrap(),
                RecencyEvidence::Covered {
                    last_sent: Some(sent_at),
                    covered_since: Some(30_000),
                }
            );
        }
        for model in [
            "claude-haiku-4-5",
            "claude-haiku-4-5-20251001 ",
            "claude-haiku-4-5",
            "spaced-model",
        ] {
            assert_eq!(
                view.recently_sent(&binding, model).unwrap(),
                RecencyEvidence::Covered {
                    last_sent: None,
                    covered_since: Some(30_000),
                }
            );
        }
    }

    #[test]
    fn completed_reported_models_merge_only_under_the_selected_binding() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let first = binding("anthropic_sub.anthropic_messages");
        let second = binding("anthropic_api.anthropic_messages");
        let view = BindingLearningView::seeded(store, &[]);
        let tz = jiff::tz::TimeZone::UTC;

        view.note_observed(&first, "same-model", 1_769_954_400_000, Some(100), &tz)
            .unwrap();
        view.note_observed(&first, "same-model", 1_769_958_000_000, Some(150), &tz)
            .unwrap();
        view.note_observed(&second, "same-model", 1_770_044_400_000, Some(70), &tz)
            .unwrap();
        view.note_observed(&first, "day-only-model", 1_770_044_400_000, None, &tz)
            .unwrap();
        view.note_observed(
            &first,
            "Claude-Haiku-4-5-20251001",
            1_770_044_400_000,
            Some(200),
            &tz,
        )
        .unwrap();

        let first_learned = view.learned(&first, "same-model").unwrap().unwrap();
        assert_eq!(first_learned.distinct_days(), ["2026-02-01"]);
        assert_eq!(first_learned.max_prompt(), Some(150));
        let second_learned = view.learned(&second, "same-model").unwrap().unwrap();
        assert_eq!(second_learned.distinct_days(), ["2026-02-02"]);
        assert_eq!(second_learned.max_prompt(), Some(70));
        let day_only = view.learned(&first, "day-only-model").unwrap().unwrap();
        assert_eq!(day_only.distinct_days(), ["2026-02-02"]);
        assert_eq!(day_only.max_prompt(), None);
        let snapshot = view
            .learned(&first, "Claude-Haiku-4-5-20251001")
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.max_prompt(), Some(200));
        assert_eq!(view.learned(&first, "claude-haiku-4-5").unwrap(), None);
    }
}
