//! State tables (plan: "State as tables"): small, read-modify-write data
//! alongside the insert-only ledger — lanes, learned models, allowances,
//! pings, the last meter snapshot, and the meta key/value table.
//!
//! These are *not* part of the ledger's insert-only discipline: the store
//! exposes upserts and the caller owns the read-modify-write cycle (e.g.
//! bumping a lane's `updated_ms` and `prompt_tokens` after a response).
//! Expiry rules that depend on clocks (allowances self-expiring once their
//! window's reset passes, the 30-day lane prune) are the caller's too — the
//! store has no clock beyond `ts_ms` values handed to it.

use serde_json::Value;

use super::{Error, Result, row_of, rows_of};
use rusqlite::Connection;

/// One lane row: `key` is the composite `"sessionId|toolsHash"` (plan: Lane
/// tracking + sleep lock). `ping` lanes never hold the sleep lock.
///
/// The predecessor's lane record, in store form: `updated_ms` is its
/// `at` (moved only
/// when a response the API actually served completes), `noticed_at` is
/// `noticedAt` (moved only when the cold notice fires — the decision
/// unit keeps the two questions apart),
/// `forced_from`/`forced_to` are `forced:
/// {from, to}` (the sticky-upgrade record, consulted while the lane's cache
/// may still be warm).
#[derive(Debug, Clone, PartialEq)]
pub struct Lane {
    pub key: String,
    pub session_id: Option<String>,
    pub tools_hash: Option<String>,
    pub updated_ms: i64,
    pub prompt_tokens: Option<i64>,
    pub ttl: Option<i64>,
    pub ping: Option<bool>,
    /// When the cold notice last fired for this lane's idle spell; never
    /// moves with `updated_ms` (a notice resets nothing it measures).
    pub noticed_at: Option<i64>,
    /// The sticky upgrade: the model this lane's conversation was moved off.
    pub forced_from: Option<String>,
    /// The model its cache now lives on.
    pub forced_to: Option<String>,
}

/// One learned-model row: days served and max prompt observed for an exact
/// model identity, plus its hand-verified context-window catalogue entry
/// (plan: Model catalogues).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelEntry {
    pub model_id: String,
    /// Days-served history as JSON (list of day markers).
    pub days_json: Option<Value>,
    /// Max prompt observed for this identity; rewrites never exceed it.
    pub max_prompt: Option<i64>,
    /// Context-window catalogue entry as JSON.
    pub context_window_json: Option<Value>,
}

/// One allowance row: a quota window opened against a meter, keyed by the
/// reset value that ends it (plan: allowances keyed by reset value,
/// self-expiring — old rows are simply never loaded again once their reset
/// passes).
#[derive(Debug, Clone, PartialEq)]
pub struct Allowance {
    pub session_id: String,
    pub meter: String,
    pub reset_value: i64,
}

/// One ping run (plan: ping windows). Every field but `ts_ms` is
/// `None` on a row written before migration v4 added it.
#[derive(Debug, Clone, PartialEq)]
pub struct PingRecord {
    /// Row id, assigned on insert; `None` while inserting.
    pub id: Option<i64>,
    pub ts_ms: i64,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    /// The predicted boundary: `floor(fire time, 10 min) + 5 h`.
    pub boundary_ms: Option<i64>,
    /// The slot this run served, `hh:mm`.
    pub slot: Option<String>,
    /// What the run did.
    pub action: Option<PingAction>,
    /// The boundary the ledger reported back: for a ping, the 5-hour
    /// reset on the rows it produced; for a skip, the reset of the window
    /// already open. `None` when the ledger could not say.
    pub observed_ms: Option<i64>,
    /// Whether the observed boundary matched the prediction; `None` when
    /// there was nothing to compare.
    pub verified: Option<bool>,
    /// The decision to ping was taken for want of any meter reading,
    /// not because one showed no window open.
    pub assumed: Option<bool>,
}

/// What one ping run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingAction {
    /// Sent the client request.
    Ping,
    /// A window was already open, so nothing was sent.
    Skip,
    /// The client did not run or did not exit cleanly.
    Failed,
}

impl PingAction {
    /// The stored string form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Skip => "skip",
            Self::Failed => "failed",
        }
    }

    /// Parse the stored string form; `None` for unknown values.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ping" => Some(Self::Ping),
            "skip" => Some(Self::Skip),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// The last meter snapshot for one meter-source backend (plan: Server
/// core). Every response updates its own provider's slot when the
/// backend has meters, so a spent reading stops counting once its
/// window's reset passes. Keyed by provider id (migration v3): the
/// anthropic sub's quota shape and the codex sub's usage-limit shape
/// never overwrite each other, and the quota gate reads only its own.
#[derive(Debug, Clone, PartialEq)]
pub struct MetersSnapshot {
    pub updated_ms: i64,
    /// Meter-source-dependent snapshot, stored whole as JSON.
    pub snapshot: Value,
}

