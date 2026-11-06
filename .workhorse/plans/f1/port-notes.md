# Porting server tests to an authoritative server

Delete this file with the card. The pilot port is `tests/server_proxy.rs`;
read its diff for worked examples.

## Constructors

There is one kind of server: authoritative. `Server::new(config, store)` is the
service's (host keyring, sleep lock, cached catalogues); tests never call it.
Everything else goes through `Server::with_seams(config, store, Seams)`:

- `Seams::isolated()`: refusing sleep-lock spawner, empty `MemoryStore`
  keyring, empty catalogues. Nothing of the host.
- `.with_spawner(Box<dyn LockSpawner>)`: the awake and shutdown suites' probe.
- `.with_secrets(Arc<dyn SecretStore>)`: seed a stored key
  (`MemoryStore::set(.., "openrouter", ..)`) or `MemoryStore::unavailable()`.
- `.with_catalogs(FetchedCatalogs)`: startup catalogues.

Revision one is established during construction from the config's legacy
routing fields (`policy::legacy::establish_revision_one`), or the store's
active revision is loaded if it has one. To test a custom policy, call
`store.activate_revision_one(&policy, 1, 0, &[])` on the store *before*
building the server (see `spawn_toker_with_policy` in server_codex.rs and
server_anthropic.rs). Activating after construction now collides with the
revision the constructor already made (revision id 2, not 1): the six
`server::tests` failures listed below are this.

## Helpers

`tests/common/mod.rs`, with `mod common;`:

- `common::server(config, &store) -> Server`: isolated, no catalogues.
- `common::server_with_models(config, &store, &[("openrouter", &["z-ai/glm-5.3"])])`:
  catalogues listing exactly those ids. Sources: `openrouter`, `openai_api`,
  `anthropic`, `codex_sub`.
- `common::server_with(config, &store, seams)`, `common::catalogs(&[..])`.
- `common::serve(server).await -> SocketAddr`: bind loopback and spawn.

Library side: `FetchedCatalogs::listing([(source, &[ids])])` (presence only, no
windows or prices). For richer listings keep using the `parse_*` functions and
`Seams::with_catalogs`. Prefer passing catalogues through the seams over
`server.install_catalog(..)` after construction: revision one's inventory then
matches what a restart would see.

A catalogue is not needed to route: an absent catalogue is permissive, and the
generated candidates take the requested model as a provider-owned string. Only
supply one when the test is about the catalogue (cold gate writes-free
pricing, `/v1/models`, selectors, `ModelMissingFromFreshCatalogue`).

For unit tests inside `src/`, use `Server::with_seams(config, store,
Seams::isolated())`; `FetchedCatalogs::listing` works there too.

## Common failures and fixes

| Symptom | Cause | Fix |
| --- | --- | --- |
| 503 `route_unavailable` (anthropic: "no available backend") | Policy refuses a candidate whose credential is `KnownMissingOrExpired`: no stored key and none on the request. Applies to openrouter, openai_api, anthropic_api (needs a key) and anthropic_sub (needs an oauth token). | Give the backend a stored key (`api_key = Some(..)` in the config, or seeded keyring), or send `Authorization: Bearer ..` / `x-api-key` from the test client. server_proxy uses `keyed_config(upstream)`. |
| Upstream got the stored key where the test expected the client's | A held key replaces whatever credential the client brought (openrouter, see `providers/openrouter.rs`). | Assert the stored key, or drop the stored key to test pass-through. Both directions are in `auth_passes_through_when_present_and_injects_when_absent`. |
| `policy_routing.is_none()` assertions | Every row now carries policy provenance. | Assert `is_some()`; assert fields only when the test is about them. |
| `store.load_lanes().is_empty()` | Policy pins a lane (`pin: Some(..)`) even for providers with no cache clock. | Assert the lane has no `ttl`/`prompt_tokens` instead of no lane. |
| `GET /v1/models` forwarded to the upstream / byte-identical body | `/v1/models` is a local projection of the revision's publication; nothing is forwarded. | Assert local answer and that the mock saw nothing. Revision one publishes only the picker rows (`/f/claude/v1/models`, needs an openrouter catalogue matching the default picker rules, e.g. `z-ai/glm-5.3` gives `openrouter/z-ai/glm-5.3`). The Chat listing is empty under revision one. |
| `x-toker-not-configured` marker / 404 not-configured expected | Policy answers its own way: no default for the protocol gives 502 "could not resolve", a disabled provider or no backend for an administrative path gives 503 `api_error`/server_error. The marker is no longer emitted on these paths. | Assert the policy answer and that nothing reached the upstream. A disabled-prefix refusal leaves one `route-unavailable` row. |
| Unknown to `Server::with_shadow_observer` / `CollectingObserver` | Shadow mode is gone from construction (`policy_shadow` is always `None`). | Delete the observer assertions; see below. |
| Revision id 2 where a test expects 1 | See Constructors. | Activate before building the server. |
| Config rejected at construction | `establish_revision_one` runs `config.validate()`: a default naming a disabled backend fails. | Fix the config. |

