# Live model routing implementation

## Purpose

Implement the policy described in
[`live-model-routing-policy.md`](live-model-routing-policy.md) without creating
a second, temporary routing system that survives beside the final one.

The implementation moves model presentation, rewrites, fallback chains, and
family upgrades into one compiled policy. The provider registry remains the
source of configured backends, credentials, endpoints, and verified wire
bindings. Each request captures one immutable compiled policy revision and
resolves to a content-free attempt plan before any upstream request is sent.

This plan is complete only when the completion criteria in the design plan are
implemented. It is not a smaller first version of that design.

## Settled contracts

### Frontend scope

A policy item has one of two frontend scopes:

- `profile`, naming an exact `/f/<name>` frontend and its protocol;
- `protocol`, applying to every frontend using that protocol.

An exact profile scope wins over a protocol scope. Within one scope, exact
presented slugs win over rewrite rules, and rewrite priorities are unique.
This gives named frontends local overrides while allowing new or hand-written
frontend prefixes to inherit a useful protocol policy.

The unprefixed frontend is a real profile named `unprefixed`; it does not
silently borrow the policy of a named frontend.

### Draft and activation ownership

The daemon owns one global draft. Draft writes carry an expected generation;
a stale writer receives a conflict and must reload. A TUI can inspect the
ledger while the daemon is unavailable, but editing, validation, preview, and
activation require the daemon's gated control API.

The active policy is an immutable `Arc<CompiledPolicy>`. Every inference
request clones that `Arc` once. Activation commits the revision, swaps the
active `Arc`, and updates the publication target while holding the policy
mutation lock. Requests already in progress finish on the revision they
captured.

Lanes pinned to an older revision cause that compiled revision to be loaded
from the revision store and retained in a bounded in-memory cache. Old lanes
therefore do not get reinterpreted through the current policy.

### Matchers and templates

Policy matchers are exact strings, globs, or Rust regular expressions. Globs
use `globset`. Regular expressions use the Rust `regex` crate, added with
`cargo add` when the matcher compiler lands. Only regular expressions expose
captures. Templates use `${name}` and `${1}` placeholders and are rejected if
they reference a capture their matcher does not define.

Rewrite priorities are integers unique within a frontend scope. Profile rules
run before protocol rules. This makes overlap intentional and removes any need
to guess whether two arbitrary regular expressions intersect.

### Version ordering

A family classifier declares one parser for every version it emits:

- semantic version;
- `YYYY-MM-DD` or `YYYYMMDD` release date;
- numeric tuple with a declared component count.

Semantic-version support is added through `cargo add`, rather than pinning an
unverified dependency version in advance. Numeric tuples compare component by
component and never parse decimal-looking text as a number. A family may not
mix parser kinds. Missing, malformed, or overflowing captures leave an offer
unclassified.

No built-in name heuristic runs after policy compilation. Revision one carries
explicit classifiers generated from the legacy behavior.

### Availability and cost

Availability is `available`, `unavailable`, or `unknown`, with a list of
content-free evidence. Only `unavailable` skips a candidate. An absent or stale
catalogue is `unknown`, so it cannot remove a candidate.

Provider bindings declare a cost class:

- `subscription` for `anthropic_sub` and `codex_sub`;
- `billed` for `anthropic_api` and `openrouter`;
- `unknown` for a future binding whose cost semantics are not verified.

A route's fallback edge, not merely its destination candidate, grants or
refuses a move into `billed`. This applies to preflight skips and post-send
fallback alike. A route that would cross that boundary without an explicit
grant is invalid.

Health circuits are policy data. Each candidate may map a classified failure
to a circuit duration. No implicit duration is guessed. A successful attempt,
a policy activation, or a relevant credential/catalogue generation change
clears the matching circuit.

### Response commitment

Fallback is possible only before the frontend response head is committed.
For a streaming attempt, the backend adapter retains the upstream head and
enough of the stream to classify an immediate provider error or the first
canonical event. Once the frontend head or any body bytes are emitted, the
attempt is committed and no later failure can select another candidate.

`ambiguous_transport` is distinct from a transport failure proven to have
happened before the request was accepted. It is never enabled by migration and
requires an explicit fallback condition because the first provider may have
accepted and billed the request.

### Publication metadata

A concrete presentation may publish verified fields from its exact offer. A
virtual presentation publishes only policy-owned metadata and verified facts
common to every currently viable candidate. It never copies an arbitrary raw
catalogue object from the candidate that happens to win.

