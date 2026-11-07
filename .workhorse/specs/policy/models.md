---
id: RMODEL
---

# Learning models and electing upgrades

Which version of a model family is current is decided from the models each binding has served, rather than from a maintained list.
This spec covers the evidence Toker learns, how a family's target is elected, when a request may be moved to it, and which declared facts decide whether a model fits.

## Terminology

### Requested model

The model identity a client named in its request.

### Effective model

The model identity Toker sent upstream.

### Served model

The model identity a provider's response reports as having served it.

## Identity authority

Only the served model MUST add a day's evidence, raise a learned prompt size, select a context ceiling, or choose a price.

The effective model MUST be read only to decide which model a cache belongs to.

The requested model MUST be read only as routing provenance.

> [!NOTE]
> A candidate's alias target is an opaque destination rather than evidence of capability or price, and the same separation covers a provider that quietly serves a model nobody asked for.

## Learned evidence

Learning MUST be keyed by binding and exact model identity together.

> [!NOTE]
> The same name on a subscription and on an API key is two models for every question asked here, and an observation on one must not license a move onto the other.

For each pair, Toker MUST record the distinct local days the model was served, the largest prompt seen, and whether it has been explicitly promoted.

A completed response MUST merge its observation in, adding the day, keeping the maximum prompt, and leaving a promotion in place once set.
The identity merged MUST be the one the response reported, and MUST NOT be the one requested.
A response naming no model MUST teach nothing.

Days MUST be local calendar days, and the timezone MUST be an input to the merge.

> [!NOTE]
> Having been seen on seven separate days is a statement about how someone works, not about UTC.

Toker MUST NOT rebuild learning from history.

> [!NOTE]
> Rows written before policy routing carry no binding, so there is nothing to attribute them to, and a machine adopting policy routing starts with no learning.
> The scaled bar below keeps that cheap.

## Electing a family's target

An upgrade policy names a classifier, the sources its candidates are drawn from, how much evidence a model needs, and whether a manual promotion stands in for that evidence.

A model MUST become its family's target once it has been served on more distinct local days than the requirement, which MUST be half the days the binding has any evidence for, capped at a small maximum.

> [!NOTE]
> An absolute bar asks a young ledger for evidence it cannot contain, leaving a fresh install dormant for a week.
> A scaled bar accepts what is present on day one and tightens as history arrives.

A candidate MUST belong to the same family as the requested model and MUST carry a strictly greater version.

> [!NOTE]
> The first request naming a new model arrives before that model has been learned, so electing the newest known model without this rule rewrites it downward, the new model never accrues a day, and the account is pinned below it permanently.

Versions MUST be compared segment by segment and numerically.

> [!NOTE]
> String order makes a version with a two-digit segment older than a one-digit one, and a bare major version newer than its own point release.

The election MUST decide only which model is the target.
Whether to move a request to it MUST be decided separately, by the cache rules below.

> [!NOTE]
> That separation makes an early, weak bar acceptable.
> The worst such a bar can do is route new sessions to a model that was trialled, within one family.

### Classifying an identity

A family split MUST fold a published dated snapshot to its undated form before comparing.

An identity carrying a date the catalogue does not know MUST have no family at all.

> [!NOTE]
> Its observed days and prompt sizes would otherwise authorise routing traffic to a different identity.

An identity with no version MUST be excluded from the election, both as a target and as a baseline.

### Promotion

A promotion MUST mark one learned pair explicitly eligible, where its upgrade policy allows a promotion to stand in for evidence.

A promotion MUST replace the day count for that one model and nothing else.
A promoted model MUST still be newer within its family and MUST still fit the prompt.

Toker MUST refuse to promote a pair that has never been served.
A promotion MUST change only that eligibility, leaving observed days and prompt sizes as the provider's evidence.

## Only where no cache can be lost

A request MUST be moved to the elected target only where the move cannot cost a cached prefix.

