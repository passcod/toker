//! Atomically captured routing policy state.
//!
//! The runtime owns immutable snapshots of the active revision and the evidence
//! it resolves against, and compiles stored revisions for the lanes pinned to
//! them.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use crate::catalog::offers::OfferInventory;
use crate::routing::RouteRegistry;
use crate::store::{PolicyCompilerOrigin, PolicyRevision, Store};

use super::{
    ActivationScope, AvailabilitySnapshot, BindingId, CandidateId, Compilation, CompiledPolicy,
    FailureClass, PolicyOrigin, RouteId,
};

const REVISION_CACHE_CAPACITY: usize = 32;

/// One policy and all mutable evidence captured consistently for a request.
#[derive(Clone)]
pub struct PolicySnapshot {
    revision_id: i64,
    activation_scope: ActivationScope,
    forced_cache_transition: bool,
    policy: Arc<CompiledPolicy>,
    inventory: Arc<OfferInventory>,
    availability: Arc<AvailabilitySnapshot>,
    inventory_generation: u64,
    catalogue_generations: Arc<BTreeMap<BindingId, u64>>,
    credential_generations: Arc<BTreeMap<BindingId, u64>>,
}

#[derive(Clone)]
pub(crate) struct PolicyEvidence {
    inventory: Arc<OfferInventory>,
    availability: Arc<AvailabilitySnapshot>,
    inventory_generation: u64,
    catalogue_generations: Arc<BTreeMap<BindingId, u64>>,
    credential_generations: Arc<BTreeMap<BindingId, u64>>,
}

impl PolicySnapshot {
    /// The live evidence this snapshot was captured with, for resolving a
    /// lane's pinned revision against the same inventory.
    pub(crate) fn evidence(&self) -> PolicyEvidence {
        PolicyEvidence {
            inventory: self.inventory.clone(),
            availability: self.availability.clone(),
            inventory_generation: self.inventory_generation,
            catalogue_generations: self.catalogue_generations.clone(),
            credential_generations: self.credential_generations.clone(),
        }
    }

    pub fn revision_id(&self) -> i64 {
        self.revision_id
    }

    pub fn activation_scope(&self) -> ActivationScope {
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

    pub fn catalogue_generation(&self, binding_id: &BindingId) -> u64 {
        self.catalogue_generations
            .get(binding_id)
            .copied()
            .unwrap_or_default()
    }

    pub fn credential_generation(&self, binding_id: &BindingId) -> u64 {
        self.credential_generations
            .get(binding_id)
            .copied()
            .unwrap_or_default()
    }
}

struct RuntimeState {
    current: CurrentPolicy,
    inventory: Arc<OfferInventory>,
    availability: Arc<AvailabilitySnapshot>,
    inventory_generation: u64,
    catalogue_generations: Arc<BTreeMap<BindingId, u64>>,
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
    catalogue_generation: u64,
    credential_generation: u64,
}

#[derive(Clone)]
struct CurrentPolicy {
    revision_id: i64,
    activation_scope: ActivationScope,
    forced_cache_transition: bool,
    policy: Arc<CompiledPolicy>,
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

fn bump_generations(
    generations: &mut Arc<BTreeMap<BindingId, u64>>,
    changed: &BTreeSet<BindingId>,
) {
    if changed.is_empty() {
        return;
    }
    let mut next = (**generations).clone();
    for binding_id in changed {
        next.entry(binding_id.clone())
            .and_modify(|value| *value = value.saturating_add(1))
            .or_insert(1);
    }
    *generations = Arc::new(next);
}

pub(crate) fn compile_stored_revision(
    revision: &PolicyRevision,
    topology: &RouteRegistry,
    inventory: &OfferInventory,
) -> Compilation {
    match revision.compiler_origin {
        PolicyCompilerOrigin::GeneratedRevisionOne => {
            super::legacy::compile_document(revision.document.clone(), topology, inventory)
        }
        PolicyCompilerOrigin::Authored => super::compile(
            revision.document.clone(),
            topology,
            inventory,
            PolicyOrigin::Authored,
        ),
    }
}

fn clear_binding_circuits(
    circuits: &Mutex<HashMap<HealthCircuitKey, OpenCircuit>>,
    changed: &BTreeSet<BindingId>,
) {
    if changed.is_empty() {
        return;
    }
    circuits
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .retain(|key, _| !changed.contains(&key.binding_id));
}

impl PolicyRuntime {
    /// Start with `revision` active, at inventory generation zero.
    pub fn new(
        store: Arc<Store>,
        topology: RouteRegistry,
        revision: &crate::store::PolicyRevision,
        policy: Arc<CompiledPolicy>,
        inventory: Arc<OfferInventory>,
        availability: Arc<AvailabilitySnapshot>,
    ) -> Self {
        Self {
            store,
            topology,
            state: RwLock::new(RuntimeState {
                current: CurrentPolicy {
                    revision_id: revision.revision_id,
                    activation_scope: revision.activation_scope,
                    forced_cache_transition: revision.forced_cache_transition,
                    policy,
                },
                inventory,
                availability,
                inventory_generation: 0,
                catalogue_generations: Arc::new(BTreeMap::new()),
                credential_generations: Arc::new(BTreeMap::new()),
            }),
            revisions: Mutex::new(RevisionCache::default()),
            circuits: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
        }
    }

