# Design loop — settings and routing

Card X1. Rigour: Standard (cap 6 rounds; two clean rounds, the second by fresh eyes, no criticals and at most three suggestions).

## Step 0 — does the card need design?

Yes. The card asks for full redesigns of the settings and routing area, at least two alternatives, aiming for ease of use.
Both pages add and rearrange surfaces rather than follow a pattern the app already has, so this is design work and not a build.

The area in scope is two pages, not one:

- the **config page** (`src/tui/config/`), tabs `1 providers` and `2 settings`;
- the **routing page** (`src/tui/routing/`), tabs `1 presentations`, `2 routes, candidates & defaults`, `3 rewrites, families, upgrades & compaction`, `4 preview, revisions & activation`.

The card's description names "settings/routing" as one area, and the two pages are what an operator means by it: everything that is configured, as against the dashboard, which is what is observed.

## Step 1 — references

Asked the user in prose whether a product configures something comparable well. No answer yet; the app's own best surfaces (the dashboard) stand as the reference meanwhile.

## Rigs and disk

No browser rig: Toker is a terminal app. Captures come from running the real TUI in tmux against a sandbox service, and mockups are HTML grids per `.workhorse/design/mockups.md`.

| round | free on /home | notes |
| --- | --- | --- |
| 0 | 360G | google-chrome-stable present for screenshotting mockups; tmux present for capture |

## Taste ledger

| # | call | reason | before | after | verdict |
| --- | --- | --- | --- | --- | --- |
| 1 | `V` is deleted rather than improved, and its diagnostics attach to the records they concern | `RREV#draft` already validates the whole draft before storing it, so `V` adds nothing to validity; its only unique content is the advisory diagnostics | `current/routing-status-validated-100x24.png` | all five directions | kept: no spec change needed, and it removes a command and a popup |
| 2 | The draft is stated as a count of changes, not as a generation number | `draft 2` tells the operator nothing they can act on; "3 changes not live" names the action | the live header `draft 2 · …` | A `draft ≠ rev 2 · +1 ~2 · a activate`, B `revision 2 live · no changes` | kept; B's wording reads better than A's |
| 3 | Activation keeps its consent step for an immediate activation that would lose a warm cache, and loses it everywhere else | `RREV#activation` requires that consent, and the design language requires a destructive action to arm rather than fire; every other scope disturbs nothing in flight | the current activation modal | the live-apply note | kept |
| 4 | Saving a valid form puts it live, for new lanes; there is no draft in the interface and no apply key | The operator: "the entire idea was to apply live, so you don't have a validate-and-then-apply step". A save is one deliberate act per record, not a keystroke, so a revision per save is acceptable. New lanes is the scope that disturbs nothing in flight, so it needs no consent | the critique's `rev 2 live · draft +1 ~2 · [a] apply` | the converged mockups | kept over the critique's suggestion, on the operator's words. Moving running lanes onto a revision is an explicit act from history, and arms when a warm cache would be lost (call 3) |
| 5 | The design assumes the generated-only semantics are retired: no `legacy-` names, no "generated" property, records carry invented authored names | The operator: "every legacy behaviour should die". Designing a surface for a property that is to be deleted keeps it alive. The retirement itself is compiler work, recorded as needing a card | every direction keeps or dims `legacy-route-` | the converged mockups | kept |
| 6 | D is chosen, composed with E's route page as the detail level and C's chain as the trace | The fresh critique (`critique-round-1.md`): D is the operator's table, the only main frame passing the blur test, one grid held at three widths, the leanest colour inventory; its faults are polish and one missing frame. D and E are two levels of one design and C is a third | the five directions | the converged mockups | kept |
| 7 | Panels are sized to their content and the ground below is left bare; short panels stack | The dashboard never draws a box taller than what is in it. D1 has 4 empty rows only because its content happens to be 36 rows long; A1, B1 and C1 have 20 to 26 | `compare/` A1, B1, C1, D4 | the converged mockups | kept |
| 8 | No sentence on the page: no narration rows, no subtitles in title rules, no explanatory empty states | Every direction narrated, and the reference has not one such line; the lookup order is already the tab strip's arrows | every direction's table heads | the converged mockups | kept; what a newcomer needs goes in the `?` overlay |
| 9 | A revision is one record's edit, and history reads as a list of single changes | Under call 4 every save is a revision, so a revision bundling three edits describes a workflow the design does not have | the first history frame, `add rewrite, add claude-sub fallback, drop a default` | `converged-04-history.html` | kept; rev 1, written by setup, is the one exception |
| 10 | Reactivating a revision is `a make live`, not `r` | `r` refreshes on every page; overloading it on one tab would make the same key do two things | `[r] reactivate` | `converged-04-history.html` and the `?` overlay | kept |
| 11 | Only the focused panel shows a selected row | Two reversed rows on one screen leave the operator not knowing where `e` goes | Models with both UPGRADES and OFFERS selected | `converged-03-tables.html` | kept |
| 12 | Rules are not named; a rule reads as `match → route` everywhere | The form has no name field and call 5 retired generated names, so a name could only be derived; deriving it for every rule keeps one shape | `strip anthropic_sub/` beside a derived `claude-* → openrouter-messages` | round-2 fixes | kept |
| 13 | History and the after-save frame show the same moment: rev 10 live, two warm lanes left on rev 9 | The lane counts disagreed between three frames; one moment drawn everywhere makes every count checkable | `critique-round-2.md` #10 | round-2 fixes | kept |
| 14 | Every form saves with `ctrl+s`; `enter` opens the route and nothing else | One key per meaning: settings saved with `enter` and the 24-row summary opened the detail with it | `critique-round-2.md` #3, #4 | round-2 fixes | kept; the detail at 24 rows opens with `pgdn` |
| 15 | `reach` says what a request gets, not whether the first candidate is up | A rule whose first candidate is off but whose fallback serves read as dead in dim `off` while it carried hits; the routes table already said `off · billed fallback` | `critique-round-3.md` #1 | round-3 fixes | kept |
| 16 | Digits always mean tabs; the route page steps between routes with `←/→` and shows `1 of 7` | A second strip of digit chips gave `1`-`6` two meanings on one screen | the route switcher `1 2 3 4 5 6 7` | round-3 fixes | kept |
| 17 | One way to reorder: `m` lifts, `↑/↓` move, `m` drops, `esc` puts it back | Two mechanisms, one of which dropped with `enter`, which call 14 reserves | `m … enter drops it` and `alt+↑/↓` | round-3 fixes | kept |
| 18 | A gap with no default reads `refused · no route`, not `nothing catches it` | The user asked for it to say plainly that Toker refuses the request | `→ nothing catches it` | `→ refused · no route`, still yellow: a standing condition like `no cred`, while red stays for a form refusing a value | user's call |

