//! The three lookup tables as the routing page reads them: presentations,
//! rewrites and defaults, grouped by the scope they belong to in the order a
//! request is looked up, each record with its reach, how many requests it
//! caught, and what already catches all or part of what it matches
//! (spec: UTUI#reading-a-table).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::{Context, hits_body, matcher_body, reach_body, scope_fields};
use crate::policy::shadow::{Basis, Catches, Earlier, Shadowing, Shape};
use crate::policy::{
    AdapterMetadata, ArgumentBinding, BindingId, CompiledRewrite, FrontendProtocol, FrontendScope,
    MatcherSpec, NoRoute, PolicyDocument, PresentationId, PresentationRoute, PresentationSource,
    ProfileId, REQUESTED_MODEL_PARAMETER, Reach, RewriteId, RewriteRule, RouteId, RouteReference,
    offer_reach,
};

/// Models named in a presentation's `lists`, the rest being counted.
const LISTED_MODELS: usize = 5;

/// What sends a request to a route or an offer: one record of the tables.
#[derive(Debug, Clone)]
pub(super) struct Referrer {
    /// `presentation`, `rewrite` or `default`.
    pub kind: &'static str,
    /// The record's identifier; a default has none, being named by its scope.
    pub id: Option<String>,
    pub scope: FrontendScope,
    /// Its place in its scope's table, from one.
    pub position: Option<usize>,
    pub matcher: Option<Value>,
    pub slugs: Vec<String>,
}

impl Referrer {
    pub(super) fn body(&self) -> Value {
        let mut body = scope_fields(&self.scope);
        body["kind"] = json!(self.kind);
        body["id"] = json!(self.id);
        body["position"] = json!(self.position);
        body["matcher"] = self.matcher.clone().unwrap_or(Value::Null);
        body["slugs"] = json!(self.slugs);
        body
    }
}

/// The tables, as JSON in the order the page draws them, and the references
/// the routes and providers read back.
pub(super) struct Rules {
    pub scopes: Vec<Value>,
    pub presentations: Vec<Value>,
    pub rewrites: Vec<Value>,
    pub defaults: Vec<Value>,
    /// Everything that sends requests to a route, in lookup order.
    pub to_route: BTreeMap<RouteId, Vec<Referrer>>,
    /// Presentations naming an offer on a binding directly, with no route.
    pub to_offer: Vec<(Referrer, BindingId)>,
}

/// One presentation as published: the names it holds and where they go.
struct Published {
    scope: FrontendScope,
    slugs: BTreeSet<String>,
    routes: BTreeSet<RouteId>,
    /// An offer named outright: its generated candidate and binding, and the
    /// model, one per slug.
    offers: Vec<(crate::policy::CandidateId, BindingId, String)>,
    /// The same offers' models, each with the place a selector gave it, so
    /// that the first is the newest the presentation lists.
    ranked: Vec<(usize, String)>,
}

/// What building the tables looks things up in, made once per build.
struct Tables<'a> {
    ctx: &'a Context<'a>,
    /// Scopes in the order the tables are drawn.
    order: Vec<FrontendScope>,
    rewrite_rules: BTreeMap<&'a RewriteId, &'a RewriteRule>,
    published: BTreeMap<PresentationId, Published>,
    /// Every rewrite matcher, compiled and run over the catalogue once.
    shadowing: Shadowing<'a>,
}

/// Scopes in the order the tables are drawn: for each protocol in turn, each
/// profile that has any record, then the protocol itself, since what a
/// profile's rules miss is tried against its protocol's
/// (spec: UTUI#reading-a-table).
/// Every protocol is present, so one with no policy still has a place to say
/// that it refuses.
fn scope_order(document: &PolicyDocument) -> Vec<FrontendScope> {
    let mut profiles: BTreeSet<(FrontendProtocol, String)> = BTreeSet::new();
    let scopes = document
        .defaults
        .iter()
        .map(|record| &record.scope)
        .chain(document.presentations.iter().map(|record| &record.scope))
        .chain(document.rewrites.iter().map(|record| &record.scope));
    for scope in scopes {
        if let FrontendScope::Profile { profile, protocol } = scope {
            profiles.insert((*protocol, profile.as_str().to_owned()));
        }
    }
    let mut order = Vec::new();
    for protocol in [
        FrontendProtocol::AnthropicMessages,
        FrontendProtocol::OpenaiChat,
        FrontendProtocol::OpenaiResponses,
    ] {
        for (_, profile) in profiles.iter().filter(|(at, _)| *at == protocol) {
            if let Ok(profile) = ProfileId::new(profile.clone()) {
                order.push(FrontendScope::Profile { profile, protocol });
            }
        }
        order.push(FrontendScope::Protocol { protocol });
    }
    order
}

