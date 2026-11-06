# F1: remove legacy routing machinery and finish docs

Changeset 18 of `docs/plans/live-model-routing-implementation.md`, plus the
other outstanding plans in `docs/plans/`. The prime directive applies: two
users, no live fleet, take the expedient path.

## Decisions

- An existing lane with no routing pin resolves under the active revision like
  a new lane. This costs at most one cold re-read per lane that was warm at
  upgrade, and lets the legacy resolver go entirely. No pin seeding.
- In scope beyond changeset 18: the OpenRouter placeholder-bearer bug, moving
  the subscription signing checklist out of `docs/plans/`, and opencode session
  lanes. Server safeguards moved to its own card (G1).

## Findings that widen changeset 18

Changeset 12 left legacy routing reachable in production:

- `Server::resolve_policy_attempt` returns `Ok(None)` for an unpinned existing
  lane (scope `new_lanes`, or not cold) and on a stored/facts lane mismatch;
  Messages, Chat and Responses then route via `RouteRegistry::resolve`, and
  Messages re-runs legacy compaction, force-newest and model map.
- count_tokens and batches apply the TOML model map on top of the policy's
  effective model, because the guard is `policy_attempt.is_some()` and only
  `/v1/messages` sets it.
- Administrative paths return not-configured when no legacy Anthropic default
  resolves, before policy is consulted.
- `/v1/models` on non-OpenAI profiles forwards to the legacy Chat default.
- `toker promote` compiles the legacy config at runtime.
- The policy compaction resolver calls `middleware::models::{family_of,
  newer_than}` directly.
- The legacy `ModelStore` is still written per request; `/_toker/models/merge`
  has no in-tree client.

## Steps

### Make policy routing total

- [x] Unpinned existing lanes resolve under the active revision; a lane
      mismatch is an error path, not a legacy fallback
- [x] Drop the legacy-default gate on administrative Anthropic paths; resolve
      count_tokens, batches, unmatched and batch GET/cancel from policy only
- [x] Stop applying provider model maps after policy resolution (count_tokens,
      batches) and stop passing them into cold-gate compaction pricing
- [x] `/v1/models` for every profile from the active revision
- [x] `toker promote` and `toker status` read the active revision; drop the
      "legacy-authoritative" wording and the `serve` default-backend log line

### Delete the legacy runtime

- [x] `RouteRegistry` becomes topology and binding lookup only: no prefix,
      alias or default interpretation; `default_backend_*` read only by the
      legacy compiler and setup import
- [x] Remove runtime model-map plumbing (`Provider::model_map`,
      `canonical::map_model`, `inference::apply_model_map`,
      `compaction_target_of`); keep `middleware::model_map` parsing and
      `rewrite_model_positions` for the legacy compiler and batch splicing
- [x] Move `family_of`/`newer_than` beside the revision-one classifier in
      `policy/`; route compaction through classifier output
- [x] Move `force_newest` constants and `prompt_bound` into `policy/`; delete
      `decide`, `ForceContext`, `force_target_of`, `ModelStore::force_target`
- [x] Remove `compact_spec` runtime reads; compaction comes from the compiled
      route only
  - No runtime `compact_spec`/`gates.compact_model` read remains (only the
    legacy compiler authors it). `RequestFacts::legacy_compaction` stays: it
    is the lane and body input of the stored `RevisionOneLegacy` compaction
    target, which revision one's persisted document carries.
- [x] Stop dual-writing the legacy `ModelStore`; remove `/_toker/models/merge`
      and test-only promotion planning
- [x] Remove shadow mode (`policy/shadow.rs`, `LegacyShadow` ownership, shadow
      observers and callers); rename the live `shadow_*` helpers and
      `capture_shadow`
