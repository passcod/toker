# Redesign the routing and configuration pages

The design is converged in `design/` (start with `design/how-it-works.md`), and the specs carry it.
The mockups are `converged-01` to `converged-08` under the card's mockups folder; each step below names the frame it builds towards.

## Changes underneath the screens

The spec review settled four changes to how Toker works, which the implementation has to make beside the new pages.

- The draft is retired.
  The draft replace, draft edit and activate control operations go, with the draft table's writers; a save (edit applied to the active revision, activated for new and cold lanes) and a reactivation are the only ways a revision becomes active after the first.
  The draft table stays in the schema, since migrations are append-only, but nothing reads or writes it.
- The deliberate import of `toker.toml` routing goes, with `src/tui/routing/import.rs` and its entry in the routing view.
  First-revision creation in setup and at startup is unchanged apart from no longer copying into the draft.
- Provenance records the resolver's facts and the default that matched, through a new append-only migration.
  Rows written before it are not replayable; nothing backfills them.
- A provider prefix naming a disabled provider walks its rewrite's route like any other, so a fallback can serve it.

The README's routing paragraph describes the draft, validate and activate flow, and needs rewriting once the pages ship.

## Notes from reading the code

- **The prefix change is mostly already true.**
  The resolver has no special case for a disabled prefix; the behaviour comes from revision one giving each provider prefix a rewrite onto a route with that provider as its only candidate (`policy/legacy.rs` `add_explicit_prefixes`), so a disabled candidate (`SkipReason::Disabled`) leaves nothing to fall to.
  What is left is a resolver test proving a prefix rewrite whose route has a fallback is served by it, and that one without is refused rather than sent to a default.
- **There is no save and no widen in the backend today.**
  Activation already does the right compare-and-swap inside one immediate transaction (`store/state.rs` `activate_policy_revision`), checking the draft generation and the active revision; save reuses that with the draft check dropped.
- **A save needs a new revision source.**
  `routing_policy_revisions.source` has a CHECK allowing only `legacy`, `draft` and `reactivation` (v9), so recording `save` means rebuilding the table the way v9 did, triggers included.
  Revisions written as `draft` stay as they are.
- **Widening lives on the policy state, not the revision.**
  A revision is never edited, so the widened scope and consent go in new columns on `routing_policy_state`, cleared by the next activation, and the pin read (`policy/transition.rs`) takes the widened scope where one is set.
- **Provenance already records more than it looked.**
  The trace's first event is `Lookup{source, presentation_id, rewrite_id}`, whose source distinguishes profile from protocol and presentation from rewrite from default.
  Missing are the profile itself, a column the hit count can read for a default, and most of the resolver's facts (prompt bound, counts, capabilities, affinity, class); `requested_model`, the protocol and the safeguards ask (v21) are already columns.
- **The config data-loss bugs are both the TUI sending part of a row.**
  `tui/config/model.rs` has no `session_headers`, `transcript_roots` or `refresh_url`, so `settings_body` and `provider_body` omit them, and the server writes the whole row with them cleared (`server/control/config.rs`).
  The fix is the TUI carrying every field it read, plus a server test that an edit leaves fields it did not send unchanged.
- **The routing page is rebuilt rather than restyled.**
  Today it has four tabs listing records as one line of JSON each, with a JSON fallback in the detail pane and validation behind `V`.
  Keep the control client, the conflict handling, `reconcile.rs` (picker sync after activation) and `template.rs` (smallest record of each kind); keep `form.rs`'s typed schema, completion and reordering, and change how it is drawn and checked.
- **The header buttons are designed but not built.**
  `chrome::header` draws `?` and a single `⇄` chip, and tabs collapse to numbers then clip; the design guide already describes the page buttons and the scrolling strip.

## Checklist

### Fix the config edits that drop fields

- [x] `tui/config/model.rs`: carry `session_headers` and `transcript_roots` in `Settings`, and `refresh_url` in `Provider`, from `parse_snapshot` through `settings_body` and `provider_body`.
- [x] `server/control/config.rs`: test that a settings edit and a provider edit leave the fields they carry unchanged, round-tripped through the snapshot.
- [x] Record the two triaged issues as fixed against their commits.

### Store

