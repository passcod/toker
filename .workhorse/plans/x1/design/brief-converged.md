# Brief: the converged design

Direction D was chosen, composed with E's route page and C's trace chain.
This brief is settled: every decision below is made, and the job is to draw it faithfully, at real size, with the data given here.
Where something is not covered, follow the design system and the existing D frames, in that order, and say in your report what you decided.

Read first, in this order:

- `.workhorse/design/mockups.md` (the HTML shell, Iosevka Term, the width and font-stack checks) and `.workhorse/design/design-system.md` (colour as de-emphasis, bold, dim, reversed, absence, panel titles, controls, arming).
- `.workhorse/plans/x1/design/critique-round-1.md`, sections 2D, 2E, 3, 4, 5 and 6. Every finding there about D and E is to be applied unless this brief overrides it.
- The D mockups (`.workhorse/design/mockups/x1/d-*.html`) as the starting HTML, and `e-routes.html` (route page, edges), `c-chain.html` (the chain), `a-routing.html` (presentations table, catalogue) for the parts taken from them.

## Rules that bind every frame

- **Panels are as tall as their content.** A list that scrolls takes the remaining rows; a detail panel ends at its last line; the ground below the last panel is left bare, never boxed. No panel is drawn to the frame and then filled.
- **No sentence on the page.** No narration row under a title, no subtitle in a title rule, no explanatory empty state, no second hint line. A panel title is the word alone: `┌REWRITES──`. Explanation belongs in the `?` overlay.
- **Controls are written once, on the last content line of the panel that owns them.** No footers, no control line separated from the content by blank rows.
- **Fixed panel titles.** `RULE`, `ROUTE`, `PROVIDER`, `TRACE`, never a record name in the title slot. The record's name is the first line inside, bold.
- **Colour:** `no cred`, `refused · no route`, spends (`billed`) and publication not synced are yellow; a refusal is red; `off` is dim; `ok` is plain; nothing is green; identities are never coloured; no coloured backgrounds anywhere, and a selected row is plain reversed text.
- **Uniform columns are not columns.** When every row of a table has the same value in a column, the column goes and the value is said once in the scope head (`anthropic_messages  5 rules · prefix → ${model}`). Shed uniform columns before varying ones when narrowing.
- **`hits`** right-aligns against the box; `-` where not recorded, `≥n` for a floor, never blank.
- **No `legacy-` anything.** The generated-only semantics are to be retired, so the records carry authored names (data sheet below) and no record is marked as generated.
- **No JSON**, and no regex where a pattern notation says it (`anthropic_sub/${model}`); the raw form appears only inside an edit field that asks for one.

## The lifecycle

Saving a valid form puts it live. There is no draft in the interface and no apply key.

