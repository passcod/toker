# Converged frames 04, 05 and 07: report

Written to `.workhorse/design/mockups/x1/`: `converged-04-history.html`, `converged-05-config.html`, `converged-07-empty-keys.html`.
Screenshots are in `/home/felix/.cache/workhorse-rig/passcod__toker/cards/x1/work/shots/converged/`.
All six frames are 120x40, built from a small generator (not in the repo) so every row is exactly 120 cells.

## What I drew

- **04 history.**
  Revisions table, newest first (`rev`, `when`, `what changed`, `lanes`), nine revisions, rev 9 selected and marked `live`.
  REVISION detail sized to content: bold `rev 9`, the three `+ ~ -` lines from the brief, a lanes line, controls on the last line.
  Second frame: `[m]` armed as plain reversed text, with a final line in yellow and plain saying which lanes lose their cache (`ses-hot, ses-crow`) and how to confirm.
  Header strip is the chain with `6 history 9` current, right side `rev 9 live`.
- **05 config.**
  Providers: tabs `1 providers 5 · 2 settings`, table (provider, state, credential, bindings, used by) with `anthropic_sub` selected, PROVIDER detail with no section heads and the brief's rows.
  Settings: grouped label/value rows including `session headers` and `transcript roots`, `cold min tokens` in edit with a red refusal beside it, and the one control line `[enter] fix value to save   [esc] cancel` (`fix value to save` dim).
- **07 empty and keys.**
  A fresh install's rewrites: the three scope rails, a dim `-` where rows go, `else  no default  → nothing catches it` (yellow) in each, `no policy yet` on the right of the header, all counts 0, controls `[n] needs a route first` (dim) and `[4] go to routes`.
  The routing `?` overlay, centred over a rewrites table at rest, two columns under one `KEYS` title with `? or esc close` in the bottom border.
  Left column: every routing key, `IN HISTORY`, `IN AN EDIT`.
  Right column: `MARKS` (`>`, `↓`, `+ ~ -`, `${model}`, `no cred`, `off`, `nothing catches it`, `billed`, `≥`, `-`, `…`, `live`, `armed`), `ON A ROUTE`, `IN A TRACE`.

## Decisions where the brief was silent or loose

- **Lanes line.**
  The brief's `2 warm lanes on rev 9 · 5 cold lanes moved` cannot hold with rev 9 live and selected, because warm lanes left behind sit on the previous revision.
  I wrote `2 warm lanes still on rev 8 · 5 cold lanes moved to rev 9`.
  The `lanes` column shows lanes currently on each revision: `5 cold` on rev 9, `2 warm` on rev 8, dim `none` elsewhere.
- **Reactivate on the live revision** is drawn as dim `[a] already live`, per the design system's unavailable-control rule.
- **Armed control** is plain reversed (bold), not black-on-yellow, because the brief bans coloured backgrounds.
  The prompt line's `lose their cache` clause is yellow as a spend.