- [x] Migration: rebuild `routing_policy_revisions` to allow `save` as a source, recreating its triggers as v9 did; add `PolicyRevisionSource::Save`.
- [x] Migration: add the widened scope, its force consent and when it was widened to `routing_policy_state`.
- [x] Migration: add to `requests` the profile, the matched lookup source and scope, and the resolver facts that are not already columns, with a marker that this row records them, so hit counts and replay can tell an older row from one with nothing in it.
- [x] `server/record*.rs` and `store/ledger.rs`: write those columns on every policy-routed row.
- [x] `store/state.rs`: stop `activate_policy_revision`, `activate_revision_one` and `retire_generated_revisions` writing the draft columns; remove `compare_and_swap_policy_draft` and `clear_policy_draft`; drop the draft fields from `load_routing_policy_state` and its callers.
- [x] `store/state.rs`: a query counting the last 24 hours' requests by presentation, rewrite and default, per record identifier across revisions, returning floor or not recorded where the window reaches rows older than the marker.
- [x] `store/state.rs`: a query listing recent requests with the facts preview needs, each flagged replayable or not.
- [x] Check `requests` has an index serving a 24-hour window, and add one if not.

### Control operations

- [x] `server/control/routing.rs` and `server/mod.rs`: `POST routing/save`, carrying the expected active revision, an edit and an optional description; applies the edit to the active document, compiles, and activates with `new_and_cold_lanes` and source `save`; a moved revision answers a conflict naming the live one.
- [x] `POST routing/widen`, carrying the expected active revision, the scope and force consent; refuses a narrowing; goes through `require_cache_consent` for immediate.
- [x] `policy/transition.rs` and `policy/pinning.rs`: read the widened scope and consent where set (RPINS).
- [x] `POST routing/validate`: compile an edit against the active revision (the body carries the edit and the expected active revision), returning diagnostics with their paths.
- [x] `POST routing/preview`: resolve against the active revision, an unsaved edit or a kept revision; take invented facts or a recorded request's identifier, answering not replayable for a row without the facts.
- [x] Preview over recent traffic: for an unsaved edit, replay the last 24 hours' requests it bears on under the active revision and under the edit, returning each changed one with both outcomes and a count of unreplayable rows.
- [x] `POST routing/reactivate`: carry the expected active revision, activate with `new_and_cold_lanes`, stop writing the draft.
- [x] Remove `PUT draft`, `PATCH draft` and `POST activate`, their handlers and the `Server` methods behind them; the snapshot drops `draft`, `draft_generation` and `draft_diagnostics`.
- [x] Resolver test for a disabled-provider prefix with and without a fallback (RSOLV).

### Facts the page reads

- [x] `server/control/derived.rs`: each rewrite's, default's and presentation's reach, as one of the five outcomes UTUI lists, by walking its route against the snapshot's availability, naming the edge or setting that stopped it.
- [x] Hit counts for the last 24 hours, from the store query, with floor and not-recorded states.
- [x] Shadowing: for each rule, the earlier presentations, profile rules and same-scope rules that catch all or part of what it matches (prefix and exact by comparison, glob and regex against the offers they would catch).
- [x] Per route: what refers to it, the lanes it holds and its reach; per revision: lanes pinned to it.
- [x] Per upgrade policy: the evidence bar and the move it would make now; compaction's target and bindings.
- [x] Per provider: the presentations, rewrites, defaults and routes reaching it through any offer or candidate, fallbacks included.
- [x] `policy/diagnostic.rs`: a sentence and a fix for each diagnostic code the editor can raise, for the form to show beside the field.

### Config gate check

- [x] `server/control/config.rs` `commit`: refuse a `cold_min_tokens` larger than every context window the enabled bindings resolve (`catalog::windows::resolve_context_window`), naming the largest; accept it where none resolves (GCOLD).

### Header (every page)

- [x] `tui/chrome.rs`: replace the `⇄` chip with a button per page, padded and spaced a cell, current page bold blue, others dark grey, each with a click rectangle; `Header` returns them.
- [x] `tui/chrome.rs` tabs: drop counts, then scroll around the current tab with `‹` and `›`, then numbers, then the current tab alone; the buttons never shorten.
- [x] `tui/mod.rs`: route a click on a page button to that page; `Tab` still cycles; drop `tab or ⇄` from every page's `KEYS`.
- [x] Dashboard and accounts headers use the same buttons; snapshots at 120, 100 and 80 columns.

