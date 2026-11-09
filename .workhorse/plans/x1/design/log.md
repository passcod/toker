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

## In flight

- directions A, B, C, D and E

## Next

- critique the three side by side with a fresh critic, choose one, record why in the ledger
- refine the chosen mockup through a few UI-loop iterations before anything is built
