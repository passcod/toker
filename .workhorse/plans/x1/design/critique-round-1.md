# Critique, round 1: five directions for the settings and routing pages

Fresh critic.
I read the judgement method, the TUI design language and survey sections 1 and 2, then every one of the 43 frames at full size, plus blurred and greyscale renders of the six main frames side by side and 2x crops of the dense regions of A2, B8, C4, D1, E1 and E8.
I did not read the direction notes, the brief, the log, the ledger or the mockup sources.

Row counts below are read off the 120x40 frames: the body under the header is 38 rows, and "empty" means a row inside a drawn box carrying nothing but the box.

## 1. Calibration: what the dashboard does that these must match

1. **Panels are sized to their content, and only the lists flex.**
   TOKENS is eight rows because it has eight readings; CACHE REBUILDS is two; SESSIONS and CONTEXT absorb whatever is left.
   So the frame is full from top to bottom and the eye reads it as one page of readings, not as boxes with something in the corner.
2. **Colour sits on readings and nowhere else.**
   `200k` is yellow because a known non-native ceiling is worth noticing; `7m ago` is red because freshness is past five minutes; the hit-rate bar and `100%` are red because they are exhausted.
   Labels are dim, values are plain, names are plain.
   Nothing is coloured because of what it *is*, only because of what it *reads*.
3. **Two left edges per panel and a right edge for verdicts.**
   Labels share column 1, values share one column, bars start at one column in both TOKENS and RATE & QUOTA, and every number is right-aligned in a tabular column.
   The right edge of the frame carries the clocks and verdicts, so the line reads label, value, bar, verdict.
4. **The page says nothing about itself.**
   No key hints, no footer, no sentence explaining what a panel is for.
   Absence is written in the fixed vocabulary (`no session with enough history yet`, `none — every prefix held`, `estimating`, `-`), and the only chrome is the `┌TITLE─` box.

Five-second impression of the reference: the red hit-rate bar, then the yellow `200k` and the quota percentages, then the numbers.
That is the right order: the readings that need attention lead, the rest recedes.
Under blur the surviving shapes are the bars and the dense number columns, which are the content.

For comparison, NOW fails all four: JSON in list rows, a `… 1378 more records` list of mixed record kinds, two half-empty boxes, and a tab named `4 preview, revisions & activation`.

## 2. Per direction

Across all five, the type inventory is the same and passes: one size, three weights (bold, plain, dim) plus reverse for focus.
I do not repeat that below.

### A: keep the skeleton, polish every part

**Five-second impression (A1).** The red message line under the header, then the bold reversed tab `1 presentations`, then the reversed selected row.
The content, the fifteen presentations, comes fourth.
Wrong order: an error about `settings.json` is leading a routing page.

**Blur.** Two tall boxes side by side survive, with a smudge in the top third of each.
The boxes are what you see, not the records.

**Greyscale.** Hierarchy survives: selection is reverse, the `~` change marker is a glyph, dim prefixes stay dim.
The one casualty is the `billed fallback` and `no credential` badges on selected rows, which depend on a yellow background and become a slightly different grey block.

**Colour inventory.**
Yellow: `billed fallback`, `billed spend`, `disabled`, `no credential`, `not configured` (spends or needs noticing: justified, except `disabled`, which is dim in one place and yellow in another on the same frame A2).
Red: the whole Claude-settings message line, the validation message in A8, `rejected` nowhere.
Blue and DarkGray: the header chips.
A yellow *background* with black text appears on `billed fallback` (A2) and `no credential` (A4): the design language reserves Black on Yellow for the armed `burn` control.

**Alignment.** Within the list, label column at 2, slug at 11, except at 28; good.
A4's `state` header sits one cell right of its `off`/`on` values.
Controls change form between frames: `e edit  d remove` (A1), `[s] store a key  [space] turn off` (A4), `[p] promote this model` (A3).

**Subtraction.** The `stage 1 · exact name` and `stage 4` sub-headers narrate the resolver; the detail's `a Claude picker row, listed as…` repeats the list header; `only a model Toker has served can be promoted` explains the refusal before it happens.

