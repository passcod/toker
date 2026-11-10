# Requests in flight on the dashboard

## Technical notes

- The in-flight record is a store table, inserted when the server begins handling a request, updated at each stage change, and deleted once the request's last ledger row is written.
  It is not a ledger table, so no ledger reader has to learn about it.
  Chosen over polling the service's memory through a control operation because the dashboard already re-reads on `PRAGMA data_version` moving, so an insert there reaches the next tick with no new plumbing.
- The atomic in-flight count behind the `status` operation stays as it is; `toker restart` and the sleep lock read it, and nothing here needs them to change.
- Every stage change moves `data_version`, which triggers a display read and marks the heavy passes stale.
  The heavy passes are already spaced by `QUOTA_MIN`, so the extra writes should not make the loop spin, but check it under a busy stream.
- The service clears the table on start, before it accepts connections.
- A new ledger column records when the server began handling the request, so completed requests can be bucketed by arrival.
  Rows from before it lack the column and fall back to their group's first row.
- Session rows today come only from measurement rows (`is_api_measurement` gates the per-session pass in `tui/model.rs`).
  Gated, failed and in-flight-only sessions need the session map seeded from every row naming a session, not only measurements.
- The marks are settled in the design system: `held`/`sent`/`recv` with `+N` in the idle cell, `✗` red for a failure, `~` and dim for a model not yet served.
  `LEGEND` in `src/tui/view.rs` gains the same rows verbatim.
  The idle column widens to fit the stage cell; check the shed order at 80 and 100 columns.
- The quota gate's row (`record_quota_blocked` in `server/record_anthropic.rs`) writes no requested or effective model, though it already knows the provider and account.
  It needs both from the recording context, as error rows already take them.
  Older gate rows lack them, so a gated-only session from before the change still reads `?`.

## Build steps

Locally, each section ends with `cargo check`, `cargo clippy --all-targets` and `cargo fmt --check` only; the test suite runs in CI.

### Store

- [ ] Append one migration to `MIGRATIONS` in `src/store/schema.rs`: an `in_flight` table, and `requests.started_ms`.
  - `in_flight`: a row id, `started_ms`, `attempt_group_id`, `session_id`, `lineage`, `frontend`, `requested_model`, `req_messages`, `stage` (CHECK in `held`, `waiting`, `streaming`), and the current attempt's `attempt_index`, `provider`, `binding_id` and `effective_model`, all nullable bar the stage.
  - `requests.started_ms`: nullable, with no backfill.
- [ ] Store methods in `src/store/state.rs` (beside the other read-modify-write tables): `begin_in_flight` returning the row id, `in_flight_sent` (attempt fields, stage to `waiting`), `in_flight_streaming`, `end_in_flight`, `clear_in_flight`, and `in_flight_rows` for readers.
- [ ] `RequestRow` gains `started_ms`, written by `record_request`; `DisplayRow` and `display_rows_since` in `src/store/ledger.rs` carry `started_ms`, `requested_model` and `effective_model`.
- [ ] `latest_ts_ms` gives way to a freshness read: the newest `started_ms` across `in_flight` and `requests`, falling back to `ts_ms` on rows without one.

### Server: recording the request in flight

- [ ] Extend `InFlightGuard` in `src/server/mod.rs` to own the in-flight record: begun with the session, lineage, frontend, requested model and message count, and deleted on drop.
  The record and the counter part ways on pings: `anthropic.rs` begins the counter only for non-ping `/v1/messages`, while every inference request (pings included) gets a record.
  `count_tokens` and batches get neither, as they are usage paths and not turns.
