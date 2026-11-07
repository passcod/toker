---
id: LATT
---

# Attempts

One request from a client is not always one request to a provider.
This spec covers how sends are grouped, what each is recorded as, and how a reader counts requests without conflating them with sends.

## A logical request and its attempts

A logical request is one thing the client asked for.
An attempt is one send to one provider.

Toker MUST mint a group identifier when it begins handling a request, and MUST give the first send index zero with the reason that it was the initial send.
Every further send MUST stay in that group at the next index, and MUST record the reason it exists.

Two things add an attempt.

| cause | reason recorded |
| --- | --- |
| renegotiating a refused thinking mode | that negotiation |
| falling back to the next candidate | the failure class that caused the fall |

The refusal that prompts a renegotiation MUST be recorded as an error row at its own index, with the provider's own error type, and the retry MUST be the next index.
What that retry may change is specified by [CANON](../canonical.md#thinking-that-cannot-be-turned-off).

A refusal prompting a renegotiation MUST NOT open a health circuit.

> [!NOTE]
> A model asking for a different thinking mode is working.

Which failures may fall back, and what fallback may do, is specified by [RPOL](../policy/overview.md#fallback-and-commitment).
Where the resolver or the lane's transition rules refuse a fall, the failed response MUST be forwarded and the group MUST end there.

A gate notice is decided once for the whole request, and MUST NOT be decided again for a later attempt.

## Accounting is per attempt

Every attempt MUST be recorded as it ends, attached to the provider that received it.

A fallback MUST re-aim the recording context before the next send, to the new provider, route, binding, candidate, cost semantics, policy trace, effective model and any move policy made for it.

> [!NOTE]
> Two rows of one request can therefore name different providers and different models, and a failed attempt on a subscription followed by a successful one on a billed provider is not one cost.

| outcome | row |
| --- | --- |
| the attempt failed | an error row, carrying the failure class, or the provider's error type where there was none |
| the attempt completed | a measurement row |

A failed attempt MUST NOT be priced and MUST carry no usage.
A failed attempt MUST still carry its own response's meters.

> [!NOTE]
> A throttling response is the only evidence of throttling the ledger ever gets.

Only the attempt that completes MUST update the lane, including its routing pin, teach the binding's learning, and clear the health circuits it had tripped.

> [!NOTE]
> A pin is therefore always the provider that really served, and a fallback never pins the lane to the provider that failed.

Each send MUST be noted as recently sent for its binding and model immediately before its transport begins.

> [!NOTE]
> A request that reached a provider may have written a cache even if it then failed.

A client that disconnects after a send and before any response is committed MUST leave an error row for the attempt in flight, recording that the client cancelled.
That row MUST NOT pin the lane.

> [!NOTE]
> A cancelled attempt did not establish what the cache lives on.

### A request with no attempt

A request Toker cannot prepare a body for, on any binding, has no attempt to attach anything to and MUST NOT be part of a group.

It MUST be one error row recording local preparation, carrying the status the client saw and the request's profile, session, requested model and lineage, with the provider, route, binding and attempt fields absent.

A fallback candidate that cannot be prepared after an attempt has already failed MUST NOT write such a row.

> [!NOTE]
> The failed attempt's row stands as the group's final outcome.

## Reading the rows

Nothing MUST sum a group into a single synthetic row.

A group's final outcome is its attempt of greatest index, or its route-unavailable row where it made no attempt.

A reader counting logical requests MUST count distinct groups, and MUST count a row with no group as one.
A reader measuring a quantity of work or money MUST sum the physical rows.

### Counting within a window

A group MUST be counted once, the first time any of its rows falls within the window.

> [!NOTE]
> A group straddling the window's start then counts once rather than as half a request.

A failure a fallback recovered from MUST appear in the error count and MUST NOT appear in the count of failed requests.

> [!NOTE]
> The group's final row is the attempt that completed, so the client saw no failure.
> The error count is the provider-health signal rather than the operator's.

An average over rows MUST take the row as its unit.

> [!NOTE]
> A fallback that produced two measurement rows contributes two rows to a cache hit rate, because each really was a prefix read or written.

### Provider evidence is per attempt

Everything reading a provider's evidence MUST read it per attempt.

The quota fit, the meter reads, the cache-rebuild walk and learning MUST each take the individual row's own snapshot or [served model](../policy/models.md#served-model).

> [!NOTE]
> A total across a group would be a number no provider ever reported.

Which rows predate attempt identity, and what a reader owes a reader when a range reaches back past it, is specified by [LROWS](schema.md).
