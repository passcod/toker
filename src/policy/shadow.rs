//! Non-authoritative observation of legacy-policy equivalence.
//!
//! Shadow comparison receives already-derived facts and an already-made legacy
//! decision. It can only resolve immutable inputs and notify an observer. All
//! failures and panics are caught and never reach request handling.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, RwLock};

use super::{
    AttemptPlan, AvailabilitySnapshot, CompiledPolicy, LaneFacts, LearningView, RequestFacts,
    ResolveFailure, RouteProvenance, resolve,
};
use crate::catalog::offers::OfferInventory;

/// Content-free projection shared by legacy and shadow decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowOutcome {
    pub protocol: super::FrontendProtocol,
    pub profile: Option<super::ProfileId>,
    pub requested_model: Option<String>,
    pub provider: String,
    pub binding_id: super::BindingId,
    pub route_source: String,
    pub baseline_model: String,
    pub upgraded_model: Option<String>,
    pub aliased_model: Option<String>,
    pub effective_model: String,
    pub compaction: bool,
}

/// The authoritative decision already made by existing code.
pub type LegacyOutcome = ShadowOutcome;

impl ShadowOutcome {
    pub fn from_plan(facts: &RequestFacts, plan: &AttemptPlan) -> Self {
        Self {
            protocol: facts.protocol,
            profile: facts.profile.clone(),
            requested_model: facts.requested_model.clone(),
            provider: plan.provider.clone(),
            binding_id: plan.binding_id.clone(),
            route_source: provenance_code(&plan.provenance),
            baseline_model: plan.baseline_model.clone(),
            upgraded_model: plan.upgraded_model.clone(),
            aliased_model: plan.aliased_model.clone(),
            effective_model: plan.effective_model.clone(),
            compaction: plan.compaction,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowComparison {
    Match,
    Mismatch,
    InternalError,
}

/// Stable content-free observer record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowRecord {
    pub comparison: ShadowComparison,
    pub code: &'static str,
    pub legacy: Option<LegacyOutcome>,
    pub shadow: Option<ShadowOutcome>,
}

/// Observation is deliberately one-way; an observer cannot return a value that
/// might influence request handling.
pub trait ShadowObserver: Send + Sync + 'static {
    fn observe(&self, record: ShadowRecord);
}

#[derive(Debug, Default)]
pub struct TracingObserver;

impl ShadowObserver for TracingObserver {
    fn observe(&self, record: ShadowRecord) {
        match record.comparison {
            ShadowComparison::Match => tracing::debug!(code = record.code, "policy shadow match"),
            ShadowComparison::Mismatch => tracing::warn!(
                code = record.code,
                legacy = ?record.legacy,
                shadow = ?record.shadow,
                "policy shadow mismatch"
            ),
            ShadowComparison::InternalError => {
                tracing::warn!(code = record.code, "policy shadow observation failed")
            }
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct CollectingObserver {
    records: Arc<Mutex<Vec<ShadowRecord>>>,
}

impl CollectingObserver {
    pub fn records(&self) -> Vec<ShadowRecord> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl ShadowObserver for CollectingObserver {
    fn observe(&self, record: ShadowRecord) {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(record);
    }
}

/// Immutable policy inputs and a one-way observer.
pub struct PolicyShadow<L> {
    snapshot: Arc<RwLock<Arc<ShadowSnapshot>>>,
    learning: Arc<L>,
    observer: Arc<dyn ShadowObserver>,
}

struct ShadowSnapshot {
    policy: Arc<CompiledPolicy>,
    inventory: Arc<OfferInventory>,
}

impl<L> Clone for PolicyShadow<L> {
    fn clone(&self) -> Self {
        Self {
            snapshot: self.snapshot.clone(),
            learning: self.learning.clone(),
            observer: self.observer.clone(),
        }
    }
}

impl<L> PolicyShadow<L>
where
    L: LearningView + Send + Sync + 'static,
{
    pub fn new(
        policy: Arc<CompiledPolicy>,
        inventory: Arc<OfferInventory>,
        learning: Arc<L>,
        observer: Arc<dyn ShadowObserver>,
    ) -> Self {
        Self {
            snapshot: Arc::new(RwLock::new(Arc::new(ShadowSnapshot {
                policy,
                inventory,
            }))),
            learning,
            observer,
        }
    }

    /// Atomically replace the compiled legacy policy and immutable offer
    /// inventory together. In-flight observations keep their captured pair.
    pub fn replace_snapshot(
        &self,
        policy: Arc<CompiledPolicy>,
        inventory: Arc<OfferInventory>,
    ) {
        *self
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Arc::new(ShadowSnapshot { policy, inventory });
    }

    pub fn policy(&self) -> Arc<CompiledPolicy> {
        self.snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .policy
            .clone()
    }

    pub fn observe_record(&self, record: ShadowRecord) {
        let _ = catch_unwind(AssertUnwindSafe(|| self.observer.observe(record)));
    }

    /// Compare one already-authoritative decision. This function never returns
    /// a result to the caller and contains both resolver and observer panics.
    pub fn observe(
        &self,
        facts: RequestFacts,
        lane: Option<LaneFacts>,
        availability: AvailabilitySnapshot,
        legacy: LegacyOutcome,
    ) {
        let record = catch_unwind(AssertUnwindSafe(|| {
            let inventory = self
                .inventory
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            match resolve(
                &self.policy,
                &inventory,
                self.learning.as_ref(),
                lane.as_ref(),
                &availability,
                &facts,
            ) {
                Ok(plan) => {
                    let shadow = ShadowOutcome::from_plan(&facts, &plan);
                    let comparison = if legacy == shadow {
                        ShadowComparison::Match
                    } else {
                        ShadowComparison::Mismatch
                    };
                    ShadowRecord {
                        comparison,
                        code: if comparison == ShadowComparison::Match {
                            "policy-shadow-match"
                        } else {
                            "policy-shadow-mismatch"
                        },
                        legacy: Some(legacy),
                        shadow: Some(shadow),
                    }
                }
                Err(error) => failure_record(error, legacy),
            }
        }))
        .unwrap_or(ShadowRecord {
            comparison: ShadowComparison::InternalError,
            code: "policy-shadow-resolve-panic",
            legacy: None,
            shadow: None,
        });

        let _ = catch_unwind(AssertUnwindSafe(|| self.observer.observe(record)));
    }
}

fn failure_record(error: ResolveFailure, legacy: LegacyOutcome) -> ShadowRecord {
    let code = match error {
        ResolveFailure::NoRoute => "policy-shadow-no-route",
        ResolveFailure::MissingRoute => "policy-shadow-missing-route",
        ResolveFailure::InvalidArgument => "policy-shadow-invalid-argument",
        ResolveFailure::InvalidTemplate => "policy-shadow-invalid-template",
        ResolveFailure::LearningUnavailable => "policy-shadow-learning-unavailable",
        ResolveFailure::RouteUnavailable { .. } => "policy-shadow-route-unavailable",
    };
    ShadowRecord {
        comparison: ShadowComparison::InternalError,
        code,
        legacy: Some(legacy),
        shadow: None,
    }
}

fn provenance_code(provenance: &RouteProvenance) -> String {
    match provenance {
        RouteProvenance::ProfilePresentation(id) => format!("profile-presentation:{id}"),
        RouteProvenance::ProtocolPresentation(id) => format!("protocol-presentation:{id}"),
        RouteProvenance::ProfileRewrite(id) => format!("profile-rewrite:{id}"),
        RouteProvenance::ProtocolRewrite(id) => format!("protocol-rewrite:{id}"),
        RouteProvenance::ProfileDefault => "profile-default".to_owned(),
        RouteProvenance::ProtocolDefault => "protocol-default".to_owned(),
        RouteProvenance::Compaction => "compaction".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Panics;
    impl ShadowObserver for Panics {
        fn observe(&self, _: ShadowRecord) {
            panic!("observer panic")
        }
    }

    #[test]
    fn collecting_observer_keeps_stable_content_free_records() {
        let observer = CollectingObserver::default();
        observer.observe(ShadowRecord {
            comparison: ShadowComparison::InternalError,
            code: "policy-shadow-no-route",
            legacy: None,
            shadow: None,
        });
        assert_eq!(observer.records()[0].code, "policy-shadow-no-route");
    }

    #[test]
    fn observer_contract_can_be_contained_by_catch_unwind() {
        let observer: Arc<dyn ShadowObserver> = Arc::new(Panics);
        assert!(
            catch_unwind(AssertUnwindSafe(|| observer.observe(ShadowRecord {
                comparison: ShadowComparison::Match,
                code: "policy-shadow-match",
                legacy: None,
                shadow: None,
            })))
            .is_err()
        );
    }
}