pub(super) fn upsert_lane(conn: &Connection, lane: &Lane) -> Result<()> {
    conn.execute(
        "INSERT INTO lanes (key, session_id, tools_hash, updated_ms, prompt_tokens, ttl, ping,
                            noticed_at, forced_from, forced_to)
         VALUES (:key, :session_id, :tools_hash, :updated_ms, :prompt_tokens, :ttl, :ping,
                 :noticed_at, :forced_from, :forced_to)
         ON CONFLICT (key) DO UPDATE SET
             session_id = excluded.session_id,
             tools_hash = excluded.tools_hash,
             updated_ms = excluded.updated_ms,
             prompt_tokens = excluded.prompt_tokens,
             ttl = excluded.ttl,
             ping = excluded.ping,
             noticed_at = excluded.noticed_at,
             forced_from = excluded.forced_from,
             forced_to = excluded.forced_to",
        rusqlite::named_params! {
            ":key": lane.key,
            ":session_id": lane.session_id,
            ":tools_hash": lane.tools_hash,
            ":updated_ms": lane.updated_ms,
            ":prompt_tokens": lane.prompt_tokens,
            ":ttl": lane.ttl,
            ":ping": lane.ping,
            ":noticed_at": lane.noticed_at,
            ":forced_from": lane.forced_from,
            ":forced_to": lane.forced_to,
        },
    )?;
    Ok(())
}

pub(super) fn load_lane(conn: &Connection, key: &str) -> Result<Option<Lane>> {
    row_of(
        conn,
        "SELECT key, session_id, tools_hash, updated_ms, prompt_tokens, ttl, ping,
                noticed_at, forced_from, forced_to
         FROM lanes WHERE key = ?1",
        [key],
        read_lane,
    )
}

pub(super) fn load_lanes(conn: &Connection) -> Result<Vec<Lane>> {
    rows_of(
        conn,
        "SELECT key, session_id, tools_hash, updated_ms, prompt_tokens, ttl, ping,
                noticed_at, forced_from, forced_to
         FROM lanes ORDER BY key",
        [],
        read_lane,
    )
}

fn read_lane(row: &rusqlite::Row<'_>) -> Result<Lane> {
    Ok(Lane {
        key: row.get("key")?,
        session_id: row.get("session_id")?,
        tools_hash: row.get("tools_hash")?,
        updated_ms: row.get("updated_ms")?,
        prompt_tokens: row.get("prompt_tokens")?,
        ttl: row.get("ttl")?,
        ping: row.get("ping")?,
        noticed_at: row.get("noticed_at")?,
        forced_from: row.get("forced_from")?,
        forced_to: row.get("forced_to")?,
    })
}

pub(super) fn upsert_model(conn: &Connection, entry: &ModelEntry) -> Result<()> {
    let days_json = super::opt_json_to_text(&entry.days_json)?;
    let context_window_json = super::opt_json_to_text(&entry.context_window_json)?;
    conn.execute(
        "INSERT INTO models (model_id, days_json, max_prompt, context_window_json)
         VALUES (:model_id, :days_json, :max_prompt, :context_window_json)
         ON CONFLICT (model_id) DO UPDATE SET
             days_json = excluded.days_json,
             max_prompt = excluded.max_prompt,
             context_window_json = excluded.context_window_json",
        rusqlite::named_params! {
            ":model_id": entry.model_id,
            ":days_json": days_json,
            ":max_prompt": entry.max_prompt,
            ":context_window_json": context_window_json,
        },
    )?;
    Ok(())
}

pub(super) fn load_model(conn: &Connection, model_id: &str) -> Result<Option<ModelEntry>> {
    row_of(
        conn,
        "SELECT model_id, days_json, max_prompt, context_window_json
         FROM models WHERE model_id = ?1",
        [model_id],
        read_model,
    )
}

pub(super) fn load_models(conn: &Connection) -> Result<Vec<ModelEntry>> {
    rows_of(
        conn,
        "SELECT model_id, days_json, max_prompt, context_window_json
         FROM models ORDER BY model_id",
        [],
        read_model,
    )
}

fn read_model(row: &rusqlite::Row<'_>) -> Result<ModelEntry> {
    Ok(ModelEntry {
        model_id: row.get("model_id")?,
        days_json: super::opt_json_from_text(row.get("days_json")?)?,
        max_prompt: row.get("max_prompt")?,
        context_window_json: super::opt_json_from_text(row.get("context_window_json")?)?,
    })
}

