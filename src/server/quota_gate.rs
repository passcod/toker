//! Request-global Anthropic subscription quota decisions.
//!
//! Routing selects one initial destination before this gate runs. A local block
//! ends the request and never becomes candidate-unavailability or fallback
//! evidence; later physical attempts do not evaluate the gate again.

use axum::http::HeaderMap;
use serde_json::Value;

use crate::ir::Release;
use crate::middleware::lanes;
use crate::middleware::quota::{
    Blocking, GateDecision, Grant, Meter, Meters, decide, grant_for, plan_room,
};

use crate::providers::Provider;
use crate::routing::ProtocolId;
use crate::store::CredentialDigest;

use super::proxy::{outgoing_credential_digest, outgoing_headers};
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

/// The credential a request will be served with, and the account it names.
#[derive(Debug, Clone, Default)]
pub(crate) struct RequestCredential {
    pub(crate) digest: Option<CredentialDigest>,
    pub(crate) account_number: Option<i64>,
}

/// The account a gate decision is about: the one named by the credential
/// the request is about to be sent with (spec: ACCT#routing-does-not-select-an-account).
///
/// The gate runs before the request is sent, but the credential it will carry
/// is already decided, so the account is read off that. Reading it off the
/// session's last row instead, or the provider's, followed a login switched
/// mid-session onto the old account's windows. Because a blocked request
/// sends nothing and writes no served row, that guess then repeated for every
/// request after it.
///
/// A credential that resolves to no account answers `None`, and the gate then
/// has no reading to stop the request on.
pub(crate) fn request_credential(
    server: &Server,
    provider: &dyn Provider,
    incoming: &HeaderMap,
    frontend: ProtocolId,
) -> RequestCredential {
    let headers = outgoing_headers(
        provider,
        incoming,
        &server.config().session_header_names,
        frontend,
    );
    let digest = outgoing_credential_digest(&headers);
    let account_number = digest
        .as_ref()
        .and_then(|digest| server.observe_credential(provider.id(), digest));
    RequestCredential {
        digest,
        account_number,
    }
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
    // Only the account's own reading. A gate with no account to read has no
    // evidence and does not fire: the provider's slot pools every account
    // that ever served through it, and gating one account's session on
    // another's spend is exactly what this card exists to stop. It would
    // also stop a session the release marker could not then free, because
    // an allowance is keyed by the account it was granted on
    // (spec: GQUOTA#an-allowance-belongs-to-an-account).
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
    account_number: Option<i64>,
    release: Option<Release>,
    stale_meters: Option<&Value>,
) -> Option<(Release, Grant)> {
    if !armed(server, backend_id) {
        return None;
    }
    let (session, release) = session_id.zip(release)?;
    // No account, no window to release against, so nothing is recorded.
    let account_number = account_number?;
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
    account_number: Option<i64>,
    frontend: Option<&str>,
    stale_meters: Option<Value>,
) -> Option<QuotaBlock> {
    if !armed(server, backend_id) {
        return None;
    }
    let allowances = allowances_for_session(server, session_id, account_number);
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
