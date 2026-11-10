---
status: draft
---

# Config howto guide

Recipes for configuring Toker from scratch, gathered while working through real cases, plus the rough edges found along the way.

## Recipe: send Claude Code to codex, mapping each Claude tier to a GPT model

### Symptom

Claude Code, pointed at Toker with only `codex_sub` able to take Anthropic-protocol traffic, fails every request with:

```
The 'claude-opus-5-5' model is not supported when using Codex with a ChatGPT account.
```

### Why

Setup makes the only enabled Anthropic-protocol provider the Anthropic default.
The default route passes the requested model through unchanged, so codex receives the `claude-*` name.
The rewrites tab still shows that default as served, because a passed-through model is never checked against a catalogue.

### Fix

Add one rewrite per Claude tier, each sending its requests to the codex route with a fixed model.

1. Open `toker tui` and go to the routing page.
2. On the models tab, read the codex binding's catalogue offers and pick the model id for each tier.
   The id is sent as typed and is not checked against the catalogue, so copy it exactly.
3. On the rewrites tab, press `n` to add a rewrite, and fill in:
   - **id**: something like `claude-opus-to-codex`
   - **scope**: protocol `anthropic_messages` (or profile `claude` with protocol `anthropic_messages`, to affect only Claude Code)
   - **priority**: unique within the scope; the generated rules use `-10000` and `0` to `4`, so start at `10`
   - **matches**: glob `claude-opus-*`
   - **route**: `route-codex_sub-codex_responses`
   - **arguments**: add `requested_model` as a literal, set to the chosen codex model id
4. Save with `ctrl-s`. The edit activates for new and cold lanes.
5. Repeat for `claude-sonnet-*` and `claude-haiku-*`.
   Claude Code sends background requests (titles, summaries) on the small model, so leaving haiku unmapped still fails some requests.
6. Press `t` to trace a request for each tier and check that the outcome names the codex binding and the expected model.

### Notes for the guide

- The route's `requested_model` parameter is how a rewrite overrides the model: the generated per-provider routes all take it, so no new route is needed.
- A glob matches the dated snapshot spellings too.

## Recipe: always send a tier to the newest matching model

Instead of a literal model in each rewrite, point the rewrite at a route whose candidate picks the newest matching offer from the catalogue.

1. On the models tab, add a classifier on `codex_sub.codex_responses` that splits the model ids into family and version.
   The codex listing carries no release dates, so catalogue chronology cannot order its offers and a classifier is the only way to rank them.
2. On the routes tab, add a route with one candidate on `codex_sub.codex_responses` whose model is an offer selector: include a glob for the tier's models, ordered by classified version with that classifier.
3. Point the tier's rewrite at that route, with no arguments.

The winner is looked up again whenever the catalogue refreshes, so a new release is picked up within a day without editing the policy.

### Selector or upgrade

- A **selector** is "always the newest that matches", with no evidence bar.
  A refreshed winner on a warm lane is a change under the same revision, so the route's own cache transition decides whether that lane moves.
- An **upgrade** keeps the literal model and moves to a newer version in the same family, only where no cache can be lost, and keeps the move for the rest of the conversation.
  It reads served-day evidence, and a model a rewrite never sends is never served, so the scaled and day-count bars never clear and promotion refuses a never-served model.
  On a rewrite target it only works with the `none` bar.
  It is attached to a candidate, so on the generated codex route it would also apply to `codex_sub/…` prefixed requests and the default.

For mapping Claude tiers onto another provider, the selector is the direct fit.

## Rough edges found

- No way to configure a second key or login for the same provider.
- A codex-only Anthropic default passes `claude-*` names to codex, and nothing on the routing page flags it.
- No way to duplicate a record, so each tier's rewrite is entered from the blank template.
- Codex has no generated classifier, and its listing has no release dates, so "newest codex model" needs a hand-written classifier regex first.
