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

/// A provider's concrete dialect of a nominal protocol.
///
/// Protocol identity answers which broad wire shape is in use. Dialect
/// identity selects the provider-specific rendering and interpretation rules
/// within that shape; it must therefore never be inferred from
/// [`ProtocolId`] alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DialectId {
    AnthropicMessages,
    OpenRouterMessages,
    OpenRouterChatCompletions,
    CodexResponses,
}

impl DialectId {
    pub const fn as_str(self) -> &'static str {
        match self {
            DialectId::AnthropicMessages => "anthropic_messages",
            DialectId::OpenRouterMessages => "openrouter_messages",
            DialectId::OpenRouterChatCompletions => "openrouter_chat_completions",
            DialectId::CodexResponses => "codex_responses",
        }
    }
}

impl fmt::Display for DialectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Semantics a backend binding has been live-verified to accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Capabilities {
    /// Sampling parameters (`temperature`, `top_p`, `max_tokens`, and stops).
    pub sampling: bool,
    /// System-role messages among the conversation's items.
    pub system_in_messages: bool,
    /// Replaying another provider's reasoning blocks.
    pub thinking_replay: bool,
    /// Image content blocks.
    pub images: bool,
}

impl Capabilities {
    /// The Messages features verified on Anthropic and OpenRouter's Messages
    /// binding. Signed reasoning replay is additionally enforced by the
    /// adapter; an unsigned block is reported and omitted.
    pub const MESSAGES: Capabilities = Capabilities {
        sampling: true,
        system_in_messages: true,
        thinking_replay: true,
        images: true,
    };

    /// OpenRouter's live Chat Completions binding. The request and response
    /// corpus verifies sampling, in-band instruction roles, images, and tool
    /// calls. Provider reasoning payloads have no portable replay shape.
    pub const CHAT: Capabilities = Capabilities {
        sampling: true,
        system_in_messages: true,
        thinking_replay: false,
        images: true,
    };

    /// The Codex subscription Responses binding's live-verified capabilities.
    pub const CODEX: Capabilities = Capabilities {
        sampling: false,
        system_in_messages: false,
        thinking_replay: false,
        images: true,
    };
}

/// A canonical backend adapter implementation.
///
/// This is code identity, not provider identity. Multiple provider dialects
/// may eventually share one adapter; the dialect remains an explicit input to
/// that adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendAdapterId {
    AnthropicMessages,
    OpenAiChatCompletions,
    CodexResponses,
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
    dialect: DialectId,
    canonical: Option<CanonicalBackend>,
}

impl BackendBinding {
    /// Declare a live wire binding whose universal canonical egress adapter
    /// has not been completed yet.
    pub const fn declared(protocol: ProtocolId, dialect: DialectId) -> BackendBinding {
        BackendBinding {
            protocol,
            dialect,
            canonical: None,
        }
    }

    /// Declare a binding with a verified canonical egress adapter.
    pub const fn canonical(
        protocol: ProtocolId,
        dialect: DialectId,
        adapter: BackendAdapterId,
        capabilities: Capabilities,
    ) -> BackendBinding {
        BackendBinding {
            protocol,
            dialect,
            canonical: Some(CanonicalBackend {
                adapter,
                capabilities,
            }),
        }
    }

    pub const fn protocol(self) -> ProtocolId {
        self.protocol
    }

    pub const fn dialect(self) -> DialectId {
        self.dialect
    }

    /// The canonical implementation and verified capabilities, once this
    /// binding can participate in the universal pipeline.
    pub const fn canonical_backend(self) -> Option<CanonicalBackend> {
        self.canonical
    }
}

/// The canonical egress half of a backend binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CanonicalBackend {
    adapter: BackendAdapterId,
    capabilities: Capabilities,
}

impl CanonicalBackend {
    pub const fn adapter(self) -> BackendAdapterId {
        self.adapter
    }

    pub const fn capabilities(self) -> Capabilities {
        self.capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::Capabilities;
    use super::{BackendAdapterId, BackendBinding, DialectId, FrontendProfile, ProtocolId};

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

    #[test]
    fn a_binding_separates_protocol_dialect_and_adapter_readiness() {
        let pending =
            BackendBinding::declared(ProtocolId::AnthropicMessages, DialectId::OpenRouterMessages);
        assert_eq!(pending.protocol(), ProtocolId::AnthropicMessages);
        assert_eq!(pending.dialect(), DialectId::OpenRouterMessages);
        assert_eq!(pending.canonical_backend(), None);

        let ready = BackendBinding::canonical(
            ProtocolId::OpenAiResponses,
            DialectId::CodexResponses,
            BackendAdapterId::CodexResponses,
            Capabilities::CODEX,
        );
        let canonical = ready.canonical_backend().expect("adapter is verified");
        assert_eq!(canonical.adapter(), BackendAdapterId::CodexResponses);
        assert_eq!(canonical.capabilities(), Capabilities::CODEX);
    }
}
