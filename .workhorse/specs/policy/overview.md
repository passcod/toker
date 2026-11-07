---
id: RPOL
---

# The routing policy

Every inference request is routed by the active revision of the routing policy, a versioned document held in the ledger.
This spec covers the document, how a revision is drafted and activated, and how a request is resolved against one.

There MUST be no other router.
A change to the policy MUST reach live traffic by activation, without restarting the service.

Which bytes a route may alter is specified by [CANON](../canonical.md), what a lane pin is by [LLANE](../ledger/lanes.md), and how models are learned and upgraded by [RMODEL](models.md).

## The document

A policy is one document holding eight collections.

| collection | what it holds |
| --- | --- |
| defaults | one route per frontend scope, for a name nothing else matched |
| presentations | the models a profile is shown |
| routes | named ordered chains of candidates, with the edge granting each fall to the next |
| rewrites | matchers on the requested model that pick a route, for names never presented |
| classifiers | per binding, how a model identifier splits into a family and a version |
| upgrades | which newer version of a candidate's family a request may move to |
| compaction | where a cold compaction goes |
| description | free text |

A frontend scope is a protocol, or a frontend profile together with a protocol.
A profile entry MUST take precedence over a protocol entry of the same kind.

A binding is named by its provider and dialect together.
Renaming one MUST be treated as a migration rather than a change of display.

### Parsing

The parser MUST refuse unknown fields, duplicate keys and non-integer numbers before typed parsing.
Identifiers MUST be bounded in length and drawn from a restricted character set, and the document, its nesting depth, its collections and its strings MUST each be bounded in size.

Because unknown fields are refused, adding even an optional field changes which documents are accepted, so the format MUST carry a version.
A document using a feature of a later version than it declares MUST be refused.

### Fingerprints

A document's fingerprint MUST be a cryptographic hash over a version-specific domain tag and a canonical encoding, with object keys sorted and arrays in their semantic order.
Each format version MUST have its own domain tag, so that one fingerprint never means two things.

A route fingerprint MUST cover one route's semantics alone: its candidates, edges, alias policies, health circuits, upgrade policies, classifiers and lane transition policy.
Labels, descriptions and the offer an inventory happens to select MUST NOT take part.

> [!NOTE]
> A catalogue refresh must not change a route fingerprint, because lane pins carry it and every pinned lane would otherwise be invalidated whenever a provider published a model.

### Presentations

A presentation names a slug a profile is shown, and sources it as one exact offer on one binding, as a selector over a binding's offers, or as a virtual model backed by a route.

A selector MUST be expanded when the policy is compiled, so that a selection tracking the newest matching offer follows the inventory as catalogues refresh.
An identifier a classifier cannot parse is not an older version, and MUST NOT be selected where the ordering is by classified version.

A slug MUST be unique within its scope.
A slug equal to a native model identifier reachable from that profile MUST be refused unless the document consents to shadowing it.

> [!NOTE]
> Without that consent a presented name silently hides a real model of the same name.

## Validation

Validation compiles a draft against one captured inventory and returns diagnostics, each carrying a code, a severity, the record's identifier and a path.

A diagnostic's path MUST be built only from static field names and bounded indices, so that a request or a model string can never appear in one.

Validation MUST check that every reference resolves, that route arguments match declared parameters exactly, that a regex capture's source exists, that each candidate's binding is routable from the scope it is used in, that classifier and upgrade bindings agree, that rewrite priorities are unique within a scope, that defaults are unique within a scope, that required capabilities are within the binding's declared capabilities, and that shadowing is consented to.

An error MUST block activation.

Provider configuration and catalogue presence are evidence rather than validity.
An unconfigured binding, an offer absent from the inventory and an unresolved selector MUST be reported without blocking.

> [!NOTE]
> A fallback chain is legitimately prepared before the provider serving it exists.

### Edge rules

An edge says whether a fall to the next candidate is allowed before the request is sent, which classified failures allow it once the request has been sent, and which consequences it consents to.

A fall from a subscription binding to one that is billed, or whose cost is unknown, MUST require the edge's consent to crossing that boundary.
An edge listing an ambiguous transport failure among those it falls on MUST require consent to that separately.

> [!NOTE]
> An ambiguous transport failure leaves open that the provider accepted and billed the request, so falling to another provider may pay twice.

An edge crossing a cache identity MUST NOT declare that it preserves a warm cache.

## Lifecycle

### Draft

There MUST be exactly one draft, held in the ledger so that two editors share it.

Every write to the draft MUST carry the generation it read and MUST be applied by compare-and-swap.
A stale write MUST change nothing and MUST return the current state.

A draft MUST be replaced whole, or edited by a bounded atomic list of typed operations addressing records by identifier.
An edit MUST NOT be able to address an incidental path within the document.
The whole result MUST be validated before it is stored.

Renaming a record MUST be expressed as a removal and an addition.

### Preview

