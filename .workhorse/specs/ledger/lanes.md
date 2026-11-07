---
id: LLANE
---

# Lanes, cache identity and pins

A session is not a cache entry.
One session interleaves several independent prefixes, each with its own cache, so comparing a request against whatever preceded it in time compares unrelated conversations.

This spec covers the lane, the cache identity a lane's pin records, and the single decision every move of that pin goes through.

## The lane

A lane MUST be keyed by the session together with a digest of the request's tool set.

Everything comparing consecutive requests MUST compare within a lane.

A lane with no session, or from before the tool digest was recorded, MUST be no lane at all.
Toker MUST NOT collapse such requests into a shared lane.

A request carrying no tools MUST key the empty tool set's lane, as a tool set of its own.

The lane table MUST be bounded in size and age, and pruned on a periodic tick.

### Reading lanes

A reader walking lanes MUST walk every row it reads and filter to its window afterwards.

> [!NOTE]
> A lane's predecessor may fall outside the window, so filtering first opens every window with rebuilds that never happened.

A reader MUST NOT treat a collapsing message count as a compaction.

> [!NOTE]
> A subagent starting looks identical by shape.
> The compaction markers are the only separator, as [GCOMP](../gates/compaction.md) specifies.

An aggregate over a session MUST NOT mix its lanes.
Where a single figure must stand for a session's context, it MUST be taken from the session's largest lane.

> [!NOTE]
> A small utility call after a large main-agent turn otherwise reads as a collapse.

### A lane is not a conversation either

A lane narrows a session to a tool set and no further.

> [!NOTE]
> Subagents share their parent's session, so a fresh subagent whose tool set matches an earlier sibling's lands in that sibling's lane.

A lane records the cache that was written.
Only the request bounds the prefix this send would re-read, so anything reading a lane to judge a request MUST also ask whether the request could be the one the lane describes.

## Cache identity

A cache identity is the binding together with the exact model sent upstream.

A lane MUST have one cache identity at a time.

> [!NOTE]
> On the providers Toker serves, the same lane sent to another provider, or to the same provider on another model, reads nothing of what was written, however stable the client's own model string stayed.
> A virtual slug whose route was edited to another provider otherwise makes two unrelated provider contexts look like one warm lane.

A binding MAY declare a cache domain, being an identifier and a verified cache lifetime.

A binding with no cache domain MUST NOT be treated as uncached.

> [!NOTE]
> It means the lifetime has not been verified, so such a lane reads its lifetime from what it recorded instead.

A pin MUST carry the route fingerprint as well as the models.

> [!NOTE]
> The effective model alone cannot say whether a lane is still on the route it was resolved under.

A pinned lane MUST start its candidate walk from its pinned candidate only while the route's fingerprint still matches, and MUST start from the first candidate otherwise.

## The pin

A pin MUST record the policy revision, the route's identifier and fingerprint, the candidate, the binding, the baseline model, the effective model and a generation.
Those MUST be all present or all absent, and that MUST be enforced by the store.

A pin MUST be written only when a response is recorded as served.
A failed attempt, a cancelled attempt and a failure before sending MUST NOT pin.

An ordinary lane observation MUST omit every pin column.

> [!NOTE]
> A response then never overwrites a routing decision made concurrently.

A pin MUST be written by compare-and-swap against the generation and the whole previous pin, or its absence, as the request saw it when it began.

The loser of a concurrent swap MUST record its own row and MUST NOT be retried or re-resolved.

> [!NOTE]
> The winner's decision was made on the same evidence.

A first pin of a new lane, and a replacement caused by an activation, MUST be written only while the revision they were resolved under is still active.

> [!NOTE]
> A request that resolved under one revision and finished after a later one became active cannot write the earlier revision back.

A move to a revision older than the lane's MUST be refused.

### A lane with no pin

A lane with no pin MUST resolve under the active revision exactly as a new lane does, whatever the activation scope, and MUST be pinned by its next response.

Toker MUST NOT seed a pin from anything but a served response.

> [!NOTE]
> The cache such a lane may hold cannot be attributed to a binding, and this costs at most one cold re-read per lane that was warm.

