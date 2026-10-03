# toker — unified local proxy toolsuite

Date: 2026-10-03
Status: agreed design, phase 1 in progress

## Purpose

A single Rust binary that replaces the ad-hoc proxy stack used today:

- **claude-token-proxy** (Node, at `~/code/claude-token-proxy`) — Anthropic pass-through proxy measuring token/cache usage, managing subscription quota limits, and upgrading models.
- **openrouter-ledger-proxy** (Node, at `~/code/this-computer/packages/openrouter-ledger-proxy`) — OpenRouter pass-through teeing real billed cost (`usage.cost`) into a JSONL ledger, with an opencode TUI plugin for session attribution.

toker absorbs both, generalises to multiple backends and frontend protocols, and keeps the context/cache management and usage/pricing insight that made those tools worth having — without the haphazard data loss of a proxy stack that drops useful information at every hop.

Core value: both the context+cache management features grown used to at work, and insight into token/pricing/plan usage, and the ability to switch frontends (opencode, claude, codex) for comparison without changing the backend.

## What this is not — permanent non-goals

- **Multi-user or team features.** Out of scope forever, not deferred. Single local user, loopback only.
- **macOS in v1.** The launchd/caffeinate paths from ctp can come later; v1 is Linux-only.
- **Legacy ctp tooling.** API price comparisons, subscription limit modelling, and the plan/overage constants in `pricing.mjs` die with ctp.

## Architecture

Three layers, with most of the unique features in the middle as route-scoped toggles rather than hardcoded per backend:

```
frontend adapters  →  middleware chain  →  backend adapters
(protocols)           (features)            (providers)
```

### Frontend protocol adapters