fn protocol_of(scope: &FrontendScope) -> FrontendProtocol {
    match scope {
        FrontendScope::Profile { protocol, .. } | FrontendScope::Protocol { protocol } => *protocol,
    }
}

pub(super) fn build(ctx: &Context<'_>, slug_rows: &[Value]) -> Rules {
    // What each presentation publishes and where those names go.
    let mut published: BTreeMap<PresentationId, Published> = BTreeMap::new();
    for (scope, slug, presentation) in ctx.policy.all_presentations() {
        let entry = published
            .entry(presentation.id().clone())
            .or_insert_with(|| Published {
                scope,
                slugs: BTreeSet::new(),
                routes: BTreeSet::new(),
                offers: Vec::new(),
                ranked: Vec::new(),
            });
        entry.slugs.insert(slug.to_owned());
        match presentation.route() {
            PresentationRoute::Reference(reference) => {
                entry.routes.insert(reference.route_id().clone());
            }
            PresentationRoute::Exact {
                candidate_id,
                binding_id,
                model_id,
                ..
            } => {
                entry
                    .offers
                    .push((candidate_id.clone(), binding_id.clone(), model_id.clone()));
                entry.ranked.push((presentation.rank(), model_id.clone()));
            }
        }
    }

    let rewrite_rules: BTreeMap<&RewriteId, &RewriteRule> = ctx
        .document
        .rewrites
        .iter()
        .map(|rule| (&rule.id, rule))
        .collect();
    let tables = Tables {
        ctx,
        order: scope_order(ctx.document),
        shadowing: Shadowing::new(
            &ctx.names,
            ctx.document.rewrites.iter().map(|rule| &rule.model_matcher),
        ),
        rewrite_rules,
        published,
    };
    let Tables {
        order,
        rewrite_rules,
        published,
        ..
    } = &tables;

    let mut rules = Rules {
        scopes: Vec::new(),
        presentations: Vec::new(),
        rewrites: Vec::new(),
        defaults: Vec::new(),
        to_route: BTreeMap::new(),
        to_offer: Vec::new(),
    };

    for scope in order {
        let documented: Vec<_> = ctx
            .document
            .presentations
            .iter()
            .filter(|presentation| presentation.scope == *scope)
            .collect();
        let compiled_rewrites = ctx.policy.rewrites(scope);
        let default = ctx.policy.default_route(scope);

        for (index, presentation) in documented.iter().enumerate() {
            let record = presentation_record(
                ctx,
                presentation,
                index + 1,
                published.get(&presentation.id),
                published,
                slug_rows,
            );
            let referrer = Referrer {
                kind: "presentation",
                id: Some(presentation.id.as_str().to_owned()),
                scope: scope.clone(),
                position: Some(index + 1),
                matcher: None,
                slugs: published
                    .get(&presentation.id)
                    .map(|published| published.slugs.iter().cloned().collect())
                    .unwrap_or_default(),
            };
            if let Some(published) = published.get(&presentation.id) {
                for route_id in &published.routes {
                    rules
                        .to_route
                        .entry(route_id.clone())
                        .or_default()
                        .push(referrer.clone());
                }
                let bindings: BTreeSet<&BindingId> = published
                    .offers
                    .iter()
                    .map(|(_, binding, _)| binding)
                    .collect();
                for binding in bindings {
                    rules.to_offer.push((referrer.clone(), binding.clone()));
                }
            }
            rules.presentations.push(record);
        }

        for (index, rewrite) in compiled_rewrites.iter().enumerate() {
            let Some(rule) = rewrite_rules.get(rewrite.id()) else {
                continue;
            };
            let position = index + 1;
            let record = rewrite_record(&tables, scope, position, rewrite, rule);
            rules
                .to_route
                .entry(rewrite.target().route_id().clone())
                .or_default()
                .push(Referrer {
                    kind: "rewrite",
                    id: Some(rewrite.id().as_str().to_owned()),
                    scope: scope.clone(),
                    position: Some(position),
                    matcher: Some(matcher_body(&rule.model_matcher)),
                    slugs: Vec::new(),
                });
            rules.rewrites.push(record);
        }

        let exists = default.is_some();
        let mut default_record = scope_fields(scope);
        default_record["exists"] = json!(exists);
        match default {
            Some(reference) => {
                let route_id = reference.route_id();
                default_record["route_id"] = json!(route_id);
                default_record["reach"] = match ctx.reach.get(route_id) {
                    Some(reach) => reach_body(ctx, reach, Some(route_id)),
                    None => reach_body(ctx, &Reach::RefusedNoRoute(NoRoute::RouteMissing), None),
                };
                let hits = ctx
                    .hits
                    .defaults
                    .iter()
                    .find(|hits| hits.scope == *scope)
                    .map_or(0, |hits| hits.hits);
                default_record["hits"] = hits_body(ctx.hits, hits);
                rules
                    .to_route
                    .entry(route_id.clone())
                    .or_default()
                    .push(Referrer {
                        kind: "default",
                        id: None,
                        scope: scope.clone(),
                        position: None,
                        matcher: None,
                        slugs: Vec::new(),
                    });
            }
            None => {
                // A scope with no default is not an error to a profile, whose
                // misses go on to its protocol, but a protocol's are refused.
                default_record["route_id"] = Value::Null;
                default_record["reach"] = if matches!(scope, FrontendScope::Protocol { .. }) {
                    reach_body(ctx, &Reach::RefusedNoRoute(NoRoute::NoDefault), None)
                } else {
                    Value::Null
                };
                default_record["hits"] = Value::Null;
            }
        }
        let else_of = match (default.is_some(), scope) {
            (true, _) => "default",
            (false, FrontendScope::Profile { .. }) => "protocol",
            (false, FrontendScope::Protocol { .. }) => "refused",
        };
        // A profile with no default has nothing to show in the defaults tab.
        if exists || matches!(scope, FrontendScope::Protocol { .. }) {
            rules.defaults.push(default_record);
        }

        let mut entry = scope_fields(scope);
        entry["presentations"] = json!(documented.len());
        entry["rewrites"] = json!(compiled_rewrites.len());
        entry["has_default"] = json!(exists);
        entry["else"] = json!(else_of);
        rules.scopes.push(entry);
    }
    rules
}

