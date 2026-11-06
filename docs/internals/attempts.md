# Attempts

One request from a client is not always one request to a provider. A refused
`thinking: disabled` is sent again, and a provider that is down hands the
request to the next candidate of the route. The ledger keeps every send as its
own row (see [ledger-schema.md](ledger-schema.md) for the columns), and the
views built on it have to answer two different questions from the same rows:
how many things did the user ask for, and what did the sends cost. They are
different sums, and mixing them up is the bug this page exists to prevent.

## A logical request and its attempts

A logical request is what the client asked once. An attempt is one actual send
to one backend. The handler mints an `attempt_group_id` when it starts and gives
the first send index zero with reason `initial` (`server/attempt.rs`,
`initial_provenance`). Every further send stays in the group with the next
index, and its reason says why it exists.

Two things add an attempt:

- **Thinking negotiation.** Claude Code turns thinking off on its side calls and
  decides per model whether `disabled` is allowed from a list built into the
  client, and Anthropic has refused it on some models in bursts, with a 400
  whose message names `between_tools`. On the Anthropic-native Messages path
  only (`server/anthropic.rs`, `retry_thinking_off`), toker sends the request
  once more with `thinking` rewritten to `between_tools`, to the same candidate.
  The refusal is recorded as an `error` row at index zero, and the retry is the
  next index with reason `thinking_negotiation`. This is the one place that
  reads an error message, and only for that one fixed word; a refusal without it
  is classified as any other error. It is not a health event: the refusal does
  not open a circuit, because a model that wants `between_tools` is working. If
  the retry fails too, that failure is classified normally and may go on to a
  fallback.
- **Fallback.** A failure that the policy's classification can name, and only
  one, may move the request to the next candidate of the same route. The failure
  classes are closed: `authentication`, `model_not_found`, `rate_limit`,
  `provider_quota`, `overload`, `server_error`, `transport_before_accept` and
  `ambiguous_transport`. Classification reads the provider's native error type
  or code and the status, never message text (`classify_anthropic_error`,
  `classify_openai_error`, `classify_responses_error`), and a failure it cannot
  name has no class and is forwarded to the client as it arrived, because an
  unknown or request-specific failure must not spend another provider's quota.
  The new attempt's reason is the class, so the ledger says what each move was a
  response to.

What fallback may do is decided by the policy, not here: whether the chain has a
next candidate, whether the move crosses a cache boundary the lane's transition
policy forbids, whether a billed candidate may follow a subscription one. When
the resolver (`policy::resolve_after_failure`) or the transition check says no,
the failed response is forwarded and the group ends there. A classified failure
also opens that candidate's health circuit for the time the candidate configures
for that class (none if it configures none), except an `authentication` failure
on a credential the request itself brought, which says nothing about toker's
own. A gate notice is request-global and is not decided again for a fallback
attempt.

### Why fallback only happens before commitment

A fallback has to be invisible to the client, which means it can only happen
before the client has been sent any part of a response. So each attempt is
inspected before its response head is returned: for a streaming response, until
the first complete non-keepalive event has arrived or 64 KiB have been buffered
(`PRECOMMIT_BODY_CAP`), and for an error, its whole body up to the same bound.
The inspection sees an error that arrives as a status, and one that arrives as
the first event of a stream with a 200 head. The bytes buffered are then
replayed to the client unchanged if the attempt is accepted or if no fallback is
chosen. After the head has gone out, a later failure is the client's to see and
is never retried: a second provider cannot continue a half-written answer. A
response that fails this inspection locally (a first frame that is not a valid
event) has no safe body to forward and no class, so it ends the group with a
gateway error.

The inspection is content-free by the same invariant as everything else: it
reads an error's type and code to choose a class, and only the class leaves
the adapter.

## Accounting is per attempt

Every attempt is recorded as it ends, attached to the backend that received it.
A fallback re-aims the record context before the next send: the provider, route
and cost semantics of the new backend, its binding and candidate, its policy
trace, its effective model and the upgrade or compaction move policy made for
it. So the two rows of one request can name different providers and different
models, and each row's `cost_kind` is that provider's own: a failed attempt on
the subscription and a successful one on the API are not one cost.

