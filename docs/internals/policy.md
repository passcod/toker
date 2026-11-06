# The routing policy

Every inference request is routed by one thing: the active revision of the
routing policy, a versioned document in the ledger (`src/policy/`). There is no
other router. `toker.toml` names providers and credentials; which model goes to
which backend, what the pickers list, when a conversation may move to a newer
model and what happens when a backend fails are all policy. A change reaches
live traffic by activation, with no restart.

This file is the document, its lifecycle, the compiler, the resolver and the
fallback rules. What a lane pin is, and when moving one is allowed, is in
[lanes-and-cache.md](lanes-and-cache.md). Which bytes a route may alter is in
[routing.md](routing.md).

## The document

A policy is one JSON document (`PolicyDocument` in `policy/document.rs`) with
eight collections:

| Collection | What it holds |
| --- | --- |
| `defaults` | One route per frontend scope, for names nothing else matched. |
| `presentations` | Models a frontend is shown: an exact offer, an offer selector, or a virtual model backed by a route. |
| `routes` | Named, ordered chains of candidates, with the edge that grants each fall to the next. |
| `rewrites` | Matchers on the requested model that pick a route, for names that were never presented. |
| `classifiers` | Per backend binding: how to split a model id into a family and a version. |
| `upgrades` | Upgrade policies: which newer version of a candidate's family a request may move to. |
| `compaction` | Where a cold compaction goes. |
| `description` | Free text. |

A frontend scope is a protocol, or a profile plus a protocol. The profile is the
`/f/<name>` prefix the request came in on; an unprefixed request is the
`unprefixed` profile (`Server::request_facts`). A profile entry beats a protocol
entry of the same kind.

A backend binding is the pair `<provider-id>.<dialect-id>`, for example
`openrouter.openrouter_messages` or `codex_sub.codex_responses`. The set is
closed static topology (`BACKEND_BINDINGS` in `routing.rs`), distinct from a
provider and from a frontend-to-binding route. Renaming one is a migration, not
a display change.

The parser is strict on purpose. Every struct denies unknown fields, ids are
`[a-z0-9._-]` of at most 128 bytes, duplicate keys and non-integer numbers are
refused before typed parsing (`decode_policy`), and the document, depth,
collection and string sizes are bounded (`MAX_DOCUMENT_BYTES` and friends).
Because unknown fields are refused, adding even an optional field changes which
inputs are accepted, so the format carries a version
(`CURRENT_FORMAT_VERSION`, 3; 1 and 2 still load). Version 2 added the
tool-choice capability flags and version 3 the revision-one compaction target;
a document using a later feature at an earlier version is refused.

The fingerprint is the SHA-256 of a version-specific domain tag and the
canonical encoding: object keys sorted, arrays in their semantic order,
compact UTF-8 (`policy_fingerprint`). Each format version has its own domain, so
a fingerprint never means two things.

A route fingerprint (`route_fingerprint`) covers one route's semantics: its
candidates, edges, alias policies, health circuits, upgrade policies,
classifiers and lane transition policy. Labels, descriptions and the offer an
inventory happens to select do not take part, so a catalogue refresh never
changes it. It has its own domain tags, versioned independently of the
document. Lane pins carry it; see [lanes-and-cache.md](lanes-and-cache.md).

### Presentations

A presentation names a frontend slug and one of three sources:

- `exact_offer`: one model on one binding. It compiles to a generated
  single-candidate route (id hashed from the presentation, binding and model)
  with no upgrade, alias or fallback.
- `offer_selector`: include and exclude matchers over a binding's offers, an
  ordering (`catalogue_chronology`, `classified_version` or an explicit id
  list) and a keep count. The slug and label are templates over `${model_id}`,
  `${provider_id}` and `${binding_id}`. It is expanded to exact offers **when
  the policy is compiled**, so "the newest matching offer" tracks the inventory
  because every catalogue refresh recompiles the active revision (below). An
  unclassifiable id is not an older version; with `classified_version` ordering
  it is not selected at all.