Preview resolves an invented request against the draft using the same resolver inference uses, and returns the outcome and the trace.

Preview MUST use the snapshot's availability without per-request credential evidence and without open circuits.

> [!NOTE]
> It therefore shows what the policy says rather than what one request's headers would make of it.

### Activation

Activation MUST take the draft generation it was shown, compile the draft, and then in one transaction insert a new revision, make it active, replace the draft with the activated document at the next generation, and hand every publication adapter the new desired revision and inventory generation.

The runtime MUST install the compiled policy only after that transaction commits, so that a request in flight keeps the snapshot it captured.

A stale draft generation MUST fail the transaction, as MUST an active revision that moved.

An activation's scope decides what becomes of lanes already pinned to an earlier revision.

| scope | effect |
| --- | --- |
| new lanes | a pinned lane keeps resolving under the revision it is pinned to |
| new and cold lanes | a lane whose cache is provably cold moves to the new revision; the rest stay |
| immediate | the next request resolves under the new revision |

An immediate activation that would lose a warm cache MUST require explicit consent, and MUST be refused while any lane is warm or not provably cold and that consent is absent.
Without consent, a lane that became warm after the check MUST stay on its pinned revision.

A revision a lane is pinned to MUST be kept and compiled on demand against current evidence.

### History and reactivation

A revision MUST never be edited.

The active pointer MUST never move backwards.
Reactivating an earlier revision MUST copy it into a new revision rather than move the pointer, and a pin to a revision newer than the active one MUST be a stale-revision error.

> [!NOTE]
> A lane pinned to a revision must find that revision unchanged.

Reactivation MUST check consent and compilation before replacing the draft, so that a refused reactivation leaves the draft and its generation untouched.

The operation that produced a revision and the meaning of its document are separate properties, and both MUST be stored.

> [!NOTE]
> Restoring a generated revision keeps the generated semantics only because its compiler origin travels with it, independently of the fact that a reactivation produced it.

A revision MUST record its compiler origin, and a revision derived from another MUST inherit it.
Semantics marked as generated MUST be refused in an authored document.

What the generated first revision contains is specified by [LMIG](../ledger/migration.md).

### Catalogue refresh

Each catalogue refresh MUST recompile the active revision against the new inventory, bump a generation for each binding whose catalogue changed, clear that binding's open circuits, advance the publication handoff, and swap the runtime snapshot as one update.

A refresh whose compilation fails MUST leave the previous snapshot in place.

## The compiled policy

Compilation closes a validated document against the route topology and one immutable offer inventory, and the result MUST be immutable.

A document MUST NOT carry an inventory winner.
A winner MUST be looked up when the policy is compiled, for a presentation, or when a request is resolved, for a candidate selector.

> [!NOTE]
> The same revision then follows the catalogue instead of freezing a model list at the moment it was authored.

## The resolver

The resolver MUST be pure.
It MUST read no clock, no store, no provider and no request body, taking the present moment as one of its inputs, so that the control surface can run the same function the request path does.

### Facts in

The facts the resolver receives MUST be content-free: the profile and protocol, the requested model, the lane key, an upper bound on the prompt size, message and tool counts, the capabilities the request needs, a binding affinity, the request's class, and the present moment.

A request carrying provider-owned state, such as signed or redacted thinking or a provider's encrypted reasoning, MUST set a binding affinity, because only the binding that produced that state can replay it.
Such a request on a lane with no pin MUST be refused.

### Lookup order

The first of these to match MUST decide the route.

1. a presentation with that exact slug, for the profile, then for the protocol
2. a rewrite, for the profile, then for the protocol, in ascending priority
3. the profile's default, then the protocol's default

