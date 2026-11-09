# Direction D: the firewall

Mockups are in `.workhorse/design/mockups/x1/`:

| file | frames |
| --- | --- |
| `d-01-rewrites.html` | rewrites ordered (120x40), a request falling through, a rule edited with a live refusal, a rule moved |
| `d-02-other-tables.html` | routes with fallback order, presentations, mangle, empty fresh install, empty accounts (100x24) |
| `d-03-config.html` | providers with the broken one and its fix, settings with a refusal, history |
| `d-04-narrow.html` | rewrites, routes and presentations at 100x24, rewrites at 80x24 |
| `d-05-keys.html` | the `?` overlay |

All frames are checked: every row is exactly the claimed width, and the font stack names `"Iosevka Term"`.

## The structure, in one sentence

The routing page is the request's path drawn as a chain of ordered tables (presentations, rewrites, defaults, routes, with mangle beside it), where each table is a list of "match → target" rows read top to bottom, and the header strip is both the tab strip and that chain.

Providers, settings and revisions are not ordered, so they are a separate config page of plain tabs (`1 providers  2 settings  3 history`) with no arrows.

## Rendering a wide rule table in a terminal

This is the problem the direction is judged on, so the choices are stated.

**Position is structure, not a number.** Rules sit in groups by scope, since a rewrite's priority is only unique within a scope. A group is a rail: `┌─ anthropic_messages` at the top, `│` down the left of every row, `└─ else default anthropic_messages → …` at the bottom. The small number in the gutter is dim and is there to say "rule 4 of 5" in sentences; the thing that carries order is the rail and the else row. The else row is the end-of-chain policy of a firewall chain. It is also where the next table is named, so the chain reads continuously.

**One row, one line, chosen columns.** The rewrites table at 120 keeps: position, kind, match, route, model out, reach, hits. It deliberately does not make rows two lines. A rewrite's identity is the match, so the match column shows the pattern in the same `${model}` notation as the model-out column: `anthropic_sub/${model} → … ${model}`. That puts the transformation on the rule: the prefix goes in, the bare name goes out. The raw regex is one level down, in RULE.

**What was cut from the row, and why.** The rule id (`legacy-prefix-anthropic-anthropic_sub`): thirty-odd characters that restate the match, and they are what made today's list unreadable because they share a prefix. Priority: it is the position. The route's display name, candidate count, the scope: scope is the group. The matcher's raw regex: the display notation carries what the operator needs. All of it is in the RULE panel for the selected row.

**Reach is the column that earns its width.** It says `ok`, `off` or `no cred` for the provider the route ends at. It is what lets an operator see, in the rules table itself, that five rules lead to a provider with no credential. It is the last column to go when narrow.

**Narrowing sheds in this order.** Borders (ruled panels), then the detail panel to a single line, then `hits`, then the shared `legacy-route-` head of a route id replaced by `…` (the tail is what distinguishes routes, so the head goes, not the tail), then `kind`, then `model out`, and last `reach`. The match column does not shed: `anthropic_sub/${model}` is 22 cells at its longest, so it fits at 80. At 80x24 the table still shows the rail, the match, the target tail and reach.

**Fall-through is shown in the table.** A trace marks `↓` on every rule the request fell past, a bold `>` on the one that caught it, and dims what was not reached, including the else row and other scopes. The header strip carries the same marks per table (`1 ↓ → 2 >`), so the trace crosses tables, and in the routes table the walk continues: candidate 1 is passed over (no credential), candidate 2 serves, and the model is rewritten by the alias.

**Overlap.** Two rules that can both match are reported when it matters: while editing (the preview shows each sampled request before and after), while moving (what changes if this passes that), and as `dead` in the reach column on a rule that an earlier one swallows. Prefix rules that cannot overlap say so in RULE (`can both match? no: the prefixes differ`).

**Tabs at 120 and below.** The strip is `1 presentations 15 → 2 rewrites 14 → 3 defaults 4 → 4 routes 7 · 5 mangle 11` and fits at 100. At 80 the others collapse to numbers but keep their arrows (`1 → 2 rewrites 14 → 3 → 4 · 5`), so they still read as an ordered chain rather than as bare numbers.

## What an operator sees first, second, third on the main frame

1. The chain strip with the current table highlighted, and the table's scope groups. The chain tells them where in the resolution they are before they read a row.
2. The rule rows, with the rail and else row showing the order and where a miss goes, and `no cred` standing out in yellow in the reach column.
3. The RULE panel for the selected row: example in and out, where it sends, why it is or is not working, and the controls.

That order is core first, drill down: the table is the overview, the selected rule is the detail, and the raw regex or id is below that.

## Steps against today

Counts follow survey section 2: key presses from the dashboard, not counting typing or cursor moves unless the survey counted them.

