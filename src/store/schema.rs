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
];

/// Apply pending migrations in order. A fresh database runs every entry.
pub(super) fn migrate(conn: &mut Connection) -> Result<()> {
    let mut current = conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
    for (index, script) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version > current {
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
