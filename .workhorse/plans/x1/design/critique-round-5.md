# Critique, round 5

## 1. Verdict

The set is close to finished and holds together well.
Counts agree across every frame (14 rewrites, 4 rules and 2 defaults on claude-sub, 10 lanes before and after the save, 11 models, the history adding up from setup to rev 10), the binding rules hold everywhere, and an operator can follow a request from the tab strip through the trace to the provider without leaving the screens.
There are no criticals.
Three suggestions remain: the `?` overlay does not explain several marks the frames use (most importantly `claude settings not synced`), the route page's `5 cold pinned` contradicts the overlay's definition of pinned, and the settings frame refuses `175k`, the TUI's own way of writing a token count.
That is a clean round by the stated bar, with a handful of nits worth sweeping up.

## 2. Per frame

### Rewrites (converged-01-rewrites-01 to -05)

- 01 and 02: the rail, scope heads and `else` rows read as an ordered rule table, which is what the firewall steer asks for.
  The uniform `kind` and `sends` live in the scope head as the brief says.
  The 135x50 frame changes nothing but column gaps and bare ground, which is right.
- 03, trace: answer first, then the numbered chain; the table's `↓` and bold `>` with everything unreached dimmed makes the path obvious.
  `goes to … as anthropic/claude-sonnet-5 · billed` is the single most useful line in the set.
- 04, edit: the inline form, readings beside each field, the red refusal and `fix sends to save` in place of `save` all work.
  The `kind` column appears once a glob makes it non-uniform, and the scope heads drop it, which is consistent with the rule.
- 05, after save: header reads as the lifecycle says, with `saved` bold, the lane clause dim and the one `claude settings not synced` in yellow.
  The tab strip collapses to numbers to make room; that follows the design system's order, but see nit 10.

### Routes (converged-02-routes-01 to -03)

- 01: `sent to`, `matched by`, `lanes` and `now` answer "what is this route and is it working" in one row.
  The ROUTE detail drops `[m] move`, which the route page and the brief have (nit 9).
- 02, route page: sections in the right order, the off handlers named dim on the `handled by` line, the switcher reduced to `1 of 7` with `[←/→] other routes`.
  The `lane transition` line's `2 warm, 5 cold pinned` contradicts the overlay (suggestion 2).
- 03, adding a fallback: refusal and dim reading sit where the brief puts them and the route page's own controls give way to the form's.
  The outgoing-name field is labelled `model` here but `sends` in the rewrite form (nit 5).

### Presentations, defaults, models (converged-03-tables-01 to -04)

- 01: the `else  no slug equals it  → rewrites` row ties presentations into the chain nicely.
  The scope head uses `${model_id}` where every other frame uses `${model}`, and `as`, `shown as` and `published no` are never explained (suggestion 1).
- 02: the `no default` row and the detail are right.
  `unprefixed/…` is a term the operator meets here, on the route page and on the provider page, with no explanation anywhere (suggestion 1).
- 03 and 04: controls following focus between FAMILIES and UPGRADES is clear, and the OFFERS window counts add up (180 + 14 + 246 = 440).

### History (converged-04-history-01, -02)

- 01: the revision list reads back to setup coherently, and `[a] already live` dim is the design system's unavailable-control pattern.
  Rev 3 names a presentation `claude-sonnet-5`, which is not how presentations are named anywhere else, and rev 8 omits the scope although that pair of rules exists in three (nit 6).
- 02, armed: the control inverts and the last line says what is lost (`ses-hot, ses-crow`, which matches the preview) and how to confirm.

### Config (converged-05-config-01, -02)

- 01: opens on the problem, `credential none` in yellow in the detail, plain label/value rows with no section heads.
  Column headers are plain here but dim on the routing tables and on the same page at 80 columns (nit 1).
- 02: the gates/session/notices/fixed-here grouping is easy to scan and `fixed here` with its source is good.
  The refusal itself is the problem (suggestion 3), and the control line sits at the panel's foot rather than in an inline form (nit 4).

### Narrow (converged-06-narrow-01 to -04)

- 01 and 02: ruled chrome, the detail reduced to a one-line summary with `[pgdn] more`, and the settled `no cred · billed` at 80 columns all hold.
- 03: the edge line wraps to two rows cleanly, but the narrowed gap runs `subscription as asked` together as if it were a phrase (nit 8).
- 04: the bindings column is shed first and the rest stays legible.

### Empty and keys (converged-07-empty-keys-01, -02)

- 01: the three rails with `0 rules` and `refused · no route` say exactly what a fresh install does, and `[n] needs a route first   [4] go to routes` points at the fix.
- 02: the overlay is well organised by context (history, edit, route, trace) and its key rows match the controls drawn elsewhere.
  Its MARKS section is short of what the frames use (suggestion 1), and the underlying panels' corners show in the outer columns either side of it (nit 7).

