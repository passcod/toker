---
id: RPROV
---

# Routing provenance

Every policy-routed row records how its request was routed.
This spec covers each row's contents and the trace beside it.

Where those rows live is specified by [SLEDGR](../store/ledger.md).

Provenance MUST be content-free and identity-only.

Each row of a policy-routed request MUST carry all of these.

| recorded |
| --- |
| the policy revision |
| the route's identifier and fingerprint |
| the presentation, rewrite or default that matched, with the scope it matched in |
| the facts the resolver was given, as [RSOLV](resolver.md#facts-in) lists them |
| the lane's state as the resolver read it: its cache temperature, its pin's route fingerprint, candidate, binding and effective model, and the upgrade it holds |
| the candidate and the binding |
| the model at each stage of its shaping |
| the [served model](models.md#served-model) |
| the attempt group, the index within it, and the reason for the attempt |

A request the resolver was given no lane for MUST record that it was given none, distinctly from a row written before the lane was recorded.

> [!NOTE]
> The resolver starts its walk at the lane's pinned candidate, so a replay that read a pinned lane as a new one could reach a candidate the request never could.

> [!NOTE]
> Those facts are what let a recorded request be replayed against another revision, as [RREV](revisions.md#preview) specifies, and they are content-free by the same rule the resolver's inputs are.

A row written before a field of that table was recorded MUST read as lacking it, and MUST NOT have it filled in from the rows around it or from the policy.

A routing trace MUST be a bounded structure recording how the route was found, each candidate with its binding and either its availability or a closed reason for skipping it, and each upgrade and alias from and to.

Where a trace would exceed its bound, candidate model spellings MUST be dropped first, and candidate identities and verdicts MUST be kept.

A trace written by an earlier version MUST remain valid history.

A response MUST echo the model the client requested rather than the model that served it.
The ledger MUST keep the requested, effective and served models separately.

Where a provider's response names no model, the served model MUST be absent.
Where a provider's response names no model, the served model MUST NOT be filled in from the effective model.