- `virtual`: a route reference, with a slug that needs no provider-owned
  counterpart. The same slug may be exposed on several frontends, each with its
  own route.

Slugs are unique within a scope. One that equals a native model id (or
`provider/id`) reachable from that frontend needs `shadow_native: true`; without
it the compiler refuses, so a presented name never silently hides a real model.
Adapter metadata (`claude`'s `behaves_as`, for instance) is typed, not free
JSON.

### Routes, candidates and edges

A route has declared parameters (`requested_model` is the one reserved name),
a first candidate and steps; each step is an edge plus the candidate it leads
to. Every route reference, from a presentation, a rewrite, a default or the
compaction target, carries one value for every declared parameter: a literal,
the requested model, or a regex capture of the rewrite's matcher. Captures are
valid only on rewrites with a regex matcher. A candidate's model is `exact`, the
`requested_model` parameter, a `template` over parameters, or an
`offer_selector` that keeps exactly one winner.

A candidate also carries an optional upgrade policy, an optional alias policy
(applied last, after the upgrade), hard `required_capabilities`, an `enabled`
flag and `health_circuits` (below).

An edge says whether a fall to the next candidate is allowed before send
(`preflight`), after which classified failures it is allowed once the request
has been sent (`post_send`), and three consents: `billed_boundary`,
`ambiguous_transport` and `cache_transition`.

### Revision one, and why it still exists

Revision one is the policy generated from the legacy routing fields of
`toker.toml` (`policy/legacy.rs`): `default_backend_anthropic` and
`default_backend_openai_chat`, each provider's `model_map`, the OpenRouter
`picker` rules, `[gates] force_newest` and `compact_model`. It is
the first routing document a ledger gets. When a service starts and the ledger
has no active revision, `establish_revision_one` compiles the config, inserts it
as source `legacy` and activates it in one transaction; `toker setup` calls the
same function so the machine it verified is the machine the service would
build. An active revision is loaded as it stands and the config is not
consulted. A corrupt or uncompilable active revision fails startup rather than
falling back to the file.

Nothing at request time reads those config fields. They are read in three
places only: that first activation, setup, and the TUI's import, which compiles
the current file into a draft for the user to review.

What it generates, one thing per old behaviour:

- a `legacy-route-*` route per binding, whose one candidate takes the requested
  model, carries the binding's model map as its alias policy, and is enabled
  only when the provider is configured;
- defaults for the Messages, Chat and (when codex is enabled) Responses
  protocols, for the protocol and for the `unprefixed` profile;
- prefix rewrites (`openrouter/<id>`, `codex_sub/<id>` and so on, per provider
  and protocol) that route the part after the prefix, and an `anthropic/<id>`
  rewrite to the Messages default. Every known provider owns its prefix even
  while disabled, so a prefix naming an unconfigured backend lands on a disabled
  candidate and gets a local answer instead of falling through to a default;
- when no Anthropic default exists but some configured Messages binding has a
  model map, a `^claude-.+` rewrite at the lowest precedence to that binding,
  so Claude Code's own side calls (title, classifier, helper), which name bare
  `claude-*` ids whatever the user picked, still reach a configured backend
  through its alias policy. Policy never recognises those calls by their
  prompt;
- presentation selectors for the OpenRouter picker rules;
- per Messages and Responses binding, a `legacy-family` classifier and a
  `legacy-force-newest` upgrade policy, enabled iff `force_newest` was;
- a compaction target (`revision_one_legacy`).

Revision one needs things authored policy cannot say, and they are marked
`PolicyOrigin::GeneratedRevisionOne` at compile time: the `revision_one_model`
and `revision_one_family` matchers (the legacy model-map matching, which folds
published snapshot aliases and reads the family split in `policy/family.rs`),
the `legacy_hyphenated` classifier (the same split instead of an authored
regex, because it was measured on real traffic and an unpublished dated id has
no family), the `legacy_numeric_tuple` parser, the `revision_one_adaptive`
learning bar, `allow_revision_one_legacy_learning`, and the legacy compaction
target. The compiler refuses all of them from an authored document
(`legacy-*-forbidden`). The origin is stored on each revision
(`compiler_origin`) and inherited: an edit of revision one, and a reactivation
of it, keep compiling with the legacy semantics. A first activation also fills
the draft with the new document and records `legacy_imported_ms`, so a ledger
cannot be claimed twice.

The legacy form exists because the learned-model election, the compaction
retarget and the model maps were measured and tuned in production before the
policy existed, and reproducing them exactly was the condition for retiring the
old machinery. It also gives a fresh ledger a router without anyone having to
write one.

A revision-one policy also publishes (generated listings) the Chat and
Responses offers the old `/v1/models` projection listed, under the slugs a
presentation does not hold. Those are listed, never matched when routing: a
request naming one resolves through the rewrites and defaults, which keep the
route's upgrade and alias policy.

## Lifecycle

State is three tables (`store/schema.rs`, migration v7 on): immutable
`routing_policy_revisions`, the singleton `routing_policy_state` (active
revision, and one draft with its generation, base revision and fingerprint) and
`routing_publications`.

**Draft.** There is one draft, in the ledger, so two TUIs share it. Every write
carries the generation it read and compares-and-swaps it
(`compare_and_swap_policy_draft`); a stale write gets the current state back and
changes nothing. A draft is replaced whole (`routing-draft-replace`, through
the same strict decoder as everything else) or edited with `PolicyEdit`
(`routing-draft-edit`): a bounded, atomic list of typed operations addressing
records by id (`AddRoute`, `ReplaceRewrite`, `RemovePresentation`, and so
on). It is not JSON
Patch, so no edit can touch an incidental path, and the whole result is
validated before it is stored. Renaming is remove then add.

**Validation** (`routing-validate`) compiles the draft against one captured
inventory and returns diagnostics: a code, a severity, the record id and a JSON
pointer path built only from static field names and bounded indices, so a
request or model string can never end up in one. The compiler checks, among
other things, that every reference resolves; that route arguments match
declared parameters exactly; that regex capture sources exist; that each
candidate's binding is routable from the scope it is used in (a route from the
frontend protocol to that binding must exist); that classifier and upgrade
policy bindings agree; that rewrite priorities are unique per scope and defaults
unique per scope; that required capabilities are within the binding's declared
`Capabilities`; that shadowing a native model is consented to; and the three
edge rules below. Errors block activation. Provider configuration and catalogue
presence are evidence, not validity, so an unconfigured binding, an offer not
in the inventory or an unresolved selector are `info` findings and never block:
a fallback chain can be prepared before its provider exists. (Info findings are
produced but, as the control surface is written, not returned: only errors
reach `validate` and the snapshot.)

