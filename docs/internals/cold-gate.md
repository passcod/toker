# The cold-cache gate

`middleware/cold.rs` holds the decision and the notice; the sequencing is in
`server/anthropic.rs` (and, without the outlook or the retarget, in
`server/proxy.rs` for the openai path). Most of this was learned on ctp and
ported as it stood.

## Only a request that could be the re-read is stopped

The lane says how big a prefix was cached and how long ago it was touched; it
does not say that this request is the one about to re-read it. Subagents share
their parent's session id, so a fresh subagent with a sibling's tool set lands
in that sibling's lane (see [lanes.md](lanes.md)). On 2026-10-05 one was stopped
on its first request over a 484k re-read its predecessor had left four hours
before. ctp had the same gap.

So `decide_cold` takes `request_bound`, the request's own upper bound on its
prompt (`force_newest::prompt_bound`: the body's bytes over two, two being below
the least bytes-per-token observed on large requests). A bound under
`cold_min_tokens` forwards whatever the lane held, before the lane is consulted;
an unknown bound decides nothing either way. A bound can only rule a request
out, never in: the lane still has to be cold and large.

## The notice speaks only when the window is already running out

On a meter-source backend the gate asks the weight fit what the re-read costs
and the burn ladder where the 5-hour meter is heading (`cold::quota_outlook`),
and stays quiet unless that window is already projected to run out before it
resets **with the re-read added**. Over ctp's log this withheld more than half
the notices that used to fire.

It is **not** a test of whether the re-read tips the window over, and writing it
as one would be a claim the numbers do not support. Replayed against every
notice in ctp's log, the re-read never changed the verdict: fitted, a 200k
re-read was ~2.1% of a 5-hour window, so it could only tip a projection that
already landed within 2% of the target, and none did. What it does is bring an
existing wall forward by 1–6 minutes, against walls 19–227 minutes out. That is
what the notice says. Text that reads true in the moment and is false on
inspection is the present-tense mistake (below) in a new place.

Three things keep it truthful, and removing any one breaks it invisibly:

- **Every uncertain input fires.** No fit, no weight for the model, a weight the
  data cannot separate from zero, a rolled window, too little history for a
  burn: all answer `known: false`, and the gate behaves as if the outlook did
  not exist. A withheld notice is a re-read the user never hears about, so this
  feature is allowed to be less useful and is not allowed to be silently wrong.
- **A withheld notice is written down** (`kind = 'cold-quiet'`), carrying the
  estimate it rested on. Silence is exactly what a gate that stopped working
  looks like; the row is the only thing separating them.
- **The weights are refitted, never pinned** (invariant 7, in the hot path), and
  priced at the top of their spread (see [quota.md](quota.md)). Where a group
  borrowed its weight from the group it was folded into, the estimate can only
  be too high, which is the safe direction here: too high fires a notice that
  need not have fired, too low loses one that should have. The notice says "up
  to about".

The outlook reads only the routed backend's own meters, and prices the re-read
as the identity the backend's model map will actually send, since the fit's
weights are keyed on served identities and a client's alias is not one.

The 7-day meter gets a veto but no estimate. The weights are fitted against
5-hour windows and say nothing about a weekly one, but whether the weekly meter
is already heading for its own wall needs no weight, and an *unmeasurable*
weekly meter is not a reason to fire, or the feature would go silent whenever
the ledger is young.

`decide_cold` is called twice on the request path, deliberately: the first call
is cheap and says whether anything would fire at all, and only then is a refit
plus two burn measurements worth doing. The rule that reads the outlook stays
inside `decide_cold` (as `ColdDecision::Quiet`) rather than being restated by
the caller, so the two cannot drift.

### Where cache writes are free, nothing is said

Before the outlook, the caller checks the backend's fetched model catalogue:
when it says this model's cache writes cost nothing, the re-read the notice
warns about is free, and the would-be notice becomes a `cold-quiet` row instead.
Only a positive "free" verdict exempts; an unknown model never does. This is the
only check on the openai path, which has no meter source and so no outlook.

### Why Sonnet and Haiku had no weight of their own

Worth knowing before trying to give them one; this is ctp's measurement. They
were not folded for collinearity but by the *instability* net,
`MAX_CONTRIBUTION_SWING`. Forced to carry their own weights, the non-negative
solver pinned Sonnet at exactly 0 with zero leave-one-out spread, which is the
non-negativity boundary rather than a measurement that Sonnet is free.

