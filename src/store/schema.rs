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

use super::Result;
use rusqlite::Connection;

/// The append-only migration list. `PRAGMA user_version` counts entries
/// applied; keep in sync with the row model in [super::ledger] and the state
/// structs in [super::state].
pub(super) const MIGRATIONS: &[&str] = &[
    // v1 — initial schema: the requests ledger, state tables, and meta.
    r#"
    -- requests: the insert-only ledger, one row per request (plan: Storage).
    -- kind IS NULL marks a real API measurement; every other kind is
    -- proxy-written (invariant 3: proxy rows are never API measurements).
    -- No content columns — digests, counts, lengths only (invariant 1).
    -- Absence is stored as NULL, never as a default (invariant 3); ts_ms is
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

    -- State tables (plan: "State as tables"): small, read-modify-write
    -- data, upserted by the caller. Lanes are keyed by the composite
    -- "sessionId|toolsHash" (plan: Lane tracking; the 30-day prune is the
    -- caller's job, not the schema's).
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
    -- (plan: allowances keyed by reset value, self-expiring).
    CREATE TABLE allowances (
        session_id  TEXT NOT NULL,
        meter       TEXT NOT NULL,
        reset_value INTEGER NOT NULL,
        PRIMARY KEY (session_id, meter, reset_value)
    );

    -- Ping runs: one insert per `claude -p` probe (plan: ping windows).
    CREATE TABLE pings (
        id          INTEGER PRIMARY KEY,
        ts_ms       INTEGER NOT NULL,
        exit_code   INTEGER,
        duration_ms INTEGER,
        boundary_ms INTEGER
    );

    -- Last meter snapshot, single row: every response updates it when the
    -- backend has meters (plan: Server core). The JSON shape is
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
    // v7 — non-authoritative live-routing storage. The singleton active
    // pointer starts NULL and this migration never changes it. Policy runtime
    // does not read these tables until the later atomic-cutover changeset.
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
];

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
            tx.execute_batch(script)?;
            tx.pragma_update(None, "user_version", version)?;
            tx.commit()?;
            current = version;
        }
    }
    Ok(())
}
