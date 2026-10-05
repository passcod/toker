# Frontends, backends, and the bytes in between

ctp had one frontend and one upstream, and forwarded every path but its control
one. toker has several of each, so most of what is new here is about which route
a request takes and what that route may do to it.

## Protocols and backends

A frontend speaks a protocol; a backend is a provider of one. One loopback port
serves them all, because the paths do not collide:

| Protocol | Paths | Backends |
| --- | --- | --- |
| Anthropic Messages | `/v1/messages`, `/v1/messages/count_tokens`, `/v1/messages/batches…` | `anthropic_sub`, `anthropic_api`, `codex_sub` (translated) |
| OpenAI Chat | `/v1/chat/completions`, `/v1/models` | `openrouter` |

A backend is enabled by its `[providers.X]` block's presence in `toker.toml`.
Each protocol has a default (`default_backend_anthropic`,
`default_backend_openai_chat`), which bare model names go to. A model string can
name its backend per request: `anthropic_sub/…`, `anthropic_api/…`,
`anthropic/…` (the protocol default) and `openrouter/…` are stripped and routed
(`strip_anthropic_prefix` in `server/anthropic.rs`, `strip_provider_prefix` in
`server/proxy.rs`), and the row records both `requested_model` and
`effective_model`. A prefix naming a backend whose block is absent is answered
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
| `openrouter` | stored key, injected only when the request has none | none | `billed`, from `usage.cost` |
| `codex_sub` | always toker-signed from `~/.codex/auth.json` | `x-codex-*` headers, stored per backend, not gated | NULL: no per-token price to verify |

The three cost kinds are never conflated (`CostKind`).

Meters are fed from **every** response of a meter-source backend, not only the
accounted ones: a 429 or a background call still reports them, and a gate fed
only from accounted rows goes stale.

The codex login is shared with the codex CLI, which refreshes the same file on
its own schedule. `providers/codex/auth.rs` re-reads it right before every
refresh and adopts the CLI's tokens if they moved; the race that remains is
documented there. That refresh is the one write toker makes outside its state
dir. As the unit template in `setup/wizard.rs` stands (`ProtectHome=read-only`,
`ReadWritePaths` the state dir alone), the service cannot write that file, so
check that before relying on a refresh from the service rather than the CLI.

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