A compaction on a lane proven cold, under a policy naming a route for compactions, MUST resolve to that route instead, as [GCOMP](../gates/compaction.md#choosing-the-target) specifies.

A request carrying no model MUST resolve only through a default, MUST take that route's first candidate with no model, upgrade, alias or fallback, and MUST refuse a request that does carry a model.

### Walking the route

Candidates MUST be walked in order, starting at the lane's pinned candidate where the pin's route fingerprint still matches the route's, and at the first candidate otherwise.

> [!NOTE]
> Recovery of an earlier candidate must not pull a pinned lane back on its own.

For each candidate in turn:

A selector that resolves to no winner MUST end the walk.

> [!NOTE]
> An absent inventory is not evidence for falling back.

An edge crossed to reach a candidate MAY deny it, through fallback being disabled before send, through the billed boundary without consent, or through a warm cache whose loss the edge does not consent to.
Each of those MUST end the walk, because later candidates sit behind the same edge.

Availability MUST be three-valued, distinguishing positive presence, positive absence and no evidence either way.
Only positive absence MUST skip a candidate.

| positively absent |
| --- |
| the provider is unconfigured or disabled |
| a credential is known to be missing or expired |
| the binding cannot represent a required capability |
| the binding is not the request's affinity |
| the candidate is disabled |
| a fresh catalogue positively lacks the model |

A credential that fails only when tried is unknown, and the candidate MUST stay.
A stale or absent catalogue MUST be unknown.

Only an exact-model candidate and an exact presentation MUST be checked against a catalogue listing.
A model from a template, from the requested model or from a default MUST stay an opaque provider string.

> [!NOTE]
> An incomplete listing must not become an allowlist against the upstream.

A candidate whose target has an open circuit MUST be skipped.

### Shaping the model

For the surviving candidate, the model MUST be shaped in this order: the upgrade, then the compaction retarget, then the alias.

The plan MUST keep every stage distinctly, together with whether the client's own spelling decided the baseline.

### When nothing serves

A route with no surviving candidate MUST be answered as having no available route, carrying the trace.
A revision with no default for the protocol MUST be answered as not configured, since no binding was ever a candidate.

Both MUST leave a route-unavailable row, as [ROUTE](../routing.md#when-no-route-is-available) specifies.

### Resolving a single candidate

Inference MUST resolve with traversal, skipping unavailable candidates, and the plan MUST carry a continuation for the path after the request is sent.

Token counting and batch creation MUST examine only the first candidate and MUST NOT cross an edge.

> [!NOTE]
> Neither can be retried or pinned, so a chain has nothing to offer them.

## Circuits

A candidate MAY declare, for a classified failure, how long its target is out of service.

An attempt failing with a class the candidate has a circuit for MUST open one against that target.
An open circuit MUST make its target unavailable until it expires.

A success on the same target MUST clear its circuit.
A catalogue refresh of the binding, or a change of credential, MUST drop it.

> [!NOTE]
> Both are evidence that the failure may no longer hold.

An authentication failure MUST NOT open a circuit where the credential that failed was the client's own, passed through.

## Fallback and commitment

Fallback answers a candidate failing after its request was sent.
A request is a group of attempts, one ledger row each, each charged to the provider that received it.

### Failures that may fall back

Only a failure classified without reading message text, and listed by the crossed edge, MUST be eligible to fall back.

Classification MUST use the provider's own error types and codes, and MUST NOT use the wording of a message.
Status alone MUST classify only authentication, rate limiting and server errors.

An unclassified failure MUST be forwarded as the provider sent it.

A connect failure and a failure after the connection was accepted MUST be classified apart, because the latter leaves open that the provider accepted and billed the request.

A fall MUST re-check the billed boundary and the warm-cache rule.

The quota gate firing is not a candidate failing, and MUST NOT fall through to a billed provider.

### When a response is committed

Nothing is committed until the response head is returned to the client.
Before that, Toker MUST hold the provider's head and a bounded prefix of its body while it decides.

| condition | outcome |
| --- | --- |
| a non-success status within the cap | buffered in full and classified |
| a non-success status beyond the cap | forwarded as it stands, unclassified |
| a compressed body | committed as it arrives, since it cannot be read |
| a stream's first complete non-keepalive frame | committed |
| a stream's first frame carrying an in-band error | a classified failure |
| a stream ending or breaking before any frame, or whose first frame is invalid | a local 502, falling back to nothing |
| a non-streaming success | committed at once |

After commitment there MUST be no retry, because the client holds bytes.

A client disconnecting before commitment MUST cancel the attempt and record it, MUST NOT select another candidate, and MUST NOT pin the lane.

### One decision governs

On a classified, permitted failure, Toker MUST continue the same plan from the failed candidate, choosing the next by the same rules, folding in the crossed edge's consents, and building a fresh transition decision from the lane state captured when the request began.

Only the accepted attempt MUST pin the lane.

> [!NOTE]
> Falling back mid-session therefore pins the replacement for later turns, and a recovered higher-priority candidate does not pull the lane back.

Fallback MUST NOT make a provider's encrypted reasoning portable.
An affinity keeps such a request on its binding, which may leave nothing to fall to.

The retry that renegotiates a refused thinking mode is not fallback.
It is a second send to the same target within one attempt, as [CANON](../canonical.md#thinking-that-cannot-be-turned-off) specifies.

## Provenance

Provenance MUST be content-free and identity-only.

Each row of a policy-routed request MUST carry the revision, the route's identifier and fingerprint, the presentation or rewrite that matched, the candidate and binding, the model at each stage of its shaping, the [served model](models.md#served-model), and its attempt group, index and reason.

A routing trace MUST be a bounded structure recording how the route was found, each candidate with its binding and either its availability or a closed reason for skipping it, and each upgrade and alias from and to.

Where a trace would exceed its bound, candidate model spellings MUST be dropped first, and candidate identities and verdicts MUST be kept.

A trace written by an earlier version MUST remain valid history.

A response MUST echo the model the client requested rather than the model that served it.
The ledger MUST keep the requested, effective and served models separately.

Where a provider's response names no model, the served model MUST be absent, and MUST NOT be filled in from the effective model.
