//! The models tab's facts: for each upgrade policy the evidence it asks for
//! and the move it would make now, and where cold compactions go and on which
//! bindings (spec: UTUI#models).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::{Context, Inputs, reach_body};
use crate::policy::{
    BindingId, CompactionTarget, CompiledCandidate, InventorySource, LearningView, MinimumLearning,
    RouteId, SubstituteModel, UpgradePolicy, UpgradeRejection, upgrade_standing,
};

/// The upgrade policies, with where each is used, and the uses by route.
pub(super) struct Upgrades {
    rows: Vec<Value>,
    by_route: BTreeMap<RouteId, Vec<Value>>,
}

impl Upgrades {
    pub(super) fn body(&self) -> Value {
        Value::Array(self.rows.clone())
    }

    /// The uses of an upgrade policy by the candidates of one route, each with
    /// the moves it would make now.
    pub(super) fn moves_for(&self, route: &RouteId) -> Vec<Value> {
        self.by_route.get(route).cloned().unwrap_or_default()
    }
}

pub(super) fn upgrades<L: LearningView>(ctx: &Context<'_>, inputs: &Inputs<'_, L>) -> Upgrades {
    let mut rows = Vec::new();
    let mut by_route: BTreeMap<RouteId, Vec<Value>> = BTreeMap::new();
    for compiled in ctx.policy.upgrades() {
        let upgrade = compiled.policy();
        let mut used_by = Vec::new();
        for route in ctx.policy.routes() {
            for candidate in route.candidates() {
                if candidate.upgrade_policy_id() != Some(&upgrade.id) {
                    continue;
                }
                let (moves, held_back) = if upgrade.enabled && candidate.enabled() {
                    moves(ctx, inputs, candidate, upgrade)
                } else {
                    (Vec::new(), Vec::new())
                };
                let bar = bar_for(inputs, candidate.binding_id(), upgrade);
                let usage = json!({
                    "route_id": route.id(),
                    "candidate_id": candidate.id(),
                    "binding_id": candidate.binding_id(),
                    "provider": ctx.provider_of(candidate.binding_id()),
                    "candidate_enabled": candidate.enabled(),
                    "bar": bar,
                    "moves": moves,
                    "held_back": held_back,
                });
                by_route.entry(route.id().clone()).or_default().push(json!({
                    "policy_id": upgrade.id,
                    "candidate_id": candidate.id(),
                    "enabled": upgrade.enabled && candidate.enabled(),
                    "bar": usage["bar"],
                    "moves": usage["moves"],
                    "held_back": usage["held_back"],
                }));
                used_by.push(usage);
            }
        }
        rows.push(json!({
            "id": upgrade.id,
            "enabled": upgrade.enabled,
            "classifier_id": upgrade.classifier_id,
            "evidence_bar": evidence_bar(upgrade.minimum_learning),
            "allow_manual_promotion": upgrade.allow_manual_promotion,
            "inventory_sources": upgrade.inventory_sources,
            "lane_transition": upgrade.lane_transition,
            "used_by": used_by,
        }));
    }
    Upgrades { rows, by_route }
}

/// What a policy asks of a model before it may be moved to.
fn evidence_bar(minimum: MinimumLearning) -> Value {
    match minimum {
        MinimumLearning::None => json!({"kind": "none"}),
        MinimumLearning::DistinctDays { days } => json!({"kind": "distinct_days", "days": days}),
        MinimumLearning::ExplicitPromotion => json!({"kind": "explicit_promotion"}),
        MinimumLearning::Scaled => json!({"kind": "scaled"}),
    }
}

/// The bar as one binding sees it. Scaled depends on how many days the
/// binding has any evidence for, so it is only a number here: strictly more
/// distinct days than half of those, capped.
fn bar_for<L: LearningView>(
    inputs: &Inputs<'_, L>,
    binding: &BindingId,
    upgrade: &UpgradePolicy,
) -> Value {
    let mut bar = evidence_bar(upgrade.minimum_learning);
    if upgrade.minimum_learning == MinimumLearning::Scaled {
        let active_days = inputs.learning.active_days(binding).ok();
        bar["active_days"] = json!(active_days);
        bar["needs_days"] = json!(
            active_days
                .map(|active| { crate::policy::learning::requirement(active).floor() as u64 + 1 })
        );
    }
    if let MinimumLearning::DistinctDays { days } = upgrade.minimum_learning {
        bar["needs_days"] = json!(days);
    }
    bar
}

