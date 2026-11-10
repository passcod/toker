---
id: GPING
---

# Pinging a window open

Where a provider anchors a rate limit window to the request that opens it, sending a request at a chosen time places that window's boundary.
This spec covers which providers that applies to, how a boundary is predicted, when a ping is sent or skipped, and how a run records whether the schedule held.

## Scope

Pinging applies only to a provider that anchors a rate limit window to the request opening it.
Among the providers [ROUTE](../routing/overview.md#providers) specifies, the Anthropic subscription alone meters windows this way.

Toker MUST NOT send a scheduled ping, nor hold the machine awake for one, while no enabled provider anchors a window to its opening request.

> [!NOTE]
> A provider that meters without anchoring, or that does not meter at all, gains nothing from a request sent at a chosen time.

Toker MUST keep a marker file in the state directory present exactly while the ping schedule is turned on and such a provider is enabled, and the scheduled ping and hold jobs MUST run only while it is present.
The ping schedule MUST be off until the operator turns it on.
The service MUST bring the marker in line when it starts, whenever a provider is turned on or off, and whenever the schedule is.

> [!NOTE]
> The jobs are installed once and switched by the marker, so turning the schedule or a provider on or off from the dashboard starts or stops pinging without asking the platform for elevation.
> The condition is on the job rather than its timer: a timer's condition is read only when the timer starts, so one skipped at boot would stay off after the marker appeared.

Every requirement below is about such a provider's windows.

## The schedule

The schedule is a list of slots, each a time of day on the local clock with the days of the week it runs on.

Each slot MUST name at least one day.
Two slots MUST NOT share a time of day.

At each slot, Toker MUST hold the machine awake long enough for that slot's ping to fire, and MUST send the ping a few minutes after the slot.
A ping carried past midnight by that delay MUST run on the day after each of its slot's days.

> [!NOTE]
> A machine just woken needs a moment for its network to settle, and one left to itself would doze off again before the ping.

A slot MUST be refused where its ping would fall within five hours of another slot's ping on a day both run, or across the midnight between two days they run.

> [!NOTE]
> The later ping would always find its account's window already open, and be skipped as [sending a ping](#sending-a-ping) requires.

The schedule MUST NOT be turned on while it has no slots.

### Waking the machine

The schedule is in one of three positions: off, pinging while the machine is awake, or waking the machine from suspend at each slot to ping.
The machine MUST be woken only in the third.

> [!NOTE]
> A ping schedule is useful on a machine that is up anyway, and on one that sleeps, but a wake with no ping behind it only spends battery.

### Where the schedule is kept

The installed timer units MUST be the record of the slots, their days, and whether the machine is woken, and the store MUST NOT hold a copy of any of them.

> [!NOTE]
> The units are what fires, so a copy kept beside them is a second answer that can disagree, and the service is sandboxed away from the units it would need to repair one.

Waking the machine needs the system manager, so placing the wake, changing its slots or days, and removing it MUST ask the platform for elevation.
Turning the schedule on or off, and editing its slots while the machine is not woken, MUST NOT.

The elevated step MUST be a shell script holding only the commands that write or remove the wake's units and have the system manager pick up the change, with the units' contents written out in it.
Toker itself MUST NOT run with elevation.

> [!NOTE]
> The platform shows the command it is asked to authorise, so a script carrying the units in full shows exactly what will be written as root, where elevating Toker would ask for trust in the whole binary.

## Where a boundary falls

A five-hour window's reset falls at the fire time of the request that opened it, floored to a ten-minute grid, plus five hours.

Toker MUST predict a boundary from the time the ping actually fired.
Toker MUST NOT predict it from the slot the ping was scheduled for.

> [!NOTE]
> A ping fires some minutes after its slot, so a prediction made from the slot is ten minutes early whenever the two straddle a grid line.

## Sending a ping

A ping is one minimal inference request sent on a schedule.

Toker MUST tag a ping with the configured ping header, and MUST record the ping flag on its ledger row.
Toker MUST NOT identify a ping by the size of its request or of its usage.

> [!NOTE]
> Most real requests consume less of a window than any threshold that would exclude a ping, so a size test silently reclassifies real traffic.

Toker MUST skip a ping while any measurement in the ledger reports a five-hour reset still ahead on the account being pinged.
Where the ledger cannot answer, Toker MUST send the ping.

> [!NOTE]
> A window that is already open gains nothing from a ping, and the run would record the existing window's reset as its own achievement.
> A wrong skip forfeits the boundary outright, where a wrong send costs one minimal request.

Toker MUST refuse a ping that fires more than ten minutes after its scheduled time.

> [!NOTE]
> Suppressing a timer's catch-up across a restart does not suppress a realtime elapse that passed while the machine was suspended, which fires the moment the machine wakes and would place the day's boundary wherever that happened to be.

## A window belongs to an account

A rate limit window is an account's, as [ACCT](../store/accounts.md#what-is-attributed-to-an-account) specifies, so a ping opens the window of one account and leaves every other account's unopened.

Toker MUST schedule a ping for each account it knows on a provider that anchors windows.

Toker MUST send a ping with the credential of the account it is pinging.

> [!NOTE]
> Choosing which account a ping opens a window for is not routing, which decides where a model goes and never whose window pays for it.
> It is Toker's own request with no client behind it, so the credential it carries is Toker's to choose, which is the one place that choice exists.

A ping run MUST record the account it was sent for.

## A ping is routed like any other request

A ping names a model, and the active policy revision decides where that model goes.

Only a ping-tagged row served by the subscription provider, on the account the ping was sent for, counts as having opened a window.
A ping the policy sent to another provider, or that failed over from the subscription, MUST be recorded as having observed no boundary.

## Recording a run

Each ping run MUST be recorded with the boundary it predicted, the boundary the ledger subsequently reported, and whether the two matched.

A readback MUST consider only rows logged after the ping began.
