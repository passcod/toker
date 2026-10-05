# Internals

What was learned building this, by topic. AGENTS.md holds the rules; these hold
the reasons, and most of the reasons are a bug that was paid for. Many were paid
for in claude-token-proxy (ctp), the Node proxy toker replaces; where a lesson
came from there, the topic says so, and where toker does not yet honour it, the
topic says that too and points at the open items in [the cutover
plan](../plans/cutover.md).

| Topic | |
| --- | --- |
| [invariants.md](invariants.md) | Each invariant in full, and the incident behind it. |
| [routing.md](routing.md) | Frontends, backends, the `/f/<frontend>` prefix, and which bytes a route may change. |
| [notices.md](notices.md) | Gate notices: per-frontend styles, why they are byte-stable, why they answer 200. |
| [lanes.md](lanes.md) | A session is not a cache entry: compare within a tool-set lane. |
| [compaction.md](compaction.md) | Telling a compaction from the title summariser, and every wording it comes in. |
| [cold-gate.md](cold-gate.md) | The cold notice: when it speaks, the request bound, its two clocks, the outlook, why it is past-tense. |
| [models.md](models.md) | Learning the newest model from the ledger, keeping an upgrade once made, context ceilings. |
| [quota.md](quota.md) | What a rate-limit window meters, the traps in fitting it, and why the outlook prices at the top of the spread. |
| [forecasting.md](forecasting.md) | Projecting when a meter runs out, and totalling one over a span. |
| [pinging.md](pinging.md) | Opening a window on schedule buys phase, not capacity. |
| [sleep-lock.md](sleep-lock.md) | The idle-sleep lock and the wake timer. |
| [ledger-schema.md](ledger-schema.md) | Row kinds, and which rows predate which fields, imported ctp rows included. |
| [measuring.md](measuring.md) | Probes that cannot resolve the thing they look for, and the limits of a green test run. |

## Layout

| Path | |
| --- | --- |
| `src/main.rs` | The CLI surface (clap). Every subcommand is a call into `cmds`. |
| `src/cmds.rs` | Subcommand wiring: `serve`, `setup`, `status`, `tui`, `import`, `promote`, and the hidden timer verbs. |
| `src/config.rs` | `toker.toml` load: providers, protocol defaults, `[gates]`, `[notices]`, env overrides. |
| `src/secrets.rs` | API keys in the OS keyring, behind a seam tests replace. |
| `src/server/mod.rs` | The axum listener: socket activation, routes, the `/f/<frontend>` strip, the upstream idle timeout, the sleep-lock tick. |
| `src/server/anthropic.rs` | The Anthropic Messages frontend: release marker, quota gate, cold gate, compaction retarget, force-newest, model map, in that order. |
| `src/server/proxy.rs` | The OpenAI-chat frontend (opencode → openrouter), with its own cold notice. |
| `src/server/codex.rs` | The anthropic frontend's branch onto the codex backend: translate both ways instead of forwarding bytes. |
| `src/server/control.rs` | The `/_toker/*` control endpoints and their header gate. |
| `src/server/record.rs`, `record_anthropic.rs` | Row assembly at stream completion; store errors are logged, never raised. |
| `src/ir/` | The request IR: a `serde_json::Value` with typed views per protocol (`anthropic.rs`, `openai_chat.rs`), the canonical IR for cross-protocol routes (`canonical.rs`), and the per-request fidelity check (`fidelity.rs`). |
| `src/translate/` | Cross-protocol translation, Anthropic Messages ↔ codex Responses. Pure. |
| `src/observe/` | The side-parser riding each response stream: SSE splitting and usage capture. Infallible. |
| `src/middleware/quota.rs` | The quota gate: exhaustion, expiry, allowances, the block notice. Pure. |
| `src/middleware/cold.rs` | The cold gate, the compaction retarget, the burn ladder and the quota weight fit the outlook prices with. Pure apart from two store conveniences. |
| `src/middleware/lanes.rs` | The lane table: key, TTL stickiness, response merge, restart reseed, prune policy, ping tag. |
| `src/middleware/models.rs` | The learned model store: family election, days served, `max_prompt`, promotion. |
| `src/middleware/force_newest.rs` | Moving a request onto its family's newest model where no cache can be lost. |
| `src/middleware/model_map.rs` | The configured model routing map: model positions only, untouched bytes elsewhere. |
| `src/middleware/notice.rs` | Rendering a notice in the frontend's style. Pure. |
| `src/middleware/awake.rs` | The idle-sleep lock: what counts as live, the platform command, the detached child. |
| `src/providers/` | Backends: `anthropic.rs` (sub and API, and the meter-header parser), `openrouter.rs`, `codex/` (login, wire types, SSE, meters). |
| `src/catalog/` | Hand-verified prices (`pricing.rs`) and context windows (`windows.rs`), each with a `VERIFIED_ON`; the providers' fetched model listings (`fetched.rs`). |
| `src/store/` | The SQLite ledger (`ledger.rs`), the state tables (`state.rs`), and the append-only migrations (`schema.rs`). |
| `src/import.rs` | `toker import`: ctp's `usage.jsonl` into the ledger, with a checkpoint. |
| `src/timers.rs` | The wake/hold/ping verbs the systemd units run. |
| `src/setup/` | The wizard and its tested halves: atomic config writes, frontend patchers, the wiring check, unit templates, the bundled opencode plugin. |
| `src/tui/` | The dashboard: aggregation (`model.rs`, `quota.rs`, `rebuilds.rs`), rendering (`view.rs`), session labels from transcripts, locale formatting. |
| `plugins/opencode/toker-cost/` | The opencode sidebar plugin; `setup/plugin.rs` embeds it. |
| `systemd/` | Hand-installed units from before the wizard; `toker setup` writes its own from templates in `setup/wizard.rs`. |
| `tests/server_*.rs` | End to end: the real router against mock upstreams that capture every byte. Spend no quota. |
| `tests/*_prefix_stability.rs`, `ir_serialisation.rs`, `translate_corpus.rs` | Byte round-trips and prefix stability, per protocol and across the translated route. |
| `tests/node_reference_*.rs` | ctp's own decision fixture, vendored at `tests/fixtures/ctp/node-reference-v1.json`, driven against the ports. |
| `tests/tui_pty.rs` | The TUI's steady-state CPU probe under a pseudo-terminal. |
