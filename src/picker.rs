//! Claude Code's `/model` picker rows, published from the active routing
//! policy and the cached catalogues.
//!
//! Claude Code reads extra picker rows from `modelPicker` in
//! `~/.claude/settings.json` (user settings only; a project's settings
//! cannot carry it). Picking a row sends its `model` verbatim, so the rows
//! are the policy's Claude presentations: what the policy offers is edited
//! in `toker tui`, and a new model replaces an old one on the next sync
//! without anyone editing anything. [`sync`] compiles the active revision
//! against the cached catalogues and reconciles the settings file. The
//! service never runs it: it may not write outside the state dir, so
//! `toker picker sync` runs as the user, from setup, after an activation
//! in the routing TUI, and from the daily picker timer.
//!
//! [`PickerRule`] and [`default_rules`] are not picker behaviour any more:
//! they are the legacy `[[providers.openrouter.picker]]` vocabulary that the
//! revision-one compiler and the legacy import turn into presentations.

use std::collections::BTreeSet;

use anyhow::{Context, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::catalog::{fetched, offers};
use crate::policy::{AdapterMetadata, FrontendProtocol, ProfileId};
use crate::routing::RouteRegistry;
use crate::store::{
    PublicationAdapter, PublicationCasResult, PublicationKey, PublicationResultUpdate,
    PublicationState, RoutingPublication, Store,
};

/// The prefix that routes a model to openrouter on the Anthropic wire,
/// and marks a picker row as toker's: rows whose `model` starts with it
/// are replaced on every sync, every other row is the user's.
pub const ROW_PREFIX: &str = "openrouter/";

/// One picker rule (`[[providers.openrouter.picker]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PickerRule {
    /// Globs over openrouter model ids; a model matching any is a candidate.
    #[serde(rename = "match")]
    pub matches: Vec<String>,
    /// Globs that take a candidate back out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// The model whose client-side handling Claude Code applies to these
    /// rows (its `behavesAs`): a family name (`opus`, `sonnet`, `haiku`,
    /// `fable`), resolved at sync time, or a full Claude model id, passed
    /// through. Claude Code does not offer a row for a model it does not
    /// know without one.
    pub behaves_as: String,
    /// An openrouter variant suffix (`:floor`, `:nitro`) appended to the
    /// id the row sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// How many of the newest matches (by the listing's `created`) the
    /// rule offers.
    #[serde(default = "default_keep", skip_serializing_if = "is_default_keep")]
    pub keep: usize,
}

fn default_keep() -> usize {
    1
}

fn is_default_keep(keep: &usize) -> bool {
    *keep == 1
}

impl PickerRule {
    fn new(matches: &[&str], exclude: &[&str], behaves_as: &str) -> PickerRule {
        PickerRule {
            matches: matches.iter().map(|glob| (*glob).to_owned()).collect(),
            exclude: exclude.iter().map(|glob| (*glob).to_owned()).collect(),
            behaves_as: behaves_as.to_owned(),
            variant: None,
            keep: 1,
        }
    }

    /// Refuse a rule that could never produce a row, at config load
    /// rather than at the first sync.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.matches.is_empty() {
            bail!("a picker rule needs at least one `match` glob");
        }
        globs(&self.matches)?;
        globs(&self.exclude)?;
        if self.behaves_as.trim().is_empty() {
            bail!("picker rule {:?}: `behaves_as` is empty", self.matches);
        }
        if self.keep == 0 {
            bail!("picker rule {:?}: `keep` must be at least 1", self.matches);
        }
        if let Some(variant) = &self.variant
            && (!variant.starts_with(':') || variant.len() == 1)
        {
            bail!(
                "picker rule {:?}: `variant` must look like `:floor`, got {variant:?}",
                self.matches
            );
        }
        Ok(())
    }
}

fn globs(patterns: &[String]) -> anyhow::Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).with_context(|| format!("picker glob {pattern:?}"))?);
    }
    Ok(builder.build()?)
}