- A save creates a revision and activates it for new lanes and for lanes provably cold. That scope needs no consent and disturbs nothing in flight.
- The header's right side reads `rev 9 live` at rest. Directly after a save that left warm lanes behind it reads `saved · rev 10 live · 2 lanes on rev 9`, with `saved` bold and the lane clause dim.
- When the Claude settings publication is not reconciled, the header adds `· claude settings not synced` in yellow. Show this in exactly one frame.
- Moving running lanes onto the live revision, and reactivating an old revision, happen from the history tab. Moving warm lanes immediately arms rather than fires (the design system's arming rule): the control inverts, the panel's last line says what will be lost and how to confirm.

## The routing page

The tab strip is the chain:

`1 presentations 15 → 2 rewrites 14 → 3 defaults 4 → 4 routes 7 · 5 models 11 · 6 history 9`

The current tab bold and reversed, the others dim. The strip carries no trace marks. At 80 columns the non-current tabs collapse to their numbers and keep their arrows.

Every table tab is a table with the selected row's detail panel below it, sized to content. `enter` on a route opens the full-height route page; `esc` returns with the selection kept.

Each page opens on the tab holding a problem when there is one, else on its first tab. On this data the config page opens on providers with `anthropic_sub` selected.

### Rewrites

Three scope groups, each a rail `┌─` head, `│` rows, `└─ else` row naming the default and where it goes. Columns: position (dim), match, `→ route`, reach, hits. `kind` and `model` are uniform here and go to the scope heads.

The `openai_responses` group's else row is `else  no default  → refused · no route`, with `refused · no route` yellow.

RULE detail, sized to content: bold rule name, then label/value rows `match`, `example`, `sends to`, `reach` (with the reason, e.g. `no cred  anthropic_sub is on with no credential`), `overlaps` (`none: the prefixes differ`), `recent`. Controls on the last line: `[e] edit  [n] new  [m] move  [space] turn off  [d] delete  [t] trace  [enter] open route`.

### Trace (`t` from any routing tab)

The request goes in a header line inside TRACE: `trace  claude/anthropic_messages · model anthropic_sub/claude-sonnet-5`. The table above keeps the `↓` (fell past) and bold `>` (caught) gutter marks and dims what was not reached.

TRACE is C's numbered chain, answer first:

- `goes to  openrouter.openrouter_messages as anthropic/claude-sonnet-5 · billed` bold, `billed` yellow.
- then numbered links, one line each unless a link needs its reason: `1 presentations  ↓ fell past  no slug equals it`, `2 rewrites  > caught  rule 2 in anthropic_messages  anthropic_sub/${model} → claude-sonnet-5`, `3 defaults  not reached` (dim), `4 route  claude-sub`, `5 candidate 1  ↓ fell past  anthropic_sub.anthropic_messages  no cred`, `6 candidate 2  > serves  openrouter.openrouter_messages  billed  alias claude-* → anthropic/claude-*`, `7 upgrade  none  no newer model in sonnet`, `8 provider  openrouter  on  credential in keyring`, `9 gate  passes  cold gate on · 6,883 tokens, under the floor`.
- Controls: `[t] trace another  [enter] open link  [esc] close`.

### Editing a rule (live validation)

The edit opens inline under the row, as D8 does. Fields: `match` (kind cycles with `←/→`: prefix, glob, regex; then the pattern), `route` (a picker of the seven routes), `sends` (`${model}` or a fixed model).

Each field's reading sits beside it: plain for a reading (`catches claude-sonnet-5 and 2 more`), red for a refusal (`a glob captures nothing, so ${model} would be empty: use a prefix or a regex`). The control line is one line: `[ctrl+s] fix sends to save   [esc] cancel`, with `fix sends to save` dim in place of `save` while invalid.

PREVIEW below, sized to content: `3 of 5 recent requests would change route` bold, then the request / now / with this rule table. Nothing else.

### Routes

The table: route, `sent to` (the candidate chain as `anthropic_sub → openrouter`), `rules`, `defaults`, `lanes`, `now`. `now` for claude-sub reads `no cred · falls to openrouter, billed` with `no cred` and `billed` yellow.

ROUTE detail below the table is the `sent to` section alone: candidate rows, with E8's edge line between each pair on the same grid:

```
1  anthropic_sub.anthropic_messages   subscription   as asked                    no cred
   ↓ when unavailable before send · overloaded · 5xx    billed: allow   ambiguous retry: deny   cache: preserve warm
2  openrouter.openrouter_messages     billed         alias claude-* → anthropic/claude-*   ok
```

Controls: `[enter] open route  [e] edit  [n] add fallback  [m] move  [t] trace`.

### The route page (`enter` on a route)

Full height. Bold route name first line; the route switcher `1 2 3 4 5 6 7` at the right of that line, current reversed. No rail glyph, no `client` or `provider` stations.

Sections with bold heads, in this order:

1. `sent to  2 candidates`, the candidate rows and edges as above. The first candidate is the default selection.
2. `handled by  3 on · 4 off`, the three that are on (upgrade, lane transition, compaction), one line each. The off ones are not listed; `enter` on the head lists them.
3. `matched by  5 rules · 2 defaults`, one line each: `rewrite  anthropic_messages 2  anthropic_sub/${model}`, ... `default  anthropic_messages`.

Controls on the last content line: `[e] edit  [n] add fallback  [m] move  [t] trace  [esc] back`. Ground bare below.

A second frame: adding a fallback, inline in `sent to`, with a refusal beside the model field (`codex_responses does not offer claude-sonnet-5`) and a reading beside the binding field (`codex_sub is off: skipped until it is on`, dim), and the one control line.

### Presentations, defaults, models

- **Presentations:** A1's table fix applied to D3, with the rules above. PRESENTATION detail sized to content.
- **Defaults:** four rows plus the `openai_responses  no default  refused · no route` row (yellow). Detail sized to content.
- **Models:** FAMILIES (5), UPGRADES (5), COMPACTION (one line) stacked and sized to content; then OFFERS, A3's windowed catalogue (`… 170 above`, model, ctx, date, `… 236 more`), which takes the remaining rows because it scrolls.

### History

The revisions table, newest first: `rev`, `when`, `what changed` (a sentence derived from the diff), `lanes`. The live one marked `live`.

The selected revision's detail: its diff as `+ ~ -` lines in words (`+ rewrite anthropic_messages 6  claude-* → openrouter-messages`, `~ route claude-sub  fallback 2 added`, `- default unprefixed/openai_chat`), then a lanes line (`2 warm lanes on rev 9 · 5 cold lanes moved`). Controls: `[r] reactivate  [m] move running lanes  [c] compare a request`.

A second frame: `[m]` armed, as the lifecycle section says.

## The config page

Tabs `1 providers 5 · 2 settings`, no arrows (they are not ordered), no history (it moved to routing).

- **Providers:** table (provider, state, credential, bindings, used by), `anthropic_sub` selected because it is the problem. PROVIDER detail: bold name first line, then label/value rows `state on`, `credential none` (yellow), `refresh url`, `bindings`, `rewrites 5`, `defaults 2`, `routes 1`. No section heads inside the detail. Controls on the last line: `[e] enter key  [space] turn off  [enter] open route`.
- **Settings:** label/value rows including `session headers` and `transcript roots`, one row in edit with a red refusal beside it and the one control line.

## Data sheet

Use these names everywhere, so frames agree. Everything else (counts, catalogue entries, presentations) is taken from the existing D and A frames with the renames applied.

| was | now |
| --- | --- |
| `legacy-route-anthropic_sub-anthropic_messages` | `claude-sub` |
| `legacy-route-anthropic_api-anthropic_messages` | `claude-api` |
| `legacy-route-openrouter-openrouter_messages` | `openrouter-messages` |
| `legacy-route-openrouter-openrouter_chat_completions` | `openrouter-chat` |
| `legacy-route-openrouter-openrouter_responses` | `openrouter-responses` |
| `legacy-route-openai_api-openai_chat_completions` | `openai-chat` |
| `legacy-route-codex_sub-codex_responses` | `codex` |
| `legacy-prefix-<binding>-<provider>` rules | `strip <provider>/` (e.g. `strip anthropic_sub/`) |
| `legacy-family-*` classifiers | `claude-opus`, `claude-sonnet`, `claude-haiku`, `gpt`, `qwen` |
| `legacy-force-newest-*` upgrades | `newest on <route>` |

Providers: `anthropic_sub` on, no credential; `openrouter` on, keyring; `anthropic_api`, `codex_sub`, `openai_api` off. Live revision 9, nine revisions.
Session ids, where shown, are invented (`ses-free`, `ses-hot`, `ses-crow`). No email addresses appear.

## Files and frames

Write to `.workhorse/design/mockups/x1/`, each file a column of frames with an `<h2>` caption per frame, as the D files are.

| file | frames |
| --- | --- |
| `converged-01-rewrites.html` | rewrites at rest 120x40; the same at 135x50 (the operator's terminal); trace; editing with a live refusal; after save (header as the lifecycle says) |
| `converged-02-routes.html` | routes table with the ROUTE detail; the route page; adding a fallback |
| `converged-03-tables.html` | presentations; defaults; models |
| `converged-04-history.html` | history at rest; moving warm lanes, armed |
| `converged-05-config.html` | providers, opened on the problem; settings with a refusal |
| `converged-06-narrow.html` | rewrites 100x24; rewrites 80x24; route page 100x24; providers 80x24 |
| `converged-07-empty-keys.html` | a fresh install's rewrites (no rules: the rail, `-` where rows go, the else rows); the routing `?` overlay with every mark used above in its MARKS section |
| `converged-08-save-flow.html` | the edit-refuse-fix-save sequence as an animated frame: the four states cycled with CSS keyframes, no script, with a static strip of the four beneath for review |

Every frame is checked before you finish: each row exactly the claimed width, the font stack naming `"Iosevka Term"`, as `mockups.md` describes.
