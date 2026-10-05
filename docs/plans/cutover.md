# Cutover from claude-token-proxy

Date: 2026-10-05

Swap toker in for claude-token-proxy (ctp) as the live proxy on this machine,
then fix the remaining parity gaps while using toker for real.
Sources are the parity review against ctp: its `docs/internals/` lessons, its
request path, and a side-by-side render of `live.mjs` and the TUI from the
same imported log.

The work falls into four gates.
A blocks the swap.
B must land before the next morning timer slot, because the swap disables
ctp's timers.
C is the rest of what we agreed, done after the swap.
D is not expected today.

## Gate A: before the swap

1. **Setup creates the state dir.**
   `units_step` (`setup/wizard.rs:1295`) writes `ReadWritePaths={state_dir}`
   but never creates it, and systemd fails the namespace setup with
   `226/NAMESPACE` before `Store::open` could.
   `create_dir_all(state_dir)` before installing the units, reported like
   the other non-fatal failures.
2. **Mid-body upstream failure aborts the client stream.**
   The observed streams map an upstream error to `Poll::Ready(None)`
   (`server/anthropic.rs:1230`, `server/proxy.rs:690`; `codex.rs:341` sets
   `done`), and `buffer_up_to` (`server/proxy.rs:600`) forwards a truncated
   body as complete.
   Change the item type from `Infallible` to an error so hyper aborts the
   response, as ctp's `res.destroy()` did; a truncated buffered body
   becomes a 502.
3. **Inter-chunk idle timeout upstream.**
   Wrap the upstream body streams in a 300 s timeout between chunks, which
   leaves long healthy streams alone but frees the in-flight hold (and the
   sleep lock) on a stall.
   Correct the module docs at `server/mod.rs:26-32` and `:89-90`.
4. **Unmatched paths pass through.**
   Add a router fallback forwarding to the default anthropic backend
   (`server/mod.rs:309`), as ctp forwarded everything but its control path.
   `GET /v1/models` must not carry an anthropic credential to openrouter
   (`server/proxy.rs:325-368`).
5. **Freshness in the TUI header.**
   ctp's "last req Ns ago" (`live.mjs:235-243`): age of the newest ledger row
   of any kind, from a cheap `max(ts_ms)` query rather than the window rows;
   green under 30 s, yellow under 300 s, red beyond, red "no data" when the
   ledger is empty.
6. **Drop `toker report`.**
   Remove the subcommand (`main.rs`), its stub, the plan's report section
   (`toker-toolsuite.md:158`), and any README mention.
   The quota fit stays: the cold gate's outlook uses it.
7. **Import faithfulness.**
   Count, rather than silently drop, ctp fields the importer does not map
   (`import.rs:434-526`), including the early `compacting: true` rows.
8. **Swap.**
   Run setup (listener verified before any settings are patched), re-import
   to close the gap, disable ctp's `claude-token-hold.timer` and
   `claude-token-ping.timer`, and leave `claude-token-proxy.service` running
   so rollback is only pointing the settings back.

## Gate B: before the next morning slot

Align toker's schedule to ctp's, not the reverse: hold at 07:20 and 12:20,
ping at 07:31 and 12:31, weekdays.

1. **Wake service.**
   `toker-wake.timer` has no `Unit=` and no `toker-wake.service` exists
   (`setup/wizard.rs:398-417`); write a `/bin/true` service as ctp did.
2. **Skip when a window is open.**
   ctp's `decidePing` (`ping.mjs:61-74`): no ping while any response row
   reports a 5-hour reset in the future (`timers.rs:354-480`).
3. **Cheap ping.**
   `--model haiku --strict-mcp-config`, cwd a temp dir, 120 s timeout
   (`timers.rs:406-413`, unit at `setup/wizard.rs:519-532`).
4. **Boundary from the fire time, read back.**
   `window_boundary_ms` floors the slot, not the fire time
   (`timers.rs:249`); after the ping, read `reset5h` from the ledger and
   record whether it matched, as `ping-window.mjs:146-169` did.
   Readback considers only rows from the anthropic subscription backend.
   Fix the README arithmetic.
5. **Header merge.**
   Append to `ANTHROPIC_CUSTOM_HEADERS` rather than overwrite it.
6. **Timer accuracy.**
   `AccuracySec=1s` on the ping timer, `10s` on hold.
7. **Hold on wall-clock time.**
   Tick against the wall clock and exit when the lock dies
   (`timers.rs:275-298`; ctp `hold-awake.mjs:37-43`).
8. **Wizard defaults** to the ctp slots above.

## Gate C: after the swap

### TUI

