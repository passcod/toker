---
id: SEC
---

# Security properties

Toker sits between a client and a provider, holding credentials and seeing every prompt that passes.
This spec states the properties that follow from that position and names the mechanism upholding each.
The requirements themselves belong to the specs named here.

## No prompt or completion content is recorded

Toker MUST record only counts, lengths, digests, and whether a marker string it defined in advance matched.

Toker MUST parse a response body for its reported usage and MUST discard the rest.

A detection marker MUST be a string chosen in advance, and only whether it matched MUST be recorded, as [GCOMP](gates/compaction.md#recording-an-unmatched-wording) specifies.

> [!NOTE]
> Whether a known string matched is a derived fact, as a hash is.
> Recording where it sat and which roles surrounded it carries positions and flags, never text.

A label a display reads from a client's own files MUST be read when the display renders and MUST NOT reach the store.

An import MUST report the names of fields it did not map and MUST NOT report their values, as [SLEDGR](store/ledger.md#imported-rows) specifies.

A routing diagnostic's path MUST be built only from static field names and bounded indices, as [RDOC](policy/document.md#validation) specifies, so that a request or a model string cannot appear in one.

An export MUST carry nothing that is not a ledger column, as [UREAD](cli/readers.md#export) specifies.

Policy data and diagnostics MUST carry no prompt, completion or provider error body, as [DCTRL](service/control.md#the-operations) specifies.

## A credential is never logged, stored or displayed

Toker MUST NOT write a credential's value to a log, to the store, or to any output.

Toker MUST report which source supplies a credential and MUST NOT report the value, as [UCLI](cli/commands.md#status) specifies.

A debug rendering of a credential-bearing value MUST redact it, as [DCONF](service/config.md#credentials) specifies.

Toker MUST read request headers by name and MUST NOT capture them wholesale, as [ROUTE](routing/credentials.md) specifies.

> [!NOTE]
> Headers carry the client's bearer, so a wholesale capture puts it wherever the capture goes.

A credential Toker holds MUST live in the operating system's keyring, in an environment variable, or as a literal in a file readable only by its owner.

## A credential reaches only the party it was meant for

A credential a client supplied for one provider MUST be removed before the request reaches a different one, as [ROUTE](routing/credentials.md) specifies.

A provider that signs its own requests MUST strip whatever the client sent first.

> [!NOTE]
> A client's bearer on one protocol is an arrangement between that client and Toker, and forwarding it to a third party would leak it.

Where Toker borrows a passing credential for a request of its own, that request MUST be bounded in purpose and frequency, and the credential MUST NOT be stored, cached or logged.

## The proxy is not reachable from the network

Toker MUST bind loopback only, as [DLIFE](service/lifecycle.md#binding) specifies.

> [!NOTE]
> Toker signs requests with the credentials it holds, so anything that reaches it is served on the operator's own account without presenting a credential of its own.

## A web page cannot drive the control surface

A page in the operator's browser can reach loopback, so every control endpoint MUST demand a custom header naming its operation, and every mutation MUST additionally demand its method and a JSON content type.

Toker MUST NOT emit cross-origin resource sharing headers and MUST NOT serve a preflight route, as [DCTRL](service/control.md#the-gate) specifies.

> [!NOTE]
> A browser cannot send either requirement cross-origin without a preflight it will not get answered, so the real request is never sent.

A control path added later MUST take the same checks.

## Accounting cannot stop a session

Observing and recording a request MUST NOT be able to fail it, as [SATT](store/attempts.md) and [GQUOTA](gates/quota.md#firing) specify.

Only a gate MUST be able to stop a session, and every gate decision MUST be taken at one place.
