//! The identities in toker's protocol/provider route graph.
//!
//! Protocol identities, implemented adapter paths, and the shared model-target
//! resolver described in `docs/internals/routing.md`. Protocol identities are
//! data; frontend and backend adapter behavior remains in typed modules.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::policy::{BindingId, CostClass};
use crate::providers::Provider;

/// One inference wire protocol understood by a frontend or backend binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProtocolId {
    #[serde(rename = "anthropic")]
    AnthropicMessages,
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    #[serde(rename = "openai_responses")]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DialectId {
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
    #[serde(rename = "openrouter_messages")]
    OpenRouterMessages,
    #[serde(rename = "openrouter_chat_completions")]
    OpenRouterChatCompletions,
    #[serde(rename = "openrouter_responses")]
    OpenRouterResponses,
    #[serde(rename = "codex_responses")]
    CodexResponses,
}

impl DialectId {
    pub const fn as_str(self) -> &'static str {
        match self {
            DialectId::AnthropicMessages => "anthropic_messages",
            DialectId::OpenAiChatCompletions => "openai_chat_completions",
            DialectId::OpenRouterMessages => "openrouter_messages",
            DialectId::OpenRouterChatCompletions => "openrouter_chat_completions",
            DialectId::OpenRouterResponses => "openrouter_responses",
            DialectId::CodexResponses => "codex_responses",
        }
    }
}

impl fmt::Display for DialectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityEvidence {
    Supported,
    Unsupported,
    Unknown,
}

impl CapabilityEvidence {
    pub const fn is_unsupported(self) -> bool {
        matches!(self, Self::Unsupported)
    }

    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }
}

/// Evidence for semantics a backend binding may need to represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Capabilities {
    /// Sampling parameters (`temperature`, `top_p`, `max_tokens`, and stops).
    pub sampling: CapabilityEvidence,
    /// System-role messages among the conversation's items.
    pub system_in_messages: CapabilityEvidence,
    /// Replaying another provider's reasoning blocks.
    pub thinking_replay: CapabilityEvidence,
    /// Image content blocks.
    pub images: CapabilityEvidence,
    /// Tool declarations and tool call/result turns.
    pub tools: CapabilityEvidence,
    /// A tool choice that forbids tool calls.
    pub tool_choice_none: CapabilityEvidence,
    /// A tool choice that forces one named tool.
    pub tool_choice_named: CapabilityEvidence,
    /// A forced-tool shape whose name is absent.
    pub tool_choice_unnamed: CapabilityEvidence,
    /// A frontend tool-choice kind outside the canonical known set.
    pub tool_choice_other: CapabilityEvidence,
}

impl Capabilities {
    pub const OPENAI_CHAT: Capabilities = Capabilities {
        sampling: CapabilityEvidence::Unknown,
        system_in_messages: CapabilityEvidence::Unknown,
        thinking_replay: CapabilityEvidence::Unsupported,
        images: CapabilityEvidence::Unknown,
        tools: CapabilityEvidence::Unknown,
        tool_choice_none: CapabilityEvidence::Supported,
        tool_choice_named: CapabilityEvidence::Supported,
        tool_choice_unnamed: CapabilityEvidence::Unsupported,
        tool_choice_other: CapabilityEvidence::Unknown,
    };

    pub const UNKNOWN: Capabilities = Capabilities {
        sampling: CapabilityEvidence::Unknown,
        system_in_messages: CapabilityEvidence::Unknown,
        thinking_replay: CapabilityEvidence::Unknown,
        images: CapabilityEvidence::Unknown,
        tools: CapabilityEvidence::Unknown,
        tool_choice_none: CapabilityEvidence::Unknown,
        tool_choice_named: CapabilityEvidence::Unknown,
        tool_choice_unnamed: CapabilityEvidence::Unknown,
        tool_choice_other: CapabilityEvidence::Unknown,
    };

    /// The Messages features verified on Anthropic and OpenRouter's Messages
    /// binding. Signed reasoning replay is additionally enforced by the
    /// adapter; an unsigned block is reported and omitted.
    pub const MESSAGES: Capabilities = Capabilities {
        sampling: CapabilityEvidence::Supported,
        system_in_messages: CapabilityEvidence::Supported,
        thinking_replay: CapabilityEvidence::Supported,
        images: CapabilityEvidence::Supported,
        tools: CapabilityEvidence::Supported,
        tool_choice_none: CapabilityEvidence::Unsupported,
        tool_choice_named: CapabilityEvidence::Supported,
        tool_choice_unnamed: CapabilityEvidence::Supported,
        tool_choice_other: CapabilityEvidence::Unknown,
    };

