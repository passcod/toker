//! A realistic routing snapshot for the page's tests: the policy document,
//! the facts the daemon derives from it, the catalogue offers and where
//! each client's settings stand.
//!
//! It is the body `routing-snapshot` answers with, written by hand in the
//! shape `control::derived` builds, around the same invented world the
//! design drawings use: three protocols, five providers, seven routes and
//! fourteen rewrites. The names are made up.

use serde_json::{Value, json};

pub(super) const SUB: &str = "anthropic_sub.anthropic_messages";
pub(super) const API: &str = "anthropic_api.anthropic_messages";
pub(super) const OR_MESSAGES: &str = "openrouter.openrouter_messages";
pub(super) const OR_CHAT: &str = "openrouter.openrouter_chat_completions";
pub(super) const OR_RESPONSES: &str = "openrouter.openrouter_responses";
pub(super) const OPENAI: &str = "openai_api.openai_chat_completions";
pub(super) const CODEX: &str = "codex_sub.codex_responses";

/// How the world is set up.
#[derive(Debug, Clone, Copy)]
pub(super) struct World {
    /// A default for the `unprefixed` profile of the messages and chat
    /// protocols, ahead of each protocol's own.
    pub(super) profile_defaults: bool,
    /// A rule of its own for the `unprefixed` profile of the messages
    /// protocol, so that its group ends by passing to the protocol's.
    pub(super) profile_rules: bool,
    /// The state after a save: a glob rule added to the messages protocol,
    /// revision 10 live, two warm lanes still on revision 9 and the claude
    /// settings not yet written.
    pub(super) saved: bool,
    /// A fresh install: a live revision holding no record at all.
    pub(super) empty: bool,
}

impl World {
    pub(super) const AT_REST: World = World {
        profile_defaults: false,
        saved: false,
        empty: false,
        profile_rules: false,
    };
    pub(super) const FULL: World = World {
        profile_defaults: true,
        saved: false,
        empty: false,
        profile_rules: false,
    };
    pub(super) const PROFILED: World = World {
        profile_defaults: true,
        profile_rules: true,
        saved: false,
        empty: false,
    };
    pub(super) const SAVED: World = World {
        profile_defaults: false,
        saved: true,
        empty: false,
        profile_rules: false,
    };
    pub(super) const EMPTY: World = World {
        profile_defaults: false,
        saved: false,
        empty: true,
        profile_rules: false,
    };
}

/// A provider's state: whether it is on and whether its credential resolves.
fn provider(provider: &str, binding: &str, enabled: bool, credential: bool, cost: &str) -> Value {
    let evidence = |yes: bool, good: &str, bad: &str| {
        if yes {
            json!({"state": "yes", "reason": good})
        } else {
            json!({"state": "no", "reason": bad})
        }
    };
    json!({
        "binding_id": binding,
        "provider": provider,
        "configured": {"state": "yes", "reason": "configured"},
        "enabled": evidence(enabled, "enabled", "disabled"),
        "credential": evidence(credential, "present", "absent"),
        "catalogue": {"state": "fresh", "fetched_at_ms": 1_760_000_000_000_i64},
        "capabilities": {"sampling": "supported", "tools": "supported", "images": "supported"},
        "cost_class": cost,
    })
}

fn availability() -> Vec<Value> {
    vec![
        provider("anthropic_sub", SUB, true, false, "subscription"),
        provider("anthropic_api", API, false, false, "billed"),
        provider("openrouter", OR_MESSAGES, true, true, "billed"),
        provider("openrouter", OR_CHAT, true, true, "billed"),
        provider("openrouter", OR_RESPONSES, true, true, "billed"),
        provider("openai_api", OPENAI, false, false, "billed"),
        provider("codex_sub", CODEX, false, true, "subscription"),
    ]
}

/// One candidate of a route.
struct Candidate {
    id: &'static str,
    binding: &'static str,
    provider: &'static str,
    cost: &'static str,
}

const fn cand(
    id: &'static str,
    binding: &'static str,
    provider: &'static str,
    cost: &'static str,
) -> Candidate {
    Candidate {
        id,
        binding,
        provider,
        cost,
    }
}

fn serving(candidate: &Candidate) -> Value {
    json!({
        "candidate_id": candidate.id,
        "binding_id": candidate.binding,
        "provider": candidate.provider,
        "cost": candidate.cost,
        "billed": candidate.cost == "billed",
    })
}

fn skipped(candidate: &Candidate, reason: &str) -> Value {
    json!({
        "candidate_id": candidate.id,
        "binding_id": candidate.binding,
        "provider": candidate.provider,
        "reason": reason,
    })
}

fn served_first(route: &str, first: &Candidate) -> Value {
    json!({
        "outcome": "served_first", "refused": false, "route_id": route,
        "serving": serving(first), "skipped": [], "edge": null, "cause": null,
    })
}

fn served_later(route: &str, passed: (&Candidate, &str), then: &Candidate) -> Value {
    json!({
        "outcome": "served_later", "refused": false, "route_id": route,
        "serving": serving(then), "skipped": [skipped(passed.0, passed.1)],
        "edge": null, "cause": null,
    })
}

fn unavailable(route: &str, passed: (&Candidate, &str)) -> Value {
    json!({
        "outcome": "refused_unavailable", "refused": true, "route_id": route,
        "serving": null, "skipped": [skipped(passed.0, passed.1)],
        "edge": null, "cause": null,
    })
}

