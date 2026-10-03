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

use super::{Result, row_of, rows_of};
use rusqlite::Connection;

/// One lane row: `key` is the composite `"sessionId|toolsHash"` (plan: Lane
/// tracking + sleep lock). `ping` lanes never hold the sleep lock.
#[derive(Debug, Clone, PartialEq)]
pub struct Lane {
    pub key: String,
    pub session_id: Option<String>,
    pub tools_hash: Option<String>,
    pub updated_ms: i64,
    pub prompt_tokens: Option<i64>,
    pub ttl: Option<i64>,
    pub ping: Option<bool>,
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

/// One ping run (plan: ping windows).
#[derive(Debug, Clone, PartialEq)]
pub struct PingRecord {
    /// Row id, assigned on insert; `None` while inserting.
    pub id: Option<i64>,
    pub ts_ms: i64,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub boundary_ms: Option<i64>,
}

/// The last meter snapshot, single-row (plan: Server core). Every response
/// updates it when the backend has meters, so a spent reading stops
/// counting once its window's reset passes.
#[derive(Debug, Clone, PartialEq)]
pub struct MetersSnapshot {
    pub updated_ms: i64,
    /// Meter-source-dependent snapshot, stored whole as JSON.
    pub snapshot: Value,
}

pub(super) fn upsert_lane(conn: &Connection, lane: &Lane) -> Result<()> {
    conn.execute(
        "INSERT INTO lanes (key, session_id, tools_hash, updated_ms, prompt_tokens, ttl, ping)
         VALUES (:key, :session_id, :tools_hash, :updated_ms, :prompt_tokens, :ttl, :ping)
         ON CONFLICT (key) DO UPDATE SET
             session_id = excluded.session_id,
             tools_hash = excluded.tools_hash,
             updated_ms = excluded.updated_ms,
             prompt_tokens = excluded.prompt_tokens,
             ttl = excluded.ttl,
             ping = excluded.ping",
        rusqlite::named_params! {
            ":key": lane.key,
            ":session_id": lane.session_id,
            ":tools_hash": lane.tools_hash,
            ":updated_ms": lane.updated_ms,
            ":prompt_tokens": lane.prompt_tokens,
            ":ttl": lane.ttl,
            ":ping": lane.ping,
        },
    )?;
    Ok(())
}

pub(super) fn load_lane(conn: &Connection, key: &str) -> Result<Option<Lane>> {
    row_of(
        conn,
        "SELECT key, session_id, tools_hash, updated_ms, prompt_tokens, ttl, ping
         FROM lanes WHERE key = ?1",
        [key],
        read_lane,
    )
}

pub(super) fn load_lanes(conn: &Connection) -> Result<Vec<Lane>> {
    rows_of(
        conn,
        "SELECT key, session_id, tools_hash, updated_ms, prompt_tokens, ttl, ping
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
        "INSERT INTO pings (ts_ms, exit_code, duration_ms, boundary_ms)
         VALUES (?1, ?2, ?3, ?4)",
        (
            ping.ts_ms,
            ping.exit_code,
            ping.duration_ms,
            ping.boundary_ms,
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
    })
}

pub(super) fn save_meters(conn: &Connection, snapshot: &MetersSnapshot) -> Result<()> {
    let snapshot_json = serde_json::to_string(&snapshot.snapshot)?;
    conn.execute(
        "INSERT INTO meters_state (id, updated_ms, snapshot) VALUES (1, ?1, ?2)
         ON CONFLICT (id) DO UPDATE SET
             updated_ms = excluded.updated_ms,
             snapshot = excluded.snapshot",
        (snapshot.updated_ms, snapshot_json),
    )?;
    Ok(())
}

pub(super) fn load_meters(conn: &Connection) -> Result<Option<MetersSnapshot>> {
    row_of(
        conn,
        "SELECT updated_ms, snapshot FROM meters_state WHERE id = 1",
        [],
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
