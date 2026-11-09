//! Pure conversion of resolved legacy configuration into revision-one policy.
//!
//! This module translates routing vocabulary only. It does not persist a
//! policy or consult the ledger.

use std::collections::BTreeMap;

use anyhow::Context;

use crate::catalog::offers::{self, OfferInventory};
use crate::config::Config;
use crate::middleware::model_map::ModelMap;
use crate::picker::{PickerRule, default_rules};
use crate::routing::{BackendBinding, ProtocolId, RouteRegistry};

use super::{
    AdapterMetadata, AliasPolicy, AliasRule, ArgumentBinding, CURRENT_FORMAT_VERSION,
    CacheTransitionPolicy, Candidate, CandidateId, CandidateModel, CompactionPolicy,
    CompactionTarget, Compilation, DefaultRoute, FamilyClassifier, FrontendProtocol, FrontendScope,
    InventorySource, LaneTransitionPolicy, MatcherSpec, MinimumLearning, OfferSelector,
    PolicyDocument, PolicyOrigin, Presentation, PresentationId, PresentationSource, ProfileId,
    REQUESTED_MODEL_PARAMETER, RewriteId, RewriteRule, Route, RouteChain, RouteId, RouteReference,
    SelectorOrdering, StableId, UpgradePolicy, UpgradePolicyId, VersionParserSpec,
};

const LEGACY_FAMILY_CLASSIFIER: &str = "legacy-family";
const LEGACY_UPGRADE_POLICY: &str = "legacy-force-newest";
const REQUESTED_ROUTE_PARAMETER: &str = REQUESTED_MODEL_PARAMETER;

/// Build the inventory-independent revision-one routing document from config.
pub fn document(config: &Config, topology: &RouteRegistry) -> anyhow::Result<PolicyDocument> {
    LegacyBuilder::new(config, topology).build()
}

/// Build and validate the inventory-independent revision-one routing document.
pub fn validated(
    config: &Config,
    topology: &RouteRegistry,
) -> anyhow::Result<super::ValidatedPolicyDocument> {
    document(config, topology)?
        .validate()
        .context("validating generated legacy policy")
}

/// Build, validate, and compile revision one without persisting it.
pub fn compile(
    config: &Config,
    topology: &RouteRegistry,
    inventory: &OfferInventory,
) -> anyhow::Result<Compilation> {
    Ok(compile_document(
        validated(config, topology)?,
        topology,
        inventory,
    ))
}

/// Compile and activate revision one from the legacy config, or load the
/// revision that is already active without consulting the config. Startup and
/// setup both go through here, so a machine setup activates is the machine the
/// service would have activated itself.
pub fn establish_revision_one(
    store: &crate::store::Store,
    config: &Config,
    topology: &RouteRegistry,
    inventory: &OfferInventory,
    publication_generation: i64,
    activated_ms: i64,
    configuration: Option<&crate::store::Configuration>,
) -> anyhow::Result<(crate::store::PolicyRevision, super::CompiledPolicy)> {
    config.validate()?;
    let document = validated(config, topology)?;
    let generated = compile_document(document.clone(), topology, inventory)
        .into_policy()
        .map_err(|diagnostics| {
            anyhow::anyhow!(
                "generated revision one has {} compile diagnostics",
                diagnostics.len()
            )
        })?;
    let activation = store.activate_revision_one_importing(
        &document,
        activated_ms,
        publication_generation,
        &crate::store::publication_targets()?,
        configuration,
    )?;
    match activation {
        crate::store::RevisionOneActivation::Activated { revision, .. } => {
            Ok((revision, generated))
        }
        crate::store::RevisionOneActivation::AlreadyActive { revision, .. } => {
            let policy = super::runtime::compile_stored_revision(&revision, topology, inventory)
                .into_policy()
                .map_err(|diagnostics| {
                    anyhow::anyhow!(
                        "active policy revision {} has {} compile diagnostics",
                        revision.revision_id,
                        diagnostics.len()
                    )
                })?;
            Ok((revision, policy))
        }
    }
}

pub fn compile_document(
    document: super::ValidatedPolicyDocument,
    topology: &RouteRegistry,
    inventory: &OfferInventory,
) -> Compilation {
    // The bare-slug half of the listing follows the document's own protocol
    // default, so a stored revision lists what it routes rather than what
    // the config says today.
    let defaults: Vec<_> = [ProtocolId::OpenAiChat, ProtocolId::OpenAiResponses]
        .into_iter()
        .map(|protocol| {
            let provider = default_provider(document.document(), topology, protocol);
            (protocol, provider)
        })
        .collect();
    let mut compilation = super::compile(
        document,
        topology,
        inventory,
        PolicyOrigin::GeneratedRevisionOne,
    );
    if let Some(policy) = compilation.policy_mut() {
        for (protocol, default_provider) in defaults {
            policy.add_generated_publication(
                protocol.into(),
                offers::for_frontend_inventory(
                    topology,
                    inventory,
                    protocol,
                    default_provider.as_deref(),
                )
                .into_iter()
                .map(|offer| {
                    (
                        offer.routed_model_id,
                        offer.binding.id().clone(),
                        offer.backend_model_id,
                    )
                }),
            );
        }
    }
    compilation
}

