//! The routing control operations behind `/_toker/routing/*`: save, widen,
//! validate, preview and reactivate (spec: RREV).
//!
//! Each runs under the server's routing control lock, against one captured
//! catalogue inventory, and names the active revision it was made against.
//! There is no staged state between a save and live traffic: a save applies
//! its edit to the active document and activates the result in one step.

use std::sync::Arc;

use crate::policy::{
    ActivationScope, AttemptPlan, BindingId, CapabilityFlags, FrontendProtocol, PolicyEdit,
    PolicyEditError, ProfileId, RecordedFacts, RequestClass, RequestFacts, ResolveFailure,
};
use crate::server::record::now_ms;
use crate::store::{
    ActivatePolicyRevision, PolicyRevision, PolicyRevisionSource, ROUTING_WINDOW_MS,
};

use super::{
    RoutingActivation, RoutingControlError, Server, compile_control, compile_control_policy,
    compile_stored_policy, preview_result_body, require_cache_consent,
};

/// The most recent requests a replay walks. A day of heavy use holds far
/// fewer routed requests than this; hitting it is reported, not hidden.
const REPLAY_ROW_CAP: u32 = 10_000;

/// The most changed requests a replay returns in full. The count of all of
/// them is returned beside.
const REPLAY_CHANGED_CAP: usize = 200;

/// What a preview resolves against.
#[derive(Debug, Clone)]
pub(crate) enum PreviewAgainst {
    /// The live revision.
    Active,
    /// The live revision's document with an edit applied, not saved.
    Edit(PolicyEdit),
    /// A revision kept in the history.
    Revision(i64),
}

/// The request a preview resolves.
#[derive(Debug, Clone)]
pub(crate) enum PreviewSubject {
    Invented(InventedRequest),
    /// A ledger row by id, replayed from the facts it recorded.
    Recorded(i64),
}

#[derive(Debug, Clone)]
pub(crate) struct InventedRequest {
    pub(crate) protocol: FrontendProtocol,
    pub(crate) profile: Option<ProfileId>,
    pub(crate) model: Option<String>,
    pub(crate) lane_key: Option<String>,
    pub(crate) prompt_bound: u64,
    pub(crate) message_count: Option<u64>,
    pub(crate) tool_count: u64,
    pub(crate) required_capabilities: CapabilityFlags,
    /// The request asks for server safeguards.
    pub(crate) asks_server_safeguards: bool,
    pub(crate) binding_affinity: Option<BindingId>,
}

#[derive(Debug)]
pub(crate) struct RoutingPreviewRequest {
    pub(crate) expected_revision_id: Option<i64>,
    pub(crate) against: PreviewAgainst,
    pub(crate) subject: PreviewSubject,
}

#[derive(Debug)]
pub(crate) struct RoutingPreview {
    pub(crate) active_revision_id: i64,
    /// The revision the request was resolved under, where one was named.
    pub(crate) against_revision_id: Option<i64>,
    /// What compiling the previewed document reported besides refusals.
    pub(crate) diagnostics: Vec<crate::policy::Diagnostic>,
    pub(crate) outcome: PreviewOutcome,
}

#[derive(Debug)]
pub(crate) enum PreviewOutcome {
    Resolved {
        /// The facts that were resolved, as the control body renders them.
        facts: RecordedFacts,
        result: serde_json::Value,
    },
    /// The row did not record the facts the resolver reads, so nothing was
    /// resolved in their place.
    NotReplayable { reason: NotReplayable },
}

pub(crate) use crate::policy::facts::NotReplayable;

#[derive(Debug)]
pub(crate) struct RoutingValidation {
    pub(crate) active_revision_id: i64,
    pub(crate) valid: bool,
    pub(crate) diagnostics: Vec<crate::policy::Diagnostic>,
    /// Set where the edit could not be applied to the document at all, so
    /// there was nothing to compile.
    pub(crate) refusal: Option<PolicyEditError>,
}

