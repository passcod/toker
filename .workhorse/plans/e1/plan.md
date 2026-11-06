# Setup ownership and legacy import

Changeset 17 of `docs/plans/live-model-routing-implementation.md`. The daemon
already establishes revision one from TOML at startup when no active revision
exists, `toker picker sync` already reconciles the active policy's Claude rows,
and `Config::to_owned_file` / `write_owned_config` already omit operational
routing fields. Nothing in setup uses them yet: the wizard still asks for
protocol defaults, writes them and every legacy routing field back to TOML, and
installs the picker timer as an OpenRouter feature. The TUI has no import
action.

## Decisions

- Setup establishes revision one itself, in the database, before it writes a
  config without routing fields. The ordering is the recovery: a crash after
  activation leaves a database-owned machine whose next setup run rewrites the
  config; a crash before it leaves the old config and no active revision, so
  either setup or the daemon's startup activation compiles it as before. A
  config without routing fields is never on disk beside a database with no
  active revision.
- Once the database owns routing, setup rewrites drop the legacy routing
  fields (defaults, model maps, picker rules, `force_newest`, `compact_model`)
  and say so. Nothing is lost: revision one is their compiled form and stays
  reactivatable. A setup run that keeps the config as is leaves it untouched.
- Under database ownership the wizard does not ask for protocol defaults; it
  reports the active revision and points at the routing TUI.
- Import-to-draft runs in the TUI: it reloads `toker.toml`, builds the legacy
  document from it, and replaces the draft through the existing guarded
  `PUT /_toker/routing/draft` with the loaded generation. No new control
  endpoint. Activation stays the normal validate-and-activate flow.
- The picker timer becomes Claude publication, not an OpenRouter feature: it is
  installed whenever a Claude frontend is detected. `picker::sync` loses its
  legacy OpenRouter-rules rendering branch (an absent active revision is an
  error naming setup and the service), and `toker picker defaults` goes. The
  rule types and `default_rules` stay as revision-one compiler input. This
  pulls the picker part of changeset 18 forward; the rest of 18 is untouched.

## Also changed

- `Config::load` and `load_from` no longer check the legacy protocol defaults
  against the enabled backends, so an owned config with several backends and
  no defaults loads everywhere. `Config::validate` still does, and is called
  by `establish_revision_one` and by the TUI import.
- `write_config` is the only config writer and always omits legacy routing
  fields; the full-fidelity writer and `to_file` (now test-only) are retired.
- The legacy picker rendering (`picker::rows`, `eligible`, `resolve_behaves_as`,
  `patchers::patch_model_picker`, the openrouter listing fixture) went with
  `picker defaults`.

## Topology without credentials

- [x] Build revision one from a credential-free registry
  (`RouteRegistry::configured_bindings`) in setup and the TUI. The legacy
  builder reads model maps from the config rather than from constructed
  providers, so no provider (and no codex login) is needed;
  `legacy::establish_revision_one` is the shared compile-and-activate used by
  startup and setup.
- [x] Move `publication_targets` out of `server/mod.rs` to where setup can call
  it, so setup and startup establish identical publication rows.

## Setup activation and ownership

- [x] Detect routing ownership in `detect_state`: read the policy state from
  `db_path` (absent file means not owned). A database that cannot be read stops
  the wizard before anything changes.
- [x] Report ownership in the detection summary (active revision id and
  source).
- [x] Not owned, with choices: apply choices to the config in memory, create
  the state dir, compile and validate the legacy document, call
  `Store::activate_revision_one`, then write with `write_owned_config`. A
  compile or activation failure stops before the config write.
- [x] Owned, with choices: skip the default-backend questions, write with
  `write_owned_config`, and report any legacy routing fields the rewrite
  dropped.
- [x] Replace the write-step summary's `anthropic → …; openai_chat → …` line,
  and the codex_sub translated-route note that names
  `[providers.codex_sub.model_map]`, with a line pointing at the routing TUI.
- [x] Keep verify, frontend patching and units ordering unchanged; confirm
  verify still passes against a daemon serving the setup-activated revision.

## Import legacy config into draft

- [x] Add `Import legacy config into draft` to the routing page's activation
  view, behind a confirmation that the current draft will be replaced.
- [x] Reload config, build the document and `PUT` it as the draft with the
  loaded generation (the build runs when the confirmation is accepted, on the
  page's own thread: a small file read and compile). A config that
  fails to load or build reports its error and leaves the draft alone; a stale
  generation takes the existing conflict path.
- [x] Show the resulting validation diagnostics as for any draft replacement.

## Picker timer

- [x] Install the picker units whenever a Claude frontend is detected; drop the
  OpenRouter-absent removal branch.
- [x] Reword the unit descriptions, the setup report line and the
  `toker picker sync` output from OpenRouter rows to Claude `/model` rows.
- [x] Remove the legacy rendering branch of `picker::sync` and the
  `picker defaults` subcommand.

## Docs and finalisation

- [x] Rewrite the README's picker and default-backend sections for database
  ownership: the TUI owns routing, setup writes providers and listener only,
  and import-to-draft is how TOML routing gets in.
- [x] Update the setup snapshots for the new wording and step behaviour.
- [x] Update changeset 17 in the implementation plan as done, and note the
  picker items taken from changeset 18.