The edge rules (`validate_edges`): a fall from a subscription binding to a
billed or unknown-cost binding needs `billed_boundary: allow`; a `post_send`
list that includes `ambiguous_transport` needs `ambiguous_transport: allow`
(the provider may already have accepted and billed the request); and an edge
that crosses a cache identity may not say `preserve_warm`. Cost classes are on
the binding: `subscription` for `anthropic_sub` and `codex_sub`, `billed` for
`anthropic_api`, `openai_api` and `openrouter`.

**Preview** (`routing-preview`) resolves an invented request shape (protocol,
profile, model, prompt bound, message and tool counts, required capabilities,
binding affinity, and optionally a real lane key) against the draft with the
same `resolve` function inference uses, and returns the outcome and the trace.
It uses the snapshot's availability without per-request credential evidence and
without open circuits, so it shows what the policy says, not what a particular
request's headers would change.

**Activation** (`routing-activate`) takes the draft generation it was shown, a
scope and an optional consent flag. It compiles the draft, then in one immediate
transaction inserts a new revision (source `draft`, parent the revision that was
active), makes it the active revision, replaces the draft with the activated
document at the next generation, and hands every publication adapter the new
desired revision and inventory generation (`activate_policy_revision`). Only
after the commit does the runtime install the compiled policy
(`PolicyRuntime::replace_active`), so a request in flight keeps the immutable
snapshot it captured. A stale draft generation, or an active revision that
moved, fails the transaction.

