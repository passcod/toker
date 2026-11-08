# Drawing a TUI mockup

How to produce a mockup of `toker tui` that looks like what the terminal actually renders.

A mockup here is a standalone HTML file holding a monospace grid in a `<pre>`, with a `<span>` around every run that differs in style.
HTML rather than a fenced text block because colour, dim and reverse carry half the meaning in this TUI, and a plain grid drops all three — see [the design language](design-system.md), which is the companion to this file and says what the colours mean.

Start from [`designs/tui-shell.html`](designs/tui-shell.html): copy it, and the palette, the cell metrics and a class per style are already right.
[`designs/dashboard-reference.html`](designs/dashboard-reference.html) is the pinned 120×40 dashboard drawn in that shell, which is the thing to diff against when a mockup looks off and you cannot say why.

## Where the file goes

A mockup for a card goes in `.workhorse/design/mockups/{card-id}/{slug}.html`, which is scratch and is emptied when the card merges.
One that someone outside the card will need goes in `.workhorse/design/designs/{slug}.html` instead, and persists.

## Start from a real frame

The renderer is a pure function of its snapshot, so the repository already holds faithful captures of most screens:

| file | screen |
| --- | --- |
| `src/tui/snapshots/…view…frame_100x30` | the dashboard, all panels |
| `src/tui/snapshots/…view…full_frame_120x40` | the dashboard with room to breathe |
| `src/tui/snapshots/…view…quota_frame_200x30` | the quota block at full width |
| `src/tui/snapshots/…view…legend_100x30` | the `?` overlay over the dashboard |
| `src/tui/snapshots/…view…full_frame_100x30_scrolled` | the lists scrolled, with `… N more` |
| `src/tui/snapshots/…detail…session_popup_100x30` | a session detail popup |
| `src/tui/routing/snapshots/…the_presentations_page` | the routing master/detail page |
| `src/tui/routing/snapshots/…the_*_form` | the routing modal forms |
| `src/tui/config/snapshots/…providers_100x24` | the providers page |
| `src/tui/config/snapshots/…settings_100x24` | the settings page |
| `src/tui/config/snapshots/…unreachable_100x24` | either config page with the service down |

Find the one that covers the screen you are changing, paste its grid into the shell, and change only what the work changes.
Do not rebuild a screen from your reading of the rendering code: the snapshots are what it produces, and re-deriving a layout is how column positions and padding drift.

Where no snapshot covers the screen, read the rendering code for that panel in `src/tui/` and follow the conventions in the design language for anything new.

**The snapshots carry symbols only.**
`insta` pins each cell's character and discards its style, so every colour has to come from somewhere else: the `const` comments and `Span::styled` calls in `src/tui/view.rs`, the style assertions in that file's tests, and the colour rules in the design language.
This is the part a mockup most often gets wrong, so do it deliberately rather than by eye.

**Wrap, never rewrite.**
Add colour by putting a `<span>` around a run of the text that is already there, rather than retyping the line with markup in it.
Retyping is how a cell goes missing, and a span wrapped around existing text cannot change the width by construction — which is what lets
`designs/dashboard-reference.html` be checked character-for-character against the snapshot it came from.
The same holds when you author a new region: get the plain grid right and padded first, then style it.

## The grid

The shell names Iosevka Term, and a frame takes its width from its own longest row.
Iosevka is narrower per cell than most monospace faces, so a frame set in a generic `monospace` comes out noticeably wider than it ever renders, and every judgement about whether a column fits is then made against the wrong width.

A row has to be exactly as many cells wide as the frame you are drawing, 120 for a 120x40, and there are two separate ways to get that wrong.

**Miscounting.** A hand-padded row that runs a character short or long disagrees with its neighbours, and the panel borders stop lining up with the content.
Count rather than trusting the look.

**Mis-advancing.** A row holding the right number of characters can still render too wide, because a browser is not a terminal: it has no cell grid to enforce, and a font may advance a glyph by two cells if it likes.
A terminal draws the arrows, the proportion bar's mark and the em dash one cell each, and so do Iosevka Term and Iosevka Fixed.
Plain Iosevka gives them two, which is enough to push one bar row half again as wide as the frame around it.
Name `Iosevka Term` or `Iosevka Fixed`, and never plain `Iosevka`.
All three declare themselves fixed-pitch, so no amount of counting characters distinguishes them.

## What to copy and what to author

**Copy, exactly:** every character of the regions the work does not change, their column positions, and the real copy including its capitalisation.
Keep the sample data from the snapshot rather than substituting your own — the names, counts and models in it were chosen to exercise the panel.

**Author fresh:** only the feature or tweak being mocked.
Apply the design language to it: colour by function, bold for emphasis, dim for absent or inactive, the right bar for the kind of quantity, the fixed absence vocabulary, lower case.

**Do not re-style a region you are not changing**, even where you can see an improvement.
A mockup that quietly improves the surrounding screen is the main way a mockup stops being evidence of anything.

## What a mockup must not invent

- **A colour outside the sixteen.** The TUI sets no RGB anywhere. If a reading needs a distinction the palette cannot make, that is a finding to raise, not a hex value to pick.
- **A confident zero.** Absence is written. Use `-`, `?`, `≥` or the right phrase from the vocabulary; a mocked-up zero teaches the wrong thing about what the screen does.
- **A glyph outside the legend**, for anything an operator reads as a mark. The legend is a contract. A new mark means a new legend row, which is a design decision worth stating.
- **A reading that colour alone distinguishes.** `NO_COLOR` strips every colour and keeps the modifiers, so check your mockup still reads with the colours off.
- **Chrome in a form no page uses.** Panel titles are upper case and unspaced inside the border.

## Interaction

A mockup may show interaction — a selection moved, a modal open, a control armed, a sequence of frames.
How is yours to decide; nothing here prescribes a mechanism or rules one out.

## Checking it

- Every row is the same number of cells, and that number is the frame width you claimed. Hand-padding gets this wrong often enough to be worth checking rather than eyeballing:

  ```sh
  python3 - mockups/my-card/my-mockup.html <<'EOF'
  import html, re, sys
  for path in sys.argv[1:]:
      for n, f in enumerate(re.findall(r'<pre class="frame">(.*?)</pre>',
                                       open(path, encoding='utf-8').read(), re.S), 1):
          w = {len(html.unescape(re.sub(r'<[^>]+>', '', l))) for l in f.split('\n')}
          print(path, f'frame {n}:',
                f'{w.pop()} cells' if len(w) == 1 else f'RAGGED {sorted(w)}')
  EOF
  ```

  It matches `class="frame"` exactly, so a key or swatch carrying another class is skipped.
- The frame names a single-width family. Counting characters cannot catch a glyph that advances two cells, so check the stack itself:

  ```sh
  grep -n font-family mockups/my-card/my-mockup.html
  ```

  Plain `Iosevka` anywhere in that list is a bug, whatever its position. Name `"Iosevka Term"` or `"Iosevka Fixed"` and let it fall through to generic `monospace`.
- Open it in a browser and look at it. A panel border that stops short of the content beside it means the grid has torn, and neither check above will have told you.
- Lay it beside the snapshot you started from and confirm the unchanged regions are character-for-character identical.
- Turn the colours off and confirm it still reads.
- Check each colour you used against the function table in the design language, rather than against whether it looks right.
