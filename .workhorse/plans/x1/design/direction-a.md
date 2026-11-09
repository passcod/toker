# Direction A: the skeleton kept, the execution rebuilt

Mockups are in `.workhorse/design/mockups/x1/`:

| file | frames |
| --- | --- |
| `a-routing.html` | views 1 to 4 at 120x40, view 4 with an offers section open (the 1,416 records), and a presentation form showing live validation |
| `a-config.html` | providers with `anthropic_sub` selected, settings with an invalid value being typed |
| `a-narrow-empty-keys.html` | view 1 and view 4 at 100x24, view 2 at 80x24, a fresh install with no policy, the empty accounts page, the routing overlay and the config overlay |

The checks in `mockups.md` pass on every frame: each row is exactly the frame's width, and the font stack names Iosevka Term only.
There is no JSON anywhere in the frames.

## What this direction is

The routing page keeps its four views, its master and detail panes and its key set.
The config page gains the same master and detail anatomy.
Four things change inside that skeleton.
Every record is a sentence, and a list that holds several kinds is cut into headed sections with counts.
The order in which Toker resolves a request is visible, because the headings are numbered by stage and the rows within a stage are in the order they are tried.
The detail pane leads with a few facts and the rest is behind `enter`.
A field says what is wrong beside itself, and there is no validate step, so `V` is gone.

## How the brief's six fixes and the four-point steer are met

**Rows.**
A presentation row is `behaves as`, the model pattern it matches and what it excludes, so the fifteen `openrouter/${model_id}` rows are all different.
A route is a two-line entry: the id with its shared `legacy-route-` prefix dimmed, then the chain in words (`anthropic_sub → openrouter · billed fallback`).
A rewrite row is its priority, its prefix and its target.
A binding offer row is the model id, its context and its release month.
Rows shed columns before they clip, and a long id clips with `…` after its dimmed shared prefix.

**Several kinds in one list.**
Section headings with a count and a short dim gloss (`rewrites · anthropic_messages  stage 2 · lowest first ─── 5`).
View 3 has six headings, view 4 has four open sections and three folded ones.
At 100x24 the list windows around the selection and counts what it cut above and below.
Inside a long section the first line is a sticky heading with the position (`187/440`), so the kind is never scrolled away.

**Tab strip.**
`1 presentations  2 routes  3 rules  4 revisions` is 47 columns and fits at 80.
"Rules" stands for the four kinds in view 3, and the headings inside it name them.
"Revisions" stands for view 4, which is mostly revisions, the draft and what the draft changes.

**No JSON.**
Every kind has a formatter, including revision, draft, publication, offer, compaction and the problems list.
The raw-record fallback is deleted rather than tidied.
A regex is shown as what it means (`a name starting openrouter/`), with the pattern itself one `enter` away.

**Status.**
The header says `draft ≠ rev 2 · +1 ~2 · a activate`: there is an unactivated draft, it adds one record and changes two, and `a` activates.
Each changed record carries `+` or `~` in the first column of its row, and its detail says what changed (`except *-code · *-thinking (was *-code)`).
The draft row in view 4 lists the changes by kind and name.
The Claude settings reconcile result moves out of the header to the page message line, where the design language puts messages.

**Config detail pane.**
`anthropic_sub` is the only yellow row.
Its detail says what is wrong in a sentence and offers the fix as a control: `[s] store a key   [space] turn off   [e] endpoint`.
Controls that cannot act stay visible and dim with the reason in place of the label.
Settings carry their meaning, their default and who owns them, and the two fixed rows say they belong to `toker setup`.

**The operator's steer.**

- Tables and order: the headings read stage 1 (exact name), stage 2 (rewrites by priority), stage 3 (defaults), stage 4 (candidates in order).
  A rewrite's detail says `4th of 5`, what is tried before and after it, and that stage 1 comes before and stage 3 after.
  A route's detail ends with which rules reach it.
- Live validation: the form in `a-routing.html` marks an unclosed `[` beside the field, and the save control is dim and reads `[ctrl+s] fix 1 field first`.
  The settings entry does the same for a non-number.
  Warnings the compiler reports are a `problems` section in view 4 and a `PROBLEM` block in the affected record's detail, in plain words.
  Nothing is a report to read.
- Core first: a detail pane shows four to eight facts, and `enter` opens the rest.
  Bindings, lanes and offers are folded sections, so the revisions view reads as about twenty lines.
- Personal data: no email address or account name from the captures appears in any frame, and paths use `/home/user`.

## Marks and keys

New marks, each a row in the `?` legend (drawn in the routing overlay): `+ ~ -` (added, changed, removed since the active revision) and `▸ ▾` (a section folded, or open).
New keys: `[` `]` jump between sections, `space` folds a section, `/` filters the view, `enter` opens more fields.
Removed: `V`, and the overlay's "in a form" block, which could never be opened because the overlay cannot open over a form.
`n` adds a record of the selected kind, so the kind chooser only appears when nothing is selected.
`n` and `x` and `i` are shown only where they act.
Config keys: `e` edits the endpoint (or the selected setting), `s` stores a credential, `space` toggles, `c` clears a safeguards mark.

