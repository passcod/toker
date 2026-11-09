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

## In flight

- capture rig + current-state matrix (agent `capture`)
- domain and UI survey (agent `survey`)

## Next

- critique the current pages against the protocol
- three directions, mocked at 100x24 and 120x40
