//! Apply an already-decided lane transition with exactly one Store CAS.
//!
//! Resolution and cache-safety remain pure in [`super::transition`]. This
//! service is the persistence boundary used after a successful attempt; a
//! conflict preserves the winner and is never retried or re-resolved.

use crate::store::{LanePin, LanePinCasResult, Store};

use super::{LaneTransitionDecision, TransitionDenied};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinPersistence {
    Unchanged,
    Denied(TransitionDenied),
    Updated { generation: i64 },
    Conflict,
}

pub fn persist_transition(
    store: &Store,
    lane_key: &str,
    decision: &LaneTransitionDecision,
) -> crate::store::Result<PinPersistence> {
    let result = match decision {
        LaneTransitionDecision::FirstPin {
            expected_generation,
            new_pin,
            ..
        } => store.compare_and_swap_lane_pin(
            lane_key,
            *expected_generation,
            None,
            Some(&LanePin::from(new_pin.clone())),
        )?,
        LaneTransitionDecision::Replace {
            expected_generation,
            expected_pin,
            new_pin,
            ..
        } => store.compare_and_swap_lane_pin(
            lane_key,
            *expected_generation,
            Some(&LanePin::from(expected_pin.clone())),
            Some(&LanePin::from(new_pin.clone())),
        )?,
        LaneTransitionDecision::Keep { .. } => return Ok(PinPersistence::Unchanged),
        LaneTransitionDecision::Denied(reason) => return Ok(PinPersistence::Denied(*reason)),
    };
    Ok(match result {
        LanePinCasResult::Updated { generation } => PinPersistence::Updated { generation },
        LanePinCasResult::Conflict => PinPersistence::Conflict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{BindingId, CandidateId, PinIdentity, RouteFingerprint, RouteId};
    use crate::store::{Lane, NewPolicyRevision, PolicyRevisionSource};

    fn policy() -> crate::policy::ValidatedPolicyDocument {
        crate::policy::PolicyDocument {
            format_version: crate::policy::CURRENT_FORMAT_VERSION,
            description: None,
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

    fn pin(revision: i64, effective: &str) -> PinIdentity {
        PinIdentity {
            policy_revision_id: revision,
            policy_route_id: RouteId::new("route").unwrap(),
            route_fingerprint: "11".repeat(32).parse::<RouteFingerprint>().unwrap(),
            candidate_id: CandidateId::new("candidate").unwrap(),
            binding_id: BindingId::new("openrouter.openrouter_chat_completions").unwrap(),
            baseline_model: "baseline".to_owned(),
            effective_model: effective.to_owned(),
        }
    }

    fn setup() -> (Store, String, i64) {
        let store = Store::open(":memory:").unwrap();
        let document = policy();
        let revision = store
            .insert_policy_revision(&NewPolicyRevision {
                created_ms: 1,
                activated_ms: 2,
                description: None,
                activation_scope: crate::policy::ActivationScope::NewLanes,
                forced_cache_transition: false,
                source: PolicyRevisionSource::Draft,
                parent_revision_id: None,
                document: &document,
            })
            .unwrap();
        let key = "session|tools".to_owned();
        store
            .upsert_lane(&Lane {
                key: key.clone(),
                session_id: Some("session".to_owned()),
                tools_hash: Some("tools".to_owned()),
                updated_ms: 0,
                prompt_tokens: None,
                ttl: None,
                ping: None,
                noticed_at: None,
                forced_from: None,
                forced_to: None,
                pin_generation: 0,
                pin: None,
            })
            .unwrap();
        (store, key, revision.revision_id)
    }

    #[test]
    fn first_pin_and_replace_each_issue_one_cas() {
        let (store, key, revision) = setup();
        let first = pin(revision, "first");
        assert_eq!(
            persist_transition(
                &store,
                &key,
                &LaneTransitionDecision::FirstPin {
                    expected_generation: 0,
                    new_pin: first.clone(),
                    source: crate::policy::TransitionSource::NewLane,
                },
            )
            .unwrap(),
            PinPersistence::Updated { generation: 1 }
        );
        let replacement = pin(revision, "second");
        assert_eq!(
            persist_transition(
                &store,
                &key,
                &LaneTransitionDecision::Replace {
                    expected_generation: 1,
                    expected_pin: first,
                    new_pin: replacement.clone(),
                    source: crate::policy::TransitionSource::Upgrade,
                },
            )
            .unwrap(),
            PinPersistence::Updated { generation: 2 }
        );
        assert_eq!(
            store.load_lane(&key).unwrap().unwrap().pin,
            Some(LanePin::from(replacement))
        );
    }

    #[test]
    fn a_conflict_is_returned_without_retrying() {
        let (store, key, revision) = setup();
        let first = pin(revision, "first");
        store
            .compare_and_swap_lane_pin(&key, 0, None, Some(&LanePin::from(first.clone())))
            .unwrap();
        let stale = LaneTransitionDecision::FirstPin {
            expected_generation: 0,
            new_pin: pin(revision, "stale"),
            source: crate::policy::TransitionSource::NewLane,
        };
        assert_eq!(
            persist_transition(&store, &key, &stale).unwrap(),
            PinPersistence::Conflict
        );
        let loaded = store.load_lane(&key).unwrap().unwrap();
        assert_eq!(loaded.pin_generation, 1);
        assert_eq!(loaded.pin, Some(LanePin::from(first)));
    }
}