**Against the reference at 120 wide.** The dashboard's panels end where their content ends; A1 draws two 38-row boxes for 17 rows of list and 12 rows of detail, leaving 22 and 26 empty rows.
At 100x24 (A5) the same page is full and reads well: the fault is the frame, not the content.

**Operator's words.** Tables: partly, lists with sub-sections but no columns for rule order in rewrites (the rules tab is not shown).
No JSON: yes.
Live validation: yes (A8, beside the field, `[ctrl+s] fix 1 field first`).
Core first: yes, with `enter more fields`.

**Findings, ranked.**

1. Drop the yellow-background badges; write `billed fallback` and `no credential` in yellow foreground on the plain row, and on a reversed row let them read as plain reversed text, as every other value does.
2. Take the Claude-settings message out of the header area on every frame; show it once as the `publications` state in the revisions tab and as a yellow clause in the page status (`claude settings not synced`), not as a red sentence above the content.
3. Size the DETAIL panel to its content and give the spare rows to the list, as the dashboard gives them to SESSIONS; at 120x40 that means the list shows all fifteen presentations and the box ends under `d remove`.
4. Pick one control form and use it everywhere: `[e] edit   [d] remove`, three spaces apart, on the panel's last content line.
5. Replace the PROBLEM paragraph (A2, A4) with two lines in the label/value grid: `credential  none` in yellow and a control `[s] store a key`; cut `Tab opens the config page; s there stores a key.`
6. Drop the `stage N · …` sub-headers from list sections; `routes  7` and `defaults  5` carry everything the operator needs, and the resolver order belongs in the `?` overlay.
7. Centre the edit popup (A8) and clear behind it; it floats in the upper third over both panels while the DETAIL still shows the same record's old values behind it.
8. Cut the `keep  1` field or label it (`keep newest 1`); it is the only field on the form whose meaning cannot be read from the row.
9. In the empty state (A7) say it once: `no presentations yet` in the list and the two controls in the detail; cut the four-line paragraph and the duplicated `n add one` / `n add a presentation`.
10. Give `candidate disabled` one treatment: dim text in the route row, dim text in the defaults rows, and no yellow.
11. The 80x24 detail (A6) cuts the PROBLEM mid-sentence with `… 9 more lines · pgdn`; shed the PROBLEM prose before the CANDIDATES block, since the candidate line already carries `now: available`.

### B: one configuration tree

**Five-second impression (B1).** Yellow `1 not configured` at the top of the tree, then the reversed `policy` row, then the bold detail heading `a requested name is looked up in this order`.
The order is defensible: the broken provider is the one thing to act on.
But the fourth thing is 25 empty rows between the detail text and `[v] trace a name` pinned at the bottom.

**Blur.** Two boxes; the tree is a narrow smudge down the left third of the left box, the detail a smudge in the top quarter of the right.
Content does not survive.

**Greyscale.** Survives: the tree's disclosure glyphs, counts and `~` markers carry the state.
`on, not configured` loses its yellow but still reads.

**Colour inventory.**
Yellow: `1 not configured`, `on, not configured` (justified), a full sentence `anthropic_sub is on but has no credential.` in B2 (a reading, but a sentence in colour is a warning panel, not a readout), yellow background on the selected row's `on, not configured` in B4 and B6 (badge).
No red, no green anywhere.
Blue and DarkGray chips.

**Alignment.** Tree indent is two cells per level and the right-aligned counts (`38 offers`, `1 candidate`) share an edge; good.
Detail label/value edges change frame to frame: `on   openrouter` at 1/6 in B2, `context 1M · served on 4 days` as prose in B3, `k  type a key here` at 1/4 in B4.

**Subtraction.** `five, fixed: Toker keeps a row for each, on or not`; `learned from the provider's model list and from traffic; nothing here is configured`; `the first stage that matches picks the route / a profile's entry beats its protocol's at every stage`; `[n] new  the five are fixed` (a control that exists to say it cannot be used).
Each of these is narration and each would go.

**Against the reference.** The tree is 17 rows in a 38-row box in B1, B2, B4; the detail is 6 to 10 rows plus a pinned control line at row 38.
The reference has no pinned control line anywhere.

