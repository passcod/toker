//! The resolver's facts as a ledger row keeps them.
//!
//! A row of a policy-routed request records the facts the resolver was given,
//! so that the request can later be resolved again under another revision
//! (spec: RPROV#routing-provenance). They are content-free by the rule the
//! resolver's own inputs follow: identities, counts and capability flags.
//!
//! The lane's state is among them, as the resolver read it: a replay that
//! resolved a pinned lane as a new one could start its walk at a candidate
//! the request would never have reached.

use super::{
    BindingId, CacheTemperature, CandidateId, CapabilityFlags, FrontendProtocol, LaneFacts,
    PolicyFingerprint, ProfileId, ProvisionalPin, RequestClass, RequestFacts, RouteFingerprint,
    resolve::CompactionEvidence,
};

/// The content-free facts of one request, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedFacts {
    pub profile: Option<ProfileId>,
    pub protocol: FrontendProtocol,
    pub requested_model: Option<String>,
    pub prompt_bound: u64,
    pub message_count: Option<u64>,
    pub tool_count: u64,
    pub required_capabilities: CapabilityFlags,
    pub asks_server_safeguards: bool,
    pub binding_affinity: Option<BindingId>,
    pub class: RequestClass,
    pub lane: RecordedLane,
}

/// The lane's state as the resolver was given it (spec: RSOLV#facts-in).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedLane {
    /// The row was written before the lane's state was recorded.
    NotRecorded,
    /// The resolver was given no lane: a new one, or one the service resolves
    /// as new.
    New,
    /// Boxed: the facts travel inside a resolve error and a preview
    /// outcome, which clippy keeps small.
    Read(Box<RecordedLaneState>),
}

/// What the resolver reads of a lane it is given. The pin's revision and
/// baseline model are not among it: the service sets the pin's policy
/// fingerprint to that of the policy resolving, and the resolver reads
/// nothing else of the pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedLaneState {
    pub temperature: CacheTemperature,
    pub pin: Option<RecordedPin>,
    pub forced_from: Option<String>,
    pub forced_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedPin {
    pub route_fingerprint: RouteFingerprint,
    pub candidate_id: CandidateId,
    pub binding_id: BindingId,
    pub effective_model: String,
}

impl RecordedLane {
    pub fn of(lane: Option<&LaneFacts>) -> Self {
        lane.map_or(Self::New, |lane| {
            Self::Read(Box::new(RecordedLaneState {
                temperature: lane.temperature,
                pin: lane.pinned.as_ref().map(|pin| RecordedPin {
                    route_fingerprint: pin.route_fingerprint.clone(),
                    candidate_id: pin.candidate_id.clone(),
                    binding_id: pin.binding_id.clone(),
                    effective_model: pin.effective_model.clone(),
                }),
                forced_from: lane.forced_from.clone(),
                forced_to: lane.forced_to.clone(),
            }))
        })
    }

    /// The lane to give the resolver again under the policy whose
    /// fingerprint is `policy`, as the service gives it a pinned lane.
    /// `None` for a new lane, and for one not recorded, which is not
    /// replayable.
    pub fn lane_facts(&self, policy: &PolicyFingerprint) -> Option<LaneFacts> {
        let Self::Read(lane) = self else {
            return None;
        };
        Some(LaneFacts {
            temperature: lane.temperature,
            pinned: lane.pin.as_ref().map(|pin| ProvisionalPin {
                policy_revision_id: None,
                policy_fingerprint: Some(policy.clone()),
                route_fingerprint: pin.route_fingerprint.clone(),
                candidate_id: pin.candidate_id.clone(),
                binding_id: pin.binding_id.clone(),
                // Not recorded, and the resolver reads only the effective
                // model of a pin.
                baseline_model: pin.effective_model.clone(),
                effective_model: pin.effective_model.clone(),
            }),
            forced_from: lane.forced_from.clone(),
            forced_to: lane.forced_to.clone(),
        })
    }
}

/// Why a recorded request cannot be resolved again without a fact assumed
/// (spec: RREV#preview).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotReplayable {
    /// The row predates the recording of the resolver's facts.
    FactsNotRecorded,
    /// A compaction's cold-lane evidence is read beside the facts and is not
    /// recorded.
    CompactionEvidenceNotRecorded,
    /// The row predates the recording of the lane's state.
    LaneNotRecorded,
}