1. **Main lane.**
   Session rows take ctx, model, prompt now, msgs, compactions and idle from
   the session's main lane, as `cold.mjs:427-439` chooses it; requests and
   output stay session-wide.
   Add `tools_hash` to `DisplayRow` (`store/ledger.rs:591`); rework
   `aggregate` (`tui/model.rs:353`); correct the doc at `tui/model.rs:78-92`.
2. **Small terminals.**
   Fixed sections first, then the two lists share the remainder with a floor
   of one row each and a "… N more" line; borders go before data; TOKENS
   never clips its hit-rate pair; slack goes under the content
   (`tui/view.rs:188-247`, `:323-336`).
3. **Bottom strip.**
   RATE & QUOTA takes the full width; SPEND renders only when it has billed
   data; the bar shrinks before reset clocks drop; fix the missing gap at
   `tui/view.rs:1364`.
4. **Sessions table.**
   Label column stays (titles over ids); an unlabelled session shows 8 chars
   of id; numbers right-aligned; `claude-` stripped from model; compactions
   header `↺`; columns sized to their data; shed OUT, peak, compactions, msgs
   before idle; counts without separators, tokens with.
5. **Requests line.**
   One line: sparkline across the whole window with dim `·` for empty
   buckets, rate, and errors and drift only when non-zero.
6. **Refresh.**
   1 s tick; re-query only when `PRAGMA data_version` moves; quota and
   rebuilds refresh on new data, at most every 10 s.
7. **Locale.**
   Times from `LC_ALL` > `LC_TIME` > `LANG`, numbers from `LC_ALL` >
   `LC_NUMERIC` > `LANG`, skipping `C` and `POSIX` (ctp's `fmt.mjs`), through
   ICU4X; display only, never notices; one formatter replacing both
   `grouped()` copies.
8. **Nits, legend, colour.**
   "≥3%" not "≥+3%"; spent label alignment; dim lowercase column headers;
   "1 req"; a `?` legend overlay; honour `NO_COLOR`.
9. **`custom-title` labels.**
   Highest rank, plus the 256 KiB head scan (`transcript.mjs:43-86`,
   `tui/labels.rs:233-283`).

### Notices

10. **Per-frontend styles.**
    The frontend comes from a `/f/<frontend>` base-URL prefix that setup
    writes into each patched settings file; the router strips it.
    Styles: GFM alert by default (`[!CAUTION]` for the quota block,
    `[!WARNING]` for the cold notice); `> [!TOKER]` for claude via
    Workhorse; a backticked `★ Toker` block, 50 columns frozen, for plain
    claude; plain on opt-in.
    Configured per frontend in a `[notices]` table.
    Verify must probe a prefixed path before setup patches anything.

### Setup and config

11. **Optional backends.**
    A `[providers.X]` block's presence enables it; provider fields become
    `Option`; drop the openrouter requirement at `server/mod.rs:139`;
    a protocol with no backend answers a clear not-configured error.
12. **Wizard multi-select** of backends, pre-ticked from detection, with
    auth and defaults asked only for those ticked.
13. **Auth choices** per key backend: frontend brings its own
    (pass-through), stored by toker (keyring, falling back to the 0600
    toml), or an env var with an explicit note that it must be in the
    systemd user environment.

### Promote

14. Grant only days the ledger already holds; clear the bar with
    `floor(needed) + 1`; raise the prompt ceiling to the family's best by
    default; report the bar and days; work offline when nothing listens
    (`middleware/models.rs:287-316`, `cmds.rs:210-266`).

### Request path

15. Quota-block notice and row carry `contextTokens` from the lane
    (`server/anthropic.rs:371-408`).
16. Batch model-map provenance recorded (`server/anthropic.rs:800-824`).
17. Upstream failure answers a 502 with an anthropic-shaped JSON error.
18. Merged system messages prefixed `[system]` again (`middleware/cold.rs:1969`).
19. Tool-less requests get a lane from the empty tool list's hash, as ctp
    did (`ir/anthropic.rs:207-211`, `middleware/lanes.rs:136`).
20. The served model is marked from the response, not only before sending
    (`server/anthropic.rs:843`).
21. Cold outlook reads only the routed backend's meters
    (`middleware/cold.rs:1661`).
22. Compaction retarget and outlook price the mapped model; drop the stale
    "no host model map yet" comments (`middleware/cold.rs:1030`, `:1995`,
    `middleware/models.rs:263`).
23. OpenAI route: summarising exemption, a `cold-quiet` row when withheld,
    and no `/compact` promise it cannot keep (`server/proxy.rs:196-246`).
24. Error rows keep `rateLimits` on 429s.

## Gate D: later

- `toker export`: implement, and fail rather than exit 0 until then.
- `watch-context-window` equivalent.
- Console quota events (80/90/100 % crossings, binding meter changes).
- `systemChange` computed at capture time instead of storing ladders on
  every row.
- Allowance pruning.
