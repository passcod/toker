# Live model routing policy and TUI

## Purpose

Replace the growing collection of provider-specific model picker, model map,
default backend, and force-newest configuration with one live, versioned model
policy edited through a second TUI page.

The policy presents models to every frontend, resolves requested names onto
ordered backend targets, and upgrades a selected target within its model
family. Those three decisions activate atomically, so toker never publishes a
model whose route is absent or applies an upgrade rule from a different policy
revision.

Provider credentials, endpoints, listener configuration, and provider enablement
remain bootstrap configuration. Operational model policy moves into the SQLite
state directory and can change without restarting the service.

[`live-model-routing-implementation.md`](live-model-routing-implementation.md)
maps this design onto the current store, route registry, inference handlers,
publication adapters, control API, and TUI in implementation changesets.

## Outcomes

- Every frontend sees a deliberate model list assembled from all configured
  backend catalogues.
- A presented model may be a concrete backend offer, the newest matching offer,
  or a user-defined virtual model with no provider-owned slug.
- A requested model resolves to an ordered list of backend targets rather than
  depending implicitly on a protocol default.
- Route targets can fall back when positive evidence says an earlier target
  cannot serve the request.
- Family classification and version ordering are configurable for every
  backend, while learned eligibility, promotion, context fit, and cache safety
  continue to govern transparent upgrades.
- Policy edits are drafted, validated, previewed, and activated as one revision
  from the TUI.
- Existing warm lanes remain stable unless the activation or fallback policy
  explicitly permits a transition.
- The ledger records the frontend-visible identity, baseline resolved target,
  final upgraded target, policy revision, and fallback reason without recording
  request or response content.

## Non-goals

- The routing page does not collect or display credentials.
- It does not make an unconfigured provider configured; it may retain that
  provider as an unavailable candidate in a fallback chain.
- It does not infer model families, versions, capabilities, or prices from
  names. Each fact comes from a catalogue, a verified backend binding, or an
  explicit policy rule.
- It does not fail over after response bytes have reached the frontend.
- It does not make provider-encrypted reasoning portable across providers.
- It does not let the service write arbitrary frontend configuration outside
  the state directory. User-side reconcilers retain that responsibility.

## Model identities

Keep these identities distinct throughout routing and recording:

1. **Offer identity**: one model advertised by one backend binding, including
   backend, provider-owned model id, protocol capabilities, catalogue source,
   and catalogue freshness.
2. **Presented identity**: the slug, label, description, and compatibility hints
   exposed to one frontend.
3. **Route identity**: a named policy containing ordered backend candidates.
4. **Baseline target**: the candidate and model chosen before an upgrade.
5. **Effective target**: the exact backend and model sent upstream.

A presented identity may reference a concrete offer or a route identity. A
virtual model is a presented identity backed by a route identity and need not
share a slug with any offer.

The same virtual slug may be exposed on several frontends. Each exposure names
the route used for that frontend protocol and carries frontend-specific
metadata. Policy validation refuses duplicate slugs within one frontend
namespace and requires an explicit override to shadow a native model.

## Policy data model

Store immutable activated revisions plus one mutable draft in SQLite. Normal
service startup reads the active revision before serving inference traffic.
One activation transaction validates and promotes the complete draft.

### Policy revisions

Each revision carries:

- monotonically increasing revision id;
- creation and activation timestamps;
- optional user-facing description;
- activation scope;
- the presentation, route, family, and upgrade records belonging to it.

Retain prior revisions so the TUI can inspect and reactivate one. Reactivation
creates a new revision rather than mutating history.

### Presentations

A presentation record names a frontend and one of three sources:

- an exact offer;
- an offer selector with include globs, exclude globs, ordering, and keep count;
- a virtual model referencing a route.

It also carries the frontend slug, label, description, enabled state, and an
adapter-owned metadata object. Claude's `behavesAs` belongs in that metadata;
it is not a provider or canonical-model property.

Selectors evaluate over the joined `catalog::offers` inventory, never a raw
provider catalogue. Only offers with a complete route to the frontend protocol
are candidates. Newest selection uses explicit parsed version facts or
catalogue chronology, never lexical guessing.

### Routes and candidates

A route has a stable id, display name, ordered candidates, and lane transition
policy. Each candidate carries:

- backend binding;
- exact model or offer selector;
- optional upgrade policy;
- required request capabilities;
- classified fallback conditions;
- whether falling to it crosses a cost boundary that needs explicit consent.

Presentation records and transparent rewrite rules both reference routes. This
keeps fallback behavior in one place and prevents a picker entry from silently
disagreeing with request routing.

### Transparent rewrite rules

An ordered rewrite rule matches:

- frontend identity;
- frontend protocol;
- requested model by exact value, glob, or configured family classifier.

It resolves to a route and may supply captures to a candidate's model template.
Rules therefore choose the backend as well as the rewritten model. A model
string rewrite that leaves backend selection to a protocol default is not a
complete rule.

