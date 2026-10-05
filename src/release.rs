//! Granting and revoking quota-gate releases: the allowance rows the gate
//! reads, and the ledger rows that record each change. Shared by the
//! release markers on the request path ([`crate::server`]) and the TUI's
//! gate controls ([`crate::tui`]), so both build the same rows.
//!
//! The gate loads a session's allowances from the store on every request
//! (`quota::decide`), so a grant or a revoke written here is live from the
//! session's next request, whichever process wrote it.

use anyhow::bail;
use serde_json::{Value, json};

use crate::config::GatesConfig;
use crate::ir::Release;
use crate::middleware::quota::{Grant, Meter, Meters, grant_ahead};
use crate::store::{Allowance, RequestRow, RowKind, Store};

/// The backend the quota gate meters, and so the only one a release
/// applies to.
pub const GATED_BACKEND: &str = "anthropic_sub";

/// Record one allowance per meter `fresh` grants. A store error is
/// returned for the caller to log or show.
pub fn record_grant(
    store: &Store,
    session_id: &str,
    release: Release,
    fresh: &Grant,
) -> anyhow::Result<()> {
    for (meter, reset) in [
        (Meter::FiveHour, fresh.five_hour),
        (Meter::SevenDay, fresh.seven_day),
    ] {
        if let Some(reset) = reset {
            store.record_allowance(&Allowance {
                session_id: session_id.to_owned(),
                meter: meter.as_str().to_owned(),
                reset_value: reset,
                release,
            })?;
        }
    }
    Ok(())
}

/// The grant as the session now holds it: `fresh` for the meters it
/// granted, the live allowance already held for the rest. The merge never
/// replaces: a release while only the 5-hour window is spent must not
/// read as wiping an existing 7-day allowance.
pub fn merged(store: &Store, session_id: &str, fresh: &Grant, now_ms: i64) -> Grant {
    Grant {
        five_hour: fresh
            .five_hour
            .or_else(|| prior_live(store, session_id, "5h", now_ms)),
        seven_day: fresh
            .seven_day
            .or_else(|| prior_live(store, session_id, "7d", now_ms)),
    }
}

/// The live prior allowance a session holds for one meter. A store error
/// reads as nothing held: this only shapes the record, never the gate.
fn prior_live(store: &Store, session_id: &str, meter: &str, now_ms: i64) -> Option<i64> {
    store
        .load_session_allowances(session_id)
        .ok()?
        .into_iter()
        .filter(|allowance| allowance.meter == meter)
        .map(|allowance| allowance.reset_value)
        .filter(|reset| reset.saturating_mul(1000) > now_ms)
        .max()
}

/// Grant `release` to a session from the TUI, ahead of time: the current
/// 5-hour window whether or not it is exhausted, and the 7-day window
/// only when it is ([`grant_ahead`]). Records the allowances and the
/// `released` row (`extra.via = "tui"`), and returns what was granted.
/// Fails, writing nothing, when no meter reading names a window to grant
/// for.
pub fn grant_from_tui(
    store: &Store,
    session_id: &str,
    release: Release,
    gates: &GatesConfig,
    now_ms: i64,
) -> anyhow::Result<Grant> {
    let meters = store
        .load_meters(GATED_BACKEND)?
        .map(|meters| meters.snapshot);
    let fresh = grant_ahead(meters.as_ref().map(Meters::over), now_ms);
    if fresh.five_hour.is_none() && fresh.seven_day.is_none() {
        bail!("no meter reading names a window to release yet");
    }
    record_grant(store, session_id, release, &fresh)?;
    let merged = merged(store, session_id, &fresh, now_ms);
    let mut row = released_row(
        session_id,
        GATED_BACKEND,
        release,
        &merged,
        meters.as_ref(),
        gates,
        now_ms,
    );
    tag_via_tui(&mut row);
    store.record_request(&row)?;
    Ok(fresh)
}

