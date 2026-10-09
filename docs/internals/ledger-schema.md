# Ledger schema

The ledger is the `requests` table in the SQLite file at the config's `db_path`:
insert-only, one row per request, written only by `Store::record_request`
(`store/ledger.rs` has the row type, `RequestRow`, whose fields match the
columns one to one). The state tables beside it (lanes, learned models,
allowances, pings, meter snapshots, meta) are read-modify-write and are not part
of the record; lanes and allowances are pruned on a 30-second tick (see
[lanes.md](lanes.md) and [quota.md](quota.md)). Migrations are an append-only list gated by `PRAGMA user_version`
(`store/schema.rs`): never edit, reorder or delete an entry, because a database
in the wild has had exactly the first *n* applied, by position.

Every column but `ts_ms` is nullable, and NULL means "not recorded", never zero.
Fields were added over time, both in ctp and in toker, so **earlier rows lack
later fields**: always handle NULL, and never treat a missing field as a
negative value. `betas` NULL means "not captured", not "no betas".

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
| `error` | A non-2xx on a usage path: `status`, `error_type`, `retry_after_ms`. Never priced. |
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
  named; `effective_model` what toker sent; `raw_model` what the response says
  served it, with `model` its pricing-normalised form. The served identity is
  authoritative for pricing, context, observed days and `max_prompt`; the other
  two are routing provenance only.
- `forced_from`/`forced_to` mark every request force-newest moved, not only the
  first: since the upgrade became sticky, each warm request on a moved lane
  carries them. `downgraded_from`/`downgraded_to` mark a compaction retarget
  that changed the model; `cache_stripped` and `system_merged` stand on their
  own, because a compaction already on the target model is rewritten without
  being downgraded.
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
- `extra.thinkingRewrite` is `"between_tools"` on the row of a request the
  upstream refused for its `thinking: disabled` and toker sent again with
  `between_tools` (see [routing.md](routing.md)). The refused attempt has no
  row of its own, so this key is how many refusals there were. Absent before
  2026-10-09, when the refusal was recorded as an ordinary error row instead.
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