All exposed on one loopback port (paths don't collide), served by one socket-activated process:

| Adapter | Paths | Driven by |
| --- | --- | --- |
| Anthropic Messages | `/v1/messages`, `/v1/messages/count_tokens`, `/v1/messages/batches` | claude, claude-sdk, anything Anthropic-shaped |
| OpenAI Chat | `/v1/chat/completions`, `/v1/models` | opencode, anything OpenAI-shaped |
| OpenAI Responses | `/v1/responses` | Codex CLI |

Plus a `/_toker/*` control and query endpoint on the same listener: localhost only, gated by a custom header (a cross-origin web page cannot drive it without an unanswered preflight), used for models-merge (promote), status, and the attribution plugin's queries.

### Intermediate representation

Every request is parsed into a canonical internal model: messages/turns, tools, system prompt, sampling params, cache directives, plus raw preservation of all unmodeled fields so provider-specific extensions survive the trip. Middleware transforms the IR; the backend adapter serialises to the backend's protocol. **There is no passthrough code path** — passthrough is what the IR produces when nothing transforms it (see invariant 4).

### Middleware

Route-scoped toggles (route = frontend × backend), enabled per route in config:

| Middleware | Availability | Notes |
| --- | --- | --- |
| Recording + costing | always on | Cost semantics are per-backend (billed / estimated / plan-equivalent — see Storage) |
| Lane tracking + sleep lock | always on | Lane = session × tools-hash; the ctp lane rule carries over |
| Cold gate | any backend | Needs only idle time + prompt size per lane, which the IR always has |
| Compaction retarget | any backend | Where the frontend protocol exposes compaction detection |
| Force-newest model rewrite | any backend | Per-provider family map + empirical maxPrompt safety; never downgrade, never lose a cache, sticky once moved |
| Model routing map | route-level | Deliberate, once-per-change byte edits |
| Quota gate + release marker | requires a meter source | Anthropic sub is the only meter source today. The meter interface is open so codex sub's usage limits can become one if their shape supports it. Marker *stripping* stays unconditional (frozen API) |
| Ping tagging | toggle | Marks ping lanes so they never hold the sleep lock |

### Backend providers

Providers within a protocol share an adapter; they differ in auth, cost semantics, and meter parsing:

| Backend | Protocol | Auth | Usage/cost insight |
| --- | --- | --- | --- |
| anthropic sub | Anthropic | CLI's OAuth (passthrough when the frontend brings it, else toker-signed from the reused token) | quota meters from `anthropic-ratelimit-*` response headers → quota gate, 5h/7d/overage forecasting |
| anthropic api | Anthropic | API key | token buckets, list-price estimated cost |
| openai api | OpenAI Chat | API key | token buckets incl. `cached_tokens` (automatic prefix caching), estimated cost |
| codex sub | Responses | `~/.codex/auth.json` reuse + refresh | its usage-limit shape (verify at implementation; candidate meter source) |
| openrouter | OpenAI Chat | API key | real billed `usage.cost` + serving provider captured verbatim — ledger parity |
| lunaroute | OpenAI Chat | `lr_` API key | token buckets; whether their API exposes per-request cost to verify at implementation |

### Routing

- Configured **default backend per frontend protocol**; bare model names go to the protocol default.
- **`provider/model` names override per request** (`openrouter/z-ai/glm-5.3`, `anthropic/claude-opus-5`, `lunaroute/…`) — switch backends from the frontend without touching toker config.
- Requested vs effective model always recorded.

### Dependencies

Best-of-ecosystem, no asceticism: use an existing crate whenever one fits. Settled choices — **jiff** (not chrono) for time/timezones, serde_json (`preserve_order` + `arbitrary_precision`), rusqlite, axum + tokio, reqwest, ratatui + crossterm, clap, `inquire` for the wizard, `toml_edit` for formatting-preserving config patching, `keyring` for credentials, `listenfd` for socket activation, `anyhow`/`thiserror`, `eventsource-stream` where an off-the-shelf SSE parser fits the translation direction. Hand-rolled bits must earn their keep (the zero-copy side-observer tee is the known example).

## Invariants

Carried over from ctp where marked, new where noted:

1. **No content stored.** Counts, lengths, digests, and whether fixed known-in-advance marker strings matched — never prompt/completion/system/tool text. Session labels come from frontend transcripts at view time, read-only. *(ctp)*
2. **Credentials never logged, never in the ledger.** Stored only in config/keyring; request auth headers read by name only where needed; response headers (no secrets there) captured wholesale. *(ctp, adapted: ctp stored no credentials at all — see Credentials)*
3. **Absence ≠ zero, and proxy-written rows are never API measurements.** Unknown/estimating/stale are explicit verdicts, never silent guesses; a withheld cold notice is logged (`cold-quiet`) so silence is distinguishable from breakage. *(ctp)*
4. **Serialisation purity.** IR → bytes is a pure function of the parsed value (serde_json `preserve_order` + `arbitrary_precision`, deterministic minimal escaping, raw unmodeled fields). No serialisation decision ever depends on runtime state — meters, gates, clocks. The only byte changes are deliberate: a middleware transform or a config change, each a user-visible, once-per-change event. The release marker `$#$BURN$#$` is a frozen public API and its stripping rule stays byte-stable forever. *(new; ctp's measured hazard was state-varying transformation)*
5. **Prefix stability, by construction and by testing.** For a conversation that only appends, the body sent upstream keeps the upstream's cacheable prefix stable — same-protocol or translated, transformed or not:
   - **Untransformed requests are passthrough by construction, verified per request.** The server byte-compares the re-serialised body against the original buffer on every request (cheap memcmp). Bytes match — the normal case — the original buffer is forwarded, identical to a passthrough proxy. Bytes differ: forward the original (still safe), and record a **fidelity-drift row** (route, frontend, divergence digest). Drift is a visible, queryable metric, not a hoped-for absence.
   - **Transformed/translated requests**: the transform is pure and deterministic, so turn N+1 reproduces turn N's bytes exactly wherever the conversation didn't change, in any protocol pair. The prefix the upstream sees is stable even though it never existed in the frontend's wire format.
   - **Enforced by**: round-trip byte-equality corpus tests (`serialize(parse(body)) == body` against captured real traffic, seeded from ctp fixtures); prefix-stability property tests per frontend×backend pair (mutate only the conversation tail, assert the upstream body is unchanged up to the mutation point); the CACHE REBUILDS analytics as the end-to-end production signal (systematic serialisation rebuilds would localise to nowhere).
   - **Honest caveat**: the *first* request of an existing conversation through a translated route rebuilds the upstream cache once — the upstream's prefix genuinely changes. Bounded, deliberate, visible in the ledger. *(new, replaces ctp's byte-splice precaution)*
6. **Accounting must never break a session; only a gate may stop one.** Observational parsing runs around an already-forwarded byte stream; a parse failure loses a measurement, never a request. *(ctp; free in Rust via Result, but the design stance carries)*

## Server core

- Buffer request bodies for gating decisions (ctp behaviour; bodies are small relative to streams).
- Response streams pass through with backpressure; an opportunistic, crash-proof SSE side-parser extracts usage/model/cost. Tolerant of `\r\n` dialects with end-of-stream flush; skips keep-alives and non-JSON events; per-response state latches id/model/provider from the final usage-bearing chunk with earlier-chunk fallback.
- Force `accept-encoding: identity` on upstream requests we side-parse; pass unexpected compressed responses through unledgered (ledger-proxy lesson).
- No idle timeout on streams (equivalent of `requestTimeout = 0`); 300 s read timeout; upstream abort wired to client hangup; a client that hangs up mid-stream produces no row.
- Non-2xx on a usage path → `error` row (status, type, retry-after), never priced.
- Gates answer 200 with a synthetic assistant turn (SSE or JSON per the request), never an error status — measured at work: 529 retries silently, 429 mislabels, 403 looks like broken credentials.
- Every response updates the meter snapshot (when the backend has meters), not just accounted rows; a spent reading stops counting once its window's reset passes, so the gate can never wedge (ctp's `expired()` rule).