    /// OpenRouter's live Chat Completions binding. The request and response
    /// corpus verifies sampling, in-band instruction roles, images, and tool
    /// calls. Provider reasoning payloads have no portable replay shape.
    pub const CHAT: Capabilities = Capabilities {
        sampling: CapabilityEvidence::Supported,
        system_in_messages: CapabilityEvidence::Supported,
        thinking_replay: CapabilityEvidence::Unsupported,
        images: CapabilityEvidence::Supported,
        tools: CapabilityEvidence::Supported,
        tool_choice_none: CapabilityEvidence::Supported,
        tool_choice_named: CapabilityEvidence::Supported,
        tool_choice_unnamed: CapabilityEvidence::Unsupported,
        tool_choice_other: CapabilityEvidence::Unknown,
    };

    /// The Codex subscription Responses binding's live-verified capabilities.
    pub const CODEX: Capabilities = Capabilities {
        sampling: CapabilityEvidence::Unsupported,
        system_in_messages: CapabilityEvidence::Unsupported,
        thinking_replay: CapabilityEvidence::Unsupported,
        images: CapabilityEvidence::Supported,
        tools: CapabilityEvidence::Supported,
        tool_choice_none: CapabilityEvidence::Supported,
        tool_choice_named: CapabilityEvidence::Unsupported,
        tool_choice_unnamed: CapabilityEvidence::Unsupported,
        tool_choice_other: CapabilityEvidence::Unsupported,
    };

    /// The streamed Responses subset verified against OpenRouter. The adapter
    /// deterministically rejects/drops these hard semantics, so they are known
    /// unsupported even where upstream support itself was not probed.
    pub const OPENROUTER_RESPONSES: Capabilities = Capabilities {
        sampling: CapabilityEvidence::Unsupported,
        system_in_messages: CapabilityEvidence::Unsupported,
        thinking_replay: CapabilityEvidence::Unsupported,
        images: CapabilityEvidence::Unsupported,
        tools: CapabilityEvidence::Supported,
        tool_choice_none: CapabilityEvidence::Supported,
        tool_choice_named: CapabilityEvidence::Unsupported,
        tool_choice_unnamed: CapabilityEvidence::Unsupported,
        tool_choice_other: CapabilityEvidence::Unsupported,
    };
}

/// A canonical backend adapter implementation.
///
/// This is code identity, not provider identity. Multiple provider dialects
/// may eventually share one adapter; the dialect remains an explicit input to
/// that adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BackendAdapterId {
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
    #[serde(rename = "codex_responses")]
    CodexResponses,
    #[serde(rename = "openrouter_responses")]
    OpenRouterResponses,
}

impl BackendAdapterId {
    pub const fn as_str(self) -> &'static str {
        match self {
            BackendAdapterId::AnthropicMessages => "anthropic_messages",
            BackendAdapterId::OpenAiChatCompletions => "openai_chat_completions",
            BackendAdapterId::CodexResponses => "codex_responses",
            BackendAdapterId::OpenRouterResponses => "openrouter_responses",
        }
    }
}

impl fmt::Display for BackendAdapterId {
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

/// Verified prompt-cache continuity for one backend binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheDomain {
    id: &'static str,
    ttl_ms: i64,
}

impl CacheDomain {
    pub const fn new(id: &'static str, ttl_ms: i64) -> CacheDomain {
        CacheDomain { id, ttl_ms }
    }

    pub const fn id(self) -> &'static str {
        self.id
    }

    pub const fn ttl_ms(self) -> i64 {
        self.ttl_ms
    }
}

/// One native wire exposed by a provider.
///
/// A binding is a verified declaration, not an inference from an endpoint's
/// spelling. Its id is closed static topology, while provider configuration is
/// resolved separately by [`RouteRegistry`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BackendBinding {
    id: BindingId,
    provider: &'static str,
    protocol: ProtocolId,
    dialect: DialectId,
    cost_class: CostClass,
    cache_domain: Option<CacheDomain>,
    canonical: Option<CanonicalBackend>,
    administrative: AdministrativeCapabilities,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct AdministrativeCapabilities {
    pub count_tokens: bool,
    pub batches: bool,
}

impl AdministrativeCapabilities {
    pub const MESSAGES: Self = Self {
        count_tokens: true,
        batches: true,
    };

    pub const OPENROUTER_MESSAGES: Self = Self {
        count_tokens: false,
        batches: false,
    };
}