| task | today | direction D |
| --- | --- | --- |
| enable a provider | about 5 (providers are 3 to 5 rows apart) | `Tab Tab`, up to 4 moves (one row per provider), `space` |
| supply a key for it | 3 more | 3 more (`e`, type, `enter`), and the page offers it |
| turn a gate on or off | 4 to 5 | 4 to 5, same |
| set the notice style or a number | 4, and a bad value is refused only on enter | same, and the refusal is live beside the field |
| check why a request went somewhere | about 5 and a JSON popup | `t`, type the request, `enter`: 3, read in the table |
| change which model serves a name | 12 to 20 | about 5: `2`, select, `e`, edit, `ctrl+s` |
| add a fallback | 22 `Down` plus dozens | `n` in the route, binding, model, `ctrl+s`; the edge defaults are visible |
| validate the draft | 1 (`V`) | none, the form is validated as typed |
| activate | 3 to 5 | none: save applies, to new lanes by default |
| reactivate a revision | 4 | 5 (`Tab Tab`, `3`, select, `x`, confirm): one worse |
| promote a model | 3 | 3 (`5`, select the upgrade, `p`) |

## What D is worse at

- **One table at a time.** The current design is no better here, but D promises a chain and shows one link of it. The header strip and the else rows are the only connection, and an operator wanting the overall picture still has to visit four tables. A route with several failing candidates and three rules leading to it is read across two tables.
- **It assumes the operator knows what they are doing.** That is the steer, but a first-time operator sees `${model}`, `glob` and `regex` and is no better told what they mean than today.
- **Mangle does not fit the metaphor.** Classifiers, upgrades and compaction are not ordered and not match-to-target in the firewall sense. They are drawn in the same table frame so they look like one family, but the header says "none is ordered", which is the table admitting it is not one.
- **The dead-rule and overlap claims are only as good as the matcher analysis.** Glob against glob and prefix against prefix are decidable cheaply. Regex against regex is not in general, so the page would have to say `may overlap` where it cannot prove otherwise.
- **History grows with every save.** Live apply means each save is a revision, so a day of tinkering is a long history. D mitigates with generated sentences, but the list will be noisy.
- **The detail panel is the first thing lost narrow.** At 100x24 there is no room for it, so the explanation of why a rule says `no cred` is a keypress away rather than on screen.
- **Moving a rule between scopes is not drawn.** Order only exists inside a scope, so a rule cannot be dragged across groups; changing its scope is an edit.
- **Reactivate is one step further** than today, because it lives on the config page.

## Marks introduced, and their legend rows

All are in the `?` overlay's MARKS section (`d-05-keys.html`).

| mark | meaning |
| --- | --- |
| `>` | a trace was caught here (bold) |
| `↓` | a trace fell past this rule |
| `${model}` | the name the match captured, sent on; `as asked` is sent unchanged |
| `…` at the head of a route id | the shared `legacy-route-` head has been clipped |
| `no cred` | the route's provider is on but has no credential |
| `off` | the route's provider is off |
| `dead` | an earlier rule catches everything this one would |
| `lifted` | the rule `m` is holding |
| `↑` | an upgrade moves the model to a newer one (existing mark, extended to policy) |
| `hits` | requests in 24h; blank for none, `≥` a floor, `-` not recorded |

`↓` and `↑` are also used to name arrow keys in overlays; the design language already allows that.

## Notes on internals (not drawn into the mockups)

- **A single save-that-applies.** The TUI can compose draft write and activation from the existing endpoints, but each save is then two calls and a race with another editor. A single "write this record and activate at scope X" endpoint would make live apply atomic. The default scope would be `new_lanes`, with a warm-cache forced transition only ever armed deliberately.
- **A one-record validation endpoint, or a validator in the TUI.** Live validation needs the same checks the compiler makes (a glob has no captures, a route id exists, a regex compiles). Most are cheap to do locally; "this route id exists" and "this binding is implemented" need the snapshot, which the page already has.
- **Rewrite position instead of priority.** The page orders by position and the TUI renumbers priorities on a move, spacing them with gaps. If the policy document allowed a rewrite to be inserted "before this one", the TUI could drop the renumbering. Priority being unique within a scope makes the current form workable.
- **A `prefix` matcher.** The legacy migration writes `^provider/(?P<model>.+)$` thirteen times. The display recognises that shape and writes `provider/${model}`, but a first-class prefix matcher would make it exact rather than a pattern match on a regex string.
- **Hits per rule.** `hits` reads the provenance already recorded per ledger row. If provenance names the rule id, the count is a group-by; before provenance existed rows carry nothing, hence the `≥` and the blank.
- **A trace endpoint that reads the live revision.** Preview resolves against the draft and ignores circuits and credentials. With live apply the draft and the active revision are the same, so the page can say so, but the "credentials ignored" caveat stays unless the trace endpoint is given the evidence.
- **Sentence descriptions of revisions** can be derived from the diff against the parent in the TUI; no schema change is needed, but storing it in the revision's description at save time would make `toker` CLI history read the same way.
- **Credential source on the config endpoint.** Choosing the keyring or an environment variable is only possible through `toker setup`, so the providers fix offers `[e] enter a key` and a pointer. Accepting the source in the config endpoint would let the fix be complete.
