# Survey: the settings and routing pages

Card X1 asks for the settings and routing interface to be redesigned for usability.
This document describes what exists today, so that a designer who has never seen the codebase can work from it.
It proposes nothing.

Paths are relative to the repository root unless stated.
`file.rs:N` cites a line in the checkout this survey was written against.
Spec ids (`RDOC`, `UTUI` and so on) are the `id` in each spec's frontmatter, and `#slug` is a heading anchor within it.

The two pages in scope are both terminal pages of `toker tui`.
The config page is `src/tui/config/`, with tabs `1 providers` and `2 settings`.
The routing page is `src/tui/routing/`, with tabs `1 presentations`, `2 routes, candidates & defaults`, `3 rewrites, families, upgrades & compaction` and `4 preview, revisions & activation`.
The dashboard (`src/tui/view.rs`, `src/tui/model.rs`) is out of scope beyond the shared frame in `src/tui/chrome.rs`.

## 0. How the pages sit in the app

The TUI has four pages: dashboard, routing, config, accounts.
`Tab` or the `⇄` chip moves to the next page in that fixed order, and wraps (`src/tui/mod.rs:595-620`).
So routing is one `Tab` from the dashboard and config is two.
`Esc` on routing or config goes straight back to the dashboard, not to the previous page (`src/tui/mod.rs:454-458`).
`?` or the `?` chip in the header opens a key overlay; `q` quits.
Entering a page re-reads its snapshot, and so does `r`.
Nothing else refreshes either page on a timer.
The config page re-reads after every successful edit; the routing page does not (see section 7).

Both pages talk to the running service over its loopback control endpoints (`DCTRL`), through a small worker thread (`src/tui/control.rs`).
The service is the only writer of configuration and of the policy draft.
Neither page reads the store directly.

Any modal (a routing form or confirmation, a config text entry) takes every key, `Tab`, `Esc` and `q` included, so the page's own keys and the overlay are unavailable until it closes (`src/tui/mod.rs:426-435`).

Shared frame (`src/tui/chrome.rs`):

- Header row: `?` chip at far left, `⇄` chip after it only when the terminal is at least 80 columns wide (`chrome.rs:22`, `chrome.rs:107-121`), then the tab strip, then a right-aligned status string.
- Under the header, a red message line for the last error, wrapped over as many rows as it needs, leaving the body at least one row (`chrome.rs:263-315`).
- Panels are boxed, or, when the content would not fit inside a box, ruled (`TITLE ───`), which saves a row (`chrome.rs:29-90`).
- A list shows one line per entry, windowed around the selection, with `… N more records` or `… N records above` lines (`chrome.rs:424-493`).
- The selected row is drawn in reverse video.
- The key overlay is a centred popup, bordered, closed by `?` or `Esc` (`chrome.rs:506-554`).
- `NO_COLOR` strips every colour from the finished frame and keeps bold, dim and reverse (`chrome.rs:566-582`, `src/tui/mod.rs:176-178`).
  These two pages use very little colour in the first place: the error line is red, the `?` and `⇄` chips are grey and blue, and the config page's text cursor is cyan.
  Everything else is text, reverse video (selection, focused form row), bold or dim.

## 1. The domain, in plain words

Read this section as a glossary.
For each noun it says what it is, whether the operator configures it or it is learned or fixed, and where the pages show it.

### Fixed by the program (the operator cannot add, remove or rename these)

**Client.**
A program that sends requests to Toker rather than to a provider: Claude Code, opencode, the codex CLI.
Not shown on these pages by name.

**Frontend profile.**
A named variant of the API Toker offers, selected by a `/f/<name>` URL prefix, one per client.
A request with no prefix is the profile `unprefixed` (`FPROF`).
Profile names are chosen by whoever patches the client (`toker setup` does it); policy records can be scoped to a profile.
On these pages a profile appears only as part of a *scope* (below) and as a free-text field in the preview form.

**Protocol.**
The wire shape a client speaks.
The forms offer `anthropic_messages`, `openai_chat` and `openai_responses` (`src/tui/routing/form.rs:74`); the specs call the first one `anthropic` (`ROUTE#dialects`).
Fixed.

**Provider.**
A configured upstream account: its credential and its endpoint.
The five known are `anthropic_sub` (Anthropic on a Claude subscription), `anthropic_api` (Anthropic on an API key), `openrouter`, `openai_api`, `codex_sub` (ChatGPT on a codex subscription) (`ROUTE#providers`, and `src/server/control/config.rs:29-35`).
Toker holds a row for every one of them whether or not it is enabled (`DCONF#in-the-store`).
The operator configures: enabled, endpoint, where the credential comes from.
The operator cannot add or remove a provider.
`codex_sub` holds no credential of Toker's own; it uses the login it shares with the codex CLI (`src/tui/config/model.rs:46-50`).

**Dialect.**
A provider's concrete rules within a protocol: `anthropic_messages`, `openrouter_messages`, `openai_chat_completions`, `openrouter_chat_completions`, `codex_responses`, `openrouter_responses` (`ROUTE#dialects`).
Fixed.

**Binding.**
A declaration that one provider serves one protocol in one dialect, carrying its capabilities and cost class.
Written `provider.dialect`, for example `codex_sub.codex_responses` (`ROUTE#naming-a-binding`).
This string is what the routing page calls a "binding" and what candidates, classifiers and offers refer to.
Fixed topology; it does not depend on configuration or policy (`ROUTE#bindings`).
Its **cost class** is `subscription`, `billed` or `unknown`; its **capabilities** are each supported, unsupported or unknown (sampling, tools, images, server safeguards and so on).
Shown read-only in the status tab's binding rows (`availability`) and in each candidate line.

**Backend adapter.**
Code that renders a request onto a provider's wire.
Never shown.
Not to be confused with the two other things the pages call an "adapter" (see section 7).

**Cost class and the billed boundary.**
Moving from a `subscription` binding to a `billed` or `unknown` one spends money the subscription did not.
An edge that crosses it needs explicit consent (`RDOC#edge-rules`).

### Configured by the operator, as the policy

The policy is one document with eight collections (`RDOC#collections`).
It lives in the store as numbered revisions, plus one mutable draft.
These are the collections, in the vocabulary the pages use.

**Presentation.**
A model name (a *slug*) that a client sees in its model list, and where it points.
Its source is one of: an exact offer (one model on one binding), an offer selector (a rule that picks the newest matching offer on a binding), or a virtual model backed by a route.
It has a scope, an optional label and description, an enabled flag, a "shadows a native model" consent flag, and optional adapter metadata for clients that need a full model record.
Presentations are about what is *listed*; listing does not make a name routable and a routable name need not be listed (`RPUB`, opening).
Tab 1.

**Route.**
A named, ordered chain of candidates with the rules for falling from each to the next.
The first candidate is tried first; later ones are fallbacks.
A route also has a display name, parameters (placeholders its candidates can use), a lane-transition policy, and a flag saying whether it honours server safeguards.
Tab 2.
Note the word's second sense: the glossary's "route" (`.workhorse/specs/overview.md`, Terminology) is *a binding a request has been resolved onto, together with the model it will be sent as*, which is a per-request outcome.
The page's "route" is the configured chain.