**Operator's words.** Tables: no; the ordered rules are hidden behind `▸ 2 rewrites 14`, and when opened (B5 shows routes opened) they are a flat list of ids with one right column, not a table with the match, the route and the reach.
No JSON: yes.
Live validation: not shown; B8 is a diff, not an edit.
Core first, drill down: yes, this is B's strength, and the `~` change markers propagating up the tree (B8) are the best "what did I change" of the five.

**Findings, ranked.**

1. Restore the detail pane at 80 columns (B6 shows the tree alone): stack it under the tree as the design language says routing does below 100 columns, with the tree taking up to a third.
2. Draw the children of a stage as a table, not a tree list: when `4 routes` opens, the rows need `sent to`, `lanes`, `now` columns, and when `2 rewrites` opens they need `match → route  reach`, in scope order.
   Without this the operator cannot see rule order, which is the thing they asked for.
3. Unpin the controls from row 38; put them on the line after the last content line of the detail, and let the box end there.
4. Cut `[n] new  the five are fixed`; a control nobody can press is not a control.
5. Replace the yellow sentence in B2 with the grid the dashboard would use: `anthropic_sub  on · no credential` in yellow as a value, and the explanation gone.
6. Strip `legacy-route-` consistently: the tree strips it (B5), the detail's second line shows it in full, and A and D keep it; pick one (dim prefix, as A does) and use it on every surface.
7. Drop the `→ opens a provider: endpoint, bindings` and `→ chronology, family` hint lines; `→` means a transition in this app's glyph vocabulary, not "press right".
8. Make the big list a list: B3 shows nine offers and `… 29 more` inside a tree that cannot scroll them; either the catalogue is a windowed list in the detail pane with `… N above / … N more`, or it is not in the tree.
9. Put `accounts` out of the tree; it is a separate page in the app and out of scope for this card, and its presence makes this tree the whole app.
10. `on apply` at the top right of the detail (B1, B5, B7) has no referent on screen; replace it with the page status's words (`revision 2 live · no changes`) or drop it.
11. In B7 (empty), `1 presentations  none` keeps the lookup numerals on an empty policy; show the numerals only when there is an order to read.

### C: lead with the resolution chain

**Five-second impression (C1).** The bold REQUEST line `goes to openrouter.openrouter_messages as anthropic/claude-haiku-4-5`, then the reversed link `1 matched presented …`, then the green `serves` / `passes` at links 7 and 8.
This is the right order for the question "where does this request go": the answer leads, the proof follows.

**Blur.** The two-row REQUEST band survives as a bright bar across the top, then the chain as a block in the right box's top third.
Better than A and B: the shape is content.
Still, 20 empty rows in each lower box.

**Greyscale.** Survives; status words (`matched`, `not reached`, `skipped`, `cannot serve`) carry the reading in text.
The only colour-only signal is the red background on `cannot serve` (C4), which becomes a grey block.

**Colour inventory.**
Green: `serves`, `passes` (a fine reading: justified).
Yellow: `skipped`, `differs`, `billed boundary`, `diverges at the route's candidates`, `claude settings not synced`, `1 request differs from active` (worth noticing: justified, though six yellow items on C8 is a lot).
Red: `nowhere`, `cannot serve`, `no route`, `no candidate` (failed: justified).
Red background on `cannot serve` in the selected row (C4) and yellow background on `differs` (C8): badges, same fault as A.

**Alignment.** The chain has three edges (number at 1, status at 6, kind at 18, text at 27) and holds them in every frame.
The inline rewrite table under link 2 (C2, C5) opens a second grid (`pri` at 9, `rule` at 13, `matches` at 48) nested inside the first; readable, but the frame now has two tables with no shared edge.
Rail ages are right-aligned.

**Subtraction.** `exact slug on claude/anthropic_messages, no order: the slug decides`; `tried in ascending priority; the first match decides`; `a request that brings its own credential keeps it; a named request has …` (clipped); `offers the providers list, grouped by binding`.
Each is narration.
The panel title `WORTH A LOOK` (C3) is a joke where the rest of the app has nouns.

