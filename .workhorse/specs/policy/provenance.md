---
id: RPROV
---

# Routing provenance

Every policy-routed row records how its request was routed.
This spec covers what each row carries and the trace beside it.

Where those rows live is specified by [SLEDGR](../store/ledger.md).

Provenance MUST be content-free and identity-only.

Each row of a policy-routed request MUST carry the revision, the route's identifier and fingerprint, the presentation or rewrite that matched, the candidate and binding, the model at each stage of its shaping, the [served model](models.md#served-model), and its attempt group, index and reason.

A routing trace MUST be a bounded structure recording how the route was found, each candidate with its binding and either its availability or a closed reason for skipping it, and each upgrade and alias from and to.

Where a trace would exceed its bound, candidate model spellings MUST be dropped first, and candidate identities and verdicts MUST be kept.

A trace written by an earlier version MUST remain valid history.

A response MUST echo the model the client requested rather than the model that served it.
The ledger MUST keep the requested, effective and served models separately.

Where a provider's response names no model, the served model MUST be absent, and MUST NOT be filled in from the effective model.
