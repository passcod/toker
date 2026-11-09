# Critique, round 4

A fresh read of the 30 converged frames against `design-system.md`, `mockups.md`, `brief-converged.md` and the routing specs, with no earlier round read.

## 1. Verdict

Close, but not finished.
The structure holds: an operator can read where a request goes from the rewrites table, confirm it with a trace, see why it is billed, and fix it in place with a form that refuses until it is right.
Every binding rule I could check mechanically passes: every row is the claimed width, the font stack names Iosevka Term, no green is applied inside any frame, the selected row carries no colour, no JSON or regex notation appears, nothing is named `legacy-`, and every count agrees across every frame it recurs in.
What remains is wording and teaching.
The one refusal shown tells the operator to fix a different field from the one the control line names, the route page carries a sentence that only makes sense inside a trace, the `?` overlay leaves the lifecycle's own vocabulary undefined, and the 80-column table sheds the spend warning before anything cheaper.
None of these misleads badly enough to be critical, but there are more than three of them, so this is not a clean round.

## 2. Per frame

### converged-01-rewrites (01 to 05)

01, 02: the table reads well at both widths.
Three scope rails, position dim, `else` row naming the default and its reach, `hits` right-aligned with `-` and `≥n`.
The RULE detail is sized to content and its controls sit on its last line.
The `reach` reason in prose (`anthropic_sub has no credential, so openrouter serves it, billed`) is the clearest line on the page.
03: the trace is the best frame in the set.
Answer first, numbered chain, `↓` and `>` gutter marks on the table, unreached rows dimmed, and the selected link reversed for `[enter] open link`.
04: the edit opens inline under the row, the kind column appears because it is no longer uniform, and the scope heads switch from `prefix → ${model}` to `sends ${model}` to match.
The refusal on `sends` says `use a prefix or a regex` while the control line says `fix sends to save`; see finding 1.
PREVIEW is sized to content and says one thing.
05: the header reads `saved · rev 10 live · 2 lanes on rev 9 · claude settings not synced` with the tabs collapsed to numbers to make room, which is the design system's stated order.
The `sends` column appears now that one rule differs, and the scope heads drop the uniform clause.

### converged-02-routes (01 to 03)

01: `matched by` folds the brief's `rules` and `defaults` columns into one, which reads better than two mostly-empty columns.
The ROUTE detail is the candidate chain with the edge line between candidates, exactly as the brief draws it.
`[m] move` is absent here and present on the route page, which is right: the table's selection is a route, and routes are not ordered.
02: the route switcher is `1 of 7` plus `[←/→] other routes` rather than the brief's seven reversed chips.
That is a sound deviation; seven reversed cells would have been the loudest thing on the page.
The `handled by` head says `3 on · 4 off` and nothing on the page or in the overlay reaches the four; see finding 2.
The upgrade line says `no newer model in sonnet`, a trace reading on a page with no request in view; see finding 4.
03: the fallback form is right: a dim reading beside `binding`, a red refusal beside `model`, one control line, and the page's own control line gone while the form owns the keys.

### converged-03-tables (01 to 04)

01: presentations is dense but legible, and the PRESENTATION detail's `overlaps  row 6 z-ai/glm-*-flash  kept apart by except *-flash*` is exactly the kind of answer a firewall operator wants.
The else row's `→ 2 rewrites` reads as a count at first glance; see finding 6.
`published  no` is plain, while the brief colours publication not synced yellow; it is unclear whether this is a setting or a sync state (nit).
02: defaults is clean.
The `openai_responses  no default  → refused · no route` row is the right way to show an absent default, and the DEFAULT detail correctly has no `[m] move`.
03, 04: FAMILIES, UPGRADES, COMPACTION stacked and sized to content, OFFERS taking the rest with `… 180 above` and `… 246 more` summing to the stated 440.
Moving focus from UPGRADES to FAMILIES moves the control line with it, so UPGRADES' rows shift down a cell; the two panels trade a row so nothing below moves (nit).

### converged-04-history (01, 02)

01: newest first, `live` bold on rev 10, `what changed` in words, and the diff as `+ ~ -` lines.
`[a] already live` is dim with the reason in place of the label, per the design system.
The lanes column reads `8` on rev 10 and `2 warm` on rev 9, two formats for one column (nit).
02: arming is right: `[m] move running lanes` reversed and bold, the panel's last line names the two lanes that lose their cache and how to confirm or back out.

### converged-05-config (01, 02)