/// How a replayed request routed under one document: where it went, and which
/// record caught it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayOutcome {
    pub(crate) outcome: &'static str,
    pub(crate) route_id: Option<String>,
    pub(crate) candidate_id: Option<String>,
    pub(crate) binding_id: Option<String>,
    pub(crate) effective_model: Option<String>,
    pub(crate) failure: Option<&'static str>,
    pub(crate) matched: serde_json::Value,
}

impl ReplayOutcome {
    /// Where the request went. A different rule reaching the same place is
    /// not a change of outcome.
    fn destination(&self) -> impl PartialEq + '_ {
        (
            self.outcome,
            &self.route_id,
            &self.candidate_id,
            &self.binding_id,
            &self.effective_model,
            self.failure,
        )
    }

    fn of(result: Result<AttemptPlan, ResolveFailure>) -> Self {
        match result {
            Ok(plan) => Self {
                outcome: "selected",
                route_id: plan.route_id.map(|id| id.to_string()),
                candidate_id: plan.candidate_id.map(|id| id.to_string()),
                binding_id: Some(plan.binding_id.to_string()),
                effective_model: Some(plan.effective_model),
                failure: None,
                matched: provenance_body(&plan.provenance),
            },
            Err(ResolveFailure::RouteUnavailable {
                route_id,
                provenance,
                ..
            }) => Self {
                outcome: "unavailable",
                route_id: Some(route_id.to_string()),
                candidate_id: None,
                binding_id: None,
                effective_model: None,
                failure: None,
                matched: provenance_body(&provenance),
            },
            Err(error) => Self {
                outcome: "unresolved",
                route_id: None,
                candidate_id: None,
                binding_id: None,
                effective_model: None,
                failure: Some(super::preview_failure_code(&error)),
                matched: serde_json::Value::Null,
            },
        }
    }
}

/// One replayed request whose outcome changes under the edit.
#[derive(Debug)]
pub(crate) struct ReplayChange {
    pub(crate) request_id: i64,
    pub(crate) ts_ms: i64,
    pub(crate) facts: RecordedFacts,
    pub(crate) under_active: ReplayOutcome,
    pub(crate) under_edit: ReplayOutcome,
}

#[derive(Debug)]
pub(crate) struct RoutingReplay {
    pub(crate) active_revision_id: i64,
    pub(crate) since_ms: i64,
    pub(crate) until_ms: i64,
    /// Requests resolved under both.
    pub(crate) replayed: u64,
    /// Requests in the window whose rows do not record what the resolver
    /// reads, so they were not resolved.
    pub(crate) unreplayable: u64,
    /// The walk stopped at its row cap with older requests still in the
    /// window.
    pub(crate) truncated: bool,
    pub(crate) changed_total: u64,
    pub(crate) changed: Vec<ReplayChange>,
    pub(crate) diagnostics: Vec<crate::policy::Diagnostic>,
}

pub(crate) fn provenance_body(provenance: &crate::policy::RouteProvenance) -> serde_json::Value {
    use crate::policy::RouteProvenance as P;
    let (kind, scope, id) = match provenance {
        P::ProfilePresentation(id) => ("presentation", Some("profile"), Some(id.to_string())),
        P::ProtocolPresentation(id) => ("presentation", Some("protocol"), Some(id.to_string())),
        P::ProfileRewrite(id) => ("rewrite", Some("profile"), Some(id.to_string())),
        P::ProtocolRewrite(id) => ("rewrite", Some("protocol"), Some(id.to_string())),
        P::ProfileDefault => ("default", Some("profile"), None),
        P::ProtocolDefault => ("default", Some("protocol"), None),
        P::Compaction => ("compaction", None, None),
    };
    serde_json::json!({ "kind": kind, "scope": scope, "id": id })
}

/// The control basis one operation works from: the active revision, its
/// state and one catalogue inventory with its availability.
pub(super) struct ControlBasis {
    pub(super) active_revision: PolicyRevision,
    pub(super) state: crate::store::RoutingPolicyState,
    pub(super) inventory: crate::catalog::offers::OfferInventory,
    pub(super) availability: crate::policy::AvailabilitySnapshot,
    pub(super) inventory_generation: i64,
}

