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
| OpenAI Chat | `/v1/chat/completions`, `/v1/models` | `openrouter`, `codex_sub`, `anthropic_api`, `anthropic_sub` (translated) |
| OpenAI Responses | `/v1/responses`, `/v1/models` | `codex_sub`, `openrouter`, `anthropic_api`, `anthropic_sub` (translated) |

A backend is enabled by its `[providers.X]` block's presence in `toker.toml`.
Each protocol has a default (`default_backend_anthropic`,
`default_backend_openai_chat`), which bare model names go to. A model string can
name its backend per request: `anthropic_sub/…`, `anthropic_api/…`,
`anthropic/…` (the Messages protocol default), `openrouter/…`, and
`codex_sub/…` are stripped and routed where the shared route registry declares
that frontend-to-binding path. The resolved `ModelTarget` keeps the provider,
verified backend binding, requested model, and effective model distinct; the
row records both model spellings. A prefix naming a backend whose block is
absent is answered locally, never sent to the default with the prefix still on.
An unknown prefix belongs to the provider's model id and stays intact. A
protocol with no enabled backend answers its routes with a not-configured error
in that protocol's own shape, carrying the `x-toker-not-configured` header, and
reaches no upstream.

For a known OpenAI frontend profile, `/v1/models` is projected locally from
the fetched provider catalogues through this same route registry. Each listed
model has a provider-qualified id; the configured default also gets a bare
alias. A model without a complete route is absent. Chat receives the OpenAI
list shape, built from known fields rather than copying a foreign provider's
entry; Codex receives its native `models` shape with routed slugs and its
provider metadata retained. Catalogue age stays with each internal offer.
Codex's model parser requires a full `ModelInfo` entry, so foreign providers
remain explicitly routable but are not advertised in its models response until
their metadata projection is verified.
When no usable catalogue has arrived, the endpoint returns 503, not a false
empty list. Unknown and unprefixed profiles keep path-driven forwarding.
The service loads cached catalogues before serving, then refreshes in the
background. Claude's picker filters the OpenRouter Messages offers from the
same join, subject to its own tool/text and user-rule constraints.

Every path the route table does not match is forwarded to the default anthropic
backend (`anthropic::unmatched`), as ctp forwarded everything, except the
`/_toker/` namespace, which never leaves the proxy.

The Codex Responses route is canonical even though both ends speak Responses. Toker
parses the request into canonical IR, deterministically renders it for the
Codex binding, interprets upstream events canonically, and renders Responses
events back to the client. Compatible extensions, provider-owned input items
(including encrypted reasoning and incremental tool declarations), and
provider-owned tool kinds replay opaquely through the canonical model. The
same applies to provider-owned response items and their incremental events;
the terminal `end_turn` flag remains authoritative even when the item has no
portable tool-call shape. This
lets a newer frontend shape reach a compatible backend without requiring toker
to understand its contents. Toker always replaces the frontend credential
with its shared Codex login before the request leaves loopback.

Every Chat route is canonical. The OpenRouter binding deterministically renders
Chat again and interprets its response before the frontend renders Chat. It
observes the provider response before translation so billing evidence stays
intact. Chat may also select `codex_sub` as its configured default or with a
`codex_sub/<model>` prefix; that binding renders a Responses request and
translates the Responses turn back to Chat SSE or complete JSON. Toker signs
the Codex upstream itself, records the route as `openai_chat:codex_sub`, and
never forwards the frontend credential.
The Anthropic Messages bindings accept Chat and Responses requests after
canonical rendering. A foreign OpenAI bearer is removed before provider-owned
authentication is added. Responses and Chat output limits are optional on
their own wires but required by Messages: a positive caller limit wins, then
the fetched model's declared `max_tokens`; absent evidence is a local 400,
not a guessed limit. The reverse path interprets Messages events or JSON and
renders the client's OpenAI wire, while Anthropic usage is observed before
translation.

### What differs per backend

