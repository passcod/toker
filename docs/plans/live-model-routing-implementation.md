# Live model routing implementation

Status: planned; implementation has not started.

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
silently borrow the policy of a named frontend. Defaults use the same scope and
precedence: an exact profile default wins before the protocol default.

### Stable binding identity

A binding is a closed static-topology identity with the stable spelling
`<provider-id>.<dialect-id>`, for example
`openrouter.openrouter_messages` and `codex_sub.codex_responses`. It is not the
provider id and not a frontend-to-binding route. Policy, inventory, learning,
health, pin, and ledger references all use this identity.

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
captures. Transparent requested-model rewrites use only these matcher kinds;
binding-local family classifiers run after candidate selection.

Routes declare their parameters. `requested_model` is a reserved explicit
parameter. Templates use `${name}` placeholders and may reference only declared
route parameters. Every presentation, rewrite, and default route reference has
an argument-binding map with one entry for every declared parameter. Each entry
selects exactly one typed source: `Literal(String)`, `RequestedModel`,
`RegexCaptureName(String)`, or `RegexCaptureIndex(u32)`. Capture sources are
accepted only on a rewrite reference whose matcher is a regular expression;
presentations and defaults cannot bind captures. Compilation rejects missing,
duplicate, or undeclared arguments, and capture bindings whose matcher does not
provide the named or numbered capture.

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
explicit classifiers generated from the legacy behavior. Its generated
classifier may declare `legacy_hyphenated = true`, a revision-one-only strategy
that preserves the old split where numeric version segments can precede the
family name (for example `claude-3-7-sonnet`) and published snapshot aliases
fold before classification. Authored policies cannot select this strategy.

### Availability and cost

Availability is `available`, `unavailable`, or `unknown`, with a list of
content-free evidence. Only `unavailable` skips a candidate. An absent or stale
catalogue is `unknown`, so it cannot remove a candidate.

