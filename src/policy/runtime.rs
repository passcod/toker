//! Atomically captured routing policy state for shadow and eventual live use.
//!
//! The runtime owns immutable snapshots and revision compilation. It does not
//! grant routing authority: handlers still decide whether a captured policy is
//! observational or authoritative.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use crate::catalog::offers::OfferInventory;
use crate::routing::RouteRegistry;
use crate::store::{PolicyRevisionSource, Store};

use super::{
    ActivationScope, AvailabilitySnapshot, BindingId, CandidateId, CompiledPolicy, FailureClass,
    PolicyOrigin, RouteId,
};

const REVISION_CACHE_CAPACITY: usize = 32;

/// One policy and all mutable evidence captured consistently for a request.
#[derive(Clone)]
pub struct PolicySnapshot {
    revision_id: Option<i64>,
    activation_scope: Option<ActivationScope>,
    forced_cache_transition: bool,
    policy: Arc<CompiledPolicy>,
    inventory: Arc<OfferInventory>,
    availability: Arc<AvailabilitySnapshot>,
    inventory_generation: u64,
    credential_generations: Arc<BTreeMap<BindingId, u64>>,
}

#[derive(Clone)]
pub(crate) struct PolicyEvidence {
    inventory: Arc<OfferInventory>,
    availability: Arc<AvailabilitySnapshot>,
    inventory_generation: u64,
    credential_generations: Arc<BTreeMap<BindingId, u64>>,
}

impl PolicySnapshot {
    pub fn revision_id(&self) -> Option<i64> {
        self.revision_id
    }

    pub fn activation_scope(&self) -> Option<ActivationScope> {
        self.activation_scope
    }

    pub fn forced_cache_transition(&self) -> bool {
        self.forced_cache_transition
    }

    pub fn policy(&self) -> &Arc<CompiledPolicy> {
        &self.policy
    }

    pub fn inventory(&self) -> &Arc<OfferInventory> {
        &self.inventory
    }

    pub fn availability(&self) -> &Arc<AvailabilitySnapshot> {
        &self.availability
    }

    pub fn inventory_generation(&self) -> u64 {
        self.inventory_generation
    }

    pub fn credential_generation(&self, binding_id: &BindingId) -> u64 {
        self.credential_generations
            .get(binding_id)
            .copied()
            .unwrap_or_default()
    }
}

struct RuntimeState {
    candidate: Option<Arc<CompiledPolicy>>,
    inventory: Arc<OfferInventory>,
    availability: Arc<AvailabilitySnapshot>,
    inventory_generation: u64,
    credential_generations: Arc<BTreeMap<BindingId, u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HealthCircuitKey {
    pub policy_revision_id: i64,
    pub route_id: RouteId,
    pub candidate_id: CandidateId,
    pub binding_id: BindingId,
    pub model_id: String,
    pub failure: FailureClass,
}

#[derive(Debug, Clone, Copy)]
struct OpenCircuit {
    until_ms: i64,
    inventory_generation: u64,
    credential_generation: u64,
}

#[derive(Clone)]
struct CachedRevision {
    policy: Arc<CompiledPolicy>,
    activation_scope: ActivationScope,
    forced_cache_transition: bool,
}

#[derive(Default)]
struct RevisionCache {
    policies: HashMap<i64, CachedRevision>,
    recency: VecDeque<i64>,
}

impl RevisionCache {
    fn get(&mut self, revision_id: i64) -> Option<CachedRevision> {
        let revision = self.policies.get(&revision_id)?.clone();
        self.touch(revision_id);
        Some(revision)
    }

    fn insert(&mut self, revision_id: i64, revision: CachedRevision) {
        self.policies.insert(revision_id, revision);
        self.touch(revision_id);
        while self.policies.len() > REVISION_CACHE_CAPACITY {
            if let Some(evicted) = self.recency.pop_front() {
                self.policies.remove(&evicted);
            }
        }
    }

