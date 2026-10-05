# Forecasting a window

The burn ladder in `middleware/cold.rs` (`burn_rate`, `burn_rate_samples`,
`project_to`) projects when a meter runs out. The cold outlook runs it over full
rows and the TUI's quota panel over its narrow meter rows (`tui/quota.rs`), so
there is one ladder, not two. Utilisation is cumulative and arrives on every
response, so the rate is just the spread between two readings over the time
between them, and every trap is in choosing those two readings. All of these
were paid for in ctp:

- **A window is not a window.** Utilisation restarts at zero on a roll, so a
  lookback spanning a reset reports the quota refilling. Clamp to the reset
  value the latest reading carries.
- **The reset field does not catch every restart.** The overage meter's reset
  sat at the same monthly instant while its utilisation fell 0.64 → 0.55, so
  twelve days of readings counted as one accounting period and the burn was
  diluted into "on track" while the meter was actively being spent. A cumulative
  meter that goes *down* has restarted whatever the field says.
- **The figures are decimal, the arithmetic is binary.** `0.57 - 0.55` is
  `0.019999999999999907`, which fails a bare `>= 0.02` noise floor. That one
  comparison sent a meter climbing two steps in six minutes down to a five-day
  bound. Compare with an epsilon (`EPS`).
- **Measure to now, not to the last reading**, or a rate keeps claiming you are
  burning after you have stopped.
- **A projection that spans nights must be measured across one.** The same
  ladder that serves a 5-hour window measured 3%/hour of the *weekly* meter
  during an evening and projected it through the nights to come, putting the
  wall on Thursday; the last 48 hours, the window so far, and an explicit
  active-hours model all said Sunday, within ten hours of each other. So when
  the reset is over a day away the measured span must be at least a day
  (`min_span_for`), which carries the duty cycle for free, with nothing to
  define or model. Judge that on the span **measured**, never the rung asked
  for: across an overnight gap a 24-hour rung found a reading 5.3 hours old and
  reported an evening as a day.
- **The start of a window is a reading of zero.** Where the length is known
  (5-hour, 7-day; `MeterSpec::length_ms`) reconstruct it; otherwise the first
  day of every weekly window has under a day of history and can only shrug. The
  overage window's length is *not* known (its reset is monthly but its
  utilisation has restarted elsewhere), so nothing may assume where it began.
- **Utilisation is not monotonic in ledger order.** Requests run concurrently
  and finish out of order, each response carrying the figure from when it was
  served: 0.29, 0.29, 0.23, 0.29, 0.30 within five seconds, observed. Take the
  running maximum as the current value. A fall is only a restart if it never
  returns *and* has held longer than reordering can explain: request duration
  was p99.9 164 s and max 284 s over 20,583 requests, so ten minutes
  (`REORDER_MS`). Without the second half, a dip in the newest reading restarts
  the window trivially, because nothing follows it to return.
- **A meter that has not moved is not a meter you know nothing about.** The 1%
  quantisation gives its rate a ceiling (`Burn::Bounded`); if even the ceiling
  does not reach the wall before the reset, "on track" is a measurement.
  Reporting "estimating" for everything unmoving would make the overage line
  permanently useless.

## Totalling a span

The TUI's span totals (the `spent` figures) answer the other question: what a
fixed span of wall clock cost. They have one trap of their own on top of every
trap above. **The baseline is the reading before the span, not the first one
inside it.** The first in-span reading already includes whatever that request
consumed, so measuring from it drops a request's worth of spend; it is the lane
rule's predecessor trap wearing a clock face. Where no earlier reading exists,
the period's own start stands in *only* where that start provably falls inside
the span (a window that opened before it may have been spent against
beforehand), and otherwise the answer is a floor and says so.

The totals also drop every row carrying a `kind` (`readings_of` in
`tui/quota.rs`), which the burn deliberately does not: a `blocked` row's
`rate_limits` is an old figure wearing a fresh timestamp, harmless as the newest
reading of a rate and ruinous as the baseline of a total. Exclude by the
presence of a kind, never by listing kinds: listing them is how a `released` row
once slipped into ctp's quota fit.

## A reading is only about its own window

Once a window's reset passes, a reading says nothing about the current one, and
nothing replaces it until traffic resumes, so `quota::exhausted_meters` takes
`now` and ignores meters whose window has ended (`quota::expired`). Without that
the gate never reopens (a blocked request never refreshes the meters that would
clear it) and the live view contradicts itself ("gated · window rolled over").
ctp's fixtures had absolute reset timestamps from a past capture, which read as
permanently expired and quietly turned every block case into a forward. Keep
fixture timestamps relative to the run's own now.

Against an armed gate the wall is `quota::THRESHOLD`, not 1.0. Import it
(`cold::outlook_target` does) rather than restating it, so the view and the gate
cannot drift. Whether the gate was armed is recorded per row (`gate_on`),
because the toggle lives in the service's config and a reader of the ledger
cannot see it; the TUI marks a gate state it had to assume for rows that predate
the field.
