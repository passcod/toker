---
id: FRONT
---

# The frontend

Toker offers clients an API on one loopback port, in several protocols.
This spec covers those protocols and the paths that select them.

The profile variants of that API are specified by [FPROF](profiles.md), and the topology a request is routed onto by [ROUTE](../routing/overview.md).

## Protocols

A protocol is a broad wire shape.
Toker MUST serve these three protocols:

| protocol | paths |
| --- | --- |
| `anthropic` | `/v1/messages`, `/v1/messages/count_tokens`, `/v1/messages/batches` and below |
| `openai_chat` | `/v1/chat/completions`, `/v1/models` |
| `openai_responses` | `/v1/responses`, `/v1/models` |

No two protocols claim the same inference path, so one listener serves them all.

Toker MUST determine an inference request's protocol from its path alone.
Where a path is claimed by more than one protocol, as `/v1/models` is, Toker MUST determine the protocol from the frontend profile instead.

The `/_toker/` namespace is not a protocol and MUST NOT leave the proxy.
Its endpoints are specified by [DCTRL](../service/control.md).

