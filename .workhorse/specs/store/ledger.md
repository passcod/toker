---
id: SROWS
---

# The ledger

The ledger is the append-only record of requests within the store, one row per upstream attempt or proxy decision.
This spec covers how rows are written and what each kind records.

The store around it, its migrations and the rule that an absent value is not a zero are specified by [STORE](overview.md).

## Writing

The ledger MUST be insert-only.
Every row MUST be written through one function, so that no caller can assemble a row of its own shape.

## Row kinds

A row with no kind is a measurement of a provider's own report.
Any other kind is a row Toker wrote about itself.

That rule MUST be kept in one place, so that every consumer classifies identically.

A reader MUST exclude Toker's own rows by the presence of a kind, and MUST NOT exclude them by listing the kinds it knows.

> [!NOTE]
> Listing kinds is how a kind added later slips into a quota fit and into per-request averages.

Any measurement of a provider's own reports, and of when they arrived, MUST exclude them.

> [!NOTE]
> Counting them reports the proxy's own staleness back as the provider's.

| kind | what it records |
| --- | --- |
| blocked | the quota gate stopped a request |
| released | a release granted an allowance |
| revoked | a session's allowances were deleted |
| cold | the cold notice fired |
| cold-quiet | a cold notice was withheld, with the estimate it rested on |
| cold-recap | a recap on a cold lane was answered and never forwarded |
| awake | the sleep lock changed hands |
| error | an attempt failed, or a usage path answered other than success |
| route-unavailable | policy would not route the request, so nothing was sent |

A row recording a decision Toker took without reaching a provider MUST carry Toker's last-seen meter copy rather than a response's own snapshot, and MUST be distinguishable as such.

A row recording the sleep lock MUST carry no profile, provider or route.

> [!NOTE]
> The lock is not a route.

## Model identity

A row MUST carry the [requested model](../policy/models.md#requested-model), the [effective model](../policy/models.md#effective-model) and the [served model](../policy/models.md#served-model) separately, together with the pricing-normalised form of the served one.

A response naming no model MUST leave the served model absent.
Nothing MUST fill it from the effective model.

## Fields that need care

A record of an upgrade MUST be written on every request the upgrade moved, not only the first.

> [!NOTE]
> The lane remembers the move while its cache is warm, so each warm request on a moved lane carries it.

A record of a compaction's model move and a record of its body transform MUST stand independently.

> [!NOTE]
> A compaction already on the target model is rewritten without being moved.

Both MUST be taken from the attempt plan rather than from the body transform.

A row MUST record whether each gate was armed for that request.

> [!NOTE]
> The toggles are configuration the ledger cannot otherwise see.

A cost MUST be stored with its kind, and the kinds MUST NOT be summed together.

A meter snapshot MUST be kept as the provider reported it, and a header Toker does not recognise MUST be kept rather than dropped.

A row MUST carry the reset times of the windows its meters named, so that any window can be reconstructed afterwards.

## System prompt changes

A system prompt change MUST be localised at capture time, by comparing the request against the newest row in its lane that carries a prompt hash, read from the ledger.

> [!NOTE]
> Neither an in-memory map nor the lane table survives long enough: a map is lost on every restart, and lanes are pruned.

Where the hashes match, the row MUST drop its comparison ladders.
Where they differ, the row MUST keep them and record where the change fell.

An absent change record MUST NOT be read as the prompt having held.

> [!NOTE]
> It is also a lane's first row, a row with no session and so no lane, and a change whose baseline could not be read.
> Only a matching prompt hash says the prompt held.

A reader MUST prefer the stored location of a change, MAY re-derive it from the ladders where it is absent, and MUST report that the location is unknown where neither bounds anything.

## Eras within one table

Nothing backfills, so a reader meets every era in one table.

| era | what it lacks |
| --- | --- |
| before policy routing, and every imported row | the policy, attempt and lineage columns |
| after attempt identity but before policy routed everything | the policy columns, while carrying the attempt columns |
| since policy routes everything | nothing |

A reader MUST NOT require the policy columns where the attempt columns are present.

A missing policy revision on a recent measurement MUST be treated as a defect rather than as an era.

A row from before attempt identity MUST be read as one logical request, even where it was in fact two sends.

## Imported rows

An import MUST write rows that look like Toker's own, filling what the source recorded and leaving absent what it did not.

An import MUST NOT invent a value the source never carried.

A view over a range reaching into imported rows MUST say so rather than read those absences as zeros.

An importer MUST count every field of the source it did not map, by name, separating fields left out deliberately from fields it does not recognise.

> [!NOTE]
> An unrecognised field means the mapping is behind the source.

A field whose meaning changed in the source carries its eras across with the rows, so a count over such a range MUST be reported as a floor.

## Export

How rows are exported is specified by [UREAD](../cli/readers.md).
