# The TUI's design language

How `toker tui` looks and reads: the ground it renders on, the colours and marks it uses and what each one means, how panels are framed, what it sheds as the terminal narrows, how it words things, and how an operator drives it.

This file is not normative.
[UTUI](../specs/tui/dashboard.md) is, and the two divide like this:

> UTUI says what must be distinguishable.
> This file says how it is distinguished.

So UTUI requires that a meter approaching exhaustion read as distinct from one on track, and that a session served on a newer model be marked.
It names no colour, no threshold and no glyph.
Those are here, where changing one is a design decision rather than a spec revision — and where `NO_COLOR` stays conforming, because a spec that demanded a colour would make a supported mode non-conforming.

The renderer is a pure function of its snapshot, so `src/tui/snapshots/` and `src/tui/routing/snapshots/` hold faithful captures of real frames.
Read them before drawing anything; [mockups.md](mockups.md) says how.

> [!NOTE]
> Toker's only interface is a terminal, so the general Workhorse mockup defaults — stone greys, a burnt orange accent, Inter, a 4px spacing grid, border radii — describe nothing here and must not be applied. This file is the palette and the metrics for this repository.

## The ground

Toker is developed against Ghostty with no theme override, so the reference ground is Ghostty's stock palette: `#282c34` background, `#ffffff` foreground, `faint-opacity = 0.5`, `minimum-contrast = 1`.
The font is Iosevka Term, which is narrower per cell than most monospace faces.
A mockup that does not name it misrepresents every width judgement, and width judgements are most of this layout's design.
The Term variant specifically: plain Iosevka leaves the arrows, the proportion bar's mark and the em dash at their natural two-cell width, which tears the grid in a browser though never in a terminal.

The TUI sets no RGB colour anywhere.
It uses the ANSI sixteen by name, so the actual pixels are whatever the operator's terminal theme says.
The hex below is the reference theme's answer, not a value in the code.

| ratatui | ANSI | hex | contrast on ground |
| --- | --- | --- | --- |
| `Reset` | default fg | `#ffffff` | 14.00:1 |
| `Yellow` | 3 | `#f0c674` | 8.69:1 |
| `Green` | 2 | `#b5bd68` | 6.96:1 |
| `Cyan` | 6 | `#8abeb7` | 6.75:1 |
| `Blue` | 4 | `#81a2be` | 5.23:1 |
| `Red` | 1 | `#cc6666` | 3.77:1 |
| `DarkGray` | 8 | `#666666` | 2.44:1 |
| `Black` | 0 | `#1d1f21` | — (used only as a foreground on yellow) |

### Colour is a de-emphasis channel

The hierarchy is the opposite of what an author expects, and it is the most useful fact in this file.
Plain white text is the most prominent thing on the screen at 14:1.
Red is a dusty rose at 3.77:1, quieter than yellow at 8.69:1, so a meter at 96% reads *softer* than the same meter at 85%.

Emphasis therefore comes from weight and from leaving text plain, not from colour.
Reaching for a saturated red or green produces a cockpit warning panel where the real screen is a calm readout.

## What each colour means

Colour is assigned by function, and the same function keeps the same colour across every panel.

**Green — a reading that is fine, or a capability that is better than asked for.**
A meter under 80%. A cache hit rate over 95%. Freshness under 30 seconds. A native million-token context ceiling (bold as well, being the best case). A session served on a newer model than it requested, bright on the latest turn and dim on earlier ones. The `over` control, and a held plan release.

**Yellow — a reading worth noticing, or a release that spends.**
A meter between 80% and 95%. A hit rate between 80% and 95%. Freshness between 30 seconds and five minutes. A known context ceiling that is not the native million. A session released past the armed gate (`$`) or to the end of the plan (`%`). A forecast that reaches the armed gate's threshold rather than true exhaustion (`stops ~T`), the `gated ·` prefix, and overage in use. The `burn` control, inverted to `Black` on `Yellow` once armed.

**Red — exhausted, failed, or about to be.**
A meter over 95%, or one whose verdict is `spent` or `out ~T`. A hit rate under 80%. Freshness beyond five minutes, and `no data` on an empty ledger. Errors, failed requests and drift in the requests line, and the sparkline bucket holding an error. A context occupancy over 80% on a live session, with its `← compacts soon` marker. A session whose newest decision was a gate stopping it (`!`). `overage IN USE`. A popup's error message.

**Cyan — identity rather than judgement.**
A session's working directory in its label, and the context occupancy bar at a share that is not yet a concern. Cyan never means good or bad; it distinguishes.

**Blue — one use only.** The `⇄` page-switch chip on the dashboard header.

**DarkGray — chrome that is not content.** The `?` legend chip.

**Plain (`Reset`) — everything else**, which is most of the screen: labels, counts, names, the fields in a detail popup.

### Thresholds

