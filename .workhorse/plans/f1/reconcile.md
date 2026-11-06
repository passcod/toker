# Reconciling the live-model-routing plans against the code (F1)

Code audited at 44c1c1c (no source changes since); internals docs audited at
33d8369, which landed while this ran. Read-only: no source, test or doc was edited. Statuses: PRESENT,
PARTIAL, MISSING, MOVED (consciously moved or dropped, with where it is
recorded), OPEN (promised, and still an unticked step in `plan.md`).

## Summary

Counts across the detail tables (rows): 92 PRESENT, 7 PARTIAL, 1 MISSING, 3 MOVED, 4 OPEN.

Not present or only partly present, most serious first:

| # | Item | Status | Judgement |
| --- | --- | --- | --- |
| 1 | Auxiliary bare `claude-*` calls with no Anthropic binding | MISSING | Bug against the completion criteria. On a config with an OpenRouter binding and no Anthropic binding, revision one has no rule for them: 502, no row. The generator meant to cover it can never fire. No test. See the mandatory section. |
| 2 | Routing TUI cannot create records or touch defaults | MISSING | Gap vs design, and README contradicts the code. No `n` add key, defaults are not listed, and editing is raw record JSON, not typed forms. README says "Make it the Chat default in the routing page". |
| 3 | Revision-one legacy learning is never consulted, and the ledger replay is never run | MISSING | Gap vs design with a visible effect on an upgraded ledger: promotions and learned days in the old `models` table are ignored, and binding learning starts empty. Not recorded anywhere as dropped. |
| 4 | Routing page content: availability, cost-boundary marks, pinned lanes, family/version examples, upgrade winner, per-presentation desired/applied state, activation cost-boundary changes | PARTIAL | Gap vs design. The page is a master/detail over the raw typed records plus a few status lines. |
| 5 | Responses `/v1/models` cannot publish a virtual presentation | PARTIAL | Gap vs design, recorded in `docs/internals/publication.md`; the whole listing goes 503 instead. |
| 6 | Declared context windows have no production writer | PARTIAL | Cosmetic. `learned_models_by_binding.context_window_json` is only written by tests; `note_observed` passes `None`. The old writer (the merge endpoint) is the thing F1 deleted. |
| 7 | Internals docs | OPEN | Mostly landed during the audit (up to 33d8369). Left: `routing.md` line 53 ("Unknown and unprefixed profiles keep path-driven forwarding", but `/v1/models` is a policy projection for every profile), the provider-owned-state-on-unpinned-lane note, the `policy.md` auxiliary-route paragraph, README's "make it the default in the routing page". |
| 8 | `plan.md` bookkeeping | cosmetic | Steps still unticked that the log shows done: `compact_spec` reads, OpenRouter bearer (36522b7), signing checklist (604dd2a), opencode lanes (2c02e92, aae82a1). The streaming untranslatable-body decision is genuinely open. |
| 9 | Structure differs from the plan's file names | cosmetic | No `src/publication/` (it is `src/picker.rs`), no `src/server/inference/` directory (`inference.rs` plus `attempt.rs`), no `tui/routing/form.rs`, error classification lives in `server/attempt.rs` rather than behind each backend adapter, `PresentedModel` is `ModelOffer`. |

Design promises the code contradicts: items 1, 2, 3, 4 and 5 above; and, by the
decision recorded in `plan.md`, "existing warm lanes remain stable" for lanes
that predate policy (they resolve under the active revision like new lanes;
no pin seeding). That last one is MOVED, not a defect.

## Mandatory: auxiliary bare `claude-*` with no Anthropic binding

Scenario: config with only `[providers.openrouter]` (a key), revision one
established at first start, Claude Code calling `/f/claude/v1/messages` with
`claude-haiku-4-5` (classifier, title, helper) and `thinking: disabled`.

Code path, in order:

1. `server/anthropic.rs` builds `RequestFacts` (profile `claude`, protocol
   Messages, `requested_model = claude-haiku-4-5`) and calls
   `resolve_policy_attempt`, which reaches `policy::resolve::lookup`.
2. Exact presentations: the only ones are the picker's `openrouter/<id>` slugs
   (`legacy-picker-*`); no match. Profile and protocol rewrites: the generated
   ones are the four provider-prefix rewrites `^<provider>/(.+)$` and, only
   when `default_backend_anthropic` is set, `legacy-anthropic-protocol-alias`;
   none matches a bare name. `legacy-auxiliary-claude` is not generated (below).
