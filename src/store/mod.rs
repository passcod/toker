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
//! - No content is ever stored (spec: SEC#no-prompt-or-completion-content-is-recorded): digests, counts, lengths only.
//! - Absence ≠ zero (spec: STORE#not-recorded-is-not-zero): every non-key column is nullable and the
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

mod accounts;
mod config;
mod ledger;
mod schema;
mod state;

pub use accounts::{
    Account, AccountIdentity, AccountNameSource, CredentialAccount, CredentialDigest, MixedHistory,
};
pub use config::{Configuration, ProviderConfigRow, SettingsConfigRow};
pub use ledger::{
    AttemptGroupId, AttemptIndex, AttemptProvenance, AttemptReason, ChildRole, CostKind,
    DisplayRow, JSON_TEXT_COLUMNS, KindFilter, LaneSystemRow, LocalisationRow,
    MAX_ROUTING_TRACE_EVENTS, MeterRow, PolicyRouteProvenance, PolicyRoutingOutcome, RebuildRow,
    RelationSource, RequestFilter, RequestLookupProvenance, RequestRow, ResolvedRoute, RowKind,
    SafeguardsRow, SessionCostGroup, SessionLineage, SessionSummary, StoredCandidateState,
    StoredLadders, StoredLookupSource, StoredRoutingTrace, StoredRoutingTraceEvent,
    StoredSkipReason, StoredTransitionDenied, is_api_measurement,
};
pub use state::{
    ActivatePolicyRevision, ActivatedPolicyRevision, Allowance, BindingLearning,
    BindingLearningUpdate, DraftCasResult, InitialPublication, Lane, LanePin, LanePinCasResult,
    MetersSnapshot, ModelEntry, NewPolicyRevision, PingAction, PingRecord, PolicyCompilerOrigin,
    PolicyDraft, PolicyRevision, PolicyRevisionSource, PublicationAdapter, PublicationCasResult,
    PublicationKey, PublicationResultUpdate, PublicationState, RevisionOneActivation,
    RoutingPolicyState, RoutingPublication, SafeguardsMark, local_day, publication_targets,
};

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
    #[error("no data home: set XDG_DATA_HOME or HOME")]
    NoDataHome,
    #[error("unknown value {value:?} in column {column}")]
    UnknownDbValue { column: &'static str, value: String },
    #[error("invalid stored routing state: {0}")]
    InvalidRoutingState(String),
    #[error("invalid policy document: {0}")]
    InvalidPolicyDocument(String),
    #[error("invalid account state: {0}")]
    InvalidAccountState(String),
}

/// Result type for all store operations.
pub type Result<T> = std::result::Result<T, Error>;

