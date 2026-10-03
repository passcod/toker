//! SQLite storage: the requests ledger plus state tables.
//!
//! Plan: "Storage" — [`default_db_path`] resolves to
//! `$XDG_DATA_HOME/toker/toker.db` (fallback `~/.local/share/toker/toker.db`),
//! and [`Store::open`] accepts any path so config and tests can override it.
//! The database runs in WAL mode with a 5 s busy timeout, so the TUI, the
//! daemon, and `toker export` can all hold the file while the daemon writes.
//!
//! - `requests` is insert-only, one row per request, cost in three explicit
//!   kinds never conflated ([`CostKind`]); see the `ledger` submodule.
//! - State (lanes, learned models, allowances, pings, last meters, meta)
//!   lives in read-modify-write tables; see the `state` submodule.
//! - No content is ever stored (invariant 1): digests, counts, lengths only.
//! - Absence ≠ zero (invariant 3): every non-key column is nullable and the
//!   row model uses `Option` throughout — `None` round-trips as NULL, never
//!   as `0` or `""`.
//!
//! Concurrency: one connection behind a [`std::sync::Mutex`]. This keeps the
//! write path simple and ordered — the daemon is a single process, and
//! serialising its own operations costs nothing — while WAL + busy timeout
//! let other *processes* (TUI, export) read the same file concurrently. If a
//! hot in-process read path ever contends with writes, open extra
//! connections on the same path; the schema and pragmas already allow it.
//!
//! Migrations: the `schema` submodule holds a numbered, append-only list
//! gated by `PRAGMA user_version` — no external migration files.

mod ledger;
mod schema;
mod state;

pub use ledger::{
    CostKind, RequestRow, RowKind, SessionCostGroup, SessionSummary, is_api_measurement,
};
pub use state::{Allowance, Lane, MetersSnapshot, ModelEntry, PingRecord};

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::Value;

/// Store-layer errors: SQLite/JSON/IO wrapped with context, plus the
/// store-specific cases (a poisoned lock, a missing data home, a stored
/// enum value the code no longer understands).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("store mutex poisoned")]
    MutexPoisoned,
    #[error("no data home: set XDG_DATA_HOME or HOME")]
    NoDataHome,
    #[error("unknown value {value:?} in column {column}")]
    UnknownDbValue { column: &'static str, value: String },
}

/// Result type for all store operations.
pub type Result<T> = std::result::Result<T, Error>;

/// `$XDG_DATA_HOME/toker/toker.db`, falling back to
/// `~/.local/share/toker/toker.db` when XDG is unset (plan: Storage).
/// Callers may pass any path to [`Store::open`] instead.
pub fn default_db_path() -> Result<PathBuf> {
    let data_home = match env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .ok_or(Error::NoDataHome)?;
            PathBuf::from(home).join(".local/share")
        }
    };
    Ok(data_home.join("toker").join("toker.db"))
}