/// Age and cap the lanes table: drop
/// lanes whose `updated_ms` sits in the future (a clock that moved — a
/// lane that could never go idle) or further back than `max_age_ms`, then
/// keep only the newest `max_lanes`. Rows the caller wrote are already
/// well-formed, so the file-era paranoia about hand-edited records does
/// not apply — the SQL is the whole rule. Returns how many rows went.
///
/// The caller owns the cadence: the prune runs on the 30-second flush,
/// never per request.
pub(super) fn prune_lanes(
    conn: &Connection,
    now_ms: i64,
    max_lanes: usize,
    max_age_ms: i64,
) -> Result<u64> {
    let aged = conn.execute(
        "DELETE FROM lanes WHERE updated_ms > ?1 OR updated_ms < ?2",
        [now_ms, now_ms.saturating_sub(max_age_ms)],
    )?;
    let capped = conn.execute(
        "DELETE FROM lanes WHERE key NOT IN
            (SELECT key FROM lanes ORDER BY updated_ms DESC LIMIT ?1)",
        [max_lanes.min(i64::MAX as usize) as i64],
    )?;
    Ok((aged + capped) as u64)
}

/// Idempotent: recording the same (session, meter, reset) allowance twice is
/// one row.
pub(super) fn record_allowance(conn: &Connection, allowance: &Allowance) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO allowances (session_id, meter, reset_value)
         VALUES (?1, ?2, ?3)",
        (
            &allowance.session_id,
            &allowance.meter,
            allowance.reset_value,
        ),
    )?;
    Ok(())
}

pub(super) fn load_allowances(conn: &Connection) -> Result<Vec<Allowance>> {
    rows_of(
        conn,
        "SELECT session_id, meter, reset_value FROM allowances
         ORDER BY session_id, meter, reset_value",
        [],
        read_allowance,
    )
}

fn read_allowance(row: &rusqlite::Row<'_>) -> Result<Allowance> {
    Ok(Allowance {
        session_id: row.get("session_id")?,
        meter: row.get("meter")?,
        reset_value: row.get("reset_value")?,
    })
}

pub(super) fn record_ping(conn: &Connection, ping: &PingRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO pings (ts_ms, exit_code, duration_ms, boundary_ms,
                            slot, action, observed_ms, verified, assumed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        (
            ping.ts_ms,
            ping.exit_code,
            ping.duration_ms,
            ping.boundary_ms,
            ping.slot.as_deref(),
            ping.action.map(PingAction::as_str),
            ping.observed_ms,
            ping.verified,
            ping.assumed,
        ),
    )?;
    Ok(())
}

/// Pings with `ts_ms >= ts_ms`, oldest first; like the ledger window, the
/// newest `limit` rows are kept when the window overflows.
pub(super) fn pings_since(conn: &Connection, ts_ms: i64, limit: i64) -> Result<Vec<PingRecord>> {
    rows_of(
        conn,
        "SELECT * FROM (
            SELECT * FROM pings WHERE ts_ms >= ?1
            ORDER BY ts_ms DESC, id DESC LIMIT ?2
        ) ORDER BY ts_ms ASC, id ASC",
        [ts_ms, limit],
        read_ping,
    )
}

fn read_ping(row: &rusqlite::Row<'_>) -> Result<PingRecord> {
    Ok(PingRecord {
        id: row.get("id")?,
        ts_ms: row.get("ts_ms")?,
        exit_code: row.get("exit_code")?,
        duration_ms: row.get("duration_ms")?,
        boundary_ms: row.get("boundary_ms")?,
        slot: row.get("slot")?,
        action: match row.get::<_, Option<String>>("action")? {
            None => None,
            Some(text) => Some(PingAction::parse(&text).ok_or(Error::UnknownDbValue {
                column: "action",
                value: text,
            })?),
        },
        observed_ms: row.get("observed_ms")?,
        verified: row.get("verified")?,
        assumed: row.get("assumed")?,
    })
}

pub(super) fn save_meters(
    conn: &Connection,
    provider_id: &str,
    snapshot: &MetersSnapshot,
) -> Result<()> {
    let snapshot_json = serde_json::to_string(&snapshot.snapshot)?;
    conn.execute(
        "INSERT INTO meters_by_provider (provider_id, updated_ms, snapshot) VALUES (?1, ?2, ?3)
         ON CONFLICT (provider_id) DO UPDATE SET
             updated_ms = excluded.updated_ms,
             snapshot = excluded.snapshot",
        (provider_id, snapshot.updated_ms, snapshot_json),
    )?;
    Ok(())
}

pub(super) fn load_meters(conn: &Connection, provider_id: &str) -> Result<Option<MetersSnapshot>> {
    row_of(
        conn,
        "SELECT updated_ms, snapshot FROM meters_by_provider WHERE provider_id = ?1",
        [provider_id],
        read_meters,
    )
}

fn read_meters(row: &rusqlite::Row<'_>) -> Result<MetersSnapshot> {
    let snapshot = super::opt_json_from_text(row.get::<_, Option<String>>("snapshot")?)?
        .expect("meters_state.snapshot is NOT NULL");
    Ok(MetersSnapshot {
        updated_ms: row.get("updated_ms")?,
        snapshot,
    })
}

pub(super) fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        (key, value),
    )?;
    Ok(())
}

pub(super) fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    row_of(
        conn,
        "SELECT value FROM meta WHERE key = ?1",
        [key],
        |row| Ok(row.get(0)?),
    )
}
