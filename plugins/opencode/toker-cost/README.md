# toker-cost — opencode plugin

The opencode sidebar/footer plugin: real billed spend and token/context
breakdown for the active session, fetched from toker's
`/_toker/session` endpoint (session attribution is exact — opencode
sends `x-session-id`, which toker records per row; the token-vector
join era is over).

What it renders:

- **Footer**: context tokens (opencode's own data) + real billed spend
  for the session.
- **Sidebar**: a Context block (tokens, context-window percent,
  per-category breakdown: input / output / reasoning / cache read /
  cache write) and a Spend block (total billed, per-provider rows
  `provider · requests · $cost`). Nothing renders when toker answers
  nothing — absence, not zeros.

## Install

`toker setup` installs this plugin to
`~/.config/opencode/plugins/toker-cost/` when opencode is detected
(decline the offer to skip). Manual install:

```sh
cp -r plugins/opencode/toker-cost ~/.config/opencode/plugins/
```

The plugin is discovered from the directory beside opencode's
`opencode.json`. It requires opencode's provider baseURL pointing at
toker (the setup wizard wires that too).