impl BackendBinding {
    /// Declare a live wire binding whose universal canonical egress adapter
    /// has not been completed yet.
    pub const fn declared(
        id: &'static str,
        provider: &'static str,
        protocol: ProtocolId,
        dialect: DialectId,
        cost_class: CostClass,
        cache_domain: Option<CacheDomain>,
    ) -> BackendBinding {
        BackendBinding {
            id: BindingId::from_static(id),
            provider,
            protocol,
            dialect,
            cost_class,
            cache_domain,
            canonical: None,
            administrative: AdministrativeCapabilities {
                count_tokens: false,
                batches: false,
            },
        }
    }

    /// Declare a binding with a verified canonical egress adapter.
    #[allow(clippy::too_many_arguments)]
    pub const fn canonical(
        id: &'static str,
        provider: &'static str,
        protocol: ProtocolId,
        dialect: DialectId,
        cost_class: CostClass,
        cache_domain: Option<CacheDomain>,
        adapter: BackendAdapterId,
        capabilities: Capabilities,
        administrative: AdministrativeCapabilities,
    ) -> BackendBinding {
        BackendBinding {
            id: BindingId::from_static(id),
            provider,
            protocol,
            dialect,
            cost_class,
            cache_domain,
            canonical: Some(CanonicalBackend {
                adapter,
                capabilities,
            }),
            administrative,
        }
    }

    pub fn id(&self) -> &BindingId {
        &self.id
    }

    pub fn id_str(&self) -> &str {
        self.id.as_str()
    }

    pub const fn provider(&self) -> &'static str {
        self.provider
    }

    pub const fn protocol(&self) -> ProtocolId {
        self.protocol
    }

    pub const fn dialect(&self) -> DialectId {
        self.dialect
    }

    pub const fn cost_class(&self) -> CostClass {
        self.cost_class
    }

    pub const fn cache_domain(&self) -> Option<CacheDomain> {
        self.cache_domain
    }

    /// The canonical implementation and verified capabilities, once this
    /// binding can participate in the universal pipeline.
    pub const fn canonical_backend(&self) -> Option<CanonicalBackend> {
        self.canonical
    }

    pub const fn administrative_capabilities(&self) -> AdministrativeCapabilities {
        self.administrative
    }
}

pub const ANTHROPIC_SUB_MESSAGES: BackendBinding = BackendBinding::canonical(
    "anthropic_sub.anthropic_messages",
    "anthropic_sub",
    ProtocolId::AnthropicMessages,
    DialectId::AnthropicMessages,
    CostClass::Subscription,
    None,
    BackendAdapterId::AnthropicMessages,
    Capabilities::MESSAGES,
    AdministrativeCapabilities::MESSAGES,
);

pub const ANTHROPIC_API_MESSAGES: BackendBinding = BackendBinding::canonical(
    "anthropic_api.anthropic_messages",
    "anthropic_api",
    ProtocolId::AnthropicMessages,
    DialectId::AnthropicMessages,
    CostClass::Billed,
    None,
    BackendAdapterId::AnthropicMessages,
    Capabilities::MESSAGES,
    AdministrativeCapabilities::MESSAGES,
);

pub const OPENAI_API_CHAT: BackendBinding = BackendBinding::canonical(
    "openai_api.openai_chat_completions",
    "openai_api",
    ProtocolId::OpenAiChat,
    DialectId::OpenAiChatCompletions,
    CostClass::Billed,
    None,
    BackendAdapterId::OpenAiChatCompletions,
    Capabilities::OPENAI_CHAT,
    AdministrativeCapabilities {
        count_tokens: false,
        batches: false,
    },
);

pub const OPENROUTER_CHAT: BackendBinding = BackendBinding::canonical(
    "openrouter.openrouter_chat_completions",
    "openrouter",
    ProtocolId::OpenAiChat,
    DialectId::OpenRouterChatCompletions,
    CostClass::Billed,
    Some(CacheDomain::new("openrouter.chat", 10 * 60 * 1000)),
    BackendAdapterId::OpenAiChatCompletions,
    Capabilities::CHAT,
    AdministrativeCapabilities {
        count_tokens: false,
        batches: false,
    },
);

pub const OPENROUTER_MESSAGES: BackendBinding = BackendBinding::canonical(
    "openrouter.openrouter_messages",
    "openrouter",
    ProtocolId::AnthropicMessages,
    DialectId::OpenRouterMessages,
    CostClass::Billed,
    None,
    BackendAdapterId::AnthropicMessages,
    Capabilities::MESSAGES,
    AdministrativeCapabilities::OPENROUTER_MESSAGES,
);

pub const OPENROUTER_RESPONSES: BackendBinding = BackendBinding::canonical(
    "openrouter.openrouter_responses",
    "openrouter",
    ProtocolId::OpenAiResponses,
    DialectId::OpenRouterResponses,
    CostClass::Billed,
    None,
    BackendAdapterId::OpenRouterResponses,
    Capabilities::OPENROUTER_RESPONSES,
    AdministrativeCapabilities {
        count_tokens: false,
        batches: false,
    },
);