/// The provider a generated document defaults `protocol` to, if any.
fn default_provider(
    document: &PolicyDocument,
    topology: &RouteRegistry,
    protocol: ProtocolId,
) -> Option<String> {
    let scope = FrontendScope::Protocol {
        protocol: protocol.into(),
    };
    let default = document
        .defaults
        .iter()
        .find(|default| default.scope == scope)?;
    topology
        .routes()
        .filter(|route| route.frontend() == protocol)
        .find(|route| {
            backend_route_id(route.binding()).is_ok_and(|id| id == default.target.route_id)
        })
        .map(|route| route.binding().provider().to_owned())
}

struct LegacyBuilder<'a> {
    config: &'a Config,
    topology: &'a RouteRegistry,
    policy: PolicyDocument,
}

impl<'a> LegacyBuilder<'a> {
    fn new(config: &'a Config, topology: &'a RouteRegistry) -> Self {
        Self {
            config,
            topology,
            policy: PolicyDocument {
                format_version: CURRENT_FORMAT_VERSION,
                description: Some("Generated revision-one legacy routing".to_owned()),
                defaults: vec![],
                presentations: vec![],
                routes: vec![],
                rewrites: vec![],
                classifiers: vec![],
                upgrades: vec![],
                compaction: None,
            },
        }
    }

    fn build(mut self) -> anyhow::Result<PolicyDocument> {
        self.add_family_policy()?;
        self.add_backend_routes()?;
        self.add_defaults()?;
        self.add_explicit_prefixes()?;
        self.add_anthropic_alias()?;
        self.add_picker()?;
        self.add_compaction()?;
        Ok(self.policy)
    }

    fn add_family_policy(&mut self) -> anyhow::Result<()> {
        for binding in self.topology.bindings() {
            if !supports_legacy_families(binding) {
                continue;
            }
            let classifier_id = classifier_id(binding)?;
            self.policy.classifiers.push(FamilyClassifier {
                id: classifier_id.clone(),
                binding_id: binding.id().clone(),
                // The generated classifier intentionally reproduces the
                // legacy family split only for known hyphenated ids. An
                // unpublished eight-digit Claude snapshot is excluded.
                // Compilation replaces this generated classifier with the
                // exact revision-one family split, including numeric-prefix
                // names and published-alias folding. The matcher remains the
                // bounded authored shape carried by the canonical document.
                model_matcher: MatcherSpec::Regex(
                    r"^(?:claude-)?(?P<family>[a-z0-9][a-z0-9-]*?)-(?P<version>[0-9]+(?:-[0-9]+)*)$"
                        .to_owned(),
                ),
                family_template: "${family}".to_owned(),
                version_template: "${version}".to_owned(),
                version_parser: VersionParserSpec::LegacyNumericTuple {
                    separator: "-".to_owned(),
                },
                legacy_hyphenated: true,
            });
            self.policy.upgrades.push(UpgradePolicy {
                id: upgrade_id(binding)?,
                classifier_id,
                enabled: self.config.gates.force_newest,
                minimum_learning: MinimumLearning::RevisionOneAdaptive,
                allow_manual_promotion: true,
                inventory_sources: vec![InventorySource::Learned, InventorySource::Served],
                lane_transition: LaneTransitionPolicy::MoveWhenCold,
                allow_revision_one_legacy_learning: false,
            });
        }
        Ok(())
    }

    fn add_backend_routes(&mut self) -> anyhow::Result<()> {
        for binding in self.topology.bindings() {
            let route_id = backend_route_id(binding)?;
            let alias_policy = model_map_for(self.config, binding.provider())
                .map(alias_policy)
                .transpose()?;
            let upgrade_policy_id = supports_legacy_families(binding)
                .then(|| upgrade_id(binding))
                .transpose()?;
            self.policy.routes.push(Route {
                honours_server_safeguards: false,
                id: route_id,
                display_name: format!("Legacy {} route", binding.provider()),
                description: None,
                parameters: vec![parameter(REQUESTED_ROUTE_PARAMETER)?],
                chain: RouteChain {
                    first: Candidate {
                        id: candidate_id(binding)?,
                        binding_id: binding.id().clone(),
                        model: CandidateModel::RequestedModel,
                        upgrade_policy_id,
                        alias_policy,
                        required_capabilities: Default::default(),
                        enabled: self.topology.binding_is_configured(binding),
                        health_circuits: vec![],
                    },
                    steps: vec![],
                },
                // A pinned lane may move once its cache is gone, which is
                // what the force-newest upgrade and the compaction retarget
                // need. The combination with their own policies is the
                // stricter of the two, so PreservePinned here would deny
                // both for every pinned lane however long it idled.
                lane_transition: LaneTransitionPolicy::MoveWhenCold,
            });
        }
        Ok(())
    }

