//! Database schema and migrations.
//!
//! Migrations are a numbered, append-only list gated by `PRAGMA user_version`:
//! entry `i` (0-based) migrates a database from version `i` to `i + 1`, and a
//! fresh database runs every entry in order, each inside its own transaction
//! together with its `user_version` bump. There are no external migration
//! files — the list lives in code so the binary always carries its own
//! history.
//!
//! **Append-only rule**: never edit, reorder, or delete an existing entry. A
//! database in the wild whose `user_version` is `n` has had exactly entries
//! `0..n` applied, by position. Fixes and changes must be *new* entries
//! (`ALTER TABLE …`, or the copy-into-a-new-table pattern for column
//! reshaping), so old databases keep matching the old entries.

use super::{Error, Result};
use rusqlite::Connection;

/// The append-only migration list. `PRAGMA user_version` counts entries
/// applied; keep in sync with the row model in [super::ledger] and the state
/// structs in [super::state].
pub(super) const MIGRATIONS: &[&str] = &[
    // v1 — initial schema: the requests ledger, state tables, and meta.
    r#"
    --requests: the insert-only ledger, one row per request.
    -- kind IS NULL marks a real API measurement; every other kind is
    -- proxy-written (spec: STORE#not-recorded-is-not-zero: proxy rows are never API measurements).
    -- No content columns — digests, counts, lengths only (spec: SEC#no-prompt-or-completion-content-is-recorded).
    -- Absence is stored as NULL, never as a default (spec: STORE#not-recorded-is-not-zero); ts_ms is
    -- the only required column besides the rowid.
    CREATE TABLE requests (
        id                  INTEGER PRIMARY KEY,
        ts_ms               INTEGER NOT NULL,
        duration_ms         INTEGER,
        kind                TEXT,
        frontend            TEXT,
        provider            TEXT,
        route               TEXT,
        session_id          TEXT,
        ping                INTEGER,
        model               TEXT,
        raw_model           TEXT,
        requested_model     TEXT,
        effective_model     TEXT,
        input               INTEGER,
        cache_read          INTEGER,
        cache_write_total   INTEGER,
        cache_write_5m      INTEGER,
        cache_write_1h      INTEGER,
        output              INTEGER,
        reasoning           INTEGER,
        iterations          INTEGER,
        web_searches       INTEGER,
        code_execs          INTEGER,
        ttl_split_known     INTEGER,
        usage_presence      TEXT,
        usage_raw           TEXT,
        cost_usd            REAL,
        cost_kind           TEXT,
        rate_limits         TEXT,
        req_bytes           INTEGER,
        req_messages        INTEGER,
        req_tools           INTEGER,
        tools_hash          TEXT,
        system_chars        INTEGER,
        system_hash         TEXT,
        system_blocks       TEXT,
        system_messages     INTEGER,
        compact_generations INTEGER,
        summarising         INTEGER,
        system_change       TEXT,
        system_ladder       TEXT,
        system_tail         TEXT,
        gate_on             INTEGER,
        cold_on             INTEGER,
        forced_from         TEXT,
        forced_to           TEXT,
        downgraded_from     TEXT,
        downgraded_to       TEXT,
        cache_stripped      INTEGER,
        system_merged       INTEGER,
        model_mappings      TEXT,
        drift_digest        TEXT,
        status              INTEGER,
        error_type          TEXT,
        retry_after_ms      INTEGER,
        extra               TEXT,
        betas               TEXT,
        geo                 TEXT,
        fast                INTEGER
    );

    -- Window queries anchor on ts_ms (rows arrive slightly out of order);
    -- the TUI and report walk per-session windows.
    CREATE INDEX requests_ts_ms_idx ON requests (ts_ms);
    CREATE INDEX requests_session_id_idx ON requests (session_id);

    --State tables: small, read-modify-write
    -- data, upserted by the caller. Lanes are keyed by the composite
    --"sessionId|toolsHash".
    CREATE TABLE lanes (
        key           TEXT PRIMARY KEY,
        session_id    TEXT,
        tools_hash    TEXT,
        updated_ms    INTEGER NOT NULL,
        prompt_tokens INTEGER,
        ttl           INTEGER,
        ping          INTEGER
    );

    -- Learned models: days served and the max prompt observed per exact
    -- model identity, plus the hand-verified context-window catalogue entry.
    CREATE TABLE models (
        model_id            TEXT PRIMARY KEY,
        days_json           TEXT,
        max_prompt          INTEGER,
        context_window_json TEXT
    );

    -- Allowances opened against a quota window, keyed by reset value so a
    -- new window starts clean while old rows self-expire at load time
    --.
    CREATE TABLE allowances (
        session_id  TEXT NOT NULL,
        meter       TEXT NOT NULL,
        reset_value INTEGER NOT NULL,
        PRIMARY KEY (session_id, meter, reset_value)
    );

    --Ping runs: one insert per `claude -p` probe.
    CREATE TABLE pings (
        id          INTEGER PRIMARY KEY,
        ts_ms       INTEGER NOT NULL,
        exit_code   INTEGER,
        duration_ms INTEGER,
        boundary_ms INTEGER
    );

    -- Last meter snapshot, single row: every response updates it when the
    --backend has meters. The JSON shape is
    -- meter-source-dependent (anthropic sub today, codex maybe later), so
    -- it is stored whole rather than in typed columns.
    CREATE TABLE meters_state (
        id         INTEGER PRIMARY KEY CHECK (id = 1),
        updated_ms INTEGER NOT NULL,
        snapshot   TEXT NOT NULL
    );

    -- Generic store-level key/value facts (e.g. created_at). The schema
    -- version is NOT stored here — that is PRAGMA user_version.
    CREATE TABLE meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // v2 — lanes: the cold gate's noticedAt (decideCold's separation: `at`
    // moves only when a response reaches upstream, `noticedAt` only when
    // the notice fires, so a notice never resets the lane's cache clock)
    // and the sticky-upgrade record (`forced: {from, to}` — an upgrade is
    // decided once, and every later request in that conversation must stay
    // on the model its cache now lives on). Additive ALTER TABLE only,
    // per the append-only rule; a v1 row reads the new columns as NULL.
    r#"
    ALTER TABLE lanes ADD COLUMN noticed_at INTEGER;
    ALTER TABLE lanes ADD COLUMN forced_from TEXT;
    ALTER TABLE lanes ADD COLUMN forced_to TEXT;
    "#,
    // v3 — meters_state becomes per-provider: the anthropic sub's quota
    // meters and the codex sub's usage limits are different shapes with
    // different consumers (the quota gate reads only the anthropic_sub
    // slot), so each meter source keeps its own last snapshot under its
    // provider id. The copy-into-a-new-table pattern per the append-only
    // rule: the existing single slot becomes the anthropic_sub row (its
    // only writer), so imported/old data survives the migration, and the
    // old table goes — nothing reads it after this entry.
    r#"
    CREATE TABLE meters_by_provider (
        provider_id TEXT PRIMARY KEY,
        updated_ms INTEGER NOT NULL,
        snapshot    TEXT NOT NULL
    );
    INSERT INTO meters_by_provider (provider_id, updated_ms, snapshot)
        SELECT 'anthropic_sub', updated_ms, snapshot FROM meters_state;
    DROP TABLE meters_state;
    "#,
    // v4 — pings record what the run decided and what it achieved, as the
    // predecessor's pings.json did: the slot it served, the action
    // ('ping', 'skip' while a window is already open, 'failed' when the
    // client did not complete), the boundary read back off the ledger
    // (observed_ms, beside boundary_ms, the prediction), whether the two
    // matched, and whether the decision was assumed for want of any
    // meter reading. A prediction recorded as if it were the outcome is
    // the failure this exists to prevent. Additive ALTER TABLE only; a v3
    // row reads every new column as NULL — unknown, not "no".
    r#"
    ALTER TABLE pings ADD COLUMN slot TEXT;
    ALTER TABLE pings ADD COLUMN action TEXT;
    ALTER TABLE pings ADD COLUMN observed_ms INTEGER;
    ALTER TABLE pings ADD COLUMN verified INTEGER;
    ALTER TABLE pings ADD COLUMN assumed INTEGER;
    "#,
    // v5 — a lane index on the ledger. Capture now compares each request's
    // system prompt with its lane's previous row, read from the ledger on
    // every anthropic response, and keeps the ladders only where the
    // prompt changed, so finding a baseline's rungs also walks back
    // through the lane. Both reads are by session × tools hash, newest
    // first; the session index alone would scan and sort a whole session
    // per response. Existing rows are indexed, not rewritten.
    r#"
    CREATE INDEX requests_lane_idx ON requests (session_id, tools_hash, ts_ms);
    "#,
    // v6 — which release an allowance is: `overage` (the burn marker,
    // every allowance before this column existed) or `plan` (the over
    // marker: past the gate only while the plan has room). The default
    // is what every existing row was granted as.
    r#"
    ALTER TABLE allowances ADD COLUMN release TEXT NOT NULL DEFAULT 'overage';
    "#,
    // v7 — live-routing storage. The singleton active pointer starts NULL
    // and this migration never changes it; the server establishes revision
    // one from the config's routing fields the first time it starts.
    r#"
    CREATE TABLE routing_policy_revisions (
        revision_id              INTEGER PRIMARY KEY AUTOINCREMENT,
        created_ms               INTEGER NOT NULL,
        activated_ms             INTEGER NOT NULL,
        description              TEXT,
        activation_scope         TEXT NOT NULL CHECK (
            activation_scope IN ('new_lanes', 'new_and_cold_lanes', 'immediate')
        ),
        forced_cache_transition  INTEGER NOT NULL CHECK (
            forced_cache_transition IN (0, 1)
        ),
        source                   TEXT NOT NULL CHECK (
            source IN ('legacy', 'draft', 'reactivation')
        ),
        parent_revision_id       INTEGER REFERENCES routing_policy_revisions(revision_id),
        document_json            TEXT NOT NULL CHECK (json_valid(document_json)),
        fingerprint              TEXT NOT NULL UNIQUE CHECK (
            length(fingerprint) = 64
            AND fingerprint NOT GLOB '*[^0-9a-f]*'
        )
    );
    CREATE INDEX routing_policy_revisions_activated_idx
        ON routing_policy_revisions (activated_ms);
    CREATE INDEX routing_policy_revisions_parent_idx
        ON routing_policy_revisions (parent_revision_id);

    CREATE TABLE routing_policy_state (
        id                       INTEGER PRIMARY KEY CHECK (id = 1),
        active_revision_id       INTEGER REFERENCES routing_policy_revisions(revision_id),
        draft_generation         INTEGER NOT NULL DEFAULT 0 CHECK (draft_generation >= 0),
        draft_base_revision_id   INTEGER REFERENCES routing_policy_revisions(revision_id),
        draft_document_json      TEXT CHECK (
            draft_document_json IS NULL OR json_valid(draft_document_json)
        ),
        draft_fingerprint        TEXT CHECK (
            draft_fingerprint IS NULL OR (
                length(draft_fingerprint) = 64
                AND draft_fingerprint NOT GLOB '*[^0-9a-f]*'
            )
        ),
        draft_updated_ms         INTEGER,
        legacy_imported_ms       INTEGER,
        CHECK (
            (draft_base_revision_id IS NULL
             AND draft_document_json IS NULL
             AND draft_fingerprint IS NULL
             AND draft_updated_ms IS NULL)
            OR
            (draft_base_revision_id IS NOT NULL
             AND draft_document_json IS NOT NULL
             AND draft_fingerprint IS NOT NULL
             AND draft_updated_ms IS NOT NULL)
        )
    );
    INSERT INTO routing_policy_state (id) VALUES (1);

    CREATE TABLE routing_publications (
        scope_key                    TEXT NOT NULL,
        adapter                      TEXT NOT NULL,
        desired_revision_id          INTEGER NOT NULL
            REFERENCES routing_policy_revisions(revision_id),
        applied_revision_id          INTEGER
            REFERENCES routing_policy_revisions(revision_id),
        desired_inventory_generation INTEGER NOT NULL CHECK (
            desired_inventory_generation >= 0
        ),
        applied_inventory_generation INTEGER CHECK (
            applied_inventory_generation >= 0
        ),
        state                        TEXT NOT NULL CHECK (
            state IN ('pending', 'applied', 'failed', 'unsupported')
        ),
        detail_code                  TEXT,
        updated_ms                   INTEGER NOT NULL,
        PRIMARY KEY (scope_key, adapter),
        CHECK (
            (applied_revision_id IS NULL
             AND applied_inventory_generation IS NULL)
            OR
            (applied_revision_id IS NOT NULL
             AND applied_inventory_generation IS NOT NULL)
        ),
        CHECK (
            state != 'applied'
            OR (applied_revision_id = desired_revision_id
                AND applied_inventory_generation = desired_inventory_generation)
        )
    );
    CREATE INDEX routing_publications_desired_revision_idx
        ON routing_publications (desired_revision_id);
    CREATE INDEX routing_publications_state_idx
        ON routing_publications (state);

    CREATE TABLE learned_models_by_binding (
        binding_id          TEXT NOT NULL,
        model_id            TEXT NOT NULL,
        days_json           TEXT CHECK (days_json IS NULL OR json_valid(days_json)),
        max_prompt          INTEGER,
        context_window_json TEXT CHECK (
            context_window_json IS NULL OR json_valid(context_window_json)
        ),
        explicitly_promoted INTEGER NOT NULL DEFAULT 0 CHECK (
            explicitly_promoted IN (0, 1)
        ),
        updated_ms          INTEGER NOT NULL,
        PRIMARY KEY (binding_id, model_id)
    );
    CREATE INDEX learned_models_by_binding_model_idx
        ON learned_models_by_binding (model_id);

    ALTER TABLE lanes ADD COLUMN pin_generation INTEGER NOT NULL DEFAULT 0
        CHECK (pin_generation >= 0);
    ALTER TABLE lanes ADD COLUMN policy_revision_id INTEGER;
    ALTER TABLE lanes ADD COLUMN policy_route_id TEXT;
    ALTER TABLE lanes ADD COLUMN route_fingerprint TEXT CHECK (
        route_fingerprint IS NULL OR (
            length(route_fingerprint) = 64
            AND route_fingerprint NOT GLOB '*[^0-9a-f]*'
        )
    );
    ALTER TABLE lanes ADD COLUMN candidate_id TEXT;
    ALTER TABLE lanes ADD COLUMN binding_id TEXT;
    ALTER TABLE lanes ADD COLUMN baseline_model TEXT;
    ALTER TABLE lanes ADD COLUMN effective_model TEXT;
    CREATE INDEX lanes_policy_revision_idx ON lanes (policy_revision_id);
    CREATE INDEX lanes_binding_idx ON lanes (binding_id);
    CREATE TRIGGER lanes_pin_insert_check
    BEFORE INSERT ON lanes
    WHEN NOT (
        (NEW.policy_revision_id IS NULL
         AND NEW.policy_route_id IS NULL
         AND NEW.route_fingerprint IS NULL
         AND NEW.candidate_id IS NULL
         AND NEW.binding_id IS NULL
         AND NEW.baseline_model IS NULL
         AND NEW.effective_model IS NULL)
        OR
        (NEW.policy_revision_id IS NOT NULL
         AND NEW.policy_route_id IS NOT NULL
         AND NEW.route_fingerprint IS NOT NULL
         AND NEW.candidate_id IS NOT NULL
         AND NEW.binding_id IS NOT NULL
         AND NEW.baseline_model IS NOT NULL
         AND NEW.effective_model IS NOT NULL
         AND EXISTS (
             SELECT 1 FROM routing_policy_revisions
             WHERE revision_id = NEW.policy_revision_id
         ))
    ) BEGIN
        SELECT RAISE(ABORT, 'invalid lane pin');
    END;
    CREATE TRIGGER lanes_pin_update_check
    BEFORE UPDATE OF policy_revision_id, policy_route_id, route_fingerprint,
                     candidate_id, binding_id, baseline_model, effective_model
    ON lanes
    WHEN NOT (
        (NEW.policy_revision_id IS NULL
         AND NEW.policy_route_id IS NULL
         AND NEW.route_fingerprint IS NULL
         AND NEW.candidate_id IS NULL
         AND NEW.binding_id IS NULL
         AND NEW.baseline_model IS NULL
         AND NEW.effective_model IS NULL)
        OR
        (NEW.policy_revision_id IS NOT NULL
         AND NEW.policy_route_id IS NOT NULL
         AND NEW.route_fingerprint IS NOT NULL
         AND NEW.candidate_id IS NOT NULL
         AND NEW.binding_id IS NOT NULL
         AND NEW.baseline_model IS NOT NULL
         AND NEW.effective_model IS NOT NULL
         AND EXISTS (
             SELECT 1 FROM routing_policy_revisions
             WHERE revision_id = NEW.policy_revision_id
         ))
    ) BEGIN
        SELECT RAISE(ABORT, 'invalid lane pin');
    END;

    ALTER TABLE requests ADD COLUMN policy_revision_id INTEGER;
    ALTER TABLE requests ADD COLUMN presentation_id TEXT;
    ALTER TABLE requests ADD COLUMN rewrite_id TEXT;
    ALTER TABLE requests ADD COLUMN policy_route_id TEXT;
    ALTER TABLE requests ADD COLUMN route_fingerprint TEXT CHECK (
        route_fingerprint IS NULL OR (
            length(route_fingerprint) = 64
            AND route_fingerprint NOT GLOB '*[^0-9a-f]*'
        )
    );
    ALTER TABLE requests ADD COLUMN candidate_id TEXT;
    ALTER TABLE requests ADD COLUMN binding_id TEXT;
    ALTER TABLE requests ADD COLUMN baseline_model TEXT;
    ALTER TABLE requests ADD COLUMN attempt_group_id TEXT CHECK (
        attempt_group_id IS NULL OR (
            length(attempt_group_id) = 32
            AND attempt_group_id NOT GLOB '*[^0-9a-f]*'
        )
    );
    ALTER TABLE requests ADD COLUMN attempt_index INTEGER CHECK (
        attempt_index IS NULL OR attempt_index >= 0
    );
    ALTER TABLE requests ADD COLUMN fallback_reason TEXT;
    ALTER TABLE requests ADD COLUMN routing_trace TEXT CHECK (
        routing_trace IS NULL OR json_valid(routing_trace)
    );
    ALTER TABLE requests ADD COLUMN parent_session_id TEXT;
    ALTER TABLE requests ADD COLUMN child_role TEXT;
    ALTER TABLE requests ADD COLUMN relation_source TEXT;
    CREATE INDEX requests_policy_revision_idx
        ON requests (policy_revision_id);
    CREATE INDEX requests_attempt_group_idx
        ON requests (attempt_group_id, attempt_index);
    CREATE UNIQUE INDEX requests_attempt_identity_idx
        ON requests (attempt_group_id, attempt_index)
        WHERE attempt_index IS NOT NULL;
    CREATE INDEX requests_parent_session_idx
        ON requests (parent_session_id);

    CREATE TRIGGER requests_provenance_insert_check
    BEFORE INSERT ON requests
    WHEN (NEW.attempt_index IS NOT NULL AND NEW.attempt_group_id IS NULL)
      OR NOT (
          (NEW.parent_session_id IS NULL
           AND NEW.child_role IS NULL
           AND NEW.relation_source IS NULL)
          OR
          (NEW.parent_session_id IS NOT NULL
           AND NEW.child_role = 'guardian'
           AND NEW.relation_source = 'codex_guardian'
           AND NEW.session_id IS NOT NULL
           AND NEW.parent_session_id != NEW.session_id)
      )
    BEGIN
        SELECT RAISE(ABORT, 'invalid request provenance');
    END;
    CREATE TRIGGER requests_lineage_cycle_check
    BEFORE INSERT ON requests
    WHEN NEW.parent_session_id IS NOT NULL AND EXISTS (
        WITH RECURSIVE ancestors(session_id) AS (
            SELECT NEW.parent_session_id
            UNION
            SELECT r.parent_session_id
            FROM requests AS r
            JOIN ancestors AS a ON r.session_id = a.session_id
            WHERE r.parent_session_id IS NOT NULL
        )
        SELECT 1 FROM ancestors WHERE session_id = NEW.session_id
    )
    BEGIN
        SELECT RAISE(ABORT, 'cyclic request lineage');
    END;
    "#,
    // v8 — harden attempt identity and policy provenance constraints that
    // cannot be expressed by ALTER TABLE. Store writes are serialised on one
    // connection, so predecessor checks deterministically enforce contiguous
    // zero-origin attempt indices.
    r#"
    CREATE UNIQUE INDEX requests_preflight_identity_idx
        ON requests (attempt_group_id)
        WHERE attempt_group_id IS NOT NULL AND attempt_index IS NULL;

    CREATE TRIGGER requests_attempt_sequence_check
    BEFORE INSERT ON requests
    WHEN NEW.attempt_group_id IS NOT NULL AND (
        (NEW.attempt_index IS NULL AND EXISTS (
            SELECT 1 FROM requests
            WHERE attempt_group_id = NEW.attempt_group_id
              AND attempt_index IS NOT NULL
        ))
        OR
        (NEW.attempt_index IS NOT NULL AND EXISTS (
            SELECT 1 FROM requests
            WHERE attempt_group_id = NEW.attempt_group_id
              AND attempt_index IS NULL
        ))
        OR
        (NEW.attempt_index = 0 AND EXISTS (
            SELECT 1 FROM requests
            WHERE attempt_group_id = NEW.attempt_group_id
              AND attempt_index IS NOT NULL
        ))
        OR
        (NEW.attempt_index > 0 AND NOT EXISTS (
            SELECT 1 FROM requests
            WHERE attempt_group_id = NEW.attempt_group_id
              AND attempt_index = NEW.attempt_index - 1
        ))
    ) BEGIN
        SELECT RAISE(ABORT, 'invalid request attempt sequence');
    END;

    CREATE TRIGGER requests_policy_revision_check
    BEFORE INSERT ON requests
    WHEN NEW.policy_revision_id IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM routing_policy_revisions
        WHERE revision_id = NEW.policy_revision_id
    ) BEGIN
        SELECT RAISE(ABORT, 'invalid request policy revision');
    END;
    "#,
    // v9 — permit reactivation to create a distinct immutable revision with
    // the same canonical document fingerprint. Rebuild the parent and both
    // tables with declared foreign keys together; lane/request references are
    // enforced by triggers and need no table rebuild.
    r#"
    PRAGMA defer_foreign_keys = ON;

    DROP TRIGGER lanes_pin_insert_check;
    DROP TRIGGER lanes_pin_update_check;
    DROP TRIGGER requests_policy_revision_check;

    CREATE TABLE routing_policy_revisions_new (
        revision_id              INTEGER PRIMARY KEY AUTOINCREMENT,
        created_ms               INTEGER NOT NULL,
        activated_ms             INTEGER NOT NULL,
        description              TEXT,
        activation_scope         TEXT NOT NULL CHECK (
            activation_scope IN ('new_lanes', 'new_and_cold_lanes', 'immediate')
        ),
        forced_cache_transition  INTEGER NOT NULL CHECK (
            forced_cache_transition IN (0, 1)
        ),
        source                   TEXT NOT NULL CHECK (
            source IN ('legacy', 'draft', 'reactivation')
        ),
        parent_revision_id       INTEGER REFERENCES routing_policy_revisions_new(revision_id),
        document_json            TEXT NOT NULL CHECK (json_valid(document_json)),
        fingerprint              TEXT NOT NULL CHECK (
            length(fingerprint) = 64
            AND fingerprint NOT GLOB '*[^0-9a-f]*'
        )
    );
    INSERT INTO routing_policy_revisions_new
        (revision_id, created_ms, activated_ms, description, activation_scope,
         forced_cache_transition, source, parent_revision_id, document_json, fingerprint)
    SELECT revision_id, created_ms, activated_ms, description, activation_scope,
           forced_cache_transition, source, parent_revision_id, document_json, fingerprint
    FROM routing_policy_revisions;

    CREATE TABLE routing_policy_state_new (
        id                       INTEGER PRIMARY KEY CHECK (id = 1),
        active_revision_id       INTEGER REFERENCES routing_policy_revisions_new(revision_id),
        draft_generation         INTEGER NOT NULL DEFAULT 0 CHECK (draft_generation >= 0),
        draft_base_revision_id   INTEGER REFERENCES routing_policy_revisions_new(revision_id),
        draft_document_json      TEXT CHECK (
            draft_document_json IS NULL OR json_valid(draft_document_json)
        ),
        draft_fingerprint        TEXT CHECK (
            draft_fingerprint IS NULL OR (
                length(draft_fingerprint) = 64
                AND draft_fingerprint NOT GLOB '*[^0-9a-f]*'
            )
        ),
        draft_updated_ms         INTEGER,
        legacy_imported_ms       INTEGER,
        CHECK (
            (draft_base_revision_id IS NULL
             AND draft_document_json IS NULL
             AND draft_fingerprint IS NULL
             AND draft_updated_ms IS NULL)
            OR
            (draft_base_revision_id IS NOT NULL
             AND draft_document_json IS NOT NULL
             AND draft_fingerprint IS NOT NULL
             AND draft_updated_ms IS NOT NULL)
        )
    );
    INSERT INTO routing_policy_state_new
    SELECT * FROM routing_policy_state;

    CREATE TABLE routing_publications_new (
        scope_key                    TEXT NOT NULL,
        adapter                      TEXT NOT NULL,
        desired_revision_id          INTEGER NOT NULL
            REFERENCES routing_policy_revisions_new(revision_id),
        applied_revision_id          INTEGER
            REFERENCES routing_policy_revisions_new(revision_id),
        desired_inventory_generation INTEGER NOT NULL CHECK (
            desired_inventory_generation >= 0
        ),
        applied_inventory_generation INTEGER CHECK (
            applied_inventory_generation >= 0
        ),
        state                        TEXT NOT NULL CHECK (
            state IN ('pending', 'applied', 'failed', 'unsupported')
        ),
        detail_code                  TEXT,
        updated_ms                   INTEGER NOT NULL,
        PRIMARY KEY (scope_key, adapter),
        CHECK (
            (applied_revision_id IS NULL
             AND applied_inventory_generation IS NULL)
            OR
            (applied_revision_id IS NOT NULL
             AND applied_inventory_generation IS NOT NULL)
        ),
        CHECK (
            state != 'applied'
            OR (applied_revision_id = desired_revision_id
                AND applied_inventory_generation = desired_inventory_generation)
        )
    );
    INSERT INTO routing_publications_new
    SELECT * FROM routing_publications;

    DROP TABLE routing_policy_state;
    DROP TABLE routing_publications;
    DROP TABLE routing_policy_revisions;
    ALTER TABLE routing_policy_revisions_new RENAME TO routing_policy_revisions;
    ALTER TABLE routing_policy_state_new RENAME TO routing_policy_state;
    ALTER TABLE routing_publications_new RENAME TO routing_publications;

    CREATE INDEX routing_policy_revisions_activated_idx
        ON routing_policy_revisions (activated_ms);
    CREATE INDEX routing_policy_revisions_parent_idx
        ON routing_policy_revisions (parent_revision_id);
    CREATE INDEX routing_policy_revisions_fingerprint_idx
        ON routing_policy_revisions (fingerprint);
    CREATE INDEX routing_publications_desired_revision_idx
        ON routing_publications (desired_revision_id);
    CREATE INDEX routing_publications_state_idx
        ON routing_publications (state);

    CREATE TRIGGER lanes_pin_insert_check
    BEFORE INSERT ON lanes
    WHEN NOT (
        (NEW.policy_revision_id IS NULL
         AND NEW.policy_route_id IS NULL
         AND NEW.route_fingerprint IS NULL
         AND NEW.candidate_id IS NULL
         AND NEW.binding_id IS NULL
         AND NEW.baseline_model IS NULL
         AND NEW.effective_model IS NULL)
        OR
        (NEW.policy_revision_id IS NOT NULL
         AND NEW.policy_route_id IS NOT NULL
         AND NEW.route_fingerprint IS NOT NULL
         AND NEW.candidate_id IS NOT NULL
         AND NEW.binding_id IS NOT NULL
         AND NEW.baseline_model IS NOT NULL
         AND NEW.effective_model IS NOT NULL
         AND EXISTS (
             SELECT 1 FROM routing_policy_revisions
             WHERE revision_id = NEW.policy_revision_id
         ))
    ) BEGIN
        SELECT RAISE(ABORT, 'invalid lane pin');
    END;
    CREATE TRIGGER lanes_pin_update_check
    BEFORE UPDATE OF policy_revision_id, policy_route_id, route_fingerprint,
                     candidate_id, binding_id, baseline_model, effective_model
    ON lanes
    WHEN NOT (
        (NEW.policy_revision_id IS NULL
         AND NEW.policy_route_id IS NULL
         AND NEW.route_fingerprint IS NULL
         AND NEW.candidate_id IS NULL
         AND NEW.binding_id IS NULL
         AND NEW.baseline_model IS NULL
         AND NEW.effective_model IS NULL)
        OR
        (NEW.policy_revision_id IS NOT NULL
         AND NEW.policy_route_id IS NOT NULL
         AND NEW.route_fingerprint IS NOT NULL
         AND NEW.candidate_id IS NOT NULL
         AND NEW.binding_id IS NOT NULL
         AND NEW.baseline_model IS NOT NULL
         AND NEW.effective_model IS NOT NULL
         AND EXISTS (
             SELECT 1 FROM routing_policy_revisions
             WHERE revision_id = NEW.policy_revision_id
         ))
    ) BEGIN
        SELECT RAISE(ABORT, 'invalid lane pin');
    END;

    CREATE TRIGGER requests_policy_revision_check
    BEFORE INSERT ON requests
    WHEN NEW.policy_revision_id IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM routing_policy_revisions
        WHERE revision_id = NEW.policy_revision_id
    ) BEGIN
        SELECT RAISE(ABORT, 'invalid request policy revision');
    END;
    "#,
    // v10 — make a preflight-only terminal explicit and bound the versioned
    // content-free trace envelope. No normal v9 writer emits either shape;
    // malformed raw fixtures block the migration for repair rather than being
    // rewritten in this insert-only ledger.
    r#"
    CREATE TRIGGER requests_terminal_trace_check
    BEFORE INSERT ON requests
    WHEN (NEW.attempt_group_id IS NOT NULL AND NEW.attempt_index IS NULL AND (
              NEW.kind != 'route-unavailable' OR NEW.routing_trace IS NULL
          ))
       OR NEW.kind = 'route-unavailable' AND (
              NEW.attempt_group_id IS NULL OR NEW.attempt_index IS NOT NULL
              OR NEW.routing_trace IS NULL
          )
       OR NEW.routing_trace IS NOT NULL AND (
              length(CAST(NEW.routing_trace AS BLOB)) > 65536
              OR json_type(NEW.routing_trace) != 'object'
              OR json_extract(NEW.routing_trace, '$.version') != 1
              OR json_type(NEW.routing_trace, '$.events') != 'array'
              OR json_array_length(NEW.routing_trace, '$.events') > 128
          )
    BEGIN
        SELECT RAISE(ABORT, 'invalid request routing terminal or trace');
    END;
    "#,
    // v11 — routing trace v2 adds typed crossed-edge provenance while v1
    // remains valid historical data. The Rust reader validates the closed event
    // vocabulary and edge sequence; SQLite keeps enforcing the bounded envelope.
    r#"
    DROP TRIGGER requests_terminal_trace_check;
    CREATE TRIGGER requests_terminal_trace_check
    BEFORE INSERT ON requests
    WHEN (NEW.attempt_group_id IS NOT NULL AND NEW.attempt_index IS NULL AND (
              NEW.kind != 'route-unavailable' OR NEW.routing_trace IS NULL
          ))
       OR NEW.kind = 'route-unavailable' AND (
              NEW.attempt_group_id IS NULL OR NEW.attempt_index IS NOT NULL
              OR NEW.routing_trace IS NULL
          )
       OR NEW.routing_trace IS NOT NULL AND (
              length(CAST(NEW.routing_trace AS BLOB)) > 65536
              OR json_type(NEW.routing_trace) != 'object'
              OR json_type(NEW.routing_trace, '$.version') IS NOT 'integer'
              OR json_extract(NEW.routing_trace, '$.version') NOT IN (1, 2)
              OR json_type(NEW.routing_trace, '$.events') IS NOT 'array'
              OR json_array_length(NEW.routing_trace, '$.events') > 128
          )
    BEGIN
        SELECT RAISE(ABORT, 'invalid request routing terminal or trace');
    END;
    "#,
    // v12 — close attempt-reason and preflight-terminal shapes for raw SQL
    // writers. The physical fallback_reason name is retained for compatibility;
    // its closed Rust meaning is the reason this actual attempt exists.
    r#"
    CREATE TRIGGER requests_attempt_reason_check
    BEFORE INSERT ON requests
    WHEN (NEW.attempt_group_id IS NULL AND (
              NEW.attempt_index IS NOT NULL OR NEW.fallback_reason IS NOT NULL
          ))
       OR (NEW.attempt_index IS NOT NULL AND NEW.fallback_reason IS NULL)
       OR (NEW.attempt_index IS NULL AND NEW.fallback_reason IS NOT NULL)
       OR (NEW.attempt_index = 0 AND NEW.fallback_reason != 'initial')
       OR (NEW.attempt_index > 0 AND NEW.fallback_reason = 'initial')
       OR (NEW.fallback_reason IS NOT NULL AND NEW.fallback_reason NOT IN (
              'initial', 'thinking_negotiation', 'authentication',
              'model_not_found', 'rate_limit', 'provider_quota', 'overload',
              'server_error', 'transport_before_accept', 'ambiguous_transport'
          ))
    BEGIN
        SELECT RAISE(ABORT, 'invalid request attempt reason');
    END;

    CREATE TRIGGER requests_route_terminal_shape_check
    BEFORE INSERT ON requests
    WHEN NEW.kind = 'route-unavailable' AND (
           NEW.candidate_id IS NOT NULL
        OR NEW.binding_id IS NOT NULL
        OR NEW.baseline_model IS NOT NULL
        OR NEW.fallback_reason IS NOT NULL
    )
    BEGIN
        SELECT RAISE(ABORT, 'route-unavailable carries send-only provenance');
    END;
    "#,
    // v13 — publication adapters own only the exact row identities they last
    // applied. The set advances atomically with the applied revision/inventory
    // pair, while pending and failed desired handoffs retain it.
    r#"
    ALTER TABLE routing_publications ADD COLUMN applied_identities_json TEXT
        CHECK (
            applied_identities_json IS NULL OR (
                applied_revision_id IS NOT NULL
                AND json_valid(applied_identities_json)
                AND json_type(applied_identities_json) = 'array'
                AND json_array_length(applied_identities_json) <= 1024
            )
        );
    "#,
    // v14 — a v12 applied publication predates exact row ownership. It cannot
    // remain applied after v13: treating unknown ownership as empty would let
    // the next reconciliation remove a user's row. It is pending until the
    // user-side adapter applies and records an exact set.
    r#"
    UPDATE routing_publications
    SET state = 'pending',
        applied_revision_id = NULL,
        applied_inventory_generation = NULL,
        detail_code = 'legacy_applied_identities_unknown'
    WHERE state = 'applied' AND applied_identities_json IS NULL;
    "#,
    // v15 — revision history source records the operation that created it;
    // compiler origin records document semantics. Restoring generated revision
    // one is a reactivation that must still compile with legacy compatibility.
    r#"
    ALTER TABLE routing_policy_revisions ADD COLUMN compiler_origin TEXT
        NOT NULL DEFAULT 'authored' CHECK (
            compiler_origin IN ('authored', 'generated_revision_one')
        );
    WITH RECURSIVE generated_lineage(revision_id) AS (
        SELECT revision_id
        FROM routing_policy_revisions
        WHERE source = 'legacy'

        UNION ALL

        SELECT child.revision_id
        FROM routing_policy_revisions AS child
        JOIN generated_lineage AS parent
          ON child.parent_revision_id = parent.revision_id
        WHERE child.source = 'reactivation'
    )
    UPDATE routing_policy_revisions
    SET compiler_origin = 'generated_revision_one'
    WHERE revision_id IN (SELECT revision_id FROM generated_lineage);
    "#,
    // v16 — a session index on the lane table. The lane key is the composite
    // `session|tools`, so a session's lanes were only reachable by scanning
    // every lane; the opencode plugin asks for one session's lanes every
    // couple of seconds. Newest first matches that read's order.
    r#"
    CREATE INDEX lanes_session_idx ON lanes (session_id, updated_ms);
    "#,
    // v17 — drop the declared context window from binding learning. Its only
    // writer was the retired merge endpoint, so no production row carries one.
    // The context-fit check reads the inventory window, then the largest
    // prompt served.
    r#"
    ALTER TABLE learned_models_by_binding DROP COLUMN context_window_json;
    "#,
    // v18 — configuration leaves the file for the store (spec: DCONF#in-the-store).
    // A nullable column means "the built-in default", never today's value
    // frozen into a row, so a default that moves moves for a machine that
    // never set it. A fresh machine has no rows at all and is every default
    // with every provider disabled.
    r#"
    -- One row per provider toker knows, present whether or not it is on, so
    -- switching a provider off keeps the endpoint and credential source it
    -- was configured with (spec: DCONF#in-the-store).
    CREATE TABLE config_providers (
        provider       TEXT PRIMARY KEY,
        enabled        INTEGER NOT NULL CHECK (enabled IN (0, 1)),
        -- The endpoint. Configurable for every provider, subscriptions
        -- included: toker is a proxy, so another may sit in front of it.
        upstream       TEXT,
        -- Where the credential comes from. These are sources, not an
        -- exclusive choice: WCRED resolves env, then keyring, then literal.
        cred_env       TEXT,
        cred_keyring   INTEGER CHECK (cred_keyring IS NULL OR cred_keyring IN (0, 1)),
        cred_literal   TEXT,
        -- Codex only, NULL on every other provider.
        refresh_url    TEXT,
        client_version TEXT,
        version_probe  INTEGER CHECK (version_probe IS NULL OR version_probe IN (0, 1))
    );

    -- Everything that is not per-provider and not per-profile. One row, so a
    -- read is a single lookup and there is no key namespace to typo.
    CREATE TABLE config_settings (
        id               INTEGER PRIMARY KEY CHECK (id = 1),
        session_headers  TEXT,
        ping_header      TEXT,
        awake            INTEGER CHECK (awake IS NULL OR awake IN (0, 1)),
        transcript_roots TEXT,
        quota_enabled    INTEGER CHECK (quota_enabled IS NULL OR quota_enabled IN (0, 1)),
        cold_enabled     INTEGER CHECK (cold_enabled IS NULL OR cold_enabled IN (0, 1)),
        cold_outlook     INTEGER CHECK (cold_outlook IS NULL OR cold_outlook IN (0, 1)),
        cold_min_tokens  INTEGER CHECK (cold_min_tokens IS NULL OR cold_min_tokens >= 0),
        cold_idle_min    REAL CHECK (cold_idle_min IS NULL OR cold_idle_min >= 0),
        notice_default   TEXT
    );

    -- Per-frontend-profile notice styles; a profile with no row takes the
    -- default above. The name check is the router's own set (spec: FPROF),
    -- so a row that could never match a request cannot be written.
    CREATE TABLE config_notices (
        profile TEXT PRIMARY KEY
            CHECK (length(profile) BETWEEN 1 AND 64
                   AND profile NOT GLOB '*[^a-z0-9_-]*'),
        style   TEXT NOT NULL CHECK (style IN ('gfm', 'toker', 'block', 'plain'))
    );
    "#,
    // v19 — account identity is learned from credential digests, independently
    // of the configured provider. Values never enter the store; resolution can
    // later fill an account on every ledger row that carried its digest.
    r#"
    CREATE TABLE accounts (
        number        INTEGER PRIMARY KEY AUTOINCREMENT,
        provider      TEXT NOT NULL,
        upstream_id   TEXT,
        email         TEXT,
        named_by      TEXT NOT NULL CHECK (named_by IN ('local', 'upstream', 'operator')),
        first_seen_ms INTEGER NOT NULL,
        last_seen_ms  INTEGER NOT NULL,
        CHECK (upstream_id IS NOT NULL OR email IS NOT NULL)
    );
    CREATE UNIQUE INDEX accounts_provider_upstream_id_idx
        ON accounts (provider, upstream_id) WHERE upstream_id IS NOT NULL;
    CREATE UNIQUE INDEX accounts_provider_email_idx
        ON accounts (provider, email) WHERE email IS NOT NULL;

    CREATE TABLE credential_accounts (
        digest         TEXT PRIMARY KEY CHECK (
            length(digest) = 64 AND digest NOT GLOB '*[^0-9a-f]*'
        ),
        provider       TEXT NOT NULL,
        account_number INTEGER REFERENCES accounts(number),
        first_seen_ms  INTEGER NOT NULL,
        last_seen_ms   INTEGER NOT NULL
    );
    CREATE INDEX credential_accounts_provider_account_idx
        ON credential_accounts (provider, account_number);

    ALTER TABLE requests ADD COLUMN credential_digest TEXT CHECK (
        credential_digest IS NULL OR (
            length(credential_digest) = 64
            AND credential_digest NOT GLOB '*[^0-9a-f]*'
        )
    );
    ALTER TABLE requests ADD COLUMN account_number INTEGER REFERENCES accounts(number);
    CREATE INDEX requests_provider_account_idx
        ON requests (provider, account_number, ts_ms);
    "#,
    // v20 — quota state belongs to the account that spent it rather than to
    // the provider it was spent through (spec: ACCT#what-is-attributed-to-an-account).
    // A provider whose accounts Toker cannot tell apart holds none, and keeps
    // reading and writing `meters_by_provider`.
    //
    // Allowances rebuild rather than widen: the key gains the account, and a
    // row that names none could not be honoured under the new rule anyway.
    // Nothing is lost that the ledger needs — this table is live state, and
    // every row in it prunes when its own window ends.
    r#"
    CREATE TABLE meters_by_account (
        provider_id    TEXT NOT NULL,
        account_number INTEGER NOT NULL REFERENCES accounts(number),
        updated_ms     INTEGER NOT NULL,
        snapshot       TEXT NOT NULL,
        PRIMARY KEY (provider_id, account_number)
    );

    CREATE TABLE allowances_v20 (
        session_id     TEXT NOT NULL,
        account_number INTEGER NOT NULL REFERENCES accounts(number),
        meter          TEXT NOT NULL,
        reset_value    INTEGER NOT NULL,
        release        TEXT NOT NULL DEFAULT 'overage',
        PRIMARY KEY (session_id, account_number, meter, reset_value)
    );
    DROP TABLE allowances;
    ALTER TABLE allowances_v20 RENAME TO allowances;

    ALTER TABLE pings ADD COLUMN account_number INTEGER REFERENCES accounts(number);
    -- `requests_provider_account_idx` orders by account before time, so the
    -- newest attributed row for a provider could only be found by sorting
    -- every attributed row it has: ~96,700 of them on the machine this was
    -- written against. This index seeks the provider and walks time backwards
    -- instead, stopping at the first row that names an account.
    CREATE INDEX requests_provider_ts_idx ON requests (provider, ts_ms);

    -- The account that served, part of the lane's cache identity
    -- (spec: SLANE#cache-identity). Null where the provider has no accounts
    -- Toker can tell apart, and on every pin written before this column, so
    -- it stays outside the pin's all-present-or-all-absent rule.
    ALTER TABLE lanes ADD COLUMN account_number INTEGER REFERENCES accounts(number);

    "#,
    // v21 — server safeguards (spec: WSAFE). A binding that refused the ask
    // is remembered here, across restarts, until a catalogue refresh, a
    // credential change or the operator clears it. It is read-modify-write
    // state, never ledger: a row is cleared, not appended.
    //
    // The request columns record only presence and outcome, never the ask or
    // a result (spec: SLEDGR#fields-that-need-care). All are NULL on a row
    // that predates them, which says nothing either way.
    //
    // The attempt-reason trigger is replaced to admit the one extra send a
    // refused ask earns.
    r#"
    CREATE TABLE binding_safeguards_marks (
        binding_id TEXT PRIMARY KEY,
        marked_ms  INTEGER NOT NULL
    );

    ALTER TABLE requests ADD COLUMN safeguards_asked INTEGER
        CHECK (safeguards_asked IN (0, 1));
    ALTER TABLE requests ADD COLUMN safeguards_support TEXT
        CHECK (safeguards_support IN ('supported', 'unsupported', 'unknown'));
    ALTER TABLE requests ADD COLUMN safeguards_marked INTEGER
        CHECK (safeguards_marked IN (0, 1));
    ALTER TABLE requests ADD COLUMN safeguards_results_seen INTEGER
        CHECK (safeguards_results_seen IN (0, 1));
    ALTER TABLE requests ADD COLUMN safeguards_results_replayed INTEGER
        CHECK (safeguards_results_replayed IN (0, 1));
    ALTER TABLE requests ADD COLUMN safeguards_skipped_candidate INTEGER
        CHECK (safeguards_skipped_candidate IN (0, 1));
    ALTER TABLE requests ADD COLUMN safeguards_walked_again INTEGER
        CHECK (safeguards_walked_again IN (0, 1));

    DROP TRIGGER requests_attempt_reason_check;
    CREATE TRIGGER requests_attempt_reason_check
    BEFORE INSERT ON requests
    WHEN (NEW.attempt_group_id IS NULL AND (
              NEW.attempt_index IS NOT NULL OR NEW.fallback_reason IS NOT NULL
          ))
       OR (NEW.attempt_index IS NOT NULL AND NEW.fallback_reason IS NULL)
       OR (NEW.attempt_index IS NULL AND NEW.fallback_reason IS NOT NULL)
       OR (NEW.attempt_index = 0 AND NEW.fallback_reason != 'initial')
       OR (NEW.attempt_index > 0 AND NEW.fallback_reason = 'initial')
       OR (NEW.fallback_reason IS NOT NULL AND NEW.fallback_reason NOT IN (
              'initial', 'thinking_negotiation', 'safeguards_negotiation',
              'authentication', 'model_not_found', 'rate_limit', 'provider_quota',
              'overload', 'server_error', 'transport_before_accept',
              'ambiguous_transport'
          ))
    BEGIN
        SELECT RAISE(ABORT, 'invalid request attempt reason');
    END;
    "#,
    // v22 — a revision may be recorded as a `save` (spec: RREV#saving-an-edit).
    // The source CHECK cannot be widened in place, so this rebuilds the parent
    // and the two tables that reference it, as v9 did, with the triggers that
    // name it dropped first and recreated after. Existing revisions keep their
    // source: those written as `draft` stay `draft`. The draft columns of the
    // state row stay in the schema, since migrations are append-only, but
    // nothing reads or writes them any more.
    r#"
    PRAGMA defer_foreign_keys = ON;

    DROP TRIGGER lanes_pin_insert_check;
    DROP TRIGGER lanes_pin_update_check;
    DROP TRIGGER requests_policy_revision_check;

    CREATE TABLE routing_policy_revisions_new (
        revision_id              INTEGER PRIMARY KEY AUTOINCREMENT,
        created_ms               INTEGER NOT NULL,
        activated_ms             INTEGER NOT NULL,
        description              TEXT,
        activation_scope         TEXT NOT NULL CHECK (
            activation_scope IN ('new_lanes', 'new_and_cold_lanes', 'immediate')
        ),
        forced_cache_transition  INTEGER NOT NULL CHECK (
            forced_cache_transition IN (0, 1)
        ),
        source                   TEXT NOT NULL CHECK (
            source IN ('legacy', 'draft', 'reactivation', 'save')
        ),
        parent_revision_id       INTEGER REFERENCES routing_policy_revisions_new(revision_id),
        document_json            TEXT NOT NULL CHECK (json_valid(document_json)),
        fingerprint              TEXT NOT NULL CHECK (
            length(fingerprint) = 64
            AND fingerprint NOT GLOB '*[^0-9a-f]*'
        ),
        compiler_origin          TEXT NOT NULL DEFAULT 'authored' CHECK (
            compiler_origin IN ('authored', 'generated_revision_one')
        )
    );
    INSERT INTO routing_policy_revisions_new
        (revision_id, created_ms, activated_ms, description, activation_scope,
         forced_cache_transition, source, parent_revision_id, document_json,
         fingerprint, compiler_origin)
    SELECT revision_id, created_ms, activated_ms, description, activation_scope,
           forced_cache_transition, source, parent_revision_id, document_json,
           fingerprint, compiler_origin
    FROM routing_policy_revisions;

    CREATE TABLE routing_policy_state_new (
        id                       INTEGER PRIMARY KEY CHECK (id = 1),
        active_revision_id       INTEGER REFERENCES routing_policy_revisions_new(revision_id),
        draft_generation         INTEGER NOT NULL DEFAULT 0 CHECK (draft_generation >= 0),
        draft_base_revision_id   INTEGER REFERENCES routing_policy_revisions_new(revision_id),
        draft_document_json      TEXT CHECK (
            draft_document_json IS NULL OR json_valid(draft_document_json)
        ),
        draft_fingerprint        TEXT CHECK (
            draft_fingerprint IS NULL OR (
                length(draft_fingerprint) = 64
                AND draft_fingerprint NOT GLOB '*[^0-9a-f]*'
            )
        ),
        draft_updated_ms         INTEGER,
        legacy_imported_ms       INTEGER,
        CHECK (
            (draft_base_revision_id IS NULL
             AND draft_document_json IS NULL
             AND draft_fingerprint IS NULL
             AND draft_updated_ms IS NULL)
            OR
            (draft_base_revision_id IS NOT NULL
             AND draft_document_json IS NOT NULL
             AND draft_fingerprint IS NOT NULL
             AND draft_updated_ms IS NOT NULL)
        )
    );
    INSERT INTO routing_policy_state_new
        (id, active_revision_id, draft_generation, draft_base_revision_id,
         draft_document_json, draft_fingerprint, draft_updated_ms, legacy_imported_ms)
    SELECT id, active_revision_id, draft_generation, draft_base_revision_id,
           draft_document_json, draft_fingerprint, draft_updated_ms, legacy_imported_ms
    FROM routing_policy_state;

    CREATE TABLE routing_publications_new (
        scope_key                    TEXT NOT NULL,
        adapter                      TEXT NOT NULL,
        desired_revision_id          INTEGER NOT NULL
            REFERENCES routing_policy_revisions_new(revision_id),
        applied_revision_id          INTEGER
            REFERENCES routing_policy_revisions_new(revision_id),
        desired_inventory_generation INTEGER NOT NULL CHECK (
            desired_inventory_generation >= 0
        ),
        applied_inventory_generation INTEGER CHECK (
            applied_inventory_generation >= 0
        ),
        state                        TEXT NOT NULL CHECK (
            state IN ('pending', 'applied', 'failed', 'unsupported')
        ),
        detail_code                  TEXT,
        updated_ms                   INTEGER NOT NULL,
        applied_identities_json      TEXT CHECK (
            applied_identities_json IS NULL OR (
                applied_revision_id IS NOT NULL
                AND json_valid(applied_identities_json)
                AND json_type(applied_identities_json) = 'array'
                AND json_array_length(applied_identities_json) <= 1024
            )
        ),
        PRIMARY KEY (scope_key, adapter),
        CHECK (
            (applied_revision_id IS NULL
             AND applied_inventory_generation IS NULL)
            OR
            (applied_revision_id IS NOT NULL
             AND applied_inventory_generation IS NOT NULL)
        ),
        CHECK (
            state != 'applied'
            OR (applied_revision_id = desired_revision_id
                AND applied_inventory_generation = desired_inventory_generation)
        )
    );
    INSERT INTO routing_publications_new
        (scope_key, adapter, desired_revision_id, applied_revision_id,
         desired_inventory_generation, applied_inventory_generation, state,
         detail_code, updated_ms, applied_identities_json)
    SELECT scope_key, adapter, desired_revision_id, applied_revision_id,
           desired_inventory_generation, applied_inventory_generation, state,
           detail_code, updated_ms, applied_identities_json
    FROM routing_publications;

    DROP TABLE routing_policy_state;
    DROP TABLE routing_publications;
    DROP TABLE routing_policy_revisions;
    ALTER TABLE routing_policy_revisions_new RENAME TO routing_policy_revisions;
    ALTER TABLE routing_policy_state_new RENAME TO routing_policy_state;
    ALTER TABLE routing_publications_new RENAME TO routing_publications;

    CREATE INDEX routing_policy_revisions_activated_idx
        ON routing_policy_revisions (activated_ms);
    CREATE INDEX routing_policy_revisions_parent_idx
        ON routing_policy_revisions (parent_revision_id);
    CREATE INDEX routing_policy_revisions_fingerprint_idx
        ON routing_policy_revisions (fingerprint);
    CREATE INDEX routing_publications_desired_revision_idx
        ON routing_publications (desired_revision_id);
    CREATE INDEX routing_publications_state_idx
        ON routing_publications (state);

    CREATE TRIGGER lanes_pin_insert_check
    BEFORE INSERT ON lanes
    WHEN NOT (
        (NEW.policy_revision_id IS NULL
         AND NEW.policy_route_id IS NULL
         AND NEW.route_fingerprint IS NULL
         AND NEW.candidate_id IS NULL
         AND NEW.binding_id IS NULL
         AND NEW.baseline_model IS NULL
         AND NEW.effective_model IS NULL)
        OR
        (NEW.policy_revision_id IS NOT NULL
         AND NEW.policy_route_id IS NOT NULL
         AND NEW.route_fingerprint IS NOT NULL
         AND NEW.candidate_id IS NOT NULL
         AND NEW.binding_id IS NOT NULL
         AND NEW.baseline_model IS NOT NULL
         AND NEW.effective_model IS NOT NULL
         AND EXISTS (
             SELECT 1 FROM routing_policy_revisions
             WHERE revision_id = NEW.policy_revision_id
         ))
    ) BEGIN
        SELECT RAISE(ABORT, 'invalid lane pin');
    END;
    CREATE TRIGGER lanes_pin_update_check
    BEFORE UPDATE OF policy_revision_id, policy_route_id, route_fingerprint,
                     candidate_id, binding_id, baseline_model, effective_model
    ON lanes
    WHEN NOT (
        (NEW.policy_revision_id IS NULL
         AND NEW.policy_route_id IS NULL
         AND NEW.route_fingerprint IS NULL
         AND NEW.candidate_id IS NULL
         AND NEW.binding_id IS NULL
         AND NEW.baseline_model IS NULL
         AND NEW.effective_model IS NULL)
        OR
        (NEW.policy_revision_id IS NOT NULL
         AND NEW.policy_route_id IS NOT NULL
         AND NEW.route_fingerprint IS NOT NULL
         AND NEW.candidate_id IS NOT NULL
         AND NEW.binding_id IS NOT NULL
         AND NEW.baseline_model IS NOT NULL
         AND NEW.effective_model IS NOT NULL
         AND EXISTS (
             SELECT 1 FROM routing_policy_revisions
             WHERE revision_id = NEW.policy_revision_id
         ))
    ) BEGIN
        SELECT RAISE(ABORT, 'invalid lane pin');
    END;

    CREATE TRIGGER requests_policy_revision_check
    BEFORE INSERT ON requests
    WHEN NEW.policy_revision_id IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM routing_policy_revisions
        WHERE revision_id = NEW.policy_revision_id
    ) BEGIN
        SELECT RAISE(ABORT, 'invalid request policy revision');
    END;
    "#,
    // v23 — widening an activation's scope after the fact (spec:
    // RREV#widening-an-activation). A revision is never edited, so the
    // widened scope, the consent given with it and when lie on the state row,
    // and the next activation clears them. NULL means the revision's own
    // scope stands.
    r#"
    ALTER TABLE routing_policy_state ADD COLUMN widened_scope TEXT
        CHECK (widened_scope IN ('new_and_cold_lanes', 'immediate'));
    ALTER TABLE routing_policy_state ADD COLUMN widened_forced_cache_transition INTEGER
        CHECK (widened_forced_cache_transition IN (0, 1));
    ALTER TABLE routing_policy_state ADD COLUMN widened_ms INTEGER;
    "#,
    // v24 — the resolver's facts and the lookup that matched, on every
    // policy-routed row (spec: RPROV). `facts_recorded` is the marker: it is
    // 1 on a row that carries the facts and NULL on every row written before,
    // which therefore read as lacking them (not replayable; a hit count over
    // them is a floor) rather than as rows whose facts were all absent. A
    // fact that is itself absent, such as no requested model, is NULL beside
    // the marker. Nothing here holds a prompt, a body or a header value; the
    // capabilities are a JSON object of booleans.
    //
    // The protocol, requested model and the safeguards ask are repeated here
    // although other columns say something like them: those are written per
    // row kind with their own rules (a route-unavailable row records no ask,
    // `frontend` is a wire label, not the resolver's protocol), and a replay
    // must read exactly what the resolver was given.
    //
    // `requests_ts_ms_idx` (v1) already serves a 24-hour window.
    r#"
    ALTER TABLE requests ADD COLUMN facts_recorded INTEGER
        CHECK (facts_recorded IN (1));
    ALTER TABLE requests ADD COLUMN facts_profile TEXT;
    ALTER TABLE requests ADD COLUMN facts_protocol TEXT
        CHECK (facts_protocol IN ('anthropic_messages', 'openai_chat', 'openai_responses'));
    ALTER TABLE requests ADD COLUMN facts_requested_model TEXT;
    ALTER TABLE requests ADD COLUMN facts_prompt_bound INTEGER
        CHECK (facts_prompt_bound >= 0);
    ALTER TABLE requests ADD COLUMN facts_message_count INTEGER
        CHECK (facts_message_count >= 0);
    ALTER TABLE requests ADD COLUMN facts_tool_count INTEGER
        CHECK (facts_tool_count >= 0);
    ALTER TABLE requests ADD COLUMN facts_capabilities TEXT
        CHECK (facts_capabilities IS NULL OR json_valid(facts_capabilities));
    ALTER TABLE requests ADD COLUMN facts_asks_safeguards INTEGER
        CHECK (facts_asks_safeguards IN (0, 1));
    ALTER TABLE requests ADD COLUMN facts_binding_affinity TEXT;
    ALTER TABLE requests ADD COLUMN facts_class TEXT
        CHECK (facts_class IN ('inference', 'compaction'));
    ALTER TABLE requests ADD COLUMN lookup_kind TEXT
        CHECK (lookup_kind IN ('presentation', 'rewrite', 'default', 'compaction'));
    ALTER TABLE requests ADD COLUMN lookup_scope TEXT
        CHECK (lookup_scope IN ('profile', 'protocol'));

    CREATE TRIGGER requests_facts_shape_check
    BEFORE INSERT ON requests
    WHEN (NEW.facts_recorded IS NULL AND (
              NEW.facts_profile IS NOT NULL OR NEW.facts_protocol IS NOT NULL
              OR NEW.facts_requested_model IS NOT NULL
              OR NEW.facts_prompt_bound IS NOT NULL
              OR NEW.facts_message_count IS NOT NULL
              OR NEW.facts_tool_count IS NOT NULL
              OR NEW.facts_capabilities IS NOT NULL
              OR NEW.facts_asks_safeguards IS NOT NULL
              OR NEW.facts_binding_affinity IS NOT NULL
              OR NEW.facts_class IS NOT NULL
              OR NEW.lookup_kind IS NOT NULL OR NEW.lookup_scope IS NOT NULL
          ))
       OR (NEW.facts_recorded IS NOT NULL AND (
              NEW.policy_revision_id IS NULL
              OR NEW.facts_protocol IS NULL OR NEW.facts_prompt_bound IS NULL
              OR NEW.facts_tool_count IS NULL OR NEW.facts_capabilities IS NULL
              OR NEW.facts_asks_safeguards IS NULL OR NEW.facts_class IS NULL
          ))
       OR (NEW.lookup_kind IS NULL AND NEW.lookup_scope IS NOT NULL)
       OR (NEW.lookup_kind IN ('compaction') AND NEW.lookup_scope IS NOT NULL)
       OR (NEW.lookup_kind IN ('presentation', 'rewrite', 'default')
           AND NEW.lookup_scope IS NULL)
       OR (NEW.lookup_scope = 'profile' AND NEW.facts_profile IS NULL)
    BEGIN
        SELECT RAISE(ABORT, 'invalid request resolver facts');
    END;
    "#,
    // v25 — the lane the resolver was given, beside the rest of its facts
    // (spec: RSOLV#facts-in). The resolver reads the lane's temperature, its
    // pin and the upgrade the lane holds, and starts its walk at the pinned
    // candidate: a replay that resolved a pinned lane as a new one would
    // reach a candidate the request never could.
    //
    // `facts_lane` is the marker as well as the value: NULL on a row written
    // before the lane was recorded, which therefore reads as lacking it
    // rather than as having been given no lane, 'new' where the resolver was
    // given none, and the lane's cache temperature where it was. The pin
    // columns are the identities the resolver reads of a pin, and are absent
    // together on an unpinned lane. The pin's revision and baseline model are
    // not here: the resolver reads neither.
    //
    // The forced pair is repeated from `forced_from`/`forced_to`, which the
    // row writes with their own per-kind rules; a replay must read what the
    // resolver was given.
    r#"
    ALTER TABLE requests ADD COLUMN facts_lane TEXT
        CHECK (facts_lane IN ('new', 'warm', 'cold', 'unknown'));
    ALTER TABLE requests ADD COLUMN facts_lane_route_fingerprint TEXT;
    ALTER TABLE requests ADD COLUMN facts_lane_candidate_id TEXT;
    ALTER TABLE requests ADD COLUMN facts_lane_binding_id TEXT;
    ALTER TABLE requests ADD COLUMN facts_lane_effective_model TEXT;
    ALTER TABLE requests ADD COLUMN facts_lane_forced_from TEXT;
    ALTER TABLE requests ADD COLUMN facts_lane_forced_to TEXT;

    CREATE TRIGGER requests_facts_lane_shape_check
    BEFORE INSERT ON requests
    WHEN (NEW.facts_lane IS NULL AND (
              NEW.facts_lane_route_fingerprint IS NOT NULL
              OR NEW.facts_lane_candidate_id IS NOT NULL
              OR NEW.facts_lane_binding_id IS NOT NULL
              OR NEW.facts_lane_effective_model IS NOT NULL
              OR NEW.facts_lane_forced_from IS NOT NULL
              OR NEW.facts_lane_forced_to IS NOT NULL
          ))
       OR (NEW.facts_lane IS NOT NULL AND NEW.facts_recorded IS NULL)
       OR (NEW.facts_lane = 'new' AND (
              NEW.facts_lane_route_fingerprint IS NOT NULL
              OR NEW.facts_lane_candidate_id IS NOT NULL
              OR NEW.facts_lane_binding_id IS NOT NULL
              OR NEW.facts_lane_effective_model IS NOT NULL
              OR NEW.facts_lane_forced_from IS NOT NULL
              OR NEW.facts_lane_forced_to IS NOT NULL
          ))
       OR ((NEW.facts_lane_route_fingerprint IS NULL) != (NEW.facts_lane_candidate_id IS NULL))
       OR ((NEW.facts_lane_route_fingerprint IS NULL) != (NEW.facts_lane_binding_id IS NULL))
       OR ((NEW.facts_lane_route_fingerprint IS NULL) != (NEW.facts_lane_effective_model IS NULL))
    BEGIN
        SELECT RAISE(ABORT, 'invalid request resolver lane facts');
    END;
    "#,
];

fn validate_v10_history(conn: &Connection) -> Result<()> {
    let invalid: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM requests
             WHERE (attempt_group_id IS NOT NULL AND attempt_index IS NULL AND (
                        kind != 'route-unavailable' OR routing_trace IS NULL
                    ))
                OR kind = 'route-unavailable' AND (
                        attempt_group_id IS NULL OR attempt_index IS NOT NULL
                        OR routing_trace IS NULL
                    )
                OR routing_trace IS NOT NULL AND (
                        length(CAST(routing_trace AS BLOB)) > 65536
                        OR json_type(routing_trace) != 'object'
                        OR json_extract(routing_trace, '$.version') != 1
                        OR json_type(routing_trace, '$.events') != 'array'
                        OR json_array_length(routing_trace, '$.events') > 128
                    )
         )",
        [],
        |row| row.get(0),
    )?;
    if invalid {
        Err(Error::InvalidRoutingState(
            "invalid stored routing terminal or trace".into(),
        ))
    } else {
        Ok(())
    }
}