Exact routes generated for presented slugs run before general rewrite rules.
General rules handle frontend-owned names that were never presented, including
Claude Code side calls. Ambiguous rules at the same priority are validation
errors rather than first-match accidents.

Protocol defaults remain a final compatibility fallback for unmatched bare
names. The TUI shows them alongside policy rules, but a presented or virtual
model never relies on one implicitly.

### Family classifiers and versions

A classifier is scoped to a backend model namespace and contains:

- an exact, glob, or regular-expression matcher;
- a family identity;
- explicit version captures;
- a declared parser and ordering for those captures.

Supported version parsers cover known structured forms such as semantic
versions, release dates, and numeric generation tuples. Unparseable captures
make that model unclassified; toker does not repair or order them heuristically.

Classification produces a backend-local family and structured version. An
upgrade never moves between route candidates or backends. Fallback chooses a
candidate; upgrade selects an eligible version within that candidate's family.

## Resolution pipeline

Resolve every inference request through these stages:

1. Parse the frontend protocol and requested model.
2. Resolve an exact presented or virtual slug when one exists.
3. Otherwise evaluate transparent rewrite rules, then the protocol default.
4. Load or choose the lane's pinned route candidate.
5. Reject candidates that cannot satisfy verified request capabilities.
6. Apply that candidate's family classifier and upgrade policy.
7. Run learned eligibility, promotion, context-fit, and cache-safety decisions.
8. Produce the final `ModelTarget` used by the backend binding and ledger row.

The result includes content-free provenance for every decision: policy
revision, presentation or rewrite rule, chosen candidate, skipped candidates
and reasons, upgrade decision, and final binding.

## Availability and fallback

Availability is evidence-based and tri-state: available, unavailable, or
unknown. Unknown never silently becomes unavailable.

Positive preflight evidence of unavailability includes:

- provider absent or disabled in bootstrap configuration;
- missing or known-expired credentials;
- no verified binding from the frontend protocol;
- request capabilities the binding cannot represent;
- a fresh catalogue positively missing an exact target;
- an explicitly disabled candidate;
- an open health circuit created from classified provider failures.

A stale or absent catalogue is unknown, not proof that a target is absent.

Each candidate declares its post-send fallback conditions. Initially support
explicit provider authentication failure, model-not-found, rate limit, quota,
overload, server error, and transport failure before response bytes. Keep
ambiguous transport failure distinct because the provider may already have
accepted and billed the request. Never retry another candidate after any
response byte has crossed to the frontend.

Crossing from a subscription to a billed API or router is a cost boundary. The
policy must opt into such a fallback explicitly, and the TUI marks it before
activation.

Record attempted candidates and classified outcomes without storing provider
error bodies beyond the existing safe error fields. Accounting for every
attempt remains attached to the backend that received it; fallback must not
collapse multiple upstream attempts into one fictional charge.

## Lane stability and activation

Pin the resolved route candidate, effective model, and policy revision to the
lane. Include a route fingerprint in lane continuity so a stable virtual slug
whose target changed cannot make two provider/model contexts appear to be one
warm lane.

Activation supports three scopes:

- **new lanes**: existing lanes retain their revision and target;
- **new and cold lanes**: a cold existing lane may resolve under the new
  revision;
- **immediate**: the next request may resolve under the new revision, with an
  explicit warning listing warm lanes that could move.

Recovery of a higher-priority candidate never pulls a pinned lane back by
itself. A mid-session fallback pins the replacement candidate for later turns.
When the move crosses providers, compatible opaque replay rules still apply;
the fallback mechanism does not make foreign encrypted reasoning usable.

The existing cache rule remains authoritative: a model transition happens only
when no cache can be lost or the activation explicitly forces it. Forced
activation is recorded as such.

## Upgrade policy

Adapt the learned-model and force-newest mechanisms to operate on classified
backend-local families rather than hardcoded name parsing.

An upgrade policy selects among versions that:

- belong to the chosen candidate's family;
- are present in the applicable offer inventory or have been served and
  learned through that binding;
- meet distinct-day learning or explicit promotion requirements;
- have a verified or learned context ceiling that fits the request;
- do not downgrade the structured version;
- satisfy cache and lane transition rules.

Manual promotion remains available from the routing TUI and the CLI. Promotion
updates learned eligibility, not the routing policy revision. The routing page
shows which candidate version would win now and why another version does not.

## Frontend publication adapters

Make publication a frontend capability with adapter-specific desired and
applied state.

### Dynamic model endpoints

OpenAI Chat and Responses `/v1/models` responses derive directly from the
active policy and current offers. A virtual entry uses its presented slug and
safe known metadata; it never copies arbitrary provider catalogue objects.
When no policy route is currently usable, the endpoint must distinguish a
temporarily unavailable inventory from a genuinely empty configured list.

### Claude Code settings