A proven-cold temperature MUST be kept for such a lane, because the compaction route needs it.

A request carrying provider-owned state on a lane with no pin MUST be refused, since only the binding that produced that state can replay it and nothing records which that was.

A pin MUST live in the lane row, so that pruning a lane drops its pin.

## Revision selection

| pinned revision | resolves under |
| --- | --- |
| none, or the active one | the active revision |
| newer than the active one | an error |
| older, under a new-lanes activation | the pinned revision |
| older, under a new-and-cold-lanes activation | the active revision where the lane is provably cold, else the pinned one |
| older, under an immediate activation | the active revision, unless the move would lose a warm cache and the activation was not forced |

An older revision MUST be compiled on demand against the current inventory and availability.

> [!NOTE]
> A pinned lane keeps its policy without keeping stale evidence.

## One decision for every move

Every move of a pin MUST go through one pure decision, taking the lane state captured when the request began, the pin the resolver prospectively produced, and the cause of the move.

That decision MUST yield a first pin, a keep, a replacement, or a denial with its reason.
The caller MUST apply it with one compare-and-swap after the response succeeds.

A denied request MUST NOT be sent.

> [!NOTE]
> Routing it anyway would lose the cache the policy just said to keep.

| cause | what must consent |
| --- | --- |
| compaction | the plan's lane transition policy, and its cache transition policy where the cache changes |
| upgrade | the plan's lane transition policy |
| fallback | the strictest cache transition among the crossed edges |
| the operator choosing another route, candidate or model | nothing; the choice carries its own consent to lose the cache it leaves |
| anything else differing under the same revision | the route's own cache transition |
| activation | the activation's scope, and its force consent where a warm cache would be lost |

Where no consent is recorded, the decision MUST be to keep the pin.

The source of a transition MUST decide whether the activation fence applies, and MUST NOT be stored.
What the ledger keeps of a move is the row's identities.

The upgrade's own sticky record is separate from the pin, and is specified by [RMODEL](../policy/models.md#an-upgrade-is-decided-once-and-then-kept).

## Coldness evidence

Coldness is asked about in three ways, and they MUST NOT share a source.

### Cache temperature

This MUST be answered for the lane's pinned binding, as warm, cold or unknown.

| condition | answer |
| --- | --- |
| the lane is not pinned to the binding asked about | unknown |
| the lane recorded no cache lifetime | unknown |
| the lane is stamped in the future | unknown |
| idle time has reached the lifetime | cold |
| otherwise | warm |

The lifetime MUST be the binding's declared cache domain lifetime where it has one, and the lifetime the lane recorded from the provider's reported usage otherwise.

Every consumer MUST treat unknown as warm.

> [!NOTE]
> A wrong verdict of cold rebuilds a whole prefix; a wrong verdict of warm costs one upgrade, or one move that waits for the next idle spell.

### Lane coldness

The cold gate and the compaction retarget MUST read the lane's own record rather than its cache temperature, as [GCOLD](../gates/cold.md#a-lanes-cache-lifetime) specifies, where an unrecorded lifetime reads as the longest.

That default is deliberately the opposite of cache temperature's.

> [!NOTE]
> A notice interrupts the operator, so guessing that the cache is live costs less than guessing it is gone.
> A transition moves a cache, so the reverse holds.

For a compaction, the resolver MUST be told the lane is cold only where that record says so, whatever the pin and the binding's lifetime would say.

### Activation consent

Activation consent MUST count lanes by cache temperature, and MUST count a lane with no pin as warm or unknown.

A ledger that cannot be read MUST refuse the activation, and MUST NOT read as having no warm lanes.

## Fallback and the pin

A fallback to a candidate on another binding is a change of cache identity.

On a warm lane it MUST require every crossed edge to consent to losing the cache.
On a cold lane consent conditional on coldness MUST suffice.
A lane Toker has no record of MAY move freely, subject to the billed-boundary consent.

An edge crossing cache identities MUST NOT be able to declare that it preserves a warm cache, and the compiler MUST refuse one that does.

> [!NOTE]
> Such an edge could never be taken.

Once a fallback response is accepted its candidate MUST be pinned, so that later turns stay there and do not return when the earlier candidate recovers.