/// The built-in rules, used when the config names none: one flagship
/// and at most one fast model per well-known lab, the Auto Router, and,
/// only when `with_anthropic`, Anthropic's four families. With an
/// anthropic backend enabled, Claude Code's built-in lineup already
/// reaches those, and a second row per model would only crowd the
/// picker. Checked against openrouter's listing of 2026-10-06 (the
/// vendored fixture, and the snapshot of the rows it produces).
pub fn default_rules(with_anthropic: bool) -> Vec<PickerRule> {
    let mut rules = vec![
        PickerRule::new(&["openai/gpt-*-sol"], &["*-pro"], "opus"),
        PickerRule::new(&["openai/gpt-*-luna"], &["*-pro"], "haiku"),
        PickerRule::new(&["google/gemini-*-flash"], &[], "sonnet"),
        PickerRule::new(&["moonshotai/kimi-k*"], &["*-code", "*-thinking"], "sonnet"),
        PickerRule::new(
            &["z-ai/glm-*"],
            &["*-flash*", "*v*", "*-turbo", "*-prime"],
            "sonnet",
        ),
        PickerRule::new(&["z-ai/glm-*-flash"], &[], "haiku"),
        PickerRule::new(&["deepseek/deepseek-v*-pro*"], &[], "sonnet"),
        PickerRule::new(&["deepseek/deepseek-v*-flash*"], &["*-vision*"], "haiku"),
        PickerRule::new(&["qwen/qwen*-max*"], &["*-prime"], "sonnet"),
        PickerRule::new(&["minimax/minimax-m*"], &[], "sonnet"),
        PickerRule::new(&["openrouter/auto"], &[], "sonnet"),
    ];
    if with_anthropic {
        for family in ["opus", "sonnet", "haiku", "fable"] {
            rules.push(PickerRule::new(
                &[&format!("anthropic/claude-{family}-*")],
                &[],
                family,
            ));
        }
    }
    rules
}

/// One `modelPicker.options` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub model: String,
    pub label: String,
    pub description: String,
    pub behaves_as: String,
}

impl Row {
    /// The row as Claude Code's settings spell it.
    pub fn to_json(&self) -> Value {
        json!({
            "model": self.model,
            "label": self.label,
            "description": self.description,
            "behavesAs": self.behaves_as,
        })
    }
}

fn claude_publication_key() -> anyhow::Result<PublicationKey> {
    Ok(PublicationKey::new(
        crate::policy::FrontendScope::Protocol {
            protocol: FrontendProtocol::AnthropicMessages,
        },
        PublicationAdapter::new("claude_settings")?,
    ))
}

pub(crate) fn configured_bindings(
    config: &crate::config::Config,
) -> BTreeSet<crate::policy::BindingId> {
    let enabled = config.enabled_backends();
    crate::routing::BACKEND_BINDINGS
        .iter()
        .filter(|binding| enabled.contains(&binding.provider()))
        .filter(|binding| binding.canonical_backend().is_some())
        .map(|binding| binding.id().clone())
        .collect()
}

/// The cached catalogues of the backends `config` enables: what the publication
/// generation of a Claude picker sync is computed over.
pub(crate) fn configured_catalogs(
    config: &crate::config::Config,
    dir: Option<&std::path::Path>,
) -> fetched::FetchedCatalogs {
    let mut catalogs = dir.map(fetched::load_cached).unwrap_or_default();
    catalogs.retain_sources(configured_catalogue_sources(config));
    catalogs
}

fn configured_catalogue_sources(config: &crate::config::Config) -> BTreeSet<&'static str> {
    config
        .enabled_backends()
        .into_iter()
        .filter_map(fetched::FetchedCatalogs::source_of)
        .collect()
}