    pub fn capture(&self) -> Arc<PolicySnapshot> {
        let state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::new(PolicySnapshot {
            revision_id: state.current.revision_id,
            activation_scope: state.current.activation_scope,
            forced_cache_transition: state.current.forced_cache_transition,
            policy: state.current.policy.clone(),
            inventory: state.inventory.clone(),
            availability: state.availability.clone(),
            inventory_generation: state.inventory_generation,
            catalogue_generations: state.catalogue_generations.clone(),
            credential_generations: state.credential_generations.clone(),
        })
    }

    /// Run an activation-derived write only while its revision remains active.
    pub(crate) fn with_activation_fence<T>(
        &self,
        revision_id: i64,
        action: impl FnOnce() -> T,
    ) -> Option<T> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let allowed = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .current
            .revision_id
            == revision_id;
        allowed.then(action)
    }

    pub fn replace_active(
        &self,
        revision: &crate::store::PolicyRevision,
        policy: Arc<CompiledPolicy>,
        inventory: Arc<OfferInventory>,
        availability: Arc<AvailabilitySnapshot>,
        changed_catalogue_bindings: &BTreeSet<BindingId>,
    ) -> Arc<PolicySnapshot> {
        self.replace_policy(
            CurrentPolicy {
                revision_id: revision.revision_id,
                activation_scope: revision.activation_scope,
                forced_cache_transition: revision.forced_cache_transition,
                policy,
            },
            inventory,
            availability,
            changed_catalogue_bindings,
        )
    }

    fn replace_policy(
        &self,
        current: CurrentPolicy,
        inventory: Arc<OfferInventory>,
        availability: Arc<AvailabilitySnapshot>,
        changed_catalogue_bindings: &BTreeSet<BindingId>,
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
        bump_generations(&mut state.catalogue_generations, changed_catalogue_bindings);
        state.current = current.clone();
        state.inventory = inventory.clone();
        state.availability = availability.clone();
        let snapshot = Arc::new(PolicySnapshot {
            revision_id: current.revision_id,
            activation_scope: current.activation_scope,
            forced_cache_transition: current.forced_cache_transition,
            policy: current.policy,
            inventory,
            availability,
            inventory_generation: state.inventory_generation,
            catalogue_generations: state.catalogue_generations.clone(),
            credential_generations: state.credential_generations.clone(),
        });
        clear_binding_circuits(&self.circuits, changed_catalogue_bindings);
        snapshot
    }

    /// Capture a stored revision against the current immutable live evidence.
    /// Cache misses are serialized so one revision is compiled at most once.
    pub fn capture_revision(
        &self,
        revision_id: i64,
    ) -> Result<Arc<PolicySnapshot>, PolicyRuntimeError> {
        let evidence = self.capture().evidence();
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
            let activation_scope = stored.activation_scope;
            let forced_cache_transition = stored.forced_cache_transition;
            let compilation = compile_stored_revision(&stored, &self.topology, &evidence.inventory);
            let policy = compilation
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
            revision_id,
            activation_scope: revision.activation_scope,
            forced_cache_transition: revision.forced_cache_transition,
            policy: revision.policy,
            inventory: evidence.inventory.clone(),
            availability: evidence.availability.clone(),
            inventory_generation: evidence.inventory_generation,
            catalogue_generations: evidence.catalogue_generations.clone(),
            credential_generations: evidence.credential_generations.clone(),
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
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let binding = &key.binding_id;
        let catalogue_generation = snapshot.catalogue_generation(binding);
        let credential_generation = snapshot.credential_generation(binding);
        let state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .catalogue_generations
            .get(binding)
            .copied()
            .unwrap_or_default()
            != catalogue_generation
            || state
                .credential_generations
                .get(binding)
                .copied()
                .unwrap_or_default()
                != credential_generation
        {
            return;
        }
        drop(state);
        self.circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(key)
            .and_modify(|circuit| {
                if circuit.catalogue_generation == catalogue_generation
                    && circuit.credential_generation == credential_generation
                {
                    circuit.until_ms = circuit.until_ms.max(until_ms);
                }
            })
            .or_insert(OpenCircuit {
                until_ms,
                catalogue_generation,
                credential_generation,
            });
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
        circuit.catalogue_generation == snapshot.catalogue_generation(&key.binding_id)
            && circuit.credential_generation == snapshot.credential_generation(&key.binding_id)
    }

