# The control endpoints

Everything under `/_toker/` is answered by toker itself and never forwarded; an
unknown path there is a local 404 (`anthropic::unmatched`). The listener binds
loopback only, but a web page in the user's browser can reach loopback, so the
paths are gated so that a page cannot drive them.

## The gate

Every endpoint names its operation in the `x-toker-control` header, compared
case-insensitively with the verb that operation expects. A request without it,
with the wrong one, with the wrong method, or (for a body) without a JSON
content type is a 403 with a plain-text body saying what is required. The check
runs before the body is read, so a refused request costs nothing and parses
nothing.

A browser can send neither half without a preflight. A cross-origin request with
a custom header, or with `content-type: application/json`, is preceded by an
`OPTIONS` request, and the page may only continue if that is answered with CORS
headers. Nothing in toker emits those, and the routes accept every method
(`any(...)`), so a preflight reaches the handler, fails the verb and method
check, and is answered 403 with no CORS headers. The browser then never sends
the real request. This is why there is no preflight route and why none may be
added: one would be the hole. A new control path takes the same two checks, a
verb of its own and a JSON body for anything that mutates.

Reads (`GET`) need only the verb. Every mutating operation needs the verb, its
method and a JSON content type (a prefix match on `application/json`).

| Verb | Method and path | |
| --- | --- | --- |
| `status` | `GET /_toker/status` | Resolved config (no secrets), ledger size, uptime, in-flight count, the active policy revision id, this process's `instance` id. |
| `session` | `GET /_toker/session?session=<id>` | One session's attribution aggregate and lane view. See [lanes.md](lanes.md). |
| `shutdown` | `POST /_toker/shutdown` | Drain and exit, for `toker restart`. Body `{"instance": id, "force"?: bool}`. See [routing.md](routing.md). |
| `routing-snapshot` | `GET /_toker/routing/snapshot` | Everything the routing page shows. |
| `routing-revisions` | `GET /_toker/routing/revisions` | Revision history without documents. |
| `routing-draft-replace` | `PUT /_toker/routing/draft` | Replace the draft with a complete document. |
| `routing-draft-edit` | `PATCH /_toker/routing/draft` | Apply typed edit operations to the draft. |
| `routing-validate` | `POST /_toker/routing/validate` | Compile the draft; report diagnostics. |
| `routing-preview` | `POST /_toker/routing/preview` | Resolve a described request against the draft. |
| `routing-activate` | `POST /_toker/routing/activate` | Make the draft the active revision. |
| `routing-reactivate` | `POST /_toker/routing/reactivate` | Copy a historical revision into a new active one. |
| `routing-promote` | `POST /_toker/routing/promote` | Mark a learned model eligible for upgrades. |
| `routing-publication` | `POST /_toker/routing/publication` | Record a failed or unsupported publication. |

`shutdown` is the one mutation that is not about routing: it names the instance
`status` reported, so a restart that raced another never stops an instance it
did not see (a mismatch is 409 with the current id), and it can only restart the
service.

## Routing operations

All of them answer JSON of the shape `{"toker": <operation>, "ok": bool, ...}`.
A failure adds `"error": <stable code>` and whatever structured detail the code
carries. Nothing in a body is content: it is revision ids, generations, policy
records and paths, counts, and catalogue facts. An offer is projected as
binding, provider, model id, context window, fetch time and chronology; its raw
provider record is deliberately not, because a provider can add fields the API
has not reviewed.

### The draft and its generation

The daemon holds one draft, in the ledger beside the active revision
(`routing_policy_state`). It is shared by every client, and it survives a
restart. Every change to it increments `draft_generation`, and every operation
that writes states the generation it was based on (`expected_generation`). If
the draft moved, the answer is 409 `stale_draft` carrying the current generation
and draft, and nothing is written. The routing page turns that into a conflict
view with a reapply option, rather than overwriting a change it has not seen.

Activation also writes the draft: the new active document becomes the draft, one
generation later, so the draft is never empty and "the draft equals the active
revision" is a state, not an absence. `base_revision_id` on the draft names the
revision it was last synchronised to.

