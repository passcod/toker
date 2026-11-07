---
id: RPIN
---

# Pinning a lane to a route

A lane remembers the route its cache was built on, and every move of that pin goes through one decision.
This spec covers which revision a pinned lane resolves under, what consents a move needs, and what a fallback does to a pin.

What a lane is, and what its pin records, are specified by [SLANE](../store/lanes.md).

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

The upgrade's own sticky record is separate from the pin, and is specified by [RMODEL](models.md#an-upgrade-is-decided-once-and-then-kept).

## Fallback and the pin

A fallback to a candidate on another binding is a change of cache identity.

On a warm lane it MUST require every crossed edge to consent to losing the cache.
On a cold lane consent conditional on coldness MUST suffice.
A lane Toker has no record of MAY move freely, subject to the billed-boundary consent.

An edge crossing cache identities MUST NOT be able to declare that it preserves a warm cache, and the compiler MUST refuse one that does.

> [!NOTE]
> Such an edge could never be taken.

Once a fallback response is accepted its candidate MUST be pinned, so that later turns stay there and do not return when the earlier candidate recovers.
