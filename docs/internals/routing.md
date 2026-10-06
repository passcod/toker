# Frontends, backends, and the bytes in between

ctp had one frontend and one upstream, and forwarded every path but its control
one. toker has several of each, so most of what is new here is about which route
a request takes and what that route may do to it.

## Protocols and backends

A frontend speaks a protocol; a backend is a provider of one. One loopback port
serves them all, because the paths do not collide:

| Protocol | Paths | Backends |
| --- | --- | --- |
| Anthropic Messages | `/v1/messages`, `/v1/messages/count_tokens`, `/v1/messages/batches…` | `anthropic_sub`, `anthropic_api`, `openrouter` (by prefix only), `codex_sub` (translated) |
| OpenAI Chat | `/v1/chat/completions`, `/v1/models` | `openrouter` |

A backend is enabled by its `[providers.X]` block's presence in `toker.toml`.
Each protocol has a default (`default_backend_anthropic`,
`default_backend_openai_chat`), which bare model names go to. A model string can
name its backend per request: `anthropic_sub/…`, `anthropic_api/…`,
`anthropic/…` (the protocol default) and `openrouter/…` are stripped and routed
(`strip_anthropic_prefix` in `server/anthropic.rs`, `strip_provider_prefix` in
`server/proxy.rs`; `openrouter/` on both protocols), and the row records both
`requested_model` and `effective_model`. A prefix naming a backend whose block is absent is answered
locally, never sent to the default with the prefix still on. A protocol with no
enabled backend answers its routes with a not-configured error in that
protocol's own shape, carrying the `x-toker-not-configured` header, and reaches
no upstream.

Every path the route table does not match is forwarded to the default anthropic
backend (`anthropic::unmatched`), as ctp forwarded everything, except the
`/_toker/` namespace, which never leaves the proxy.

The OpenAI Responses frontend (`/v1/responses`, for driving toker from the codex
CLI) is designed but not served yet; `proto/openai_responses.rs` is a
placeholder.

### What differs per backend