Claude's model picker remains a user-side reconciliation target because the
service may write only in its state directory. Generalise `toker picker sync`
from OpenRouter rules to the active presentation policy. It owns only entries
tagged by toker, retains unrelated user rows, and writes the desired policy
revision into its own state after a successful atomic settings update.

The TUI runs as the user and may invoke the same reconciliation library after
activation. Failure to update Claude settings does not roll back the server
policy; it leaves the adapter in a visible pending or failed state with the
last applied revision. The periodic user timer converges it later.

### Other frontends

Add publication adapters only after verifying the frontend's discovery or
configuration surface. A frontend with no writable list still receives exact
virtual and rewrite routing when it requests a configured slug; the TUI labels
that presentation adapter unsupported rather than inventing a config format.

## Control surface

Add gated `/_toker/` endpoints for:

- reading offers, active policy, draft policy, and publication status;
- replacing or patching a draft;
- validating a draft and returning content-free diagnostics;
- previewing resolution for an invented model/request shape;
- activating a validated draft with a chosen scope;
- listing prior revisions and reactivating one;
- promoting a classified model through the existing learned-model operation.

Every mutation requires its matching `x-toker-control` verb and JSON content
type. The service serialises policy mutations so two TUI instances cannot
overwrite one another; draft writes carry an expected revision token.

Policy data and diagnostics contain no credentials, prompts, completions, tool
arguments, or provider response bodies.

## Routing TUI page

Add a top-level page beside the existing operational dashboard. Switching pages
does not discard an uncommitted draft.

The routing page contains four coordinated views:

1. **Presented models**: frontend, slug, label, source, desired/applied state,
   and current route summary.
2. **Routes**: ordered candidates, live availability, cost boundary, pinned
   lanes, fallback conditions, and selected baseline target.
3. **Rewrites and families**: matchers, shadowing diagnostics, parsed family and
   version examples, and upgrade winner.
4. **Preview and activation**: a content-free request shape, full resolution
   trace, affected lanes, validation findings, revision diff, and activation
   scope.

Editing uses forms appropriate to field type rather than raw TOML. Catalogue
and offer selectors provide searchable completion, but retain explicitly typed
values that are presently unavailable so fallback policies can be prepared
before a provider is configured.

The page visibly distinguishes draft, active, and externally applied revisions.
Destructive removal and immediate warm-lane activation require confirmation.

## Configuration migration

On first startup without an active database policy, compile current static
configuration into revision one:

- OpenRouter picker rules become presentation selectors and Claude metadata;
- each provider model map becomes ordered transparent rewrite rules targeting
  that provider binding;
- protocol defaults become compatibility default routes;
- existing learned model rows and promotion state remain in their current
  tables and are interpreted through the migrated classifiers;
- the force-newest toggle becomes the enabled state of the generated upgrade
  policies.

Write a migration marker so later TOML edits do not silently replace live
policy. After migration, setup reports that operational model routing is owned
by the TUI and offers an explicit re-import action rather than merging files
implicitly.

Config serialization retains legacy fields while migration support is needed,
but generated setup config stops expanding model policy into TOML once the
database policy exists.

## Implementation sequence

1. Introduce the policy schema, immutable revisions, draft lifecycle, and
   content-free provenance types in the store and routing modules.
2. Generalise catalogue offers into the inventory consumed by presentation,
   candidate selectors, and family classification.
3. Compile existing defaults, model maps, picker rules, and force-newest state
   into an in-memory policy representation without changing request behavior.
4. Route inference through exact presented identities, named routes, ordered
   candidates, and lane pins while retaining static configuration as the source.
5. Add evidence-based candidate availability and preflight fallback, followed
   by explicitly classified post-send fallback before response bytes.
6. Move family parsing and version comparison behind configured classifiers;
   adapt learned eligibility, promotion, context fit, and force-newest to the
   chosen candidate.
7. Persist revision one from legacy configuration and switch runtime reads to
   the active database policy.
8. Generalise `/v1/models` projection and Claude picker reconciliation from the
   active presentation records, including desired/applied revision state.
9. Add the gated draft, validation, preview, activation, rollback, and promotion
   control endpoints.
10. Add the routing TUI page and forms over those endpoints, including route
    traces, availability reasons, cost-boundary warnings, lane impact, and
    external publication status.
11. Stop writing new operational model policy into setup-generated TOML while
    retaining explicit legacy import and compatibility reading.
12. Reconcile the implementation against this plan, document the resulting
    internals, and remove this plan only when every implementation item is
    complete or deliberately moved to a separately recorded follow-up.

## Completion criteria

The feature is complete when one activated revision can, without a restart:

- publish concrete, selected-latest, and virtual models to each supported
  frontend;
- route a virtual or rewritten name to an ordered backend candidate chain;
- explain candidate availability and fallback choices;
- keep warm lanes pinned while applying the requested activation scope;
- upgrade within the selected candidate's configured family using learned and
  promoted evidence;
- reconcile external frontend configuration and report its applied revision;
- roll back by activating a prior policy as a new revision;
- record every routing identity and decision without storing conversation
  content.
