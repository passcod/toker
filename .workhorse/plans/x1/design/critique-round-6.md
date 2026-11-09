# Critique round 6: the converged frames

Fresh eyes, no earlier rounds read.
Judged against `design-system.md`, `mockups.md`, `brief-converged.md` with the later steers, and the routing specs where the semantics mattered.
All 30 PNGs viewed at full size; the HTML passed the width check (every frame exactly its claimed width) and the `.frame` stack names `"Iosevka Term"`.

## 1. Verdict

This is finished, bar two small things.
An operator who has never seen it can read where a request goes from the rewrites table alone, follow the chain through the tab strip, and get the full answer from a trace; the forms refuse live and say what to do instead; nothing on any screen is a sentence, a JSON blob, a legacy name or a green.
The data agrees with itself across all 30 frames (I summed every count I could and they reconcile), and the narrow frames shed sensibly.
No critical findings.
Two suggestions: the `hits` column never says its window, and the presentation detail's `published  no` contradicts a header that says the settings are synced.
The rest is polish.

## 2. Per frame

**Rewrites at rest (01-01, 01-02).**
The rail, the uniform-column scope heads (`5 rules · prefix · sends ${model}`), the yellow `no cred · billed fallback`, the dim `off`, right-aligned `hits` with `-` and `≥n`, the yellow `refused · no route` else row.
RULE detail sized to content, name bold, label/value rows, controls on the last line, ground bare below.
135x50 is the same frame with more air and nothing stretched to fill it, as the brief requires.
Correct.

**Trace (01-03).**
Gutter marks `↓` and bold `>` on the table, the unreached rows dimmed (including the anthropic_messages else row, correctly), answer first in bold with `billed` yellow, nine numbered links, the selected link reversed.
Link 9's `6,883 tokens, under the floor` agrees with the settings page's `cold min tokens 175k`.
Correct.

**Editing with a refusal (01-04) and the save flow (08-01..08).**
The `kind` column appears as soon as a non-prefix rule exists and the scope heads drop `prefix`; the `sends` column appears after save when rule 6 sends `as asked`; the heads shrink to `6 rules`.
The refusal is red bold beside the field it belongs to and says what to do instead; `[ctrl+s] fix sends to save` dims the verb while invalid; state 1 reads `fix 2 fields to save` with dim `needed` beside each empty required field and no PREVIEW yet, which is the right quietness.
PREVIEW sized to content, bold headline, changed rows bold and unchanged rows dim.
In state 1 the `kind` column is shown while every row is still `prefix`; that keeps the layout from jumping as the operator cycles the kind, and I would keep it.

**After save (01-05).**
Header `saved · rev 10 live · 2 lanes on rev 9 · claude settings not synced` with the strip collapsed to numbers, per the steer.
`rewrites 15`, `6 rules`, rule 6 `recent  -`.
RULE detail for rule 6 folds `· sends as asked` dim onto the match line; see nit below.

**Routes (02-01), route page (02-02), adding a fallback (02-03).**
`matched by  4 rules · 2 defaults` per route sums to 14 rules, matching the tab.
The ROUTE detail is the `sent to` section alone with E8's edge line, `allow` yellow, `billed` yellow, `no cred` yellow.
The route page has the fixed title, bold name, `1 of 7` dim at right, three bold section heads, off handlers dim on the `handled by` line with the alias among the on ones, and `[←/→] other routes` in the controls.
The fallback form sits inline as candidate 3 with a dim reading beside `binding` and a red refusal beside `sends`, and the page's control line gives way to the form's.
The `sent to` head still says `2 candidates` while the third is being added; see nit.

**Presentations (03-01), defaults (03-02), models (03-03, 03-04).**
Presentations: the uniform slug pattern lifted into the head, `except` with dim `-` for none, the else row falling to `→ rewrites`, which is the chain made visible.
PRESENTATION detail is good except `published  no`; see suggestion 2.
Defaults: four rows plus the yellow `no default` row; one such row per protocol is right, since the protocol default covers every profile.
Models: FAMILIES, UPGRADES and COMPACTION sized to content, OFFERS windowed with `… 180 above` and `… 246 more` summing to 440.
The two models frames show focus moving between panels, with the control line following focus; that is the controls-once rule applied well.