The scope decides what happens to lanes already pinned to an earlier revision:

- `new_lanes`: they keep resolving under the revision they are pinned to.
- `new_and_cold_lanes`: one whose cache is provably cold resolves under the new
  revision; the rest stay.
- `immediate`: the next request resolves under the new revision. A move that
  would lose a warm cache needs the activation's `force_cache_transition`
  consent, and the activation itself is refused (`ForcedTransitionRequired`)
  while any lane is warm or not provably cold and the flag is absent. Without
  consent a lane that became warm after the check stays on its pinned revision
  (`Server::resolve_policy_attempt`).

The revisions a lane is pinned to are kept and compiled on demand against the
current evidence, in a 32-entry cache (`PolicyRuntime::capture_revision`).

**History and reactivation.** Revisions are never edited. `routing-revisions`
lists them; `routing-reactivate` copies a selected one into a **new** revision
(source `reactivation`, parent the selected one, the selected one's compiler
origin), because the active pointer must never move backwards: a lane pinned to
revision 7 must find revision 7 unchanged, and a pin to a revision newer than
the active one is a stale-revision error. Reactivation checks consent and
compilation first and only then replaces the draft with the selected document,
so a refused reactivation leaves the user's draft and its generation alone.
Restoring the generated revision one keeps its legacy semantics because the
history source (what operation made the revision) and the compiler origin (what
its document means) are separate columns (migration v15).

**Catalogue refresh.** Offer selectors and the generated listings depend on
the offer inventory, so each catalogue refresh recompiles the active revision
against the new inventory, bumps a generation for each binding whose catalogue
changed, clears that binding's open circuits, advances the publication handoff,
and swaps the runtime snapshot (`Server::replace_catalogs`). A refresh whose
compilation fails is logged and leaves the old snapshot.

**Promotion** (`routing-promote`, `toker promote`) marks a provider-reported
model explicitly eligible in the learned evidence for its binding. It does not
create a revision; see [models.md](models.md).

**Publication.** `routing_publications` has a row per frontend scope and
adapter: `dynamic_chat_models`, `dynamic_responses_models` (the server's own
`/v1/models` projections, which swap with the runtime snapshot and so start
applied) and `claude_settings` (the user-side picker reconciler, which runs as
the user because the service may write only the state dir, and so starts
pending). Each row holds the desired and applied revision and inventory
generation. An acknowledgement is a compare-and-swap against the desired
pair, so a stale one cannot overwrite a newer handoff. The generic
`routing-publication` endpoint records only `failed` or `unsupported`; an
adapter records its own `applied` result and the exact row identities it owns,
so no caller can claim a user's row. Failure to update Claude's settings never
rolls back the policy; the row stays visible and the daily timer converges it.

All control verbs (`routing-snapshot`, `-draft-replace`, `-draft-edit`,
`-validate`, `-preview`, `-activate`, `-reactivate`, `-revisions`, `-promote`,
`-publication`) are checked against `x-toker-control` before the body is read;
mutations require a JSON content type and refuse unknown fields (invariant 4).
Policy data and diagnostics hold no credentials, prompts, completions or
provider error bodies.

## The compiled policy

`compile` (`policy/compile.rs`) closes a validated document against the route
topology and one immutable offer inventory and yields a `CompiledPolicy`: routes
with their fingerprints, presentations per scope (selectors expanded), a
publication set per scope, rewrites sorted by priority, defaults, classifiers,
upgrade policies and the compaction target. It is immutable and shared. A
document never carries an inventory winner; the winner is looked up when the
policy is compiled (presentations) or when a request is resolved (a candidate
selector), so the same revision follows the catalogue.

## The resolver

