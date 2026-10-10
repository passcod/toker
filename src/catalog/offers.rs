//! Frontend-visible models are a legacy projection of canonical backend offers
//! through the implemented route graph, never one upstream's raw listing.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::fetched::FetchedCatalogs;
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
    pub fetched_at_ms: i64,
    pub raw: Value,
}

/// Canonical provider inventory, keyed without frontend aliases.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OfferInventory {
    by_backend: BTreeMap<(BindingId, String), BackendOffer>,
}

impl OfferInventory {
    pub fn from_offers(offers: impl IntoIterator<Item = BackendOffer>) -> Self {
        Self {
            by_backend: offers
                .into_iter()
                .map(|offer| {
                    (
                        (offer.binding_id.clone(), offer.backend_model_id.clone()),
                        offer,
                    )
                })
                .collect(),
        }
    }

    pub fn get(&self, binding_id: &BindingId, backend_model_id: &str) -> Option<&BackendOffer> {
        self.by_backend
            .get(&(binding_id.clone(), backend_model_id.to_owned()))
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &BackendOffer> {
        self.by_backend.values()
    }

    pub fn len(&self) -> usize {
        self.by_backend.len()
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
            let key = (binding_id.clone(), model.id.clone());
            by_backend.entry(key).or_insert_with(|| BackendOffer {
                binding_id,
                provider: provider.to_owned(),
                backend_model_id: model.id.clone(),
                binding: binding.clone(),
                context_window: model.context_window,
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
    pub fetched_at_ms: i64,
    pub raw: Value,
}

/// Project canonical offers into the legacy frontend aliases. Equal slugs on
/// two providers never merge. Staleness remains in `fetched_at_ms`, not a
/// made-up fresh claim.
pub fn for_frontend(
    registry: &RouteRegistry,
    catalogs: &FetchedCatalogs,
    frontend: ProtocolId,
) -> Vec<ModelOffer> {
    let inventory = inventory(registry, catalogs);
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
                    raw: json!({"id": "vendor/model", "created": 7}),
                }],
            },
        );
        catalogs
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
