//! One route as the routes tab and the route page read it: its candidates and
//! the edges between them, the lanes it holds, its reach, what sends requests
//! to it, and the blocks that handle a request on it
//! (spec: UTUI#routes).

use serde_json::{Value, json};

use super::rules::{Referrer, Rules};
use super::{Context, Inputs, cost_name, lanes_body, matcher_body, reach_body, rejection_name};
use crate::policy::{
    BindingId, CapabilityFlags, CompactionTarget, CompiledCandidateModel, CompiledRoute,
    LearningView, upgrade_standing,
};

pub(super) fn route_body<L: LearningView>(
    ctx: &Context<'_>,
    route: &CompiledRoute,
    inputs: &Inputs<'_, L>,
    rules: &Rules,
) -> Value {
    let policy = ctx.policy;
    let candidates: Vec<Value> = route
        .candidates()
        .iter()
        .map(|candidate| {
            // The baseline is only known up front for a fixed model; a
            // template, selector or requested-model candidate gets its
            // baseline from the request.
            let standing = match candidate.model() {
                CompiledCandidateModel::Exact(baseline) => upgrade_standing(
                    policy,
                    inputs.inventory,
                    inputs.learning,
                    candidate,
                    baseline,
                )
                .ok()
                .flatten()
                .map(|election| {
                    json!({
                        "baseline": baseline,
                        "winner": election.winner,
                        "rejected": election
                            .rejected
                            .iter()
                            .map(|(model, reason)| json!({
                                "model": model,
                                "reason": rejection_name(*reason),
                            }))
                            .collect::<Vec<_>>(),
                    })
                }),
                _ => None,
            };
            json!({
                "id": candidate.id(),
                "binding_id": candidate.binding_id(),
                "provider": ctx.provider_of(candidate.binding_id()),
                "enabled": candidate.enabled(),
                "cost_class": cost_name(ctx.cost_of(candidate.binding_id())),
                "upgrade_policy_id": candidate.upgrade_policy_id(),
                "upgrade": standing,
            })
        })
        .collect();
    let edges: Vec<Value> = route
        .edges()
        .iter()
        .enumerate()
        .map(|(index, edge)| {
            let from = ctx.cost_of(route.candidates()[index].binding_id());
            let to = ctx.cost_of(route.candidates()[index + 1].binding_id());
            json!({
                "from": route.candidates()[index].id(),
                "to": route.candidates()[index + 1].id(),
                "from_cost": cost_name(from),
                "to_cost": cost_name(to),
                "crosses_cost_boundary": from == crate::policy::CostClass::Subscription
                    && matches!(
                        to,
                        crate::policy::CostClass::Billed | crate::policy::CostClass::Unknown
                    ),
                "billed_boundary": edge.billed_boundary,
                "ambiguous_transport": edge.ambiguous_transport,
                "post_send": edge.post_send,
                "cache_transition": edge.cache_transition,
                "preflight": edge.preflight,
            })
        })
        .collect();
    let matched_by = rules
        .to_route
        .get(route.id())
        .map_or(&[][..], Vec::as_slice);
    // The providers the candidates are on, in order and each once, as the
    // routes table writes the chain (`anthropic_sub → openrouter`).
    let mut providers: Vec<String> = Vec::new();
    for candidate in route.candidates() {
        let provider = ctx.provider_of(candidate.binding_id());
        if !providers.contains(&provider) {
            providers.push(provider);
        }
    }
    json!({
        "id": route.id(),
        "pinned_lanes": lanes_body(inputs.pinned.get(route.id())),
        "candidates": candidates,
        "edges": edges,
        "providers": providers,
        "reach": reach_body(
            ctx,
            ctx.reach.get(route.id()).expect("every route has a reach"),
            Some(route.id()),
        ),
        "lane_transition": route.lane_transition(),
        "matched_by": matched_by.iter().map(Referrer::body).collect::<Vec<_>>(),
        "matched_by_counts": {
            "presentations": count(matched_by, "presentation"),
            "rewrites": count(matched_by, "rewrite"),
            "defaults": count(matched_by, "default"),
        },
        "handled_by": handled_by(ctx, route),
    })
}

fn count(referrers: &[Referrer], kind: &str) -> usize {
    referrers
        .iter()
        .filter(|referrer| referrer.kind == kind)
        .count()
}

