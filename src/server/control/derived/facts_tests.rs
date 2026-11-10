//! The facts the routing page reads, over one small policy: reach, hits,
//! shadowing, what refers to a route, what depends on a provider, and what an
//! upgrade or a compaction would do now.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::{ActivePolicy, Inputs, build};
use crate::catalog::offers::{BackendOffer, OfferInventory};
use crate::policy::{
    AvailabilitySnapshot, BindingAvailability, CatalogueAvailability, CompiledPolicy, CostClass,
    Evidence, NoLearning, PolicyDocument, compile,
};
use crate::routing::{ANTHROPIC_API_MESSAGES, ANTHROPIC_SUB_MESSAGES, Capabilities, RouteRegistry};
use crate::server::RoutingLaneImpacts;
use crate::store::{DefaultHits, HitCounts, HitCoverage};

const SUB: &str = "anthropic_sub.anthropic_messages";
const API: &str = "anthropic_api.anthropic_messages";

fn param() -> Value {
    json!({"requested_model": {"kind": "requested_model"}})
}

fn captured() -> Value {
    json!({"requested_model": {"kind": "regex_capture_name", "name": "model"}})
}

fn candidate(id: &str, binding: &str) -> Value {
    json!({
        "id": id,
        "binding_id": binding,
        "model": {"kind": "requested_model"},
    })
}

fn edge(preflight: &str) -> Value {
    json!({
        "preflight": preflight,
        "post_send": [],
        "billed_boundary": "allow",
        "ambiguous_transport": "deny",
        "cache_transition": "allow_cache_loss",
    })
}

fn route(id: &str, first: &str, then: Option<(&str, &str)>) -> Value {
    let steps: Vec<Value> = then
        .into_iter()
        .map(|(binding, preflight)| {
            json!({"edge": edge(preflight), "candidate": candidate("then", binding)})
        })
        .collect();
    json!({
        "id": id,
        "display_name": id,
        "parameters": ["requested_model"],
        "chain": {"first": candidate("first", first), "steps": steps},
        "lane_transition": "move_when_cold",
    })
}

fn protocol(name: &str) -> Value {
    json!({"kind": "protocol", "protocol": name})
}

fn profile(name: &str) -> Value {
    json!({"kind": "profile", "profile": name, "protocol": "anthropic_messages"})
}

fn target(route: &str, parameters: Value) -> Value {
    json!({"route_id": route, "parameters": parameters})
}

fn rewrite(id: &str, scope: Value, priority: i32, matcher: Value, to: &str) -> Value {
    json!({
        "id": id,
        "scope": scope,
        "priority": priority,
        "model_matcher": matcher,
        // Only a regular expression has a capture to send on.
        "target": target(to, if matcher["kind"] == "regex" { captured() } else { param() }),
    })
}

fn presentation(id: &str, scope: Value, slug: &str, to: &str) -> Value {
    json!({
        "id": id,
        "scope": scope,
        "slug": slug,
        "source": {"kind": "virtual", "target": target(to, param())},
    })
}

fn prefix(provider: &str) -> Value {
    json!({"kind": "regex", "value": format!("^{provider}/(?P<model>.+)$")})
}

fn offer(binding: &str, provider: &str, model: &str) -> BackendOffer {
    let binding_id = binding.parse().expect("binding id");
    BackendOffer {
        binding: [
            ANTHROPIC_SUB_MESSAGES.clone(),
            ANTHROPIC_API_MESSAGES.clone(),
        ]
        .into_iter()
        .find(|candidate| *candidate.id() == binding_id)
        .expect("known binding"),
        binding_id,
        provider: provider.to_owned(),
        backend_model_id: model.to_owned(),
        // A model of unknown size does not fit any prompt, so an upgrade to it
        // is refused for that.
        context_window: Some(200_000),
        chronology: None,
        fetched_at_ms: 1,
        raw: Value::Null,
    }
}

fn inventory() -> OfferInventory {
    OfferInventory::from_offers([
        offer(SUB, "anthropic_sub", "claude-sonnet-4-5"),
        offer(SUB, "anthropic_sub", "claude-sonnet-4-6"),
        offer(API, "anthropic_api", "claude-haiku-4-5"),
    ])
}

