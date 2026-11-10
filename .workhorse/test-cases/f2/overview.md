# Requests in flight on the dashboard

Scenarios that show a request on the dashboard from its arrival, and give sessions only a gate or a failure touched a row.

## Store

- [x] The migration on a copy of the real store keeps every row and leaves `started_ms` absent on them.
- [x] An in-flight record round-trips through its stages, and clearing removes every record.
- [x] Freshness reads the newest request, in flight or recorded, falling back to when a row was written.

## Server

- [x] A record exists while a response streams and is gone only after its row lands (verifies spec: SATT#while-a-request-is-in-flight).
- [x] A ping has a record and no sleep-lock count.
- [x] A client that hangs up leaves in flight only after its cancellation row is written.
- [x] A fallback moves the record to the next attempt, and a clone of the guard holds it until the last drops.
- [x] A record a crash left behind is cleared when the service starts.
- [x] A gate row carries the requested and effective model, and when handling began (verifies spec: SLEDGR#fields-that-need-care).
- [x] The cold, cold-recap and cold-quiet rows carry both models.
- [x] A request the policy cannot route has its requested model and no effective one, and is not in flight once its row lands.

## Dashboard

- [x] A session with only a gate row, only a failure, or only a request in flight each get a row (verifies spec: UTUI#sessions).
- [x] `reqs` counts logical requests: two attempts are one, and an in-flight group with no rows yet is one.
- [x] The failure mark sets, clears on a later success, and ignores a failure a fallback recovered from.
- [x] A group still in flight neither sets nor clears the failure mark, and counts once.
- [x] A served model beats a routed one; with none served the routed one is marked `~` and dimmed with its ceiling.
- [x] The idle cell shows `held`, `sent` or `recv` with its age, and `+N` for more in flight; the longest-running leads.
- [x] A request in flight counts in the minute it began, and a request that began before the window counts in its first minute.
- [x] Rows without `started_ms` are placed by when they were written.
- [x] Staying quiet: a window with nothing in flight aggregates as it did, and a quiet frame shows none of the new marks.
- [x] A child's request in flight does not move its parent's message count.
- [x] The session detail lists each request in flight with stage, age, provider, model and attempt, saying billed where the binding bills.
- [x] The legend teaches the new marks.
- [x] Snapshots of the sessions panel at 80, 100 and 120 columns, and of the detail popup.

## By hand

- [ ] Send a message through Claude Code and see the session's row show `sent` before inference ends.
- [ ] Gate a session and release it from its row without it ever having completed a turn.
