---
id: RREV
---

# Revisions

A policy document reaches live traffic as a numbered revision, edited and activated without restarting the service.
This spec covers saving an edit, previewing one, activation and its scopes, and the history behind it.

The document those operations carry is specified by [RDOC](document.md).

## Saving an edit

A save applies an edit to the active revision's document and activates the result, as one operation.
A save and a [reactivation](#history-and-reactivation) are the only ways a revision becomes active after the [first revision](migration.md#the-first-revision).

An edit MUST be a bounded atomic list of typed operations addressing records by identifier.
An edit MUST NOT be able to address an incidental path within the document.
Renaming a record MUST be expressed as a removal and an addition.

A save MUST carry the active revision it was made against, and MUST fail, changing nothing, where the active revision has moved.
The result MUST be validated whole, and an error MUST refuse the save and change nothing.

A save that compiles MUST be activated with the new and cold lanes scope.

> [!NOTE]
> That scope moves only the lanes it can move without losing a warm cache, so a save needs no consent and disturbs nothing in flight.
> Moving the rest is a separate, deliberate step, by [widening](#widening-an-activation).

> [!NOTE]
> Editing the live document directly leaves nothing staged beside it that a save could carry into live traffic unseen, or that a later activation could use to undo a save.

## Preview

Preview resolves a described request using the same resolver inference uses, and returns the outcome and the trace.

Preview MUST be able to resolve against the active revision, an edit not yet saved, or any revision kept in the history.

Preview MUST accept a described request that is invented, and one taken from the facts a ledger row recorded, as [RPROV](provenance.md) specifies.
A row that lacks any of the facts the resolver reads MUST be reported as not replayable, and MUST NOT be replayed with a fact assumed in its place.

> [!NOTE]
> Replaying recent requests shows what an edit would change for the traffic the operator actually has, which an invented request cannot.
> A replay with a guessed capability or prompt size can show a candidate serving that the request would in fact skip, which is worse than showing nothing.

Preview MUST use the snapshot's availability without per-request credential evidence and without open circuits.

An invented request MAY ask for server safeguards.
The outcome MUST then show, for each candidate, whether its binding's effective support for them is supported, unsupported or unknown, as [WSAFE](../wire/safeguards.md#which-bindings-receive-it) specifies.

> [!NOTE]
> It therefore shows what the policy says rather than what one request's headers would make of it.

## Activation

Activation makes a compiled document the active revision.
It MUST, in one transaction, check that the active revision is the one the operation was made against, insert a new revision, make it active, and hand every publication adapter the new desired revision and inventory generation.
An active revision that moved MUST fail the transaction.

The runtime MUST install the [compiled policy](overview.md#the-compiled-policy) only after that transaction commits, so that a request in flight keeps the snapshot it captured.

An activation's scope decides what becomes of lanes already pinned to an earlier revision.

| scope | effect |
| --- | --- |
| new lanes | a pinned lane keeps resolving under the revision it is pinned to |
| new and cold lanes | a lane whose cache is provably cold moves to the new revision; the rest stay |
| immediate | the next request resolves under the new revision |

> [!NOTE]
> Neither a save nor a reactivation activates with the new lanes scope.
> It is kept because the active revision on a machine may have been activated with it, and widening starts from it.

An immediate activation that would lose a warm cache MUST require explicit consent.
An immediate activation that would lose a warm cache MUST be refused while any lane is warm or not provably cold and that consent is absent.
Without consent, a lane that became warm after the check MUST stay on its pinned revision.

A revision a lane is pinned to MUST be kept and compiled on demand against current evidence.

### Widening an activation

The active revision's scope MUST be able to be widened after it was activated, from new lanes to new and cold lanes, or from either to immediate, without creating a revision.
A widening MUST carry the active revision it widens, and MUST fail, changing nothing, where the active revision has moved.
Widening to immediate MUST follow the same consent rules as an immediate activation.
A scope MUST NOT be narrowed.

> [!NOTE]
> Moving running lanes onto the live revision is a decision about lanes, not about the policy, so it leaves the history as it was.
> Consent given to lose the caches of one revision's lanes is not consent to lose them for whatever revision replaced it meanwhile.

## History and reactivation

A revision MUST never be edited, and MUST be deleted only by the [retirement of a generated revision](migration.md#retiring-generated-revisions).

The active pointer MUST never move backwards.
Reactivating an earlier revision MUST copy it into a new revision rather than move the pointer, and a pin to a revision newer than the active one MUST be a stale-revision error.

> [!NOTE]
> A lane pinned to a revision must find that revision unchanged.

A reactivation MUST carry the active revision it was made against, and MUST be activated with the new and cold lanes scope.
A reactivation that fails to compile against current evidence MUST change nothing.

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
