# Publishing models to frontends

A frontend learns which models exist from two places: the model list it asks
toker for, and, for Claude Code, rows toker writes into Claude's own settings.
Both are projections of the same two inputs: the active policy revision and the
offer inventory (what each configured backend's catalogue lists). Neither is a
copy of any upstream's listing, and neither is edited by hand.

Publication is separate from routing. A slug that is listed is not thereby
routed, and a slug that routes need not be listed. Publishing only decides what
a frontend is shown. See [routing.md](routing.md) for how a request is routed
and [control.md](control.md) for how the policy is edited.

## What each frontend lists

`GET /v1/models` is answered locally for every frontend and never forwarded. It
is not a usage path and writes no ledger row. The frontend profile (the
`/f/<name>` prefix) picks the protocol, and `active_publication`
(`server/mod.rs`) renders the active revision's publication for that profile and
protocol with `catalog::offers::render_policy`. A profile that names no protocol
(an unfamiliar name, or no prefix) gets the Chat shape.

| Profile | Protocol | Shape |
| --- | --- | --- |
| `claude`, `workhorse` | Anthropic Messages | `{data: [{type, id, display_name, created_at}], has_more, first_id, last_id}` |
| `opencode`, no prefix, unknown | OpenAI Chat | `{object: "list", data: [{id, object, owned_by, created}]}` |
| `codex` | OpenAI Responses | `{models: [...]}`, the Codex catalogue's own entries with the slug replaced |

A listing contains the revision's presentations for that scope, and, for Chat
and Responses on a machine whose revision one was generated from the legacy
config, a generated listing (below). A presentation is an exact offer (one
model on one binding), a selector over a binding's offers (the newest few
matching some globs, for instance), or a virtual model that names a route. An
exact or selected presentation takes its display data from the offer's own
catalogue record, so `display_name` and `created_at` are the provider's. A
virtual one has no provider record: it names itself and has no creation time
rather than a made-up one.

Entries are sorted by slug, so the same revision and inventory render the same
bytes. A Chat or Anthropic list may carry virtual slugs. A Responses list
cannot: the Codex client's catalogue parser needs a complete model record for
every entry, and a virtual model has none, so a revision that publishes one on
the Responses protocol makes that listing unresolved. Only `codex_sub` offers
are projected to Responses for the same reason: a record from another provider
is not in the shape the client requires, so it is routable but not advertised.

### Unresolved listings answer 503

If an exact presentation names an offer the inventory does not hold (the
catalogue has not been fetched yet, or the model left the provider's listing),
the whole listing is unresolved and answers 503 `catalog_unavailable`. A partial
list would advertise a model that nothing can route, or silently drop one the
user chose. Failing the list is visible; a short list is not. The same
presentation compiles with an `offer-unknown` info diagnostic, which is how the
TUI shows it before activation.

### The generated listing

Revision one lists more than its presentations. Before policy routing, the Chat
and Responses lists were `<provider>/<id>` for every configured provider, plus
the bare `<id>` for the provider that was that protocol's default. Clients had
learned those slugs, so the generated revision keeps listing them. The slugs are
computed when the revision is compiled (`legacy::compile_document`) from the
stored document's own defaults and the current inventory, not read from the
config, so a stored revision lists what it routes rather than what a file says
today. They are publication only: a request naming one is routed by the
revision's rewrites and defaults, which keep the alias and upgrade policy of the
route they reach. A slug that a presentation also publishes belongs to the
presentation.

## Generations

Two numbers say which state a publication was rendered from. The revision id
names the policy. The inventory generation (`fetched::snapshot_generation`)
names the catalogue snapshot: a digest, folded into an `i64`, of every cached
catalogue source's fetch time and each model's id, context window, chronology
and raw record. It is deterministic, so a second process that reads the same
cache computes the same number. An empty cache has the fixed generation 0.

The generation exists because the service and the user-side picker sync are
different processes. The service holds catalogues in memory, and refreshes them
daily. The picker sync reads the same catalogues from the cache directory. A
sync that renders rows from a cache that moved since the service last handed off
would publish rows for an inventory the policy was never compiled against, so
it recomputes the generation over the cache it read and refuses to continue
unless it equals the one the service recorded (below).

Each catalogue refresh replaces the in-memory catalogues, the offer inventory,
the availability snapshot and the compiled policy as one update, under the same
lock the control operations take. Model-list readers see the old set or the new
set, never a mix. If the active revision no longer compiles against the new
inventory, the refresh is dropped and logged, and the previous set stays.

## Publication state

`routing_publications` holds one row per adapter and frontend scope. The
adapters are fixed (`store::publication_targets`):

| Adapter | Scope | Who applies it |
| --- | --- | --- |
| `dynamic_chat_models` | OpenAI Chat | The service, as part of installing the revision. |
| `dynamic_responses_models` | OpenAI Responses | The service, likewise. |
| `claude_settings` | Anthropic Messages | The user-side picker sync. |

Each row carries a desired revision and desired inventory generation, an applied
pair, the exact identities the adapter owns, a state, and a stable detail code.
The states are `pending`, `applied`, `failed` and `unsupported`.

The service's own adapters are applied by construction: the model list is
rendered on request from the runtime snapshot, so activating a revision marks
them applied in the same transaction that records the revision, with an empty
identity set because they own nothing external. The `claude_settings` row starts
`pending` at every activation and every catalogue handoff, because the service
cannot write Claude's settings. The desired pair and the policy it names are
committed atomically with the revision itself (see [control.md](control.md)), so
no committed revision lacks a desired publication, and the model lists the
service serves never run ahead of or behind what the row says is wanted.

Setting a new desired pair resets the state to `pending` and clears the detail
code, but keeps the applied pair and the owned identities. That is deliberate:
ownership has to survive a handoff the adapter has not reconciled yet (below).

A recorded result is a compare-and-swap on the desired pair the adapter
rendered. A result for a stale pair does not apply, and a `failed` or
`unsupported` result never overwrites a row that is already `applied` for that
pair. An `applied` row must carry applied values equal to the desired ones and
an identity set; the schema checks both, and a row read back that violates them
is a stored-state error.

## Claude's settings

Claude Code adds rows to its `/model` picker from `modelPicker` in
`~/.claude/settings.json`. The row shape and where Claude honours it are in
[routing.md](routing.md). What matters here is that toker writes into a file the
user also edits, owned by a program that hot-reloads it into every live session.
So the service never writes it (it may write only the state directory), and the
user-side writer has to be sure it only ever touches its own rows.

### Who runs it

`toker picker sync` runs as the user and calls `picker::sync`. It runs:

- once from `toker setup`, through `toker-picker.service`, so setup exercises
  the sandboxed path the timer will use;
- after every successful activation or reactivation in the routing TUI, on a
  short thread, so a slow or failed write can only produce a note on the page
  and never undo or delay the activation (`tui/routing/reconcile.rs`);
- daily from `toker-picker.timer`, which is randomised by up to an hour and
  persistent, since a late sync is still correct.

The units are installed whenever Claude is a frontend, not only with an
OpenRouter backend: the rows come from the policy, not from one provider. The
service unit is sandboxed to Claude's settings directory (the writer renames a
temporary file over `settings.json`, so it needs the directory) and, optionally,
the state directory, where even a read-only ledger open needs its WAL
shared-memory file.

