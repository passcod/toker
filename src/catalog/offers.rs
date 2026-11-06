//! Frontend-visible models are a legacy projection of canonical backend offers
//! through the implemented route graph, never one upstream's raw listing.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::fetched::{FetchedCatalogs, ModelChronology};
use crate::policy::BindingId;
use crate::routing::{BackendBinding, ProtocolId, RouteRegistry};

/// One provider-listed model on exactly one stable backend binding.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendOffer {
    pub binding_id: BindingId,
    pub provider: String,
    pub backend_model_id: String,
    pub binding: BackendBinding,
    pub context_window: Option<u64>,
    pub chronology: Option<ModelChronology>,
    pub fetched_at_ms: i64,
    pub raw: Value,
}

/// Canonical provider inventory, keyed without frontend aliases.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OfferInventory {
    by_backend: BTreeMap<BindingId, BTreeMap<String, BackendOffer>>,
}

impl OfferInventory {
    pub fn from_offers(offers: impl IntoIterator<Item = BackendOffer>) -> Self {
        let mut by_backend = BTreeMap::<BindingId, BTreeMap<String, BackendOffer>>::new();
        for offer in offers {
            by_backend
                .entry(offer.binding_id.clone())
                .or_default()
                .insert(offer.backend_model_id.clone(), offer);
        }
        Self { by_backend }
    }

    pub fn get(&self, binding_id: &BindingId, backend_model_id: &str) -> Option<&BackendOffer> {
        self.by_backend
            .get(binding_id)
            .and_then(|offers| offers.get(backend_model_id))
    }

    pub fn iter(&self) -> impl Iterator<Item = &BackendOffer> {
        self.by_backend.values().flat_map(|offers| offers.values())
    }

