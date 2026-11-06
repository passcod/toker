# The lane rule

**A session is not a cache entry.** This is the mistake to know about; ctp made
it three separate times in one day, in three different files, and toker made it
again in the cold gate (below).

One Claude Code session interleaves several independent prefixes (the main
agent, subagents, and small utility calls), each with its own cache. Comparing a
request against whatever preceded it in wall-clock order compares unrelated
conversations and invents rebuilds that never happened.

Everything that compares consecutive requests must compare **within a tool-set
lane** (`session_id` + `tools_hash`, `lanes::lane_key`). What a lane's cache
actually lives on, which backend and model, is a separate fact the lane carries
as its routing pin; see [lanes-and-cache.md](lanes-and-cache.md). The lane table
is the `lanes` table in the ledger, upserted on every response and pruned to
4000 lanes and 30 days on a 30-second timer, where ctp rewrote a JSON file.
Related traps:

- A lane's predecessor may fall outside your time window. Walk lanes over every
  row read, then filter to the window, or each window opens with phantom "new
  prefix" rebuilds. The TUI's rebuild walk (`tui/rebuilds.rs`) reads a 24-hour
  tail for exactly this.
- A message count collapsing to a handful is a **subagent starting**, not a
  compaction. They are indistinguishable by shape; use the compaction markers.
- Per-session aggregates (a peak, a max) mix lanes. A small utility call after a
  big main-agent turn reads as a collapse. The quota notice's context size uses
  the session's *largest* lane (`lanes::session_prompt`) for that reason.
- The cold gate is keyed per lane for the same reason. Keyed per session, a
  two-token title summariser would stand in for the main agent's 400k prefix
  *and* would keep resetting its idle clock, so the notice would never fire for
  the lane that matters.

## A lane is not a conversation either

The lane key narrows a session to a tool set, and that is as far as it goes.
Subagents share their parent's session id, so a fresh subagent whose tool set
matches an earlier sibling's lands in that sibling's lane. On 2026-10-05 one was
stopped by the cold gate on its first request, over a 484k re-read its
predecessor had left four hours before; the request itself was small. ctp had
the same gap.

What the lane records is what *was* cached. Only the request says what this send
would re-read, so `cold::decide_cold` also takes an upper bound on the request's
own prompt (`policy::force_newest::prompt_bound`: bytes over two, from the least
bytes-per-token observed above 200k). A request whose bound is under the
threshold forwards whatever its lane held; an unknown bound decides nothing.
The upgrade policy's context-fit check reads the same bound. Anything else that
reads a lane to judge a request should ask whether the request could be the
thing the lane describes.

## The session endpoint's lane view

`GET /_toker/session?session=<id>` carries a `lanes` array for the opencode
plugin, read from the lane table through `lanes_session_idx` (exact session id,
newest first), not from the ledger and not by filtering every lane. Each entry
is the lane's recorded metadata plus a state worked out at query time:
`tools_id` (the first six characters of the tool-set digest), `state`,
`prompt_tokens`, `ttl_ms`, `idle_ms`, `warm_remaining_ms`, `noticed_ms`, and
`forced` (`{from, to}`, the sticky upgrade the policy made, see
[policy.md](policy.md)). The pin is not part of this view.

- `state` is `ping` for a ping lane, which is never called warm or cold because
  its cache is not the conversation's. Otherwise it is `cold` once idle time
  reaches `cold::ttl_of`, the gate's own reading, and `warm` before. A lane
  stamped in the future has no idle time, so its state is null.
- `warm_remaining_ms` needs a recorded TTL. An unrecorded tier reads as the long
  one for `state`, as it does in the gate, but gives nothing to count down from.
- A session with no lane rows, or whose lanes were pruned, answers `[]`. Fields
  a lane never recorded are null, not zero.

## Lanes toker keys differently

ctp collapsed a missing session or tool hash into a shared `?` lane. toker does
not: no session, or a row older than the tools-hash field, means no lane at all,
and a request with no tools keys the empty tool list's lane, as a real tool set
of its own. The TUI's rebuild walk gives a NULL `tools_hash` its own lane rather
than a shared one.

OpenRouter Chat lanes run on its 10-minute sticky window
(`lanes::OPENAI_LANE_TTL_MS`), a recognised duration in `cold::ttl_of`. Reading
it as the unrecognised long tier would hold the sleep lock for an hour after
every OpenRouter request. Other Chat bindings record no clock, because their
cache lifetime is not verified and they must not borrow OpenRouter's. When a
request carries a session, their lane row exists to hold the routing pin, but
it has no TTL and no prompt size: the cold gate cannot fire for it, and the
policy reads its coldness as unknown, never cold
([lanes-and-cache.md](lanes-and-cache.md)).