**Against the reference.** Three boxed panels on C1, each cut to the frame; REQUEST is sized to its content (two rows) and reads like a dashboard panel; the other two are not.
C3 is four panels and the closest to the dashboard's stacked shape of any frame here, though CATALOGUE leaves ten rows empty.

**Operator's words.** Tables: only inline, one stage at a time, under the link that matched (C2 shows the rewrites table filtered to scope).
The `2 records` tab, where the full tables presumably live, is never shown.
No JSON: yes.
Live validation: not shown in any frame; the editing slot (C8) is spent on the draft-against-active comparison.
Core first: yes, strongly.

**Findings, ranked.**

1. Show the records tab.
   Eight frames and none of them is the surface where the operator adds a fallback or reorders a rewrite; the design cannot be judged complete without it.
2. Show an edit with validation; the operator named it as a requirement and C has no frame for it.
   C4's inline credential entry is the closest, and it is a text entry, not a form.
3. Drop the red and yellow backgrounds on `cannot serve` and `differs`; red foreground on a plain row is the house form, and on a reversed row the word reads as plain reversed text.
4. Rename `WORTH A LOOK` to `DIAGNOSTICS`, and put the severity in the row (`warning` is already there) rather than in the title.
5. Size REQUESTS and CHAIN to content at 40 rows the way REQUEST already is; let CHAIN end under its last link and put the `draft 2 and active revision 2 route this request the same way` line in the REQUEST panel as a third row, where the comparison belongs.
6. Make the clipped explanations under link 7 (C4) readable: two lines end in `…` with no way to see the rest.
   Cut the second sentence; wrap the first.
7. Keep the rail at 100 columns (C5 sheds it and tells the operator to press `s`); the rail is how a recent request is chosen, and 100 is the repo's own snapshot width.
   Stack it over the chain, as the design language says to do below 100.
8. In the rail, show `cannot serve` and `no route` once each as the verdict of the request, not red in the rail *and* red in `goes to` *and* red at link 9: three red marks for one fact.
9. Reduce C8's yellow: `diverges at the route's candidates` as a bold plain heading; keep yellow for `billed boundary`, which is the thing that spends.
10. Drop `[t] other request  [g] send elsewhere` from inside the REQUEST panel's first row and put them where C puts every other control, on the last line of the panel they act on.
11. The `defaults` section of the rail duplicates link 3 of the chain for the selected request and offers nothing for the others; cut it, or make it the four default routes with their `now` reading so it earns its rows.

### D: the policy as ordered rule tables

**Five-second impression (D1).** The reversed row `2 regex anthropic_sub/${model} → legacy-route-…`, then the yellow `no cred` column, then the bold scope heads `anthropic_messages  5 rules`.
Right order: the rule in focus, then the readings that need attention, then the structure.
The RULE detail comes fourth and should.

**Blur.** The table survives as a dense block filling the upper panel, with the scope heads as three brighter bands; the detail is a block of label/value rows below.
Of the five, this is the only main frame whose blur looks like the reference's: content fills the frame.

**Greyscale.** Survives; `no cred`, `off`, `ok` are words, the trace arrows are glyphs, `≥2` is a glyph.
Nothing is lost.

**Colour inventory.**
Yellow: `no cred` in the reach column (justified: a rule that matches and has nowhere to go is worth noticing).
Green: `ok` in D8's field status only; the same word is plain in D1's reach column.
Red: the refusal in D8 (justified).
No backgrounds, no badges.
This is the leanest inventory of the five and the closest to the design language.

**Alignment.** One grid for the whole table: number at 3, kind at 7, match at 11, `→` at 29, route at 31, model at 62, reach at 68, hits right-aligned at 72.
The `else` row sits in the grid.
The detail has one label edge at 1 and one value edge at 10 and holds them.
Columns are held at 100 and 80 (D5, D6).
The best alignment of the five.

**Subtraction.** `14 rules in 3 scopes · a request tries one scope's rules top to bottom and the first match decides` says what the title rule `first match wins within a scope` already says.
The `kind` column reads `regex` on all fourteen rows and the `model` column reads `${model}` on all fourteen: two columns of noise.
D4's `fix  [e] enter an api key  [space] turn it off` and its footer `[e] enter key  [space] turn off` are the same controls twice.