Observations held for the critique rather than settled here:

- **Every direction so far wastes most of the terminal.** A, B and C all leave 20 to 30 of 40 rows empty on their main frames, and the operator's terminal is 50 rows. The current design does this too, so it is not a regression, but no direction has taken the space. The dashboard, by contrast, fills its height.
- **C's `cannot serve` and `no candidate` are reversed red blocks**, which makes them the loudest thing on the screen. The design language is explicit that colour is a de-emphasis channel here and that red is quieter than yellow, so this inverts the house hierarchy.

## Step 1 continued — the live instance

The user gave access to their running `toker tui` in tmux pane 4:0.0 at 135x50, to drive without killing or misconfiguring the proxy.
Driven read-only: `Tab`, `←/→` and `?` only, never a key that could mutate the policy, and restored to the dashboard afterwards.
Captures are in the card's rig work directory under `live/`, plain and ANSI.
`current-state.md` beside this file is what they show.

The real store makes the screens far worse than the committed snapshots do, because the snapshot fixtures hold a handful of rows and the real one holds 1,416.

## Step 2 — direction

Three directions briefed separately from `brief.md`, which carries the common context so the stances are the only difference:

| direction | stance | structure |
| --- | --- | --- |
| A | faithful to the reference | keep the pages, views and master/detail; make every part as good as the dashboard |
| B | the most minimal | delete the two pages and six tabs for one configuration tree |
| C | fastest at the main task | lead with the resolution chain that answers "where does this go, and why"; records are reached through it |
| D | the operator's first idea | ordered rule tables, read as firewall, forwarding and mangle tables |
| E | the operator's second idea | one page per route, read downward from what it matches to where it goes |