    pub fn open_circuits(&self, snapshot: &PolicySnapshot, now_ms: i64) -> super::CircuitSnapshot {
        let mut circuits = self
            .circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        circuits.retain(|_, circuit| circuit.until_ms > now_ms);
        circuits
            .iter()
            .filter(|(key, circuit)| {
                key.policy_revision_id == snapshot.revision_id
                    && circuit.catalogue_generation
                        == snapshot.catalogue_generation(&key.binding_id)
                    && circuit.credential_generation
                        == snapshot.credential_generation(&key.binding_id)
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

    pub fn clear_circuit(&self, snapshot: &PolicySnapshot, key: &HealthCircuitKey) {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let catalogue_generation = snapshot.catalogue_generation(&key.binding_id);
        let credential_generation = snapshot.credential_generation(&key.binding_id);
        self.circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|candidate, circuit| {
                candidate != key
                    || circuit.catalogue_generation != catalogue_generation
                    || circuit.credential_generation != credential_generation
            });
    }

    pub fn clear_target_circuits(
        &self,
        snapshot: &PolicySnapshot,
        route_id: &RouteId,
        candidate_id: &CandidateId,
        binding_id: &BindingId,
        model_id: &str,
    ) {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let catalogue_generation = snapshot.catalogue_generation(binding_id);
        let credential_generation = snapshot.credential_generation(binding_id);
        self.circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|key, circuit| {
                key.policy_revision_id != snapshot.revision_id
                    || key.route_id != *route_id
                    || key.candidate_id != *candidate_id
                    || key.binding_id != *binding_id
                    || key.model_id != model_id
                    || circuit.catalogue_generation != catalogue_generation
                    || circuit.credential_generation != credential_generation
            });
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
                &RouteRegistry::new(std::iter::empty::<Arc<dyn crate::providers::Provider>>()),
                &OfferInventory::default(),
                PolicyOrigin::Authored,
            )
            .into_policy()
            .unwrap(),
        )
    }

    fn runtime() -> (Arc<Store>, PolicyRuntime) {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let revision_id = revision(&store, "initial");
        let revision = store.load_policy_revision(revision_id).unwrap().unwrap();
        let runtime = PolicyRuntime::new(
            store.clone(),
            RouteRegistry::new(std::iter::empty::<Arc<dyn crate::providers::Provider>>()),
            &revision,
            compiled("initial"),
            Arc::new(OfferInventory::default()),
            Arc::new(AvailabilitySnapshot::default()),
        );
        (store, runtime)
    }