    fn touch(&mut self, revision_id: i64) {
        if let Some(index) = self.recency.iter().position(|value| *value == revision_id) {
            self.recency.remove(index);
        }
        self.recency.push_back(revision_id);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyRuntimeError {
    #[error(transparent)]
    Store(#[from] crate::store::Error),
    #[error("policy revision {0} does not exist")]
    MissingRevision(i64),
    #[error("policy revision {0} did not compile")]
    Compilation(i64),
}

/// Shared policy state. The mutation lock serialises complete snapshot swaps,
/// revision cache misses, generation changes, and circuit invalidation.
pub struct PolicyRuntime {
    store: Arc<Store>,
    topology: RouteRegistry,
    state: RwLock<RuntimeState>,
    revisions: Mutex<RevisionCache>,
    circuits: Mutex<HashMap<HealthCircuitKey, OpenCircuit>>,
    mutation: Mutex<()>,
}

impl PolicyRuntime {
    pub fn new(
        store: Arc<Store>,
        topology: RouteRegistry,
        policy: Arc<CompiledPolicy>,
        inventory: Arc<OfferInventory>,
        availability: Arc<AvailabilitySnapshot>,
    ) -> Self {
        let credential_generations = Arc::new(BTreeMap::new());
        Self {
            store,
            topology,
            state: RwLock::new(RuntimeState {
                candidate: Some(policy),
                inventory,
                availability,
                inventory_generation: 0,
                credential_generations,
            }),
            revisions: Mutex::new(RevisionCache::default()),
            circuits: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
        }
    }

    pub fn capture(&self) -> Option<Arc<PolicySnapshot>> {
        let state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Some(Arc::new(PolicySnapshot {
            revision_id: None,
            activation_scope: None,
            forced_cache_transition: false,
            policy: state.candidate.clone()?,
            inventory: state.inventory.clone(),
            availability: state.availability.clone(),
            inventory_generation: state.inventory_generation,
            credential_generations: state.credential_generations.clone(),
        }))
    }

    /// Capture the immutable revision named by a lane pin, or the current
    /// candidate snapshot for an unpinned/absent lane. This selection is
    /// observational until policy routing becomes authoritative.
    pub fn capture_for_lane(
        &self,
        lane: Option<&crate::store::Lane>,
    ) -> Result<Option<Arc<PolicySnapshot>>, PolicyRuntimeError> {
        match lane.and_then(|lane| lane.pin.as_ref()) {
            Some(pin) => self.capture_revision(pin.policy_revision_id).map(Some),
            None => Ok(self.capture()),
        }
    }

    pub fn replace_candidate(
        &self,
        policy: Arc<CompiledPolicy>,
        inventory: Arc<OfferInventory>,
        availability: Arc<AvailabilitySnapshot>,
    ) -> Arc<PolicySnapshot> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.inventory_generation = state.inventory_generation.saturating_add(1);
        state.candidate = Some(policy.clone());
        state.inventory = inventory.clone();
        state.availability = availability.clone();
        let snapshot = Arc::new(PolicySnapshot {
            revision_id: None,
            activation_scope: None,
            forced_cache_transition: false,
            policy,
            inventory,
            availability,
            inventory_generation: state.inventory_generation,
            credential_generations: state.credential_generations.clone(),
        });
        self.circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        snapshot
    }

    pub fn replace_evidence_and_clear_candidate(
        &self,
        inventory: Arc<OfferInventory>,
        availability: Arc<AvailabilitySnapshot>,
    ) {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.inventory_generation = state.inventory_generation.saturating_add(1);
        state.inventory = inventory;
        state.availability = availability;
        state.candidate = None;
        self.circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Capture a stored revision against the current immutable live evidence.
    /// Cache misses are serialized so one revision is compiled at most once.
    pub fn capture_revision(
        &self,
        revision_id: i64,
    ) -> Result<Arc<PolicySnapshot>, PolicyRuntimeError> {
        let evidence = self.capture_evidence();
        self.capture_revision_with_evidence(revision_id, &evidence)
    }

    pub(crate) fn capture_revision_with_evidence(
        &self,
        revision_id: i64,
        evidence: &PolicyEvidence,
    ) -> Result<Arc<PolicySnapshot>, PolicyRuntimeError> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let revision = if let Some(revision) = self
            .revisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(revision_id)
        {
            revision
        } else {
            let stored = self
                .store
                .load_policy_revision(revision_id)?
                .ok_or(PolicyRuntimeError::MissingRevision(revision_id))?;
            let origin = if stored.source == PolicyRevisionSource::Legacy {
                PolicyOrigin::GeneratedRevisionOne
            } else {
                PolicyOrigin::Authored
            };
            let activation_scope = stored.activation_scope;
            let forced_cache_transition = stored.forced_cache_transition;
            let policy =
                super::compile(stored.document, &self.topology, &evidence.inventory, origin)
                    .into_policy()
                    .map_err(|_| PolicyRuntimeError::Compilation(revision_id))?;
            let revision = CachedRevision {
                policy: Arc::new(policy),
                activation_scope,
                forced_cache_transition,
            };
            self.revisions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(revision_id, revision.clone());
            revision
        };
        Ok(Arc::new(PolicySnapshot {
            revision_id: Some(revision_id),
            activation_scope: Some(revision.activation_scope),
            forced_cache_transition: revision.forced_cache_transition,
            policy: revision.policy,
            inventory: evidence.inventory.clone(),
            availability: evidence.availability.clone(),
            inventory_generation: evidence.inventory_generation,
            credential_generations: evidence.credential_generations.clone(),
        }))
    }

    pub(crate) fn capture_evidence(&self) -> PolicyEvidence {
        let state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        PolicyEvidence {
            inventory: state.inventory.clone(),
            availability: state.availability.clone(),
            inventory_generation: state.inventory_generation,
            credential_generations: state.credential_generations.clone(),
        }
    }

    pub(crate) fn capture_shadow(
        &self,
        pinned_revision: Option<i64>,
        now_ms: i64,
    ) -> Option<(Arc<PolicySnapshot>, PolicyEvidence, super::CircuitSnapshot)> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let policy = state.candidate.clone()?;
        let evidence = PolicyEvidence {
            inventory: state.inventory.clone(),
            availability: state.availability.clone(),
            inventory_generation: state.inventory_generation,
            credential_generations: state.credential_generations.clone(),
        };
        let snapshot = Arc::new(PolicySnapshot {
            revision_id: None,
            activation_scope: None,
            forced_cache_transition: false,
            policy,
            inventory: evidence.inventory.clone(),
            availability: evidence.availability.clone(),
            inventory_generation: evidence.inventory_generation,
            credential_generations: evidence.credential_generations.clone(),
        });
        let circuits = self.open_circuits_with_evidence(pinned_revision, &evidence, now_ms);
        Some((snapshot, evidence, circuits))
    }

    pub fn bump_credential_generation(&self, binding_id: &BindingId) -> u64 {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut generations = (*state.credential_generations).clone();
        let generation = generations
            .entry(binding_id.clone())
            .and_modify(|value| *value = value.saturating_add(1))
            .or_insert(1);
        let generation = *generation;
        state.credential_generations = Arc::new(generations);
        self.circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|key, _| &key.binding_id != binding_id);
        generation
    }