    pub fn len(&self) -> usize {
        self.by_backend.values().map(BTreeMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.by_backend.is_empty()
    }
}

/// Join configured providers' fetched catalogues to their stable bindings.
///
/// Each `(binding id, backend model id)` appears once. Frontend provider
/// prefixes and bare default aliases are deliberately absent.
pub fn inventory(registry: &RouteRegistry, catalogs: &FetchedCatalogs) -> OfferInventory {
    let mut by_backend = BTreeMap::new();
    for binding in registry.bindings() {
        if !registry.binding_is_configured(binding) {
            continue;
        }
        let provider = binding.provider();
        let Some(source) = FetchedCatalogs::source_of(provider) else {
            continue;
        };
        let Some(catalog) = catalogs.get(source) else {
            continue;
        };
        for model in &catalog.models {
            let binding_id = binding.id().clone();
            by_backend
                .entry(binding_id.clone())
                .or_insert_with(BTreeMap::new)
                .entry(model.id.clone())
                .or_insert_with(|| BackendOffer {
                    binding_id,
                    provider: provider.to_owned(),
                    backend_model_id: model.id.clone(),
                    binding: binding.clone(),
                    context_window: model.context_window,
                    chronology: model.chronology,
                    fetched_at_ms: catalog.fetched_at_ms,
                    raw: model.raw.clone(),
                });
        }
    }
    OfferInventory { by_backend }
}

/// The existing frontend model shape retained until policy publication owns
/// these surfaces. `routed_model_id` is projection metadata, not inventory
/// identity.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelOffer {
    pub provider: String,
    pub backend_model_id: String,
    pub routed_model_id: String,
    pub binding: BackendBinding,
    pub context_window: Option<u64>,
    pub chronology: Option<ModelChronology>,
    pub fetched_at_ms: i64,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PolicyPublication {
    Ready(Value),
    Unresolved,
}

pub fn render_policy(
    policy: &crate::policy::CompiledPolicy,
    inventory: &OfferInventory,
    profile: Option<&crate::policy::ProfileId>,
    protocol: crate::policy::FrontendProtocol,
) -> PolicyPublication {
    let publication = policy.publication(profile, protocol);
    let mut exact = Vec::new();
    let mut virtual_slugs = Vec::new();
    for (slug, presentation) in publication {
        match presentation.route().exact_offer() {
            Some((binding_id, model_id)) => {
                let Some(offer) = inventory.get(binding_id, model_id) else {
                    return PolicyPublication::Unresolved;
                };
                exact.push(ModelOffer {
                    provider: offer.provider.clone(),
                    backend_model_id: offer.backend_model_id.clone(),
                    routed_model_id: slug.to_owned(),
                    binding: offer.binding.clone(),
                    context_window: offer.context_window,
                    chronology: offer.chronology,
                    fetched_at_ms: offer.fetched_at_ms,
                    raw: offer.raw.clone(),
                });
            }
            None => virtual_slugs.push(slug.to_owned()),
        }
    }
    exact.sort_by(|left, right| left.routed_model_id.cmp(&right.routed_model_id));
    match protocol {
        crate::policy::FrontendProtocol::OpenaiChat => {
            let mut data = render_chat(&exact)
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            data.extend(
                virtual_slugs
                    .into_iter()
                    .map(|slug| json!({"id": slug, "object": "model", "owned_by": "toker"})),
            );
            data.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
            PolicyPublication::Ready(json!({"object": "list", "data": data}))
        }
        crate::policy::FrontendProtocol::OpenaiResponses if virtual_slugs.is_empty() => {
            PolicyPublication::Ready(render_responses(&exact))
        }
        crate::policy::FrontendProtocol::OpenaiResponses => PolicyPublication::Unresolved,
        crate::policy::FrontendProtocol::AnthropicMessages => {
            let mut data: Vec<Value> = exact
                .iter()
                .map(|offer| anthropic_entry(&offer.routed_model_id, &offer.raw))
                .collect();
            data.extend(
                virtual_slugs
                    .into_iter()
                    .map(|slug| anthropic_entry(&slug, &Value::Null)),
            );
            data.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
            let first = data.first().map(|entry| entry["id"].clone());
            let last = data.last().map(|entry| entry["id"].clone());
            PolicyPublication::Ready(json!({
                "data": data,
                "has_more": false,
                "first_id": first,
                "last_id": last,
            }))
        }
    }
}

/// One entry of Anthropic's models list. `display_name` and `created_at`
/// are the provider's own when its listing carried them; a published slug
/// with no listing entry (a virtual model) names itself and has no creation
/// time rather than a made-up one.
fn anthropic_entry(slug: &str, raw: &Value) -> Value {
    let mut entry = Map::new();
    entry.insert("type".to_owned(), json!("model"));
    entry.insert("id".to_owned(), json!(slug));
    let display = raw
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or(slug);
    entry.insert("display_name".to_owned(), json!(display));
    if let Some(created) = raw.get("created_at").and_then(Value::as_str) {
        entry.insert("created_at".to_owned(), json!(created));
    }
    Value::Object(entry)
}

/// Project canonical offers into the legacy frontend aliases. Equal slugs on
/// two providers never merge. Staleness remains in `fetched_at_ms`, not a
/// made-up fresh claim.
pub fn for_frontend(
    registry: &RouteRegistry,
    catalogs: &FetchedCatalogs,
    frontend: ProtocolId,
) -> Vec<ModelOffer> {
    for_frontend_inventory(registry, &inventory(registry, catalogs), frontend)
}

/// Project one already-captured canonical inventory into the legacy frontend
/// aliases. This is the shared source for both the authoritative model-list
/// response and revision-one policy publication.
pub fn for_frontend_inventory(
    registry: &RouteRegistry,
    inventory: &OfferInventory,
    frontend: ProtocolId,
) -> Vec<ModelOffer> {
    let mut offers = Vec::new();
    for offer in inventory.iter() {
        let routed = format!("{}/{}", offer.provider, offer.backend_model_id);
        if let Some(projected) = projected(registry, offer, frontend, routed) {
            offers.push(projected);
        }
        if let Some(projected) =
            projected(registry, offer, frontend, offer.backend_model_id.clone())
        {
            offers.push(projected);
        }
    }
    offers.sort_by(|a, b| a.routed_model_id.cmp(&b.routed_model_id));
    offers.dedup_by(|a, b| a.routed_model_id == b.routed_model_id);
    offers
}

fn projected(
    registry: &RouteRegistry,
    offer: &BackendOffer,
    frontend: ProtocolId,
    routed_model_id: String,
) -> Option<ModelOffer> {
    // Codex's catalogue parser requires a full ModelInfo shape, not just a
    // slug. A foreign provider is routable explicitly, but not advertised
    // until a verified frontend metadata projection exists for it.
    if frontend == ProtocolId::OpenAiResponses && offer.provider != "codex_sub" {
        return None;
    }
    let target = registry.resolve(frontend, Some(&routed_model_id)).ok()?;
    if target.provider().id() != offer.provider
        || target.binding() != &offer.binding
        || target.effective_model() != Some(offer.backend_model_id.as_str())
    {
        return None;
    }
    Some(ModelOffer {
        provider: offer.provider.clone(),
        backend_model_id: offer.backend_model_id.clone(),
        routed_model_id,
        binding: offer.binding.clone(),
        context_window: offer.context_window,
        chronology: offer.chronology,
        fetched_at_ms: offer.fetched_at_ms,
        raw: offer.raw.clone(),
    })
}

/// The OpenAI Chat models list. Do not copy a provider's whole entry into
/// this shape: only fields whose meaning this renderer knows are emitted.
pub fn render_chat(offers: &[ModelOffer]) -> Value {
    let data: Vec<Value> = offers
        .iter()
        .map(|offer| {
            let mut entry = Map::new();
            entry.insert("id".to_owned(), json!(offer.routed_model_id));
            entry.insert("object".to_owned(), json!("model"));
            entry.insert("owned_by".to_owned(), json!(offer.provider));
            if let Some(created) = offer.raw.get("created").and_then(Value::as_i64) {
                entry.insert("created".to_owned(), json!(created));
            }
            Value::Object(entry)
        })
        .collect();
    json!({ "object": "list", "data": data })
}

/// The Codex CLI's models shape. A Codex entry is already in this dialect,
/// so keep its provider-owned metadata and change only the routed slug.
/// Other provider entries are not projected until their Responses binding
/// and this frontend shape have been verified together.
pub fn retain_publication(
    offers: &mut Vec<ModelOffer>,
    slugs: &std::collections::BTreeSet<String>,
) {
    offers.retain(|offer| slugs.contains(&offer.routed_model_id));
}

pub fn render_responses(offers: &[ModelOffer]) -> Value {
    let models: Vec<Value> = offers
        .iter()
        .filter(|offer| offer.provider == "codex_sub")
        .filter_map(|offer| {
            let mut entry = offer.raw.as_object()?.clone();
            entry.insert("slug".to_owned(), json!(offer.routed_model_id));
            Some(Value::Object(entry))
        })
        .collect();
    json!({ "models": models })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::catalog::fetched::FetchedModel;
    use crate::providers::{OpenRouter, Provider};
    use crate::routing::{OPENROUTER_CHAT, OPENROUTER_MESSAGES};

    fn registry() -> RouteRegistry {
        let provider: Arc<dyn Provider> = Arc::new(OpenRouter::new(
            "https://openrouter.ai/api/v1".parse().expect("url"),
            None,
        ));
        RouteRegistry::new(
            [provider],
            [
                (ProtocolId::AnthropicMessages, "openrouter".to_owned()),
                (ProtocolId::OpenAiChat, "openrouter".to_owned()),
            ],
        )
    }

    fn catalogs() -> FetchedCatalogs {
        let mut catalogs = FetchedCatalogs::default();
        catalogs.set(
            "openrouter",
            super::super::fetched::FetchedCatalog {
                fetched_at_ms: 42,
                models: vec![FetchedModel {
                    id: "vendor/model".to_owned(),
                    context_window: Some(123_456),
                    chronology: ModelChronology::from_unix_seconds(7),
                    raw: json!({"id": "vendor/model", "created": 7}),
                }],
            },
        );
        catalogs
    }

    #[test]
    fn policy_publication_filters_without_changing_offer_metadata() {
        let mut offers = for_frontend(&registry(), &catalogs(), ProtocolId::OpenAiChat);
        let kept = offers
            .iter()
            .find(|offer| offer.routed_model_id == "openrouter/vendor/model")
            .cloned()
            .expect("prefixed projection");
        retain_publication(
            &mut offers,
            &["openrouter/vendor/model".to_owned()].into_iter().collect(),
        );
        assert_eq!(offers, vec![kept]);
    }

    #[test]
    fn canonical_inventory_keys_models_by_binding_without_aliases() {
        let inventory = inventory(&registry(), &catalogs());
        assert_eq!(inventory.len(), 3);
        for binding in [OPENROUTER_CHAT, OPENROUTER_MESSAGES] {
            let offer = inventory
                .get(binding.id(), "vendor/model")
                .expect("canonical offer");
            assert_eq!(offer.backend_model_id, "vendor/model");
            assert_eq!(offer.fetched_at_ms, 42);
            assert_eq!(offer.context_window, Some(123_456));
            assert_eq!(offer.raw["created"], 7);
        }
        assert!(
            inventory
                .iter()
                .all(|offer| !offer.backend_model_id.starts_with("openrouter/"))
        );
    }

    #[test]
    fn legacy_projection_adds_prefix_and_default_alias_without_changing_metadata() {
        let offers = for_frontend(&registry(), &catalogs(), ProtocolId::OpenAiChat);
        assert_eq!(offers.len(), 2);
        assert_eq!(offers[0].routed_model_id, "openrouter/vendor/model");
        assert_eq!(offers[1].routed_model_id, "vendor/model");
        assert!(offers.iter().all(|offer| offer.fetched_at_ms == 42));
        assert!(offers.iter().all(|offer| offer.raw["created"] == 7));
    }
}