**History (04-01, 04-02).**
Newest first, `live` bold on rev 10, `what changed` sentences, lanes `8 cold` and `2 warm` reconciling with the routes table's `2 warm · 5 cold` plus `3 cold`.
`[a] already live` is the unavailable-control form from the design system, and avoids the global `r`.
Armed: the control reverses and the last line says what is lost (`2 warm lanes lose their cache: ses-hot, ses-crow`, yellow) and how to confirm or cancel.
Correct.

**Providers (05-01) and settings (05-02).**
Providers opened on the problem; `used by` counts include fallbacks (openrouter `5 routes`, `10 rewrites`, `4 defaults` all reconcile).
PROVIDER detail with `credential  none` yellow and the three controls.
Settings: rail groups, one row in edit with a red refusal that names the actual ceiling (`more than any configured model's context, 1M`), `fixed here` rows dim with their reason.
`cold idle minutes  lane TTL` is plain where it should read as absent; see nit.

**Narrow (06-01..04).**
100x24 and 80x24 rewrites: the table keeps every row, the detail panel sheds to one line with `[pgdn] more`, the spend clause shortens to `no cred · billed` at 80, the tab strip collapses, panels go ruled.
Route page at 100x24 wraps the edge line onto two rows and fits exactly.
Providers at 80x24 sheds `bindings` first.
All read.

**Empty (07-01) and `?` (07-02).**
Fresh install: the rail with `0 rules` heads and `no default` else rows, `[n] needs a route first` dim with `[4] go to routes` beside it, `no policy yet` in the header.
The overlay covers every mark I found on the screens and the keys for every control drawn, in the `key  what it does` form, with `? or esc close` in the border.
`pinned` is listed with nothing drawn behind it; see nit.

## 3. Cross-frame consistency

- Names: every route, rule, provider, family and session id is the same wherever it recurs; no `legacy-` anywhere.
- Counts: rewrites 14 → 15, presentations 15, defaults 4, routes 7, models 11 (5 + 5 + 1), history 9 → 10, providers 5; routes' `matched by` sums to the rewrites count; providers' `used by` sums match the routes' candidate chains; lanes 2 warm + 8 cold everywhere they appear; OFFERS 180 + 14 + 246 = 440.
- Marks and colours: `no cred`, `billed`, `billed fallback`, `refused · no route`, `credential none`, `allow`, `not synced` and the armed warning are yellow everywhere; refusals are red bold everywhere; `off` is dim everywhere; `ok` plain; identities never coloured; the selected row plain reversed; no green, no RGB.
- Controls: `[key] label`, three spaces apart, on the panel's last content line, replaced by the form's line while editing; the same verbs (`edit`, `new`, `move`, `turn off`, `delete`, `trace`, `open route`) across every table detail.
- Forms: `match`, `route`/`binding`, `sends` fields with `←/→` cyclers; the reading or refusal beside the field; `[ctrl+s] fix … to save` dim until valid; `[esc] cancel`.
- Header: chips, strip, right-side status, collapsing to numbers at 100 and after a save at 120.
- Two small drifts: the fallback form's head does not say `· 1 new` as the rewrites form's does, and the same reach vocabulary sits under `reach` on rewrites and defaults but under `now` on routes.

## 4. Ranked findings

1. **suggestion** `hits` never says its window.
   Frames: 01-01, 01-02, 01-03, 01-04, 01-05, 03-02, 06-01, 06-02, 07-01, 08-01..08.
   The column reads `41`, `≥1`, `-` with no period; only the RULE detail's `recent  … ≥1 in 24h` hints that it is a 24h count, and the `?` overlay has no `hits` entry.
   An operator deciding whether a rule is dead needs to know whether 41 is today or ever.
   Fix: add a MARKS line to the overlay (`hits  requests caught in the last 24h; - not recorded`) and, since there is room, head the column `hits 24h`.

