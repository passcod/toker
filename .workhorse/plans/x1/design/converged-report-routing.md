# Converged frames 01, 02, 03, 08: report

## Drawn

All files are in `.workhorse/design/mockups/x1/`.

- `converged-01-rewrites.html`: rewrites at rest 120x40; the same at 135x50; trace; editing with the live refusal; after save.
- `converged-02-routes.html`: routes table with the ROUTE detail; the route page for `claude-sub`; adding a fallback inline in `sent to`.
- `converged-03-tables.html`: presentations; defaults; models (FAMILIES, UPGRADES, COMPACTION, then OFFERS taking the remaining rows).
- `converged-08-save-flow.html`: four states (open, refused, fixed, saved) cycled with CSS keyframes, 3 s each, no script, with a label cycling alongside; then the four static frames beneath.

Every frame is 120x40 except the 135x50 one.

## Decisions where the brief was silent or short

- **Columns come and go with uniformity.** At rest, `kind` (prefix) and the sends column (`${model}`) are uniform, so they sit in the scope heads. In the editing frame the glob row makes `kind` vary, so `kind` returns and the heads read `6 rules · → ${model}`. After save the sends column varies too, so both return and the heads read `6 rules`.
- **The fix in the save flow is `as asked`** (see "Changes after review"). A new rule's record name is derived from its fields: `claude-* → openrouter-messages`.
- **State 1 of the save flow is the form just opened.** Its control reads `[ctrl+s] fix 2 fields to save`, with the dim label in place of `save`.
- **Hits.** Unrecorded is a dim `-`, floors are `≥n`, the else rows carry hits too. Rule 2 shows `≥1` because the preview sample has a request that rule 2 caught. The D frame said "no requests in 24h", which contradicted D8.
- **Routes table.** See "Changes after review" for the merged `matched by` column. No lanes is a dim `none`. `codex` gets `off · falls to openrouter, billed`, so `sent to` there is `codex_sub → openrouter`.
- **Defaults frame.** The row for `openai_responses` reads `no default → nothing catches it`, and row 1 is selected, so the yellow row stays visible. `unprefixed/openai_chat` goes to `openrouter-chat`, matching the D1 else row rather than the A frame.
- **Models frame, no detail panel.** The brief gives none, so the controls for the selected UPGRADES row are on UPGRADES' last line. UPGRADES carries a head line `newest  5 upgrades · evidence none`, because `evidence none` is uniform and is said once there. The `↑` in UPGRADES is plain. FAMILIES holds the five authored names with their match pattern, an example and what it reads as; the gpt row shows two patterns (`gpt-* · openai/gpt-*`).
- **OFFERS arithmetic.** The brief's `… 170 above` / `… 236 more` assumes A3's 34-row window. Here the window is 14 rows and the selection (187 of 440) is inside it, so the counts are `… 180 above` and `… 246 more`.
- **Presentations.** The slug is said once in the scope head, with `→ openrouter-messages`. The record name (bold first line of PRESENTATION) is its include pattern, since the old `legacy-picker-4` name is not in the data sheet. `published no` is plain, since the unsynced marker is only in the header of one frame.
- **Trace.** The request goes in TRACE as a header line, as the brief says, so the REWRITES panel has no trace row. The caught row is selected and bold reversed with a bold `>` in the gutter.
- **Header status when long.** At 120 wide, the full strip plus `saved · rev 10 live · 2 lanes on rev 9 · claude settings not synced` does not fit, so the non-current tabs collapse to numbers with arrows, as the design system says. After the save the current tab reads `2 rewrites 15` and history would read 10.
- **`editing · rev 9 live`** is the header right side while a form is open, following D8.
- **Route page.** A blank row follows the name line and separates sections. While adding a fallback, the page's usual control line is replaced by the form's one control line (`[ctrl+s] fix model to save   [esc] cancel`), so controls are written once. The third candidate has no edge row yet. Controls use three spaces between them, per the design system, where the brief wrote two in places.
- **Claude settings not synced** is in exactly one frame so far: the after-save frame of 01. The after-save state in 08 omits it, so the clause is not duplicated. When I last looked, 04, 05 and 07 had no copy of it either.

## Disagreements with the brief's own data

- The brief says the route page reads `matched by 5 rules · 2 defaults` and the provider detail `rewrites 5`. The 14-rule table it also fixes has only **4** rules reaching `claude-sub` (anthropic_messages 1 and 2, openai_chat 5, openai_responses 3). I drew 4, so the page agrees with the table and the routes table's `rules` column sums to 14 (4, 3, 1, 1, 1, 1, 3). If frame 05 says `rewrites 5`, one of them needs changing; I did not invent a fifth rule.
- The D frames' provider detail got its 5 from a `claude-sonnet-*` rule that is not in the D1 table.