fn no_route() -> Value {
    json!({
        "outcome": "refused_no_route", "refused": true, "route_id": null,
        "serving": null, "skipped": [], "edge": null, "cause": "no_default",
    })
}

fn matcher_prefix(provider: &str) -> Value {
    json!({
        "kind": "prefix",
        "text": format!("{provider}/${{model}}"),
        "value": format!("^{provider}/(?P<model>.+)$"),
        "prefix": format!("{provider}/"),
        "capture": "model",
    })
}

fn matcher_glob(glob: &str) -> Value {
    json!({"kind": "glob", "text": glob, "value": glob})
}

fn scope_json(label: &str) -> Value {
    match label.split_once('/') {
        Some((profile, protocol)) => {
            json!({"kind": "profile", "profile": profile, "protocol": protocol})
        }
        None => json!({"kind": "protocol", "protocol": label}),
    }
}

/// The fields every record carries about the scope it is in.
fn scope_fields(label: &str) -> Value {
    let (profile, protocol) = match label.split_once('/') {
        Some((profile, protocol)) => (Some(profile), protocol),
        None => (None, label),
    };
    json!({
        "scope": scope_json(label),
        "scope_label": label,
        "protocol": protocol,
        "profile": profile,
    })
}

fn merge(mut base: Value, extra: Value) -> Value {
    if let (Some(base), Value::Object(extra)) = (base.as_object_mut(), extra) {
        base.extend(extra);
    }
    base
}

fn hits(count: Option<u64>, state: &str) -> Value {
    json!({"count": count, "state": state})
}

/// A rewrite as the tables read it.
struct Rule {
    scope: &'static str,
    matcher: Value,
    route: &'static str,
    sends: &'static str,
    hits: Value,
}

/// The messages protocol's rules, then chat's, then responses'.
fn rules(world: World) -> Vec<Rule> {
    let prefixed = |scope, provider, route, hits| Rule {
        scope,
        matcher: matcher_prefix(provider),
        route,
        sends: "capture",
        hits,
    };
    let none = || hits(Some(0), "floor");
    let mut rules = vec![
        prefixed("anthropic_messages", "anthropic", "claude-sub", none()),
        prefixed(
            "anthropic_messages",
            "anthropic_sub",
            "claude-sub",
            hits(Some(1), "floor"),
        ),
        prefixed("anthropic_messages", "anthropic_api", "claude-api", none()),
        prefixed(
            "anthropic_messages",
            "openrouter",
            "openrouter-messages",
            hits(Some(41), "exact"),
        ),
        prefixed("anthropic_messages", "codex_sub", "codex", none()),
    ];
    if world.profile_rules {
        rules.insert(
            0,
            Rule {
                scope: "unprefixed/anthropic_messages",
                matcher: matcher_glob("claude-haiku-*"),
                route: "claude-sub",
                sends: "as_asked",
                hits: none(),
            },
        );
    }
    if world.saved {
        rules.push(Rule {
            scope: "anthropic_messages",
            matcher: matcher_glob("claude-*"),
            route: "openrouter-messages",
            sends: "as_asked",
            hits: hits(Some(0), "floor"),
        });
    }
    rules.extend([
        prefixed(
            "openai_chat",
            "openrouter",
            "openrouter-chat",
            hits(Some(9), "exact"),
        ),
        prefixed("openai_chat", "openai_api", "openai-chat", none()),
        prefixed("openai_chat", "codex_sub", "codex", none()),
        prefixed("openai_chat", "anthropic_api", "claude-api", none()),
        prefixed("openai_chat", "anthropic_sub", "claude-sub", none()),
        prefixed(
            "openai_responses",
            "codex_sub",
            "codex",
            hits(Some(2), "floor"),
        ),
        prefixed("openai_responses", "anthropic_api", "claude-api", none()),
        prefixed("openai_responses", "anthropic_sub", "claude-sub", none()),
        prefixed(
            "openai_responses",
            "openrouter",
            "openrouter-responses",
            none(),
        ),
    ]);
    rules
}

/// The id a rule has in the document: its protocol and place.
fn rule_id(scope: &str, position: usize) -> String {
    let short = scope.rsplit('/').next().unwrap_or(scope);
    format!("{}-{position}", short.replace('_', "-"))
}

/// A default of a scope: where it goes and what it caught.
fn default_of(
    scope: &'static str,
    route: &'static str,
    hits: Value,
) -> (&'static str, &'static str, Value) {
    (scope, route, hits)
}

fn defaults(world: World) -> Vec<(&'static str, &'static str, Value)> {
    let mut defaults = vec![
        default_of("anthropic_messages", "claude-sub", hits(Some(3), "floor")),
        default_of("openai_chat", "openrouter-chat", hits(Some(0), "floor")),
    ];
    if world.profile_defaults {
        defaults.insert(
            0,
            default_of(
                "unprefixed/anthropic_messages",
                "claude-sub",
                hits(Some(0), "floor"),
            ),
        );
        defaults.insert(
            3,
            default_of(
                "unprefixed/openai_chat",
                "openrouter-chat",
                hits(Some(0), "floor"),
            ),
        );
    }
    defaults
}

/// The routes: each with its candidates, its reach and where it stands.
struct Route {
    id: &'static str,
    candidates: Vec<Candidate>,
    reach: Value,
    lanes: (u64, u64),
}