    /// Replace the active policy with a fresh compilation of the same
    /// revision, as a catalogue refresh does.
    fn refresh(
        store: &Store,
        runtime: &PolicyRuntime,
        description: &str,
        changed: &BTreeSet<BindingId>,
    ) -> Arc<PolicySnapshot> {
        let revision = store
            .load_policy_revision(runtime.capture().revision_id())
            .unwrap()
            .unwrap();
        runtime.replace_active(
            &revision,
            compiled(description),
            Arc::new(OfferInventory::default()),
            Arc::new(AvailabilitySnapshot::default()),
            changed,
        )
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
                compiler_origin: None,
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
    fn active_snapshot_metadata_survives_inventory_replacement() {
        let (store, _) = runtime();
        let revision_id = revision(&store, "active");
        let revision = store
            .load_policy_revision(revision_id)
            .unwrap()
            .expect("revision");
        let topology =
            RouteRegistry::new(std::iter::empty::<Arc<dyn crate::providers::Provider>>());
        let runtime = PolicyRuntime::new(
            store,
            topology,
            &revision,
            compiled("active"),
            Arc::new(OfferInventory::default()),
            Arc::new(AvailabilitySnapshot::default()),
        );
        let before = runtime.capture();
        let after = runtime.replace_active(
            &revision,
            compiled("active refresh"),
            Arc::new(OfferInventory::default()),
            Arc::new(AvailabilitySnapshot::default()),
            &BTreeSet::new(),
        );
        assert_eq!(before.revision_id(), revision_id);
        assert_eq!(after.revision_id(), revision_id);
        assert_eq!(after.activation_scope(), ActivationScope::NewLanes);
        assert_eq!(after.inventory_generation(), 1);
    }

    #[test]
    fn activation_revision_fence_moves_with_runtime_replacement() {
        let (store, _) = runtime();
        let first_id = revision(&store, "first");
        let second_id = revision(&store, "second");
        let first = store.load_policy_revision(first_id).unwrap().unwrap();
        let second = store.load_policy_revision(second_id).unwrap().unwrap();
        let runtime = PolicyRuntime::new(
            store,
            RouteRegistry::new(std::iter::empty::<Arc<dyn crate::providers::Provider>>()),
            &first,
            compiled("first"),
            Arc::new(OfferInventory::default()),
            Arc::new(AvailabilitySnapshot::default()),
        );
        assert_eq!(runtime.with_activation_fence(first_id, || 1), Some(1));
        assert_eq!(runtime.with_activation_fence(second_id, || 1), None);

        runtime.replace_active(
            &second,
            compiled("second"),
            Arc::new(OfferInventory::default()),
            Arc::new(AvailabilitySnapshot::default()),
            &BTreeSet::new(),
        );
        assert_eq!(runtime.with_activation_fence(first_id, || 1), None);
        assert_eq!(runtime.with_activation_fence(second_id, || 1), Some(1));
    }

    #[test]
    fn captured_snapshots_do_not_change_after_inventory_replacement() {
        let (store, runtime) = runtime();
        let first = runtime.capture();
        assert_eq!(first.inventory_generation(), 0);
        let second = refresh(&store, &runtime, "replacement", &BTreeSet::new());
        assert_eq!(second.inventory_generation(), 1);
        assert_eq!(first.inventory_generation(), 0);
        assert!(!Arc::ptr_eq(first.policy(), second.policy()));
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
                compiler_origin: None,
                parent_revision_id: None,
                document: &document,
            })
            .unwrap();
        let first = runtime.capture_revision(revision.revision_id).unwrap();
        let second = runtime.capture_revision(revision.revision_id).unwrap();
        assert_eq!(first.revision_id(), revision.revision_id);
        assert_eq!(first.activation_scope(), ActivationScope::NewLanes);
        assert!(!first.forced_cache_transition());
        assert!(Arc::ptr_eq(first.policy(), second.policy()));
        assert!(matches!(
            runtime.capture_revision(revision.revision_id + 1),
            Err(PolicyRuntimeError::MissingRevision(_))
        ));
    }

    #[test]
    fn generation_changes_invalidate_only_relevant_circuits() {
        let (store, runtime) = runtime();
        let binding = BindingId::new("openrouter.openrouter_messages").unwrap();
        let other = BindingId::new("anthropic_sub.anthropic_messages").unwrap();
        let key = circuit_key(runtime.capture().revision_id(), binding.clone());
        let other_key = HealthCircuitKey {
            binding_id: other.clone(),
            ..key.clone()
        };
        let first = runtime.capture();
        runtime.open_circuit(&first, key.clone(), 100);
        runtime.open_circuit(&first, other_key.clone(), 100);
        let second = refresh(
            &store,
            &runtime,
            "new inventory",
            &BTreeSet::from([binding.clone()]),
        );
        assert_eq!(first.catalogue_generation(&binding), 0);
        assert_eq!(second.catalogue_generation(&binding), 1);
        assert_eq!(second.catalogue_generation(&other), 0);
        assert!(!runtime.circuit_is_open(&second, &key, 10));
        assert!(runtime.circuit_is_open(&second, &other_key, 10));

        runtime.bump_credential_generation(&other);
        let third = runtime.capture();
        assert!(!runtime.circuit_is_open(&third, &other_key, 10));
    }

    #[test]
    fn catalogue_refresh_invalidates_only_changed_catalogue_bindings() {
        let (store, runtime) = runtime();
        let revision_id = revision(&store, "failed-refresh");
        let changed = BindingId::new("openrouter.openrouter_messages").unwrap();
        let unchanged = BindingId::new("anthropic_sub.anthropic_messages").unwrap();
        let changed_key = circuit_key(revision_id, changed.clone());
        let unchanged_key = HealthCircuitKey {
            binding_id: unchanged.clone(),
            ..changed_key.clone()
        };
        let first = runtime.capture_revision(revision_id).unwrap();
        runtime.open_circuit(&first, changed_key.clone(), 100);
        runtime.open_circuit(&first, unchanged_key.clone(), 100);
        refresh(
            &store,
            &runtime,
            "refresh",
            &BTreeSet::from([changed.clone()]),
        );
        let second = runtime.capture_revision(revision_id).unwrap();

        assert_eq!(second.catalogue_generation(&changed), 1);
        assert_eq!(second.catalogue_generation(&unchanged), 0);
        assert!(!runtime.circuit_is_open(&second, &changed_key, 10));
        assert!(runtime.circuit_is_open(&second, &unchanged_key, 10));
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
    fn open_circuits_follow_the_snapshots_own_revision() {
        let (store, _) = runtime();
        let revision_id = revision(&store, "active-circuit");
        let revision = store
            .load_policy_revision(revision_id)
            .unwrap()
            .expect("revision");
        let topology =
            RouteRegistry::new(std::iter::empty::<Arc<dyn crate::providers::Provider>>());
        let runtime = PolicyRuntime::new(
            store,
            topology,
            &revision,
            compiled("active-circuit"),
            Arc::new(OfferInventory::default()),
            Arc::new(AvailabilitySnapshot::default()),
        );
        let snapshot = runtime.capture();
        let binding = BindingId::new("openrouter.openrouter_messages").unwrap();
        let key = circuit_key(revision_id, binding.clone());
        runtime.open_circuit(&snapshot, key.clone(), 100);

        let circuits = runtime.open_circuits(&runtime.capture(), 10);
        assert!(circuits.contains(&super::super::OpenCircuitTarget {
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
        let before_credentials = runtime.capture_revision(revision_id).unwrap();
        runtime.bump_credential_generation(&binding);
        let current_credentials = runtime.capture_revision(revision_id).unwrap();
        let credential_key = circuit_key(revision_id, binding.clone());
        runtime.open_circuit(&current_credentials, credential_key.clone(), 100);
        runtime.open_circuit(&before_credentials, credential_key.clone(), 200);
        runtime.clear_circuit(&before_credentials, &credential_key);
        assert!(runtime.open_circuits(&before_credentials, 10).is_empty());
        assert_eq!(runtime.open_circuits(&current_credentials, 10).len(), 1);

        let before_catalogue = current_credentials;
        refresh(
            &store,
            &runtime,
            "catalogue-generation",
            &BTreeSet::from([binding.clone()]),
        );
        let current_catalogue = runtime.capture_revision(revision_id).unwrap();
        let catalogue_key = HealthCircuitKey {
            model_id: "new-generation".to_owned(),
            ..circuit_key(revision_id, binding)
        };
        runtime.open_circuit(&current_catalogue, catalogue_key, 100);
        assert!(runtime.open_circuits(&before_catalogue, 10).is_empty());
        assert_eq!(runtime.open_circuits(&current_catalogue, 10).len(), 1);
    }

    #[test]
    fn same_generation_failures_extend_and_success_clears_exact_target() {
        let (store, runtime) = runtime();
        let revision_id = revision(&store, "circuit-mutations");
        let snapshot = runtime.capture_revision(revision_id).unwrap();
        let binding = BindingId::new("openrouter.openrouter_messages").unwrap();
        let key = circuit_key(revision_id, binding.clone());
        let other = HealthCircuitKey {
            model_id: "other-model".to_owned(),
            ..key.clone()
        };
        runtime.open_circuit(&snapshot, key.clone(), 100);
        runtime.open_circuit(&snapshot, key.clone(), 50);
        runtime.open_circuit(&snapshot, other.clone(), 100);
        assert!(runtime.circuit_is_open(&snapshot, &key, 75));
        runtime.clear_target_circuits(
            &snapshot,
            &key.route_id,
            &key.candidate_id,
            &key.binding_id,
            &key.model_id,
        );
        assert!(!runtime.circuit_is_open(&snapshot, &key, 75));
        assert!(runtime.circuit_is_open(&snapshot, &other, 75));
    }
}
