# Opencode active-session lane view

Date: 2026-10-10
Status: planned; split from the toolsuite's otherwise-shipped opencode
dashboard stretch goal.

## Purpose

Complete the active-session dashboard in the bundled opencode plugin by
showing the session's live toker lanes. Context occupancy, token/cache
breakdown, billed spend, and provider rows already ship; lane state is the
remaining promised view.

## Data contract

Extend `GET /_toker/session?session=<id>` with a `lanes` array sourced from the
state table, filtered by exact session id. Each entry carries only operational
metadata already kept in a `Lane`: the tool-set digest or a content-free short
identity derived from it, last-response time, prompt tokens, cache TTL, ping
status, last cold-notice time, and sticky model transition when present.

The endpoint computes a content-free state for each lane at query time:
`warm`, `cold`, or `ping`. It also returns the remaining warm duration when
the clock and stored TTL make that knowable. Missing lane instrumentation is
an empty array, never an invented cold lane, and missing fields remain null
rather than zero.

Add a session-filtered store read instead of loading every lane and filtering
in the control handler. Preserve the control header, loopback boundary, and
the endpoint's no-content contract.

## Plugin rendering

Extend the plugin's typed response and sidebar with a `Lanes` block. Render a
compact row per lane with state, prompt size, and warm duration or last update;
show the sticky model transition only when present. Distinguish multiple lanes
without displaying the full tool digest. Ping lanes must be identifiable and
must not look like conversation cache state.

The plugin continues to render no lane block when the array is empty or the
query fails. Its two-second cache remains the only polling cadence, so adding
the block does not create another request loop.

## Implementation changesets

1. Add the indexed session-lane store query and the content-free session API
   representation.
2. Add the plugin response type and responsive sidebar rendering, then update
   its documentation.

When the endpoint and plugin both provide the lane view described here, remove
this plan in its own `unplan:` commit.