fn offer_presentation() -> Value {
    let mut offer = presentation(
        "pr-offer",
        protocol("anthropic_messages"),
        "sub-sonnet",
        "claude-sub",
    );
    offer["source"] = json!({
        "kind": "exact_offer",
        "binding_id": SUB,
        "model_id": "claude-sonnet-4-5",
    });
    offer
}

/// Every route the policy needs, a default for each of the scopes that has
/// one, and a pair of rules and presentations that overlap.
fn document() -> Value {
    json!({
        "format_version": 5,
        "routes": [
            route("claude-sub", SUB, Some((API, "enabled"))),
            route("api-only", API, None),
            route("sub-only", SUB, None),
            route("guarded", SUB, Some((API, "disabled"))),
        ],
        "defaults": [
            {"scope": protocol("anthropic_messages"), "target": target("claude-sub", param())},
            {"scope": profile("claude"), "target": target("guarded", param())},
        ],
        "rewrites": [
            // The protocol's own rules, in an order their priorities decide.
            rewrite("p-glob", protocol("anthropic_messages"), 20,
                json!({"kind": "glob", "value": "claude-*"}), "api-only"),
            rewrite("p-prefix", protocol("anthropic_messages"), 10,
                prefix("anthropic_sub"), "sub-only"),
            rewrite("p-exact", protocol("anthropic_messages"), 30,
                json!({"kind": "exact", "value": "claude-sonnet"}), "claude-sub"),
            rewrite("p-wide", protocol("anthropic_messages"), 40,
                prefix("anthropic_sub"), "claude-sub"),
            // A profile's, which is looked up before every one of those.
            rewrite("c-prefix", profile("claude"), 10,
                prefix("anthropic_sub"), "sub-only"),
        ],
        "presentations": [
            presentation("pr-protocol", protocol("anthropic_messages"),
                "claude-sonnet", "claude-sub"),
            presentation("pr-profile", profile("claude"), "claude-sonnet", "api-only"),
            offer_presentation(),
        ],
    })
}

fn compiled(document: Value) -> (CompiledPolicy, PolicyDocument) {
    let document: PolicyDocument = serde_json::from_value(document).expect("document");
    let topology = RouteRegistry::new(std::iter::empty::<Arc<dyn crate::providers::Provider>>());
    let policy = compile(
        document.clone().validate().expect("valid"),
        &topology,
        &inventory(),
    )
    .into_policy()
    .unwrap_or_else(|diagnostics| {
        panic!(
            "the test policy compiles: {:?}",
            diagnostics
                .iter()
                .map(|diagnostic| (diagnostic.code.0.as_str(), diagnostic.path.as_str()))
                .collect::<Vec<_>>()
        )
    });
    (policy, document)
}

fn live(provider: &str, enabled: bool, credential: bool, cost: CostClass) -> BindingAvailability {
    BindingAvailability {
        provider: provider.to_owned(),
        configured: Evidence::Positive("configured"),
        enabled: if enabled {
            Evidence::Positive("enabled")
        } else {
            Evidence::Negative("disabled")
        },
        credential: if credential {
            Evidence::Positive("present")
        } else {
            Evidence::Negative("absent")
        },
        catalogue: CatalogueAvailability::Absent,
        capabilities: Capabilities::ANTHROPIC_MESSAGES,
        safeguards_marked: false,
        cost_class: cost,
    }
}

fn healthy() -> AvailabilitySnapshot {
    availability(
        live("anthropic_sub", true, true, CostClass::Subscription),
        live("anthropic_api", true, true, CostClass::Billed),
    )
}

fn availability(sub: BindingAvailability, api: BindingAvailability) -> AvailabilitySnapshot {
    AvailabilitySnapshot::new(BTreeMap::from([
        (ANTHROPIC_SUB_MESSAGES.id().clone(), sub),
        (ANTHROPIC_API_MESSAGES.id().clone(), api),
    ]))
}

fn hits(coverage: HitCoverage) -> HitCounts {
    HitCounts {
        since_ms: 0,
        until_ms: 1,
        coverage,
        recorded_requests: 0,
        unrecorded_requests: 0,
        presentations: BTreeMap::new(),
        rewrites: BTreeMap::new(),
        defaults: Vec::new(),
        compactions: 0,
        unrouted: 0,
    }
}