/// Close a session's gate from the TUI: delete its allowances, so the
/// gate applies again from its next request, and record a `revoked` row.
/// Returns how many allowances went; with none held, nothing is written.
pub fn revoke_from_tui(
    store: &Store,
    session_id: &str,
    gates: &GatesConfig,
    now_ms: i64,
) -> anyhow::Result<u64> {
    let gone = store.delete_session_allowances(session_id)?;
    if gone == 0 {
        return Ok(0);
    }
    let meters = store
        .load_meters(GATED_BACKEND)?
        .map(|meters| meters.snapshot);
    let mut row = gate_row(
        RowKind::Revoked,
        session_id,
        GATED_BACKEND,
        meters.as_ref(),
        gates,
        now_ms,
    );
    row.extra = Some(json!({ "allowances": gone }));
    tag_via_tui(&mut row);
    store.record_request(&row)?;
    Ok(gone)
}

/// The `released` row: which meters the session now holds a release for
/// (`grant`, the merged view, nulls included, as ctp's row carried its
/// whole allowance entry) and which marker or control granted it.
pub fn released_row(
    session_id: &str,
    backend_id: &str,
    release: Release,
    grant: &Grant,
    stale_meters: Option<&Value>,
    gates: &GatesConfig,
    now_ms: i64,
) -> RequestRow {
    let mut row = gate_row(
        RowKind::Released,
        session_id,
        backend_id,
        stale_meters,
        gates,
        now_ms,
    );
    // The fiveHour/sevenDay row fields, in the kind-specific payload
    // column (the schema has no dedicated columns), and which release
    // it is. Rows from before the plan marker carry no `release`: they
    // were all overage.
    row.extra = Some(json!({
        "fiveHour": grant.five_hour,
        "sevenDay": grant.seven_day,
        "release": match release {
            Release::Overage => "overage",
            Release::Plan => "plan",
        },
    }));
    row
}

/// Mark a row as written by the TUI's controls rather than a marker.
fn tag_via_tui(row: &mut RequestRow) {
    if let Some(Value::Object(extra)) = &mut row.extra {
        extra.insert("via".to_owned(), json!("tui"));
    }
}

