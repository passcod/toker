# Opening a window buys phase, not capacity

A window is anchored to the request that opens it: measured across 38 windows in
ctp's log, `reset = floor(first request, 10 min) + 5h`, with the resets landing
all over the clock (`timers::window_boundary_ms`). `toker ping-window` uses that
to put weekday boundaries at chosen times: the systemd units the wizard installs
wake the machine, hold it awake, and send one tiny `claude -p` request through
toker, tagged with the ping header. The pieces are described in `timers.rs`.

**Do not extend it in the belief that more windows means more quota.** In ctp,
pinging every time the chain lapsed, day and night, doubled the windows opened
over a fortnight (37 to 76) and moved blocked quota from 1.54 to 1.50 of one
window. Demand was 1.9 windows a day against a ceiling of 4.8, so capacity was
never what bound; the walls are bursts inside a window. All a ping can move is
where a boundary falls, and it only helps when one lands inside a burst. For the
same reason a ping is skipped while any measurement in the ledger reports a
5-hour reset still ahead: a window is already open, and a ping would be recorded
as if it had opened one. Anything the ledger cannot answer resolves to "ping",
because a wrong skip forfeits the boundary.

**The best slot is not identifiable from a fortnight.** Sweeping ctp's morning
ping across half-hours gave 1.20 at 07:30, 1.39 at 08:30, 1.26 at 09:00, 1.48 at
09:30, and 1.66 at 06:00, worse than not pinging at all, against a baseline of
1.54. Adjacent slots swing wider than the whole effect, because the chain is
serially coupled and shifting one boundary propagates through every later day.
The slots are chosen for where the *schedule* holds (the 12:30 reset went
unclaimed on 4 of 13 weekdays, for up to 393 minutes), never for a quota score.
This is the quota fit's "presence is not identifiability" wearing a clock face.

## Bugs already paid for

Each is one of the repo's existing mistakes in new clothes:

- **Identifying a class of thing by its magnitude.** ctp's first simulation told
  synthetic pings from real requests with `q > 0.001`, which silently dropped
  1,441 *real* requests that had hit the wall (19,564 of 24,573 requests carry
  under 0.001 of a window each) and understated blocked quota by 32%. Two
  scripts disagreeing is what caught it. Tag the thing; never infer it from
  size, exactly as `summarising` and the cache-TTL tiers require. toker tags
  pings with `x-toker-ping` (the configured `ping_header_name`), and the row
  carries `ping: true`.
- **`Persistent=false` does less than it reads like.** It suppresses a systemd
  timer's catch-up across a user-manager restart, and nothing else: a realtime
  elapse that passed while the machine was suspended fires the moment the lid
  opens. The unit alone would therefore have opened a window at whatever time
  the laptop woke and put the day's boundary there, silently. The lateness guard
  (`LATENESS_LIMIT_MINUTES`) is in the verb for that reason: a ping more than
  ten minutes past its fire time refuses.
- **Flooring the slot instead of the fire time.** toker's first version
  predicted the boundary from the slot, which is 11 minutes before the ping
  fires (`PING_DELAY_MINUTES`), and so predicted a boundary 10 minutes early
  whenever the two straddled a grid line: 07:20's window "ending 12:20" when the
  07:31 ping opens one ending 12:30. `window_boundary_ms` takes the fire time.
- **A fixture dated in the future.** The readback asks only about rows logged
  after the ping started, so that a ping which changed nothing cannot report the
  *previous* window's reset as its own achievement. ctp's first end-to-end run
  appeared to do just that, and the cause was a synthetic row stamped a day
  ahead, which sailed past the cutoff. Fixture timestamps are relative to the
  run's own now.

Each run is recorded in the `pings` table with the boundary it predicted, the
boundary the ledger then reported, and whether they matched, so whether the
schedule actually held is a query, not a memory.