/// What a rewrite sends on, as the page writes it: `${model}` for what the
/// match captured, `as asked` for the model the client named.
fn sends_body(target: &RouteReference) -> Value {
    match requested_binding(target) {
        None => json!({"kind": "none", "text": Value::Null}),
        Some(ArgumentBinding::RequestedModel) => json!({"kind": "as_asked", "text": "as asked"}),
        Some(ArgumentBinding::RegexCaptureName { name }) => {
            json!({"kind": "capture", "text": format!("${{{name}}}")})
        }
        Some(ArgumentBinding::RegexCaptureIndex { index }) => {
            json!({"kind": "capture", "text": format!("${{{index}}}")})
        }
        Some(ArgumentBinding::Literal { value }) => json!({"kind": "literal", "text": value}),
    }
}

/// The binding of the parameter that carries the model: the requested-model
/// parameter where the route declares it, else the only one.
fn requested_binding(target: &RouteReference) -> Option<&ArgumentBinding> {
    target
        .parameters
        .iter()
        .find(|(name, _)| name.as_str() == REQUESTED_MODEL_PARAMETER)
        .or_else(|| target.parameters.iter().next())
        .map(|(_, binding)| binding)
}

/// A name the rule catches and what it is sent on as: an offer's own name
/// where one is caught, and otherwise a name made from a prefix.
fn example(ctx: &Context<'_>, rewrite: &CompiledRewrite, rule: &RewriteRule) -> Value {
    let shape = Shape::of(&rule.model_matcher);
    let (input, invented) = match &shape {
        Shape::Exact(value) => (value.clone(), false),
        _ => {
            let caught = ctx
                .names
                .iter()
                .find(|name| rewrite.matcher().is_match(name).unwrap_or(false));
            match (caught, &shape) {
                (Some(name), _) => (name.clone(), false),
                (None, Shape::Prefix { prefix, .. }) => (format!("{prefix}model"), true),
                _ => return Value::Null,
            }
        }
    };
    let Some(captures) = rewrite.matcher().captures(&input).ok().flatten() else {
        return Value::Null;
    };
    let output = requested_binding(&rule.target).and_then(|binding| match binding {
        ArgumentBinding::RequestedModel => Some(input.clone()),
        ArgumentBinding::RegexCaptureName { name } => captures.get(name).map(str::to_owned),
        ArgumentBinding::RegexCaptureIndex { index } => {
            captures.get(&index.to_string()).map(str::to_owned)
        }
        ArgumentBinding::Literal { value } => Some(value.clone()),
    });
    json!({"in": input, "out": output, "invented": invented})
}