| Backend | Auth | Meters | Cost |
| --- | --- | --- | --- |
| `anthropic_sub` | native Claude bearer passed through; foreign frontend bearer replaced by a toker-held OAuth token or Claude's local login (in that order) | `anthropic-ratelimit-*` headers, the quota gate's only source | `plan_equivalent` (list price on a subscription) |
| `anthropic_api` | `x-api-key`, injected only when the request has none | none (its RPM headers are not quota meters and must never overwrite the gate's snapshot) | `estimated` |
| `openrouter` | stored key, injected only when the request has none; an Anthropic credential is dropped first | none | `billed`, from `usage.cost`, on both routes |
| `codex_sub` | always toker-signed from `~/.codex/auth.json` | `x-codex-*` headers, stored per backend, not gated | NULL: no per-token price to verify |

The three cost kinds are never conflated (`CostKind`).

For a foreign frontend, `anthropic_sub` resolves its own token from
`oauth_token_env`, the `anthropic_sub` keyring entry when configured, then a
literal `oauth_token` in the 0600 config. If none exists, it reads Claude
Code's `.credentials.json` at the configured `claude_credentials_path` (by
default under `$CLAUDE_CONFIG_DIR`, else `~/.claude`). That login is read for
each turn so Claude's refresh or logout takes effect without restarting toker;
an expired token is not sent. The provider adds the OAuth beta and version
headers when it signs. No credential value is logged or stored in the ledger.

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
(`/f/claude`, `/f/workhorse`, `/f/codex`); an unprefixed request is an unknown
frontend and gets the default style. Names are lowercase ASCII letters, digits,
`-` and `_`
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
canonical route with the prefix stripped before deterministic rendering. The
client's own credential is Claude's OAuth bearer, meant for Anthropic:
`OpenRouter::strip_foreign_credentials` drops any `sk-ant-` bearer and
`x-api-key`, and the stored openrouter key goes in its place. Nothing of the
subscription's reaches openrouter.ai.

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
  5.3 Flash, OpenAI's GPT-6 Luna and DeepSeek V4.1 Flash. A model that refuses
  a field answers with an error interpreted and rendered through the Messages
  adapters.

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

## Inference routes are canonical

Every valid `/v1/messages` request is parsed into one canonical request that
the Messages gate and licensed rewrites use. The selected provider binding
then renders it even when both ends speak Messages. The reverse
path observes provider bytes first, interprets SSE events or a complete body
canonically, and renders Messages for the client. This applies to
`anthropic_sub`, `anthropic_api`, OpenRouter Messages, and the translated Codex
binding. An invalid request is a local typed 400; a compressed, oversized, or
malformed provider response that cannot cross the canonical boundary is a
local typed 502. Neither reaches the other side as an unverified wire shape.
Malformed bodies retain a wire-shaped view only for the quota decision and
error presentation, not inference mutation or shape extraction. Administrative
bodies keep their legacy path-specific behavior.

The non-inference Messages surfaces remain transparent. Count-token and batch
creation retain their legacy buffer, routing rewrites, observation, and
error-row behavior; batch reads, cancellation, and unmatched administrative
paths stream without inference observation.

OpenAI Chat is canonical for every declared route. The frontend adapter parses
each request; routing and cold-gate shape extraction read that canonical
request. Then the OpenRouter Chat, Codex Responses, or Anthropic Messages
backend adapter renders it. Responses take the reverse path through canonical
events or a complete canonical turn. OpenRouter's original response bytes still
feed the ledger observer before translation, so `usage_raw`, billed cost and
the serving provider remain provider-attested evidence. An invalid Chat body
is a local 400 and a compressed or malformed upstream response is a local
502; neither is forwarded as though it had crossed the canonical boundary.

## Thinking that cannot be turned off

Claude Code turns thinking off on its side calls, the auto-mode classifier
among them, with `"thinking": {"type": "disabled"}`. Which models accept that
is a list built into the client: 2.1.292 (read 2026-10-09) lists
`claude-sonnet-5` as one that does. From 2026-10-08 Anthropic refused it on
that model in bursts, answering 400 with "To turn thinking off on this model,
send `"thinking": {"type": "between_tools"}`", while the same call succeeded
between bursts. In a burst every classifier call failed, and with it every Bash
permission check in auto mode, in sessions on any main model.

So when a `/v1/messages` request that sent `disabled` comes back 400 with an
error message naming `between_tools`, toker sends it once more with that value
in its place (`retry_thinking_off` in `server/anthropic.rs`) and forwards
whatever the second attempt gets. It never rewrites up front: nobody observed
whether `between_tools` is accepted where `disabled` still is, and the refusal
is the only evidence. The cost is one extra round trip per call during a
burst; the 400 arrives before any stream begins, so the client sees only the
retry's answer. The canonical request renderer preserves both explicit modes;
the retry changes only that semantic value in the already-rendered Messages
body. The refusal's meters still feed the gate, and the retried
row carries `extra.thinkingRewrite` (see
[ledger-schema.md](ledger-schema.md)).

This keys on the upstream's answer, not on the backend or the model, so it
applies wherever an Anthropic-wire upstream asks for it.

## Canonical routes translate, purely

An anthropic-frontend request routed to `codex_sub` never byte-forwards. It goes
through `translate::to_codex` into the canonical IR (`ir/canonical.rs`) and out
onto the Responses wire, and the response comes back through
`translate::AnthropicStream`. Every other stage of the anthropic pipeline runs
first, so the codex request carries the final effective model. Backend
rendering is canonical, so a frontend-byte fidelity check does not apply.