/// What the candidate's upgrade would do now, family by family: the move a
/// request on the family's older models would make, and the newest model
/// held back by the evidence bar where the bar is what stops one.
///
/// The baseline is chosen by the family rather than by a request, so this is
/// what a new lane with no prompt to fit would get; a request carries its own
/// baseline and prompt.
fn moves<L: LearningView>(
    ctx: &Context<'_>,
    inputs: &Inputs<'_, L>,
    candidate: &CompiledCandidate,
    upgrade: &UpgradePolicy,
) -> (Vec<Value>, Vec<Value>) {
    let Some(classifier) = ctx.policy.classifier(&upgrade.classifier_id) else {
        return (Vec::new(), Vec::new());
    };
    let binding = candidate.binding_id();
    let mut models: BTreeSet<String> = BTreeSet::new();
    for source in &upgrade.inventory_sources {
        match source {
            InventorySource::Catalogue => models.extend(
                ctx.inventory
                    .iter()
                    .filter(|offer| offer.binding_id == *binding)
                    .map(|offer| offer.backend_model_id.clone()),
            ),
            InventorySource::Served | InventorySource::Learned => {
                models.extend(inputs.learning.known_models(binding).unwrap_or_default());
            }
        }
    }
    let mut families: BTreeMap<String, Vec<(crate::policy::ParsedVersion, String)>> =
        BTreeMap::new();
    for model in models {
        if let Ok(Some(classified)) = classifier.classify(&model) {
            families
                .entry(classified.family)
                .or_default()
                .push((classified.version, model));
        }
    }
    let mut moves = Vec::new();
    let mut held_back = Vec::new();
    for (family, mut members) in families {
        members.sort_by(|left, right| {
            left.0
                .partial_cmp(&right.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.1.cmp(&right.1))
        });
        // With the oldest member as the baseline every other member is a
        // contender, so the winner is the newest the evidence bar and the
        // context allow.
        let oldest = &members[0].1;
        let Ok(Some(election)) = upgrade_standing(
            ctx.policy,
            ctx.inventory,
            inputs.learning,
            candidate,
            oldest,
        ) else {
            continue;
        };
        if let Some(winner) = election.winner {
            let at = members
                .iter()
                .position(|(_, model)| *model == winner)
                .unwrap_or(0);
            // The move a request would make from the model just below the
            // winner, which is the one most likely to be asked for.
            let from = &members[at.saturating_sub(1)].1;
            moves.push(json!({"family": family, "from": from, "to": winner}));
            continue;
        }
        let blocked = election
            .rejected
            .iter()
            .filter(|(_, reason)| *reason == UpgradeRejection::LearningMinimum)
            .filter_map(|(model, _)| members.iter().rev().position(|(_, member)| member == model))
            .min()
            .map(|from_newest| &members[members.len() - 1 - from_newest].1);
        if let Some(model) = blocked {
            let evidence = inputs.learning.learned(binding, model).ok().flatten();
            held_back.push(json!({
                "family": family,
                "model": model,
                "distinct_days": evidence.as_ref().map_or(0, |evidence| evidence.distinct_days().len()),
                "promoted": evidence.as_ref().is_some_and(|evidence| evidence.explicitly_promoted()),
            }));
        }
    }
    (moves, held_back)
}

/// Where cold compactions go: a route of their own, or a cheaper model on
/// whichever of some bindings the request reached (spec: RCOMP).
pub(super) fn compaction(ctx: &Context<'_>) -> Value {
    let Some(compaction) = &ctx.document.compaction else {
        return Value::Null;
    };
    let mut body = json!({
        "lane_transition": compaction.lane_transition,
        "cache_transition": compaction.cache_transition,
    });
    match &compaction.target {
        CompactionTarget::Route(target) => {
            body["kind"] = json!("route");
            body["route_id"] = json!(target.route_id);
            body["reach"] = ctx
                .reach
                .get(&target.route_id)
                .map_or(Value::Null, |reach| {
                    reach_body(ctx, reach, Some(&target.route_id))
                });
        }
        CompactionTarget::Substitution { model, bindings } => {
            body["kind"] = json!("substitution");
            body["model"] = json!(model.as_ref().map(|model| json!({
                "kind": match model {
                    SubstituteModel::Exact(_) => "exact",
                    SubstituteModel::Family(_) => "family",
                },
                "value": model.value(),
            })));
            body["bindings"] = json!(
                bindings
                    .iter()
                    .map(|binding| json!({
                        "binding_id": binding,
                        "provider": ctx.provider_of(binding),
                    }))
                    .collect::<Vec<_>>()
            );
            // The routes a compaction would be moved on: any holding a
            // candidate on one of the bindings.
            body["routes"] = json!(
                ctx.policy
                    .routes()
                    .filter(|route| route
                        .candidates()
                        .iter()
                        .any(|candidate| candidate.enabled()
                            && bindings.contains(candidate.binding_id())))
                    .map(|route| route.id())
                    .collect::<Vec<_>>()
            );
        }
    }
    body
}
