---
id: TOK
---

# Toker

Toker is a local proxy and ledger for LLM traffic.

## Requirements notation

The key words MUST, MUST NOT, REQUIRED, SHALL, SHALL NOT, SHOULD, SHOULD NOT, RECOMMENDED, NOT RECOMMENDED, MAY and OPTIONAL in Toker's specifications are to be interpreted as described in BCP 14 ([RFC 2119](https://www.rfc-editor.org/rfc/rfc2119), [RFC 8174](https://www.rfc-editor.org/rfc/rfc8174)) when, and only when, they appear in all capitals, as here.

## External documents

| document | what it covers |
| --- | --- |
| [Anthropic Messages API](https://docs.anthropic.com/en/api/messages) | The Messages protocol, its paths and its streaming events. |
| [Anthropic rate limit headers](https://docs.anthropic.com/en/api/rate-limits) | The meters a subscription reports, and the representative claim. |
| [OpenAI Chat Completions API](https://platform.openai.com/docs/api-reference/chat) | The Chat protocol and its streaming chunks. |
| [OpenAI Responses API](https://platform.openai.com/docs/api-reference/responses) | The Responses protocol and its event stream. |
| [OpenRouter API](https://openrouter.ai/docs/api-reference/overview) | OpenRouter's dialects of Messages, Chat and Responses, and its reported cost. |
| [HTML Living Standard, server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html) | The `text/event-stream` framing every streamed protocol uses. |

## Terminology

### Client

A program that sends inference requests to Toker rather than to a provider.

### Frontend profile

A named variant of the API Toker offers clients, selected by a request's `/f/<name>` prefix, shaped to suit one client.
Defined in [ROUTE](routing/addressing.md#frontend-profiles).

### Protocol

A broad wire shape a request and its response take.
Defined in [ROUTE](routing/overview.md#protocols).

### Dialect

A provider's concrete rendering and interpretation rules within a protocol.
Defined in [ROUTE](routing/overview.md#dialects).

### Provider

A configured upstream account, its credential and its endpoint.
Defined in [ROUTE](routing/overview.md#providers).

### Backend adapter

The implementation that renders a request onto a provider's wire and interprets what comes back.
Defined in [ROUTE](routing/overview.md#providers).

### Binding

A declaration that a provider serves one protocol in one dialect, carrying the capabilities and cost class of that pairing.
Defined in [ROUTE](routing/overview.md#bindings).

### Route

A binding a request has been resolved onto, together with the model it will be sent as.
Defined in [ROUTE](routing/addressing.md#resolving-a-route).

### Canonical request

The representation a request is parsed into, from which the outgoing body is rendered.
Defined in [CANON](routing/canonical.md).

### Store

The database Toker keeps its state in, holding the ledger alongside the lanes, the policy revisions, the learned models, the allowances, the ping runs and the meter snapshots.
Defined in [STORE](store/overview.md).

### Ledger

The append-only record of requests within the store, one row per attempt or proxy decision.
Defined in [SLEDGR](store/ledger.md).

### Offer

A model a provider serves, as a provider identifier paired with what Toker knows of its window and pricing.
Defined in [RPUB](policy/publication.md).

### Inventory

The set of offers Toker holds, joined across every provider catalogue it has fetched.
Defined in [RPUB](policy/publication.md).
