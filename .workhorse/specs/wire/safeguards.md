---
id: WSAFE
---

# Server safeguards

A client may ask the provider to review the actions its model proposes as part of the same Messages turn, and the provider returns its verdicts inside that turn's response.
This spec covers how Toker carries that ask and its results, which bindings receive it, what Toker does when an upstream refuses it, and what Toker records about it.

Toker carries the review and never takes part in it.

## The ask

A request asks for server safeguards when its body carries the `safeguards` field.
An `anthropic-beta` value announcing that field, arriving without it, MUST NOT be read as an ask.

Toker MUST treat the field's value as opaque, carrying it as a provider extension as [WCANON](canonical.md#provider-extensions) specifies.
Recognising the field by name MUST serve only to associate a request with the capability, and MUST NOT lead Toker to read, check or rewrite what it holds.

> [!NOTE]
> The field's contents and its beta spelling belong to the provider and change without notice.
> Keeping recognition to the name lets a newer client shape reach a supporting upstream without Toker being taught it.

## Which bindings receive it

A binding's support for server safeguards is a capability as [ROUTE](../routing/overview.md#bindings) specifies.
The Anthropic subscription and Anthropic API Messages bindings MUST declare it supported.
Every other Messages binding MUST declare it unknown until its request and response shapes have been verified against the upstream.
Every binding of another protocol MUST declare it unsupported.

A binding's effective support is its declared support, narrowed to unsupported where a [rejection mark](#a-rejection-mark) is held against it.

The `safeguards` field and the `anthropic-beta` value announcing it MUST travel together.
A binding whose effective support is supported or unknown MUST receive both as the client sent them.
A binding whose effective support is unsupported MUST receive neither, with the client's other `anthropic-beta` values left in place.

> [!NOTE]
> An upstream that receives the beta without its field, or the field without its beta, answers with a 400 rather than turning the feature off.
> Forwarding both to an unverified binding lets an upstream that does support it return verdicts, and the client falls back to its own review where none come back.

A translated route MUST NOT carry the ask onto another protocol, and MUST report its omission as a translation loss as [WTRANS](translation.md#opaque-replay) specifies.

## The results

A response's `safeguard_results`, in a complete response or in a stream, MUST reach a Messages client unchanged and in its original position, as [WCANON](canonical.md#provider-extensions) specifies.

The tool-use identifiers a verdict refers to MUST reach the client exactly as the provider sent them.

Toker MUST NOT synthesise a verdict, whether to allow, to deny, or to say that no review took place.
A response that carries no results MUST reach the client without any.

> [!NOTE]
> The client decides what to do with a missing verdict, from denying the action to falling back to its own review for the rest of the session.
> A verdict Toker made up would decide an action no reviewer saw.

## A rejection

Where a binding whose declared support is unknown refuses a request with a 400 whose error names the `safeguards` field, Toker MUST send the request once more without the field and without the beta announcing it.
Toker MUST forward whatever the second send returns.

Both sends MUST be recorded as separate ledger rows within one attempt group, and the refusal's meters MUST feed the quota gate.

A refusal from a binding whose declared support is supported MUST be forwarded as the provider sent it, and MUST NOT be retried.

> [!NOTE]
> A supporting upstream refusing the ask is saying something about the session, such as a credential not yet enrolled, which the client already knows how to answer.

## A rejection mark

A rejection as described above MUST set a mark against the binding that refused.
The mark MUST be held in the store and MUST survive a restart.

The mark MUST NOT change the binding's declared support, and MUST NOT be set against a binding whose declared support is supported.

A catalogue refresh of the binding, or a change of its credential, MUST clear the mark.
The operator MUST be able to clear it from the dashboard.

> [!NOTE]
> Each clearing lets the next ask probe the upstream again, at the cost of at most one refused send.

## Routing

A route MAY honour the ask, making server safeguards a requirement on its candidates for a request that asks, as [RSOLV](../routing/resolver.md#walking-the-route) specifies.

A route that does not honour the ask MUST route the request as though it had not asked.

> [!NOTE]
> The provider reviews actions inside the model's own turn, so honouring the ask moves the whole turn to a supporting binding.
> Toker cannot send the review to one provider and the turn to another.

Toker MUST NOT write a client setting that turns the ask off.

> [!NOTE]
> The client offers such a setting for a route that never supports the ask, and whether a route can is a property of the active policy, which may change after the setting is written.

## What is recorded

The ledger records whether the request asked, the serving binding's effective support, whether results were seen and passed on, and what honouring the ask did to the route, as [SLEDGR](../store/ledger.md#fields-that-need-care) specifies.

Toker MUST NOT record, log or display the contents of the `safeguards` field or of any result, nor the action a result refers to.

## Borrowed terms

- [`safeguards`](https://code.claude.com/docs/en/auto-mode-classifier-billing#make-the-session-eligible): Messages request field by which Claude Code asks the provider to review the actions in the turn. Its announcing `anthropic-beta` value has been observed as `dangerous-tool-use-2026-09-03`.
- [`safeguard_results`](https://code.claude.com/docs/en/auto-mode-classifier-billing#make-the-session-eligible): Messages response field carrying the provider's verdicts on the actions in the turn.
