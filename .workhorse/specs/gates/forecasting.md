---
id: GCAST
---

# Forecasting a meter

A meter reports cumulative utilisation on every response, so projecting when it runs out is a matter of choosing two readings and measuring between them.
This spec covers how a rate is measured, how a span is totalled, and when a reading says nothing at all.

One projection MUST serve every reader of a meter, so that a view and the gate it describes cannot disagree.

## Measuring a rate

A rate is the difference between two readings over the time between them.

Toker MUST measure to the present moment rather than to the latest reading.

> [!NOTE]
> Measuring to the latest reading keeps reporting a burn after the traffic producing it has stopped.

Toker MUST take the running maximum of the readings as a meter's current value.

> [!NOTE]
> Requests run concurrently and finish out of order, each response carrying the utilisation of the moment it was served, so the newest row is not the highest figure.

Toker MUST compare two utilisation figures with an epsilon rather than against a bare threshold.

> [!NOTE]
> The figures are decimal and the arithmetic is binary, so a difference that should equal a threshold falls just under it.

## When a window has restarted

Utilisation restarts at zero when a window rolls, so a lookback spanning a reset reports the quota refilling.

Toker MUST clamp a lookback to the reset the latest reading carries.

Toker MUST treat a cumulative meter that has fallen as having restarted, whatever its reset field reports.
Toker MUST treat a fall as a restart only where the value does not return, and only where it has held for longer than request reordering can explain.

> [!NOTE]
> A meter's reset field can name the same instant across several accounting periods, so a meter actively being spent reads as on track for as long as the field says.
> Without the second condition, a dip in the newest reading restarts the window trivially, because nothing follows it to return.

## The start of a window

Where a window's length is known, Toker MUST reconstruct its start as a reading of zero.
Where a window's length is not known, Toker MUST NOT assume where it began.

A window whose length is unknown and whose first day holds less than a day of history MUST report that it cannot project, rather than project from what it has.

## Projecting across a day

Where a meter's reset is more than a day away, the span measured MUST be at least a day.
Toker MUST judge that against the span actually measured, never against the lookback asked for.

> [!NOTE]
> A rate measured over an evening and projected through the nights that follow puts the wall days early.
> Requiring a measured day carries the duty cycle without a model of it.
> Across an overnight gap, a day-long lookback can find a reading hours old and report an evening as a day.

## When a meter has not moved

A meter's quantisation gives an unmoved meter's rate a ceiling.
Where even that ceiling does not reach the wall before the reset, Toker MUST report the meter as on track.

> [!NOTE]
> That is a measurement, not an absence of one.
> Reporting an estimate for every unmoved meter would make a rarely-moving meter's line permanently useless.

## Totalling a span

A span total answers what a fixed stretch of wall clock cost.

The baseline MUST be the reading before the span, and MUST NOT be the first reading inside it.

> [!NOTE]
> The first reading inside a span already includes what its own request consumed, so measuring from it drops a request's worth of spend.

Where no earlier reading exists, Toker MUST use the period's own start as the baseline only where that start provably falls inside the span.
Otherwise the total MUST be reported as a floor, and MUST say so.

A span total MUST exclude every row carrying a row kind.
Toker MUST exclude such rows by the presence of a kind, and MUST NOT exclude them by listing the kinds it knows.

> [!NOTE]
> A blocked row carries an old meter figure under a fresh timestamp, which is harmless as the newest reading of a rate and ruinous as the baseline of a total.
> Listing kinds is how a row kind added later slips back in.

## A reading is only about its own window

Once a window's reset has passed, its readings say nothing about the current window, and nothing replaces them until traffic resumes.

Toker MUST ignore a meter whose window has ended when deciding whether the quota gate fires.

> [!NOTE]
> A blocked request never refreshes the meters that would clear it, so without this the gate never reopens.

## The wall

Against an armed gate, the wall is the gate's own threshold rather than full utilisation.
A view MUST read that threshold from the gate rather than restate it.

Whether the gate was armed MUST be recorded on each row.

> [!NOTE]
> The gate's toggle lives in configuration, which a later reader of the ledger cannot see.