01: opened on `anthropic_sub` because it is the problem.
Off providers are dim throughout the row.
The PROVIDER detail is label/value rows with no section heads and `credential  none` yellow; the table cell is in the reversed row so its colour is not visible there, which is correct under the plain-reversed rule.
02: settings grouped by rail, one row in edit with the refusal beside it, one control line.
`idle-sleep lock` is the only hyphenated label on the page (nit).
`store ... set by TOKER_DB` names an environment variable the repository's own instructions do not; sample text, but an implementer will copy it (nit).

### converged-06-narrow (01 to 04)

01: at 100x24 the panels go ruled, the table takes every row, and the detail collapses to one line with `[pgdn] more`, which the overlay teaches.
02: at 80x24 the `reach` column drops `· billed fallback` and keeps `no cred`, so the spend disappears; see finding 5.
The tabs collapse to numbers and keep their arrows, as the brief says.
03: the route page at 100x24 wraps the edge line onto two rows and still ends on its control line.
04: providers at 80x24 sheds `bindings` first, which is the right column to lose.

### converged-07-empty-keys (01, 02)

01: the fresh install keeps the three rails with `0 rules` heads and `else  no default  → refused · no route`, and the only controls are `[n] needs a route first` (dim, reason in place of label) and `[4] go to routes`.
No narration, and the header reads `no policy yet` instead of a revision.
02: the overlay fits in 32 rows, keys left, MARKS and the contextual sections right, `? or esc close` in the border.
Every key drawn as a control in the other frames appears here, including `a`, `p`, `pgdn`, and the edit keys.
The vocabulary the lifecycle leans on (`warm`, `cold`, `pinned`, `slug`) is not in MARKS; see finding 3.
`p` is described as `trust the newest model now, without waiting for evidence` while the control reads `[p] promote` (nit).

### converged-08-save-flow (01 to 08)

The four states are consistent with the 01 frames and with each other, and the animated and static copies are identical.
State 1 shows the new row's `kind` column appearing at once and the control line reading `fix 2 fields to save` with nothing beside the two empty fields (nit).
State 2 is the refusal, state 3 shows `as asked` with its reading `the name is sent unchanged` and the control line now `[ctrl+s] save`, state 4 is the saved page without the publication clause, so the clause does appear in exactly one frame as the brief asks.

## 3. Cross-frame consistency

Counts agree everywhere they recur.
The 14 rules split 5, 5, 4 and reach the routes exactly as the routes table's `matched by` and the providers' `used by` say; the setup revision's totals plus the later revisions arrive at today's 15, 15, 4, 7 and 11; `… 180 above` plus 14 shown plus `… 246 more` is the 440 offers; and the lanes are 2 warm and 8 cold in the routes table, the route page, the after-save header and both history frames.
Names follow the data sheet throughout, and the session ids are `ses-free`, `ses-hot` and `ses-crow` in the preview and the arming line alike.

Colour is by function and only by function: `no cred`, `billed`, `billed fallback`, `refused · no route`, `allow` and `claude settings not synced` are yellow; refusals are red; `off` and unreached rows are dim; identities are plain.
Controls are `[key] label` with three spaces between, and the same key means the same verb on every panel (`e`, `n`, `m`, `d`, `t`, `enter`, `space`, `esc`).

Two wording inconsistencies: the route page's upgrade line is copied from the trace (finding 4), and the trace writes the family as `sonnet` where FAMILIES names it `claude-sonnet` (nit).
One structural one: the else row's arrow points at a route in rewrites and at a tab in presentations (finding 6).

## 4. Ranked findings

### 1. The refusal names one fix and the control line names another

Severity: suggestion.
Frames: converged-01-rewrites-04, converged-08-save-flow-02, converged-08-save-flow-06.
The refusal beside `sends` reads `a glob captures nothing, so ${model} would be empty: use a prefix or a regex`.
The control line reads `[ctrl+s] fix sends to save`.
Changing the match kind fixes the match field, not the sends field, and the flow then fixes it by setting `sends` to `as asked`, which the refusal never mentions.
An operator following the red text goes to the wrong field.
Fix: have the refusal name the field it sits on first: `a glob captures nothing, so ${model} would be empty: send as asked, or match with a prefix or regex`.

### 2. The off handlers cannot be reached from anything the screens teach

Severity: suggestion.
Frames: converged-02-routes-02, converged-02-routes-03, converged-06-narrow-03, converged-07-empty-keys-02.
The route page says `handled by  3 on · 4 off` and lists the three that are on.
The brief says `enter` on the head lists the off ones, but no frame shows that and the overlay's ON A ROUTE section does not say it, so an operator who wants to turn a handler on for this route has no path to it.
Fix: add `enter on handled by  list the off handlers` to ON A ROUTE, and either draw one frame with the list open or write the four off names dim on the head's line (`4 off: …`) so the operator can at least see what exists.

