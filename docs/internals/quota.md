# What the rate-limit window actually meters

A 5-hour window on the anthropic subscription is not a dollar amount and not a
token count. Consumption tracks **fresh prompt tokens** (uncached input plus
cache writes) and **output tokens**, weighted per model. **Cache reads carry no
measurable weight**: about 97% of the token volume and roughly 60% of the bill,
and free against quota. Forcing them any positive weight makes the fit
monotonically worse. That is ctp's measurement, and the reason a cache rebuild
matters more to quota than to spend: rebuilds were ~40% of quota against ~20% of
spend, so optimising the bill optimises the wrong thing.

toker keeps the fit only because the cold gate's outlook prices a re-read with
it (`cold::fit_quota_model`, `QuotaFit::quota_for`). ctp's `--quota` diagnostics
surface (the worst window, the unattributed bound) is not ported, and there is
no `toker report`.

## Two traps in fitting it

Both were paid for in ctp:

- **Fit between windows, never within one.** Inside a single window every
  candidate (cost, tokens, request count, request bytes) accumulates together
  and scores R² ≈ 0.99. The fit quality tells you nothing; only whether the
  slope survives across windows with different model mixes does.
- **Presence is not identifiability.** A model group whose volume moves in
  lockstep with another's has no weight the data can separate, and a
  non-negative least-squares fit will cheerfully hand one of them the other's
  weight: in testing it gave a 10%-volume rider a weight 10× too large and
  zeroed the group carrying seven eighths of the traffic. The fit measures
  lockstep directly (`MAX_COLLINEARITY_R2`) and folds the smaller group into the
  larger, which then lends it its weight as an upper bound. Do **not** test this
  from each group's *share* of a window: a dominant group's share is
  near-constant precisely because it dominates, so a share test demotes exactly
  the group that is best measured.

A group seen in too few windows has no weight, and `quota_for` answers `None`
for it, never zero: zero would read as "this traffic is free".

## A weight whose spread reaches zero is no weight

Every weight carries its spread: the least and greatest value it took across
leave-one-window-out refits (`GroupWeight`, `Spread`). Because the solver is
non-negative, a refit that reaches zero is one in which the weight could be
dropped and the windows still explained, so the point weight is whatever the
other columns left over, not a measurement. `Spread::separated` says so, and
`quota_for` refuses a fresh weight that is not separated, which leaves the
outlook blind and lets the notice fire.

That rule was added after the live ledger showed it. Over the whole ledger,
`claude-opus-5-5`'s fresh weight fitted at ~0.009 a million with a spread of
0.000–0.018, while live re-reads of 260k and 434k cache-written tokens each
moved the 5-hour meter about two points: five to eight times the point figure,
which had priced each re-read at a fraction of a point and kept the outlook
quiet on 255k and 489k rebuilds.

## Price at the top of the spread

A separated weight is still priced at the top of its spread, not the point fit.
Over the 7-day window `claude-opus-5-5` fitted 0.029 (0.014–0.055) and still
priced those re-reads at a third of what the meter moved. The outlook decides
whether to stay quiet, so an underestimate costs a missed warning and an
overestimate costs one extra notice; the cautious end is the right one to be
wrong at. The notice says "up to about" for that reason.

If you change what the outlook does with a weight, check it against a re-read
whose meter movement you can see in the ledger: a figure that only agrees with
the fit agrees with itself.

## Allowances outlive their window only as dead rows

A release grants an allowance per exhausted meter, keyed by that meter's
reported reset (`quota::grant_for`), and the gate honours it only while the
meter still reports that reset. Once the reset passes, the meter is not
exhausted (`quota::expired`) and the next window reports a new reset, so the row
can never match again. Enforcement needs no prune; the table does.

ctp dropped dead allowances whenever it loaded or saved `allowances.json`. toker
deletes every row whose reset is at or before now (`Store::prune_allowances`) on
the server's 30-second state-prune tick, beside the lane prune, starting at
startup. That is the gate's own expiry rule with no slack, so it removes nothing
the gate could still honour. The column is `NOT NULL` and a grant is recorded
only for a reported reset, so there is no unknown-reset row to keep; a reset far
in the future is kept. The gate reads one session's rows by primary key
(`Store::load_session_allowances`) rather than the whole table, so neither the
table's size nor the prune's cadence lands on the request path.

## Console events

The service logs a quota event when a meter-source backend's reading crosses
80%, 90% or 100% of its short window (`info`, then `warn` at 100%), when the
binding claim changes (`warn`, with the overage utilisation), and when the set
of per-window statuses other than `allowed` changes to a non-empty one
(`warn`). That is ctp's `noteQuota`, read with `journalctl --user -u toker`;
`server/quota_events.rs` has it. Each fires once: a threshold re-arms when the
window's reset changes or utilisation drops below 50%, and a status set is
announced once per distinct set. The statuses are not enumerated: ctp had only
seen `allowed`, and the ledger has since recorded `allowed_warning` and
`rejected`. Replayed over the ledger's month of anthropic_sub readings, the
rules produce about five events a day.

The latches are in memory, one per backend. ctp started each process with them
empty, so a restart at 85% announced 80% again. toker seeds a backend's latch
from the meter snapshot the store kept from the previous process, read on the
first reading after a restart and before that reading replaces it, so a restart
repeats nothing already said about the same window. A stored snapshot whose
window has ended seeds only the claim. A reading that carries no status at all
leaves the status latch alone, where ctp cleared it and re-announced the same
set on the next reading that had one.