- [x] Port server integration tests to an authoritative server with an active
      revision
  - Done in the pilot: one authoritative constructor family (`Server::new`,
    `Server::with_seams` + `Seams`), `tests/common` helpers, `server_proxy.rs`;
    recipe in `port-notes.md`.
  - [x] tests/server_anthropic.rs (three src fixes found on the way; see
        Findings in `port-notes.md`)
  - [x] tests/server_awake.rs
  - [x] tests/server_codex.rs
  - [x] tests/server_cold.rs
  - [x] tests/server_force_newest.rs
  - [x] tests/server_session.rs
  - [x] tests/server_shutdown.rs
  - [x] unit tests: `mod tests` in src/server/{mod,anthropic,codex,inference,
        proxy,record,record_anthropic,attempt,cancel,control}.rs (only mod.rs
        needed changes; the rest already ran on an authoritative server)
  - [x] src/tui/routing/import.rs and src/cmds.rs tests (swept, no change)
- [x] Clear stale cutover comments (`server/mod.rs`, `store/schema.rs`,
      `store/state.rs`, `anthropic.rs`, `policy/learning.rs`,
      `middleware/mod.rs`, `middleware/model_map.rs`)
  - Done: `server/mod.rs`, `store/schema.rs`, `store/state.rs`,
    `anthropic.rs`, `policy/learning.rs`, `middleware/mod.rs`,
    `middleware/model_map.rs`.

### Regressions the test port surfaced

Recorded in `port-notes.md` ("Findings"); each is a gap between the live
policy path and what the legacy path did.

- [x] Model-less requests cannot route under revision one (batch results,
      cancel, every unmatched Anthropic path answer 503 on the live config):
      add the exact-profile-then-protocol default resolution the design
      describes for model-free administrative paths
- [x] A pinned lane on a binding with no cache domain (anthropic_sub, api,
      codex) is never `Cold`, so force-newest never fires for it: derive
      coldness from the lane's recorded TTL for a lane pinned to that binding
- [x] Compaction retarget rows record no `downgraded_from`/`downgraded_to`
      because policy picks the cheaper model first; derive them from the plan
- [x] `/model` switch on a warm pinned lane answers 502 "could not resolve":
      route it as the existing `ExplicitUserChange` transition cause
- [x] Untranslatable body on a streaming request: decide SSE shape and row
  - Moved to card H1 (plain 400, no row is the accepted behaviour here).
      (currently plain JSON 400, no row)

### Other outstanding work

- [x] Policy's not-configured answers lost `x-toker-not-configured`, which
      `setup/verify.rs` reads: restore it (or teach verify the new answers)
- [x] Revision one has no Chat presentations, so `/v1/models` on a Chat profile
      (opencode) lists nothing: revision one's generated publication (the
      legacy projection, listed but never routed) now renders for Chat and
      Responses; no presentations were authored, see `port-notes.md`
- [x] Sessions mid-conversation with provider-owned state (signed thinking,
      codex reasoning items) on an unpinned lane at upgrade fail closed with
      unknown binding affinity until restarted; accepted under the no-seeding
      decision, record it in the docs

- [x] OpenRouter: when toker holds its own key, strip every incoming credential
      rather than only `sk-ant-` bearers; delete `docs/plans/token-bug-claude.md`
- [x] Move the subscription signing checklist out of `docs/plans/`
- [x] Opencode session lanes per `docs/plans/opencode-session-lanes.md`, then
      unplan it

### Docs

- [x] Rewrite stale routing passages in `routing.md`, `models.md`,
      `invariants.md`, `cold-gate.md`, `lanes.md`, `ledger-schema.md` and the
      internals `README.md` file map (add `src/policy/`)
- [x] Document policy lifecycle, resolver, fallback commitment, lane/cache
      domains, attempt read models, publication ownership, control operations,
      lineage evidence and migration recovery
- [ ] Check `README.md` and `AGENTS.md` against the result

### Reconcile and unplan

- [ ] Re-read both live-model-routing plans against the code, including the
      auxiliary bare `claude-*` scenario with no Anthropic binding; record
      anything consciously dropped
- [ ] `unplan:` the two live-model-routing plans
