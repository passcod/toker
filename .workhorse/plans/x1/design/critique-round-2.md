# Critique, round 2: the converged mockups

Fresh critic, screenshots only: the 29 frames in `shots/converged/`, the dashboard reference, the two NOW captures, the design system, the converged brief, the operator's steer and the taste ledger.
Judged as one design.

## 1. Verdict

Close, but not converged.
The structure is right and holds across every frame: one grid, content-sized panels, colour only as information, the chain in the tab strip, the table passing the blur test where the NOW screens never did.
What stands in the way is a layer of small disagreements, between frames and between the frames and the `?` overlay, that would each confuse an operator at the keyboard: focus shown in two fields at once, `enter` meaning three things, a save key that differs by page, a preview that reports changes for a rule that has no match, and a few sentences and raw enums that crept back onto the page.
No critical findings; too many suggestions for a clean round.

## 2. Protocol notes, by frame

**Rewrites at rest (01-01, 01-02).**
Five seconds: the reversed row, then the three bold scope heads, then the yellow `no cred` and `nothing catches it`, then the control line.
That is the right order.
Blur keeps the rail, the rows and the two boxes; the header vanishes.
Greyscale survives: `no cred` and `nothing catches it` are words, not just yellow.
Colour inventory: yellow for two readings, dim for position, headers, `off`, `else` and the rail, reversed for selection, the two header chips; nothing else.
One type size, bold on heads and the record name.
Alignment: `match`, `route`, `reach` headers sit on their values; `hits` is right-aligned to the box.
Subtraction: the `else` row's `default anthropic_messages` repeats the scope head two lines above it.
At 135x50 the lower 17 rows are bare, which call 7 settles.

**Trace (01-03).**
The answer line is bold and first, the chain reads top to bottom, the table dims what the request never reached.
The frame breaks call 11: row 2 is reversed in the table while the control line says `[enter] open link` and no link in TRACE is marked.
The chain's second column holds a verdict on seven lines and a bare name on two (`route  claude-sub`, `provider  openrouter`).

**Editing a rule (01-04, 08-01 to 08-08).**
The inline form, the reading beside each field, the red refusal, the dim `fix sends to save`: all as the steer asked.
Two focus marks: a text caret after `claude-*` in `match`, and the `sends` field reversed.
The PREVIEW panel in state 1 (empty form, `prefix -`, route `-`) already reads `3 of 5 recent requests would change route` with `rule 6 → openrouter-messages` on three rows.
When `kind` becomes a column the scope head reads `6 rules · → ${model}` with the arrow orphaned.
The `kind` column appears on `n`, before any row differs.

**After save (01-05, 08-04).**
Header as the lifecycle section specifies; tabs collapse to numbers because the status is long, which the design system permits.
The `sends` column appears because one row now differs: correct.
The new rule's name is `claude-* → openrouter-messages`, derived; the existing rule's is `strip anthropic_sub/`, authored.
The form has no name field.

**Routes (02-01).**
`matched by` merges the brief's `rules` and `defaults` columns into one and reads better than two; not a finding.
The ROUTE detail's edge line is the best new line in the set.
The `lanes` column totals ten; the history tab's `lanes` column totals seven.

**Route page (02-02, 06-03).**
Three bold section heads, candidate 1 reversed, controls last, ground bare.
`move_when_cold` and `allow_when_cold` are raw enum identifiers inside prose lines.
At 100x24 the edge line wraps cleanly to two rows and the page still fits.

**Adding a fallback (02-03).**
Focus is unambiguous here: one reversed field with the caret inside it.
This is the form the rule editor should match.

**Presentations (03-01).**
A1's fix applied: fifteen distinct rows, `else  no slug equals it  → 2 rewrites` closing the group into the next tab.
Control line order is `[e] edit  [m] move  [n] new`, against `[e] edit  [n] new  [m] move` on RULE, and it has no `[t] trace`.

**Defaults (03-02).**
`for  names no presentation or rewrite caught` is a sentence explaining the mechanism, which call 8 forbids on the page.
`recent  ≥3 in 24h` drops the example request that RULE's `recent` carries.

**Models (03-03).**
Four panels stacked and sized to content, OFFERS taking the rest and its window counts adding up (180 + 14 + 246 = 440).
Only UPGRADES carries a control line, so controls follow focus; nothing on the page or in the overlay says how focus moves to FAMILIES or COMPACTION.
`[p] promote` is not in the overlay.
`↑` ends every upgrade row: uniform, so it carries nothing, and it is not in MARKS.
`evidence none` in the UPGRADES head is unexplained anywhere.

**History (04-01, 04-02).**
Newest first, `live` bold, a sentence per revision, the diff in `+ ~ -` words, `[a] already live` dim with the reason in place of the label.
The armed frame does what the design system asks: the control inverts and the last line says what is lost and how to confirm.
Rev 1 reads `create 7 routes and 4 defaults` while the other tabs hold 14 rewrites, 15 presentations, 5 families and 5 upgrades that no later revision creates.