    fn add_defaults(&mut self) -> anyhow::Result<()> {
        self.add_default(
            FrontendProtocol::AnthropicMessages,
            self.config.default_backend_anthropic.as_deref(),
        )?;
        self.add_default(
            FrontendProtocol::OpenaiChat,
            self.config.default_backend_openai_chat.as_deref(),
        )?;
        if self.config.codex_sub.is_some() {
            self.add_default(FrontendProtocol::OpenaiResponses, Some("codex_sub"))?;
        }
        Ok(())
    }

    fn add_default(
        &mut self,
        protocol: FrontendProtocol,
        provider: Option<&str>,
    ) -> anyhow::Result<()> {
        let Some(binding) = binding_for(self.topology, protocol.into(), provider) else {
            return Ok(());
        };
        let target = requested_reference(backend_route_id(binding)?)?;
        self.policy.defaults.push(DefaultRoute {
            scope: FrontendScope::Protocol { protocol },
            target: target.clone(),
        });
        self.policy.defaults.push(DefaultRoute {
            scope: FrontendScope::Profile {
                profile: ProfileId::new("unprefixed")?,
                protocol,
            },
            target,
        });
        Ok(())
    }

    fn add_explicit_prefixes(&mut self) -> anyhow::Result<()> {
        // Every statically known provider prefix owns its request even while
        // disabled. The generated candidate then yields the explicit
        // not-configured result instead of falling through to a default.
        for route in self.topology.routes() {
            let provider = route.binding().provider();
            let pattern = format!(r"^{}/(?P<model>.+)$", regex::escape(provider));
            self.policy.rewrites.push(RewriteRule {
                id: RewriteId::new(format!(
                    "legacy-prefix-{}-{}",
                    protocol_slug(route.frontend()),
                    provider
                ))?,
                scope: FrontendScope::Protocol {
                    protocol: route.frontend().into(),
                },
                priority: prefix_priority(route.frontend(), provider),
                model_matcher: MatcherSpec::Regex(pattern),
                target: RouteReference {
                    route_id: backend_route_id(route.binding())?,
                    parameters: BTreeMap::from([(
                        parameter(REQUESTED_ROUTE_PARAMETER)?,
                        ArgumentBinding::RegexCaptureName {
                            name: "model".to_owned(),
                        },
                    )]),
                },
            });
        }
        Ok(())
    }

    fn add_anthropic_alias(&mut self) -> anyhow::Result<()> {
        let Some(provider) = self.config.default_backend_anthropic.as_deref() else {
            return Ok(());
        };
        let Some(binding) =
            binding_for(self.topology, ProtocolId::AnthropicMessages, Some(provider))
        else {
            return Ok(());
        };
        self.policy.rewrites.push(RewriteRule {
            id: RewriteId::new("legacy-anthropic-protocol-alias")?,
            scope: FrontendScope::Protocol {
                protocol: FrontendProtocol::AnthropicMessages,
            },
            priority: -10_000,
            model_matcher: MatcherSpec::Regex(r"^anthropic/(?P<model>.+)$".to_owned()),
            target: RouteReference {
                route_id: backend_route_id(binding)?,
                parameters: BTreeMap::from([(
                    parameter(REQUESTED_ROUTE_PARAMETER)?,
                    ArgumentBinding::RegexCaptureName {
                        name: "model".to_owned(),
                    },
                )]),
            },
        });
        Ok(())
    }

    fn add_picker(&mut self) -> anyhow::Result<()> {
        let Some(openrouter) = &self.config.openrouter else {
            return Ok(());
        };
        let rules = openrouter.picker.clone().unwrap_or_else(|| {
            default_rules(
                self.config.anthropic_sub.is_some() || self.config.anthropic_api.is_some(),
            )
        });
        let binding = self
            .topology
            .bindings()
            .find(|binding| binding.id_str() == "openrouter.openrouter_messages")
            .context("static OpenRouter Messages binding is absent")?;
        for (index, rule) in rules.iter().enumerate() {
            self.policy
                .presentations
                .push(picker_presentation(index, rule, binding)?);
        }
        Ok(())
    }