impl NotReplayable {
    pub fn of(facts: Option<&RecordedFacts>) -> Option<Self> {
        match facts {
            None => Some(Self::FactsNotRecorded),
            Some(facts) => facts.not_replayable(),
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::FactsNotRecorded => "facts_not_recorded",
            Self::CompactionEvidenceNotRecorded => "compaction_evidence_not_recorded",
            Self::LaneNotRecorded => "lane_not_recorded",
        }
    }
}

impl RecordedFacts {
    /// The facts as recorded, with the lane not yet among them: the service
    /// records the lane it gave the resolver with [`Self::with_lane`], and a
    /// plan it does not is read as lacking it rather than as a new lane.
    pub fn of(facts: &RequestFacts) -> Self {
        Self {
            profile: facts.profile.clone(),
            protocol: facts.protocol,
            requested_model: facts.requested_model.clone(),
            prompt_bound: facts.prompt_bound,
            message_count: facts.message_count,
            tool_count: facts.tool_count,
            required_capabilities: facts.required_capabilities,
            asks_server_safeguards: facts.asks_server_safeguards,
            binding_affinity: facts.binding_affinity.clone(),
            class: facts.class,
            lane: RecordedLane::NotRecorded,
        }
    }

    pub fn with_lane(mut self, lane: RecordedLane) -> Self {
        self.lane = lane;
        self
    }

    /// Why resolving these facts again would not reproduce what the resolver
    /// read at the time, if it would not. A compaction's cold-lane evidence
    /// is read beside the facts and is not recorded, so a compaction would be
    /// resolved with a fact assumed in its place.
    pub fn not_replayable(&self) -> Option<NotReplayable> {
        if self.class != RequestClass::Inference {
            Some(NotReplayable::CompactionEvidenceNotRecorded)
        } else if self.lane == RecordedLane::NotRecorded {
            Some(NotReplayable::LaneNotRecorded)
        } else {
            None
        }
    }

    pub fn replayable(&self) -> bool {
        self.not_replayable().is_none()
    }

    /// The facts to resolve again at `now_ms`. The lane's state goes beside
    /// them, from [`RecordedLane::lane_facts`].
    pub fn request_facts(&self, now_ms: i64) -> RequestFacts {
        RequestFacts {
            profile: self.profile.clone(),
            protocol: self.protocol,
            requested_model: self.requested_model.clone(),
            lane_reference: None,
            prompt_bound: self.prompt_bound,
            message_count: self.message_count,
            tool_count: self.tool_count,
            required_capabilities: self.required_capabilities,
            asks_server_safeguards: self.asks_server_safeguards,
            binding_affinity: self.binding_affinity.clone(),
            class: self.class,
            compaction_evidence: None::<CompactionEvidence>,
            accounts: Default::default(),
            now_ms,
        }
    }
}

impl From<&RequestFacts> for RecordedFacts {
    fn from(facts: &RequestFacts) -> Self {
        Self::of(facts)
    }
}

pub fn protocol_name(protocol: FrontendProtocol) -> &'static str {
    match protocol {
        FrontendProtocol::AnthropicMessages => "anthropic_messages",
        FrontendProtocol::OpenaiChat => "openai_chat",
        FrontendProtocol::OpenaiResponses => "openai_responses",
    }
}

pub fn parse_protocol(value: &str) -> Option<FrontendProtocol> {
    match value {
        "anthropic_messages" => Some(FrontendProtocol::AnthropicMessages),
        "openai_chat" => Some(FrontendProtocol::OpenaiChat),
        "openai_responses" => Some(FrontendProtocol::OpenaiResponses),
        _ => None,
    }
}

pub fn class_name(class: RequestClass) -> &'static str {
    match class {
        RequestClass::Inference => "inference",
        RequestClass::Compaction => "compaction",
    }
}

pub fn parse_class(value: &str) -> Option<RequestClass> {
    match value {
        "inference" => Some(RequestClass::Inference),
        "compaction" => Some(RequestClass::Compaction),
        _ => None,
    }
}