- [ ] Make sure the guard drops only after the last ledger row is written, on each path that carries it: `anthropic_target.rs` (stream end), `codex.rs`, `proxy.rs`, the gate and cold early returns in `anthropic.rs`, and `cancel.rs` (a client disconnect).
- [ ] Fold `note_binding_sent` (the four send sites in `anthropic.rs` and `inference.rs`) into one `note_attempt_sent` that also moves the record to the attempt, so every send, a fallback's included, updates it immediately before its transport begins.
- [ ] Mark the record `streaming` where a precommit outcome is `Accepted` (`anthropic_target.rs`, `codex.rs`, and the inference coordinators' accept arms).
- [ ] Clear `in_flight` in `Server::serve` before the listener accepts.
- [ ] Set `started_ms` on every row from `RecordCtx::started` (taken as epoch ms when handling begins, beside the `Instant`).

### Server: model identity on proxy rows

- [ ] `record_quota_blocked` in `src/server/record_anthropic.rs` takes the requested and effective model from its caller in `anthropic.rs`.
- [ ] Audit the other kinded rows for the same gap and fill what the caller knows: cold, cold-recap and cold-quiet (`record_anthropic.rs`, `record.rs`), and the route-unavailable and preparation-error rows (`unsent_row` in `record.rs`), which carry the requested model but leave the effective one empty where nothing was routed.

### Dashboard read path

- [ ] `read_window` in `src/tui/mod.rs` also reads `in_flight_rows`, and `DisplayWindow` keeps them; a stage change moves `data_version`, so the existing cadence re-reads.
- [ ] `model::aggregate` in `src/tui/model.rs` takes the in-flight rows.
  - Seed `SessionAgg` from every row naming a session, of any kind, and from every in-flight row, ahead of the measurements-only pass.
  - `requests` counts logical requests (groups, an ungrouped row as one, an in-flight group with no rows yet as one), not measurement rows.
  - New fields: the in-flight stage, elapsed time and count (the longest-running shown), and a `failed` flag from the newest group to reach a final outcome, a group still in flight neither setting nor clearing it.
  - `latest_ts_ms` (the sort key) takes an in-flight request's start.
  - A main-lane in-flight request sets `req_messages`.
  - Where no row named a served model, `model` and `provider` fall back to the newest effective model from an in-flight record or a kinded row, flagged as unserved for the view.
- [ ] Sparkline buckets place a group by its `started_ms` (falling back to its first row's `ts_ms`), count in-flight requests, and clamp an arrival before the window into its first minute.

### Dashboard view

- [ ] `session_cells` in `src/tui/view.rs`: the idle cell shows `held`/`sent`/`recv` and the elapsed time (`rel_age`), with `+N` for more in flight; `Col::Idle` widens to fit it.
- [ ] `model_cell`: a `~`-prefixed, dimmed model for an unserved one, with `ctx_cell` dimmed to match; a red `✗` after `!` for a failed session.
- [ ] `LEGEND` gains the rows already in the design system, verbatim.
- [ ] Check the column shed order and the name column's floor at 80, 100 and 120 columns.
- [ ] Session detail in `src/tui/detail.rs`: one `live` line per in-flight request with stage, elapsed time, provider, effective model and attempt (first, second…, saying billed where the provider bills).

### Tests

- [ ] Store: the migration on a copy of the real store keeps every row; `in_flight` round-trips; `clear_in_flight` empties it.
- [ ] Server, against the mock upstreams: a record exists while a response streams and is gone after its row lands; a fallback moves it to the next attempt; a client disconnect removes it; a ping gets a record and no counter; a gate row carries both models.
- [ ] Model: a gated-only, a failed-only and an in-flight-only session each get a row; `reqs` counts logical requests; the failure mark sets, clears, and ignores a recovered fallback; a group still in flight with a failed attempt row is not failed.
- [ ] Model, staying quiet: a window with no in-flight rows aggregates exactly as before, and rows without `started_ms` bucket by their first row.
- [ ] View: new insta snapshots for the sessions panel with requests in flight at 80, 100 and 120 columns, and for the detail popup; run only those snapshot tests, by name, to write and review them with `cargo insta review`, since a changed existing snapshot can only be accepted locally.

### Finish

- [ ] `cargo install --path . && toker restart`, then watch a real turn appear in `toker tui` before its response ends.
