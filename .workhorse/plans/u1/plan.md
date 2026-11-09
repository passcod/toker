# Copy overage utilisation into the monthly header

Add `anthropic-ratelimit-unified-overage-period-monthly-utilization` to forwarded responses that carry `anthropic-ratelimit-unified-overage-utilization` and lack it, as the canonical rendering spec's Response headers section requires.

## Approach

Every forwarded response builds its client-facing headers through `response_headers()` in `src/server/proxy.rs`: the Messages inference path, the unmatched passthrough paths, the translated route onto a Messages binding (`anthropic_target.rs`), the Chat proxy and the OpenRouter Responses path.
Doing the copy there covers every path, status and streaming mode with one change, and it can only fire where the source header arrived.

The meters are parsed from the upstream header map (`note_meters` and `parse_rate_limits` in `anthropic.rs`), and `response_headers()` builds a new map, so the ledger never sees the added header.
That holds by construction, but a test pins it.

Toker's own answers (quota block, cold hold, not-configured) are built without `response_headers()`, so they get no copy and need no change.
The codex path builds its responses without `response_headers()` too; codex never sends the source header, so it is left alone.

## Build

- [ ] In `response_headers()`, after the copy loop: when the upstream map has the overage utilisation header and not the monthly one, insert the monthly header with the same `HeaderValue`, with a `// spec: WCANON#response-headers` comment and a short why (Claude Code drops the overage figure from its rate limit event while overage is drawn)
- [ ] Name both header names as constants beside `response_headers()`

## Tests

- [ ] Unit tests in `proxy.rs`'s test module for `response_headers()`: copied when only the source is present, byte for byte including a non-numeric value; a provider-sent monthly value is kept; neither header present adds nothing
- [ ] `tests/server_anthropic.rs`: the `metered()` helper already sends overage utilisation; assert the monthly header on a non-streamed and a streamed Messages response, on the 401 error path, and on `count_tokens`
- [ ] `tests/server_anthropic.rs`: in the quota-block test (`a_spent_meter_blocks_with_a_synthetic_200_and_never_reaches_upstream`), assert the synthetic response carries neither header
- [ ] `tests/server_anthropic.rs`: assert the stored meter snapshot on a row has no monthly key (the existing `expected_rate_limits` comparison may already pin this; confirm it compares the whole snapshot)
- [ ] Find the test serving a Chat client from a Messages binding and assert the copy there, or add one beside the existing translated-route tests
- [ ] Tick the covered cases in the test cases file

## Verify

- [ ] `cargo clippy --all-targets` and `cargo fmt --check`; leave the full test run to CI
- [ ] `cargo install --path . && toker restart`, then by hand: with overage drawn, a Claude Code session through Toker reports `overagePeriodMonthly` on its rate limit events
