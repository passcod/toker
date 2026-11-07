---
id: ADDR
---

# Addressing a request

A request names where it is going by the profile prefix on its path and by the model it asks for.
This spec covers both, the paths no route table matches, and how a request that cannot be placed is answered.

The topology it is placed onto is specified by [ROUTE](overview.md).

## Frontend profiles

A frontend profile is a named variant of the API Toker offers clients, shaped to suit one client.

Every route MUST also answer under a `/f/<name>` prefix.
Toker MUST strip the prefix before matching the path, and MUST retain the name for the rest of the request.

A profile name MUST consist of lowercase ASCII letters, digits, `-` and `_`.
A configured notice style whose name falls outside that set MUST be refused when the configuration is read.

A request's profile name MUST select its gate notice style, which [GNOTE](../gates/notices.md) specifies, and MUST be recorded on the request's ledger row.
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

Lane pins are specified by [SLANE](../store/lanes.md).