    pub fn open_circuit(&self, snapshot: &PolicySnapshot, key: HealthCircuitKey, until_ms: i64) {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                key.clone(),
                OpenCircuit {
                    until_ms,
                    inventory_generation: snapshot.inventory_generation,
                    credential_generation: snapshot.credential_generation(&key.binding_id),
                },
            );
    }

    pub fn circuit_is_open(
        &self,
        snapshot: &PolicySnapshot,
        key: &HealthCircuitKey,
        now_ms: i64,
    ) -> bool {
        let mut circuits = self
            .circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(circuit) = circuits.get(key) else {
            return false;
        };
        if circuit.until_ms <= now_ms {
            circuits.remove(key);
            return false;
        }
        circuit.inventory_generation == snapshot.inventory_generation
            && circuit.credential_generation == snapshot.credential_generation(&key.binding_id)
    }

    pub fn open_circuits(&self, snapshot: &PolicySnapshot, now_ms: i64) -> super::CircuitSnapshot {
        self.open_circuits_for(
            snapshot.revision_id,
            snapshot.inventory_generation,
            &snapshot.credential_generations,
            now_ms,
        )
    }

    pub(crate) fn open_circuits_with_evidence(
        &self,
        revision_id: Option<i64>,
        evidence: &PolicyEvidence,
        now_ms: i64,
    ) -> super::CircuitSnapshot {
        self.open_circuits_for(
            revision_id,
            evidence.inventory_generation,
            &evidence.credential_generations,
            now_ms,
        )
    }

