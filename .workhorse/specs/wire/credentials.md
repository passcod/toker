---
id: WCRED
---

# Credentials on the wire

A request arrives with whatever credential its client holds, and leaves with whatever the provider it reaches requires.
This spec covers which of those Toker keeps, removes and adds.

Where a credential may be stored is specified by [DCONF](../service/config.md#credentials), and the property this upholds by [SEC](../security.md#a-credential-reaches-only-the-party-it-was-meant-for).

## Keeping, removing and adding

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

Where `anthropic_sub` serves a request that did not bring a usable Claude bearer, Toker MUST resolve its own token from the first of these that is set.

1. the configured environment variable
2. the keyring entry
3. a literal in the store
4. the Claude login file, found where [DCONF](../service/config.md#in-the-store) specifies
Toker MUST read that login file once per request rather than caching it, so that a refreshed or revoked login takes effect without a restart.
Toker MUST NOT send a token it can see has expired.

## Which account a credential names

The credential a request leaves with names the account it is served on, which [ACCT](../store/accounts.md) specifies.

Toker MUST resolve that account from the credential it sends upstream, and MUST NOT resolve it from the one the client arrived with where the two differ.

> [!NOTE]
> A request that brought no usable credential is served on whichever of the sources above answered, so the arriving request says nothing about whose window it spends.

A credential that resolves to no account MUST NOT prevent the request being sent.

