//! The identities in toker's protocol/provider route graph.
//!
//! Protocol identities, implemented adapter paths, and the shared model-target
//! resolver from `docs/plans/protocol-provider-mux.md`. Protocol identities are
//! data; frontend and backend adapter behavior remains in typed modules.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::providers::Provider;

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

/// One implemented path through the canonical adapter graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RouteDeclaration {
    frontend: ProtocolId,
    provider: &'static str,
    backend: ProtocolId,
}

const ROUTES: &[RouteDeclaration] = &[
    RouteDeclaration {
        frontend: ProtocolId::AnthropicMessages,
        provider: "anthropic_sub",
        backend: ProtocolId::AnthropicMessages,
    },
    RouteDeclaration {
        frontend: ProtocolId::AnthropicMessages,
        provider: "anthropic_api",
        backend: ProtocolId::AnthropicMessages,
    },
    RouteDeclaration {
        frontend: ProtocolId::AnthropicMessages,
        provider: "openrouter",
        backend: ProtocolId::AnthropicMessages,
    },
    RouteDeclaration {
        frontend: ProtocolId::AnthropicMessages,
        provider: "codex_sub",
        backend: ProtocolId::OpenAiResponses,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiChat,
        provider: "openrouter",
        backend: ProtocolId::OpenAiChat,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiChat,
        provider: "codex_sub",
        backend: ProtocolId::OpenAiResponses,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiChat,
        provider: "anthropic_api",
        backend: ProtocolId::AnthropicMessages,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiChat,
        provider: "anthropic_sub",
        backend: ProtocolId::AnthropicMessages,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiResponses,
        provider: "codex_sub",
        backend: ProtocolId::OpenAiResponses,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiResponses,
        provider: "anthropic_api",
        backend: ProtocolId::AnthropicMessages,
    },
    RouteDeclaration {
        frontend: ProtocolId::OpenAiResponses,
        provider: "anthropic_sub",
        backend: ProtocolId::AnthropicMessages,
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

    pub const fn binding(&self) -> BackendBinding {
        self.binding
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
        }
    }

    pub fn provider(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.get(id)
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
                        .strip_prefix(route.provider)
                        .and_then(|rest| rest.strip_prefix('/'))
                        .map(|rest| (*route, rest))
                })
        });
        let protocol_alias = (frontend == ProtocolId::AnthropicMessages)
            .then_some(requested_model)
            .flatten()
            .and_then(|model| model.strip_prefix("anthropic/"));

        let (provider_id, effective_model) = if let Some((route, rest)) = explicit {
            (route.provider, Some(rest))
        } else {
            let provider = self.defaults.get(&frontend).ok_or(RouteNotConfigured)?;
            (provider.as_str(), protocol_alias.or(requested_model))
        };
        let declaration = ROUTES
            .iter()
            .find(|route| route.frontend == frontend && route.provider == provider_id)
            .ok_or(RouteNotConfigured)?;
        let provider = self
            .providers
            .get(declaration.provider)
            .cloned()
            .ok_or(RouteNotConfigured)?;
        let binding = provider
            .bindings()
            .iter()
            .copied()
            .find(|binding| {
                binding.protocol() == declaration.backend && binding.canonical_backend().is_some()
            })
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

    use super::Capabilities;
    use super::{
        BackendAdapterId, BackendBinding, DialectId, FrontendProfile, ProtocolId, RouteRegistry,
    };
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

    const MESSAGES: &[BackendBinding] = &[BackendBinding::canonical(
        ProtocolId::AnthropicMessages,
        DialectId::AnthropicMessages,
        BackendAdapterId::AnthropicMessages,
        Capabilities::MESSAGES,
    )];
    const OPENROUTER: &[BackendBinding] = &[
        BackendBinding::canonical(
            ProtocolId::OpenAiChat,
            DialectId::OpenRouterChatCompletions,
            BackendAdapterId::OpenAiChatCompletions,
            Capabilities::CHAT,
        ),
        BackendBinding::canonical(
            ProtocolId::AnthropicMessages,
            DialectId::OpenRouterMessages,
            BackendAdapterId::AnthropicMessages,
            Capabilities::MESSAGES,
        ),
    ];

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
    }
}
