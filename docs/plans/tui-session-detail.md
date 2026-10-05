# TUI session detail and gate controls

Clicking a session in the SESSIONS or CONTEXT panel opens a floating window
over the frame, like the legend, with the session's fuller details and
controls for its quota gate: release it to the end of the plan (OVER), release
it into overage (BURN), or close it again.

## Decisions

- **Grant scope.** A TUI release grants ahead of time: the current 5-hour
  window's reported reset whether or not that meter is exhausted yet, and the
  7-day window only when it is exhausted. The marker keeps its exhausted-only
  rule. A release ahead of time is the point of the control (open a session
  before it is stopped), and the 7-day restriction keeps a release for the
  afternoon from quietly becoming one for the week.
- **Revoke.** A Close control deletes the session's allowances; the gate
  applies again from the session's next request.
- **BURN is two-step.** The first click or key arms it (the button reads
  `confirm burn`); a second within 5 seconds grants. OVER and Close act on one.
- **Write path.** The TUI writes through the same `Store` functions the gate
  reads, with no new `/_toker/` control path (invariant 4: prefer not adding
  one). The gate loads a session's allowances on every request, so a grant is
  live from the session's next request with no signal to the service.

## Shared release logic

A new top-level module `src/release.rs` owns granting and revoking, used by
both the server's marker path and the TUI:

- `quota::grant_ahead(meters, now)` (pure, beside `grant_for`): the 5-hour
  reset when that window has not expired, the 7-day reset only when
  `exhausted_meters` names it.
- `release::grant(store, session, release, grant, ...)`: records one
  allowance per granted meter and the `released` ledger row. The row building
  moves out of `server::record_anthropic::record_anthropic_released` so the
  server and the TUI build the same row; the server keeps its log line and
  frontend tagging. A TUI row carries `extra.via = "tui"`.
- `release::revoke(store, session, ...)`: deletes the session's allowance rows
  (new `Store::delete_session_allowances`; the ledger itself stays
  insert-only) and records a row of a new kind, `RowKind::Revoked`
  (`"revoked"`), carrying the meter snapshot like `released` does.
- `docs/internals/quota.md` and `ledger-schema.md` gain the TUI grant and
  the `revoked` kind.

## Hit testing

- `view::Drawn` gains the rows each list drew: a `Vec<(Rect, String)>` of
  screen rectangles to session ids, filled by `render_sessions` and
  `render_context` as they lay out (context entries are one or two lines, so
  the mapping is recorded, never recomputed from offsets).
- The loop's mouse handler takes `MouseEventKind::Down(MouseButton::Left)`:
  with no popup open, a click inside a recorded rectangle opens that
  session's detail; with one open, a click on a control acts, and a click
  outside the popup closes it. The existing 1000/1006 mouse mode already
  reports button presses, so no terminal mode changes.
- The legend and the detail popup are exclusive: opening one closes the
  other. `Esc` closes either.

## The detail model

A new `src/tui/detail.rs`, pure over its inputs like `model.rs`:

- Read on open and again on every display read while open: the session's
  `SessionAgg` from the snapshot, `Store::session_summary` (whole-session
  totals and span), its allowances, and the session's newest API measurement
  row through a new `Store::latest_session_row` (by the `requests_lane_idx`,
  whose leading column is `session_id`).
- Shown, each absent field said to be unknown rather than zero (invariant 3):
  - the full title (wrapped), the session id, the cwd, the frontend and the
    backend;
  - models: requested, then effective; forced (`forced_from` → `forced_to`),
    downgraded, and mapped (`model_mappings`) where the newest row says so;
  - context: prompt now and peak against the ceiling and its source,
    messages, compactions;
  - activity: requests in the window and in the whole session, first and
    last seen, token totals, billed cost where any;
  - the gate: armed or not, the 5-hour and 7-day meters, and what the
    session holds (overage or plan, per window, with its reset time), or
    nothing.

## The popup and controls

- Rendered over the frame after everything else, cleared beneath, sized to
  its content and clipped on small terminals like the legend.
- Controls in a row along its bottom, each with its key: `o` OVER, `b` BURN
  (two-step), `x` Close, `Esc` dismiss. `view::Drawn` records their
  rectangles for the click handler.
- A control that cannot act renders dim with the reason in place of its
  action: the gate is off, the session's backend is not `anthropic_sub`, no
  meter reading names a 5-hour reset yet, or (for Close) nothing is held.
- After an action the popup stays open, the loop forces a read, and the gate
  line shows the new state; a store error shows in the popup's footer line
  rather than tearing the TUI down.

## Not in this change

- Keyboard selection of sessions (arrow keys to move between rows): opening
  is by click, as asked.
- Copying the session id to the clipboard: the popup shows it in full for a
  terminal selection.
