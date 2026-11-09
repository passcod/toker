# Direction E: one page per route, read top to bottom

Mockups:

- `.workhorse/design/mockups/x1/e-routes.html`: frames 1 to 8, the overview and the route pages.
- `.workhorse/design/mockups/x1/e-elsewhere.html`: frames 9 to 13, rules, models, history, providers, settings.
- `.workhorse/design/mockups/x1/e-narrow.html`: frames 14 to 20, 100x24, 80x24, empty, a route made from nothing, accounts, the `?` overlay.

Every row of every frame was checked at its frame's width, and the font stack names `Iosevka Term` and never plain `Iosevka`.

## The direction in one sentence

A route is a page read downwards in the order a request travels, **matched by** the rules that send it here, **handled by** the blocks that check or change it, **sent to** the candidates in fallback order, with an overview of all routes as the index and the tables the pages refer to (rules, models, history) as tabs beside it.

## How it is built

**The rail.**
One continuous glyph path runs down the left of the page: `┌─ client`, three `├─` stations, `└─ provider`.
The station names are phrases that read as a sentence going down: "matched by 3 rules", "handled by 3 blocks · 4 off", "sent to 1 candidate".
Direction is carried by the two end nodes and the sentence, with no arrow to learn.
Fallback steps use the existing `→` between candidate numbers (`1 → 2`).

**Core first, and which way it drills.**
At rest a station is its head line plus the few rows that are the core:

- *matched by*: one line per rule, three at most, then `… N more`.
- *handled by*: one line per block that is on, and **one dim line naming every block that is off**.
  A block that is off is therefore never hidden, and never takes more than the one line.
- *sent to*: two lines per candidate (identity, then its live verdict), one line per fallback step for the trigger and one for the consent.

Drilling is **in place**, with `enter` on a row: the row's fields open beneath it and every other station stays at its core line, so the path stays on screen as one picture (frame 3).
Nothing opens in a modal.
`enter` on the dim `off` line lays all seven blocks out so each can be switched with `space` (frame 7).
Frame 2 against frame 3 is the pair that shows both states.

**Blocks.**
Three states, all readable with colour off: off is a dim row reading `off` that keeps its value; on is a plain row reading `on`; configured is `on` plus its value after it.
Switching a block off dims it in place and keeps the value beside it.

**Pages.**
The page strip is numbers only, `all 1 2 3 4 5 6 7`, the current one bold and reversed.
It sits in the page's first line beside the route's id, so it costs no row and does not collapse badly the way the dashboard's labelled strip does: seven pages cost 24 cells, thirty would cost 93 and would window with the existing `‹N` and `… N more`.
`[` and `]` move between routes, `o` goes to the overview, `enter` on an overview row opens that route, a click on a number jumps.
The neighbouring routes' ids are named on the line under the identity when there is room.
A new page is `n` on the overview, which opens the same page empty (frame 18).
The page-level tab strip is separate and has four tabs: `1 routes  2 rules  3 models  4 history`.

**The overview** (frame 1).
One quiet line per route: how many rules point in, how many blocks are on, where it goes, its pinned lanes, and how its first choice stands now.
The shared `legacy-route-` prefix is written once in the heading, so the ids fit.
Under the list, PATH draws the selected route in three lines, and ATTENTION lists the providers that are on but not configured, or off with a route depending on them.
TABLES indexes tabs 2 to 4 with their counts.

**Where the non-route things live.**

| record | home | also appears |
| --- | --- | --- |
| presentation, rewrite, default | tab 2 rules, three panels in the order a request meets them | the inbound station of the route they point at |
| classifier, upgrade policy, compaction | tab 3 models, a panel each | the `upgrade policy` and `compaction` blocks |
| revision, publication, diagnostic | tab 4 history | diagnostics also sit on the candidate they concern |
| provider, binding, model list | config page, tab 1 providers, with a BINDINGS panel | the candidate's own line, and `g` goes to it |
| settings | config page, tab 2 | |
| accounts | their own page, unchanged but for the empty message | |