**Against the reference.** D1 and D2 fill 36 of 38 rows; D3 fills 30; D4 fills 25 and leaves 13 empty under the provider detail; D7 (empty) leaves 35.
D's panel split is top/bottom, which is the dashboard's shape, and its table columns use the width.

**Operator's words.** Tables: exactly; ordered, grouped by scope, with an `else` row, a priority number and a reach reading per rule, and the tab strip shows the stage order with `→`.
No JSON: yes.
Live validation: the best of the five (D8): each field's status beside it, semantic (`catches claude-sonnet-5 and 2 more`, `a glob captures nothing, so ${model} would be empty`), a PREVIEW of which recent requests would change, and `[ctrl+s] fix sends to save`.
Core first: the table first, the rule's detail below, the route one `enter` away; yes.

**Findings, ranked.**

1. Show the draft and activation.
   Every D frame says `live · rev 2`; none shows a draft that differs, what applying would change, or the revisions list.
   The operator activates after every edit; this is a hole, not a polish item.
2. Shed the uniform columns first: drop `kind` and `model` whenever every row in the table has the same value, and show the value once in the scope head (`anthropic_messages  5 rules · regex → ${model}`).
   D5 at 100 sheds `hits`, which varies, and keeps `kind` and `model`, which do not: the wrong order.
3. Cut the narration row at the top of every table (`14 rules in 3 scopes · a request tries…`, `15 selectors in one scope · each lists…`, `interfaces: where a request can actually leave`) and the subtitle in the title rule; a panel title is `┌REWRITES─`, nothing more.
   The lookup order is already in the tab strip's arrows.
4. Name tab 5 by its contents: `5 models` or `5 families`, not `mangle`.
   It is not a word in the specs or the glossary, and the operator has to know what is in it.
5. Show the controls once: in D4 drop the `fix` row and keep the panel's last line; in D8 drop the second hint line (`tab next field · ←/→ change value · ctrl+s save · esc cancel`) since `ctrl+s` and `esc` are already on the line above.
6. Put the revisions where the policy is: D4's config page carries `3 history`, but history is of the policy, which lives on the other page.
   Move it to the routing page's strip, after routes.
7. Use fixed panel titles: `ANTHROPIC_SUB` (D4) is a record name in the title slot.
   Title it `PROVIDER` and put the name bold on the first line, as RULE and PRESENTATION do.
8. Let the detail panel end where its content ends (RULE has three empty rows above its control line; PROVIDER has thirteen); leave the ground bare below rather than ruling a box around it.
9. Make `ok` one colour: plain in the table, plain in the field status; green is for a reading that is *better* than asked, and a valid field is not that.
10. Fold the section heads in the provider detail (`what leaves through it`, `what sends requests here`) into the label column (`binding`, `rewrites 5`, `defaults 2`, `routes 1` are already there); the bold lower-case heads are a third heading style on a page that already has panel titles and bold names.
11. In D2 the tab strip gains `↓` and `>` glyphs during a trace (`1 presentations 15 ↓ → 2 rewrites 14 >`); keep the trace marks in the table gutter and the TRACE panel, and leave the strip alone.
12. The `hits` column at `41` and `9` is the only use of the right edge; when there are hits, right-align them against the box as the dashboard right-aligns its verdicts, and when a scope has none, write `-` rather than leave the column blank, so blank does not read as zero.

### E: one page per route, read downward

**Five-second impression (E1).** The bold route id, then the reversed rule row under `matched by`, then the yellow `not configured`.
The second landing is wrong: on a route's page, the thing the operator came for is `sent to`, and the first selectable row is a rewrite that points here.

**Blur.** A vertical rail down the left with four horizontal bands of text; the lower 16 rows of the box are empty.
The rail glyph survives better than the content does.

**Greyscale.** Survives; `on`/`off`, `not configured` and the `1 → 2` edges are text.
E2's ATTENTION loses the yellow on the provider names and becomes a plain list, which is an improvement.

**Colour inventory.**
Yellow: `not configured` (justified); `provider off`, `codex_sub off` in E2's `now` column (not justified: off is a chosen state, not a reading that needs noticing); the four provider *names* in ATTENTION (colour on an identity, which the design language reserves for cyan and uses for nothing else); `allow`, `may be lost`, `billed` in E8's edges (spends: justified).
Green: `ok` in E2 (same question as D: is a route that works *better* than asked?).
Red: the refusal in E8 (justified).

