//! Pure lane-pin and cache-transition decisions.
//!
//! This module decides whether a prospective route identity may replace the
//! lane state captured at request start. It performs no store reads or writes;
//! callers apply [`TransitionDecision::FirstPin`] or
//! [`TransitionDecision::Replace`] with one compare-and-swap after success.

use super::{
    ActivationScope, BindingId, CacheTransitionPolicy, CandidateId, LaneTransitionPolicy,
    RouteFingerprint, RouteId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTemperature {
    Warm,
    Cold,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinIdentity {
    pub policy_revision_id: i64,
    pub policy_route_id: RouteId,
    pub route_fingerprint: RouteFingerprint,
    pub candidate_id: CandidateId,
    pub binding_id: BindingId,
    pub baseline_model: String,
    pub effective_model: String,
}

impl PinIdentity {
    fn same_cache_as(&self, other: &Self) -> bool {
        self.binding_id == other.binding_id && self.effective_model == other.effective_model
    }
}

impl From<&crate::store::LanePin> for PinIdentity {
    fn from(pin: &crate::store::LanePin) -> Self {
        Self {
            policy_revision_id: pin.policy_revision_id,
            policy_route_id: pin.policy_route_id.clone(),
            route_fingerprint: pin.route_fingerprint.clone(),
            candidate_id: pin.candidate_id.clone(),
            binding_id: pin.binding_id.clone(),
            baseline_model: pin.baseline_model.clone(),
            effective_model: pin.effective_model.clone(),
        }
    }
}

impl From<PinIdentity> for crate::store::LanePin {
    fn from(pin: PinIdentity) -> Self {
        Self {
            policy_revision_id: pin.policy_revision_id,
            policy_route_id: pin.policy_route_id,
            route_fingerprint: pin.route_fingerprint,
            candidate_id: pin.candidate_id,
            binding_id: pin.binding_id,
            baseline_model: pin.baseline_model,
            effective_model: pin.effective_model,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneState {
    Absent,
    Existing {
        generation: i64,
        pin: Option<PinIdentity>,
        temperature: CacheTemperature,
    },
}

impl LaneState {
    pub fn from_stored(lane: Option<&crate::store::Lane>, temperature: CacheTemperature) -> Self {
        match lane {
            None => Self::Absent,
            Some(lane) => Self::Existing {
                generation: lane.pin_generation,
                pin: lane.pin.as_ref().map(PinIdentity::from),
                temperature,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionCause {
    Activation {
        scope: ActivationScope,
        forced_cache_transition: bool,
    },
    Upgrade {
        lane: LaneTransitionPolicy,
    },
    Fallback {
        cache: CacheTransitionPolicy,
    },
    ExplicitUserChange {
        cache: CacheTransitionPolicy,
    },
    Compaction {
        lane: LaneTransitionPolicy,
        cache: CacheTransitionPolicy,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionSource {
    NewLane,
    Activation,
    Upgrade,
    Fallback,
    ExplicitUserChange,
    Compaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionDenied {
    ExistingLaneUnpinned,
    StaleRevision,
    LanePinned,
    LaneNotCold,
    CacheNotCold,
    WarmCacheLoss,
    InvalidCauseIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionDecision {
    FirstPin {
        expected_generation: i64,
        new_pin: PinIdentity,
        source: TransitionSource,
    },
    Keep {
        pin: Option<PinIdentity>,
    },
    Replace {
        expected_generation: i64,
        expected_pin: PinIdentity,
        new_pin: PinIdentity,
        source: TransitionSource,
    },
    Denied(TransitionDenied),
}

pub fn decide_transition(
    lane: &LaneState,
    prospective: PinIdentity,
    cause: TransitionCause,
) -> TransitionDecision {
    let LaneState::Existing {
        generation,
        pin,
        temperature,
    } = lane
    else {
        return TransitionDecision::FirstPin {
            expected_generation: 0,
            new_pin: prospective,
            source: TransitionSource::NewLane,
        };
    };
    let Some(current) = pin else {
        return if *temperature != CacheTemperature::Cold {
            TransitionDecision::Denied(TransitionDenied::ExistingLaneUnpinned)
        } else if matches!(
            cause,
            TransitionCause::Activation {
                scope: ActivationScope::NewLanes,
                ..
            }
        ) {
            TransitionDecision::Keep { pin: None }
        } else {
            TransitionDecision::FirstPin {
                expected_generation: *generation,
                new_pin: prospective,
                source: source_of(cause),
            }
        };
    };
    if current == &prospective {
        return TransitionDecision::Keep {
            pin: Some(current.clone()),
        };
    }
    if prospective.policy_revision_id < current.policy_revision_id {
        return TransitionDecision::Denied(TransitionDenied::StaleRevision);
    }

    let cache_changes = !current.same_cache_as(&prospective);
    match cause {
        TransitionCause::Activation {
            scope: ActivationScope::NewLanes,
            ..
        } => {
            return TransitionDecision::Keep {
                pin: Some(current.clone()),
            };
        }
        TransitionCause::Activation {
            scope: ActivationScope::NewAndColdLanes,
            ..
        } if *temperature != CacheTemperature::Cold => {
            return TransitionDecision::Keep {
                pin: Some(current.clone()),
            };
        }
        TransitionCause::Activation {
            scope: ActivationScope::NewAndColdLanes,
            ..
        } => {}
        TransitionCause::Activation {
            scope: ActivationScope::Immediate,
            forced_cache_transition,
        } => {
            if cache_changes && *temperature != CacheTemperature::Cold && !forced_cache_transition {
                return TransitionDecision::Denied(TransitionDenied::WarmCacheLoss);
            }
        }
        TransitionCause::Upgrade { lane } => {
            if !same_candidate_baseline(current, &prospective) {
                return TransitionDecision::Denied(TransitionDenied::InvalidCauseIdentity);
            }
            if let Err(reason) = lane_allows(lane, *temperature) {
                return TransitionDecision::Denied(reason);
            }
        }
        TransitionCause::Fallback { cache } => {
            if !same_revision_route(current, &prospective) {
                return TransitionDecision::Denied(TransitionDenied::InvalidCauseIdentity);
            }
            if let Err(reason) = cache_allows(cache, cache_changes, *temperature) {
                return TransitionDecision::Denied(reason);
            }
        }
        TransitionCause::ExplicitUserChange { cache } => {
            if let Err(reason) = cache_allows(cache, cache_changes, *temperature) {
                return TransitionDecision::Denied(reason);
            }
        }
        TransitionCause::Compaction { lane, cache } => {
            if let Err(reason) = lane_allows(lane, *temperature) {
                return TransitionDecision::Denied(reason);
            }
            if let Err(reason) = cache_allows(cache, cache_changes, *temperature) {
                return TransitionDecision::Denied(reason);
            }
        }
    };
    TransitionDecision::Replace {
        expected_generation: *generation,
        expected_pin: current.clone(),
        new_pin: prospective,
        source: source_of(cause),
    }
}

fn same_revision_route(current: &PinIdentity, prospective: &PinIdentity) -> bool {
    current.policy_revision_id == prospective.policy_revision_id
        && current.policy_route_id == prospective.policy_route_id
        && current.route_fingerprint == prospective.route_fingerprint
}

fn same_candidate_baseline(current: &PinIdentity, prospective: &PinIdentity) -> bool {
    same_revision_route(current, prospective)
        && current.candidate_id == prospective.candidate_id
        && current.binding_id == prospective.binding_id
        && current.baseline_model == prospective.baseline_model
}

fn lane_allows(
    policy: LaneTransitionPolicy,
    temperature: CacheTemperature,
) -> Result<(), TransitionDenied> {
    match policy {
        LaneTransitionPolicy::PreservePinned => Err(TransitionDenied::LanePinned),
        LaneTransitionPolicy::MoveWhenCold if temperature != CacheTemperature::Cold => {
            Err(TransitionDenied::LaneNotCold)
        }
        LaneTransitionPolicy::MoveWhenCold | LaneTransitionPolicy::AllowImmediate => Ok(()),
    }
}

fn cache_allows(
    policy: CacheTransitionPolicy,
    cache_changes: bool,
    temperature: CacheTemperature,
) -> Result<(), TransitionDenied> {
    if !cache_changes {
        return Ok(());
    }
    match policy {
        CacheTransitionPolicy::AllowCacheLoss => Ok(()),
        CacheTransitionPolicy::PreserveWarm | CacheTransitionPolicy::AllowWhenCold
            if temperature == CacheTemperature::Cold =>
        {
            Ok(())
        }
        CacheTransitionPolicy::PreserveWarm => Err(TransitionDenied::WarmCacheLoss),
        CacheTransitionPolicy::AllowWhenCold => Err(TransitionDenied::CacheNotCold),
    }
}

fn source_of(cause: TransitionCause) -> TransitionSource {
    match cause {
        TransitionCause::Activation { .. } => TransitionSource::Activation,
        TransitionCause::Upgrade { .. } => TransitionSource::Upgrade,
        TransitionCause::Fallback { .. } => TransitionSource::Fallback,
        TransitionCause::ExplicitUserChange { .. } => TransitionSource::ExplicitUserChange,
        TransitionCause::Compaction { .. } => TransitionSource::Compaction,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(revision: i64, binding: &str, effective: &str) -> PinIdentity {
        PinIdentity {
            policy_revision_id: revision,
            policy_route_id: RouteId::new(format!("route-{revision}")).unwrap(),
            route_fingerprint: format!("{revision:02x}")
                .repeat(32)
                .parse::<RouteFingerprint>()
                .unwrap(),
            candidate_id: CandidateId::new("candidate").unwrap(),
            binding_id: BindingId::new(binding).unwrap(),
            baseline_model: "baseline".to_owned(),
            effective_model: effective.to_owned(),
        }
    }

    fn lane(pin: Option<PinIdentity>, temperature: CacheTemperature) -> LaneState {
        LaneState::Existing {
            generation: 4,
            pin,
            temperature,
        }
    }

    #[test]
    fn stored_lane_conversion_preserves_complete_pin_and_generation() {
        let identity = pin(1, "openrouter.openrouter_chat_completions", "model");
        let stored_pin: crate::store::LanePin = identity.clone().into();
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
            pin_generation: 7,
            pin: Some(stored_pin),
        };
        assert_eq!(
            LaneState::from_stored(Some(&lane), CacheTemperature::Unknown),
            LaneState::Existing {
                generation: 7,
                pin: Some(identity),
                temperature: CacheTemperature::Unknown,
            }
        );
    }

    #[test]
    fn absent_lane_first_pins_and_identical_pin_is_a_noop() {
        let prospective = pin(1, "openrouter.openrouter_chat_completions", "model");
        assert_eq!(
            decide_transition(
                &LaneState::Absent,
                prospective.clone(),
                TransitionCause::Activation {
                    scope: ActivationScope::NewLanes,
                    forced_cache_transition: false,
                },
            ),
            TransitionDecision::FirstPin {
                expected_generation: 0,
                new_pin: prospective.clone(),
                source: TransitionSource::NewLane,
            }
        );
        assert_eq!(
            decide_transition(
                &lane(Some(prospective.clone()), CacheTemperature::Warm),
                prospective.clone(),
                TransitionCause::Activation {
                    scope: ActivationScope::Immediate,
                    forced_cache_transition: false,
                },
            ),
            TransitionDecision::Keep {
                pin: Some(prospective)
            }
        );
    }

    #[test]
    fn existing_unpinned_lane_requires_verified_coldness() {
        let prospective = pin(1, "openrouter.openrouter_chat_completions", "model");
        for temperature in [CacheTemperature::Warm, CacheTemperature::Unknown] {
            assert_eq!(
                decide_transition(
                    &lane(None, temperature),
                    prospective.clone(),
                    TransitionCause::Activation {
                        scope: ActivationScope::Immediate,
                        forced_cache_transition: true,
                    },
                ),
                TransitionDecision::Denied(TransitionDenied::ExistingLaneUnpinned)
            );
        }
        assert_eq!(
            decide_transition(
                &lane(None, CacheTemperature::Cold),
                prospective.clone(),
                TransitionCause::Activation {
                    scope: ActivationScope::NewAndColdLanes,
                    forced_cache_transition: false,
                },
            ),
            TransitionDecision::FirstPin {
                expected_generation: 4,
                new_pin: prospective,
                source: TransitionSource::Activation,
            }
        );
    }

    #[test]
    fn activation_scope_preserves_or_replaces_the_loaded_pin() {
        let current = pin(1, "openrouter.openrouter_chat_completions", "old");
        let next = pin(2, "openrouter.openrouter_chat_completions", "new");
        assert!(matches!(
            decide_transition(
                &lane(Some(current.clone()), CacheTemperature::Cold),
                next.clone(),
                TransitionCause::Activation {
                    scope: ActivationScope::NewLanes,
                    forced_cache_transition: false,
                },
            ),
            TransitionDecision::Keep { .. }
        ));
        assert!(matches!(
            decide_transition(
                &lane(Some(current.clone()), CacheTemperature::Warm),
                next.clone(),
                TransitionCause::Activation {
                    scope: ActivationScope::NewAndColdLanes,
                    forced_cache_transition: false,
                },
            ),
            TransitionDecision::Keep { .. }
        ));
        assert_eq!(
            decide_transition(
                &lane(Some(current.clone()), CacheTemperature::Cold),
                next.clone(),
                TransitionCause::Activation {
                    scope: ActivationScope::NewAndColdLanes,
                    forced_cache_transition: false,
                },
            ),
            TransitionDecision::Replace {
                expected_generation: 4,
                expected_pin: current.clone(),
                new_pin: next.clone(),
                source: TransitionSource::Activation,
            }
        );
        assert_eq!(
            decide_transition(
                &lane(Some(current.clone()), CacheTemperature::Warm),
                next.clone(),
                TransitionCause::Activation {
                    scope: ActivationScope::Immediate,
                    forced_cache_transition: false,
                },
            ),
            TransitionDecision::Denied(TransitionDenied::WarmCacheLoss)
        );
        assert!(matches!(
            decide_transition(
                &lane(Some(current), CacheTemperature::Unknown),
                next,
                TransitionCause::Activation {
                    scope: ActivationScope::Immediate,
                    forced_cache_transition: true,
                },
            ),
            TransitionDecision::Replace { .. }
        ));
    }

    #[test]
    fn an_older_revision_never_replaces_a_newer_pin() {
        let current = pin(2, "openrouter.openrouter_chat_completions", "new");
        let stale = pin(1, "openrouter.openrouter_chat_completions", "old");
        assert_eq!(
            decide_transition(
                &lane(Some(current), CacheTemperature::Cold),
                stale,
                TransitionCause::Activation {
                    scope: ActivationScope::Immediate,
                    forced_cache_transition: true,
                },
            ),
            TransitionDecision::Denied(TransitionDenied::StaleRevision)
        );
    }

    #[test]
    fn unknown_temperature_never_satisfies_when_cold() {
        let current = pin(1, "openrouter.openrouter_chat_completions", "old");
        let next = pin(1, "openrouter.openrouter_chat_completions", "new");
        assert_eq!(
            decide_transition(
                &lane(Some(current.clone()), CacheTemperature::Unknown),
                next.clone(),
                TransitionCause::Upgrade {
                    lane: LaneTransitionPolicy::MoveWhenCold,
                },
            ),
            TransitionDecision::Denied(TransitionDenied::LaneNotCold)
        );
        assert_eq!(
            decide_transition(
                &lane(Some(current), CacheTemperature::Unknown),
                next,
                TransitionCause::Fallback {
                    cache: CacheTransitionPolicy::AllowWhenCold,
                },
            ),
            TransitionDecision::Denied(TransitionDenied::CacheNotCold)
        );
    }

    #[test]
    fn upgrade_cannot_authorize_a_candidate_or_backend_change() {
        let current = pin(1, "openrouter.openrouter_chat_completions", "old");
        let mut next = current.clone();
        next.candidate_id = CandidateId::new("other").unwrap();
        next.binding_id = BindingId::new("anthropic_api.anthropic_messages").unwrap();
        next.effective_model = "new".to_owned();
        assert_eq!(
            decide_transition(
                &lane(Some(current), CacheTemperature::Cold),
                next,
                TransitionCause::Upgrade {
                    lane: LaneTransitionPolicy::AllowImmediate,
                },
            ),
            TransitionDecision::Denied(TransitionDenied::InvalidCauseIdentity)
        );
    }

    #[test]
    fn fallback_stays_within_the_captured_revision_and_route() {
        let current = pin(1, "openrouter.openrouter_chat_completions", "old");
        let next = pin(2, "anthropic_api.anthropic_messages", "new");
        assert_eq!(
            decide_transition(
                &lane(Some(current), CacheTemperature::Cold),
                next,
                TransitionCause::Fallback {
                    cache: CacheTransitionPolicy::AllowCacheLoss,
                },
            ),
            TransitionDecision::Denied(TransitionDenied::InvalidCauseIdentity)
        );
    }

    #[test]
    fn fallback_cache_consent_is_only_needed_when_cache_identity_changes() {
        let current = pin(1, "openrouter.openrouter_chat_completions", "same");
        let mut same_cache = current.clone();
        same_cache.candidate_id = CandidateId::new("fallback").unwrap();
        assert!(matches!(
            decide_transition(
                &lane(Some(current.clone()), CacheTemperature::Warm),
                same_cache,
                TransitionCause::Fallback {
                    cache: CacheTransitionPolicy::PreserveWarm,
                },
            ),
            TransitionDecision::Replace { .. }
        ));
        let mut crossed = current.clone();
        crossed.candidate_id = CandidateId::new("fallback").unwrap();
        crossed.binding_id = BindingId::new("anthropic_api.anthropic_messages").unwrap();
        crossed.effective_model = "other".to_owned();
        assert_eq!(
            decide_transition(
                &lane(Some(current.clone()), CacheTemperature::Warm),
                crossed.clone(),
                TransitionCause::Fallback {
                    cache: CacheTransitionPolicy::PreserveWarm,
                },
            ),
            TransitionDecision::Denied(TransitionDenied::WarmCacheLoss)
        );
        assert!(matches!(
            decide_transition(
                &lane(Some(current), CacheTemperature::Warm),
                crossed,
                TransitionCause::Fallback {
                    cache: CacheTransitionPolicy::AllowCacheLoss,
                },
            ),
            TransitionDecision::Replace { .. }
        ));
    }

    #[test]
    fn explicit_user_change_uses_its_cache_consent() {
        let current = pin(1, "openrouter.openrouter_chat_completions", "old");
        let next = pin(1, "anthropic_api.anthropic_messages", "new");
        assert_eq!(
            decide_transition(
                &lane(Some(current.clone()), CacheTemperature::Unknown),
                next.clone(),
                TransitionCause::ExplicitUserChange {
                    cache: CacheTransitionPolicy::AllowWhenCold,
                },
            ),
            TransitionDecision::Denied(TransitionDenied::CacheNotCold)
        );
        assert!(matches!(
            decide_transition(
                &lane(Some(current), CacheTemperature::Warm),
                next,
                TransitionCause::ExplicitUserChange {
                    cache: CacheTransitionPolicy::AllowCacheLoss,
                },
            ),
            TransitionDecision::Replace {
                source: TransitionSource::ExplicitUserChange,
                ..
            }
        ));
    }

    #[test]
    fn compaction_requires_both_lane_and_cache_permission() {
        let current = pin(1, "openrouter.openrouter_chat_completions", "old");
        let next = pin(1, "anthropic_api.anthropic_messages", "new");
        assert_eq!(
            decide_transition(
                &lane(Some(current.clone()), CacheTemperature::Warm),
                next.clone(),
                TransitionCause::Compaction {
                    lane: LaneTransitionPolicy::MoveWhenCold,
                    cache: CacheTransitionPolicy::AllowCacheLoss,
                },
            ),
            TransitionDecision::Denied(TransitionDenied::LaneNotCold)
        );
        assert_eq!(
            decide_transition(
                &lane(Some(current.clone()), CacheTemperature::Warm),
                next.clone(),
                TransitionCause::Compaction {
                    lane: LaneTransitionPolicy::AllowImmediate,
                    cache: CacheTransitionPolicy::PreserveWarm,
                },
            ),
            TransitionDecision::Denied(TransitionDenied::WarmCacheLoss)
        );
        assert!(matches!(
            decide_transition(
                &lane(Some(current), CacheTemperature::Cold),
                next,
                TransitionCause::Compaction {
                    lane: LaneTransitionPolicy::MoveWhenCold,
                    cache: CacheTransitionPolicy::AllowWhenCold,
                },
            ),
            TransitionDecision::Replace { .. }
        ));
    }
}
