# Measuring at the right granularity

Instrumentation that cannot resolve the phenomenon looks identical to
instrumentation showing the phenomenon is absent. ctp's first two failures here
were correct implementations of the wrong granularity: a 2 KiB prefix ladder
measuring a region where changes are 7 bytes, and session-level grouping where
the cache works per lane. Neither errored; both produced confident, useless
output. The ladder toker stores (`ir/anthropic.rs`: 8 KiB rungs, then 8-byte
steps over the last 256 bytes and 64-byte steps to 1 KiB) is the corrected
geometry, and the TUI re-derives rung offsets from the same constants. Capture
checks a baseline's rung counts against that geometry before comparing, so
rungs cut by ctp's older ladders are never read as today's. The ladders are kept
only where a lane begins or its system prompt changes; see
[ledger-schema.md](ledger-schema.md).

When adding a probe, ask what size the thing you are looking for is.

A third, from the cold gate: a lane's cache TTL is not the tier of its latest
write. A warm turn writes a small delta at the 5-minute tier on top of a prefix
written earlier at the 1-hour tier: measured, `write5m` 5,912, `write1h` 0,
`cacheRead` 158,241. Reading the delta's tier as the prefix's lifetime marked
the whole lane 5-minute and fired the notice at five and six minutes idle, on
caches that were plainly still live: the request after one of them paid 87 fresh
tokens out of 179,000. The lifetime belongs to the longest-lived breakpoint, so
`lanes::lane_ttl` is sticky. Nothing errored; pointing ctp's cold view at the
real log is what caught it.

Relatedly, do not ask a row whether a cache was warm. One row folds several API
iterations, and the second reads the cache the first one wrote, so a total
rebuild still shows cache reads: judged that way, a 27-hour resume read as warm.
Ask instead whether the rebuild a notice warned about actually followed, which
is both answerable and the question the user has.

The same goes for the cache-write split. The response's 5-minute/1-hour
breakdown may be missing or may not add up to the total; the observer charges
any unexplained remainder to the 1-hour tier, the expensive one, so the estimate
errs high, and marks the row `ttl_split_known = false` (`observe/anthropic.rs`).
`usage_presence` records which metrics the response actually carried, so a
reader can tell a reported zero from an apportioned one.

## Verification, and its limits

`cargo test` is fast, spends no quota, and asserts hand-computed figures. The
server tests run the real router against mock upstreams that replay captured
traffic and record every byte sent, and ctp's own decision fixture is vendored
and replayed against the ports (`tests/node_reference_parity.rs`). Run it before
committing.

But know its limits. Three real bugs shipped past ctp's green smoke test in one
day: the test exercised markers that had been invented against bodies that had
been constructed, and confirmed the code did what had been written. What caught
all three was pointing the tool at live traffic and noticing a number that was
obviously wrong. toker repeated this: the quota weight that kept the cold
outlook quiet on 255k and 489k rebuilds passed every test, and was caught by
comparing the outlook's price with what the meter actually moved (see
[quota.md](quota.md)).

So: **run new analysis against a copy of the real ledger and sanity-check the
output** before believing it. Copy the database (with its `-wal` file, or via
`sqlite3 … ".backup"`) rather than pointing new analysis at the live file. If a
session reports 16 compactions and you know it compacted once, that is the bug.

Two habits worth keeping:

- **Test that it stays quiet when it should.** A meter with no readings renders
  no quota section at all, not a row of zeros. Every instrumentation bug so far
  made the tool *more* confident (a missing cutoff produced events, an unknown
  read as false produced a verdict, a session-wide peak produced collapses).
  None could have produced a blank screen.
- **Absence of instrumentation must never read as absence of the phenomenon.**
  Views say when a range predates a field rather than reporting a silent zero.
  See [ledger-schema.md](ledger-schema.md).

## Control bytes in source

No raw control bytes in source. ctp's proxy carried a literal NUL inside the
tool-name join rather than the `"\0"` escape, which made ugrep (what `grep` is
wired to in some agent sandboxes) classify the whole file as binary and skip it
with `-I`, silently: no match, no message, exit 1. toker's `tools_hash` joins on
the `"\0"` escape, which produces the identical string, so lane keys carried
over from ctp unchanged.
