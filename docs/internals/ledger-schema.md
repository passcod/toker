# Ledger schema

The ledger is the `requests` table in the SQLite file at the config's `db_path`:
insert-only, one row per upstream attempt or proxy decision, written only by
`Store::record_request` (`store/ledger.rs` has the row type, `RequestRow`,
whose fields match the columns one to one, with the policy and attempt columns
grouped into `PolicyRouteProvenance`, `AttemptProvenance` and `SessionLineage`).
The state tables beside it (lanes, the routing tables, learned models,
allowances, pings, meter snapshots, meta) are read-modify-write and are not part
of the record; lanes and allowances are pruned on a 30-second tick (see
[lanes.md](lanes.md) and [quota.md](quota.md)). Migrations are an append-only
list gated by `PRAGMA user_version` (`store/schema.rs`): never edit, reorder or
delete an entry, because a database in the wild has had exactly the first *n*
applied, by position.

Every column but `ts_ms` is nullable, and NULL means "not recorded", never zero.
Fields were added over time, both in ctp and in toker, so **earlier rows lack
later fields**: always handle NULL, and never treat a missing field as a
negative value. `betas` NULL means "not captured", not "no betas". The same
holds for every column the routing work added: a NULL `policy_revision_id` says
the row predates policy routing or is not a routing row, never "revision zero",
and a NULL `attempt_group_id` says the row predates attempt identity, never "a
one-attempt request" (see [Old rows](#old-rows-and-what-is-no-longer-written)).

## Row kinds

A row with `kind` NULL is an API measurement; any other kind is a row toker
wrote about itself. `ledger::is_api_measurement` is that rule, kept in one place
so every consumer classifies identically. Exclude by the *presence* of a kind,
never by listing kinds: ctp listed them, which let `released` slip into both the
quota fit and the per-request averages. Any measurement about what the API
reported, and when, must exclude them; counting them reports the proxy's own
staleness back as the API's, which is how a 77-second observed lag first read as
62.

