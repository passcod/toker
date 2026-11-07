---
id: ROUTE
---

# Routing

Toker serves several protocols on one loopback port and forwards each request to one of several configured providers.
This spec covers the topology a request is resolved onto: the protocols it may speak, the providers it may reach, and the bindings between them.

How a request names where it is going is specified by [ADDR](addressing.md), and which binding and model it takes by [RSOLV](../policy/resolver.md).

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

## Providers

A provider is a configured upstream account, its credential and its endpoint.

| provider | upstream |
| --- | --- |
| `anthropic_sub` | Anthropic, on a Claude subscription |
| `anthropic_api` | Anthropic, on an API key |
| `openrouter` | OpenRouter |
| `openai_api` | OpenAI directly |
| `codex_sub` | ChatGPT, on a codex subscription |

A provider is enabled by the presence of its `[providers.<name>]` block in the configuration file, and is disabled by its absence.
Configuration is specified by [DCONF](../service/config.md).

A backend adapter renders a canonical request onto a provider's wire and interprets what comes back.
Several providers MAY share one adapter, and one provider MAY be served by several adapters.

## Dialects

A dialect is a provider's concrete rendering and interpretation rules within a protocol.

| dialect | protocol |
| --- | --- |
| `anthropic_messages` | `anthropic` |
| `openrouter_messages` | `anthropic` |
| `openai_chat_completions` | `openai_chat` |
| `openrouter_chat_completions` | `openai_chat` |
| `codex_responses` | `openai_responses` |
| `openrouter_responses` | `openai_responses` |

A dialect MUST NOT be inferred from a protocol.
A renderer MUST select its rendering and interpretation rules from the binding's dialect alone.

## Bindings

A binding declares that one provider serves one protocol in one dialect.
A binding MUST carry:

| member | meaning |
| --- | --- |
| provider | the provider this binding reaches |
| protocol | the protocol a client speaks to reach it |
| dialect | the rendering and interpretation rules to use |
| cost class | which of the cost kinds its rows carry |
| cache domain | the prompt cache its requests share, where it has one |
| capabilities | the semantics it can represent, each as supported, unsupported or unknown |
| administrative operations | whether it serves token counting, and whether it serves batches |

A binding MUST be a declaration verified against the provider, and MUST NOT be inferred from an endpoint's spelling.
The set of bindings is fixed topology and MUST NOT depend on configuration or on policy.

A capability MUST be read from the binding that will serve the request.
A frontend MUST NOT decide at parse time which semantics a provider will refuse.

