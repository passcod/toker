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
    AvailabilitySnapshot, BindingId, CandidateId, CompiledPolicy, FailureClass, PolicyOrigin,
    RouteId,
};

const REVISION_CACHE_CAPACITY: usize = 32;

/// One policy and all mutable evidence captured consistently for a request.
#[derive(Clone)]
pub struct PolicySnapshot {
    revision_id: Option<i64>,
    policy: Arc<CompiledPolicy>,
    inventory: Arc<OfferInventory>,
    availability: Arc<AvailabilitySnapshot>,
    inventory_generation: u64,
    credential_generations: Arc<BTreeMap<BindingId, u64>>,
}

impl PolicySnapshot {
    pub fn revision_id(&self) -> Option<i64> {
        self.revision_id
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

#[derive(Default)]
struct RevisionCache {
    policies: HashMap<i64, Arc<CompiledPolicy>>,
    recency: VecDeque<i64>,
}

impl RevisionCache {
    fn get(&mut self, revision_id: i64) -> Option<Arc<CompiledPolicy>> {
        let policy = self.policies.get(&revision_id)?.clone();
        self.touch(revision_id);
        Some(policy)
    }

    fn insert(&mut self, revision_id: i64, policy: Arc<CompiledPolicy>) {
        self.policies.insert(revision_id, policy);
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
            policy: state.candidate.clone()?,
            inventory: state.inventory.clone(),
            availability: state.availability.clone(),
            inventory_generation: state.inventory_generation,
            credential_generations: state.credential_generations.clone(),
        }))
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
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let policy = if let Some(policy) = self
            .revisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(revision_id)
        {
            policy
        } else {
            let revision = self
                .store
                .load_policy_revision(revision_id)?
                .ok_or(PolicyRuntimeError::MissingRevision(revision_id))?;
            let state = self
                .state
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let origin = if revision.source == PolicyRevisionSource::Legacy {
                PolicyOrigin::GeneratedRevisionOne
            } else {
                PolicyOrigin::Authored
            };
            let policy =
                super::compile(revision.document, &self.topology, &state.inventory, origin)
                    .into_policy()
                    .map_err(|_| PolicyRuntimeError::Compilation(revision_id))?;
            let policy = Arc::new(policy);
            drop(state);
            self.revisions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(revision_id, policy.clone());
            policy
        };
        let state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(Arc::new(PolicySnapshot {
            revision_id: Some(revision_id),
            policy,
            inventory: state.inventory.clone(),
            availability: state.availability.clone(),
            inventory_generation: state.inventory_generation,
            credential_generations: state.credential_generations.clone(),
        }))
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
        let open = circuit.until_ms > now_ms
            && circuit.inventory_generation == snapshot.inventory_generation
            && circuit.credential_generation == snapshot.credential_generation(&key.binding_id);
        if !open {
            circuits.remove(key);
        }
        open
    }

    pub fn clear_circuit(&self, key: &HealthCircuitKey) {
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
        assert!(Arc::ptr_eq(first.policy(), second.policy()));
        assert!(matches!(
            runtime.capture_revision(revision.revision_id + 1),
            Err(PolicyRuntimeError::MissingRevision(_))
        ));
    }

    #[test]
    fn generation_changes_invalidate_only_relevant_circuits() {
        let (_store, runtime) = runtime();
        let binding = BindingId::new("openrouter.openrouter_messages").unwrap();
        let other = BindingId::new("anthropic_sub.anthropic_messages").unwrap();
        let key = HealthCircuitKey {
            policy_revision_id: 1,
            route_id: RouteId::new("route").unwrap(),
            candidate_id: CandidateId::new("candidate").unwrap(),
            binding_id: binding.clone(),
            model_id: "invented-model".to_owned(),
            failure: FailureClass::Overload,
        };
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
}
