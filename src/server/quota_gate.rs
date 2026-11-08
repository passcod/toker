//! Request-global Anthropic subscription quota decisions.
//!
//! Routing selects one initial destination before this gate runs. A local block
//! ends the request and never becomes candidate-unavailability or fallback
//! evidence; later physical attempts do not evaluate the gate again.

use serde_json::Value;

use crate::ir::Release;
use crate::middleware::lanes;
use crate::middleware::quota::{
    Blocking, GateDecision, Grant, Meter, Meters, decide, grant_for, plan_room,
};

use super::{Server, record::now_ms};

pub(crate) struct QuotaBlock {
    pub(crate) text: String,
    pub(crate) meter: Meter,
    pub(crate) resets_at: Option<i64>,
    pub(crate) context_tokens: Option<u64>,
    pub(crate) stale_meters: Option<Value>,
}

pub(crate) fn armed(server: &Server, backend_id: &str) -> bool {
    server.config().gates.quota_enabled && backend_id == crate::release::GATED_BACKEND
}

pub(crate) fn load_meters(server: &Server, backend_id: &str) -> Option<Value> {
    if !armed(server, backend_id) {
        return None;
    }
    server
        .store
        .load_meters(backend_id)
        .map(|snapshot| snapshot.map(|snapshot| snapshot.snapshot))
        .unwrap_or_else(|error| {
            tracing::error!(%error, "meter snapshot load failed");
            None
        })
}

pub(crate) fn grant_release(
    server: &Server,
    session_id: Option<&str>,
    backend_id: &str,
    release: Option<Release>,
    stale_meters: Option<&Value>,
) -> Option<(Release, Grant)> {
    if !armed(server, backend_id) {
        return None;
    }
    let (session, release) = session_id.zip(release)?;
    let now = now_ms();
    let fresh = grant_for(stale_meters.map(Meters::over), now);
    if let Err(error) = crate::release::record_grant(&server.store, session, release, &fresh) {
        tracing::error!(%error, "allowance record failed");
    }
    Some((
        release,
        crate::release::merged(&server.store, session, &fresh, now),
    ))
}

pub(crate) fn decide_block(
    server: &Server,
    backend_id: &str,
    session_id: Option<&str>,
    frontend: Option<&str>,
    stale_meters: Option<Value>,
) -> Option<QuotaBlock> {
    if !armed(server, backend_id) {
        return None;
    }
    let allowances = allowances_for_session(server, session_id);
    let GateDecision::Block { meter, resets_at } = decide(
        stale_meters.as_ref().map(Meters::over),
        &allowances,
        now_ms(),
    ) else {
        return None;
    };
    let context_tokens = session_id.and_then(|session| {
        server
            .store
            .load_lanes()
            .ok()
            .and_then(|lanes| lanes::session_prompt(&lanes, session))
    });
    let plan_left = stale_meters
        .as_ref()
        .is_some_and(|snapshot| plan_room(Meters::over(snapshot), meter));
    let text = Blocking::notice(
        meter,
        resets_at,
        plan_left,
        context_tokens,
        &jiff::tz::TimeZone::system(),
        server.config().notices.style_for(frontend),
    );
    Some(QuotaBlock {
        text,
        meter,
        resets_at,
        context_tokens,
        stale_meters,
    })
}

fn allowances_for_session(
    server: &Server,
    session_id: Option<&str>,
) -> Vec<crate::store::Allowance> {
    let Some(session) = session_id else {
        return Vec::new();
    };
    server
        .store
        .load_session_allowances(session)
        .unwrap_or_else(|error| {
            tracing::error!(%error, "allowances load failed");
            Vec::new()
        })
}