A lane Toker has a record of MUST be eligible only where its cache is proven gone.
This MUST ask whether the cache is gone, and MUST NOT ask whether the operator should be interrupted, as [GCOLD](../gates/cold.md#two-questions-not-one) requires.

A request with no lane evidence MUST be eligible only where the conversation is at most a few messages long, or where nothing has been sent on the requested model, on this binding, within a full cache lifetime.

A lane with no evidence MUST NOT be treated as a new session.

> [!NOTE]
> A lane table forgets lanes through restarts, evictions and pruning, so a session whose cache is warm upstream can look new here, and moving its model then destroys exactly the cache the cold gate exists to protect.

Idleness MUST be keyed on the model.

> [!NOTE]
> A cache belongs to the model it was written on.
> Keying on the tool set looked workable, since the tool list opens the prompt, but a client changes tools mid-conversation and the cache survives.

A compaction MUST NOT be upgraded by this election.
It has its own retarget, which [GCOMP](../gates/compaction.md#choosing-the-target) specifies.

### The recency record

The record of the model last sent MUST be kept apart from the lane table.

> [!NOTE]
> It must not forget within the period it answers for, which a lane table makes no promise about.

It MUST be keyed by binding and exact effective model, and MUST be marked immediately before an inference send begins rather than when its response completes.

It MUST vouch only as far back as it covers, and a binding with no recorded send MUST have no coverage, so its silence proves nothing and the answer is that the model is not idle.

It MUST be seeded at startup from the most recent rows carrying both a binding and an effective model, counting rows that record an error.

> [!NOTE]
> An error row can still be a provider response, and dropping a real send risks losing a live cache, where keeping a local failure only postpones a move.

Every doubtful value MUST answer that the move is not permitted.

> [!NOTE]
> A wrong refusal costs an upgrade; a wrong permission costs a rebuild of the whole prefix.

### An upgrade is decided once and then kept

A client is never told its model was rewritten, so every later request in that conversation names the original.

While the lane is warm, a request naming the model that was moved from MUST resolve to the model it was moved to, a warm compaction included.

> [!NOTE]
> Honouring the client's repeated request instead rebuilds the whole prefix on the original model, so every upgrade pays for its cache twice.

A request naming any other model MUST end the upgrade, being the operator choosing.
A cold lane MUST be decided afresh, having nothing to lose either way.

The record of the move MUST be rebuilt from the ledger on restart.

## Context fit

Two facts bear on whether a prompt fits a model, and they MUST be consulted in this order.

A declared context window is the provider's fact.
Where the routed binding's fetched listing names a window for the identity that would be sent, it MUST decide, in both directions.

> [!NOTE]
> A prompt over the listed window declines whatever was learned, and a prompt within it fits whatever was not.

The largest prompt Toker has watched that pair serve MUST decide only where no window is named at all, and there an unproven model MUST decline.

> [!NOTE]
> Learning undershoots by construction, being the largest prompt anyone happened to send, and has kept a large compaction on an expensive model against a listed window four times the learned figure.
> Declining costs an upgrade; admitting costs a failed request at the worst moment.

Where a lane has no measured size, the context check MUST be given an upper bound derived from the request itself, and MUST NOT be given zero.

## Resolving a declared window

For display and for reporting, a context window MUST be resolved in this order: the hand-verified catalogue, the provider's fetched listing, a stored declaration, then unknown.

A model the hand-verified catalogue knows MUST resolve to its verdict even where that verdict is unknown.

> [!NOTE]
> The hand-verified catalogue encodes beta phases a listing cannot express, so an uncaptured phase is a decision rather than a gap a listing may fill.

A declared ceiling MUST NOT be treated as evidence that a route has served a prompt of that size.

## Identities are exact

A published dated snapshot MUST fold to its catalogue key.

Family resemblance MUST NOT assign a limit to a model, and a shared name prefix MUST NOT be read as evidence of capability.

A beta header selecting a larger context MUST apply only to the catalogue entries and dates it was verified against.

> [!NOTE]
> A compatibility gateway can carry such a header while returning another provider's model, and that model takes its own ceiling or stays unknown.

Normalising a model's identity for pricing MUST NOT be read as establishing its capabilities.
