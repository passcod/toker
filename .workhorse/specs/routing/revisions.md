---
id: RREV
---

# Revisions

A policy document reaches live traffic as a numbered revision, drafted, validated and activated without restarting the service.
This spec covers the draft, the operations on it, activation and its scopes, and the history behind it.

The document those operations carry is specified by [RDOC](document.md).

## Draft

There MUST be exactly one draft, held in the store so that two editors share it.

Every write to the draft MUST carry the generation it read and MUST be applied by compare-and-swap.
A stale write MUST change nothing and MUST return the current state.

A draft MUST be replaced whole, or edited by a bounded atomic list of typed operations addressing records by identifier.
An edit MUST NOT be able to address an incidental path within the document.
The whole result MUST be validated before it is stored.

Renaming a record MUST be expressed as a removal and an addition.

## Preview

Preview resolves an invented request against the draft using the same resolver inference uses, and returns the outcome and the trace.

Preview MUST use the snapshot's availability without per-request credential evidence and without open circuits.

An invented request MAY ask for server safeguards.
The outcome MUST then show, for each candidate, whether its binding's effective support for them is supported, unsupported or unknown, as [WSAFE](../wire/safeguards.md#which-bindings-receive-it) specifies.

> [!NOTE]
> It therefore shows what the policy says rather than what one request's headers would make of it.

## Activation

Activation MUST take the draft generation it was shown and compile the draft.
It MUST then, in one transaction, insert a new revision, make it active, replace the draft with the activated document at the next generation, and hand every publication adapter the new desired revision and inventory generation.

The runtime MUST install the [compiled policy](overview.md#the-compiled-policy) only after that transaction commits, so that a request in flight keeps the snapshot it captured.

A stale draft generation MUST fail the transaction, as MUST an active revision that moved.

An activation's scope decides what becomes of lanes already pinned to an earlier revision.

| scope | effect |
| --- | --- |
| new lanes | a pinned lane keeps resolving under the revision it is pinned to |
| new and cold lanes | a lane whose cache is provably cold moves to the new revision; the rest stay |
| immediate | the next request resolves under the new revision |

An immediate activation that would lose a warm cache MUST require explicit consent.
An immediate activation that would lose a warm cache MUST be refused while any lane is warm or not provably cold and that consent is absent.
Without consent, a lane that became warm after the check MUST stay on its pinned revision.

A revision a lane is pinned to MUST be kept and compiled on demand against current evidence.

## History and reactivation

A revision MUST never be edited, and MUST be deleted only by the [retirement of a generated revision](migration.md#retiring-generated-revisions).

The active pointer MUST never move backwards.
Reactivating an earlier revision MUST copy it into a new revision rather than move the pointer, and a pin to a revision newer than the active one MUST be a stale-revision error.

> [!NOTE]
> A lane pinned to a revision must find that revision unchanged.

Reactivation MUST check consent and compilation before replacing the draft, so that a refused reactivation leaves the draft and its generation untouched.

A revision MUST record the operation that produced it.
Every revision MUST be compiled by the same compiler, whichever operation produced it.

The contents of the generated first revision are specified by [RMIG](migration.md#the-first-revision).

## Catalogue refresh

Each catalogue refresh MUST do all of these as one update.

| on refresh |
| --- |
| recompile the active revision against the new inventory |
| bump a generation for each binding whose catalogue changed |
| clear that binding's open circuits |
| advance the publication handoff |
| swap the runtime snapshot |

A refresh whose compilation fails MUST leave the previous snapshot in place.