| Backend | Auth | Meters | Cost |
| --- | --- | --- | --- |
| `anthropic_sub` | the client's own OAuth bearer, passed through | `anthropic-ratelimit-*` headers, the quota gate's only source | `plan_equivalent` (list price on a subscription) |
| `anthropic_api` | `x-api-key`, injected only when the request has none | none (its RPM headers are not quota meters and must never overwrite the gate's snapshot) | `estimated` |
| `openrouter` | stored key, injected only when the request has none; an Anthropic credential is dropped first | none | `billed`, from `usage.cost`, on both routes |
| `codex_sub` | always toker-signed from `~/.codex/auth.json` | `x-codex-*` headers, stored per backend, not gated | NULL: no per-token price to verify |

The three cost kinds are never conflated (`CostKind`).

Meters are fed from **every** response of a meter-source backend, not only the
accounted ones: a 429 or a background call still reports them, and a gate fed
only from accounted rows goes stale.

The codex login is shared with the codex CLI, which refreshes the same file on
its own schedule. `providers/codex/auth.rs` re-reads it right before every
refresh and adopts the CLI's tokens if they moved; the race that remains is
documented there. That refresh is the one write toker makes outside its state
dir, and it renames over `auth.json`, so it needs the whole directory. The unit
runs under `ProtectHome=read-only`, so setup adds that directory as an optional
`ReadWritePaths=-` entry when codex_sub is enabled (`extra_writable` in
`setup/wizard.rs`). Before that, every refresh from the service failed to
persist. A unit written by an older setup still lacks it: re-run `toker setup`.

## The `/f/<frontend>` prefix

Every route also answers under `/f/<name>`. The router strips the prefix before
matching (`strip_frontend_prefix` in `server/mod.rs`) and keeps the name, which
picks the frontend's notice style from `[notices]` and is recorded on the row's
`extra.frontend`. Setup writes the prefix into each frontend it patches
(`/f/claude`, `/f/workhorse`); an unprefixed request is an unknown frontend and
gets the default style. Names are lowercase ASCII letters, digits, `-` and `_`
(`config::is_frontend_name`), and a `[notices]` key outside that set could never
match, so it is refused.

The prefix is also why setup's wiring check has two halves (`setup/verify.rs`).
An upstream's verdict on a prefixed usage path cannot prove the prefix was
stripped: a toker that predates prefixes forwards `/f/claude/v1/messages`
upstream as it stands and relays the upstream's 404, which is exactly what a
frontend patched to that prefix would then get on every request. So the check
also asks for toker's own status through the prefix, which only a toker that
strips it can answer.

## OpenRouter models in Claude Code's `/model` picker

OpenRouter serves the Anthropic Messages wire itself, at `…/api/v1/messages`,
which the openrouter provider's `endpoint` already maps `/v1/messages` onto. So
an `openrouter/<id>` model on the anthropic frontend is a same-protocol
byte-forward with the prefix stripped. The client's own credential is Claude's
OAuth bearer, meant for Anthropic: `OpenRouter::strip_foreign_credentials` drops
any `sk-ant-` bearer and `x-api-key`, and the stored openrouter key goes in its
place. Nothing of the subscription's reaches openrouter.ai.

Two rewrites stay off this route (`picks_from_anthropic_catalogue`): the
compaction retarget and force-newest both pick a bare `claude-*` id from the
Anthropic catalogue, which on openrouter would move the conversation off the
model the user picked. Codex keeps them, because its model map turns those ids
into codex ones. The quota gate is subscription-only already; the cold gate
runs, and since openrouter is no meter source its notice says the re-read is
billed.

What the endpoint does, measured through toker on 2026-10-06:

- It reports the billed cost as `usage.cost`, in USD, beside a
  `cost_details` breakdown: on the plain body's usage, and on the final
  `message_delta`'s when streamed (`message_start` carries none). The figure
  matched the listing's per-token prices exactly. Rows record it as `billed`;
  a response without it records no cost, never an estimate from the
  catalogue, which prices Anthropic's own API and not openrouter's providers.
- The response names its upstream in a top-level `provider` (on
  `message_start`'s message when streamed), recorded as
  `extra.serving_provider`, as the chat route does.
- `/v1/messages/count_tokens` is not served: openrouter answers 404, passed
  through, and the row is an error row.
- A request shaped like Claude Code's (its beta flags including the OAuth
  one, `thinking` with a budget, `output_config.effort`, a
  `context_management` edit, `cache_control` on the system prompt and the
  last message, and a tool) succeeded with a `tool_use` stop on Z.ai's GLM
  5.3 Flash, OpenAI's GPT-6 Luna and DeepSeek V4.1 Flash. A model that does
  refuse a field answers with openrouter's own error, passed through.

### How the rows get into the picker

Claude Code 2.1.280 was read for this (2026-10-06), and it offers three ways to
add models to `/model`; only one works on a subscription:

- Gateway discovery (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`) fetches
  `<ANTHROPIC_BASE_URL>/v1/models` at launch, but only with a credential from
  `ANTHROPIC_AUTH_TOKEN`, an API key, or `apiKeyHelper`. The claude.ai login does
  not count, and setting any of those three replaces the OAuth bearer on
  `/v1/messages` as well, ending subscription passthrough. It also drops every
  id not matching `/(claude|anthropic)/i`.
- The subscription's own additions come from `/api/claude_cli/bootstrap`, which
  goes to the OAuth host, never through `ANTHROPIC_BASE_URL`.
- `modelPicker` in `~/.claude/settings.json` takes rows of `model`, `label`,
  `description` and `behavesAs`, with no credential and no id filter. It is
  honoured from user, managed and `--settings` sources only, never a project's
  settings, so the Workhorse repo-root file cannot carry it. `behavesAs` names a
  model this Claude Code knows, whose client-side handling applies; without it
  a row for an unknown model is not offered.

So toker writes `modelPicker` (`picker.rs`, `patchers::patch_model_picker`). The
rows come from rules matched against openrouter's public listing, keeping each
rule's newest matches, so a new model version replaces the old on the next sync.
The service cannot write `~/.claude`, so `toker picker sync` runs as the user:
setup runs it once through `toker-picker.service`, and `toker-picker.timer`
repeats it daily, sandboxed to claude's settings dir and the state dir. A sync
owns only rows whose model starts with `openrouter/`, rewrites the file only
when the rows changed (Claude Code hot-reloads it into every session), and
changes nothing when the listing cannot be fetched.

## Same-protocol routes forward the client's bytes

Every request is parsed into the IR, a `serde_json::Value` with `preserve_order`
and `arbitrary_precision`, so re-serialising an untouched body reproduces it
byte for byte. The server checks that on every request (`ir/fidelity.rs`): when
the bytes match, the client's original buffer is forwarded; when they differ,
the original is *still* forwarded and a `fidelity-drift` row records the
divergence. Drift is a visible metric, not a hoped-for absence. Only a
deliberate transform (the licensed rewrites in [invariants.md](invariants.md))
forwards the serialised IR instead, and that serialisation is a pure function of
the value, so the transform repeats identically on the next turn and the
upstream's cached prefix holds.

A body toker cannot parse is forwarded unchanged and unrecorded: the proxy never
rejects what it does not understand. Setup's wiring check relies on that,
posting an empty body and taking the upstream's own 401 as proof the chain is
up.

## Cross-protocol routes translate, purely

An anthropic-frontend request routed to `codex_sub` never byte-forwards. It goes
through `translate::to_codex` into the canonical IR (`ir/canonical.rs`) and out
onto the Responses wire, and the response comes back through
`translate::AnthropicStream`. Every other stage of the anthropic pipeline runs
first, so the codex request carries the final effective model. The fidelity
check is skipped, because the upstream bytes never existed on the frontend's
wire.

What a backend refuses is that backend's declared property (`Capabilities`),
never a parse-time decision in the frontend: codex refuses system-role input
items, so mid-conversation system messages merge into the preceding user turn,
and it refuses sampling parameters and cannot read another provider's thinking
blocks, so those are dropped. The table of mappings is in `translate/mod.rs`.
Translation reads no clock and no state; the model slug and the prompt cache key
are passed in. That is what keeps a translated conversation's upstream prefix
stable even though it never existed in the frontend's format. The first request
of an existing conversation over a translated route still rebuilds the upstream
cache once, because the upstream's prefix genuinely changed.

## Restarting without cutting a stream

`systemctl --user restart toker.service` stops the process with SIGTERM, and
every response still streaming through it ends mid-turn. The client retries, but
that turn is lost, in every session at once. `toker restart` replaces it:

1. It polls `/_toker/status` once a second until `in_flight` reads zero on
   `QUIET_POLLS` (3) polls in a row. `in_flight` counts only exchanges already
   under way, and an agent's next request follows its tool calls after a short
   gap, so a single idle reading can fall between two requests of one busy
   turn. Ctrl-C here changes nothing; `--max-wait` gives up the same way.
2. It posts `/_toker/shutdown` with the `instance` id status reported. A
   mismatch is a 409, so a restart that raced another never stops an instance
   it did not see.
3. The server drains (`Server::serve_listener`, axum's graceful shutdown): it
   stops accepting, lets every response under way finish, closes idle
   keep-alive connections, then exits 0. There is no drain deadline, because
   the point is never to cut a stream; a stalled upstream still fails after the
   upstream idle timeout, as it would at any time.
4. `Restart=always` in the service unit (`service_unit` in `setup/wizard.rs`)
   restarts it after any exit, a clean one included. The socket unit keeps the
   listening socket the whole time, so connections that arrive meanwhile queue
   in the kernel and the next instance accepts them.
5. The CLI polls status until a different `instance` answers, for up to 30
   seconds. Uptime cannot tell the two apart: an instance asked to stop a
   second after it started reads like its successor.

Waiting for a quiet moment is not what keeps streams whole; the drain does that
on its own. It keeps new requests from queueing behind a long one: from the
moment the listener closes until the old process exits, nothing accepts them.
`--now` skips the wait and accepts that queue.

A `toker serve` run by hand has no systemd behind it: the drain still works, but
nothing starts the next instance, and the CLI says so when its wait runs out. A
toker from before this endpoint answers status without an `instance`, and the
CLI refuses rather than falling back to a signal.
