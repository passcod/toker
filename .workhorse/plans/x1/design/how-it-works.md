# How settings and routing work, in the redesign

This is a walkthrough of the converged design for Toker's settings and routing screens, written the way the README would explain them to someone using Toker for the first time.
Each section links to the mockup frame that shows it.
The mockups are HTML drawings of the TUI at real terminal sizes, mostly 120×40; open them in a browser.

The area is two pages, **routing** and **config**, and `tab` (or the `⇄` mark in the top-left corner) switches between them.
Routing is everything that decides where a request goes.
Config is the providers you have and the handful of settings that sit outside routing.

## How a request finds a provider

Every request a client sends to Toker arrives with three facts: the client's profile (`claude`, `opencode`, `codex`, or `unprefixed` when the client used no prefix), the protocol it speaks (`anthropic_messages`, `openai_chat`, `openai_responses`), and the model name it asked for.
Toker turns those into a provider and a model by walking a fixed chain, and stopping at the first step that has an answer.

1. **Presentations.**
   These are the models Toker publishes to a client's model picker, such as the Claude Code menu.
   If the requested name is exactly one of the published slugs, that presentation decides the route.
2. **Rewrites.**
   An ordered list of rules for each protocol, read top to bottom like a firewall table.
   Each rule matches the model name (by prefix, glob or regex), says which route takes it, and says what model name to send on: usually `${model}`, the part the match captured, so `anthropic_sub/claude-sonnet-5` is sent on as `claude-sonnet-5`.
3. **Defaults.**
   If no rule caught the name, the protocol's default route takes it.
   A protocol with no default leaves the name with nowhere to go, which the screens call `nothing catches it`.
4. **The route.**
   A route is a named, ordered list of candidates, each one a provider endpoint (a binding, such as `openrouter.openrouter_messages`).
   Toker tries the first candidate, and moves to the next only under the conditions written on the edge between them: unavailable before sending, overloaded, a 5xx, and whether it may fall to a billed provider or retry an ambiguous failure.
5. **Shaping the model.**
   On the candidate that serves, the model name can still change: an upgrade to the newest model in its family, a retarget for compaction, or an alias the provider needs (OpenRouter wants `anthropic/claude-sonnet-5`, not `claude-sonnet-5`).
6. **The gates**, last, which may still hold the request back to protect quota or a cold cache.

That chain is the spine of the whole routing page.
The tab strip along the top is the chain itself, with arrows between the steps a request walks through in order:

```
1 presentations 15 → 2 rewrites 14 → 3 defaults 4 → 4 routes 7 · 5 models 11 · 6 history 9
```

The number after each name is how many records it holds.
`models` and `history` sit after a `·` rather than an arrow because a request doesn't pass through them in sequence: models holds the families, upgrades and compaction that shape the name, and history holds the revisions.

