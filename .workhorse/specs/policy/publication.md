---
id: RPUB
---

# Publishing models to profiles

A client learns which models exist from the list it asks Toker for, and, where its settings support it, from rows Toker writes into them.
This spec covers both, the generation that keeps them consistent, and how a writer owns the rows it wrote.

Both are projections of the active policy revision and the offer inventory.
Neither MUST be a copy of a provider's listing.

Publication is separate from routing.
A slug that is listed MUST NOT thereby be routable, and a slug that routes MUST NOT need to be listed.

## Answering a model listing

A model listing MUST be answered locally and MUST NOT be forwarded.
It is not a usage path, and MUST NOT write a ledger row.

The frontend profile selects the protocol whose shape is rendered.
A profile naming no protocol MUST receive the Chat shape.

A listing MUST contain the revision's presentations for that scope.

An exact or selected presentation MUST take its display data from the offer's own catalogue record.
A virtual presentation has no provider record, so it MUST name itself, and MUST report no creation time rather than invent one.

Entries MUST be ordered by slug, so that one revision and one inventory render the same bytes every time.

### Where a listing needs a complete record

A protocol whose client requires a complete model record for every entry cannot carry a virtual presentation from a provider record, because there is none.

Such a presentation MUST be listed from the adapter metadata the document carries for it, rendered under the presented slug.
A presentation without that metadata MUST be left out of the listing, and the compiler MUST report it.

The compiler MUST also report metadata attached to a presentation that cannot use it.

Only offers from the provider whose client requires the record MUST be projected into that protocol's listing.

> [!NOTE]
> Another provider's record is not in the shape that client requires, so such an offer stays routable without being advertised.

### An unresolved listing fails

Where an exact presentation names an offer the inventory does not hold, the whole listing MUST be unresolved and MUST answer 503.
Toker MUST NOT answer with a partial list.

> [!NOTE]
> A partial list either advertises a model nothing can route or silently drops one the operator chose.
> A failed list is visible; a short one is not.

Such a presentation MUST still compile, because a catalogue can legitimately lag a policy.

### Generated listings

A revision generated from an earlier configuration MUST continue to list the slugs clients had already learned.

Those slugs MUST be computed when the revision is compiled, from the stored document's own defaults and the current inventory.

> [!NOTE]
> A stored revision then lists what it routes, rather than what a configuration file says today.

A generated slug is publication only.
A request naming one MUST be routed through the revision's rewrites and defaults, which keep the alias and upgrade policy of the route they reach.

A slug a presentation also publishes MUST belong to the presentation.

## Generations

Two values say which state a publication was rendered from: the revision, and the inventory generation.

An inventory generation MUST be a deterministic digest over every cached catalogue source's fetch time and each model's identity, context window, chronology and raw record.

> [!NOTE]
> A second process reading the same cache then computes the same number, so a user-side writer can detect that the cache moved under it.

An empty cache MUST have a fixed generation.

A catalogue refresh MUST replace the catalogues, the offer inventory, the availability snapshot and the [compiled policy](overview.md#the-compiled-policy) as one update, under the lock the control operations take.
A reader MUST see the old set or the new set, and MUST NOT see a mixture.

## Publication state

Toker MUST hold one row per publication adapter and frontend scope.

Each row MUST carry a desired revision and inventory generation, the pair last applied, the exact identities the adapter owns, a state, and a stable detail code.

| state | meaning |
| --- | --- |
| pending | the desired pair has not been reconciled |
| applied | the desired pair is in place |
| failed | reconciliation was attempted and did not succeed |
| unsupported | the desired pair cannot be represented by this adapter |

An adapter the service applies by construction MUST be marked applied in the same transaction that records the revision, with an empty identity set, since it owns nothing outside the service.

An adapter the service cannot apply MUST start pending at every activation and every catalogue handoff.

The desired pair MUST be committed atomically with the revision it names.

> [!NOTE]
> No committed revision then lacks a desired publication, and the listings the service serves never run ahead of or behind what the row says is wanted.

Setting a new desired pair MUST reset the state to pending and clear the detail code, and MUST keep the applied pair and the owned identities.

> [!NOTE]
> Ownership has to survive a handoff the adapter has not reconciled yet, because until it does the previously written rows are still there and still the adapter's to remove.

A recorded result MUST be a compare-and-swap on the desired pair the adapter rendered.
A result for a stale pair MUST NOT apply.
A failure MUST NOT overwrite a row already applied for that pair.

An applied row MUST carry applied values equal to the desired ones and a non-empty identity set, and a row read back that violates either MUST be a stored-state error.

## Writing into a client's settings

Where Toker publishes into a settings file, that file is one the operator also edits, owned by a program that reloads it into every live session.

The service MUST NOT write it.

> [!NOTE]
> The service may write only its state directory, and the file belongs to the operator's own account.

A user-side writer MUST perform that reconciliation, and MUST run at least on an activation and on a recurring schedule.
Activation MUST NOT wait for it, and a failed write MUST leave the activation standing with the row recording the failure.

### Reconciling

A reconciliation MUST load the row and the revision it names, and MUST recompute the inventory generation over the cached catalogues of the enabled providers.
Where that generation differs from the one recorded, it MUST fail before touching the file.

It MUST compile that revision against that inventory and render the rows from the presentations that carry the metadata the client needs.

Where one settings file must serve several profiles and they project different rows, no single file can represent the policy, so the row MUST become unsupported and the file MUST be left untouched.

It MUST remove the rows the last successful reconciliation owns, write the new ones, and do so only where the result differs, atomically.

It MUST then record success with the exact identities it wrote, for the pair it rendered.

A database with no active revision MUST be an error raised before the settings are read.

> [!NOTE]
> Rendering from no revision would empty the operator's picker.

Where the compare-and-swap finds a newer desired pair arrived while the file was being written, the writer MUST add the identities it just wrote to the prior set and retry once against the newer pair.

> [!NOTE]
> Otherwise the second pass reads its own first write as the operator's row and refuses.

### Ownership is exact

Toker owns exactly the row identities its last successful reconciliation recorded, and nothing else.

Toker MUST NOT determine ownership from a prefix or any other pattern of the row's content.

> [!NOTE]
> A prefix rule claims an operator's own row that happens to match, and cannot say what to remove when the policy changes the slugs it publishes.

A row in the file MUST be replaced or removed only where its identity is in the previously owned set.

Where a desired row has the same identity as an existing row that is not owned, the reconciliation MUST refuse, MUST leave the file as it was, and MUST report which identity collided.
There MUST be no merge and no takeover.

A settings file whose picker structure is malformed MUST be refused, and MUST NOT be overwritten.

An identity set that has never been recorded MUST NOT be treated as an empty set.

> [!NOTE]
> Treating unknown ownership as none makes the next reconciliation read Toker's earlier rows as the operator's and refuse forever; treating it as everything deletes the operator's rows.
> Carrying an earlier install across is a migration, which [SMIG](../store/migration.md) specifies.

An identity set MUST be stored in a canonical order, bounded in size and free of duplicates.
A set that does not parse that way MUST be a stored-state error rather than an empty set.

The identity set MUST advance in the same statement as the applied revision and inventory generation.