**Providers (05-01, 06-04).**
Opens on the problem; the detail's `credential  none` is yellow; the off rows are dim.
Data agrees with every other frame (rewrite, default and route counts per provider all reconcile).

**Settings (05-02).**
Grouped rows with rail heads, one row in edit with a red refusal: good.
`(default)` appears nine times and `(the built-in one)`, `(the lane's own TTL tier)` are explanatory parentheticals.
The save key is `[enter]`; every other form in the set saves with `[ctrl+s]`.

**Narrow (06-01, 06-02).**
At 24 rows the RULE panel gives way to a one-line summary ending `[enter] detail`, so `enter` now does something it does nowhere else.
At 80 columns the `else` label is seven cells longer than the longest rule and pushes the `route` column right by that much.

**Fresh install (07-01).**
The rail, the three `else  no default  → nothing catches it` rows and `[n] needs a route first   [4] go to routes` are right.
The dim `-` row where rules would go uses the mark the overlay defines as `not recorded, which is not zero`, under a head that says `0 rules`.

**`?` overlay (07-02).**
Complete for the table, history, route, trace and edit contexts; `armed` and `live` are documented.
Missing: `p`, `↑`, and how to move between stacked panels.
A sliver of the RULE panel's first letters (`RU`, `s`, `m`, `e`, `s`, `r`, `o`, `r`, `[`) shows between the page border and the popup's left edge.
The table behind the overlay lays `route` at column 33 and `reach` at column 86 where 01-01 at the same size has them at 36 and 71, and row 2's hits reads `-` where every other frame reads `≥1`.

## 3. Cross-frame consistency

Holds: header chips, tab strip wording and arrows, `rev 9 live` at rest, panel titles upper case in the border, record name bold on the first line, controls on the last content line with three spaces between, yellow for `no cred` / `nothing catches it` / `billed` / `allow` / `credential none` / `not synced` / cache loss, red for refusals only, no green, dim for `off`, reversed for selection and armed controls.
Counts reconcile across providers, routes, rewrites and defaults.

Does not hold:

- **Save key.** `[ctrl+s]` on the rule form and the fallback form; `[enter]` on settings.
- **`enter`.** `open route` on every detail, `open link` in the trace, `detail` at 24 rows.
- **Focus.** One mark in the fallback form, two in the rule form, a reversed table row plus `open link` in the trace.
- **Control lines on the three table details.** RULE `e n m space d t enter`; PRESENTATION `e m n space d enter`; DEFAULT `e n space d enter`.
- **Lanes.** After save: `2 lanes on rev 9`. History: `2 warm` on rev 8, `2 warm lanes still on rev 8`. Routes: ten lanes; history: seven.
- **Record names.** `strip anthropic_sub/` authored, `claude-* → openrouter-messages` derived, no name field.
- **Column positions.** 07-01 and 07-02 lay the rewrites table differently from 01-01 at the same size.
- **Overlay vs keys.** `p` drawn, not listed; `↑` drawn, not listed.

## 4. Findings, ranked

Severity on the design-judgement scale: critical, suggestion, nit.