fn rewrite_record(
    tables: &Tables<'_>,
    scope: &FrontendScope,
    position: usize,
    rewrite: &CompiledRewrite,
    rule: &RewriteRule,
) -> Value {
    let ctx = tables.ctx;
    let route_id = rewrite.target().route_id();
    let mut record = scope_fields(scope);
    record["id"] = json!(rewrite.id());
    record["priority"] = json!(rewrite.priority());
    record["position"] = json!(position);
    record["matcher"] = matcher_body(&rule.model_matcher);
    record["sends"] = sends_body(&rule.target);
    record["route_id"] = json!(route_id);
    record["example"] = example(ctx, rewrite, rule);
    record["reach"] = match ctx.reach.get(route_id) {
        Some(reach) => reach_body(ctx, reach, Some(route_id)),
        None => reach_body(ctx, &Reach::RefusedNoRoute(NoRoute::RouteMissing), None),
    };
    record["hits"] = hits_body(
        ctx.hits,
        ctx.hits.rewrites.get(rewrite.id()).copied().unwrap_or(0),
    );
    record["shadowed_by"] = json!(rewrite_shadows(
        tables,
        scope,
        position,
        &rule.model_matcher,
    ));
    record
}

/// What is looked at before this rewrite and catches all or part of what it
/// matches: a presentation, a rule of a profile ahead of its protocol, or an
/// earlier rule in its own scope (spec: UTUI#reading-a-table).
///
/// A record of a profile ahead of a protocol's rule catches only that
/// profile's requests, which the entry says with `scope_limited`.
fn rewrite_shadows<'a>(
    tables: &Tables<'a>,
    scope: &FrontendScope,
    position: usize,
    spec: &'a MatcherSpec,
) -> Vec<Value> {
    let Tables {
        ctx,
        order,
        rewrite_rules,
        published,
        shadowing,
    } = tables;
    let protocol = protocol_of(scope);
    let in_protocol_scope = matches!(scope, FrontendScope::Protocol { .. });
    let mut found = Vec::new();

    // Presentations are looked up first: the profile's, then the protocol's.
    // A protocol's rewrite sits behind every profile's presentations too.
    let presentations = |wanted: &dyn Fn(&FrontendScope) -> bool| {
        ctx.document
            .presentations
            .iter()
            .filter(|presentation| wanted(&presentation.scope))
            .filter_map(|presentation| {
                published
                    .get(&presentation.id)
                    .map(|published| (presentation, published))
            })
            .collect::<Vec<_>>()
    };
    let profile_wanted = |candidate: &FrontendScope| {
        matches!(candidate, FrontendScope::Profile { .. })
            && protocol_of(candidate) == protocol
            && (in_protocol_scope || candidate == scope)
    };
    let protocol_wanted = |candidate: &FrontendScope| {
        matches!(candidate, FrontendScope::Protocol { .. }) && protocol_of(candidate) == protocol
    };
    for (presentation, published) in presentations(&profile_wanted)
        .into_iter()
        .chain(presentations(&protocol_wanted))
    {
        let Some((catches, basis)) = shadowing.compare(&Earlier::Names(&published.slugs), spec)
        else {
            continue;
        };
        found.push(shadow_entry(
            &Catcher {
                by: "presentation",
                id: Some(presentation.id.as_str()),
                scope: &presentation.scope,
                position: None,
                matcher: None,
            },
            catches,
            &basis,
            in_protocol_scope && matches!(presentation.scope, FrontendScope::Profile { .. }),
        ));
    }

    // Then a profile's rules, ahead of the protocol's.
    if in_protocol_scope {
        for profile_scope in order.iter().filter(|candidate| {
            matches!(candidate, FrontendScope::Profile { .. }) && protocol_of(candidate) == protocol
        }) {
            for (index, earlier) in ctx.policy.rewrites(profile_scope).iter().enumerate() {
                let Some(rule) = rewrite_rules.get(earlier.id()) else {
                    continue;
                };
                if let Some((catches, basis)) =
                    shadowing.compare(&Earlier::Matcher(&rule.model_matcher), spec)
                {
                    found.push(shadow_entry(
                        &Catcher {
                            by: "profile_rule",
                            id: Some(earlier.id().as_str()),
                            scope: profile_scope,
                            position: Some(index + 1),
                            matcher: Some(&rule.model_matcher),
                        },
                        catches,
                        &basis,
                        true,
                    ));
                }
            }
        }
    }

    // And last the rules above it in its own scope.
    for (index, earlier) in ctx
        .policy
        .rewrites(scope)
        .iter()
        .enumerate()
        .take(position - 1)
    {
        let Some(rule) = rewrite_rules.get(earlier.id()) else {
            continue;
        };
        if let Some((catches, basis)) =
            shadowing.compare(&Earlier::Matcher(&rule.model_matcher), spec)
        {
            found.push(shadow_entry(
                &Catcher {
                    by: "earlier_rule",
                    id: Some(earlier.id().as_str()),
                    scope,
                    position: Some(index + 1),
                    matcher: Some(&rule.model_matcher),
                },
                catches,
                &basis,
                false,
            ));
        }
    }
    found
}