Replace and edit take generation, validate and preview take it optionally
(omitted means whatever is current), activate and reactivate require it. The
generation is also checked inside the store transaction, so a second process
touching the ledger cannot slip a change between the check and the write; the
server additionally serialises every routing operation, and every catalogue
refresh, under one mutex.

### Reads

`snapshot` returns the active revision (with its document), the draft
generation and draft, the draft's compile diagnostics, the inventory
generation and offers, lane impacts, and every publication row. The diagnostics
are computed against the live inventory on each call, so a draft that was valid
last week and names a model that has since left a catalogue shows it.

`revisions` returns each revision's id, timestamps, description, activation
scope, whether a forced cache transition was recorded, source (`legacy`,
`draft`, `reactivation`), parent, and fingerprint, but not its document.

### Writing the draft

`PUT` replaces the draft with a complete policy document. The document goes
through the same bounded canonical decoder that persistence and activation use,
so there is one parser for control, storage and activation; a document that does
not decode is 422 `invalid_policy` with no diagnostics (it never became a
document to diagnose). A replacement is not compiled. It can name a route that
does not exist; that surfaces at validation.

`PATCH` applies `{"edit": {"operations": [...]}}`, a closed vocabulary of typed
operations (`set_description`, and add, replace or remove of defaults,
presentations, routes, rewrites, classifiers, upgrades, the compaction policy
and route candidates) applied to the current draft, 1 to 128 per request. It
exists so an editor cannot smuggle arbitrary path writes through the control
plane, and so a conflict can be retried: the routing page keeps the batch that
got a 409 intact and reapplies it on a reloaded draft. Edit errors name record
kinds, ids and policy paths, so their text is returned as `detail`. A missing
record is 404 `record_not_found`, a result that is not a valid document 422
`invalid_policy`, and any other bad edit 422 `invalid_policy_edit`.

The body cap is the document cap (1 MiB of canonical document) plus 16 KiB of
envelope for these two. Every other operation is capped at 1 MiB. Going over is
400 `invalid_request`, as is a body that is not JSON or has an unknown field:
every request type refuses unknown fields, so a typo is an error and not a
silently ignored option.

### Validate and preview

`validate` compiles the draft against one captured inventory and answers 200
with `valid` and `diagnostics`. Each diagnostic is a stable `code`, a
`severity`, the `record_id` and a JSON-pointer `path` into the document. They
are content free by construction: paths are built only from schema field names
and bounded indices. Diagnostics come only from a compilation that failed; a
draft that compiles has none, whatever it would warn about.

`preview` runs the resolver the request path uses, against the draft, for a
described request: protocol, profile, model, optionally a lane key (whose real
pin and temperature are used), a prompt size bound, message and tool counts,
required capabilities and a binding affinity. It answers what would be chosen,
with a trace of the lookup, each candidate considered or skipped and why, the
alias and upgrade applied, and the cost or cache transition implied. The result
is `selected`, `unavailable` (a route was found but no candidate can serve), or
`unresolved` with a code (`no_route`, `invalid_template` and so on). It reads
the learning evidence and catalogue availability as they stand, but not live
credentials or open health circuits, so it says what the policy would pick, not
what a send would survive. It records nothing and sends nothing.

### Activating

An activation compiles the draft, then in one `IMMEDIATE` transaction checks the
generation and active revision, inserts the new revision, points the active
pointer at it, replaces the draft with its document, and sets every publication
adapter's desired revision and inventory generation (see
[publication.md](publication.md)). Only after the commit is the runtime swapped
to the new compiled policy, as one snapshot of policy, inventory and
availability, so a request in flight keeps the one it captured. A compile
failure is 422 `invalid_policy` with diagnostics and changes nothing.

The body names an activation scope, which says what happens to lanes already
pinned to an older revision:

- `new_lanes`: pinned lanes stay on their revision; only new lanes see this one.
- `new_and_cold_lanes`: a pinned lane moves once its cache is cold.
- `immediate`: every lane resolves under the new revision now.