3. Defaults: `default_backend_anthropic` is `None` for an OpenRouter-only
   config (`config::default_backend` derives it from anthropic_sub,
   anthropic_api and codex_sub only), so `add_defaults` writes no Messages
   default. `default_reference` finds none.
4. Result: `ResolveFailure::NoRoute` ("no presentation, rewrite, or default
   route matched"). `anthropic.rs` answers 502 `api_error` "the active routing
   policy could not resolve this request" with `x-toker-not-configured:
   anthropic`, logs an ERROR, and writes no ledger row (only
   `RouteUnavailable` writes a `route-unavailable` row), no lane, no upstream
   call. Content and request shape are never inspected, so the "no
   inspection" half holds; the "routes by policy to a configured backend" half
   does not.
5. Same on this config: `count_tokens` 503 "no tokenizer backend for this
   model", `/v1/files` 503 "no backend for this path".

Confirmed by running the built binary (`target/debug/toker serve`, temp
config, state dirs under /tmp, mock upstream): bare `claude-haiku-4-5` gave 502
with the marker and the upstream saw nothing; `openrouter/z-ai/glm-5.3` on the
same server gave 200, the ledger row had `rewrite_id =
legacy-prefix-anthropic-openrouter`, and the response echoed the requested
model. Only that one row existed afterwards.

Why the generator never fires: `add_auxiliary_claude_route` (`policy/legacy.rs`)
runs only when `default_backend_anthropic` is `None` and some configured
Messages binding has a model map. `model_map_for` returns a map only for
anthropic_sub, anthropic_api and codex_sub, and any one of those being
configured makes `default_backend_anthropic` non-`None`, so the two conditions
are exclusive for every config `Config::validate` accepts. The unit test
`auxiliary_bare_claude_routes_without_anthropic_default_via_model_map` hides
this: its first half builds an empty topology and asserts the rule is absent
(vacuous), its second half sets `config.default_backend_anthropic = None` by
hand on an anthropic_api config, which validation rejects. The in-flight
`docs/internals/policy.md` repeats the generator's condition as if it were a
working route; that text needs to say it is not reachable, or the route needs
fixing.

What does work: codex_sub-only (the reference machine). The Messages default is
codex_sub, so bare `claude-*` takes the default route and the codex model map
alias applies (`server_codex.rs` covers it through the default, not through the
auxiliary rewrite). Anthropic-bound machines take the Anthropic default.

Parentage: nothing in `server/anthropic.rs` sets lineage (`lineage: None`), so
none is invented. No test asserts it for a side query with a fresh session id.

Tests: none covers the scenario at server level. Needed:

1. First decide what revision one should do for OpenRouter-only. OpenRouter has
   no model map, so there is no generated target for a haiku-class call; the
   options are to generate nothing and say so (then the 502 is the documented
   outcome) or to author a rule from the picker `behaves_as` rows. Then either
   path needs a test.
2. `tests/server_anthropic.rs`: OpenRouter-only config (no anthropic_sub, as in
   `openrouter_config` minus the sub), POST bare `claude-haiku-4-5` with
   `thinking: disabled` and a fresh `x-claude-code-session-id`. Assert the
   decided outcome: if routed, 200, the mock saw the policy-chosen model, the
   row has provider openrouter, `rewrite_id`, `requested_model =
   claude-haiku-4-5`, and `parent_session_id`/`child_role`/`relation_source`
   NULL; if refused, 502 with the marker, no upstream traffic, no row.
3. `policy/legacy.rs`: replace the vacuous test with one over validated
   configs, asserting the auxiliary rewrite is generated for the configs the
   design means, or deleting `add_auxiliary_claude_route` if it cannot be.
4. A policy-authored `^claude-` rewrite (via `store.activate_revision_one`
   before building the server, as `spawn_toker_with_policy` does) routing to the
   OpenRouter route: proves policy can serve these calls without content
   inspection, independent of what revision one generates.
5. A cold compaction call on a bare `claude-*` model, same config.

## Table by changeset

### Settled contracts and design sections (cross-cutting)

| Item | Status | Evidence |
| --- | --- | --- |
| Frontend scope: profile beats protocol; `unprefixed` is a real profile; defaults use same precedence | PRESENT | `document::FrontendScope`; `resolve::lookup`/`default_reference`; `Server::request_facts` falls back to `unprefixed` |
| Stable binding id `<provider>.<dialect>` | PRESENT | `routing.rs` `BACKEND_BINDINGS`; `BindingId`; ids in rows, lanes, learning |
| Draft CAS, daemon-owned; stale writer gets conflict | PRESENT | `Store::compare_and_swap_policy_draft`; `control/routing.rs` `draft_conflict_response`; `server_routing_control.rs` |
| Active policy an immutable `Arc`, captured once per request; old revisions cached for pinned lanes | PRESENT | `policy/runtime.rs` (`REVISION_CACHE_CAPACITY = 32`, `capture`, `compile_stored_revision`) |
| Matchers exact/glob/regex via `globset`/`regex`; templates; argument bindings; capture validation | PRESENT | `policy/matcher.rs`; `ArgumentBinding`; compile diagnostics `missing-regex-capture`, `capture-binding-not-regex`, `route-arguments-mismatch` |
| Rewrite priorities unique per scope; profile before protocol | PRESENT | `duplicate-rewrite-priority`; `lookup` order |
| Version parsers (semver, date, tuple), no mixing, no name heuristics after compile | PRESENT | `policy/version.rs`; `semver` in Cargo.toml; `legacy_hyphenated` refused unless generated (`compile.rs:763`) |
| Availability tri-state; absent/stale catalogue is unknown | PRESENT | `resolve.rs` tests `fresh_missing_offer_skips_but_absent_catalogue_is_unknown_and_viable`, `unresolved_selector_does_not_authorize_preflight_fallback` |
| Cost classes and consent matrix enforced at compile | PRESENT | compile diagnostics `billed-boundary-consent-required`, `ambiguous-transport-consent-required`, `cache-loss-consent-required`; test `route_edges_enforce_cost_ambiguous_transport_and_cache_consent` |
| Health circuits are policy data; keyed; cleared on success/generation change | PRESENT | `Candidate::health_circuits`; `HealthCircuitKey`; runtime tests `generation_changes_invalidate_only_relevant_circuits`, `same_generation_failures_extend_and_success_clears_exact_target` |
| Per-attempt credential state, fresh headers per attempt | PRESENT | `Server::credential_snapshot`, `Provider::credential_availability`; server tests in `server_anthropic.rs` (`a_subscription_with_nothing_to_sign_with_is_refused_before_the_wire`) |
| Response commitment: bounded precommit retention, keepalives do not commit, client disconnect cancels | PRESENT | `server/attempt.rs` `precommit_sse`, `PRECOMMIT_BODY_CAP = 64 KiB`; `server/cancel.rs`; test `client_hangup_aborts_upstream_and_records_cancellation` |
| `ambiguous_transport` distinct, never enabled by migration | PRESENT | `FailureClass::AmbiguousTransport`; edge field `ambiguous_transport`; revision one's edges have none |
| Requested/baseline/effective/reported identities separate; response echoes requested model; absent reported stays absent | PRESENT | observed live (`model` echo `openrouter/z-ai/glm-5.3`); `BindingLearningView::note_observed` takes a reported model; `tui/detail.rs` test asserts `reported -` |
| Session lineage: guardian only on the `codex` profile, only from the raw session header, non-empty suffix; cycles/self rejected; missing parent kept | PRESENT | `server/codex.rs:119`; `SessionLineage::codex_guardian`; v7 triggers `requests_provenance_insert_check`, `requests_lineage_cycle_check`; `server_codex.rs::codex_guardian_lineage_requires_the_exact_profile_header_and_child_identity`; `tui/model.rs` tests (missing parent) |
| Claude side queries never attached | PRESENT | `anthropic.rs:827` `lineage: None`; no test pins it (see the mandatory section) |
| Virtual presentation publishes only policy-owned metadata | PRESENT | `offers::render_policy` emits slug only for virtual entries |
| Claude settings stay user-side; daemon never writes them | PRESENT | `picker.rs` run by `toker picker sync`; `tui/routing/reconcile.rs` |
| Codex/opencode use dynamic `/v1/models` | PRESENT | `server/proxy.rs::models`, `Server::active_publication` |

### 1. Policy vocabulary and canonical documents

| Item | Status | Evidence |
| --- | --- | --- |
| Document types: defaults, presentations (3 sources), routes, rewrites, classifiers, upgrades, compaction | PRESENT | `policy/document.rs` |
| Ids lowercase ASCII, unique per kind; limits | PRESENT | `StableId`, `unique_ids`, `MAX_*` consts |
| Bounded canonical JSON, duplicate keys/unknown fields/non-integers rejected, sorted keys | PRESENT | `policy/canonical.rs` `decode_policy`; tests `rejects_duplicate_unknown_and_invalid_utf8`, `numeric_positions_accept_integer_tokens_only_even_on_overflow` |
| Fingerprint over domain tag + bytes; route fingerprint excludes display and inventory | PRESENT | `POLICY_FINGERPRINT_DOMAIN_V*`; `route_fingerprint_excludes_display_and_normalizes_sets` |
| Protocol, dialect, adapter ids serialisable with stable spellings | PRESENT | `routing.rs` `ProtocolId`, `DialectId`, `BackendAdapterId` derive Serialize |
| Diagnostics with code, severity, record id, JSON path | PRESENT | `policy/diagnostic.rs` |

### 2. Topology and canonical offer inventory

| Item | Status | Evidence |
| --- | --- | --- |
| Stable binding id, cost class, capabilities, configured lookup | PRESENT | `BackendBinding`, `RouteRegistry::binding_is_configured` |
| `RouteRegistry` is topology and lookup only | PRESENT | `routing.rs` (`bindings`, `routes`, `resolve_binding`, `resolve_binding_model_free`); no prefix/alias/default code |
| `catalog::offers` emits canonical offers without frontend aliases | PRESENT | `OfferInventory`, `inventory()`; aliases only in `for_frontend_inventory`, used for revision one's generated listing (recorded in `port-notes.md`) |

### 3. Matcher, classifier, selector, compiler

| Item | Status | Evidence |
| --- | --- | --- |
| Indexed matchers, expanded presentations, candidate chains, alias, compaction, fingerprints | PRESENT | `policy/compile.rs` |
| Selector ordering: chronology / classified version / explicit, lexical only as tiebreak | PRESENT | `policy/selector.rs` |
| Structured diagnostics for invalid references and unsafe transitions | PRESENT | 27 distinct codes in `compile.rs` (`missing-route`, `duplicate-presentation-slug`, `native-presentation-shadow-consent-required`, ...) |
| Compilation independent of live availability | PRESENT | `compile` takes topology + inventory only; unconfigured providers compile |

### 4. Legacy compiler and binding-aware read interfaces

| Item | Status | Evidence |
| --- | --- | --- |
| Legacy config to in-memory document: defaults, prefixes, aliases, model maps, picker, force-newest, compaction, classifiers | PRESENT | `policy/legacy.rs` |
| Auxiliary bare `claude-*` rule | PARTIAL | generated by `add_auxiliary_claude_route` but unreachable; see the mandatory section |
| Binding-keyed learning interface | PRESENT | `policy/learning.rs` `BindingLearningView` |
| Read-only adapter over the legacy `ModelStore`; generated policies alone may consult legacy learning | MISSING | `ModelStore` deleted in d1fa80e ("nothing read the store"); `allow_revision_one_legacy_learning` is stored and validated but no resolver code reads it (`grep` finds only compile/canonical/legacy). Not recorded as dropped. |

### 5. Pure resolver and shadow equivalence

| Item | Status | Evidence |
| --- | --- | --- |
| Request facts, frontend inheritance, exact lookup, rewrite/default, candidate preflight, upgrade/alias, compaction, content-free traces | PRESENT | `policy/resolve.rs` (4077 lines incl. tests) |
| Pure: no mutation, I/O, clock | PRESENT | `resolve.rs` header; clock passed in `RequestFacts::now_ms` |
| Preview uses the same entry point | PRESENT | `Server::preview_routing_draft` -> `resolve` |
| Shadow comparison on every path; unexplained mismatches block cutover | MOVED | shadow mode removed on purpose: `plan.md` ("Remove shadow mode"), commit f49effc |

### 6. Persistence and binding-scoped state

| Item | Status | Evidence |
| --- | --- | --- |
| `routing_policy_revisions` (+ indexes, fingerprint index) | PRESENT | `schema.rs` v7, v9 (rebuild to allow equal fingerprints), v15 `compiler_origin` |
| `routing_policy_state` singleton, draft all-or-none, CAS write, activation transaction | PRESENT | v7 CHECK; `Store::compare_and_swap_policy_draft`, `activate_policy_revision`, `activate_revision_one` |
| `routing_publications` keyed `(scope_key, adapter)`, CAS result, applied implies equal pair | PRESENT | v7; `compare_and_swap_publication_result`; v13 adds `applied_identities_json` |
| `learned_models_by_binding` + merge semantics + model index | PRESENT | v7; `merge_binding_learning`; `load_binding_learnings_for_model` |
| Idempotent replay of attributable legacy rows into binding table | PARTIAL | `Store::replay_attributable_binding_learning` exists and is tested but is never called outside `store/` tests (live gap, item 3) |
| Lane pin columns, all-or-none triggers, `pin_generation`, CAS | PRESENT | v7 triggers; `compare_and_swap_lane_pin`; `policy/pinning.rs` |
| Requests provenance, attempt, trace, lineage columns, indexes, partial unique, triggers | PRESENT | v7-v12 |
| `routing_trace` in `JSON_TEXT_COLUMNS`; export/import preserve | PRESENT | `store/ledger.rs:2551`; `export.rs` tests |
| Attempt read models: logical count, totals over attempt rows, `reported` from `raw_model` | PRESENT | `count_logical_requests`; `ledger.rs:1886,1999`; `tui/detail.rs` |
| `route-unavailable` terminal row | PRESENT | v10/v12 triggers; `record_route_unavailable` |
| Observations, recency, family election, promotion on binding ids; CLI promote requires binding when ambiguous | PRESENT | `BindingLearningView`; `cmds.rs:442`; `Server::promote_routing_learning` |
| Declared context windows in binding learning | PARTIAL | column exists; no production writer (item 6) |

### 7. Runtime and revision cache

| Item | Status | Evidence |
| --- | --- | --- |
| `PolicyRuntime` on `Server`: snapshot (policy + inventory), revision cache, mutation lock, generations, circuits | PRESENT | `policy/runtime.rs`; `Server::routing_control` lock |
| Catalogue refresh swaps the inventory `Arc` without touching in-flight snapshots | PRESENT | runtime tests `captured_snapshots_do_not_change_after_inventory_replacement`, `catalogue_refresh_invalidates_only_changed_catalogue_bindings` |

### 8. Shared inference coordinator

| Item | Status | Evidence |
| --- | --- | --- |
| Messages, Chat and Responses all go through one coordinator | PRESENT | `server/inference.rs` `coordinate_messages`/`_chat`/`_responses`; called from `anthropic.rs:1360`, `proxy.rs:497`, `codex.rs:345` |
| Fresh render per attempt; attempt never mutates the baseline | PRESENT | `InferenceRequest`/`InferenceAttempt` (module header) |
| Renderers get the requested identity; observation keeps effective and reported | PRESENT | live echo check; `note_observed` |

### 9. Lane pins and the common transition

| Item | Status | Evidence |
| --- | --- | --- |
| Pin route/candidate/binding/baseline/effective/fingerprint/revision with CAS | PRESENT | `policy/pinning.rs::persist_transition` |
| One cache-domain-aware transition function; unknown TTL never cold; explicit user change | PRESENT | `policy/transition.rs` tests; `cf3edde` |
| Seed or conservatively preserve warm lanes at cutover | MOVED | `plan.md` Decisions: unpinned existing lanes resolve like new lanes, at most one cold re-read, no seeding |

### 10. Preflight availability and cost edges

| Item | Status | Evidence |
| --- | --- | --- |
| Credential state, binding checks, hard capabilities, fresh-catalogue absence, disabled candidates, keyed circuits | PRESENT | `resolve.rs` `common_denial`, `SkipReason`; tests `preflight_edges_enforce_disabled_cost_cache_capability_and_affinity` |
| Every crossed edge enforced; local quota gate never candidate unavailability | PRESENT | `every_crossed_edge_must_allow_warm_cache_loss`; quota gate stays in `quota_gate.rs` |

### 11. Post-send fallback and attempt accounting

| Item | Status | Evidence |
| --- | --- | --- |
| Error classifiers (auth, not-found, rate limit, quota, overload, server, pre-accept and ambiguous transport) | PRESENT | `server/attempt.rs` (not behind adapters, cosmetic) |
| Bounded delayed commitment; edge-conditioned retry from `resolve_after_failure` | PRESENT | `attempt.rs`; test `active_messages_policy_falls_back_before_commitment` |
| One row per attempt, group id, thinking negotiation in same group, successful fallback pinned | PRESENT | `thinking_retry_precedes_route_fallback_in_one_attempt_group` |

### 12. Revision-one cutover

| Item | Status | Evidence |
| --- | --- | --- |
| Atomic activation of revision one with ownership marker; corrupt/missing active revision fails startup | PRESENT | `Server::new` (`mod.rs:458-480`); tests `authoritative_startup_*` |
| `count_tokens`, batches, unmatched, model-free paths, publication, compaction all from policy; no bootstrap-default escape | PRESENT | `plan.md` steps ticked; `anthropic.rs` `resolve_policy_first_candidate(s)`, `resolve_policy_model_free`; tests `administrative_paths_route_by_policy_with_no_legacy_default_backend`, `count_tokens_and_batches_send_the_policy_model_not_the_mapped_one` (observed 503 when no default exists) |
| Invariant 6 updated | PRESENT | `AGENTS.md` invariant 6 |

### 13. Claude settings reconciliation

| Item | Status | Evidence |
| --- | --- | --- |
| Shared adapter, last-applied identities for ownership and collisions, applied revision persisted, async, failure leaves server active | PRESENT | `picker.rs::sync_active_policy`, `applied_identities`; `tui/routing/reconcile.rs` thread + message |
| Incompatible profile sets reported | PRESENT | `IncompatibleClaudeProfile`; test `incompatible_anthropic_profile_presentations_are_rejected` |

### 14. Control surface

| Item | Status | Evidence |
| --- | --- | --- |
| snapshot, draft PUT/PATCH, validate, preview, activate, revisions, reactivate, promote, publication | PRESENT | `server/mod.rs:1652-1667`; `control/routing.rs` |
| Verb gates, JSON content type, `deny_unknown_fields`, separate policy-body limit, no preflight route | PRESENT | `control_token_ok`, `MAX_ROUTING_POLICY_BODY`; `server_routing_control.rs` |

### 15. TUI framework and read-only views

| Item | Status | Evidence |
| --- | --- | --- |
| `Page`, `⇄` action, Tab toggle, legend overlay per page, background control client | PRESENT | `tui/mod.rs:127`; `tui/view.rs:506`; `tui/routing/client.rs` |
| Four master/detail views | PRESENT | `tui/routing/model.rs::View` |
| Views show what the design lists (availability, cost boundary, pinned lanes, parsed examples, upgrade winner, desired/applied per presentation) | PARTIAL | `tui/routing/view.rs::detail_lines` pretty-prints the selected record's JSON; no availability, cost or lane columns (item 4) |
| Dashboard groups explicit children, child-role breakdown, parent title via transcript, `reported` from `raw_model` | PRESENT | `tui/model.rs:468-567`; `tui/mod.rs:674`; `tui/detail.rs` |

### 16. TUI editing, preview, activation

| Item | Status | Evidence |
| --- | --- | --- |
| Typed forms: enum choice, matcher kind, offer completion, ordered-list movement | PARTIAL | edit is a raw JSON `Modal::Record` (Ctrl+Enter saves); candidate reordering (`m`) and promote offer completion (Ctrl+O) exist; no matcher/enum forms, no `form.rs` |
| Add a record (`n`), edit, remove with confirmation | PARTIAL | `e` edit and `d` remove exist; nothing sends `add_*` edits; defaults are not listed or editable, though the daemon supports `AddDefault`/`ReplaceDefault`/`RemoveDefault` |
| Confirmations: removal, forced warm-lane movement, activation, reactivation | PRESENT | `Modal::Remove/Activate/Reactivate`, `f` toggles forced transition |
| Conflict preserves edits, shows daemon copy, reload or reapply | PRESENT | `Modal::Conflict`; test `conflict_preserves_edits_and_offers_reapply` |
| Preview trace, revision browsing, activation scope, reactivation, promotion, publication status | PRESENT | `Modal::Preview/PreviewResult/Activate/Reactivate/Promote`, `activation_summary` (omits cost-boundary changes) |
| Claude reconciliation after activation, decoupled | PRESENT | `reconcile.rs` |

### 17. Setup ownership and legacy import

| Item | Status | Evidence |
| --- | --- | --- |
| Setup activates before writing a config without routing fields; owned machine never asked or replaced; recovery of the config/database transaction | PRESENT | `wizard.rs:1983-2023`, `activate_revision_one`; test `policy_owned_write_omits_operational_routing_fields` |
| Import-to-draft in the TUI status view | PRESENT | `tui/routing/import.rs`, `Modal::Import` |
| Picker timer for any Claude frontend; legacy picker rendering and `toker picker defaults` removed | PRESENT | note under changeset 17 in the implementation plan; `picker.rs` header |

### 18. Remove superseded machinery, document internals

| Item | Status | Evidence |
| --- | --- | --- |
| Runtime model-map application removed (providers, handlers, `map_model`, `apply_model_map`) | PRESENT | no matches in `src/`; `middleware/model_map.rs` kept for the legacy compiler and batch splice |
| Default/prefix interpretation out of `RouteRegistry` | PRESENT | `routing.rs` |
| Hard-coded family/newer-than replaced by classifier output | PRESENT | `policy/family.rs` used only by revision-one matchers and the generated classifier |
| `force_newest` reduced and moved under `policy/` | PRESENT | `policy/force_newest.rs` (135 lines of constants and `prompt_bound`); `ModelStore`, `decide`, `ForceContext` gone |
| Old picker rendering path removed | PRESENT | `picker.rs` is the policy sync; `PickerRule` is legacy input only |
| `compact_spec` runtime reads removed | PRESENT | no runtime read; `plan.md` still shows the box unticked |
| Dual-written `ModelStore` and `/_toker/models/merge` removed | PRESENT | d1fa80e; `src/middleware/models.rs` gone |
| Shadow mode removed | MOVED | `plan.md`; f49effc |
| Tests ported to an authoritative server | PRESENT | `plan.md` ticks; `tests/common` |
| Docs: policy lifecycle, resolver, fallback commitment | PRESENT | `docs/internals/policy.md` (its auxiliary-route paragraph is wrong, see the mandatory section) |
| Docs: lane/cache domains | PRESENT | `docs/internals/lanes-and-cache.md` |
| Docs: attempt read models, route-unavailable rows | PRESENT | `docs/internals/ledger-schema.md`, `docs/internals/attempts.md` |
| Docs: publication ownership | PRESENT | `docs/internals/publication.md` |
| Docs: control operations | PRESENT | `docs/internals/control.md` |
| Docs: lineage evidence | PRESENT | `docs/internals/lineage.md` (exists at 33d8369; content not re-audited line by line) |
| Docs: migration recovery | PRESENT | `docs/internals/migration.md` (exists at 33d8369; content not re-audited line by line) |
| Docs: stale routing passages rewritten (`routing.md`, `models.md`, `invariants.md`, `cold-gate.md`) | PARTIAL | `routing.md`, `models.md` and the control verbs are rewritten; `routing.md:53` still says unknown and unprefixed profiles keep path-driven forwarding. `invariants.md`, `compaction.md`, `notices.md` had uncommitted edits at audit time |
| README and AGENTS checked against the result | OPEN | AGENTS.md is consistent with the code. README's two "make it the default in the routing page" passages (OpenAI Chat, Codex Chat) cannot be done in the TUI (item 2) |
| Sessions with provider-owned state on an unpinned lane fail closed (400 "unknown binding affinity") until restarted; record in docs | OPEN | `anthropic.rs:551`; `plan.md` asks for a docs note |
| OpenRouter placeholder bearer, signing checklist, opencode lanes | PRESENT | 36522b7, 604dd2a, 2c02e92 + aae82a1 (`docs/plans/` holds only the two live plans) |
| Untranslatable streaming body: SSE shape and row | OPEN | `plan.md`; currently plain 400 and no row |
| The two plans themselves removed in an `unplan:` commit | OPEN | last step; do it after items 1-3 are fixed or recorded as moved |