## Credentials (hybrid)

- **API-key backends** (anthropic api, openai api, openrouter, lunaroute): keys entered in setup, stored via the system keyring (`keyring` crate / Secret Service) with 0600-file fallback; setup and `status` report which is in use (the hardened service unit needs dbus access for the keyring path and must degrade gracefully).
- **Subscription backends** (anthropic sub, codex sub): OAuth tokens reused from the CLIs' own stores, with toker performing refresh when it must sign requests itself.
- **Pass-through-when-present**: if the incoming request already carries an Authorization/api-key header, forward it verbatim — claude keeps bringing its own credential; toker injects stored creds only when the frontend has none for that backend.
- The ping subsystem keeps shelling out to `claude -p` as the only credentialed client (ctp pattern).

## Storage

SQLite, `$XDG_DATA_HOME/toker/toker.db`, WAL mode. Engine choice: plain **rusqlite** behind a std `Mutex<Connection>`, considered against Turso/libSQL (async SQLite) and rejected — toker's write rate is one row per request (minutes-scale, never throughput-bound, blocking calls are microseconds) and libSQL's advantages (async, remote/replicated modes) are irrelevant to a single-user loopback tool. The `Store` API is encapsulated, so the engine stays swappable if that ever changes.

- **`requests`** — insert-only, one row per request, carrying the ctp row schema verbatim (token buckets, `rateLimits`, shape fields — toolsHash, system hashes/ladders/tail, compaction markers, `usagePresence` — gate provenance, adaptive-rewrite provenance) **plus**: provider/backend, frontend protocol, routing provenance (`requestedModel`/`effectiveModel`/batch `modelMappings`), the raw provider usage JSON verbatim (OpenRouter's `usage.cost`/`cost_details`, serving provider — ledger parity), fidelity-drift rows, and cost in three explicit kinds, never conflated:
  - `billed` — provider-reported (openrouter today)
  - `estimated` — catalog-priced (API backends)
  - `plan_equivalent` — list-price on a subscription ("what is the plan worth?")
- Proxy-written row kinds (`blocked`, `released`, `cold`, `cold-quiet`, `awake`, `error`, `fidelity-drift`) carry no `rateLimits` unless explicitly a stale-copy kind; all excluded from API measurements.
- **State as tables**: lanes (keyed `sessionId|toolsHash`, 30-day prune), learned models (days served, `maxPrompt`), allowances (keyed by reset value, self-expiring), pings, last-meters.
- **`toker import`** ingests ctp's `usage.jsonl` (honoring ctp's `docs/internals/log-schema.md` field-era notes) so learned model state and 7-day forecasting stay continuous from day one.
- **`toker export`** emits JSONL for greppability.
- Timestamps: rows arrive slightly out of order; window anchoring rules carry over (5h anchored to first request, fixed 7-day boundary).