/// `$XDG_DATA_HOME/toker/toker.db`, falling back to
/// `~/.local/share/toker/toker.db` when XDG is unset.
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
        // The state directory, not just the database file: a credential
        // literal written here passes through the -wal and -shm sidecars
        // beside it, so closing the file alone would leave it readable
        // (spec: DCONF#state). Enforced on every open rather than at setup,
        // because a directory created before toker kept a credential in its
        // store was made under the umask.

        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
            close_state_dir(parent)?;
        }
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", true)?;
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

    /// Open an existing ledger for reading only: no file or directory is
    /// created, no migration runs, and nothing (not even
    /// `meta.created_at`) is written. The readers that are not the
    /// service (`toker export`, `toker watch-context-window`) use this,
    /// so pointing one at the wrong path fails instead of leaving an
    /// empty ledger behind, and an older binary never migrates the live
    /// file under the service.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Store> {
        use rusqlite::OpenFlags;
        let path = path.as_ref();
        if !path.exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no ledger at {}", path.display()),
            )));
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let version = conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
        if version < 1 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} is not a toker ledger (schema version 0)",
                    path.display()
                ),
            )));
        }
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// The mutexed connection; every operation goes through this, so
    /// writes are ordered and no statement interleaves with another.
    ///
    /// A poisoned lock is recovered, not reported: recording swallows
    /// store errors, so treating poison as one would leave the ledger dark
    /// until a restart after any panic that happened while the lock was
    /// held. The connection itself is consistent either way: every
    /// statement is complete or not run, and a transaction dropped by the
    /// unwind rolls back.
    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        Ok(self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    /// Append one request row. The only `requests` writer; there is no
    /// update or delete path anywhere in the store.
    pub fn record_request(&self, row: &RequestRow) -> Result<()> {
        ledger::insert(&*self.conn()?, row)
    }

    /// Observe a credential without persisting its value. A resolved identity
    /// also backfills every older row carrying its digest in this transaction.
    pub fn observe_credential(
        &self,
        provider: &str,
        digest: &CredentialDigest,
        at_ms: i64,
        identity: Option<&AccountIdentity>,
    ) -> Result<Option<i64>> {
        let mut conn = self.conn()?;
        accounts::observe(&mut conn, provider, digest, at_ms, identity)
    }

    /// The account `identity` names, opened if none matches. Unlike
    /// [`Store::observe_credential`] it needs no credential: it is for naming
    /// the account a history that predates accounts was served on.
    pub fn open_account(
        &self,
        provider: &str,
        identity: &AccountIdentity,
        at_ms: i64,
    ) -> Result<i64> {
        let mut conn = self.conn()?;
        accounts::open_account(&mut conn, provider, identity, at_ms)
    }

    /// Every learned account, including accounts that have not served in a
    /// current dashboard window.
    pub fn load_accounts(&self) -> Result<Vec<Account>> {
        accounts::load_accounts(&*self.conn()?)
    }

    /// Every observed credential digest, including entries no source has named.
    pub fn load_credential_accounts(&self) -> Result<Vec<CredentialAccount>> {
        accounts::load_credentials(&*self.conn()?)
    }

    /// How many of a provider's rows carry `digest` — the requests held
    /// against a credential no source has named yet.
    pub fn rows_for_digest(&self, provider: &str, digest: &str) -> Result<i64> {
        accounts::rows_for_digest(&*self.conn()?, provider, digest)
    }

    /// A credential digest that reaches an account, for naming it by hand.
    pub fn credential_of(&self, account: i64) -> Result<Option<CredentialDigest>> {
        accounts::credential_of(&*self.conn()?, account)
    }

    /// How many of a provider's rows predate accounts, carrying neither an
    /// account nor a credential digest. Setup moves these onto an account,
    /// and every other command refuses while any remain.
    pub fn unattributed_row_count(&self, provider: &str) -> Result<i64> {
        accounts::unattributed_row_count(&*self.conn()?, provider)
    }

    /// Where a provider's pre-account rows carry two accounts' windows, the
    /// row that proves it. `None` when they read as one account's history.
    pub fn unattributed_window_conflict(&self, provider: &str) -> Result<Option<MixedHistory>> {
        accounts::unattributed_window_conflict(&*self.conn()?, provider)
    }

    /// Write `account` onto every one of a provider's pre-account rows, in
    /// one transaction. Returns how many moved.
    pub fn attribute_unattributed_rows(&self, provider: &str, account: i64) -> Result<usize> {
        let mut conn = self.conn()?;
        accounts::attribute_unattributed_rows(&mut conn, provider, account)
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

    /// The export walk: every row `filter` admits, oldest first, as a
    /// JSON object keyed by column name, streamed from the cursor (see
    /// `ledger::for_each_export` for the value rules). An error `emit`
    /// returns ends the walk and comes back as [`Error::Io`], so a
    /// closed pipe can be told apart from a store failure.
    pub fn for_each_export(
        &self,
        filter: &RequestFilter,
        emit: impl FnMut(&serde_json::Map<String, Value>) -> std::io::Result<()>,
    ) -> Result<()> {
        ledger::for_each_export(&*self.conn()?, filter, emit)
    }

    /// Every row `filter` admits, oldest first, as [`RequestRow`]s, one
    /// at a time from the cursor.
    pub fn for_each_request(
        &self,
        filter: &RequestFilter,
        visit: impl FnMut(RequestRow) -> Result<()>,
    ) -> Result<()> {
        ledger::for_each_row(&*self.conn()?, filter, visit)
    }

    /// The quota panel's meter lookback: rows with `ts_ms >= ts_ms` that
    /// carry a meter snapshot or a gate flag, as narrow [`MeterRow`]s,
    /// oldest first; when more than `limit` match, the newest `limit` are
    /// kept. See `ledger::meter_rows_since` for why the filter reads both
    /// columns — the gate-seen rule needs `gate_on` off meter-less rows.
    pub fn meter_rows_since(&self, ts_ms: i64, limit: u64) -> Result<Vec<MeterRow>> {
        let limit = limit.min(i64::MAX as u64) as i64;
        ledger::meter_rows_since(&*self.conn()?, ts_ms, limit)
    }

    /// The furthest 5-hour reset (epoch seconds) reported by
    /// API-measured rows from `provider` logged at or after `ts_ms`,
    /// optionally only ping-tagged ones (see `ledger::furthest_reset5h`
    /// for why the furthest and why only measurements).
    pub fn furthest_reset5h(
        &self,
        provider: &str,
        account: Option<i64>,
        ts_ms: i64,
        ping_only: bool,
    ) -> Result<Option<i64>> {
        ledger::furthest_reset5h(&*self.conn()?, provider, account, ts_ms, ping_only)
    }

    /// The account the ping that landed at or after `ts_ms` opened a window
    /// for, read off the rows it produced.
    pub fn ping_account(&self, provider: &str, ts_ms: i64) -> Result<Option<i64>> {
        ledger::ping_account(&*self.conn()?, provider, ts_ms)
    }

    /// Whether a ping-tagged API-measured row from `provider` was logged
    /// at or after `ts_ms`.
    pub fn ping_landed(&self, provider: &str, ts_ms: i64) -> Result<bool> {
        ledger::ping_landed(&*self.conn()?, provider, ts_ms)
    }

    /// The display tick's window read (sessions/spend/rate/context/
    /// tokens): every row with `ts_ms >= ts_ms` as narrow [`DisplayRow`]s,
    /// oldest first; when the window holds more than `limit` rows the
    /// newest `limit` are kept. See `ledger::display_rows_since` for why
    /// there is no kind filter: the display aggregation consumes every
    /// row kind in the window, including the kinds whose only
    /// contribution is existing (`window_empty`).
    pub fn display_rows_since(&self, ts_ms: i64, limit: u64) -> Result<Vec<DisplayRow>> {
        let limit = limit.min(i64::MAX as u64) as i64;
        ledger::display_rows_since(&*self.conn()?, ts_ms, limit)
    }

    /// The rebuild walk's tail read (the TUI's quota cadence):
    /// measurement rows (`kind IS NULL`) with `ts_ms >= ts_ms` as
    /// narrow [`RebuildRow`]s, oldest first, cap keeping the newest.
    /// See `ledger::rebuild_rows_since` for the kind filter and the
    /// tail-length rule — the tail must reach back before the display
    /// window so lanes get their real predecessors (anti-phantom).
    pub fn rebuild_rows_since(&self, ts_ms: i64, limit: u64) -> Result<Vec<RebuildRow>> {
        let limit = limit.min(i64::MAX as u64) as i64;
        ledger::rebuild_rows_since(&*self.conn()?, ts_ms, limit)
    }

    /// The rebuild panel's targeted second query: the heavy
    /// localisation columns (ladders, tails, the capture-time
    /// `system_change`) for a specific set of row ids — fetched only
    /// for the rows a system-prompt change was attributed to.
    pub fn localisation_rows(&self, ids: &[i64]) -> Result<Vec<LocalisationRow>> {
        ledger::localisation_rows(&*self.conn()?, ids)
    }

    /// The lane's newest measurement row carrying a system prompt: the
    /// baseline capture compares a new request's system prompt with. See
    /// `ledger::lane_system_predecessor` for why the ledger, not the lane
    /// table.
    pub fn lane_system_predecessor(
        &self,
        session_id: &str,
        tools_hash: &str,
    ) -> Result<Option<LaneSystemRow>> {
        ledger::lane_system_predecessor(&*self.conn()?, session_id, tools_hash)
    }

    /// The stored rungs of the system prompt `system_hash` in a lane, from
    /// the newest row at or before `(ts_ms, id)` that kept them. See
    /// `ledger::lane_system_ladders`.
    pub fn lane_system_ladders(
        &self,
        session_id: &str,
        tools_hash: &str,
        system_hash: &str,
        at_or_before: (i64, i64),
    ) -> Result<Option<StoredLadders>> {
        ledger::lane_system_ladders(
            &*self.conn()?,
            session_id,
            tools_hash,
            system_hash,
            at_or_before,
        )
    }

    /// Run raw SQL against the store: tests that need a ledger read to
    /// fail use it to break the schema under the code being tested.
    #[cfg(test)]
    pub(crate) fn execute_for_test(&self, sql: &str) -> Result<()> {
        Ok(self.conn()?.execute_batch(sql)?)
    }

    /// Total ledger row count.
    pub fn count_requests(&self) -> Result<i64> {
        ledger::count_requests(&*self.conn()?)
    }

    /// Total logical request count. Explicit attempt groups count once;
    /// ungrouped historical and proxy rows each count independently.
    pub fn count_logical_requests(&self) -> Result<i64> {
        ledger::count_logical_requests(&*self.conn()?)
    }

    /// The newest ledger row's timestamp, of any kind; `None` when the
    /// ledger is empty. The TUI header's freshness reads this rather
    /// than the display window, so a dead proxy still shows its age.
    pub fn latest_ts_ms(&self) -> Result<Option<i64>> {
        ledger::latest_ts_ms(&*self.conn()?)
    }

    /// SQLite's `PRAGMA data_version` on this store's one connection: a
    /// counter that moves whenever ANOTHER connection — another
    /// process included, WAL or not — commits to the database, and
    /// never for this connection's own commits. The TUI polls it each
    /// second and re-reads the window only when it moved; the value is
    /// per-connection, which is why this lives on the store that owns
    /// the connection rather than opening one per check.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn()?
            .query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }

    /// The `/_toker/session` aggregate for one session id: the count and
    /// span of its measurement rows, token sums, the billed total, and
    /// the billed-cost breakdowns by serving provider and model (see
    /// `ledger`). A session with no rows answers `requests: 0` with
    /// `None` everywhere else.
    pub fn session_summary(&self, session_id: &str) -> Result<SessionSummary> {
        ledger::session_summary(&*self.conn()?, session_id)
    }

    /// One display parent's aggregate, including only its own raw rows and
    /// children with explicit stored lineage. This does not alter raw lane or
    /// cache identity.
    pub fn display_session_summary(&self, session_id: &str) -> Result<SessionSummary> {
        ledger::display_session_summary(&*self.conn()?, session_id)
    }

    /// One session's newest API measurement row (see
    /// `ledger::latest_session_row`).
    pub fn latest_session_row(&self, session_id: &str) -> Result<Option<RequestRow>> {
        ledger::latest_session_row(&*self.conn()?, session_id)
    }

    /// The latest measurement attributed to a display parent, including only
    /// explicit children. The returned row retains its raw child identity.
    pub fn latest_display_session_row(&self, session_id: &str) -> Result<Option<RequestRow>> {
        ledger::latest_display_session_row(&*self.conn()?, session_id)
    }

    /// Upsert one lane (caller owns the read-modify-write cycle).
    pub fn upsert_lane(&self, lane: &Lane) -> Result<()> {
        state::upsert_lane(&*self.conn()?, lane)
    }

    /// Create an absent lane without replacing evidence written concurrently.
    pub fn insert_lane_if_absent(&self, lane: &Lane) -> Result<()> {
        state::insert_lane_if_absent(&*self.conn()?, lane)
    }

    /// One lane by key.
    pub fn load_lane(&self, key: &str) -> Result<Option<Lane>> {
        state::load_lane(&*self.conn()?, key)
    }

    /// All lanes, by key.
    pub fn load_lanes(&self) -> Result<Vec<Lane>> {
        state::load_lanes(&*self.conn()?)
    }

    /// Replace a lane's complete policy pin only if generation and prior
    /// identity still match the caller's snapshot.
    pub fn compare_and_swap_lane_pin(
        &self,
        key: &str,
        expected_generation: i64,
        expected_pin: Option<&LanePin>,
        new_pin: Option<&LanePin>,
    ) -> Result<LanePinCasResult> {
        state::compare_and_swap_lane_pin(
            &*self.conn()?,
            key,
            expected_generation,
            expected_pin,
            new_pin,
        )
    }

    /// Insert an immutable revision without activating it.
    pub fn insert_policy_revision(
        &self,
        revision: &NewPolicyRevision<'_>,
    ) -> Result<PolicyRevision> {
        state::insert_policy_revision(&*self.conn()?, revision)
    }

    /// Atomically establish revision-one routing ownership, or load the
    /// already-active revision without consulting the supplied legacy document.
    pub fn activate_revision_one(
        &self,
        document: &crate::policy::ValidatedPolicyDocument,
        activated_ms: i64,
        desired_inventory_generation: i64,
        publications: &[InitialPublication],
    ) -> Result<RevisionOneActivation> {
        self.activate_revision_one_importing(
            document,
            activated_ms,
            desired_inventory_generation,
            publications,
            None,
        )
    }

    /// Activate revision one and import a configuration in the same
    /// transaction, so a machine moving off its file does both or neither
    /// (spec: RMIG#migrating-the-file-and-the-store-together).
    pub fn activate_revision_one_importing(
        &self,
        document: &crate::policy::ValidatedPolicyDocument,
        activated_ms: i64,
        desired_inventory_generation: i64,
        publications: &[InitialPublication],
        configuration: Option<&Configuration>,
    ) -> Result<RevisionOneActivation> {
        let mut conn = self.conn()?;
        state::activate_revision_one(
            &mut conn,
            document,
            activated_ms,
            desired_inventory_generation,
            publications,
            configuration,
        )
    }

    /// Atomically install an already-compiled policy revision and its desired
    /// publication rows. The caller swaps the in-memory runtime afterwards.
    pub fn activate_policy_revision(
        &self,
        activation: &ActivatePolicyRevision<'_>,
    ) -> Result<ActivatedPolicyRevision> {
        state::activate_policy_revision(&mut *self.conn()?, activation)
    }

    pub fn load_policy_revision(&self, revision_id: i64) -> Result<Option<PolicyRevision>> {
        state::load_policy_revision(&*self.conn()?, revision_id)
    }

    pub fn load_policy_revisions(&self) -> Result<Vec<PolicyRevision>> {
        state::load_policy_revisions(&*self.conn()?)
    }

    /// Whether the ledger's schema has every migration applied. A reader that
    /// opened it read-only uses this to tell an older ledger, which has no
    /// routing ownership yet, from one it cannot read.
    pub fn schema_is_current(&self) -> Result<bool> {
        let version = self
            .conn()?
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
        Ok(version >= schema::MIGRATIONS.len() as i64)
    }

    /// Configuration whole, as the store holds it (spec: DCONF#in-the-store).
    pub fn load_configuration(&self) -> Result<Configuration> {
        config::load_configuration(&*self.conn()?)
    }

    /// Write a whole configuration, replacing what is there. The import's
    /// path for a machine whose routing already moved: there is no revision
    /// to activate, so the rows are the whole transaction.
    pub fn write_configuration(&self, configuration: &Configuration) -> Result<()> {
        let mut conn = self.conn()?;
        // Immediate, not deferred: a deferred transaction that upgrades to a
        // write can come back BUSY without waiting out the busy timeout,
        // because SQLite will not retry an upgrade that could deadlock.
        // The long waits that first showed this up were a scratch setup run
        // opening the real machine's store, not a holder inside the wizard:
        // with each run on its own store the lock is taken in microseconds.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        config::write_configuration(&tx, configuration)?;
        tx.commit()?;
        Ok(())
    }

    /// Write one provider's row.
    pub fn put_config_provider(&self, row: &ProviderConfigRow) -> Result<()> {
        config::put_provider(&*self.conn()?, row)
    }

    /// Write the settings row.
    pub fn put_config_settings(&self, row: &SettingsConfigRow) -> Result<()> {
        config::put_settings(&*self.conn()?, row)
    }

    /// Set one profile's notice style.
    pub fn put_config_notice(&self, profile: &str, style: &str) -> Result<()> {
        config::put_notice(&*self.conn()?, profile, style)
    }

    /// Return one profile to the default notice style.
    pub fn delete_config_notice(&self, profile: &str) -> Result<()> {
        config::delete_notice(&*self.conn()?, profile)
    }

    pub fn load_routing_policy_state(&self) -> Result<RoutingPolicyState> {
        state::load_routing_policy_state(&*self.conn()?)
    }

    pub fn compare_and_swap_policy_draft(
        &self,
        expected_generation: i64,
        base_revision_id: i64,
        document: &crate::policy::ValidatedPolicyDocument,
        updated_ms: i64,
    ) -> Result<DraftCasResult> {
        state::compare_and_swap_policy_draft(
            &*self.conn()?,
            expected_generation,
            base_revision_id,
            document,
            updated_ms,
        )
    }

    pub fn clear_policy_draft(&self, expected_generation: i64) -> Result<DraftCasResult> {
        state::clear_policy_draft(&*self.conn()?, expected_generation)
    }

    pub fn set_publication_desired(
        &self,
        key: &PublicationKey,
        desired_revision_id: i64,
        desired_inventory_generation: i64,
        updated_ms: i64,
    ) -> Result<()> {
        state::set_publication_desired(
            &*self.conn()?,
            key,
            desired_revision_id,
            desired_inventory_generation,
            updated_ms,
        )
    }

    pub fn set_publications_desired(
        &self,
        publications: &[InitialPublication],
        desired_revision_id: i64,
        desired_inventory_generation: i64,
        updated_ms: i64,
    ) -> Result<Vec<RoutingPublication>> {
        let mut conn = self.conn()?;
        state::set_publications_desired(
            &mut conn,
            publications,
            desired_revision_id,
            desired_inventory_generation,
            updated_ms,
        )
    }

    pub fn load_publication(&self, key: &PublicationKey) -> Result<Option<RoutingPublication>> {
        state::load_publication(&*self.conn()?, key)
    }

    pub fn load_publications(&self) -> Result<Vec<RoutingPublication>> {
        state::load_publications(&*self.conn()?)
    }

    pub fn compare_and_swap_publication_result(
        &self,
        key: &PublicationKey,
        expected_revision_id: i64,
        expected_inventory_generation: i64,
        update: &PublicationResultUpdate,
    ) -> Result<PublicationCasResult> {
        state::compare_and_swap_publication_result(
            &*self.conn()?,
            key,
            expected_revision_id,
            expected_inventory_generation,
            update,
        )
    }

    pub fn merge_binding_learning(
        &self,
        update: &BindingLearningUpdate,
    ) -> Result<BindingLearning> {
        state::merge_binding_learning(&mut *self.conn()?, update)
    }

    pub fn promote_binding_learning(
        &self,
        binding_id: &crate::policy::BindingId,
        model_id: &str,
        updated_ms: i64,
    ) -> Result<Option<BindingLearning>> {
        state::promote_binding_learning(&mut *self.conn()?, binding_id, model_id, updated_ms)
    }

    pub fn load_binding_learning(
        &self,
        binding_id: &crate::policy::BindingId,
        model_id: &str,
    ) -> Result<Option<BindingLearning>> {
        state::load_binding_learning(&*self.conn()?, binding_id, model_id)
    }

    pub fn load_binding_learnings_for_model(&self, model_id: &str) -> Result<Vec<BindingLearning>> {
        state::load_binding_learnings_for_model(&*self.conn()?, model_id)
    }

    pub fn load_all_binding_learnings(&self) -> Result<Vec<BindingLearning>> {
        state::load_all_binding_learnings(&*self.conn()?)
    }

    pub fn load_binding_learnings(
        &self,
        binding_id: &crate::policy::BindingId,
    ) -> Result<Vec<BindingLearning>> {
        state::load_binding_learnings(&*self.conn()?, binding_id)
    }

    /// One session's lanes, newest first, through the session index.
    pub fn load_session_lanes(&self, session_id: &str) -> Result<Vec<Lane>> {
        state::load_session_lanes(&*self.conn()?, session_id)
    }

    /// Age and cap the lanes table (see `state::prune_lanes`): future-dated
    /// rows, rows older than `max_age_ms`, and everything beyond the
    /// newest `max_lanes` go. The caller owns the cadence — the prune
    /// runs on the 30-second flush, never per request.
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

    /// One session's allowances, ordered by meter, reset value (see
    /// `state::load_session_allowances`): the gate's per-request read.
    pub fn load_session_allowances(&self, session_id: &str) -> Result<Vec<Allowance>> {
        state::load_session_allowances(&*self.conn()?, session_id)
    }

    /// Delete every allowance one session holds: the TUI's Close control
    /// (see `state::delete_session_allowances`). Returns how many went.
    pub fn delete_session_allowances(&self, session_id: &str) -> Result<u64> {
        state::delete_session_allowances(&*self.conn()?, session_id)
    }

    /// Delete the allowances whose window has ended (see
    /// `state::prune_allowances`). Returns how many went.
    pub fn prune_allowances(&self, now_ms: i64) -> Result<u64> {
        state::prune_allowances(&*self.conn()?, now_ms)
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

    /// Mark a binding as having refused the server safeguards ask.
    pub fn set_safeguards_mark(
        &self,
        binding_id: &crate::policy::BindingId,
        now_ms: i64,
    ) -> Result<()> {
        state::set_safeguards_mark(&*self.conn()?, binding_id, now_ms)
    }

    /// Clear the marks held against these bindings; how many were held.
    pub fn clear_safeguards_marks(&self, bindings: &[&crate::policy::BindingId]) -> Result<u64> {
        state::clear_safeguards_marks(&*self.conn()?, bindings)
    }

    /// Every binding holding a server safeguards rejection mark.
    pub fn load_safeguards_marks(&self) -> Result<Vec<SafeguardsMark>> {
        state::load_safeguards_marks(&*self.conn()?)
    }

    /// Overwrite one meter-source backend's last snapshot, keyed by
    /// provider id (migration v3): the anthropic sub's quota meters and
    /// the codex sub's usage limits each keep their own slot.
    pub fn save_meters(&self, provider_id: &str, snapshot: &MetersSnapshot) -> Result<()> {
        state::save_meters(&*self.conn()?, provider_id, snapshot)
    }

    /// One provider's last meter snapshot, if that backend has produced
    /// one.
    pub fn load_meters(&self, provider_id: &str) -> Result<Option<MetersSnapshot>> {
        state::load_meters(&*self.conn()?, provider_id)
    }

    /// Overwrite one account's last snapshot on a provider. A reading from
    /// an unresolved credential names no account and so reaches no slot
    /// here, which is what keeps it out of every account's reading and fit
    /// (spec: ACCT#an-account-not-yet-known).
    pub fn save_account_meters(
        &self,
        provider_id: &str,
        account_number: i64,
        snapshot: &MetersSnapshot,
    ) -> Result<()> {
        state::save_account_meters(&*self.conn()?, provider_id, account_number, snapshot)
    }

    /// One account's last meter snapshot on a provider.
    pub fn load_account_meters(
        &self,
        provider_id: &str,
        account_number: i64,
    ) -> Result<Option<MetersSnapshot>> {
        state::load_account_meters(&*self.conn()?, provider_id, account_number)
    }

    /// Every account's last snapshot on a provider, by account number.
    pub fn load_meters_by_account(&self, provider_id: &str) -> Result<Vec<(i64, MetersSnapshot)>> {
        state::load_meters_by_account(&*self.conn()?, provider_id)
    }

    /// The account a session is being served on, as far as the ledger says:
    /// the one the session's newest served row names, and for a session with
    /// no row yet the one the provider served last.
    ///
    /// This is a guess from earlier traffic, for readers with no request in
    /// hand: the TUI showing a session, a release or revoke from it, and the
    /// lane pin. The quota gate does not use it. A request in flight names
    /// its account by its credential, and a gate that read it from here
    /// stayed on the account a login had been switched away from, because a
    /// blocked request writes no served row to correct it
    /// (spec: ACCT#routing-does-not-select-an-account).
    ///
    /// `None` is the answer when nothing has been served yet or no source
    /// has named the credential — an absence each caller handles, rather
    /// than a zero it could spend against (spec: ACCT#an-account-not-yet-known).
    ///
    pub fn account_for(&self, provider_id: &str, session_id: Option<&str>) -> Result<Option<i64>> {
        if let Some(session_id) = session_id
            && let Some(account) = self.session_account(provider_id, session_id)?
        {
            return Ok(Some(account));
        }
        if let Some(account) = self.provider_account(provider_id)? {
            return Ok(Some(account));
        }
        // Nothing served yet. One account known is not a guess, so take it;
        // several, and which one a request is on is exactly what nothing
        // has said yet, so answer none rather than pick the nearest.
        self.sole_account(provider_id)
    }

    /// The provider's only known account, or `None` where it holds none or
    /// holds more than one.
    pub fn sole_account(&self, provider_id: &str) -> Result<Option<i64>> {
        let mut numbers = self.account_numbers(provider_id)?;
        Ok(if numbers.len() == 1 {
            numbers.pop()
        } else {
            None
        })
    }

    /// Every account known on a provider, in discovery order. Callers that
    /// need to tell "none" from "more than one" apart read this; the table
    /// holds one row per login, so reading it whole costs nothing.
    pub fn account_numbers(&self, provider_id: &str) -> Result<Vec<i64>> {
        rows_of(
            &*self.conn()?,
            "SELECT number FROM accounts WHERE provider = ?1 ORDER BY number",
            [provider_id],
            |row| Ok(row.get("number")?),
        )
    }

    /// The account a provider most recently served, whatever the session.
    pub fn provider_account(&self, provider_id: &str) -> Result<Option<i64>> {
        row_of(
            &*self.conn()?,
            "SELECT account_number FROM requests
             WHERE provider = ?1 AND account_number IS NOT NULL
             ORDER BY ts_ms DESC, id DESC LIMIT 1",
            [provider_id],
            |row| Ok(row.get("account_number")?),
        )
    }

    /// The account a session is being served on: the newest row of that
    /// session that names one.
    pub fn session_account(&self, provider_id: &str, session_id: &str) -> Result<Option<i64>> {
        row_of(
            &*self.conn()?,
            "SELECT account_number FROM requests
             WHERE provider = ?1 AND session_id = ?2 AND account_number IS NOT NULL
             ORDER BY ts_ms DESC, id DESC LIMIT 1",
            (provider_id, session_id),
            |row| Ok(row.get("account_number")?),
        )
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
/// Make `dir` readable only by its owner. A directory that already is costs
/// nothing; one that is not is tightened in place.
#[cfg(unix)]
fn close_state_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(dir)?.permissions().mode();
    if mode & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn close_state_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

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
        AccountIdentity, AccountNameSource, ActivatePolicyRevision, Allowance, AttemptGroupId,
        AttemptIndex, AttemptProvenance, AttemptReason, BindingLearningUpdate, CostKind,
        CredentialDigest, DisplayRow, DraftCasResult, Error, InitialPublication, Lane, LanePin,
        LanePinCasResult, MAX_ROUTING_TRACE_EVENTS, MetersSnapshot, ModelEntry, NewPolicyRevision,
        PingAction, PingRecord, PolicyCompilerOrigin, PolicyRevisionSource, PolicyRouteProvenance,
        PolicyRoutingOutcome, PublicationAdapter, PublicationCasResult, PublicationKey,
        PublicationResultUpdate, PublicationState, RequestLookupProvenance, RequestRow,
        ResolvedRoute, RevisionOneActivation, RowKind, SessionCostGroup, SessionLineage, Store,
        StoredCandidateState, StoredRoutingTrace, StoredRoutingTraceEvent, StoredSkipReason,
        is_api_measurement,
    };
    use crate::ir::Release;
    use crate::policy::{
        ActivationScope, BindingId, CURRENT_FORMAT_VERSION, CandidateEvidence, CandidateId,
        FrontendProtocol, FrontendScope, PolicyDocument, PresentationId, ProfileId,
        RouteFingerprint, RouteId, RouteProvenance, SkipReason, StableId, TraceEvent,
    };
    use rusqlite::Connection;
    use serde_json::json;
    use std::collections::BTreeSet;

    /// A fresh private in-memory store per test.
    fn mem_store() -> Store {
        Store::open(":memory:").expect("open in-memory store")
    }

    /// A fresh scratch directory under /tmp/opencode (pre-created and
    /// approved for external access), unique per call so parallel tests
    /// never collide.
    fn test_dir(name: &str) -> crate::test_support::TestDir {
        crate::test_support::tempdir(&format!("store-{name}-"))
    }

    #[cfg(unix)]
    #[test]
    fn opening_the_store_closes_its_directory_to_everyone_else() {
        use std::os::unix::fs::PermissionsExt;

        let dir = test_dir("state-perms");
        let state = dir.join("state");
        std::fs::create_dir_all(&state).expect("create");
        // A directory made before toker kept a credential in its store was
        // made under the umask, so opening has to tighten it in place
        // (spec: DCONF#state).
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let _store = Store::open(state.join("toker.db")).expect("open");

        let mode = std::fs::metadata(&state)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "group and other bits are cleared");
    }

    fn user_version(store: &Store) -> i64 {
        let conn = store.conn.lock().expect("lock");
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .expect("read user_version")
    }

    fn policy(description: &str) -> crate::policy::ValidatedPolicyDocument {
        PolicyDocument {
            format_version: CURRENT_FORMAT_VERSION,
            description: Some(description.to_owned()),
            defaults: vec![],
            presentations: vec![],
            routes: vec![],
            rewrites: vec![],
            classifiers: vec![],
            upgrades: vec![],
            compaction: None,
        }
        .validate()
        .expect("valid policy")
    }

    fn revision(store: &Store, description: &str, parent_revision_id: Option<i64>) -> i64 {
        let document = policy(description);
        store
            .insert_policy_revision(&NewPolicyRevision {
                created_ms: 100,
                activated_ms: 101,
                description: Some(description),
                activation_scope: ActivationScope::NewLanes,
                forced_cache_transition: false,
                source: PolicyRevisionSource::Draft,
                compiler_origin: None,
                parent_revision_id,
                document: &document,
            })
            .expect("insert revision")
            .revision_id
    }

    fn provenanced_row(revision_id: i64) -> RequestRow {
        let mut row = bare_row(1_800_000_000_000);
        row.session_id = Some("guardian:parent-session".to_owned());
        row.binding_id = Some(BindingId::new("codex_sub.codex_responses").unwrap());
        row.attempt = Some(AttemptProvenance {
            attempt_group_id: AttemptGroupId::new("0123456789abcdef0123456789abcdef").unwrap(),
            attempt_index: AttemptIndex::zero(),
            reason: AttemptReason::Initial,
        });
        row.policy_routing = Some(PolicyRouteProvenance {
            policy_revision_id: revision_id,
            route: Some(ResolvedRoute {
                lookup: RequestLookupProvenance::ProfilePresentation(
                    PresentationId::new("presented").unwrap(),
                ),
                route_id: RouteId::new("route").unwrap(),
                route_fingerprint: "22".repeat(32).parse().unwrap(),
            }),
            trace: StoredRoutingTrace {
                version: 1,
                events: vec![
                    StoredRoutingTraceEvent::Lookup {
                        source: super::StoredLookupSource::ProfilePresentation,
                        presentation_id: Some(PresentationId::new("presented").unwrap()),
                        rewrite_id: None,
                    },
                    StoredRoutingTraceEvent::Candidate {
                        candidate_id: CandidateId::new("candidate").unwrap(),
                        crossed_from: None,
                        binding_id: BindingId::new("codex_sub.codex_responses").unwrap(),
                        model: Some("baseline".to_owned()),
                        state: StoredCandidateState::Available,
                        reason: None,
                    },
                ],
            },
            outcome: PolicyRoutingOutcome::Attempt {
                candidate_id: CandidateId::new("candidate").unwrap(),
                baseline_model: "baseline".to_owned(),
            },
        });
        row.lineage = Some(SessionLineage::codex_guardian("guardian:parent-session").unwrap());
        row
    }

    fn unpinned_lane(key: &str) -> Lane {
        Lane {
            key: key.to_owned(),
            session_id: Some("session".to_owned()),
            tools_hash: Some("tools".to_owned()),
            updated_ms: 1,
            prompt_tokens: Some(10),
            ttl: Some(300_000),
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
            pin_generation: 0,
            pin: None,
        }
    }

    /// A row with every field set, covering every column's Some path.
    fn full_row() -> RequestRow {
        RequestRow {
            safeguards: Default::default(),
            id: None,
            ts_ms: 1_769_000_000_012,
            duration_ms: Some(3_456),
            kind: None,
            frontend: Some("openai_chat".to_string()),
            provider: Some("openrouter".to_string()),
            route: Some("openai_chat:openrouter".to_string()),
            session_id: Some("ses-abc".to_string()),
            ping: Some(false),
            credential_digest: None,
            account_number: None,
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
            binding_id: None,
            attempt: None,
            policy_routing: None,
            lineage: None,
        }
    }

    /// A row with only `ts_ms` — every other column NULL.
    fn bare_row(ts_ms: i64) -> RequestRow {
        RequestRow {
            safeguards: Default::default(),
            id: None,
            ts_ms,
            duration_ms: None,
            kind: None,
            frontend: None,
            provider: None,
            route: None,
            session_id: None,
            ping: None,
            credential_digest: None,
            account_number: None,
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
            binding_id: None,
            attempt: None,
            policy_routing: None,
            lineage: None,
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
    fn a_safeguards_mark_survives_a_reopen_and_clears_by_binding() {
        let dir = test_dir("safeguards-mark");
        let db = dir.join("toker.db");
        let binding = crate::policy::BindingId::new("openrouter.openrouter_messages").unwrap();
        let other = crate::policy::BindingId::new("anthropic_sub.anthropic_messages").unwrap();
        let store = Store::open(&db).expect("open");
        store.set_safeguards_mark(&binding, 100).expect("mark");
        store
            .set_safeguards_mark(&binding, 200)
            .expect("marking again");
        drop(store);

        let store = Store::open(&db).expect("reopen");
        let marks = store.load_safeguards_marks().expect("marks");
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].binding_id, binding);
        assert_eq!(marks[0].marked_ms, 100, "the first time is kept");

        assert_eq!(store.clear_safeguards_marks(&[&other]).expect("clear"), 0);
        assert_eq!(store.clear_safeguards_marks(&[&binding]).expect("clear"), 1);
        assert!(store.load_safeguards_marks().expect("marks").is_empty());
    }

    /// The TUI's change check: `data_version` holds still with no
    /// writes, moves when another connection to the same file commits
    /// (the daemon is another process; a second store is the same
    /// shape), and does not move for this connection's own writes.
    #[test]
    fn data_version_moves_only_for_another_connections_commits() {
        let dir = crate::test_support::tempdir("toker-data-version-");
        let path = dir.join("toker.db");
        let reader = Store::open(&path).expect("open the reader");
        let writer = Store::open(&path).expect("open the writer");

        let before = reader.data_version().expect("version");
        assert_eq!(
            reader.data_version().expect("version"),
            before,
            "moved idle"
        );

        writer.record_request(&bare_row(1_000)).expect("record");
        let after = reader.data_version().expect("version");
        assert_ne!(after, before, "another connection's commit went unseen");
        assert_eq!(reader.data_version().expect("version"), after);

        reader.record_request(&bare_row(2_000)).expect("record");
        assert_eq!(
            reader.data_version().expect("version"),
            after,
            "its own commit moved it"
        );
        drop((reader, writer));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn learned_account_identity_stitches_refreshed_credentials() {
        let store = mem_store();
        let first = CredentialDigest::new("1".repeat(64)).expect("digest");
        let refreshed = CredentialDigest::new("2".repeat(64)).expect("digest");
        let identity = AccountIdentity {
            upstream_id: None,
            email: Some("agent@example.test".to_owned()),
            named_by: AccountNameSource::Local,
        };

        assert_eq!(
            store
                .observe_credential("anthropic_sub", &first, 100, Some(&identity))
                .expect("observe first"),
            Some(1)
        );
        assert_eq!(
            store
                .observe_credential("anthropic_sub", &refreshed, 200, Some(&identity))
                .expect("observe refresh"),
            Some(1)
        );
        let accounts = store.load_accounts().expect("accounts");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].number, 1);
        assert_eq!(accounts[0].email.as_deref(), Some("agent@example.test"));
        assert_eq!(accounts[0].last_seen_ms, 200);
        assert_eq!(
            store
                .load_credential_accounts()
                .expect("credentials")
                .into_iter()
                .map(|credential| credential.account_number)
                .collect::<Vec<_>>(),
            vec![Some(1), Some(1)]
        );
    }

    #[test]
    fn an_account_opens_without_a_credential_and_is_found_again_by_address() {
        let store = mem_store();
        let identity = AccountIdentity {
            upstream_id: None,
            email: Some("agent@example.test".to_owned()),
            named_by: AccountNameSource::Operator,
        };
        let first = store
            .open_account("anthropic_sub", &identity, 100)
            .expect("open");
        assert_eq!(
            store
                .open_account("anthropic_sub", &identity, 200)
                .expect("reopen"),
            first,
            "the same address is the same account"
        );
        let accounts = store.load_accounts().expect("accounts");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].named_by, AccountNameSource::Operator);

        // A credential that later resolves to that address joins it rather
        // than opening a second account.
        let digest = CredentialDigest::new("b".repeat(64)).expect("digest");
        let local = AccountIdentity {
            named_by: AccountNameSource::Local,
            ..identity.clone()
        };
        assert_eq!(
            store
                .observe_credential("anthropic_sub", &digest, 300, Some(&local))
                .expect("resolve"),
            Some(first)
        );
        assert_eq!(store.load_accounts().expect("accounts").len(), 1);

        let blank = AccountIdentity {
            email: None,
            ..identity
        };
        assert!(store.open_account("anthropic_sub", &blank, 400).is_err());
    }

    #[test]
    fn resolved_identity_backfills_earlier_digest_rows() {
        let store = mem_store();
        let digest = CredentialDigest::new("a".repeat(64)).expect("digest");
        store
            .execute_for_test(&format!(
                "INSERT INTO requests (ts_ms, provider, credential_digest) \
                 VALUES (1, 'anthropic_sub', '{}')",
                digest.as_str()
            ))
            .expect("seed unresolved row");
        assert_eq!(
            store
                .observe_credential("anthropic_sub", &digest, 100, None)
                .expect("observe"),
            None
        );
        let identity = AccountIdentity {
            upstream_id: None,
            email: Some("agent@example.test".to_owned()),
            named_by: AccountNameSource::Operator,
        };
        assert_eq!(
            store
                .observe_credential("anthropic_sub", &digest, 200, Some(&identity))
                .expect("resolve"),
            Some(1)
        );
        let account_number: Option<i64> = store
            .conn()
            .expect("connection")
            .query_row("SELECT account_number FROM requests", [], |row| row.get(0))
            .expect("read backfill");
        assert_eq!(account_number, Some(1));
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

        // Spot-check the spec: STORE#not-recorded-is-not-zero property where a silent zero would be
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
            RowKind::ColdRecap,
            RowKind::Awake,
            RowKind::Error,
            RowKind::RouteUnavailable,
            RowKind::FidelityDrift,
            RowKind::Revoked,
        ];
        for kind in kinds {
            assert!(!is_api_measurement(Some(kind)));
            assert_eq!(RowKind::parse(kind.as_str()), Some(kind));
        }

        let store = mem_store();
        for (i, kind) in kinds.iter().enumerate() {
            let mut row = bare_row(1_000 + i as i64);
            row.kind = Some(*kind);
            if *kind == RowKind::RouteUnavailable {
                let revision_id = revision(&store, "terminal-kind", None);
                row.policy_routing = Some(PolicyRouteProvenance {
                    policy_revision_id: revision_id,
                    route: Some(ResolvedRoute {
                        lookup: RequestLookupProvenance::ProtocolDefault,
                        route_id: RouteId::new("route").unwrap(),
                        route_fingerprint: "33".repeat(32).parse().unwrap(),
                    }),
                    trace: StoredRoutingTrace {
                        version: 1,
                        events: vec![StoredRoutingTraceEvent::Lookup {
                            source: super::StoredLookupSource::ProtocolDefault,
                            presentation_id: None,
                            rewrite_id: None,
                        }],
                    },
                    outcome: PolicyRoutingOutcome::RouteUnavailable {
                        attempt_group_id: AttemptGroupId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                            .unwrap(),
                    },
                });
            }
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
    fn a_panic_under_the_lock_does_not_darken_the_ledger() {
        // Recording swallows store errors, so a poisoned lock reported as
        // an error would stop every later row, silently, until a restart.
        let store = mem_store();
        std::thread::scope(|scope| {
            let poisoned = scope
                .spawn(|| {
                    let _held = store.conn.lock().expect("lock");
                    panic!("a panic while the store lock is held");
                })
                .join();
            assert!(poisoned.is_err());
        });
        assert!(store.conn.is_poisoned());
        store
            .record_request(&bare_row(100))
            .expect("records after poison");
        assert_eq!(store.latest_ts_ms().expect("latest"), Some(100));
    }

    #[test]
    fn latest_ts_ms_is_the_newest_row_of_any_kind() {
        let store = mem_store();
        assert_eq!(store.latest_ts_ms().expect("empty"), None, "empty ledger");
        // Out of insertion order, and the newest row a proxy-written
        // kind: freshness counts every row, not only measurements.
        for ts in [100, 300, 200] {
            store.record_request(&bare_row(ts)).expect("record");
        }
        let mut error = bare_row(400);
        error.kind = Some(RowKind::Error);
        store.record_request(&error).expect("record");
        assert_eq!(store.latest_ts_ms().expect("latest"), Some(400));
    }

    /// A row carrying only a gate flag (the cold-notice/release shape:
    /// those rows are written with `gate_on` and no `rate_limits`).
    fn gate_only_row(ts_ms: i64, gate_on: bool) -> RequestRow {
        let mut row = bare_row(ts_ms);
        row.gate_on = Some(gate_on);
        row
    }

    /// A row carrying only a meter snapshot (a measurement whose
    /// backend reports meters; `gate_on` predates it or was NULL).
    fn meter_only_row(ts_ms: i64, limits: serde_json::Value) -> RequestRow {
        let mut row = bare_row(ts_ms);
        row.rate_limits = Some(limits);
        row
    }

    #[test]
    fn meter_rows_since_filters_to_snapshot_or_gate_rows() {
        let store = mem_store();
        // Out of order, mixed: bare rows (neither field), gate-only
        // rows, meter-only rows, and one carrying both.
        for ts in [100, 200, 300, 400, 500, 600] {
            store.record_request(&bare_row(ts)).expect("record");
        }
        store
            .record_request(&gate_only_row(150, false))
            .expect("record");
        store
            .record_request(&meter_only_row(250, json!({"util5h": 0.4})))
            .expect("record");
        store
            .record_request(&gate_only_row(350, true))
            .expect("record");
        store
            .record_request(&meter_only_row(450, json!({"util7d": 0.8})))
            .expect("record");
        let mut both = meter_only_row(550, json!({"utilOverage": 0.6}));
        both.gate_on = Some(true);
        store.record_request(&both).expect("record");

        let rows = store.meter_rows_since(0, 100).expect("read");
        // Every fetched row carries a snapshot or a flag; the six bare
        // rows never reach the read.
        assert_eq!(rows.len(), 5, "{rows:?}");
        assert!(
            rows.iter()
                .all(|row| row.rate_limits.is_some() || row.gate_on.is_some())
        );
        assert_eq!(
            rows.iter().map(|row| row.ts_ms).collect::<Vec<_>>(),
            vec![150, 250, 350, 450, 550],
            "oldest first, boundary inclusive"
        );
        // The gate-only rows keep their absence: no snapshot was
        // invented for them (spec: STORE#not-recorded-is-not-zero).
        assert_eq!(rows[0].rate_limits, None);
        assert_eq!(rows[0].gate_on, Some(false));
        assert_eq!(rows[1].rate_limits, Some(json!({"util5h": 0.4})));
        assert_eq!(rows[1].gate_on, None);

        // The window filters the same way the full read does.
        let rows = store.meter_rows_since(250, 100).expect("window");
        assert_eq!(
            rows.iter().map(|row| row.ts_ms).collect::<Vec<_>>(),
            vec![250, 350, 450, 550]
        );
        assert!(
            store.meter_rows_since(700, 10).expect("empty").is_empty(),
            "window past the newest row is empty"
        );

        // The cap keeps the NEWEST matching rows, still oldest-first —
        // `requests_since`'s semantics over the filtered set.
        let rows = store.meter_rows_since(0, 3).expect("capped");
        assert_eq!(
            rows.iter().map(|row| row.ts_ms).collect::<Vec<_>>(),
            vec![350, 450, 550],
            "cap keeps the newest snapshot-or-flag rows"
        );
    }

    #[test]
    fn meter_rows_round_trip_kinds_flags_and_snapshots() {
        let store = mem_store();
        // One row of every shape the aggregation reads: a blocked stale
        // copy (kind + snapshot, no flag), a cold row (kind + flag, no
        // snapshot — the production shape from `record_anthropic`), and
        // a plain measurement (snapshot only).
        let mut blocked = meter_only_row(100, json!({"util5h": 0.99, "claim": "five_hour"}));
        blocked.kind = Some(RowKind::Blocked);
        store.record_request(&blocked).expect("record");
        let mut cold = bare_row(200);
        cold.kind = Some(RowKind::Cold);
        cold.gate_on = Some(true);
        store.record_request(&cold).expect("record");
        store
            .record_request(&meter_only_row(
                300,
                json!({"util7d": 0.21, "overageInUse": true}),
            ))
            .expect("record");

        let rows = store.meter_rows_since(0, 10).expect("read");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].ts_ms, 100);
        assert_eq!(rows[0].kind, Some(RowKind::Blocked));
        assert_eq!(rows[0].gate_on, None);
        assert_eq!(
            rows[0].rate_limits,
            Some(json!({"util5h": 0.99, "claim": "five_hour"}))
        );
        assert_eq!(rows[1].ts_ms, 200);
        assert_eq!(rows[1].kind, Some(RowKind::Cold));
        assert_eq!(rows[1].gate_on, Some(true));
        assert_eq!(
            rows[1].rate_limits, None,
            "the flag-only row's absence stays absence"
        );
        assert_eq!(
            rows[2].kind, None,
            "a measurement's kind round-trips as None"
        );
        assert_eq!(
            rows[2].rate_limits,
            Some(json!({"util7d": 0.21, "overageInUse": true}))
        );
    }

    #[test]
    fn meter_rows_reject_unknown_stored_kinds() {
        // Same rule as the full read: a corrupted kind must error, not
        // silently reclassify the row as an API measurement — the kind
        // column decides which snapshot-carrying rows may baseline a
        // span total.
        let store = mem_store();
        {
            let conn = store.conn.lock().expect("lock");
            conn.execute(
                "INSERT INTO requests (ts_ms, kind, rate_limits) VALUES (1, 'mystery', '{}')",
                [],
            )
            .expect("insert bogus kind");
        }
        match store.meter_rows_since(0, 10) {
            Err(Error::UnknownDbValue { column: "kind", .. }) => {}
            other => panic!("unknown kind must error, got {other:?}"),
        }
    }

    /// A measurement row on `provider` reporting a five-hour reset.
    #[cfg(test)]
    fn window_row(ts_ms: i64, provider: &str, reset_s: i64) -> RequestRow {
        let mut row = bare_row(ts_ms);
        row.provider = Some(provider.to_owned());
        row.rate_limits = Some(json!({"util5h": 0.1, "reset5h": reset_s}));
        row
    }

    const HOUR_MS: i64 = 60 * 60 * 1000;

    #[test]
    fn two_accounts_sharing_a_reset_hold_their_allowances_apart() {
        let store = mem_store();
        let one = crate::test_support::account(&store, "anthropic_sub", "one@example.test");
        let two = crate::test_support::account(&store, "anthropic_sub", "two@example.test");
        assert_ne!(one, two);
        // A reset is a time, and nothing stops two accounts' windows
        // landing on the same one. Keyed by the reset alone, a release
        // granted on the spent account would carry a session on the
        // account that was never stopped.
        let allowance = |account, release| Allowance {
            session_id: "ses-shared".to_owned(),
            account_number: account,
            meter: "5h".to_owned(),
            reset_value: 1_700_000_000,
            release,
        };
        store
            .record_allowance(&allowance(one, Release::Plan))
            .expect("record");
        store
            .record_allowance(&allowance(two, Release::Overage))
            .expect("record");
        let held = store.load_allowances().expect("allowances");
        assert_eq!(held.len(), 2, "one row each, not one overwriting the other");
        assert_eq!(
            held.iter()
                .find(|row| row.account_number == one)
                .map(|row| row.release),
            Some(Release::Plan)
        );
        assert_eq!(
            held.iter()
                .find(|row| row.account_number == two)
                .map(|row| row.release),
            Some(Release::Overage)
        );
    }

    #[test]
    fn rows_predating_accounts_are_counted_apart_from_rows_awaiting_resolution() {
        let store = mem_store();
        store
            .record_request(&window_row(1_000, "anthropic_sub", 500))
            .expect("record");
        // Another provider's rows are not waiting on anything: it has no
        // accounts to tell apart, so nothing could ever attribute them.
        store
            .record_request(&window_row(2_000, "openrouter", 500))
            .expect("record");
        // Served under a credential whose identity is not resolved yet. That
        // row is attributed, to a digest, and resolving it backfills the
        // account; setup has nothing to do for it.
        let mut pending = window_row(3_000, "anthropic_sub", 500);
        pending.credential_digest =
            Some(CredentialDigest::new("ab".repeat(32)).expect("a well-formed digest"));
        store.record_request(&pending).expect("record");

        // A block the gate wrote after accounts began names neither, since
        // it sent nothing upstream. It is not history from before accounts,
        // and counting it would stop every command from running.
        let mut blocked = window_row(4_000, "anthropic_sub", 500);
        blocked.kind = Some(RowKind::Blocked);
        store.record_request(&blocked).expect("record");

        assert_eq!(
            store
                .unattributed_row_count("anthropic_sub")
                .expect("count"),
            1
        );
        assert_eq!(
            store.unattributed_row_count("openrouter").expect("count"),
            1,
            "counted per provider, so a caller asks only about one that has accounts"
        );
    }

    #[test]
    fn one_accounts_windows_read_as_one_series() {
        let store = mem_store();
        let reset = |hours: i64| hours * 3_600;
        let minutes = |m: i64| m * 60;
        // One series, in the three shapes a real store holds it. Each
        // window's rows report its own reset; the next window starts
        // where the last one ended; and a window still open is
        // re-anchored forward in ten-minute steps while traffic keeps
        // arriving, which is what a live store does all day.
        for (at_h, reset_s) in [
            (1, reset(5)),
            (3, reset(5)),
            (4, reset(5) + minutes(10)),
            (4, reset(5) + minutes(20)),
            (6, reset(10)),
            (9, reset(10)),
        ] {
            store
                .record_request(&window_row(at_h * HOUR_MS, "anthropic_sub", reset_s))
                .expect("record");
        }
        // A reading naming a window that had already ended when it was
        // served is a stale copy, not a second account's window.
        store
            .record_request(&window_row(7 * HOUR_MS, "anthropic_sub", reset(5)))
            .expect("record");
        assert_eq!(
            store
                .unattributed_window_conflict("anthropic_sub")
                .expect("walk"),
            None
        );
    }

    #[test]
    fn two_accounts_windows_overlap_and_are_refused() {
        let store = mem_store();
        let reset = |hours: i64| hours * 3_600;
        // Two logins taking turns: each row names a window still ahead of
        // it, and the series steps back to the earlier one while the later
        // one is still open. One account's window only moves forward.
        // Each reset is within five hours of the row reporting it, as a
        // five-hour window's always is.
        for (at_h, reset_h) in [(1, 5), (2, 6), (3, 5)] {
            store
                .record_request(&window_row(at_h * HOUR_MS, "anthropic_sub", reset(reset_h)))
                .expect("record");
        }
        assert_eq!(
            store
                .unattributed_window_conflict("anthropic_sub")
                .expect("walk"),
            Some(crate::store::MixedHistory {
                at_ms: 3 * HOUR_MS,
                open_reset_s: reset(6),
                row_reset_s: reset(5),
            })
        );
    }

    #[test]
    fn attributing_a_history_moves_every_row_once_and_repeats_harmlessly() {
        let store = mem_store();
        let account = crate::test_support::account(&store, "anthropic_sub", "one@example.test");
        for at in [1_000, 2_000, 3_000] {
            store
                .record_request(&window_row(at, "anthropic_sub", 500))
                .expect("record");
        }
        // A digest-bearing row is already attributed and must not be moved:
        // whichever account its digest resolves to is not this one to choose.
        let mut pending = window_row(4_000, "anthropic_sub", 500);
        pending.credential_digest =
            Some(CredentialDigest::new("ab".repeat(32)).expect("a well-formed digest"));
        store.record_request(&pending).expect("record");

        assert_eq!(
            store
                .attribute_unattributed_rows("anthropic_sub", account)
                .expect("attribute"),
            3
        );
        assert_eq!(
            store
                .unattributed_row_count("anthropic_sub")
                .expect("count"),
            0
        );
        // An interrupted move is repaired by running setup again, which finds
        // nothing left to do rather than failing.
        assert_eq!(
            store
                .attribute_unattributed_rows("anthropic_sub", account)
                .expect("attribute"),
            0
        );
    }

    #[test]
    fn the_furthest_reset_reads_only_measurements_from_the_provider() {
        let store = mem_store();
        let row = |ts_ms: i64, provider: &str, ping: Option<bool>, reset: serde_json::Value| {
            let mut row = bare_row(ts_ms);
            row.provider = Some(provider.to_owned());
            row.ping = ping;
            row.rate_limits = Some(json!({"util5h": 0.1, "reset5h": reset}));
            row
        };
        // Out of order: the furthest reset wins, not the newest row's.
        store
            .record_request(&row(1_000, "anthropic_sub", None, json!(500)))
            .expect("record");
        store
            .record_request(&row(2_000, "anthropic_sub", Some(true), json!(700)))
            .expect("record");
        store
            .record_request(&row(3_000, "anthropic_sub", None, json!(600)))
            .expect("record");
        // Another backend, a proxy-written copy, and a non-numeric
        // reset: none of them is a reading.
        store
            .record_request(&row(3_000, "anthropic_api", Some(true), json!(9_000)))
            .expect("record");
        let mut blocked = row(3_000, "anthropic_sub", None, json!(9_000));
        blocked.kind = Some(RowKind::Blocked);
        store.record_request(&blocked).expect("record");
        store
            .record_request(&row(3_000, "anthropic_sub", None, json!("9000")))
            .expect("record");

        assert_eq!(
            store
                .furthest_reset5h("anthropic_sub", None, 0, false)
                .expect("read"),
            Some(700)
        );
        assert_eq!(
            store
                .furthest_reset5h("anthropic_sub", None, 2_500, false)
                .expect("read"),
            Some(600),
            "the cutoff drops older rows"
        );
        assert_eq!(
            store
                .furthest_reset5h("anthropic_sub", None, 2_500, true)
                .expect("read"),
            None,
            "no ping-tagged reading after the cutoff"
        );
        assert_eq!(
            store
                .furthest_reset5h("codex_sub", None, 0, false)
                .expect("read"),
            None
        );

        assert!(store.ping_landed("anthropic_sub", 2_000).expect("read"));
        assert!(!store.ping_landed("anthropic_sub", 2_001).expect("read"));
        assert!(
            store.ping_landed("anthropic_api", 2_001).expect("read"),
            "the API's own ping row is still that provider's"
        );
    }

    #[test]
    fn pings_reject_unknown_stored_actions() {
        let store = mem_store();
        {
            let conn = store.conn.lock().expect("lock");
            conn.execute(
                "INSERT INTO pings (ts_ms, action) VALUES (1, 'mystery')",
                [],
            )
            .expect("insert bogus action");
        }
        match store.pings_since(0, 10) {
            Err(Error::UnknownDbValue {
                column: "action", ..
            }) => {}
            other => panic!("unknown action must error, got {other:?}"),
        }
    }

    #[test]
    fn display_rows_round_trip_the_narrow_projection() {
        let store = mem_store();
        // A full row — including an `extra` payload (the production
        // openrouter shape) and every other column the narrow read
        // does not carry.
        let mut row = full_row();
        row.extra = Some(json!({"serving_provider": "Relace"}));
        store.record_request(&row).expect("record");

        let rows = store.display_rows_since(0, 10).expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            DisplayRow {
                ts_ms: row.ts_ms,
                kind: None, // full_row is a measurement
                attempt_group_id: None,
                attempt_index: None,
                session_id: Some("ses-abc".to_string()),
                lineage: None,
                tools_hash: Some("sha256:tool5".to_string()),
                model: Some("z-ai/glm-5.3".to_string()),
                provider: Some("openrouter".to_string()),
                input: Some(12_345),
                cache_read: Some(100_000),
                cache_write_5m: Some(3_000),
                cache_write_1h: Some(2_000),
                output: Some(678),
                reasoning: Some(90),
                serving_provider: Some("Relace".to_string()),
                cost_usd: Some(0.00213),
                cost_kind: Some(CostKind::Billed),
                req_messages: Some(42),
                compact_generations: Some(1),
                forced_to: Some("z-ai/glm-5.3".to_string()),
            },
            "the eighteen display columns round-trip; the rest never cross"
        );

        // A bare row's absence stays absence on every one of the ten.
        store.record_request(&bare_row(9_999)).expect("record");
        let rows = store.display_rows_since(9_999, 10).expect("read");
        assert_eq!(
            rows[0],
            DisplayRow {
                ts_ms: 9_999,
                kind: None,
                attempt_group_id: None,
                attempt_index: None,
                session_id: None,
                lineage: None,
                tools_hash: None,
                model: None,
                provider: None,
                input: None,
                cache_read: None,
                cache_write_5m: None,
                cache_write_1h: None,
                output: None,
                reasoning: None,
                serving_provider: None,
                cost_usd: None,
                cost_kind: None,
                req_messages: None,
                compact_generations: None,
                forced_to: None,
            }
        );
    }

    #[test]
    fn display_rows_round_trip_explicit_lineage() {
        let store = mem_store();
        let mut row = bare_row(1);
        row.session_id = Some("guardian:parent-session".to_owned());
        row.lineage = SessionLineage::codex_guardian("guardian:parent-session");
        store.record_request(&row).expect("record");

        let display = store.display_rows_since(0, 10).expect("read");
        assert_eq!(display.len(), 1);
        let lineage = display[0].lineage.as_ref().expect("lineage");
        assert_eq!(lineage.parent_session_id(), "parent-session");
        assert_eq!(lineage.child_role().as_str(), "guardian");
    }

    #[test]
    fn display_rows_since_keeps_every_kind_windows_and_caps_like_requests_since() {
        let store = mem_store();
        // Out of order, mixed kinds: bare measurements, an error row,
        // a fidelity-drift row. The display read keeps EVERY kind (no
        // filter — `window_empty` needs them all), orders oldest-first
        // with the id tie-break, windows inclusively, and caps keeping
        // the newest — `requests_since`'s semantics over the same set.
        for ts in [100, 300, 200, 500, 400] {
            store.record_request(&bare_row(ts)).expect("record");
        }
        let mut error = bare_row(150);
        error.kind = Some(RowKind::Error);
        store.record_request(&error).expect("record");
        let mut drift = bare_row(250);
        drift.kind = Some(RowKind::FidelityDrift);
        drift.session_id = Some("ses-x".to_owned());
        drift.model = Some("z-ai/glm-5.3".to_owned());
        drift.provider = Some("openrouter".to_owned());
        drift.input = Some(7);
        drift.cache_read = Some(3);
        drift.output = Some(1);
        drift.cost_usd = Some(1.0);
        drift.cost_kind = Some(CostKind::Billed);
        store.record_request(&drift).expect("record");

        let ts = |rows: Vec<DisplayRow>| rows.into_iter().map(|r| r.ts_ms).collect::<Vec<_>>();
        assert_eq!(
            ts(store.display_rows_since(0, 100).expect("all")),
            vec![100, 150, 200, 250, 300, 400, 500],
            "oldest first, every kind kept"
        );
        let all = store.display_rows_since(0, 100).expect("all");
        assert_eq!(all[1].kind, Some(RowKind::Error));
        assert_eq!(all[3].kind, Some(RowKind::FidelityDrift));
        assert_eq!(all[3].session_id.as_deref(), Some("ses-x"));
        assert_eq!(all[3].cost_kind, Some(CostKind::Billed));

        assert_eq!(
            ts(store.display_rows_since(250, 100).expect("window")),
            vec![250, 300, 400, 500],
            "window is inclusive of the boundary"
        );
        assert!(
            store.display_rows_since(600, 10).expect("empty").is_empty(),
            "window past the newest row is empty"
        );
        assert_eq!(
            ts(store.display_rows_since(0, 3).expect("capped")),
            vec![300, 400, 500],
            "cap keeps the newest rows, still oldest-first"
        );
    }

    /// A rebuild-walk row carrying the classifier's full shape: a lane,
    /// a system prompt with blocks, a compaction generation, and a
    /// ≥-threshold rewrite.
    fn rebuild_row(ts_ms: i64, session: &str) -> RequestRow {
        let mut row = bare_row(ts_ms);
        row.session_id = Some(session.to_owned());
        row.tools_hash = Some("sha256:tools-1".to_owned());
        row.system_hash = Some("sha256:sys-1".to_owned());
        row.system_chars = Some(43_696);
        row.system_blocks = Some(json!([
            {"hash": "sha256:b1", "chars": 1_000},
            {"hash": "sha256:b2", "chars": 42_696},
        ]));
        row.req_messages = Some(42);
        row.req_tools = Some(17);
        row.compact_generations = Some(1);
        row.summarising = Some(false);
        row.cache_read = Some(66_944);
        row.cache_write_total = Some(60_000);
        row.cache_write_5m = Some(0);
        row.cache_write_1h = Some(60_000);
        row.input = Some(673);
        row.model = Some("claude-opus-5".to_owned());
        row
    }

    #[test]
    fn rebuild_rows_since_filters_measurements_windows_and_caps() {
        let store = mem_store();
        // Out of order, mixed kinds: measurements, an error row, a
        // blocked row. The proxy-written kinds must never reach the
        // walk — a notice row has no system hash, and read as a lane
        // predecessor it attributes the next rebuild to a changed
        // system prompt (a proxy-written row is not evidence about any
        // prompt).
        for ts in [100, 300, 200] {
            store
                .record_request(&rebuild_row(ts, "ses-a"))
                .expect("record");
        }
        let mut error = bare_row(150);
        error.kind = Some(RowKind::Error);
        store.record_request(&error).expect("record");
        let mut blocked = bare_row(250);
        blocked.kind = Some(RowKind::Blocked);
        store.record_request(&blocked).expect("record");

        let rows = store.rebuild_rows_since(0, 100).expect("read");
        assert_eq!(rows.len(), 3, "measurements only, kinds never cross");
        assert_eq!(
            rows.iter().map(|row| row.ts_ms).collect::<Vec<_>>(),
            vec![100, 200, 300],
            "oldest first, (ts, id) tie-break"
        );
        let row = &rows[0];
        assert_eq!(row.id, 1, "the row id rides the narrow read");
        assert_eq!(row.session_id.as_deref(), Some("ses-a"));
        assert_eq!(row.tools_hash.as_deref(), Some("sha256:tools-1"));
        assert_eq!(row.system_hash.as_deref(), Some("sha256:sys-1"));
        assert_eq!(row.system_chars, Some(43_696));
        assert_eq!(
            row.system_blocks,
            Some(json!([
                {"hash": "sha256:b1", "chars": 1_000},
                {"hash": "sha256:b2", "chars": 42_696},
            ])),
            "the one JSON column parses once, here"
        );
        assert_eq!(row.req_messages, Some(42));
        assert_eq!(row.compact_generations, Some(1));
        assert_eq!(row.cache_read, Some(66_944));
        assert_eq!(row.cache_write_total, Some(60_000));
        assert_eq!(row.input, Some(673));
        assert_eq!(row.model.as_deref(), Some("claude-opus-5"));

        // The window is inclusive; the cap keeps the newest.
        assert_eq!(
            store
                .rebuild_rows_since(200, 100)
                .expect("window")
                .iter()
                .map(|row| row.ts_ms)
                .collect::<Vec<_>>(),
            vec![200, 300]
        );
        assert_eq!(
            store
                .rebuild_rows_since(0, 2)
                .expect("capped")
                .iter()
                .map(|row| row.ts_ms)
                .collect::<Vec<_>>(),
            vec![200, 300],
            "cap keeps the newest measurement rows"
        );

        // A bare row's absence stays absence on every column — an
        // unknown rewrite is counted as such by the walk, never zero.
        store.record_request(&bare_row(400)).expect("record");
        let rows = store.rebuild_rows_since(350, 10).expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cache_write_total, None);
        assert_eq!(rows[0].system_blocks, None);
        assert_eq!(rows[0].tools_hash, None);
    }

    #[test]
    fn lane_system_reads_stay_in_the_lane_and_skip_proxy_rows() {
        let store = mem_store();
        let mut first = rebuild_row(100, "ses-a");
        first.system_ladder = Some(r#"["r1"]"#.to_owned());
        first.system_tail = Some(r#"["t1"]"#.to_owned());
        store.record_request(&first).expect("record");
        // Same prompt, ladders dropped.
        store
            .record_request(&rebuild_row(200, "ses-a"))
            .expect("record");
        // Another lane in the session, another session, and a proxy row,
        // all newer: none is the lane's predecessor.
        let mut other_tools = rebuild_row(300, "ses-a");
        other_tools.tools_hash = Some("sha256:tools-2".to_owned());
        other_tools.system_hash = Some("sha256:sys-9".to_owned());
        store.record_request(&other_tools).expect("record");
        let mut other_session = rebuild_row(300, "ses-b");
        other_session.system_hash = Some("sha256:sys-9".to_owned());
        store.record_request(&other_session).expect("record");
        let mut proxy = rebuild_row(400, "ses-a");
        proxy.kind = Some(RowKind::Error);
        proxy.system_hash = Some("sha256:sys-9".to_owned());
        store.record_request(&proxy).expect("record");

        let prev = store
            .lane_system_predecessor("ses-a", "sha256:tools-1")
            .expect("read")
            .expect("a predecessor");
        assert_eq!(prev.ts_ms, 200);
        assert_eq!(prev.system_hash, "sha256:sys-1");
        assert_eq!(prev.system_chars, Some(43_696));
        assert!(
            store
                .lane_system_predecessor("ses-c", "sha256:tools-1")
                .expect("read")
                .is_none(),
            "a lane with no rows has no predecessor"
        );

        // The rungs come from the row that kept them, at or before the
        // predecessor, and only for its hash.
        let ladders = store
            .lane_system_ladders(
                "ses-a",
                "sha256:tools-1",
                "sha256:sys-1",
                (prev.ts_ms, prev.id),
            )
            .expect("read");
        assert_eq!(
            ladders,
            Some((Some(vec!["r1".to_owned()]), Some(vec!["t1".to_owned()])))
        );
        assert_eq!(
            store
                .lane_system_ladders("ses-a", "sha256:tools-1", "sha256:sys-1", (99, i64::MAX))
                .expect("read"),
            None,
            "nothing before the first row"
        );
        assert_eq!(
            store
                .lane_system_ladders(
                    "ses-a",
                    "sha256:tools-1",
                    "sha256:sys-2",
                    (prev.ts_ms, prev.id)
                )
                .expect("read"),
            None,
            "another prompt's rungs are never borrowed"
        );
    }

    #[test]
    fn localisation_rows_fetch_only_the_asked_ids() {
        let store = mem_store();
        // Three shaped rows; the middle one carries ladders, a tail,
        // and a capture-time change.
        let first = rebuild_row(100, "ses-a");
        store.record_request(&first).expect("record");
        let mut changed = rebuild_row(200, "ses-a");
        changed.system_hash = Some("sha256:sys-2".to_owned());
        changed.system_ladder = Some(r#"["r1","r2"]"#.to_owned());
        changed.system_tail = Some(r#"["t1","t2"]"#.to_owned());
        changed.system_change =
            Some(json!({"delta": 105, "where": "block 1, in the last 8 bytes"}));
        store.record_request(&changed).expect("record");
        let third = rebuild_row(300, "ses-a");
        store.record_request(&third).expect("record");
        let ids: Vec<i64> = store
            .rebuild_rows_since(0, 10)
            .expect("rows")
            .iter()
            .map(|row| row.id)
            .collect();

        // The targeted fetch: only the asked ids come back, keyed by
        // id, heavy columns parsed.
        let rows = store
            .localisation_rows(&[ids[1], ids[2]])
            .expect("localise");
        assert_eq!(rows.len(), 2);
        let by_id: std::collections::HashMap<_, _> =
            rows.into_iter().map(|row| (row.id, row)).collect();
        let changed = by_id.get(&ids[1]).expect("the changed row");
        assert_eq!(
            changed.system_ladder,
            Some(vec!["r1".to_owned(), "r2".to_owned()])
        );
        assert_eq!(
            changed.system_tail,
            Some(vec!["t1".to_owned(), "t2".to_owned()])
        );
        assert_eq!(
            changed.system_change,
            Some(json!({"delta": 105, "where": "block 1, in the last 8 bytes"}))
        );
        let third = by_id.get(&ids[2]).expect("the third row");
        assert_eq!(third.system_ladder, None);
        assert_eq!(third.system_tail, None);
        assert_eq!(third.system_change, None);

        // Empty ids ask for nothing and get nothing.
        assert!(store.localisation_rows(&[]).expect("empty").is_empty());
    }

    #[test]
    fn display_rows_reject_unknown_stored_enums() {
        // Both enum columns decide classification (kind: measurement or
        // not; cost_kind: billed or priced-but-not), so a corrupted
        // value must error on the narrow read exactly as it does on
        // the full-row read — never silently reclassify a row.
        let store = mem_store();
        {
            let conn = store.conn.lock().expect("lock");
            conn.execute(
                "INSERT INTO requests (ts_ms, kind) VALUES (1, 'mystery')",
                [],
            )
            .expect("insert bogus kind");
        }
        match store.display_rows_since(0, 10) {
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
        match store.display_rows_since(0, 10) {
            Err(Error::UnknownDbValue {
                column: "cost_kind",
                ..
            }) => {}
            other => panic!("unknown cost_kind must error, got {other:?}"),
        }
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
    fn session_summary_groups_attempts_but_sums_each_attempts_usage() {
        let store = mem_store();
        let group = AttemptGroupId::new("feedfeedfeedfeedfeedfeedfeedfeed").unwrap();
        for (ts_ms, index, reason, input) in [
            (1, 0, AttemptReason::Initial, 3),
            (2, 1, AttemptReason::ThinkingNegotiation, 5),
        ] {
            let mut row = bare_row(ts_ms);
            row.session_id = Some("ses-attempts".to_owned());
            row.binding_id = Some(BindingId::new("anthropic_sub.anthropic_messages").unwrap());
            row.input = Some(input);
            row.attempt = Some(AttemptProvenance {
                attempt_group_id: group.clone(),
                attempt_index: AttemptIndex::try_from(index).unwrap(),
                reason,
            });
            store.record_request(&row).unwrap();
        }

        let summary = store.session_summary("ses-attempts").unwrap();
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.input, Some(8));
    }

    #[test]
    fn session_lanes_are_exact_newest_first_and_indexed() {
        let store = mem_store();
        let lane = |session: &str, tools: &str, updated_ms: i64| Lane {
            key: format!("{session}|{tools}"),
            session_id: Some(session.to_owned()),
            tools_hash: Some(tools.to_owned()),
            updated_ms,
            prompt_tokens: None,
            ttl: None,
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
            pin: None,
            pin_generation: 0,
        };
        for lane in [
            lane("s1", "aaaa", 1_000),
            lane("s1", "bbbb", 3_000),
            lane("s1", "cccc", 3_000),
            lane("s10", "aaaa", 9_000),
        ] {
            store.upsert_lane(&lane).expect("upsert");
        }

        let keys: Vec<String> = store
            .load_session_lanes("s1")
            .expect("lanes")
            .into_iter()
            .map(|lane| lane.key)
            .collect();
        assert_eq!(
            keys,
            vec!["s1|bbbb", "s1|cccc", "s1|aaaa"],
            "exact session, newest first, key breaks the tie"
        );
        assert!(store.load_session_lanes("nope").expect("lanes").is_empty());

        let conn = store.conn().expect("conn");
        let plan: Vec<String> = conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT key FROM lanes
                 WHERE session_id = ?1 ORDER BY updated_ms DESC, key",
            )
            .expect("prepare")
            .query_map(["s1"], |row| row.get::<_, String>(3))
            .expect("plan")
            .map(|detail| detail.expect("detail"))
            .collect();
        assert!(
            plan.iter().any(|step| step.contains("lanes_session_idx")),
            "the read uses the session index: {plan:?}"
        );
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
    fn display_session_summary_includes_only_explicit_children() {
        let store = mem_store();
        let mut parent = bare_row(1_000);
        parent.session_id = Some("parent".to_owned());
        parent.input = Some(3);
        store.record_request(&parent).expect("parent");

        let mut child = bare_row(2_000);
        child.session_id = Some("guardian:parent".to_owned());
        child.lineage = SessionLineage::codex_guardian("guardian:parent");
        child.input = Some(5);
        store.record_request(&child).expect("child");

        let mut unlinked = bare_row(3_000);
        unlinked.session_id = Some("unlinked-nearby".to_owned());
        unlinked.input = Some(7);
        store.record_request(&unlinked).expect("unlinked");

        let summary = store.display_session_summary("parent").expect("summary");
        assert_eq!(summary.requests, 2);
        assert_eq!(summary.input, Some(8));
        assert_eq!(summary.last_ts_ms, Some(2_000));
        assert_eq!(
            store
                .latest_display_session_row("parent")
                .expect("latest")
                .expect("child row")
                .session_id
                .as_deref(),
            Some("guardian:parent")
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
            pin_generation: 0,
            pin: None,
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
        let account = crate::test_support::account(&store, "anthropic_sub", "one@example.test");
        let five_h = Allowance {
            session_id: "ses-1".to_string(),
            account_number: account,
            meter: "5h".to_string(),
            reset_value: 1_769_100_000_000,
            release: Release::Overage,
        };
        store.record_allowance(&five_h).expect("record");
        store.record_allowance(&five_h).expect("record again");
        let seven_d = Allowance {
            session_id: "ses-1".to_string(),
            account_number: account,
            meter: "7d".to_string(),
            reset_value: 1_769_700_000_000,
            release: Release::Plan,
        };
        store.record_allowance(&seven_d).expect("record");
        assert_eq!(
            store.load_allowances().expect("allowances"),
            vec![five_h.clone(), seven_d.clone()],
            "same allowance twice is one row"
        );

        // A second release for the same window only widens it: plan over
        // overage stays overage, overage over plan becomes overage.
        store
            .record_allowance(&Allowance {
                release: Release::Plan,
                ..five_h.clone()
            })
            .expect("narrower");
        store
            .record_allowance(&Allowance {
                release: Release::Overage,
                ..seven_d.clone()
            })
            .expect("wider");
        assert_eq!(
            store.load_allowances().expect("allowances"),
            vec![
                five_h,
                Allowance {
                    release: Release::Overage,
                    ..seven_d
                }
            ],
        );

        // Pings: insert-only, windowed like the ledger.
        let ping = PingRecord {
            id: None,
            ts_ms: 3_000,
            exit_code: Some(0),
            duration_ms: Some(1_234),
            boundary_ms: Some(2_400),
            slot: Some("07:20".to_owned()),
            action: Some(PingAction::Ping),
            observed_ms: Some(2_400),
            verified: Some(true),
            assumed: Some(false),
            account_number: None,
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
            slot: None,
            action: None,
            observed_ms: None,
            verified: None,
            assumed: None,
            account_number: None,
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

        // Meters: per-provider slots (migration v3), overwritten on every
        // save, and one backend's snapshot never answers for another's.
        let meters = MetersSnapshot {
            updated_ms: 4_000,
            snapshot: json!({"5h": {"used": 42, "limit": 100}}),
        };
        store.save_meters("anthropic_sub", &meters).expect("save");
        assert_eq!(
            store.load_meters("anthropic_sub").expect("load"),
            Some(meters.clone())
        );
        let fresher = MetersSnapshot {
            updated_ms: 4_500,
            snapshot: json!({"5h": {"used": 50, "limit": 100}}),
        };
        store
            .save_meters("anthropic_sub", &fresher)
            .expect("save again");
        assert_eq!(
            store.load_meters("anthropic_sub").expect("load"),
            Some(fresher)
        );
        // A slot never written reads as absent, never as another's data.
        assert_eq!(store.load_meters("codex_sub").expect("load"), None);
        let codex = MetersSnapshot {
            updated_ms: 5_000,
            snapshot: json!({"primary": {"used_percent": 12.5}}),
        };
        store.save_meters("codex_sub", &codex).expect("save codex");
        assert_eq!(store.load_meters("codex_sub").expect("load"), Some(codex));
        assert_eq!(
            store
                .load_meters("anthropic_sub")
                .expect("load")
                .expect("present")
                .updated_ms,
            4_500,
            "the anthropic slot survived the codex save"
        );

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
            pin_generation: 0,
            pin: None,
        };

        // Fresh, within the age window: kept.
        store
            .upsert_lane(&lane("fresh", now - 1_000))
            .expect("upsert");
        // Exactly 30 days old: kept (the age window drops strictly older).
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

        // The cap keeps the NEWEST rows (sorted by `updated_ms`
        // descending and sliced), not the first-inserted.
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

    /// The newest measurement of one session: proxy rows and other
    /// sessions' rows never answer, and a session without a measurement
    /// is `None`.
    #[test]
    fn latest_session_row_is_the_session_s_newest_measurement() {
        let store = mem_store();
        let row = |ts_ms: i64, session: &str, model: &str| {
            let mut row = bare_row(ts_ms);
            row.session_id = Some(session.to_owned());
            row.requested_model = Some(model.to_owned());
            row
        };
        store
            .record_request(&row(1_000, "ses-a", "old"))
            .expect("record");
        store
            .record_request(&row(2_000, "ses-a", "new"))
            .expect("record");
        store
            .record_request(&row(3_000, "ses-b", "other"))
            .expect("record");
        let mut blocked = row(4_000, "ses-a", "blocked");
        blocked.kind = Some(RowKind::Blocked);
        store.record_request(&blocked).expect("record");
        let latest = store
            .latest_session_row("ses-a")
            .expect("read")
            .expect("a row");
        assert_eq!(latest.requested_model.as_deref(), Some("new"));
        assert_eq!(store.latest_session_row("ses-none").expect("read"), None);
    }

    #[test]
    fn allowance_prune_drops_ended_windows_only() {
        let store = mem_store();
        // Epoch milliseconds now; resets are epoch seconds.
        let now = 1_790_000_000_500;
        let now_s = now / 1000;
        let account = crate::test_support::account(&store, "anthropic_sub", "one@example.test");
        let allowance = |session: &str, meter: &str, reset_value: i64| Allowance {
            session_id: session.to_owned(),
            account_number: account,
            meter: meter.to_owned(),
            reset_value,
            release: Release::Overage,
        };
        // A window that ended an hour ago: dead.
        store
            .record_allowance(&allowance("ses-a", "5h", now_s - 3600))
            .expect("record");
        // Resetting this very second: the gate already reads it as expired
        // (`reset * 1000 <= now`), so the prune agrees.
        store
            .record_allowance(&allowance("ses-a", "7d", now_s))
            .expect("record");
        // One second ahead, and a week ahead: live.
        store
            .record_allowance(&allowance("ses-b", "5h", now_s + 1))
            .expect("record");
        store
            .record_allowance(&allowance("ses-b", "7d", now_s + 7 * 86_400))
            .expect("record");
        // A reset far beyond any window (a unit mix-up, say) is kept: the
        // prune only ever errs towards keeping.
        store
            .record_allowance(&allowance("ses-c", "5h", now * 1000))
            .expect("record");

        assert_eq!(store.prune_allowances(now).expect("prune"), 2);
        let left: Vec<(String, String)> = store
            .load_allowances()
            .expect("allowances")
            .into_iter()
            .map(|row| (row.session_id, row.meter))
            .collect();
        assert_eq!(
            left,
            vec![
                ("ses-b".to_owned(), "5h".to_owned()),
                ("ses-b".to_owned(), "7d".to_owned()),
                ("ses-c".to_owned(), "5h".to_owned()),
            ]
        );
        assert_eq!(store.prune_allowances(now).expect("prune again"), 0);

        // The per-session read sees only that session's rows.
        assert_eq!(
            store.load_session_allowances("ses-b").expect("ses-b"),
            vec![
                allowance("ses-b", "5h", now_s + 1),
                allowance("ses-b", "7d", now_s + 7 * 86_400),
            ]
        );
        assert_eq!(
            store.load_session_allowances("ses-a").expect("ses-a"),
            Vec::new()
        );
    }

    #[test]
    fn routing_state_starts_without_an_active_revision_and_revisions_round_trip() {
        let store = mem_store();
        let state = store.load_routing_policy_state().expect("state");
        assert_eq!(state.active_revision_id, None);
        assert_eq!(state.draft_generation, 0);
        assert_eq!(state.draft, None);
        assert_eq!(state.legacy_imported_ms, None);

        let id = revision(&store, "first", None);
        let loaded = store
            .load_policy_revision(id)
            .expect("load")
            .expect("revision");
        assert_eq!(loaded.description.as_deref(), Some("first"));
        assert_eq!(loaded.activation_scope, ActivationScope::NewLanes);
        assert_eq!(loaded.source, PolicyRevisionSource::Draft);
        assert_eq!(loaded.document, policy("first"));
        assert_eq!(
            store
                .load_routing_policy_state()
                .unwrap()
                .active_revision_id,
            None
        );

        let child = revision(&store, "child", Some(id));
        assert_eq!(store.load_policy_revisions().unwrap().len(), 2);
        assert_eq!(
            store
                .load_policy_revision(child)
                .unwrap()
                .unwrap()
                .parent_revision_id,
            Some(id)
        );
        let missing_parent = store.insert_policy_revision(&NewPolicyRevision {
            created_ms: 1,
            activated_ms: 1,
            description: None,
            activation_scope: ActivationScope::Immediate,
            forced_cache_transition: true,
            source: PolicyRevisionSource::Reactivation,
            compiler_origin: None,
            parent_revision_id: Some(999_999),
            document: &policy("missing-parent"),
        });
        assert!(matches!(missing_parent, Err(Error::Sqlite(_))));
    }

    #[test]
    fn revision_one_activation_is_atomic_and_idempotent() {
        let store = mem_store();
        let document = policy("revision one");
        let publication = PublicationKey::new(
            FrontendScope::Profile {
                profile: ProfileId::new("claude").unwrap(),
                protocol: FrontendProtocol::AnthropicMessages,
            },
            PublicationAdapter::new("claude_settings").unwrap(),
        );
        let publication = InitialPublication {
            key: publication,
            state: PublicationState::Applied,
        };
        let activated = store
            .activate_revision_one(&document, 200, 7, std::slice::from_ref(&publication))
            .expect("activate");
        let RevisionOneActivation::Activated {
            revision,
            state,
            publications,
        } = activated
        else {
            panic!("first call activates")
        };
        assert_eq!(revision.source, PolicyRevisionSource::Legacy);
        assert_eq!(state.active_revision_id, Some(revision.revision_id));
        assert_eq!(state.legacy_imported_ms, Some(200));
        assert_eq!(state.draft_generation, 1);
        assert_eq!(
            state.draft.as_ref().unwrap().base_revision_id,
            revision.revision_id
        );
        assert_eq!(state.draft.as_ref().unwrap().document, document);
        assert_eq!(publications.len(), 1);
        assert_eq!(publications[0].desired_revision_id, revision.revision_id);
        assert_eq!(publications[0].desired_inventory_generation, 7);
        assert_eq!(publications[0].state, PublicationState::Applied);
        assert_eq!(
            publications[0].applied_revision_id,
            Some(revision.revision_id)
        );
        assert_eq!(publications[0].applied_inventory_generation, Some(7));

        let other = policy("ignored after ownership");
        let restarted = store
            .activate_revision_one(&other, 300, 8, &[])
            .expect("load active");
        let RevisionOneActivation::AlreadyActive {
            revision: loaded,
            state: loaded_state,
            publications: loaded_publications,
        } = restarted
        else {
            panic!("restart reuses active revision")
        };
        assert_eq!(loaded.revision_id, revision.revision_id);
        assert_eq!(loaded.document, document);
        assert_eq!(loaded_state, state);
        assert_eq!(loaded_publications, publications);
        assert_eq!(store.load_policy_revisions().unwrap().len(), 1);
    }

    #[test]
    fn revision_one_activation_rolls_back_on_publication_failure() {
        let store = mem_store();
        let document = policy("revision one");
        let duplicate = PublicationKey::new(
            FrontendScope::Protocol {
                protocol: FrontendProtocol::AnthropicMessages,
            },
            PublicationAdapter::new("claude_settings").unwrap(),
        );
        let conn = store.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_revision_one_publication BEFORE INSERT ON routing_publications
             BEGIN SELECT RAISE(ABORT, 'injected publication failure'); END;",
        )
        .unwrap();
        drop(conn);

        assert!(
            store
                .activate_revision_one(
                    &document,
                    200,
                    0,
                    &[InitialPublication {
                        key: duplicate,
                        state: PublicationState::Pending,
                    }],
                )
                .is_err()
        );
        let state = store.load_routing_policy_state().unwrap();
        assert_eq!(state.active_revision_id, None);
        assert_eq!(state.legacy_imported_ms, None);
        assert_eq!(state.draft_generation, 0);
        assert_eq!(store.load_policy_revisions().unwrap(), vec![]);
        assert_eq!(store.load_publications().unwrap(), vec![]);

        store
            .conn
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_revision_one_publication")
            .unwrap();
        let retried = store
            .activate_revision_one(&document, 201, 0, &[])
            .expect("retry activates");
        assert!(matches!(retried, RevisionOneActivation::Activated { .. }));
        assert_eq!(store.load_policy_revisions().unwrap().len(), 1);
    }

    #[test]
    fn live_activation_is_atomic_and_reactivation_preserves_compiler_origin() {
        let store = mem_store();
        let legacy = policy("legacy routing");
        let RevisionOneActivation::Activated {
            revision: first, ..
        } = store
            .activate_revision_one(&legacy, 100, 4, &[])
            .expect("activate revision one")
        else {
            panic!("revision one is activated")
        };
        let publication = InitialPublication {
            key: PublicationKey::new(
                FrontendScope::Protocol {
                    protocol: FrontendProtocol::OpenaiChat,
                },
                PublicationAdapter::new("dynamic_chat_models").unwrap(),
            ),
            state: PublicationState::Pending,
        };
        let activated = store
            .activate_policy_revision(&ActivatePolicyRevision {
                created_ms: 200,
                expected_draft_generation: None,
                description: Some("restore the initial routing"),
                activation_scope: ActivationScope::Immediate,
                forced_cache_transition: false,
                source: PolicyRevisionSource::Reactivation,
                compiler_origin: PolicyCompilerOrigin::GeneratedRevisionOne,
                parent_revision_id: Some(first.revision_id),
                expected_active_revision_id: first.revision_id,
                document: &first.document,
                desired_inventory_generation: 9,
                publications: std::slice::from_ref(&publication),
            })
            .expect("reactivate revision");
        assert_ne!(activated.revision.revision_id, first.revision_id);
        assert_eq!(
            activated.revision.parent_revision_id,
            Some(first.revision_id),
            "restoration creates an auditable forward revision"
        );
        assert_eq!(
            activated.revision.source,
            PolicyRevisionSource::Reactivation
        );
        assert_eq!(
            activated.revision.compiler_origin,
            PolicyCompilerOrigin::GeneratedRevisionOne
        );
        assert_eq!(
            store
                .load_routing_policy_state()
                .unwrap()
                .active_revision_id,
            Some(activated.revision.revision_id)
        );
        assert_eq!(
            activated.publications[0].desired_revision_id,
            activated.revision.revision_id
        );
        assert_eq!(activated.publications[0].desired_inventory_generation, 9);
        assert_eq!(
            store
                .load_policy_revision(first.revision_id)
                .unwrap()
                .unwrap(),
            first
        );
    }

    #[test]
    fn live_activation_rolls_back_the_revision_when_publication_handoff_fails() {
        let store = mem_store();
        let document = policy("initial routing");
        store
            .activate_revision_one(&document, 100, 0, &[])
            .expect("activate revision one");
        let conn = store.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_live_publication BEFORE INSERT ON routing_publications
             BEGIN SELECT RAISE(ABORT, 'injected publication failure'); END;",
        )
        .unwrap();
        drop(conn);
        let before = store.load_routing_policy_state().unwrap();
        assert!(
            store
                .activate_policy_revision(&ActivatePolicyRevision {
                    created_ms: 200,
                    expected_draft_generation: None,
                    description: None,
                    activation_scope: ActivationScope::Immediate,
                    forced_cache_transition: false,
                    source: PolicyRevisionSource::Draft,
                    compiler_origin: PolicyCompilerOrigin::Authored,
                    parent_revision_id: None,
                    expected_active_revision_id: 1,
                    document: &policy("new routing"),
                    desired_inventory_generation: 7,
                    publications: &[InitialPublication {
                        key: PublicationKey::new(
                            FrontendScope::Protocol {
                                protocol: FrontendProtocol::OpenaiChat,
                            },
                            PublicationAdapter::new("dynamic_chat_models").unwrap(),
                        ),
                        state: PublicationState::Pending,
                    }],
                })
                .is_err()
        );
        assert_eq!(store.load_routing_policy_state().unwrap(), before);
        assert_eq!(store.load_policy_revisions().unwrap().len(), 1);
    }

    #[test]
    fn policy_draft_cas_updates_conflicts_and_clears() {
        let store = mem_store();
        let base = revision(&store, "base", None);
        let first = policy("draft one");
        assert!(matches!(
            store
                .compare_and_swap_policy_draft(0, base, &first, 200)
                .unwrap(),
            DraftCasResult::Updated {
                generation: 1,
                draft: Some(_)
            }
        ));
        let second = policy("draft two");
        let stale = store
            .compare_and_swap_policy_draft(0, base, &second, 201)
            .unwrap();
        assert!(
            matches!(stale, DraftCasResult::Conflict { current } if current.draft_generation == 1)
        );
        let state = store.load_routing_policy_state().unwrap();
        assert_eq!(state.active_revision_id, None);
        assert_eq!(state.draft.as_ref().unwrap().document, first);
        assert!(matches!(
            store.clear_policy_draft(1).unwrap(),
            DraftCasResult::Updated {
                generation: 2,
                draft: None
            }
        ));
        assert_eq!(store.load_routing_policy_state().unwrap().draft, None);
    }

    #[test]
    fn publication_scope_keys_reject_noncanonical_stored_values() {
        let store = mem_store();
        let revision_id = revision(&store, "publication scope", None);
        for scope in [
            "protocol:ANTHROPIC_MESSAGES",
            "protocol:unknown",
            "profile:Claude:anthropic_messages",
            "profile:claude:anthropic",
        ] {
            {
                let conn = store.conn.lock().unwrap();
                conn.execute(
                    "INSERT INTO routing_publications
                     (scope_key, adapter, desired_revision_id,
                      desired_inventory_generation, state, updated_ms)
                     VALUES (?1, 'claude_settings', ?2, 0, 'pending', 1)",
                    rusqlite::params![scope, revision_id],
                )
                .unwrap();
            }
            assert!(matches!(
                store.load_publications(),
                Err(Error::InvalidRoutingState(_))
            ));
            store
                .conn
                .lock()
                .unwrap()
                .execute("DELETE FROM routing_publications", [])
                .unwrap();
        }
    }

    #[test]
    fn restating_the_applied_pair_keeps_a_publication_applied() {
        let store = mem_store();
        let revision_id = revision(&store, "restated publication", None);
        let key = PublicationKey::new(
            FrontendScope::Protocol {
                protocol: FrontendProtocol::OpenaiChat,
            },
            PublicationAdapter::new("claude_settings").unwrap(),
        );
        let target = |state| InitialPublication {
            key: key.clone(),
            state,
        };
        store
            .set_publications_desired(&[target(PublicationState::Applied)], revision_id, 4, 100)
            .unwrap();
        let restated = store
            .set_publications_desired(&[target(PublicationState::Pending)], revision_id, 4, 200)
            .unwrap();
        assert_eq!(restated[0].state, PublicationState::Applied);
        let moved = store
            .set_publications_desired(&[target(PublicationState::Pending)], revision_id, 5, 300)
            .unwrap();
        assert_eq!(
            moved[0].state,
            PublicationState::Pending,
            "a new inventory generation is something left to apply"
        );
    }

    #[test]
    fn publication_desired_handoff_updates_all_adapters_atomically() {
        let store = mem_store();
        let revision_id = revision(&store, "publication handoff", None);
        let publication = |adapter: &str, state| InitialPublication {
            key: PublicationKey::new(
                FrontendScope::Protocol {
                    protocol: FrontendProtocol::OpenaiChat,
                },
                PublicationAdapter::new(adapter).unwrap(),
            ),
            state,
        };
        let rows = store
            .set_publications_desired(
                &[
                    publication("dynamic_chat_models", PublicationState::Applied),
                    publication("claude_settings", PublicationState::Pending),
                ],
                revision_id,
                9,
                100,
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        let dynamic = rows
            .iter()
            .find(|row| row.key.adapter().as_str() == "dynamic_chat_models")
            .unwrap();
        assert_eq!(dynamic.state, PublicationState::Applied);
        assert_eq!(dynamic.applied_revision_id, Some(revision_id));
        assert_eq!(dynamic.applied_inventory_generation, Some(9));
        let claude = rows
            .iter()
            .find(|row| row.key.adapter().as_str() == "claude_settings")
            .unwrap();
        assert_eq!(claude.state, PublicationState::Pending);
        assert_eq!(claude.applied_revision_id, None);

        store
            .execute_for_test(
                "CREATE TRIGGER fail_publication_handoff BEFORE UPDATE ON routing_publications
                 WHEN NEW.adapter = 'claude_settings'
                 BEGIN SELECT RAISE(ABORT, 'injected handoff failure'); END;",
            )
            .unwrap();
        assert!(
            store
                .set_publications_desired(
                    &[
                        publication("dynamic_chat_models", PublicationState::Applied),
                        publication("claude_settings", PublicationState::Pending),
                    ],
                    revision_id,
                    10,
                    101,
                )
                .is_err()
        );
        assert!(
            store
                .load_publications()
                .unwrap()
                .iter()
                .all(|row| row.desired_inventory_generation == 9),
            "a failed adapter update rolls the entire desired handoff back"
        );
    }

    #[test]
    fn publication_results_are_cas_guarded_and_state_checked() {
        let store = mem_store();
        let first = revision(&store, "first", None);
        let second = revision(&store, "second", Some(first));
        let key = PublicationKey::new(
            FrontendScope::Profile {
                profile: ProfileId::new("claude").unwrap(),
                protocol: FrontendProtocol::AnthropicMessages,
            },
            PublicationAdapter::new("claude_settings").unwrap(),
        );
        store.set_publication_desired(&key, first, 4, 100).unwrap();
        assert_eq!(
            store.load_publication(&key).unwrap().unwrap().state,
            PublicationState::Pending
        );
        let first_identities = BTreeSet::from([
            "openrouter/anthropic/claude-sonnet".to_owned(),
            "route/virtual-model".to_owned(),
        ]);
        assert_eq!(
            store
                .compare_and_swap_publication_result(
                    &key,
                    first,
                    4,
                    &PublicationResultUpdate {
                        state: PublicationState::Applied,
                        applied_identities: Some(first_identities.clone()),
                        detail_code: None,
                        updated_ms: 101,
                    },
                )
                .unwrap(),
            PublicationCasResult::Updated,
        );
        store.set_publication_desired(&key, second, 5, 102).unwrap();
        let pending = store.load_publication(&key).unwrap().unwrap();
        assert_eq!(pending.applied_identities, Some(first_identities.clone()));
        let write_failed = StableId::new("write_failed").unwrap();
        let stale = store
            .compare_and_swap_publication_result(
                &key,
                first,
                4,
                &PublicationResultUpdate {
                    state: PublicationState::Failed,
                    applied_identities: None,
                    detail_code: Some(write_failed.clone()),
                    updated_ms: 103,
                },
            )
            .unwrap();
        assert!(
            matches!(stale, PublicationCasResult::Conflict { current: Some(row) }
            if row.desired_revision_id == second && row.state == PublicationState::Pending)
        );
        let invalid = store.compare_and_swap_publication_result(
            &key,
            second,
            5,
            &PublicationResultUpdate {
                state: PublicationState::Failed,
                applied_identities: Some(first_identities.clone()),
                detail_code: Some(write_failed.clone()),
                updated_ms: 104,
            },
        );
        assert!(matches!(invalid, Err(Error::InvalidRoutingState(_))));
        assert_eq!(
            store
                .compare_and_swap_publication_result(
                    &key,
                    second,
                    5,
                    &PublicationResultUpdate {
                        state: PublicationState::Failed,
                        applied_identities: None,
                        detail_code: Some(write_failed.clone()),
                        updated_ms: 105,
                    },
                )
                .unwrap(),
            PublicationCasResult::Updated,
        );
        let failed = store.load_publication(&key).unwrap().unwrap();
        assert_eq!(failed.detail_code, Some(write_failed));
        assert_eq!(failed.applied_identities, Some(first_identities));
    }

    #[test]
    fn applied_publications_require_canonical_exact_identities_and_reject_late_failures() {
        let store = mem_store();
        let revision_id = revision(&store, "publication identities", None);
        let key = PublicationKey::new(
            FrontendScope::Protocol {
                protocol: FrontendProtocol::AnthropicMessages,
            },
            PublicationAdapter::new("claude_settings").unwrap(),
        );
        store
            .set_publication_desired(&key, revision_id, 1, 1)
            .unwrap();
        let no_identities = store.compare_and_swap_publication_result(
            &key,
            revision_id,
            1,
            &PublicationResultUpdate {
                state: PublicationState::Applied,
                applied_identities: None,
                detail_code: None,
                updated_ms: 2,
            },
        );
        assert!(matches!(no_identities, Err(Error::InvalidRoutingState(_))));

        let identities = BTreeSet::new();
        assert_eq!(
            store
                .compare_and_swap_publication_result(
                    &key,
                    revision_id,
                    1,
                    &PublicationResultUpdate {
                        state: PublicationState::Applied,
                        applied_identities: Some(identities),
                        detail_code: None,
                        updated_ms: 3,
                    },
                )
                .unwrap(),
            PublicationCasResult::Updated
        );
        let late = store
            .compare_and_swap_publication_result(
                &key,
                revision_id,
                1,
                &PublicationResultUpdate {
                    state: PublicationState::Failed,
                    applied_identities: None,
                    detail_code: Some(StableId::new("write_failed").unwrap()),
                    updated_ms: 4,
                },
            )
            .unwrap();
        assert!(matches!(late, PublicationCasResult::Conflict { .. }));

        store
            .execute_for_test(
                "UPDATE routing_publications
                 SET applied_identities_json = '[\"z\",\"a\"]'
                 WHERE scope_key = 'protocol:anthropic_messages'",
            )
            .unwrap();
        assert!(matches!(
            store.load_publication(&key),
            Err(Error::InvalidRoutingState(_))
        ));

        let large = (0..crate::policy::MAX_COLLECTION_ITEMS)
            .map(|index| {
                format!(
                    "{index:04}-{}",
                    "x".repeat(crate::policy::MAX_STRING_BYTES - 5)
                )
            })
            .collect::<BTreeSet<_>>();
        assert!(matches!(
            store.compare_and_swap_publication_result(
                &key,
                revision_id,
                1,
                &PublicationResultUpdate {
                    state: PublicationState::Applied,
                    applied_identities: Some(large),
                    detail_code: None,
                    updated_ms: 5,
                },
            ),
            Err(Error::InvalidRoutingState(_))
        ));
    }

    #[test]
    fn binding_learning_merge_unions_maxes_and_promotes() {
        let store = mem_store();
        let binding = BindingId::new("openrouter.openrouter_messages").unwrap();
        let update =
            |days: &[&str], max_prompt, explicitly_promoted, updated_ms| BindingLearningUpdate {
                binding_id: binding.clone(),
                model_id: "model-x".to_owned(),
                days: days
                    .iter()
                    .map(|day| (*day).to_owned())
                    .collect::<BTreeSet<_>>(),
                max_prompt,
                explicitly_promoted,
                updated_ms,
            };
        store
            .merge_binding_learning(&update(&["2026-10-02"], Some(100), false, 20))
            .unwrap();
        let merged = store
            .merge_binding_learning(&update(&["2026-10-01"], Some(50), true, 10))
            .unwrap();
        assert_eq!(
            merged.days.into_iter().collect::<Vec<_>>(),
            ["2026-10-01", "2026-10-02"]
        );
        assert_eq!(merged.max_prompt, Some(100));
        assert!(merged.explicitly_promoted);
        assert_eq!(merged.updated_ms, 20);
        let later = store
            .merge_binding_learning(&update(&[], Some(300), false, 30))
            .unwrap();
        assert_eq!(later.max_prompt, Some(300));
        assert_eq!(store.load_binding_learnings(&binding).unwrap(), vec![later]);
    }

    #[test]
    fn binding_promotion_is_existing_only_and_model_lookup_is_binding_sorted() {
        let store = mem_store();
        let first = BindingId::new("anthropic_api.anthropic_messages").unwrap();
        let second = BindingId::new("anthropic_sub.anthropic_messages").unwrap();
        for binding in [&second, &first] {
            store
                .merge_binding_learning(&BindingLearningUpdate {
                    binding_id: binding.clone(),
                    model_id: "same-model".to_owned(),
                    days: ["2026-10-01".to_owned()].into_iter().collect(),
                    max_prompt: Some(100),
                    explicitly_promoted: false,
                    updated_ms: 10,
                })
                .unwrap();
        }

        assert_eq!(
            store
                .load_binding_learnings_for_model("same-model")
                .unwrap()
                .into_iter()
                .map(|row| row.binding_id)
                .collect::<Vec<_>>(),
            [first.clone(), second.clone()],
        );
        let promoted = store
            .promote_binding_learning(&first, "same-model", 20)
            .unwrap()
            .unwrap();
        assert!(promoted.explicitly_promoted);
        assert_eq!(
            promoted.days,
            ["2026-10-01".to_owned()].into_iter().collect()
        );
        assert_eq!(promoted.max_prompt, Some(100));
        assert_eq!(promoted.updated_ms, 20);
        assert!(
            !store
                .load_binding_learning(&second, "same-model")
                .unwrap()
                .unwrap()
                .explicitly_promoted
        );
        assert_eq!(
            store
                .promote_binding_learning(&first, "never-served", 30)
                .unwrap(),
            None,
        );
        assert_eq!(
            store.load_binding_learning(&first, "never-served").unwrap(),
            None,
        );
    }

    #[test]
    fn lane_pin_cas_and_ordinary_upsert_preserve_pin_identity() {
        let store = mem_store();
        let revision_id = revision(&store, "pin", None);
        let lane = unpinned_lane("session|tools");
        store.upsert_lane(&lane).unwrap();
        let account = crate::test_support::account(&store, "openrouter", "pin@example.test");
        let pin = LanePin {
            policy_revision_id: revision_id,
            policy_route_id: RouteId::new("route").unwrap(),
            route_fingerprint: "11".repeat(32).parse::<RouteFingerprint>().unwrap(),
            candidate_id: CandidateId::new("candidate").unwrap(),
            binding_id: BindingId::new("openrouter.openrouter_messages").unwrap(),
            account_number: Some(account),
            baseline_model: "base".to_owned(),
            effective_model: "effective".to_owned(),
        };
        assert_eq!(
            store
                .compare_and_swap_lane_pin(&lane.key, 0, None, Some(&pin))
                .unwrap(),
            LanePinCasResult::Updated { generation: 1 },
        );
        assert_eq!(
            store
                .compare_and_swap_lane_pin(&lane.key, 0, None, Some(&pin))
                .unwrap(),
            LanePinCasResult::Conflict,
        );
        let mut ordinary = lane.clone();
        ordinary.updated_ms = 99;
        store.upsert_lane(&ordinary).unwrap();
        let loaded = store.load_lane(&lane.key).unwrap().unwrap();
        assert_eq!(loaded.updated_ms, 99);
        assert_eq!(loaded.pin_generation, 1);
        assert_eq!(loaded.pin.as_ref(), Some(&pin));
        assert_eq!(
            store
                .compare_and_swap_lane_pin(&lane.key, 1, None, None)
                .unwrap(),
            LanePinCasResult::Conflict,
        );
        assert_eq!(
            store
                .compare_and_swap_lane_pin(&lane.key, 1, Some(&pin), None)
                .unwrap(),
            LanePinCasResult::Updated { generation: 2 },
        );
    }

    #[test]
    fn insert_lane_if_absent_preserves_existing_evidence_and_pin() {
        let store = mem_store();
        let revision_id = revision(&store, "insert-if-absent", None);
        let mut existing = unpinned_lane("session|existing");
        store.upsert_lane(&existing).unwrap();
        let pin = LanePin {
            policy_revision_id: revision_id,
            policy_route_id: RouteId::new("route").unwrap(),
            route_fingerprint: "22".repeat(32).parse::<RouteFingerprint>().unwrap(),
            candidate_id: CandidateId::new("candidate").unwrap(),
            binding_id: BindingId::new("openrouter.openrouter_messages").unwrap(),
            account_number: None,
            baseline_model: "base".to_owned(),
            effective_model: "effective".to_owned(),
        };
        store
            .compare_and_swap_lane_pin(&existing.key, 0, None, Some(&pin))
            .unwrap();

        let placeholder = Lane {
            key: existing.key.clone(),
            session_id: existing.session_id.clone(),
            tools_hash: existing.tools_hash.clone(),
            updated_ms: 99,
            prompt_tokens: None,
            ttl: None,
            ping: Some(true),
            noticed_at: None,
            forced_from: None,
            forced_to: None,
            pin_generation: 0,
            pin: None,
        };
        store.insert_lane_if_absent(&placeholder).unwrap();

        let loaded = store.load_lane(&existing.key).unwrap().unwrap();
        assert_eq!(loaded.updated_ms, existing.updated_ms);
        assert_eq!(loaded.prompt_tokens, existing.prompt_tokens);
        assert_eq!(loaded.ttl, existing.ttl);
        assert_eq!(loaded.ping, existing.ping);
        assert_eq!(loaded.pin_generation, 1);
        assert_eq!(loaded.pin, Some(pin));

        existing.key = "session|absent".to_owned();
        store.insert_lane_if_absent(&existing).unwrap();
        assert_eq!(store.load_lane(&existing.key).unwrap(), Some(existing));
    }

    #[test]
    fn concurrent_first_lane_pin_has_one_winner() {
        let dir = test_dir("lane-pin-race");
        let db = dir.join("toker.db");
        let setup = Store::open(&db).unwrap();
        let revision_id = revision(&setup, "race", None);
        let lane = unpinned_lane("session|race");
        setup.upsert_lane(&lane).unwrap();
        drop(setup);

        let first = Store::open(&db).unwrap();
        let second = Store::open(&db).unwrap();
        let first_snapshot = first.load_lane(&lane.key).unwrap().unwrap();
        let second_snapshot = second.load_lane(&lane.key).unwrap().unwrap();
        let pin = LanePin {
            policy_revision_id: revision_id,
            policy_route_id: RouteId::new("route").unwrap(),
            route_fingerprint: "22".repeat(32).parse::<RouteFingerprint>().unwrap(),
            candidate_id: CandidateId::new("candidate").unwrap(),
            binding_id: BindingId::new("openrouter.openrouter_messages").unwrap(),
            account_number: None,
            baseline_model: "base".to_owned(),
            effective_model: "effective".to_owned(),
        };
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let first_barrier = barrier.clone();
        let key = lane.key.clone();
        let first_pin = pin.clone();
        let first_thread = std::thread::spawn(move || {
            first_barrier.wait();
            first
                .compare_and_swap_lane_pin(
                    &key,
                    first_snapshot.pin_generation,
                    first_snapshot.pin.as_ref(),
                    Some(&first_pin),
                )
                .unwrap()
        });
        let second_barrier = barrier;
        let key = lane.key.clone();
        let second_pin = pin.clone();
        let second_thread = std::thread::spawn(move || {
            second_barrier.wait();
            second
                .compare_and_swap_lane_pin(
                    &key,
                    second_snapshot.pin_generation,
                    second_snapshot.pin.as_ref(),
                    Some(&second_pin),
                )
                .unwrap()
        });
        let results = [first_thread.join().unwrap(), second_thread.join().unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, LanePinCasResult::Updated { generation: 1 }))
                .count(),
            1,
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == LanePinCasResult::Conflict)
                .count(),
            1,
        );
        let loaded = Store::open(&db)
            .unwrap()
            .load_lane(&lane.key)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.pin_generation, 1);
        assert_eq!(loaded.pin, Some(pin));
    }

    #[test]
    fn stale_old_revision_completion_cannot_restore_its_pin() {
        let store = mem_store();
        let old_revision = revision(&store, "old", None);
        let new_revision = revision(&store, "new", Some(old_revision));
        let lane = unpinned_lane("session|activation");
        store.upsert_lane(&lane).unwrap();
        let pin = |revision, route: &str, fingerprint: &str| LanePin {
            policy_revision_id: revision,
            policy_route_id: RouteId::new(route).unwrap(),
            route_fingerprint: fingerprint.parse::<RouteFingerprint>().unwrap(),
            candidate_id: CandidateId::new("candidate").unwrap(),
            binding_id: BindingId::new("openrouter.openrouter_messages").unwrap(),
            account_number: None,
            baseline_model: "base".to_owned(),
            effective_model: "effective".to_owned(),
        };
        let old_pin = pin(old_revision, "old-route", &"33".repeat(32));
        assert_eq!(
            store
                .compare_and_swap_lane_pin(&lane.key, 0, None, Some(&old_pin))
                .unwrap(),
            LanePinCasResult::Updated { generation: 1 },
        );
        let stale = store.load_lane(&lane.key).unwrap().unwrap();
        let new_pin = pin(new_revision, "new-route", &"44".repeat(32));
        assert_eq!(
            store
                .compare_and_swap_lane_pin(&lane.key, 1, Some(&old_pin), Some(&new_pin))
                .unwrap(),
            LanePinCasResult::Updated { generation: 2 },
        );
        assert_eq!(
            store
                .compare_and_swap_lane_pin(
                    &lane.key,
                    stale.pin_generation,
                    stale.pin.as_ref(),
                    Some(&old_pin),
                )
                .unwrap(),
            LanePinCasResult::Conflict,
        );
        let loaded = store.load_lane(&lane.key).unwrap().unwrap();
        assert_eq!(loaded.pin_generation, 2);
        assert_eq!(loaded.pin, Some(new_pin));
    }

    #[test]
    fn routing_reads_work_read_only_and_writes_fail_through_sqlite() {
        let dir = test_dir("routing-read-only");
        let db = dir.join("toker.db");
        let store = Store::open(&db).unwrap();
        let id = revision(&store, "read only", None);
        drop(store);
        let store = Store::open_read_only(&db).unwrap();
        assert_eq!(
            store
                .load_policy_revision(id)
                .unwrap()
                .unwrap()
                .description
                .as_deref(),
            Some("read only")
        );
        assert_eq!(
            store
                .load_routing_policy_state()
                .unwrap()
                .active_revision_id,
            None
        );
        let write = store.set_publication_desired(
            &PublicationKey::new(
                FrontendScope::Protocol {
                    protocol: FrontendProtocol::AnthropicMessages,
                },
                PublicationAdapter::new("claude_settings").unwrap(),
            ),
            id,
            0,
            1,
        );
        assert!(matches!(write, Err(Error::Sqlite(_))));
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

    #[test]
    fn migration_v3_moves_the_single_meter_slot_to_the_anthropic_sub_row() {
        let dir = test_dir("v2-upgrade");
        let db = dir.join("toker.db");
        // A v2-shaped database: the first two migrations applied, with a
        // single-slot meters_state row (the only shape a v2 proxy could
        // write — the anthropic sub was its only meter source).
        std::fs::create_dir_all(&dir).expect("create the db parent dir");
        {
            let conn = Connection::open(&db).expect("open raw v2 db");
            for script in &schema::MIGRATIONS[..2] {
                conn.execute_batch(script).expect("apply v1+v2 migrations");
            }
            conn.pragma_update(None, "user_version", 2)
                .expect("stamp v2");
            conn.execute(
                "INSERT INTO meters_state (id, updated_ms, snapshot) VALUES (1, 12345, \
                 '{\"util5h\":0.42,\"reset5h\":1769500800}')",
                [],
            )
            .expect("insert a v2 meters row");
        }

        // Opening with current code migrates cleanly: user_version reaches
        // the head, the old single slot survives as the anthropic_sub row
        // (its only writer), and a codex save lands beside it without
        // touching it.
        let store = Store::open(&db).expect("v2 db migrates");
        assert_eq!(user_version(&store), schema::MIGRATIONS.len() as i64);
        let meters = store
            .load_meters("anthropic_sub")
            .expect("load")
            .expect("the v2 slot became the anthropic_sub row");
        assert_eq!(meters.updated_ms, 12_345);
        assert_eq!(
            meters.snapshot,
            json!({"util5h": 0.42, "reset5h": 1769500800}),
            "imported/old data survives the migration whole"
        );

        let codex = MetersSnapshot {
            updated_ms: 99_000,
            snapshot: json!({"primary": {"used_percent": 80}}),
        };
        store.save_meters("codex_sub", &codex).expect("codex slot");
        assert_eq!(store.load_meters("codex_sub").expect("load"), Some(codex));
        assert_eq!(
            store
                .load_meters("anthropic_sub")
                .expect("load")
                .expect("still present")
                .updated_ms,
            12_345,
            "the codex save never overwrites the anthropic slot"
        );

        // Reopen: at the head, no re-run, both slots persist.
        drop(store);
        let reopened = Store::open(&db).expect("reopen");
        assert_eq!(user_version(&reopened), schema::MIGRATIONS.len() as i64);
        assert!(
            reopened
                .load_meters("anthropic_sub")
                .expect("load")
                .is_some()
        );
        assert!(reopened.load_meters("codex_sub").expect("load").is_some());
    }
    #[test]
    fn migration_v8_hardens_attempt_sequences_and_policy_references() {
        fn raw_insert(
            conn: &Connection,
            ts: i64,
            group: Option<&str>,
            index: Option<i64>,
            revision: Option<i64>,
        ) -> rusqlite::Result<usize> {
            let terminal = group.is_some() && index.is_none();
            conn.execute(
                "INSERT INTO requests
                 (ts_ms, kind, attempt_group_id, attempt_index, policy_revision_id,
                  routing_trace, fallback_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                         CASE WHEN ?4 IS NULL THEN NULL
                              WHEN ?4 = 0 THEN 'initial'
                              ELSE 'thinking_negotiation' END)",
                rusqlite::params![
                    ts,
                    terminal.then_some("route-unavailable"),
                    group,
                    index,
                    revision,
                    terminal.then_some(r#"{"version":1,"events":[]}"#),
                ],
            )
        }

        let exercise = |store: &Store| {
            let revision_id = revision(store, "attempts", None);
            let conn = store.conn.lock().unwrap();
            let preflight = "00000000000000000000000000000001";
            raw_insert(&conn, 1, Some(preflight), None, None).unwrap();
            assert!(raw_insert(&conn, 2, Some(preflight), None, None).is_err());
            assert!(raw_insert(&conn, 3, Some(preflight), Some(0), None).is_err());

            let mixed = "00000000000000000000000000000002";
            raw_insert(&conn, 4, Some(mixed), Some(0), None).unwrap();
            assert!(raw_insert(&conn, 5, Some(mixed), None, None).is_err());

            let first_gap = "00000000000000000000000000000003";
            assert!(raw_insert(&conn, 6, Some(first_gap), Some(1), None).is_err());

            let gap = "00000000000000000000000000000004";
            raw_insert(&conn, 7, Some(gap), Some(0), None).unwrap();
            assert!(raw_insert(&conn, 8, Some(gap), Some(2), None).is_err());
            raw_insert(&conn, 9, Some(gap), Some(1), None).unwrap();

            assert!(raw_insert(&conn, 10, None, None, Some(999_999)).is_err());
            raw_insert(&conn, 11, None, None, Some(revision_id)).unwrap();
        };

        let fresh = mem_store();
        exercise(&fresh);

        let dir = test_dir("v7-upgrade");
        let db = dir.join("toker.db");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            conn.pragma_update(None, "foreign_keys", true).unwrap();
            for script in &schema::MIGRATIONS[..7] {
                conn.execute_batch(script).unwrap();
            }
            conn.pragma_update(None, "user_version", 7).unwrap();
        }
        let migrated = Store::open(&db).unwrap();
        assert_eq!(user_version(&migrated), schema::MIGRATIONS.len() as i64);
        exercise(&migrated);
    }

    #[test]
    fn migration_v6_to_head_preserves_old_null_defaults() {
        let dir = test_dir("v6-upgrade");
        let db = dir.join("toker.db");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            for script in &schema::MIGRATIONS[..6] {
                conn.execute_batch(script).unwrap();
            }
            conn.pragma_update(None, "user_version", 6).unwrap();
            conn.execute("INSERT INTO requests (ts_ms) VALUES (123)", [])
                .unwrap();
            conn.execute(
                "INSERT INTO lanes
                 (key, session_id, tools_hash, updated_ms, prompt_tokens, ttl, ping,
                  noticed_at, forced_from, forced_to)
                 VALUES ('old|tools', 'old', 'tools', 123, 7, NULL, NULL, NULL, NULL, NULL)",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(user_version(&store), schema::MIGRATIONS.len() as i64);
        assert_eq!(
            store
                .load_routing_policy_state()
                .unwrap()
                .active_revision_id,
            None
        );
        let lane = store.load_lane("old|tools").unwrap().unwrap();
        assert_eq!(lane.pin_generation, 0);
        assert_eq!(lane.pin, None);
        let row = store.requests_since(0, 10).unwrap().pop().unwrap();
        assert_eq!(row.policy_routing, None);
        assert_eq!(row.lineage, None);
    }

    #[test]
    fn resolver_trace_conversion_is_exhaustive_versioned_and_bounded() {
        let candidate = CandidateEvidence {
            candidate_id: CandidateId::new("candidate").unwrap(),
            crossed_from: None,
            binding_id: BindingId::new("openrouter.openrouter_messages").unwrap(),
            model: Some("model".to_owned()),
            result: Err(SkipReason::CredentialUnavailable),
        };
        let trace = StoredRoutingTrace::from_resolver(&[
            TraceEvent::Lookup(RouteProvenance::ProfilePresentation(
                PresentationId::new("presented").unwrap(),
            )),
            TraceEvent::Candidate(candidate),
            TraceEvent::Upgrade {
                from: "old".to_owned(),
                to: "new".to_owned(),
            },
            TraceEvent::Alias {
                from: "new".to_owned(),
                to: "effective".to_owned(),
            },
        ])
        .unwrap();
        assert_eq!(trace.version, 2);
        assert_eq!(trace.events.len(), 4);
        let crossed = StoredRoutingTrace::from_resolver(&[
            TraceEvent::Candidate(CandidateEvidence {
                candidate_id: CandidateId::new("first").unwrap(),
                crossed_from: None,
                binding_id: BindingId::new("openrouter.openrouter_messages").unwrap(),
                model: Some("one".to_owned()),
                result: Err(SkipReason::Disabled),
            }),
            TraceEvent::Candidate(CandidateEvidence {
                candidate_id: CandidateId::new("second").unwrap(),
                crossed_from: Some(CandidateId::new("first").unwrap()),
                binding_id: BindingId::new("openrouter.openrouter_messages").unwrap(),
                model: Some("two".to_owned()),
                result: Ok(()),
            }),
        ])
        .unwrap();
        assert!(matches!(
            &crossed.events[1],
            StoredRoutingTraceEvent::Candidate {
                candidate_id,
                crossed_from: Some(source),
                ..
            } if candidate_id.as_str() == "second" && source.as_str() == "first"
        ));
        let open = StoredRoutingTrace::from_resolver(&[
            TraceEvent::Lookup(RouteProvenance::ProtocolDefault),
            TraceEvent::Candidate(CandidateEvidence {
                candidate_id: CandidateId::new("candidate").unwrap(),
                crossed_from: None,
                binding_id: BindingId::new("openrouter.openrouter_messages").unwrap(),
                model: Some("effective".to_owned()),
                result: Err(SkipReason::HealthCircuitOpen),
            }),
        ])
        .unwrap();
        assert!(matches!(
            &open.events[1],
            StoredRoutingTraceEvent::Candidate {
                state: StoredCandidateState::Skipped,
                reason: Some(StoredSkipReason::HealthCircuitOpen),
                ..
            }
        ));
        let large = StoredRoutingTrace::from_resolver(
            &(0..64)
                .map(|index| {
                    TraceEvent::Candidate(CandidateEvidence {
                        candidate_id: CandidateId::new(format!("candidate-{index}")).unwrap(),
                        crossed_from: None,
                        binding_id: BindingId::new("openrouter.openrouter_messages").unwrap(),
                        model: Some("x".repeat(crate::policy::MAX_STRING_BYTES)),
                        result: Err(SkipReason::Disabled),
                    })
                })
                .collect::<Vec<_>>(),
        )
        .expect("a valid maximum-sized route has storable provenance");
        assert!(large.events.iter().all(|event| matches!(
            event,
            StoredRoutingTraceEvent::Candidate { model: None, .. }
        )));
        assert!(
            serde_json::to_vec(&large).unwrap().len() <= super::ledger::MAX_ROUTING_TRACE_BYTES
        );
        assert!(
            StoredRoutingTrace::from_resolver(&vec![
                TraceEvent::Lookup(
                    RouteProvenance::ProfileDefault
                );
                MAX_ROUTING_TRACE_EVENTS + 1
            ])
            .is_err()
        );
        for malformed in [
            serde_json::from_value::<StoredRoutingTrace>(json!({
                "version": 1,
                "events": [{
                    "kind": "candidate",
                    "candidate_id": "second",
                    "crossed_from": "first",
                    "binding_id": "openrouter.openrouter_messages",
                    "model": "two",
                    "state": "available",
                    "reason": null
                }]
            }))
            .unwrap(),
            serde_json::from_value::<StoredRoutingTrace>(json!({
                "version": 2,
                "events": [{
                    "kind": "candidate",
                    "candidate_id": "second",
                    "crossed_from": "first",
                    "binding_id": "openrouter.openrouter_messages",
                    "model": "two",
                    "state": "available",
                    "reason": null
                }]
            }))
            .unwrap(),
            serde_json::from_value::<StoredRoutingTrace>(json!({
                "version": 2,
                "events": [
                    {
                        "kind": "candidate",
                        "candidate_id": "first",
                        "binding_id": "openrouter.openrouter_messages",
                        "model": "one",
                        "state": "skipped",
                        "reason": "disabled"
                    },
                    {
                        "kind": "candidate",
                        "candidate_id": "second",
                        "crossed_from": "wrong",
                        "binding_id": "openrouter.openrouter_messages",
                        "model": "two",
                        "state": "available",
                        "reason": null
                    }
                ]
            }))
            .unwrap(),
            serde_json::from_value::<StoredRoutingTrace>(json!({
                "version": 3,
                "events": []
            }))
            .unwrap(),
        ] {
            assert!(malformed.validate().is_err());
        }
        let malformed = serde_json::from_value::<StoredRoutingTrace>(json!({
            "version": 3,
            "events": []
        }))
        .unwrap();
        let store = mem_store();
        let revision_id = revision(&store, "malformed-trace", None);
        let mut row = bare_row(1);
        row.binding_id = Some(BindingId::new("openrouter.openrouter_messages").unwrap());
        row.attempt = Some(AttemptProvenance {
            attempt_group_id: AttemptGroupId::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap(),
            attempt_index: AttemptIndex::zero(),
            reason: AttemptReason::Initial,
        });
        row.policy_routing = Some(PolicyRouteProvenance {
            policy_revision_id: revision_id,
            route: Some(ResolvedRoute {
                lookup: RequestLookupProvenance::ProtocolDefault,
                route_id: RouteId::new("route").unwrap(),
                route_fingerprint: "44".repeat(32).parse().unwrap(),
            }),
            trace: malformed,
            outcome: PolicyRoutingOutcome::Attempt {
                candidate_id: CandidateId::new("candidate").unwrap(),
                baseline_model: "model".to_owned(),
            },
        });
        assert!(matches!(
            store.record_request(&row),
            Err(Error::InvalidRoutingState(_))
        ));
    }

    #[test]
    fn complete_request_provenance_and_lineage_round_trip() {
        let store = mem_store();
        let revision_id = revision(&store, "provenance", None);
        let row = provenanced_row(revision_id);
        store.record_request(&row).unwrap();
        let mut loaded = store.requests_since(0, 10).unwrap().pop().unwrap();
        loaded.id = None;
        assert_eq!(loaded, row);
    }

    #[test]
    fn lineage_accepts_missing_parents_and_rejects_self_and_cycles() {
        let store = mem_store();
        let revision_id = revision(&store, "lineage", None);
        let mut child = provenanced_row(revision_id);
        child.policy_routing = None;
        store.record_request(&child).unwrap();

        let mut parent = bare_row(child.ts_ms + 1);
        parent.session_id = Some("parent-session".to_owned());
        parent.lineage = Some(
            SessionLineage::codex_guardian(&format!(
                "guardian:{}",
                child.session_id.clone().unwrap()
            ))
            .unwrap(),
        );
        assert!(matches!(
            store.record_request(&parent),
            Err(Error::Sqlite(_))
        ));

        let mut self_parent = bare_row(child.ts_ms + 2);
        self_parent.session_id = Some("same".to_owned());
        self_parent.lineage = Some(SessionLineage::codex_guardian("guardian:same").unwrap());
        assert!(matches!(
            store.record_request(&self_parent),
            Err(Error::InvalidRoutingState(_))
        ));
    }

    #[test]
    fn attempt_group_ids_and_indices_are_closed() {
        let first = AttemptGroupId::fresh();
        let second = AttemptGroupId::fresh();
        assert_ne!(first, second);
        for group in [&first, &second] {
            assert_eq!(group.as_str().len(), 32);
            assert!(
                group
                    .as_str()
                    .bytes()
                    .all(|byte| { byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) })
            );
        }
        let zero = AttemptIndex::zero();
        assert_eq!(zero.get(), 0);
        assert_eq!(zero.next().unwrap().get(), 1);
        assert!(AttemptIndex::try_from(-1).is_err());
    }

    #[test]
    fn logical_request_count_groups_attempts_and_keeps_legacy_rows_independent() {
        let store = mem_store();
        let group = AttemptGroupId::new("feedfeedfeedfeedfeedfeedfeedfeed").unwrap();
        for (ts_ms, index, reason) in [
            (1, 0, AttemptReason::Initial),
            (2, 1, AttemptReason::ThinkingNegotiation),
        ] {
            let mut row = bare_row(ts_ms);
            row.binding_id = Some(BindingId::new("anthropic_sub.anthropic_messages").unwrap());
            row.attempt = Some(AttemptProvenance {
                attempt_group_id: group.clone(),
                attempt_index: AttemptIndex::try_from(index).unwrap(),
                reason,
            });
            store.record_request(&row).unwrap();
        }
        store.record_request(&bare_row(3)).unwrap();

        assert_eq!(store.count_requests().unwrap(), 3);
        assert_eq!(store.count_logical_requests().unwrap(), 2);
        assert_eq!(mem_store().count_logical_requests().unwrap(), 0);
    }

    #[test]
    fn legacy_attempt_provenance_round_trips_without_policy_fields() {
        let store = mem_store();
        let group = AttemptGroupId::new("feedfeedfeedfeedfeedfeedfeedfeed").unwrap();
        let binding = BindingId::new("anthropic_sub.anthropic_messages").unwrap();

        let mut first = bare_row(1);
        first.kind = Some(RowKind::Error);
        first.binding_id = Some(binding.clone());
        first.attempt = Some(AttemptProvenance {
            attempt_group_id: group.clone(),
            attempt_index: AttemptIndex::zero(),
            reason: AttemptReason::Initial,
        });
        store.record_request(&first).unwrap();

        let mut second = bare_row(2);
        second.binding_id = Some(binding);
        second.attempt = Some(AttemptProvenance {
            attempt_group_id: group,
            attempt_index: AttemptIndex::try_from(1).unwrap(),
            reason: AttemptReason::ThinkingNegotiation,
        });
        store.record_request(&second).unwrap();

        let rows = store.requests_since(0, 10).unwrap();
        assert_eq!(rows[0].attempt, first.attempt);
        assert_eq!(rows[1].attempt, second.attempt);
        assert!(rows.iter().all(|row| row.policy_routing.is_none()));
    }

    #[test]
    fn actual_attempt_requires_binding_and_index_reason_agreement() {
        let mut row = bare_row(1);
        row.attempt = Some(AttemptProvenance {
            attempt_group_id: AttemptGroupId::new("feedfeedfeedfeedfeedfeedfeedfeed").unwrap(),
            attempt_index: AttemptIndex::zero(),
            reason: AttemptReason::Initial,
        });
        let store = mem_store();
        assert!(matches!(
            store.record_request(&row),
            Err(Error::InvalidRoutingState(_))
        ));

        row.binding_id = Some(BindingId::new("anthropic_sub.anthropic_messages").unwrap());
        row.attempt.as_mut().unwrap().reason = AttemptReason::Overload;
        assert!(matches!(
            store.record_request(&row),
            Err(Error::InvalidRoutingState(_))
        ));
        row.attempt.as_mut().unwrap().attempt_index = AttemptIndex::try_from(1).unwrap();
        row.attempt.as_mut().unwrap().reason = AttemptReason::Initial;
        assert!(matches!(
            store.record_request(&row),
            Err(Error::InvalidRoutingState(_))
        ));
    }

    #[test]
    fn request_attempt_provenance_round_trips_and_sequence_is_enforced() {
        fn routing(revision_id: i64) -> PolicyRouteProvenance {
            PolicyRouteProvenance {
                policy_revision_id: revision_id,
                route: Some(ResolvedRoute {
                    lookup: RequestLookupProvenance::ProtocolDefault,
                    route_id: RouteId::new("route").unwrap(),
                    route_fingerprint: "55".repeat(32).parse().unwrap(),
                }),
                trace: StoredRoutingTrace {
                    version: 1,
                    events: vec![StoredRoutingTraceEvent::Lookup {
                        source: super::StoredLookupSource::ProtocolDefault,
                        presentation_id: None,
                        rewrite_id: None,
                    }],
                },
                outcome: PolicyRoutingOutcome::Attempt {
                    candidate_id: CandidateId::new("candidate").unwrap(),
                    baseline_model: "model".to_owned(),
                },
            }
        }

        let store = mem_store();
        let revision_id = revision(&store, "request", None);
        let group = AttemptGroupId::new("abcdefabcdefabcdefabcdefabcdefab").unwrap();
        let mut first = bare_row(1);
        first.binding_id = Some(BindingId::new("openrouter.openrouter_messages").unwrap());
        first.attempt = Some(AttemptProvenance {
            attempt_group_id: group.clone(),
            attempt_index: AttemptIndex::zero(),
            reason: AttemptReason::Initial,
        });
        first.policy_routing = Some(routing(revision_id));
        store.record_request(&first).unwrap();
        let mut second = first.clone();
        second.ts_ms = 2;
        second.attempt = Some(AttemptProvenance {
            attempt_group_id: group.clone(),
            attempt_index: AttemptIndex::try_from(1).unwrap(),
            reason: AttemptReason::ThinkingNegotiation,
        });
        store.record_request(&second).unwrap();
        let rows = store.requests_since(0, 10).unwrap();
        assert_eq!(rows[0].policy_routing, first.policy_routing);
        assert_eq!(rows[1].policy_routing, second.policy_routing);

        let mut gap = first.clone();
        gap.ts_ms = 3;
        gap.attempt = Some(AttemptProvenance {
            attempt_group_id: group,
            attempt_index: AttemptIndex::try_from(3).unwrap(),
            reason: AttemptReason::ThinkingNegotiation,
        });
        assert!(matches!(store.record_request(&gap), Err(Error::Sqlite(_))));

        let terminal_group = AttemptGroupId::new("0123456789abcdef0123456789abcdef").unwrap();
        let mut terminal = bare_row(4);
        terminal.kind = Some(RowKind::RouteUnavailable);
        terminal.policy_routing = Some(PolicyRouteProvenance {
            policy_revision_id: revision_id,
            route: Some(ResolvedRoute {
                lookup: RequestLookupProvenance::ProtocolDefault,
                route_id: RouteId::new("route").unwrap(),
                route_fingerprint: "66".repeat(32).parse().unwrap(),
            }),
            trace: StoredRoutingTrace {
                version: 1,
                events: vec![StoredRoutingTraceEvent::Lookup {
                    source: super::StoredLookupSource::ProtocolDefault,
                    presentation_id: None,
                    rewrite_id: None,
                }],
            },
            outcome: PolicyRoutingOutcome::RouteUnavailable {
                attempt_group_id: terminal_group,
            },
        });
        store.record_request(&terminal).unwrap();
        let mut wrong_kind = terminal.clone();
        wrong_kind.ts_ms = 5;
        wrong_kind.kind = Some(RowKind::Error);
        assert!(matches!(
            store.record_request(&wrong_kind),
            Err(Error::InvalidRoutingState(_))
        ));
        let mut selected_binding = terminal;
        selected_binding.ts_ms = 6;
        selected_binding.binding_id =
            Some(BindingId::new("openrouter.openrouter_messages").unwrap());
        assert!(matches!(
            store.record_request(&selected_binding),
            Err(Error::InvalidRoutingState(_))
        ));
    }

    #[test]
    fn routeless_terminals_round_trip_and_only_in_their_own_shape() {
        use crate::policy::TransitionDenied;
        let store = mem_store();
        let revision_id = revision(&store, "routeless", None);
        let group = |n: u8| AttemptGroupId::new(format!("{n:02x}").repeat(16)).unwrap();
        let terminal = |ts_ms, routing| {
            let mut row = bare_row(ts_ms);
            row.kind = Some(RowKind::RouteUnavailable);
            row.policy_routing = Some(routing);
            row
        };

        let no_route = terminal(
            1,
            PolicyRouteProvenance::from_no_route(revision_id, group(1)).unwrap(),
        );
        store.record_request(&no_route).unwrap();
        let denied = terminal(
            2,
            PolicyRouteProvenance::from_transition_denied(
                revision_id,
                None,
                TransitionDenied::StaleRevision,
                group(2),
            )
            .unwrap(),
        );
        store.record_request(&denied).unwrap();
        let rows = store.requests_since(0, 10).unwrap();
        assert_eq!(rows[0].policy_routing, no_route.policy_routing);
        assert_eq!(rows[1].policy_routing, denied.policy_routing);
        let columns: (Option<String>, Option<String>) = store
            .conn()
            .unwrap()
            .query_row(
                "SELECT policy_route_id, route_fingerprint FROM requests WHERE ts_ms = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(columns, (None, None));

        // A routeless trace is its one terminal event, and an attempt always
        // has a route.
        let mut routeless_lookup = no_route.clone();
        routeless_lookup.ts_ms = 3;
        routeless_lookup
            .policy_routing
            .as_mut()
            .unwrap()
            .trace
            .events
            .insert(
                0,
                StoredRoutingTraceEvent::Lookup {
                    source: super::StoredLookupSource::ProtocolDefault,
                    presentation_id: None,
                    rewrite_id: None,
                },
            );
        let mut routeless_attempt = bare_row(4);
        routeless_attempt.binding_id =
            Some(BindingId::new("openrouter.openrouter_messages").unwrap());
        routeless_attempt.attempt = Some(AttemptProvenance {
            attempt_group_id: group(3),
            attempt_index: AttemptIndex::zero(),
            reason: AttemptReason::Initial,
        });
        routeless_attempt.policy_routing = Some(PolicyRouteProvenance {
            outcome: PolicyRoutingOutcome::Attempt {
                candidate_id: CandidateId::new("candidate").unwrap(),
                baseline_model: "model".to_owned(),
            },
            ..no_route.policy_routing.clone().unwrap()
        });
        let mut found_route_says_none = terminal(
            5,
            PolicyRouteProvenance::from_route_unavailable(
                revision_id,
                RouteId::new("route").unwrap(),
                "77".repeat(32).parse().unwrap(),
                &crate::policy::RouteProvenance::ProtocolDefault,
                &[crate::policy::TraceEvent::Lookup(
                    crate::policy::RouteProvenance::ProtocolDefault,
                )],
                group(4),
            )
            .unwrap(),
        );
        found_route_says_none
            .policy_routing
            .as_mut()
            .unwrap()
            .trace
            .events
            .push(StoredRoutingTraceEvent::NoRoute);
        for row in [routeless_lookup, routeless_attempt, found_route_says_none] {
            assert!(
                matches!(
                    store.record_request(&row),
                    Err(Error::InvalidRoutingState(_))
                ),
                "{row:?}"
            );
        }
    }

    #[test]
    fn a_denied_move_keeps_the_route_it_refused() {
        use crate::policy::TransitionDenied;
        let store = mem_store();
        let revision_id = revision(&store, "denied", None);
        let routing = PolicyRouteProvenance::from_transition_denied(
            revision_id,
            Some((
                RouteId::new("route").unwrap(),
                "88".repeat(32).parse().unwrap(),
                &crate::policy::RouteProvenance::ProtocolDefault,
                &[crate::policy::TraceEvent::Lookup(
                    crate::policy::RouteProvenance::ProtocolDefault,
                )],
            )),
            TransitionDenied::WarmCacheLoss,
            AttemptGroupId::fresh(),
        )
        .unwrap();
        assert_eq!(
            routing.trace.events.last(),
            Some(&StoredRoutingTraceEvent::TransitionDenied {
                reason: super::StoredTransitionDenied::WarmCacheLoss
            })
        );
        let mut row = bare_row(1);
        row.kind = Some(RowKind::RouteUnavailable);
        row.policy_routing = Some(routing);
        store.record_request(&row).unwrap();
        let stored = &store.requests_since(0, 10).unwrap()[0];
        assert_eq!(stored.policy_routing, row.policy_routing);
        assert_eq!(
            stored
                .policy_routing
                .as_ref()
                .and_then(|routing| routing.route.as_ref())
                .map(|route| route.route_id.as_str()),
            Some("route")
        );
    }

    #[test]
    fn binding_learning_merge_is_atomic_across_store_connections() {
        let dir = test_dir("learning-atomic");
        let db = dir.join("toker.db");
        let first = Store::open(&db).unwrap();
        let second = Store::open(&db).unwrap();
        let binding = BindingId::new("openrouter.openrouter_messages").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let first_binding = binding.clone();
        let first_barrier = barrier.clone();
        let first_merge = std::thread::spawn(move || {
            first_barrier.wait();
            first.merge_binding_learning(&BindingLearningUpdate {
                binding_id: first_binding,
                model_id: "model".to_owned(),
                days: ["2026-10-01".to_owned()].into_iter().collect(),
                max_prompt: Some(100),
                explicitly_promoted: true,
                updated_ms: 20,
            })
        });
        let second_binding = binding.clone();
        let second_merge = std::thread::spawn(move || {
            barrier.wait();
            second.merge_binding_learning(&BindingLearningUpdate {
                binding_id: second_binding,
                model_id: "model".to_owned(),
                days: ["2026-10-02".to_owned()].into_iter().collect(),
                max_prompt: Some(50),
                explicitly_promoted: false,
                updated_ms: 10,
            })
        });
        first_merge.join().unwrap().unwrap();
        second_merge.join().unwrap().unwrap();
        let reader = Store::open(&db).unwrap();
        let merged = reader
            .load_binding_learning(&binding, "model")
            .unwrap()
            .unwrap();
        assert_eq!(
            merged.days.into_iter().collect::<Vec<_>>(),
            ["2026-10-01", "2026-10-02"]
        );
        assert_eq!(merged.max_prompt, Some(100));
        assert!(merged.explicitly_promoted);
        assert_eq!(merged.updated_ms, 20);
    }

    #[test]
    fn reactivation_revision_may_reuse_its_parents_fingerprint() {
        let store = mem_store();
        let document = policy("same content");
        let parent = store
            .insert_policy_revision(&NewPolicyRevision {
                created_ms: 1,
                activated_ms: 2,
                description: Some("parent"),
                activation_scope: ActivationScope::NewLanes,
                forced_cache_transition: false,
                source: PolicyRevisionSource::Draft,
                compiler_origin: None,
                parent_revision_id: None,
                document: &document,
            })
            .unwrap();
        let child = store
            .insert_policy_revision(&NewPolicyRevision {
                created_ms: 3,
                activated_ms: 4,
                description: Some("reactivated"),
                activation_scope: ActivationScope::Immediate,
                forced_cache_transition: false,
                source: PolicyRevisionSource::Reactivation,
                compiler_origin: None,
                parent_revision_id: Some(parent.revision_id),
                document: &document,
            })
            .unwrap();
        assert_ne!(child.revision_id, parent.revision_id);
        assert_eq!(child.parent_revision_id, Some(parent.revision_id));
        assert_eq!(child.fingerprint, parent.fingerprint);
        assert_eq!(
            store
                .load_routing_policy_state()
                .unwrap()
                .active_revision_id,
            None
        );
    }
    #[test]
    fn migration_v8_refuses_v7_attempt_history_that_cannot_satisfy_new_constraints() {
        let dir = test_dir("v7-malformed-attempts");
        let db = dir.join("toker.db");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            for script in &schema::MIGRATIONS[..7] {
                conn.execute_batch(script).unwrap();
            }
            conn.pragma_update(None, "user_version", 7).unwrap();
            let group = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            conn.execute(
                "INSERT INTO requests (ts_ms, attempt_group_id, attempt_index)
                 VALUES (1, ?1, NULL), (2, ?1, NULL)",
                [group],
            )
            .unwrap();
        }
        let error = match Store::open(&db) {
            Ok(_) => panic!("malformed v7 history must block v8 migration"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("UNIQUE constraint failed"), "{error}");
        let conn = Connection::open(&db).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 7, "failed v8 migration rolls back atomically");
    }

    #[test]
    fn migration_v10_rejects_malformed_v9_terminal_history_atomically() {
        let dir = test_dir("v9-malformed-terminal");
        let db = dir.join("toker.db");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            for script in &schema::MIGRATIONS[..9] {
                conn.execute_batch(script).unwrap();
            }
            conn.pragma_update(None, "user_version", 9).unwrap();
            conn.execute(
                "INSERT INTO requests (ts_ms, kind, attempt_group_id, routing_trace)
                 VALUES (1, 'error', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
                         '{\"version\":1,\"events\":[]}')",
                [],
            )
            .unwrap();
        }
        let error = match Store::open(&db) {
            Ok(_) => panic!("malformed v9 terminal history must block v10 migration"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("invalid stored routing terminal"), "{error}");
        let conn = Connection::open(&db).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 9, "failed v10 migration rolls back atomically");
    }

    #[test]
    fn migration_v11_preserves_v1_traces_and_accepts_v2_only() {
        let dir = test_dir("v10-routing-trace-v2");
        let db = dir.join("toker.db");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            for script in &schema::MIGRATIONS[..10] {
                conn.execute_batch(script).unwrap();
            }
            conn.pragma_update(None, "user_version", 10).unwrap();
            conn.execute(
                "INSERT INTO requests (ts_ms, routing_trace) VALUES (1, ?1)",
                [r#"{"version":1,"events":[]}"#],
            )
            .unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(user_version(&store), schema::MIGRATIONS.len() as i64);
        drop(store);
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO requests (ts_ms, routing_trace) VALUES (2, ?1)",
            [r#"{"version":2,"events":[]}"#],
        )
        .unwrap();
        for invalid in [
            r#"{"version":3,"events":[]}"#,
            r#"{}"#,
            r#"{"version":null,"events":[]}"#,
            r#"{"version":2}"#,
        ] {
            assert!(
                conn.execute(
                    "INSERT INTO requests (ts_ms, routing_trace) VALUES (3, ?1)",
                    [invalid],
                )
                .is_err(),
                "invalid trace envelope was accepted: {invalid}"
            );
        }
        let historical: String = conn
            .query_row(
                "SELECT routing_trace FROM requests WHERE ts_ms=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(historical, r#"{"version":1,"events":[]}"#);
    }

    #[test]
    fn migration_v12_closes_attempt_reason_and_terminal_shapes() {
        let dir = test_dir("v11-attempt-reasons");
        let db = dir.join("toker.db");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            for script in &schema::MIGRATIONS[..11] {
                conn.execute_batch(script).unwrap();
            }
            conn.pragma_update(None, "user_version", 11).unwrap();
            conn.execute("INSERT INTO requests (ts_ms) VALUES (1)", [])
                .unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(user_version(&store), schema::MIGRATIONS.len() as i64);
        drop(store);
        let conn = Connection::open(&db).unwrap();
        let group = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        conn.execute(
            "INSERT INTO requests
             (ts_ms, attempt_group_id, attempt_index, fallback_reason)
             VALUES (2, ?1, 0, 'initial')",
            [group],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO requests
             (ts_ms, attempt_group_id, attempt_index, fallback_reason)
             VALUES (3, ?1, 1, 'thinking_negotiation')",
            [group],
        )
        .unwrap();
        for sql in [
            "INSERT INTO requests (ts_ms, attempt_group_id, attempt_index) VALUES (4, 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 0)",
            "INSERT INTO requests (ts_ms, attempt_group_id, attempt_index, fallback_reason) VALUES (5, 'cccccccccccccccccccccccccccccccc', 0, 'overload')",
            "INSERT INTO requests (ts_ms, attempt_group_id, attempt_index, fallback_reason) VALUES (6, 'dddddddddddddddddddddddddddddddd', 0, 'invented')",
        ] {
            assert!(conn.execute(sql, []).is_err(), "accepted {sql}");
        }
    }

    #[test]
    fn migration_v14_invalidates_applied_publications_without_exact_identities() {
        let dir = test_dir("v13-publication-identities");
        let db = dir.join("toker.db");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            for script in &schema::MIGRATIONS[..13] {
                conn.execute_batch(script).unwrap();
            }
            conn.pragma_update(None, "user_version", 13).unwrap();
            conn.execute(
                "INSERT INTO routing_policy_revisions
                 (revision_id, created_ms, activated_ms, description, activation_scope,
                  forced_cache_transition, source, parent_revision_id, document_json, fingerprint)
                 VALUES (1, 1, 1, NULL, 'new_lanes', 0, 'legacy', NULL, ?1, ?2)",
                rusqlite::params![
                    String::from_utf8(crate::policy::policy_bytes(&policy("v13")).unwrap())
                        .unwrap(),
                    crate::policy::policy_fingerprint(&policy("v13"))
                        .unwrap()
                        .to_string(),
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO routing_publications
                 (scope_key, adapter, desired_revision_id, applied_revision_id,
                  desired_inventory_generation, applied_inventory_generation, state, updated_ms)
                 VALUES ('protocol:anthropic_messages', 'claude_settings', 1, 1, 3, 3,
                         'applied', 1)",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&db).unwrap();
        assert_eq!(user_version(&store), schema::MIGRATIONS.len() as i64);
        let publication = store.load_publications().unwrap().pop().unwrap();
        assert_eq!(publication.state, PublicationState::Pending);
        assert_eq!(publication.applied_revision_id, None);
        assert_eq!(publication.applied_inventory_generation, None);
        assert_eq!(publication.applied_identities, None);
        assert_eq!(
            publication.detail_code.unwrap().as_str(),
            "legacy_applied_identities_unknown"
        );
    }

    #[test]
    fn migration_v15_preserves_legacy_compiler_origin_for_historical_reactivation() {
        let dir = test_dir("v14-compiler-origin");
        let db = dir.join("toker.db");
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = policy("legacy generated revision");
        let restored = policy("historical reactivation");
        let restored_again = policy("historical reactivation again");
        {
            let conn = Connection::open(&db).unwrap();
            for script in &schema::MIGRATIONS[..14] {
                conn.execute_batch(script).unwrap();
            }
            conn.pragma_update(None, "user_version", 14).unwrap();
            for (revision_id, source, parent, document) in [
                (1, "legacy", None, &legacy),
                (2, "reactivation", Some(1), &restored),
                (3, "reactivation", Some(2), &restored_again),
            ] {
                conn.execute(
                    "INSERT INTO routing_policy_revisions
                     (revision_id, created_ms, activated_ms, description, activation_scope,
                      forced_cache_transition, source, parent_revision_id, document_json,
                      fingerprint)
                     VALUES (?1, 1, 1, NULL, 'immediate', 0, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        revision_id,
                        source,
                        parent,
                        String::from_utf8(crate::policy::policy_bytes(document).unwrap()).unwrap(),
                        crate::policy::policy_fingerprint(document)
                            .unwrap()
                            .to_string(),
                    ],
                )
                .unwrap();
            }
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(
            store
                .load_policy_revision(1)
                .unwrap()
                .unwrap()
                .compiler_origin,
            PolicyCompilerOrigin::GeneratedRevisionOne
        );
        assert_eq!(
            store
                .load_policy_revision(2)
                .unwrap()
                .unwrap()
                .compiler_origin,
            PolicyCompilerOrigin::GeneratedRevisionOne
        );
        assert_eq!(
            store
                .load_policy_revision(3)
                .unwrap()
                .unwrap()
                .compiler_origin,
            PolicyCompilerOrigin::GeneratedRevisionOne,
            "compiler origin propagates across every historical reactivation"
        );
    }

    #[test]
    fn migration_v12_rejects_malformed_v11_history_atomically() {
        let dir = test_dir("v11-malformed-attempt-reason");
        let db = dir.join("toker.db");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            for script in &schema::MIGRATIONS[..11] {
                conn.execute_batch(script).unwrap();
            }
            conn.pragma_update(None, "user_version", 11).unwrap();
            conn.execute(
                "INSERT INTO requests
                 (ts_ms, attempt_group_id, attempt_index, fallback_reason)
                 VALUES (1, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 0, 'overload')",
                [],
            )
            .unwrap();
        }
        let error = match Store::open(&db) {
            Ok(_) => panic!("malformed v11 history must block v12 migration"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("invalid stored attempt reason"), "{error}");
        let conn = Connection::open(&db).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 11);
    }

    #[test]
    fn migration_v9_preserves_all_revision_references_and_foreign_keys() {
        let dir = test_dir("v8-revision-rebuild");
        let db = dir.join("toker.db");
        std::fs::create_dir_all(&dir).unwrap();
        let document = policy("same document");
        let json = String::from_utf8(crate::policy::policy_bytes(&document).unwrap()).unwrap();
        let fingerprint = crate::policy::policy_fingerprint(&document)
            .unwrap()
            .to_string();
        {
            let conn = Connection::open(&db).unwrap();
            conn.pragma_update(None, "foreign_keys", true).unwrap();
            for script in &schema::MIGRATIONS[..8] {
                conn.execute_batch(script).unwrap();
            }
            conn.pragma_update(None, "user_version", 8).unwrap();
            conn.execute(
                "INSERT INTO routing_policy_revisions
                 (revision_id, created_ms, activated_ms, description, activation_scope,
                  forced_cache_transition, source, parent_revision_id, document_json, fingerprint)
                 VALUES (1, 1, 2, 'parent', 'new_lanes', 0, 'draft', NULL, ?1, ?2)",
                rusqlite::params![json, fingerprint],
            )
            .unwrap();
            let child_json =
                String::from_utf8(crate::policy::policy_bytes(&policy("child")).unwrap()).unwrap();
            let child_fingerprint = crate::policy::policy_fingerprint(&policy("child"))
                .unwrap()
                .to_string();
            conn.execute(
                "INSERT INTO routing_policy_revisions
                 (revision_id, created_ms, activated_ms, description, activation_scope,
                  forced_cache_transition, source, parent_revision_id, document_json, fingerprint)
                 VALUES (2, 3, 4, 'child', 'immediate', 0, 'draft', 1, ?1, ?2)",
                rusqlite::params![child_json, child_fingerprint],
            )
            .unwrap();
            conn.execute(
                "UPDATE routing_policy_state SET draft_generation=1, draft_base_revision_id=2,
                 draft_document_json=?1, draft_fingerprint=?2, draft_updated_ms=5 WHERE id=1",
                rusqlite::params![child_json, child_fingerprint],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO routing_publications
                 (scope_key, adapter, desired_revision_id, applied_revision_id,
                  desired_inventory_generation, applied_inventory_generation, state, updated_ms)
                 VALUES ('protocol:anthropic_messages','claude_settings',2,1,2,1,'pending',6)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO lanes
                 (key, updated_ms, pin_generation, policy_revision_id, policy_route_id,
                  route_fingerprint, candidate_id, binding_id, baseline_model, effective_model)
                 VALUES ('lane',7,1,1,'route',?1,'candidate',
                         'openrouter.openrouter_messages','base','effective')",
                ["11".repeat(32)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO requests
                 (ts_ms, policy_revision_id, policy_route_id, route_fingerprint,
                  candidate_id, binding_id, baseline_model, attempt_group_id,
                  attempt_index, fallback_reason, routing_trace)
                 VALUES (8,2,'route',?1,'candidate','openrouter.openrouter_messages',
                         'base','abcdefabcdefabcdefabcdefabcdefab',0,'initial',?2)",
                rusqlite::params![
                    "22".repeat(32),
                    r#"{"version":1,"events":[{"kind":"lookup","source":"protocol_default","presentation_id":null,"rewrite_id":null}]}"#,
                ],
            )
            .unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(user_version(&store), schema::MIGRATIONS.len() as i64);
        assert_eq!(
            store
                .load_policy_revision(2)
                .unwrap()
                .unwrap()
                .parent_revision_id,
            Some(1)
        );
        assert_eq!(
            store
                .load_routing_policy_state()
                .unwrap()
                .draft
                .unwrap()
                .base_revision_id,
            2
        );
        let publication = store.load_publications().unwrap().pop().unwrap();
        assert_eq!(publication.desired_revision_id, 2);
        assert_eq!(publication.applied_revision_id, Some(1));
        assert_eq!(
            store
                .load_lane("lane")
                .unwrap()
                .unwrap()
                .pin
                .unwrap()
                .policy_revision_id,
            1
        );
        assert_eq!(
            store.requests_since(0, 10).unwrap()[0]
                .policy_routing
                .as_ref()
                .map(|routing| routing.policy_revision_id),
            Some(2)
        );
        let reactivated = store
            .insert_policy_revision(&NewPolicyRevision {
                created_ms: 9,
                activated_ms: 10,
                description: Some("reactivation"),
                activation_scope: ActivationScope::Immediate,
                forced_cache_transition: false,
                source: PolicyRevisionSource::Reactivation,
                compiler_origin: None,
                parent_revision_id: Some(1),
                document: &document,
            })
            .unwrap();
        assert_eq!(reactivated.fingerprint.to_string(), fingerprint);
        assert_eq!(
            store
                .load_routing_policy_state()
                .unwrap()
                .active_revision_id,
            None
        );
        let conn = store.conn.lock().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM routing_policy_revisions WHERE fingerprint=?1",
                [&fingerprint],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            2
        );
        let violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0);
        for table in ["routing_policy_state", "routing_publications"] {
            let target: String = conn
                .query_row(
                    &format!("SELECT \"table\" FROM pragma_foreign_key_list('{table}') LIMIT 1"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(target, "routing_policy_revisions");
        }
    }
}