    fn add_compaction(&mut self) -> anyhow::Result<()> {
        let spec = self
            .config
            .gates
            .compact_model
            .as_deref()
            .unwrap_or("sonnet");
        let eligible_bindings = self
            .topology
            .routes()
            .filter(|route| route.frontend() == ProtocolId::AnthropicMessages)
            .map(|route| route.binding())
            .filter(|binding| binding.provider() != "openrouter")
            .map(|binding| binding.id().clone())
            .collect();
        self.policy.compaction = Some(CompactionPolicy {
            target: CompactionTarget::RevisionOneLegacy {
                model_spec: spec.to_owned(),
                eligible_bindings,
            },
            lane_transition: LaneTransitionPolicy::MoveWhenCold,
            cache_transition: CacheTransitionPolicy::AllowWhenCold,
        });
        Ok(())
    }
}

fn picker_presentation(
    index: usize,
    rule: &PickerRule,
    binding: &BackendBinding,
) -> anyhow::Result<Presentation> {
    let include = rule
        .matches
        .iter()
        .map(|glob| MatcherSpec::Glob(glob.clone()))
        .collect();
    let exclude = rule
        .exclude
        .iter()
        .map(|glob| MatcherSpec::Glob(glob.clone()))
        .collect();
    let variant = rule.variant.as_deref().unwrap_or_default();
    Ok(Presentation {
        id: PresentationId::new(format!("legacy-picker-{index}"))?,
        scope: FrontendScope::Profile {
            profile: ProfileId::new("claude")?,
            protocol: FrontendProtocol::AnthropicMessages,
        },
        slug: format!("openrouter/${{model_id}}{variant}"),
        label: None,
        description: None,
        enabled: true,
        // Legacy picker rows intentionally publish the native routed slug.
        shadow_native: true,
        source: PresentationSource::OfferSelector {
            selector: OfferSelector {
                binding_id: binding.id().clone(),
                include,
                exclude,
                ordering: SelectorOrdering::CatalogueChronology,
                keep: u16::try_from(rule.keep).context("picker keep exceeds policy limit")?,
            },
            label_template: Some("${model_id}".to_owned()),
        },
        metadata: Some(AdapterMetadata::Claude {
            behaves_as: Some(rule.behaves_as.clone()),
        }),
    })
}

fn alias_policy(map: &ModelMap) -> anyhow::Result<AliasPolicy> {
    let mut rules = Vec::new();
    for (selector, target) in map.entries() {
        if let Some(model) = selector.strip_prefix("model:") {
            rules.push(AliasRule {
                matcher: MatcherSpec::RevisionOneModel(model.to_owned()),
                exclude: vec![],
                replacement: target.to_owned(),
            });
        }
    }
    for (selector, target) in map.entries() {
        let Some(family) = selector.strip_prefix("family:") else {
            continue;
        };
        rules.push(AliasRule {
            matcher: MatcherSpec::RevisionOneFamily(family.to_owned()),
            exclude: vec![],
            replacement: target.to_owned(),
        });
    }
    Ok(AliasPolicy { rules })
}

/// The model map configured for `provider`, read from the config so that
/// building revision one never needs a constructed (credentialed) provider.
fn model_map_for<'a>(config: &'a Config, provider: &str) -> Option<&'a ModelMap> {
    match provider {
        "anthropic_sub" => config.anthropic_sub.as_ref()?.model_map.as_ref(),
        "anthropic_api" => config.anthropic_api.as_ref()?.model_map.as_ref(),
        "codex_sub" => config.codex_sub.as_ref()?.model_map.as_ref(),
        _ => None,
    }
}

fn requested_reference(route_id: RouteId) -> anyhow::Result<RouteReference> {
    Ok(RouteReference {
        route_id,
        parameters: BTreeMap::from([(
            parameter(REQUESTED_ROUTE_PARAMETER)?,
            ArgumentBinding::RequestedModel,
        )]),
    })
}

fn binding_for<'a>(
    topology: &'a RouteRegistry,
    protocol: ProtocolId,
    provider: Option<&str>,
) -> Option<&'a BackendBinding> {
    let provider = provider?;
    topology
        .routes()
        .find(|route| route.frontend() == protocol && route.binding().provider() == provider)
        .map(|route| route.binding())
}

fn supports_legacy_families(binding: &BackendBinding) -> bool {
    matches!(
        binding.protocol(),
        ProtocolId::AnthropicMessages | ProtocolId::OpenAiResponses
    )
}

fn backend_route_id(binding: &BackendBinding) -> anyhow::Result<RouteId> {
    RouteId::new(format!("legacy-route-{}", stable_part(binding.id_str()))).map_err(Into::into)
}

fn candidate_id(binding: &BackendBinding) -> anyhow::Result<CandidateId> {
    CandidateId::new(format!(
        "legacy-candidate-{}",
        stable_part(binding.id_str())
    ))
    .map_err(Into::into)
}