    fn open_circuits_for(
        &self,
        revision_id: Option<i64>,
        inventory_generation: u64,
        credential_generations: &BTreeMap<BindingId, u64>,
        now_ms: i64,
    ) -> super::CircuitSnapshot {
        let Some(revision_id) = revision_id else {
            return super::CircuitSnapshot::new();
        };
        let mut circuits = self
            .circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        circuits.retain(|_, circuit| circuit.until_ms > now_ms);
        circuits
            .iter()
            .filter(|(key, circuit)| {
                key.policy_revision_id == revision_id
                    && circuit.inventory_generation == inventory_generation
                    && circuit.credential_generation
                        == credential_generations
                            .get(&key.binding_id)
                            .copied()
                            .unwrap_or_default()
            })
            .map(|(key, _)| super::OpenCircuitTarget {
                route_id: key.route_id.clone(),
                candidate_id: key.candidate_id.clone(),
                binding_id: key.binding_id.clone(),
                model_id: key.model_id.clone(),
                failure: key.failure,
            })
            .collect()
    }

    pub fn clear_circuit(&self, key: &HealthCircuitKey) {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(key);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::policy::{
        ActivationScope, CURRENT_FORMAT_VERSION, PolicyDocument, ValidatedPolicyDocument,
    };
    use crate::store::{NewPolicyRevision, PolicyRevisionSource};

    fn document(description: &str) -> ValidatedPolicyDocument {
        PolicyDocument {
            format_version: CURRENT_FORMAT_VERSION,
            description: Some(description.to_owned()),
            defaults: vec![],
            presentations: vec![],
            routes: vec![],
            rewrites: vec![],
            classifiers: vec![],
            upgrades: vec![],
            compaction: None,
        }
        .validate()
        .unwrap()
    }

    fn compiled(description: &str) -> Arc<CompiledPolicy> {
        Arc::new(
            super::super::compile(
                document(description),
                &RouteRegistry::new(
                    std::iter::empty::<Arc<dyn crate::providers::Provider>>(),
                    std::iter::empty::<(crate::routing::ProtocolId, String)>(),
                ),
                &OfferInventory::default(),
                PolicyOrigin::Authored,
            )
            .into_policy()
            .unwrap(),
        )
    }

    fn runtime() -> (Arc<Store>, PolicyRuntime) {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let runtime = PolicyRuntime::new(
            store.clone(),
            RouteRegistry::new(
                std::iter::empty::<Arc<dyn crate::providers::Provider>>(),
                std::iter::empty::<(crate::routing::ProtocolId, String)>(),
            ),
            compiled("candidate"),
            Arc::new(OfferInventory::default()),
            Arc::new(AvailabilitySnapshot::default()),
        );
        (store, runtime)
    }

    fn revision(store: &Store, description: &'static str) -> i64 {
        store
            .insert_policy_revision(&NewPolicyRevision {
                created_ms: 1,
                activated_ms: 2,
                description: Some(description),
                activation_scope: ActivationScope::NewLanes,
                forced_cache_transition: false,
                source: PolicyRevisionSource::Draft,
                parent_revision_id: None,
                document: &document(description),
            })
            .unwrap()
            .revision_id
    }

    fn circuit_key(revision_id: i64, binding_id: BindingId) -> HealthCircuitKey {
        HealthCircuitKey {
            policy_revision_id: revision_id,
            route_id: RouteId::new("route").unwrap(),
            candidate_id: CandidateId::new("candidate").unwrap(),
            binding_id,
            model_id: "invented-model".to_owned(),
            failure: FailureClass::Overload,
        }
    }

    #[test]
    fn captured_snapshots_do_not_change_after_inventory_replacement() {
        let (_store, runtime) = runtime();
        let first = runtime.capture().unwrap();
        assert_eq!(first.inventory_generation(), 0);
        let second = runtime.replace_candidate(
            compiled("replacement"),
            Arc::new(OfferInventory::default()),
            Arc::new(AvailabilitySnapshot::default()),
        );
        assert_eq!(second.inventory_generation(), 1);
        assert_eq!(first.inventory_generation(), 0);
        assert!(!Arc::ptr_eq(first.policy(), second.policy()));
    }

    #[test]
    fn failed_candidate_refresh_still_advances_live_evidence() {
        let (_store, runtime) = runtime();
        let first = runtime.capture().unwrap();
        runtime.replace_evidence_and_clear_candidate(
            Arc::new(OfferInventory::default()),
            Arc::new(AvailabilitySnapshot::default()),
        );
        assert!(runtime.capture().is_none());
        assert_eq!(
            runtime
                .state
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .inventory_generation,
            first.inventory_generation() + 1
        );
    }

    #[test]
    fn stored_revisions_compile_lazily_and_reuse_the_cached_policy() {
        let (store, runtime) = runtime();
        let document = document("stored");
        let revision = store
            .insert_policy_revision(&NewPolicyRevision {
                created_ms: 1,
                activated_ms: 2,
                description: Some("stored"),
                activation_scope: ActivationScope::NewLanes,
                forced_cache_transition: false,
                source: PolicyRevisionSource::Draft,
                parent_revision_id: None,
                document: &document,
            })
            .unwrap();
        let first = runtime.capture_revision(revision.revision_id).unwrap();
        let second = runtime.capture_revision(revision.revision_id).unwrap();
        assert_eq!(first.revision_id(), Some(revision.revision_id));
        assert_eq!(first.activation_scope(), Some(ActivationScope::NewLanes));
        assert!(!first.forced_cache_transition());
        assert!(Arc::ptr_eq(first.policy(), second.policy()));
        assert!(matches!(
            runtime.capture_revision(revision.revision_id + 1),
            Err(PolicyRuntimeError::MissingRevision(_))
        ));
    }

    #[test]
    fn pinned_lane_captures_its_stored_revision() {
        let (store, runtime) = runtime();
        let document = document("pinned");
        let revision = store
            .insert_policy_revision(&NewPolicyRevision {
                created_ms: 1,
                activated_ms: 2,
                description: Some("pinned"),
                activation_scope: ActivationScope::NewLanes,
                forced_cache_transition: false,
                source: PolicyRevisionSource::Draft,
                parent_revision_id: None,
                document: &document,
            })
            .unwrap();
        let lane = crate::store::Lane {
            key: "lane".to_owned(),
            session_id: None,
            tools_hash: None,
            updated_ms: 0,
            prompt_tokens: None,
            ttl: None,
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
            pin_generation: 1,
            pin: Some(crate::store::LanePin {
                policy_revision_id: revision.revision_id,
                policy_route_id: RouteId::new("route").unwrap(),
                route_fingerprint: "11".repeat(32).parse().unwrap(),
                candidate_id: CandidateId::new("candidate").unwrap(),
                binding_id: BindingId::new("openrouter.openrouter_messages").unwrap(),
                baseline_model: "base".to_owned(),
                effective_model: "effective".to_owned(),
            }),
        };
        let captured = runtime.capture_for_lane(Some(&lane)).unwrap().unwrap();
        assert_eq!(captured.revision_id(), Some(revision.revision_id));
        let reused = runtime.capture_for_lane(Some(&lane)).unwrap().unwrap();
        assert!(Arc::ptr_eq(captured.policy(), reused.policy()));
        assert_eq!(
            runtime
                .capture_for_lane(None)
                .unwrap()
                .unwrap()
                .revision_id(),
            None
        );
    }

    #[test]
    fn generation_changes_invalidate_only_relevant_circuits() {
        let (_store, runtime) = runtime();
        let binding = BindingId::new("openrouter.openrouter_messages").unwrap();
        let other = BindingId::new("anthropic_sub.anthropic_messages").unwrap();
        let key = circuit_key(1, binding.clone());
        let other_key = HealthCircuitKey {
            binding_id: other,
            ..key.clone()
        };
        let first = runtime.capture().unwrap();
        runtime.open_circuit(&first, key.clone(), 100);
        runtime.open_circuit(&first, other_key.clone(), 100);
        assert!(runtime.circuit_is_open(&first, &key, 10));
        runtime.bump_credential_generation(&binding);
        let second = runtime.capture().unwrap();
        assert!(!runtime.circuit_is_open(&second, &key, 10));
        assert!(runtime.circuit_is_open(&second, &other_key, 10));
        runtime.replace_candidate(
            compiled("new inventory"),
            Arc::new(OfferInventory::default()),
            Arc::new(AvailabilitySnapshot::default()),
        );
        assert!(!runtime.circuit_is_open(&runtime.capture().unwrap(), &other_key, 10));
    }

    #[test]
    fn immutable_circuit_views_filter_revision_expiry_and_exact_keys() {
        let (store, runtime) = runtime();
        let revision_id = revision(&store, "circuits");
        let other_revision_id = revision(&store, "other-circuits");
        let snapshot = runtime.capture_revision(revision_id).unwrap();
        let binding = BindingId::new("openrouter.openrouter_messages").unwrap();
        let key = circuit_key(revision_id, binding.clone());
        let other_revision = circuit_key(other_revision_id, binding.clone());
        let other_failure = HealthCircuitKey {
            failure: FailureClass::ServerError,
            ..key.clone()
        };
        let expired = HealthCircuitKey {
            model_id: "expired".to_owned(),
            ..key.clone()
        };
        runtime.open_circuit(&snapshot, key.clone(), 100);
        runtime.open_circuit(&snapshot, other_revision, 100);
        runtime.open_circuit(&snapshot, other_failure, 100);
        runtime.open_circuit(&snapshot, expired, 10);

        let view = runtime.open_circuits(&snapshot, 10);
        assert_eq!(view.len(), 2);
        assert!(view.contains(&super::super::OpenCircuitTarget {
            route_id: key.route_id,
            candidate_id: key.candidate_id,
            binding_id: binding,
            model_id: key.model_id,
            failure: FailureClass::Overload,
        }));
    }

    #[test]
    fn stale_queries_cannot_remove_newer_generation_circuits() {
        let (store, runtime) = runtime();
        let revision_id = revision(&store, "generation");
        let binding = BindingId::new("openrouter.openrouter_messages").unwrap();
        let stale = runtime.capture_revision(revision_id).unwrap();
        runtime.bump_credential_generation(&binding);
        let current = runtime.capture_revision(revision_id).unwrap();
        let key = circuit_key(revision_id, binding);
        runtime.open_circuit(&current, key, 100);

        assert!(runtime.open_circuits(&stale, 10).is_empty());
        assert_eq!(runtime.open_circuits(&current, 10).len(), 1);
    }
}