The scope is recorded on the revision. `immediate` can strand a warm cache, so
it needs consent: if any lane is warm, or not provably cold, and the request
does not set `force_cache_transition`, the answer is 409
`forced_cache_transition_required` with `lane_impacts` (`total`, `unpinned`,
`warm`, `cold`, `unknown`). A lane that predates policy pins has no recorded
cache domain and counts as unknown, never as cold: unknown must not prove cold.
The flag is stored on the revision, so history says whose cache was knowingly
spent. The other scopes need no consent because they never move a warm lane.

An optional `description` is stored with the revision.

### Reactivating

`reactivate` takes a historical `revision_id`. History is immutable and the
active pointer only moves forward, so it does not point back: it copies the
selected document into the draft and activates that as a new revision with
source `reactivation` and the selected revision as parent. Everything that can
refuse it (the generation, the consent gate, a compile against the live
inventory) is checked before the draft is replaced, so a refusal leaves the
user's draft and generation untouched. An unknown id is 404
`revision_not_found`.

Compiler origin is separate from source. A revision records which compiler its
document needs: the generated revision one needs the one that understands the
legacy vocabulary it was built from, which an authored document may not use. A
normal activation inherits the active revision's origin; a reactivation takes
the selected revision's. See [migration.md](migration.md).

### Promotion

`promote` takes `binding_id` and `model_id` and sets the sticky
`explicitly_promoted` bit on that binding's learned record, making the model
eligible for upgrades without enough days of use (see [models.md](models.md)).
It refuses a pair that has never been served (404 `unseen_binding_model`), so
promotion marks a record that exists and never invents one. It changes only that
bit: observed days, prompt ceilings and declared windows stay provider
evidence.

### Publication results

`publication` takes a scope, an adapter name, the desired revision and
inventory generation the adapter rendered, and an outcome. It accepts only
`failed` and `unsupported`, with an optional stable detail code, and refuses
`applied` (400 `adapter_owned_result_required`): an applied result carries the
exact row identities the adapter owns, and only the adapter that wrote those
rows can say what they are. The update is a compare-and-swap, so a result for a
superseded pair is 409 `stale_publication` with the current row, and cannot
overwrite a newer handoff or a success.

### Error codes

| Status | `error` | When |
| --- | --- | --- |
| 400 | `invalid_request` | Not JSON, unknown field, over the cap, empty edit list. |
| 400 | `adapter_owned_result_required`, `invalid_publication_adapter` | Publication. |
| 403 | (plain text) | Verb, method or content type. |
| 404 | `record_not_found`, `revision_not_found`, `unseen_binding_model` | Named thing absent. |
| 409 | `stale_draft`, `draft_missing`, `stale_publication`, `forced_cache_transition_required` | State moved, or consent needed. |
| 422 | `invalid_policy`, `invalid_policy_edit` | The document or edit is not valid. |
| 500 | `routing_control_failure` | A store or internal error, logged, not detailed. |

## The routing page

`toker tui` has a second page, entered from the header, with four views:
presentations, routes and candidates, rewrites, families and upgrades, and a
status view for preview, revisions and activation. It is built from the
endpoints above and holds no state of its own beyond the last snapshot and the
user's typed input.

- All traffic runs on one worker thread with a small bounded queue. Key presses
  only enqueue; the terminal never waits on HTTP, and a daemon that is down or
  restarting leaves the last snapshot readable. Reads time out after two seconds
  and writes after twenty, because a write that times out client-side may still
  land, and the page must not report a failure for a change the daemon made.
- The page edits the draft through `PATCH`, with typed records serialised
  exactly as the daemon's types, and tracks the generation of its last reply. A
  409 `stale_draft` opens the conflict view and keeps the local batch for
  reapply.
- Activation shows what changed, the lane impact counts and the publications
  affected before it asks, and sends the scope and, only if the user ticks it,
  `force_cache_transition`.
- "Import legacy config" reads `toker.toml` in the TUI process, compiles its
  routing fields into a document, and sends it with `PUT` at the loaded
  generation, so it is an ordinary draft change that must validate and be
  activated like any other (see [migration.md](migration.md)).
- After a successful activation or reactivation the page starts the Claude
  settings sync on its own thread and reports the outcome as a note. The
  activation does not depend on it (see [publication.md](publication.md)).