fn derive(document: Value, availability: &AvailabilitySnapshot, hits: &HitCounts) -> Value {
    let (policy, document) = compiled(document);
    let pinned = BTreeMap::new();
    let revision_lanes = BTreeMap::new();
    build(&Inputs {
        availability,
        inventory: &inventory(),
        learning: &NoLearning,
        circuits: &Default::default(),
        pinned: &pinned,
        publications: &[],
        active: Some(ActivePolicy {
            policy: &policy,
            document: &document,
        }),
        hits,
        revision_lanes: &revision_lanes,
        revision_at_risk: &Default::default(),
    })["active"]
        .clone()
}

fn find<'a>(records: &'a Value, id: &str) -> &'a Value {
    records
        .as_array()
        .expect("an array")
        .iter()
        .find(|record| record["id"] == id)
        .unwrap_or_else(|| panic!("no record {id} in {records}"))
}

fn ids(records: &Value) -> Vec<&str> {
    records
        .as_array()
        .expect("an array")
        .iter()
        .map(|record| record["id"].as_str().expect("an id"))
        .collect()
}

#[test]
fn rewrites_come_in_page_order_profile_scope_first_then_ascending_priority() {
    let active = derive(document(), &healthy(), &hits(HitCoverage::Complete));

    assert_eq!(
        ids(&active["rewrites"]),
        ["c-prefix", "p-prefix", "p-glob", "p-exact", "p-wide"]
    );
    let first = &active["rewrites"][0];
    assert_eq!(first["scope_label"], "claude/anthropic_messages");
    assert_eq!(first["profile"], "claude");
    assert_eq!(first["position"], 1);
    assert_eq!(active["rewrites"][2]["position"], 2);
    assert_eq!(active["rewrites"][2]["priority"], 20);

    // Every protocol is a group, so one with no policy can say it refuses.
    let scopes: Vec<_> = active["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|scope| {
            (
                scope["scope_label"].as_str().unwrap(),
                scope["else"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        scopes,
        [
            ("claude/anthropic_messages", "default"),
            ("anthropic_messages", "default"),
            ("openai_chat", "refused"),
            ("openai_responses", "refused"),
        ]
    );
}

#[test]
fn matchers_are_given_in_their_own_notation() {
    let active = derive(document(), &healthy(), &hits(HitCoverage::Complete));

    let prefix = &find(&active["rewrites"], "p-prefix")["matcher"];
    assert_eq!(prefix["kind"], "prefix");
    assert_eq!(prefix["text"], "anthropic_sub/${model}");
    assert_eq!(prefix["prefix"], "anthropic_sub/");
    assert_eq!(prefix["capture"], "model");
    assert_eq!(
        find(&active["rewrites"], "p-glob")["matcher"]["kind"],
        "glob"
    );
    assert_eq!(
        find(&active["rewrites"], "p-exact")["matcher"]["kind"],
        "exact"
    );
    assert_eq!(
        find(&active["rewrites"], "p-prefix")["sends"]["text"],
        "${model}"
    );
}

#[test]
fn a_healthy_route_is_served_by_its_first_candidate() {
    let active = derive(document(), &healthy(), &hits(HitCoverage::Complete));

    let reach = &find(&active["rewrites"], "p-prefix")["reach"];
    assert_eq!(reach["outcome"], "served_first");
    assert_eq!(reach["refused"], false);
    assert_eq!(reach["serving"]["provider"], "anthropic_sub");
    assert_eq!(reach["serving"]["billed"], false);
    assert_eq!(active["first_refused_tab"], Value::Null);
}

#[test]
fn a_missing_credential_falls_to_a_later_candidate_and_says_it_is_billed() {
    let availability = availability(
        live("anthropic_sub", true, false, CostClass::Subscription),
        live("anthropic_api", true, true, CostClass::Billed),
    );
    let active = derive(document(), &availability, &hits(HitCoverage::Complete));

    let reach = &find(&active["rewrites"], "p-wide")["reach"];
    assert_eq!(reach["outcome"], "served_later");
    assert_eq!(reach["serving"]["provider"], "anthropic_api");
    assert_eq!(reach["serving"]["billed"], true);
    assert_eq!(reach["skipped"][0]["provider"], "anthropic_sub");
    assert_eq!(reach["skipped"][0]["reason"], "no_credential");
    // The protocol's default goes to that route too.
    let default = active["defaults"]
        .as_array()
        .unwrap()
        .iter()
        .find(|default| default["scope_label"] == "anthropic_messages")
        .unwrap();
    assert_eq!(default["reach"]["outcome"], "served_later");
}

#[test]
fn a_walk_stopped_by_an_edge_names_the_setting_that_stopped_it() {
    let availability = availability(
        live("anthropic_sub", true, false, CostClass::Subscription),
        live("anthropic_api", true, true, CostClass::Billed),
    );
    let active = derive(document(), &availability, &hits(HitCoverage::Complete));

    // The profile's default is the route whose edge forbids falling back
    // before anything is sent.
    let default = active["defaults"]
        .as_array()
        .unwrap()
        .iter()
        .find(|default| default["scope_label"] == "claude/anthropic_messages")
        .unwrap();
    assert_eq!(default["reach"]["outcome"], "refused_edge");
    assert_eq!(default["reach"]["refused"], true);
    assert_eq!(default["reach"]["edge"]["setting"], "preflight");
    assert_eq!(default["reach"]["edge"]["from"], "first");
    assert_eq!(default["reach"]["edge"]["to"], "then");
    assert_eq!(default["reach"]["skipped"][0]["reason"], "no_credential");
}

#[test]
fn every_candidate_unavailable_is_refused_with_each_reason() {
    let availability = availability(
        live("anthropic_sub", false, true, CostClass::Subscription),
        live("anthropic_api", true, true, CostClass::Billed),
    );
    let mut document = document();
    document["presentations"] = json!([]);
    let active = derive(document, &availability, &hits(HitCoverage::Complete));

    let reach = &find(&active["rewrites"], "p-prefix")["reach"];
    assert_eq!(reach["outcome"], "refused_unavailable");
    assert_eq!(reach["refused"], true);
    assert_eq!(reach["skipped"][0]["reason"], "provider_off");
    // The page opens on the first tab holding a refusal, in lookup order:
    // with no presentation to refuse, the rewrites.
    assert_eq!(active["first_refused_tab"], "rewrites");
}

#[test]
fn a_protocol_with_no_default_is_refused_for_want_of_a_route() {
    let active = derive(document(), &healthy(), &hits(HitCoverage::Complete));

    let defaults = active["defaults"].as_array().unwrap();
    let chat = defaults
        .iter()
        .find(|default| default["scope_label"] == "openai_chat")
        .expect("a protocol is always drawn");
    assert_eq!(chat["exists"], false);
    assert_eq!(chat["reach"]["outcome"], "refused_no_route");
    assert_eq!(chat["reach"]["cause"], "no_default");
    assert_eq!(chat["hits"], Value::Null);
    // A scope with no default is not a record, so it does not draw the page
    // to its tab.
    assert_eq!(active["first_refused_tab"], Value::Null);
}

#[test]
fn a_presentation_reads_the_worst_of_what_it_publishes() {
    let availability = availability(
        live("anthropic_sub", false, true, CostClass::Subscription),
        live("anthropic_api", true, true, CostClass::Billed),
    );
    let active = derive(document(), &availability, &hits(HitCoverage::Complete));

    // To a route whose fallback serves it, billed.
    let virtual_one = find(&active["presentation_records"], "pr-protocol");
    assert_eq!(virtual_one["reach"]["outcome"], "served_later");
    // To one offer, which is off.
    let offer = find(&active["presentation_records"], "pr-offer");
    assert_eq!(offer["reach"]["outcome"], "refused_unavailable");
    assert_eq!(offer["reach"]["skipped"][0]["reason"], "provider_off");
    assert_eq!(offer["bindings"], json!([SUB]));
    assert_eq!(active["first_refused_tab"], "presentations");
}

#[test]
fn hits_carry_their_state_and_a_count_only_where_there_is_one() {
    let mut counts = hits(HitCoverage::Floor);
    counts.rewrites.insert("p-prefix".parse().unwrap(), 3);
    counts
        .presentations
        .insert("pr-protocol".parse().unwrap(), 2);
    counts.defaults.push(DefaultHits {
        scope: serde_json::from_value(protocol("anthropic_messages")).unwrap(),
        hits: 7,
    });
    let active = derive(document(), &healthy(), &counts);

    assert_eq!(
        find(&active["rewrites"], "p-prefix")["hits"],
        json!({"count": 3, "state": "floor"})
    );
    // A record nothing caught is zero or more, not absent.
    assert_eq!(
        find(&active["rewrites"], "p-glob")["hits"],
        json!({"count": 0, "state": "floor"})
    );
    assert_eq!(
        find(&active["presentation_records"], "pr-protocol")["hits"]["count"],
        2
    );
    let default = active["defaults"]
        .as_array()
        .unwrap()
        .iter()
        .find(|default| default["scope_label"] == "anthropic_messages")
        .unwrap();
    assert_eq!(default["hits"], json!({"count": 7, "state": "floor"}));

    let complete = derive(document(), &healthy(), &hits(HitCoverage::Complete));
    assert_eq!(
        find(&complete["rewrites"], "p-glob")["hits"],
        json!({"count": 0, "state": "exact"})
    );

    // Nothing recorded is not zero.
    let unrecorded = derive(document(), &healthy(), &hits(HitCoverage::NotRecorded));
    assert_eq!(
        find(&unrecorded["rewrites"], "p-glob")["hits"],
        json!({"count": null, "state": "not_recorded"})
    );
}

fn shadows<'a>(active: &'a Value, id: &str) -> &'a Vec<Value> {
    find(&active["rewrites"], id)["shadowed_by"]
        .as_array()
        .expect("shadows")
}

