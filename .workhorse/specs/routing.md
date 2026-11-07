---
id: ROUTE
---

# Routing

Toker serves several protocols on one loopback port and forwards each request to one of several configured providers.
This spec covers the paths, the topology a request is resolved onto, what each provider does to a request's credentials, and how a request with nowhere to go is answered.
Which binding and model a request takes is decided by [RPOL](policy/overview.md).
How an inference request's body is rendered is specified by [CANON](canonical.md).

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
Its endpoints are specified by [DCTRL](service/control.md).

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
Configuration is specified by [DCONF](service/config.md).

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

## Frontend profiles

A frontend profile is a named variant of the API Toker offers clients, shaped to suit one client.

Every route MUST also answer under a `/f/<name>` prefix.
Toker MUST strip the prefix before matching the path, and MUST retain the name for the rest of the request.

A profile name MUST consist of lowercase ASCII letters, digits, `-` and `_`.
A configured notice style whose name falls outside that set MUST be refused when the configuration is read.

A request's profile name MUST select its gate notice style, which [GNOTE](gates/notices.md) specifies, and MUST be recorded on the request's ledger row.
The name MUST also scope policy, so that a presentation, rewrite or default scoped to one profile applies to that profile alone.

A request that arrives without a prefix MUST be treated as the profile named `unprefixed`.
A prefix naming a profile Toker does not recognise MUST NOT be resolved to a recognised one.

## Resolving a route

A route is a binding a request has been resolved onto, together with the model it will be sent as.

A resolved route MUST keep the provider, the binding, the model the client requested and the model that will be sent distinct from one another.
A ledger row MUST record both model spellings.

Where a model identifier carries a provider prefix that policy recognises, the request MUST resolve to that provider.
A prefixed identifier naming a disabled provider MUST be answered as having no available route, and MUST NOT be sent to a default binding with the prefix still attached.
A prefix policy does not recognise MUST be treated as part of the provider's own model identifier and MUST be left intact.

An Anthropic path the route table does not match MUST be forwarded to the binding the Messages default route names first, without a model being resolved, rewritten or upgraded.

## When no route is available

Each of these MUST produce an error in the shape of the protocol the client spoke, and MUST write a route-unavailable row to the ledger.

| condition | status | row |
| --- | --- | --- |
| no binding is presented, rewritten or defaulted to for this protocol | a not-configured error carrying the `x-toker-not-configured` header | carries no route |
| the lane's pin refuses the move to the resolved route | 502 | names the route refused |
| every candidate is unavailable, through an absent provider block, a missing credential or an open circuit | 503 | carries the trace of what was tried |

A request answered in any of these ways MUST NOT reach an upstream.

Lane pins are specified by [LLANE](ledger/lanes.md).

## Credentials

A request that brings its own credential for the provider it reaches keeps it.
A credential meant for one provider MUST be removed before the request reaches a different one.
Toker MUST NOT log a credential value, and MUST NOT write one to the store.
Toker MUST read request headers by name, and MUST NOT capture them wholesale.

| provider | what Toker does |
| --- | --- |
| `anthropic_sub` | passes a Claude bearer through on a native Messages request. For any other protocol it replaces the client's bearer with a Toker-held OAuth token, or failing that an unexpired Claude login. |
| `anthropic_api` | injects `x-api-key` only when the request carries neither `x-api-key` nor `authorization`. |
| `openrouter` | drops `x-api-key` always, and drops a bearer whenever Toker holds an OpenRouter key or the bearer is Anthropic's, then injects the stored key. With no stored key, a client's own OpenRouter key passes through. |
| `openai_api` | drops Anthropic and OpenRouter credentials, then injects the configured bearer only when the request carries no native bearer. An unrecognised bearer on its native Chat wire passes through. |
| `codex_sub` | strips whatever `authorization` the request carried and signs the upstream itself. |

> [!NOTE]
> A client's bearer on a Messages request is the subscription's, so forwarding it to any upstream other than Anthropic would hand a third party the subscription credential.
> Claude Code's own settings carry a placeholder bearer, so the presence of one proves nothing about its origin.

Where `anthropic_sub` serves a request that did not bring a usable Claude bearer, Toker MUST resolve its own token from, in order: the configured environment variable, the keyring entry, a literal in the configuration file, and the Claude login file at its configured path.
Toker MUST read that login file once per request rather than caching it, so that a refreshed or revoked login takes effect without a restart.
Toker MUST NOT send a token it can see has expired.

## Meters and cost

A meter is a provider's own report of what a rate limit window has left.

Toker MUST feed meters from every response a meter-bearing provider returns, including errors and requests Toker made for itself.

| provider | meters | cost kind |
| --- | --- | --- |
| `anthropic_sub` | the Anthropic rate limit headers | plan equivalent |
| `anthropic_api` | none | estimated |
| `openrouter` | none | billed, from the provider's reported cost |
| `openai_api` | none | estimated |
| `codex_sub` | the codex headers, recorded but not gated | none |

The Anthropic rate limit headers on `anthropic_sub` MUST be the quota gate's only source.
`anthropic_api` returns request-rate headers which are not quota meters, and they MUST NOT overwrite the gate's snapshot.

A model, service tier or token category Toker has no verified price for MUST leave the row's cost unrecorded rather than record a zero.

The quota gate is specified by [GQUOTA](gates/quota.md), and the cold gate by [GCOLD](gates/cold.md).

## Model listings

A `/v1/models` response MUST be projected from the active policy and the offer inventory, and MUST NOT be a provider's listing passed through.

Each model listed MUST have a complete route to the profile that asked for it.
Where no usable catalogue has been loaded, or a presented offer is absent from the inventory, the endpoint MUST answer 503 rather than an empty list.

What each protocol's listing contains is specified by [RPUB](policy/publication.md).

## Administrative paths

Token counting and batch creation MUST resolve only their first candidate, and MUST require a binding that declares the operation.

Toker MUST retain the client's bytes on these requests, and MUST replace only the model at the positions each operation documents.
A batch whose requests resolve to more than one binding MUST be refused with a 400.

Batch reads, batch cancellation and unmatched administrative paths MUST stream through without inference observation.
