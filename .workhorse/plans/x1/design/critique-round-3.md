# Critique round 3: the converged set, fresh eyes

Read: `design-judgement.md`, `design-system.md`, `brief-converged.md`, the operator's addendum in `brief.md`, and the taste ledger in `log.md`.
Seen: `REF-dashboard-live.png`, `NOW-presentations.png`, `NOW-routing-status.png`, and all 30 PNGs in `shots/converged/`, at full size.
The HTML was opened only to settle two emphasis questions the screenshots left ambiguous (the trace's `caught` weight, the settings' dim values).

## 1. Verdict

The structure has converged: one grid held at four widths, the tab strip as the chain, the detail sized to content, no JSON, no sentence on the page, validation beside the field, and the design reads as the dashboard's sibling.
What stands in the way is five meaning-level disagreements, none structural: the rewrites `reach` column says `off` on rules that are serving traffic, the route page gives the digit keys a second meaning, the keys overlay offers two ways to reorder and drops a row with `enter`, the settings page dims every value including `on`, and the providers table does not colour the one fault the page opens on.
No criticals, five suggestions, so not yet a clean round by the stop rule's count, but each fix is a cell or a line, not a redraw.

## 2. Per frame

### Rewrites at rest (01-01 at 120x40, 01-02 at 135x50)

Five seconds: the reversed row, then the yellow `no cred` and `nothing catches it`, then the three bold scope heads.
That is the right order for a table someone scans for faults.
Blur: the rail, the three groups and the reversed bar survive; the chrome is two boxes.
Greyscale: yellow at 8.69:1 flattens to near-white, so `no cred` reads as plain and `off` still reads as dim; the hierarchy holds, as the design system promises.
Colour inventory: yellow for no credential and for the uncaught else, dim for off and for positions, bold for scope heads and the detail's first line, reversed focus, the two header chips.
Nothing is decorative.
Alignment: the `route` heading sits over the names, not the arrow; `hits` right-aligns against the box; the detail's label column shares one edge.
Subtraction: the per-row `→` repeats what the `route` heading says, but it is the brief's chosen notation and it carries the else rows; leave it.
Against the reference: panel titles, header chips, dim column headings and right-aligned counts match the dashboard.
Against the operator: ordered rules, several tables, the order visible as the semantics.
At 135x50 nothing stretches to fill; the ground below RULE is bare, as call 7 asks.

### Trace (01-03)

Answer first in bold, then nine numbered links, the table above dimmed except the rows the request touched.
Blur: the two bright rows in the table and the bold `goes to` line survive, which is the trace's point.
Two small lapses: link 4 (`route`) carries a bare `→` in the verdict column where every other link has a word, and `> serves` is bold while `> caught` is not (the HTML confirms `caught` is only reversed).
The table's row 2 is bold and the trace's row 2 is reversed; one is the mark and one is focus, and they do not fight.

### Editing a rule (01-04) and the save flow (08-01 to 08-08)

The inline form under row 6, the refusal in red beside the field it concerns, `fix sends to save` dim in place of `save`, the PREVIEW naming the three requests that would move.
This is the operator's "validation happens live on the interface" drawn exactly.
The four states agree with each other and state 4 agrees with 01-05.
Two things to note.
The `anthropic_messages` head says `6 rules` while the tab says `rewrites 14`: the head counts the unsaved form as a rule.
The empty placeholder row reads `prefix -  → -  -  -`, spending `-` (defined as "not recorded, which is not zero") on "not yet typed".

### After save (01-05)

`saved` bold, the lane clause dim, `claude settings not synced` yellow, the tabs collapsed to numbers to make room, exactly as the design system orders shedding.
The table has grown `kind` and `sends` columns and the scope heads have lost their uniform clauses: the uniform-columns rule applied correctly.
The RULE detail for `claude-*` does not say `sends  as asked`, which is the one fact that distinguishes this rule and the thing the edit refused a moment earlier; the `example` line implies it.

### Routes (02-01), the route page (02-02), adding a fallback (02-03)