/// A proxy-written gate row: no duration, no usage, never priced, and
/// excluded from API measurements by its kind. It carries the last-seen
/// meter snapshot the change rested on (row parity with `blocked`: a
/// blocked session never refreshes meters, so this is often the same
/// spent reading the next blocked row carries; measure reset lag from
/// response rows only, never these).
fn gate_row(
    kind: RowKind,
    session_id: &str,
    backend_id: &str,
    stale_meters: Option<&Value>,
    gates: &GatesConfig,
    now_ms: i64,
) -> RequestRow {
    RequestRow {
        id: None,
        ts_ms: now_ms,
        duration_ms: None,
        kind: Some(kind),
        frontend: Some("anthropic".to_owned()),
        provider: Some(backend_id.to_owned()),
        route: Some(format!("anthropic:{backend_id}")),
        session_id: Some(session_id.to_owned()),
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
        rate_limits: stale_meters.cloned(),
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
        // A marker is stripped and recorded whether or not the gate is
        // armed: the row must not claim the gate was on when it wasn't.
        gate_on: Some(gates.quota_enabled),
        cold_on: Some(gates.cold_enabled),
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

#[cfg(test)]
mod tests {
    use super::{GATED_BACKEND, grant_from_tui, revoke_from_tui};
    use crate::config::GatesConfig;
    use crate::ir::Release;
    use crate::store::{Allowance, MetersSnapshot, RowKind, Store};
    use serde_json::json;

    const NOW: i64 = 2_000_000_000_000;
    const RESET5H: i64 = 2_000_003_600;
    const RESET7D: i64 = 2_000_500_000;

    fn store_with(util5h: f64, util7d: f64) -> Store {
        let store = Store::open(":memory:").expect("store");
        store
            .save_meters(
                GATED_BACKEND,
                &MetersSnapshot {
                    updated_ms: NOW,
                    snapshot: json!({
                        "util5h": util5h, "reset5h": RESET5H,
                        "util7d": util7d, "reset7d": RESET7D,
                        "overageInUse": false,
                    }),
                },
            )
            .expect("meters");
        store
    }

    fn gates() -> GatesConfig {
        GatesConfig {
            quota_enabled: true,
            ..GatesConfig::default()
        }
    }

    /// Ahead of time: the 5-hour window though it is not exhausted, and
    /// not the healthy 7-day one. The row says the TUI granted it.
    #[test]
    fn a_tui_grant_opens_the_current_five_hour_window_ahead_of_time() {
        let store = store_with(0.40, 0.20);
        let grant = grant_from_tui(&store, "ses-a", Release::Plan, &gates(), NOW).expect("grant");
        assert_eq!(grant.five_hour, Some(RESET5H));
        assert_eq!(grant.seven_day, None);
        assert_eq!(
            store.load_session_allowances("ses-a").expect("allowances"),
            vec![Allowance {
                session_id: "ses-a".to_owned(),
                meter: "5h".to_owned(),
                reset_value: RESET5H,
                release: Release::Plan,
            }]
        );
        let rows = store.requests_since(0, 10).expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, Some(RowKind::Released));
        let extra = rows[0].extra.as_ref().expect("extra");
        assert_eq!(extra["via"], "tui");
        assert_eq!(extra["release"], "plan");
        assert_eq!(extra["fiveHour"], RESET5H);
        assert_eq!(rows[0].gate_on, Some(true));
    }

    /// An exhausted 7-day window is granted too.
    #[test]
    fn a_tui_grant_covers_an_exhausted_seven_day_window() {
        let store = store_with(0.40, 0.995);
        let grant =
            grant_from_tui(&store, "ses-a", Release::Overage, &gates(), NOW).expect("grant");
        assert_eq!(grant.seven_day, Some(RESET7D));
    }

    /// No meter reading: nothing to grant for, nothing written.
    #[test]
    fn a_tui_grant_without_meters_fails_and_writes_nothing() {
        let store = Store::open(":memory:").expect("store");
        assert!(grant_from_tui(&store, "ses-a", Release::Plan, &gates(), NOW).is_err());
        assert!(store.load_allowances().expect("allowances").is_empty());
        assert!(store.requests_since(0, 10).expect("rows").is_empty());
    }

    /// Revoking deletes the session's allowances only, records one
    /// `revoked` row, and with nothing held writes nothing.
    #[test]
    fn a_tui_revoke_closes_one_session_s_gate() {
        let store = store_with(0.40, 0.20);
        grant_from_tui(&store, "ses-a", Release::Overage, &gates(), NOW).expect("grant");
        grant_from_tui(&store, "ses-b", Release::Overage, &gates(), NOW).expect("grant");
        assert_eq!(
            revoke_from_tui(&store, "ses-a", &gates(), NOW).expect("revoke"),
            1
        );
        assert!(
            store
                .load_session_allowances("ses-a")
                .expect("a")
                .is_empty()
        );
        assert_eq!(store.load_session_allowances("ses-b").expect("b").len(), 1);
        let rows = store.requests_since(0, 10).expect("rows");
        let revoked: Vec<_> = rows
            .iter()
            .filter(|row| row.kind == Some(RowKind::Revoked))
            .collect();
        assert_eq!(revoked.len(), 1);
        assert_eq!(revoked[0].session_id.as_deref(), Some("ses-a"));
        assert_eq!(revoked[0].extra.as_ref().expect("extra")["via"], "tui");

        assert_eq!(
            revoke_from_tui(&store, "ses-a", &gates(), NOW).expect("again"),
            0
        );
        assert_eq!(
            store
                .requests_since(0, 10)
                .expect("rows")
                .iter()
                .filter(|row| row.kind == Some(RowKind::Revoked))
                .count(),
            1,
            "nothing held, nothing recorded"
        );
    }
}
