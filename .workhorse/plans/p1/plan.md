# A design guide for the TUI

The card's purpose is accurate mockups.
Everything below serves that: an author who has read the guide should be able to draw a frame that looks like what the terminal actually renders, and get the words and the colours right as well as the shape.

## What gets written

Three artefacts, in `.workhorse/design/`:

- `design-system.md` — the language. The ground it renders on, the palette and what each colour means, the modifiers, the glyph vocabulary, chrome, layout and the shedding order, bars and sparklines, copy voice, the interaction model.
  Workhorse's design-sourcing cascade reads this filename, so it is the entry point.
- `mockups.md` — how to author one. The cell grid, what to copy from the pinned frames and what to author fresh, the fidelity rules, and what a mockup must not invent.
- `designs/tui-shell.html` — the copyable shell. The palette as custom properties and a class per style, so a mockup starts by copying a file rather than by re-deriving CSS from prose.

## The line between the spec and the guide

UTUI says what must be distinguishable.
The design guide says how it is distinguished.

So UTUI gains the function and never the form: "a meter approaching exhaustion is marked distinctly from one on track", not a colour or a percentage band; "a session served on a newer model is marked", not `↑`.
The guide carries the hex, the thresholds and the glyphs, where changing one is a design decision rather than a spec revision.

This also keeps `NO_COLOR` conforming.
`strip_colour` drops every colour and keeps the modifiers, so a spec that demanded a colour would make a supported mode non-conforming.

## What lifts into UTUI

- The colour semantics, as functions: what must read as distinct from what, without naming colours or bands.
- The shedding order, in full. UTUI already carries "a table sheds columns before its content is truncated"; the rest of the ordering is the same kind of guarantee — borders before data, TOKENS before CACHE REBUILDS, and the quota block never cut.
- The glyph vocabulary, as functions: what must be marked, not which mark does it.

Numeric thresholds stay in the code, where the constants already carry their rationale, and are repeated in the guide as description.

## The ground, verified

The terminal is Ghostty with no theme set, so it runs the stock palette on a `#282c34` ground, with `faint-opacity = 0.5` and `minimum-contrast = 1`.
The font is Iosevka, which is narrower per cell than a typical monospace; a shell that does not name it misrepresents every width judgement, and width judgements are most of this layout's design.

| ratatui | hex | contrast on ground |
| --- | --- | --- |
| `Reset` | `#ffffff` | 14.00:1 |
| `Yellow` | `#f0c674` | 8.69:1 |
| `Green` | `#b5bd68` | 6.96:1 |
| `Cyan` | `#8abeb7` | 6.75:1 |
| `Blue` | `#81a2be` | 5.23:1 |
| `Red` | `#cc6666` | 3.77:1 |
| `DarkGray` | `#666666` | 2.44:1 |

The hierarchy is the opposite of what an author expects, and it is the single most useful fact in the guide.
Plain white text is the most prominent thing on the screen; red is a dusty rose quieter than yellow, so a meter at 96% reads *softer* than the same meter at 85%.
Colour here is a de-emphasis channel, and emphasis comes from `BOLD` and from plain text.
A mockup reaching for `#ff0000` produces a cockpit warning panel where the real screen is a calm readout.

`Modifier::DIM` maps exactly onto `opacity: 0.5`, which is worth having: dim carries meaning (an idle session, the `-` for "not reported", the rule in ruled chrome), and approximating it as a grey would cut its link to the colour underneath.

## Starting material

`src/tui/snapshots/` and `src/tui/routing/snapshots/` hold pinned full-frame renders — the dashboard at 100×30, 120×40 and 200×30, the legend overlay, the session popup, the routing pages and forms.
The renderer is a pure function of its snapshot, so these are faithful captures rather than sketches: the TUI's equivalent of a design snapshot, and the baseline a mockup starts from.

They carry symbols only.
Style is asserted separately in the tests, so the colours have to be read out of the assertions and the `const` comments in `src/tui/view.rs`.
Worth building one worked example — the 120×40 dashboard, in full colour, as HTML — so the guide proves itself and the next author has something to diff against.

## Interaction in mockups

A mockup may show interaction.
The guide neither prescribes a mechanism nor rules the idea out; that is the mocking author's call.

## Chrome

Settled now: the config pages adopt the caps-unspaced panel title the dashboard and the routing page already share, replacing `┌ configuration ┐` with the `┌PROVIDERS───┐` form.
Config is the only page that diverges, and it is the newest code.

The deeper divergences go to #S1, because they are behaviour rather than wording:

- The dashboard owns a boxed/ruled degradation that sheds borders before data. Routing and config have none, and simply clip when narrowed.
- The dashboard hand-builds every row from `Paragraph` and `Span`; config and routing use ratatui's `List` with `highlight_style`.
- Three key-hint dialects. The dashboard teaches keys through the `?` overlay and has no footer; config writes `space toggles  e edits` on double spaces in the third person; routing writes `n new · e edit` on middle dots in the bare form. The tab strips disagree too, config lowercase and routing in caps.

## Steps

- [x] Write `design-system.md`
- [x] Write `mockups.md`
- [x] Write `designs/tui-shell.html`
- [x] Build the 120×40 dashboard worked example in colour, as `designs/dashboard-reference.html`
- [x] Tighten UTUI: colour as function, glyphs as function, the full shedding order
- [x] Bring the config pages' panel titles into line
- [x] Pin the config pages as frames, which they had never been

## What building it taught

Hand-padding a frame does not work.
The first shell frame came out between 85 and 91 cells a row, which is the exact failure the guide warns about, so both frames are now built by a method that makes the width right by construction: author the plain grid, assert every row, then wrap styled runs in spans without ever rewriting the text.
That rule — wrap, never rewrite — is what lets `dashboard-reference.html` be checked character-for-character against the snapshot it came from, and it is why the width check is in the guide rather than left to the eye.

Reading the styles out rather than guessing them paid twice.
The hit rate's figure is plain and only its bar is coloured, which no reasonable author would guess.
And ses-crow sits at 85% occupancy, over the band that would turn a bar red, but it has been idle ten minutes, so the bar dims and the `← compacts soon` marker never appears — idle wins over the threshold.
Both are now in the reference with the reason beside them.

A correct cell count does not mean a correct render, which is the mistake the width check itself invited.
Both frames counted clean and still tore in the browser, because the font stack led with plain `Iosevka`: that family leaves the characters Unicode calls ambiguous or wide at their natural width, so the arrows, the proportion bar's mark and the em dash each advanced two cells.
The dashboard's cache-read row carries 74 bar marks and rendered 187 cells against its neighbours' 120, which dragged the frame's width out past every panel border in it.
Iosevka Term and Iosevka Fixed force one cell; all three declare themselves fixed-pitch, so nothing but measuring the rendered advance separates them.
The guide now treats miscounting a row and mis-advancing one as two different faults, and says to open the file.

The config pages turned out to have no frame tests at all, alone among the rendered surfaces.
That is why changing their titles broke nothing, and it was also a hole in the guide, which tells a mockup author to start from a pinned frame and could not point at one for those pages.
They now have three: providers, settings, and either page with the service down.