| Kind | What it records |
| --- | --- |
| `blocked` | The quota gate stopped a request. `rate_limits` is toker's last-seen meter copy, not a response header: nothing reached upstream. `extra` holds `meter`, `resets_at`, and `context_tokens`. |
| `released` | A release marker, or the TUI's controls, granted an allowance. Its `rate_limits` is a last-seen copy too. `extra.release` says which release: `overage` or `plan`; rows from before the plan marker lack it, and were all overage. `extra.via` is `tui` for a TUI grant, absent for a marker. |
| `revoked` | The TUI's Close control deleted a session's allowances. `extra.allowances` counts them, `extra.via` is `tui`, and `rate_limits` is a last-seen copy. |
| `cold` | The cold notice fired. No `rate_limits`. |
| `cold-quiet` | A cold notice was withheld. `extra` carries `quotaExtra` (the re-read's estimated share of a 5-hour window), `quotaBound` (true where that weight was borrowed, so an upper bound), and `util5h` (the utilisation the decision rested on, from the burn, not the last-seen copy). The request itself went through, so an ordinary row follows. |
| `cold-recap` | A Claude Code recap on a cold lane was answered by the gate and never forwarded. `extra` carries the `cold` row's `idleMs`, `lastPrompt` and `reqMessages`. The lane is untouched, so the notice is still armed. Absent before 2026-10-06. |
| `awake` | The sleep lock changed hands. No `rate_limits`, and no frontend, provider or route: the lock is not a route. |
| `error` | A failed upstream attempt, or a non-2xx on a usage path: `status`, `error_type`, `retry_after_ms`. Never priced. On a policy-routed send it carries attempt identity like any attempt, and `error_type` is the failure class (`authentication`, `rate_limit`, `server_error`, ...), `upstream_error`, `response_protocol`, `local_preparation`, or `client_cancelled`. `local_preparation` is also a request toker could not build a body for at all, which has no attempt, provider, route or binding (see [attempts.md](attempts.md)); the refused half of a thinking negotiation keeps the upstream's own error type. Its `rate_limits` is the failed response's own snapshot. |
| `route-unavailable` | Policy would not route the request, so nothing was sent: no route matched, every candidate was skipped, or the lane's pin refused the move. Carries the routing trace and a group id but no attempt index, provider, binding, candidate or reason. See [Route-unavailable rows](#route-unavailable-rows). |
| `fidelity-drift` | Historical only: before universal canonical inference rendering, re-serialisation diverged from the client's bytes; `drift_digest` identifies the divergence. The original bytes were forwarded. New inference rows do not emit this kind. |

`blocked` rows' `context_tokens` is the size of the session's largest lane, the
figure the notice reported. It is NULL where the lane table did not know the
session: a reader must not read that as an empty conversation, and the notice
drops the clause rather than printing a zero.

`cold-quiet` rows exist because a withheld notice and a broken gate are both
silent. A view counting firings without them reports a working feature as a
regression.

## Fields that need care

- **Model identity has three stages.** `requested_model` is what the client
  named; `effective_model` what toker sent in that attempt, after the route's
  upgrade, compaction move and alias; `raw_model` what the response says served
  it, with `model` its pricing-normalised form. The served identity is
  authoritative for pricing, context, observed days and `max_prompt`; the other
  two are routing provenance only. A response that names no model leaves
  `raw_model` and `model` NULL: nothing fills them from `effective_model`, and
  the TUI shows `reported -`. A fallback attempt has its own `effective_model`,
  so the two attempts of one request can differ.
- `forced_from`/`forced_to` mark every request an upgrade policy moved, not only
  the first: the lane remembers the move while its cache is warm, so each warm
  request on a moved lane carries them (the resolver reads them back from the
  lane). `downgraded_from`/`downgraded_to` mark a compaction that policy moved
  to another model; `cache_stripped` and `system_merged` stand on their own,
  because a compaction already on the target model is rewritten without being
  downgraded. All four are taken from the attempt plan, not from the body
  transform.
- `model_mappings` is written only on batch creation: one
  `{requestIndex, requestedModel, effectiveModel}` entry per embedded request
  whose model policy moved. NULL means no entry moved, or a row that is not a
  batch.
- `gate_on` and `cold_on` record whether each gate was armed for that request:
  the toggles are config the ledger cannot otherwise see.
- `ping` marks a request the window pinger sent.
- `extra.frontend` is the `/f/<frontend>` name the request came through; absent
  on unprefixed requests and on every row before prefixes existed.
- `extra.recap` is `true` on a measurement row that was a Claude Code recap
  forwarded upstream (a warm lane's). Absent before 2026-10-06, when recaps were
  not detected.
- `extra.compactMarker` says where a compaction wording sat when one appeared in
  the last four messages, matched or not: `fromEnd`, `role`, `trailing` (the
  roles after it), `lineStart`, `toolResult`. Absent before 2026-10-06.
- A `thinking: disabled` refusal and its `between_tools` retry are separate
  rows in one attempt group (see [attempts.md](attempts.md)): index zero is the
  400 with reason `initial`, and index one has reason `thinking_negotiation`.
  Historical rows from 2026-10-09 through 2026-10-13 recorded only the retry
  and carried `extra.thinkingRewrite = "between_tools"`; they are not
  backfilled because the refused attempt's complete evidence is unavailable.
- `summarising` on toker's own rows was never set from the cutover until
  2026-10-06 (a trailing system message hid the prompt; see
  [compaction.md](compaction.md)), so a compaction count by that flag over that
  range is zero, not a measurement. Count by `compact_generations` on the rows
  after instead.
- `cost_usd` comes with `cost_kind` (`billed`, `estimated`, `plan_equivalent`),
  and the kinds are never summed together blindly.
- `rate_limits` is a meter snapshot in ctp's camelCase shape (`util5h`,
  `reset5h`, `claim`, …), kept verbatim so existing analysis keeps working;
  unknown headers land in its `other` map rather than being dropped. Resets are
  epoch seconds; `ts_ms` is epoch milliseconds.
- `reset5h`/`reset7d` group rows into the exact quota windows the API reported,
  so any window can be reconstructed afterwards. The 7-day window has a fixed
  boundary, so a ledger started mid-week can only give a partial first week.

## Policy provenance

Every request that policy routed says which decision routed it. The columns are
flat on the row, grouped in Rust as `PolicyRouteProvenance`:

| Column | Meaning |
| --- | --- |
| `policy_revision_id` | The immutable revision whose compiled policy resolved this send. A lane pinned to an older revision resolves under that one, so this is not always the active revision. |
| `presentation_id`, `rewrite_id` | Which record matched the client's model string: at most one is set, and neither when the profile or protocol default matched, or when the compaction route did. |
| `policy_route_id`, `route_fingerprint` | The route and its semantic fingerprint (64 lowercase hex). The fingerprint changes when the route's meaning does, so a lane pin can tell an edited route from the one it was made under. Absent, together with the lookup ids, only on a route-unavailable row that never reached a route. |
| `candidate_id`, `binding_id` | The chain candidate that was chosen and the backend binding it names, as `provider.adapter`. Absent on a route-unavailable row. |
| `baseline_model` | What the candidate named before any upgrade, compaction move or alias: the client's spelling for a requested-model candidate, the policy's own exact model otherwise. |
| `routing_trace` | The content-free account of the lookup and of every candidate considered. |

The lookup source (which of presentation, rewrite or default matched) is stored
twice, in the two id columns and as the first event of the trace. The writer
refuses a row where they disagree, and the reader fails on one rather than
skipping it, so a stored contradiction is loud. `effective_model` (above) is the
other end of `baseline_model`: the pair is how a reader sees what the upgrade,
the compaction move and the alias did to this attempt.

The trace is a JSON object, `{"version": 2, "events": [...]}`, at most 128
events and 64 KiB, with model strings of at most 1,024 bytes (`store/ledger.rs`,
`StoredRoutingTrace`). Version 1 rows are valid history and carry no
crossed-edge evidence; the reader accepts both. Events are a closed vocabulary:

- `lookup`: the source above, with the matched record id.
- `candidate`: a candidate id, its binding, the model it would have sent, and
  `available` or `skipped` with a closed reason (`disabled`, `binding_unknown`,
  `provider_unconfigured`, `binding_disabled`, `credential_unavailable`,
  `missing_required_capability`, `binding_affinity`,
  `model_missing_from_fresh_catalogue`, `model_unknown`, `selector_unresolved`,
  `health_circuit_open`, `preflight_fallback_disabled`,
  `billed_boundary_denied`, `warm_cache_transition_denied`,
  `warm_lane_transition_denied`). `crossed_from` names the candidate before it
  when the chain stepped across an edge to reach this one.
- `upgrade` and `alias`: the `from` and `to` models of each step.
- `no_route`: no route matched the request. Only a route-unavailable row has
  it, as its only event, and then the row has no lookup, route or fingerprint.
- `transition_denied`: the lane's pin refused the move, with the closed reason
  (`stale_revision`, `lane_pinned`, `lane_not_cold`, `cache_not_cold`,
  `warm_cache_loss`, `invalid_cause_identity`). It closes the trace of a
  route-unavailable row: after the events that chose the route, or alone when
  the lane was pinned to a revision the service cannot see yet.

Nothing in it is request content: ids, models, states and reasons only. When a
maximum-sized trace would exceed the bound, candidate model spellings are
dropped before anything else, because the candidate id and verdict are the
decision and the spelling is detail. An unknown version, event kind or reason is
rejected on insert and on read, so a future writer's vocabulary is a migration,
not a silent widening.

Each attempt row carries the trace of the plan that selected it. Candidates
skipped before the first send ride on the first attempt's row, and a fallback
attempt's row carries the trace of the resolution that chose it.

## Attempt identity

One logical request can make several upstream sends. Each send is its own row,
and the rows share an identity:

- `attempt_group_id`: 32 lowercase hex characters (a random UUID without
  dashes), minted once per logical request.
- `attempt_index`: zero-based and contiguous within the group.
- `fallback_reason`: why this attempt exists. The column name predates the
  attempt model and is kept; its meaning is "the reason this actual attempt
  exists". Index zero is always `initial`, and `initial` is never used at any
  other index. The rest are `thinking_negotiation` (the same candidate resent
  after a `thinking: disabled` refusal) and the failure classes that licensed a
  move to the next candidate: `authentication`, `model_not_found`, `rate_limit`,
  `provider_quota`, `overload`, `server_error`, `transport_before_accept`,
  `ambiguous_transport`.

[attempts.md](attempts.md) says when each happens. The schema enforces the shape
rather than trusting the writer: a unique index on `(attempt_group_id,
attempt_index)` where the index is not NULL, an `attempt_index` without a group
is refused, an insert at index *n* needs index *n* - 1 already present (and
index zero needs none), the reason must be in the closed set and agree with the
index, and a group is either sends or one preflight terminal, never both (the
triggers `requests_attempt_sequence_check`, `requests_attempt_reason_check`,
`requests_provenance_insert_check`). Store writes are serialised on one
connection, which is what makes the predecessor check deterministic.

A client that disconnects after a send but before any response is committed
leaves an `error` row for that attempt with `error_type = client_cancelled`. The
send happened and may have cost something, so it is a row, not an absence.

Counting follows from the shape, and the rule is stated once here: **a logical
request is a distinct non-NULL `attempt_group_id`, or a row with none**; rows
with a NULL group are historical or proxy rows and each stand alone. Token,
cost, meter and error totals sum the physical rows. Quota fitting and model
learning read each attempt's own evidence and never a group total.
[attempts.md](attempts.md) has the read models that apply this.

## Route-unavailable rows

When policy will not route a request before any send, toker answers the client
with an error and writes one `route-unavailable` row, so the refusal does not
vanish from the ledger just because nothing was sent. There are three causes,
told apart by the trace:

- every candidate of the selected route was skipped (the candidate events say
  why);
- no route matched (`no_route`): for a missing default the client gets a 502
  marked not configured;
- the lane's pin refused the move (`transition_denied`), answered like any
  other failure to resolve, with a 502.

The row has a group id but no `attempt_index` and no `fallback_reason` (the
terminal shape, enforced by `requests_route_terminal_shape_check` and
`requests_terminal_trace_check`): it has `policy_revision_id` and the trace, and
the route, its fingerprint and the lookup ids whenever a route was chosen. It
must not have a `candidate_id`, `binding_id` or `baseline_model`, because none
was chosen. `provider`, `route`, `effective_model`, `model`, usage and cost are
NULL and its `requested_model` is the client's. It carries session lineage
where the handler knows it, which is the Responses path. It is not an API
measurement and is never priced.

A failure of the policy or the service to resolve a request (a template that
does not render, an unavailable learning snapshot, a selected binding that is
not configured) is a fault, not a refusal, and writes no row. A pin refusing a
fallback after an attempt already failed does not either: that group already
has its error row, and its final outcome is that error.

Only the inference paths write it. `count_tokens`, batch creation and the
transparent administrative forwards leave no row either way.

## Session lineage columns

`parent_session_id`, `child_role` and `relation_source` are either all NULL or
all set, and the only value pair accepted is role `guardian` from source
`codex_guardian`. The parent must differ from the row's own `session_id`, and an
insert that would close a cycle through earlier rows is refused
(`requests_provenance_insert_check`, `requests_lineage_cycle_check`). A parent
with no row of its own is valid. `session_id` is never rewritten: it stays the
raw identity the lane and the cache are keyed on. What counts as evidence, and
why almost nothing does, is in [lineage.md](lineage.md).

## Routing tables

These are state, not record: they are read-modify-write through the store's
compare-and-swap operations and none is pruned.

### Revisions and the active pointer

`routing_policy_revisions` holds every policy revision ever activated. A
revision is immutable: the store has no update or delete for it, and nothing
reads one without checking that its `document_json` is byte-identical to the
canonical re-encoding of itself and that its `fingerprint` (SHA-256 of that
document, 64 lowercase hex) matches. Columns:

- `revision_id`, assigned in order, never reused.
- `created_ms`, `activated_ms`: equal at the moment of insertion.
- `description`, free text from whoever activated it.
- `activation_scope`: `new_lanes` (a lane pinned to an older revision keeps
  resolving under it), `new_and_cold_lanes` (a pinned lane whose cache is cold
  moves too) or `immediate` (every lane resolves under the revision).
- `forced_cache_transition`: 0 or 1, whether an `immediate` activation may
  discard warm caches.
- `source`: how it came to exist: `legacy` (generated revision one),
  `draft` (an authored draft applied) or `reactivation` (an earlier revision
  restored as a new one).
- `compiler_origin`: `authored` or `generated_revision_one`, which compiler
  semantics the document needs. It is separate from `source` on purpose:
  restoring revision one is a reactivation, but its document still needs the
  compatibility compiler.
- `parent_revision_id`: the revision it replaced, or for a reactivation the one
  restored.
- `document_json`, `fingerprint`. The fingerprint is not unique: reactivating a
  revision makes a new row with the same document, which is why v9 exists.

`routing_policy_state` is a singleton row (`id = 1`, created by v7):

- `active_revision_id` is NULL until the server first starts. It then
  establishes revision one in a single immediate transaction
  (`Store::activate_revision_one`, called by
  `policy::legacy::establish_revision_one`): insert the revision generated from
  the config's routing fields, make it active, seed the draft with its document,
  set `legacy_imported_ms`, and create the publication rows. If a revision is
  already active the config's routing fields are not consulted again. A
  `legacy_imported_ms` with no active revision is an error, not a state to
  repair silently. The column name is historical: it is the time revision one
  was generated.
- The draft is `draft_base_revision_id`, `draft_document_json`,
  `draft_fingerprint`, `draft_updated_ms`, all set or all NULL, with
  `draft_generation`, a counter every draft write, clear or activation bumps. A
  draft write or an activation names the generation it expects and loses on
  mismatch, so two editors cannot overwrite each other silently. Activation also
  checks the active revision is the one the document was compiled against.

### Publications

`routing_publications` records, per frontend scope and adapter, which revision
the outside world should reflect and which it was last made to reflect. The key
is `(scope_key, adapter)`. `scope_key` is `protocol:<protocol>` or
`profile:<profile>:<protocol>` with the protocol one of `anthropic_messages`,
`openai_chat`, `openai_responses`; no nullable column takes part in identity.
Revision one creates three rows: `dynamic_chat_models` and
`dynamic_responses_models` (both start `applied`, because they are served live
from the revision and own no external row) and `claude_settings` (starts
`pending` until the Claude settings reconciliation runs).

- `desired_revision_id` and `desired_inventory_generation` say what to publish;
  the `applied_*` pair says what was last published. The inventory generation
  exists because a selector can change its winner when a catalogue refreshes,
  without any new revision.
- `state` is `pending`, `applied`, `failed` or `unsupported`. `applied` requires
  both applied values to equal the desired pair (a CHECK and a reader check).
  `pending`, `failed` and `unsupported` may keep the last applied pair, so a
  failure does not forget what is still published.
- `detail_code` is a stable identifier, never a free-form error, so nothing an
  external tool said can end up in the ledger.
- `applied_identities_json` is the exact set of external row identities the
  adapter wrote when it last applied (for example the setting keys it owns): a
  sorted, unique JSON array of at most 1,024 entries and 64 KiB. An adapter may
  remove only what it listed there, so a row the user wrote by hand is never
  touched. It moves atomically with the applied pair and survives `pending` and
  `failed` handoffs.
- A result is applied by compare-and-swap on the desired pair, so a late
  reconciler cannot mark newer desired state applied. A failed or unsupported
  result never overwrites an applied one.

### Binding learning

`learned_models_by_binding`, keyed by `(binding_id, model_id)`, is what the
upgrade policies elect from. Evidence belongs to one binding because the same
model name on two providers is two different things with different caches and
prices.

- `days_json`: the sorted set of local days (`YYYY-MM-DD`, in the machine's
  time zone when the response arrived) on which the binding served this model.
- `max_prompt`: the largest prompt (input, cache read and cache write) seen.
- `explicitly_promoted`: set by `toker promote`, never cleared.
- A merge unions days, takes the larger `max_prompt`, ORs the promotion and
  keeps the later `updated_ms`. NULL stays NULL.

It is written after a completed response that names a model
(`Server::note_binding_observed`), using the provider's reported model, never
the effective one, so a response that names nothing teaches nothing. The
"recently sent" evidence the resolver uses to judge whether a cache could still
be warm is not in this table: it lives in memory, is marked just before each
send, and is rebuilt at startup from the newest 20,000 ledger rows that carry
both `binding_id` and `effective_model`. `error` rows count, conservatively: a
provider may have answered, and forgetting a send could lose a live cache.

Nothing rebuilds this table from earlier ledger rows: ones without a binding
cannot be attributed to one, and are deliberately not copied into every possible
binding. Learning starts empty on a ledger that predates binding attribution and
grows from live responses. The model's declared window is not stored here; the
context-fit check reads the offer inventory, then `max_prompt`.

### Lane pins

`lanes` gained a routing pin, seven columns that are all NULL or all set:
`policy_revision_id`, `policy_route_id`, `route_fingerprint`, `candidate_id`,
`binding_id`, `baseline_model`, `effective_model`, plus `pin_generation`, a
counter. A trigger refuses a half-set pin and a revision that does not exist
(`lanes_pin_insert_check`, `lanes_pin_update_check`). A pin is written when an
attempt completes, never at resolution time and never by a client that
cancelled. It is replaced by compare-and-swap on the generation and the complete
prior identity (`Store::compare_and_swap_lane_pin`), so a completion that lost a
race cannot restore an older pin. Ordinary lane upserts (`Store::upsert_lane`,
which moves `updated_ms`, `prompt_tokens`, `ttl` and the notice and upgrade
fields) never name the pin columns, so they cannot overwrite one. A lane with no
pin predates policy routing, or has not completed an attempt under it; the
resolver treats it like a new lane (see [lanes.md](lanes.md)).
`lanes_session_idx` serves the per-session lane read the opencode plugin makes.

## Migrations from v7

v1 to v6 are the original schema, the lane notice and sticky-upgrade columns,
per-provider meters, ping outcomes, the lane index and the allowance release
kind. Routing storage is v7 onwards, and each entry is there for a reason:

| Version | What and why |
| --- | --- |
| v7 | Live-routing storage: the revisions table, the state singleton (active pointer NULL, set only by the server), publications, binding learning, the lane pin columns and their triggers, and every provenance, attempt and lineage column on `requests` with their indexes and checks. No backfill: old rows stay NULL. |
| v8 | Constraints `ALTER TABLE` cannot express: a unique index for preflight terminals, the attempt-sequence trigger, the policy-revision-exists trigger. v7 had no writer for attempt fields; if a raw fixture had put malformed ones in, v8 would abort and leave history alone. |
| v9 | A table rebuild so `fingerprint` is no longer unique. Reactivating a revision creates a distinct immutable revision with the same document. The rebuild carries the parent and both dependent tables together and recreates the triggers that referenced them. |
| v10 | Makes the preflight terminal explicit and bounds the trace envelope (object, version, events array, 128 events, 64 KiB). Existing rows are validated first. |
| v11 | The trace accepts version 2 beside version 1. SQLite keeps only the bounded envelope; the Rust reader owns the closed event vocabulary. |
| v12 | The closed attempt-reason set and the route-unavailable shape, for raw SQL writers too. Existing rows are validated first. |
| v13 | `applied_identities_json` on publications, so an adapter owns only the rows it listed. |
| v14 | A publication that was `applied` before v13 has no ownership record, and treating unknown as empty would let the next reconciliation delete a user's row. It becomes `pending` with detail `legacy_applied_identities_unknown`, until the adapter applies again and records an exact set. |
| v15 | `compiler_origin`. Existing revision-one lineage (revision one and reactivations descending from it) is marked `generated_revision_one`. |
| v16 | `lanes_session_idx`. |
| v17 | Drops `learned_models_by_binding.context_window_json`: nothing wrote it. |

The ledger is insert-only, so a migration never rewrites a request row. Where a
constraint is added to existing history (v10 and v12), the validation runs first
inside the migration's own transaction; if a row would violate it the migration
fails and the database stays at the previous version, and the service does not
start until the row is repaired by hand. No toker writer produces such a row.
`Store::open_read_only` never migrates, so `toker export`, `toker
watch-context-window` and an older binary leave the live file alone, and
`Store::schema_is_current` lets a reader tell a ledger that has not been
migrated yet from one it cannot read.

## Old rows and what is no longer written

Nothing here backfills, so a reader meets every era in one table:

- **Before v7** (and every row imported from ctp): none of the policy, attempt
  or lineage columns exist. Each such row is one logical request and has no
  routing provenance. A request that was in fact two sends reads as one row, for
  example the 2026-10-09 to 2026-10-13 thinking retry rows under "Fields that
  need care".
- **Between v7 and policy being the only router:** a send may carry attempt
  identity and `binding_id` without any policy columns, because a request the
  policy did not route was still sent. Do not require the policy columns where
  the attempt columns are present.
- **Since policy routes everything:** every inference attempt is written with
  both, so a missing `policy_revision_id` on a recent measurement is a bug to
  chase, not an era to explain.

What the service no longer writes, and how to treat what is left behind:

- The **`models` table** (days served, max prompt, declared context window per
  bare model id) has no writer. Routing never reads it: the learned evidence the
  upgrade policies use is `learned_models_by_binding`, and an old row's
  promotion was not copied because its provider cannot be reconstructed. Only
  `toker watch-context-window` still reads its declared windows. Treat it as
  frozen history.
- **`routing_policy_state.legacy_imported_ms`** and revisions with source
  `legacy` are still written, once, when revision one is generated. The names
  are historical.
- **`lanes.forced_from` and `lanes.forced_to`** are still written and are how a
  warm lane keeps its upgrade, but the decision that sets them is now the
  revision's upgrade policy.
- The legacy **model map, force-newest toggle, default-backend fields and
  prefixes** are no longer applied at request time. They are inputs to revision
  one only. Rows that say a model map or force-newest moved a request were
  written by the earlier code, and `model_mappings` on a batch row from that era
  records the TOML map's moves where a current one records policy's.
- `fidelity-drift` rows are historical only (see above).

## Imported ctp rows

`toker import` maps ctp's `usage.jsonl` into this table (`import.rs`), so the
ledger reaches back past toker's own first request. Imported rows are written to
look like toker's own: `frontend` is `anthropic`, `provider` is `anthropic_sub`
unless the row named one, `cost_kind` is `plan_equivalent` by default, and kind
payloads ride `extra` in the shapes toker's writers use. What they cannot carry
is anything ctp never recorded: no `usage_raw`, no `extra.frontend`, no
historical `fidelity-drift` rows. A view over a range that reaches into
imported rows must say so rather than read those absences as zeros.

ctp's own field eras carry over with the rows:

- `summarising` before 2026-09-22 matched only the full compaction's wording, so
  a partial compaction in that range is an ordinary request: the flag is absent,
  and its `compact_generations` shows up on the requests *after* it instead. See
  [compaction.md](compaction.md).
- `summarising` between 2026-09-22T04:16Z and ctp's line-anchor fix (committed
  05:37Z the same day, live from ctp's next restart) was matched anywhere in the
  last message, so a row in *that* window may carry it falsely: one is known, an
  ordinary turn that quoted the table of wordings. Corroborate with
  `compact_generations` on the rows after it before believing a count there.
- ctp's very first name for the flag, `compacting` (2026-09-03, eleven minutes),
  matched anywhere in the body; the importer leaves it out on purpose and counts
  it as such.
- Before ctp made upgrades sticky, a lane's later rows show the old model, and
  that is what served them.
- Rows before ctp's ping field cannot say whether they were pings, so a count of
  pings reaching past it is a floor.

The importer counts every ctp field it does not map, by name, split into fields
left out on purpose and fields it has never heard of; an unknown one means the
mapping is behind the source.

## Export

`toker export` (`export.rs`, over `Store::for_each_export`) writes the rows as
JSONL, oldest first, streamed from the cursor. It is driven by the columns the
query returns, not by `RequestRow`, so a column a later migration adds is
exported the day it exists, and a stored value this binary does not understand
is shown rather than refused.

- Keys are the column names. The export uses the ledger's vocabulary so a key
  means exactly what this page says the column means; ctp's camelCase would
  make toker rows look like ctp rows while meaning something else.
- NULL is omitted, never written as `null` or zero. A missing key is "not
  recorded", and the same caution about earlier rows applies.
- Values are as stored: the boolean columns are `0`/`1`. The JSON-text columns
  (`JSON_TEXT_COLUMNS`: `rate_limits`, `extra`, `betas`, the ladders,
  `usage_raw`, …) are nested, re-serialised compactly; `usage_raw`'s exact
  bytes stay in the ledger.
- `ts`, the RFC 3339 UTC form of `ts_ms`, is the one key that is not a column,
  there so a date can be grepped.
- `usage_raw` is exported: it is the provider's `usage` object (counts and
  costs), which is all the recorders store there. Nothing that is not a column
  is added, so the export can carry no content or credential the ledger does
  not.

