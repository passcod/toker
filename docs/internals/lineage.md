# Session lineage

Some requests belong to a session without being its conversation: a reviewer
sub-session that a client opens beside the main one, or a utility call. The
ledger can say so, for recording and display, when the wire says so. It never
says so on a guess, and it never lets the relation change how anything is
routed.

## Two identities

A request's **raw session** is the identity it arrived with: the first of the
configured `session_header_names` the request carries (by default
`x-toker-session`, `x-claude-code-session-id`, `x-session-id`), or on the
Responses protocol the body's `prompt_cache_key` when there is one. It is
stored in `session_id` whole and is what the lane table, the allowances and the
prompt cache are keyed on. [lanes.md](lanes.md) is why: a cache belongs to one
conversation's prefix, and two sessions never share one.

A **display parent** is a second, optional identity: `parent_session_id`, with a
`child_role` and a `relation_source` saying what kind of child it is and what
evidence established it. The three columns are all set or all NULL. The raw
session is never replaced by the parent, so a child that is shown under its
parent still has its own lane, its own cache and its own allowance.

## What counts as evidence

Exactly one convention is recognised. On the Responses protocol, through the
`codex` frontend (`/f/codex`), a session header of the form
`guardian:<session id>` marks a Codex guardian session, and the suffix is the
session it belongs to. toker records it as role `guardian`, source
`codex_guardian`, with the whole header value kept as `session_id` and the
suffix as `parent_session_id` (`SessionLineage::codex_guardian`, read in
`server/codex.rs`). The conditions are all required and all checked:

- The frontend is `codex`. The same string from another client is just a
  session id.
- The identity came from the session *header*. If the body's `prompt_cache_key`
  supplied the session instead, the header is not the identity the cache uses
  and no relation is recorded.
- The suffix is not empty.
- The store accepts only the verified role and source pair, a parent that
  differs from the child, and a row that does not close a cycle through earlier
  rows (see [ledger-schema.md](ledger-schema.md)). The reader re-checks the same
  on the way out, so a bad stored value fails the read instead of being
  displayed.

A parent that has no row of its own is fine: the relation is kept so the display
can resolve it later, and until then the child shows under the parent's id.

The relation rides the measurement and error rows of the Responses path. Rows
toker writes about itself (gate notices, releases, `route-unavailable`) do not
carry it, and nor do rows of the other frontends.

## Why nothing else counts

Evidence is something the request itself states, on the wire, in a field the
frontend defines for the purpose. Not evidence: when the request arrived, which
model it named, how many messages or tools it carried, whether a transcript
exists for it, which connection it used, or what its body looks like. Every one
of those is a guess that is right until the day it is wrong, and a wrong lineage
is worse than none because it moves one conversation's tokens and cost into
another's. An unlinked session simply stays its own row, including one whose id
looks like a UUID and happens to sit beside a busier session.

The same holds for the calls clients make on the side. Whatever session id such
a call carries is its raw identity: one that carries the main session's id is
already in that session's accounting, and one that carries a fresh id and no
parent identifier is a session of its own, because there is nothing on the wire
to relate it to an originator. toker does not attach it by looking at it. If a
frontend later sends an explicit parent or request-class header, the rule is to
read only those named, content-free headers, and a request class may label a
role without establishing a parent unless the wire also supplies a verifiable
relation. Nothing reads such a header today.

### Auxiliary Claude calls and the model they name

The same discipline is why a Claude Code classifier, title, helper or compaction
call is routed like any other. Policy resolves a request from content-free
facts: the frontend profile and protocol, the requested model, the raw lane
reference, the size and counts of the request, and whether it is a compaction
(decided from fixed markers and the tool set, see
[compaction.md](compaction.md)). It does not know a call is a classifier, and no
route is chosen by the call's role or shape. A bare `claude-*` model on these
calls is looked up the way any model name is: presentation, then rewrite rules,
then the profile default, then the protocol default.

The case that needs care is a machine with no Anthropic default: the config
names none and enables either no Anthropic-protocol backend or several without
choosing one. A bare `claude-*` name then matches no default, and the request
would fail as not configured even though the user has a working backend. The
generated revision one handles it: when the config names no Anthropic default
but a configured Messages binding has a model map, a rewrite rule
(`legacy-auxiliary-claude`, matching `^(?P<model>claude-.+)$`, last in priority
order) sends every bare `claude-*` request to that binding's route, where the
route's alias policy rewrites the name through the same map. The whole mechanism
runs on the model string alone, so a classifier, a title call and a helper reach
the mapped backend without anything inspecting what they carry, and none is
invented a parent. With no such binding and no default, the request has no route
and gets the not-configured answer.

An authored policy owns this itself: a profile or protocol default, or a rewrite
rule on the Claude names, decides where those calls go.

## Lineage is for display and accounting only

Routing reads the raw session. `RequestFacts.lane_reference` is the raw lane key
and the code says in a comment that it is deliberately not lineage. Outside
recording the row, `parent_session_id` is read only by the store's display
queries and the dashboard; the resolver, the lane pin, the gates and the
allowances never see it. A guardian session therefore has its own lane, goes
cold on its own clock and is gated on its own allowances. Lineage is read in
exactly two places.

- **The dashboard** (`tui/model.rs`). Rows with a relation are grouped under the
  parent for accounting: the parent's request count, token totals and spend
  include its linked children, and the session detail names the children by role
  (`2 guardian`). The lanes underneath stay keyed by raw session, so a child's
  large prompt cannot become the parent's context size: a display session reads
  its main lane from its own raw session when it has one in the window, and
  falls back to a child lane only when the parent has none. A child's title and
  working directory are resolved through the parent's id, never the child's own.
- **The session reads.** `Store::display_session_summary` and
  `Store::latest_display_session_row`, behind the TUI's detail popup, match
  `session_id = ?1 OR parent_session_id = ?1`. The `/_toker/session` endpoint,
  which the opencode plugin polls, uses `Store::session_summary` and matches the
  exact `session_id` only: a client asking about a session gets that session,
  not its children.

Old rows have no lineage, and a session that was a guardian before the relation
was recorded shows as its own session. That is a gap, not a negative.