fn routes() -> Vec<Route> {
    let sub = || cand("sub", SUB, "anthropic_sub", "subscription");
    let api = || cand("api", API, "anthropic_api", "billed");
    let or_messages = || cand("billed", OR_MESSAGES, "openrouter", "billed");
    let or_chat = || cand("chat", OR_CHAT, "openrouter", "billed");
    let or_responses = || cand("responses", OR_RESPONSES, "openrouter", "billed");
    let openai = || cand("openai", OPENAI, "openai_api", "billed");
    let codex = || cand("codex", CODEX, "codex_sub", "subscription");
    vec![
        Route {
            id: "claude-sub",
            reach: served_later("claude-sub", (&sub(), "no_credential"), &or_messages()),
            candidates: vec![sub(), or_messages()],
            lanes: (2, 5),
        },
        Route {
            id: "claude-api",
            reach: unavailable("claude-api", (&api(), "provider_off")),
            candidates: vec![api()],
            lanes: (0, 0),
        },
        Route {
            id: "openrouter-messages",
            reach: served_first("openrouter-messages", &or_messages()),
            candidates: vec![or_messages()],
            lanes: (0, 0),
        },
        Route {
            id: "openrouter-chat",
            reach: served_first("openrouter-chat", &or_chat()),
            candidates: vec![or_chat()],
            lanes: (0, 3),
        },
        Route {
            id: "openrouter-responses",
            reach: served_first("openrouter-responses", &or_responses()),
            candidates: vec![or_responses()],
            lanes: (0, 0),
        },
        Route {
            id: "openai-chat",
            reach: unavailable("openai-chat", (&openai(), "provider_off")),
            candidates: vec![openai()],
            lanes: (0, 0),
        },
        Route {
            id: "codex",
            reach: served_later("codex", (&codex(), "provider_off"), &or_responses()),
            candidates: vec![codex(), or_responses()],
            lanes: (0, 0),
        },
    ]
}

fn reach_of(route: &str) -> Value {
    routes()
        .into_iter()
        .find(|candidate| candidate.id == route)
        .map_or_else(no_route, |route| route.reach)
}

fn edge_between(from: &str, to: &str) -> Value {
    json!({
        "from": from, "to": to,
        "from_cost": "subscription", "to_cost": "billed",
        "crosses_cost_boundary": true,
        "preflight": "enabled",
        "post_send": ["overload", "server_error"],
        "billed_boundary": "allow",
        "ambiguous_transport": "deny",
        "cache_transition": "preserve_warm",
    })
}

fn candidate_json(candidate: &Candidate) -> Value {
    json!({
        "id": candidate.id,
        "binding_id": candidate.binding,
        "provider": candidate.provider,
        "enabled": true,
        "cost_class": candidate.cost,
        "upgrade_policy_id": null,
        "upgrade": null,
    })
}

/// Everything that sends a request to `route`, in the order the tables are
/// read.
fn referrers(route: &str, world: World) -> Vec<Value> {
    let mut found = Vec::new();
    let mut position: std::collections::BTreeMap<&str, usize> = Default::default();
    for rule in rules(world) {
        let at = position.entry(rule.scope).or_default();
        *at += 1;
        if rule.route == route {
            found.push(merge(
                scope_fields(rule.scope),
                json!({
                    "kind": "rewrite",
                    "id": rule_id(rule.scope, *at),
                    "position": *at,
                    "matcher": rule.matcher,
                    "slugs": [],
                }),
            ));
        }
    }
    for (scope, to, _) in defaults(world) {
        if to == route {
            found.push(merge(
                scope_fields(scope),
                json!({"kind": "default", "id": null, "position": null, "matcher": null, "slugs": []}),
            ));
        }
    }
    found
}

/// A route with its handling blocks.
fn route_json(route: &Route, world: World) -> Value {
    let candidates: Vec<Value> = route.candidates.iter().map(candidate_json).collect();
    let edges: Vec<Value> = route
        .candidates
        .windows(2)
        .map(|pair| edge_between(pair[0].id, pair[1].id))
        .collect();
    let providers: Vec<&str> = {
        let mut providers: Vec<&str> = Vec::new();
        for candidate in &route.candidates {
            if !providers.contains(&candidate.provider) {
                providers.push(candidate.provider);
            }
        }
        providers
    };
    let matched_by = referrers(route.id, world);
    let count = |kind: &str| {
        matched_by
            .iter()
            .filter(|referrer| referrer["kind"] == kind)
            .count()
    };
    let on_claude_sub = route.id == "claude-sub";
    let compacts = matches!(route.id, "claude-sub" | "claude-api");
    let alias = if on_claude_sub {
        json!({"enabled": true, "candidates": [{
            "candidate_id": "billed", "binding_id": OR_MESSAGES, "provider": "openrouter",
            "rules": [{"match": matcher_glob("claude-*"), "exclude": [], "to": "anthropic/claude-*"}],
        }]})
    } else {
        json!({"enabled": false, "candidates": []})
    };
    json!({
        "id": route.id,
        "pinned_lanes": {
            "warm": route.lanes.0, "cold": route.lanes.1, "unknown": 0,
            "total": route.lanes.0 + route.lanes.1,
        },
        "candidates": candidates,
        "edges": edges,
        "providers": providers,
        "reach": route.reach,
        "lane_transition": "move_when_cold",
        "matched_by": matched_by,
        "matched_by_counts": {
            "presentations": count("presentation"),
            "rewrites": count("rewrite"),
            "defaults": count("default"),
        },
        "handled_by": {
            "upgrade": {
                "enabled": on_claude_sub,
                "candidates": if on_claude_sub {
                    json!([{"candidate_id": "sub", "policy_id": "newest", "enabled": true}])
                } else {
                    json!([])
                },
            },
            "compaction": if compacts {
                json!({"enabled": true, "kind": "substitution",
                       "model": {"kind": "family", "value": "sonnet"},
                       "bindings": [SUB, API], "cache_transition": "allow_when_cold"})
            } else {
                json!({"enabled": false})
            },
            "alias": alias,
            "server_safeguards": {"enabled": false, "candidates": []},
            "required_capabilities": {"enabled": false, "candidates": []},
            "health_circuits": {"enabled": false, "candidates": []},
        },
    })
}