Export and `watch-context-window` open the ledger read-only
(`Store::open_read_only`): a wrong path fails instead of creating an empty
ledger, and an older binary never migrates the live file.

## System prompt changes

`system_change` (`{delta, where}`) and the ladders (`system_ladder`,
`system_tail`) have three eras, and a reader must tell them apart:

| Rows | `system_change` | Ladders |
| --- | --- | --- |
| Imported ctp rows | On changed rows, from ctp's in-memory lane map | Only on changed rows; every row from 2026-09-03 02:06Z to 07:50Z, cut to ctp's first geometry (2 KiB steps, a 4 KiB tail) |
| toker, before capture-time localisation | Never | Every row |
| toker, since | On changed rows | A lane's first row and changed rows |

Capture (`middleware/system_change.rs`) compares each anthropic measurement
with the newest measurement row in its lane that has a system hash, read from
the ledger. The ledger, not ctp's map or the lane table, because neither of
those survives: ctp's map was lost on every restart and the lane table is pruned
at 30 days. Where the hashes match, the row drops its ladders. Where they
differ, the rungs of the predecessor's prompt are found by its hash on the
newest lane row that kept them, and the change is localised as ctp did:
changed blocks, then the first differing 8 KiB step, then the tail window.

So a NULL `system_change` is not "unchanged". It is also a lane's first row,
every row before capture localised, a session-less row (no lane), and a change
whose baseline could not be read or was cut to another geometry; in all of
those the row keeps its ladders. Only a matching `system_hash` says the prompt
held. The rebuild panel prefers the stored `where`, re-derives it from ladders
where it is absent (finding a predecessor's dropped rungs by hash), and says
"where unknown" when neither bounds anything.
