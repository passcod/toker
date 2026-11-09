//! Pure conversion of resolved legacy configuration into revision-one policy.
//!
//! This module translates routing vocabulary only. It does not persist a
//! policy or consult the ledger.

use std::collections::BTreeMap;

use anyhow::Context;

use crate::catalog::model_identity;
use crate::catalog::offers::OfferInventory;
use crate::config::Config;
use crate::middleware::model_map::ModelMap;
use crate::picker::{PickerRule, default_rules};
use crate::routing::{BackendBinding, ProtocolId, RouteRegistry};

use super::{
    AdapterMetadata, AliasPolicy, AliasRule, ArgumentBinding, CURRENT_FORMAT_VERSION,
    CacheTransitionPolicy, Candidate, CandidateId, CandidateModel, CompactionPolicy,
    CompactionTarget, Compilation, DefaultRoute, FamilyClassifier, FrontendProtocol, FrontendScope,
    InventorySource, LaneTransitionPolicy, MatcherSpec, MinimumLearning, OfferSelector,
    PolicyDocument, Presentation, PresentationId, PresentationSource, ProfileId,
    REQUESTED_MODEL_PARAMETER, RewriteId, RewriteRule, Route, RouteChain, RouteId, RouteReference,
    SelectorOrdering, StableId, SubstituteModel, UpgradePolicy, UpgradePolicyId, VersionParserSpec,
};

const FAMILY_CLASSIFIER: &str = "family";
const UPGRADE_POLICY: &str = "force-newest";
const REQUESTED_ROUTE_PARAMETER: &str = REQUESTED_MODEL_PARAMETER;

/// A version as the generated classifier reads it: numeric segments joined
/// by `-`, none long enough to be a date, so an identity still carrying a
/// date after folding (an unpublished snapshot) has no family.
// spec: RMIG#in-authored-vocabulary
const ANTHROPIC_VERSION: &str = "[0-9]{1,4}(?:-[0-9]{1,4})*";

/// The compaction model the configuration names when it names none.
const DEFAULT_COMPACTION_FAMILY: &str = "sonnet";

/// The compaction model that turns the model change off, leaving the cold
/// transform alone.
const COMPACTION_MODEL_OFF: &str = "off";

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
    Ok(super::compile(
        validated(config, topology)?,
        topology,
        inventory,
    ))
}