`policy::resolve` is pure: it takes the compiled policy, the offer inventory, a
read-only learning snapshot, the lane facts, an availability snapshot, the open
circuits and the request facts, and returns an `AttemptPlan` or a
`ResolveFailure`. It reads no clock (`now_ms` is a fact), no store, no provider
and no body, so the control surface can run the same function the request path
does.

**Facts in.** `RequestFacts` is content-free and built by the frontend adapter
from the canonical request: profile and protocol, the requested model, the raw
lane key, a prompt-size upper bound (`force_newest::prompt_bound`), message and
tool counts, the capabilities the request needs (`required_capabilities`:
images, tools, tool-choice shapes, thinking replay and so on), a binding
affinity, the class (`Inference` or `Compaction`, from fixed markers) and the
clock. Affinity is set when the request carries provider-owned state (signed or
redacted thinking, OpenRouter `reasoning_details`, Codex encrypted reasoning):
only the binding that produced it can replay it, so only the pinned binding
qualifies, and such a request on a lane with no pin is refused.

**Lookup order**, first hit wins:

1. a presentation for the profile, then for the protocol, with that exact slug;
2. a rewrite for the profile, then for the protocol, in ascending priority
   (lower number first; equal priorities in one scope are a compile error, so
   there is no first-match accident);
3. the profile default, then the protocol default.

A compaction on a lane proven cold, under a policy with a route compaction
target, replaces the result with that route. A request with no model resolves
only through a default (`resolve_model_free`): batch reads and cancellation and
the unmatched Messages paths use it, and take the default route's first
candidate with no model, upgrade, alias or fallback, refusing a request that
does carry a model.

**The route.** Arguments are materialised from the reference. Candidates are
walked in order, starting at the lane's pinned candidate when the pin's route
fingerprint still equals the route's (recovery of an earlier candidate never
pulls a pinned lane back by itself) and at the first otherwise. For each one:

- the model is materialised. A selector with no winner is `selector_unresolved`
  and ends the walk: an absent inventory is not evidence for falling back;
- an edge crossed to reach it can deny the candidate (`preflight` disabled, the
  billed boundary without consent, or a warm cache whose loss the edge does not
  consent to); the first two end the walk, because later candidates are behind
  the same edge, and so does a warm-cache denial;