Claude settings remain a user-side publication adapter. The daemon records
desired state but never writes those settings. Codex and opencode use their
dynamic `/v1/models` surfaces until a separate, verified writable discovery
surface exists.

## Policy document

Add `src/policy/` and make its serialisable document independent of runtime
provider objects. The top-level `PolicyDocument` has a format version and
ordered collections of:

- protocol defaults;
- presentations;
- named routes;
- transparent rewrite rules;
- family classifiers;
- upgrade policies.

Every record has a stable user-visible id. Identifiers are lowercase ASCII
letters, digits, `-`, `_`, and `.`, with ids unique within their record kind.
Descriptions and labels are display text; they never participate in identity.

### Presentations

Represent the three presentation sources explicitly:

- `ExactOffer` names a binding and exact backend model id;
- `OfferSelector` contains include and exclude matchers, ordering, keep count,
  and slug/label templates;
- `Virtual` names an existing route.

Every presentation also carries its frontend scope, slug or slug template,
label, description, enabled state, shadow-native consent, and adapter-owned
metadata. The compiler preserves metadata as typed adapter input rather than
interpreting fields such as Claude's `behavesAs` as model facts.

An exact offer compiles to a generated one-candidate route. Each selected
offer also compiles to a generated exact route, so publication and resolution
cannot disagree. A selector defaults to the offer's routed id for its slug;
policy templates may instead create a stable or friendlier slug. Duplicate
expanded slugs in one frontend namespace are validation errors.

Selector ordering is one of:

- catalogue chronology, using verified catalogue fields and fetch chronology;
- classified version, naming a family classifier;
- explicit policy order.

Lexical model-id order is available only as a final deterministic tiebreaker,
never as evidence that one model is newer.

### Routes and edges

A route contains ordered candidates and an edge between each adjacent pair.
A route also has a display name and its lane transition policy. A candidate
has:

- stable candidate id;
- binding id;
- exact model, requested model, capture template, or offer selector;
- optional upgrade-policy id;
- required capability additions;
- enabled state.

An edge has:

- preflight fallback enabled or disabled;
- classified post-send conditions;
- billed-boundary consent;
- ambiguous-transport consent.

Resolution walks candidates in order. Skipping several unavailable candidates
requires every crossed edge to permit the relevant transition. Recovery of an
earlier candidate does not move a pinned lane back automatically.

### Families and upgrades

A classifier is scoped to a binding and contains a matcher, a family template,
a version parser, and its captures. Compilation produces a binding-local
`ClassifiedModel { family, version }`.

An upgrade policy contains:

- classifier id;
- enabled state;
- minimum learning rule;
- whether manual promotion is accepted;
- eligible inventory sources;
- lane transition policy;
- whether revision-one legacy learning may be consulted.

Only the generated revision-one policies can enable legacy learning. New and
edited policies use binding-scoped learning.

### Defaults and rewrites

A protocol default is a route id scoped to a frontend protocol. It is the final
compatibility route for an unmatched bare name and may use the requested model
as a candidate template.

A rewrite rule contains frontend scope, unique priority, model matcher, route
id, and optional capture values supplied to candidate templates. Exact
presented slugs are compiled into a separate lookup and always precede this
ordered rule set.

## Stored representation

Add the following in the next append-only schema migration.

### `routing_policy_revisions`

One immutable row per activation:

- `revision_id INTEGER PRIMARY KEY`;
- `created_ms INTEGER NOT NULL`;
- `activated_ms INTEGER NOT NULL`;
- `description TEXT`;
- `activation_scope TEXT NOT NULL`;
- `source TEXT NOT NULL` (`legacy`, `draft`, or `reactivation`);
- `parent_revision_id INTEGER`;
- `document_json TEXT NOT NULL`;
- `fingerprint TEXT NOT NULL`.

The document is stored as canonical JSON because it is loaded, validated, and
activated as one unit. Child policy records are not independently mutable, so
normalising them into editable SQL rows would introduce partial-policy states
without providing a query path the service needs.

### `routing_policy_state`

A singleton row contains:

- active revision id;
- draft generation, starting at zero and increasing on every mutation;
- draft base revision id;
- nullable draft canonical JSON;
- draft update timestamp;
- legacy-import timestamp.

Store methods perform compare-and-swap draft writes and activation
transactions. Reactivating an old revision copies its document into a new
revision row; the active pointer never moves backwards to an old row.
Activation resets the draft base to the new revision, retains the activated
document as the next editable draft, and advances its generation. A successful
writer can therefore continue editing without reconstructing the policy.