The Responses frontend to Codex binding takes the same path despite matching
protocol names. Protocol equality lets it replay compatible opaque extensions;
it does not bypass canonical request or event handling. Responses routing and
ledger shape extraction also read canonical IR, with the original byte length
and `input` array presence carried separately rather than inferred.

The OpenRouter Responses binding is a narrower dialect. A completed live
streamed function-call probe on `openai/gpt-4.1-mini` verified a required tool,
`max_output_tokens`, terminal usage, and provider-reported billed cost. An
earlier `openai/gpt-5-nano` probe ended incomplete at its output cap, so it
does not establish reasoning replay. Toker streams upstream even for a JSON
frontend turn, aggregating that turn locally; it removes Codex's bearer and
signs with OpenRouter's key. It omits unverified reasoning controls, encrypted
content requests, cache keys and other opaque fields with content-free loss
reports, rejects unknown tool collections and unsupported input items, and
records only the provider's reported cost. This binding does not claim images
or reasoning replay.
An invalid-model live probe on 2026-10-09 returned HTTP 400 with numeric
`error.code: 400`; the shared error parser accepts both numeric and symbolic
codes so the accompanying message survives HTTP and SSE error handling.

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
every response still streaming through it ends mid-turn. The socket unit keeps
accepting meanwhile and harnesses retry a cut stream, so this is safe and was
the way before `toker restart`, but each retry re-sends that turn, in every
session at once. `toker restart` avoids the cut:

1. It polls `/_toker/status` once a second until `in_flight` reads zero on
   `QUIET_POLLS` (3) polls in a row. `in_flight` counts only exchanges already
   under way, and an agent's next request follows its tool calls after a short
   gap, so a single idle reading can fall between two requests of one busy
   turn. Ctrl-C here changes nothing. After `--max-wait` (30 s unless given)
   with no quiet moment, or with status not answering at all, it skips to
   the forced stop in step 6 instead of giving up: a session that never idles
   is not a reason to keep the old binary.
2. It posts `/_toker/shutdown` with the `instance` id status reported. A
   mismatch is a 409, so a restart that raced another never stops an instance
   it did not see.
3. The server drains (`Server::serve_listener`, axum's graceful shutdown): it
   stops accepting, lets every response under way finish, closes idle
   keep-alive connections, then exits 0. There is no drain deadline, because
   the point is never to cut a stream by itself; the forced stop in step 6 is
   what bounds a drain that does not end.
4. `Restart=always` in the service unit (`service_unit` in `setup/wizard.rs`)
   restarts it after any exit, a clean one included. The socket unit keeps the
   listening socket the whole time, so connections that arrive meanwhile queue
   in the kernel and the next instance accepts them.
5. The CLI polls status until a different `instance` answers, for up to 30
   seconds. Uptime cannot tell the two apart: an instance asked to stop a
   second after it started reads like its successor.
6. If those 30 seconds pass with the old instance still draining, or the
   shutdown request got no answer, it posts the shutdown again with `"force":
   true`: the graceful attempt had its time, and a response that will not end
   does not get to hold the new binary back. The server stops waiting for
   connections and returns from `serve_listener` as it would after a drain, so
   the sleep lock is released the same way and the process exits 0 through
   `main`, which drops the runtime (bounded to 5 seconds) and so cancels the
   responses still under way, running their destructors. They are cut; the
   client retries them. `Restart=always` starts the next instance, and step 5's
   poll runs once more for 30 seconds. After that failure it reports rather
   than forcing again. The request names the last instance that answered, so a
   daemon that never answered status at all has nothing to address, and the
   command says to use `systemctl --user restart toker.service` instead.

Waiting for a quiet moment is not what keeps streams whole; the drain does that
on its own. It keeps new requests from queueing behind a long one: from the
moment the listener closes until the old process exits, nothing accepts them.
`--now` skips the wait and accepts that queue, up to the same 30 seconds.

A `toker serve` run by hand has no systemd behind it: the drain still works, but
nothing starts the next instance, and the CLI says so when its wait runs out. A
toker from before this endpoint answers status without an `instance`, and the
CLI refuses rather than falling back to a signal.

Setup uses this same drain-and-restart path when its initial state check found
an active socket on the configured port. Reinstalling and reloading a unit does
not change an already-running process: without the restart it would verify the
old in-memory config (and possibly the old binary inode), even though setup's
state summary had read the current `toker.toml`. A fresh or port-changing setup
does not take this path; socket activation starts the installed service, while
a live socket whose port changed still needs the explicit socket restart setup
reports.
