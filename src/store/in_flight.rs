//! Requests in flight: state beside the ledger, not ledger.
//!
//! A row is written when the server begins handling a request, moved at each
//! stage change, and deleted once the request's last ledger row has been
//! written, so a reader sees a request from arrival rather than from the end
//! of its response. The table carries no prompt or completion content.

use rusqlite::Connection;

use super::ledger::parse_optional_id;
use super::{AttemptGroupId, AttemptIndex, Error, Result, SessionLineage, rows_of};

/// How far a request has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InFlightStage {
    /// Received, and not yet sent to any provider.
    Held,
    /// An attempt was sent, and no response to it has begun.
    Waiting,
    /// A response to the current attempt is under way.
    Streaming,
}

impl InFlightStage {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "held" => Some(Self::Held),
            "waiting" => Some(Self::Waiting),
            "streaming" => Some(Self::Streaming),
            _ => None,
        }
    }
}

/// What the server knows of a request once it has read it, which is later
/// than when it began handling it.
#[derive(Debug, Clone, Default)]
pub struct InFlightIdentity<'a> {
    pub session_id: Option<&'a str>,
    pub lineage: Option<&'a SessionLineage>,
    pub requested_model: Option<&'a str>,
    pub req_messages: Option<i64>,
}

/// The attempt a request's current send belongs to.
#[derive(Debug, Clone)]
pub struct InFlightAttempt<'a> {
    pub group_id: &'a AttemptGroupId,
    pub index: AttemptIndex,
    pub provider: &'a str,
    pub binding_id: Option<&'a str>,
    pub effective_model: Option<&'a str>,
}

/// One request being handled.
#[derive(Debug, Clone, PartialEq)]
pub struct InFlightRequest {
    pub id: i64,
    /// Epoch milliseconds at which handling began.
    pub started_ms: i64,
    pub attempt_group_id: Option<AttemptGroupId>,
    pub session_id: Option<String>,
    pub lineage: Option<SessionLineage>,
    pub frontend: Option<String>,
    pub requested_model: Option<String>,
    pub req_messages: Option<i64>,
    pub stage: InFlightStage,
    pub attempt_index: Option<AttemptIndex>,
    pub provider: Option<String>,
    pub binding_id: Option<String>,
    pub effective_model: Option<String>,
}

pub(super) fn begin(conn: &Connection, started_ms: i64, frontend: Option<&str>) -> Result<i64> {
    conn.execute(
        "INSERT INTO in_flight (started_ms, frontend, stage) VALUES (?1, ?2, 'held')",
        rusqlite::params![started_ms, frontend],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(super) fn identify(conn: &Connection, id: i64, identity: &InFlightIdentity<'_>) -> Result<()> {
    conn.execute(
        "UPDATE in_flight SET session_id = ?2, parent_session_id = ?3,
                requested_model = ?4, req_messages = ?5
         WHERE id = ?1",
        rusqlite::params![
            id,
            identity.session_id,
            identity.lineage.map(SessionLineage::parent_session_id),
            identity.requested_model,
            identity.req_messages,
        ],
    )?;
    Ok(())
}

pub(super) fn sent(conn: &Connection, id: i64, attempt: &InFlightAttempt<'_>) -> Result<()> {
    conn.execute(
        "UPDATE in_flight SET stage = 'waiting', attempt_group_id = ?2, attempt_index = ?3,
                provider = ?4, binding_id = ?5, effective_model = ?6
         WHERE id = ?1",
        rusqlite::params![
            id,
            attempt.group_id.as_str(),
            attempt.index.get(),
            attempt.provider,
            attempt.binding_id,
            attempt.effective_model,
        ],
    )?;
    Ok(())
}

pub(super) fn streaming(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE in_flight SET stage = 'streaming' WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

pub(super) fn end(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM in_flight WHERE id = ?1", [id])?;
    Ok(())
}

pub(super) fn clear(conn: &Connection) -> Result<u64> {
    Ok(conn.execute("DELETE FROM in_flight", [])? as u64)
}

pub(super) fn rows(conn: &Connection) -> Result<Vec<InFlightRequest>> {
    rows_of(
        conn,
        "SELECT id, started_ms, attempt_group_id, session_id, parent_session_id, frontend,
                requested_model, req_messages, stage, attempt_index, provider, binding_id,
                effective_model
         FROM in_flight ORDER BY started_ms ASC, id ASC",
        [],
        read_row,
    )
}

fn read_row(row: &rusqlite::Row<'_>) -> Result<InFlightRequest> {
    let stage: String = row.get("stage")?;
    let stage = InFlightStage::parse(&stage).ok_or(Error::UnknownDbValue {
        column: "in_flight.stage",
        value: stage,
    })?;
    let session_id: Option<String> = row.get("session_id")?;
    let parent: Option<String> = row.get("parent_session_id")?;
    let lineage = match (&session_id, parent) {
        (Some(_), Some(parent)) => Some(
            SessionLineage::codex_guardian(&format!("guardian:{parent}")).ok_or_else(|| {
                Error::InvalidRoutingState("stored in-flight lineage has an empty parent".into())
            })?,
        ),
        _ => None,
    };
    Ok(InFlightRequest {
        id: row.get("id")?,
        started_ms: row.get("started_ms")?,
        attempt_group_id: parse_optional_id(
            "in_flight.attempt_group_id",
            row.get("attempt_group_id")?,
        )?,
        session_id,
        lineage,
        frontend: row.get("frontend")?,
        requested_model: row.get("requested_model")?,
        req_messages: row.get("req_messages")?,
        stage,
        attempt_index: row
            .get::<_, Option<i64>>("attempt_index")?
            .map(AttemptIndex::try_from)
            .transpose()
            .map_err(Error::InvalidRoutingState)?,
        provider: row.get("provider")?,
        binding_id: row.get("binding_id")?,
        effective_model: row.get("effective_model")?,
    })
}