pub const CODEX_SUB_RESPONSES: BackendBinding = BackendBinding::canonical(
    "codex_sub.codex_responses",
    "codex_sub",
    ProtocolId::OpenAiResponses,
    DialectId::CodexResponses,
    CostClass::Subscription,
    None,
    BackendAdapterId::CodexResponses,
    Capabilities::CODEX,
    AdministrativeCapabilities {
        count_tokens: false,
        batches: false,
    },
);

pub const BACKEND_BINDINGS: &[BackendBinding] = &[
    ANTHROPIC_SUB_MESSAGES,
    ANTHROPIC_API_MESSAGES,
    OPENAI_API_CHAT,
    OPENROUTER_CHAT,
    OPENROUTER_MESSAGES,
    OPENROUTER_RESPONSES,
    CODEX_SUB_RESPONSES,
];

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

/// One implemented path through the canonical adapter graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDeclaration {
    frontend: ProtocolId,
    binding: BackendBinding,
}

impl RouteDeclaration {
    pub const fn frontend(&self) -> ProtocolId {
        self.frontend
    }

    pub const fn binding(&self) -> &BackendBinding {
        &self.binding
    }
}

pub const ROUTES: &[RouteDeclaration] = &[
    RouteDeclaration {
        frontend: ProtocolId::AnthropicMessages,
        binding: ANTHROPIC_SUB_MESSAGES,
    },
    RouteDeclaration {
        frontend: ProtocolId::AnthropicMessages,
        binding: ANTHROPIC_API_MESSAGES,
    },
    RouteDeclaration {
        frontend: ProtocolId::AnthropicMessages,
        binding: OPENROUTER_MESSAGES,
    },
    RouteDeclaration {
        frontend: ProtocolId::AnthropicMessages,
        binding: CODEX_SUB_RESPONSES,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiChat,
        binding: OPENROUTER_CHAT,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiChat,
        binding: OPENAI_API_CHAT,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiChat,
        binding: CODEX_SUB_RESPONSES,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiChat,
        binding: ANTHROPIC_API_MESSAGES,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiChat,
        binding: ANTHROPIC_SUB_MESSAGES,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiResponses,
        binding: CODEX_SUB_RESPONSES,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiResponses,
        binding: ANTHROPIC_API_MESSAGES,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiResponses,
        binding: ANTHROPIC_SUB_MESSAGES,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiResponses,
        binding: OPENROUTER_RESPONSES,
    },
];

/// A fully resolved inference destination.
///
/// The provider owns endpoint, credential, meter, and cost behavior. The
/// binding chooses the backend wire adapter. Model identities remain separate:
/// `requested_model` is the frontend spelling, while `effective_model` has
/// only toker's outer routing prefix removed.
#[derive(Clone)]
pub struct ModelTarget {
    provider: Arc<dyn Provider>,
    binding: BackendBinding,
    requested_model: Option<String>,
    effective_model: Option<String>,
}

impl ModelTarget {
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    pub const fn binding(&self) -> &BackendBinding {
        &self.binding
    }

    pub fn requested_model(&self) -> Option<&str> {
        self.requested_model.as_deref()
    }

    pub fn effective_model(&self) -> Option<&str> {
        self.effective_model.as_deref()
    }
}

/// Failure to resolve an implemented route because its configured provider is
/// absent. Handlers render this in the frontend protocol's own error shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteNotConfigured;

/// The enabled providers and defaults projected onto the implemented route
/// graph. Handlers resolve through this registry instead of maintaining their
/// own provider-prefix tables.
#[derive(Clone)]
pub struct RouteRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
    defaults: HashMap<ProtocolId, String>,
    /// User-side policy publication needs the static topology and an inventory
    /// from cached catalogues, but must neither initialise providers nor read
    /// credentials. Normal registries leave this empty.
    configured_binding_ids: HashSet<BindingId>,
}

impl RouteRegistry {
    pub fn new(
        providers: impl IntoIterator<Item = Arc<dyn Provider>>,
        defaults: impl IntoIterator<Item = (ProtocolId, String)>,
    ) -> RouteRegistry {
        RouteRegistry {
            providers: providers
                .into_iter()
                .map(|provider| (provider.id().to_owned(), provider))
                .collect(),
            defaults: defaults.into_iter().collect(),
            configured_binding_ids: HashSet::new(),
        }
    }

    /// Build a credential-free topology view for user-side publication. This
    /// deliberately cannot resolve a request, only declare which bindings the
    /// loaded configuration has enabled.
    pub fn configured_bindings(binding_ids: impl IntoIterator<Item = BindingId>) -> RouteRegistry {
        RouteRegistry {
            providers: HashMap::new(),
            defaults: HashMap::new(),
            configured_binding_ids: binding_ids.into_iter().collect(),
        }
    }