impl Server {
    pub(super) fn control_basis_locked(&self) -> anyhow::Result<ControlBasis> {
        let revision_id = self.policy_runtime.capture().revision_id();
        let active_revision = self
            .store
            .load_policy_revision(revision_id)?
            .ok_or_else(|| anyhow::anyhow!("active policy revision {revision_id} is missing"))?;
        let catalogs = self
            .catalogs
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Ok(ControlBasis {
            active_revision,
            state: self.store.load_routing_policy_state()?,
            inventory: crate::catalog::offers::inventory(&self.registry, &catalogs),
            availability: super::availability_for(&self.registry, &catalogs),
            inventory_generation: crate::catalog::fetched::snapshot_generation(&catalogs),
        })
    }

    /// Refuse an operation made against a revision that is no longer live,
    /// naming the one that is.
    fn expect_active_revision(
        basis: &ControlBasis,
        expected: Option<i64>,
    ) -> Result<(), RoutingControlError> {
        match expected {
            Some(expected) if expected != basis.active_revision.revision_id => {
                Err(RoutingControlError::StaleRevision {
                    live_revision_id: basis.active_revision.revision_id,
                })
            }
            _ => Ok(()),
        }
    }

    /// Apply `edit` to the active revision's document, compile the result and
    /// activate it for new and cold lanes, as one operation
    /// (spec: RREV#saving-an-edit). An error changes nothing.
    pub(crate) fn save_routing_edit(
        &self,
        expected_revision_id: i64,
        edit: &PolicyEdit,
        description: Option<&str>,
    ) -> Result<RoutingActivation, RoutingControlError> {
        let _control = self
            .routing_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let basis = self.control_basis_locked()?;
        Self::expect_active_revision(&basis, Some(expected_revision_id))?;
        let document = edit.apply_validated(&basis.active_revision.document)?;
        self.activate_document_locked(
            &basis,
            &document,
            description,
            PolicyRevisionSource::Save,
            None,
        )
    }

    /// Copy a kept revision into a new active one, for new and cold lanes
    /// (spec: RREV#history-and-reactivation).
    pub(crate) fn reactivate_routing_policy(
        &self,
        expected_revision_id: i64,
        selected_revision_id: i64,
        description: Option<&str>,
    ) -> Result<RoutingActivation, RoutingControlError> {
        let _control = self
            .routing_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let basis = self.control_basis_locked()?;
        Self::expect_active_revision(&basis, Some(expected_revision_id))?;
        let selected = self
            .store
            .load_policy_revision(selected_revision_id)?
            .ok_or(RoutingControlError::MissingRevision(selected_revision_id))?;
        self.activate_document_locked(
            &basis,
            &selected.document,
            description,
            PolicyRevisionSource::Reactivation,
            Some(selected_revision_id),
        )
    }

    fn activate_document_locked(
        &self,
        basis: &ControlBasis,
        document: &crate::policy::ValidatedPolicyDocument,
        description: Option<&str>,
        source: PolicyRevisionSource,
        parent_revision_id: Option<i64>,
    ) -> Result<RoutingActivation, RoutingControlError> {
        // Neither operation moves a lane it could lose a warm cache on.
        let scope = ActivationScope::NewAndColdLanes;
        let impacts = self.routing_lane_impacts_locked(now_ms())?;
        require_cache_consent(&impacts, scope, false)?;
        let policy = compile_control_policy(document, &self.registry, &basis.inventory)?;
        let activation = self
            .store
            .activate_policy_revision(&ActivatePolicyRevision {
                created_ms: now_ms(),
                description,
                activation_scope: scope,
                forced_cache_transition: false,
                source,
                parent_revision_id,
                expected_active_revision_id: basis.active_revision.revision_id,
                document,
                desired_inventory_generation: basis.inventory_generation,
                publications: &crate::store::publication_targets()?,
            })?;
        self.policy_runtime.replace_active(
            &activation.revision,
            Arc::new(policy),
            Arc::new(basis.inventory.clone()),
            Arc::new(basis.availability.clone()),
            &std::collections::BTreeSet::new(),
        );
        Ok(RoutingActivation {
            activation,
            lane_impacts: impacts,
        })
    }