fn classifier_id(binding: &BackendBinding) -> anyhow::Result<super::ClassifierId> {
    super::ClassifierId::new(format!(
        "{LEGACY_FAMILY_CLASSIFIER}-{}",
        stable_part(binding.id_str())
    ))
    .map_err(Into::into)
}

fn upgrade_id(binding: &BackendBinding) -> anyhow::Result<UpgradePolicyId> {
    UpgradePolicyId::new(format!(
        "{LEGACY_UPGRADE_POLICY}-{}",
        stable_part(binding.id_str())
    ))
    .map_err(Into::into)
}

fn parameter(value: &str) -> anyhow::Result<StableId> {
    StableId::new(value).map_err(Into::into)
}

fn stable_part(value: &str) -> String {
    value.replace('.', "-")
}

fn prefix_priority(protocol: ProtocolId, provider: &str) -> i32 {
    let protocol = match protocol {
        ProtocolId::AnthropicMessages => 0,
        ProtocolId::OpenAiChat => 100,
        ProtocolId::OpenAiResponses => 200,
    };
    let provider = match provider {
        "anthropic_sub" => 0,
        "anthropic_api" => 1,
        "openrouter" => 2,
        "openai_api" => 3,
        "codex_sub" => 4,
        _ => 99,
    };
    protocol + provider
}

