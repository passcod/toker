# Settling the TUI's chrome across its pages

The four pages (dashboard, routing, config, accounts) move to one convention for framing, rows and key hints.

## Decisions

- **Key hints live in a `?` overlay per page, and no page carries a footer.**
  The dashboard's `?` keeps its legend of marks and gains a keys section; routing, config and accounts each list their own keys.
  A modal or a text entry, which takes every key including `?`, still carries its own one-line prompt.
- **Hints read `e edit · n new`:** bare verbs, lower case, `·` between them, arrows as glyphs.
  An underlined mnemonic was considered and dropped: routing's keys include arrows, `Ctrl+S`, digits, `Enter` and a shifted `V`, none of which a letter in a label can carry.
- **Rows are hand-built everywhere**, on the dashboard's list window: a selected row reversed, `… N more` and `… N above` counts, and every drawn row's rectangle recorded for the mouse.
  ratatui's `List` goes, which also fixes config's click landing on the wrong row once its list has scrolled.
- **Framing is shared:** `Chrome` and `panel` leave `view.rs` for a module every page draws through, and every page goes ruled when its content does not fit boxed.
- **The header is the same row on every page:** the dashboard's `?` and `⇄` chips at the far left, then the page's own tabs or status.
  `⇄` and `Tab` both go to the next page, on every page.
- **Tabs are lower case**, the current one bold and reversed, the rest dim, every tab clickable.
  When the strip does not fit, the other tabs collapse to their number, then the current label clips with `…`.
- **Routing stacks its list over its detail below 100 columns**, the list taking what it needs up to a third of the body.
- **`NO_COLOR` is applied once over the finished frame of whichever page is showing**, rather than on the dashboard alone.

## Checklist

- [ ] Shared chrome module: `Chrome`, `panel`, header chips, tab strip, list window with selection, key overlay, `clip`, `strip_colour`
- [ ] Dashboard draws through it; legend gains a keys section
- [ ] Config: header and tabs, chrome switch, hand-built rows, inline entry, message line, overlay; `←/→`, `h/l`, `j/k`
- [ ] Accounts: header, chrome switch, entries collapsing to their head line when short, selection window, message line, overlay; `j/k`
- [ ] Routing: header with tabs and status, stacked layout below 100 columns, chrome switch, hand-built rows, overlay, modal and form prompts in the settled dialect
- [ ] Loop: chips and `?` on every page, `⇄` as next page, colour stripped on every page
- [ ] UTUI: shedding and the key overlay apply to every page
- [ ] Design guide: chrome, copy and interaction describe the settled convention; S1 references gone
- [ ] Tests and snapshots; test cases ticked