#[test]
fn a_rule_is_shadowed_by_an_earlier_rule_of_its_own_scope() {
    let active = derive(document(), &healthy(), &hits(HitCoverage::Complete));

    // `p-wide` is the same prefix as `p-prefix`, so it catches nothing.
    let found: Vec<_> = shadows(&active, "p-wide")
        .iter()
        .filter(|shadow| shadow["by"] == "earlier_rule")
        .collect();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["id"], "p-prefix");
    assert_eq!(found[0]["catches"], "all");
    assert_eq!(found[0]["basis"], "comparison");
    assert_eq!(found[0]["position"], 1);
    assert_eq!(found[0]["scope_limited"], false);
    // The first of a scope has nothing of its own scope above it.
    assert!(
        shadows(&active, "p-prefix")
            .iter()
            .all(|shadow| shadow["by"] != "earlier_rule")
    );
}

#[test]
fn a_protocol_rule_is_shadowed_by_a_profile_rule_for_that_profile_only() {
    let active = derive(document(), &healthy(), &hits(HitCoverage::Complete));

    let found: Vec<_> = shadows(&active, "p-prefix")
        .iter()
        .filter(|shadow| shadow["by"] == "profile_rule")
        .collect();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["id"], "c-prefix");
    assert_eq!(found[0]["catches"], "all");
    assert_eq!(found[0]["profile"], "claude");
    assert_eq!(found[0]["scope_limited"], true);
}