## Checks

- Width check from `mockups.md` on all four files: every row of every frame is exactly the claimed width (120 or 135) and every frame has the claimed row count; no RAGGED.
- Font stack: `"Iosevka Term", "Iosevka Fixed", "DejaVu Sans Mono", monospace` only; no plain `Iosevka`.
- Body grep of all four files: no `legacy`, no `green` class used, no JSON, no email addresses, no `@` handles; the only backgrounds are the reversed class. The old `.on-yellow` rule was dropped from the CSS.
- Screenshots in `/home/felix/.cache/workhorse-rig/passcod__toker/cards/x1/work/shots/converged/` for every frame, each looked at. Checked against the binding rules: panels end at their content with bare ground below (the models frame is exactly full because OFFERS scrolls); no sentence rows; controls on the last content line of the owning panel; fixed titles `RULE`, `ROUTE`, `TRACE`; nothing green; selected rows plain reversed.
- The animation: headless screenshots only capture the first frame, so I checked the keyframes by loading a copy with the delays shifted by -4 s. It showed state 2 with its label, so the cycle order and labels are right.
- The generator is outside the repo, in `/tmp/x1gen/`; the HTML files are static output.

## Changes after review

- **`sends` is `as asked` for the new rule.** In the edit frame and in all four states of 08, `←/→` on `sends` cycles `${model}` and `as asked`: the refused state shows `${model}  ←/→` and the fixed state shows `as asked  ←/→` with the reading `the name is sent unchanged`. After save the row's sends cell reads `as asked`; RULE `example` is `claude-haiku-4-5 → claude-haiku-4-5` and `overlaps` is `none: no rule above catches claude-*`. (01 frames 4 and 5, 08 states 2 to 4; state 1 only gained the arrows hint.)
- **Models has one selection.** UPGRADES keeps the reversed row; OFFERS has none, and its head reads `440 offers` instead of the position.
- **Routes `matched by`.** `rules` and `defaults` are merged into one `matched by` column (`4 rules · 2 defaults`, `3 rules`, `1 rule · 2 defaults`, zero part dropped). With the full `now` text this table does not fit in 120 cells (about 127 needed), so the routes-table frame was drawn at 135x40 (superseded: see Round 2 fixes, it is back at 120x40).

## converged-06-narrow.html

- Rewrites at 100x24 and 80x24 use ruled panels (a boxed table would need two more rows than 24 allows). Height forces the only shedding: RULE becomes the single line under the table (`no cred  anthropic_sub is on with no credential`, `[enter] detail` at the right). Every column, including `hits` and `reach`, still fits at 80, so none was shed. The `else` text sets the minimum width of the match column.
- The non-current tabs collapse to numbers at 100 as well as 80, since the full strip (96 cells) plus `rev 9 live` does not fit in 100.
- The `claude-sub` route page at 100x24: everything fits (23 of 24 rows), so nothing is counted with a `…` line. The consent clause of the edge wraps to its own line under the edge so the grid fits.
- Providers at 80x24 opens on `anthropic_sub`. The `bindings` count column is shed (the detail carries it), and every PROVIDER line is kept. Numbers match `converged-05-config.html` (4 rewrites, 2 defaults, 1 route).
- Same checks as above: every row is exactly the claimed width (100 or 80) and 24 rows; font stack unchanged; screenshots looked at.

## Round 2 fixes

Applied from `critique-round-2.md` section 4 with the coordinator's rulings. Width and font checks re-run on 01, 02, 03, 06 and 08 (all rows exactly the claimed width, Iosevka Term only); every touched frame re-shot and looked at.

