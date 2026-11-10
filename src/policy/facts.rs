//! The resolver's facts as a ledger row keeps them.
//!
//! A row of a policy-routed request records the facts the resolver was given,
//! so that the request can later be resolved again under another revision
//! (spec: RPROV#routing-provenance). They are content-free by the rule the
//! resolver's own inputs follow: identities, counts and capability flags.
//!
//! The lane's state is not among them. It is evidence the resolver reads
//! beside the facts, and the ledger does not hold what it was at the time, so
//! a replay resolves as a new lane.

use super::{
    BindingId, CapabilityFlags, FrontendProtocol, ProfileId, RequestClass, RequestFacts,
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
}

impl RecordedFacts {
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
        }
    }

    /// Whether resolving these facts again would reproduce what the resolver
    /// read at the time. A compaction's cold-lane evidence is read beside the
    /// facts and is not recorded, so a compaction would be resolved with a
    /// fact assumed in its place.
    pub fn replayable(&self) -> bool {
        self.class == RequestClass::Inference
    }

    /// The facts to resolve again, as a new lane at `now_ms`.
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