fn sends_json(sends: &str) -> Value {
    match sends {
        "as_asked" => json!({"kind": "as_asked", "text": "as asked"}),
        _ => json!({"kind": "capture", "text": "${model}"}),
    }
}

fn rewrite_records(world: World) -> Vec<Value> {
    let mut position: std::collections::BTreeMap<&str, usize> = Default::default();
    rules(world)
        .into_iter()
        .map(|rule| {
            let at = position.entry(rule.scope).or_default();
            *at += 1;
            let (input, output) = match rule.matcher["kind"].as_str() {
                Some("glob") => ("claude-haiku-4-5".to_owned(), "claude-haiku-4-5".to_owned()),
                _ => (
                    format!(
                        "{}claude-sonnet-5",
                        rule.matcher["prefix"].as_str().unwrap_or("")
                    ),
                    "claude-sonnet-5".to_owned(),
                ),
            };
            merge(
                scope_fields(rule.scope),
                json!({
                    "id": rule_id(rule.scope, *at),
                    "priority": *at * 10,
                    "position": *at,
                    "matcher": rule.matcher,
                    "sends": sends_json(rule.sends),
                    "route_id": rule.route,
                    "example": {"in": input, "out": output, "invented": false},
                    "reach": reach_of(rule.route),
                    "hits": rule.hits,
                    "shadowed_by": [],
                }),
            )
        })
        .collect()
}

fn default_records(world: World) -> Vec<Value> {
    let mut records: Vec<Value> = defaults(world)
        .into_iter()
        .map(|(scope, route, hits)| {
            merge(
                scope_fields(scope),
                json!({"exists": true, "route_id": route, "reach": reach_of(route), "hits": hits}),
            )
        })
        .collect();
    // A protocol with no default says that it refuses.
    records.push(merge(
        scope_fields("openai_responses"),
        json!({"exists": false, "route_id": null, "reach": no_route(), "hits": null}),
    ));
    records
}

fn scope_records(world: World) -> Vec<Value> {
    let count = |scope: &str| {
        rules(world)
            .iter()
            .filter(|rule| rule.scope == scope)
            .count()
    };
    let has_default = |scope: &str| defaults(world).iter().any(|default| default.0 == scope);
    let entry = |scope: &str, presentations: usize| {
        let has = has_default(scope);
        merge(
            scope_fields(scope),
            json!({
                "presentations": presentations,
                "rewrites": count(scope),
                "has_default": has,
                "else": match (has, scope.contains('/')) {
                    (true, _) => "default",
                    (false, true) => "protocol",
                    (false, false) => "refused",
                },
            }),
        )
    };
    let mut scopes = vec![entry("claude/anthropic_messages", 15)];
    if world.profile_defaults {
        scopes.push(entry("unprefixed/anthropic_messages", 0));
    }
    scopes.push(entry("anthropic_messages", 0));
    if world.profile_defaults {
        scopes.push(entry("unprefixed/openai_chat", 0));
    }
    scopes.push(entry("openai_chat", 0));
    scopes.push(entry("openai_responses", 0));
    scopes
}

/// The fifteen presentations of Claude Code's picker: what each includes,
/// leaves out, shows as, and the newest model it lists.
const PRESENTATIONS: [(&str, &[&str], &str, u64, &str); 15] = [
    (
        "openai/gpt-*-sol",
        &["*-pro"],
        "opus",
        3,
        "openai/gpt-5.6-sol",
    ),
    (
        "openai/gpt-*-luna",
        &["*-pro"],
        "haiku",
        2,
        "openai/gpt-5.6-luna",
    ),
    (
        "google/gemini-*-flash",
        &[],
        "sonnet",
        3,
        "google/gemini-3.5-flash",
    ),
    (
        "moonshotai/kimi-k*",
        &["*-code", "*-thinking"],
        "sonnet",
        4,
        "moonshotai/kimi-k2.7",
    ),
    (
        "z-ai/glm-*",
        &["*-flash*", "*v*", "*-turbo", "*-prime"],
        "sonnet",
        5,
        "z-ai/glm-5.1",
    ),
    ("z-ai/glm-*-flash", &[], "haiku", 2, "z-ai/glm-5-flash"),
    (
        "deepseek/deepseek-v*-pro*",
        &[],
        "sonnet",
        2,
        "deepseek/deepseek-v4-pro",
    ),
    (
        "deepseek/deepseek-v*-flash*",
        &["*-vision*"],
        "haiku",
        2,
        "deepseek/deepseek-v4-flash",
    ),
    (
        "qwen/qwen*-max*",
        &["*-prime"],
        "sonnet",
        3,
        "qwen/qwen3.5-max",
    ),
    (
        "minimax/minimax-m*",
        &[],
        "sonnet",
        3,
        "minimax/minimax-m2.5",
    ),
    ("openrouter/auto", &[], "sonnet", 1, "openrouter/auto"),
    (
        "anthropic/claude-opus-*",
        &[],
        "opus",
        3,
        "anthropic/claude-opus-5",
    ),
    (
        "anthropic/claude-sonnet-*",
        &[],
        "sonnet",
        4,
        "anthropic/claude-sonnet-5",
    ),
    (
        "anthropic/claude-haiku-*",
        &[],
        "haiku",
        3,
        "anthropic/claude-haiku-4-5",
    ),
    (
        "anthropic/claude-fable-*",
        &[],
        "fable",
        1,
        "anthropic/claude-fable-5",
    ),
];

