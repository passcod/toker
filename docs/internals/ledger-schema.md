# Ledger schema

The ledger is the `requests` table in the SQLite file at the config's `db_path`:
insert-only, one row per request, written only by `Store::record_request`
(`store/ledger.rs` has the row type, `RequestRow`, whose fields match the
columns one to one). The state tables beside it (lanes, learned models,
allowances, pings, meter snapshots, meta) are read-modify-write and are not part
of the record. Migrations are an append-only list gated by `PRAGMA user_version`
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
| `released` | The release marker granted an allowance. Its `rate_limits` is a last-seen copy too. |
| `cold` | The cold notice fired. No `rate_limits`. |
| `cold-quiet` | A cold notice was withheld. `extra` carries `quotaExtra` (the re-read's estimated share of a 5-hour window), `quotaBound` (true where that weight was borrowed, so an upper bound), and `util5h` (the utilisation the decision rested on, from the burn, not the last-seen copy). The request itself went through, so an ordinary row follows. |
| `awake` | The sleep lock changed hands. No `rate_limits`, and no frontend, provider or route: the lock is not a route. |
| `error` | A non-2xx on a usage path: `status`, `error_type`, `retry_after_ms`. Never priced. |
| `fidelity-drift` | Re-serialisation diverged from the client's bytes; `drift_digest` says where. The original bytes were forwarded. |

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
`fidelity-drift` rows. A view over a range that reaches into imported rows must
say so rather than read those absences as zeros.

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

Not yet in toker (cutover plan, Gate D): `system_change` is not computed at
capture time, so only imported rows carry it and the TUI localises system
changes from the stored ladders instead; and `toker export`, the JSONL view of
the ledger, is not implemented (it currently prints "not implemented" and exits
0).