/// The stored form of the lane as the resolver was given it: `new` for no
/// lane, or the given lane's cache temperature.
pub fn lane_name(lane: &RecordedLane) -> Option<&'static str> {
    match lane {
        RecordedLane::NotRecorded => None,
        RecordedLane::New => Some("new"),
        RecordedLane::Read(lane) => Some(match lane.temperature {
            CacheTemperature::Warm => "warm",
            CacheTemperature::Cold => "cold",
            CacheTemperature::Unknown => "unknown",
        }),
    }
}

/// The temperature a stored lane value names, or `None` for `new`.
pub fn parse_lane_temperature(value: &str) -> Option<Option<CacheTemperature>> {
    match value {
        "new" => Some(None),
        "warm" => Some(Some(CacheTemperature::Warm)),
        "cold" => Some(Some(CacheTemperature::Cold)),
        "unknown" => Some(Some(CacheTemperature::Unknown)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one spelling of a protocol: the control surface, the derived
    /// facts, the revision diffs and the store's publication keys all read
    /// it from here, and the store's rows hold it, so it round-trips.
    #[test]
    fn every_protocol_has_one_name_that_parses_back() {
        for protocol in [
            FrontendProtocol::AnthropicMessages,
            FrontendProtocol::OpenaiChat,
            FrontendProtocol::OpenaiResponses,
        ] {
            // Exhaustive, so a protocol added without a name fails here.
            match protocol {
                FrontendProtocol::AnthropicMessages
                | FrontendProtocol::OpenaiChat
                | FrontendProtocol::OpenaiResponses => {}
            }
            assert_eq!(parse_protocol(protocol_name(protocol)), Some(protocol));
        }
    }

    fn pinned() -> LaneFacts {
        LaneFacts {
            temperature: CacheTemperature::Warm,
            pinned: Some(ProvisionalPin {
                policy_revision_id: Some(3),
                policy_fingerprint: None,
                route_fingerprint: "11".repeat(32).parse().unwrap(),
                candidate_id: "second".parse().unwrap(),
                binding_id: "openrouter.openai_chat".parse().unwrap(),
                baseline_model: "base".to_owned(),
                effective_model: "effective".to_owned(),
            }),
            forced_from: Some("from".to_owned()),
            forced_to: Some("to".to_owned()),
        }
    }

    #[test]
    fn a_recorded_pinned_lane_is_given_back_under_the_replaying_policy() {
        let fingerprint: PolicyFingerprint = "22".repeat(32).parse().unwrap();
        let lane = RecordedLane::of(Some(&pinned()))
            .lane_facts(&fingerprint)
            .expect("a lane that was read is given back");
        let pin = lane.pinned.expect("the pin is kept");
        // The walk starts at the pinned candidate only where the pin's policy
        // fingerprint is the resolving policy's, as the service sets it.
        assert_eq!(pin.policy_fingerprint, Some(fingerprint));
        assert_eq!(
            pin.route_fingerprint,
            pinned().pinned.unwrap().route_fingerprint
        );
        assert_eq!(pin.candidate_id.as_str(), "second");
        assert_eq!(pin.binding_id.as_str(), "openrouter.openai_chat");
        assert_eq!(pin.effective_model, "effective");
        assert_eq!(lane.temperature, CacheTemperature::Warm);
        assert_eq!(lane.forced_from.as_deref(), Some("from"));
        assert_eq!(lane.forced_to.as_deref(), Some("to"));
    }

    #[test]
    fn a_lane_not_recorded_is_not_replayable_and_a_new_one_is() {
        let fingerprint: PolicyFingerprint = "22".repeat(32).parse().unwrap();
        let facts = RecordedFacts {
            profile: None,
            protocol: FrontendProtocol::OpenaiChat,
            requested_model: Some("m".to_owned()),
            prompt_bound: 1,
            message_count: Some(1),
            tool_count: 0,
            required_capabilities: CapabilityFlags::default(),
            asks_server_safeguards: false,
            binding_affinity: None,
            class: RequestClass::Inference,
            lane: RecordedLane::NotRecorded,
        };
        assert_eq!(facts.not_replayable(), Some(NotReplayable::LaneNotRecorded));
        let new = facts.with_lane(RecordedLane::New);
        assert_eq!(new.not_replayable(), None);
        assert_eq!(new.lane.lane_facts(&fingerprint), None);
    }
}