## What the operator sees on the main frame

The main frame is routing view 1 (`a-routing.html`, frame 1).

1. The header status, `draft ≠ rev 2 · +1 ~2 · a activate`, and the red message line.
   Whether there is unactivated work, and whether something failed, is what decides what to do next.
2. The selected row and its short detail: what this row does and what changed in it.
   It is the thing the operator came to work on.
3. The rest of the list, each row different from its neighbours, under one heading that names the scope and the stage.
   The list is context for the selection, not the thing to read first.

## Steps, against today

Counted as in survey section 2: key presses and entries from the dashboard, excluding typed values, with moves counted separately.

| task | today | direction A |
| --- | --- | --- |
| enable a provider | `Tab Tab`, then up to about 19 moves to a name row (providers are 3 to 5 rows apart), `space`: about 5 to 22 | `Tab Tab`, up to 4 moves, `space`: 3 to 7 |
| store a key | select the credential row, `e`, type, `enter`: 3 after reaching the provider | `s`, type, `enter`: 2 after selecting the provider |
| set an endpoint | select the row, `e`, retype the whole URL, `enter` | `e`, edit the prefilled URL, `enter` |
| turn a gate on or off | `Tab Tab 2`, select, `space`: 4 to 5 | the same count, with the row's meaning beside it |
| change which model serves a request | 12 to 20, including optional `V` | about 8: `Tab`, view, select, `e`, edit, `ctrl+s`, `a`, `enter` |
| add a fallback | 22 `Down` plus dozens more | unchanged by this direction (see below) |
| validate the draft | `V`, read JSON | 0: it is live |
| preview a change | no diff outside the activation modal | `Tab 4`, then the draft row (the second row): 2 |
| activate | `a`, `h/l`, `f`, `y`: 3 to 5 | `a`, `enter`: 2, with the consent prompt only when needed |
| promote a model | `p`, type binding, `tab`, type model, `tab`, `enter` | select the offer (`/` to filter), `p`, `enter`: 3 plus the filter |
| reactivate a revision | `Tab 4`, select, `x`, confirm: 4 to 5, and `x` can target the wrong one | the same count, and `x` is dim unless a revision is selected |

The fallback figure is not improved.
The route form is not drawn here, so the 22-press walk to `+ add candidate` stays unless the form leads with the candidate list, which is a form redesign this direction does not cover.

## What this direction is worse at

- The tabs do not follow the lookup order.
  Stage 2 (rewrites) is in view 3 and stage 3 (defaults) is in view 2, so order is told by headings and a detail line rather than by one table in order.
  A direction that cut the views by stage would show it better.
- The stage numbers are my addition.
  They are accurate to `RSOLV#lookup-order`, but the app has never labelled anything that way.
- More chrome: sections, folds, a sticky heading, two-line entries.
  At 100x24 and below a section heading costs a row, and a route entry sheds to one line, so the 80-column frame reads like the old design with better rows.
- The list and detail are 50/50 instead of 42/58, so a detail line has about 10 fewer columns than today.
- `enter` for more is one more step to see an edge, a pattern or the safeguards, where today everything is already on screen.
  That is the cost of core first.
- The record form is still a form.
  The only form drawn is a presentation, and the route and candidate forms need their own redesign.
- The `+ ~ -` marks need the page to diff the draft against the active revision on every refresh.
  The activation modal already does, so this is presentation work, but the page's `derived` data goes stale after an edit today, and these marks would too.

## Internal changes that would help

These are notes only; none is in the mockups.

- A way to validate one edit without applying it, so that a form can show the compiler's own message beside a field as the operator types.
  Without it the page has to carry its own validators for globs, ids and matchers, which can drift from the compiler.
- Diagnostics that carry the record id and the field, not a path into the document, so a message can sit beside the field it concerns.
- The compiled expansion of an offer selector.
  A presentation's detail could then list the models it will publish, instead of describing the rule.
- A snapshot that refreshes `derived`, the diagnostics and the revision history after an edit and an activation.
- Per-binding offer counts, and offers fetched per binding on request, instead of every offer in every snapshot.
- A mode where an edit applies live without a visible draft step.
  It would make activation and the `~` marks unnecessary for the common case, and it needs the draft and revision model to stay underneath, since lanes pin to revisions.
- The config edit as a patch of the fields changed, since it is a whole-row `PUT` today and drops fields the page does not show.

## Data that is not from the captures

The picker rows (`legacy-picker-0` to `14`), the rewrites, the classifiers, the upgrades and the compaction policy come from the generator in `src/policy/legacy.rs` and `src/picker.rs`, not the captures, because the captures clip every row.
The counts match the captures.
Illustrative, and marked so in the captions where it matters: the offer ids, context sizes and release months; the per-binding offer counts (only the 1,399 total, from 1,416 less the 17 other records, is derived); the publication states; the severity and record named by each problem; the draft's three changes; and the `from toker.toml` source of revision 1.