fn validate_v12_history(conn: &Connection) -> Result<()> {
    let invalid: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM requests
             WHERE (attempt_group_id IS NULL AND (
                        attempt_index IS NOT NULL OR fallback_reason IS NOT NULL
                    ))
                OR (attempt_index IS NOT NULL AND fallback_reason IS NULL)
                OR (attempt_index IS NULL AND fallback_reason IS NOT NULL)
                OR (attempt_index = 0 AND fallback_reason != 'initial')
                OR (attempt_index > 0 AND fallback_reason = 'initial')
                OR (fallback_reason IS NOT NULL AND fallback_reason NOT IN (
                       'initial', 'thinking_negotiation', 'authentication',
                       'model_not_found', 'rate_limit', 'provider_quota', 'overload',
                       'server_error', 'transport_before_accept', 'ambiguous_transport'
                   ))
                OR kind = 'route-unavailable' AND (
                       candidate_id IS NOT NULL OR binding_id IS NOT NULL
                       OR baseline_model IS NOT NULL OR fallback_reason IS NOT NULL
                   )
         )",
        [],
        |row| row.get(0),
    )?;
    if invalid {
        Err(Error::InvalidRoutingState(
            "invalid stored attempt reason or route terminal".into(),
        ))
    } else {
        Ok(())
    }
}

/// Apply pending migrations in order. A fresh database runs every entry.
pub(super) fn migrate(conn: &mut Connection) -> Result<()> {
    let mut current = conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
    for (index, script) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version > current {
            // v7 had no normal writer for attempt provenance. If raw fixtures
            // populated non-NULL attempt fields in a shape v8 cannot constrain,
            // v8 aborts atomically for explicit repair rather than rewriting
            // insert-only request history.
            //
            // The script and its user_version bump commit together, so a
            // crash mid-migration leaves the database at the prior version.
            let tx = conn.transaction()?;
            if version == 10 {
                validate_v10_history(&tx)?;
            } else if version == 12 {
                validate_v12_history(&tx)?;
            }
            tx.execute_batch(script)?;
            tx.pragma_update(None, "user_version", version)?;
            tx.commit()?;
            current = version;
        }
    }
    Ok(())
}
