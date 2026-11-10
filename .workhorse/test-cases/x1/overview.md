# Routing and configuration pages

Scenarios that show the redesigned routing, providers, settings and accounts pages, and the operations behind them, work.
Ticked cases are covered by an automated test; unticked ones still want a person at a real `toker tui` against a copy of the real store.

## Saving and the live revision

- [x] A save applies an edit to the active revision and goes live for new and cold lanes (verifies spec: RREV).
- [x] A save made against a revision that has since moved is refused with a conflict naming the live one, and the form keeps what was entered (verifies spec: RREV).
- [x] A save that does not compile changes nothing and reports diagnostics with a sentence and a fix (verifies spec: RREV).
- [x] Widening cannot narrow, and widening to immediate needs consent where a warm lane would lose its cache (verifies spec: RREV, RPIN).
- [x] Reactivating an earlier revision copies it into a new active one for new and cold lanes (verifies spec: RREV).
- [ ] Save an edit on a copy of the real store, then check a running session on the old revision is untouched.

## Preview and replay

- [x] A recorded request written before the resolver's facts were recorded is reported as not replayable, never replayed with a guessed fact (verifies spec: RPROV, RREV).
- [x] Replaying the last 24 hours under an unsaved edit lists only requests whose destination changes, and counts the unreplayable ones (verifies spec: RREV).
- [x] A prefixed model naming a disabled provider is served by its route's fallback, and refused where the route has none (verifies spec: RSOLV).
- [ ] The edit preview against a copy of the real store reads as plausible: the changed requests are ones the edit should touch.

## Facts the pages read

- [x] Each rewrite, default and presentation reports one of the five reach outcomes, naming the edge or setting that stopped it (verifies spec: UTUI).
- [x] Shadowing is reported for exact, prefix, glob and regex rules.
- [x] A route, provider, upgrade policy and compaction target report what refers to them and what they reach.
- [x] Hit counts read as a floor where the window reaches rows older than the recorded facts, and as not recorded where none carry them; absence is never shown as zero.
- [ ] Compare hit counts for a day against a count made by hand from the copied store.

## The routing page

- [x] Every frame of the converged design draws at its mockup size, and at 100x24 and 80x24 (verifies spec: UTUI).
- [x] An empty policy draws every protocol refused and points the add control at routes first.
- [x] The keys overlay names every key the page owns and explains every mark it draws.
- [x] Adding, removing and moving a rewrite, and adding, removing and reordering a route's candidates, produce the right typed edits.
- [x] A removal the rest of the policy refuses names the records that still use it.
- [x] Trace shows the outcome first, marks the catching row and the rows fallen past, and says so for an unreplayable request.
- [x] The history tab shows what each revision changed in words, and moving running lanes arms first, naming the lanes that would lose a warm cache.
- [ ] Use the page for a real change end to end: add a fallback, save, trace a request, move running lanes.
- [ ] Mouse clicks on the header page buttons switch pages at each width.

## Configuration pages

- [x] A settings edit and a provider edit leave every other field as it was, including session headers, transcript roots and the refresh URL (verifies spec: DCONF).
- [x] A cold-gate minimum larger than every resolvable context window is refused, naming the largest; it is accepted where none resolves (verifies spec: GCOLD).
- [x] Token counts accept full figures and the `175k` and `1M` shorthand.
- [x] A refusal from the service shows beside the field with the typed value kept.
- [x] The providers page opens on an enabled provider that is not configured, and clears a safeguards rejection mark.
- [ ] Edit each setting on a copy of the real store and confirm the live service reads it without a restart.

## Accounts page

- [x] Only a credential no source has resolved can be named; naming a known account does nothing (verifies spec: UTUI).
