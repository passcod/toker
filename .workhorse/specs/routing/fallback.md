---
id: RFALL
---

# Fallback and commitment

Fallback answers a candidate failing after its request was sent.
This spec covers which failures may fall back, when a response is committed and can no longer be retried, and what a fall does to the lane.

The skips that happen before a request is sent are part of resolution, as [RSOLV](resolver.md#walking-the-route) specifies.

Fallback answers a candidate failing after its request was sent.
A request is a group of attempts, one ledger row each, each charged to the provider that received it.

## Failures that may fall back

Only a failure classified without reading message text, and listed by the crossed edge, MUST be eligible to fall back.

Classification MUST use the provider's own error types and codes, and MUST NOT use the wording of a message.
Status alone MUST classify only authentication, rate limiting and server errors.

An unclassified failure MUST be forwarded as the provider sent it.

A connect failure and a failure after the connection was accepted MUST be classified apart, because the latter leaves open that the provider accepted and billed the request.

A fall MUST re-check the billed boundary and the warm-cache rule.

The quota gate firing is not a candidate failing, and MUST NOT fall through to a billed provider.

## When a response is committed

Nothing is committed until the response head is returned to the client.
Before that, Toker MUST hold the provider's head and a bounded prefix of its body while it decides.

| condition | outcome |
| --- | --- |
| a non-success status within the cap | buffered in full and classified |
| a non-success status beyond the cap | forwarded as it stands, unclassified |
| a compressed body | committed as it arrives, since it cannot be read |
| a stream's first complete non-keepalive frame | committed |
| a stream's first frame carrying an in-band error | a classified failure |
| a stream ending or breaking before any frame, or whose first frame is invalid | a local 502, falling back to nothing |
| a non-streaming success | committed at once |

After commitment there MUST be no retry, because the client holds bytes.

A client disconnecting before commitment MUST cancel the attempt and record it, MUST NOT select another candidate, and MUST NOT pin the lane.

## One decision governs

On a classified, permitted failure, Toker MUST continue the same plan from the failed candidate, choosing the next by the same rules, folding in the crossed edge's consents, and building a fresh transition decision from the lane state captured when the request began.

Only the accepted attempt MUST pin the lane.

> [!NOTE]
> Falling back mid-session therefore pins the replacement for later turns, and a recovered higher-priority candidate does not pull the lane back.

Fallback MUST NOT make a provider's encrypted reasoning portable.
An affinity keeps such a request on its binding, which may leave nothing to fall to.

The retry that renegotiates a refused thinking mode is not fallback.
It is a second send to the same target within one attempt, as [WCANON](../wire/canonical.md#thinking-that-cannot-be-turned-off) specifies.

