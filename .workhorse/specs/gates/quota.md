---
id: GQUOTA
---

# The quota gate

The quota gate stops a session that would spend past a subscription's rate limit window.
This spec covers a window's metered consumption, how it is fitted from the ledger, when the gate fires, and how a session releases past it.

## Borrowed terms

| term | meaning |
| --- | --- |
| claim | the window a provider names as representative of a reading, reported in its rate limit headers |
| overage | spend beyond a subscription plan's included allotment, billed separately |

## Metered consumption

A rate limit window meters fresh prompt tokens, which are uncached input together with cache writes, and output tokens.
Both are weighted per model.

Cache reads carry no measurable weight.

> [!NOTE]
> Cache reads are the great majority of token volume and most of the bill, so optimising spend and optimising quota pull in different directions, and a cache rebuild costs far more against quota than against the bill.

## Fitting a weight

A weight MUST be fitted from the ledger on every run, and MUST NOT be stored as a constant.
Every weight MUST carry the spread it took across refits that each leave one window out.

A fit MUST compare across windows, and MUST NOT fit within a single window.

> [!NOTE]
> Within one window every candidate quantity accumulates together and fits almost perfectly, so the fit's quality says nothing.
> Only a slope surviving across windows with different model mixes does.

Where two model groups move in lockstep, the fit MUST measure that directly and fold the smaller group into the larger, which then lends the smaller its weight as an upper bound.
The fit MUST NOT decide this from a group's share of a window.

> [!NOTE]
> A non-negative solver hands one of two lockstep groups the other's weight, and has given a small group a weight many times too large while zeroing the group carrying most of the traffic.
> A dominant group's share is near-constant because it dominates, so a share test demotes exactly the group that is best measured.

A group seen in too few windows has no weight, and Toker MUST report it as unknown rather than as zero.

> [!NOTE]
> A zero weight reads as a claim that the traffic is free.

### A weight whose spread reaches zero is no weight

Where a refit reaches zero, the weight could have been dropped with the windows still explained, so the point figure is the residue of the other columns rather than a measurement.

Toker MUST refuse a weight whose spread reaches zero.
Refusing it leaves the outlook unable to price, which MUST let the notice fire rather than suppress it.

### Pricing at the top of the spread

Toker MUST price with the top of a weight's spread rather than its point figure.
A notice quoting such a price MUST express it as an upper bound.

> [!NOTE]
> The outlook decides whether to stay quiet, so underestimating costs a missed warning and overestimating costs one extra notice.

## Scope of a fit

A fit MUST read each attempt's own row, and MUST read only the rows of the provider whose meters it prices.
A fit MUST NOT read a total grouped across providers.

A request that failed over from the subscription therefore contributes the attempts the subscription served and nothing another provider did.
Attempts are specified by [SATT](../store/attempts.md).

## Firing

The gate's threshold leaves the last part of a window unspent unless the session releases past it.
How the stop is delivered is specified by [GNOTE](notices.md), and how exhaustion is projected by [GCAST](forecasting.md).

## Releases

A release grants a session passage past the gate, and is recorded as an allowance row.

| release | effect |
| --- | --- |
| burn | releases into overage for the rest of the window |
| overage-only | releases only while the plan still has room |

The overage-only release MUST hold only while the blocked meter reads under full utilisation and the provider reports that overage is not in use.
Once either condition fails, the gate MUST stop the session again, and only the burn release MUST carry it further.

A second release for the same window MUST only widen the first, never narrow it.

A request that crosses into overage MAY still be billed partly as overage, which the meters report only in that request's own response.

### Granting a release

A release MUST be granted either by a marker the session sends or by the operator acting on the session directly, and both MUST take the same path.

The markers are `$#$BURN$#$` and `$#$OVER$#$`.
These strings are a published interface and MUST NOT change.
They are stripped from every request, as [WCANON](../wire/canonical.md#licensed-transformations) requires.

Where the operator grants a release directly, Toker MUST allow it ahead of exhaustion for the short window.
Where the operator grants a release directly, Toker MUST allow it for the long window only once that window is exhausted.

> [!NOTE]
> A release meant for an afternoon would otherwise become one for the week.

Revoking a session's releases MUST delete its allowances, so that the gate applies again from the next request.

The gate MUST load a session's allowances on every request.
The gate MUST NOT depend on being signalled that one was granted.

### Allowances expire with their window

An allowance MUST be keyed by the reset its meter reported.
An allowance MUST be honoured only while the meter still reports that reset.

Once that reset passes the meter is no longer exhausted and the next window reports a new reset, so the row can never match again.
Enforcement therefore needs no pruning, and Toker MUST prune only to bound the table.

Toker MUST delete an allowance whose reset is at or before the present moment, on the same periodic tick as its other state pruning.
Toker MUST read one session's allowances by key rather than scan the table, so that neither the table's size nor the prune's cadence falls on the request path.

## Console events

Toker MUST log an event when a meter-bearing provider's reading crosses each of several thresholds of its short window, when the representative claim changes, and when the set of per-window statuses other than allowed becomes non-empty or changes.

Each event MUST fire once.
A threshold MUST re-arm when the window's reset changes or when utilisation falls well below it.

Toker MUST NOT enumerate the statuses it recognises.
Toker MUST treat any status other than allowed as reportable.

> [!NOTE]
> The set of statuses a provider sends grows without notice, and statuses beyond the first known one have already appeared.

Toker MUST seed each provider's event latches from the meter snapshot kept across a restart, read before the first new reading replaces it, so that a restart repeats nothing already said about the same window.
A stored snapshot whose window has ended MUST seed only the claim.
A reading carrying no status MUST leave the status latch untouched.
