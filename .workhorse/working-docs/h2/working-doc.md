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

## Rough edges found

- No way to configure a second key or login for the same provider.
- A codex-only Anthropic default passes `claude-*` names to codex, and nothing on the routing page flags it.
