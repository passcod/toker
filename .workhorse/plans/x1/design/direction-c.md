# Direction C: design around the question

Card X1.
Mockups: `.workhorse/design/mockups/x1/c-chain.html`, `c-draft.html`, `c-records.html`, `c-config.html`, `c-narrow.html`, twenty frames in all.

Everything not in the live captures is drawn: the fifteen presentation ids, the rewrite priorities, the default targets, every recent request shape except the last one, and the three-candidate draft route.
Every caption says what the operator just did.
No account identity from the captures appears.

## What the direction is

One sentence: the routing page opens on a chain, the path one request takes through presentation, rewrite, default, route, candidate, binding, provider and gate, and every link on it is the record behind it, so you reach a record by asking about a request, and the record lists are a secondary page.

The pages:

| page | holds |
| --- | --- |
| `1 chain` | the request and its answer on top, recent request shapes and defaults on the left, the chain on the right; the same drawing, read from a route, gives the route map |
| `2 records` | a kind rail (presentations, routes, defaults, rewrites, classifiers, upgrades, compaction, revisions, draft, catalogue, and pointers to providers and settings) and one list per kind |
| `3 revisions` | history, publication, warnings, the catalogue grouped by binding |
| config | providers and settings, kept as a page of its own, but each provider row says what leads to it |

## The decisions the brief asked for

**The chain at rest.**
The last request the proxy served, with the link that decided it selected.
It is the one chain that is guaranteed to be real, it ties to the dashboard's `last req`, and it is the likeliest thing the operator came to ask about.
The rail beside it lists recent request shapes and each scope's default, marking the ones that cannot serve, so a broken chain is visible before anything is opened, and a fresh install with a ledger still has chains to draw from its defaults.

**A chain where something is wrong.**
Frame 3.
The walk goes default, route, candidate, binding and stops at the provider link, red, `cannot serve`, and the answer line above says `nowhere` and why.
The link carries the ways out: `k` sets a credential in place, `o` opens the provider, `b` adds a fallback behind the candidate, and `g` reroutes the request.
The rail marks the same request `cannot serve`, and the provider page says which routes and defaults lead to a provider that cannot serve.

**Editing from a link.**
Frame 6.
`e` on the link opens the record behind it, and the chain stays on screen above the form, showing the draft as last saved.
It does not redraw while a field is half typed, because the page only ever resolves saved documents.
On save it redraws against the draft; if the request now routes differently the rail and the header say `differs`.

**Draft against active.**
Frames 7 and 8.
The chain is drawn once for what is the same and twice, an active line over a draft line, from the first link where they diverge.
The decision is then what changes and who moves: records changed, requests that route differently, the billed boundary crossed, and per lane scope how many lanes move.
There is nothing to validate.
A saved draft was checked as each field was edited, so the screen says so in one dim line, and the apply control arms because it can start spending.
The old modal's counts survive; its cost-boundary lines and its error text do not.

**Where the records live.**
The `records` page: a kind rail with counts, one list per kind, `n` creating a record of the kind in focus without asking which.
Records no chain touches are listed like any other, and the revisions page holds history, publication, warnings and the catalogue.
The 1,416 rows are 17 small things and 1,399 catalogue offers; the offers are grouped by binding, counted, and open on demand.
Fifteen identical presentations are grouped under the scope that shares the slug template, and differ by what they select.
Four kinds in one list are four rail rows.

**Where a gate lives.**
A gate is the last link of the chain, and its setting lives on the settings page.
It is a link because a gate is the other reason a request does not go where the policy says, and the question "why did it not go" has to include it.
It is not part of resolution, though: it reads live evidence, so for a named request it says `not checked` rather than guessing, and for a real session it states the reading (`6,883 tokens, under the floor`).

**Provider that is on but broken.**
Frames 3 and 13.

**Empty.**
Frame 15: no chain to draw, why a request would fail, the providers that could serve, and `n` making a route and its default together.

## Changes forced by the operator's own steer

**Ordering is the semantics.**
A selected presentation, rewrite or default link opens a table in firewall order: priority, rule, what it matches, and `matched`, `fell past` or `not tried`.
The rows that fell past are the debugging.
Records lists sort rewrites by priority within scope and say so in the group heading.

**No JSON.**
Nothing draws a record as JSON and nothing asks for it.
`ROUTING PREVIEW` is replaced by the chain.

**Live validation, `V` gone.**
Frames 6 and 14.
A field says what is wrong beside itself, a form that cannot be saved shows the control dim with the reason, and a `try it` row tests the request against the matcher as typed.
There is no validate key and no report.

**Core first.**
A link is one line: a verdict, a kind, a name.
The table, the alternatives and the controls appear only on the selected link.
A collapsed candidate has one sub-line, and the blocks (upgrade, alias, circuits) appear when the candidate is selected.

## One drawing or two

