---
id: DCTRL
---

# The control endpoints

Toker answers a reserved namespace itself, for a local client to read its state and edit its policy.
This spec covers the gate those endpoints sit behind, and what each one does.

Everything under the control namespace MUST be answered by Toker and MUST NOT be forwarded.
An unknown path within it MUST be a local 404.

## The gate

The listener binds loopback only, but a page in the operator's browser can reach loopback, so the paths MUST be gated so that a page cannot drive them.

Every endpoint MUST name its operation in a dedicated header, compared case-insensitively against the operation it expects.

A request MUST be refused with a 403 and a plain-text body naming the operation and method required, where it carries no such header, the wrong operation, the wrong method, or, for a request with a body, no JSON content type.

That check MUST run before the body is read.

> [!NOTE]
> A refused request then costs nothing and parses nothing.

A read MUST require the operation alone.
A mutation MUST require the operation, its method and a JSON content type.

Toker MUST NOT emit cross-origin resource sharing headers, and MUST NOT serve a preflight route.

> [!NOTE]
> A browser can send neither the custom header nor a JSON content type cross-origin without first making a preflight request.
> Every route accepting every method means a preflight reaches the handler, fails the operation and method check, and is answered without those headers, so the browser never sends the real request.
> A preflight route would be the hole this closes.

A control path added later MUST take the same two checks, MUST carry an operation of its own, and MUST require a JSON body where it mutates.

## The operations

| operation | what it does |
| --- | --- |
| status | the resolved configuration without secrets, the store's size, uptime, the in-flight count, the active revision and this process's instance identity |
| session | one session's attribution aggregate and its lane view |
| shutdown | drain and exit |
| routing snapshot | everything the routing view shows |
| routing revisions | the revision history, without documents |
| routing draft replace | replace the draft with a complete document |
| routing draft edit | apply typed edit operations to the draft |
| routing validate | compile the draft and report diagnostics |
| routing preview | resolve a described request against the draft |
| routing activate | make the draft the active revision |
| routing reactivate | copy a historical revision into a new active one |
| routing promote | mark a learned model eligible for upgrades |
| routing publication | record a failed or unsupported publication |

What each routing operation means is specified by [RREV](../routing/revisions.md), and the publication handoff by [RPUB](../routing/publication.md#publication-state).

The publication operation MUST record only a failure or a lack of support.

> [!NOTE]
> Only the adapter that wrote rows can state which identities it owns, so no caller may claim another's.

Shutdown MUST name the instance that status reported, and MUST refuse with a conflict where the running instance differs.

> [!NOTE]
> A restart racing another then never stops an instance it did not see.

Shutdown MUST be able to restart the service and nothing else.

Policy data and diagnostics MUST carry no credential, prompt, completion or provider error body.

## The session view

The session endpoint MUST read lanes from the lane table by exact session, and MUST NOT filter the whole table or read the ledger for them.

Each lane MUST report its recorded metadata together with a state worked out at query time.

A ping lane MUST be reported as such, and MUST NOT be called warm or cold.

> [!NOTE]
> Its cache is not the conversation's.

A lane stamped in the future has no idle time, so its state MUST be absent.

A countdown to a lane going cold MUST require a recorded cache lifetime, and MUST be absent where the lifetime was only assumed.

A session with no lanes, or whose lanes were pruned, MUST answer with an empty list.
A field a lane never recorded MUST be absent rather than zero.