    /// Widen the active revision's activation scope, so lanes still pinned to
    /// an earlier revision move onto it, without creating a revision
    /// (spec: RREV#widening-an-activation).
    pub(crate) fn widen_routing_activation(
        &self,
        expected_revision_id: i64,
        scope: ActivationScope,
        forced_cache_transition: bool,
    ) -> Result<RoutingWidening, RoutingControlError> {
        let _control = self
            .routing_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let basis = self.control_basis_locked()?;
        Self::expect_active_revision(&basis, Some(expected_revision_id))?;
        // Whether this is a widening is the store's answer, decided inside
        // the transaction that applies it; a check out here would only lose
        // the race it appears to guard.
        let impacts = self.routing_lane_impacts_locked(now_ms())?;
        require_cache_consent(&impacts, scope, forced_cache_transition)?;
        match self.store.widen_activation(
            expected_revision_id,
            scope,
            forced_cache_transition,
            now_ms(),
        )? {
            crate::store::WidenResult::Widened { state } => {
                self.policy_runtime.widen_active(
                    expected_revision_id,
                    scope,
                    forced_cache_transition,
                );
                Ok(RoutingWidening {
                    revision_id: expected_revision_id,
                    scope,
                    forced_cache_transition,
                    widened_ms: state.widened.map(|widening| widening.widened_ms),
                    lane_impacts: impacts,
                })
            }
            crate::store::WidenResult::Conflict { current } => {
                Err(RoutingControlError::StaleRevision {
                    live_revision_id: current.active_revision_id.unwrap_or_default(),
                })
            }
            crate::store::WidenResult::NotAWidening {
                current_scope,
                current_forced_cache_transition,
            } => Err(RoutingControlError::NotAWidening {
                scope: current_scope,
                forced_cache_transition: current_forced_cache_transition,
            }),
        }
    }

    /// Compile an edit against the active revision and report what it says,
    /// changing nothing (spec: RREV#saving-an-edit).
    pub(crate) fn validate_routing_edit(
        &self,
        expected_revision_id: i64,
        edit: &PolicyEdit,
    ) -> Result<RoutingValidation, RoutingControlError> {
        let _control = self
            .routing_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let basis = self.control_basis_locked()?;
        Self::expect_active_revision(&basis, Some(expected_revision_id))?;
        let active_revision_id = basis.active_revision.revision_id;
        let document = match edit.apply_validated(&basis.active_revision.document) {
            Ok(document) => document,
            Err(refusal) => {
                return Ok(RoutingValidation {
                    active_revision_id,
                    valid: false,
                    diagnostics: Vec::new(),
                    refusal: Some(refusal),
                });
            }
        };
        let compilation = compile_control(&document, &self.registry, &basis.inventory);
        // Info and warning findings ride along with a policy that compiled,
        // so validation shows the notes (an unconfigured binding, an offer the
        // catalogue lacks) the same way it shows the errors.
        Ok(RoutingValidation {
            active_revision_id,
            valid: compilation.policy().is_some(),
            diagnostics: compilation.diagnostics().to_vec(),
            refusal: None,
        })
    }

