# Live routing configuration

B1 makes routing configuration live and reversible from the TUI. Every edit is
applied immediately as a new immutable configuration revision. If an edit routes
traffic incorrectly, the operator selects any earlier revision and applies it
immediately as a new revision. The TUI makes the active effect visible through
presented models, routes, active revision, and recent routing outcomes.

There are no drafts, staged edits, preview requests, or validation screens.
Validation remains an internal precondition of applying an edit: an invalid
configuration does not replace the current live configuration.

## Boundary and ownership

The preceding `live-model-routing` branch provides the policy document,
compiler, revision store, binding-scoped learning, publication rows, immutable
runtime snapshots, and request resolver. B1 adds the daemon-owned control API
and complete Store transactions for applying a configuration and applying an
older configuration again. It does not duplicate compiler, resolver, or lane
transition rules in HTTP handlers.

`src/server/control.rs` remains the local control boundary. A routing control
submodule owns request/response DTOs, JSON decoding, response projection, and
calls to server orchestration methods. `Server` combines Store transactions with
`PolicyRuntime` swaps, current offer inventory, availability, publication state,
and lane-impact calculation. The Store owns atomic database state transitions.

## Control boundary

Every route lives under `/_toker/routing/` and remains locally handled, never
falling through to Anthropic forwarding. Each operation has its own
`x-toker-control` verb:

- `routing-snapshot`
- `routing-apply`
- `routing-revisions`
- `routing-restore`
- `routing-promote`
- `routing-publication`

The gate check precedes parsing. Every non-GET operation also requires
`application/json`; a missing or wrong gate, method, or content type receives
the existing local 403 response. Configuration documents use a body ceiling
matching the policy document maximum. Smaller operations use a bounded control
body limit. Request DTOs use `serde(deny_unknown_fields)`.

The API returns only policy-owned state and content-free operational evidence.
It excludes credentials and their fingerprints, request or response content,
tool arguments, and upstream error bodies. Compilation failures return stable
diagnostic code, severity, policy record ID, and bounded JSON-pointer path.

## Immediate apply

`POST /_toker/routing/apply` receives the complete new policy document and an
optional description. The TUI owns its editing state locally and sends the
whole document whenever the operator confirms an edit. The daemon validates and
compiles the candidate against the current route topology and current inventory
before it writes anything. A compilation failure leaves the live configuration
untouched and returns its diagnostics.

A successful apply uses one immediate Store transaction to:

1. insert a new immutable revision whose parent is the currently active
   revision;
2. make that revision active; and
3. set desired publication revision and inventory generation for every
   applicable adapter.

Only after that transaction commits does the server swap the compiled policy
into `PolicyRuntime`. Requests already in flight continue using their captured
policy snapshot. A failed publication reconciler never rolls back live routing;
it reports pending or failed publication state instead.

The apply response includes the new active revision, its fingerprint, desired
publication state, and lane-impact summary by warm/cold state. The server uses
the existing transition decision function for that summary rather than
reimplementing cache rules. Existing lanes retain their pinned policy and target
unless the established lane-transition policy allows a move.

## History and restore

`GET /_toker/routing/revisions` returns immutable revision summaries in revision
order: revision and parent IDs, timestamps, source, description, activation
scope, forced-cache-transition state, and fingerprint. The active configuration
is visible in `GET /_toker/routing/snapshot`, alongside safe routing evidence:
current offers and availability, publication rows, open circuits, and
lane-impact counts.

`POST /_toker/routing/restore` names a historical revision and an optional
replacement description. The Store loads and recompiles that revision’s
configuration against current topology and inventory, then creates a new
immutable revision with the selected revision as its parent. It never mutates
history or moves the active pointer backwards. The new revision becomes active
through the same atomic transaction and runtime swap as a direct apply.

A restore is therefore an auditable forward operation: the history says which
configuration was selected and when, while the selected revision remains
unchanged for later reuse.

## Promotion and publication results

`POST /_toker/routing/promote` names both binding ID and model ID. The binding
is mandatory because a provider-owned model name is not globally unique. The
operation delegates to binding-scoped learning promotion and returns the
updated learning record or a typed unseen result. It does not create a routing
configuration revision.

`POST /_toker/routing/publication` accepts adapter scope and identity, expected
desired revision and inventory generation, outcome state, owned external row
identities, and optional stable detail code. It uses the existing publication
compare-and-swap so a late client-side reconciliation cannot overwrite newer
desired state. Free-form external error text is not accepted or stored.

## Implementation

- [x] Add atomic Store transactions for live apply and restore-as-new-revision.
- [x] Add Server orchestration that compiles a candidate against one captured
  inventory, persists the revision, and swaps the live runtime after commit.
- [x] Add gated routing snapshot, apply, history, restore, promotion, and
  publication-result endpoints.
- [x] Add a routing TUI page: Tab switches pages, e opens the active document
  in `$EDITOR` for immediate apply, and Enter restores a selected revision after
  a second confirmation.
- [x] Cover the control boundary and all live revision transitions with router
  and Store tests.
- [x] Run formatting, focused unit/integration tests, and clippy.

Full `cargo test` remains blocked by six pre-existing Codex policy-fallback
failures in `tests/server_codex.rs`; these failures are outside this card's
control and TUI paths.

## Tests

Server tests drive the real router and cover every operation’s correct gate,
wrong gate, wrong method, content type, unknown fields, and body limit. They
cover failed compilation preserving the active runtime and Store state; direct
apply creating and activating a new immutable revision; restore creating a new
revision rather than modifying history; runtime snapshots remaining stable for
in-flight and pinned lanes; binding-qualified promotion; and stale publication
acknowledgements not overwriting newer desired state.