fn presentation_records() -> Vec<Value> {
    PRESENTATIONS
        .iter()
        .enumerate()
        .map(|(index, (include, exclude, tier, lists, newest))| {
            let family = newest.rsplit_once('-').map_or(*newest, |(head, _)| head);
            let models: Vec<String> = if *include == "z-ai/glm-*" {
                ["glm-5.1", "glm-5", "glm-4.7", "glm-4.6", "glm-4.5"]
                    .iter()
                    .map(|model| format!("z-ai/{model}"))
                    .collect()
            } else {
                (0..*lists.min(&5))
                    .map(|older| {
                        if older == 0 {
                            (*newest).to_owned()
                        } else {
                            format!("{family}-{}", 5 - older)
                        }
                    })
                    .collect()
            };
            merge(
                scope_fields("claude/anthropic_messages"),
                json!({
                    "id": format!("pick-{}", index + 1),
                    "position": index + 1,
                    "enabled": true,
                    "slug": "openrouter/${model_id}",
                    "label": null,
                    "shadow_native": false,
                    "source": {
                        "kind": "offer_selector",
                        "binding_id": OR_MESSAGES,
                        "include": [matcher_glob(include)],
                        "exclude": exclude.iter().map(|glob| matcher_glob(glob)).collect::<Vec<_>>(),
                        "keep": 5,
                        "ordering": {"kind": "chronology"},
                    },
                    "slugs": [newest.rsplit('/').next().unwrap_or(newest)],
                    "behaves_as": tier,
                    "lists": {"count": lists, "newest": newest, "models": models},
                    "listed": 1,
                    "published": "yes",
                    "route_ids": [],
                    "bindings": [OR_MESSAGES],
                    "reach": served_first("openrouter-messages",
                        &cand("billed", OR_MESSAGES, "openrouter", "billed")),
                    "hits": hits(None, "not_recorded"),
                    "shadowed_by": [],
                }),
            )
        })
        .collect()
}

/// The five families the classifiers read: a pattern, an example it parses
/// and what it reads it as.
const FAMILIES: [(&str, &str, &str, &str, &str); 5] = [
    (
        "claude-opus",
        "claude-opus-*",
        "claude-opus-4-7",
        "opus",
        "4.7",
    ),
    (
        "claude-sonnet",
        "claude-sonnet-*",
        "claude-sonnet-4-5",
        "sonnet",
        "4.5",
    ),
    (
        "claude-haiku",
        "claude-haiku-*",
        "claude-haiku-4-5",
        "haiku",
        "4.5",
    ),
    ("gpt", "gpt-*", "openai/gpt-5.6-sol", "sol", "5.6"),
    ("qwen", "qwen/qwen*", "qwen/qwen3.5-max", "max", "3.5"),
];

fn classifiers() -> Vec<Value> {
    FAMILIES
        .iter()
        .map(|(id, pattern, example, family, version)| {
            json!({
                "id": id,
                "binding_id": SUB,
                "matcher": matcher_glob(pattern),
                "parsed": [{"model": example, "family": family, "version": version}],
                "unparsed": [],
            })
        })
        .collect()
}

/// The upgrades in use: route, candidate, binding, from and to.
const MOVES: [(&str, &str, &str, &str, &str, u64, u64); 5] = [
    (
        "claude-sub",
        "sub",
        SUB,
        "claude-opus-4-7",
        "claude-opus-5",
        3,
        4,
    ),
    (
        "claude-api",
        "api",
        API,
        "claude-sonnet-4-5",
        "claude-sonnet-5",
        3,
        4,
    ),
    (
        "openrouter-messages",
        "billed",
        OR_MESSAGES,
        "anthropic/claude-sonnet-4-5",
        "anthropic/claude-sonnet-5",
        3,
        4,
    ),
    (
        "openrouter-responses",
        "responses",
        OR_RESPONSES,
        "openai/gpt-5.5-sol",
        "openai/gpt-5.6-sol",
        2,
        3,
    ),
    (
        "codex",
        "codex",
        CODEX,
        "gpt-5.5-terra",
        "gpt-5.6-terra",
        2,
        3,
    ),
];

fn upgrades() -> Vec<Value> {
    let used_by: Vec<Value> = MOVES
        .iter()
        .map(|(route, candidate, binding, from, to, needs, active)| {
            json!({
                "route_id": route,
                "candidate_id": candidate,
                "binding_id": binding,
                "provider": binding.split('.').next().unwrap_or(""),
                "candidate_enabled": true,
                "bar": {"kind": "scaled", "active_days": active, "needs_days": needs},
                "moves": [{"family": "any", "from": from, "to": to}],
                "held_back": [],
            })
        })
        .collect();
    vec![json!({
        "id": "newest",
        "enabled": true,
        "classifier_id": "claude-opus",
        "evidence_bar": {"kind": "scaled"},
        "allow_manual_promotion": true,
        "inventory_sources": ["catalogue", "served"],
        "lane_transition": "move_when_cold",
        "used_by": used_by,
    })]
}

