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
- **Routes `matched by`.** `rules` and `defaults` are merged into one `matched by` column (`4 rules · 2 defaults`, `3 rules`, `1 rule · 2 defaults`, zero part dropped). With the full `now` text this table does not fit in 120 cells (about 127 needed), so the routes-table frame is now **135x40**, with three or more spaces before `matched by`. The route page and add-fallback frames stay 120x40.

## converged-06-narrow.html

- Rewrites at 100x24 and 80x24 use ruled panels (a boxed table would need two more rows than 24 allows). Height forces the only shedding: RULE becomes the single line under the table (`no cred  anthropic_sub is on with no credential`, `[enter] detail` at the right). Every column, including `hits` and `reach`, still fits at 80, so none was shed. The `else` text sets the minimum width of the match column.
- The non-current tabs collapse to numbers at 100 as well as 80, since the full strip (96 cells) plus `rev 9 live` does not fit in 100.
- The `claude-sub` route page at 100x24: everything fits (23 of 24 rows), so nothing is counted with a `…` line. The consent clause of the edge wraps to its own line under the edge so the grid fits.
- Providers at 80x24 opens on `anthropic_sub`. The `bindings` count column is shed (the detail carries it), and every PROVIDER line is kept. Numbers match `converged-05-config.html` (4 rewrites, 2 defaults, 1 route).
- Same checks as above: every row is exactly the claimed width (100 or 80) and 24 rows; font stack unchanged; screenshots looked at.
