//! What in routing depends on each provider: the presentations, rewrites,
//! defaults and routes that reach it through any offer or candidate, a
//! fallback included, so turning it off shows its full reach
//! (spec: UTUI#providers).
//!
//! A candidate that is switched off in the policy does not count, since a
//! provider going away changes nothing for it.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::Context;
use super::rules::{Referrer, Rules};

pub(super) fn providers_body(ctx: &Context<'_>, rules: &Rules) -> Value {
    // Every provider the availability knows, so one nothing reaches still has
    // a row saying so.
    let mut providers: BTreeSet<String> = ctx
        .availability
        .iter()
        .map(|(_, live)| live.provider.clone())
        .collect();
    for route in ctx.policy.routes() {
        for candidate in route.candidates() {
            providers.insert(ctx.provider_of(candidate.binding_id()));
        }
    }
    Value::Array(
        providers
            .iter()
            .map(|provider| provider_body(ctx, rules, provider))
            .collect(),
    )
}

fn provider_body(ctx: &Context<'_>, rules: &Rules, provider: &str) -> Value {
    let routes: Vec<&crate::policy::RouteId> = ctx
        .policy
        .routes()
        .filter(|route| {
            route.candidates().iter().any(|candidate| {
                candidate.enabled() && ctx.provider_of(candidate.binding_id()) == provider
            })
        })
        .map(|route| route.id())
        .collect();

    // Each record once, whichever way it reaches the provider.
    let mut referrers: Vec<&Referrer> = Vec::new();
    for route in &routes {
        referrers.extend(rules.to_route.get(*route).into_iter().flatten());
    }
    referrers.extend(
        rules
            .to_offer
            .iter()
            .filter(|(_, binding)| ctx.provider_of(binding) == provider)
            .map(|(referrer, _)| referrer),
    );
    let mut seen: BTreeSet<(&'static str, String)> = BTreeSet::new();
    referrers.retain(|referrer| {
        let key = referrer
            .id
            .clone()
            .unwrap_or_else(|| super::scope_label(&referrer.scope));
        seen.insert((referrer.kind, key))
    });

    let of_kind = |kind: &str| referrers.iter().filter(move |referrer| referrer.kind == kind);
    let mut by_protocol: BTreeMap<&str, usize> = BTreeMap::new();
    for rewrite in of_kind("rewrite") {
        *by_protocol
            .entry(super::super::routing::protocol_name(super::protocol_of(
                &rewrite.scope,
            )))
            .or_default() += 1;
    }
    let bindings: Vec<Value> = ctx
        .availability
        .iter()
        .filter(|(_, live)| live.provider == provider)
        .map(|(binding, live)| {
            json!({
                "binding_id": binding,
                "cost_class": super::cost_name(live.cost_class),
            })
        })
        .collect();
    json!({
        "provider": provider,
        "bindings": bindings,
        "presentations": {
            "count": of_kind("presentation").count(),
            "ids": of_kind("presentation").map(|referrer| &referrer.id).collect::<Vec<_>>(),
        },
        "rewrites": {
            "count": of_kind("rewrite").count(),
            "by_protocol": by_protocol,
            "ids": of_kind("rewrite").map(|referrer| &referrer.id).collect::<Vec<_>>(),
        },
        "defaults": {
            "count": of_kind("default").count(),
            "scopes": of_kind("default")
                .map(|referrer| super::scope_label(&referrer.scope))
                .collect::<Vec<_>>(),
        },
        "routes": {"count": routes.len(), "ids": routes},
    })
}