/// A catalogue of 440 invented offers on one binding, then a few on the
/// rest: the first 180 sort ahead of the OpenAI models, the rest after.
fn offers() -> Vec<Value> {
    let named: [(&str, u64, &str); 14] = [
        ("openai/gpt-4-turbo-preview", 128_000, "2024-01"),
        ("openai/gpt-4.1", 1_048_576, "2025-04"),
        ("openai/gpt-4.1-mini", 1_048_576, "2025-04"),
        ("openai/gpt-4.1-nano", 1_048_576, "2025-04"),
        ("openai/gpt-4o", 128_000, "2024-05"),
        ("openai/gpt-4o-2024-05-13", 128_000, "2024-05"),
        ("openai/gpt-4o-2024-08-06", 128_000, "2024-08"),
        ("openai/gpt-4o-2024-11-20", 128_000, "2024-11"),
        ("openai/gpt-4o-audio-preview", 128_000, "2024-10"),
        ("openai/gpt-4o-mini", 128_000, "2024-07"),
        ("openai/gpt-4o-mini-2024-07-18", 128_000, "2024-07"),
        ("openai/gpt-4o-mini-search-preview", 128_000, "2025-03"),
        ("openai/gpt-4o-search-preview", 128_000, "2025-03"),
        ("openai/gpt-4o:extended", 128_000, "2024-05"),
    ];
    let at = |month: &str| {
        let (year, month) = month.split_once('-').expect("a month");
        // Mid-month, so the date a page reads is the month it was named for.
        let days = days_from_civil(
            year.parse().expect("year"),
            month.parse().expect("month"),
            15,
        );
        days * 86_400_000
    };
    let offer = |model: String, window: u64, month: &str| {
        json!({
            "binding_id": OR_CHAT, "provider": "openrouter", "model_id": model,
            "context_window": window, "fetched_at_ms": 1_760_000_000_000_i64,
            "chronology_ms": at(month),
        })
    };
    let mut offers = Vec::new();
    for index in 0..180 {
        offers.push(offer(
            format!("anthropic/model-{index:03}"),
            200_000,
            "2024-06",
        ));
    }
    for (model, window, month) in named {
        offers.push(offer(model.to_owned(), window, month));
    }
    for index in 0..246 {
        offers.push(offer(format!("qwen/model-{index:03}"), 262_144, "2025-02"));
    }
    for (binding, provider, model) in [
        (SUB, "anthropic_sub", "claude-opus-4-7"),
        (SUB, "anthropic_sub", "claude-sonnet-4-5"),
        (API, "anthropic_api", "claude-sonnet-5"),
    ] {
        offers.push(json!({
            "binding_id": binding, "provider": provider, "model_id": model,
            "context_window": 200_000, "fetched_at_ms": 1_760_000_000_000_i64,
            "chronology_ms": at("2025-08"),
        }));
    }
    offers
}

/// Days from 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The policy document the derived facts were made from: what an edit
/// replaces, adds to or removes.
fn document(world: World) -> Value {
    let mut position: std::collections::BTreeMap<&str, usize> = Default::default();
    let rewrites: Vec<Value> = rules(world)
        .into_iter()
        .map(|rule| {
            let at = position.entry(rule.scope).or_default();
            *at += 1;
            let glob = rule.matcher["kind"] == "glob";
            json!({
                "id": rule_id(rule.scope, *at),
                "scope": scope_json(rule.scope),
                "priority": *at * 10,
                "model_matcher": if glob {
                    json!({"kind": "glob", "value": rule.matcher["value"]})
                } else {
                    json!({"kind": "regex", "value": rule.matcher["value"]})
                },
                "target": {
                    "route_id": rule.route,
                    "parameters": if glob {
                        json!({"requested_model": {"kind": "requested_model"}})
                    } else {
                        json!({"requested_model": {"kind": "regex_capture_name", "name": "model"}})
                    },
                },
            })
        })
        .collect();
    let routes: Vec<Value> = routes()
        .iter()
        .map(|route| {
            let candidate = |candidate: &Candidate| {
                json!({
                    "id": candidate.id, "binding_id": candidate.binding,
                    "model": {"kind": "requested_model"}, "enabled": true,
                })
            };
            json!({
                "id": route.id,
                "display_name": route.id,
                "parameters": ["requested_model"],
                "chain": {
                    "first": candidate(&route.candidates[0]),
                    "steps": route.candidates[1..].iter().map(|next| json!({
                        "edge": {
                            "preflight": "enabled", "post_send": ["overload", "server_error"],
                            "billed_boundary": "allow", "ambiguous_transport": "deny",
                            "cache_transition": "preserve_warm",
                        },
                        "candidate": candidate(next),
                    })).collect::<Vec<_>>(),
                },
                "lane_transition": "move_when_cold",
            })
        })
        .collect();
    let defaults: Vec<Value> = defaults(world)
        .into_iter()
        .map(|(scope, route, _)| {
            json!({
                "scope": scope_json(scope),
                "target": {
                    "route_id": route,
                    "parameters": {"requested_model": {"kind": "requested_model"}},
                },
            })
        })
        .collect();
    let presentations: Vec<Value> = PRESENTATIONS
        .iter()
        .enumerate()
        .map(|(index, (include, exclude, tier, _, _))| {
            json!({
                "id": format!("pick-{}", index + 1),
                "scope": scope_json("claude/anthropic_messages"),
                "slug": "openrouter/${model_id}",
                "metadata": {"adapter": "claude", "behaves_as": tier},
                "source": {
                    "kind": "offer_selector",
                    "selector": {
                        "binding_id": OR_MESSAGES,
                        "include": [{"kind": "glob", "value": include}],
                        "exclude": exclude.iter()
                            .map(|glob| json!({"kind": "glob", "value": glob}))
                            .collect::<Vec<_>>(),
                        "ordering": {"kind": "chronology"},
                        "keep": 5,
                    },
                },
            })
        })
        .collect();
    json!({
        "format_version": 5,
        "description": "the routing in use",
        "presentations": presentations,
        "routes": routes,
        "defaults": defaults,
        "rewrites": rewrites,
        "classifiers": FAMILIES.iter().map(|(id, pattern, ..)| json!({
            "id": id, "binding_id": SUB,
            "model_matcher": {"kind": "glob", "value": pattern},
            "family_template": "${family}", "version_template": "${version}",
            "version_parser": {"kind": "numeric_tuple", "separator": "-"},
        })).collect::<Vec<_>>(),
        "upgrades": [{
            "id": "newest", "classifier_id": "claude-opus",
            "minimum_learning": {"kind": "scaled"},
            "inventory_sources": ["catalogue", "served"],
            "lane_transition": "move_when_cold",
        }],
        "compaction": {
            "target": {"kind": "substitution", "model": {"family": "sonnet"}, "bindings": [SUB, API]},
            "lane_transition": "move_when_cold",
            "cache_transition": "allow_when_cold",
        },
    })
}

