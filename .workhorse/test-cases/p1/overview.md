# Verifying the TUI design guide

The guide's job is accurate mockups, so most of what is worth checking is whether its claims still match the product.
A guide that has drifted is worse than none, because it is believed.

## The artefacts hold together

- [x] Every row of every `class="frame"` block in `designs/tui-shell.html` is the same width
- [x] Every row of the frame in `designs/dashboard-reference.html` is 120 cells
- [x] The reference frame's text is character-for-character the 120×40 snapshot, once spans are stripped
- [x] No frame names plain `Iosevka`, which advances the arrows, the proportion mark and the em dash by two cells each
- [x] Every row of every frame advances the same number of *cells* in the family the stack selects, not merely the same number of characters
- [ ] Both files render in a browser with every row ending in the same column, and no row wrapping
- [ ] The reference frame, viewed side by side with `toker tui` at 120×40, matches in colour as well as shape

## The guide matches the code

- [x] Every link and heading anchor in the specs resolves (`.github/check-spec-links.py`)
- [ ] Each colour in the guide's function table is the colour the code applies at that site
- [ ] Each threshold in the guide's table is the constant the code uses
- [ ] The nineteen legend entries in the guide match `LEGEND` in `src/tui/view.rs`
- [ ] No mark the dashboard renders is missing from the legend, and no legend entry lacks a mark (verifies spec: UTUI#marks)

## Chrome

- [x] The config pages render `┌PROVIDERS───┐`, `┌SETTINGS───┐` and `┌CONFIGURATION───┐`, flush against the rule as the dashboard's and the routing page's are
- [x] The providers page, the settings page and either page with the service down are pinned as frames, so a mockup author has somewhere to start and a chrome change cannot pass unnoticed
- [x] A config page with the service down offers no values to edit and says why (verifies spec: UTUI#the-configuration-pages)
- [x] A provider's credential renders as presence only, never as a value (verifies spec: SEC#a-credential-is-never-logged-or-displayed)

## The spec holds without naming form

- [x] UTUI names no colour, no threshold and no glyph — it mentions colour only in the rules about colour
- [ ] The dashboard remains readable with `NO_COLOR` set, every distinction still carried (verifies spec: UTUI#distinguishing-a-reading)
- [ ] A terminal narrowed step by step sheds in the order UTUI states, framing first and the quota block never (verifies spec: UTUI#what-is-shed-first)