#[test]
fn a_rule_is_shadowed_by_a_presentation_that_publishes_a_name_it_matches() {
    let active = derive(document(), &healthy(), &hits(HitCoverage::Complete));

    // `claude-sonnet` is published, by the profile and the protocol.
    let by_presentation: Vec<_> = shadows(&active, "p-exact")
        .iter()
        .filter(|shadow| shadow["by"] == "presentation")
        .collect();
    assert_eq!(
        by_presentation
            .iter()
            .map(|shadow| shadow["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["pr-profile", "pr-protocol"]
    );
    assert!(
        by_presentation
            .iter()
            .all(|shadow| shadow["catches"] == "all")
    );
    assert_eq!(by_presentation[0]["scope_limited"], true);
    assert_eq!(by_presentation[1]["scope_limited"], false);

    // A glob is judged against the names clients could send, so it is only
    // caught in part, and the names are the evidence.
    let glob = shadows(&active, "p-glob")
        .iter()
        .find(|shadow| shadow["by"] == "presentation" && shadow["id"] == "pr-protocol")
        .expect("the protocol's presentation");
    assert_eq!(glob["catches"], "part");
    assert_eq!(glob["basis"], "names");
    assert_eq!(glob["examples"], json!(["claude-sonnet"]));
}

#[test]
fn a_rule_nothing_overlaps_has_no_shadows() {
    let active = derive(document(), &healthy(), &hits(HitCoverage::Complete));

    // The profile's rule is first in the order, and no presentation publishes
    // a name under its prefix.
    assert!(shadows(&active, "c-prefix").is_empty());
}

#[test]
fn a_protocol_presentation_is_hidden_by_the_profile_s_of_the_same_name() {
    let active = derive(document(), &healthy(), &hits(HitCoverage::Complete));

    let hidden = find(&active["presentation_records"], "pr-protocol")["shadowed_by"]
        .as_array()
        .unwrap();
    assert_eq!(hidden.len(), 1);
    assert_eq!(hidden[0]["id"], "pr-profile");
    assert_eq!(hidden[0]["catches"], "all");
    assert!(
        find(&active["presentation_records"], "pr-profile")["shadowed_by"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_route_says_what_refers_to_it_and_how_many_of_each() {
    let active = derive(document(), &healthy(), &hits(HitCoverage::Complete));

    let sub_only = find(&active["routes"], "sub-only");
    assert_eq!(
        sub_only["matched_by_counts"],
        json!({"presentations": 0, "rewrites": 2, "defaults": 0})
    );
    let referrers: Vec<_> = sub_only["matched_by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|referrer| {
            (
                referrer["kind"].as_str().unwrap(),
                referrer["id"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        referrers,
        [("rewrite", "c-prefix"), ("rewrite", "p-prefix")]
    );

    let claude_sub = find(&active["routes"], "claude-sub");
    assert_eq!(
        claude_sub["matched_by_counts"],
        json!({"presentations": 1, "rewrites": 2, "defaults": 1})
    );
    // A default has no identifier, so it is named by its scope.
    let default = claude_sub["matched_by"]
        .as_array()
        .unwrap()
        .iter()
        .find(|referrer| referrer["kind"] == "default")
        .unwrap();
    assert_eq!(default["id"], Value::Null);
    assert_eq!(default["scope_label"], "anthropic_messages");
}

#[test]
fn a_route_carries_its_providers_reach_and_the_lanes_it_holds() {
    let (policy, document) = compiled(document());
    let pinned = BTreeMap::from([(
        "claude-sub".parse().unwrap(),
        RoutingLaneImpacts {
            total: 3,
            warm: 2,
            cold: 1,
            ..Default::default()
        },
    )]);
    let revision_lanes = BTreeMap::from([
        (
            7,
            RoutingLaneImpacts {
                total: 2,
                warm: 2,
                ..Default::default()
            },
        ),
        (
            9,
            RoutingLaneImpacts {
                total: 1,
                cold: 1,
                ..Default::default()
            },
        ),
    ]);
    let derived = build(&Inputs {
        availability: &healthy(),
        inventory: &inventory(),
        learning: &NoLearning,
        circuits: &Default::default(),
        pinned: &pinned,
        publications: &[],
        active: Some(ActivePolicy {
            policy: &policy,
            document: &document,
        }),
        hits: &hits(HitCoverage::Complete),
        revision_lanes: &revision_lanes,
        revision_at_risk: &Default::default(),
    });

    let route = find(&derived["active"]["routes"], "claude-sub");
    assert_eq!(
        route["providers"],
        json!(["anthropic_sub", "anthropic_api"])
    );
    assert_eq!(route["reach"]["outcome"], "served_first");
    assert_eq!(
        route["pinned_lanes"],
        json!({"warm": 2, "cold": 1, "unknown": 0, "total": 3})
    );
    assert_eq!(
        find(&derived["active"]["routes"], "api-only")["pinned_lanes"]["total"],
        0
    );

    let revisions = derived["revisions"].as_array().unwrap();
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0]["revision_id"], 7);
    assert_eq!(revisions[0]["warm"], 2);
    assert_eq!(revisions[1]["revision_id"], 9);
    assert_eq!(revisions[1]["total"], 1);
}

#[test]
fn a_provider_counts_everything_reaching_it_through_a_fallback_too() {
    let active = derive(document(), &healthy(), &hits(HitCoverage::Complete));
    let providers = &active["providers"];
    let provider = |name: &str| {
        providers
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["provider"] == name)
            .unwrap_or_else(|| panic!("no provider {name}"))
    };

    // anthropic_api comes first on one route only, and second on two more.
    let api = provider("anthropic_api");
    assert_eq!(
        api["routes"]["ids"],
        json!(["api-only", "claude-sub", "guarded"])
    );
    // Reached by: the glob, `p-exact` and `p-wide` on `claude-sub`, and the
    // profile's default on `guarded`; the profile's presentation on
    // `api-only`; the protocol default on `claude-sub`.
    assert_eq!(
        api["rewrites"]["ids"],
        json!(["p-glob", "p-exact", "p-wide"])
    );
    assert_eq!(
        api["rewrites"]["by_protocol"],
        json!({"anthropic_messages": 3})
    );
    assert_eq!(api["defaults"]["count"], 2);
    assert_eq!(
        api["defaults"]["scopes"],
        json!(["claude/anthropic_messages", "anthropic_messages"])
    );
    assert_eq!(
        api["presentations"]["ids"],
        json!(["pr-profile", "pr-protocol"])
    );

    // anthropic_sub, which holds an offer named outright by a presentation.
    let sub = provider("anthropic_sub");
    assert!(
        sub["presentations"]["ids"]
            .as_array()
            .unwrap()
            .contains(&json!("pr-offer"))
    );
    assert_eq!(sub["routes"]["count"], 3);
}

#[test]
fn a_provider_nothing_reaches_says_so_and_a_switched_off_candidate_does_not_count() {
    let mut document = document();
    document["defaults"] = json!([
        {"scope": protocol("anthropic_messages"), "target": target("sub-only", param())},
    ]);
    document["rewrites"] = json!([]);
    document["presentations"] = json!([]);
    let routes = document["routes"].as_array_mut().unwrap();
    routes.retain(|route| route["id"] != "guarded" && route["id"] != "api-only");
    // The only route left to the API is a candidate that is off.
    routes
        .iter_mut()
        .find(|route| route["id"] == "claude-sub")
        .unwrap()["chain"]["steps"][0]["candidate"]["enabled"] = json!(false);
    let active = derive(document, &healthy(), &hits(HitCoverage::Complete));

    let api = active["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["provider"] == "anthropic_api")
        .unwrap();
    assert_eq!(api["routes"]["count"], 0);
    assert_eq!(api["rewrites"]["count"], 0);
    assert_eq!(api["defaults"]["count"], 0);
    assert_eq!(api["presentations"]["count"], 0);
}

/// A document with a family of two models on the subscription, an upgrade
/// asking `minimum`, and a compaction.
fn upgrading(minimum: Value, compaction: Value) -> Value {
    let mut document = document();
    document["classifiers"] = json!([{
        "id": "family",
        "binding_id": SUB,
        "model_matcher": {
            "kind": "regex",
            "value": r"^claude-(?P<family>[a-z]+)-(?P<version>[0-9-]+)$",
        },
        "family_template": "${family}",
        "version_template": "${version}",
        "version_parser": {"kind": "numeric_tuple", "separator": "-"},
    }]);
    document["upgrades"] = json!([{
        "id": "newest",
        "classifier_id": "family",
        "minimum_learning": minimum,
        "inventory_sources": ["catalogue"],
        "lane_transition": "move_when_cold",
    }]);
    let first = &mut document["routes"][0]["chain"]["first"];
    first["model"] = json!({"kind": "exact", "model_id": "claude-sonnet-4-5"});
    first["upgrade_policy_id"] = json!("newest");
    document["compaction"] = compaction;
    document
}

#[test]
fn an_upgrade_says_the_bar_it_asks_and_the_move_it_would_make_now() {
    let document = upgrading(json!({"kind": "none"}), Value::Null);
    let active = derive(document, &healthy(), &hits(HitCoverage::Complete));

    let upgrade = find(&active["upgrades"], "newest");
    assert_eq!(upgrade["evidence_bar"], json!({"kind": "none"}));
    let used = &upgrade["used_by"][0];
    assert_eq!(used["route_id"], "claude-sub");
    assert_eq!(used["provider"], "anthropic_sub");
    assert_eq!(
        used["moves"],
        json!([{"family": "sonnet", "from": "claude-sonnet-4-5", "to": "claude-sonnet-4-6"}])
    );
    assert_eq!(used["held_back"], json!([]));

    // The route names the policy and candidate the move is filed under,
    // rather than shipping a second copy of it.
    let on_route = &find(&active["routes"], "claude-sub")["handled_by"]["upgrade"];
    assert_eq!(on_route["enabled"], true);
    assert_eq!(on_route["candidates"][0]["policy_id"], "newest");
    assert_eq!(
        on_route["candidates"][0]["candidate_id"],
        used["candidate_id"]
    );
    assert_eq!(on_route.get("moves"), None, "{on_route}");
}

#[test]
fn an_upgrade_the_evidence_bar_holds_back_names_the_model_and_its_days() {
    let document = upgrading(json!({"kind": "distinct_days", "days": 3}), Value::Null);
    let active = derive(document, &healthy(), &hits(HitCoverage::Complete));

    let upgrade = find(&active["upgrades"], "newest");
    assert_eq!(
        upgrade["evidence_bar"],
        json!({"kind": "distinct_days", "days": 3})
    );
    let used = &upgrade["used_by"][0];
    assert_eq!(used["bar"]["needs_days"], 3);
    assert_eq!(used["moves"], json!([]));
    assert_eq!(
        used["held_back"],
        json!([{
            "family": "sonnet",
            "model": "claude-sonnet-4-6",
            "distinct_days": 0,
            "promoted": false,
        }])
    );
}

#[test]
fn compaction_names_its_target_and_the_bindings_and_routes_it_applies_to() {
    let substitution = json!({
        "target": {
            "kind": "substitution",
            "model": {"exact": "claude-haiku-4-5"},
            "bindings": [API],
        },
        "lane_transition": "move_when_cold",
        "cache_transition": "allow_when_cold",
    });
    let active = derive(
        upgrading(json!({"kind": "none"}), substitution),
        &healthy(),
        &hits(HitCoverage::Complete),
    );

    let compaction = &active["compaction"];
    assert_eq!(compaction["kind"], "substitution");
    assert_eq!(
        compaction["model"],
        json!({"kind": "exact", "value": "claude-haiku-4-5"})
    );
    assert_eq!(
        compaction["bindings"],
        json!([{"binding_id": API, "provider": "anthropic_api"}])
    );
    assert_eq!(compaction["cache_transition"], "allow_when_cold");
    assert_eq!(
        compaction["routes"],
        json!(["api-only", "claude-sub", "guarded"])
    );
    // Only routes holding one of the bindings are handled by it.
    assert_eq!(
        find(&active["routes"], "claude-sub")["handled_by"]["compaction"]["enabled"],
        true
    );
    assert_eq!(
        find(&active["routes"], "sub-only")["handled_by"]["compaction"],
        json!({"enabled": false})
    );

    let to_route = json!({
        "target": target("api-only", param()),
        "lane_transition": "move_when_cold",
        "cache_transition": "allow_when_cold",
    });
    let active = derive(
        upgrading(json!({"kind": "none"}), to_route),
        &healthy(),
        &hits(HitCoverage::Complete),
    );
    assert_eq!(active["compaction"]["kind"], "route");
    assert_eq!(active["compaction"]["route_id"], "api-only");
    assert_eq!(active["compaction"]["reach"]["outcome"], "served_first");

    let none = derive(document(), &healthy(), &hits(HitCoverage::Complete));
    assert_eq!(none["compaction"], Value::Null);
}

#[test]
fn a_selector_presentation_says_how_many_models_it_lists_and_the_newest_first() {
    let mut document = upgrading(json!({"kind": "none"}), Value::Null);
    document["presentations"] = json!([{
        "id": "pick",
        "scope": protocol("anthropic_messages"),
        "slug": "pick-${model_id}",
        "metadata": {"adapter": "claude", "behaves_as": "sonnet"},
        "source": {
            "kind": "offer_selector",
            "selector": {
                "binding_id": SUB,
                "include": [{"kind": "glob", "value": "claude-sonnet-*"}],
                "exclude": [],
                "ordering": {"kind": "classified_version", "classifier_id": "family"},
                "keep": 5,
            },
        },
    }]);
    let active = derive(document, &healthy(), &hits(HitCoverage::Complete));

    let record = find(&active["presentation_records"], "pick");
    assert_eq!(
        record["lists"],
        json!({
            "count": 2,
            "newest": "claude-sonnet-4-6",
            "models": ["claude-sonnet-4-6", "claude-sonnet-4-5"],
        })
    );
    assert_eq!(record["behaves_as"], "sonnet");
    assert_eq!(
        record["slugs"],
        json!(["pick-claude-sonnet-4-5", "pick-claude-sonnet-4-6"])
    );
}

#[test]
fn a_virtual_presentation_lists_no_models() {
    let active = derive(document(), &healthy(), &hits(HitCoverage::Complete));
    assert_eq!(
        find(&active["presentation_records"], "pr-protocol")["lists"],
        Value::Null
    );
}