**Candidate.**
One link in a route: a binding plus a model specification (an exact model id, "the requested model" as the client named it, a template over the route's parameters, or a selector that keeps one winner from the catalogue).
It may also name an upgrade policy, carry alias rules (rewrite the model name just before sending), require capabilities, be disabled, and declare health circuits (take this target out of service for N seconds after a classified failure).

**Edge.**
The rules for falling from one candidate to the next: whether to fall before the request is sent (when the candidate is unavailable), on which classified failures to fall after sending, consent to start billed spend, consent to retry an ambiguous send (which might bill twice), and what may happen to the prompt cache (`preserve_warm`, `allow_when_cold`, `allow_cache_loss`).
Edges are not listed separately; they appear inside the route (form.rs `EDGE`, `src/tui/routing/form.rs:320-334`).

**Default.**
One route per frontend scope, used when a requested name matched neither a presentation nor a rewrite.
At most one per scope (`RDOC#collections`).
Shown in tab 2 beside routes, as `default <scope>`.
Not to be confused with the config page's "default notice style" or the `(default)` suffix on unset settings.

**Rewrite.**
A matcher on the requested model name (exact, glob, regex, or two kinds that only generated policy may use) that picks a route, with a priority unique within its scope.
For names that are never presented (for instance a client's own side-call model names).
Tab 3.

**Classifier (shown as "family" in the tab label, "classifier (family)" in the add chooser).**
Per binding, how a model identifier splits into a family and a version: a matcher plus family and version templates plus a version parser (semver, date, numeric tuple).
Needed so Toker can tell that `x-4.6` is newer than `x-4.5` within one family.
Tab 3.

**Upgrade (an upgrade policy).**
States which newer version of a candidate's family a request may be moved to.
Names a classifier, how much evidence a model needs (`none`, distinct days, explicit promotion, generated-only "adaptive"), which sources count as evidence (catalogue, served, learned), whether manual promotion stands in for evidence, and a lane-transition policy.
Candidates refer to one by id.
Tab 3.

**Compaction policy.**
At most one record: where a *cold* compaction (a request summarising a conversation whose cache has gone) is sent, as a route, plus lane and cache-transition policies.
Tab 3, appears as a single row named `compaction`.

**Description.**
Free text on the draft.
Edited by pressing `e` on the `draft` row of tab 4.

### Policy lifecycle

**Draft.**
The one mutable copy of the policy, held by the service so two editors can share it.
It carries a *generation* counter; every write names the generation it read, and a stale write changes nothing and returns the current state (`RREV#draft`).
The header shows `draft N`, where N is that generation, which is not a revision number.
Tabs 1-3 list the draft's records when there is a draft, else the active revision's (`src/tui/routing/model.rs:315-364`).
After an activation the draft is a copy of the activated document, so the draft nearly always exists.

**Revision.**
An immutable numbered policy.
Never edited.
Has a description, creation and activation time, activation scope, whether the cache transition was forced, a source (what produced it), a parent revision, and a fingerprint (`src/server/control/routing.rs:658-688`).
Exactly one is active.
The active pointer never moves backwards (`RREV#history-and-reactivation`).

**Activation.**
Compile the draft, insert it as a new revision, make it active, and replace the draft with the activated document at the next generation, in one transaction (`RREV#activation`).
Takes an *activation scope* that decides what happens to lanes already pinned to an older revision:

| scope | label in the modal | effect |
| --- | --- | --- |
| `new_lanes` | new lanes | a pinned lane keeps resolving under its pinned revision |
| `new_and_cold_lanes` | new and cold lanes | a lane whose cache is provably cold moves to the new revision; the rest stay |
| `immediate` | immediate | the next request resolves under the new revision |

An immediate activation that would lose a warm cache needs explicit consent (the modal's "forced warm-cache transition" switch), and is refused without it while any lane is warm or not provably cold.
An error-severity diagnostic blocks activation; the other severities do not (`RDOC#validation`).

**Reactivation.**
Copying an earlier revision forward into a *new* revision (the pointer never moves back).

**Diagnostic.**
A finding from compiling the draft: a code, a severity (`error`, `warning` or `info`, `src/policy/diagnostic.rs:16-20`), the id of the record it concerns, and a static path inside the document (`RDOC#validation`).
An unconfigured binding, an offer missing from the catalogue and an unresolved selector are reported but do not block, because a fallback chain is legitimately prepared before its provider exists.

**Publication.**
The projection of the policy's presentations into something a client reads, tracked per *publication adapter* and frontend scope with a state of `pending`, `applied`, `failed` or `unsupported`, a desired and an applied revision, and a desired and an applied inventory generation (`RPUB#publication-state`).
The service answers model-list requests itself; Claude Code additionally gets rows written into its `settings.json`, which only a user-side writer may do.
The TUI is such a writer: after a successful activation it starts a background reconcile and puts the outcome in the header status (`src/tui/routing/reconcile.rs`, notes `Claude settings: reconciling`, `updated`, `already current`, `not reconciled (...)`).
That happens only if the TUI could resolve the user's home (`src/cmds.rs:222-231`).

### Learned or observed (the operator does not configure these)

**Offer / inventory / catalogue.**
A model a provider serves, with its context window and chronology, as fetched from the provider's model listing (`overview.md`, Terminology).
The inventory is the union across providers.
The routing snapshot carries it as `offers`; forms complete model ids from it.
The status tab lists every offer as a row (see section 3 tab 4).
A catalogue is `fresh`, `absent` or stale; it is evidence, not validity.

**Availability.**
Per binding and per request, three-valued: positively present, positively absent, or no evidence (`RSOLV#walking-the-route`).
Positively absent covers: unconfigured or disabled provider, a credential known to be missing or expired, a capability the binding cannot represent, a candidate disabled, a fresh catalogue that lacks the model, and an open circuit.
The route detail's `now:` line renders a simplified version of it (`src/tui/routing/detail.rs:390-421`).

**Account.**
Which login served a request; learned from the credential, never configured (`ROUTE#providers`, `ACCT`).
Not on these pages.

**Learned model / promotion.**
Toker records, per binding and exact model, which days it was served and the largest prompt, and elects a family's newest model as the upgrade target once it has enough evidence (`RMODEL`).
*Promotion* marks one served model explicitly eligible.
It is evidence, not policy: it changes no revision and takes effect without activation (`UCLI#store-access`).
Toker refuses to promote a model that has never been served.

**Lane.**
One independent prompt-cache prefix: a session together with a digest of its tool set (`SLANE#the-lane`).
Many lanes per session.
Observed; the operator never creates one.
A lane has a cache temperature: warm, cold or unknown.

**Pin.**
A lane's record of the route, candidate, revision and effective model its cache was built on.
Written automatically when a response is served, never seeded from anything else, and moved only through a single decision that checks consents (`SLANE#the-pin`, `RPIN`).
The operator cannot set, move or clear a pin.
On these pages pins appear only as counts: `pinned lanes: 2 warm, 1 cold, 0 unknown` in a route's detail (`detail.rs:271-278`) and `lanes: 4 total, 1 unpinned, 2 warm, 1 cold, 0 unknown` in the activation modal (`model.rs:215-226`).

**Circuit.**
Runtime state: a candidate's target taken out of service for a while after a failure it declared a circuit for.
Shown as `N open circuit(s)` in a candidate's `now:` line and as an `OPEN CIRCUITS` block in a binding's detail.
Preview deliberately ignores circuits (`RREV#preview`).

**Scope (two unrelated meanings).**
A *frontend scope* is a protocol, or a profile plus a protocol; it is a field in presentations, rewrites and defaults and is shown as `protocol` or `profile/protocol` (`model.rs:1286-1301`).
An *activation scope* is the table above.
Both are labelled just "scope" on screen.

### Configuration (the config page)

Held in the store as typed rows, edited through the service (`DCONF#in-the-store`):

- per provider: enabled, endpoint, credential source (OS keyring, an environment variable, or a literal kept in the store), and provider-specific settings (`refresh_url`, `client_version`, `version_probe`);
- gates: the **quota gate** (stops a request that would spend past a subscription's rate-limit window) and the **cold-cache gate** (stops a request that would re-read an expired cache), each with a toggle, plus the cold gate's **outlook** switch (stay quiet when the window is not heading for exhaustion), **minimum tokens** (default 175000 per the README) and **idle minutes** (otherwise the lane's own cache lifetime) (`GATE`, `GCOLD`);
- **notice style**: how a gate's notice is rendered, per profile and a default; `gfm`, `toker`, `block` or `plain`, `gfm` by default (`GNOTE#styles`);
- the **session header names** and **ping header** name;
- the **idle-sleep lock** (`awake`);
- extra **transcript roots** the dashboard searches for session labels.

The store path and the listener port cannot live in the store and are shown read-only (`DCONF#before-the-store`).

**Server safeguards.**
A Messages client can ask the provider to review proposed actions in the same turn.
Each Messages binding declares support as supported, unsupported or unknown, and a refusal from the upstream puts a *rejection mark* on the binding that makes it read as unsupported until cleared (`WSAFE`).
Shown on the config page, one row per Messages binding, and on routes (a route may honour it).

### What contains or references what

```
provider (5, fixed) ---- enabled / endpoint / credential  (config page)
   |
   +--< binding = provider.dialect  (fixed topology; protocol, cost class, capabilities)
            ^
            |  referenced by binding id from:
            |
policy document (draft = one mutable copy; revision = immutable, numbered, one active)
   |
   +-- presentations --(source)--> exact offer | offer selector | virtual -> route
   |        scope: protocol | profile+protocol           (publication projects these to clients)
   +-- routes --< candidates (first + steps) ; each step = edge + candidate
   |        candidate --> binding + model spec, upgrade policy id, alias rules, circuits
   |        edge       --> fallback consent: billed boundary, ambiguous send, cache
   +-- defaults    (scope -> route)           one per scope
   +-- rewrites    (scope, priority, matcher -> route)
   +-- classifiers (binding -> family/version split)  <--referenced by-- upgrades
   +-- upgrades    (classifier, evidence bar)         <--referenced by-- candidates
   +-- compaction  (route + transitions)              one at most
   +-- description

request resolution (RSOLV#lookup-order): presentation by exact slug
   -> rewrite by ascending priority -> default.   (profile entry beats protocol entry)
   then walk the route's candidates, skipping positively-absent ones.

lane (session + tool set)  --pinned to-->  revision, route, candidate, binding, model
   learned from served responses, not configured; counts surface on these pages

learned evidence (days served, largest prompt, promoted?) per (binding, model)
   feeds upgrade election; independent of revisions
```

## 2. What an operator actually does here

The operator is one person running Toker on a couple of machines.
Keys are as bound today.
"Steps" counts key presses and text entries from the dashboard, not counting typing of values, and treats `Tab` to reach the page as one step.
Reaching the config page costs two `Tab` presses; routing costs one; `Esc` returns.

"Page" means the task can be finished on the two in-scope pages.
"Setup" means `toker setup`.
"CLI" means another `toker` subcommand (`UCLI#the-commands`).

| task | on these pages? |
| --- | --- |
| enable or disable a provider | yes, config tab 1 |
| set a provider's endpoint | yes, config tab 1 |
| put an API key into the store | yes, config tab 1 (literal only) |
| choose keyring or an environment variable as the credential source, or name the variable | no: setup |
| add or remove a provider | no: the five are fixed |
| turn the quota or cold gate on or off, and set the cold thresholds | yes, config tab 2 |
| set the default notice style | yes, config tab 2, as free text |
| set a notice style for one profile | no: the service has an endpoint, the page has no control |
| change session header names, transcript roots, or per-provider extras (`refresh_url`, `client_version`, `version_probe`) | no: the service accepts them, the page has no control |
| change the listener port | no: shown read-only; lives in the socket unit, set by setup or the `PORT` environment |
| change the store path | no: shown read-only; `TOKER_DB` |
| change the sleep-schedule slots | no: setup |
| check why a request went where it did | partly: preview a made-up request against the draft; real requests' provenance is not on these pages |
| change which model serves which request | yes, routing tabs 1-3 |
| add a fallback | yes, inside the route form |
| pin a lane | no: pins are written by served responses only, and nothing offers a manual pin |
| promote a model | yes (`p`), or CLI `toker promote` |
| preview a change | partly: preview resolves one invented request against the draft; there is no draft-versus-active comparison outside the activation modal |
| validate the draft | yes (`V`) |
| activate a draft | yes (`a`) |
| reactivate an older revision | yes (`x`, tab 4) |
| publish a model list to a client | yes: author presentations, then activate; the TUI reconciles Claude's settings itself; `toker picker sync` retries |
| import a leftover `toker.toml` routing into the draft | yes (`i`, tab 4), only meaningful on a machine that still has the file |
| restart the service to pick up a new binary | no: CLI `toker restart` |

### Task details

**Enable a provider.**
Who: the operator, after setup or when adding an account.
Needs in front of them: which providers are on, whether each has a credential, and whether anything routes to it.
Press: `Tab Tab`, move down to the provider's name row, `space` or `Enter`.
Rows per provider are: name, endpoint, credential (not for `codex_sub`), then one server-safeguards row per Messages binding, so providers are three to five rows apart.
If the credential is missing the row then reads `on, not configured` (`config/view.rs:213-224`).
To supply a key: select the credential row, `e` (or `Enter`), type, `Enter`; the entry shows one dot per typed character and starts empty (`config/view.rs:144-148`, `config/model.rs:439-443`).
Steps: about 5 to reach the toggle, 3 more for a key.
The page does not say whether any route in the policy reaches a freshly enabled provider.
The first revision generated at migration builds routes per binding "enabled only where the provider is configured" (`RMIG#the-first-revision`); nothing on either page connects enabling a provider to the policy having a route to it.

**Set an endpoint.**
Select the `endpoint` row, `e` or `Enter`, type the full URL, `Enter`; an empty entry clears it back to "(the built-in one)".
The editor starts empty, so changing one character means retyping the whole URL.

**Turn a gate on or off.**
`Tab Tab 2`, select `quota gate` (first row) or `cold gate` (second), `space`.
The new value is applied by the service and the page re-reads.
Steps: 4 to 5.
Numbers (`cold min tokens`, `cold idle minutes`) and text (`ping header`, `default notice style`) open a free-text entry; a non-number is refused locally with `cold min tokens: "x" is not a number` (`config/model.rs:505-529`).
The page states no unit or valid values for any of them.

**Change the listener port.**
Cannot be done here.
The `listener port` row shows it and `Enter` or `e` on it does nothing, silently.

**Check why a request went somewhere unexpected.**
Who: the operator, after seeing a surprising provider or model.
Needs: the request's requested, effective and served models, the policy revision that resolved it, which presentation, rewrite or default matched, each candidate's verdict, and whether upgrade or alias changed the model.
That provenance is recorded per ledger row (`RPROV`) and shown on the dashboard's session detail, which is outside these pages.
On these pages the operator can only invent a request: `Tab`, `4` (any tab works), `v`, then fill the preview form (protocol, profile, model, lane key, prompt bound, message count, tool count, asks for server safeguards), `Enter`.
The reply is a popup of pretty-printed JSON, closed by any key, with no scroll (see section 3).
Preview resolves against the *draft*, not the active revision, "without per-request credential evidence and without open circuits" (`RREV#preview`), so it can disagree with what the live proxy did.
Steps: about 5 plus typing the model and reading JSON.

**Change which model serves which request.**
Depends on what the operator wants:

- change what a name in a client's model list points at: tab 1, select the presentation, `e`, edit its source (about 10 fields), `ctrl+s`;
- change which provider or model a route uses: tab 2, select the route, `e`, move to the candidate's `model` field (about eight `Down` presses from the top), change `kind` with `←/→`, type or `Tab`-complete the model, `ctrl+s`;
- catch a name nothing presents: tab 3, `n`, `1`, fill a rewrite;
- change what happens for any other name: tab 2, select the `default` row, `e`, pick another route.

Then, because everything edits the draft: `V` (optional), `a`, choose a scope, `y`.
Total: roughly 12 to 20 key presses plus typing.
No on-screen cue says the change is not live until activation, other than the remove confirmation's "This changes only the draft" (`view.rs:413`).

**Add a fallback.**
Open the route form (`e` on the route in tab 2).
A one-candidate route's `+ add candidate` row is the 23rd focusable row, so 22 `Down` presses from `id`, measured from the `the_route_form` snapshot rows.
`Enter` adds a candidate and an edge into it.
The new candidate has an empty `id`, an empty `binding`, kind `exact` with an empty model, and the new edge starts at `fallback before send: disabled`, no after-send failures ticked, `may start billed spend: deny`, `may retry an ambiguous send: deny`, `cache transition: preserve_warm` (`form.rs:1352-1362`, `form.rs:1608-1643`).
So a fallback added and saved without touching the edge exists but never fires.
To make it work the operator must fill `id`, `binding` (`Tab` completes from the implemented bindings), set the model, flip `fallback before send` to `enabled`, tick the failures to fall on, and decide the two consents and the cache transition, then `ctrl+s`, then activate.
Steps: dozens of key presses (22 `Down` alone) plus typing.
Only the top-level `id` or `slug` being blank is caught before sending (`form.rs:1061-1072`); a blank candidate id goes to the service and comes back as an error.

**Pin a lane.**
Not a task the product offers.

**Promote a model.**
`p` on any tab opens a form with `binding` and `model`; `Tab` completes the binding from the implemented bindings and the model from the catalogue offers for that binding; `Enter` submits.
The reply opens the generic popup titled `ROUTING PREVIEW` with the raw JSON.
The service refuses a model never served (`unseen_binding_model`, HTTP 404, `src/server/control/routing.rs:446-451`), but the completions come from catalogue offers, not from served models, so the form can offer models that will be refused.
The CLI `toker promote` infers the binding and offers a dry run (`UCLI#store-access`).

**Preview a change.**
There is no diff view.
`a` opens the activation modal, which lists `changed records: +route x, ~presentation y, -default z` by kind and id, any cost-boundary changes, lane impact counts, the number of draft diagnostics and each publication's state (`model.rs:203-242`).
That is the only place the draft is compared with the active revision.

**Activate a draft.**
`a`, `h/l` to set the scope (starts at `new lanes`), `f` to toggle the forced warm-cache transition, `y` or `Enter` to confirm.
If a forced transition is needed the service answers 409 `forced_cache_transition_required`, which is shown as an error line inside the modal with the lane counts above it; the operator has to know to press `f` then `y` again.
If the draft has an error diagnostic the service refuses, and the error line lists up to five `code at path` pairs (`model.rs:1491-1513`).
The modal shows only a count of diagnostics, with no severity (`model.rs:229-232`).
Steps: 3 to 5.

**Publish a model list to a client.**
Author presentations on tab 1 and activate.
Whether a presentation reached a client is shown in its detail under `PUBLICATION` (`desired listed` or `left out (reason)`, `applied yes/no/unknown`, then state and revision numbers) and in tab 4's `publications` rows.
After activation the header status shows the Claude settings reconcile outcome.
If it failed, the message names `toker picker sync` as the retry.

**Reactivate a revision.**
Tab 4, select a revision row, `x`, confirm.
`x` on a row that is not a revision silently targets another revision (see section 7).

## 3. The screens as they are

### Layout shared by all four routing tabs

Source: `src/tui/routing/view.rs:47-148`.

```
 ? ⇄  1 presentations  2  3  4                                  draft 4        <- header: chips, tabs, status
 <red error line, if any, wrapped>
┌TITLE────────────────────┐┌DETAIL──────────────────────────────────┐
│ list (one line per row) ││ draft generation 4  (bold, always)      │
│ …                       ││ detail lines for the selected row       │
└─────────────────────────┘└─────────────────────────────────────────┘
```

- List column is 42% of the body width, the detail pane the rest (`view.rs:83-85`).
- Below 100 columns the two stack: list on top, taking its row count plus frame but at most a third of the body height and at least three rows, detail below (`view.rs:37-39`, `view.rs:72-82`).
  A test pins the 100/99 boundary (`view.rs:821-831`).
- Both panes are boxed only while neither loses a line to its border, else both go ruled (`view.rs:87-96`).
- The list panel's title is the tab's upper-case title: `PRESENTATIONS`, `ROUTES, CANDIDATES & DEFAULTS`, `REWRITES, FAMILIES, UPGRADES & COMPACTION`, `PREVIEW, REVISIONS & ACTIVATION` (`model.rs:30-37`).
  The detail panel's title is always `DETAIL`.
- The detail pane word-wraps and scrolls with `PageUp`/`PageDown` (10 lines) or the wheel over it (3 lines per notch); its offset resets to 0 whenever the selection or tab changes (`model.rs:10-11`, `model.rs:278-299`).
  The furthest scroll is measured from an estimate of wrapped height (`view.rs:155-164`).
- Header status (`status_text`, `view.rs:167-179`) is `draft <generation>`, then `working` while a command is in flight, then the settings-sync note, joined by ` · `.
  It sits to the right of the tabs and the tabs collapse before it clips (`chrome.rs:234-258`).
- Tab strip (`chrome.rs:157-199`): shows every label in full if the row is wide enough, otherwise collapses the other tabs to their numbers and keeps the current one in full, otherwise clips the current one.
  The four full labels with gaps are about 129 columns before the chips, status and padding, so at the usual snapshot width of 120 the strip already reads `1 presentations  2  3  4` (see the first snapshot).
  The other tabs' names are not visible without visiting them.

### Keys on the routing page

Overlay text `view.rs:182-220`; handler `model.rs:582-636`; loop dispatch `src/tui/mod.rs:459-537`.

| key | action | scope |
| --- | --- | --- |
| `↑` `↓` `j` `k` | move the selection | all tabs |
| `←` `→` `h` `l` | previous or next tab, wrapping | all tabs |
| `1` to `4` | jump to a tab | all tabs |
| `PageUp` `PageDown` | scroll the detail pane | all tabs |
| `n` | add a record; on tabs 2 and 3 first asks which kind | tabs 1-3; on tab 4 sets the error `there is nothing to add in this view` |
| `e` | edit the selected record in a form; on the `draft` row edit the description | tabs 1-3 and the `draft` row; on any other tab-4 row sets the error `this item is not a draft policy record` |
| `d` | remove the selected record (confirm) | tabs 1-3; silently nothing on tab 4 and on an empty list |
| `v` | preview form | all tabs |
| `V` | validate the draft | all tabs |
| `a` | activation modal | all tabs |
| `p` | promote form | all tabs |
| `x` | reactivation modal | tab 4 only; consumed but inert on tabs 1-3 |
| `i` | import modal | tab 4 only; consumed but inert on tabs 1-3 |
| `r` | refresh the snapshot | not while a modal is open (`src/tui/mod.rs:459-463`) |
| click | select a row; click a tab; focus a form field | |
| wheel | scrolls what is under the pointer: form focus, list selection or detail pane (`routing/mod.rs:173-183`) | |
| `Esc` | back to the dashboard | |
| `Tab` / `⇄` | next page | |
| `q` | quit | |

`Enter` on a list row does nothing.
On the config page `Enter` acts on the selected row; on the routing page it does nothing.
While a request is in flight every key except `Esc` is swallowed, and `Esc` closes the modal without cancelling the request (`model.rs:583-588`).

### Modals and forms

Any form is a large centred popup, at most 110 wide and two rows shorter than the screen, with a fields window that follows the focus, then a two-line footer: the form's key help (dim) and the last refusal prefixed `! ` (`view.rs:326-371`).
Form key help is `↑/↓ field · ←/→ change · tab complete · ctrl+s save · esc cancel` for records, `enter save · esc cancel` for the description, `tab complete from a daemon offer · enter record · esc cancel` for promote and `content-free request facts only · space toggle · enter preview · esc cancel` for preview (`form.rs:541-552`).
The overlay's `IN A FORM` section adds `alt+↑/↓` (move a list item or candidate), `ctrl+d` (remove a list item) and `space` (toggle) (`view.rs:211-220`).
The overlay cannot be opened while a form is open (`src/tui/mod.rs:426-429`).

Form rows (`form.rs:887-992`):

- a text field is `label: value`, with `▏` as the cursor while focused and `(none)` for empty optional fields;
- a choice is `label: < value >`, changed with `←/→` or `Enter` or `Space`;
- a switch is `[x] label` or `[ ] label`;
- a union's variant is `kind: < exact >`, and switching rebuilds the fields beneath it, keeping same-typed fields that share a key;
- a list shows items and `+ add <thing>`; `ctrl+d` removes the focused item;
- headings are upper-case and not focusable;
- `(fixed)` marks the `id` of an existing record, and, for defaults, the scope, which cannot be changed in an edit (`form.rs:1123-1135`); typing into it sets the note `fixed once added: add a new record to change it`;
- `(not offered)` marks a completed field whose value is not in the known candidates (bindings, catalogue models, record ids) once the field is left;
- a daemon diagnostic against a field shows as `  ! <code>` at the end of that row;
- under a focused completable field, one line `Tab: a | b | c (+N)` lists up to six matches with the active one in `[ ]`; the list matches by substring.

`Enter` in a record form does not save; it flips a switch, cycles a choice, adds to a list or moves on.
`ctrl+s` (or `ctrl+Enter`) saves.
In the short forms (description, promote, preview) `Enter` on a text field saves.

#### Fields, per form

The schemas are constants at `form.rs:74-479`.

- **Presentation** (`PRESENTATION`): `id`, `scope` (kind `protocol` or `profile`; `protocol`; for profile also `profile` text), `slug`, `label`, `description`, `enabled`, `shadows a native model`, `source` (kind `exact_offer`: `binding`, `model`; kind `offer_selector`: a `selector` with `binding`, `include` and `exclude` lists of matchers, `ordering` (catalogue chronology, classified version with a classifier, or an explicit model list), `keep` count, plus `label template`; kind `virtual`: a `route` reference with `route` and named `arguments`), `adapter metadata` (optional: `claude` with `behaves as`; `openai`; `codex` with an optional model record of about ten fields).
- **Route** (`ROUTE`): `id`, `name`, `description`, `parameters` (list of text), `candidates` (the chain; see below), `lane transition` (`preserve_pinned`, `move_when_cold`, `allow_immediate`), `honours server safeguards`.
  - Each **candidate**: `id`, `binding`, `model` (kind `exact`: `model`; `requested_model`; `template`: text; `offer_selector`: include, exclude, ordering), `upgrade policy` (optional id), `alias rules` (optional list of `matches`, `except`, `replacement`), `required capabilities` (nine switches), `enabled`, `health circuits` (list of failure plus seconds open).
  - Each **edge** into a candidate after the first, under a heading `FALLBACK FROM CANDIDATE n TO m`: `fallback before send` (`disabled`/`enabled`), `fallback after send on` (eight failure switches: authentication, model not found, rate limit, provider quota, overload, server error, transport before accept, ambiguous transport), `may start billed spend`, `may retry an ambiguous send` (both `deny`/`allow`), `cache transition` (`preserve_warm`, `allow_when_cold`, `allow_cache_loss`).
  - Candidates can be moved with `alt+↑/↓` while edges keep their positions (`form.rs:1462-1466`).
- **Default** (`DEFAULT`): `scope`, `route` (a reference: `route` id and `arguments`).
- **Rewrite** (`REWRITE`): `id`, `scope`, `priority` (integer), `matches` (kind `exact`, `glob`, `regex`, `revision_one_model`, `revision_one_family`, with `value`, `pattern`, `model` or `family`), `route` reference.
- **Classifier** (`CLASSIFIER`): `id`, `binding`, `matches` (matcher), `family template`, `version template` (both with `${name}` capture templates), `version parser` (kind `semver`, `date` with format, `numeric_tuple` with `components` and `separator`, `legacy_numeric_tuple` with `separator`).
- **Upgrade** (`UPGRADE`): `id`, `classifier` (id reference), `enabled`, `minimum learning` (kind `none`, `distinct_days` with `days`, `explicit_promotion`, `revision_one_adaptive`), `allow manual promotion`, `inventory sources` (catalogue, served, learned), `lane transition`.
- **Compaction** (`COMPACTION`): `route` reference, `lane transition`, `cache transition`.
- **Draft description**: `description`.
- **Promote**: `binding`, `model`.
- **Preview**: `protocol`, `profile`, `model`, `lane key`, `prompt bound`, `message count`, `tool count`, `asks for server safeguards`.

Two of the matcher kinds, `revision_one_model` and `revision_one_family`, and the minimum-learning kind `revision_one_adaptive`, are the generated-only semantics that `RMIG#generated-semantics` says the compiler refuses in an authored document.
The form offers them as choices anyway.

**Templates for "add".**
`n` fills the form from a template, not from blank: presentation `new-presentation` / `new-model` on `anthropic_sub.anthropic_messages`; route `new-route` with one candidate; default on `openai_chat` pointing at the first draft route (or `route-id`); rewrite `new-rewrite` with `exact` match `model-name`, scope `openai_chat`; classifier `new-classifier` with a regex; upgrade `new-upgrade` pointing at the nonexistent `new-classifier`; compaction pointing at the first draft route (`template.rs:40-109`).
A dangling reference shows as `(not offered)` in the form.
The add chooser lists kinds as `1 route`, `2 default` (tab 2) and `1 rewrite`, `2 classifier (family)`, `3 upgrade`, `4 compaction policy` (tab 3), picked by digit or `Enter` on the preselected one (`model.rs:728-744`, `view.rs:402-411`).

**Other modals** (`view.rs:288-322`, `397-478`; a pretty-printed or text popup, sized to its longest line up to 100 columns and to its line count, never scrolled; any key closes a result popup):

- *Remove* (`y`/`Enter` confirms, `n`/`Esc` cancels): `Remove this <collection key> record? This changes only the draft.`, then the record as compact JSON.
  The collection key is plural, so it reads `Remove this routes record?`.
- *Activate*: described above; the confirmation line stays last, the impact summary above it, the last error beneath it.
- *Reactivate*: `Create a forward revision from revision N?` then `The active pointer never moves backwards.`, then the same scope and force controls and summary as Activate.
- *Import*: `Replace the current draft with the routing in toker.toml?` and `The active revision does not change until the draft is activated.`
  It reads the default config path; any load or compile failure becomes an error line and leaves the draft alone (`model.rs:991-1018`).
- *Conflict* (`DRAFT CHANGED BY DAEMON`), opened by a 409 `stale_draft`: shows the daemon's generation and its draft as pretty JSON; `r` reloads and discards local edits, `a` reapplies the local typed edit batch to the daemon's copy, `Esc` closes (`model.rs:946-970`).
- *Result popup* titled `ROUTING PREVIEW`: used for a preview result, for a validation result, and for a promote result.
  Content is the key hint `any key close` followed by the reply as pretty JSON.
  If the preview asked for server safeguards, a `SERVER SAFEGUARDS asked for` summary precedes the JSON: each candidate with binding, effective support and `skipped: lacks server safeguards`, whether the route was walked again, and the serving binding's support and mark (`detail.rs:53-99`).
- *Add kind chooser* titled `ADD POLICY RECORD`.

### Tab 1: presentations

- List: one row per presentation of the draft, `<slug>  <JSON of the whole record>` (section 4).
- Detail (`detail.rs:150-201`): `<slug>  <label>`; `scope: <scope>`; `source: exact offer M on B` | `virtual, routes through R` | `offer selector`; `enabled: yes/no`; then `PUBLICATION`: for each adapter publishing it, `<adapter>: desired listed | left out (reason); applied yes/no/unknown`, then `state S; revision A applied, D desired`.
  Zero rows reads `not published by any adapter`.
  Then, if any, `DIAGNOSTICS` with `<severity> <code> at <path>`.
  If the draft does not compile the publication block is replaced with `no derived facts: the draft does not compile`.
- Keys: the shared set; `n` goes straight to the add-presentation form (one kind).
- Empty: list shows `no routing data yet`, detail `routing snapshot has no entries for this view`.

### Tab 2: routes, candidates and defaults

- List: all routes (`<id>  <JSON>`), then all defaults (`default <scope>  <JSON>`).
  There are no headings; a default appears as a row after the last route.
- Route detail (`detail.rs:257-304`): `<id>  <display name>`; `lane transition: <policy>`; a line `honours server safeguards: a request that asks needs them` only when true; `pinned lanes: W warm, C cold, U unknown`, or `not in the compiled policy yet`, or the does-not-compile note.
  Then `CANDIDATES`, each as:
  - `N. <id>  <binding>  [<cost class>]` plus `  DISABLED` when disabled;
  - `     model: exact X | the requested model | template T | selected from the catalogue`;
  - `     now: available | not configured | disabled | no credential; catalogue fresh|absent|…; N open circuit(s)`;
  - `     upgrade: <baseline> -> <winner or "no newer model">` when an upgrade policy applies.
  Between candidates, an edge line `   v fallback on|off; after send: <failures or none>; cache: <transition>`, followed by `     $ COST BOUNDARY <from> -> <to>, consent <allow|deny>` when it crosses.
- Default detail: `default for <scope>`, `route: <id>`, then the target route's full detail underneath (`detail.rs:423-442`).
- Keys: `n` opens the kind chooser (route or default); `e`, `d` on either.
- A diagnostic against a route is listed under `DIAGNOSTICS` when the route has an `id` (a default has none, so none are shown for defaults).

### Tab 3: rewrites, families, upgrades and compaction

- List: rewrites, then classifiers, then upgrades, then the compaction row if there is one.
  A flat mixture of four entity types with no separators.
- Rewrite detail (`detail.rs:444-461`): `<id>  priority N`, `scope`, `matches: <kind> <value>`, `routes to: <route>`.
- Classifier detail (`detail.rs:463-509`): `<id>  on <binding>`, the matcher, `family <template>, version <template> (<parser>)`, then `EXAMPLES FROM THE CATALOGUE`: lines `model -> family F, version V` for models that parse, `model -> matched, version not parsed`, or `no catalogue model for this binding matches`.
- Upgrade detail (`detail.rs:511-565`): `<id>  classifier <id>` (+ `  DISABLED`), `lane transition`, then `STANDING FOR A NEW LANE`: each route/candidate using it, `baseline X -> Y`, and `not <model>: <reason>` for rejected candidates (`family mismatch`, `not newer`).
  An upgrade no candidate uses says so.
- Compaction: detail is pretty-printed JSON (no layout exists for it).
- Keys: `n` opens a four-way chooser.
  Adding a compaction policy when one exists is refused by the service (one record at most).

### Tab 4: preview, revisions and activation

- List, in this order, with no headings (`model.rs:360-412`):
  1. the active revision
  2. the draft
  3. every revision in history
  4. each publication (adapter and scope)
  5. each binding's availability
  6. the lane-impact counts
  7. each draft diagnostic
  8. each catalogue offer
- So the active revision appears twice if it is in history.
  The offers can number in the hundreds with a real catalogue; nothing groups or filters them.
- Details:
  - publication (`detail.rs:621-641`): `<adapter>  <scope>`, `state S (detail code); revision A applied, D desired`, `inventory generation A applied, D desired`;
  - binding (`detail.rs:567-619`): `<binding>  <provider>`, `cost class`, `configured: yes (reason)`, `enabled: …`, `credential: …`, `catalogue: …`, `CAPABILITIES` one per line, `OPEN CIRCUITS` if any;
  - active revision, draft, history revision, lane impacts, diagnostic, offer: pretty-printed JSON (`detail.rs:22`, `json` at `detail.rs:113-119`).
    The draft's and the active revision's JSON is the entire policy document.
- Keys: `x` reactivates the selected revision, `i` imports, `e` on the `draft` row edits the description; `n` is refused.
  Preview, validate, activate and promote work from every tab but are conceptually this tab's.

### Config page

Layout (`src/tui/config/view.rs:39-114`): the header row with `?`, `⇄` and two tabs; the error line; and one full-width boxed panel titled `PROVIDERS` or `SETTINGS`.
There is no detail pane, no status text and no list/detail split.
Rows are 22-column label then value; the selected row is reversed.
An open text entry is drawn as two extra lines under the row being edited: `new value  <typed>▏` and a dim `enter save, empty clears · esc cancel` (`view.rs:140-165`).

Keys (`config/mod.rs:18-29`, `config/model.rs:302-339`, overlay `view.rs:27-37`):

| key | action |
| --- | --- |
| `↑` `↓` `j` `k` | select a row |
| `←` `→` `h` `l` | switch to the other tab (there are only two) |
| `1` `2` | go to a tab |
| `space` or `Enter` | toggle the row if it is a switch, else open a text entry |
| `e` | open a text entry; does nothing on a switch row |
| `c` | clear the rejection marks on the focused provider's bindings; only meaningful on tab 1 |
| `r` | refresh |
| `Esc` | back to the dashboard (from a text entry: cancel it) |
| `Tab` / `⇄` | next page |
| `q` | quit |
| text entry | `Enter` saves, `Esc` cancels, `Backspace` erases, other characters append; no cursor movement |

**Tab 1, providers.**
For each of the five providers, in the service's order (`anthropic_api`, `anthropic_sub`, `codex_sub`, `openai_api`, `openrouter`), these rows:

- `<name>  on | off | on, not configured` (switch)
- `    endpoint  <URL or (the built-in one)>` (text)
- `    credential  env NAME, keyring, stored, or none (NAME is unset)` (text, sets the stored literal), not for `codex_sub`
- `    server safeguards  <binding>  supported|unsupported|unknown[, marked unsupported]`, one per Messages binding of that provider (shown, not editable; `c` clears the mark).

`on, not configured` is shown when a provider is on and holds credentials of its own but none resolves (`view.rs:213-224`).
Nothing on the page says what a `server safeguards` row means.

**Tab 2, settings.**
Ten rows:

| row | kind | shown when unset |
| --- | --- | --- |
| `quota gate` | switch | `on (default)` |
| `cold gate` | switch | `on (default)` |
| `cold outlook` | switch | `on (default)` |
| `cold min tokens` | number | `(the built-in floor)` |
| `cold idle minutes` | number | `(the lane's own TTL tier)` |
| `idle-sleep lock` | switch | `on (default)` |
| `ping header` | text | `(the built-in one)` |
| `default notice style` | text | `(the built-in one)` |
| `listener port` | read-only | the number |
| `store` | read-only | the path |

Not on the page, though `DCONF#in-the-store` and `UTUI#settings` say the settings page covers them: the notice style of each frontend profile, the session header names, and the extra transcript roots.
Also not on tab 1, though `UTUI#providers` says the page can "set the settings particular to that provider" and "choose where its credential comes from": `refresh_url`, `client_version`, `version_probe` and the choice between keyring and an environment variable.
The snapshot carries all of these (`src/server/control/config.rs:62-141`); the page parses only part of it (`config/model.rs:590-652`).

Edit mechanics: nothing is applied locally.
Every edit is a whole-row `PUT` built from the last snapshot with one field changed, sent to the service; on success the page re-reads the snapshot (`config/model.rs:245-292`).
A value the service refuses comes back as `<setting>: <error>` in the red line, and the stored value is untouched.
Clearing marks on several bindings sends one request per binding in a chain (`config/model.rs:355-394`).

**Config messages and states:**

- Service down or unreadable: the page replaces its content with a panel titled `CONFIGURATION` reading `the service is not answering` and `configuration is edited through it, so there is nothing to show`, with the error line above (`view.rs:49-70`).
  The error text is `config-snapshot: the service is not answering` when the worker queue refused it, or the shared client's `routing service unavailable (…)` when the HTTP call failed, even on the config page (`control.rs:191`).
- Non-number in a number field: `cold min tokens: "x" is not a number`, `cold idle: "x" is not a number`, refused before sending.
- `c` with no mark held: `<provider>: no rejection mark is held`.
- `c` on the settings tab, and `Enter` or `e` on `listener port`, `store` or a server-safeguards row: silently nothing.

**Config narrowing:** no columns to shed; the boxed panel goes ruled when its rows (including the open entry's two lines) would not fit, then the list windows around the selection with `… N more rows` or `… N more settings` counts (`view.rs:116-126`, test `view.rs:335-355`); lines clip with `…`.
The tab strip collapses the inactive tab to its number only if it must.

### What the routing page sheds as the terminal narrows

In this order, from the code:

1. Below roughly 140 columns (129 for the labels, plus chips, padding and the status text) the inactive tab labels collapse to numbers; below that the current label clips (`chrome.rs:183-198`).
2. Below 100 columns the list and detail stack (`view.rs:37-39`).
3. Below 80 columns the `⇄` chip disappears (`chrome.rs:22`).
4. Too short for the content: panels go ruled rather than boxed (`view.rs:87-96`).
5. The list windows around the selection with `… N more/above records` counts.
6. List lines clip with `…`; the detail pane wraps and scrolls.
7. Header status text is clipped before the tabs collapse past their narrowest.
8. Forms scale to the terminal (width at most 110, height minus 2) and scroll to follow focus; there is no further shedding, so a very small terminal shows few form rows.

## 4. Where the list rows come from

This is the question the committed snapshots raise: why a list row looks like `mixed  {"id":"p-mixed","slug":"mixed","label":"…`.

**Answer: the row text is `<name>  <the record as compact JSON>`, produced by one generic function, with no per-type formatting.**

`src/tui/routing/view.rs:61-68` builds the list:

```rust
let list: Vec<Vec<Line<'static>>> = if entries.is_empty() {
    vec![vec![Line::raw("no routing data yet")]]
} else {
    entries.iter().map(|entry| vec![Line::raw(entry_summary(entry))]).collect()
};
```

`entry_summary` is at `view.rs:222-254`.
For a default it returns `format!("default {}  {}", scope_label(value), short(value))` (`view.rs:224-230`).
For everything else it finds a name and returns `format!("{name}  {}", short(value))` (`view.rs:253`).
The name is the first string-valued key among `slug`, `id`, `name`, `revision`, `adapter`, `binding_id`, `provider` (`view.rs:231-241`); failing that the first integer among `id`, `revision`, `revision_id` (`view.rs:242-251`); failing that the collection key itself, for example `draft` or `compaction` (`view.rs:252`).
`short` is `serde_json::to_string(value)` (`view.rs:280-282`): the entire record, serialised on one line.

The list never shows more than what fits: each row is one line, clipped to the column width with `…` (`chrome::list`, `chrome.rs:440`).
At 120 columns the list column is about 50 wide, so the operator sees the name and roughly the first 35 to 45 characters of JSON.

**It is neither deliberate presentation nor a debug fallback.
It is the original generic rendering that was never replaced.**
Evidence:

- The first routing page (commit `649ea9c`, `feat(tui): add read-only routing page`) had this function already: the same name search (without `binding_id`, and with integers read as unsigned only), the same `format!("{name}  {}", short(value))`, and the same `short`, in a page that only dumped the snapshot read-only.
- Later commits kept the list rows and added typed detail (`detail.rs`, doc comment: "An item the page has no layout for shows as JSON", `detail.rs:1-5`) and typed forms (`form.rs`) alongside it.
  Commit `1c7802c` (the H1 detail and forms work) added the typed detail pane and rewrote the file around it, but `entry_summary` still ends in `short(value)`.
- The comment on the detail module calls JSON the fallback for items without a layout.
  The *list* has no such typed layer and no comment about intent.
- The committed snapshots (`src/tui/routing/snapshots/toker__tui__routing__view__tests__the_presentations_page.snap` and the routes, rewrites and status pages) encode the output as the expected result; they pin the behaviour but were not written to endorse it, and no test asserts on the JSON.

What the generic rule produces, by entity:

| entity | name shown | notes |
| --- | --- | --- |
| presentation | slug | |
| route | route id | the display name is not used |
| default | `default <scope>` | the only special case; the route it points to is not named |
| rewrite | id | |
| classifier | id | |
| upgrade | id | |
| compaction | the word `compaction` | the record has no id |
| active revision | the integer revision id | `revision_id` is an integer, so it falls to the integer search |
| draft | the word `draft` | |
| history revision | the integer revision id | same as the active row, so two rows can read `3  {…` |
| publication | adapter name | two scopes of one adapter read identically |
| binding availability | binding id | |
| lane impacts | the word `lane_impacts` | |
| diagnostic | the word `diagnostic` | every diagnostic reads the same: `diagnostic  {"code":…` |
| offer | binding id | `model_id` is not in the name list, so every offer on a binding has the same name |

Because the JSON is clipped, whatever distinguishes two rows with the same name is usually off the right edge.

## 5. The state matrix

"Covered" means a committed insta snapshot shows it.
Tests that assert on text or geometry without a snapshot are noted as "asserted".
Status of the two frame tests in `src/tui/routing/view.rs` that assert layout (stacking at 99/100 columns, tab collapse at 90, ruled borders at height 12) is "asserted".

### Routing page

| state | what the screen does | covered |
| --- | --- | --- |
| no snapshot yet (starting, or service down at start) | list: `no routing data yet`; detail: `loading routing snapshot…` while loading, else `routing snapshot has no entries for this view`; no `draft N` in the header; `n` opens a template form, but `ctrl+s` and `y` in activation silently do nothing because there is no generation (`model.rs:1021`, `model.rs:1094`) | no |
| loaded, but zero records of this tab's kind | identical text to the row above: `no routing data yet` and `routing snapshot has no entries for this view`; neither names the kind or says `n` adds one | no |
| fresh install | the service refuses to start without an active revision (`RMIG#ownership`), so a policy always exists; what it contains depends on which providers were enabled when the first revision was generated | no |
| one item | list shows one row; the row is selected | partly: presentations has one row |
| a few items | rows, one per record | yes (rewrites, status) |
| many items | list windows with `… N more records` and `… N records above` | asserted |
| a long name | clipped with `…`; names are not truncated separately from the JSON | no |
| narrow terminal (under 100 columns) | list stacks above detail | asserted |
| short terminal | panels go ruled | asserted |
| header tabs at 120 columns | inactive tabs collapse to numbers | yes (every snapshot) |
| a diagnostic present on the selected record | `DIAGNOSTICS` block in detail, `info virtual-presentation-unlisted-in-responses at /presentations/0/metadata` | yes (presentations) |
| diagnostics of severity warning or error | same block; no different treatment by severity, no colour | no |
| the draft does not compile | detail shows `no derived facts: the draft does not compile` in place of availability, upgrade standing, publication rows, pinned lanes | asserted (`detail.rs:825`) |
| draft differing from the active revision | not shown anywhere persistent; only the activation modal's `changed records:` line; the header says only `draft N` | partly (the activation form's summary shows added records) |
| draft equal to the active revision | `changed records: none` in the activation modal | no |
| a record's derived facts out of date after an edit | not signalled; see section 7 | no |
| error line | red line under the header, `name: code (HTTP n): detail; code at path`; cleared by the next successful snapshot or edit | no |
| service unreachable after a snapshot was loaded | last snapshot stays navigable; red `snapshot: routing service unavailable (…)`; edits fail on submit | no |
| conflict with another editor | modal `DRAFT CHANGED BY DAEMON` | no |
| settings sync note | header status after activation | no |
| a modal that is still working | header says `working`; keys other than `Esc` are ignored | no |
| form open, valid | form popup | yes (route, presentation, default, rewrite, classifier, upgrade, compaction) |
| form scrolled to a later field | window follows focus | yes |
| form with a client-side refusal | footer note `! id is empty` or `! fixed once added…` | no |
| form with a daemon refusal | footer `! draft-edit: invalid_policy_edit (HTTP 422): id already exists`; form stays open | yes |
| form completion lines, `(not offered)`, `! <diagnostic code>` on a field | as described in section 3 | yes (completions and `(not offered)`); diagnostic marker no |
| add-kind chooser, remove confirm, import confirm, reactivate | text popups | asserted (chooser); others no |
| activation modal | scope `new lanes` selected, force `no`, impact summary | yes |
| activation refused (forced transition required, invalid policy) | error line at the foot of the modal | asserted (`view.rs:534-598`) |
| preview form | defaults `anthropic_messages`, prompt bound 0, tool count 0 | yes |
| preview result | pretty JSON popup | yes (with safeguards summary); the plain result is not snapshotted |
| validation result | pretty JSON popup titled `ROUTING PREVIEW` | no |
| promote form and promote result | form yes; result popup no | form only |
| key overlay | `KEYS` and `IN A FORM` sections | asserted |

### Config page

| state | what the screen does | covered |
| --- | --- | --- |
| providers, mixed | rows as in section 3, three providers on and off, one with a mark | yes |
| settings | ten rows | yes |
| service unreachable | `CONFIGURATION` panel and the error line | yes |
| open text entry | two lines under the row; credential shown as dots | no |
| refused value | red line under the header | asserted (`view.rs:419-429`) |
| local refusal (non-number) | red line | asserted in model tests |
| short terminal | ruled panel, windowed list with a count | asserted |
| no providers (a service returning none) | an empty panel | no |
| a provider that is on but has no credential | `on, not configured` | asserted in model tests (`model.rs:790`), not in a frame |
| key overlay | nine keys | asserted |

### What the matrix leaves uncovered

No snapshot shows: any empty state, a long name, a scrolled list, the stacked layout, a failure or conflict, the remove or import or reactivate confirmations, the validation result, the promote result, the open config entry, or the page with colour removed.
No snapshot distinguishes the selected row, because it is reverse video; every snapshot is plain text.

## 6. Constraints a redesign must respect

These come from the specs.
A redesign may change how something looks; it cannot change what has to be true.

### The pages, their contents and their keys (`UTUI`)

- `UTUI#rendering`: "Rendering MUST be a pure function of a captured snapshot, the clock and the view's own state."
  The existing split (a `render(frame, state, overlay)` per page, tests drawing into a `TestBackend`) is how that is met.
- `UTUI#what-is-shed-first`: "Every page MUST render into whatever terminal size it is given, shedding in a fixed order rather than clipping."
  "Panel framing MUST be shed before any content is."
  "A table MUST shed columns before its content is truncated."
  "A panel MUST shed its own rows down to the reading it exists to give before any panel is dropped whole."
  "A list cut short MUST say how many of its entries it left out, on each side that hides any."
- `UTUI#distinguishing-a-reading`: "A reading MUST NOT be distinguished by colour alone."
  "Every page MUST honour `NO_COLOR`, and MUST remain readable with every colour removed."
  The spec names no colour; hue and thresholds are design decisions recorded in a design guide.
  `NO_COLOR` is applied to the whole finished frame, keeping modifiers (`chrome.rs:572-582`).
  The spec cites `design/design-system.md`; that file does not exist in this checkout.
- `UTUI#marks`: "Every mark the dashboard renders MUST be explained in the legend, and the legend MUST explain no mark the dashboard does not render."
  That is about the dashboard's marks, but the routing and config pages use the same overlay mechanism.
- `UTUI#finding-the-keys`: "Every page MUST offer an overlay listing the keys it acts on, opened by the same key on every page."
  "The header of every page MUST carry a control that opens that overlay, at every terminal size."
  "A modal or a text entry that takes every key MUST say the keys it acts on itself, since the overlay cannot open over it."
  "Every page MUST be able to move to the next page by the same key."
  The page may not carry a hint footer; the overlay and tests enforce that (`view.rs:515`).
- `UTUI#the-routing-page`: "The routing page MUST show the active revision, the draft, the revision history and the publication state."
  "The page MUST be able to validate, preview, activate and reactivate, through the control operations `DCTRL` specifies."
  "An activation MUST NOT wait on reconciling a client's settings, and a slow or failed reconciliation MUST be reported on the page without undoing or delaying the activation."
  "The page MAY import a configuration file into the draft, behind a confirmation."
- `UTUI#the-configuration-pages`: both pages "MUST write through the control operations", an edit "MUST take effect without a restart", "A refused value MUST be reported on the page, leaving the stored value as it was", and "Where the service is not answering, both pages MUST report that rather than offer values to edit."
- `UTUI#providers`: show every provider, enabled or not, with endpoint and credential source; enable and disable; set endpoint; choose where the credential comes from; set provider-specific settings; show whether a credential is set and never what it is; show a provider whose credential does not resolve as not configured, naming the source that did not supply one; show each Messages binding's declared support for server safeguards and whether a rejection mark is held; be able to clear that mark.
- `UTUI#settings`: show and edit the gate settings, the notice style of each frontend profile, the session and ping header names, the idle-sleep lock, and the extra transcript roots; "MUST show where the store is and which port the listener answers on, and MUST NOT offer to edit either."
- `SEC#a-credential-is-never-logged-or-displayed` and `DCONF#credentials`: "The dashboard MUST show whether a credential is set, and MUST NOT show what it is."
  The current entry echoes one dot per character typed, which the code treats as acceptable (`config/view.rs:144-148`).

### Configuration (`DCONF`, `DCTRL`)

- `DCONF#before-the-store`: the store path and the listener port "cannot be held in" the store, resolve identically in every process, and are not editable on the page.
- `DCONF#in-the-store`: "Toker MUST hold a row for every provider it knows, whether or not that provider is enabled."
  "Enablement MUST be a field of that row."
  "Every provider's endpoint MUST be configurable, the subscription providers included."
  "A value that identifies Toker to a provider MUST NOT be configurable."
  "A notice style naming a profile outside the permitted character set MUST be refused."
- `DCONF#editing`: "An edit MUST be made through the service's control surface, and the service MUST apply it to its own running state in the same operation."
  "The control surface MUST refuse a value it cannot use, naming the setting it refused."
- `DCONF#configuration-never-refuses-a-start`: a machine with no provider enabled "MUST still start and MUST still serve, answering each request that nothing is configured."
  So a page with every provider off is a legitimate state, not an error.
- `DCTRL#the-operations`: the operations a page may use are those in the table: `config snapshot`, `config edit`, `config credential`, `safeguards mark clear`, and the routing set.
  "A credential value MUST reach Toker only through the credential operation, and no operation MUST return one."
  A configuration refusal names the setting.
  A control request needs a custom operation header and, for a mutation, a JSON body; the TUI client already does (`DCTRL#the-gate`).
- `USETUP#writing`: "Setup MUST ask only what the dashboard cannot later change", being the listener port, the schedule slots, and the providers the machine serves with where each credential comes from.
  Credential source is asked by setup, while `UTUI#providers` says the page can choose it too.

### The policy document (`RDOC`)

- `RDOC#collections`: the document holds exactly eight collections.
  "A rewrite's priority MUST be unique within its scope.
  A default MUST be unique within its scope."
  "A classifier and an upgrade policy that work together MUST name the same binding."
- `RDOC#presentations`: "A slug MUST be unique within its scope.
  A slug equal to a native model identifier reachable from that profile MUST be refused unless the document consents to shadowing it."
  A selector "MUST be expanded when the policy is compiled".
- `RDOC#validation`: a diagnostic carries "a code, a severity, the record's identifier and a path", the path built only from static field names and bounded indices; "An error MUST block activation"; "An unconfigured binding, an offer absent from the inventory and an unresolved selector MUST be reported without blocking."
  So a design must still let an operator author a fallback to a provider that is not yet configured and must not treat that diagnostic as a reason to stop them.
- `RDOC#edge-rules`: "A fall from a subscription binding to one that is billed, or whose cost is unknown, MUST require the edge's consent to crossing that boundary.
  An edge listing an ambiguous transport failure among those it falls on MUST require consent to that separately."
  "An edge crossing a cache identity MUST NOT declare that it preserves a warm cache."
  The form currently lets an operator choose `preserve_warm` on such an edge; the service's compiler is what refuses it.
- `RDOC#parsing`: unknown fields, duplicate keys and non-integer numbers are refused; identifiers are bounded and use a restricted character set.
  Free-text id entry will hit these limits.

### Revisions and activation (`RREV`, `RPIN`)

- `RREV#draft`: "There MUST be exactly one draft", writes are compare-and-swap on the generation, "A stale write MUST change nothing and MUST return the current state", and "Renaming a record MUST be expressed as a removal and an addition."
  That is why the form locks an existing record's `id` and a default's scope (`form.rs:1123-1135`) and why a conflict modal exists.
  "An edit MUST NOT be able to address an incidental path within the document" is why edits are typed operations (`model.rs:1234-1283`).
- `RREV#preview`: preview "resolves an invented request against the draft using the same resolver inference uses", "MUST use the snapshot's availability without per-request credential evidence and without open circuits", and with safeguards asked must show each candidate's effective support.
  So preview cannot answer for a real request's headers or an open circuit.
- `RREV#activation`: the three scopes in the table of section 1, "An immediate activation that would lose a warm cache MUST require explicit consent" and "MUST be refused while any lane is warm or not provably cold and that consent is absent."
  A stale draft generation fails the activation.
- `RREV#history-and-reactivation`: "A revision MUST never be edited.
  The active pointer MUST never move backwards.
  Reactivating an earlier revision MUST copy it into a new revision rather than move the pointer."
  "Reactivation MUST check consent and compilation before replacing the draft, so that a refused reactivation leaves the draft and its generation untouched."
  Semantics marked generated "MUST be refused in an authored document".
- `RREV#catalogue-refresh`: a refresh recompiles the active revision and swaps the runtime snapshot; so the `derived` facts the page shows can change with no operator action.
- `RPIN#one-decision-for-every-move` and `SLANE#the-pin`: pins are written only on a served response and moved through one decision; nothing lets an operator pin, so the design should not imply that.

### Publication (`RPUB`)

- States are exactly `pending`, `applied`, `failed`, `unsupported` (`RPUB#publication-state`), each with a stable detail code.
- "Publication is separate from routing.
  A slug that is listed MUST NOT thereby be routable, and a slug that routes MUST NOT need to be listed."
- "Where an exact presentation names an offer the inventory does not hold, the whole listing MUST be unresolved and MUST answer 503."
  Such a presentation "MUST still compile", so a presentation naming an absent offer is a diagnosed, not an invalid, draft.
- "The service MUST NOT write" a client's settings file; "A user-side writer MUST perform that reconciliation", and "Activation MUST NOT wait for it, and a failed write MUST leave the activation standing with the row recording the failure."
- A virtual presentation on a protocol whose clients need complete model records is left out of that listing unless it carries adapter metadata, and the compiler reports it (`RPUB#where-a-listing-needs-a-complete-record`).
- Ownership of rows in a client's settings is exact and never inferred from a pattern (`RPUB#ownership-is-exact`).

### Resolution vocabulary (`RSOLV`, `RPROV`)

- The lookup order is fixed: exact-slug presentation, then rewrite in ascending priority, then default, each for the profile and then the protocol (`RSOLV#lookup-order`).
  A profile entry beats a protocol entry (`ROUTE#frontend-scope`).
- Availability "MUST be three-valued, distinguishing positive presence, positive absence and no evidence either way."
  Only positive absence skips a candidate; "A credential that fails only when tried is unknown, and the candidate MUST stay."
  The absence vocabulary a page must be able to say (from the `positively absent` table): provider unconfigured or disabled; credential missing or expired; capability unrepresentable; safeguards unsupported; not the request's binding affinity; candidate disabled; catalogue positively lacks the model; plus an open circuit.
- "A stale or absent catalogue MUST be unknown."
- The three request errors (`RSOLV#when-nothing-serves`): not-configured with the `x-toker-not-configured` header, pin refusal (502), and all-candidates-unavailable (503) with the trace.
- `RPROV`: provenance is identity-only and content-free; a policy page must not show request contents.
- `RSOLV#facts-in` and `RREV#preview`: the facts a preview can state are exactly the content-free list (profile, protocol, requested model, lane key, prompt bound, message and tool counts, capabilities, safeguards, affinity, class).
  The form's "content-free request facts only" help text reflects this.

### Learning and compaction (`RMODEL`, `RCOMP`)

- "Toker MUST refuse to promote a pair that has never been served." "A promotion MUST change only that eligibility."
- A promotion changes learned evidence, not policy; the CLI notes it needs no revision and no running service (`UCLI#store-access`), while the control operation also refreshes the service's learning snapshot.
- A compaction policy names a route or a model substitution; a substitution applies only where cold, the transform would succeed, the target holds the lane's prompt, is strictly cheaper, and has a known price (`RCOMP#choosing-the-target`).
  The current compaction form offers only a route target.

### Glossary terms the spec defines (`overview.md`)

Client, frontend profile, protocol, dialect, provider, backend adapter, binding, route, canonical request, store, ledger, offer, inventory.
"Route" in that glossary is the resolved outcome, not the configured chain.

## 7. What is genuinely confusing

Observations only, each with evidence.
Where two observations are the same cause, they are grouped.

### Tab structure and labels

1. **Three of the four routing tabs are named after lists of unrelated things.**
   `routes, candidates & defaults`, `rewrites, families, upgrades & compaction` and `preview, revisions & activation` (`model.rs:41-47`) each name three or four entities, and the title bar repeats them in capitals.
   Tab 3 holds four separate record types joined by nothing but "not a route".
   Tab 3's label says "families" while its rows, its detail and its form call that record a classifier, and the add chooser says `classifier (family)`.
2. **At common terminal widths the inactive tab names are invisible.**
   The strip needs about 129 columns for every label; below that it reads `1 presentations  2  3  4` (every committed snapshot at 120 columns).
   An operator has to open each tab to find out what the others are called.
3. **Tab 4 is a bin.**
   It lists the active revision, the draft, history, publications, bindings, a lane-impact count, diagnostics and every catalogue offer in one flat list with no headings (`model.rs:360-412`), and it is also where `x` and `i` live.
   Its title `PREVIEW, REVISIONS & ACTIVATION` names three actions, but preview and activation are keys that work on every tab, not content on this one.
   Preview has no content here at all.
4. **A list that mixes entity types shows them with the same visual form.**
   Tab 2 puts defaults after routes in one list; tab 3 puts rewrites, classifiers, upgrades and compaction in one; tab 4 puts eight kinds in one.
   Only the name text (and `default ` for defaults) hints at kind.

### The list rows

5. **Every list row is a clipped JSON blob**, as described in section 4.
   The two things an operator needs to tell rows apart or judge them (where a rewrite points, what a route's candidates are, whether a presentation is enabled) are in the JSON, off the right edge.
6. **Rows of the same kind can read identically.**
   Every diagnostic is `diagnostic  {…`; every offer on a binding is named by the binding; two publications of one adapter share a name; the active revision and its copy in history are both `N  {…`.

### Draft and live

7. **The lists show the draft, not what is live, and nothing says so.**
   Tabs 1-3 list `draft_policy(snapshot).or_else(active_policy(snapshot))` (`model.rs:319`).
   The title says nothing like "draft"; the header says `draft 4`, a generation counter that reads like a revision number.
   After activation the draft is a copy of the live policy, so before and after an edit look alike.
   There is no persistent marker for "this record differs from live" or "there are unactivated changes"; the only comparison is the `changed records:` line inside the activation modal.
8. **Editing a record does not say it only edits the draft.**
   Only the remove confirmation does (`view.rs:413`).
   The edit form's title is `EDIT ROUTE` and its help says `ctrl+s save`.
9. **The page's own data goes stale after an edit.**
   After a successful edit the page replaces the draft and generation but keeps the old `derived` facts and `draft_diagnostics` (`receive_draft_reply`, `model.rs:532-542`; `set_daemon_draft`, `model.rs:559-573`).
   The detail pane's availability, upgrade standing, pinned lanes and publication rows, the `DIAGNOSTICS` block, and the cost-boundary lines in the activation modal (`model.rs:1307-1367`) all read `derived`.
   Until the operator presses `r` they describe the previous draft, with no marker.
   Activation likewise merges only `active_revision`, `draft`, `lane_impacts` and `publications` (`model.rs:544-557`).
10. **The revision history is loaded once and never refreshed.**
    `revisions()` is sent only in `Page::new` (`routing/mod.rs:72`), which runs at TUI start; `refresh()` (page entry, `r`) fetches the snapshot only (`routing/mod.rs:76-80`).
    A revision activated during the session does not appear in tab 4's list; if the service was down at TUI start the history stays empty.
    The `x` key's own error, `refresh revisions, then choose a revision to reactivate` (`model.rs:809`), tells the operator to do something the page has no way to do.
11. **`x` can quietly target the oldest revision.**
    `open_reactivate` takes the selected row's `revision_id`, and falls back to `self.revisions.first()` (`model.rs:790-801`).
    The service lists revisions `ORDER BY revision_id`, oldest first (`src/store/state.rs:1141`), so with the draft, a publication, a binding or a diagnostic selected, `x` opens `Create a forward revision from revision 1?`, which is the generated first revision.
    The modal names the number but no description or time; the row text discards both.

### Terms

12. **"Route" means two things.**
    In the glossary it is a resolved binding plus model for one request; on the page it is a configured chain of candidates (section 1).
13. **"Adapter" means three things.**
    A backend adapter renders requests (glossary); a *publication adapter* writes a model list for a client (`adapter` in the status tab's publication rows and `dynamic_responses_models` in snapshots); and a presentation's `adapter metadata` form has `adapter: claude | openai | codex` as its tag (`form.rs:209-223`).
    None is explained on screen.
14. **"Scope" means two things**: the frontend scope on every record, and the activation scope in the modal, both written `scope:`.
15. **"Default" means several things**: a routing default (tab 2), `(default)` suffixed to an unset gate in config, and `default notice style` in config.
16. **"Binding" is a string, not a concept the operator configures.**
    `anthropic_sub.anthropic_messages` appears in routes, classifiers, offers, upgrades and safeguards rows, completed from a fixed list.
    The operator has to know it is provider-dot-dialect.
17. **"Lane transition" and "cache transition" use two different three-word vocabularies for similar ideas.**
    `preserve_pinned`, `move_when_cold`, `allow_immediate` against `preserve_warm`, `allow_when_cold`, `allow_cache_loss` (`form.rs:75-78`), shown as raw identifiers with underscores in form choices.
    Nothing says when a lane's transition applies versus an edge's.
18. **"Promote" is used for an unrelated action.**
    The overlay says `p  promote` (`view.rs:196`); the form is titled `PROMOTE LEARNED MODEL` (`form.rs:512`).
    It marks a *model* eligible for upgrades; it does not promote a draft, a revision or a route, and it takes effect immediately without the draft and activation (`UCLI#store-access`).
    It is bound on every tab, including those with nothing to do with models.
19. **"Upgrade" names a policy but the row reads as an event.**
    A `newest` row with `classifier gpt-family` is a rule about when a request may move to a newer model.
20. **"Preview" is also the title of results that are not previews.**
    Validate (`V`) and promote (`p`) results open the same popup, titled `ROUTING PREVIEW` (`model.rs:103`, `model.rs:492-503`, `model.rs:520`).
21. **Draft generation versus revision.**
    The header's `draft 4`, each detail pane's bold first line `draft generation 4`, and the active revision `3` are three numbers that look alike and mean different things.

### Keys

22. **Keys that work everywhere but belong to one place.**
    `v`, `V`, `a` and `p` fire from tabs 1-3 where the thing they act on (the whole draft, the whole policy, learned models) is not what is selected.
    `x` and `i` are consumed on tabs 1-3 and do nothing.
23. **`v` and `V` are neighbours**: preview a request versus validate the draft, case-only difference (`model.rs:605-609`).
24. **The same key does different things on the two pages and in modals.**
    `Enter` does nothing on a routing list row, toggles or edits on a config row, saves a short routing form, and advances or flips a switch in a record form.
    `e` on routing opens a form, on config opens a text entry and is ignored on switches.
    `1-4` jump tabs on routing, `1 2` on config.
    `Tab` changes page everywhere except inside a form, where it completes.
    `Esc` leaves the page for the dashboard, not for the previous page, and inside a form cancels it.
    `←/→` switch tabs on both pages but change values inside a form, and in a config text entry do nothing.
25. **Silent no-ops.**
    `d` on a non-record row, `Enter` or `e` on the config `listener port`, `store` and server-safeguards rows, `c` on the settings tab, `ctrl+s` or activation `y` when no snapshot has loaded, and `x` or `i` on tabs 1-3 give no message.
    Other refusals give an error, so the operator cannot tell which kind of silence they are looking at.
26. **Saving is `ctrl+s` in a record form but `Enter` in the short forms.**
    In a record form `Enter` does not save, it flips a switch or moves on, while in the promote, preview and description forms `Enter` is the save, so the two kinds teach opposite habits.

### Adding and editing

27. **A record form is one long flat page of every field.**
    A one-candidate route is about 40 rows including nine capability switches and seven edge-failure switches; adding a second candidate takes 22 `Down` presses to reach `+ add candidate` (section 2).
    Only top-level `id` and `slug` blank-ness is checked before sending (`form.rs:1061-1072`).
28. **"Add a fallback" creates one that does nothing.**
    A new edge starts at `fallback before send: disabled` with no failures ticked and all consents `deny` (`form.rs:1608-1643`); the operator must know to turn it on.
29. **Templates hide a decision.**
    A new default or rewrite template names the draft's first route, and a new upgrade names `new-classifier`, which may not exist.
    The form marks the second as `(not offered)` but no text explains the marker.
30. **Choices include values an authored document cannot use.**
    `revision_one_model`, `revision_one_family` and `revision_one_adaptive` are offered in form choices (`form.rs:113-116`, `form.rs:436`), though `RMIG#generated-semantics` says the compiler refuses them.
31. **Promotion completes models that may not be promotable.**
    The completions are catalogue offers (`form_context`, `model.rs:693-696`), but the service refuses an unseen model (`routing.rs:446-451`).
32. **Preview results are raw JSON in a popup that cannot scroll.**
    Any key closes it, and its height is capped at the screen (`view.rs:306`).
    A preview trace longer than the screen is unreadable beyond the cutoff (see the `a_preview_that_asked` snapshot, which ends at the opening of the first trace entry).
    Diagnostic codes (`virtual-presentation-unlisted-in-responses`) have no explanation anywhere.
33. **Activation consent speaks in code.**
    The 409 shows as `activate: forced_cache_transition_required (HTTP 409)`.
    The three scope names and `forced warm-cache transition` are not explained in the modal.
    The modal shows `draft diagnostics: N` but not severity, and the operator cannot see from it whether an error will block.

### Config page

34. **The settings rows have no descriptions.**
    `cold outlook`, `cold min tokens`, `cold idle minutes`, `ping header` and `default notice style` carry no unit, no meaning and no list of valid values (for example `gfm`, `toker`, `block`, `plain` for the notice style); an empty entry means "back to the built-in" but the built-in is not stated.
35. **Text entries start empty and show no current value.**
    Changing an endpoint means retyping it (`config/model.rs:439-443`).
36. **The provider list has no explanation for what a row group means.**
    A `server safeguards` row is selectable but not editable; the way to clear a mark is `c`, which exists only in the overlay and acts on the provider, not the row.
    `codex_sub` has no `credential` row and no text saying why.
37. **The config page cannot do things the specs say it must, and the wire already carries them.**
    The snapshot has `session_headers`, `transcript_roots`, `notices`, `refresh_url`, `client_version` and `version_probe`; the page reads and sends none of the first two or the last three, and never calls the per-profile notice endpoint (`src/server/mod.rs:1991`).
    Credential source can only be set to a stored literal; keyring and the environment variable are setup-only, though `UTUI#providers` says the page can choose.
38. **Editing a config row overwrites fields the page never shows.**
    The settings edit is a whole-row `PUT` (`src/server/control/config.rs:199-219`, `src/store/config.rs:175-201`), and the page's body (`config/model.rs:562-573`) leaves out `session_headers` and `transcript_roots`, so toggling any gate writes those as unset.
    The provider edit likewise leaves out `refresh_url` (`config/model.rs:552-560`, `config.rs:183-197`, `config.rs:326`).
    This is a data-loss defect rather than a layout problem, noted here because it constrains how the page is rebuilt: an edit built from a partial copy of the row loses the rest.
39. **A provider that is on is not known to be reachable by anything.**
    Enabling a provider changes configuration; whether the policy has any route to it depends on the policy.
    Neither page ties the two together.
40. **Nothing on either page explains the split between setup and the pages.**
    Port, schedule slots and credential source (keyring or environment) are setup-only.
    The settings page shows port read-only without saying that setup owns it.

### Things the operator must know that no screen tells them

- Edits go to a draft and do nothing live until activation.
- A freshly added fallback must have `fallback before send` turned on to ever fire.
- Pins cannot be set by hand, and counts of warm and cold lanes are the only view of them.
- Preview resolves the draft against evidence-free availability, so it can differ from what the proxy did a minute ago.
- Promote needs a model that has already been served.
- `toker restart`, `toker picker sync` and `toker promote` exist.
- A published slug need not be routable and vice versa.

### Smaller points

- The routing page's counts say `records` and the config page's say `rows` and `settings`; the key overlay says `a record` and `a provider`.
- Remove reads `Remove this routes record?` (plural collection key at `view.rs:413`).
- The import modal names `toker.toml`, a file the specs say Toker does not write and that exists only on a machine not yet moved (`DCONF#moving-a-machine-off-the-file`); on a moved machine the key leads to a path error.
- The config page's own transport error text reads `routing service unavailable (…)` (the shared client, `control.rs:191`).
- Neither page auto-refreshes; a catalogue refresh that changes availability is invisible until `r`.
- Mouse support exists (click row, click tab, click form field, wheel) and is the only way to move to a field in a form without stepping through every row.