## How legacy config fields reach the revision

Tests keep configuring through `Config`; it compiles into revision one at
construction (`policy/legacy.rs`):

- `default_backend_anthropic`, `default_backend_openai_chat`, and codex_sub for
  Responses become protocol defaults; every provider prefix
  (`openrouter/..`, `anthropic_api/..`) becomes a rewrite to that provider's
  route, even while disabled; `anthropic/..` goes to the Anthropic default.
- Each provider's `model_map` becomes an alias policy on its candidate (applied
  by policy, not by the provider).
- `gates.force_newest` is the `enabled` flag of each backend's upgrade policy
  (learning comes from the ledger via `BindingLearningView`, seeded from the
  rows present at construction).
- `gates.compact_model` becomes the compaction policy.
- `openrouter.picker` (or the default rules) becomes the claude-profile
  presentations.
- A provider block that is absent or disabled makes its candidate disabled.

Changing `config` after construction has no effect on routing; build the
server with the final config.

## Shadow and force-newest tests

- Shadow tests (anything built on `CollectingObserver`, `ShadowComparison`,
  `ShadowRecord`, `with_shadow_observer`, "publication match" waits): delete.
  Shadow mode no longer exists in construction and is removed entirely by a
  later step. Keep the traffic assertions in the same test; drop only the
  observer ones. server_proxy's `shadow_observation_is_one_per_request_and_cannot_alter_chat_traffic`
  was deleted whole: it asserted only observer behaviour (one record per
  request, a panicking observer not affecting the response). The observer's
  panic isolation goes with shadow mode; `poisoned_meters_state_never_blocks_the_openai_path`
  still covers accounting state failing without breaking a session.
- Force-newest tests that drove the legacy `decide`/`ModelStore::force_target`
  and asserted lane `forced_from`/`forced_to`: port to revision one's upgrade
  policy (learned/served evidence, `upgraded_model` in the row's policy
  provenance), or delete when `policy::resolve`'s tests already cover the same
  decision. Do not weaken a "stays quiet when it should" assertion.

## Remaining at the pilot's end

Integration tests (all compile, all still on the mechanical migration):
server_anthropic.rs (45 failing), server_awake.rs (8), server_codex.rs (6),
server_cold.rs (1), server_force_newest.rs (7), server_session.rs (1),
server_shutdown.rs (2). server_routing_control.rs passes.

Unit tests: `src/server/mod.rs` `mod tests` (6 failing, the revision-id
collision above), plus a sweep of `src/server/{anthropic,codex,inference,
proxy,record,record_anthropic,attempt,cancel,control}.rs` test modules,
`src/tui/routing/import.rs` and `src/cmds.rs` for anything that assumes a
server without an active revision (they compile and pass today).

## Findings the pilot surfaced (decide, do not just port around)

- Under revision one `/v1/models` for a Chat profile (opencode) lists nothing:
  `render_policy` renders presentations only, and revision one has none for
  Chat. A missing catalogue also reads as an empty picker (200) where the
  legacy path answered 503 `catalog_unavailable`.
- Policy not-configured answers lost the `x-toker-not-configured` marker that
  `setup/verify.rs` reads; verify now sees toker's own 502/503 instead.
- Compaction retarget provenance: under an authoritative server policy picks
  the cheaper model before the compaction transform runs, so
  `retarget_compaction` sees no model change and rows record
  `downgraded_from`/`downgraded_to` as None (the TUI detail view reads them).
  `server_cold`'s retarget test now asserts the policy outcome's baseline
  instead. Decide whether the compaction branch of `anthropic.rs` should
  derive them from the plan (`baseline_model` vs `retargeted_model`).
- The compaction retarget target comes from the binding's own learning
  (`merge_binding_learning`), published as a snapshot when the server is built;
  seeding the legacy `ModelStore` (`upsert_model`) or seeding after
  construction has no effect. Tests seed the store before building the server.