fn rows_for_profile(
    policy: &crate::policy::CompiledPolicy,
    profile: Option<&ProfileId>,
) -> anyhow::Result<Vec<Row>> {
    policy
        .publication(profile, FrontendProtocol::AnthropicMessages)
        .into_values()
        .filter_map(|presentation| {
            let AdapterMetadata::Claude { behaves_as } = presentation.metadata()? else {
                return None;
            };
            let behaves_as = behaves_as.as_ref()?;
            Some(Row {
                model: presentation.slug().to_owned(),
                label: presentation
                    .label()
                    .unwrap_or_else(|| presentation.slug())
                    .to_owned(),
                description: presentation.description().unwrap_or_default().to_owned(),
                behaves_as: behaves_as.to_owned(),
            })
        })
        .map(|row| {
            if row.model.is_empty() {
                bail!("policy generated an empty Claude picker model")
            }
            Ok(row)
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
#[error(
    "Claude settings cannot represent the incompatible picker presentation for profile {profile:?}"
)]
struct IncompatibleClaudeProfile {
    profile: String,
}

/// Claude Code has one user settings file even when toker serves several
/// Anthropic profiles. A profile-local presentation is publishable only when
/// every profile projects the same rows as the unprefixed/protocol view.
fn policy_rows(policy: &crate::policy::CompiledPolicy) -> anyhow::Result<Vec<Row>> {
    let claude = ProfileId::new("claude")?;
    let shared = rows_for_profile(policy, Some(&claude))?;
    for profile in policy.anthropic_profiles() {
        let rows = rows_for_profile(policy, Some(&profile))?;
        if rows != shared {
            return Err(IncompatibleClaudeProfile {
                profile: profile.as_str().to_owned(),
            }
            .into());
        }
    }
    Ok(shared)
}

fn cached_policy_rows(
    config: &crate::config::Config,
    store: &Store,
    publication: RoutingPublication,
) -> anyhow::Result<(Vec<Row>, RoutingPublication)> {
    let revision = store
        .load_policy_revision(publication.desired_revision_id)?
        .context("Claude settings publication references no policy revision")?;
    let catalogs = configured_catalogs(config, fetched::cache_dir().ok().as_deref());
    if catalogs.is_empty()
        && publication.desired_inventory_generation != fetched::EMPTY_SNAPSHOT_GENERATION
    {
        bail!("Claude picker catalogue is unavailable; run `toker picker sync` again");
    }
    if fetched::snapshot_generation(&catalogs) != publication.desired_inventory_generation {
        bail!("Claude picker catalogue changed while synchronising; run `toker picker sync` again");
    }
    let registry = RouteRegistry::configured_bindings(configured_bindings(config));
    let inventory = offers::inventory(&registry, &catalogs);
    let policy = crate::policy::runtime::compile_stored_revision(&revision, &registry, &inventory)
        .into_policy()
        .map_err(|diagnostics| {
            anyhow::anyhow!(
                "active policy revision {} has {} compilation diagnostics",
                revision.revision_id,
                diagnostics.len()
            )
        })?;
    Ok((policy_rows(&policy)?, publication))
}

/// What one sync did.
#[derive(Debug)]
pub struct SyncReport {
    pub rows: Vec<Row>,
    /// Whether the settings file changed (always `false` on a dry run).
    pub changed: bool,
}

/// Reconcile Claude Code's `/model` rows in `settings` with the active routing
/// policy and the cached catalogues. A machine whose database has no active
/// revision has nothing to publish: that is an error, raised before the
/// settings are touched, because empty rows would empty the picker.
pub fn sync(
    config: &crate::config::Config,
    settings: &std::path::Path,
    dry_run: bool,
) -> anyhow::Result<SyncReport> {
    let no_revision = || {
        anyhow::anyhow!(
            "no active routing revision in {}: run `toker setup`, or start the service",
            config.db_path.display()
        )
    };
    // Neither creates nor migrates the store (spec: STORE#opening-the-store):
    // the picker timer runs this unattended, so a store directory gone from
    // under the service must not come back empty from here.
    let opened = if dry_run {
        Store::open_read_only(&config.db_path)
    } else {
        Store::open_existing(&config.db_path)
    };
    let store = match opened {
        Ok(store) => store,
        Err(error) if error.is_missing_store() => return Err(no_revision()),
        Err(error) => return Err(error.into()),
    };
    if store
        .load_routing_policy_state()?
        .active_revision_id
        .is_none()
    {
        return Err(no_revision());
    }
    sync_active_policy(config, &store, settings, dry_run)
}

fn legacy_picker_identities(settings: &std::path::Path) -> BTreeSet<String> {
    std::fs::read_to_string(settings)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|settings| {
            settings
                .pointer("/modelPicker/options")
                .and_then(Value::as_array)
                .cloned()
        })
        .into_iter()
        .flatten()
        .filter_map(|row| {
            row.get("model")
                .and_then(Value::as_str)
                .filter(|model| model.starts_with(ROW_PREFIX))
                .map(str::to_owned)
        })
        .collect()
}