**Alignment.** Each section has its own grid: rules at 4/10/22/46, blocks at 4/21/25, candidates at 4/8/… , edges at 4/9/13.
The `on` column at 21 shares an edge with nothing.
Within a section, alignment is clean; across the page there is no vertical line but the rail.

**Subtraction.** The `client` and `provider` stations at the top and bottom of the rail are labels with nothing under them on every frame, including E4 where the provider is the problem.
The TABLES panel on E2 and E7 re-lists the tab strip with descriptions (`what a request is matched by, before it reaches a route`).
`4 off  server safeguards · required capabilities · alias rules · health circuits` is a list of what is not on.

**Against the reference.** E1 leaves 16 rows empty; E2 leaves 14 in ROUTES while pinning three fixed panels at the bottom; E3 leaves 20 in UPGRADES while CLASSIFIERS is tight; E7 leaves 28.
The bottom-pinned trio on E2 is the dashboard's RATE & QUOTA idea, but the dashboard pins one panel and gives the rest to the list; E pins three and the list is eight rows.

**Operator's words.** Tables: inverted; the route page is primary and the rule tables are tab 2 and not shown, except as the `matched by` fragment and the ROUTES overview (E2), which is a good table.
No JSON: yes.
Live validation: yes (E8), beside the field, with `enter can't save: open for is not a whole number`.
Core first: the route page is core-only with `4 off` folded, yes; but `handled by` (blocks) sits above `sent to` (candidates), which is the less important above the more.

**Findings, ranked.**

1. Cut the `client` and `provider` stations and the rail glyph; the three bold section heads carry the structure, and a `┌ ├ └` spine with two empty ends is decoration.
2. Put `sent to` above `handled by`; the candidates are what a route *is*, and the blocks modify them.
   Make the first candidate the default selection, not the first rewrite.
3. Take the yellow off `provider off`, `codex_sub off` and the provider names in ATTENTION; off is plain, and an identity is never coloured.
   ATTENTION then holds one yellow item, `anthropic_sub  on, not configured`, which is the one that needs it.
4. Drop the TABLES panel; the tab strip already names the tabs, and the counts can ride in the strip as D does (`2 rules 33`).
5. Give E2's ROUTES the spare rows and size PATH and ATTENTION to content; with seven routes and one attention item the page should end a few rows under ATTENTION, not pin three boxes to the floor.
6. Say the validation once (E8): the red clause beside the field *or* the dim `enter can't save: …` line, not both; keep the one beside the field and make the control line `[enter] fix open for first   [esc] cancel`, as A8 and D8 do.
7. Fold `4 off  server safeguards · required capabilities · alias rules · health circuits` into the `handled by` head (`3 blocks on · 4 off`) with `enter` to open the off ones; a list of what is not configured is noise on a core-only page.
8. Show the rules tab and the history tab; six of eight frames are the route page or the models tab, and the operator's ordered tables are never seen.
9. In E3, split the three panels by content, not evenly: CLASSIFIERS and COMPACTION are sized right; UPGRADES should end after its last row.
10. Make the edge lines (`1 → 2  when 1 is unavailable · on rate limit · provider quota · overload` / `crosses to billed: allow   ambiguous retry: deny   cache: may be lost`) one grid with the candidate rows above and below them; they are the best rendering of an edge in any direction and deserve a shared edge.
11. The header `rev 2 · live · 1 edit not saved` (E8) does not say whether saving makes the edit live or puts it in the draft; the operator needs `draft` to appear the moment an edit is saved, and E never shows the word.

## 3. Cross-cutting

