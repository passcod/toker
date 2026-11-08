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

/// The account a gate decision is about. The gate runs before the request
/// is sent, so it cannot read the account off a response: it takes the one
/// the session is already being served on, and for a session with no row
/// yet the one this provider served last.
///
/// A machine with one account answers both the same way. A machine with
/// several can gate a session's very first request against the wrong
/// account's windows; that request then writes the row that settles it, so
/// the window of error is one request and it does not recur.
pub(crate) fn gate_account(
    server: &Server,
    backend_id: &str,
    session_id: Option<&str>,
) -> Option<i64> {
    server
        .store
        .account_for(backend_id, session_id)
        .unwrap_or_else(|error| {
            tracing::error!(%error, "account lookup failed");
            None
        })
}

/// One account's meters for the gate. With no account known there is no
/// reading: an unresolved credential's meters feed no account's fit, and
/// pooling every account's into one is the conflation this card exists to
/// end (spec: ACCT#what-is-attributed-to-an-account).
pub(crate) fn load_meters_for(
    server: &Server,
    backend_id: &str,
    account_number: Option<i64>,
) -> Option<Value> {
    if !armed(server, backend_id) {
        return None;
    }
    server
        .store
        .load_account_meters(backend_id, account_number?)
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
    // No account, no window to release against, so nothing is recorded.
    let account_number = gate_account(server, backend_id, Some(session))?;
    let now = now_ms();
    let fresh = grant_for(stale_meters.map(Meters::over), now);
    if let Err(error) =
        crate::release::record_grant(&server.store, session, account_number, release, &fresh)
    {
        tracing::error!(%error, "allowance record failed");
    }
    Some((
        release,
        crate::release::merged(&server.store, session, account_number, &fresh, now),
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
    let allowances = allowances_for_session(
        server,
        session_id,
        gate_account(server, backend_id, session_id),
    );
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

/// A session's allowances on one account. An allowance releases the window
/// it was granted against and no other, so one account's never un-gates
/// another's (spec: ACCT#what-is-attributed-to-an-account). With no account
/// known there is no window to have been released, so none apply.
fn allowances_for_session(
    server: &Server,
    session_id: Option<&str>,
    account_number: Option<i64>,
) -> Vec<crate::store::Allowance> {
    let Some((session, account_number)) = session_id.zip(account_number) else {
        return Vec::new();
    };
    server
        .store
        .load_session_allowances(session)
        .map(|allowances| {
            allowances
                .into_iter()
                .filter(|allowance| allowance.account_number == account_number)
                .collect()
        })
        .unwrap_or_else(|error| {
            tracing::error!(%error, "allowances load failed");
            Vec::new()
        })
}