## Attribution

**Headers-first.** Verify at implementation whether opencode can be configured to send a custom per-session header via provider config; if yes, setup injects it and attribution is server-side and exact. Claude (`x-claude-code-session-id`) and codex already identify themselves. **Fallback**: adapt the ledger-cost join plugin to query `/_toker` — exact token-vector + ±120 s FIFO join, unchanged logic.

## Model catalogues

- **Learned-newest store**: per exact model identity, days served + max prompt observed; a model becomes its family's rewrite target after enough distinct days; never beyond observed maxPrompt. *(ctp, per-provider in toker)*
- **Context-window ceilings**: hand-verified catalogue per provider (exact normalised identities; Anthropic native-1M/fixed-200k, Codex declarations like `gpt-5.6-sol`/`gpt-5.6-luna` at 872k, openrouter/lunaroute from their catalogues). Exact-identity matching throughout; unknown stays `?` rather than confidently wrong. *(ctp)*
- **Pricing**: hand-verified per-provider table with a freshness date; unknown model → `costUsd: null` + one-time warning, never guessed. *(ctp)*

## Sleep lock, wake, ping

- **Sleep lock** (always evaluated, toggleable): while any lane is live (its cache would be: TTL past last response) or anything is in flight, hold an idle-only sleep lock via a detached child that dies with the proxy's PID. GNOME → `gnome-session-inhibit`, other Linux → `systemd-inhibit --what=idle --mode=block`. Deliberately idle-only (a logind sleep block is both too strong and not strong enough — measured at work). Transitions logged as `awake` rows.
- **Wake service** (optional in setup): system timer with `WakeSystem=true` (CAP_WAKE_ALARM, the only root unit), user hold timer (`hold` verb, 15 m) and ping timer (`ping-window` verb, lateness guard >10 min, `Persistent=false`). Ping opens a quota window by shelling out to `claude -p` with the ping header injected via `ANTHROPIC_CUSTOM_HEADERS`. Buys phase, not capacity. *(ctp, Linux-only paths only)*

## TUI

ratatui + crossterm, long-running (~2 s refresh from SQLite), replacing `watch … live.mjs`:

- Panels: **sessions** (context ceiling, model, requests, prompt now/peak, messages, compactions, `↑` newer-model marker, `$` released marker), **context** bars (occupancy vs known ceilings, red past 80 %, nothing claimed for `?`), **tokens** (input buckets, hit rate over the reusable prefix), **cache rebuilds** (≥50 k rewrites, cause-classified by walking lanes over every row read — ctp's phantom-rebuild lesson), **rate & quota** (sparkline, meter bars + reset times + forecasts, `spent` span totals, `binding` claim).
- New: **spend** panel — billed vs estimated, per-provider breakdown à la the ledger sidebar; per-backend panels render only when that backend has traffic.
- Session labels from transcripts (`~/.claude`, `$CLAUDE_CONFIG_DIR`, configured extra roots for harnesses like Workhorse), read-only at view time.

## `toker report`

Inherits the `summarise.mjs` views that still matter: rebuilds (with cause localisation ladders), quota fit (NNLS weights refit from the ledger, collinearity/lockstep demotion, model rows with any `kind` excluded), compactions, cold, models. Locale-correct clocks (ctp's `LC_ALL`→`LC_TIME`→`LANG` lesson).

## Setup wizard

`toker setup` — idempotent, re-run to change anything:

1. Pick backends → auth each (paste key / reuse CLI tokens / keyring-with-fallback).
2. Defaults per frontend protocol.
3. Optional toggles: wake service (root unit, sudo prompt), sleep lock, ping windows.
4. Wire frontends: claude (user + Workhorse repo settings — both required, directory-scoped settings win), opencode (provider baseURL + session header if supported), codex (`config.toml` via `toml_edit`, formatting-preserving), or generic env vars into shell rc.
5. All config patching atomic (temp-write, re-parse, compare, rename — the opencode.json lesson); ordering enforced: **bind the socket first, then point clients at it** (env hot-reload into running sessions kills live sessions if the listener isn't up).

No config files written by hand unless wanted; `toker.toml` exists for hand-editing later.

## Deployment

- `cargo install toker --git …`, cargo-binstall-able via git tags; dev flow is `cargo run`.
- systemd: `toker.socket` (ListenStream 127.0.0.1:PORT, Accept=no) is the enabled unit; `toker.service` (Type=exec, Restart=always) starts on demand, hardened like the ledger unit (NoNewPrivileges, ProtectSystem=strict, ProtectHome=read-only, ReadWritePaths=state dir, RestrictAddressFamilies incl. AF_UNIX for dbus/keyring).
- Socket activation via `listenfd`/LISTEN_FDS+LISTEN_PID guard; fallback to direct bind for dev.
- New port, not 18082 — ctp stays running at work during migration; both coexist, frontends switch per the ordering rule.

## Phases

Each phase usable standalone; dogfood-first ordering:

1. **Opencode + openrouter, today.** OpenAI-chat frontend + openrouter backend, IR core with the serialisation-purity and fidelity-monitor machinery, full recording (billed cost + serving provider verbatim), lanes, minimal TUI (sessions, context, spend). Same-protocol from day one, so this phase is the live test of IR re-serialisation against OpenRouter's real cache behaviour. Exit: ledger proxy retired.
2. **Anthropic.** Frontend endpoint + api/sub backends, full middleware (meters → quota gate, release marker, cold gate, compaction retarget, force-newest), `toker import`, sleep lock, quota panels. Exit: ctp retired at work.
3. **Codex.** Responses frontend endpoint + codex sub backend (auth reuse, usage-limit shape verified; possibly promoted to a meter source), cross-protocol translation as needed.
4. **OpenAI api + lunaroute** backends (adapter exists; these are auth + costing semantics).
5. **Grown TUI + `report` + `setup` wizard + wake/hold/ping units + attribution plugin fallback.**

## Stretch goals

- **Per-session dashboard in the opencode sidebar.** Extend the opencode plugin beyond cost attribution: render the toker dashboard specialised to the *active* session — context occupancy, token/cache breakdown, spend, lane state — querying `/_toker`. Builds on the attribution plugin work (phase 5).
- **Native rendering for gate notices.** Gate and cold notices are synthetic assistant turns; render them in the frontend's own structured format where one exists: claude's insight block (claude-only rendering), the generic GFM `> [!NOTE]` alert (Workhorse and every markdown-ish renderer show it sensibly), degrading to plain text elsewhere. **The GFM alert is the default** — toker cannot yet tell anthropic-frontend clients apart, so the generic wrapper wins; `notice_style = "insight"` opts in for claude-only setups. Notice text stays a pure function of the gate decision (invariant 4) — the wrapper's width is frozen (50 columns) so a rendered notice enters replayed history byte-stably like any other turn.

## Verify at implementation (known unknowns)

- Codex sub's usage-limit reporting shape (headers vs response body).
- Whether lunaroute exposes per-request cost or only dashboard usage reports.
- ~~Whether opencode supports per-provider custom headers for session attribution~~ **Resolved 2026-10-03: opencode sends `x-session-id` natively** (toker's default header list picks it up) — attribution is server-side and exact; no join plugin needed.
- claude CLI's Linux credential store location and refresh mechanics for the anthropic-sub signing path (still pending — matters when a non-claude frontend drives anthropic sub).
- Anthropic's cache granularity vs re-serialisation in practice (the fidelity monitor answers this in production; corpus tests answer it beforehand — zero drift rows observed on live openai traffic so far).