**The empty rows are a real fault, and the orchestrator has read it correctly, but the cure is not more content.**
Counted on the main frames: A1 22 and 26 empty rows in its two boxes; B1 21 and 25; C1 20 and 20; D1 4; E1 16; E2 14 in ROUTES.
The reference has none, and the reason is not that it has more to say.
It sizes every fixed panel to its content and gives the remainder to the lists that scroll, so a box never outlives its contents.
Four of the five directions draw their panels to the frame first and fill them second, which under blur makes the box the dominant shape and the content a smudge in its corner, and that is the "generated" look the method warns about: even everything, nothing leads.
D does not have the problem only because its content happens to be 36 rows; at 60 rows it would.
The right answer is the dashboard's rule applied here: at 120x40, a list or table takes what it needs, a detail panel is as tall as its content, controls sit on its last content line, and the ground below is left bare rather than boxed.
Where two panels sit side by side and both are short (A1, B1), stack them instead; A5 and B5 at 100x24 are better frames than A1 and B1 at 120x40 for exactly this reason.
The operator asked for core first and drill-down, so padding the frame with more fields would be the wrong fix.

**Every direction narrates.**
`a request tries one scope's rules top to bottom and the first match decides` (D), `five, fixed: Toker keeps a row for each` (B), `exact slug on claude/anthropic_messages, no order: the slug decides` (C), `Edits go to the draft; nothing is live until you activate` (A), `what a request is matched by, before it reaches a route` (E).
The method names this as the surest tell, and the reference has not one such line.
Each belongs in the `?` overlay or nowhere.

**Controls drift toward footers.**
B pins them at row 38 of the detail; D's RULE and PROVIDER leave blank rows above a bottom control line; E's TABLES is a footer in a box.
The design language says no page carries a footer, and a control is written on the last line of the panel that owns it.

**Coloured backgrounds on selected rows.**
A (`billed fallback`, `no credential`), B (`on, not configured`), C (`cannot serve`, `differs`) each paint a cell of the reversed row yellow or red.
Only D and E never do.
The house form is a coloured foreground on a plain row; in reverse it is plain reversed text.

**Five header vocabularies for one fact.**
`draft ≠ rev 2 · +1 ~2 · a activate`, `revision 2 live · no changes`, `draft 2 · same as active · claude settings not synced`, `live · rev 2`, `rev 2 · live · 1 edit not saved`.
Pick one, in the form the design language gives (`draft 4 · working`): `rev 2 live` when the draft equals it, `rev 2 live · draft +1 ~2` when it does not, `[a] apply` as a control only where it can be pressed.

**`legacy-route-` four ways.**
A dims the prefix, B strips it, D keeps it in full and elides it with `…` at 100, E keeps it.
Dim prefix everywhere, as A does, since the id must stay recognisable and the tail is what varies.

**Yellow on `off`.**
A (`disabled`), E (`provider off`) colour a chosen state.
D and B write `off` dim.
Dim is right: off is absence, not a warning.

## 4. The choice: D

D should be refined and built.

Against the operator's words: D is the firewall table they named, with scopes as chains, rules in priority order, an `else` row for the default, a reach reading per rule and the stage order drawn as arrows in the tab strip.
It has no JSON.
Its D8 is the only editing frame of the five where validation is semantic rather than syntactic, where the refusal names the consequence (`${model} would be empty`), and where a preview shows which real requests would move; it meets "a field says what is wrong beside itself; an invalid form cannot be saved" more completely than A8 or E8.
It leads with the table and puts the rule's detail and the route one key away: core first, drill down.

Against the protocol: D is the only direction whose blur test passes on its main frame, whose colour inventory is one justified yellow plus the refusal red, whose alignment is one grid held at three widths, and whose panels are stacked full-width like the dashboard's.
Its failings (narration rows, two noise columns, duplicated controls, `mangle`, the missing draft/activation surface) are all polish or an omitted frame; none is structural.

B is the runner-up on "core first", but it hides the tables and loses the detail at 80 columns.
C answers "why did this go there" better than anything else here, and that is a tab, not a page.
E's route page is the best detail view of a route, and that is a level, not a page (see 5).
A is the most finished but keeps the skeleton the operator called shockingly bad, and finishes it rather than fixing it.

**Take into D:**

- Take C's chain into D's TRACE panel.
  D2's trace is four stages in prose; C1's is ten numbered links with a status column, a `goes to` line that answers the question first, and the match table opened inline under the deciding link.
  Make D2's TRACE that chain, keep D's table above it with the `↓`/`>` gutter marks, and drop C's REQUEST panel in favour of D's `trace  claude/anthropic_messages · model openrouter/qwen3-coder` header line.
