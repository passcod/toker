# Settling the TUI's chrome across its pages

Scenarios checking that the four pages share one header, one framing, one way of building rows and one way of teaching keys.

## Header and tabs

- [ ] Every page draws the `?` chip three cells wide in `DarkGray` and the `⇄` chip one cell wide in `Blue`, and drops the `⇄` below 80 columns
- [ ] Clicking `⇄` on any page goes to the same page `Tab` does
- [ ] Clicking a chip does nothing while a routing form, a config entry or an accounts naming entry is open
- [ ] Config's and routing's tabs are lower case, the current one bold and reversed
- [ ] On a narrow terminal routing's other tabs collapse to their number before the current label clips with `…`
- [ ] Routing's `draft N · working` status clips only once the tabs have collapsed
- [ ] Clicking a tab on config or routing switches to that view

## Framing and shedding (verifies spec: UTUI)

- [ ] Config goes ruled on a terminal too short for its rows boxed, and stays boxed on one that fits them
- [ ] Accounts sheds the unselected entries to their head line once they do not all fit, the selected entry keeping its fields
- [ ] Routing goes ruled when either its list or its detail pane would lose content to a border
- [ ] Routing stacks its list over its detail below 100 columns, and sits them side by side from 100
- [ ] A list cut short counts what it left out above and below, on config, routing and accounts
- [ ] A row longer than its panel ends in `…` rather than being cut at the edge
- [ ] `NO_COLOR` strips every colour from the routing, config and accounts pages, not only the dashboard

## Rows and the mouse

- [ ] On config, a click on a row selects the row drawn there once the list has scrolled
- [x] On routing, a click on a row selects the row drawn there once the list has scrolled
- [ ] On accounts, a click selects the entry it lands on, and the wheel moves the selection
- [ ] The selected row is reversed across the panel's whole width

## Keys

- [ ] No page draws a footer
- [ ] `?` opens each page's own overlay: LEGEND with its marks and keys on the dashboard, KEYS on the others
- [x] The routing overlay names every key the page acts on (verifies spec: UTUI)
- [ ] Every key a page forwards is named in its overlay, on config and accounts as on routing (verifies spec: UTUI)
- [ ] `Esc` with an overlay open closes the overlay and leaves the page where it was
- [ ] Config and accounts move their selection with `j`/`k`, and config switches views with `←/→` and `h`/`l`
- [ ] Config's text entry and accounts' naming entry each show their keys under the entry
- [ ] Routing's modals and forms write their prompts as `y/enter confirm · n/esc cancel`

## Messages

- [ ] A refused config value shows under the header in red, with the list still standing below it
- [ ] An accounts read that fails says so over the last list read rather than replacing it