1. **Suggestion. Rule form (01-04, 08-01, 08-02, 08-05, 08-06): show focus once.** The caret sits after `claude-*` in `match` while `sends` is reversed. Make the fallback form (02-03) the pattern: the focused field alone is reversed, the caret is inside it when it takes text, and `←/→` marks it when it cycles. No caret in an unfocused field.
2. **Suggestion. Save flow state 1 (08-01, 08-05): the preview must be quiet for an empty rule.** The form reads `prefix -` with route `-`, and PREVIEW says `3 of 5 recent requests would change route`. Leave PREVIEW out until the match and route fields both hold a value, or draw the panel with `-` in the `with this rule` column and no bold line.
3. **Suggestion. One save key.** Settings (05-02) saves with `[enter]`; the rule and fallback forms save with `[ctrl+s]`, and the overlay teaches `ctrl+s save, once nothing is refused`. Change settings to `[ctrl+s] fix value to save   [esc] cancel`.
4. **Suggestion. `enter` means one thing.** At 24 rows (06-01, 06-02) the summary line ends `[enter] detail`; everywhere else `enter` opens the route and the overlay says so. Give the collapsed detail a key that works at every height (the overlay already reserves `pgup/pgdn  scroll the detail`; `pgdn` opening the detail as a popup when it has no panel would read naturally), and keep `[enter] open route` on that line.
5. **Suggestion. Trace (01-03): one focus, matching call 11.** Either the trace holds focus, in which case a link is reversed and the table row is not, or the table holds it and the control line cannot say `[enter] open link`. Reverse link 2 (`> caught`) as the default selection and leave the table's row unreversed, bold only.
6. **Suggestion. Models tab (03-03): say how focus moves between stacked panels.** Four panels, one with a selected row and a control line, and no key on the page or in the overlay moves to another. Add one line to the overlay (`↑/↓` past a panel's edge moves to the next panel, or a named key), and draw FAMILIES with focus in one frame so the rule "controls follow focus" is seen.
7. **Suggestion. Settings (05-02): parentheses are not a device in this language.** `(default)` nine times, `(the built-in one)` twice across the set, `(the lane's own TTL tier)` once. Dim already means subordinate. Write the value and let dim carry "not set here": `quota gate  on`, `default notice style  built-in`, `cold idle minutes  lane TTL`, `refresh url  built-in` on the provider detail.
8. **Suggestion. Defaults (03-02): drop the `for` row.** `names no presentation or rewrite caught` is the sentence call 8 moved to the overlay. The title DEFAULT, the bold scope name and the overlay's `nothing catches it` entry already say it.
9. **Suggestion. Record names: decide whether rules are named.** The form has no name field, so a saved rule's first line must be derived; then derive it for every rule the same way (`anthropic_sub/${model} → claude-sub`, matching the new rule's `claude-* → openrouter-messages`), or add a `name` field to the form and keep `strip anthropic_sub/`. Call 5 retired generated names; it did not settle whether authored ones exist.
10. **Suggestion. Lanes agree between frames.** After save (01-05, 08-04) reads `2 lanes on rev 9`; history (04-01) pins the two warm lanes to rev 8 and sums to seven lanes where routes (02-01) sum to ten. Either move the warm lanes to rev 9 in history (`9 live … 2 warm · 5 cold`, detail `2 warm lanes still on rev 9`) and add the three openrouter-chat cold lanes to some revision's count, or change the save header to `2 lanes on rev 8`.
11. **Suggestion. Fresh install (07-01): no `-` row.** `-` is defined in the same set as `not recorded, which is not zero`; here the head says `0 rules`, which is none at all. Run the rail from the head straight to the `else` row. The brief asked for `-`; the design system's vocabulary binds over it.
12. **Suggestion. Rewrites `else` row: drop the repeated scope name.** `else  default anthropic_messages  → claude-sub` names the scope the head two lines up already names, and at 80 columns (06-02) the label pushes `route` seven cells right. Read `else  default  → claude-sub`, with `no default` kept where there is none.
13. **Suggestion. One control line shape for RULE, PRESENTATION and DEFAULT.** Same order (`[e] edit  [n] new  [m] move  [space] turn off  [d] delete  [t] trace  [enter] open route`), `[m] move` omitted only where rows are unordered (defaults), and `[t] trace` on all three or none, since the overlay lists `t` as a page key.
14. **Suggestion. UPGRADES (03-03): `[p] promote` is not in the overlay.** Add `p` to the overlay with what it does, or remove the control.
15. **Nit. UPGRADES: drop the `↑` on every row.** It is uniform, so it tells nothing, and MARKS does not list it; the dashboard legend gives `↑` a different meaning.
16. **Nit. UPGRADES head: `evidence none` is unexplained.** Write it in words or list it in MARKS.
17. **Nit. Scope head while `kind` is a column (01-04, 01-05, 08-*): `6 rules · → ${model}`.** The arrow was attached to `prefix`; alone it is cryptic. Read `6 rules · sends ${model}`, and drop the clause entirely once `sends` is itself a column (01-05 already does).
18. **Nit. Trace second column: verdicts or names, not both.** Links 4 and 8 put a name where the others put a verdict. `4 route  →  claude-sub` and `8 provider  on  openrouter · credential in keyring` keep the column to one kind.
19. **Nit. Raw enums in prose lines.** `move_when_cold`, `allow_when_cold` on the route page and COMPACTION. Write `moves when cold`, `cache allowed when cold`.
20. **Nit. DEFAULT `recent` matches RULE's shape.** `recent  claude-sonnet-5  ≥3 in 24h`, the example request then the floor.
21. **Nit. Overlay popup (07-02): cover the RULE panel's text column.** A column of first letters shows down the left edge. Widen the popup one cell each side, or inset it so a whole word shows.
22. **Nit. 07-01 and 07-02 use the same grid as 01-01.** Same width, same table: `route` and `reach` should sit where 01-01 puts them, and row 2's hits should read `≥1`.
23. **Nit. History rev 1 accounts for what exists.** Call 9 makes rev 1 the one bundled revision; its sentence should cover the rewrites, presentations, families and upgrades the other tabs show, or later revisions should.
24. **Nit. Routes tab `[m] move` (02-01).** Routes are unordered; with focus in the ROUTES table the key has nothing to move. Drop it from that detail's line, or make it visibly act on the candidate rows.

## 5. Counts

Critical: 0. Suggestion: 14. Nit: 10.

## 6. Backlog, set aside

Not findings against these frames; cells the matrix does not yet have, or features for a later round.

- OFFERS shows one binding's catalogue; choosing the binding and narrowing 440 offers.
- Frames for: `[d] delete` armed; `[m]` lifting a row and dropping it; the `t` prompt that names the request to trace; `enter` on `handled by` listing the off steps; `[c] compare a request`; the providers table with `anthropic_sub` unselected, to see whether `none` is yellow in the row.
- A provider's `[e] enter key` form.
- `claude settings not synced` as something the operator can act on from the header or the providers tab.