    pub fn provider(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.get(id)
    }

    /// Every declared backend binding, regardless of configured providers.
    pub fn bindings(&self) -> impl ExactSizeIterator<Item = &BackendBinding> + '_ {
        BACKEND_BINDINGS.iter()
    }

    pub fn binding(&self, id: &BindingId) -> Option<&'static BackendBinding> {
        BACKEND_BINDINGS
            .iter()
            .find(|binding| binding.id_str() == id.as_str())
    }

    /// Every implemented frontend-to-binding edge, regardless of configured
    /// providers.
    pub fn routes(&self) -> impl ExactSizeIterator<Item = &RouteDeclaration> + '_ {
        ROUTES.iter()
    }

    pub fn provider_is_configured(&self, provider: &str) -> bool {
        self.providers.contains_key(provider)
    }

    pub fn binding_is_configured(&self, binding: &BackendBinding) -> bool {
        self.configured_binding_ids.contains(binding.id())
            || self
                .providers
                .get(binding.provider())
                .is_some_and(|provider| {
                    provider.bindings().iter().any(|implemented| {
                        implemented == binding && implemented.canonical_backend().is_some()
                    })
                })
    }

    /// Build a configured target from a policy-selected binding and model.
    ///
    /// Static topology alone is not enough to send: the frontend edge must be
    /// implemented, and the configured provider must expose that exact verified
    /// binding.
    pub fn resolve_binding(
        &self,
        frontend: ProtocolId,
        binding_id: &BindingId,
        requested_model: Option<&str>,
        effective_model: &str,
    ) -> Result<ModelTarget, RouteNotConfigured> {
        let declaration = ROUTES
            .iter()
            .find(|route| route.frontend == frontend && route.binding.id() == binding_id)
            .ok_or(RouteNotConfigured)?;
        let provider = self
            .providers
            .get(declaration.binding.provider())
            .cloned()
            .ok_or(RouteNotConfigured)?;
        let binding = provider
            .bindings()
            .iter()
            .find(|binding| {
                **binding == declaration.binding && binding.canonical_backend().is_some()
            })
            .cloned()
            .ok_or(RouteNotConfigured)?;
        Ok(ModelTarget {
            provider,
            binding,
            requested_model: requested_model.map(str::to_owned),
            effective_model: Some(effective_model.to_owned()),
        })
    }

    /// Resolve a frontend model spelling to its provider, backend binding, and
    /// two model identities. Unknown prefixes are provider-owned model ids and
    /// therefore remain intact on the protocol default.
    pub fn resolve(
        &self,
        frontend: ProtocolId,
        requested_model: Option<&str>,
    ) -> Result<ModelTarget, RouteNotConfigured> {
        let explicit = requested_model.and_then(|model| {
            ROUTES
                .iter()
                .filter(|route| route.frontend == frontend)
                .find_map(|route| {
                    model
                        .strip_prefix(route.binding.provider())
                        .and_then(|rest| rest.strip_prefix('/'))
                        .map(|rest| (route, rest))
                })
        });
        let protocol_alias = (frontend == ProtocolId::AnthropicMessages)
            .then_some(requested_model)
            .flatten()
            .and_then(|model| model.strip_prefix("anthropic/"));

        let (provider_id, effective_model) = if let Some((route, rest)) = explicit {
            (route.binding.provider(), Some(rest))
        } else {
            let provider = self.defaults.get(&frontend).ok_or(RouteNotConfigured)?;
            (provider.as_str(), protocol_alias.or(requested_model))
        };
        let declaration = ROUTES
            .iter()
            .find(|route| route.frontend == frontend && route.binding.provider() == provider_id)
            .ok_or(RouteNotConfigured)?;
        let provider = self
            .providers
            .get(declaration.binding.provider())
            .cloned()
            .ok_or(RouteNotConfigured)?;
        let binding = provider
            .bindings()
            .iter()
            .find(|binding| {
                **binding == declaration.binding && binding.canonical_backend().is_some()
            })
            .cloned()
            .ok_or(RouteNotConfigured)?;
        Ok(ModelTarget {
            provider,
            binding,
            requested_model: requested_model.map(str::to_owned),
            effective_model: effective_model.map(str::to_owned),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::HeaderMap;
    use reqwest::Url;

    use super::{
        ANTHROPIC_SUB_MESSAGES, AdministrativeCapabilities, BACKEND_BINDINGS, BackendAdapterId,
        BackendBinding, Capabilities, DialectId, FrontendProfile, OPENAI_API_CHAT, OPENROUTER_CHAT,
        OPENROUTER_MESSAGES, ProtocolId, ROUTES, RouteRegistry,
    };
    use crate::policy::CostClass;
    use crate::providers::Provider;

    struct FakeProvider {
        id: &'static str,
        bindings: &'static [BackendBinding],
    }

    impl Provider for FakeProvider {
        fn id(&self) -> &str {
            self.id
        }

        fn bindings(&self) -> &'static [BackendBinding] {
            self.bindings
        }

        fn endpoint(&self, _path: &str) -> Url {
            "http://127.0.0.1:9".parse().expect("fixture URL")
        }

        fn inject_auth(&self, _outgoing: &mut HeaderMap) {}
    }

    const MESSAGES: &[BackendBinding] = &[ANTHROPIC_SUB_MESSAGES];
    const OPENROUTER_MESSAGES_ONLY: &[BackendBinding] = &[OPENROUTER_MESSAGES];
    const OPENROUTER: &[BackendBinding] = &[OPENROUTER_CHAT, OPENROUTER_MESSAGES];

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
        let pending = BackendBinding::declared(
            "openrouter.openrouter_messages",
            "openrouter",
            ProtocolId::AnthropicMessages,
            DialectId::OpenRouterMessages,
            CostClass::Billed,
            None,
        );
        assert_eq!(pending.protocol(), ProtocolId::AnthropicMessages);
        assert_eq!(pending.dialect(), DialectId::OpenRouterMessages);
        assert_eq!(pending.canonical_backend(), None);

        let ready = BackendBinding::canonical(
            "codex_sub.codex_responses",
            "codex_sub",
            ProtocolId::OpenAiResponses,
            DialectId::CodexResponses,
            CostClass::Subscription,
            None,
            BackendAdapterId::CodexResponses,
            Capabilities::CODEX,
            AdministrativeCapabilities::default(),
        );
        let canonical = ready.canonical_backend().expect("adapter is verified");
        assert_eq!(canonical.adapter(), BackendAdapterId::CodexResponses);
        assert_eq!(canonical.capabilities(), Capabilities::CODEX);
        assert!(!ready.administrative_capabilities().count_tokens);
        assert!(!ready.administrative_capabilities().batches);
        assert!(
            ANTHROPIC_SUB_MESSAGES
                .administrative_capabilities()
                .count_tokens
        );
        assert!(ANTHROPIC_SUB_MESSAGES.administrative_capabilities().batches);
        assert!(
            !OPENROUTER_MESSAGES
                .administrative_capabilities()
                .count_tokens
        );
        assert!(!OPENROUTER_MESSAGES.administrative_capabilities().batches);
    }

    #[test]
    fn tool_choice_capabilities_match_backend_renderers() {
        use super::CapabilityEvidence::{Supported, Unknown, Unsupported};

        let cases = [
            (
                BackendAdapterId::AnthropicMessages,
                Capabilities::MESSAGES,
                (Unsupported, Supported, Supported, Unknown),
            ),
            (
                BackendAdapterId::OpenAiChatCompletions,
                Capabilities::CHAT,
                (Supported, Supported, Unsupported, Unknown),
            ),
            (
                BackendAdapterId::CodexResponses,
                Capabilities::CODEX,
                (Supported, Unsupported, Unsupported, Unsupported),
            ),
            (
                BackendAdapterId::OpenRouterResponses,
                Capabilities::OPENROUTER_RESPONSES,
                (Supported, Unsupported, Unsupported, Unsupported),
            ),
        ];
        for (adapter, capabilities, expected) in cases {
            assert_eq!(
                (
                    capabilities.tool_choice_none,
                    capabilities.tool_choice_named,
                    capabilities.tool_choice_unnamed,
                    capabilities.tool_choice_other,
                ),
                expected,
                "{adapter}"
            );
        }
    }

    #[test]
    fn configured_provider_must_expose_the_exact_verified_binding() {
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider {
            id: "openrouter",
            bindings: OPENROUTER_MESSAGES_ONLY,
        });
        let registry = RouteRegistry::new([provider], []);
        assert!(registry.binding_is_configured(&OPENROUTER_MESSAGES));
        assert!(
            !registry.binding_is_configured(&OPENROUTER_CHAT),
            "provider id alone does not verify another declared binding"
        );
    }

    #[test]
    fn stable_wire_identities_round_trip_and_reject_unknown_spellings() {
        for (value, protocol) in [
            ("anthropic", ProtocolId::AnthropicMessages),
            ("openai_chat", ProtocolId::OpenAiChat),
            ("openai_responses", ProtocolId::OpenAiResponses),
        ] {
            assert_eq!(
                serde_json::to_string(&protocol).unwrap(),
                format!("\"{value}\"")
            );
            assert_eq!(
                serde_json::from_str::<ProtocolId>(&format!("\"{value}\"")).unwrap(),
                protocol
            );
        }
        for (value, dialect) in [
            ("anthropic_messages", DialectId::AnthropicMessages),
            ("openai_chat_completions", DialectId::OpenAiChatCompletions),
            ("openrouter_messages", DialectId::OpenRouterMessages),
            (
                "openrouter_chat_completions",
                DialectId::OpenRouterChatCompletions,
            ),
            ("openrouter_responses", DialectId::OpenRouterResponses),
            ("codex_responses", DialectId::CodexResponses),
        ] {
            assert_eq!(
                serde_json::to_string(&dialect).unwrap(),
                format!("\"{value}\"")
            );
            assert_eq!(
                serde_json::from_str::<DialectId>(&format!("\"{value}\"")).unwrap(),
                dialect
            );
        }
        for (value, adapter) in [
            ("anthropic_messages", BackendAdapterId::AnthropicMessages),
            (
                "openai_chat_completions",
                BackendAdapterId::OpenAiChatCompletions,
            ),
            ("codex_responses", BackendAdapterId::CodexResponses),
            (
                "openrouter_responses",
                BackendAdapterId::OpenRouterResponses,
            ),
        ] {
            assert_eq!(
                serde_json::to_string(&adapter).unwrap(),
                format!("\"{value}\"")
            );
            assert_eq!(
                serde_json::from_str::<BackendAdapterId>(&format!("\"{value}\"")).unwrap(),
                adapter
            );
        }
        assert!(serde_json::from_str::<ProtocolId>(r#""unknown""#).is_err());
        assert!(serde_json::from_str::<DialectId>(r#""unknown""#).is_err());
        assert!(serde_json::from_str::<BackendAdapterId>(r#""unknown""#).is_err());
    }

    #[test]
    fn topology_is_static_and_configuration_is_separate() {
        let registry = RouteRegistry::new(
            [Arc::new(FakeProvider {
                id: "anthropic_sub",
                bindings: MESSAGES,
            }) as Arc<dyn Provider>],
            [],
        );
        assert_eq!(registry.bindings().count(), BACKEND_BINDINGS.len());
        assert_eq!(registry.routes().count(), ROUTES.len());
        assert!(registry.provider_is_configured("anthropic_sub"));
        assert!(!registry.provider_is_configured("openrouter"));
        assert!(registry.binding_is_configured(&ANTHROPIC_SUB_MESSAGES));
        assert!(!registry.binding_is_configured(&OPENROUTER_MESSAGES));
        assert!(registry.routes().any(|route| {
            route.frontend() == ProtocolId::OpenAiChat
                && route.binding().id_str() == "openrouter.openrouter_chat_completions"
        }));
        assert!(registry.binding(OPENROUTER_MESSAGES.id()).is_some());
    }

    #[test]
    fn declared_bindings_pin_identity_cost_adapter_capabilities_and_cache_evidence() {
        let expected = [
            (
                "anthropic_sub.anthropic_messages",
                CostClass::Subscription,
                ProtocolId::AnthropicMessages,
                DialectId::AnthropicMessages,
                BackendAdapterId::AnthropicMessages,
                Capabilities::MESSAGES,
                None,
            ),
            (
                "anthropic_api.anthropic_messages",
                CostClass::Billed,
                ProtocolId::AnthropicMessages,
                DialectId::AnthropicMessages,
                BackendAdapterId::AnthropicMessages,
                Capabilities::MESSAGES,
                None,
            ),
            (
                "openai_api.openai_chat_completions",
                CostClass::Billed,
                ProtocolId::OpenAiChat,
                DialectId::OpenAiChatCompletions,
                BackendAdapterId::OpenAiChatCompletions,
                Capabilities::OPENAI_CHAT,
                None,
            ),
            (
                "openrouter.openrouter_chat_completions",
                CostClass::Billed,
                ProtocolId::OpenAiChat,
                DialectId::OpenRouterChatCompletions,
                BackendAdapterId::OpenAiChatCompletions,
                Capabilities::CHAT,
                Some(("openrouter.chat", 600_000)),
            ),
            (
                "openrouter.openrouter_messages",
                CostClass::Billed,
                ProtocolId::AnthropicMessages,
                DialectId::OpenRouterMessages,
                BackendAdapterId::AnthropicMessages,
                Capabilities::MESSAGES,
                None,
            ),
            (
                "openrouter.openrouter_responses",
                CostClass::Billed,
                ProtocolId::OpenAiResponses,
                DialectId::OpenRouterResponses,
                BackendAdapterId::OpenRouterResponses,
                Capabilities::OPENROUTER_RESPONSES,
                None,
            ),
            (
                "codex_sub.codex_responses",
                CostClass::Subscription,
                ProtocolId::OpenAiResponses,
                DialectId::CodexResponses,
                BackendAdapterId::CodexResponses,
                Capabilities::CODEX,
                None,
            ),
        ];
        assert_eq!(BACKEND_BINDINGS.len(), expected.len());
        for (binding, (id, cost, protocol, dialect, adapter, capabilities, cache)) in
            BACKEND_BINDINGS.iter().zip(expected)
        {
            assert_eq!(binding.id_str(), id);
            assert_eq!(binding.cost_class(), cost);
            assert_eq!(binding.protocol(), protocol);
            assert_eq!(binding.dialect(), dialect);
            let canonical = binding.canonical_backend().expect("implemented adapter");
            assert_eq!(canonical.adapter(), adapter);
            assert_eq!(canonical.capabilities(), capabilities);
            assert_eq!(
                binding
                    .cache_domain()
                    .map(|domain| (domain.id(), domain.ttl_ms())),
                cache
            );
        }
    }

    #[test]
    fn the_registry_resolves_provider_binding_and_both_model_identities() {
        let registry = RouteRegistry::new(
            [
                Arc::new(FakeProvider {
                    id: "anthropic_sub",
                    bindings: MESSAGES,
                }) as Arc<dyn Provider>,
                Arc::new(FakeProvider {
                    id: "openrouter",
                    bindings: OPENROUTER,
                }),
            ],
            [
                (ProtocolId::AnthropicMessages, "anthropic_sub".to_owned()),
                (ProtocolId::OpenAiChat, "openrouter".to_owned()),
            ],
        );

        let target = registry
            .resolve(
                ProtocolId::AnthropicMessages,
                Some("openrouter/openai/gpt-5.2"),
            )
            .expect("implemented route");
        assert_eq!(target.provider().id(), "openrouter");
        assert_eq!(target.binding().dialect(), DialectId::OpenRouterMessages);
        assert_eq!(target.requested_model(), Some("openrouter/openai/gpt-5.2"));
        assert_eq!(target.effective_model(), Some("openai/gpt-5.2"));

        let selected = registry
            .resolve_binding(
                ProtocolId::AnthropicMessages,
                OPENROUTER_MESSAGES.id(),
                Some("presented-model"),
                "openai/gpt-5.3",
            )
            .expect("policy-selected route");
        assert_eq!(selected.provider().id(), "openrouter");
        assert_eq!(selected.binding(), &OPENROUTER_MESSAGES);
        assert_eq!(selected.requested_model(), Some("presented-model"));
        assert_eq!(selected.effective_model(), Some("openai/gpt-5.3"));

        let alias = registry
            .resolve(
                ProtocolId::AnthropicMessages,
                Some("anthropic/claude-opus-5"),
            )
            .expect("protocol alias");
        assert_eq!(alias.provider().id(), "anthropic_sub");
        assert_eq!(alias.effective_model(), Some("claude-opus-5"));

        let provider_owned = registry
            .resolve(ProtocolId::OpenAiChat, Some("anthropic/claude-opus-5"))
            .expect("unknown prefix belongs to the model id");
        assert_eq!(provider_owned.provider().id(), "openrouter");
        assert_eq!(
            provider_owned.effective_model(),
            Some("anthropic/claude-opus-5")
        );
    }

    #[test]
    fn an_explicit_but_disabled_provider_is_not_sent_to_the_default() {
        let registry = RouteRegistry::new(
            [Arc::new(FakeProvider {
                id: "anthropic_sub",
                bindings: MESSAGES,
            }) as Arc<dyn Provider>],
            [(ProtocolId::AnthropicMessages, "anthropic_sub".to_owned())],
        );

        assert!(
            registry
                .resolve(
                    ProtocolId::AnthropicMessages,
                    Some("openrouter/z-ai/glm-5.3")
                )
                .is_err()
        );
        assert!(
            registry
                .resolve_binding(
                    ProtocolId::AnthropicMessages,
                    OPENROUTER_MESSAGES.id(),
                    Some("presented-model"),
                    "z-ai/glm-5.3",
                )
                .is_err(),
            "policy cannot select a binding whose provider is disabled"
        );
        assert!(
            registry
                .resolve_binding(
                    ProtocolId::OpenAiResponses,
                    OPENAI_API_CHAT.id(),
                    Some("presented-model"),
                    "gpt-5.3",
                )
                .is_err(),
            "policy cannot select a binding without a frontend edge"
        );
    }
}