The operator's steer arrived after A, B and C were briefed, and is appended to `brief.md` as an addendum.
It added two directions and three constraints that bind all five.

### The steer

- **There is no precedent**, so network routing is the near neighbour: AWS route tables, MikroTik firewall, forwarding and mangle tables. What to take is an ordered set of rules mapping one thing to another, possibly transforming on the way, in several tables for several stages. Toker's resolution order already is this and the current design hides it.
- **No JSON**, to edit or to read.
- **Live validation, and no validate-then-apply step.** A field says what is wrong beside itself and an invalid form cannot be saved. `V` is to be designed away, not improved. The draft and revision machinery stays internally, because lanes pin to revisions; what goes is the operator's experience of the ritual.
- **Core first, then drill down.**

A, B and C were sent the addendum mid-flight. B was additionally told to be honest about the mismatch between a tree, which expresses containment, and a rule table, which expresses ordered evaluation; C was told to show the rules a request fell past as well as the one that caught it, and to add a frame reading its chain from the route's side.

## Rigs and disk

| round | free on /home | notes |
| --- | --- | --- |
| 0 | 360G | google-chrome-stable present for screenshotting mockups; tmux present for capture |

No browser rig is leased: Toker is a terminal app, so the capture rig is tmux plus an ANSI-to-HTML converter, and the mockups are HTML grids.
Captures live in the card's rig work directory, in `live/` (the operator's instance, 135x50), `current/` (a sandbox on a copy of the real store, at 100x24, 120x40 and 80x24) and `empty/` (a fresh store).
The scripts are in `scripts/` there: `matrix.py`, `empty.py`, `ansi2html.py`, `shot.py`.

**The captures carry real account email addresses**, because they come from a copy of the real store.
Toker is public, so nothing from them goes into the repository without invented names in place of the real ones.

All five are drawn: 76 frames in 19 files, screenshotted to PNG frame by frame with `split_shot.py` in the rig work directory, and 43 of them curated into `compare/` as a matched set of eight per direction plus the reference and the current design.

## What I have seen myself, before the critique

Not a verdict; the fresh critic has not reported yet.