### Save flow (converged-08-save-flow-01 to -08)

- The four states (empty form with two `needed`, refused, fixed, saved) tell the live-validation story with no apply step, as the steer requires.
- The PREVIEW only appears once there is something to preview, which is right.
- The static strip matches the animated frames exactly.

## 3. Cross-frame consistency

- Names follow the data sheet everywhere; nothing `legacy-` survives, no JSON, no regex outside an edit field.
- Colour is by function throughout: nothing green, `no cred` and `billed` yellow, `refused · no route` yellow, refusals red, `off` dim, identities plain, a selected row plain reversed with its yellow dropped.
- Counts agree across tabs, details, the provider page and history, including the post-save increments (`rewrites 15`, `history 10`, rev 10 `8` lanes plus rev 9 `2 warm`).
- `hits` is right-aligned with `-` and `≥n`, never blank, on every table that carries it.
- Inconsistencies found are small: header row styling (nit 1), the scope head wording for the same fact (nit 2), the edit cursor glyph (nit 3), where an edit's control line sits (nit 4), and the outgoing-name field's label (nit 5).

## 4. Ranked findings

### Suggestion 1: the `?` overlay does not explain marks the frames use

Frames: converged-07-empty-keys-02, with the marks appearing in converged-01-rewrites-05, converged-03-tables-01, converged-03-tables-02, converged-02-routes-02, converged-05-config-01.

The brief asks for every mark used above in the MARKS section, and the operator has only the overlay to decode the screens.
Missing are `claude settings not synced` (yellow in the header, with nothing saying whether it will resolve itself or what to do), `unprefixed/…` on defaults, `${model_id}` beside the `${model}` the overlay does explain, and the presentations' `as`, `shown as` and `published`.
The `off` row also says only "provider or step turned off", which does not tell the operator that a rule whose route is off still catches the request and refuses it, unless a fallback follows (`off · billed fallback`).
A firewall-minded reader expects a disabled thing to be skipped, so the definition should say what happens to the request.

Fix: add rows for `not synced` (the settings file has not caught up with the live revision yet; it retries on its own), `unprefixed` (requests from a client using no `/f/<name>` profile), `as` (the tier a presentation is listed under in the client's picker) and `published` (whether it is written into the client's settings).
Use `${model}` in the presentations scope head if it means the same thing, otherwise add `${model_id}`.
Reword `off` to "provider or step turned off; requests caught here are refused unless a fallback follows".
The overlay is already near the frame's height at 120x40, so expect `… N more` or a tighter layout.

### Suggestion 2: `5 cold pinned` contradicts the overlay's definition of pinned

Frames: converged-02-routes-02, converged-02-routes-03, converged-06-narrow-03, against converged-07-empty-keys-02.

The overlay defines pinned as "a lane held on its revision until cold", so a cold pinned lane is a contradiction, and the line can also be read as "(2 warm, 5 cold) pinned".
The routes table writes the same lanes as `2 warm · 5 cold`.
An operator deciding whether a save will disturb running sessions needs this term to mean one thing.

Fix: write `moves when cold · 2 warm · 5 cold`, matching the routes table.
If the pin itself is the point, redefine pinned in the overlay to cover every lane's recorded route and revision, and drop "until cold".

### Suggestion 3: the settings frame refuses the TUI's own token notation

Frames: converged-05-config-02.

The design system writes token counts as `175k`, so at rest this field would show `175k`, and retyping exactly what was shown gets `not a number: whole tokens, 175000 not 175k`.
A refusal the operator can only learn by tripping over it is the opposite of the live-validation steer's intent, and drawing it teaches the implementation to build it.

Fix: accept `k` and `M` suffixes, read back as the whole number beside the field (`175,000 tokens`).
Show a genuine refusal in this frame instead, such as a value above the largest context ceiling the configured providers offer, or a negative number.

### Nit 1: column header rows are styled two ways

Frames: converged-04-history-01, converged-04-history-02, converged-05-config-01, converged-05-config-02, converged-07-empty-keys-01, against converged-06-narrow-04 and every routing table.

The routing tables and the 80-column providers frame draw their header rows dim; REVISIONS, PROVIDERS and SETTINGS at 120 columns and the empty REWRITES draw them plain, at full prominence.
Fix: dim every column header row.

### Nit 2: the scope head says the same fact two ways

Frames: converged-01-rewrites-01, against converged-01-rewrites-04 and converged-08-save-flow-01 to -03, -05 to -07.

At rest the head reads `prefix → ${model}`; while editing it reads `sends ${model}` with `prefix` moved to a column.
Fix: use `prefix · sends ${model}` at rest, so the head names the same things in both states.

### Nit 3: two cursor glyphs