/// The blocks that handle a request on this route, each with whether it is
/// enabled and what it is set to, so the page can name the ones that are on
/// with their setting and the ones that are not.
///
/// Read from the document, since it holds what compilation closes over: the
/// alias rules as written, and the circuits.
fn handled_by(ctx: &Context<'_>, route: &CompiledRoute) -> Value {
    let Some(document) = ctx
        .document
        .routes
        .iter()
        .find(|document| document.id == *route.id())
    else {
        return Value::Null;
    };
    let candidates: Vec<_> = document.chain.candidates().collect();

    let upgrade_candidates: Vec<Value> = candidates
        .iter()
        .filter_map(|candidate| {
            let id = candidate.upgrade_policy_id.as_ref()?;
            let enabled = ctx
                .policy
                .upgrade(id)
                .is_some_and(|upgrade| upgrade.policy().enabled);
            Some(json!({
                "candidate_id": candidate.id,
                "policy_id": id,
                "enabled": enabled && candidate.enabled,
            }))
        })
        .collect();

    let alias: Vec<Value> = candidates
        .iter()
        .filter_map(|candidate| {
            let rules = &candidate.alias_policy.as_ref()?.rules;
            (!rules.is_empty()).then(|| {
                json!({
                    "candidate_id": candidate.id,
                    "binding_id": candidate.binding_id,
                    "provider": ctx.provider_of(&candidate.binding_id),
                    "rules": rules.iter().map(|rule| json!({
                        "match": matcher_body(&rule.matcher),
                        "exclude": rule.exclude.iter().map(matcher_body).collect::<Vec<_>>(),
                        "to": rule.replacement,
                    })).collect::<Vec<_>>(),
                })
            })
        })
        .collect();

    let required: Vec<Value> = candidates
        .iter()
        .filter_map(|candidate| {
            let names = capability_names(candidate.required_capabilities);
            (!names.is_empty())
                .then(|| json!({"candidate_id": candidate.id, "capabilities": names}))
        })
        .collect();

    let circuits: Vec<Value> = candidates
        .iter()
        .filter(|candidate| !candidate.health_circuits.is_empty())
        .map(|candidate| {
            json!({
                "candidate_id": candidate.id,
                "circuits": candidate.health_circuits.iter().map(|circuit| json!({
                    "failure": circuit.failure,
                    "seconds": circuit.duration_seconds,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    let safeguards: Vec<Value> = candidates
        .iter()
        .map(|candidate| {
            let live = ctx.availability.get(&candidate.binding_id);
            json!({
                "candidate_id": candidate.id,
                "binding_id": candidate.binding_id,
                "support": live.map(|live| live.capabilities.server_safeguards.as_str()),
                "marked": live.is_some_and(|live| live.safeguards_marked),
            })
        })
        .collect();

    json!({
        "upgrade": {
            "enabled": upgrade_candidates.iter().any(|candidate| candidate["enabled"] == json!(true)),
            "candidates": upgrade_candidates,
        },
        "compaction": compaction_on(ctx, route),
        "alias": {"enabled": !alias.is_empty(), "candidates": alias},
        "server_safeguards": {
            "enabled": document.honours_server_safeguards,
            "candidates": safeguards,
        },
        "required_capabilities": {"enabled": !required.is_empty(), "candidates": required},
        "health_circuits": {"enabled": !circuits.is_empty(), "candidates": circuits},
    })
}

/// Whether the policy's compaction applies on this route: it sends cold
/// compactions here, or moves them on a binding this route holds.
fn compaction_on(ctx: &Context<'_>, route: &CompiledRoute) -> Value {
    let Some(compaction) = &ctx.document.compaction else {
        return json!({"enabled": false});
    };
    let held: Vec<&BindingId> = route
        .candidates()
        .iter()
        .filter(|candidate| candidate.enabled())
        .map(|candidate| candidate.binding_id())
        .collect();
    match &compaction.target {
        CompactionTarget::Route(target) if target.route_id == *route.id() => json!({
            "enabled": true,
            "kind": "route",
            "lane_transition": compaction.lane_transition,
            "cache_transition": compaction.cache_transition,
        }),
        CompactionTarget::Substitution { model, bindings } => {
            let here: Vec<&BindingId> = bindings
                .iter()
                .filter(|binding| held.contains(binding))
                .collect();
            if here.is_empty() {
                json!({"enabled": false})
            } else {
                json!({
                    "enabled": true,
                    "kind": "substitution",
                    "model": model.as_ref().map(|model| json!({
                        "kind": match model {
                            crate::policy::SubstituteModel::Exact(_) => "exact",
                            crate::policy::SubstituteModel::Family(_) => "family",
                        },
                        "value": model.value(),
                    })),
                    "bindings": here,
                    "cache_transition": compaction.cache_transition,
                })
            }
        }
        CompactionTarget::Route(_) => json!({"enabled": false}),
    }
}

fn capability_names(flags: CapabilityFlags) -> Vec<&'static str> {
    [
        ("sampling", flags.sampling),
        ("system_in_messages", flags.system_in_messages),
        ("thinking_replay", flags.thinking_replay),
        ("images", flags.images),
        ("tools", flags.tools),
        ("tool_choice_none", flags.tool_choice_none),
        ("tool_choice_named", flags.tool_choice_named),
        ("tool_choice_unnamed", flags.tool_choice_unnamed),
        ("tool_choice_other", flags.tool_choice_other),
    ]
    .into_iter()
    .filter_map(|(name, required)| required.then_some(name))
    .collect()
}