    /// Resolve a request through the same pure resolver inference uses, under
    /// the active revision, an unsaved edit or a kept revision
    /// (spec: RREV#preview).
    pub(crate) fn preview_routing(
        &self,
        request: RoutingPreviewRequest,
    ) -> Result<RoutingPreview, RoutingControlError> {
        let _control = self
            .routing_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let basis = self.control_basis_locked()?;
        Self::expect_active_revision(&basis, request.expected_revision_id)?;
        let active_revision_id = basis.active_revision.revision_id;
        let (policy, diagnostics, against_revision_id) = match &request.against {
            PreviewAgainst::Active => {
                let compilation = crate::policy::runtime::compile_stored_revision(
                    &basis.active_revision,
                    &self.registry,
                    &basis.inventory,
                );
                let diagnostics = compilation.diagnostics().to_vec();
                (
                    compilation
                        .into_policy()
                        .map_err(RoutingControlError::Compilation)?,
                    diagnostics,
                    Some(active_revision_id),
                )
            }
            PreviewAgainst::Edit(edit) => {
                let document = edit.apply_validated(&basis.active_revision.document)?;
                let compilation = compile_control(&document, &self.registry, &basis.inventory);
                let diagnostics = compilation.diagnostics().to_vec();
                (
                    compilation
                        .into_policy()
                        .map_err(RoutingControlError::Compilation)?,
                    diagnostics,
                    None,
                )
            }
            PreviewAgainst::Revision(revision_id) => {
                let revision = self
                    .store
                    .load_policy_revision(*revision_id)?
                    .ok_or(RoutingControlError::MissingRevision(*revision_id))?;
                let compilation = crate::policy::runtime::compile_stored_revision(
                    &revision,
                    &self.registry,
                    &basis.inventory,
                );
                let diagnostics = compilation.diagnostics().to_vec();
                (
                    compilation
                        .into_policy()
                        .map_err(RoutingControlError::Compilation)?,
                    diagnostics,
                    Some(*revision_id),
                )
            }
        };
        let now = now_ms();
        let (facts, lane) = match request.subject {
            PreviewSubject::Invented(invented) => {
                let lane = invented
                    .lane_key
                    .as_deref()
                    .map(|key| self.store.load_lane(key))
                    .transpose()?
                    .flatten();
                // Given to the resolver as the service gives it a live
                // request's lane: an unpinned lane that is not proven cold
                // resolves as new, and a pin carries the fingerprint of the
                // policy resolving.
                let lane_facts = lane
                    .as_ref()
                    .and_then(|lane| {
                        self.lane_facts(
                            Some(lane),
                            lane.pin
                                .as_ref()
                                .and_then(|pin| self.registry.binding(&pin.binding_id)),
                            &Default::default(),
                            now,
                            false,
                        )
                    })
                    .filter(|lane| {
                        lane.pinned.is_some()
                            || lane.temperature == crate::policy::CacheTemperature::Cold
                    })
                    .map(|mut lane| {
                        if let Some(pin) = lane.pinned.as_mut() {
                            pin.policy_fingerprint = Some(policy.fingerprint().clone());
                        }
                        lane
                    });
                let facts = RequestFacts {
                    profile: invented.profile,
                    protocol: invented.protocol,
                    requested_model: invented.model,
                    lane_reference: invented.lane_key,
                    prompt_bound: invented.prompt_bound,
                    message_count: invented.message_count,
                    tool_count: invented.tool_count,
                    required_capabilities: invented.required_capabilities,
                    asks_server_safeguards: invented.asks_server_safeguards,
                    binding_affinity: invented.binding_affinity,
                    class: RequestClass::Inference,
                    compaction_evidence: None,
                    // A dry run has no request behind it, so no credential
                    // to name an account.
                    accounts: Default::default(),
                    now_ms: now,
                };
                (facts, lane_facts)
            }
            PreviewSubject::Recorded(request_id) => {
                let row = self
                    .store
                    .routed_request(request_id)?
                    .ok_or(RoutingControlError::RequestNotFound(request_id))?;
                if let Some(reason) = NotReplayable::of(row.facts.as_ref()) {
                    return Ok(RoutingPreview {
                        active_revision_id,
                        against_revision_id,
                        diagnostics,
                        outcome: PreviewOutcome::NotReplayable { reason },
                    });
                }
                let facts = row.facts.expect("a replayable row carries its facts");
                (
                    facts.request_facts(now),
                    facts.lane.lane_facts(policy.fingerprint()),
                )
            }
        };
        let availability = basis
            .availability
            .at(now, crate::catalog::fetched::CACHE_TTL_MS)
            .with_safeguards_marks(&self.safeguards_marks());
        let learning = crate::policy::LearningView::snapshot(self.binding_learning.as_ref());
        let result = crate::policy::resolve(
            &policy,
            &basis.inventory,
            &learning,
            lane.as_ref(),
            &availability,
            &facts,
        );
        Ok(RoutingPreview {
            active_revision_id,
            against_revision_id,
            diagnostics,
            outcome: PreviewOutcome::Resolved {
                facts: RecordedFacts::of(&facts)
                    .with_lane(crate::policy::facts::RecordedLane::of(lane.as_ref())),
                result: preview_result_body(result, &availability),
            },
        })
    }