Frames: converged-05-config-02 (`175k_`), against converged-02-routes-03 and converged-08-save-flow-01 (`▏`).
Fix: use `▏` everywhere.

### Nit 4: the settings edit puts its control line at the panel's foot

Frames: converged-05-config-02.

The rewrite and fallback edits open an inline form under the row with the control line as its last line; the settings edit puts the control line 14 rows below the field.
Fix: open the same `┌ │ └` form under the edited row, with the control line on its `└`.

### Nit 5: the outgoing-name field has two labels

Frames: converged-02-routes-03, against converged-01-rewrites-04.

The fallback form calls it `model` and takes free text; the rewrite form calls it `sends` and cycles with `←/→`; the candidate rows show it as `as asked` or `alias …`.
Fix: label it `sends` in both forms, cycling `as asked`, `alias`, and a fixed model.

### Nit 6: history's sentences name things inconsistently

Frames: converged-04-history-01, converged-04-history-02.

Rev 3 says `add presentation claude-sonnet-5`, but presentations are named by their include pattern (`anthropic/claude-sonnet-*`).
Rev 8 says `move anthropic_sub/${model} above anthropic_api/${model}` without a scope, though that pair exists in all three scopes, while rev 2 does name its scope.
The live revision's lanes read `8` where the others read `2 warm` or `none`.
Fix: `add presentation anthropic/claude-sonnet-*`, `move anthropic_sub/${model} above anthropic_api/${model} in anthropic_messages`, and `8 cold` (or `3 warm · 5 cold`, whatever the data is).

### Nit 7: the overlay leaves panel corners in the outer columns

Frames: converged-07-empty-keys-02.

The popup is 118 wide in a 120 frame, so column 0 and 119 show the underlying REWRITES and RULE borders, corners included, which reads as debris.
Fix: inset the popup by at least two columns each side so the panels behind read as panels.

### Nit 8: the narrow route page runs two columns together

Frames: converged-06-narrow-03.

At 100 columns `subscription as asked` has a single space between the type and the model shaping, so it reads as one phrase.
Fix: keep at least two spaces between columns, or shed the type column (it is already implied by `billed` on the next candidate) before the gap closes.

### Nit 9: the routes tab's ROUTE detail drops `[m] move`

Frames: converged-02-routes-01.

The brief lists `[m] move` there and the route page has it.
Fix: add it, or say in the overlay that candidates are moved from the route page only.

### Nit 10: a save collapses the tab strip at 120 columns

Frames: converged-01-rewrites-05, converged-08-save-flow-04, converged-08-save-flow-08.

`saved · rev 10 live · 2 lanes on rev 9` is long enough to collapse the tabs to numbers at the operator's usual width, so the moment after every save loses the strip's labels.
This follows the design system's order, so it is not wrong, but it is avoidable.
Fix: write `saved · rev 10 · 2 lanes on 9`, which fits beside the full strip at 120.

### Nit 11: a refusal and a reading differ only by colour in some forms

Frames: converged-01-rewrites-04, converged-08-save-flow-02, converged-08-save-flow-06, converged-05-config-02.

Under `NO_COLOR` the red refusal and the plain readings beside the other fields render identically; the fallback form avoids this by dimming its reading.
The control line's `fix sends to save` still names the field, so nothing is lost, only salience.
Fix: make a refusal bold as well as red, which the design system already uses for the urgent case.

### Nit 12: the provider page counts dependents two ways in one cell

Frames: converged-05-config-01, converged-06-narrow-04.

openrouter's `5 routes` counts routes where it is a fallback, but its `3 rewrites · 2 defaults` counts only rules whose route starts at openrouter.
All of claude-sub's traffic is currently served by openrouter, yet none of those rules count towards it, so turning openrouter off looks smaller than it is.
Fix: count rules through every route the provider serves, or label the cell so the two measures are distinguishable (`first for 3 rewrites · in 5 routes`).

## 5. Counts

- critical: 0
- suggestion: 3
- nit: 12

## 6. Backlog

- Where a failed publication says why (pending against failed, and the colliding identity on a refusal), and the key that retries it.
- An UPGRADE detail showing the evidence an upgrade is waiting on, so `[p] promote` is pressed knowing what it skips.
- Choosing which binding OFFERS shows, and its controls.
- A rule turned off with `space`, drawn so it reads as skipped rather than as refusing.
- The RULE detail and PREVIEW ending at what is served and under which name (`→ openrouter.openrouter_messages as anthropic/claude-haiku-4-5 · billed`), as the trace does, so a rule change that moves requests between routes but not between providers or costs reads as such.
- The settings page at rest, with its detail and controls.
- Editing a route's edge (when to fall, billed, ambiguous retry, cache) from the route page.
- The route page for a single-candidate route such as openrouter-messages, showing its alias policy.
