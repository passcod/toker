//! The identities in toker's protocol/provider route graph.
//!
//! This is phase 1 of `docs/plans/protocol-provider-mux.md`: it names the
//! graph the existing handlers already implement without changing dispatch.
//! Protocol identities are data; frontend and backend adapter behavior stays
//! in its existing typed modules until more than one composition needs it.

use std::fmt;

/// One inference wire protocol understood by a frontend or backend binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolId {
    AnthropicMessages,
    OpenAiChat,
    OpenAiResponses,
}

impl ProtocolId {
    /// The stable spelling used by config, ledger, and setup surfaces today.
    pub const fn as_str(self) -> &'static str {
        match self {
            ProtocolId::AnthropicMessages => "anthropic",
            ProtocolId::OpenAiChat => "openai_chat",
            ProtocolId::OpenAiResponses => "openai_responses",
        }
    }
}

impl fmt::Display for ProtocolId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A configured client identity carried by `/f/<name>`.
///
/// The request path remains authoritative for inference. `protocol` supplies
/// the answer only for frontend-owned ambiguous surfaces such as `/v1/models`.
/// An unfamiliar name stays `None`: it is never guessed into a known client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontendProfile {
    name: String,
    protocol: Option<ProtocolId>,
}

impl FrontendProfile {
    /// Resolve the profiles setup writes, plus `opencode` for hand-written
    /// prefixed configurations. Every other valid name remains unknown.
    pub fn named(name: impl Into<String>) -> FrontendProfile {
        let name = name.into();
        let protocol = match name.as_str() {
            "claude" | "workhorse" => Some(ProtocolId::AnthropicMessages),
            "opencode" => Some(ProtocolId::OpenAiChat),
            "codex" => Some(ProtocolId::OpenAiResponses),
            _ => None,
        };
        FrontendProfile { name, protocol }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn protocol(&self) -> Option<ProtocolId> {
        self.protocol
    }
}

/// One native wire exposed by a provider.
///
/// A binding is a verified declaration, not an inference from an endpoint's
/// spelling. Later phases attach a dialect adapter and capabilities here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BackendBinding {
    protocol: ProtocolId,
}

impl BackendBinding {
    pub const fn new(protocol: ProtocolId) -> BackendBinding {
        BackendBinding { protocol }
    }

    pub const fn protocol(self) -> ProtocolId {
        self.protocol
    }
}

#[cfg(test)]
mod tests {
    use super::{FrontendProfile, ProtocolId};

    #[test]
    fn known_frontend_profiles_name_their_ambiguous_protocol() {
        assert_eq!(
            FrontendProfile::named("claude").protocol(),
            Some(ProtocolId::AnthropicMessages)
        );
        assert_eq!(
            FrontendProfile::named("workhorse").protocol(),
            Some(ProtocolId::AnthropicMessages)
        );
        assert_eq!(
            FrontendProfile::named("opencode").protocol(),
            Some(ProtocolId::OpenAiChat)
        );
        assert_eq!(
            FrontendProfile::named("codex").protocol(),
            Some(ProtocolId::OpenAiResponses)
        );
    }

    #[test]
    fn an_unknown_frontend_name_stays_protocol_unknown() {
        let profile = FrontendProfile::named("invented-client");
        assert_eq!(profile.name(), "invented-client");
        assert_eq!(profile.protocol(), None);
    }
}