### `routing_publications`

One row per frontend scope and adapter contains desired revision, applied
revision, desired and applied inventory generation, state (`pending`,
`applied`, `failed`, or `unsupported`), a stable detail code, and update
timestamp. Free-form external error bodies are not stored. Tracking inventory
generation lets a selector publish a newly discovered winner without creating
a fictional policy revision.

### Binding-scoped learning

Add `learned_models_by_binding`, keyed by `(binding_id, model_id)`, with the
same days, maximum prompt, and context declaration facts as the existing
`models` table. New observations and promotions write only this table.

Keep the existing `models` table as the revision-one legacy learning source.
This preserves historical promotions whose provider attribution cannot be
reconstructed. At startup, replay attributable ledger observations into the
binding-scoped table without copying unassignable grants. Generated revision
one upgrade policies explicitly consult the legacy table as well; any policy
created or edited through the routing surface does not.

### Lane pins

Extend `lanes` with nullable:

- policy revision id;
- route id and route fingerprint;
- candidate id;
- binding id;
- baseline model;
- effective model.

Old rows remain unpinned and acquire a pin only after the active policy serves
a response. A pin moves on a permitted activation transition or successful
fallback, never merely because catalogue or health state changed.

### Ledger provenance

Extend `requests` with nullable:

- policy revision id;
- presentation id;
- rewrite id;
- policy route id;
- route fingerprint;
- candidate id;
- baseline model;
- attempt group id and attempt index;
- fallback reason;
- routing trace JSON.

Every upstream attempt gets its own ledger row and cost semantics. The trace
contains only ids, classifications, availability states, and reason codes.
Preflight skips ride on the first attempted row or the local terminal row. The
existing requested and effective model columns retain their current meanings.
When every candidate is unavailable before send, write one proxy-kind
`route-unavailable` row carrying the trace; absence of an upstream attempt does
not make the routing decision disappear from the ledger.

## Compiled runtime

`src/policy/compile.rs` converts a document plus route topology into a
`CompiledPolicy`. Compilation:

- validates ids, references, scopes, priorities, templates, and parser kinds;
- compiles glob and regex matchers;
- verifies every binding exists in the static route topology, even when its
  provider is presently unconfigured;
- validates billed-boundary consent from provider cost classes;
- compiles presentation and candidate selectors for later inventory snapshots;
- materialises the current selector winners for diagnostics without freezing
  them into the revision;
- builds exact-presentation and ordered-rewrite indexes;
- computes route fingerprints from semantic route fields;
- emits structured diagnostics with code, severity, record id, and JSON path.

Compilation does not require every provider or offer to be currently
available. Missing live state becomes preview evidence, not a malformed
policy.

`PolicyRuntime` lives on `Server` and owns:

- the active compiled policy;
- the current immutable offer inventory and its generation;
- a revision cache for pinned lanes;
- the mutation lock;
- generation counters for catalogues and credential availability;
- in-memory health circuits.

A request captures the compiled policy and offer inventory together as a
`PolicySnapshot`. Catalogue refresh atomically replaces the inventory `Arc`.
Selectors are materialised from the captured inventory, so new offers affect
new requests and publication without mutating the policy revision. Lane pins
still hold their exact selected model.

The provider `RouteRegistry` becomes topology and provider lookup only. It
exposes stable binding ids, adapters, capabilities, cost classes, and enabled
provider objects. It no longer interprets frontend model strings once policy
routing is active.

## Request resolution and execution

### Request facts

After parsing to canonical IR, derive a content-free `RequestFacts` containing
frontend profile and protocol, requested model, session/lane identity, prompt
bound, message/tool counts, required capability flags, and opaque provider
affinity. Encrypted or provider-owned reasoning carries an affinity to the
binding that can replay it; policy routing cannot erase that constraint.

Model-bearing non-inference paths use the same policy vocabulary with narrower
execution rules. `count_tokens` resolves the candidate whose tokenizer is
being asked and remains unledgered. A batch resolves every embedded model and
is accepted only when all entries select one batch-capable binding; it has no
cross-binding post-send fallback. An unmatched model-free Anthropic path uses
the active protocol default's binding and retains transparent forwarding.
These paths do not retain a legacy model-map or bootstrap-default escape hatch.

### Resolution

`policy::resolve` takes a compiled revision, current offers, request facts,
lane pin, learned state, and availability snapshot. It returns either a local
typed failure or an `AttemptPlan` with:

- selected presentation or rewrite provenance;
- ordered candidates and traversed edges;
- baseline and upgraded model for each viable candidate;
- availability evidence and preflight skips;
- lane transition decision;
- route fingerprint and policy revision.

The resolver is pure. It neither mutates a body nor sends a request. Preview
uses the same entry point with invented content-free request facts.

### Shared attempt coordinator

Add `src/server/inference/` as the only coordinator for Messages, Chat, and
Responses inference. Frontend modules remain responsible for parsing and
rendering their wire. Backend adapter modules remain responsible for provider
wire rendering, authentication, observation, and canonical response events.

The coordinator:

1. captures a policy revision;
2. obtains an attempt plan;
3. renders a fresh provider request from canonical IR for each attempt;
4. records `note_served` against the attempted binding before sending;
5. retains the response until it is accepted or classified for fallback;
6. records each completed or failed attempt against its actual backend;
7. pins the successful candidate and effective model;
8. hands the accepted canonical response to the frontend renderer.

An attempt never mutates the canonical request used to render a later attempt.
Provider-specific semantic changes are applied to a fresh render. This keeps a
failed candidate's mapping, compaction target, auth, and dialect extensions out
of the fallback candidate.

### Error classification

Move safe provider error classification behind backend adapters. The common
classes are authentication, model-not-found, rate-limit, quota, overload,
server-error, pre-accept transport, ambiguous transport, and unclassified.
Classification may inspect provider response data transiently, but persistence
keeps only the existing safe fields and stable class/reason codes.

The coordinator consults the outgoing edge after classification. A condition
not explicitly listed ends on that candidate. A response already committed to
the frontend always ends on that candidate.

## Publication

Refactor `catalog::offers` into an `OfferInventory` containing one canonical
offer per provider binding and backend model id. Bare aliases and provider
prefixes stop being inventory entries; the policy compiler creates presented
slugs from canonical offers.

`GET /v1/models` captures the active policy and expands presentations for the
request's exact frontend profile with protocol inheritance. Chat and Responses
renderers receive safe `PresentedModel` values rather than raw offers.
Responses metadata adapters remain explicit per frontend dialect.

Refactor `picker.rs` into a shared publication layer:

- `publication::claude` renders and reconciles toker-owned Claude rows;
- `publication::models` builds dynamic endpoint entries;
- `publication::status` updates desired/applied adapter state;
- legacy picker rules remain only as input to revision-one compilation.

The periodic picker command and the routing TUI call the same Claude
reconciler. They reconcile both policy revision and inventory generation, so a
selector can publish a newer discovered offer under the same policy revision.
A failed settings write leaves the desired state pending or failed and does
not roll back server activation.

## Control API

Add gated endpoints with operation-specific `x-toker-control` values:

- `GET /_toker/routing/snapshot` returns offers, active revision, draft,
  diagnostics, publication state, and lane-impact counts;
- `PUT /_toker/routing/draft` replaces the draft with an expected generation;
- `PATCH /_toker/routing/draft` applies typed edit operations with an expected
  generation;
- `POST /_toker/routing/validate` compiles the current draft;
- `POST /_toker/routing/preview` resolves invented request facts;
- `POST /_toker/routing/activate` activates the draft with scope and optional
  forced cache transition;
- `GET /_toker/routing/revisions` lists immutable revision summaries;
- `POST /_toker/routing/reactivate` copies a previous document into a new
  active revision;
- `POST /_toker/routing/promote` promotes a model for one binding;
- `POST /_toker/routing/publication` records a user-side adapter result for an
  expected desired revision and inventory generation.

All JSON request types use `deny_unknown_fields`. Apply an explicit policy-body
limit independent of the small promotion endpoint's limit. Mutation replies
return the new generation or revision and enough canonical state for the TUI
to replace its local copy without a second write.

Validation and preview return stable diagnostic codes and paths. They do not
return credentials, credential fingerprints, provider error bodies, prompts,
tool arguments, or response content.

## Routing TUI

Split the TUI's state into a top-level `Page` and page-owned state. Keep the
dashboard modules focused on the operational page, and add:

- `src/tui/routing/client.rs` for gated control requests;
- `src/tui/routing/model.rs` for draft, selection, and editor state;
- `src/tui/routing/view.rs` for the routing page and hit map;
- `src/tui/routing/form.rs` for typed modal editors.

