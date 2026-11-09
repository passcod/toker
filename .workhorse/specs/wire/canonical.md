---
id: WCANON
---

# Canonical rendering

Toker parses every inference request into a canonical request and renders the outgoing body from it, rather than forwarding the client's bytes.
This spec covers the guarantees rendering makes, which transformations may change a request's meaning, how a body that cannot cross the canonical boundary is answered, and which response headers reach the client.
Administrative paths keep the client's bytes instead, as [ROUTE](../routing/overview.md) specifies.

## Rendering

Toker MUST parse each inference request into a canonical request.
Toker MUST render the outgoing body from that canonical request.
Toker MUST render the body even where the client's protocol and the binding's protocol are the same.

Rendering MUST be deterministic.
A renderer MUST read no clock and no stored state.
A renderer MUST take the model identifier and the prompt cache key as inputs.

Because rendering is deterministic, a conversation that only appends reproduces its earlier bytes exactly.
Toker MUST preserve that property: where the conversation up to a point has not changed, the rendered body up to that point MUST be byte-identical to the previous turn's.

> [!NOTE]
> Every provider Toker serves keys its prompt cache on an exact byte prefix, charging a cached read only where the bytes it holds match.
> A renderer that reorders a map or formats a number differently between turns then silently converts every cached read into a full one.

## Licensed transformations

These are the only transformations that may change a canonical request's meaning before it is rendered.

| transformation | what it changes | when |
| --- | --- | --- |
| strip release markers | removes the marker strings Toker defines | on every Messages request, whatever the gate's state |
| set model | the candidate's model identifier | when policy selects a model, having licensed the cache transition |
| retarget compaction | model, cache breakpoints and mid-conversation system messages, together or not at all | on a cold compaction only |
| strip message effort | removes the reasoning effort a mid-conversation system message carries | where the binding's model refuses it |
| thinking between tools | turns a disabled thinking mode into the between-tools mode | on the single retry that follows an upstream refusal naming it |
| apply model alias | the candidate's model identifier | where policy compiled an alias for it |

Changing any other position of the body is a defect.

Release markers are specified by [GQUOTA](../gates/quota.md), compaction by [GCOMP](../gates/compaction.md), and model selection by [RSOLV](../routing/resolver.md#shaping-the-model).

Stripping release markers MUST happen on every Messages request regardless of whether the gate is enabled.

> [!NOTE]
> The marker strings are a published interface, and stripping them conditionally would change the cached prefix of every conversation that carries one.

Stripping a message's reasoning effort MUST leave any other content of that message intact.
Stripping a message's reasoning effort MUST remove the message only where effort was all it carried.

> [!NOTE]
> A client replays a mid-conversation system message on every turn, so a model that refuses the effort it carries refuses every remaining turn of the session.

### Dropping cache breakpoints

Removing a request's cache breakpoints requires a licence beyond the transformation that rewrites the body, and there are two.

Toker MAY drop cache breakpoints where the model has changed, since a cache identity includes the model sent, as [SLANE](../store/lanes.md#cache-identity) specifies, leaving none for the new model to lose.
Toker MAY drop cache breakpoints where the caller has asserted that the request's cache is already cold.
Where neither holds, retargeting MUST leave the breakpoints in place.

> [!NOTE]
> On a warm lane those breakpoints earn the free read, so dropping them without one of these two licences converts a cached read into a full one.

Retargeting a compaction MUST change its model, its cache breakpoints and its mid-conversation system messages together, or change none of them.

> [!NOTE]
> A compaction's body is never replayed and the client's transcript is untouched.
> Neither holds for an ordinary request, so this transformation does not generalise to one.

## Thinking that cannot be turned off

Where a Messages request that asked for thinking to be disabled is refused with a 400 whose message names the between-tools mode, Toker MUST send the request once more with that mode in place.
Toker MUST forward whatever the second send returns.

Toker MUST NOT apply that mode before a refusal.
Toker MUST judge each turn afresh, leaving the client's transcript untouched, so that the next turn sends the disabled mode again.

Both sends MUST be recorded as separate ledger rows within one attempt group, and the refusal's meters MUST feed the quota gate.

This MUST key on the upstream's answer rather than on the binding or the model.

> [!NOTE]
> Whether the between-tools mode is accepted wherever the disabled mode still is has not been established, so the refusal is the only evidence that a model wants it.

A request that asked for thinking to be disabled MUST have visible thinking, thinking signatures, redacted thinking and provider-opaque reasoning removed from its response before the client's renderer runs.
This MUST hold whether or not the binding can express the disabled mode.
Accounting MUST still record the provider's reported reasoning tokens.

## The canonical boundary

A request Toker cannot parse MUST be refused locally with a typed 400, and MUST NOT reach an upstream.
A provider response that cannot cross the canonical boundary, through compression, size or malformation, MUST be answered with a typed 502, and MUST NOT be forwarded as though it had crossed.

A refusal of a request that asked to stream MUST be delivered in that protocol's own stream error form.
A body that is not JSON cannot carry a stream preference, and MUST be refused with the protocol's plain error instead.

A refusal before any attempt MUST leave an error row naming local preparation, carrying no attempt, provider, route or binding.
Attempts and their rows are specified by [SATT](../store/attempts.md).

A malformed body MUST retain a wire-shaped view for the quota decision and for presenting the error.
A malformed body MUST NOT have that view used for inference or for extracting the request's shape.

Toker MUST observe a provider's response bytes before translating them, so that reported usage, billed cost and the serving provider remain the provider's own evidence.

## Response headers

A response Toker forwards from a provider MUST carry the provider's response headers, except those describing the connection and those describing the encoding or framing of a body Toker re-frames.

Where a provider's response carries `anthropic-ratelimit-unified-overage-utilization` and no `anthropic-ratelimit-unified-overage-period-monthly-utilization`, Toker MUST add the latter to the forwarded response with the former's value, byte for byte.
Toker MUST NOT replace an `anthropic-ratelimit-unified-overage-period-monthly-utilization` the provider sent.
This MUST hold on every forwarded response, whatever its path, status, streaming mode or client protocol.
Toker MUST NOT add either header to a response it answers itself.

The meters Toker records MUST be read from the provider's own headers, without the added one, as [SLEDGR](../store/ledger.md#fields-that-need-care) requires of a meter snapshot.

> [!NOTE]
> Claude Code reports rate limit state to its host from these headers.
> While overage is drawn, its report names the exhausted plan window and leaves out the overage utilisation, but it passes the monthly overage period utilisation through in every state.
> Copying the one into the other gives a host the share of the month's overage allowance spent on every turn.
> Claude Code marks that field internal, so a later version may stop reading it, and the copy then does nothing.
>
> A response Toker answers itself has no provider reading behind it, and a stored one would present a past figure as current.

## Output limits

The Messages protocol requires an output limit, which the Chat and Responses protocols leave optional.
Where a request rendered onto a Messages binding carries no limit, Toker MUST take a positive limit the caller supplied, and failing that the model's declared maximum from the fetched catalogue.
Where neither is available, Toker MUST refuse the request locally with a 400 rather than supply a limit of its own.
