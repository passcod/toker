---
id: DLIFE
---

# Running the service

Toker runs as a long-lived service behind a socket its supervisor holds.
This spec covers how it starts, how it is replaced without cutting a response, and the lock it holds to keep a machine awake while sessions are live.

## Binding

Toker MUST bind loopback only.

Toker MUST accept a listening socket from its supervisor where one is handed over, so that connections arriving while no instance is running queue in the kernel rather than being refused.

Each running instance MUST have an identity distinguishable from its successor's.

> [!NOTE]
> Uptime cannot tell two instances apart: one asked to stop a second after it started reads like its successor.

## Replacing the running instance

Stopping the process outright ends every response still streaming through it mid-turn.
That is safe, because the socket keeps accepting and clients retry a cut stream, but each retry re-sends that turn in every session at once.

Replacing an instance without cutting a stream MUST follow this sequence.

1. Poll the service's status until the in-flight count reads zero on several consecutive polls.
2. Ask the service to shut down, naming the instance that status reported.
3. The service drains: it stops accepting, lets every response under way finish, closes idle connections, and exits successfully.
4. The supervisor starts the next instance, which accepts the connections that queued meanwhile.
5. Poll status until a different instance answers.

The in-flight count MUST count only exchanges already under way.

> [!NOTE]
> An agent's next request follows its tool calls after a short gap, so a single idle reading can fall between two requests of one busy turn.
> Several consecutive quiet polls distinguish a gap from a lull.

Where no quiet moment arrives within the wait, or status does not answer at all, Toker MUST proceed to the forced stop rather than give up.

> [!NOTE]
> A session that never idles is not a reason to keep running the old binary.

The drain MUST have no deadline of its own.

> [!NOTE]
> Its whole purpose is never to cut a stream, so the forced stop bounds a drain that does not end.

Where the next instance has not answered within the wait, Toker MUST ask again with a force flag, which MUST stop waiting for connections and return as though the drain had completed, so that the lock is released the same way and destructors run.
Responses still under way are then cut, and clients retry them.

After a second failure Toker MUST report rather than force again.

A shutdown request MUST name the last instance that answered status, so a service that never answered has nothing to address and the operator is told to use the supervisor directly.

Waiting for a quiet moment does not keep streams whole; the drain does that alone.

> [!NOTE]
> The wait keeps new requests from queueing behind a long one, since nothing accepts them between the listener closing and the old process exiting.

A service started by hand has no supervisor behind it.
The drain still works, nothing starts the next instance, and Toker MUST say so when its wait runs out.

Setup MUST take this same path where it found an active listener on the configured port.

> [!NOTE]
> Reinstalling a unit does not change an already-running process, so without the restart setup would verify the old in-memory configuration.

## The sleep lock

While a session is live, being a lane whose cache would still be warm or a request in flight, Toker MUST hold a lock preventing the machine from sleeping on idle.

That lock MUST prevent only an idle sleep, and MUST NOT prevent the operator suspending the machine.

> [!NOTE]
> A lock against sleep itself refuses the operator's own Suspend, and does not stop a lid close anyway, since the platform ignores such locks for that by default.
> Which lock the desktop honours differs between environments, so the mechanism is chosen per environment.

A ping MUST NOT hold the lock.

> [!NOTE]
> A machine woken to ping would otherwise stay up for the ping's own cache lifetime with nobody at it.

The flag marking a ping MUST survive the lane reseed after a restart and the lane prune.

> [!NOTE]
> The first restart after a ping would otherwise turn its lane back into a session.

Toker MUST re-evaluate the lock against the wall clock on a periodic tick, and MUST NOT rely on a timer.

> [!NOTE]
> A monotonic clock stops during suspend, so a timeout aimed at the expiry slips by however long the machine was shut.

A wake alarm MUST be armed only on the way into suspend, and MUST NOT be armed on power off.

> [!NOTE]
> A shut-down machine, which means a travelling one, must not boot in a bag.

The lock MUST be held by a process that watches the service and ends with it however the service dies.

A failure to take the lock MUST back off before retrying.

> [!NOTE]
> Evaluation runs on every response, so a missing session bus would otherwise spawn a process per request.

Evaluation MUST NOT be able to fail a request.

A row MUST be written only when the held state changes.

> [!NOTE]
> The row separates a lock released because sessions went quiet from a lock that quietly failed.

A drained exit MUST end the lock itself and MUST NOT write a row.

> [!NOTE]
> No lock then outlives the process, and nothing afterwards takes it back.
> A release row would say the sessions went quiet, and at exit they need not have: the next instance reseeds its lanes and writes the hold again.