/// Compile and activate revision one from the legacy config, or load the
/// revision that is already active without consulting the config. Startup and
/// setup both go through here, so a machine setup activates is the machine the
/// service would have activated itself, and both retire the generated
/// revisions an earlier version stored before anything reads them.
///
/// The config is built into a first revision up front but only needed where
/// no revision is active, so a config that cannot route stops only a machine
/// that has nothing else to route by.
pub fn establish_revision_one(
    store: &crate::store::Store,
    config: &Config,
    topology: &RouteRegistry,
    inventory: &OfferInventory,
    publication_generation: i64,
    activated_ms: i64,
    configuration: Option<&crate::store::Configuration>,
) -> anyhow::Result<(crate::store::PolicyRevision, super::CompiledPolicy)> {
    let first = config
        .validate()
        .and_then(|()| validated(config, topology))
        .and_then(|document| {
            let policy = super::compile(document.clone(), topology, inventory)
                .into_policy()
                .map_err(|diagnostics| {
                    anyhow::anyhow!(
                        "generated revision one has {} compile diagnostics",
                        diagnostics.len()
                    )
                })?;
            Ok((document, policy))
        });
    let activation = store.activate_revision_one_importing(
        first.as_ref().ok().map(|(document, _)| document),
        activated_ms,
        publication_generation,
        &crate::store::publication_targets()?,
        configuration,
    )?;
    match activation {
        crate::store::RevisionOneActivation::Activated { revision, .. } => {
            let (_, policy) = first.expect("activation used the first revision");
            Ok((revision, policy))
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
        crate::store::RevisionOneActivation::NeedsDocument => {
            Err(first.expect_err("a first revision was offered unless it failed to build"))
        }
    }
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
                description: Some("Generated first revision".to_owned()),
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
        self.add_compaction();
        Ok(self.policy)
    }

    fn add_family_policy(&mut self) -> anyhow::Result<()> {
        for binding in self.topology.bindings() {
            if !serves_anthropic_identities(binding) {
                continue;
            }
            let classifier_id = classifier_id(binding)?;
            self.policy.classifiers.push(FamilyClassifier {
                id: classifier_id.clone(),
                binding_id: binding.id().clone(),
                model_matcher: MatcherSpec::Regex(format!(
                    "^claude-(?P<family>[a-z]+(?:-[a-z]+)*)-(?P<version>{ANTHROPIC_VERSION})$"
                )),
                family_template: "${family}".to_owned(),
                version_template: "${version}".to_owned(),
                version_parser: VersionParserSpec::NumericTuple {
                    components: None,
                    separator: "-".to_owned(),
                },
            });
            self.policy.upgrades.push(UpgradePolicy {
                id: upgrade_id(binding)?,
                classifier_id,
                enabled: self.config.gates.force_newest,
                minimum_learning: MinimumLearning::Scaled,
                allow_manual_promotion: true,
                inventory_sources: vec![InventorySource::Learned, InventorySource::Served],
                lane_transition: LaneTransitionPolicy::MoveWhenCold,
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
            let upgrade_policy_id = serves_anthropic_identities(binding)
                .then(|| upgrade_id(binding))
                .transpose()?;
            self.policy.routes.push(Route {
                honours_server_safeguards: false,
                id: route_id,
                display_name: format!("{} route", binding.provider()),
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
                    "prefix-{}-{}",
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
            id: RewriteId::new("anthropic-protocol-alias")?,
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

    fn add_compaction(&mut self) {
        let configured = self
            .config
            .gates
            .compact_model
            .as_deref()
            .unwrap_or(DEFAULT_COMPACTION_FAMILY);
        let model = compaction_model(configured);
        let bindings = self
            .topology
            .bindings()
            .filter(|binding| serves_anthropic_identities(binding))
            .map(|binding| binding.id().clone())
            .collect();
        self.policy.compaction = Some(CompactionPolicy {
            target: CompactionTarget::Substitution { model, bindings },
            lane_transition: LaneTransitionPolicy::MoveWhenCold,
            cache_transition: CacheTransitionPolicy::AllowWhenCold,
        });
    }
}

/// The configured compaction model as a substitution target: an identity
/// carrying a version is one exact model, a bare name is a family, and
/// `off` or nothing at all moves no model.
fn compaction_model(configured: &str) -> Option<SubstituteModel> {
    let identity =
        model_identity(configured).filter(|identity| identity != COMPACTION_MODEL_OFF)?;
    if identity.bytes().any(|byte| byte.is_ascii_digit()) {
        return Some(SubstituteModel::Exact(identity));
    }
    let family = identity.strip_prefix("claude-").unwrap_or(&identity);
    (!family.is_empty()).then(|| SubstituteModel::Family(family.to_owned()))
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
        id: PresentationId::new(format!("picker-{index}"))?,
        scope: FrontendScope::Profile {
            profile: ProfileId::new("claude")?,
            protocol: FrontendProtocol::AnthropicMessages,
        },
        slug: format!("openrouter/${{model_id}}{variant}"),
        label: None,
        description: None,
        enabled: true,
        // Picker rows intentionally publish the native routed slug.
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

/// A model map as alias rules, exact models first. Alias rules match the
/// canonical identity, so the map's canonical model selectors match exactly,
/// and a family selector matches what the generated classifier places in
/// that family.
// spec: RMIG#in-authored-vocabulary
fn alias_policy(map: &ModelMap) -> anyhow::Result<AliasPolicy> {
    let mut rules = Vec::new();
    for (selector, target) in map.entries() {
        if let Some(model) = selector.strip_prefix("model:") {
            rules.push(AliasRule {
                matcher: MatcherSpec::Exact(model.to_owned()),
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
            matcher: MatcherSpec::Regex(format!(
                "^claude-{}-{ANTHROPIC_VERSION}$",
                regex::escape(family)
            )),
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

/// Whether `binding` serves Anthropic's own `claude-<family>-<version>`
/// identities, the only shape the generated classifier reads. OpenRouter's
/// and codex's identities are other shapes, so they get no family policy.
fn serves_anthropic_identities(binding: &BackendBinding) -> bool {
    matches!(binding.provider(), "anthropic_sub" | "anthropic_api")
}

fn backend_route_id(binding: &BackendBinding) -> anyhow::Result<RouteId> {
    RouteId::new(format!("route-{}", stable_part(binding.id_str()))).map_err(Into::into)
}

fn candidate_id(binding: &BackendBinding) -> anyhow::Result<CandidateId> {
    CandidateId::new(format!("candidate-{}", stable_part(binding.id_str()))).map_err(Into::into)
}

fn classifier_id(binding: &BackendBinding) -> anyhow::Result<super::ClassifierId> {
    super::ClassifierId::new(format!(
        "{FAMILY_CLASSIFIER}-{}",
        stable_part(binding.id_str())
    ))
    .map_err(Into::into)
}

fn upgrade_id(binding: &BackendBinding) -> anyhow::Result<UpgradePolicyId> {
    UpgradePolicyId::new(format!(
        "{UPGRADE_POLICY}-{}",
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
                .any(|rule| rule.id.as_str() == "prefix-anthropic-openrouter")
        );
        assert!(
            document
                .rewrites
                .iter()
                .any(|rule| rule.id.as_str() == "anthropic-protocol-alias")
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
            .find(|rule| rule.id.as_str() == "prefix-anthropic-openrouter")
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
        assert_eq!(
            policy.rules[0].matcher,
            MatcherSpec::Exact("claude-opus-4-5".to_owned())
        );
        assert_eq!(policy.rules[0].replacement, "exact-target");
        assert_eq!(policy.rules[1].replacement, "family-target");
        // Alias rules match the canonical identity, so a published snapshot
        // reaches them already folded to its undated form.
        let family = policy.rules[1].matcher.compile().unwrap();
        assert!(family.is_match("claude-opus-4-5").unwrap());
        assert!(family.is_match("claude-opus-5").unwrap());
        assert!(!family.is_match("claude-opus-4-5-20990101").unwrap());
        assert!(!family.is_match("claude-sonnet-4-5").unwrap());
        assert!(!family.is_match("claude-3-7-opus").unwrap());
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
    fn force_newest_generates_anthropic_classifiers_with_the_scaled_bar() {
        for (setting, expected) in [("true", true), ("false", false)] {
            let config = load_config(&format!(
                "[providers.anthropic_sub]\n[providers.openrouter]\n[providers.codex_sub]\n\
                 [gates]\nforce_newest = {setting}\n"
            ));
            let topology = topology(&config);
            let document = document(&config, &topology).unwrap();
            assert_eq!(
                document
                    .classifiers
                    .iter()
                    .map(|classifier| classifier.binding_id.as_str())
                    .collect::<Vec<_>>(),
                [
                    "anthropic_sub.anthropic_messages",
                    "anthropic_api.anthropic_messages"
                ],
                "only Anthropic's identities are classified, whether or not configured"
            );
            assert!(
                document
                    .upgrades
                    .iter()
                    .all(|upgrade| upgrade.enabled == expected
                        && upgrade.minimum_learning == MinimumLearning::Scaled
                        && upgrade.allow_manual_promotion)
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
            let parser = VersionParserSpec::NumericTuple {
                components: None,
                separator: "-".to_owned(),
            };
            for (model, family, version) in [
                ("claude-opus-4-10", "opus", "4-10"),
                ("claude-opus-5", "opus", "5"),
                ("claude-sonnet-4-5-20250929", "sonnet", "4-5"),
                ("claude-haiku-4-5[1m]", "haiku", "4-5"),
            ] {
                let classified = classifier
                    .classify(model)
                    .expect("classification succeeds")
                    .expect("compiled family");
                assert_eq!(classified.family, family, "{model}");
                assert_eq!(
                    classified.version,
                    parser.parse(version).unwrap(),
                    "{model}"
                );
            }
            for model in [
                "claude-opus-4-9-20261225",
                "claude-3-7-sonnet",
                "claude-mythos-preview",
                "gpt-5-codex",
            ] {
                assert!(
                    classifier
                        .classify(model)
                        .expect("classification succeeds")
                        .is_none(),
                    "{model} has no family",
                );
            }
        }
    }

    #[test]
    fn compact_target_handles_none_off_family_and_exact() {
        let sonnet = Some(SubstituteModel::Family("sonnet".to_owned()));
        let cases = [
            (None, sonnet.clone()),
            (Some("off"), None),
            (Some(""), None),
            (Some("sonnet"), sonnet.clone()),
            (Some("claude-sonnet"), sonnet),
            (
                Some("claude-sonnet-5"),
                Some(SubstituteModel::Exact("claude-sonnet-5".to_owned())),
            ),
        ];
        for (spec, expected) in cases {
            let compact = spec
                .map(|value| format!("compact_model = \"{value}\"\n"))
                .unwrap_or_default();
            let config = load_config(&format!(
                "[providers.anthropic_sub]\n[providers.openrouter]\n[providers.codex_sub]\n\
                 [gates]\n{compact}"
            ));
            let topology = topology(&config);
            let document = document(&config, &topology).unwrap();
            let CompactionTarget::Substitution { model, bindings } =
                &document.compaction.as_ref().unwrap().target
            else {
                panic!("generated compaction is a substitution")
            };
            assert_eq!(model, &expected, "{spec:?}");
            assert_eq!(
                bindings
                    .iter()
                    .map(|binding| binding.as_str())
                    .collect::<Vec<_>>(),
                [
                    "anthropic_sub.anthropic_messages",
                    "anthropic_api.anthropic_messages"
                ],
                "{spec:?}"
            );
            assert!(
                document
                    .routes
                    .iter()
                    .all(|route| route.id.as_str() != "compaction"),
                "compaction preserves the ordinarily selected route"
            );
        }
    }

    #[test]
    fn chat_publication_lists_what_the_revision_routes() {
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
        let expected: std::collections::BTreeSet<String> = [
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
        assert_eq!(compiled, expected);
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
    fn chat_listing_renders_the_derived_publication() {
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
            panic!("the inventory resolves every derived slug");
        };
        let ids: Vec<_> = listing["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["lab/one", "openrouter/lab/one"]);

        // A Messages frontend has no derived listing to add.
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
    fn derived_publication_is_not_limited_by_document_bounds() {
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
