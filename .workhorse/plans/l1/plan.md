# Moving configuration into the store

Implementation notes and build order for L1.
The specs are the normative source; this records how the code gets there and why.

## Shape

`Config` stays as the resolved in-memory view.
Only its source changes: store rows and the environment, rather than TOML.
That keeps the blast radius to its construction rather than its several hundred read sites.

The file-only routing fields (`default_backend_*`, per-provider `model_map`, `openrouter.picker`, `gates.compact_model`, `gates.force_newest`) stay as fields on `Config`, left at their empty defaults when it is loaded from the store.
`policy::legacy` then needs no change at all: on an import it sees the file's values, and on a fresh machine it sees the empty defaults and authors revision one from the enabled providers alone, which is what RMIG already requires.

Two settings never reach the store: `db_path` (XDG default, `TOKER_DB`) and `port` (`DEFAULT_PORT`, `PORT`).
They stay fields on `Config` because every reader already treats them as resolved configuration.

## Build order

Each phase compiles and keeps the suite green.

### 1. Store tables and the row layer

- [x] Migration appending `config_providers`, `config_settings`, `config_notices`
- [x] `store::config` read and write for each table
- [x] State directory forced to owner-only on open
- [x] `activate_revision_one_importing`, writing rows inside the revision-one transaction

### 2. The file becomes import-only

- [x] `Config::from_store`, resolving the same view the file produced
- [x] `PORT` and `TOKER_DB` resolve beside it, neither reachable from the store
- [ ] Move the TOML types and parsing into their own module, reached only by setup's import
- [x] `Config::load` reads the store, not the file
- [x] `FileConfig::into_rows` for what the import writes
- [x] Retire `TOKER_PORT` and `TOKER_UPSTREAM`, both still read on the file path

### 3. The gate

- [x] Every command except setup refuses while a configuration file exists
- [x] The service refuses the same way, exiting so the supervisor does not retry

### 4. Live edits

- [x] Server holds the config behind a swap, as `PolicyRuntime` holds the active revision
- [x] `config snapshot`, `config edit`, `config credential` control operations
- [x] Edit-time validation: refuse the value, leave the row as it was
- [x] A stored setting that cannot be used disables what it describes and never stops a start

### 5. Setup

- [x] Import: rows and revision one in one transaction, then delete the file
- [ ] Fresh machine: ask one provider and its credential, nothing more
- [x] Discover the Claude and codex directories their own way, carry them into the unit
- [x] Re-exec into the dashboard at the end

### 6. Dashboard pages

- [ ] Providers page
- [ ] Settings page
- [ ] Both report the service not answering rather than offering dead forms

### 7. Removals

- [x] Codex `originator` becomes a constant
- [x] `claude_credentials_path` and codex `auth_path` become discovery
- [ ] Prune what `config.rs` no longer needs

## Decisions worth keeping

A configuration error degrades rather than refusing a start, because the dashboard reaches configuration through the service: a service that will not start over a setting leaves no way to fix that setting.
Routing keeps the opposite rule, which RMIG already explains.

The file's existence is the whole ownership marker.
Every command except setup refusing while it exists means no row can be edited between the import and the delete, so an interrupted import is repaired by running setup again and no marker row is needed.

Endpoints stay configurable, subscription providers included, because Toker is itself a proxy and another may reasonably sit in front of it.
Values that identify Toker to a provider do not, being facts about the provider rather than preferences.


## Known gap

A service unit written before `RestartPreventExitStatus` existed still restarts into the refusal.
Systemd's own start limit bounds the loop, and setup rewrites the unit, so the window is "until setup is run" — which is the thing the refusal is telling the operator to do.
