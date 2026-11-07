---
id: RSOLV
---

# Resolving a request

The resolver turns a request and a [compiled policy](overview.md#the-compiled-policy) into the route that will serve it.
This spec covers what the resolver reads, how a route is looked up and walked, and how a request that nothing can serve is answered.

What happens when a candidate fails after its request was sent is specified by [RFALL](fallback.md).

The resolver MUST be pure.
It MUST read no clock, no store, no provider and no request body, taking the present moment as one of its inputs, so that the control surface can run the same function the request path does.

## Facts in

The facts the resolver receives MUST be content-free: the profile and protocol, the requested model, the lane key, an upper bound on the prompt size, message and tool counts, the capabilities the request needs, a binding affinity, the request's class, and the present moment.

A request carrying provider-owned state, such as signed or redacted thinking or a provider's encrypted reasoning, MUST set a binding affinity, because only the binding that produced that state can replay it.
Such a request on a lane with no pin MUST be refused.

## Lookup order

The first of these to match MUST decide the route.

1. a presentation with that exact slug, for the profile, then for the protocol
2. a rewrite, for the profile, then for the protocol, in ascending priority
3. the profile's default, then the protocol's default

A compaction on a lane proven cold, under a policy naming a route for compactions, MUST resolve to that route instead, as [GCOMP](../gates/compaction.md#choosing-the-target) specifies.

A request carrying no model MUST resolve only through a default, MUST take that route's first candidate with no model, upgrade, alias or fallback, and MUST refuse a request that does carry a model.

## Walking the route

Candidates MUST be walked in order, starting at the lane's pinned candidate where the pin's route fingerprint still matches the route's, and at the first candidate otherwise.

> [!NOTE]
> Recovery of an earlier candidate must not pull a pinned lane back on its own.

For each candidate in turn:

A selector that resolves to no winner MUST end the walk.

> [!NOTE]
> An absent inventory is not evidence for falling back.

An edge crossed to reach a candidate MAY deny it, through fallback being disabled before send, through the billed boundary without consent, or through a warm cache whose loss the edge does not consent to.
Each of those MUST end the walk, because later candidates sit behind the same edge.

Availability MUST be three-valued, distinguishing positive presence, positive absence and no evidence either way.
Only positive absence MUST skip a candidate.

| positively absent |
| --- |
| the provider is unconfigured or disabled |
| a credential is known to be missing or expired |
| the binding cannot represent a required capability |
| the binding is not the request's affinity |
| the candidate is disabled |
| a fresh catalogue positively lacks the model |

A credential that fails only when tried is unknown, and the candidate MUST stay.
A stale or absent catalogue MUST be unknown.

Only an exact-model candidate and an exact presentation MUST be checked against a catalogue listing.
A model from a template, from the requested model or from a default MUST stay an opaque provider string.

> [!NOTE]
> An incomplete listing must not become an allowlist against the upstream.

A candidate whose target has an open circuit MUST be skipped.

## Shaping the model

For the surviving candidate, the model MUST be shaped in this order: the upgrade, then the compaction retarget, then the alias.

The plan MUST keep every stage distinctly, together with whether the client's own spelling decided the baseline.

## When nothing serves

A route with no surviving candidate MUST be answered as having no available route, carrying the trace.
A revision with no default for the protocol MUST be answered as not configured, since no binding was ever a candidate.

Both MUST leave a route-unavailable row, as [ROUTE](../routing.md#when-no-route-is-available) specifies.

## Resolving a single candidate

Inference MUST resolve with traversal, skipping unavailable candidates, and the plan MUST carry a continuation for the path after the request is sent.

Token counting and batch creation MUST examine only the first candidate and MUST NOT cross an edge.

> [!NOTE]
> Neither can be retried or pinned, so a chain has nothing to offer them.

## Circuits

A candidate MAY declare, for a classified failure, how long its target is out of service.

An attempt failing with a class the candidate has a circuit for MUST open one against that target.
An open circuit MUST make its target unavailable until it expires.

A success on the same target MUST clear its circuit.
A catalogue refresh of the binding, or a change of credential, MUST drop it.

> [!NOTE]
> Both are evidence that the failure may no longer hold.

An authentication failure MUST NOT open a circuit where the credential that failed was the client's own, passed through.

