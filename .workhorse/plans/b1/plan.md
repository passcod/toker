# Routing policy control surface

B1 turns the persisted policy document and running `PolicyRuntime` into the
loopback control API used by the routing TUI. The API is daemon-owned: a TUI may
read the ledger directly, but all policy writes, validation, previews,
activation, reactivation, promotion, and publication acknowledgement go through
the running daemon.

## Boundary and ownership

The preceding `live-model-routing` branch provides the policy document,
compiler, revision store, draft compare-and-swap, binding-scoped learning,
publication rows, immutable runtime snapshots, and request resolver. B1 adds the
missing public control layer and the complete Store transactions for draft
activation and reactivation. It does not duplicate compiler, resolver, or lane
transition rules in HTTP handlers.

`src/server/control.rs` remains the local control boundary. A dedicated routing
control submodule should own request/response DTOs, JSON decoding, response
projection, and calls to the server orchestration methods. `Server` owns the
orchestration that combines Store transactions with `PolicyRuntime` swaps,
publication desired state, current offer inventory, availability, learning, and
lane-impact calculation. The Store owns only atomic database state transitions.

## Control gates and request boundary

Every route lives under `/_toker/routing/` and stays locally handled, never
falling through to Anthropic forwarding. Each operation has its own
`x-toker-control` verb:

- `routing-snapshot`
- `routing-draft-replace`
- `routing-draft-patch`
- `routing-validate`
- `routing-preview`
- `routing-activate`
- `routing-revisions`
- `routing-reactivate`
- `routing-promote`
- `routing-publication`

The gate check precedes request parsing. Every non-GET operation also requires
`application/json`; a missing or wrong gate, method, or content type receives
the existing local 403 response. Routing policy documents have their own body
ceiling matching the policy document maximum, rather than inheriting the
small-payload model-promotion limit. Smaller operations use a bounded control
body limit. All routing request DTOs use `serde(deny_unknown_fields)`.

The API returns only policy-owned state and content-free operational evidence.
It excludes credentials and their fingerprints, request or response content,
tool arguments, and upstream error bodies. Diagnostics expose stable code,
severity, policy record ID, and bounded JSON-pointer path only.

## Read model

`GET /_toker/routing/snapshot` returns one coherent snapshot for the TUI:
active revision summary, current draft and generation, offers and availability
as safe routing evidence, current compiler diagnostics, publication rows, open
circuits, and lane-impact counts. The response identifies the captured active
revision and inventory generation so the client can distinguish a later
catalogue refresh from a policy edit.

`GET /_toker/routing/revisions` returns immutable revision summaries in revision
order. Summaries include revision and parent IDs, timestamps, source,
description, activation scope, forced-cache-transition state, and fingerprint.
The complete document remains available through the active/draft snapshot or an
explicitly selected revision detail only if the TUI needs it; the listing does
not need to duplicate every historical document.

## Draft lifecycle

`PUT /_toker/routing/draft` replaces the draft with a complete policy document,
an expected draft generation, and its active base revision. The server decodes
and intrinsically validates the document before comparing and swapping it. On a
stale generation it returns conflict state, including the current generation
and canonical current draft, without applying any part of the request.

`PATCH /_toker/routing/draft` accepts typed record operations, not generic JSON
Patch. Operations add, replace, remove, or move one identified policy record,
and target a record collection or ordered route-chain position through typed
fields. The handler loads the canonical draft, applies all operations to a
copy, intrinsically validates the resulting document, and performs one
compare-and-swap using the supplied generation. It returns the same canonical
draft state as replacement. A batch is all-or-nothing: an unknown ID, duplicate
ID, invalid position, or invalid resulting document writes nothing.

Both mutation replies contain the new generation, canonical draft, fingerprint,
and updated timestamp so the TUI replaces its local state without a follow-up
write.

## Validation and preview