/// The snapshot `routing-snapshot` answers with, for `world`.
pub(super) fn snapshot(world: World) -> Value {
    let snapshot = built(world);
    if world.empty {
        emptied(snapshot)
    } else {
        snapshot
    }
}

/// `snapshot` as a fresh install has it: the live revision holds no record,
/// and every protocol still has its scope and the default row that says it
/// refuses.
fn emptied(mut snapshot: Value) -> Value {
    let policy = &mut snapshot["active_revision"]["policy"];
    for table in [
        "presentations",
        "routes",
        "defaults",
        "rewrites",
        "classifiers",
        "upgrades",
    ] {
        policy[table] = json!([]);
    }
    policy["compaction"] = Value::Null;
    let active = &mut snapshot["derived"]["active"];
    for table in [
        "routes",
        "classifiers",
        "presentation_records",
        "rewrites",
        "upgrades",
    ] {
        active[table] = json!([]);
    }
    active["compaction"] = Value::Null;
    let protocols = ["anthropic_messages", "openai_chat", "openai_responses"];
    active["scopes"] = protocols
        .iter()
        .map(|protocol| {
            merge(
                scope_fields(protocol),
                json!({"presentations": 0, "rewrites": 0, "has_default": false, "else": "refused"}),
            )
        })
        .collect();
    active["defaults"] = protocols
        .iter()
        .map(|protocol| {
            merge(
                scope_fields(protocol),
                json!({"exists": false, "route_id": null, "reach": no_route(), "hits": null}),
            )
        })
        .collect();
    snapshot
}

fn built(world: World) -> Value {
    let revision = if world.saved { 10 } else { 9 };
    let routes: Vec<Value> = routes()
        .iter()
        .map(|route| route_json(route, world))
        .collect();
    let compaction = json!({
        "lane_transition": "move_when_cold",
        "cache_transition": "allow_when_cold",
        "kind": "substitution",
        "model": {"kind": "family", "value": "sonnet"},
        "bindings": [
            {"binding_id": SUB, "provider": "anthropic_sub"},
            {"binding_id": API, "provider": "anthropic_api"},
        ],
        "routes": ["claude-sub", "claude-api"],
    });
    let publication = |adapter: &str, protocol: &str, state: &str| {
        json!({
            "scope": {"kind": "protocol", "protocol": protocol},
            "adapter": adapter,
            "desired_revision_id": revision,
            "applied_revision_id": if state == "applied" { json!(revision) } else { json!(revision - 1) },
            "desired_inventory_generation": 7,
            "applied_inventory_generation": 7,
            "state": state,
            "detail_code": null,
        })
    };
    json!({
        "toker": "routing-snapshot",
        "ok": true,
        "active_revision": {
            "revision_id": revision,
            "description": "the routing in use",
            "source": "edit",
            "parent_revision_id": revision - 1,
            "policy": document(world),
        },
        "activation": {"scope": "new_and_cold_lanes", "forced_cache_transition": false},
        "offers": offers(),
        "lane_impacts": {"total": 10, "unpinned": 0, "warm": 2, "cold": 8, "unknown": 0, "at_risk": 2},
        "publications": [
            publication(
                "claude_settings",
                "anthropic_messages",
                if world.saved { "pending" } else { "applied" },
            ),
            publication("dynamic_chat_models", "openai_chat", "applied"),
            publication("dynamic_responses_models", "openai_responses", "applied"),
        ],
        "derived": {
            "availability": availability(),
            "circuits": [],
            "revisions": if world.saved {
                json!([
                    {"revision_id": 9, "warm": 2, "cold": 0, "unknown": 0, "total": 2,
                     "at_risk": ["ses-hot", "ses-crow"]},
                    {"revision_id": 10, "warm": 0, "cold": 8, "unknown": 0, "total": 8},
                ])
            } else {
                json!([{"revision_id": 9, "warm": 2, "cold": 8, "unknown": 0, "total": 10,
                    "at_risk": ["ses-hot", "ses-crow"]}])
            },
            "holds_nothing": world.empty,
            // Worked out against the live revision: with rev 10 live, rev 9's
            // two warm lanes are the ones moving would cost a cache.
            "earlier_lanes": if world.saved {
                json!({
                    "total": 2, "at_risk": 2, "lanes": ["ses-hot", "ses-crow"],
                    "revisions": [{"revision_id": 9, "total": 2, "at_risk": 2}],
                })
            } else {
                json!({"total": 0, "at_risk": 0, "lanes": [], "revisions": []})
            },
            "active": {
                "routes": routes,
                "classifiers": classifiers(),
                "presentations": [],
                "scopes": scope_records(world),
                "presentation_records": presentation_records(),
                "rewrites": rewrite_records(world),
                "defaults": default_records(world),
                "upgrades": upgrades(),
                "compaction": compaction,
                "providers": [],
                "first_refused_tab": null,
            },
        },
    })
}

