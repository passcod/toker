//! Non-authoritative observation of legacy-policy equivalence.
//!
//! Request handlers only enqueue fully owned, content-free observations into a
//! bounded worker. Queue pressure, resolution failures, and observer panics can
//! therefore never delay or change authoritative traffic.

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use super::{
    AttemptPlan, LaneFacts, LearningView, PolicyRuntime, PolicySnapshot, RequestFacts,
    ResolveFailure, RouteProvenance,
};
const SHADOW_QUEUE_CAPACITY: usize = 64;

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
    pub retargeted_model: Option<String>,
    pub aliased_model: Option<String>,
    pub effective_model: String,
    pub compaction: bool,
}

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
            retargeted_model: plan.retargeted_model.clone(),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowRecord {
    pub comparison: ShadowComparison,
    pub code: &'static str,
    pub legacy: Option<LegacyOutcome>,
    pub shadow: Option<ShadowOutcome>,
}

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

pub struct PolicyShadow<L> {
    policy_runtime: Arc<PolicyRuntime>,
    learning: Arc<L>,
    runtime: Arc<ShadowRuntime>,
}

enum ShadowJob {
    Resolve(Box<ResolveJob>),
    Record(Box<ShadowRecord>),
}

struct ResolveJob {
    snapshot: Arc<PolicySnapshot>,
    pinned_revision: Option<i64>,
    evidence: super::runtime::PolicyEvidence,
    circuits: super::CircuitSnapshot,
    learning: super::LearningSnapshot,
    facts: RequestFacts,
    credentials: super::CredentialSnapshot,
    lane: Option<LaneFacts>,
    legacy: LegacyOutcome,
}

struct ShadowRuntime {
    sender: Mutex<Option<mpsc::SyncSender<ShadowJob>>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    dropped: AtomicU64,
}

