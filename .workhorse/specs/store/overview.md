---
id: STORE
---

# The store

The store is the database Toker keeps its state in.
This spec covers its contents, how it is migrated, and the rules every one of its tables answers to.

## Contents

The store holds the ledger, being the append-only record of requests, which [SLEDGR](ledger.md) specifies.

Everything else the store holds is read-modify-write and is not part of the ledger.

> [!NOTE]
> Policy and configuration live in the store, not in the ledger.
> Nothing is appended to the record of requests to enable a provider or activate a revision.

The store's directory is the state directory, which [DCONF](../service/config.md#state) specifies.

## Migrations

Migrations MUST be an append-only list gated by a version stored in the database.

A migration MUST never be edited, reordered or deleted.

> [!NOTE]
> A database in the wild has had exactly the first several applied, by position, so changing an earlier entry changes what a later one operates on.

A migration MUST NOT lose a row.

## Not recorded is not zero

Every column but a row's timestamp MUST be nullable, and an absent value MUST mean that the field was not recorded.

A reader MUST NOT treat an absent value as a zero, a false or a negative.

> [!NOTE]
> An absent list of beta flags means none was captured, not that the request carried none.
> An absent policy revision means the row predates policy routing, not revision zero.
> An absent attempt group means the row predates attempt identity, not a request of one attempt.

Fields are added over time, so earlier rows lack later fields.
A view whose range reaches back past a field's introduction MUST say so rather than report the absence as a measurement.

> [!NOTE]
> Absence of instrumentation must never read as absence of the phenomenon.

## Pruning

The ledger MUST NOT be pruned.

The lanes and the allowances MUST be pruned on a periodic tick, to bounds on size and age.

Pruning a lane drops its pin, as [SLANE](lanes.md#the-pin) specifies.
Pruning an allowance is bounded by the gate's own expiry rule, as [GQUOTA](../gates/quota.md#allowances-expire-with-their-window) specifies, so it removes nothing the gate could still honour.

## Opening the store

A reader that does not write MUST open the store read-only.

> [!NOTE]
> A wrong path then fails instead of creating an empty store, and an older binary never migrates the live file.