- **D fills the frame and the others do not.** D at 80x24 carries all fourteen rewrite rules in three scope groups, each ending in its `else` row, with a `reach` column, inside 24 rows. A, B, C and E leave 20 to 30 of 40 rows empty on their main frames.
- **D's tab strip is the chain.** `1 presentations 15 ↓→ 2 rewrites 14 > → 3 defaults 4 → 4 routes 7 · 5 mangle 11` is the tab strip, the pipeline and the trace in one row, and it still reads at 80 columns as `1 → 2 rewrites 14 → 3 → 4 · 5`. It solves the collapsing-tab-label problem as a side effect.
- **D's `else` rows surface a latent fault.** The `openai_responses` group ends `else no default openai_responses → no route: nothing catches it`. Nothing in the current design would show that.
- **E's overview is the best "what is wrong" surface of the five.** Its ATTENTION panel reads `anthropic_sub  on, not configured · first choice of route 1`, naming the fault and what it costs in one line, for each of four providers.
- **E's route page reads downward as asked.** A rail of `client`, `matched by`, `handled by`, `sent to`, `provider`, with blocks that are off collapsed to one dim line.
- **C answers the diagnostic question fastest.** Ten numbered links with a verdict each, the answer at the top, and the credential editable in place at the link that failed.
- **B teaches the model best.** Selecting `policy` prints the lookup order with counts, which nothing in the app says today.
- **A is the safest and the least ambitious**, and its presentations table is a complete fix for the fifteen identical rows.
- **D and E may not be competitors.** D is the rule level and E the route level; D's `[enter] open the rule's route` and E's `2 rules` tab already point at each other, and E's overview is an index to D's tables rather than a copy of them.

## Step 2.2 — the critique

A fresh critic read the five directions against the reference and the operator's words, from screenshots only: `critique-round-1.md`.
It chose D, and composed it with E and C (calls 6 to 8).
Its findings that are defects in the current product were filed as issues; the ones about the mockups are the converged brief's to-do list.

Round 1 found 2 criticals (D has no revision surface at all; panels are drawn to the frame), 9 suggestions and 3 nits.

## Round 2 — the converged set

Fresh critic, screenshots only (`critique-round-2.md`): no criticals, 14 suggestions, 10 nits.
The structure held; what stood in the way was disagreement between frames (focus shown twice, `enter` and the save key meaning different things on different pages, lane counts) and a few sentences and raw enums back on the page.
All 24 are being applied (calls 12 to 14 settle the ones that needed a decision).
The mockup findings are not filed as issues: the Workhorse run for this loop had already ended when round 1 tried to report, so they live here and in the critique.

## Round 3

A second fresh critic, who had not seen round 2 (`critique-round-3.md`): no criticals, 5 suggestions, 13 nits.
Converged in structure; the five suggestions are meaning-level (a `reach` reading that called serving rules dead, digits with two meanings, two reorder mechanisms, settings values all dim, the providers table not colouring its fault).
Not clean by the stop rule's count (at most three suggestions), so the count of clean rounds is still zero.
All 18 were applied and checked by eye.

## Round 4

A fresh critic (Fable), who read no earlier round, judged all 30 frames: 0 critical, 6 suggestions, 8 nits (`critique-round-4.md`).
Not clean, so the clean count stays at 0.
Structure, colour, widths and recurring counts held; the findings are wording and teaching:

- the glob refusal named a fix on a different field from the one the control line names
- the route page's four off handlers were unreachable from anything the screens teach
- `warm`, `cold`, `pinned` and `slug` were missing from MARKS
- the route page's upgrade line was a per-request reading
- the 80-column table dropped the spend clause first
- the presentations else row pointed at `2 rewrites`, which reads as a count

All six are being fixed, along with the nits on `claude-sonnet` naming, `promote` in the overlay, `needed` beside empty fields, `5 rules · 1 new`, and `set in toker.toml`.
Skipped: the history lanes column shape, because lanes on the live revision are a mix of warm and cold, so the bare count is the right reading; `published no` stays plain, because it's a presentation's setting rather than a sync fault; and the focus-dependent control line is how every panel behaves.

## Round 5

A fresh critic (Opus), who read no earlier round, judged all 30 frames: 0 critical, 3 suggestions, 12 nits (`critique-round-5.md`).
Clean, so the clean count is 1.
The suggestions: the overlay lacked `not synced`, `unprefixed`, `${model_id}`, `as` and `published`, and its `off` didn't say the request is refused; `5 cold pinned` contradicted the overlay's definition of pinned; and the settings frame refused `175k`, the TUI's own notation.
All three were fixed, with eleven of the nits, and checked by eye: the settings edit is now an inline form refusing `2M` against the largest context, the overlay is inset and carries the new marks, and openrouter's `used by` counts through its fallback routes (`10 rewrites · 4 defaults · 5 routes`).
Skipped: nit 10, the shorter after-save header, because the full tab strip takes about 100 of 120 cells and no lifecycle clause fits beside it; the collapse is the design system's order.

## Round 6

A fresh critic (Fable), who read no earlier round, judged all 30 frames: 0 critical, 2 suggestions, 7 nits (`critique-round-6.md`).
Clean, so the clean count is 2, the second from a fresh critic: the stop rule is met, at the cap.
The two suggestions are applied as final polish, without another round: `hits` never said its window, so it's now `hits 24h` with a MARKS line; and a presentation's `published  no` contradicted the synced header, so it now reads `yes`, the scope's state.
The nits applied: `· 1 new` on the fallback form, a `sends` row of its own in the RULE detail, session ids on every PREVIEW row, `reach` replacing `now` on the routes table, `pinned` dropped from MARKS since no frame draws it, and `follows lane TTL` dim in settings.
Not applied: the explanation of off handlers in the overlay, which belongs to the handler picker that `enter` on `handled by` opens, which is in the backlog.

## Where it stands

- the five direction mockups (a to e) are removed at the user's request; the converged set is the only one left, and the directions survive in `direction-*.md` and git history
- loop finished: rounds 5 and 6 clean, from fresh critics; round 6's polish applied by the drawing agents
- the user flagged that the presentations `as` tiers were treated as Claude's; the guide now says tiers are the client's own, and the wider problem (Toker hardcodes tiers and families in code) is card Y1