The routes table merges `rules` and `defaults` into `matched by`, which is better than the brief's two columns.
The ROUTE detail's edge line (`↓ when unavailable before send · overloaded · 5xx   billed: allow   ambiguous retry: deny   cache: preserve warm`) is the best single line in the set: dim labels, plain values, one yellow where money is.
The route page's three bold section heads read downward as the operator asked.
The fallback form: `codex_sub is off: skipped until it is on` dim as a reading, `codex_responses does not offer claude-sonnet-5` red as a refusal, one control line.
The route switcher `1 2 3 4 5 6 7` is a second strip of reversed digit chips on a screen whose header already has one; see finding 2.
`sent to  3 candidates` counts the unsaved form, as the rewrites head does.

### Presentations (03-01), defaults (03-02)

Presentations is A1's table fix landed: `include`, `except`, `as`, `lists`, `newest listed`, with the slug and route said once in the scope head and `-` where there is no except.
The detail's `overlaps  row 6 z-ai/glm-*-flash  kept apart by except *-flash*` is a reading the current product cannot give.
Defaults is five rows and a four-line detail; it carries no position column and no `[m] move`, which is right for an unordered table.

### Models (03-03, 03-04)

Four stacked panels, the first three sized to content, OFFERS taking the rest with `… 180 above` and `… 246 more` (180 + 14 + 246 = 440: the arithmetic holds).
Focus moves panel to panel and only the focused panel shows a selection and a control line (call 11 honoured).
Three nits: UPGRADES rows repeat `newest on` five times under a head that already says `newest`; `[p] promote` is a control nothing in the set explains; COMPACTION's wording differs from the route page's for the same fact.

### History at rest (04-01) and armed (04-02)

Newest first, `live` bold on rev 10, `2 warm` on rev 9, `none` dim elsewhere.
Rev 1's `setup: 7 routes, 4 defaults, 13 rewrites, 14 presentations, 4 families, 4 upgrades` plus the nine single-record revisions reproduce every count in the tab strip (15, 15, 4, 7, 5+5+1), which is the kind of consistency the brief asked for.
`[a] already live` dim with the reason in place of the label is the design system's unavailable-control rule.
Armed: the control inverts and the last line says what will be lost, in yellow, and how to confirm.
Rev 1's row runs to one space short of the `lanes` column; it is the one exception row and it fits.

### Providers (05-01) and settings (05-02)

Providers: off rows dim as whole rows, the problem row selected, the detail's `credential  none` yellow.
But the table's own `none` on the `on` row is plain, so the table does not show what the page opened on; see finding 5.
Settings: four rail groups, the refused field reversed with the refusal beside it, one control line.
Every value is dim, including `on`; see finding 4.
The header's right side is empty here where providers shows `2 providers on`.

### Narrow (06-01 to 06-04)