### Routing page

- [x] `tui/routing/model.rs`: tabs presentations, rewrites, defaults, routes, models, history, each with its count; lookup tabs drawn as a sequence with arrows, set apart from models and history by `·`; open on the first tab holding a refused reach.
- [x] Remove the draft from the TUI's state: `generation`, `conflicted_edits` and the Conflict modal become the save's moved-revision handling, keeping the form as entered and naming the live revision.
- [x] Remove `import.rs`, `Modal::Import`, the `i` key and `State.legacy_policy`.
- [x] Rewrites table (`converged-01` rewrites at rest, 135×50): grouped by scope, profile ahead of protocol, ascending priority; group head stating shared values; rows as match → route with reach and hits; a profile group ending by passing to its protocol; the protocol group ending with its defaults, or refused where none catches.
- [x] Matchers drawn in their own notation (prefix with its capture, glob, exact), regex only for a regex.
- [x] Rule detail as a bullet list: match with a worked example, what it sends on, route and candidates, reach with its reason, shadowing, recent catches; controls on its last line.
- [x] Presentations and defaults tables (`converged-03`).
- [x] Routes table and the route page (`converged-02`): candidates with edge conditions, blocks handling it with disabled ones named dimly, inbound references; `←`/`→` steps between routes, `esc` returns with the selection kept.
- [x] Models tab (`converged-03` models, models-families): families, upgrades with evidence bar and the move now, compaction line with bindings, offers as a scrolled window; focus moves between panels at their edges; `p` promotes.
- [x] Trace (`converged-01` trace): from any routing tab, an invented request or one picked from recent requests; outcome first, then one line per lookup stage and route step; the table marks the catching row `>` and rows fallen past `↓`; an unreplayable request says so.
- [x] Editing in place (`converged-01` editing, `converged-08`): the form opens under the row, one line per field with its reading beside it; validate on each change against the active revision; refusals in red with the fix; warnings beside the field without blocking; cross-record refusals shown with the form naming the records; `ctrl+s` labelled with what is in the way while refused.
- [x] Add, remove and `m` to move a rewrite within its scope; add, remove and reorder a route's candidates (`converged-02` add-fallback).
- [x] Edit preview under the table: changed requests from the last 24 hours, now and with the change, and the unreplayable count.
- [x] Header status: live revision, lanes on earlier revisions, publication pending, failed or unsupported with its detail (`converged-01` after-save).
- [x] History tab (`converged-04`): revisions newest first with a summary and pinned lanes; detail as the diff in words; `a` reactivates, `c` traces under the selected revision, `m` widens to move running lanes, arming first and naming the lanes whose warm cache it would lose.
- [ ] Empty policy (`converged-07` empty): every protocol group drawn with its refused `else` row, and the add control pointing at routes first.
- [ ] Keys overlay (`converged-07` keys): grouped by where each key applies; every mark the page draws is explained.
- [ ] Shedding at 100×24 and 80×24 (`converged-06`): uniform columns into the group head, tabs give way, panels lose framing first.
- [ ] Snapshot tests for each frame above, replacing the 18 current routing snapshots.

### Config pages

- [ ] Providers (`converged-05` providers, `converged-06` providers-80x24): enabled or not, endpoint, credential source, endpoints count, routing dependents; open on an enabled provider that is not configured; detail with the fix on its last line; safeguards support and rejection mark per Messages binding, with clearing.
- [ ] Settings (`converged-05` settings): gates, notice style per profile, session and ping header names, idle-sleep lock, transcript roots; unset values say their fallback; port and store path shown, not editable.
- [ ] Edits in a small form under the row, checked as they change; token counts accept full figures and the `175k`/`1M` shorthand; a refusal beside the field.

### Accounts page

- [x] Check against UTUI's accounts section: every known account with number, email and last seen; unresolved credentials as their own entry; naming one; which accounts were named by the operator.

### Docs

- [ ] `README.md`: rewrite the routing paragraph and the direct OpenAI API walkthrough for save in place, without the draft, `V` or `a`.
