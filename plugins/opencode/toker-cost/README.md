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
  `provider · requests · $cost`), and a Lanes block, one row per toker
  lane of the session (a lane is a tool set with its own prompt cache, so
  a subagent shows as its own row):
  `id · state · prompt size · time`, where `id` is a short prefix of the
  tool-set digest, `state` is `warm` (with the time left on its cache
  TTL) or `cold` (with its idle time), and a second line gives the model
  a lane was moved off and on to when toker did that. A ping lane, from
  toker's window pinger, reads `ping id · time ago` with no prompt size
  or countdown, so it is not mistaken for conversation cache. Nothing
  renders when toker answers nothing, and the Lanes block is left out
  when toker holds no lane for the session: absence, not zeros.

The plugin asks toker for all of this in one `/_toker/session` request,
cached for two seconds, so the Lanes block adds no polling of its own.

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
