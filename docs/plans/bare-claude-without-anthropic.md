# Bare `claude-*` side calls on a toker with no Anthropic backend

Date: 2026-10-09
Status: deferred. Another refactor was under way in a separate session when
this was written and may change how a backend is chosen; reconcile with it
before starting.

## The problem

Claude Code's side calls name a bare Claude model whatever the session's main
model is: the auto-mode classifier (`claude-sonnet-5`, picked by a server-side
flag keyed on the main model), titles and the other haiku helpers, and
compaction when toker does not retarget it. A bare model goes to
`default_backend_anthropic`. That is fine where the default is `anthropic_sub`
or `anthropic_api`, but:

- With openrouter as the only backend, the anthropic protocol has no default
  (config refuses `openrouter` there; it is prefix-only), so every bare request
  is answered locally with not-configured. The session's own turns work
  through `openrouter/…`, and the classifier, titles and compaction all fail.
- With `codex_sub` as the default, a bare `claude-*` goes through translation
  and works only if `[providers.codex_sub.model_map]` covers its family; the
  classifier then judges permissions with a GPT model. What an unmapped id does
  there was not checked.

A model map cannot help on openrouter: maps belong to `anthropic_sub`,
`anthropic_api` and `codex_sub`, and are applied after a backend is chosen.

Not verified: whether Claude Code runs the auto-mode classifier at all without
an Anthropic login. The classifier model comes from a server-side flag, so
that depends on the account.

## Direction

Route by model family, not by recognising classifier calls. Every one of these
side calls names a Claude family, so one mechanism covers all of them, whereas
recognising the classifier by its shape (no tools, two messages, a
`</block>` or `</severity>` stop sequence) is fragile and fixes only the
classifier.

Preferred over a new top-level fallback setting:

- `[providers.openrouter.model_map]`, with the same selectors as the other
  backends, so `family:sonnet` can name `anthropic/claude-sonnet-5` or any
  other openrouter model.
- `openrouter` accepted as `default_backend_anthropic`, so a bare id reaches
  that map. `picks_from_anthropic_catalogue` already keeps force-newest and the
  compaction retarget off openrouter; with a map in place they could write a
  `claude-*` id the map then turns into an openrouter one, as codex does.

Open:

- Whether a map should be required when openrouter is the anthropic default,
  since openrouter's own ids carry a vendor prefix (`anthropic/…`); whether it
  accepts a bare `claude-sonnet-5` was not checked.
- Sending the classifier to a different model from other sonnet-family side
  calls would need shape detection after all. Leave it until someone wants it.
- Setup's wizard and verify would need to offer and check the new default.
