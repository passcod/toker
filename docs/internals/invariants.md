# The invariants, with their history

The design's own list is in `docs/plans/toker-toolsuite.md` under Invariants,
numbered differently (it adds serialisation purity and prefix stability, which
[routing.md](routing.md) covers). This is the list AGENTS.md states, with the
reasons.

1. **Never log or store prompt or completion content.** Only counts, lengths,
   and digests. Bodies are parsed for `usage` and discarded. The compaction
   markers are fixed strings known in advance, and only *whether* they matched
   is recorded: that is a derived fact, like a hash, not a copy. Session labels
   in the TUI are read from the frontend's own transcripts at view time
   (`tui/labels.rs`) and never reach the ledger. `toker import` reports the
   names of ctp fields it did not map, never their values.

2. **Credentials pass through, or are stored where the design says, and are
   never logged or printed.** ctp stored no credential at all: the subscription
   bearer passed through verbatim and that was the whole story. toker serves
   backends that need a key, so the rule became where a key may live and who may
   see it:

   - A request that brings its own credential keeps it
     (pass-through-when-present). The anthropic subscription only ever passes
     through; toker holds no subscription token for it. The anthropic API
     injects `x-api-key` only when the request carries neither `x-api-key` nor
     `authorization`. On a route to openrouter another provider's credential is
     dropped before the stored one is injected, never forwarded: on the openai
     path, and on the anthropic path's `openrouter/` route, where every request
     carries Claude's subscription bearer.
   - One request toker makes itself uses a passing credential: the anthropic
     models listing, which names each model's context window. With no API key
     stored, toker has no other way to read it. While that catalogue is stale,
     a subscription request's bearer is copied into one background GET of
     `/v1/models`, at most once an hour, and dropped when the GET ends. It is
     marked sensitive, and never stored, cached or logged; the cache file holds
     the listing only. `Server::borrow_catalog_credential` has the conditions.
   - The codex backend is the exception: it always signs itself and strips
     whatever `authorization` the frontend brought first. That value is a
     frontend-to-toker arrangement (claude sends a dummy bearer), and forwarding
     it to chatgpt.com would leak it to a third party.
   - A key handed to toker lives in the OS keyring (`secrets.rs`), an env var
     the service can see, or as a literal in `toker.toml`, which setup creates
     0600 for that reason. `toker status` says which source is set, never the
     value. `CodexAuth`'s `Debug` redacts its tokens by hand.
   - Request headers are read by name (`anthropic-beta`, the session and ping
     headers), never captured wholesale, because they carry the bearer.

3. **Accounting must never break a session; only a gate may stop one.**
   Observation rides an already-forwarded byte stream, and a parser failure
   loses a measurement, never a request. The gates are the only exceptions: they
   stop requests deliberately, before forwarding, and every such decision goes
   through `quota::decide` or `cold::decide_cold`. If you find yourself dropping
   a request anywhere else, that is the bug this invariant exists to catch.

   This is not theoretical. ctp's gate first called its marker strip bare on the
   request path; a body with a non-iterable `messages` threw, and because the
   handler was async the rejection was unhandled and the process exited,
   listener gone, every in-flight stream severed. Its post-fetch path was no
   safer: the loop reading the upstream body ran bare for three weeks, and a
   300-second body timeout threw out of it about twenty times, each one a
   restart that took every session's stream with it.

   Rust changes the failure mode, not the rule. A panic in a handler is
   contained to that connection's task rather than the process, but it still
   drops that client's response. A panic while the store's mutex is held
   poisons it; `Store::conn` used to answer every later call with an error,
   which left the ledger dark until a restart, quietly, because recording
   swallows store errors by design. It now recovers the guard (a dropped
   transaction rolls back, so the connection stays consistent). Still, the
   observers are infallible by
   construction and also run under `catch_unwind` (`observe_chunk` in
   `server/anthropic.rs`, the same in `server/proxy.rs`), the sleep-lock
   evaluation runs under one too, and recording logs a store error and moves on.
   Anything new on the request path should fail into "no measurement", never
   into an `unwrap`.

   A mid-stream upstream failure is the one place toker deliberately breaks a
   response: it aborts the client's stream rather than ending it cleanly, so a
   truncated body cannot pass as a complete one. That is the upstream's failure
   surfaced, not accounting's.

   There are two gates. `middleware/quota.rs` stops a session because the quota
   is spent and needs the release marker to continue; `middleware/cold.rs` stops
   one because it is about to rebuild an expired cache, fires once, and re-arms
   itself. Neither is a licence for a third.

   Every ledger row goes through `Store::record_request`, the only writer of
   `requests`, which has no update or delete path. ctp also needed its
   `appendRow` to keep each row in memory, because the outlook measured against
   an in-memory tail and a row written straight to the file was invisible to it
   until a restart. toker's readers query the ledger (`cold::outlook_over` reads
   the newest rows), so that half of the lesson is moot; the single writer is
   not.