/// The earlier record that catches some of a rewrite's matches.
struct Catcher<'a> {
    by: &'a str,
    id: Option<&'a str>,
    scope: &'a FrontendScope,
    position: Option<usize>,
    matcher: Option<&'a MatcherSpec>,
}

fn shadow_entry(
    catcher: &Catcher<'_>,
    catches: Catches,
    basis: &Basis,
    scope_limited: bool,
) -> Value {
    let mut entry = scope_fields(catcher.scope);
    entry["by"] = json!(catcher.by);
    entry["id"] = json!(catcher.id);
    entry["position"] = json!(catcher.position);
    entry["matcher"] = catcher.matcher.map_or(Value::Null, matcher_body);
    entry["catches"] = json!(catches.code());
    match basis {
        Basis::Comparison => {
            entry["basis"] = json!("comparison");
            entry["compared"] = Value::Null;
            entry["examples"] = json!([]);
        }
        Basis::Names { compared, examples } => {
            entry["basis"] = json!("names");
            entry["compared"] = json!(compared);
            entry["examples"] = json!(examples);
        }
    }
    entry["scope_limited"] = json!(scope_limited);
    entry
}

fn presentation_record(
    ctx: &Context<'_>,
    presentation: &crate::policy::Presentation,
    position: usize,
    published: Option<&Published>,
    all: &BTreeMap<PresentationId, Published>,
    slug_rows: &[Value],
) -> Value {
    let mut record = scope_fields(&presentation.scope);
    record["id"] = json!(presentation.id);
    record["position"] = json!(position);
    record["enabled"] = json!(presentation.enabled);
    record["slug"] = json!(presentation.slug);
    record["label"] = json!(presentation.label);
    record["shadow_native"] = json!(presentation.shadow_native);
    record["source"] = match &presentation.source {
        PresentationSource::ExactOffer {
            binding_id,
            model_id,
        } => json!({"kind": "exact_offer", "binding_id": binding_id, "model_id": model_id}),
        PresentationSource::OfferSelector { selector, .. } => json!({
            "kind": "offer_selector",
            "binding_id": selector.binding_id,
            "include": selector.include.iter().map(matcher_body).collect::<Vec<_>>(),
            "exclude": selector.exclude.iter().map(matcher_body).collect::<Vec<_>>(),
            "keep": selector.keep,
            "ordering": selector.ordering,
        }),
        PresentationSource::Virtual { target } => {
            json!({"kind": "virtual", "route_id": target.route_id})
        }
    };

    let slugs: Vec<&String> =
        published.map_or_else(Vec::new, |published| published.slugs.iter().collect());
    record["slugs"] = json!(slugs);
    record["behaves_as"] = match &presentation.metadata {
        Some(AdapterMetadata::Claude { behaves_as }) => json!(behaves_as),
        _ => Value::Null,
    };
    // What it lists, newest first as its selector ordered them. A virtual
    // presentation lists one name and no models.
    record["lists"] = match published {
        Some(published) if !published.ranked.is_empty() => {
            let mut models = published.ranked.clone();
            models.sort();
            json!({
                "count": models.len(),
                "newest": models[0].1,
                "models": models.iter().map(|(_, model)| model).take(LISTED_MODELS).collect::<Vec<_>>(),
            })
        }
        _ => Value::Null,
    };
    // Whether what it publishes is listed, from the per-name rows the
    // publication state is read from.
    let rows: Vec<&Value> = slug_rows
        .iter()
        .filter(|row| row["id"] == json!(presentation.id))
        .collect();
    let listed = rows.iter().filter(|row| row["desired"] == "listed").count();
    record["listed"] = json!(listed);
    record["published"] = if rows.is_empty() {
        Value::Null
    } else if listed == rows.len() {
        json!("yes")
    } else if listed == 0 {
        json!("no")
    } else {
        json!("partly")
    };
    record["route_ids"] = json!(published.map_or_else(Vec::new, |published| {
        published.routes.iter().collect::<Vec<_>>()
    }));
    record["bindings"] = json!(
        published
            .map(|published| published
                .offers
                .iter()
                .map(|(_, binding, _)| binding)
                .collect::<BTreeSet<_>>())
            .unwrap_or_default()
    );

    // The least healthy reach of what it publishes. A selector's names are
    // all offers of one binding, so they only differ where the catalogue
    // does; the worst of them is what to show.
    let reaches: Vec<(Reach, Option<&RouteId>)> = published.map_or_else(Vec::new, |published| {
        let by_route = published.routes.iter().filter_map(|route_id| {
            ctx.reach
                .get(route_id)
                .map(|reach| (reach.clone(), Some(route_id)))
        });
        let by_offer = published.offers.iter().map(|(candidate, binding, model)| {
            (
                offer_reach(candidate, binding, model, ctx.availability, ctx.inventory),
                None,
            )
        });
        by_route.chain(by_offer).collect()
    });
    record["reach"] = reaches
        .iter()
        .max_by_key(|(reach, _)| reach.rank())
        .filter(|_| presentation.enabled)
        .map_or(Value::Null, |(reach, route_id)| {
            reach_body(ctx, reach, *route_id)
        });
    record["hits"] = hits_body(
        ctx.hits,
        ctx.hits
            .presentations
            .get(&presentation.id)
            .copied()
            .unwrap_or(0),
    );
    record["shadowed_by"] = json!(presentation_shadows(presentation, published, all));
    record
}

/// A protocol's presentation is hidden, for a profile, by that profile's own
/// presentation of the same name, which is looked up first.
fn presentation_shadows(
    presentation: &crate::policy::Presentation,
    published: Option<&Published>,
    all: &BTreeMap<PresentationId, Published>,
) -> Vec<Value> {
    let (Some(published), FrontendScope::Protocol { protocol }) = (published, &presentation.scope)
    else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (id, other) in all {
        if !matches!(other.scope, FrontendScope::Profile { .. })
            || protocol_of(&other.scope) != *protocol
        {
            continue;
        }
        let shared: Vec<String> = published
            .slugs
            .intersection(&other.slugs)
            .cloned()
            .collect();
        if shared.is_empty() {
            continue;
        }
        let catches = if shared.len() == published.slugs.len() {
            Catches::All
        } else {
            Catches::Part
        };
        found.push(shadow_entry(
            &Catcher {
                by: "presentation",
                id: Some(id.as_str()),
                scope: &other.scope,
                position: None,
                matcher: None,
            },
            catches,
            &Basis::Names {
                compared: published.slugs.len(),
                examples: shared.into_iter().take(3).collect(),
            },
            true,
        ));
    }
    found
}