Activation does not wait for any of this. The server publishes the new revision,
then the `claude_settings` row is `pending` until some sync records a result. A
failed sync leaves the activation standing and the row `failed`.

### What a sync does

1. It loads the `claude_settings` row and the revision it names, and recomputes
   the inventory generation over the cached catalogues of the enabled backends.
   If the cache is empty but the recorded generation is not, or the two
   generations differ, it fails before touching the file with
   `settings_render_failed`.
2. It compiles that revision against that inventory and renders the picker rows
   from the Claude presentations that carry a `behavesAs` model. Claude has one
   user settings file whatever profiles toker serves, so if any Anthropic
   profile projects different rows from the shared view, no single file can
   represent the policy and the row becomes `unsupported`
   (`incompatible_profile_presentation`) with the file untouched.
3. It reconciles the file (`patchers::reconcile_model_picker`): it removes the
   rows the last successful reconciliation owns and writes the new ones, only if
   the result differs, atomically.
4. It records `applied` with the exact identities it wrote, for the pair it
   rendered.

A database with no active revision is an error raised before the settings are
read. Empty rows would empty the user's picker.

If the compare-and-swap in step 4 finds that a newer desired pair arrived while
the file was being written, the sync adds the identities it just wrote to the
prior set and retries once against the newer pair. Without that, the second
pass would see its own first write as a user's row and refuse.

### Why ownership is exact

An earlier design owned every picker row whose `model` started with
`openrouter/`. That treats a user's own row with such a name as toker's, and
overwrites or deletes it on the next sync. Ownership by prefix also cannot say
what to remove when the policy changes the slugs it publishes.

The rule now is that toker owns exactly the `model` strings its last successful
reconciliation recorded in `applied_identities`, and nothing else:

- A row in the file is replaced or removed only if its `model` is in the prior
  owned set.
- If a desired row has the same `model` as an existing row that is not in the
  prior owned set, the sync refuses (`settings_write_failed`) and leaves the
  file as it was. The row is the user's. There is no merge and no takeover, and
  the failure says which `model` collided.
- A malformed picker (`modelPicker` or `options` of the wrong JSON type) is
  refused, never overwritten.
- A set that has never been recorded is not an empty set. Treating unknown
  ownership as none would make the next sync read toker's earlier rows as the
  user's and refuse forever, and treating it as everything would delete the
  user's. Migration 14 therefore moves every `applied` row that predates
  identities back to `pending` with the detail code
  `legacy_applied_identities_unknown`, and the first sync after that adopts only
  rows with the old `openrouter/` prefix, once, to carry the earlier install
  across. After that it works from recorded identities.
- Identities are stored in canonical order with a size bound and no duplicates,
  and a row that does not parse that way is a stored-state error, not an empty
  set.

This is why the identity set advances in the same statement as the applied
revision and inventory, and why a `pending` or `failed` desired handoff keeps
the last applied set: until a reconciliation succeeds, the file still holds
those rows and they are still toker's to remove.

## Where code and plan differ

The design plans described a generic publication endpoint through which adapters
report success. The endpoint exists (`routing-publication`, see
[control.md](control.md)) but refuses `applied` results: only the adapter that
wrote the rows can state which identities it owns. Nothing in the tree calls it
today. The picker sync records its results directly in the ledger.