- availability is evidence-based and three-valued (`Evidence`: positive,
  negative, unknown). Positive absence skips the candidate: provider
  unconfigured or disabled, a credential known to be missing or expired
  (`credential_snapshot`; a credential that only fails when tried is unknown and
  the candidate stays), a required capability the binding cannot represent, a
  binding other than the affinity, an `enabled: false` candidate, and a model a
  **fresh** catalogue positively lacks. Only an exact-model candidate and an
  exact presentation are checked against the listing: a model from a template,
  `requested_model` or a default stays an opaque provider string, because an
  incomplete listing must not become an upstream allowlist. A stale or absent
  catalogue is unknown (`CatalogueAvailability`, aged by
  `AvailabilitySnapshot::at` from the caller's clock);
- a target with an open circuit is skipped (below);
- then, for the one that survives, the model is shaped in a fixed order: the
  **upgrade** (a newer version within the candidate's family), the revision-one
  compaction retarget, and the **alias** (the candidate's model-map rules; the
  first rule whose matcher hits and exclusions do not wins, and the replacement
  is a template over the route's parameters and captures). The plan keeps every
  stage: `baseline_model`, `upgraded_model`, `retargeted_model`,
  `aliased_model` and `effective_model`, plus `baseline_from_request`, which
  says whether the client's own spelling decided the baseline.

A route with no surviving candidate is `RouteUnavailable`, carrying the trace;
the handler answers 503 in the frontend's own error shape and records a
`route-unavailable` row. A revision with no default for the protocol is
`NoRoute` and is answered as not configured (header `x-toker-not-configured`),
because no backend was ever a candidate.

**First candidate versus traversal.** Inference resolves with traversal:
unavailable candidates are skipped and a later one may be chosen, and the plan
holds a continuation for the post-send path. `count_tokens` and batch creation
use `resolve_first_candidate`, which examines only the first candidate and never
crosses an edge, because nothing about them can be retried or pinned: the
chosen binding must support the operation (`count_tokens` and `batches` are
declared per binding) and a batch must resolve to a single binding for all its
entries. Their bodies change only the documented model positions, to the plan's
effective model.

**Upgrade policy and classifiers.** A classifier is scoped to a binding and
turns a model id into a family and a parsed version (`semver`, a release date, a
fixed-arity numeric tuple, or revision one's variable tuple). An id that does
not classify is not an older or newer version; it is outside the upgrade. An
upgrade policy picks, among the candidate's binding's models that are in the
same family and strictly newer than the baseline, the newest one that: comes
from a listed inventory source (`catalogue`, `served`, `learned`); meets the
minimum learning (none, N distinct days, explicit promotion, or revision one's
adaptive bar, strictly more than `min(7, active_days / 2)` distinct days); has a
context ceiling the request's prompt bound fits (the offer's listed window if
there is one, otherwise a learned declared window or the largest prompt seen).
Manual promotion satisfies learning when the policy allows it. It never
downgrades and never changes candidate or binding: fallback chooses a candidate,
upgrade chooses a version inside it. The reasons are in [models.md](models.md):
the election bar scales so a young ledger is not dormant; a downgrade would pin
the account below a new model forever because the new model could never accrue
days.

An upgrade is considered only where no cache can be lost
(`policy/force_newest.rs`): on a lane proven cold, or, for a request with no
lane record, a conversation of at most `NEW_CONVERSATION_MESSAGES` messages or
one whose asked model has been idle for a full cache TTL (`idle_for_ttl`, read
against the whole 1-hour tier and only as far back as the recency record
reaches). A lane the table has forgotten is not a new session. Once decided, an
upgrade is sticky: while the lane is warm, a request still naming the baseline
resolves to the recorded `forced_to`, because the client never learns of the
rewrite and honouring its next request would pay for the prefix twice. A
compaction never upgrades.

The upgraded plan's lane transition policy is the stricter of the route's, the
rule that selected it and the upgrade policy's (`stricter_transition`).

**Compaction.** The compaction target is either a route (authored) or the
revision-one legacy form: a model spec, such as `sonnet`, and the bindings it
applies to. The legacy form moves a cold compaction to the cheaper member of the
named family (or the exact model), keeping only a target that is eligible by
the revision-one bar, priced, cheaper than the source and within the lane's
prompt size. The body transforms a compaction also gets (cache-write strip,
system merge) are the cold gate's, not policy's; see
[compaction.md](compaction.md) and [cold-gate.md](cold-gate.md).

**Circuits.** A candidate may declare `health_circuits`: for a classified
failure, how long the target is out. When an attempt fails with a failure class
the candidate has a circuit for, the target (revision, route, candidate,
binding, effective model) opens one; any open circuit makes the target
unavailable to later requests until it expires, a success on the same target
clears it, and a catalogue refresh of the binding or a credential change
(`bump_credential_generation`) drops it, because both are evidence the failure
may no longer hold. An authentication failure never opens one when the failing
credential was the client's own passing credential: that is the client's
problem, not the backend's.

## Fallback and commitment

Fallback is the policy's answer to a candidate failing **after** a request was
sent; the preflight skips above are the same rules before it. A request is a
group of attempts (`attempt_group_id`, `attempt_index`), one ledger row each,
each charged to the backend that received it.

**What may fall back.** Only a failure that was classified without reading
message text, and that the crossed edge lists in `post_send`:
`authentication`, `model_not_found`, `rate_limit`, `provider_quota`,
`overload`, `server_error`, `transport_before_accept` and
`ambiguous_transport` (`attempt.rs`). Classification uses the provider's own
error types and codes (Anthropic `rate_limit_error`, OpenAI `insufficient_quota`
and so on), never wording, and status alone only for 401, 429 and 5xx. A 4xx
nothing names, an `invalid_request_error`, a refusal or anything unclassified
has `class = None` and is forwarded as the upstream sent it. A connect error is
`transport_before_accept`; any other transport error, or a body that dies before
commitment, is `ambiguous_transport`, kept apart because the provider may have
accepted and billed. The edge must consent to it separately, and the fall
re-checks the billed boundary and the warm-cache rule. The local quota gate is
not a candidate failing and never falls through to billed service.

**When a response is committed.** Until the frontend response head is returned
nothing is committed, and the coordinator holds the upstream head and at most
`PRECOMMIT_BODY_CAP` (64 KiB) of body while it decides. A non-success status
is buffered in full (up to the cap) and classified; over the cap it is
forwarded as it stands, unclassified. A compressed body cannot be read and
is committed as it arrives. A successful stream is committed on the first
complete non-keepalive SSE frame: an in-band error frame is a classified
failure, a canonical first event commits (`CanonicalEvent`), any other valid
frame commits (`OtherValidFrame`), and the cap commits (`ByteCapReached`). A
stream that ends or breaks before any frame, or whose first frame is not valid,
is a local 502 and falls back to nothing. A non-streaming success commits
at once. After commitment there is no retry, ever: the client has bytes. A
client that disconnects before commitment cancels the attempt and records a
`client_cancelled` error row; it never selects another candidate and never pins
the lane.

**One decision governs.** On a classified, edge-permitted failure
`resolve_after_failure` continues the same plan from the failed candidate: the
next candidate is chosen by the same rules, the crossed edge's consents are
folded in (`fallback_cache_transition`), and a new `PolicyAttempt` is built with
a fresh transition decision from the lane state captured when the request
began. The attempt that is accepted is the only one that pins: fall back
mid-session pins the replacement for later turns, and a recovered
higher-priority candidate does not pull the lane back. Details of the decision
are in [lanes-and-cache.md](lanes-and-cache.md). Fallback does not make a
provider's encrypted reasoning portable; affinity keeps such a request on its
binding, which can leave nothing to fall to.

The `thinking: disabled` retry in [routing.md](routing.md) is not fallback: it
is a second send to the same target inside one attempt, recorded as its own row
with reason `thinking_negotiation`.

## What is recorded

Provenance is content-free and identity-only (invariant 1). Each row of a
policy-routed request carries the policy revision, the route id and fingerprint,
the presentation or rewrite id that matched, the candidate and binding, the
baseline model, `requested_model`, `effective_model` and `raw_model` (what the
response said served it), and its attempt group, index and reason
(`initial`, `thinking_negotiation`, or the failure class that caused the fall).
`routing_trace` is a bounded JSON object (`StoredRoutingTrace`, version 2; at
most 128 events and 64 KiB, enforced in SQL) of events:

- `lookup`: how the route was found (profile or protocol presentation, rewrite
  or default, or compaction) and the id of the presentation or rewrite;
- `candidate`: the candidate, the binding, the model if one was materialised,
  the crossed-from candidate when an edge was crossed to reach it, and either
  `available` or `skipped` with a closed reason (`disabled`,
  `provider_unconfigured`, `binding_disabled`, `credential_unavailable`,
  `missing_required_capability`, `binding_affinity`,
  `model_missing_from_fresh_catalogue`, `selector_unresolved`,
  `health_circuit_open`, `preflight_fallback_disabled`,
  `billed_boundary_denied`, `warm_cache_transition_denied`,
  `warm_lane_transition_denied`, and the others in `StoredSkipReason`);
- `upgrade` and `alias`: from and to.

When a route is large enough to exceed the byte bound, candidate model spellings
are dropped first; the candidate identities and verdicts stay. A request no
candidate could serve is a `route-unavailable` row with no send-only fields,
carrying the trace and an attempt group of its own. A version 1 trace (no
crossed-edge evidence) is still valid history. The row kinds are in
[ledger-schema.md](ledger-schema.md).

A translated or rewritten response echoes the model the frontend requested, not
the effective or reported one; the ledger keeps all three, so the echo does not
erase the routing. When the provider's response names no model, the reported
identity is absent; it is not filled from the effective model.