2. **suggestion** `published  no` on a presentation contradicts the header and has no control behind it.
   Frame: 03-01.
   The header reads `rev 9 live` with no `not synced`, so the Claude settings are reconciled, yet the selected presentation says `published  no` in plain text.
   The publication spec keeps one state per adapter and scope (pending, applied, failed, unsupported), not a flag per presentation, so there is no per-row meaning for this to carry, and nothing on the page lets the operator change it.
   Fix: drop the row, or make it the scope's publication state in the header's vocabulary (`published  yes` plain when applied, `published  not synced` yellow otherwise) so the two never disagree.

3. **nit** The fallback form's head does not count the new row.
   Frame: 02-03.
   Rewrites in edit says `5 rules · 1 new`; the route page in edit says `sent to  2 candidates` while candidate 3 is on screen.
   Fix: `sent to  2 candidates · 1 new`.

4. **nit** `pinned` is in the legend with nothing drawn behind it.
   Frame: 07-02.
   The design system makes the legend a contract both ways.
   No lane on any frame is `pinned`; the lanes columns use `warm` and `cold` only.
   Fix: either show a pinned lane somewhere (`2 warm · 1 pinned · 5 cold` in the routes table and the history lanes line) or drop the entry until the state is drawn.

5. **nit** `cold idle minutes  lane TTL` reads as a value when it means unset.
   Frame: 05-02.
   `cold_idle_min` is `None` here and falls back to the lane's own TTL tier; the current page writes `(the lane's own TTL tier)`.
   On this frame `lane TTL` is plain, the same weight as `175k` above it, while `transcript roots  none` two rows down is dim.
   Fix: dim it, and say the fallback (`follows lane TTL`).

6. **nit** The RULE detail folds the sends reading onto the match line.
   Frames: 01-05, 08-04, 08-08.
   `match  glob  claude-* · sends as asked` sits directly above `sends to  openrouter-messages`, so two senses of "sends" are adjacent, and the detail no longer mirrors the form's three fields.
   Fix: give the detail its own `sends` row (`sends  as asked`, or `${model}` when that is the value), between `match` and `sends to`, and let `example` carry the worked case as it does now.

7. **nit** PREVIEW shows session ids only on the rows that change.
   Frames: 01-04, 08-02, 08-03, 08-06, 08-07.
   `claude-sonnet-5  ses-free` but `openrouter/qwen3-coder` with no id.
   Nothing says why.
   Fix: show the id on every row (dim on unchanged rows), or on none.

8. **nit** The same reach vocabulary has two column names.
   Frames: 01-01 and 03-02 (`reach`) against 02-01 (`now`).
   `ok`, `off`, `no cred · billed fallback` appear under both.
   Fix: pick one; `reach` is the one the detail panels already use as a label.

9. **nit** The off handler names are unexplained anywhere the operator can read.
   Frames: 02-02, 02-03, 06-03.
   `server safeguards · required capabilities · health circuits` are dim on the `handled by` line and the `?` overlay has no entry for any handler.
   The drill-down (`enter` on the head) presumably explains them, but it is not drawn.
   Fix: one MARKS line per handler in the overlay, or confirm the handler picker carries a line of description each when it is drawn.

## 5. Counts

critical 0 / suggestion 2 / nit 7

## 6. Backlog

- Draw the handler picker that `enter` on `handled by` opens, since it is the only place the three off handlers get explained and the only way to turn one on.
- Draw the trace request prompt that `t` opens, and the `d again` and `m` (lifted row) states, so the arming and move vocabulary in the overlay has frames behind it.
- Draw the models tab with focus in COMPACTION and in OFFERS: whether they take focus, what their control lines are, and how OFFERS switches between the three openrouter bindings.
- Draw the config page's `?` overlay; the routing one is complete but the config keys (`enter key`, `turn off`, settings edit) have no overlay frame.
- Decide whether presentations count as a `matched by` kind: the route page for `openrouter-messages` would need a `presentation` line type, and a `hits` column on presentations would make the else row's fallthrough measurable.
- Draw the providers table with a different row selected, so the yellow `none` on `anthropic_sub` is seen unreversed; the "opened on the problem" claim rests on the table showing the problem when the selection is elsewhere.
- A route page one row short of fitting at 100x24 (06-03 fits exactly) will need a shedding order for the blank separator rows and the edge line; worth stating before implementation.
