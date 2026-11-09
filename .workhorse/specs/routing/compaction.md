---
id: RCOMP
---

# Retargeting a cold compaction

A compaction on a lane whose cache is gone may be moved to a cheaper model, and its body rewritten to suit.
This spec covers how coldness is proven, how the target is chosen, and the transform that follows.

How a compaction is detected is specified by [GCOMP](../gates/compaction.md).

Detection establishes only that a request is a compaction.
Which route and model it runs on MUST be decided by the active policy, as [RSOLV](resolver.md#lookup-order) specifies.

Toker MUST pass the resolver the request's class and the lane's own evidence of how cold it is.
Toker MUST NOT let the marker text reach policy.

## Proving a lane cold

Coldness MUST be proven from the lane's own record, being the time it was last updated against its recorded cache lifetime, with the configured minimum prompt size and idle floor applied.

Coldness MUST be read from the same clock the cold notice uses.
Coldness MUST NOT depend on whether that notice is enabled.

> [!NOTE]
> A user who runs a compaction after reading a cold notice must find the lane cold, or the notice recommended something the proxy then declines to do.

A compaction on a lane that is not cold by its own record has unknown temperature, which MUST rule out every move below.

Coldness here MUST NOT be delegated to the lane's pin.

> [!NOTE]
> Once the recorded lifetime has run out, no cache remains on any binding for a pin to protect.

## Choosing the target

A policy revision names either a route for compactions, or a model substitution applying to the route the request would have taken anyway.

A compaction route MUST apply only to a lane proven cold, and MAY move the compaction to a different provider as well as a different model.

A model substitution names the bindings it applies to, and either an exact model or a family to move to.
It MUST keep the binding the request reached, and MUST apply only where that binding is among those it names.
A substitution naming no model MUST keep every compaction's model, so only the transform applies.

> [!NOTE]
> A compaction route fixes one binding, so it cannot say "a cheaper model on whichever account the lane was already using", and that is what keeps a substituted compaction on the subscription it was going to spend anyway.

A model substitution MUST apply only where all of these hold:

| condition |
| --- |
| the lane is proven cold |
| the body transform below would succeed |
| the target holds the lane's recorded prompt size |
| the target's input rate is strictly below the source's, after aliasing |
| the target has a known price |

Where the substitution names a family, Toker MUST classify through the upgrade policy of the candidate the request reached, and MUST pick that family's newest model the binding has learned that clears that policy's evidence bar, which [RMODEL](models.md#electing-a-familys-target) specifies.
Where that candidate declares no upgrade policy, a family substitution MUST NOT apply.
Where any condition fails, the compaction MUST keep its model and only the transform applies.

## The transform

Once the target is chosen, Toker MUST strip the compaction's cache breakpoints and merge its mid-conversation system messages into a safe host, together or not at all.

> [!NOTE]
> A cold lane would otherwise pay to write a cache nobody will read.

Toker MUST probe the transform on a copy of the body before resolving, so that a model is never moved for a body the transform would then decline.

Toker MUST record that breakpoints were stripped and that system messages were merged.
Toker MUST record the model moved from and to.

> [!NOTE]
> By the time the transform runs the request is already on the new model, so the move cannot be read off the request itself.

A fallback attempt on a compaction MUST re-apply the transform for the model that attempt was given.

A compaction that detection missed MUST NOT be retargeted, whatever the policy names.