- **#1 focus in the rule form.** Only the focused field is reversed. State 1 focuses `match` (`prefix  ▏` with `←/→`); states 2 and 3 focus `sends` (`${model}  ←/→`, then `as asked  ←/→`). Unfocused fields are plain, with no caret and no bold `glob`.
- **#2.** Save-flow state 1 (match and route empty) has no PREVIEW panel.
- **#4.** At 24 rows the summary line ends `[pgdn] more   [enter] open route`. At 80 the reason text is shed, leaving `no cred   [pgdn] more   [enter] open route`; at 100 the reason stays.
- **#5.** In the trace, link 2 is the reversed selection; the table row is bold only, with the bold `>`.
- **#6.** New fourth frame in 03: Models with focus in FAMILIES (reversed row, own `[e] edit   [n] new   [d] delete`; UPGRADES has no selection and no control line). The caption says `↑` past the top of UPGRADES moved focus there, not `↓` as the message said, because FAMILIES is the panel above UPGRADES in the stack. Change the caption if `↓` was meant for some other order.
- **#8, #20.** DEFAULT has no `for` row; `recent` reads `claude-sonnet-5  ≥3 in 24h`.
- **#9.** No rule names: the first line of RULE is `match → route` for every rule (`anthropic_sub/${model} → claude-sub`, `claude-* → openrouter-messages`).
- **#12.** The else row reads `else  default  → claude-sub` (01, 06, 08); `else  no default  → nothing catches it` is unchanged. At 80 columns the route column moved six cells left as a result.
- **#13.** RULE, PRESENTATION and DEFAULT share one control line (`[e] edit   [n] new   [m] move   [space] turn off   [d] delete   [t] trace   [enter] open route`); DEFAULT omits `[m] move`.
- **#14.** `[p] promote` kept on UPGRADES (when UPGRADES holds focus).
- **#15, #16.** No `↑` on the upgrade rows; the head reads `newest  5 upgrades`.
- **#17.** While `kind` is a column the scope head reads `6 rules · sends ${model}`; once `sends` is a column the clause goes. At rest it still reads `prefix → ${model}`, as the brief gave it.
- **#18.** Trace link 4 reads `4 route  →  claude-sub`; link 8 reads `8 provider  on  openrouter · credential in keyring`.
- **#19.** `moves when cold`, `cache allowed when cold` on the route pages, the narrow route page and COMPACTION.
- **#24.** `[m] move` is dropped from the ROUTES detail line; the route page keeps it.
- **Routes table width.** Back to 120x40. `now` is `no cred · billed fallback` for claude-sub and `off · billed fallback` for codex, with `billed fallback` yellow outside the reversed row. Columns are tighter, with two spaces between each pair.
- **Lanes (#10).** The 01-05 header `saved · rev 10 live · 2 lanes on rev 9` stands. The strip in that frame cannot read `6 history 10`: the long status forces the non-current tabs to collapse to numbers, so it shows `6`. At 120 columns the full strip (90 cells) and that status (38) do not fit together. Everywhere else it is `6 history 9` and `rev 9 live`.

## Round 3 fixes

Applied from `critique-round-3.md` section 4. Width and font checks re-run on 01, 02, 03, 06 and 08 (all rows exactly the claimed width, Iosevka Term only); every touched frame re-shot and looked at.

- **#1 reach.** Rules and else rows ending at `claude-sub` read `no cred · billed fallback`; at `codex`, `off · billed fallback`. `no cred` is yellow and `off` dim, `billed fallback` yellow outside the reversed row. Plain dim `off` stays on `claude-api` and `openai-chat`; `ok` and `nothing catches it` are unchanged. RULE's reach line reads `no cred · billed fallback  anthropic_sub has no credential, so openrouter serves it, billed`, and fits at 120. At 80 the column sheds to its first clause. The 100-wide summary line carries the full clause plus a shortened reason (`anthropic_sub has no credential`); the 80-wide one has `no cred` only. I also applied the same wording to the Defaults frame in 03 and its DEFAULT detail, for consistency.
- **#2.** The digit switcher is gone from the route page (02-02, 02-03, 06-03); a dim `1 of 7` sits at the right of the name line. The control line is `[e] edit   [n] add fallback   [m] move   [t] trace   [←/→] other routes   [esc] back`.
- **#6.** While a form is open the heads count saved records (`5 rules`, `2 candidates`); after the save the head reads `6 rules`.
- **#7, #8.** Trace link 4 is `4 route  claude-sub` with no arrow; `> caught` is bold (and reversed, as the selected link), like `> serves`.
- **#9, #10, #5 in 06-04.** `refresh url  built-in`; off providers are dim as whole rows. `none` is yellow on an `on` provider; in this frame that row is selected, so the yellow shows only in the detail (which has it).
- **#11.** The ROUTE detail line is `[e] edit   [n] add fallback   [t] trace   [enter] open route`.
- **#13.** UPGRADES rows begin at the route name (`claude-sub   claude-opus-4-7 → claude-opus-5`).
- **#14.** `[p] promote` kept.
- **#15.** COMPACTION reads `cold compactions → sonnet   cache allowed when cold`.
- **#17.** The `claude-*` RULE detail reads `match  glob  claude-* · sends as asked`.
- **#18.** The new-row placeholder in save-flow state 1 leaves untyped cells blank: only `6` and `prefix` remain. The form beneath keeps its own `-` for the empty route field.

## Round 4 fixes

Regenerated from `/tmp/x1gen` (which already carries the `refused · no route` label). The frame captions now get their ids from the generator, and a regeneration before any change reproduced the existing HTML exactly, so the ids and the label are as they were. Width and font checks pass on 01, 02, 03, 06 and 08; every file was re-shot and the changed frames looked at.

- **#1.** The refusal beside `sends` reads `a glob captures nothing, so ${model} would be empty:` and, wrapped, `send as asked, or match with a prefix or regex` (01-04 and the refused states of 08).
- **#2.** The route page head reads `handled by  3 on · 4 off: server safeguards · required capabilities · health circuits · classifier`, with the names dim, in 02-02, 02-03 and 06-03. At 100 columns it fits exactly (98 of 98 cells), so no shortening was needed. E's route frames hold only seven blocks, so there is no fourth off block other than `alias rules` (excluded) and `compaction` (which this page lists as on). I used `classifier`, a field of E's upgrade block. It is the weakest of the options, because an upgrade needs a classifier, so replace the name if you prefer. The coherent alternative is to make `alias rules` on and read `4 on · 3 off`.
- **#3.** The route page upgrade line reads `newest on claude-sub · claude-opus-4-7 → claude-opus-5`.
- **#4.** Trace link 7 reads `no newer model in claude-sonnet`.
- **#5.** At 80x24 the spend clause is kept as `no cred · billed` and `off · billed`, in the column and in the summary line. Nothing had to be shed: the longer text fits beside `hits` at 80.
- **#6.** The presentations else row reads `→ rewrites`.
- **#7.** In the edit states the head reads `5 rules · 1 new · sends ${model}` (01-04 and all four 08 states).
- **#8.** In save-flow state 1 a dim `needed` sits beside the empty `match` and `route` fields.
- **#2, revised: alias rules are on.** The route page head reads `handled by  4 on · 3 off: server safeguards · required capabilities · health circuits` (off names dim), replacing the `classifier` choice above. A fourth on line follows the other three: `alias  claude-* → anthropic/claude-* · on openrouter`. Applied in 02-02, 02-03 and 06-03. The 100x24 route page still fits, now using exactly 24 of 24 rows with nothing shed.

## Round 5 fixes

Caption ids kept; widths and fonts re-checked on 01, 02, 03, 06 and 08; files re-shot and the changed frames looked at. Nit 10 skipped as instructed.

- **S2.** The route page lane line reads `moves when cold · 2 warm · 5 cold` in 02-02, 02-03 and 06-03.
- **Nit 2.** The at-rest scope heads read `5 rules · prefix · sends ${model}` (01, 06). The presentations head keeps `→ openrouter-messages`, which points at a route.
- **Nit 5.** The add-fallback form's outgoing-name field is labelled `sends`, focused and reversed with the typed fixed model `claude-sonnet-5`, a caret, and `←/→` showing it cycles. The control line reads `fix sends to save`, and the refusal stays.
- **Nit 8.** At 100x24 there are two spaces between `subscription` and `as asked` (the model and reach columns moved one cell right).
- **Nit 9.** The routes tab's ROUTE detail line gains `[m] move`: `[e] edit   [n] add fallback   [m] move   [t] trace   [enter] open route`.
- **Nit 11.** Every refusal is bold and red: the sends refusal (01-04, 08 refused states) and the model refusal in 02-03.

## Round 6 polish

Caption ids kept; widths and fonts re-checked on 01, 02, 03, 06 and 08 (no ragged rows); files re-shot and the changed frames looked at.

- **S1.** The column is headed `hits 24h` in the rewrites tables (01, 06, 08) and the defaults table (03). It fits at 80 columns too, so no frame falls back to `hits`.
- **S2.** PRESENTATION reads `published  yes`.
- **Nit 3.** Add-fallback reads `sent to  2 candidates · 1 new`.
- **Nit 6.** RULE has its own `sends` row between `match` and `sends to`: `as asked` for the new rule (01-05, 08-04, 08-08) and `${model}` for the at-rest rule.
- **Nit 7.** PREVIEW shows a session id on every row, in a labelled `session` column (`ses-free`, `ses-hot`, `ses-crow`, `ses-moth`, `ses-wren`), dim on the two rows that don't change.
- **Nit 8.** The routes table's last column is headed `reach`.
