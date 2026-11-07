---
id: GPING
---

# Pinging a window open

A rate limit window is anchored to the request that opens it, so sending a request at a chosen time places that window's boundary.
This spec covers how a boundary is predicted, when a ping is sent or skipped, and how a run records whether the schedule held.

## Where a boundary falls

A five-hour window's reset falls at the fire time of the request that opened it, floored to a ten-minute grid, plus five hours.

Toker MUST predict a boundary from the time the ping actually fired, and MUST NOT predict it from the slot the ping was scheduled for.

> [!NOTE]
> A ping fires some minutes after its slot, so a prediction made from the slot is ten minutes early whenever the two straddle a grid line.

## Sending a ping

A ping is one minimal inference request sent on a schedule.

Toker MUST tag a ping with the configured ping header, and MUST record the ping flag on its ledger row.
Toker MUST NOT identify a ping by the size of its request or of its usage.

> [!NOTE]
> Most real requests consume less of a window than any threshold that would exclude a ping, so a size test silently reclassifies real traffic.

Toker MUST skip a ping while any measurement in the ledger reports a five-hour reset still ahead.
Where the ledger cannot answer, Toker MUST send the ping.

> [!NOTE]
> A window that is already open gains nothing from a ping, and the run would record the existing window's reset as its own achievement.
> A wrong skip forfeits the boundary outright, where a wrong send costs one minimal request.

Toker MUST refuse a ping that fires more than ten minutes after its scheduled time.

> [!NOTE]
> Suppressing a timer's catch-up across a restart does not suppress a realtime elapse that passed while the machine was suspended, which fires the moment the machine wakes and would place the day's boundary wherever that happened to be.

## A ping is routed like any other request

A ping names a model, and the active policy revision decides where that model goes.

Only a ping-tagged row served by the subscription provider counts as having opened a window.
A ping the policy sent to another provider, or that failed over from the subscription, MUST be recorded as having observed no boundary.

## Recording a run

Each ping run MUST be recorded with the boundary it predicted, the boundary the ledger subsequently reported, and whether the two matched.

> [!NOTE]
> Whether the schedule held is then a query rather than a recollection.

A readback MUST consider only rows logged after the ping began.