/// An open `toker.db`: the requests ledger plus state tables, one mutexed
/// connection (see the module docs for the concurrency choice).
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if needed) the database at `path`, set WAL mode and
    /// the busy timeout, run pending migrations, and stamp `meta.created_at`
    /// on first open. `":memory:"` opens a private in-memory database — the
    /// tests rely on that, and the pragmas no-op gracefully there.
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        // Returns the effective mode ("wal", or "memory" on :memory:).
        conn.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        })?;
        conn.busy_timeout(Duration::from_secs(5))?;
        schema::migrate(&mut conn)?;
        let store = Store {
            conn: Mutex::new(conn),
        };
        if store.get_meta("created_at")?.is_none() {
            // Zero only if the system clock is before the epoch; good enough
            // for a creation stamp.
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            store.set_meta("created_at", &now.to_string())?;
        }
        Ok(store)
    }

    /// The mutexed connection; every operation goes through this, so
    /// writes are ordered and no statement interleaves with another.
    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| Error::MutexPoisoned)
    }

    /// Append one request row. The only `requests` writer; there is no
    /// update or delete path anywhere in the store.
    pub fn record_request(&self, row: &RequestRow) -> Result<()> {
        ledger::insert(&*self.conn()?, row)
    }

    /// Append many rows in one transaction — the import batch path (plan:
    /// `toker import`). Returns the first and last assigned row ids so the
    /// importer can checkpoint the id range its rows occupy; see
    /// `ledger::insert_batch` for the batching rationale.
    pub fn record_requests(&self, rows: &[RequestRow]) -> Result<(Option<i64>, Option<i64>)> {
        let mut conn = self.conn()?;
        ledger::insert_batch(&mut conn, rows)
    }

    /// Rows with `ts_ms >= ts_ms`, oldest first; when the window holds more
    /// than `limit` rows the newest `limit` are kept (see `ledger`).
    pub fn requests_since(&self, ts_ms: i64, limit: u64) -> Result<Vec<RequestRow>> {
        let limit = limit.min(i64::MAX as u64) as i64;
        ledger::requests_since(&*self.conn()?, ts_ms, limit)
    }

    /// Total ledger row count.
    pub fn count_requests(&self) -> Result<i64> {
        ledger::count_requests(&*self.conn()?)
    }

    /// The `/_toker/session` aggregate for one session id: the count and
    /// span of its measurement rows, token sums, the billed total, and
    /// the billed-cost breakdowns by serving provider and model (see
    /// `ledger`). A session with no rows answers `requests: 0` with
    /// `None` everywhere else.
    pub fn session_summary(&self, session_id: &str) -> Result<SessionSummary> {
        ledger::session_summary(&*self.conn()?, session_id)
    }

    /// Upsert one lane (caller owns the read-modify-write cycle).
    pub fn upsert_lane(&self, lane: &Lane) -> Result<()> {
        state::upsert_lane(&*self.conn()?, lane)
    }

    /// One lane by key.
    pub fn load_lane(&self, key: &str) -> Result<Option<Lane>> {
        state::load_lane(&*self.conn()?, key)
    }

    /// All lanes, by key.
    pub fn load_lanes(&self) -> Result<Vec<Lane>> {
        state::load_lanes(&*self.conn()?)
    }

    /// Age and cap the lanes table (see `state::prune_lanes`): future-dated
    /// rows, rows older than `max_age_ms`, and everything beyond the
    /// newest `max_lanes` go. The caller owns the cadence — ctp prunes on
    /// its 30-second flush, never per request.
    pub fn prune_lanes(&self, now_ms: i64, max_lanes: usize, max_age_ms: i64) -> Result<u64> {
        state::prune_lanes(&*self.conn()?, now_ms, max_lanes, max_age_ms)
    }

    /// Upsert one learned-model entry.
    pub fn upsert_model(&self, entry: &ModelEntry) -> Result<()> {
        state::upsert_model(&*self.conn()?, entry)
    }

    /// One learned-model entry by exact identity.
    pub fn load_model(&self, model_id: &str) -> Result<Option<ModelEntry>> {
        state::load_model(&*self.conn()?, model_id)
    }

    /// All learned-model entries, by identity.
    pub fn load_models(&self) -> Result<Vec<ModelEntry>> {
        state::load_models(&*self.conn()?)
    }

    /// Record an allowance; idempotent per (session, meter, reset value).
    pub fn record_allowance(&self, allowance: &Allowance) -> Result<()> {
        state::record_allowance(&*self.conn()?, allowance)
    }

    /// All allowances, ordered by session, meter, reset value.
    pub fn load_allowances(&self) -> Result<Vec<Allowance>> {
        state::load_allowances(&*self.conn()?)
    }

    /// Append one ping run.
    pub fn record_ping(&self, ping: &PingRecord) -> Result<()> {
        state::record_ping(&*self.conn()?, ping)
    }

    /// Pings with `ts_ms >= ts_ms`, oldest first, newest `limit` kept.
    pub fn pings_since(&self, ts_ms: i64, limit: u64) -> Result<Vec<PingRecord>> {
        let limit = limit.min(i64::MAX as u64) as i64;
        state::pings_since(&*self.conn()?, ts_ms, limit)
    }

    /// Overwrite the last meter snapshot (single row).
    pub fn save_meters(&self, snapshot: &MetersSnapshot) -> Result<()> {
        state::save_meters(&*self.conn()?, snapshot)
    }

    /// The last meter snapshot, if the backend has produced one.
    pub fn load_meters(&self) -> Result<Option<MetersSnapshot>> {
        state::load_meters(&*self.conn()?)
    }

    /// Set a meta key/value.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        state::set_meta(&*self.conn()?, key, value)
    }

    /// Get a meta value by key.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        state::get_meta(&*self.conn()?, key)
    }
}

/// Serialise an optional JSON-typed column. `None` stays NULL — never
/// `"null"` or `{}` (absence ≠ zero).
fn opt_json_to_text(value: &Option<Value>) -> Result<Option<String>> {
    match value {
        None => Ok(None),
        Some(value) => Ok(Some(serde_json::to_string(value)?)),
    }
}

/// Parse an optional JSON-typed column back.
fn opt_json_from_text(text: Option<String>) -> Result<Option<Value>> {
    match text {
        None => Ok(None),
        Some(text) => Ok(Some(serde_json::from_str(&text)?)),
    }
}

/// Run `read` over at most the first row `sql` yields. Shared by the
/// submodules so row readers return [`Result`] (our error type), letting
/// JSON and enum parsing inside them report uniformly.
fn row_of<T, P>(
    conn: &Connection,
    sql: &str,
    params: P,
    read: impl Fn(&rusqlite::Row<'_>) -> Result<T>,
) -> Result<Option<T>>
where
    P: rusqlite::Params,
{
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params)?;
    match rows.next()? {
        Some(row) => Ok(Some(read(row)?)),
        None => Ok(None),
    }
}

