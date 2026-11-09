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

## In flight

- capture rig: ANSI to HTML to PNG, plus form and empty-state captures from a sandbox
- directions A, B and C

## Next

- critique the three side by side with a fresh critic, choose one, record why in the ledger
- refine the chosen mockup through a few UI-loop iterations before anything is built
