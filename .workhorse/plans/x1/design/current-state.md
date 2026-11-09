# What the settings and routing pages do now

Captured from the user's live `toker tui` (tmux 4:0.0, 135x50) on 2026-11-09, driven read-only: navigation and the `?` overlay only, no key that could mutate the policy.
The captures are in the card's rig work directory under `live/`, as plain text and as ANSI with the colour intact.

The committed insta snapshots understate the problem, because their fixtures hold a handful of rows.
Against the real store the same screens hold hundreds.

## The pages

`Tab` cycles four pages: dashboard, routing, config, accounts.
Only the dashboard is good, and it is the reference for everything below.

## Routing, view 4 — preview, revisions & activation

One flat list of **1,416 records**, 49 of which fit on a 50-row terminal, the rest counted as `… 1367 more records`.

The list interleaves, with nothing marking where one kind ends and the next begins: two revisions, the draft, three publications, seven bindings, a `lane_impacts` summary row, three diagnostics, and then roughly 1,400 further rows.
Of those, long runs render identical text — twelve consecutive rows reading `anthropic_sub.anthropic_messages  {"binding_id":"anthr…`, then a run of `openrouter.openrouter_chat_completions  {"binding_id":…`.
Nothing distinguishes one row in a run from the next, so a selection cannot be aimed.

The detail pane for a revision is raw pretty-printed JSON, wrapped at the pane width with no indent carried, so a long string value breaks to column 0 and its closing `)$"` lands alone on the next line.

## Routing, view 1 — presentations

Fifteen rows, every one of which reads `openrouter/${model_id}`.
The template is the row's text, so the rows are indistinguishable from each other.

## Routing, view 3 — rewrites, families, upgrades & compaction

Twenty-five rows holding four kinds: thirteen prefix rewrites, one protocol alias, five classifiers, five upgrade policies, and the compaction policy.
Nothing marks the boundaries.
They can be told apart only by reading the id prefix — `legacy-prefix-`, `legacy-family-`, `legacy-force-newest-` — which encodes the kind only because a migration happened to name them that way.
`compaction` is a singleton policy, and it is the twenty-fifth row of a list.

## Routing, view 2 — routes, candidates & defaults

Eleven rows: seven routes then four defaults.
The best of the four views, because the detail pane has a real formatter for a route and its candidates.

## Config — providers

Five providers, each with two or three indented fields.
`anthropic_sub` reads `on, not configured` with `credential none`.
The page states the broken state and offers nothing to do about it.
There is no detail pane, so the page's structure has nothing in common with routing's.

## Config — settings

Ten rows of label and value, `on (default)` where nothing was set.
This is the one screen in the area that reads well.

## Accounts

A single empty `ACCOUNTS` box with no rows and no message.
The design language requires absence to be written; here nothing is.

## Keys

The routing page binds eighteen keys at the top level and eight more inside a form.
The overlay's own wording gives the model away:

> `n  add a record, asking which kind where a view lists several`

A view lists several kinds of record, and adding one begins by asking which.
That is a database browser's model, surfaced to the operator as the interface.

The config page binds five keys, including `space toggle` and `e edit`, and nothing on the page says so.

## The header

`draft 2 · Claude settings not reconciled (patching the claude model picker at /home/f…`

The page's one piece of standing status is a draft generation number and a message clipped mid-path.
