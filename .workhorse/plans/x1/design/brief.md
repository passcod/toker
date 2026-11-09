# Common brief for the direction mockups

Card X1 redesigns Toker's settings and routing pages.
Three directions are being mocked separately so they do not converge.
This file is what all three share.
Your own prompt says which direction you are drawing, and that is the part you must not take from here.

## Read these first, in this order

1. `.workhorse/design/design-system.md` — the TUI's palette, glyph vocabulary, bar kinds, chrome rules, copy rules and interaction rules. It is binding. The general Workhorse mockup defaults (stone greys, burnt orange, Inter, 4px grid) describe nothing here and must not be applied.
2. `.workhorse/design/mockups.md` — how to draw a mockup of this TUI, and how to check it.
3. `.workhorse/plans/x1/survey.md` is not a path; the survey is `.workhorse/plans/x1/design/survey.md`. Read sections 1 (the domain), 2 (what an operator does), 6 (constraints) and 7 (what is confusing). Skim 3 for the current layout.
4. `.workhorse/plans/x1/design/current-state.md` — what the live instance actually shows.

## The reference

The app's own dashboard is the reference for consistency, and it is good.
`.workhorse/design/designs/dashboard-reference.html` is it, drawn in the shell.
`/home/felix/.cache/workhorse-rig/passcod__toker/cards/x1/work/live/00-as-found.txt` is the live one at 135x50.

Match its density, its quietness, its one-thing-per-line rhythm and its way of writing absence.
A new surface that looks as though the same person drew the dashboard has succeeded on principle 11.

## The real data to draw with

Use the live captures in `/home/felix/.cache/workhorse-rig/passcod__toker/cards/x1/work/live/` as your data.
They are plain text (`.txt`) and ANSI (`.ansi`) pairs:

| file | screen |
| --- | --- |
| `00-as-found.txt` | the dashboard |
| `04-routes.txt` | routing view 1, presentations (misnamed; it is presentations) |
| `05-routes-real.txt` | routing view 2, routes, candidates & defaults |
| `02-wrapped.txt` | routing view 3, rewrites, families, upgrades & compaction |
| `01-tab1.txt` and `03-presentations.txt` | routing view 4, preview, revisions & activation (both are view 4) |
| `06-config.txt` | config tab 1, providers |
| `07-settings.txt` | config tab 2, settings |
| `08-config-keys.txt` | the config page's `?` overlay |
| `10-routing-keys.txt` | the routing page's `?` overlay |
| `09-routing-keys.txt` | the accounts page, empty, with its overlay |

Draw with the real names from these: `legacy-route-anthropic_sub-anthropic_messages`, `legacy-prefix-openai-responses-codex_sub`, `legacy-family-anthropic_sub-anthropic_messages`, `openrouter/${model_id}`, `anthropic_sub.anthropic_messages`, `codex_sub.codex_responses`, `openrouter.openrouter_chat_completions`.
Invented tidy names hide exactly the problem the redesign is for: these ids are long, repetitive and share prefixes.

## The hard cases every direction must draw

A direction that only draws the happy case has not been designed.
Each of these must appear in at least one of your frames:

- **The 1,416 records.** View 4 holds 1,416 records against the real store, with runs of a dozen rows that render identical text today.
- **Fifteen identical presentations.** Every presentation reads `openrouter/${model_id}`, because the slug is the template.
- **Four kinds in one list.** View 3 holds prefix rewrites, a protocol alias, classifiers, upgrade policies and the singleton compaction policy.
- **A provider that is on but broken.** `anthropic_sub  on, not configured  credential none`.
- **Empty.** A fresh install with no policy at all, and the accounts page with nothing in it. Absence is written, never left blank.
- **A long name that must clip.**

## Constraints you may not design around

- **The sixteen ANSI colours only, by function**, as the design language assigns them. No RGB. Colour is a de-emphasis channel here: plain white is the most prominent thing on screen, red is quieter than yellow. Emphasis is bold, not colour.
- **`NO_COLOR` must still read.** Every distinction has to survive the colours being stripped, with the modifiers kept.
- **The absence vocabulary is fixed:** `-` not reported, `?` unknown, `≥` a floor, and the phrases the design language lists. Never a computed zero.
- **A mark an operator reads must be in the `?` legend.** A new mark means a new legend row, and you must write that row.
- **Panel titles are upper case, inside the border, unspaced.** Lower case everywhere else except the word Toker.
- **A control is written `[key] label`;** a key hint is `e edit · n new`.
- **A destructive or spending action arms rather than fires.**
- **The internals are not yours to change.** The card says so. The policy document's shape, the draft and revision model, activation, publication and the control endpoints all stay as they are. You are redesigning how they are presented and driven. A proposal that would make configuring easier if an internal changed is welcome, but write it as a note in your report, not into the mockup.

## What to produce

Mockups at **120x40** for the main frames and **100x24** for at least two of them, since 100x24 is the size the repo's own snapshots use and the size where the current design breaks down.
One `80x24` frame showing what your direction sheds when narrow.

Several frames may live in one HTML file, each in its own `<pre class="frame">`, with a short caption above each saying what it shows and what the operator just did.
Start from `.workhorse/design/designs/tui-shell.html`: copy it, so the palette, the cell metrics and a class per style are already right.

**Check every file before you finish**, with both checks in `.workhorse/design/mockups.md`: every row exactly the frame's width, and the font stack naming `"Iosevka Term"` and never plain `Iosevka`. A ragged frame is a failed deliverable.

## Your report

Write it as the last section of your own report file, named in your prompt. It must say:

- the one sentence that describes your direction's structure;
- what an operator sees first, second and third on your main frame, and why that is the right order;
- for each of the tasks in survey section 2 that these pages own, how many steps it takes in your direction against how many it takes today;
- what your direction is worse at than the current design, honestly;
- any internal change that would make configuring materially easier, as a note.

Do not read the other directions' files, and do not look in `.workhorse/design/mockups/x1/` for anything but your own output.