`toker tui` receives the configured loopback endpoint alongside the ledger
path. The routing client runs on a small background worker with bounded request
and reply channels, so a slow or restarting daemon never blocks terminal event
handling. Dashboard reads continue directly against SQLite.

The header action strip renders the existing inverse-video `?` followed by a
blue inverse-video `⇄` routing action. `Tab` or clicking `⇄` toggles the
dashboard and routing pages; `?` continues to toggle the legend. The legend is
an overlay for the current page and includes routing-page keys while that page
is active.

The routing page is a keyboard-first master/detail interface with four views:

- presentations;
- routes and candidates;
- rewrites, families, and upgrades;
- preview, revisions, and activation.

The common navigation is arrows or `j`/`k`, `Enter` to inspect/edit, `n` to
add, `d` to request removal, and `Esc` to close a form or return to the parent
view. Forms own text editing, enum choice, matcher kind, offer completion, and
ordered-list movement. Removal, billed-boundary consent, forced warm-lane
movement, activation, and reactivation use explicit confirmation states.

The page keeps the draft generation it loaded. A conflict preserves local
edits, shows the server revision beside them, and offers reload or a field-level
reapply through typed patch operations. Switching pages never drops a draft or
an open validation result.

Preview accepts only model, protocol/profile, capability toggles, counts, and
prompt bound. Its trace shows presentation/rewrite match, pinned or selected
candidate, skipped candidates, upgrade choice, cost transition, and final
binding. It has no prompt editor.

Activation shows changed presentations and routes, affected lane counts by
warm/cold state, cost-boundary changes, desired publication adapters, and the
chosen activation scope before confirmation.

## Legacy migration and steady state

At server startup, an absent active revision triggers one legacy compilation
and activation transaction. The compiler consumes:

- protocol defaults from `Config`;
- every provider model map;
- configured or built-in OpenRouter picker rules;
- the force-newest gate;
- the existing family behavior as explicit generated classifiers.

The generated document reproduces provider-prefix routing, protocol aliases,
model-map precedence, picker variants and `behavesAs`, force-newest election,
and static defaults. It records `source = legacy` and enables legacy learning
only on the generated upgrade policies.

Once an active revision exists, later TOML changes do not alter it. Setup reads
the policy state and reports that routing is database-owned. The routing TUI
offers `Import legacy config into draft`, which recompiles the current file
into a draft and requires normal validation and activation.

Keep legacy config fields readable and round-trippable while old files exist,
but stop emitting model maps, picker rules, defaults, and force-newest into new
setup-generated configuration after policy ownership begins. Provider enable,
endpoint, credential, and listener fields remain in TOML.

For a fresh setup, compile and activate the initial policy from the wizard's
provider and model choices before omitting those operational fields from the
written config. For an existing active policy, setup never replaces it; its
only write path is the explicit import-to-draft action.

After every inference path uses compiled policy:

- remove runtime model-map application from providers and handlers;
- remove string-prefix/default interpretation from `RouteRegistry`;
- replace hard-coded `family_of` and `newer_than` calls with classifier output;
- reduce `force_newest` to the generic cache-safe transition logic and move it
  under the upgrade module;
- remove the old picker rendering path in favour of publication adapters;
- retain only legacy parsing and import conversion code needed for existing
  configurations.

## Implementation changesets

Each changeset leaves one authoritative path for the behavior it introduces.
Later changesets remove compatibility code as soon as its last runtime caller
moves.

### 1. Policy vocabulary and canonical documents

Add `src/policy/document.rs`, `matcher.rs`, `version.rs`, and shared ids,
frontend scopes, binding ids, capability flags, candidate edges, and
diagnostics. Add canonical JSON rendering and semantic fingerprints. Make
protocol, dialect, and adapter identities serialisable with their existing
stable spellings.

### 2. Policy persistence and binding-scoped state

Add the policy, publication, binding-learning, lane-pin, and ledger-provenance
schema. Add transactional store methods for draft compare-and-swap,
activation, revision loading, publication status, and binding-scoped learning.
Keep policy writes out of the insert-only ledger path.

### 3. Route topology and canonical offer inventory

Give every implemented provider binding a stable id, cost class, capability
set, and configured-state lookup. Refactor `RouteRegistry` into topology and
provider lookup. Refactor `catalog::offers` to emit canonical backend offers
without frontend aliases.

### 4. Matcher, classifier, selector, and compiler

Compile policy documents into indexed matchers, structured versions, expanded
presentations, candidate chains, and route fingerprints. Produce structured
diagnostics for every invalid reference or unsafe transition. Keep compilation
independent of live provider availability.

