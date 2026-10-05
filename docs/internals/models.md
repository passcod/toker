# Learning versions, declaring capabilities

`middleware/models.rs` decides which version of each family is current from what
the ledger has served; `middleware/force_newest.rs` decides whether to move a
request onto it. The context-window catalogue is separate, in
`catalog/windows.rs` and `catalog/fetched.rs`. All three are ports of ctp's,
with its incidents.

## The election

A model becomes its family's target once it has been served on more distinct
local days than `requirement`: half the days the store has seen anything, at
most `MAX_REQUIRED_DAYS` (7). Three things make rewriting safe, and removing any
one breaks it in a way that looks like it works:

- **The bar scales with available history.** An absolute bar asks a young ledger
  for evidence it cannot contain, so a fresh install would sit dormant for a
  week; a scaled one accepts what is present on day one, when there is nothing
  else to go on, and tightens as history arrives. Verified against ctp's real
  log: the same three family targets fell out of every prefix of it, and a model
  used for one day never captured a family once there were six days to judge
  against.
- **Never downgrade.** The target must be strictly newer than what was asked for
  (`newer_than`). Without it the feature eats itself: the first request naming a
  new model arrives before that model is in the store, so "use the newest known"
  rewrites it *down*, the new model never accumulates a day, and the account is
  pinned below it permanently.
- **The election only picks the target.** Whether to rewrite at all is decided
  separately (`force_newest::decide`), and those rules never relax. That
  separation is what makes a weak early bar acceptable: the worst it can do is
  route new sessions to a model you trialled, in that family only.

Days are local calendar days, because "seen on seven separate days" is a
statement about how someone works, not about UTC. The timezone is an input
(`local_day`), and tests pin theirs.

Unpublished dated Claude identities (a `claude-…` id ending in an eight-digit
date the catalogue does not know) have no family at all, so their observed days
or prompt sizes cannot authorise routing traffic to a different identity.

## Only where no cache can be lost

`force_newest::decide` moves a request only when the move cannot cost a cached
prefix:

- **a cold lane**, whose cache is already gone (the cache question,
  `lane_is_cold` with no size floor, never the notice's question);
- **an unknown lane with at most `NEW_CONVERSATION_MESSAGES` messages**, which
  bounds what a mistake could cost to a system prompt and a tool list;
- **nothing served on the asked model within a full cache TTL**
  (`idle_for_ttl`), because a cache belongs to the model it was written on.

An unknown lane is **not** a new session. The lane table forgets lanes (a
restart before ctp's flush, an eviction, a prune, a fresh ledger), so a session
whose cache is warm upstream can look new here, and rewriting its model then
destroys that cache, which is the exact cost the cold gate exists to prevent.
The third condition is what admits a subagent that opens on an inherited
history: measured 2026-09-25, a 45-tool lane asking for `claude-opus-5` beside
main lanes all served on `claude-opus-5-5` was never upgraded under the first
two rules alone.

It is keyed on the model because nothing narrower holds. The tool set looked
like it would, since the tool list opens the prompt, but Claude Code changes
tools mid-conversation and the cache survives (130 tools to 138 at message 403,
reading 526,706 tokens). The record of served models (`ModelStore::note_served`,
`last_served`) is kept apart from the lane table because it must not forget
within the hour it answers for, and it vouches only as far back as the ledger
tail it was seeded from (`covered_since`): a record that starts inside the
horizon answers "not idle". With a model map configured, the recency read uses
the identity the map will actually send (`ForceContext::served_as`), because a
mapped request's cache lives on the target upstream.

With no measured size for an unknown lane, the context check is given an upper
bound from the request's bytes (`prompt_bound`). A zero there once waved every
unknown lane past the context check as soon as a deep conversation could
qualify.

## An upgrade is decided once and then kept

The client never learns of the rewrite, so every later request in that
conversation names the old model again. At first ctp honoured that: only the
first request of an upgraded conversation moved, and the second rebuilt the
whole prefix back on the old model, so every upgrade paid for its cache twice.
The lane now records `forced_from`/`forced_to`, and while the lane is warm a
request still naming `from` goes to `to` (`sticky_target`), a warm compaction
included, since it reads the same cache. A request naming any other model is the
user choosing, and ends the upgrade; a cold lane is decided afresh, because it
has nothing to lose either way. The record is rebuilt from the rows'
`forced_from`/`forced_to` on restart (`lanes_from_rows`), or the first request
after one would be the rebuild all over again.

## Two context facts, never conflated

- `max_prompt` is the largest prompt toker (or ctp, via import) has actually
  watched that model serve. `fits_context` consults only this value. It is the
  conservative guard for force-newest, the compaction retarget, and promotion,
  and an unproven model declines, which costs an upgrade where the alternative
  costs a failed request at the worst possible moment.
- A declared context window is a provider fact, resolved by
  `catalog::windows::resolve_context_window` in this order: the hand-verified
  catalogue (which encodes beta phases a listing cannot), then the provider's
  fetched listing, then a stored declaration, then unknown. A model the
  hand-verified catalogue knows resolves to its verdict even when that verdict
  is unknown: an uncaptured beta phase is a decision, not a gap a listing may
  fill. A declared ceiling is never evidence that this route has served a prompt
  of that size, and nothing raises `max_prompt` from one.

Identities are exact. Published dated snapshots fold into their catalogue key,
but family resemblance never assigns a future model a limit, and a `claude-`
prefix is not capability evidence. The 1M-context beta header selects a limit
only for the catalogue entries and dates it applied to; a compatibility gateway
can carry it while returning another provider's model, and that model gets its
own ceiling or stays unknown. Pricing normalisation is not capability identity.
The catalogue's sources and dates are in `catalog/windows.rs` (its
`VERIFIED_ON`); move the date when you re-verify.

## Requested, effective, served

A row carries three identities: `requested_model` (what the client named),
`effective_model` (what toker sent), and `raw_model` (what the response says
served it, with `model` its normalised form). Only the served identity may add a
day, raise `max_prompt`, select a context ceiling, or choose a price. A routing
map target is an opaque destination, not capability or pricing evidence, and the
same separation covers an upstream that falls back to a model nobody asked for.

## Promotion

`toker promote --model …` grants a served model the days to become its family's
target early (`plan_promotion`). It grants only days the ledger already holds,
because invented days would raise the bar they were meant to clear and un-elect
other families' targets, and it refuses a model never served. It raises the
prompt ceiling to the family's best by default, since a promotion left at "seen
holding 4,000 tokens" would apply only to new conversations; it never declares a
context capacity. A running service takes it through `/_toker/models/merge`;
with nothing listening it applies to the ledger directly.