| reading | green | yellow | red |
| --- | --- | --- | --- |
| meter utilisation | ≤ 0.80 | > 0.80 | > 0.95 |
| cache hit rate | > 0.95 | > 0.80 | ≤ 0.80 |
| freshness of the newest row | < 30 s | < 5 min | ≥ 5 min, and on an empty ledger |
| context occupancy | — | — | > 0.80 while not idle |

A session goes quiet after three minutes without a request, and a context panel listing needs at least three requests in the session's main lane before it will claim anything about occupancy.

These live as named constants in `src/tui/view.rs`, each carrying its rationale.
They are repeated here as description; the code is where they are changed.

## Modifiers

**`BOLD` is the emphasis channel**, since colour is not.
It renders as a heavier weight and keeps its colour — the reference terminal does not map bold onto the bright palette.
It marks the best case (a native 1M ceiling), the most urgent case (a stopped session's `!`), the current turn (a bright `↑` against the dim earlier ones), a sparkline bucket holding an error, a ruled panel's title, and a legend key.

**`DIM` means absent, inactive or subordinate.**
It renders at half the foreground's opacity, so it keeps the colour underneath rather than flattening to grey — which matters, because a dim green `↑` is still a green `↑`.
It marks an idle session and its bar, a `?` for an unknown ceiling, a proportion bar in TOKENS, a reset clock beside a verdict, the `estimating` and `window rolled over` verdicts, a rule in ruled chrome, an unavailable control in a popup, and a footer.

**`REVERSED` means a handle to press or a row in focus.**
The header chips, and the selected row in a list.

**`NO_COLOR`.**
Set to a non-empty value, `strip_colour` clears every foreground and background off the finished buffer and keeps every modifier.
This runs once over the frame rather than at each style site, so no panel can forget it.
Bold and dim therefore have to carry the reading alone, which is why nothing is distinguished by colour and colour only.

## Glyphs

### The operator's vocabulary

These 19 are documented to the operator in the `?` legend, which makes them a contract rather than a habit.
The table is `LEGEND` in `src/tui/view.rs` verbatim, so a drift between the two is a plain diff.
The legend and the renderer must agree: a mark missing from it is undocumented, and an entry with nothing behind it describes a screen that does not exist.

| mark | meaning |
| --- | --- |
| `↑` | served on a newer model: bright on the latest turn, dim earlier |
| `$` | released past the armed quota gate for this window |
| `%` | released to the end of the plan, stopping before overage |
| `!` | stopped: the last thing this session got was a gate notice |
| `↺` | compactions so far; - for none |
| `-` | not reported, which is not zero |
| `?` | unknown: model, context ceiling, or token count |
| `≥` | a floor: some rows unknown, or the span predates the data |
| `<1%` | spent less than the meter's 1% step shows; 0% is none at all |
| `·` | no requests in that slice of the window; red holds an error |
| `on track` | the meter resets before it runs out |
| `estimating` | too little data to separate burn from noise |
| `stops ~T` | reaches the armed gate's threshold at T |
| `out ~T` | runs out at T |
| `gated ·` | past the gate; the countdown is to exhaustion |
| `on track?` | a trailing ?: the gate's state is assumed, not observed |
| `last req` | green under 30s, yellow under 5m, red beyond |
| `ctx` | green: native 1M; yellow: another known ceiling |
| `idle` | dim once a session has been quiet 3m |

### Structural glyphs

`─ │ ┌ ┐ └ ┘` frame panels and popups.
`·` separates clauses on a line as well as marking an empty bucket.
`→` shows a transition (`claude → anthropic_sub`, a forced model rewrite, a byte count before and after).
`←/→ ↑/↓` name arrow keys in a footer.
`…` marks a clipped name or a `… N more` list cut.
`×` and `≠` appear in comparisons, `≈` in an approximation.

## Bars

Three kinds, each with its own fill, and they are not interchangeable.

**Occupancy — `█` filled, `░` padded.**
A share of a known ceiling, where the empty part is as meaningful as the full part.
Quota meters, context occupancy, the cache hit rate.
Filled to `round(frac × width)`, always drawn to full width.

**Proportion — `▬` filled, space padded.**
One bucket's share of a total, where only the filled part means anything and the row continues past it.
The TOKENS rows and the rebuild causes.

**Sparkline — `▁▂▃▄▅▆▇█`, with a dim `·` for an empty bucket.**
The requests line, which always spans the whole window: as the terminal narrows the minute buckets fold into fewer wider ones rather than the line cutting to the newest minutes and silently dropping the rest.
A bucket's level is `ceil(requests × 8 / max)`, so any activity at all shows at least `▁`.
An empty bucket is a dim `·` so a quiet stretch reads as quiet rather than as a gap; a bucket holding an error is bold red.

A quota bar is at most 22 cells and shrinks as the panel narrows.
Below 10 cells it stops reading as a share, so the line sheds a clause instead — the reset clock is the meter's other answer.

## Chrome

**A panel title is upper case and inside the border**, run straight into the rule with no padding: `┌SESSIONS────────┐`, `┌DETAIL────┐`, `┌LEGEND────┐`.

**Panels are boxed while the page has room and ruled when it does not.**
A boxed panel costs two rows; the ruled form is a bold title, a single space, then a dim `─` run — `SESSIONS ────` — and costs one.
The ruled form still insets its content by a column each side, so every width-driven choice is the same in both.
The rule stops a cell short of the edge, so two panels side by side never run their titles into each other.

This degradation is the dashboard's.
Routing and config do not have it yet and clip when narrowed; settling that is card S1's work.

**A page with views carries a numbered tab strip** above its body, the current view in bold: `1 providers   2 settings`.
Config writes its tabs in lower case and routing writes them in capitals, which is the same divergence as the footers and goes to card S1 with them.

**A popup is centred, cleared behind, bordered, and padded a column each side.**
It is as wide as its widest line within a floor and a ceiling, never wider than the frame.
Overlays are exclusive: opening one closes the other.

**The header carries its chips at the far left**, reversed: a `?` for the legend, then a `⇄` for the page switch.
On the dashboard the `?` is three cells and `DarkGray`, the `⇄` is a single `Blue` cell in the gutter beside it, and the `⇄` disappears below 80 columns where the status needs every cell and `Tab` is the way across.
On the routing page both are three cells and neither is coloured — a fourth divergence for card S1.
On a one-row terminal the `?` survives even when the header has given its row away, because it is the way out to the legend at any size.

## Layout and shedding

The dashboard stacks SESSIONS, CONTEXT, TOKENS, CACHE REBUILDS — with SPEND taking half the rebuild row when the window carries a cost — and then the bottom strip holding RATE & QUOTA.
The two lists share whatever the fixed sections leave, each floored at a row and cut with a `… N more` line.
On a tall terminal the spare rows go to the lists rather than leaving the bottom strip floating above blank ones.

**What goes, in order.** Borders before data, because a border is the cheapest thing on the page to lose.
Then TOKENS sheds rows down to its hit-rate pair, then CACHE REBUILDS down to its summary.
A column sheds before its content truncates.
Only a terminal too short for any of that cuts whole rows from the top of the middle, and the quota block is never cut — it is the part worth watching.

**The label column is the one that resists.**
A session's name renders at twelve cells or more, below which the column shows the eight-character short id instead, because too narrow a name is noise.
The table gives the label its leftover width and sheds a numeric column rather than let the label drop under twenty cells: a title is worth more than a peak or an output total.

## Copy

**Lower case throughout**, except panel titles and the word Toker.
Field labels, column headings, verdicts and key hints are all lower case.
The routing page's tab labels are the one exception that is not meant to be one, and go to card S1 with the rest.

**`·` separates clauses**, with a space each side: `last 30m · 2 sessions · 17 requests`.

**Absence is written, never computed away.**
This is UTUI's rule and the vocabulary is fixed: `-` for not reported, `?` for unknown, `≥` for a floor, `N req unknown` for a partial bucket, `no cost data: N` for unpriced requests, `no requests in window`, `no data in window`, `no session with enough history yet`, `no meter reading`, `window rolled over`, `estimating`.
A zero is only ever written when the real answer is none at all, which is why `<1%` exists as a separate reading from `0%`.
A counter that would read zero is left out rather than rendered — a zero drift count is noise on every quiet frame.

**Numbers are comma-grouped** and may follow the host's locale, except in a gate notice, which is byte-stable.
Token counts abbreviate to `1M`, `200k`, `872k`.
Costs carry their precision rather than rounding to nothing.

**A reading says what it is, not how confident it is.**
`stops ~14:53` and `out ~Thu 08:49` differ because a gate is recoverable and exhaustion is not.
A trailing `?` on a verdict means the gate's state was assumed rather than observed.

## Interaction

**Keys are global where they can be.**
`q` quits from any page, though not from inside a modal, which takes every key. `Tab` cycles pages. `?` opens the legend and `?` or `Esc` closes it. `Esc` returns to the dashboard from another page. `r` refreshes. `↑`/`↓` move a selection, with `k`/`j` alongside them, and `←`/`→` with `h`/`l` move between views.

**Focus is a reversed row**, and a list scrolls to keep its selection in sight.

**A modal takes every key** until it is dismissed, and a key arriving while a command is in flight is ignored rather than queued.

**A destructive or spending action arms rather than fires.**
The wider quota release needs a second confirmation, and an armed control inverts to show it is live.
An unavailable control stays visible and dim with the reason in place of its label, rather than disappearing.

**A control is written `[key] label`**, three spaces between controls.

**The mouse works where the keyboard does.**
Every list records the rectangle of each row it drew, so a click names the row it landed on, and the header chips are clickable.

**Keys are discoverable, but not yet consistently.**
The dashboard teaches them through the `?` overlay and has no footer; config and routing each carry a footer in a different dialect.
That divergence is card S1's work, and until it lands, follow the page you are mocking rather than inventing a fourth form.