/// Run `read` over every row `sql` yields, in query order.
fn rows_of<T, P>(
    conn: &Connection,
    sql: &str,
    params: P,
    read: impl Fn(&rusqlite::Row<'_>) -> Result<T>,
) -> Result<Vec<T>>
where
    P: rusqlite::Params,
{
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(read(row)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::schema;
    use super::{
        Allowance, CostKind, Error, Lane, MetersSnapshot, ModelEntry, PingRecord, RequestRow,
        RowKind, SessionCostGroup, Store, is_api_measurement,
    };
    use rusqlite::Connection;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A fresh private in-memory store per test.
    fn mem_store() -> Store {
        Store::open(":memory:").expect("open in-memory store")
    }

    /// A fresh scratch directory under /tmp/opencode (pre-created and
    /// approved for external access), unique per call so parallel tests
    /// never collide.
    fn test_dir(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            PathBuf::from("/tmp/opencode").join(format!("{}-{}-{}", std::process::id(), name, n));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    fn user_version(store: &Store) -> i64 {
        let conn = store.conn.lock().expect("lock");
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .expect("read user_version")
    }

    /// A row with every field set, covering every column's Some path.
    fn full_row() -> RequestRow {
        RequestRow {
            id: None,
            ts_ms: 1_769_000_000_012,
            duration_ms: Some(3_456),
            kind: None,
            frontend: Some("openai_chat".to_string()),
            provider: Some("openrouter".to_string()),
            route: Some("openai_chat:openrouter".to_string()),
            session_id: Some("ses-abc".to_string()),
            ping: Some(false),
            model: Some("z-ai/glm-5.3".to_string()),
            raw_model: Some("z-ai/glm-5.3".to_string()),
            requested_model: Some("openrouter/z-ai/glm-5.3".to_string()),
            effective_model: Some("z-ai/glm-5.3".to_string()),
            input: Some(12_345),
            cache_read: Some(100_000),
            cache_write_total: Some(5_000),
            cache_write_5m: Some(3_000),
            cache_write_1h: Some(2_000),
            output: Some(678),
            reasoning: Some(90),
            iterations: Some(1),
            web_searches: Some(0),
            code_execs: Some(2),
            ttl_split_known: Some(true),
            usage_presence: Some(json!({
                "input": true, "output": true, "cache_read": true, "cost": true,
            })),
            usage_raw: Some(
                r#"{"prompt_tokens":12345,"cost":0.00213,"cost_details":{"upstream":"0.0019"}}"#
                    .to_string(),
            ),
            cost_usd: Some(0.00213),
            cost_kind: Some(CostKind::Billed),
            rate_limits: None, // kind-gated: a real measurement never carries a stale copy
            req_bytes: Some(51_234),
            req_messages: Some(42),
            req_tools: Some(17),
            tools_hash: Some("sha256:tool5".to_string()),
            system_chars: Some(9_876),
            system_hash: Some("sha256:sys2".to_string()),
            system_blocks: Some(json!([
                {"hash": "sha256:b1", "chars": 4000},
                {"hash": "sha256:b2", "chars": 5876},
            ])),
            system_messages: Some(2),
            compact_generations: Some(1),
            summarising: Some(false),
            system_change: Some(json!({"added": 1, "removed": 0})),
            system_ladder: Some("2/17/42/1".to_string()),
            system_tail: Some("sha256:tail9".to_string()),
            gate_on: Some(true),
            cold_on: Some(false),
            forced_from: Some("z-ai/glm-5.2".to_string()),
            forced_to: Some("z-ai/glm-5.3".to_string()),
            downgraded_from: None,
            downgraded_to: None,
            cache_stripped: Some(true),
            system_merged: Some(false),
            model_mappings: Some(json!([{"from": "gpt-5.6", "to": "z-ai/glm-5.3"}])),
            drift_digest: None,
            status: None,
            error_type: None,
            retry_after_ms: None,
            extra: None,
            betas: Some("context-1m-2025-08-07".to_string()),
            geo: Some("NZ".to_string()),
            fast: Some(true),
        }
    }

    /// A row with only `ts_ms` — every other column NULL.
    fn bare_row(ts_ms: i64) -> RequestRow {
        RequestRow {
            id: None,
            ts_ms,
            duration_ms: None,
            kind: None,
            frontend: None,
            provider: None,
            route: None,
            session_id: None,
            ping: None,
            model: None,
            raw_model: None,
            requested_model: None,
            effective_model: None,
            input: None,
            cache_read: None,
            cache_write_total: None,
            cache_write_5m: None,
            cache_write_1h: None,
            output: None,
            reasoning: None,
            iterations: None,
            web_searches: None,
            code_execs: None,
            ttl_split_known: None,
            usage_presence: None,
            usage_raw: None,
            cost_usd: None,
            cost_kind: None,
            rate_limits: None,
            req_bytes: None,
            req_messages: None,
            req_tools: None,
            tools_hash: None,
            system_chars: None,
            system_hash: None,
            system_blocks: None,
            system_messages: None,
            compact_generations: None,
            summarising: None,
            system_change: None,
            system_ladder: None,
            system_tail: None,
            gate_on: None,
            cold_on: None,
            forced_from: None,
            forced_to: None,
            downgraded_from: None,
            downgraded_to: None,
            cache_stripped: None,
            system_merged: None,
            model_mappings: None,
            drift_digest: None,
            status: None,
            error_type: None,
            retry_after_ms: None,
            extra: None,
            betas: None,
            geo: None,
            fast: None,
        }
    }

    #[test]
    fn open_migrates_from_scratch_and_stamps_meta() {
        let store = mem_store();
        assert_eq!(user_version(&store), schema::MIGRATIONS.len() as i64);
        assert!(store.get_meta("created_at").expect("meta").is_some());
    }

    #[test]
    fn file_db_persists_and_reads_across_connections() {
        let dir = test_dir("file-db");
        let db = dir.join("nested").join("toker.db"); // exercises parent-dir creation
        let writer = Store::open(&db).expect("open writer");
        writer.record_request(&full_row()).expect("record");

        // WAL: a second connection reads committed rows while the first
        // stays open.
        let reader = Store::open(&db).expect("open reader");
        assert_eq!(reader.count_requests().expect("count"), 1);
        drop(reader);
        drop(writer);

        // Reopen: the row persists and migrations do not re-run.
        let reopened = Store::open(&db).expect("reopen");
        assert_eq!(reopened.count_requests().expect("count"), 1);
        assert_eq!(
            reopened.requests_since(0, 10).expect("rows").len(),
            1,
            "row persisted across reopen"
        );
        assert_eq!(user_version(&reopened), schema::MIGRATIONS.len() as i64);
    }

    #[test]
    fn full_row_round_trips() {
        let store = mem_store();
        let row = full_row();
        store.record_request(&row).expect("record");

        let mut rows = store.requests_since(0, 10).expect("read");
        assert_eq!(rows.len(), 1);
        let mut got = rows.remove(0);
        assert_eq!(got.id, Some(1), "id assigned on insert");
        got.id = None;
        assert_eq!(got, row, "every column round-trips byte-for-value");
    }

    #[test]
    fn nulls_round_trip_as_none_never_zero() {
        let store = mem_store();
        let row = bare_row(42);
        store.record_request(&row).expect("record");

        let mut rows = store.requests_since(0, 10).expect("read");
        assert_eq!(rows.len(), 1);
        let mut got = rows.remove(0);
        assert_eq!(got.ts_ms, 42);
        got.id = None;
        assert_eq!(got, row, "absence stays absence on every column");

        // Spot-check the invariant 3 property where a silent zero would be
        // most damaging: a measurement the provider did not report.
        assert_eq!(got.input, None);
        assert_eq!(got.cost_usd, None);
        assert_eq!(got.cost_kind, None);
        assert_eq!(got.session_id, None);
        assert_eq!(got.ping, None);
        assert_eq!(got.ttl_split_known, None);
        assert_eq!(got.usage_raw, None);
    }

    #[test]
    fn proxy_kinds_are_not_api_measurements() {
        assert!(is_api_measurement(None), "NULL kind = real API measurement");
        let kinds = [
            RowKind::Blocked,
            RowKind::Released,
            RowKind::Cold,
            RowKind::ColdQuiet,
            RowKind::Awake,
            RowKind::Error,
            RowKind::FidelityDrift,
        ];
        for kind in kinds {
            assert!(!is_api_measurement(Some(kind)));
            assert_eq!(RowKind::parse(kind.as_str()), Some(kind));
        }

        let store = mem_store();
        for (i, kind) in kinds.iter().enumerate() {
            let mut row = bare_row(1_000 + i as i64);
            row.kind = Some(*kind);
            store.record_request(&row).expect("record");
        }
        let rows = store.requests_since(0, kinds.len() as u64).expect("read");
        assert_eq!(rows.len(), kinds.len());
        for (row, kind) in rows.iter().zip(kinds) {
            assert_eq!(row.kind, Some(kind), "kinds round-trip");
        }
        assert!(
            rows.iter().all(|row| !is_api_measurement(row.kind)),
            "no proxy-written row classifies as an API measurement"
        );
    }

    #[test]
    fn requests_since_windows_orders_and_limits() {
        let store = mem_store();
        // Insert out of order: ids do not follow ts order.
        for ts in [100, 300, 200, 500, 400] {
            store.record_request(&bare_row(ts)).expect("record");
        }

        let ts = |rows: Vec<RequestRow>| rows.into_iter().map(|r| r.ts_ms).collect::<Vec<_>>();
        assert_eq!(
            ts(store.requests_since(0, 100).expect("all")),
            vec![100, 200, 300, 400, 500],
            "oldest first"
        );
        assert_eq!(
            ts(store.requests_since(200, 100).expect("window")),
            vec![200, 300, 400, 500],
            "window is inclusive of the boundary"
        );
        assert_eq!(
            ts(store.requests_since(0, 3).expect("capped")),
            vec![300, 400, 500],
            "cap keeps the newest rows, still oldest-first"
        );
        assert!(
            store.requests_since(600, 10).expect("empty").is_empty(),
            "window past the newest row is empty"
        );
        assert_eq!(store.count_requests().expect("count"), 5);
    }

    #[test]
    fn batched_rows_share_one_transaction_and_report_their_id_range() {
        let store = mem_store();
        // An empty batch touches nothing and claims no ids.
        assert_eq!(
            store.record_requests(&[]).expect("empty batch"),
            (None, None)
        );

        let rows: Vec<RequestRow> = (0..5).map(bare_row).collect();
        let (first, last) = store.record_requests(&rows).expect("batch insert");
        assert_eq!((first, last), (Some(1), Some(5)), "ids span the batch");
        assert_eq!(store.count_requests().expect("count"), 5);

        // A later batch continues the range; each batch is its own
        // transaction.
        let more: Vec<RequestRow> = (5..7).map(bare_row).collect();
        let (first, last) = store.record_requests(&more).expect("second batch");
        assert_eq!((first, last), (Some(6), Some(7)));
        assert_eq!(store.count_requests().expect("count"), 7);
    }

    /// A billed openrouter measurement row for the aggregate tests.
    fn billed_row(
        ts_ms: i64,
        session: &str,
        serving_provider: Option<&str>,
        model: Option<&str>,
        cost: f64,
    ) -> RequestRow {
        let mut row = bare_row(ts_ms);
        row.session_id = Some(session.to_owned());
        row.provider = Some("openrouter".to_owned());
        row.model = model.map(str::to_owned);
        row.cost_usd = Some(cost);
        row.cost_kind = Some(CostKind::Billed);
        row.extra = serving_provider.map(|provider| json!({"serving_provider": provider}));
        row
    }

    #[test]
    fn session_summary_aggregates_measurements_tokens_and_billed_cost() {
        let store = mem_store();

        // Three billed openrouter rows: two via one serving provider,
        // one with no serving_provider named (falls back to the backend
        // id), across two models.
        let mut relace = billed_row(1_000, "ses-agg", Some("Relace"), Some("z-ai/glm-5.3"), 0.01);
        relace.input = Some(100);
        relace.output = Some(40);
        store.record_request(&relace).expect("record");
        let mut cached = billed_row(2_000, "ses-agg", Some("Relace"), Some("z-ai/glm-5.3"), 0.02);
        cached.input = Some(50);
        cached.cache_read = Some(10);
        store.record_request(&cached).expect("record");
        let mut fallback = billed_row(3_000, "ses-agg", None, Some("openai/gpt-5.2"), 0.005);
        fallback.output = Some(5);
        store.record_request(&fallback).expect("record");

        // A subscription row: measured (tokens, request count), never
        // billed — plan-equivalent cost is not spend.
        let mut sub = bare_row(4_000);
        sub.session_id = Some("ses-agg".to_owned());
        sub.provider = Some("anthropic_sub".to_owned());
        sub.model = Some("claude-opus-5".to_owned());
        sub.input = Some(7);
        sub.cache_write_total = Some(900);
        sub.cost_usd = Some(1.5);
        sub.cost_kind = Some(CostKind::PlanEquivalent);
        store.record_request(&sub).expect("record");

        // A proxy-written row is not an API measurement: excluded from
        // every count and sum.
        let mut error = bare_row(5_000);
        error.session_id = Some("ses-agg".to_owned());
        error.kind = Some(RowKind::Error);
        error.input = Some(999);
        store.record_request(&error).expect("record");

        // Another session's rows are not this session's.
        store
            .record_request(&billed_row(
                1_500,
                "ses-other",
                Some("Elsewhere"),
                Some("x/y"),
                0.5,
            ))
            .expect("record");

        let summary = store.session_summary("ses-agg").expect("summary");
        assert_eq!(summary.requests, 4, "three openrouter + one sub row");
        assert_eq!(summary.first_ts_ms, Some(1_000));
        assert_eq!(summary.last_ts_ms, Some(4_000));
        assert_eq!(summary.input, Some(157));
        assert_eq!(summary.output, Some(45));
        assert_eq!(summary.cache_read, Some(10));
        assert_eq!(summary.cache_write_total, Some(900));
        assert_eq!(summary.reasoning, None, "no row carried the metric");
        assert_eq!(
            summary.billed_total,
            Some(0.035),
            "billed only — the 1.5 plan-equivalent is not spend"
        );
        assert_eq!(
            summary.per_provider,
            vec![
                SessionCostGroup {
                    label: "Relace".to_owned(),
                    requests: 2,
                    cost_usd: Some(0.03)
                },
                SessionCostGroup {
                    label: "openrouter".to_owned(),
                    requests: 1,
                    cost_usd: Some(0.005)
                },
            ],
            "cost-desc, serving_provider first with the backend id as fallback; the sub never bills"
        );
        assert_eq!(
            summary.per_model,
            vec![
                SessionCostGroup {
                    label: "z-ai/glm-5.3".to_owned(),
                    requests: 2,
                    cost_usd: Some(0.03)
                },
                SessionCostGroup {
                    label: "openai/gpt-5.2".to_owned(),
                    requests: 1,
                    cost_usd: Some(0.005)
                },
            ],
            "same breakdown by model"
        );

        // The other session aggregates independently.
        let other = store.session_summary("ses-other").expect("summary");
        assert_eq!(other.requests, 1);
        assert_eq!(other.billed_total, Some(0.5));
        assert_eq!(
            other.per_provider,
            vec![SessionCostGroup {
                label: "Elsewhere".to_owned(),
                requests: 1,
                cost_usd: Some(0.5)
            }]
        );
    }

    #[test]
    fn session_summary_absence_is_null_and_a_real_zero_survives() {
        let store = mem_store();

        // A row that reported zeros: 0 input, a 0.0 billed cost — real
        // zeros, not absence, and they must read back as zeros.
        let mut zero_input =
            billed_row(1_000, "ses-zero", Some("Relace"), Some("z-ai/glm-5.3"), 0.0);
        zero_input.input = Some(0);
        store.record_request(&zero_input).expect("record");
        let mut zero_output = bare_row(2_000);
        zero_output.session_id = Some("ses-zero".to_owned());
        zero_output.output = Some(0);
        store.record_request(&zero_output).expect("record");

        let summary = store.session_summary("ses-zero").expect("summary");
        assert_eq!(summary.requests, 2);
        assert_eq!(summary.input, Some(0), "a reported zero is a zero");
        assert_eq!(summary.output, Some(0));
        assert_eq!(summary.cache_read, None, "no row carried the metric");
        assert_eq!(summary.reasoning, None);
        assert_eq!(summary.cache_write_total, None);
        assert_eq!(summary.billed_total, Some(0.0), "a real zero sum stays 0");
        assert_eq!(
            summary.per_provider,
            vec![SessionCostGroup {
                label: "Relace".to_owned(),
                requests: 1,
                cost_usd: Some(0.0)
            }]
        );
        assert_eq!(
            summary.per_model,
            vec![SessionCostGroup {
                label: "z-ai/glm-5.3".to_owned(),
                requests: 1,
                cost_usd: Some(0.0)
            }]
        );

        // A session whose only cost is plan-equivalent: the billed half
        // is absent, never zero.
        let mut sub = bare_row(3_000);
        sub.session_id = Some("ses-plan".to_owned());
        sub.provider = Some("anthropic_sub".to_owned());
        sub.model = Some("claude-opus-5".to_owned());
        sub.input = Some(3);
        sub.cost_usd = Some(1.5);
        sub.cost_kind = Some(CostKind::PlanEquivalent);
        store.record_request(&sub).expect("record");

        let summary = store.session_summary("ses-plan").expect("summary");
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.input, Some(3));
        assert_eq!(
            summary.billed_total, None,
            "no billed rows — absence, not a zero"
        );
        assert_eq!(summary.per_provider, Vec::<SessionCostGroup>::new());
        assert_eq!(summary.per_model, Vec::<SessionCostGroup>::new());
    }

    #[test]
    fn session_summary_without_rows_is_zero_requests_with_nulls() {
        let store = mem_store();
        // Rows exist, but not for the session asked about — an absent
        // session is indistinguishable from one that measured nothing,
        // and the answer is the same zero-and-nulls shape.
        store
            .record_request(&billed_row(
                1_000,
                "ses-real",
                Some("Relace"),
                Some("z-ai/glm-5.3"),
                0.01,
            ))
            .expect("record");

        let summary = store.session_summary("ses-missing").expect("summary");
        assert_eq!(summary.requests, 0);
        assert_eq!(summary.first_ts_ms, None);
        assert_eq!(summary.last_ts_ms, None);
        assert_eq!(summary.input, None);
        assert_eq!(summary.output, None);
        assert_eq!(summary.reasoning, None);
        assert_eq!(summary.cache_read, None);
        assert_eq!(summary.cache_write_total, None);
        assert_eq!(summary.billed_total, None);
        assert_eq!(summary.per_provider, Vec::<SessionCostGroup>::new());
        assert_eq!(summary.per_model, Vec::<SessionCostGroup>::new());

        // The empty session id is a real id, answered the same way.
        let empty = store.session_summary("").expect("summary");
        assert_eq!(empty.requests, 0);
        assert_eq!(empty.billed_total, None);
    }

    #[test]
    fn session_summary_labels_fall_back_to_backend_then_unknown() {
        let store = mem_store();
        // A billed row whose serving provider is absent falls back to
        // the backend id; one with neither falls to `unknown` — and
        // equal-cost groups tie-break by label, so the order is
        // deterministic.
        store
            .record_request(&billed_row(
                1_000,
                "ses-labels",
                Some("Relace"),
                Some("z-ai/glm-5.3"),
                0.02,
            ))
            .expect("record");
        let mut backend = billed_row(2_000, "ses-labels", None, Some("openai/gpt-5.2"), 0.01);
        backend.provider = Some("anthropic_api".to_owned());
        store.record_request(&backend).expect("record");
        let mut unknown = billed_row(3_000, "ses-labels", None, None, 0.01);
        unknown.provider = None;
        unknown.extra = None;
        store.record_request(&unknown).expect("record");

        let summary = store.session_summary("ses-labels").expect("summary");
        assert_eq!(
            summary.per_provider,
            vec![
                SessionCostGroup {
                    label: "Relace".to_owned(),
                    requests: 1,
                    cost_usd: Some(0.02)
                },
                SessionCostGroup {
                    label: "anthropic_api".to_owned(),
                    requests: 1,
                    cost_usd: Some(0.01)
                },
                SessionCostGroup {
                    label: "unknown".to_owned(),
                    requests: 1,
                    cost_usd: Some(0.01)
                },
            ],
            "cost-desc, then label asc for the equal-cost pair"
        );
        assert_eq!(
            summary.per_model,
            vec![
                SessionCostGroup {
                    label: "z-ai/glm-5.3".to_owned(),
                    requests: 1,
                    cost_usd: Some(0.02)
                },
                SessionCostGroup {
                    label: "openai/gpt-5.2".to_owned(),
                    requests: 1,
                    cost_usd: Some(0.01)
                },
                SessionCostGroup {
                    label: "unknown".to_owned(),
                    requests: 1,
                    cost_usd: Some(0.01)
                },
            ],
            "cost-desc first, then label asc for the equal-cost pair; the model-less row groups under `unknown`"
        );
    }

    #[test]
    fn state_upserts_round_trip() {
        let store = mem_store();

        // Lanes: insert, read back, read-modify-write, read back again.
        let lane = Lane {
            key: "ses-1|sha256:tool5".to_string(),
            session_id: Some("ses-1".to_string()),
            tools_hash: Some("sha256:tool5".to_string()),
            updated_ms: 1_000,
            prompt_tokens: Some(12_000),
            ttl: Some(300_000),
            ping: Some(false),
            noticed_at: None,
            forced_from: None,
            forced_to: None,
        };
        store.upsert_lane(&lane).expect("upsert lane");
        assert_eq!(
            store.load_lane(&lane.key).expect("load"),
            Some(lane.clone())
        );
        let mut bumped = lane.clone();
        bumped.updated_ms = 2_000;
        bumped.prompt_tokens = Some(15_000);
        // The v2 columns round-trip too: a notice already given, and a
        // sticky upgrade the lane must keep honouring.
        bumped.noticed_at = Some(1_900);
        bumped.forced_from = Some("claude-opus-5".to_string());
        bumped.forced_to = Some("claude-opus-5-5".to_string());
        store.upsert_lane(&bumped).expect("upsert lane again");
        assert_eq!(
            store.load_lane(&lane.key).expect("load"),
            Some(bumped.clone())
        );
        assert_eq!(store.load_lanes().expect("lanes"), vec![bumped]);
        assert_eq!(store.load_lane("missing").expect("missing"), None);

        // Models: same, including the JSON columns.
        let model = ModelEntry {
            model_id: "anthropic/claude-opus-5".to_string(),
            days_json: Some(json!(["2026-09-28", "2026-09-29"])),
            max_prompt: Some(180_000),
            context_window_json: Some(json!({"context": 200_000, "verified": "2026-10-01"})),
        };
        store.upsert_model(&model).expect("upsert model");
        assert_eq!(
            store.load_model(&model.model_id).expect("load"),
            Some(model.clone())
        );
        let mut learned_more = model.clone();
        learned_more.max_prompt = Some(190_000);
        store
            .upsert_model(&learned_more)
            .expect("upsert model again");
        assert_eq!(
            store.load_model(&model.model_id).expect("load"),
            Some(learned_more.clone())
        );
        assert_eq!(store.load_models().expect("models"), vec![learned_more]);
        assert_eq!(store.load_model("unknown/model").expect("missing"), None);

        // Allowances: idempotent per (session, meter, reset).
        let five_h = Allowance {
            session_id: "ses-1".to_string(),
            meter: "5h".to_string(),
            reset_value: 1_769_100_000_000,
        };
        store.record_allowance(&five_h).expect("record");
        store.record_allowance(&five_h).expect("record again");
        let seven_d = Allowance {
            session_id: "ses-1".to_string(),
            meter: "7d".to_string(),
            reset_value: 1_769_700_000_000,
        };
        store.record_allowance(&seven_d).expect("record");
        assert_eq!(
            store.load_allowances().expect("allowances"),
            vec![five_h, seven_d],
            "same allowance twice is one row"
        );

        // Pings: insert-only, windowed like the ledger.
        let ping = PingRecord {
            id: None,
            ts_ms: 3_000,
            exit_code: Some(0),
            duration_ms: Some(1_234),
            boundary_ms: Some(2_400),
        };
        store.record_ping(&ping).expect("record ping");
        let mut got = store.pings_since(0, 10).expect("pings");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, Some(1), "id assigned on insert");
        let mut expected = ping.clone();
        expected.id = None;
        got[0].id = None;
        assert_eq!(got[0], expected);
        let late = PingRecord {
            id: None,
            ts_ms: 5_000,
            exit_code: Some(1),
            duration_ms: None,
            boundary_ms: None,
        };
        store.record_ping(&late).expect("record ping");
        let ts: Vec<i64> = store
            .pings_since(4_000, 10)
            .expect("pings")
            .into_iter()
            .map(|p| p.ts_ms)
            .collect();
        assert_eq!(ts, vec![5_000]);
        let ts: Vec<i64> = store
            .pings_since(0, 1)
            .expect("pings")
            .into_iter()
            .map(|p| p.ts_ms)
            .collect();
        assert_eq!(ts, vec![5_000], "cap keeps the newest ping");

        // Meters: single row, overwritten on every save.
        let meters = MetersSnapshot {
            updated_ms: 4_000,
            snapshot: json!({"5h": {"used": 42, "limit": 100}}),
        };
        store.save_meters(&meters).expect("save");
        assert_eq!(store.load_meters().expect("load"), Some(meters.clone()));
        let fresher = MetersSnapshot {
            updated_ms: 4_500,
            snapshot: json!({"5h": {"used": 50, "limit": 100}}),
        };
        store.save_meters(&fresher).expect("save again");
        assert_eq!(store.load_meters().expect("load"), Some(fresher));

        // Meta: last write wins.
        store.set_meta("note", "hello").expect("set");
        assert_eq!(
            store.get_meta("note").expect("get"),
            Some("hello".to_string())
        );
        store.set_meta("note", "again").expect("set");
        assert_eq!(
            store.get_meta("note").expect("get"),
            Some("again".to_string())
        );
        assert_eq!(store.get_meta("missing").expect("get"), None);
    }

    #[test]
    fn unknown_stored_enum_value_is_an_error() {
        let store = mem_store();
        {
            let conn = store.conn.lock().expect("lock");
            conn.execute(
                "INSERT INTO requests (ts_ms, kind) VALUES (1, 'mystery')",
                [],
            )
            .expect("insert bogus kind");
        }
        match store.requests_since(0, 10) {
            Err(Error::UnknownDbValue { column: "kind", .. }) => {}
            other => panic!("unknown kind must error, got {other:?}"),
        }

        let store = mem_store();
        {
            let conn = store.conn.lock().expect("lock");
            conn.execute(
                "INSERT INTO requests (ts_ms, cost_kind) VALUES (2, 'discounted')",
                [],
            )
            .expect("insert bogus cost_kind");
        }
        match store.requests_since(0, 10) {
            Err(Error::UnknownDbValue {
                column: "cost_kind",
                ..
            }) => {}
            other => panic!("unknown cost_kind must error, got {other:?}"),
        }
    }

    #[test]
    fn lanes_prune_drops_aged_future_and_beyond_cap_rows() {
        let store = mem_store();
        let now = 1_000_000_000_000i64;
        let age = 30 * 24 * 3600 * 1000i64;
        let lane = |key: &str, updated_ms: i64| Lane {
            key: key.to_owned(),
            session_id: Some("ses".to_owned()),
            tools_hash: Some(key.to_owned()),
            updated_ms,
            prompt_tokens: Some(1),
            ttl: None,
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
        };

        // Fresh, within the age window: kept.
        store
            .upsert_lane(&lane("fresh", now - 1_000))
            .expect("upsert");
        // Exactly 30 days old: kept (ctp drops strictly older).
        store.upsert_lane(&lane("edge", now - age)).expect("upsert");
        // A day past the window: gone.
        store
            .upsert_lane(&lane("stale", now - age - 24 * 3600 * 1000))
            .expect("upsert");
        // A future timestamp is a clock that moved, not an idle-proof lane.
        store
            .upsert_lane(&lane("future", now + 60_000))
            .expect("upsert");

        assert_eq!(
            store.prune_lanes(now, 4000, age).expect("prune"),
            2,
            "the stale and future rows go"
        );
        let keys: Vec<String> = store
            .load_lanes()
            .expect("lanes")
            .into_iter()
            .map(|lane| lane.key)
            .collect();
        assert_eq!(keys, vec!["edge".to_owned(), "fresh".to_owned()]);

        // The cap keeps the NEWEST rows (ctp pruneLanes sorts by `at`
        // descending and slices), not the first-inserted.
        let store = mem_store();
        for i in 0..6 {
            store
                .upsert_lane(&lane(&format!("lane-{i}"), now - 6_000 + i as i64 * 1_000))
                .expect("upsert");
        }
        assert_eq!(store.prune_lanes(now, 4, age).expect("prune"), 2);
        let keys: Vec<String> = store
            .load_lanes()
            .expect("lanes")
            .into_iter()
            .map(|lane| lane.key)
            .collect();
        assert_eq!(keys, vec!["lane-2", "lane-3", "lane-4", "lane-5"]);
    }

    #[test]
    fn migration_v2_upgrades_a_v1_database_in_place() {
        let dir = test_dir("v1-upgrade");
        let db = dir.join("toker.db");
        // A v1-shaped database: only the first migration applied, with a
        // v1-era lane row (no noticed_at / forced columns).
        std::fs::create_dir_all(&dir).expect("create the db parent dir");
        {
            let conn = Connection::open(&db).expect("open raw v1 db");
            conn.execute_batch(schema::MIGRATIONS[0])
                .expect("apply the v1 migration alone");
            conn.pragma_update(None, "user_version", 1)
                .expect("stamp v1");
            conn.execute(
                "INSERT INTO lanes (key, session_id, tools_hash, updated_ms, prompt_tokens, ttl, ping)
                 VALUES ('ses-old|sha256:t1', 'ses-old', 'sha256:t1', 12345, 999, 300000, 0)",
                [],
            )
            .expect("insert a v1 lane row");
        }

        // Opening with current code migrates cleanly: user_version reaches
        // the head, the old row survives with the new columns reading as
        // NULL (absence, never zero), and a fresh upsert writes them.
        let store = Store::open(&db).expect("v1 db migrates");
        assert_eq!(user_version(&store), schema::MIGRATIONS.len() as i64);
        let lane = store
            .load_lane("ses-old|sha256:t1")
            .expect("v1 lane loads")
            .expect("v1 lane survived the migration");
        assert_eq!(lane.updated_ms, 12_345);
        assert_eq!(lane.prompt_tokens, Some(999));
        assert_eq!(lane.ttl, Some(300_000));
        assert_eq!(lane.noticed_at, None, "new columns start absent");
        assert_eq!(lane.forced_from, None);
        assert_eq!(lane.forced_to, None);

        let mut upgraded = lane.clone();
        upgraded.noticed_at = Some(20_000);
        upgraded.forced_from = Some("claude-opus-5".to_owned());
        upgraded.forced_to = Some("claude-opus-5-5".to_owned());
        store.upsert_lane(&upgraded).expect("upsert upgraded lane");
        assert_eq!(
            store.load_lane("ses-old|sha256:t1").expect("reload"),
            Some(upgraded)
        );

        // And the upgraded database reopens at the head without re-running.
        drop(store);
        let reopened = Store::open(&db).expect("reopen");
        assert_eq!(user_version(&reopened), schema::MIGRATIONS.len() as i64);
    }
}