`POST /_toker/routing/validate` compiles the current draft against one captured
inventory, route topology, availability state, learning snapshot, and circuit
snapshot. It does not write policy state. Intrinsic document errors and compiler
diagnostics share the stable diagnostic response shape. A valid draft returns
its fingerprint and any non-blocking diagnostics.

`POST /_toker/routing/preview` receives only invented content-free request
facts: frontend protocol and optional profile, requested model, capability
flags, prompt bound, message/tool counts, opaque binding affinity, and lane
state necessary to model a pin. It resolves via the same pure policy resolver
as inference against one captured live evidence snapshot. The response returns
the complete content-free trace: presentation or rewrite provenance, selected
route and candidate, candidate availability and skip reasons, upgrade choice,
cost transition, lane-transition result, and terminal binding or typed resolve
failure. Preview never accepts a prompt body or a raw request envelope.

## Activation transaction and runtime publication

`POST /_toker/routing/activate` names the expected draft generation, activation
scope, optional forced cache transition, and optional revision description.
Activation first compiles the exact draft against a captured inventory and route
topology. Validation failure leaves the draft and runtime unchanged.

A successful activation uses one immediate Store transaction to:

1. re-check the expected draft generation and base state;
2. insert a new immutable `draft`-source revision whose parent is the active
   revision;
3. set it active;
4. retain the activated canonical document as the next editable draft with the
   new base revision and advanced generation; and
5. set desired publication revision and inventory generation for every
   applicable adapter.

The transaction returns the created revision, post-activation draft state, and
publication state. Only after it commits does the server swap the compiled
policy into `PolicyRuntime` while holding the runtime mutation lock. A failed
publication reconciler never rolls back the activated policy; it reports a
pending or failed publication row. The activation response includes the new
revision, draft generation, lane-impact summary by warm/cold state, and desired
publication state.

The server calculates activation lane impact with the existing transition
decision function rather than reimplementing cache rules. An immediate move
that could lose a warm cache requires explicit forced-cache-transition consent.

## Revision reactivation

`POST /_toker/routing/reactivate` names an immutable historical revision,
activation scope, forced-cache-transition consent where required, and optional
description. The Store verifies that the source exists, recompiles its document
against current topology and inventory, then creates a new `reactivation`-
source revision with the selected revision as its parent. It never moves the
active pointer backwards or mutates historical data.

Reactivation uses the same atomic state transition and runtime swap sequence as
draft activation. The resulting revision becomes both active policy and the next
editable draft, with a new generation. Its publication desired state targets the
current inventory generation.

## Promotion and publication results

`POST /_toker/routing/promote` names both binding ID and model ID. The binding
is mandatory because a provider-owned model name is not globally unique. The
operation delegates to binding-scoped learning promotion and returns the
updated learning record or a typed unseen result. It changes no policy revision.

`POST /_toker/routing/publication` accepts an adapter scope, adapter identity,
expected desired revision and inventory generation, outcome state, owned
external row identities, and optional stable detail code. It applies the
existing publication compare-and-swap so a late client-side reconciliation
cannot overwrite a newer desired state. The endpoint returns the current row on
conflict. Free-form external error text is not accepted or stored.

## Failure mapping and tests

The control layer distinguishes forbidden control attempts, malformed or
out-of-bounds input, stale generation or publication state, missing revisions or
learning records, invalid policy structure, compiler diagnostics, resolver
failure, and store/runtime failure. Only successful state changes receive a
success response. Store and runtime failures are logged without leaking their
internal or provider-derived detail to the control client.

Server tests drive the real router and cover every operation’s correct gate,
wrong gate, wrong method, content type, unknown fields, and body limit. They
also cover generation conflicts; atomic patch failure; validation and preview
remaining read-only; activation and reactivation creating immutable revisions;
runtime snapshots remaining stable for in-flight/pinned lanes; forced warm-lane
transitions; binding-qualified promotion; and stale publication acknowledgements
not overwriting newer desired state.