The quickest way to see the chain in action is a trace.
[A trace of `anthropic_sub/claude-sonnet-5` from Claude Code](../../../design/mockups/x1/converged-01-rewrites.html#trace) shows the answer first (`goes to openrouter.openrouter_messages as anthropic/claude-sonnet-5 · billed`), then each link of the chain on one line: presentations fell past, rewrite rule 2 caught it, defaults weren't reached, the route is `claude-sub`, its first candidate fell past because `anthropic_sub` has no credential, the second served it on OpenRouter, no upgrade applied, and the gate let it through.
The table above the trace marks the same story in its gutter: `↓` on the rule it fell past, `>` on the rule that caught it.

## Reading the routing page

Every tab is a table, with the selected row's detail in a panel below it.
The panels are only as tall as their content, so nothing is boxed for the sake of filling the screen.
There are no explanatory sentences on the page; what the marks mean is in the `?` overlay.

[The rewrites tab at rest](../../../design/mockups/x1/converged-01-rewrites.html#rewrites-at-rest) is the one to learn first.
Rules are grouped by protocol.
Each group opens with a head saying what is common to all its rules (`anthropic_messages  5 rules · prefix → ${model}`), so the table doesn't repeat a column that's the same on every row.
Then come the rules in order, each reading as `match → route`, and the group closes with its `else` row: the default that catches whatever no rule did.

Two columns tell you whether things actually work:

- **reach** says what a request matched by this row will get.
  `ok` means the route's first choice serves it.
  `no cred · billed fallback` means the first choice is a provider that's on but has no credential, so a billed provider ends up serving it.
  `off` means the route leads only to providers that are turned off.
- **hits** counts recent requests the row caught.
  `-` means not recorded, which isn't the same as zero, and `≥3` is a floor where some rows predate the counter.

Colour only ever draws the eye to a problem: `no cred`, `billed` and `nothing catches it` are yellow, a refusal is red, `off` is dim, and everything healthy is plain.
Nothing is green.

The detail panel for a rule spells it out in words: the match, an example of a name going in and what comes out, where it sends, why its reach is what it is, whether it overlaps another rule, and recent hits.
Its last line holds the controls for that rule.
[The same frame in a larger terminal](../../../design/mockups/x1/converged-01-rewrites.html#rewrites-135x50) just gives the columns more room.

### Presentations

[Presentations](../../../design/mockups/x1/converged-03-tables.html#presentations) are the published models.
Each row includes a pattern of provider models, excludes some, says which slot of the client's picker it fills (`opus`, `sonnet`, `haiku`), and shows how many models it lists and the newest of them.
The group's `else` row (`no slug equals it → 2 rewrites`) is where the chain continues when a request names something that isn't published.
The detail panel calls out overlaps with other rows, and whether the presentation has reached the client's settings yet.

### Defaults

[Defaults](../../../design/mockups/x1/converged-03-tables.html#defaults) is the short list of last-resort routes, one per protocol and one per protocol for unprefixed clients.
The `openai_responses  no default → nothing catches it` row is yellow because a request there that no rule matches gets an error.

### Routes and the route page

[The routes tab](../../../design/mockups/x1/converged-02-routes.html#routes) lists every route with the chain of providers it sends to (`anthropic_sub → openrouter`), what points at it (`4 rules · 2 defaults`), how many lanes are pinned to it, and how it fares right now.
The detail below is the candidate list, with the fallback conditions written on the line between each pair of candidates.

`enter` opens [the route page](../../../design/mockups/x1/converged-02-routes.html#route-page), a full-height view of one route.
It reads top to bottom as three questions:

- **sent to**: the candidates, in order, with the edges between them.
- **handled by**: which per-route behaviours are on (upgrade to newest, how lanes move to a new revision, where compactions go).
- **matched by**: every rule and default that sends traffic here, so you can see a route's inbound side without hunting through the rewrites.

`←/→` steps through the other routes (`1 of 7`), and `esc` returns to the table with the selection where you left it.

### Models

[The models tab](../../../design/mockups/x1/converged-03-tables.html#models) stacks four small panels.
**Families** say how a model name is read (`claude-sonnet-4-5` reads as `sonnet · 4.5`), which is what makes "newest" meaningful.
**Upgrades** say which routes move a request to the newest model in its family, with what each would currently upgrade from and to; `p` promotes a model so it's trusted at once rather than after evidence builds up.
**Compaction** is one line on where cold compaction requests go.
**Offers** is the provider catalogue, scrolled as a window over hundreds of models.
`↑/↓` past the edge of a panel moves the focus into the next one, as [the same tab with focus in families](../../../design/mockups/x1/converged-03-tables.html#models-families) shows; only the focused panel shows a selection.

## Changing things

Every change is made in place, in the row it changes, and checked as you type.
There's no separate validate step and no apply step: a form that would be invalid can't be saved, and a form that can be saved goes live.

[Editing a rule](../../../design/mockups/x1/converged-01-rewrites.html#editing) shows a new rule 6 being added to `anthropic_messages`.
The form opens under the row with its three fields, `match`, `route` and `sends`, and each field has its reading beside it.
A reading in plain text tells you what the value does (`catches claude-sonnet-5 and 2 more`).
A reading in red is a refusal and says how to fix it: here, a glob captures nothing, so `${model}` would be empty.
While anything is refused, the save control says what's in the way (`[ctrl+s] fix sends to save`) instead of `save`.

Below the table, the **preview** replays recent requests against the edited rule (`3 of 5 recent requests would change route`), showing each one's route now and with the change, so you can see the effect before you commit to it.

[The save-flow animation](../../../design/mockups/x1/converged-08-save-flow.html#animated) cycles through the four moments of an edit: the form opening, the refusal, the fix, and the save.
[The same four as a static strip](../../../design/mockups/x1/converged-08-save-flow.html#states) is below it.

The same pattern runs everywhere: [adding a fallback on the route page](../../../design/mockups/x1/converged-02-routes.html#add-fallback) refuses a model the provider doesn't offer, and notes, dimly, that the chosen provider is off and will be skipped until it's on.
`ctrl+s` saves every form, `esc` cancels it, and `m` lifts a row to reorder it with `↑/↓`.

## How a change goes live

Each save makes a new revision of the routing policy and puts it live straight away, for new conversations and for any conversation whose cache has gone cold.

Conversations with a warm cache are different.
Each conversation runs in a lane, and a lane stays pinned to the revision it started on, because moving it to a different route or model would throw away a prompt cache that's still paying for itself.
So right after a save, [the header](../../../design/mockups/x1/converged-01-rewrites.html#after-save) reads `saved · rev 10 live · 2 lanes on rev 9`: the new revision serves everything it safely can, and two warm lanes carry on as they were.
They move across on their own once they go cold.
That frame also shows the one other thing the header can say, `claude settings not synced`, when the models published to Claude Code's settings haven't caught up with the policy.

[The history tab](../../../design/mockups/x1/converged-04-history.html#history) lists every revision, newest first, each with a one-line summary of what changed and how many lanes it holds.
The selected revision's detail is its diff in words (`+ rewrite anthropic_messages 6  claude-* → openrouter-messages  as asked`) and where its lanes stand.
From here `a` makes an older revision live again, `c` compares how a request would route under it, and `m` moves the running lanes onto the live revision now instead of waiting.

Moving warm lanes costs their cache, so `m` doesn't fire straight away.
[It arms first](../../../design/mockups/x1/converged-04-history.html#armed): the control turns reversed, and the panel's last line names what will be lost (`2 warm lanes lose their cache: ses-hot, ses-crow`) and asks for `m` again to go ahead.
That's the only confirmation anywhere in routing, because it's the only change that throws something away.

## The config page

[The providers tab](../../../design/mockups/x1/converged-05-config.html#providers) lists the five providers with whether each is on, where its credential comes from, how many endpoints it has, and what in routing uses it.
The page opens on whatever needs attention, so here `anthropic_sub` is selected: it's on, but has no credential, which is why so much of the rewrites tab reads `no cred · billed fallback`.
The detail says the same in label-and-value form and offers the fix on its last line (`[e] enter key`), alongside turning it off or jumping to the route it feeds.

[The settings tab](../../../design/mockups/x1/converged-05-config.html#settings) holds the gates, the session headers, the notice styles, and two values fixed elsewhere (the listener port and the store path, each saying what set it).
Editing works as it does in routing: here `175k` is refused because the field wants a whole number of tokens, and the save control says so until it's fixed.

## Smaller terminals, a first run, and help

At 100 or 80 columns the screens shed what is least needed first.
Columns that are uniform go into the group head, then the non-current tabs collapse to their numbers and keep their arrows, so the chain is still visible.
See [rewrites at 100×24](../../../design/mockups/x1/converged-06-narrow.html#rewrites-100x24), [rewrites at 80×24](../../../design/mockups/x1/converged-06-narrow.html#rewrites-80x24), [the route page at 100×24](../../../design/mockups/x1/converged-06-narrow.html#route-page-100x24) and [providers at 80×24](../../../design/mockups/x1/converged-06-narrow.html#providers-80x24).

[A fresh install](../../../design/mockups/x1/converged-07-empty-keys.html#empty) shows the rewrites tab with no policy yet: each protocol's group is still drawn, its `else` row says `nothing catches it`, and the controls point to the first step, since a rule needs a route to send to (`[n] needs a route first   [4] go to routes`).

[The `?` overlay](../../../design/mockups/x1/converged-07-empty-keys.html#keys) is where every key and mark is explained, grouped by where it applies (on a route, in history, in an edit, in a trace).
That's the one place on the page with explanations, which is what lets everything else stay terse.

## What changes underneath

The redesign is mostly presentation, but a few changes to how Toker works make it simpler to use, and the mockups assume them.

- **No draft and no apply.**
  Saving a valid form makes a revision and activates it for new and cold lanes.
  The separate validate key and the apply step go away, since an invalid form can't be saved in the first place.
- **Consent only where something is lost.**
  The one confirmation is moving warm lanes, which costs their cache.
- **Authored names everywhere.**
  The generated `legacy-…` records and the semantics behind them are retired.
  Routes, rules, families and upgrades carry short names someone would choose (`claude-sub`, `openrouter-chat`, `claude-sonnet`, `newest on claude-sub`), and nothing is marked as generated.
- **History lives with routing.**
  Revisions are a routing concept, so the history tab moved off the config page.
- **No JSON.**
  Nothing is shown or edited as JSON, and patterns use the `anthropic_sub/${model}` notation rather than a regex unless the rule is a regex.

## The alternatives

The converged design is direction D (rule tables), with the route page from direction E and the trace from direction C.
The five directions it was chosen from are still in the card's mockups for comparison, as the `a-`, `b-`, `c-`, `d-` and `e-` files beside the converged ones, and each is described in its own `direction-*.md` alongside this guide.