/// The `earlier_lanes` the daemon derives from the per-revision rows with
/// `live` the live revision, for tests that set those rows by hand. It
/// mirrors `server::control::derived::earlier_lanes`.
pub(super) fn earlier_lanes(revisions: &Value, live: i64) -> Value {
    let mut total = 0;
    let mut at_risk = 0;
    let mut lanes: Vec<Value> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    for revision in revisions.as_array().into_iter().flatten() {
        let number = |key: &str| revision.get(key).and_then(Value::as_u64).unwrap_or(0);
        if revision.get("revision_id").and_then(Value::as_i64) == Some(live) {
            continue;
        }
        let risk = number("warm") + number("unknown");
        total += number("total");
        at_risk += risk;
        lanes.extend(
            revision
                .get("at_risk")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
        rows.push(json!({
            "revision_id": revision["revision_id"],
            "total": number("total"),
            "at_risk": risk,
        }));
    }
    json!({"total": total, "at_risk": at_risk, "lanes": lanes, "revisions": rows})
}

/// The revision history the page reads beside the snapshot, newest first.
pub(super) fn revisions(world: World) -> Value {
    let newest = if world.saved { 10 } else { 9 };
    // The design's history: when each was made, what it changed and the
    // record it touched, as the daemon words them.
    let moment = |day: i8, hour: i8, minute: i8| {
        jiff::civil::date(2026, 11, day)
            .at(hour, minute, 0, 0)
            .in_tz("UTC")
            .expect("a moment")
            .timestamp()
            .as_millisecond()
    };
    let history = |id: i64| -> (i64, &str, &str, &str, &str) {
        match id {
            10 => (
                moment(10, 14, 41),
                "add rewrite claude-* → openrouter-messages",
                "added",
                "rewrite",
                "rewrite anthropic_messages 6  claude-* → openrouter-messages  as asked",
            ),
            9 => (
                moment(10, 14, 18),
                "add a fallback to claude-sub",
                "added",
                "candidate",
                "candidate openrouter on route claude-sub  openrouter.openrouter_messages",
            ),
            8 => (
                moment(10, 13, 7),
                "move anthropic_sub/${model} above anthropic_api/${model} in anthropic_messages",
                "moved",
                "rewrite",
                "rewrite anthropic_messages 2  anthropic_sub/${model} → claude-sub above anthropic_api/${model}",
            ),
            7 => (
                moment(10, 11, 21),
                "turn on compaction",
                "added",
                "compaction",
                "compaction",
            ),
            6 => (
                moment(10, 10, 48),
                "add upgrade newest on claude-sub",
                "added",
                "upgrade",
                "upgrade newest  on family claude-opus",
            ),
            5 => (
                moment(10, 9, 3),
                "add family qwen",
                "added",
                "family",
                "family qwen  on openrouter.openrouter_messages",
            ),
            4 => (
                moment(9, 15, 20),
                "retry openrouter-chat on 5xx",
                "changed",
                "route",
                "route openrouter-chat, the edge into fallback: post_send",
            ),
            3 => (
                moment(9, 10, 41),
                "add presentation anthropic/claude-sonnet-*",
                "added",
                "presentation",
                "presentation claude/anthropic_messages  anthropic/claude-sonnet-*",
            ),
            2 => (
                moment(9, 10, 29),
                "add rewrite anthropic_api/${model} to anthropic_messages",
                "added",
                "rewrite",
                "rewrite anthropic_messages 3  anthropic_api/${model} → claude-api  ${model}",
            ),
            _ => (moment(7, 16, 5), "", "", "", ""),
        }
    };
    json!({
        "toker": "routing-revisions",
        "ok": true,
        "revisions": (1..=newest).rev().map(|id| {
            let (at, summary, change, kind, said) = history(id);
            let first = id == 1;
            json!({
                "revision_id": id,
                "created_ms": at,
                "activated_ms": at,
                "description": null,
                "source": if first { "legacy" } else { "save" },
                "parent_revision_id": if first { Value::Null } else { json!(id - 1) },
                "summary": if first {
                    "setup: 7 routes, 4 defaults, 13 rewrites, 14 presentations, 4 families, 4 upgrades"
                } else {
                    summary
                },
                "changes": if first {
                    json!([])
                } else {
                    json!([{"change": change, "kind": kind, "id": null, "summary": summary, "text": said}])
                },
            })
        }).collect::<Vec<_>>(),
    })
}
