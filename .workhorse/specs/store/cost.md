---
id: SCOST
---

# Meters and cost

A provider reports what a rate limit window has left, and what its answer cost.
This spec covers which providers report each, and how the kinds of cost are kept apart.

A meter is a provider's own report of a rate limit window's remaining allowance.

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

The quota gate is specified by [GQUOTA](../gates/quota.md), and the cold gate by [GCOLD](../gates/cold.md).