`n` adds to whichever panel the cursor is in, and an empty panel still draws its heading and says what adds to it, so nothing needs a route to exist first.
On a route page `n` adds what belongs where the cursor is: a rule at *matched by*, a candidate and its edge at *sent to*.

**Live, and no validate step.**
A field is checked as it is typed and the refusal appears beside it in red, in words (frame 8, frame 13).
The entry's last line says `enter can't save: …` in place of `enter save` until it is valid, and the header says `1 edit not saved`.
`V` is gone and so is the activation modal.
A committed edit is one live revision at the new-lanes scope, and the line under it says what that did to lanes already pinned (frame 7).
`m` moves warm lanes onto the new revision and arms, since that is the one consent left.
See the internal notes below for why this needs a client-side debounce.

## What the operator sees on frame 2, in order

1. **The identity line and the first rule lines.**
   Which route this is, that it is page 1 of 7, and what arrives here: two rewrites and a default.
   That is the answer to "what is this route for", and it is first because it is first in the request's life.
2. **The three blocks that are on, and the dim line of four that are off.**
   What is done to a request here, and what could be.
3. **The candidate and, in yellow beneath it, `not configured`.**
   Where the request goes and the one thing wrong with that.
   It comes last because it is where a request would actually fail, and the overview's NOW column and ATTENTION panel carry the same fact for a reader who is only scanning.

The order is the request's order, so a reader who has never seen the page gets it from the sentence the stations make.

## Steps against today

"Steps" counts key presses from the dashboard, as survey section 2 does, with `Tab` to reach the page as one and typed values not counted.

| task | today | direction E |
| --- | --- | --- |
| enable a provider | about 5 | about 4 (providers are one line each, not three to five rows apart) |
| set an endpoint | about 7, and the whole URL retyped | about 6, the editor prefilled |
| put a credential in | about 8 | about 6, or 4 from the candidate that needs it (`g`, `e`, type, `enter`) |
| turn a gate on or off | 4 to 5 | 4 |
| set the notice style | 5, free text | 5, a choice with its values shown |
| change which provider or model a route uses | 12 to 20, plus typing | about 8, plus typing: no validate, no activation |
| add a fallback | about 40 (22 `Down` alone), plus typing and activation | about 12: `n` at *sent to* gives the step with trigger and consent on its own rows |
| catch a name nothing presents (rewrite) | about 9 plus fields | about 5 plus fields |
| preview a made-up request | about 5, then JSON | unchanged at about 5; the result is meant to light the path on the route page, which I did not draw |
| validate the draft | 1, then read a report | 0 |
| activate | 3 (`a`, scope, `y`) | 0 for new lanes, 2 to move warm lanes (`m`, confirm) |
| reactivate an older revision | about 6 and scrolling 1,416 rows | about 5, the list is two rows |
| promote a model | 3 | unchanged |
| import from `toker.toml` | unchanged, `i` on tab 4 | unchanged, `i` on tab 4 |

The estimates for model change and fallback assume `J` and `K` to jump between stations and the cursor starting on the first rule; neither is drawn beyond the overlay row.

## Hard cases, and where each is drawn

- The 1,416 records: history is eight rows in three short panels (frame 11); the 1,398 model rows became counts under their binding (frame 12).
- Fifteen identical presentations: one group row, members told apart by scope and selector (frame 9).
- Four kinds in one list: rules and models are separate panels and tabs (frames 9, 10).
- A provider on but broken: frames 5 and 12, and on the overview.
- Empty: frames 17, 18 and 19.
- A long name that must clip: frames 15 and 16.

## Data that is real and data that is not

Real from the captures: the route ids and their order, the first route's rule, lane transition, pinned lanes, candidate, binding and cost class, the four defaults' scopes, the thirteen prefix rewrites' ids, the provider states and credentials, the twelve models of `anthropic_sub.anthropic_messages`, the failed Claude publication.
Plausible and not read: the other routes' rule counts, blocks and lanes; which route each default and prefix rewrite points at; the fallback chain on route 7 (the capture shows one candidate; the chain is the kind of thing the page is for); presentation selectors; the model counts on the other bindings, chosen to sum to 1,398 with the capture's other rows to 1,416; the diagnostics' wording.
The capture's route detail says `now: available` for a candidate on `anthropic_sub` while the provider page says `on, not configured`; the accounts page shows the login coming from another program's store, which probably reconciles them.
I drew `not configured`, as the brief asks, and the redesign should show both facts on the candidate when they differ.

## What it is worse at than today

- **One thing across many routes.**
  Turning the upgrade policy off everywhere, or reading which routes have a circuit, means visiting seven pages.
  The old tab 3 showed all five upgrades at once.
  The models tab restores the reading but not the toggling.
- **Comparing two routes.**
  The overview shows counts, not contents, so comparison is flipping pages.
- **Candidate-level blocks are flattened.**
  Alias rules, required capabilities, the upgrade policy and circuits belong to a candidate in the document, but they are drawn as route-level rows.
  On a route with several candidates a block's row aggregates ("candidate 1 opens for 300s") and switching it per candidate needs a drill, which is one step deeper than a one-candidate route.
  This is a simplification that could mislead on a long chain.
- **Every edit is live.**
  The draft was a place to be wrong in privately.
  Here a bad rewrite is live for new lanes as soon as it is valid.
  The mitigation is only that validity is checked first and that new-lanes scope disturbs nothing in flight.
- **More keys than the old page has.**
  `[ ] o J K g m` are new, and none is guessable; the `?` overlay carries them.
- **Sparse on a tall terminal.**
  A route is about 20 rows, so at 120x40 half the frame is empty by design.
  Showing more would defeat the point, but it will look unused beside the dashboard.
- **The presentation list is an index, not a page.**
  The client's model list is edited from the rules tab, not from a route, so a presentation edit is a step removed from the route it eventually reaches.
- **Mixed results at width.**
  At 80 columns the rail and the stations survive but values clip hard, and a drilled station at 24 rows scrolls.

## Internal changes that would help

1. **A coalescing window for activation, client-side first.**
   `live-apply.md` is right that a revision per keystroke is wrong.
   The pages here activate per committed edit, which is better than per keystroke and still more than one per intent.
   The TUI should debounce: edits committed within a couple of seconds of each other, on the same page, become one activation.
   That needs no internal change; if it proves too chatty, a service-side `edit` that applies and activates as one transaction would be the next step.
2. **A field-level validation call.**
   Whole-draft validation on every keystroke is wasteful and its refusals name document paths.
   A call that checks one operation against the current draft without storing it, and returns the refusal keyed to the field, would let the beside-the-field message come from the service rather than being reimplemented in the TUI.
3. **A disabled flag on blocks that are optional lists.**
   "Off keeps its value" works inside a session only, because switching off alias rules or circuits removes them from the document.
   An `enabled` flag on those lists, as candidates already have, would let the value survive a reload and show dim on the page as drawn.
4. **A reorder operation for rewrites.**
   Priority is unique within a scope, so moving one means renumbering its neighbours.
   The rules tab sorts by priority and would like to move a row with one key.
5. **Reverse references in the snapshot.**
   `used by` (which routes use a provider, an upgrade, a classifier) and the inbound rules of a route are derivable client-side from the policy document, but the compiled policy already knows them and could hand them over with pinned-lane counts.

## Legend and overlay rows added

The `?` overlay on the route page (frame 20) carries these new marks, each with its row:

| mark | meaning |
| --- | --- |
| `┌ ├ └` | the path a request takes: client above, provider below |
| a dim row reading `off` | a block that is off; it keeps its value |
| `1 → 2` | a fallback step: what triggers it, then what it consents to |
| `all 1 2` | the route pages; the reversed one is this page |
| `‹N` and `… N more` | existing marks, reused for pages and rows out of view, so their wording widens |
