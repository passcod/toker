//! Shared deterministic offer filtering and ordering.

use std::collections::BTreeMap;

use crate::catalog::offers::{BackendOffer, OfferInventory};

use super::{BindingId, ClassifierId, CompiledSelector, ParsedVersion, SelectorOrdering};

/// Select at most `limit` offers after applying one compiled selector.
///
/// Classified versions are computed once per candidate before ordering. A
/// missing classification sorts after a classified version; model id breaks
/// every remaining tie deterministically.
pub(crate) fn select<'a>(
    inventory: &'a OfferInventory,
    binding: Option<&BindingId>,
    selector: &CompiledSelector,
    limit: usize,
    mut classify: impl FnMut(&ClassifierId, &BackendOffer) -> Option<ParsedVersion>,
) -> Vec<&'a BackendOffer> {
    let mut offers: Vec<(&BackendOffer, Option<ParsedVersion>)> = inventory
        .iter()
        .filter(|offer| binding.is_none_or(|binding| &offer.binding_id == binding))
        .filter(|offer| {
            selector.include().is_empty()
                || selector
                    .include()
                    .iter()
                    .any(|matcher| matcher.is_match(&offer.backend_model_id).unwrap_or(false))
        })
        .filter(|offer| {
            !selector
                .exclude()
                .iter()
                .any(|matcher| matcher.is_match(&offer.backend_model_id).unwrap_or(false))
        })
        .map(|offer| {
            let version = match selector.ordering() {
                SelectorOrdering::ClassifiedVersion { classifier_id } => {
                    classify(classifier_id, offer)
                }
                _ => None,
            };
            (offer, version)
        })
        .collect();

    match selector.ordering() {
        SelectorOrdering::CatalogueChronology => offers.sort_by(|(left, _), (right, _)| {
            // Known provider chronology always outranks missing chronology.
            // Fetch time is catalogue freshness, not model age.
            right
                .chronology
                .cmp(&left.chronology)
                .then_with(|| left.backend_model_id.cmp(&right.backend_model_id))
        }),
        SelectorOrdering::Explicit { model_ids } => {
            let positions: BTreeMap<_, _> = model_ids
                .iter()
                .enumerate()
                .map(|(index, id)| (id.as_str(), index))
                .collect();
            offers.retain(|(offer, _)| positions.contains_key(offer.backend_model_id.as_str()));
            offers.sort_by_key(|(offer, _)| positions[offer.backend_model_id.as_str()]);
        }
        SelectorOrdering::ClassifiedVersion { .. } => {
            // A classified-version selector selects only offers for which its
            // matcher, templates, and parser all succeeded. Unclassified ids
            // are not an older version and cannot win by lexical accident.
            offers.retain(|(_, version)| version.is_some());
            offers.sort_by(|(left, left_version), (right, right_version)| {
                right_version
                    .partial_cmp(left_version)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| left.backend_model_id.cmp(&right.backend_model_id))
            });
        }
    }

    offers
        .into_iter()
        .take(limit)
        .map(|(offer, _)| offer)
        .collect()
}

pub(crate) fn winner<'a>(
    inventory: &'a OfferInventory,
    binding: &BindingId,
    selector: &CompiledSelector,
    classify: impl FnMut(&ClassifierId, &BackendOffer) -> Option<ParsedVersion>,
) -> Option<&'a BackendOffer> {
    select(inventory, Some(binding), selector, 1, classify)
        .into_iter()
        .next()
}