### 3. The lifecycle's vocabulary is not in MARKS

Severity: suggestion.
Frames: converged-07-empty-keys-02, and the words as used in converged-02-routes-01, converged-02-routes-02, converged-04-history-01, converged-04-history-02, converged-03-tables-01.
`warm` and `cold` decide what moves on a save and what the arming line threatens to lose; `pinned` appears on the route page; `slug` is the presentations tab's whole model of itself.
None is defined in MARKS, which is the contract the design system makes for anything an operator reads as a mark.
Fix: add rows for `warm · cold` (a lane whose prompt cache is still live, and one whose cache has lapsed or never existed), `pinned` (a lane held on its revision until cold), and `slug` (the model name a client sees a presentation under).

### 4. The route page's upgrade line is a trace reading

Severity: suggestion.
Frames: converged-02-routes-02, converged-02-routes-03, converged-06-narrow-03.
`upgrade  newest on claude-sub · no newer model in sonnet` only means something for a sonnet request, and the route page has no request in view.
The UPGRADES table on the models tab says what the upgrade on claude-sub actually does: `claude-opus-4-7 → claude-opus-5`.
Fix: write the upgrade's own reading on the route page, `newest on claude-sub · claude-opus-4-7 → claude-opus-5`, and keep the per-request `no newer model in claude-sonnet` for the trace.

### 5. The 80-column table sheds the spend before anything cheaper

Severity: suggestion.
Frames: converged-06-narrow-02.
At 80 columns `reach` shows `no cred` and `off` with the `· billed fallback` clause gone, so three rules that are silently costing money read like rules that are simply broken.
The brief colours a spend yellow because it is the thing the operator most needs to notice, and the shedding order drops it first.
Fix: shorten the clause rather than drop it, `no cred · billed` and `off · billed`, and shed the rule position column or narrow `route` before the spend clause goes.

### 6. The presentations else row points at a tab where the others point at a destination

Severity: suggestion.
Frames: converged-03-tables-01.
`else  no slug equals it  → 2 rewrites` means "falls through to tab 2", but `2 rewrites` reads as a count, and in the rewrites tab the else row's arrow points at a route.
Fix: write `→ rewrites` (the tab is named, not numbered, in every other scope head), or `→ falls to rewrites`.

### 7. Nits

- converged-04-history-01, -02: the lanes column reads `8` and `2 warm`; write `8 cold` so the column has one shape.
- converged-07-empty-keys-02 and converged-03-tables-03: `[p] promote` on the panel and `trust the newest model now` in the overlay; put `promote` in the overlay's text so the key is findable by its label.
- converged-08-save-flow-01, -05: `fix 2 fields to save` with nothing beside the empty `match` and `route`; a dim `needed` beside each would make the count traceable.
- converged-03-tables-01: `published  no` is plain; if it is a sync state it should be yellow per the brief, if it is a setting say so in MARKS.
- converged-01-rewrites-03: `no newer model in sonnet` names the family as `sonnet` where FAMILIES names it `claude-sonnet`.
- converged-05-config-02: `idle-sleep lock` is the one hyphenated label; and `set by TOKER_DB` names a mechanism the repository does not document, so pick sample text that cannot be copied as fact.
- converged-01-rewrites-04 and the save-flow edit states: the scope head says `5 rules` while six rows show; `5 rules · 1 draft` would say what the count excludes.
- converged-03-tables-03, -04: the control line moves with focus and shifts UPGRADES' rows a cell; fine, but worth knowing before the implementer wonders why rows jump.

## 5. Counts

Critical: 0.
Suggestion: 6.
Nit: 8.

## 6. Backlog

- A `?` overlay for the config page, drawn once, so its `[e] enter key` and `[space] turn off` are taught the same way routing's are.
- How OFFERS picks its binding: the panel shows one of several and nothing switches it.
- The `[t] trace another` and `[c] compare a request` prompts: what the operator types and how a model name is completed.
- Editing a candidate's edge policy (`billed: allow`, `ambiguous retry: deny`, `cache: preserve warm`): no frame shows the form.
- Reactivating an old revision armed, the counterpart to the move-lanes frame.
- The resolver spec says a prefixed identifier naming a disabled provider is answered as having no route; the design sends it down the route's fallback chain (`codex_sub/${model} → codex  off · billed fallback`).
  The design is what the user asked for, so the spec needs the matching edit when this is implemented.
- Fresh-install frames for routes and providers, since `[4] go to routes` on an empty rewrites tab lands on a table this set never shows empty.
