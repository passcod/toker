# Learning versions, declaring capabilities

Which version of a model family is current is decided from what each backend
binding has served, not from a list anyone maintains. `policy/learning.rs`
holds the evidence and the election bar, `policy/family.rs` splits a model id
into family and version, and the upgrade election itself is `choose_upgrade` in
`policy/resolve.rs`, run for a candidate whose policy attaches an upgrade
policy. `policy/force_newest.rs` holds the two constants and two pure functions
the election reads. The context-window catalogue is separate, in
`catalog/windows.rs` and `catalog/fetched.rs`. All of it descends from ctp's,
with its incidents.

## What is learned, and where

Learning is per binding. A model identity means nothing until it is attached to
the backend that served it: the same name on the subscription and on an API key
is two models for every question asked here (served days, largest prompt, cache,
price), and an observation on one must not license a move onto the other.

`learned_models_by_binding` is keyed by binding and exact model id and holds the
distinct local days the model was served, the largest prompt seen, an optional
declared context window, and the `explicitly_promoted` bit. A completed response
merges its observation in (`BindingLearningView::note_observed`, which calls
`Store::merge_binding_learning`): the day is added to the set, the prompt size
keeps the maximum, and the promotion bit, once set, stays. The identity is the
provider-reported one, exactly as the response named it (`raw_model`), never
what was asked for. A response that names no model teaches nothing.

Nothing rebuilds this table from history. Ledger rows from before policy routing
carry no binding, so there is nothing to attribute them to, and a row imported
from ctp has none either. A machine that moved to policy routing therefore
starts with empty learning. That is cheap because of the scaled bar below: it
accepts what is present on day one, and tightens as days accumulate.

The older `models` table, which was keyed by model alone, is no longer written
by anything. It survives in existing ledgers, and `toker watch-context-window`
still reads the context declarations in it (below). Do not read it for routing.

## The election

An upgrade policy names a classifier (how to split an id into family and
version), the sources of candidate models, how much evidence a model needs, and
whether a manual promotion can stand in for that evidence. Revision one's
policies take their candidates from what the binding has served and learned, not
from its catalogue: a model that is listed but has never run on this machine is
not a candidate.

A model becomes the target for its family once it has been served on more
distinct local days than `requirement` (`learning::requirement`): half the days
the binding has any evidence for, capped at `MAX_REQUIRED_DAYS` (7). Three
things make rewriting safe, and removing any one breaks it in a way that looks
like it works:

- **The bar scales with available history.** An absolute bar asks a young ledger
  for evidence it cannot contain, so a fresh install would sit dormant for a
  week; a scaled one accepts what is present on day one, when there is nothing
  else to go on, and tightens as history arrives. Verified against ctp's real
  log: the same three family targets fell out of every prefix of it, and a model
  used for one day never captured a family once there were six days to judge
  against.
- **Never downgrade.** A candidate must belong to the same family and have a
  strictly greater version than the model that was asked for. Without it the
  feature eats itself: the first request naming a new model arrives before that
  model has been learned, so "use the newest known" rewrites it *down*, the new
  model never accumulates a day, and the account is pinned below it permanently.
  Versions compare segment by segment, numerically: `claude-opus-5` is newer
  than `claude-opus-4-8`, `4-10` is newer than `4-8`, and `5-1` is newer than
  `5`. String order gets all three wrong.
- **The election only picks the target.** Whether to rewrite at all is decided
  separately, by the cache rules below, and those never relax. That separation
  is what makes a weak early bar acceptable: the worst it can do is route new
  sessions to a model you trialled, in that family only.

Days are local calendar days, because "seen on seven separate days" is a
statement about how someone works, not about UTC. The timezone is an input to
the merge, and tests pin theirs.

A manual promotion (below) replaces the day count for that one model and
nothing else: it still has to be newer in its family and still has to fit the
prompt.

The family split (`family_of`) folds published snapshot aliases to their
dateless form first. An unpublished dated Claude identity (a `claude-...` id
ending in an eight-digit date the catalogue does not know) has no family at all,
so its observed days or prompt sizes cannot authorise routing traffic to a
different identity. A name with no version has no version to compare, so the
revision-one classifier leaves it out of the election, as target and as
baseline.

## Only where no cache can be lost

`choose_upgrade` moves a request only when the move cannot cost a cached prefix.
It asks about the lane first:

- **a known lane is eligible only if it is cold**: its cache is already gone.
  This is the cache question (`lane_is_cold` with no size floor), never the
  notice's question;
- **a request with no lane evidence** (the lane table forgot it, or the lane
  predates policy pins and is not proven cold) is eligible if it has at most
  `NEW_CONVERSATION_MESSAGES` messages, which bounds what a mistake could cost
  to a system prompt and a tool list;
- **or if nothing has been sent on the asked model, on this binding, within a
  full cache TTL** (`idle_for_ttl`), because a cache belongs to the model it was
  written on.

A lane with no evidence is **not** a new session. The lane table forgets lanes
(a restart before the flush, an eviction, a prune, a fresh ledger), so a session
whose cache is warm upstream can look new here, and rewriting its model then
destroys that cache, which is the exact cost the cold gate exists to prevent.
The third condition is what admits a subagent that opens on an inherited
history: measured 2026-09-25, a 45-tool lane asking for `claude-opus-5` beside
main lanes all served on `claude-opus-5-5` was never upgraded under the first
two rules alone.