100x24 and 80x24 rewrites: ruled panels, the full table in 24 rows, the detail replaced by a one-line summary with `[pgdn] more` (call 14's `pgdn`).
80x24 sheds `anthropic_sub is on with no credential` and keeps `no cred`: the right clause goes first.
100x24 route page: the edge line wraps to two lines on the same indent and everything else holds.
80x24 providers: `bindings` shed; but the off rows are no longer dim as rows (only the `off` cell is), and `refresh url` reads `(the built-in one)` where 120x40 reads `built-in`.

### Empty (07-01) and the keys overlay (07-02)

Empty: `no policy yet` in the header, three groups at `0 rules` each ending in its yellow else row, and `[n] needs a route first   [4] go to routes`, which is an unavailable control with its reason and a way out, with no sentence on the page.
The overlay: KEYS left, MARKS right, every mark the frames use listed except `as asked`.
It also lists `1-6 jump to a tab` and `1-7 switch route`, and `m lift; ↑/↓ move, enter drops it` beside `alt+↑/↓ move the row one place`; see findings 2 and 3.

## 3. Cross-frame consistency

Header wording holds: `rev 9 live` at rest, `editing · rev 9 live` in an edit, `saved · rev 10 live · 2 lanes on rev 9` after the save, `rev 10 live` on history, `no policy yet` when empty.
The config page's status (`2 providers on`) is present on providers and absent on settings, and the settings edit does not say `editing`.

The two moments are consistent with themselves.
Before: rev 9, 14 rewrites, claude-sub with `2 warm · 5 cold` lanes, openrouter-chat `3 cold`.
After: rev 10, 15 rewrites, `8 lanes on rev 10 · 2 warm lanes still on rev 9`, and 8 + 2 = 7 + 3.
The `≥3` on the anthropic_messages default is the same on the rewrites else row, the defaults table and the default's detail.
The provider `used by` counts (4/2/1 for anthropic_sub, 3/2/5 for openrouter, 3 and 3 and 1 for the off ones) all reproduce from the rewrites and routes tables.

Tab strip: identical in every routing frame of the same moment, collapsed to digits at 80 columns and when the after-save status needs the room.

Panel titles: `REWRITES`, `RULE`, `TRACE`, `PREVIEW`, `ROUTES`, `ROUTE`, `PRESENTATIONS`, `PRESENTATION`, `DEFAULTS`, `DEFAULT`, `FAMILIES`, `UPGRADES`, `COMPACTION`, `OFFERS`, `REVISIONS`, `REVISION`, `PROVIDERS`, `PROVIDER`, `SETTINGS`, `KEYS`.
Fixed, never a record name.

Control lines: `[e] edit  [n] new  [m] move  [space] turn off  [d] delete  [t] trace  [enter] open route` on RULE and PRESENTATION; DEFAULT drops `[m]`; the ROUTE detail alone puts `[enter] open route` first.
Every form saves with `[ctrl+s]` and the dim reason replaces `save` while invalid; `[esc] cancel` everywhere.

Focus: one reversed row per screen in every frame, including the models tab with four panels and the trace with its table.

The one place frames disagree in meaning: the rewrites `reach` column says `off` (dim) for `→ codex` while the routes table's `now` says `off · billed fallback` (yellow) for the same route, and the rule showing `off` carries `≥2` hits.

The table reshapes between frames as the uniform-columns rule dictates (`prefix → ${model}` in the head at rest, `kind` as a column while editing, `kind` and `sends` as columns after the glob rule lands).
This is the rule working, not a fault, but it means the columns move under the operator twice in one edit; noted for the build, not scored.

## 4. Ranked findings

1. **Suggestion.** Frames 01-01, 01-02, 01-05, 06-01, 06-02, against 02-01.
   The `reach` column reports the first candidate, not what the request gets: `codex_sub/${model} → codex` reads `off` in dim, which means inactive, while the routes table says codex is `off · billed fallback` and the rule has `≥2` hits.
   An operator scanning for dead rules would read three live, billed rules as dead.
   Change: make `reach` say what `now` says for the route, `off · billed fallback` and `no cred · billed fallback` in yellow, with `off` dim only when every candidate is off; at 80 columns shed to the first clause as the summary line already does.

2. **Suggestion.** Frames 02-02, 02-03, 06-03, 07-02.
   Digits mean two things on the route page: the overlay lists `1-6 jump to a tab` and `1-7 switch route`, and the screen shows two strips of reversed digit chips, the header's and the switcher's.
   The design system wants a key to do the same thing on every surface.
   Change: drop the digit switcher so digits always mean tabs; while a route page is open its tab is pinned, so `←/→` and `h/l` step through the routes there, and a dim `1 of 7` sits where the digits were.

3. **Suggestion.** Frame 07-02.
   Reordering has two mechanisms, `m lift; ↑/↓ move, enter drops it` and `alt+↑/↓ move the row one place`, and the first drops a lifted row with `enter`, which call 14 reserves for opening the route.
   Change: one mechanism, `m` lifts, `↑/↓` move, `m` again drops (the shape `d again to confirm` already teaches), `esc` puts it back; cut `alt+↑/↓`.

4. **Suggestion.** Frame 05-02.
   Every settings value is dim, `on` included, while the providers table draws `on` plain and `off` dim and the design system defines dim as absent, inactive or subordinate.
   A gate that is on reads as off.
   Change: values plain; dim only `none` and the two `fixed here` rows, which are set elsewhere and not editable here.

5. **Suggestion.** Frames 05-01, 06-04.
   The providers table opens on the problem but does not show it: `anthropic_sub  on  none` has `none` plain, the same as `keyring`, while the detail below colours it yellow and every other table colours `no cred` yellow.
   Change: `none` yellow on a provider that is on; the off rows keep it dim.

6. **Nit.** Frames 01-04, 02-03, 08-01 to 08-08.
   `6 rules` and `3 candidates` count the unsaved form while the tab strip keeps `rewrites 14` and `routes 7`.
   Change: heads count saved records until the save; the form row is visibly extra.

7. **Nit.** Frame 01-03.
   Link 4 reads `route  →  claude-sub` with a bare arrow in the verdict column where every other link has a word.
   Change: `4 route  claude-sub`, as the brief writes it.

8. **Nit.** Frame 01-03.
   `> serves` is bold and `> caught` is not; they are the trace's two hits.
   Change: bold both.

9. **Nit.** Frames 05-01 against 06-04.
   `refresh url` reads `built-in` at 120 columns and `(the built-in one)` at 80.
   Change: `built-in` at both.

10. **Nit.** Frames 05-01 against 06-04.
    Off providers are dim as whole rows at 120 columns and only in the `off` cell at 80.
    Change: dim the row at both widths.

11. **Nit.** Frame 02-01.
    The ROUTE detail's controls start with `[enter] open route`; every other detail ends with it.
    Change: `[e] edit  [n] add fallback  [t] trace  [enter] open route`.

12. **Nit.** Frames 05-01, 05-02.
    The config page's status `2 providers on` shows on providers and vanishes on settings, and the settings edit lacks the `editing ·` prefix the routing edits carry.
    Change: the status is the page's, drawn on both tabs, `editing · 2 providers on` during the edit.

13. **Nit.** Frames 03-03, 03-04.
    UPGRADES rows begin `newest on` five times under a head that already says `newest`, which is a uniform column in words.
    Change: rows begin at the route, `claude-sub  claude-opus-4-7 → claude-opus-5`.

14. **Nit.** Frames 03-03, 07-02.
    `[p] promote` is a control whose effect appears nowhere in the set, and the overlay's `promote the selected upgrade's newest model` does not say what changes.
    Change: cut it from the mockups, or name its effect in the overlay in the vocabulary the rest uses (`make the newest model the family's default`, if that is what it does).

15. **Nit.** Frames 03-03 against 02-02.
    The same fact reads `a cold compaction  any model  → sonnet` on models and `compaction  cold compactions → sonnet` on the route page.
    Change: one wording, the route page's.

16. **Nit.** Frames 01-05, 07-02.
    `as asked` is a sends value in the table and the diff but is not in MARKS, where `${model}` is.
    Change: add `as asked  the name is sent unchanged`, the wording the fixed state already uses.

17. **Nit.** Frame 01-05.
    The `claude-*` RULE detail omits `sends  as asked`, the row's one distinguishing fact.
    Change: add the row, or fold it into `match  glob claude-* · sends as asked`.

18. **Nit.** Frames 08-01, 08-05.
    The new-row placeholder spends `-` on "not yet typed" (`prefix -  → -  -  -`), where `-` is defined as "not recorded, which is not zero".
    Change: leave the untyped cells blank in the placeholder row; the form beneath says what is missing.

## 5. Counts

Critical 0 · suggestion 5 · nit 13.

## 6. Backlog

Not findings against these mockups; each is a feature or a frame nobody asked for.

- `[enter] open route` on a provider used by five routes (openrouter): which one opens, or a picker.
- The `d again to confirm` armed state is described in the overlay but no frame draws it.
- Whether presentations are ordered: the trace says `no slug equals it` and the overlap reads `kept apart by except`, so nothing in the frames shows order mattering, yet PRESENTATIONS carries position numbers and `[m] move`.
- The keys overlay at 80x24, where two columns will not fit.
- A trace started from a presentation or a default, to confirm the gutter marks read the same on those tables.
- The mid-edit column reflow (section 3, last paragraph): worth a filmstrip once built, to see whether the columns moving under the cursor is felt.