impl Drop for ShadowRuntime {
    fn drop(&mut self) {
        self.sender
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}

impl<L> Clone for PolicyShadow<L> {
    fn clone(&self) -> Self {
        Self {
            policy_runtime: self.policy_runtime.clone(),
            learning: self.learning.clone(),
            runtime: self.runtime.clone(),
        }
    }
}

impl<L> PolicyShadow<L>
where
    L: LearningView + Send + Sync + 'static,
{
    pub fn new(
        policy_runtime: Arc<PolicyRuntime>,
        learning: Arc<L>,
        observer: Arc<dyn ShadowObserver>,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(SHADOW_QUEUE_CAPACITY);
        let worker_runtime = policy_runtime.clone();
        let worker = thread::Builder::new()
            .name("toker-policy-shadow".to_owned())
            .spawn(move || worker_loop(receiver, worker_runtime, observer))
            .expect("policy shadow worker thread");
        Self {
            policy_runtime,
            learning,
            runtime: Arc::new(ShadowRuntime {
                sender: Mutex::new(Some(sender)),
                worker: Mutex::new(Some(worker)),
                dropped: AtomicU64::new(0),
            }),
        }
    }

    /// Return the inherited publication from one immutable policy snapshot.
    pub fn publication_slugs(
        &self,
        profile: Option<&super::ProfileId>,
        protocol: super::FrontendProtocol,
    ) -> Option<BTreeSet<String>> {
        let snapshot = self.policy_runtime.capture()?;
        Some(
            snapshot
                .policy()
                .publication_slugs(profile, protocol)
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    }

    pub fn observe_record(&self, record: ShadowRecord) {
        self.enqueue(ShadowJob::Record(Box::new(record)));
    }

    pub fn observe(
        &self,
        facts: RequestFacts,
        credentials: super::CredentialSnapshot,
        lane: Option<LaneFacts>,
        legacy: LegacyOutcome,
    ) {
        let pinned_revision = lane
            .as_ref()
            .and_then(|lane| lane.pinned.as_ref())
            .and_then(|pin| pin.policy_revision_id);
        let Some((snapshot, evidence, circuits)) = self
            .policy_runtime
            .capture_shadow(pinned_revision, facts.now_ms)
        else {
            return;
        };
        let Ok(learning) = self.learning.snapshot() else {
            return;
        };
        self.enqueue(ShadowJob::Resolve(Box::new(ResolveJob {
            snapshot,
            pinned_revision,
            evidence,
            circuits,
            learning,
            facts,
            credentials,
            lane,
            legacy,
        })));
    }

    /// Number of observations dropped because the bounded worker queue was
    /// full or disconnected. Reading this counter never touches the queue.
    pub fn dropped_observations(&self) -> u64 {
        self.runtime.dropped.load(Ordering::Relaxed)
    }

    fn enqueue(&self, job: ShadowJob) {
        let sender = self
            .runtime
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sender) = sender.as_ref()
            && sender.try_send(job).is_err()
        {
            let dropped = self.runtime.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            // Power-of-two reporting stays bounded under sustained pressure.
            // This path does no queue I/O and carries no request content.
            if dropped.is_power_of_two() {
                tracing::warn!(
                    code = "policy-shadow-observation-dropped",
                    dropped,
                    "policy shadow observations dropped"
                );
            }
        }
    }

    #[cfg(test)]
    fn with_runtime(
        policy_runtime: Arc<PolicyRuntime>,
        learning: Arc<L>,
        runtime: Arc<ShadowRuntime>,
    ) -> Self {
        Self {
            policy_runtime,
            learning,
            runtime,
        }
    }
}

fn worker_loop(
    receiver: mpsc::Receiver<ShadowJob>,
    policy_runtime: Arc<PolicyRuntime>,
    observer: Arc<dyn ShadowObserver>,
) {
    while let Ok(job) = receiver.recv() {
        let record = catch_unwind(AssertUnwindSafe(|| match job {
            ShadowJob::Record(record) => *record,
            ShadowJob::Resolve(job) => {
                let ResolveJob {
                    snapshot,
                    pinned_revision,
                    evidence,
                    circuits,
                    learning,
                    facts,
                    credentials,
                    mut lane,
                    legacy,
                } = *job;
                let snapshot = match pinned_revision {
                    Some(revision_id) => match policy_runtime
                        .capture_revision_with_evidence(revision_id, &evidence)
                    {
                        Ok(snapshot) => snapshot,
                        Err(_) => {
                            return ShadowRecord {
                                comparison: ShadowComparison::InternalError,
                                code: "policy-shadow-revision-unavailable",
                                legacy: Some(legacy),
                                shadow: None,
                            };
                        }
                    },
                    None => snapshot,
                };
                if let Some(pin) = lane.as_mut().and_then(|lane| lane.pinned.as_mut()) {
                    pin.policy_fingerprint = Some(snapshot.policy().fingerprint().clone());
                }
                let availability = snapshot
                    .availability()
                    .at(facts.now_ms, crate::catalog::fetched::CACHE_TTL_MS)
                    .with_credentials(&credentials);
                match super::resolve::resolve_with_circuits(
                    snapshot.policy(),
                    snapshot.inventory(),
                    &learning,
                    lane.as_ref(),
                    &availability,
                    &circuits,
                    &facts,
                ) {
                    Ok(mut plan) => {
                        plan.policy_revision = snapshot.revision_id();
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
            }
        }))
        .unwrap_or(ShadowRecord {
            comparison: ShadowComparison::InternalError,
            code: "policy-shadow-resolve-panic",
            legacy: None,
            shadow: None,
        });
        let _ = catch_unwind(AssertUnwindSafe(|| observer.observe(record)));
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
    use std::time::{Duration, Instant};

    use crate::catalog::offers::{BackendOffer, OfferInventory};
    use crate::policy::*;
    use crate::routing::ANTHROPIC_SUB_MESSAGES;

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

    fn fixture() -> (
        Arc<CompiledPolicy>,
        Arc<OfferInventory>,
        Arc<AvailabilitySnapshot>,
    ) {
        let binding = ANTHROPIC_SUB_MESSAGES.clone();
        let document = PolicyDocument {
            format_version: CURRENT_FORMAT_VERSION,
            description: None,
            defaults: vec![DefaultRoute {
                scope: FrontendScope::Protocol {
                    protocol: FrontendProtocol::AnthropicMessages,
                },
                target: RouteReference {
                    route_id: RouteId::new("route").unwrap(),
                    parameters: Default::default(),
                },
            }],
            presentations: vec![],
            routes: vec![Route {
                id: RouteId::new("route").unwrap(),
                display_name: "route".to_owned(),
                description: None,
                parameters: vec![],
                chain: RouteChain {
                    first: Candidate {
                        id: CandidateId::new("candidate").unwrap(),
                        binding_id: binding.id().clone(),
                        model: CandidateModel::Exact {
                            model_id: "model-a".to_owned(),
                        },
                        upgrade_policy_id: None,
                        alias_policy: None,
                        required_capabilities: Default::default(),
                        enabled: true,
                        health_circuits: vec![],
                    },
                    steps: vec![],
                },
                lane_transition: LaneTransitionPolicy::PreservePinned,
            }],
            rewrites: vec![],
            classifiers: vec![],
            upgrades: vec![],
            compaction: None,
        };
        let inventory = Arc::new(OfferInventory::from_offers([BackendOffer {
            binding_id: binding.id().clone(),
            provider: "anthropic_sub".to_owned(),
            backend_model_id: "model-a".to_owned(),
            binding: binding.clone(),
            context_window: None,
            chronology: None,
            fetched_at_ms: 10,
            raw: serde_json::json!({}),
        }]));
        let topology = crate::routing::RouteRegistry::new(
            std::iter::empty::<Arc<dyn crate::providers::Provider>>(),
            std::iter::empty::<(crate::routing::ProtocolId, String)>(),
        );
        let policy = crate::policy::compile(
            document.validate().unwrap(),
            &topology,
            &inventory,
            PolicyOrigin::Authored,
        )
        .into_policy()
        .unwrap();
        let availability = Arc::new(AvailabilitySnapshot::new(std::collections::BTreeMap::from(
            [(
                binding.id().clone(),
                BindingAvailability {
                    provider: "anthropic_sub".to_owned(),
                    configured: Evidence::Positive("configured"),
                    enabled: Evidence::Positive("enabled"),
                    credential: Evidence::Unknown,
                    catalogue: CatalogueAvailability::Fresh { fetched_at_ms: 10 },
                    capabilities: crate::routing::Capabilities::MESSAGES,
                    cost_class: CostClass::Subscription,
                },
            )],
        )));
        (Arc::new(policy), inventory, availability)
    }

    fn facts() -> RequestFacts {
        RequestFacts {
            profile: None,
            protocol: FrontendProtocol::AnthropicMessages,
            requested_model: None,
            lane_reference: None,
            prompt_bound: 1,
            message_count: Some(1),
            tool_count: 0,
            required_capabilities: Default::default(),
            binding_affinity: None,
            class: RequestClass::Inference,
            legacy_compaction: None,
            now_ms: 10,
        }
    }

    fn outcome(model: &str) -> LegacyOutcome {
        LegacyOutcome {
            protocol: FrontendProtocol::AnthropicMessages,
            profile: None,
            requested_model: None,
            provider: "anthropic_sub".to_owned(),
            binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
            route_source: "protocol-default".to_owned(),
            baseline_model: model.to_owned(),
            upgraded_model: None,
            retargeted_model: None,
            aliased_model: None,
            effective_model: model.to_owned(),
            compaction: false,
        }
    }

    fn runtime(
        policy: Arc<CompiledPolicy>,
        inventory: Arc<OfferInventory>,
        availability: Arc<AvailabilitySnapshot>,
    ) -> Arc<PolicyRuntime> {
        Arc::new(PolicyRuntime::new(
            Arc::new(crate::store::Store::open(":memory:").unwrap()),
            crate::routing::RouteRegistry::new(
                std::iter::empty::<Arc<dyn crate::providers::Provider>>(),
                std::iter::empty::<(crate::routing::ProtocolId, String)>(),
            ),
            policy,
            inventory,
            availability,
        ))
    }

    fn wait_for(observer: &CollectingObserver, count: usize) -> Vec<ShadowRecord> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let records = observer.records();
            if records.len() >= count {
                return records;
            }
            assert!(Instant::now() < deadline, "shadow worker did not drain");
            std::thread::yield_now();
        }
    }

    #[test]
    fn bounded_worker_records_match_mismatch_and_survives_observer_panic() {
        let (policy, inventory, availability) = fixture();
        let observer = Arc::new(CollectingObserver::default());
        let policy_runtime = runtime(policy.clone(), inventory.clone(), availability.clone());
        let shadow = PolicyShadow::new(policy_runtime, Arc::new(NoLearning), observer.clone());
        shadow.observe(facts(), Default::default(), None, outcome("model-a"));
        shadow.observe(facts(), Default::default(), None, outcome("model-b"));
        let records = wait_for(&observer, 2);
        assert_eq!(records[0].comparison, ShadowComparison::Match);
        assert_eq!(records[1].comparison, ShadowComparison::Mismatch);

        let panicking = PolicyShadow::new(
            runtime(policy, inventory, availability),
            Arc::new(NoLearning),
            Arc::new(Panics),
        );
        panicking.observe(facts(), Default::default(), None, outcome("model-a"));
        panicking.observe(facts(), Default::default(), None, outcome("model-a"));
        drop(panicking);
    }

    #[test]
    fn queued_resolution_keeps_the_circuit_view_from_enqueue_time() {
        let (policy, inventory, availability) = fixture();
        let policy_runtime = runtime(policy.clone(), inventory.clone(), availability.clone());
        let snapshot = policy_runtime.capture().unwrap();
        let revision_id = 7;
        let key = super::super::HealthCircuitKey {
            policy_revision_id: revision_id,
            route_id: RouteId::new("route").unwrap(),
            candidate_id: CandidateId::new("candidate").unwrap(),
            binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
            model_id: "model-a".to_owned(),
            failure: FailureClass::Overload,
        };
        let lane = LaneFacts {
            temperature: CacheTemperature::Unknown,
            pinned: Some(ProvisionalPin {
                policy_revision_id: Some(revision_id),
                policy_fingerprint: Some(policy.fingerprint().clone()),
                route_fingerprint: policy
                    .route(&RouteId::new("route").unwrap())
                    .unwrap()
                    .fingerprint()
                    .clone(),
                candidate_id: CandidateId::new("candidate").unwrap(),
                binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
                baseline_model: "model-a".to_owned(),
                effective_model: "model-a".to_owned(),
            }),
            forced_from: None,
            forced_to: None,
        };
        let (sender, receiver) = mpsc::sync_channel(2);
        let shadow_runtime = Arc::new(ShadowRuntime {
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(None),
            dropped: AtomicU64::new(0),
        });
        let shadow: PolicyShadow<NoLearning> = PolicyShadow::with_runtime(
            policy_runtime.clone(),
            Arc::new(NoLearning),
            shadow_runtime,
        );

        shadow.observe(
            facts(),
            Default::default(),
            Some(lane.clone()),
            outcome("model-a"),
        );
        policy_runtime.open_circuit(&snapshot, key.clone(), 100);
        let ShadowJob::Resolve(before_open) = receiver.try_recv().unwrap() else {
            panic!()
        };
        assert!(before_open.circuits.is_empty());

        shadow.observe(facts(), Default::default(), Some(lane), outcome("model-a"));
        policy_runtime.clear_circuit(&key);
        let ShadowJob::Resolve(before_clear) = receiver.try_recv().unwrap() else {
            panic!()
        };
        assert_eq!(before_clear.circuits.len(), 1);
    }

    #[test]
    fn full_or_closed_queue_drops_without_blocking() {
        let (policy, inventory, availability) = fixture();
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .try_send(ShadowJob::Record(Box::new(ShadowRecord {
                comparison: ShadowComparison::Match,
                code: "occupied",
                legacy: None,
                shadow: None,
            })))
            .unwrap();
        let shadow_runtime = Arc::new(ShadowRuntime {
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(None),
            dropped: AtomicU64::new(0),
        });
        let shadow: PolicyShadow<NoLearning> = PolicyShadow::with_runtime(
            runtime(policy, inventory, availability),
            Arc::new(NoLearning),
            shadow_runtime,
        );
        shadow.observe_record(ShadowRecord {
            comparison: ShadowComparison::Mismatch,
            code: "dropped",
            legacy: None,
            shadow: None,
        });
        assert_eq!(receiver.try_recv().unwrap().code(), "occupied");
        assert_eq!(shadow.dropped_observations(), 1);
        drop(receiver);
        shadow.observe_record(ShadowRecord {
            comparison: ShadowComparison::Mismatch,
            code: "closed",
            legacy: None,
            shadow: None,
        });
        assert_eq!(shadow.dropped_observations(), 2);
    }

    #[test]
    fn replacing_snapshot_changes_subsequent_resolution() {
        let (policy, inventory, availability) = fixture();
        let observer = Arc::new(CollectingObserver::default());
        let policy_runtime = runtime(policy.clone(), inventory.clone(), availability.clone());
        let shadow = PolicyShadow::new(
            policy_runtime.clone(),
            Arc::new(NoLearning),
            observer.clone(),
        );
        shadow.observe(facts(), Default::default(), None, outcome("model-a"));
        wait_for(&observer, 1);
        policy_runtime.replace_candidate(
            policy,
            Arc::new(OfferInventory::default()),
            availability,
            &BTreeSet::from([ANTHROPIC_SUB_MESSAGES.id().clone()]),
        );
        shadow.observe(facts(), Default::default(), None, outcome("model-a"));
        let records = wait_for(&observer, 2);
        assert_eq!(records[1].comparison, ShadowComparison::InternalError);
    }

    trait JobCode {
        fn code(&self) -> &'static str;
    }

    impl JobCode for ShadowJob {
        fn code(&self) -> &'static str {
            match self {
                ShadowJob::Record(record) => record.code,
                ShadowJob::Resolve(_) => "resolve",
            }
        }
    }
}