It is keyed on the model because nothing narrower holds. The tool set looked
like it would, since the tool list opens the prompt, but Claude Code changes
tools mid-conversation and the cache survives (130 tools to 138 at message 403,
reading 526,706 tokens).

The recency record is kept apart from the lane table because it must not forget
within the hour it answers for. It is keyed by binding and the exact effective
model, marked immediately before an inference send starts (`note_sent`), not
when the response finishes. It vouches only as far back as it covers
(`covered_since`, the earliest send it knows of for that binding). It is seeded
at start from the newest ledger rows that carry both a binding and an effective
model, counting error rows too, since an error row can be a provider response
and dropping a real send could lose a live cache while keeping a local failure
only postpones a move. A binding with no recorded send has no coverage, so its
silence proves nothing and the answer is "not idle". Every doubtful value
answers no, which costs an upgrade; a wrong yes costs a rebuild of the whole
prefix.

With no measured size for a lane that is unknown, the context check is given an
upper bound from the request's bytes (`prompt_bound`). A zero there once waved
every unknown lane past the context check as soon as a deep conversation could
qualify.

A compaction is never upgraded by this election. It has its own retarget, which
moves a cold compaction to a cheaper model by the same classifier output; see
[cold-gate.md](cold-gate.md).

## An upgrade is decided once and then kept

The client never learns of the rewrite, so every later request in that
conversation names the old model again. At first ctp honoured that: only the
first request of an upgraded conversation moved, and the second rebuilt the
whole prefix back on the old model, so every upgrade paid for its cache twice.
The lane now records `forced_from`/`forced_to`, and while the lane is warm a
request still naming `from` goes to `to`, a warm compaction included, since it
reads the same cache. A request naming any other model is the user choosing, and
ends the upgrade; a cold lane is decided afresh, because it has nothing to lose
either way. The record is rebuilt from the rows' `forced_from`/`forced_to` on
restart (`lanes_from_rows`), or the first request after one would be the rebuild
all over again.

## Two context facts, and which one decides a fit

- A declared context window is a provider fact. For the upgrade's fit check
  (`fits_context`) it is the routed binding's fetched models listing, looked up
  by the identity the upgrade would send. When the listing names a window it
  decides, both ways: a prompt over it declines whatever was learned, and a
  prompt within it fits whatever was not. A stored declaration on the learned
  record comes next.
- `max_prompt` is the largest prompt toker has actually watched that model serve
  on that binding. It decides a fit only when no window is named at all, and
  there an unproven model declines, which costs an upgrade where the alternative
  costs a failed request at the worst possible moment.

The order was the other way round until 2026-10-06, with `max_prompt` the only
guard. Learning undershoots by construction, since it is the largest prompt
anyone happened to send: `claude-sonnet-5` was learned at 248k against a listed
1,000,000, and a 485k cold compaction stayed on Opus because of it. The
compaction retarget's own fit check reads the listing first for the same reason.

For display and for `watch-context-window`, a window is resolved by
`catalog::windows::resolve_context_window` in this order: the hand-verified
catalogue (which encodes beta phases a listing cannot), then the provider's
fetched listing, then a stored declaration, then unknown. A model the
hand-verified catalogue knows resolves to its verdict even when that verdict is
unknown: an uncaptured beta phase is a decision, not a gap a listing may fill.
A declared ceiling is never evidence that this route has served a prompt of
that size, and nothing raises `max_prompt` from one.

`toker watch-context-window` (`watch.rs`) is the one place a declared or exact
window is turned into a claim, and it claims only what a served response proves:
an exact 1M window holding a prompt over 200k, or a prompt over a declared
ceiling. Unknown windows and prompts under the threshold say nothing. It reads
only rows from its `--since` (or the start its state file recorded), because
without the bound the whole ledger replays as news, and it keeps a seen-set in
a state file (beside the ledger unless `--state` names one) so each proof prints
once. It still takes its stored declarations from the old `models` table. Like
ctp's it has no compaction detector: within a lane a compaction and a subagent
starting both look like the prompt collapsing, and nothing in the ledger
separates them, so that negative evidence is left unreported.

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
day, raise `max_prompt`, select a context ceiling, or choose a price. The
recency record is the one reader of the effective identity, because a cache
belongs to what was sent. A candidate's alias target is an opaque destination,
not capability or pricing evidence, and the same separation covers an upstream
that falls back to a model nobody asked for.

## Promotion

A promotion marks a learned model explicitly eligible for upgrades where its
policy allows manual promotion. It is available from the routing page and the
`routing-promote` operation (see [control.md](control.md)), and from
`toker promote --model ...`. If exactly one binding has learned the exact
identity it is inferred; otherwise `--binding <provider>.<dialect>` is required.
The CLI also checks the model against the active revision's upgrade policies,
refusing one that none classifies or that several do, and has a `--dry-run`.
All of them refuse a pair that has never been served and change only the sticky
bit: observed days, prompt ceilings and context declarations remain provider
evidence.

The CLI writes the ledger directly and does not talk to the running service. The
control operation also refreshes the service's published learning snapshot.