Provider bindings declare the `subscription`, `billed`, or `unknown` cost class
specified by the design's
[cost-boundary matrix](live-model-routing-policy.md#availability-and-fallback).
A route's fallback edge, not merely its destination candidate, grants consent.
The compiler applies that matrix to preflight skips and post-send fallback and
rejects every edge that requires consent but does not grant it.

Health circuits are policy data. Each candidate may map a classified failure
to a circuit duration. No implicit duration is guessed. A circuit key contains
the captured policy revision, route and candidate, binding, concrete model, and
failure class. A matching successful attempt, policy activation, or relevant
credential/catalogue generation change clears the affected circuit.
Request-specific validation failures never open shared circuits.

Credential availability is evaluated for each request and attempt as passing
native, held, refreshable, known missing or expired, or unknown until attempt.
Traces retain only that state. Every attempt builds a fresh safe header map from
named frontend inputs before the destination provider strips foreign credentials
and injects its own; no attempt reuses another candidate's outgoing headers.

### Response commitment

Fallback is possible only before the frontend response head is committed.
For a streaming attempt, the backend adapter retains the upstream head and no
more than the fixed precommit byte cap while classifying a complete immediate
provider error or the first canonical event. It commits on an accepted canonical
event, any other valid first frame, or reaching the cap. SSE comments and
keepalives do not commit unless retaining them reaches the cap. A client
disconnect cancels the attempt rather than falling back. Once the frontend head
or any body bytes are emitted, the attempt is committed and no later failure
can select another candidate.

`ambiguous_transport` is distinct from a transport failure proven to have
happened before the request was accepted. It is never enabled by migration and
requires an explicit fallback condition because the first provider may have
accepted and billed the request.

### Model identity and response projection

Carry requested, baseline, effective, and reported model identities as separate
typed values. Backend adapters observe and record the provider's reported model
before frontend projection. A missing response model remains absent rather
than being filled from the effective target for display purposes.

Frontend renderers receive the exact requested identity in addition to the
accepted canonical response and use it for every response model echo. They do
not expose the effective target merely because a rewrite, virtual presentation,
upgrade, or fallback selected a differently named backend model.
Recently-sent cache evidence uses the effective target and is marked before the
send. Learned days, prompt ceilings, pricing identity, and context evidence
require a completed provider-reported identity. A missing reported identity is
never synthesized from the effective target for learning or display.

### Session lineage

Treat the incoming session id as the raw lane and prompt-cache identity. Add an
independent, nullable display parent, child role, and relation source derived
only from explicit content-free wire evidence or a verified structural
convention. Grouping a child for display and aggregate accounting must not
merge its lane pin, allowance, prompt-cache key, or cache state with its parent.

Recognise the `guardian:<session-id>` convention only on the known `codex`
Responses profile and only from its configured raw-session header. The suffix
must be nonempty. Retain the complete raw id, use the suffix as the display
parent, and reject malformed, self-referential, or cyclic relations. Retain a
valid relation when the parent has no ledger row so later display resolution can
find it. Claude action classifier calls already use the main session id. Do not
attach Claude side queries that use fresh ids without a parent header. Named
request-class headers may classify a request role when present, but timing,
model names, request counts, transcript absence, connection reuse, and body
shape are never lineage evidence.

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

- scoped defaults;
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
- `Virtual` names an existing route and supplies its argument-binding map.

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

A route contains declared parameters, ordered candidates, and an edge between
each adjacent pair. A route also has a display name and its lane transition
policy. A candidate has:

- stable candidate id;
- binding id;
- exact model, requested-model parameter, parameter template, or single-winner
  offer selector;
- optional upgrade-policy id;
- optional binding-local alias-policy id, applied after upgrading;
- hard capability requirements whose loss would invalidate the route;
- enabled state.

Presentation selectors may retain several offers; a route candidate selector
must select exactly one winner by its declared order. A stale inventory with a
previous winner supplies unknown availability; stale or absent inventory with
no winner is unresolved and cannot be skipped as positively unavailable. Only
a fresh catalogue's positive miss is unavailable.

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

A default is a route reference with a profile or protocol scope and an argument
binding map. The exact profile default wins before the protocol default. It is
the final compatibility route for an unmatched bare name and may bind
`requested_model` from the request, but it cannot bind regular-expression
captures.

A rewrite rule contains frontend scope, unique priority, model matcher, and a
route reference with an argument-binding map. A regular-expression rewrite may
bind route arguments from its named or numbered captures. Exact presented slugs
are compiled into a separate lookup and always precede this ordered rule set.

## Stored representation

Add the following in the next append-only schema migration.

### `routing_policy_revisions`

One immutable row per activation:

- `revision_id INTEGER PRIMARY KEY AUTOINCREMENT`;
- `created_ms INTEGER NOT NULL`;
- `activated_ms INTEGER NOT NULL`;
- `description TEXT`;
- `activation_scope TEXT NOT NULL` (`new_lanes`, `new_and_cold_lanes`, or
  `immediate`);
- `forced_cache_transition INTEGER NOT NULL` (`0` or `1`);
- `source TEXT NOT NULL` (`legacy`, `draft`, or `reactivation`);
- `parent_revision_id INTEGER`, referring to an existing immutable revision;
- `document_json TEXT NOT NULL`;
- `fingerprint TEXT NOT NULL`, exactly 64 lowercase hexadecimal characters.

Add indexes on `activated_ms`, `parent_revision_id`, and `fingerprint`.
The fingerprint is an indexed content identity, not a revision identity: a
reactivation inserts a distinct child revision carrying the same canonical
policy and fingerprint.
Revisions are inserted
only by the activation transaction; no update or delete Store API exists. The
canonical document's own format version remains inside `document_json` and is
validated before insertion and after loading.

The document is stored as canonical JSON because it is loaded, validated, and
activated as one unit. Child policy records are not independently mutable, so
normalising them into editable SQL rows would introduce partial-policy states
without providing a query path the service needs.

Canonical policy JSON rejects duplicate keys, unknown fields, non-integer
numeric policy values, invalid UTF-8, and values above explicit document,
collection, string, matcher, and template limits. Object keys sort
lexicographically, semantically ordered arrays retain their order, and output is
compact UTF-8. A fingerprint is the full SHA-256 over a versioned domain tag and
those bytes. A route fingerprint covers its transitive semantic dependencies
but excludes display text and current inventory winners.

### `routing_policy_state`

A singleton row uses `id INTEGER PRIMARY KEY CHECK (id = 1)` and contains:

- `active_revision_id INTEGER`, nullable until the atomic cutover;
- `draft_generation INTEGER NOT NULL DEFAULT 0`;
- `draft_base_revision_id INTEGER`;
- `draft_document_json TEXT`;
- `draft_fingerprint TEXT`;
- `draft_updated_ms INTEGER`;
- `legacy_imported_ms INTEGER`.

The migration inserts row `id = 1` with no active revision and no draft. Its
presence is not an ownership marker: database ownership begins only when
`active_revision_id` is non-null. This storage changeset may create and exercise
drafts and revisions, but it MUST leave that field null and MUST NOT make normal
startup read policy from SQLite.

A draft is absent only when all four draft fields are null. Otherwise base
revision, canonical JSON, fingerprint, and update timestamp are all non-null and
validated together. A compare-and-swap draft write updates the document,
fingerprint, timestamp, and `draft_generation = draft_generation + 1` only when
the caller's expected generation matches; zero changed rows is a typed conflict.

Activation uses one `IMMEDIATE` transaction to re-read the expected draft
generation, load and validate the complete draft, insert the immutable revision,
update the active pointer and publication desired rows, retain the activated
canonical document as the next draft, set its base to the new revision, and
advance the generation. Reactivation performs the same transaction after copying
an old document into a new revision; the active pointer never moves backwards.
The transaction is specified here for the later cutover but is not called in
this non-authoritative storage changeset.

### `routing_publications`

One row per frontend scope and adapter is keyed by
`(scope_key TEXT, adapter TEXT)`. `scope_key` is the canonical string
`profile:<profile>:<protocol>` or `protocol:<protocol>`; no nullable key column
participates in identity. Each row contains:

- `desired_revision_id INTEGER NOT NULL`;
- `applied_revision_id INTEGER`;
- `desired_inventory_generation INTEGER NOT NULL`;
- `applied_inventory_generation INTEGER`;
- `state TEXT NOT NULL` (`pending`, `applied`, `failed`, or `unsupported`);
- `detail_code TEXT` containing a stable identifier only;
- `updated_ms INTEGER NOT NULL`.

A result update is compare-and-swap on scope, adapter, desired revision, and
desired inventory generation, so a late reconciler cannot mark newer desired
state applied. `applied` requires both applied fields to equal the desired pair;
`pending`, `failed`, and `unsupported` may retain the last applied pair.
Free-form external errors are never stored. Add indexes on desired revision and
state.
Tracking inventory generation lets a selector publish a newly discovered winner
without creating a fictional policy revision.

### Binding-scoped learning

Add `learned_models_by_binding`, keyed by `(binding_id TEXT, model_id TEXT)`,
with:

- `days_json TEXT` containing a canonical sorted set of local-day strings;
- `max_prompt INTEGER`;
- `context_window_json TEXT`;
- `explicitly_promoted INTEGER NOT NULL DEFAULT 0`;
- `updated_ms INTEGER NOT NULL`.

A merge unions days, takes the maximum prompt, replaces a context declaration
only with caller-supplied verified evidence, ORs explicit promotion, and
advances `updated_ms`. Absence remains null rather than zero. Add an index on
`model_id`
for the CLI's ambiguity check. New observations and promotions write only this
table once the binding-aware writer changeset lands.

Keep the existing `models` table as the revision-one legacy learning source.
This preserves historical promotions whose provider attribution cannot be
reconstructed. Replay is a separate idempotent Store operation, not part of the
schema migration: rows with an attributable ledger `provider` and route binding
are merged into the binding table, while unassignable grants are not copied.
Generated revision-one upgrade policies explicitly consult the legacy table as
well; authored policies never do.

### Lane pins

Extend `lanes` with:

- `pin_generation INTEGER NOT NULL DEFAULT 0`;
- nullable `policy_revision_id`, `policy_route_id`, `route_fingerprint`,
  `candidate_id`, `binding_id`, `baseline_model`, and `effective_model`.

A lane is unpinned only when all seven pin fields are null. A pin is valid only
when all seven are non-null, the fingerprint is exact lowercase SHA-256, and its
revision exists. Add indexes on policy revision and binding. Ordinary lane
upserts preserve every pin field and generation; they never copy null pin values
over a concurrent pin.

`compare_and_swap_lane_pin` receives the expected generation and expected prior
pin identity. It updates all pin fields atomically and increments the generation
only when both expectations match. First-pin uses generation zero plus all-null
prior fields, so one concurrent completion wins. Activation/fallback uses the
loaded generation and complete prior identity, so a stale old-revision
completion cannot restore an old pin. A zero-row update is a typed conflict, not
an implicit retry.

Old rows remain unpinned in this migration. Seeding or conservative preservation
is an explicit cutover transaction after equivalence review, not migration
backfill; missing evidence never makes a warm lane new.

### Ledger provenance

Extend `requests` with nullable `policy_revision_id`, `presentation_id`,
`rewrite_id`, `policy_route_id`, `route_fingerprint`, `candidate_id`,
`baseline_model`, `attempt_group_id`, `attempt_index`, `fallback_reason`, and
`routing_trace`, plus nullable content-free `parent_session_id`, `child_role`,
and `relation_source`.

`routing_trace` is a JSON-text column and is added to `JSON_TEXT_COLUMNS`; every
trace is validated as content-free typed provenance before insertion. Add
indexes on policy revision, attempt group plus index, and parent session. The
migration does not backfill old rows: null means the row predates or did not use
policy routing, never revision zero or attempt zero.

The existing session id remains the raw wire identity used by lanes and prompt
caches. Parent aggregation is a read-model concern and never rewrites it.
Lineage fields are either all null or all non-null. The store accepts only the
verified relation source/role pairs defined by the frontend parser, rejects a
parent equal to the raw child id, and walks existing parent links under the
store's serialized connection to reject cycles before insertion. A missing
parent row is allowed. Ledger import preserves already validated fields but does
not invent lineage from historical shape or timing.

Every upstream attempt gets its own ledger row and cost semantics. Attempt
groups use a lowercase 32-hex random identifier generated once per logical
request. Actual sends use contiguous zero-based indices; `attempt_index` is null
only for a preflight-only terminal row, which still has a group id. A partial
unique index on `(attempt_group_id, attempt_index)` where the index is non-null
prevents duplicate send identities.
Schema v7 had no normal writer for these attempt fields; if a raw v7 fixture
contains malformed non-null attempt provenance, the hardening migration aborts
atomically for repair rather than mutating insert-only history.

The trace is a versioned JSON object (`version = 1`) containing at most 128
events and 64 KiB of canonical encoded JSON. Model identities within events are
at most 1,024 bytes. It contains only ids, classifications, availability states,
and closed reason codes; unknown versions, event variants, states, and reasons
are rejected on both insert and read. Preflight skips ride on the first
attempted row or local terminal row.
Logical-request views group rows with a non-null `attempt_group_id`; rows
without one remain independent legacy logical requests. Request counts count
groups plus
legacy rows, token/cost/meter totals sum actual attempt rows, and the group's
final client outcome comes from its greatest attempt index or preflight terminal
row. Quota fitting and model learning consume only the provider evidence on each
actual attempt, never a group-level synthetic total. The same-candidate
`thinking: disabled` negotiation retry is an attempt in the same group. The
existing requested and effective model columns retain their current meanings.
`raw_model` remains the provider-reported wire identity. Operational views
labelled `reported` read that field and show absence as `-`; the normalised
`model` fallback remains available only to internal served-model and learning
paths.
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

Session/lane identity here is always the raw request identity. Session lineage
is carried beside it for recording and display and cannot affect resolution or
lane selection.

Model-bearing non-inference paths use the same policy vocabulary with narrower
execution rules. `count_tokens` resolves only the first policy candidate whose
tokenizer is being asked, sends no fallback attempt, and remains unledgered. A
batch resolves every embedded model only to its first candidate and is accepted
only when all entries select one batch-capable binding; it has no cross-binding
fallback. An unmatched model-free Anthropic path resolves the exact-profile
default before the protocol default and uses only that route's first binding
while retaining transparent forwarding. These paths do not retain a legacy
model-map or bootstrap-default escape hatch.

### Gate and transformation order

Resolution chooses the initial or pinned candidate before the existing gates.
Local quota exhaustion remains a gate and cannot make a subscription candidate
fall through to billed service. The cold decision is based on the selected
cache identity. A cold compaction may then select the policy's explicit
compaction route and apply the separately licensed cache/system transforms.
Upgrade and candidate-local aliasing produce the effective first attempt.
Fallback does not rerun a gate notice.

The existing `thinking: disabled` negotiation retry runs once on the same
candidate before route fallback. It saves meter evidence, records its own
attempt, and the expected `between_tools` refusal does not open a health
circuit. If that retry also fails, its result enters ordinary failure
classification.

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
8. records the accepted provider-reported model before projection;
9. hands the accepted canonical response and original requested identity to
   the frontend renderer.

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

Extend the dashboard read model to preserve each row's raw session while
grouping rows with explicit lineage under their display parent. Parent totals
include linked child accounting, and session detail shows a child-role
breakdown; lane and cache panels continue to render the child lanes separately.
Resolve a linked child's title and cwd through its parent transcript while
still displaying its raw id and role in detail. Unlinked UUID sessions remain
separate rather than being guessed into the nearest active conversation.

The model detail line keeps `asked`, `sent`, and `reported`: requested identity,
effective target, and `raw_model`, respectively. A response that did not name a
model displays `reported -`.

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

Before policy persistence or authority lands, startup compiles the legacy
configuration in memory and runs the final pure resolver in shadow mode. The
legacy path remains authoritative for bodies, providers, credentials, gates,
responses, lanes, and recording. Shadow every inference path and every
administrative model path that will switch at cutover, including `count_tokens`,
batches, unmatched or default administrative binding lookup, dynamic model
publication, and Claude picker projection. Apply the restricted administrative
semantics for first-candidate selection, cross-binding fallback, and scoped
defaults when comparing those paths. Shadow comparison stores nothing and may
emit only content-free stable ids, model identities, and reason codes. It cannot
render or send an alternate request, and any compilation, comparison, or
observer failure is swallowed without changing the client result. Unexplained
mismatches across defaults, prefixes, aliases, model maps, publication,
force-newest, and compaction block cutover.

At the first all-policy runtime cutover, an absent active revision triggers one
legacy compilation and activation transaction. The ownership marker is written
only in that transaction, never by an earlier schema-aware binary. A corrupt or
unsupported active policy fails startup rather than reverting to TOML. The
compiler consumes:

- protocol defaults from `Config`;
- configured or built-in OpenRouter picker rules;
- the force-newest gate;
- `compact_model` as a generated compaction route;
- provider model maps as generated candidate-local alias policies;
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
frontend scopes, binding ids, capability flags, candidate edges, alias policies,
compaction policy, and diagnostics. Add bounded canonical JSON rendering and
semantic fingerprints. Make protocol, dialect, and adapter identities
serialisable with their existing stable spellings.

### 2. Route topology and canonical offer inventory

Give every implemented provider binding a stable id, cost class, capability
set, and configured-state lookup. Refactor `RouteRegistry` into topology and
provider lookup. Refactor `catalog::offers` to emit canonical backend offers
without frontend aliases.

### 3. Matcher, classifier, selector, and compiler

Compile policy documents into indexed matchers, structured versions, expanded
presentations, candidate chains, alias policies, compaction routes, and route
fingerprints. Produce structured diagnostics for every invalid reference or
unsafe transition. Keep compilation independent of live provider availability.

### 4. Legacy compiler and binding-aware read interfaces

Translate loaded config and legacy family behavior into a complete in-memory
policy document. Add the binding-keyed learning interface the final resolver
uses, backed temporarily by a read-only adapter over `ModelStore`; generated
legacy policies alone may consult unattributable legacy learning. Do not write a
policy revision or migration marker.

### 5. Pure resolver and shadow equivalence

Implement request facts, frontend inheritance, exact presentation lookup,
rewrite/default resolution, candidate preflight, binding-local upgrade and
alias selection, compaction routing, and content-free traces. Use this final
pure resolver in non-authoritative shadow comparisons for Messages, Chat,
Responses, `count_tokens`, batches, unmatched or default
administrative binding lookup, dynamic Chat and Responses model publication,
and Claude picker projection. Compare model-map, force-newest, and compaction
outcomes on every path where they apply. Administrative shadows use the
restricted execution semantics defined above: first candidate only, no
cross-binding fallback, and exact-profile default before protocol default. The
shadow may neither render nor send nor persist nor alter the legacy result.
Unexplained differences block cutover.

### 6. Policy persistence and binding-scoped state

Only after the document and resolver contracts pass shadow review, add the
policy, publication, binding-learning, lane-pin, lineage, and ledger-provenance
schema in one reviewed append-only migration, exactly as specified above. Add
round-trip and transaction tests against both a fresh database and a fixture
migrated from the immediately preceding schema version. Update every complete
`RequestRow`/`Lane` constructor, insert/select projection, import/export path,
`JSON_TEXT_COLUMNS`, and read-only Store query in the same changeset.

Add draft/revision/publication, binding-learning, and lane-pin compare-and-swap
Store APIs, but leave `routing_policy_state.active_revision_id` null. Do not
call activation, write the ownership marker, load normal runtime policy from
SQLite,
or steer traffic in this changeset. A database opened read-only exposes the new
read APIs and predictably rejects writes through SQLite's existing read-only
error path.

Move new observations, recently-sent state, family elections, context fit, and
promotion onto binding ids and compiled classifiers. Preserve the legacy table
only through revision-one policies. Change CLI promotion to require a binding
when the model is ambiguous.

### 7. Policy runtime and revision cache

Add `PolicyRuntime` to `Server`, including atomically captured policy plus offer
inventory, old-revision loading for pinned lanes, mutation serialization,
generations, and health circuits. Load and compile a candidate revision-one
snapshot in shadow mode; catalogue refresh replaces an inventory `Arc` without
mutating in-flight snapshots.

### 8. Shared inference coordinator

Introduce the canonical attempt coordinator and backend attempt interface.
Move Messages, Chat, and Responses inference onto it one frontend at a time
while retaining the legacy target decision as authority. Keep administrative
and control paths in their protocol modules. Make renderers accept the original
requested identity while observation retains effective and reported identities.

### 9. Lane pins and the common transition decision

Persist successful route, candidate, binding, baseline, effective model,
fingerprint, and revision on the lane with compare-and-swap. Seed or
conservatively preserve existing warm lanes. Apply activation, upgrade,
fallback, explicit user changes, and compaction through one cache-domain-aware
transition function; an unknown binding TTL never establishes coldness.

### 10. Preflight availability and cost edges

Add request-specific credential state, verified binding checks, hard capability
requirements, fresh-catalogue absence, disabled candidates, and keyed health
circuits. Walk every crossed edge and enforce billed/unknown-cost and warm-cache
consent before selecting the next candidate. Local quota exhaustion remains a
gate and is never candidate unavailability.

### 11. Post-send fallback and attempt accounting

Add provider error classifiers, bounded delayed commitment, edge-conditioned
retry, and fresh body/header construction from canonical inputs. Record one row
per actual attempt with group provenance, including the same-candidate thinking
negotiation retry, and pin a successful fallback target.

### 12. Atomic revision-one cutover

After shadow equivalence covers every inference and administrative model path,
activate revision one and the database ownership marker in one transaction,
atomically publish one runtime snapshot containing routing, upgrades,
presentations, dynamic Chat and Responses model lists, and desired publication
state, and switch target authority. Move `count_tokens`, batch model projection,
unmatched scoped-default binding lookup, model maps, force-newest, compaction
model selection, and server-side dynamic model publication in this cutover so
no path keeps a bootstrap-default or prior-publication escape hatch. Persist the
desired publication revision and inventory generation in the same handoff.
Update invariant 6 concurrently. A corrupt or unsupported active policy fails
startup rather than falling back.

### 13. External Claude settings reconciliation

Move Claude settings reconciliation into the shared publication adapter, use
last-applied identities for ownership and collision checks, and persist applied
revision state. Run this user-side reconciliation asynchronously after
activation; failure leaves the atomically published server state active and the
Claude adapter visibly pending or failed.

### 14. Routing control surface

Add snapshot, draft replace/patch, validation, preview, activation, revision,
reactivation, and binding-aware promotion endpoints. Apply operation-specific
control gates to reads and writes, content-type checks, body limits, optimistic
generations, and structured diagnostics; add no browser-preflight route.

### 15. TUI page framework and read-only routing views

Add the page enum, routing header action, control client, routing state, and the
four master/detail views. Preserve the dashboard's cadence and overlay behavior
while routing data refreshes independently.

Update the operational dashboard read model in this changeset too: group only
explicitly linked child sessions, resolve parent labels, retain separate child
lanes, and make `reported` use `raw_model` without an effective fallback.

### 16. TUI editing, preview, and activation

Add typed forms, selector completion, candidate ordering, confirmations,
conflict recovery, preview traces, revision browsing, activation scopes,
reactivation, promotion, and publication status. Invoke Claude reconciliation
after successful activation without coupling its failure to server policy.

### 17. Setup ownership and explicit legacy import

Teach setup to recover the specified config/database activation transaction,
detect active database ownership, stop generating new operational routing
fields, and retain bootstrap provider configuration. Add explicit
import-to-draft and update the periodic picker unit to reconcile active policy.

### 18. Remove superseded runtime machinery and document internals

Remove old model-map, default/prefix, hard-coded family, force-newest, and
picker runtime paths after their callers have moved. Document the final policy
lifecycle, resolver, fallback commitment, lane/cache domains, attempt read
models, publication ownership, control operations, lineage evidence, and
migration recovery under `docs/internals/`.

Re-read this plan and the design plan against the resulting implementation.
When every promised implementation item is present or has been consciously
moved into another recorded plan, remove this file in its own `unplan:` commit.

The reconciliation must include the auxiliary bare `claude-*` scenario: with
no Anthropic binding configured, Claude Code's classifier, title, helper, and
compaction calls route by model policy without inspecting their content or
shape. It must not invent parentage for auxiliary calls whose wire identity has
no explicit relation to an originating session.