One drawing serves both readings, with a different top.
Frame 9 is the same skeleton as frame 2: what sends requests here, the ordered links, the candidates with their steps, and the ends.
Read from a request, one path is lit and the rest are dim; read from a route, nothing is selected as taken, every step and end is listed, and the top lists what matches into the route rather than what the request matched.
They are one surface with two entry points (the rail for a request, the route list for a route), and they share the keys and the link model.
What differs is data, not layout: a route has no request, so no `not reached`, no rewrite table and no gate verdict.
What would make them genuinely two is a route with a dozen candidates and fallbacks; there the map wants a collapsed-by-default tree, which the request chain does not need.

## What an operator sees first, second, third

1. **The answer.**
   The request panel's second line: where it goes, as what model, whether it bills, and which link decided it.
   Or `nowhere`, with the reason.
2. **The decisive link, selected.**
   The chain opens with that link expanded, because the next thing an operator wants is the record to change.
3. **The rest of the chain, dim where it did not apply.**
   It confirms the answer and shows what else could have mattered.

The rail is peripheral.
The order is right because the operator's question is "where does it go", then "why", then "what else is there".

## Steps, against today

Steps are key presses from the dashboard, as in survey section 2, not counting typed values.

| task | today | here |
| --- | --- | --- |
| check why a request went somewhere | about 5 and raw JSON, for an invented request only | 1 for the last real request (the page opens on it), 3 for another (`t`, type, enter) |
| change which model serves a request | 12 to 20 | 4 to 6: `g`, choose, enter, then `a`, `y`, `y` to apply |
| add a fallback | 22 `Down` presses and dozens of fields | `b` on the candidate, then the new candidate's binding and the step's triggers, each validated beside the field; the step starts enabled |
| fix a provider without a credential | `Tab Tab`, move down, `e`, type, Enter, about 8 | 3 from the broken link (`k`, type, enter) |
| turn a gate on or off | 4 to 5 | 4, the same: `Tab Tab`, select, `space` |
| activate | 3 to 5, with a modal of counts | 3 (`a`, `y`, `y`), with the differences drawn first |
| compare draft with active | not possible outside the modal | 1, `c` |
| reactivate a revision | 5 | 4 |
| create a record from nothing | `n`, choose kind, fill template | 3: `2`, select the kind row, `n` |
| promote a model | 2 and a JSON popup | 2, and the result is a line |

## What it is worse at

- A record with no request to ask about takes one more step than today: a classifier, an upgrade or the compaction policy is two keys from the page's start, through the records page, and the chain page says nothing about them until a candidate names the upgrade.
- The chain covers the requests the ledger has seen and the ones the operator can name.
  The apply screen's "routes differently" count is over recent shapes and the defaults only, so a change that only affects a request nobody has made yet is in `records changed` and nowhere else.
- Preview has no circuit or per-request credential evidence, so a chain can disagree with what the live proxy just did.
  The rail and the `not checked` gate say so, but the operator has to read it.
- Gates are one link and one dim line; an operator who wants the whole gate behaviour has to leave for settings.
- Narrow terminals shed the rail, so picking another request is `s` and a popup rather than a list in view.
- `g` writes an exact rewrite for one model, and the habit accumulates those.
- Fixed-position kind columns eat 28 columns of every link row, so the longest ids lose a character or two (`legacy-force-newest-openrouter-openrouter_messag…`).
  The shared prefix of an id is dimmed in a wide row and elided to `…` in a narrow one; that is a typographic mark in the legend, not a glyph.

## Notes on internals

None of these is in the mockups; each would make the direction cheaper or truer.

- **Per-record validation without saving.**
  Live validation needs an endpoint that checks one record, or one field, against the draft and returns what is wrong, so the form can show it as the operator types.
  Without it the form can check only syntax locally, and a clash with another record (a priority already taken, a route that does not exist) is found at save.
- **Resolve against a form's unsaved record.**
  The live `try it` row and a chain that redraws while typing want a preview of draft-plus-this-record, not just the saved draft.
- **Preview against a chosen revision.**
  The draft-against-active frame needs the same request resolved under two documents in one call, for a batch of request shapes.
- **Recent request shapes from the ledger.**
  The rail wants distinct (profile, protocol, model) with a last-seen time and a count.
  Per-record hit counts from provenance would fill the `hits` column; today most rows show `-`.
- **A route and its default created together.**
  The empty state's `n` writes both into the draft in one save.
- **The config page's whole-row writes.**
  Editing a gate writes the row back from a partial copy, which loses `session_headers`, `transcript_roots` and `refresh_url`.
  This is a defect, and it constrains the settings page this direction adds rows to.
- **Credential source.**
  The provider page draws a source choice (none, keyring, env, stored) because UTUI says the page can set it; the page does not send it today.
- **The Claude settings note.**
  The header carries it as `claude settings not synced`, and the revisions page's publication row holds its detail; the clipped path in the old header goes away.