### 5. Legacy compiler and revision-one startup

Translate the loaded config and legacy family behavior into a complete policy
document. On first policy-aware startup, persist and activate it before the
listener begins serving inference. Load later startups strictly from the
active database revision.

### 6. Policy runtime and revision cache

Add `PolicyRuntime` to `Server`, including active-policy capture, old-revision
loading for pinned lanes, mutation serialization, inventory generations, and
health-circuit state. Make catalogue refresh replace the immutable offer
inventory and advance its generation without rebuilding or mutating an
in-flight request's policy snapshot.

### 7. Pure resolver and preview trace

Implement request facts, frontend inheritance, exact presentation lookup,
rewrite/default resolution, lane-pin selection, candidate preflight,
binding-local upgrade selection, and content-free traces. Use the same pure
resolver for live requests and invented previews.

### 8. Binding-scoped learning and promotion

Move observations, recently-served state, family elections, context fit, and
promotion onto binding ids and compiled classifiers. Preserve the legacy table
only through revision-one upgrade policies. Change CLI promotion to accept a
binding when the model is ambiguous and route the existing handover through
the binding-aware control operation.

### 9. Shared inference coordinator

Introduce the canonical attempt coordinator and backend attempt interface.
Move Messages, Chat, and Responses inference onto it one frontend at a time,
removing each handler's direct registry resolution when it moves. Keep
administrative and control paths in their protocol modules, but move
`count_tokens`, batch model projection, and unmatched default binding lookup to
their restricted policy-resolution paths in the same changeset.

### 10. Lane pins and activation scopes

Persist successful route, candidate, binding, baseline, effective model,
fingerprint, and revision on the lane. Apply `new lanes`, `new and cold lanes`,
and `immediate` transitions through one decision function. Feed affected-lane
summaries to activation preview and require the forced flag for cache-losing
immediate moves.

### 11. Preflight availability and cost edges

Add dynamic provider credential state, verified binding checks, capability
requirements, fresh-catalogue absence, disabled candidates, and policy-driven
health circuits. Walk route edges for skips and enforce billed-boundary consent
before choosing the next candidate.

### 12. Post-send fallback and attempt accounting

Add provider error classifiers, delayed response commitment, edge-conditioned
retry, and fresh rendering from canonical IR. Record one ledger row per
upstream attempt with shared attempt-group provenance and pin a successful
fallback target for later lane turns.

### 13. Active-policy model publication

Render Chat and Responses model lists from active presentations and current
inventory. Add safe virtual metadata and unavailable-inventory distinctions.
Move Claude settings reconciliation into the shared publication adapter and
persist desired/applied revision state.

### 14. Routing control surface

Add snapshot, draft replace/patch, validation, preview, activation, revision,
reactivation, and binding-aware promotion endpoints. Apply control gates,
content-type checks, body limits, optimistic generations, and structured
diagnostics consistently.

### 15. TUI page framework and read-only routing views

Add the page enum, routing header action, control client, routing state, and
the four master/detail views. Preserve the dashboard's cadence and overlay
behavior while routing data refreshes on its own generation.

### 16. TUI editing, preview, and activation

Add typed forms, selector completion, candidate ordering, confirmations,
conflict recovery, preview traces, revision browsing, activation scopes,
reactivation, promotion, and publication status. Invoke Claude reconciliation
after successful activation without coupling its failure to server policy.

### 17. Setup ownership and explicit legacy import

Teach setup to detect active database ownership, stop generating new
operational routing fields, and retain bootstrap provider configuration. Add
the TUI legacy-import action and update the periodic picker unit to reconcile
the active presentation policy.

### 18. Remove superseded runtime machinery and document internals

Remove the old model-map, default/prefix, hard-coded family, force-newest, and
picker runtime paths after their callers have moved. Document the final policy
lifecycle, resolver, fallback commitment point, lane pins, publication
adapters, control operations, and migration behavior under `docs/internals/`.

Re-read this plan and the design plan against the resulting implementation.
When every promised implementation item is present or has been consciously
moved into another recorded plan, remove this file in its own `unplan:` commit.

The reconciliation must include the auxiliary bare `claude-*` scenario from
[`bare-claude-without-anthropic.md`](bare-claude-without-anthropic.md): with no
Anthropic binding configured, Claude Code's classifier, title, helper, and
compaction calls route by model policy without inspecting their content or
shape.
