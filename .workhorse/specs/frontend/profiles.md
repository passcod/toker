---
id: FPROF
---

# Frontend profiles

A frontend profile is a named variant of the API Toker offers clients, shaped to suit one client.
This spec covers how a request selects one and what the choice governs.

Every route MUST also answer under a `/f/<name>` prefix.
Toker MUST strip the prefix before matching the path, and MUST retain the name for the rest of the request.

A profile name MUST consist of lowercase ASCII letters, digits, `-` and `_`.
A configured notice style whose name falls outside that set MUST be refused when the configuration is read.

A request's profile name MUST select its gate notice style, which [GNOTE](../gates/notices.md) specifies.
A request's profile name MUST be recorded on the request's ledger row.
The name MUST also scope policy, so that a presentation, rewrite or default scoped to one profile applies to that profile alone.

A request that arrives without a prefix MUST be treated as the profile named `unprefixed`.
A prefix naming a profile Toker does not recognise MUST NOT be resolved to a recognised one.

