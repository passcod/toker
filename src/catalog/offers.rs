//! Frontend-visible models are a projection of fetched provider catalogues
//! through the implemented route graph, never one upstream's raw listing.

use serde_json::{Map, Value, json};

use super::fetched::{FetchedCatalog, FetchedCatalogs, FetchedModel};
use crate::routing::{BackendBinding, ProtocolId, RouteRegistry};

/// One provider-listed model with a route that can actually serve it.
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

/// Join configured catalogues to verified, implemented routes. The bare
/// default alias is a separate offer; equal slugs on two providers never
/// merge. Staleness remains in `fetched_at_ms`, not a made-up fresh claim.
pub fn for_frontend(
    registry: &RouteRegistry,
    catalogs: &FetchedCatalogs,
    frontend: ProtocolId,
) -> Vec<ModelOffer> {
    let mut offers = Vec::new();
    for provider in ["anthropic_sub", "anthropic_api", "openrouter", "codex_sub"] {
        if registry.provider(provider).is_none() {
            continue;
        }
        let Some(source) = FetchedCatalogs::source_of(provider) else {
            continue;
        };
        let Some(catalog) = catalogs.get(source) else {
            continue;
        };
        for model in &catalog.models {
            let routed = format!("{provider}/{}", model.id);
            if let Some(offer) = offered(registry, catalog, model, frontend, provider, routed) {
                offers.push(offer);
            }
            // Only the configured protocol default gets a bare alias.
            if let Some(offer) = offered(
                registry,
                catalog,
                model,
                frontend,
                provider,
                model.id.clone(),
            ) {
                offers.push(offer);
            }
        }
    }
    offers.sort_by(|a, b| a.routed_model_id.cmp(&b.routed_model_id));
    offers.dedup_by(|a, b| a.routed_model_id == b.routed_model_id);
    offers
}

fn offered(
    registry: &RouteRegistry,
    catalog: &FetchedCatalog,
    model: &FetchedModel,
    frontend: ProtocolId,
    provider: &str,
    routed_model_id: String,
) -> Option<ModelOffer> {
    let target = registry.resolve(frontend, Some(&routed_model_id)).ok()?;
    if target.provider().id() != provider || target.effective_model() != Some(model.id.as_str()) {
        return None;
    }
    Some(ModelOffer {
        provider: provider.to_owned(),
        backend_model_id: model.id.clone(),
        routed_model_id,
        binding: target.binding(),
        context_window: model.context_window,
        fetched_at_ms: catalog.fetched_at_ms,
        raw: model.raw.clone(),
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
