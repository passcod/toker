# The sleep lock blocks the timeout, never the user

While `awake::decide_awake` says a session is live (a lane whose cache would
still be warm, or a request in flight), the service holds an idle-sleep lock,
and on schedule a system timer wakes a suspended machine so the ping can run
(see [pinging.md](pinging.md)). It is Linux only; ctp's macOS paths (`pmset`,
`caffeinate`, launchd) were not ported, and `toker wake-arm` survives as a
documented no-op because systemd owns the wake. Four things about it are easy to
get wrong, all learned in ctp:

- **Idle-only, on purpose.** A logind `sleep` block lock looks like the obvious
  tool and is wrong twice: it refuses the user's own Suspend, and it does not
  stop lid-close anyway, because `LidSwitchIgnoreInhibited=yes` is logind's
  default. On GNOME the lock is the session manager's "suspend" inhibitor
  (`gnome-session-inhibit`), because that is what gsd-power reads; elsewhere it
  is `systemd-inhibit --what=idle --mode=block` (`awake::inhibit_command`).
  Whether gsd-power also honours a logind idle lock could not be confirmed,
  since the Caffeine extension held the answer the whole time.
- **A ping is not a session.** The pinger tags itself with the ping header, read
  by name like `anthropic-beta`, and ping lanes never hold the lock. Without
  that, a machine woken to ping stays up an hour for the ping's own cache with
  nobody at it. The flag has to survive the restart reseed (`lanes_from_rows`)
  and the prune as well, or the first restart after a ping turns it back into a
  session.
- **Wall clock, not timers.** tokio's intervals run on a monotonic clock, and
  that clock stops during suspend. A timeout aimed at the expiry would slip by
  however long the lid was shut, so the service re-evaluates on a 60-second tick
  (`AWAKE_TICK_MS`) against the wall clock, and the `hold` verb measures its
  span the same way.
- **Wake from suspend only.** `WakeSystem=true` arms an alarm the kernel writes
  into the RTC on the way into suspend and not on poweroff. That is what keeps a
  shut-down machine, which means a travelling one, from booting in a bag. It
  needs `CAP_WAKE_ALARM`, so the wake timer is a system unit, the only
  root-level piece toker has, and its service is `/bin/true`; everything that
  touches the session bus stays in user units.

The lock is a detached child that watches the service's PID (`tail --pid=… -f
/dev/null`) in its own process group, so it lives exactly as long as the service
however the service dies. Evaluation runs on every response, so a missing
session bus would spawn a process per request; a failed take backs off for five
minutes (`RETRY_MS`). A row is written only when the held state flips (`kind =
'awake'`), because the row is what separates "released because the sessions went
quiet" from "the lock quietly failed". The whole evaluation runs under
`catch_unwind`: the lock is not worth a request.

A drained exit (`toker restart`, see [routing.md](routing.md)) kills the child
itself and writes no row (`AwakeState::shut_down`). The PID watch would end it a
moment later anyway; killing it first means no lock outlives the process, and
nothing after it, the 60-second tick included, takes it back. No row, because a
release row says the sessions went quiet, and at exit they need not have: the
next instance reseeds the lanes and its first evaluation writes the hold again.