fn protocol_slug(protocol: ProtocolId) -> &'static str {
    match protocol {
        ProtocolId::AnthropicMessages => "anthropic",
        ProtocolId::OpenAiChat => "openai-chat",
        ProtocolId::OpenAiResponses => "openai-responses",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::catalog::fetched::{FetchedCatalog, FetchedCatalogs, FetchedModel};
    use crate::catalog::offers;
    use crate::middleware::model_map::parse_model_map;
    use crate::providers::{AnthropicApi, AnthropicSub, OpenAiApi, OpenRouter, Provider};

    fn load_config(text: &str) -> Config {
        let dir = crate::test_support::tempdir("policy-legacy-");
        fs::write(dir.join("toker.toml"), text).unwrap();
        Config::load_from(&dir.join("toker.toml")).unwrap()
    }

    fn topology(config: &Config) -> RouteRegistry {
        let mut providers: Vec<Arc<dyn Provider>> = vec![];
        if let Some(provider) = &config.openrouter {
            providers.push(Arc::new(OpenRouter::new(provider.upstream.clone(), None)));
        }
        if let Some(provider) = &config.openai_api {
            providers.push(Arc::new(OpenAiApi::new(provider.upstream.clone(), None)));
        }
        if let Some(provider) = &config.anthropic_sub {
            providers.push(Arc::new(AnthropicSub::new(
                provider.upstream.clone(),
                None,
                None,
            )));
        }
        if let Some(provider) = &config.anthropic_api {
            providers.push(Arc::new(AnthropicApi::new(provider.upstream.clone(), None)));
        }
        RouteRegistry::new(providers)
    }

    #[test]
    fn provider_default_matrix_generates_protocol_and_unprefixed_defaults() {
        let cases = [
            ("", 0, 0),
            ("[providers.anthropic_sub]\n", 2, 0),
            ("[providers.openrouter]\n", 0, 2),
            (
                "default_backend_anthropic = \"anthropic_api\"\n\
                 [providers.anthropic_sub]\n[providers.anthropic_api]\n",
                2,
                0,
            ),
            (
                "default_backend_openai_chat = \"openai_api\"\n\
                 [providers.openrouter]\n[providers.openai_api]\n",
                0,
                2,
            ),
        ];
        for (text, anthropic, chat) in cases {
            let config = load_config(text);
            let topology = topology(&config);
            let document = document(&config, &topology).unwrap();
            let count = |protocol| {
                document
                    .defaults
                    .iter()
                    .filter(|default| match &default.scope {
                        FrontendScope::Profile {
                            protocol: value, ..
                        }
                        | FrontendScope::Protocol { protocol: value } => *value == protocol,
                    })
                    .count()
            };
            assert_eq!(
                count(FrontendProtocol::AnthropicMessages),
                anthropic,
                "{text}"
            );
            assert_eq!(count(FrontendProtocol::OpenaiChat), chat, "{text}");
            assert!(validated(&config, &topology).is_ok(), "{text}");
        }
    }

    #[test]
    fn prefixes_alias_and_unknown_prefix_ownership_are_encoded() {
        let config = load_config("[providers.anthropic_sub]\n[providers.openrouter]\n");
        let topology = topology(&config);
        let document = document(&config, &topology).unwrap();
        assert!(
            document
                .rewrites
                .iter()
                .any(|rule| rule.id.as_str() == "legacy-prefix-anthropic-openrouter")
        );
        assert!(
            document
                .rewrites
                .iter()
                .any(|rule| rule.id.as_str() == "legacy-anthropic-protocol-alias")
        );
        assert!(document.defaults.iter().any(|default| matches!(
            default.scope,
            FrontendScope::Protocol {
                protocol: FrontendProtocol::AnthropicMessages
            }
        )));

        let disabled = load_config("[providers.anthropic_sub]\n");
        let disabled_topology = self::topology(&disabled);
        let disabled_document = super::document(&disabled, &disabled_topology).unwrap();
        let rewrite = disabled_document
            .rewrites
            .iter()
            .find(|rule| rule.id.as_str() == "legacy-prefix-anthropic-openrouter")
            .unwrap();
        let route = disabled_document
            .routes
            .iter()
            .find(|route| route.id == rewrite.target.route_id)
            .unwrap();
        assert!(
            !route.chain.first.enabled,
            "known disabled prefix must fail explicitly"
        );
    }

    #[test]
    fn model_map_aliases_keep_exact_before_family() {
        let map = parse_model_map(
            r#"{"family:opus":"family-target","model:claude-opus-4-5":"exact-target"}"#,
        )
        .unwrap()
        .unwrap();
        let policy = alias_policy(&map).unwrap();
        assert_eq!(policy.rules.len(), 2);
        assert!(matches!(
            &policy.rules[0].matcher,
            MatcherSpec::RevisionOneModel(value) if value == "claude-opus-4-5"
        ));
        assert!(matches!(
            &policy.rules[1].matcher,
            MatcherSpec::RevisionOneFamily(value) if value == "opus"
        ));
        assert_eq!(policy.rules[0].replacement, "exact-target");
        assert_eq!(policy.rules[1].replacement, "family-target");
        let exact = policy.rules[0].matcher.compile().unwrap();
        assert!(exact.is_match("claude-opus-4-5-20251101").unwrap());
        let family = policy.rules[1].matcher.compile().unwrap();
        assert!(family.is_match("claude-3-7-opus").unwrap());
        assert!(family.is_match("claude-opus-4-5-20251101").unwrap());
    }

    #[test]
    fn a_credential_free_topology_builds_the_same_document_as_the_providers() {
        // Setup activates revision one without constructing providers, so
        // it must not see a different document from the service's.
        let config = load_config(
            "default_backend_anthropic = \"anthropic_api\"\n\
             [providers.anthropic_sub]\n\
             [providers.anthropic_api]\n\
             [providers.anthropic_api.model_map]\n\
             \"family:sonnet\" = \"claude-sonnet-5\"\n\
             [providers.openrouter]\n\
             [[providers.openrouter.picker]]\n\
             match = [\"lab/*\"]\n\
             behaves_as = \"claude-sonnet-5\"\n",
        );
        let full = document(&config, &topology(&config)).unwrap();
        let bare = document(
            &config,
            &RouteRegistry::configured_bindings(crate::picker::configured_bindings(&config)),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&full).unwrap(),
            serde_json::to_value(&bare).unwrap()
        );
        assert!(
            serde_json::to_string(&bare)
                .unwrap()
                .contains("claude-sonnet-5"),
            "the model map is compiled"
        );
    }

    #[test]
    fn picker_rules_become_binding_local_selectors_with_variants_and_metadata() {
        let config = load_config(
            "[providers.openrouter]\n\
             [[providers.openrouter.picker]]\n\
             match = [\"lab/model-*\"]\n\
             behaves_as = \"claude-sonnet-5\"\n\
             variant = \":nitro\"\n\
             keep = 2\n",
        );
        let topology = topology(&config);
        let document = document(&config, &topology).unwrap();
        let presentation = &document.presentations[0];
        assert_eq!(presentation.slug, "openrouter/${model_id}:nitro");
        assert_eq!(
            presentation.metadata,
            Some(AdapterMetadata::Claude {
                behaves_as: Some("claude-sonnet-5".to_owned())
            })
        );
        let PresentationSource::OfferSelector { selector, .. } = &presentation.source else {
            panic!("selector")
        };
        assert_eq!(selector.keep, 2);
        assert_eq!(
            selector.binding_id.as_str(),
            "openrouter.openrouter_messages"
        );
    }

    #[test]
    fn force_newest_generates_explicit_classifiers_and_legacy_upgrades() {
        for (setting, expected) in [("true", true), ("false", false)] {
            let config = load_config(&format!(
                "[providers.anthropic_sub]\n[gates]\nforce_newest = {setting}\n"
            ));
            let topology = topology(&config);
            let document = document(&config, &topology).unwrap();
            assert!(
                document
                    .classifiers
                    .iter()
                    .all(|classifier| matches!(classifier.model_matcher, MatcherSpec::Regex(_)))
            );
            assert!(
                document
                    .upgrades
                    .iter()
                    .all(|upgrade| upgrade.enabled == expected
                        && !upgrade.allow_revision_one_legacy_learning)
            );
            let compilation = compile(&config, &topology, &OfferInventory::default()).unwrap();
            let policy = compilation
                .policy()
                .unwrap_or_else(|| panic!("{:?}", compilation.diagnostics()));
            let classifier = policy
                .upgrades()
                .next()
                .and_then(|upgrade| policy.classifier(&upgrade.policy().classifier_id))
                .expect("generated classifier");
            for model in [
                "claude-opus-4-10",
                "claude-3-7-sonnet",
                "claude-3-7-sonnet-20250219",
            ] {
                let expected = crate::policy::family::family_of(model).expect("legacy family");
                let classified = classifier
                    .classify(model)
                    .expect("classification succeeds")
                    .expect("compiled family");
                assert_eq!(classified.family, expected.name, "{model}");
                assert_eq!(
                    classified.version,
                    VersionParserSpec::LegacyNumericTuple {
                        separator: "-".to_owned(),
                    }
                    .parse(
                        &expected
                            .version
                            .iter()
                            .map(u64::to_string)
                            .collect::<Vec<_>>()
                            .join("-")
                    )
                    .unwrap(),
                    "{model}"
                );
            }
            assert!(
                classifier
                    .classify("claude-opus-4-9-20261225")
                    .expect("classification succeeds")
                    .is_none(),
                "unpublished snapshots stay unclassified",
            );
        }
    }

    #[test]
    fn compact_target_handles_none_off_family_and_exact() {
        let cases = [
            (None, "sonnet"),
            (Some("off"), "off"),
            (Some(""), ""),
            (Some("sonnet"), "sonnet"),
            (Some("claude-sonnet-5"), "claude-sonnet-5"),
        ];
        for (spec, expected) in cases {
            let compact = spec
                .map(|value| format!("compact_model = \"{value}\"\n"))
                .unwrap_or_default();
            let config = load_config(&format!(
                "[providers.anthropic_sub]\n[providers.openrouter]\n[gates]\n{compact}"
            ));
            let topology = topology(&config);
            let document = document(&config, &topology).unwrap();
            let CompactionTarget::RevisionOneLegacy {
                model_spec,
                eligible_bindings,
            } = &document.compaction.as_ref().unwrap().target
            else {
                panic!("generated compaction is a legacy overlay")
            };
            assert_eq!(model_spec, expected);
            assert!(
                eligible_bindings
                    .iter()
                    .any(|binding| { binding.as_str() == "anthropic_sub.anthropic_messages" })
            );
            assert!(
                eligible_bindings
                    .iter()
                    .all(|binding| { binding.as_str() != "openrouter.openrouter_messages" })
            );
            assert!(
                document
                    .routes
                    .iter()
                    .all(|route| route.id.as_str() != "legacy-compaction"),
                "compaction preserves the ordinarily selected route"
            );
        }
    }

    #[test]
    fn chat_publication_matches_the_shared_legacy_projection() {
        let config = load_config(
            "default_backend_openai_chat = \"openrouter\"\n\
             [providers.openrouter]\n[providers.openai_api]\n",
        );
        let topology = topology(&config);
        let mut catalogs = FetchedCatalogs::default();
        for source in ["openrouter", "openai_api"] {
            catalogs.set(
                source,
                FetchedCatalog {
                    fetched_at_ms: 7,
                    models: ["shared", "specific"]
                        .into_iter()
                        .map(|id| FetchedModel {
                            id: format!("{source}-{id}"),
                            context_window: Some(100),
                            chronology: None,
                            raw: json!({"id": id}),
                        })
                        .collect(),
                },
            );
        }
        let inventory = offers::inventory(&topology, &catalogs);
        let legacy: std::collections::BTreeSet<String> = [
            "openrouter/openrouter-shared",
            "openrouter/openrouter-specific",
            "openrouter-shared",
            "openrouter-specific",
            "openai_api/openai_api-shared",
            "openai_api/openai_api-specific",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let compilation = compile(&config, &topology, &inventory).unwrap();
        let policy = compilation.policy().expect("compiled");
        let compiled: std::collections::BTreeSet<_> = policy
            .publication_slugs(None, FrontendProtocol::OpenaiChat)
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(compiled, legacy);
        let scope = FrontendScope::Protocol {
            protocol: FrontendProtocol::OpenaiChat,
        };
        assert!(
            compiled
                .iter()
                .all(|slug| policy.presentation(&scope, slug).is_none()),
            "inventory publication must not become request-routing presentations"
        );
    }

    #[test]
    fn revision_one_has_no_virtual_responses_presentation_and_no_codex_record() {
        let config = load_config(
            "default_backend_openai_chat = \"openrouter\"\n\
             [providers.openrouter]\n\
             [providers.codex_sub]\n",
        );
        let topology = topology(&config);
        let document = document(&config, &topology).unwrap();
        for presentation in &document.presentations {
            assert!(
                !matches!(presentation.metadata, Some(AdapterMetadata::Codex { .. })),
                "{}",
                presentation.id
            );
            let responses = matches!(
                presentation.scope,
                FrontendScope::Protocol {
                    protocol: FrontendProtocol::OpenaiResponses
                } | FrontendScope::Profile {
                    protocol: FrontendProtocol::OpenaiResponses,
                    ..
                }
            );
            assert!(
                !(responses && matches!(presentation.source, PresentationSource::Virtual { .. })),
                "{}",
                presentation.id
            );
        }
    }

    #[test]
    fn chat_listing_renders_the_generated_publication() {
        let config = load_config(
            "default_backend_openai_chat = \"openrouter\"\n\
             [providers.openrouter]\n",
        );
        let topology = topology(&config);
        let mut catalogs = FetchedCatalogs::default();
        catalogs.set(
            "openrouter",
            FetchedCatalog {
                fetched_at_ms: 7,
                models: vec![FetchedModel {
                    id: "lab/one".to_owned(),
                    context_window: Some(100),
                    chronology: None,
                    raw: json!({"id": "lab/one", "created": 3}),
                }],
            },
        );
        let inventory = offers::inventory(&topology, &catalogs);
        let compilation = compile(&config, &topology, &inventory).unwrap();
        let policy = compilation.policy().expect("compiled");

        // Prefixed, and bare because openrouter is the Chat default.
        let offers::PolicyPublication::Ready(listing) =
            offers::render_policy(policy, &inventory, None, FrontendProtocol::OpenaiChat)
        else {
            panic!("the inventory resolves every generated slug");
        };
        let ids: Vec<_> = listing["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["lab/one", "openrouter/lab/one"]);

        // A Messages frontend has no generated listing to add.
        let offers::PolicyPublication::Ready(messages) = offers::render_policy(
            policy,
            &inventory,
            None,
            FrontendProtocol::AnthropicMessages,
        ) else {
            panic!("an empty publication resolves");
        };
        assert!(messages["data"].as_array().unwrap().is_empty());
    }

    #[test]
    fn generated_publication_is_not_limited_by_authored_document_bounds() {
        let config = load_config("[providers.openrouter]\n");
        let topology = topology(&config);
        let mut catalogs = FetchedCatalogs::default();
        catalogs.set(
            "openrouter",
            FetchedCatalog {
                fetched_at_ms: 7,
                models: (0..600)
                    .map(|index| FetchedModel {
                        id: format!("invented/model-{index}"),
                        context_window: None,
                        chronology: None,
                        raw: json!({"id": index}),
                    })
                    .collect(),
            },
        );
        let inventory = offers::inventory(&topology, &catalogs);
        let compilation = compile(&config, &topology, &inventory).unwrap();
        let policy = compilation.policy().expect("compiled");
        assert_eq!(
            policy
                .publication_slugs(None, FrontendProtocol::OpenaiChat)
                .len(),
            1_200
        );
    }

    #[test]
    fn picker_selector_expands_deterministically_from_canonical_inventory() {
        let config = load_config(
            "[providers.openrouter]\n\
             [[providers.openrouter.picker]]\n\
             match = [\"lab/model-*\"]\n\
             behaves_as = \"claude-sonnet-5\"\n\
             keep = 2\n",
        );
        let topology = topology(&config);
        let mut catalogs = FetchedCatalogs::default();
        catalogs.set(
            "openrouter",
            FetchedCatalog {
                fetched_at_ms: 7,
                models: ["lab/model-a", "lab/model-b"]
                    .into_iter()
                    .map(|id| FetchedModel {
                        id: id.to_owned(),
                        context_window: Some(100),
                        chronology: None,
                        raw: json!({"id": id}),
                    })
                    .collect(),
            },
        );
        let inventory = offers::inventory(&topology, &catalogs);
        let compilation = compile(&config, &topology, &inventory).unwrap();
        let policy = compilation.policy().expect("compiled");
        let scope = FrontendScope::Profile {
            profile: ProfileId::new("claude").unwrap(),
            protocol: FrontendProtocol::AnthropicMessages,
        };
        assert!(
            policy
                .presentation(&scope, "openrouter/lab/model-a")
                .is_some()
        );
        assert!(
            policy
                .presentation(&scope, "openrouter/lab/model-b")
                .is_some()
        );
    }
}