    /// Replay the last 24 hours' requests under the active revision and under
    /// an unsaved edit, and return each whose outcome changes
    /// (spec: UTUI#editing). A request whose row does not record the facts
    /// the resolver reads is counted, not replayed.
    pub(crate) fn replay_recent_routing(
        &self,
        expected_revision_id: Option<i64>,
        edit: &PolicyEdit,
    ) -> Result<RoutingReplay, RoutingControlError> {
        let _control = self
            .routing_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let basis = self.control_basis_locked()?;
        Self::expect_active_revision(&basis, expected_revision_id)?;
        let document = edit.apply_validated(&basis.active_revision.document)?;
        let edited = compile_control(&document, &self.registry, &basis.inventory);
        let diagnostics = edited.diagnostics().to_vec();
        let edited = edited
            .into_policy()
            .map_err(RoutingControlError::Compilation)?;
        let active =
            compile_stored_policy(&basis.active_revision, &self.registry, &basis.inventory)?;
        let now = now_ms();
        let since_ms = now - ROUTING_WINDOW_MS;
        let rows = self
            .store
            .recent_routed_requests(since_ms, REPLAY_ROW_CAP.saturating_add(1))?;
        let truncated = rows.len() > REPLAY_ROW_CAP as usize;
        let availability = basis
            .availability
            .at(now, crate::catalog::fetched::CACHE_TTL_MS)
            .with_safeguards_marks(&self.safeguards_marks());
        let learning = crate::policy::LearningView::snapshot(self.binding_learning.as_ref());
        let mut replay = RoutingReplay {
            active_revision_id: basis.active_revision.revision_id,
            since_ms,
            until_ms: now,
            replayed: 0,
            unreplayable: 0,
            truncated,
            changed_total: 0,
            changed: Vec::new(),
            diagnostics,
        };
        for row in rows.into_iter().take(REPLAY_ROW_CAP as usize) {
            let Some(recorded) = row.facts.as_ref().filter(|facts| facts.replayable()) else {
                replay.unreplayable += 1;
                continue;
            };
            let facts = recorded.request_facts(now);
            let under_active = ReplayOutcome::of(crate::policy::resolve(
                &active,
                &basis.inventory,
                &learning,
                recorded.lane.lane_facts(active.fingerprint()).as_ref(),
                &availability,
                &facts,
            ));
            let under_edit = ReplayOutcome::of(crate::policy::resolve(
                &edited,
                &basis.inventory,
                &learning,
                recorded.lane.lane_facts(edited.fingerprint()).as_ref(),
                &availability,
                &facts,
            ));
            replay.replayed += 1;
            if under_active.destination() == under_edit.destination() {
                continue;
            }
            replay.changed_total += 1;
            if replay.changed.len() < REPLAY_CHANGED_CAP {
                replay.changed.push(ReplayChange {
                    request_id: row.id,
                    ts_ms: row.ts_ms,
                    facts: recorded.clone(),
                    under_active,
                    under_edit,
                });
            }
        }
        Ok(replay)
    }

    /// The newest routed requests, for a preview to pick one from, each
    /// flagged replayable or not.
    pub(crate) fn recent_routing_requests(
        &self,
        limit: u32,
    ) -> Result<Vec<crate::store::RecentRequest>, crate::store::Error> {
        self.store
            .recent_routed_requests(now_ms() - ROUTING_WINDOW_MS, limit)
    }
}

/// A widened activation, as the control surface answers it.
#[derive(Debug)]
pub(crate) struct RoutingWidening {
    pub(crate) revision_id: i64,
    pub(crate) scope: ActivationScope,
    pub(crate) forced_cache_transition: bool,
    pub(crate) widened_ms: Option<i64>,
    pub(crate) lane_impacts: super::RoutingLaneImpacts,
}