fn sync_active_policy(
    config: &crate::config::Config,
    store: &Store,
    settings: &std::path::Path,
    dry_run: bool,
) -> anyhow::Result<SyncReport> {
    let key = claude_publication_key()?;
    let publication = store
        .load_publication(&key)?
        .context("active policy has no Claude settings publication")?;
    let (rows, publication) = match cached_policy_rows(config, store, publication.clone()) {
        Ok(result) => result,
        Err(error) => {
            if !dry_run {
                let (state, detail_code) = publication_error_state(&error);
                record_publication_result(store, &key, &publication, state, detail_code);
            }
            return Err(error);
        }
    };
    if dry_run {
        return Ok(SyncReport {
            rows,
            changed: false,
        });
    }

    let json: Vec<_> = rows.iter().map(Row::to_json).collect();
    let legacy_unknown = publication
        .detail_code
        .as_ref()
        .is_some_and(|code| code.as_str() == "legacy_applied_identities_unknown");
    let legacy_identities = legacy_unknown.then(|| legacy_picker_identities(settings));
    let prior_identities = publication
        .applied_identities
        .as_ref()
        .or(legacy_identities.as_ref());
    let (changed, identities) =
        match crate::setup::patchers::reconcile_model_picker(settings, prior_identities, &json) {
            Ok(result) => result,
            Err(error) => {
                record_publication_result(
                    store,
                    &key,
                    &publication,
                    PublicationState::Failed,
                    "settings_write_failed",
                );
                return Err(error);
            }
        };
    let update = PublicationResultUpdate {
        state: PublicationState::Applied,
        applied_identities: Some(identities.clone()),
        detail_code: None,
        updated_ms: jiff::Timestamp::now().as_millisecond(),
    };
    match store.compare_and_swap_publication_result(
        &key,
        publication.desired_revision_id,
        publication.desired_inventory_generation,
        &update,
    )? {
        PublicationCasResult::Updated => Ok(SyncReport { rows, changed }),
        PublicationCasResult::Conflict {
            current: Some(current),
        } if current.state == PublicationState::Applied
            && current.desired_revision_id == publication.desired_revision_id
            && current.desired_inventory_generation == publication.desired_inventory_generation =>
        {
            Ok(SyncReport { rows, changed })
        }
        PublicationCasResult::Conflict {
            current: Some(current),
        } => {
            // This process wrote `identities`, but a new desired snapshot won
            // the state race before it could record them. Take those exact
            // rows into the immediate retry so the newer policy can converge
            // without mistaking the prior write for a user collision.
            let (rows, publication) = cached_policy_rows(config, store, current.clone())?;
            if publication.desired_revision_id != current.desired_revision_id
                || publication.desired_inventory_generation != current.desired_inventory_generation
            {
                bail!("Claude picker changed while synchronising; run `toker picker sync` again");
            }
            let mut prior = publication.applied_identities.clone().unwrap_or_default();
            prior.extend(identities);
            let json: Vec<_> = rows.iter().map(Row::to_json).collect();
            let (retry_changed, identities) =
                match crate::setup::patchers::reconcile_model_picker(settings, Some(&prior), &json)
                {
                    Ok(result) => result,
                    Err(error) => {
                        record_publication_result(
                            store,
                            &key,
                            &publication,
                            PublicationState::Failed,
                            "settings_write_failed",
                        );
                        return Err(error);
                    }
                };
            let update = PublicationResultUpdate {
                state: PublicationState::Applied,
                applied_identities: Some(identities),
                detail_code: None,
                updated_ms: jiff::Timestamp::now().as_millisecond(),
            };
            match store.compare_and_swap_publication_result(
                &key,
                publication.desired_revision_id,
                publication.desired_inventory_generation,
                &update,
            )? {
                PublicationCasResult::Updated => Ok(SyncReport {
                    rows,
                    changed: changed || retry_changed,
                }),
                PublicationCasResult::Conflict { .. } => {
                    bail!(
                        "Claude picker changed while synchronising; run `toker picker sync` again"
                    )
                }
            }
        }
        PublicationCasResult::Conflict { current: None } => {
            bail!("Claude picker publication disappeared while synchronising")
        }
    }
}

fn publication_error_state(error: &anyhow::Error) -> (PublicationState, &'static str) {
    if error.downcast_ref::<IncompatibleClaudeProfile>().is_some() {
        (
            PublicationState::Unsupported,
            "incompatible_profile_presentation",
        )
    } else {
        (PublicationState::Failed, "settings_render_failed")
    }
}