4. **Bind `127.0.0.1` only.** The listener binds loopback (`server/mod.rs`, and
   `ListenStream=127.0.0.1:…` in the socket unit). toker answers
   `/_toker/status`, `/_toker/models/merge`, `/_toker/session` and
   `/_toker/shutdown` itself; any other `/_toker/` path is a local 404 and
   never leaves the proxy. Each demands its verb in the `x-toker-control`
   header (`server/control.rs`), and the two POSTs, `models/merge` and
   `shutdown`, also a JSON body, so that a web page, which can reach loopback,
   cannot drive one without a preflight nothing here answers. `models/merge`
   can only add to the model store, for models already served. `shutdown`
   (`toker restart`'s) names the instance `status` reported and only stops it:
   the listener closes, every response under way finishes (or, with `force`,
   is cut), and the process exits cleanly for systemd to start again, so the
   worst a caller can do is a restart. Keep any new control path to the same terms, or
   better, do not add one.

5. **Never guess prices.** Verify against the published pricing page and move
   `catalog::pricing::VERIFIED_ON`; the context-window table has its own
   `catalog::windows::VERIFIED_ON`. Cache-read rates are stored explicitly
   rather than derived as `0.1×` because some models read at `0.025×`, and
   deriving them would overcharge those 4×. A model missing from the table gets
   a NULL cost and a one-time warning, never a number. Codex rows are
   NULL-costed for the same reason: there is no per-token price for its slugs to
   verify.

6. **Inference bodies cross canonical IR; legacy administrative bodies are the
   client's.** Every inference binding deterministically renders its backend
   wire from canonical semantics. Messages middleware still runs before that
   boundary, and only these deliberate transformations may change what the
   canonical parser sees. Count-token, batch, and unmatched administrative
   paths retain the client's buffer except for the applicable routing map:

   - `AnthropicBodyMut::strip_release` removes the release markers
     (`$#$BURN$#$`, and the plan-only `$#$OVER$#$`), strings toker itself
     defined. It runs on every `/v1/messages` request, for every backend and
     whatever the gate's toggle: the marker rule is a frozen public API, and a
     toggled strip would change the cached prefix of every conversation
     carrying one.
   - The `provider/model` prefix strip (`anthropic_sub/`, `anthropic_api/`,
     `anthropic/`, `openrouter/`) is the client choosing a backend in the model
     string; `set_model` writes the rest back.
   - Force-newest's `set_model` changes the model on a request that can lose no
     cache by it, and nothing else, keeping `cache_control` because that
     conversation still has a cache to build. See [models.md](models.md).
   - `cold::retarget_compaction` rewrites a *cold compaction only*: model, cache
     breakpoints, and mid-conversation system messages, together or not at all.
     That one is defensible because a compaction is a dead end: its body is
     never replayed and the client's transcript is untouched.
   - `strip_message_effort` removes the `output_config` a mid-conversation
     `system` message carries to change the reasoning effort, on a backend
     whose model rejects it (`Provider::accepts_message_effort`, today any
     non-Anthropic model on openrouter). The client replays that message every
     turn, so leaving it would 400 the whole session. Text in the same message
     stays; an effort-only message goes.
   - `thinking_between_tools` turns `"thinking": {"type": "disabled"}` into
     `{"type": "between_tools"}` on the one retry that follows an upstream 400
     asking for exactly that (`retry_thinking_off` in `server/anthropic.rs`).
     It waits for the refusal because the refusal is the only evidence a model
     wants it. Only the rendered request's thinking mode changes, and the
     client's transcript is untouched: the next turn sends `disabled` again
     and is judged afresh.
   - `model_map::rewrite_mapped_models` applies an explicit, configured routing
     map as the last stage and changes only model positions. An unmatched or
     disabled map returns the input bytes untouched.
   - Canonical rendering builds a new body even when the protocols match. It
     is pure, so turn N+1 reproduces turn N's backend prefix wherever the
     conversation did not change.

   Dropping `cache_control` needs a second licence on top of a body rewrite, and
   there are exactly two: a **model change**, since caches are keyed per model
   and there is provably none for the new one to lose, or the caller passing
   **`cold: true`**, which asserts the same fact directly for a compaction that
   has no cheaper model to move to. Without one of the two `retarget_compaction`
   declines, because on a warm lane those breakpoints are what earn the free
   read. Neither property holds for an ordinary request, so neither
   transformation generalises. If you are editing any other body position, that
   is the bug this invariant exists to catch.

7. **Never pin a quota weight.** Prices are published and must be verified;
   quota weights are published nowhere and must be *measured*, refitted from the
   ledger on every run (`cold::fit_quota_model`) and reported with the spread
   that says whether to believe them. A hardcoded weight is a guess wearing a
   constant's clothes. A weight whose spread reaches zero is no weight at all:
   `QuotaFit::quota_for` answers `None` for it. See [quota.md](quota.md).

8. **Gate notices are model-visible and byte-stable.** A gate answers with a
   synthetic assistant turn, and the client keeps that text in the conversation
   and replays it on every later request. A notice whose bytes varied with
   anything but its inputs would change a cached prefix after the fact. So
   `notice::render` and the notice composers are pure functions: the clock and
   the timezone are arguments, numbers are grouped by a fixed rule rather than
   the host locale, and the block style's width is frozen. No em dashes in
   notice text: a terminal font renders one two cells wide and the line reads as
   misaligned, so the text uses colons and commas. See [notices.md](notices.md).