- A failed attempt is an `error` row. It is never priced and has no usage, but
  it carries its own response's meters, because a 429 is the only evidence of
  throttling the ledger gets. `error_type` is the failure class, or
  `upstream_error` where there was none; for the thinking refusal it is the
  upstream's own error type.
- A completed attempt is a measurement row. Only the attempt that completes
  updates the lane (including the routing pin, which is therefore always the
  backend that really served), teaches the binding learning table, and clears
  the health circuits it had tripped. A failed attempt does none of that, so a
  fallback does not pin the lane to the backend that failed.
- Each send is noted as "recently sent" for its binding and model immediately
  before the transport starts, because a request that reached a provider may
  have written a cache even if it then failed.
- A client that goes away after a send and before any response is committed gets
  an `error` row for the attempt in flight with `error_type = client_cancelled`.
  It is written from the handler's destructor on the blocking pool, since the
  future that was dropped cannot write anything, and it never pins the lane: a
  cancelled attempt did not establish what the cache lives on.

Nothing sums a group into a single synthetic row, and no reader should. The
group's final outcome is its attempt of greatest index; nothing in toker
currently collapses it.

## How the read models group attempts

The rule is the one in [ledger-schema.md](ledger-schema.md): a logical request
is a distinct `attempt_group_id` or a row with none, and everything that is a
quantity of work or money sums the physical rows. In practice:

| Reader | Counts logical | Sums physical |
| --- | --- | --- |
| `Store::count_logical_requests` (the TUI header total, `toker status`, `/_toker/status`) | the ledger's rows by group, with ungrouped rows each counting, whatever their kind | |
| `/_toker/session` and the TUI detail's totals (`SessionSummary`) | `requests`, over measurement rows only | token sums, `billed_total` |
| the same summary's `per_provider` and `per_model` | | each group's `requests` is a count of billed rows, so it can exceed the logical count |
| the TUI window (`tui/model.rs`) | `window_requests`, the per-minute rate and its buckets, a session's `requests`, a lane's request count, the child-role counts | the tokens panel (including its denominator, the number of measurement rows), spend and its breakdown, the error counter |

Two details follow from counting in a window. A group is counted once, the first
time any of its rows is seen in the window, so a group that straddles the
window's start counts once and is never half a request. And the unit of the
averages is the row: a fallback that made two measurement rows contributes two
rows to the cache hit rate, because each really was a prefix read or written.

Everything that reads provider evidence reads it per attempt. The quota fit and
the meter reads take each row's own snapshot, the cache-rebuild walk reads each
measurement row as the lane's observation, and learning takes each completed
attempt's reported model. A group total would be a number no provider ever
reported.

Error-attempt rows are included in the error counter, one per failed attempt.
A request that failed over successfully therefore shows an error in the TUI
though the client saw none. That is the point: the error counter is the
provider-health signal, not the user's.

### `reported`, `sent` and `asked`

The three model identities of [ledger-schema.md](ledger-schema.md) are shown
without filling one from another. The session detail line reads the newest
measurement row of the display session and prints `asked` (`requested_model`),
`sent` (`effective_model`) and `reported` (`raw_model`). A response that did not
name a model prints `reported -`, and in particular is not filled from the
effective model or the normalised `model`: the whole point of showing both is
that they can disagree, and a fallback or an alias is exactly when they do.

The dashboard's session table is the other consumer. Its model column and the
context ceiling derived from it use the pricing-normalised `model` of the main
lane's latest row that named one, never `effective_model`, because a context
window belongs to the model that served and not to the one that was requested.

Everything that learns or prices does the same: it takes the provider-reported
identity, and a row without one teaches and prices nothing.

### Old rows

Rows before attempt identity have no group, so each reads as one logical request
even where it was two sends; the 2026-10-09 to 2026-10-13 thinking retries
recorded only the retry. A view that says "N requests" over a range reaching
back past attempt identity counts attempts there and requests after it, and
should say so rather than present one series.