The data could not resolve it either way, and more of the same traffic will not
help. Sonnet was ~6% of fresh+output volume; of 41 windows only 3 were 30% or
more non-Opus and **none** reached 80%. The most Sonnet-heavy window burned
0.162 per Mtok against an Opus-only median of 0.174, and Opus-only windows
themselves ranged 0.105–0.296: both non-Opus figures sit inside that spread.
Separating them needs Sonnet-dominated 5-hour windows, which that working
pattern does not produce. "Presence is not identifiability", one net further
down.

## Warm compactions need no help

Worth knowing before optimising the wrong one. Across every compaction in ctp's
log, cache writes were a **cold-lane phenomenon**:

| lane | read back | actually written |
| --- | --- | --- |
| warm (2–3 min idle) | 165k–997k | 118–539 tok |
| cold (3–8 h idle) | 0 | 130k–202k |

A warm compaction already pays almost nothing: it reads the prefix and writes
only the delta. So there is no version of "strip the breakpoints everywhere"
that helps; it would trade a free read for a full-price rebuild. The whole
saving lives in the cold case, which is the case `retarget_compaction` is
licensed for.

## Two clocks, not one

A lane record carries `updated_ms` (ctp's `at`) and `noticed_at`, and conflating
them breaks the feature in a way that looks like it works.

`updated_ms` means "when this lane's cache was last touched" and moves only on a
response the API actually served. `noticed_at` means "we have already spoken
about this idle spell".

The cold notice records `noticed_at` (`cold::note_lane_notice`) and deliberately
does not move `updated_ms`, because the compaction retarget judges coldness by
it. A notice that re-armed by resetting the idle clock would make the lane look
active, so the `/compact` it just recommended would no longer read as cold: the
proxy would promise a cheap compaction and then decline to give one, one
keystroke later.

It also gives the better re-arm: the notice returns when the lane is *active
again* and then goes idle, rather than on a timer. A second notice for the same
idle spell would carry the identical number to the first. A `cold-quiet` verdict
moves neither clock, so a later request in the same spell is judged again
against meters that may have tightened.

**Two clocks were not enough.** `decide_cold` answers "should the user be
interrupted", which is false once we have spoken this spell and false outright
on a summarising request. `lane_is_cold` answers "is the cache gone", which
neither of those bears on. ctp's rewrite asked the first and got the second
wrong: the notice fired at 13:31, the user ran `/compact` at 13:33 exactly as
advised, and the rewrite declined because the lane had been noticed, so a
205,535-token compaction the notice had *promised* on Sonnet ran on Opus, one
keystroke after the promise. Anything asking about the cache uses
`lane_is_cold`; only the notice uses `decide_cold`. The retarget is gated on the
compaction test and lane coldness alone, not on the notice's toggle, because the
cold licence is the lane's.

## A lane's TTL is its longest-lived breakpoint

How long a lane's cache survives comes from the tier it was seen writing
(`lanes::lane_ttl`, read by `cold::ttl_of`), and the tier is sticky: a warm turn
writes a small delta at the 5-minute tier on top of a prefix written at the
1-hour tier, and reading the delta's tier as the prefix's lifetime fired ctp's
notice at five and six minutes idle on caches that were plainly live. An
unrecorded tier reads as the long one: guessing short fires on live caches (a
false alarm costs the user a turn), guessing long only delays a true one. See
[measuring.md](measuring.md).

## The notice is a record, not advice

It is live for one turn and historical for the rest of the conversation: it
stays in the transcript, and survives compaction, because compaction keeps the
recent tail. So it is past-tense and stamped with the time it fired ("Paused by
toker at …"), the way Claude Code's own `[Request interrupted by user]` names an
event.

Written in the present tense, ctp's kept giving advice long after it stopped
being true: one reading "idle 8h 14m, consider /compact, this re-reads 200,621
tokens" sat inside a conversation that had just been compacted and held 108k,
wrong in every particular and indistinguishable from a live notice. Text that is
read far more often than it is acted on has to be correct in the state it spends
most of its life in. The rest of what a notice may and may not say is in
[notices.md](notices.md).