- Take C8's draft-against-active comparison and B8's `+ rewrite / ~ route / - default` diff into D's missing history tab.
  The history tab shows the revisions list (C3's HISTORY), the diff as B8 lists it, the lanes line, and `[a] apply`; C8's per-request comparison is reached from the trace with `[c] compare`.
- Take B's `~` markers into D's gutter.
  B8 propagates `~` from a changed record up to `policy`; D's table rows and its tab strip should carry the same mark (`2 rewrites 15 ~`), so the operator sees what the draft changed without opening history.
- Take E's edge rendering into D's routes tab.
  E8's `1 → 2  when 1 is unavailable · on rate limit · provider quota · overload / crosses to billed: allow  ambiguous retry: deny  cache: may be lost` between two candidate rows is how a route's chain should read in D's routes table detail; A2's `falls to 2 on rate_limit · billed spend allowed` is the one-line form for the table row.
- Take E2's ROUTES overview columns (`rules · blocks · sent to · lanes · now`) as D's routes tab table, since D never shows one.
- Take A3's catalogue (`… 170 above`, `ctx`, `date`, `… 236 more offers`, tabular, right-aligned) as D's offers list in the models tab, and A2's REACHED BY block into D's route detail, which D4 already does for providers as `what sends requests here`.
- Take B2's first-open rule: land on the tab that holds the problem.
  D1 opens on rewrites; when a provider is on and unconfigured, open on providers with it selected.

## 5. Do two of them compose?

Yes: D and E are two levels of one design, and C is a third.

D is the list level: the tables, one per stage, each row a rule.
E1 is the detail level for one route: which rules point here (`matched by`), what modifies it (`handled by`), where it goes (`sent to`).
D already has `[enter] open route` on every rewrite and default; E1 is what that key should open.
E's `matched by` section is D's rewrites and defaults tables filtered to the routes that point here, which is the right cross-reference, and E's `sent to` with E8's edges is the candidate chain D never draws.

Joining them: D's routes tab lists routes in E2's columns; `enter` on a route replaces the page body with E1's route page (full height, as E draws it, not D's bottom pane, because a route's page has three sections and needs the rows); `esc` returns to the table with selection kept.
E's `all 1 2 3 4 5 6 7` switcher survives as the way to step between route pages without going back.

To reconcile: drop E's rail glyph and empty stations (E finding 1); put `sent to` first (E finding 2); use D's tab strip (`1 presentations → 2 rewrites → 3 defaults → 4 routes · 5 models · 6 history`) and drop E's (`routes / rules / models / history`), since E's folds the three ordered tables into one tab; use D's header vocabulary; keep E's full-height detail for routes and D's bottom-pane detail for rules, since a rule's detail is eight rows and a route's is twenty.

C joins as D's trace: C's chain is D2's TRACE grown to full height, and C2's inline match table is D's table filtered to the deciding scope.
It is entered from D with `[t] trace` and leaves with `esc`, and it should not be a page of its own, because the operator traces to check a change and then returns to the table to make the next one.

## 6. What a design lead would change first about D

1. **Finish the lifecycle before polishing the table.**
   There is no draft, no diff, no apply and no revisions list in any D frame.
   The operator's every edit ends in an activation; a design that does not show it has not shown the task.
   Build the history tab from C3 and B8, put `draft +1 ~2` in the header the moment a row changes, and make `[a] apply` appear there and nowhere else.
2. **Take every sentence off the page.**
   The narration row under each title, the subtitle in the title rule, the `fix` paragraph in PROVIDER, the empty state's four-line explanation and its `the chain is built from the right` list, the second hint line in D8.
   What remains is the table, the detail grid and one control line, which is what the dashboard is.
3. **Make the columns earn their width and the panels their height.**
   Drop `kind` and `model` when uniform, right-align `hits` against the box with `-` for none, end the detail panel at its last line, and at 100 columns shed the uniform columns before the varying ones.
   Then the table reads as the firewall table the operator described, with nothing on the screen that is not a rule or a reading of one.