fn record_publication_result(
    store: &Store,
    key: &PublicationKey,
    publication: &RoutingPublication,
    state: PublicationState,
    detail_code: &str,
) {
    let update = PublicationResultUpdate {
        state,
        applied_identities: None,
        detail_code: Some(
            crate::policy::StableId::new(detail_code).expect("static publication failure code"),
        ),
        updated_ms: jiff::Timestamp::now().as_millisecond(),
    };
    if let Err(error) = store.compare_and_swap_publication_result(
        key,
        publication.desired_revision_id,
        publication.desired_inventory_generation,
        &update,
    ) {
        tracing::error!(%error, "could not record Claude picker publication result");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn legacy_picker_identity_adoption_is_limited_to_the_old_prefix() {
        let dir = crate::test_support::tempdir("picker-legacy-ownership-");
        let settings = dir.join("settings.json");
        std::fs::write(
            &settings,
            serde_json::to_string(&json!({"modelPicker": {"options": [
                {"model": "openrouter/old"},
                {"model": "claude-user"},
                {"model": 42}
            ]}}))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            legacy_picker_identities(&settings),
            BTreeSet::from(["openrouter/old".to_owned()])
        );
    }

    #[test]
    fn incompatible_anthropic_profile_presentations_are_rejected() {
        use crate::policy::{
            ArgumentBinding, CURRENT_FORMAT_VERSION, Candidate, CandidateId, CandidateModel,
            LaneTransitionPolicy, PolicyDocument, Presentation, PresentationId, PresentationSource,
            Route, RouteChain, RouteId, RouteReference,
        };
        use crate::routing::ANTHROPIC_SUB_MESSAGES;

        let requested_model = crate::policy::StableId::new("requested_model").unwrap();
        let route = Route {
            honours_server_safeguards: false,
            id: RouteId::new("route").unwrap(),
            display_name: "Route".to_owned(),
            description: None,
            parameters: vec![requested_model.clone()],
            chain: RouteChain {
                first: Candidate {
                    id: CandidateId::new("candidate").unwrap(),
                    binding_id: ANTHROPIC_SUB_MESSAGES.id().clone(),
                    model: CandidateModel::Exact {
                        model_id: "model".to_owned(),
                    },
                    upgrade_policy_id: None,
                    alias_policy: None,
                    required_capabilities: Default::default(),
                    enabled: true,
                    health_circuits: vec![],
                },
                steps: vec![],
            },
            lane_transition: LaneTransitionPolicy::PreservePinned,
        };
        let reference = RouteReference {
            route_id: route.id.clone(),
            parameters: BTreeMap::from([(requested_model, ArgumentBinding::RequestedModel)]),
        };
        let presentation = |id: &str, profile: &str, slug: &str| Presentation {
            id: PresentationId::new(id).unwrap(),
            scope: crate::policy::FrontendScope::Profile {
                profile: ProfileId::new(profile).unwrap(),
                protocol: FrontendProtocol::AnthropicMessages,
            },
            slug: slug.to_owned(),
            label: None,
            description: None,
            enabled: true,
            shadow_native: false,
            source: PresentationSource::Virtual {
                target: reference.clone(),
            },
            metadata: Some(AdapterMetadata::Claude {
                behaves_as: Some("claude-sonnet-5".to_owned()),
            }),
        };
        let policy = PolicyDocument {
            format_version: CURRENT_FORMAT_VERSION,
            description: None,
            defaults: vec![],
            presentations: vec![
                presentation("claude", "claude", "claude-model"),
                presentation("workhorse", "workhorse", "workhorse-model"),
            ],
            routes: vec![route],
            rewrites: vec![],
            classifiers: vec![],
            upgrades: vec![],
            compaction: None,
        }
        .validate()
        .unwrap();
        let topology = RouteRegistry::configured_bindings([ANTHROPIC_SUB_MESSAGES.id().clone()]);
        let compiled =
            crate::policy::compile(policy, &topology, &offers::OfferInventory::default())
                .into_policy()
                .unwrap();

        let error = policy_rows(&compiled).unwrap_err();
        assert!(error.downcast_ref::<IncompatibleClaudeProfile>().is_some());
    }

    #[test]
    fn rules_validate_at_load() {
        let mut rule = PickerRule::new(&["lab/*"], &[], "sonnet");
        assert!(rule.validate().is_ok());
        rule.keep = 0;
        assert!(rule.validate().is_err());
        rule.keep = 1;
        rule.variant = Some("floor".to_owned());
        assert!(rule.validate().is_err(), "a variant without its colon");
        rule.variant = None;
        rule.matches = vec!["lab/[".to_owned()];
        assert!(rule.validate().is_err(), "an unparseable glob");
    }
}