- **Control spacing** is three spaces between controls (design system, and the brief's own edit line), not the two the brief's code spans show.
- **`claude settings not synced`** is not in my frames: I took it to belong to 01's after-save frame, so it appears exactly once.
- **Config header right side:** `2 providers on` on providers (from D), nothing on settings.
- **Providers table:** `bindings` is a count (`1`, `3`); the names are in the detail.
  Provider rows for off providers are dim whole rows; the selected row is plain reversed, so the yellow `none` shows only in the detail.
- **Settings:** dropped D's `what it does` column (it was sentences); kept D's rail groups, added the per-profile notice rows (`claude`, `opencode`, `codex`), `session headers` and `transcript roots` in the session group; the fixed rows keep a dim `set by ...`.
  `refresh url` for `anthropic_sub` reads dim `(the built-in one)`.
- **History data** is invented: revisions 1 to 9 on Sat 7 to Tue 10 Nov, sentences derived from plausible diffs.
- **Rewrites layout** (needed for the overlay's underlay and the empty frame): rail, dim position, match, `→ route`, `reach` and `hits` right against the box.
  With short route names there is a wide gap between route and reach; I kept hits and reach at the right edge as the brief says.
- **RULE detail** (visible only as a sliver under the overlay) is named `strip anthropic_sub/` per the data sheet.
- **Overlay layout:** two columns, because one column did not fit the marks and the per-tab keys in 37 rows.

## Checks

- Width check: every row of all six frames is 120 cells, no RAGGED frames.
- Font stack: `.frame` names `"Iosevka Term", "Iosevka Fixed", "DejaVu Sans Mono", monospace`; no plain `Iosevka`.
- No `legacy`, no `green` class, no JSON, no regex notation, no `@`, no email addresses or client names in any frame body.
- Screenshots reviewed for each frame: panels end at their content with bare ground below, no sentences under titles, controls on the panel's last content line, no coloured backgrounds (only the header chips and the reversed selection), the one yellow background-free armed control.
- Not done: the `NO_COLOR` read-through beyond confirming that every yellow or red reading is also a word (`no cred`, `nothing catches it`, the refusal text).

## Changes after review

- **History:** rev 9 is now `add a fallback to claude-sub`, with the single diff line `~ route claude-sub  + candidate 2  openrouter.openrouter_messages  billed` (`billed` yellow).
  The rewrite and default lines are gone.
  Revs 2 to 8 are single-record edits: `add rewrite anthropic_api/${model} to anthropic_messages`, `add presentation claude-sonnet-5`, `turn off codex_sub/${model} in openai_chat`, `add family qwen`, `add upgrade newest on claude-sub`, `turn on compaction`, `move anthropic_sub/${model} above anthropic_api/${model}`.
  Rev 1 stays `create 7 routes and 4 defaults`.
- **Reactivate key:** `[a] make live`, dim `[a] already live` on the live revision.
  In the overlay `r` is plain `refresh` and `IN HISTORY` lists `a  make this revision live`.
- **Provider counts:** `anthropic_sub` is `4 rewrites · 2 defaults · 1 route` (detail `4  anthropic_messages 2 · openai_chat 1 · openai_responses 1`); `openrouter` is `3 rewrites · 2 defaults · 5 routes`.
- Width check (120 cells on every row of all six frames), font stack and screenshots re-run on all three files.

## Round 2 fixes

- **#3 save key:** settings saves with `[ctrl+s] fix value to save   [esc] cancel`.
- **#7 parentheses:** gone from all three files.
  Settings values are plain dim words: `on`, `built-in`, `lane TTL`, `none`; `refresh url  built-in` on the provider detail; the armed prompt in history reads `lose their cache: ses-hot, ses-crow`.
- **#10 and #23 history, redrawn at the after-save state:**
  - Ten revisions with rev 10 live, `6 history 10`, `2 rewrites 15`, header `rev 10 live`.
  - Rev 10 is `add rewrite claude-* → openrouter-messages`.
  - Its detail is the single `+ rewrite anthropic_messages 6  claude-* → openrouter-messages  as asked` line, then `8 lanes on rev 10 · 2 warm lanes still on rev 9`.
  - The `lanes` column reads `8` on rev 10, `2 warm` on rev 9 and dim `none` elsewhere.
  - Rev 9 stays `add a fallback to claude-sub`.
  - Rev 4 is `retry openrouter-chat on 5xx`.
  - Rev 1 is `setup: 7 routes, 4 defaults, 13 rewrites, 14 presentations, 4 families, 4 upgrades`, with revs 2, 3, 5 and 6 adding the 14th rewrite, 15th presentation, 5th family and 5th upgrade.
  - I tightened the `when` and `lanes` columns so rev 1's sentence fits without clipping.
  - The armed frame moves the 2 warm lanes from rev 9 to rev 10.
- **#11:** the fresh install has no `-` row; each rail runs from the head to the else row.
- **#12:** else rows read `else  no default  → nothing catches it`; in the overlay's underlay they read `else  default  → claude-sub`.
- **#21:** the overlay popup is 118 wide at column 1, so no RULE text shows at its edges.
- **#22:** the rewrites grid in 07-01 and 07-02 matches `converged-01-rewrites.html` frame 1: match at 6, arrow at 39, `route` at 41, `reach` at 83, `hits` against the box.
  Row 2's hits read `≥1` and the else rows carry the same hits as 01-01.
- **Overlay additions:** `p  promote the selected upgrade's newest model`; `↑/↓  past a panel's edge: next panel`; `pgdn  open the detail when it has no room` beside `pgup/pgdn  scroll the detail`.
  `↑` was never in MARKS, so there was nothing to remove.
  The underlay's RULE first line is derived (`anthropic_sub/${model} → claude-sub`) with rows as in 01-01.
- Checks re-run on all three files: every row of all six frames is 120 cells and the font stack is unchanged.
  All six frames were re-shot and viewed.

## Round 3 fixes

- **#3:** the overlay's reordering is one mechanism: `m  lift the row; ↑/↓ move it, m again drops it, esc puts it back` (on two lines); `alt+↑/↓` is cut.
- **#2:** ON A ROUTE lists `←/→  previous or next route` in place of `1-7 switch route`.
- **#4:** settings values are plain; dim only on `none` (transcript roots) and the two `fixed here` rows.
- **#5:** the providers table draws `none` yellow on any on-provider that is not the selected row.
  No such row exists today, so the frame is unchanged; the detail's yellow `none` for `anthropic_sub` stays.
- **#12:** `2 providers on` shows on both config tabs, and the settings edit reads `editing · 2 providers on`.
- **#14:** `p  trust the newest model now, without waiting for evidence`.
- **#16:** MARKS gains `as asked  the name is sent unchanged`.
- **#1:** the 07-02 underlay's reach column now reads `no cred · billed fallback` on rules and else rows ending at claude-sub, and `off · billed fallback` on rules ending at codex, with `billed fallback` yellow (plain in the reversed row).
  `off` stays plain dim for claude-api and openai-chat.
  MARKS gains `billed fallback  the first choice can't serve, so a billed fallback does`.
  The overlay now covers rows 4 to 36, so the reach column of the rules is hidden behind it; only the header rows of the table show.
- The popup is two columns of 50 and 62 cells, 33 rows tall, still 118 wide at column 1.
- Width check (120 cells on every row of all six frames) and font stack re-run; 05 and 07 re-shot and viewed.
  04 is unchanged this round.

## Round 4 fixes

- **Overlay MARKS:** added `warm · cold`, `pinned` and `slug`; `refused · no route` now reads `no rule or default: Toker refuses it`.
  Everything fit once the popup grew to 36 rows (rows 2 to 37), so no existing key or mark was merged or moved.
- **ON A ROUTE:** added `enter  on handled by: turn one on`.
- **`p`:** reads `promote: trust the newest model now, without waiting for evidence`.
- **Settings:** `set by TOKER_DB` is now `set in toker.toml`; `idle-sleep lock` kept.
- **History:** lanes column untouched.
- The frame captions keep their ids (`history`, `armed`, `providers`, `settings`, `empty`, `keys`); my generator now writes them, so regenerating does not drop them.
- Checks: `/tmp/x1gen/check.py` reports 120 cells x 40 rows on every frame of the three files and the Iosevka Term stack; all three re-shot, and the overlay frame viewed.
