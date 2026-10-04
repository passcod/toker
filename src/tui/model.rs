//! The dashboard's aggregation model: display rows in, snapshot out.
//!
//! Pure over [`DisplayRow`]s — the store's narrow display-window
//! projection (invariant 7), no terminal types, no clocks, no store —
//! so every panel number is testable against synthetic row sets. The
//! loop in the parent module owns time and I/O; [`aggregate`] receives
//! `now_ms`, the window length, the meter lookback (a superset of the
//! window — a burn rate needs a span a display window cannot hold)
//! and the local day's start as data, and the quota section is
//! [super::quota]'s aggregation of that lookback.
//!
//! Invariant 3 (absence ≠ zero) is the design rule here: a NULL column is
//! never read as zero. Sums that would need a missing operand stay `None`
//! (`input + cache_read` needs both present), a session's model stays
//! `None` until a row actually reports one, the billed total stays
//! `None` when no billed row exists, and the quota section stays `None`
//! when no row carries a meter snapshot — the view renders each of those
//! as an explicit "no data"-shaped string or no panel at all, so "no
//! rows" and "rows with zero" are visibly different states. The one
//! deliberate zero is the count of requests without cost data: a count
//! over known rows is a real number.

use std::collections::HashMap;

use super::quota::QuotaAgg;
use crate::store::{CostKind, DisplayRow, RowKind, is_api_measurement};

/// The label for rows grouped without a session id (NULL `session_id`).
pub(crate) const NO_SESSION: &str = "-";

/// Everything one dashboard frame needs, computed from one window of rows
/// plus the ledger's total row count.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Snapshot {
    /// The configured window, in minutes (`--window-mins`).
    pub window_mins: u64,
    /// The frame's reference time (epoch ms) — bucket and age math anchor here.
    pub now_ms: i64,
    /// Total rows in the ledger, from the count query.
    pub total_requests: i64,
    /// API-measurement rows in the window (proxy-written kinds excluded).
    pub window_requests: usize,
    /// True when the window holds no rows at all — the explicit "no data"
    /// signal, distinct from a window of rows that measured nothing.
    pub window_empty: bool,
    /// Sessions, most-recent-first.
    pub sessions: Vec<SessionAgg>,
    /// Billed-cost aggregation over the window's measurements.
    pub spend: SpendAgg,
    /// Request-rate aggregation over the window's measurements.
    pub rate: RateAgg,
    /// `kind = error` rows in the window (any row kind counts, not just
    /// measurements — proxy-written rows are the only source).
    pub errors: usize,
    /// `kind = fidelity-drift` rows in the window (invariant 5: drift is a
    /// visible metric, not a hoped-for absence).
    pub drift: usize,
    /// The rate & quota section, from the meter lookback (rows beyond
    /// the display window — a burn needs a span a display window cannot
    /// provide). `None` when no row carries a meter snapshot: the panel
    /// renders nothing, never zeros (invariant 3's per-backend rule).
    pub quota: Option<QuotaAgg>,
}

/// One session's aggregate over the window. All "latest" values are by
/// `ts_ms`, not input order — the ledger is insert-only and rows arrive
/// slightly out of order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionAgg {
    /// The session id, or [`NO_SESSION`] for the NULL-session group.
    pub session: String,
    /// Measurement rows in this session.
    pub requests: usize,
    /// The latest non-NULL model; `None` when no row reported one.
    pub model: Option<String>,
    /// The latest row's `input + cache_read`. `None` unless that row has
    /// both buckets — a missing operand is unknown, not zero.
    pub input_now: Option<i64>,
    /// The high-water mark of `input + cache_read` over the rows where both
    /// buckets are known; `None` when none are.
    pub input_peak: Option<i64>,
    /// Sum of reported output tokens; `None` when no row reported output.
    pub output_total: Option<i64>,
    /// Newest row timestamp (epoch ms).
    pub latest_ts_ms: i64,
}

/// Billed-cost aggregation. Only `cost_kind = billed` is summed today;
/// rows with other cost kinds are counted ([`SpendAgg::other_cost_kinds`])
/// rather than folded in — never silently dropped, never conflated.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SpendAgg {
    /// Sum of billed `cost_usd`; `None` when the window has no billed row.
    pub billed_total: Option<f64>,
    /// How many rows produced [`SpendAgg::billed_total`].
    pub billed_requests: usize,
    /// Per-provider·model billed breakdown, largest billed first.
    pub breakdown: Vec<ProviderModelSpend>,
    /// Measurement rows with NULL cost — rendered as "no cost data".
    pub no_cost_data: usize,
    /// Measurement rows carrying a non-billed cost kind (estimated,
    /// plan-equivalent): priced, so not "no cost data", but not billed
    /// either. Zero until phase 2+ introduces those kinds.
    pub other_cost_kinds: usize,
}

/// One per-provider·model billed line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProviderModelSpend {
    /// The `provider` column; `None` rendered as [`NO_SESSION`]-style dash.
    pub provider: Option<String>,
    /// The `model` column; `None` rendered the same way.
    pub model: Option<String>,
    /// Billed dollars in this provider·model group.
    pub billed: f64,
    /// Billed requests in this group.
    pub requests: usize,
}

/// Request-rate aggregation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RateAgg {
    /// Requests per minute over the whole window (count / minutes).
    pub per_minute: f64,
    /// One bucket per minute in the window, oldest first. Measurement rows
    /// fill `requests`; error rows fill `errors` so the sparkline can flag
    /// them.
    pub buckets: Vec<MinuteBucket>,
}

/// One sparkline bucket (one minute of the window).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MinuteBucket {
    /// API-measurement rows in this minute.
    pub requests: usize,
    /// `kind = error` rows in this minute, for the flagged sparkline marks.
    pub errors: usize,
}

/// The pre-refresh placeholder: an empty window of the right shape.
pub(crate) fn empty(window_mins: u64) -> Snapshot {
    aggregate(&[], None, window_mins, 0, 0)
}

/// Aggregate one window of display rows (the narrow projection the
/// display tick reads) into a [`Snapshot`]. `now_ms` is
/// the frame's reference time (bucket edges anchor to it); `total_requests` is
/// the ledger's total row count, kept distinct from the window so the
/// header can show both. `quota` is the PRECOMPUTED quota section (the
/// meter lookback aggregation, [`super::quota::aggregate`] over the
/// 7-day read), built on its own slower cadence — passing it in keeps
/// this function pure over cheap inputs. Rows may arrive
/// in any order — "latest" is decided by `ts_ms` throughout.
pub(crate) fn aggregate(
    rows: &[DisplayRow],
    quota: Option<&QuotaAgg>,
    window_mins: u64,
    now_ms: i64,
    total_requests: i64,
) -> Snapshot {
    let window_mins = window_mins.max(1) as usize;

    // Latest-by-timestamp needs ts order; sort a copy of references so the
    // caller's slice is untouched.
    let mut sorted: Vec<&DisplayRow> = rows.iter().collect();
    sorted.sort_by_key(|row| row.ts_ms);

    let mut sessions: Vec<SessionAgg> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut spend = SpendAgg {
        billed_total: None,
        billed_requests: 0,
        breakdown: Vec::new(),
        no_cost_data: 0,
        other_cost_kinds: 0,
    };
    let mut breakdown_index: HashMap<(Option<String>, Option<String>), usize> = HashMap::new();
    let mut buckets = vec![MinuteBucket::default(); window_mins];
    let mut errors = 0;
    let mut drift = 0;
    let mut window_requests = 0;

    for row in sorted {
        // Error/drift metrics see every row kind — proxy-written rows are
        // their only source — and error rows mark their minute's bucket.
        match row.kind {
            Some(RowKind::Error) => {
                errors += 1;
                buckets[bucket_of(row.ts_ms, now_ms, window_mins)].errors += 1;
            }
            Some(RowKind::FidelityDrift) => drift += 1,
            _ => {}
        }
        if !is_api_measurement(row.kind) {
            continue; // every panel below is measurements-only
        }
        window_requests += 1;
        buckets[bucket_of(row.ts_ms, now_ms, window_mins)].requests += 1;

        // Sessions: group by id (NULL under the dash), accumulate per row
        // in ts order so the last write is the latest row's value.
        let label = row
            .session_id
            .clone()
            .unwrap_or_else(|| NO_SESSION.to_owned());
        let slot = *index.entry(label.clone()).or_insert_with(|| {
            sessions.push(SessionAgg {
                session: label.clone(),
                requests: 0,
                model: None,
                input_now: None,
                input_peak: None,
                output_total: None,
                latest_ts_ms: row.ts_ms,
            });
            sessions.len() - 1
        });
        let session = &mut sessions[slot];
        session.requests += 1;
        session.latest_ts_ms = row.ts_ms;
        if let Some(model) = &row.model
            && session.model.as_deref() != Some(model.as_str())
        {
            session.model = row.model.clone();
        }
        // input + cache_read needs both buckets; missing operand = unknown.
        session.input_now = match (row.input, row.cache_read) {
            (Some(input), Some(cache_read)) => Some(input + cache_read),
            _ => None,
        };
        if let Some(now) = session.input_now
            && session.input_peak.is_none_or(|peak| now > peak)
        {
            session.input_peak = Some(now);
        }
        if let Some(output) = row.output {
            session.output_total = Some(session.output_total.unwrap_or(0) + output);
        }

        // Cost: billed sums, everything else stays explicit (above).
        match (row.cost_usd, row.cost_kind) {
            (Some(cost), Some(CostKind::Billed)) => {
                spend.billed_total = Some(spend.billed_total.unwrap_or(0.0) + cost);
                spend.billed_requests += 1;
                let key = (row.provider.clone(), row.model.clone());
                let slot = *breakdown_index.entry(key.clone()).or_insert_with(|| {
                    spend.breakdown.push(ProviderModelSpend {
                        provider: key.0.clone(),
                        model: key.1.clone(),
                        billed: 0.0,
                        requests: 0,
                    });
                    spend.breakdown.len() - 1
                });
                let entry = &mut spend.breakdown[slot];
                entry.billed += cost;
                entry.requests += 1;
            }
            (Some(_), _) => spend.other_cost_kinds += 1,
            (None, _) => spend.no_cost_data += 1,
        }
    }

    // Most-recent-first; session name breaks ties for a stable order.
    sessions.sort_by(|a, b| {
        b.latest_ts_ms
            .cmp(&a.latest_ts_ms)
            .then_with(|| a.session.cmp(&b.session))
    });
    spend.breakdown.sort_by(|a, b| {
        b.billed
            .partial_cmp(&a.billed)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.provider
                    .cmp(&b.provider)
                    .then_with(|| a.model.cmp(&b.model))
            })
    });

    Snapshot {
        window_mins: window_mins as u64,
        now_ms,
        total_requests,
        window_requests,
        window_empty: rows.is_empty(),
        sessions,
        spend,
        rate: RateAgg {
            per_minute: window_requests as f64 / window_mins as f64,
            buckets,
        },
        errors,
        drift,
        // Precomputed by the caller on the QUOTA cadence — the meter
        // lookback's 20k-row read and its aggregation are far too heavy
        // for the display tick (the spin this split fixed).
        quota: quota.cloned(),
    }
}

/// The minute bucket a timestamp falls into: index `window_mins - 1` is the
/// minute ending at `now_ms`, index 0 the oldest. Rows exactly on the
/// oldest edge belong to bucket 0; rows from a clock skewed into the
/// future clamp into the newest bucket rather than panicking.
fn bucket_of(ts_ms: i64, now_ms: i64, window_mins: usize) -> usize {
    let mins_ago = now_ms
        .saturating_sub(ts_ms)
        .div_euclid(60_000)
        .clamp(0, window_mins as i64 - 1);
    window_mins - 1 - mins_ago as usize
}

#[cfg(test)]
mod tests {
    use super::{NO_SESSION, Snapshot};
    use crate::store::{CostKind, DisplayRow, RowKind};
    use crate::tui::testrows::{
        as_display_rows, as_meter_rows, bare, billed, display_bare, display_billed,
        display_kind_row, kind_row, metered_full,
    };
    use serde_json::json;

    /// A fixed frame time: 2026-01-21T22:13:20Z-ish, arbitrary but stable.
    const NOW: i64 = 1_769_000_000_000;
    const WINDOW: u64 = 30;

    fn agg(rows: &[DisplayRow], total: i64) -> Snapshot {
        // The tests' display rows carry no meter snapshots (the narrow
        // display shape has none to carry), so the loop's quota
        // section over this window is None — absence, not zeros. The
        // precomputed-section wiring has its own test below.
        super::aggregate(rows, None, WINDOW, NOW, total)
    }

    /// A measurement row a given number of minutes before `NOW`.
    fn mins_ago(mins: i64) -> i64 {
        NOW - mins * 60_000
    }

    #[test]
    fn sessions_group_order_and_latest_values() {
        let rows = vec![
            display_billed(
                mins_ago(5),
                Some("ses-a"),
                "model-a",
                "openrouter",
                100,
                900,
                50,
                0.001,
            ),
            display_billed(
                mins_ago(2),
                Some("ses-a"),
                "model-b",
                "openrouter",
                200,
                800,
                60,
                0.002,
            ),
            display_billed(mins_ago(1), None, "model-c", "openrouter", 30, 0, 10, 0.003),
            display_billed(
                mins_ago(10),
                Some("ses-b"),
                "model-b",
                "openrouter",
                5,
                5,
                5,
                0.004,
            ),
        ];
        let snap = agg(&rows, 4);

        // Most-recent-first; the NULL-session group rides under a dash.
        let names: Vec<&str> = snap.sessions.iter().map(|s| s.session.as_str()).collect();
        assert_eq!(names, vec![NO_SESSION, "ses-a", "ses-b"]);

        let ses_a = &snap.sessions[1];
        assert_eq!(ses_a.requests, 2);
        assert_eq!(ses_a.model.as_deref(), Some("model-b"), "latest model wins");
        assert_eq!(ses_a.input_now, Some(1_000));
        assert_eq!(ses_a.input_peak, Some(1_000));
        assert_eq!(ses_a.output_total, Some(110));
        assert_eq!(ses_a.latest_ts_ms, mins_ago(2));

        let null_session = &snap.sessions[0];
        assert_eq!(null_session.session, NO_SESSION);
        assert_eq!(null_session.requests, 1);
        assert_eq!(
            null_session.input_now,
            Some(30),
            "zero cache_read is a real zero"
        );
    }

    #[test]
    fn absent_operands_stay_unknown_not_zero() {
        // One row with input but no cache_read, one with both, one with
        // neither — in a single session.
        let mut no_cache = display_bare(mins_ago(3));
        no_cache.session_id = Some("ses-x".into());
        no_cache.input = Some(500);
        let mut both = display_bare(mins_ago(2));
        both.session_id = Some("ses-x".into());
        both.input = Some(100);
        both.cache_read = Some(50);
        both.output = Some(7);
        let mut neither = display_bare(mins_ago(1)); // latest row: no tokens at all
        neither.session_id = Some("ses-x".into());

        let snap = agg(&[no_cache, both, neither], 3);
        assert_eq!(snap.sessions.len(), 1);
        let s = &snap.sessions[0];
        assert_eq!(s.requests, 3);
        assert_eq!(
            s.input_now, None,
            "latest row's missing operand makes the sum unknown, not zero"
        );
        assert_eq!(
            s.input_peak,
            Some(150),
            "peak is the max over rows where the sum is known"
        );
        assert_eq!(
            s.output_total,
            Some(7),
            "unknown output rows contribute nothing"
        );
        assert_eq!(s.model, None, "no row ever reported a model");
    }

    #[test]
    fn spend_bills_only_billed_and_counts_the_rest() {
        let rows = vec![
            display_billed(
                mins_ago(9),
                Some("ses-a"),
                "glm",
                "openrouter",
                1,
                1,
                1,
                1.5,
            ),
            display_billed(
                mins_ago(8),
                Some("ses-a"),
                "glm",
                "openrouter",
                1,
                1,
                1,
                2.5,
            ),
            display_billed(mins_ago(7), Some("ses-b"), "gpt", "lunaroute", 1, 1, 1, 1.0),
            display_billed(mins_ago(6), None, "glm", "openrouter", 1, 1, 1, 0.25),
            display_bare(mins_ago(5)), // measurement without cost: no cost data
            display_bare(mins_ago(4)), // …and another
        ];
        let mut priced_unkinded = display_bare(mins_ago(3));
        priced_unkinded.cost_usd = Some(0.75); // cost present, kind NULL: priced, not billed
        let rows = [rows, vec![priced_unkinded]].concat();

        let snap = agg(&rows, 7);
        let spend = &snap.spend;
        assert_eq!(spend.billed_total, Some(5.25));
        assert_eq!(spend.billed_requests, 4);
        assert_eq!(spend.no_cost_data, 2, "NULL cost is counted, never dropped");
        assert_eq!(
            spend.other_cost_kinds, 1,
            "priced-but-unkinded is not silently folded"
        );

        // Grouped by provider·model, not by session: the NULL-session row
        // joins the openrouter·glm line.
        assert_eq!(spend.breakdown.len(), 2);
        assert_eq!(
            (
                spend.breakdown[0].provider.as_deref(),
                spend.breakdown[0].billed
            ),
            (Some("openrouter"), 4.25),
            "largest billed first"
        );
        assert_eq!(spend.breakdown[0].requests, 3);
        assert_eq!(spend.breakdown[0].model.as_deref(), Some("glm"));
        assert_eq!(
            (
                spend.breakdown[1].provider.as_deref(),
                spend.breakdown[1].billed
            ),
            (Some("lunaroute"), 1.0)
        );

        // NULL provider/model group under the dash, not under a fake name.
        let mut no_provider = display_bare(mins_ago(2));
        no_provider.cost_usd = Some(9.0);
        no_provider.cost_kind = Some(CostKind::Billed);
        let snap = agg(&[no_provider], 1);
        let entry = &snap.spend.breakdown[0];
        assert_eq!(entry.provider, None);
        assert_eq!(entry.model, None);
        assert_eq!(entry.billed, 9.0);
    }

    #[test]
    fn billed_zero_is_a_real_zero_not_absence() {
        let mut row = display_bare(mins_ago(1));
        row.cost_usd = Some(0.0);
        row.cost_kind = Some(CostKind::Billed);
        let snap = agg(&[row], 1);
        assert_eq!(snap.spend.billed_total, Some(0.0));
        assert_eq!(snap.spend.billed_requests, 1);
        assert_eq!(snap.spend.no_cost_data, 0);
    }

    #[test]
    fn rate_buckets_per_minute_and_error_flags() {
        // 10 measurement rows in three different minutes, plus one error
        // and one drift row (which must not count as requests).
        let mut rows = Vec::new();
        for _ in 0..6 {
            rows.push(display_bare(mins_ago(3)));
        }
        for _ in 0..3 {
            rows.push(display_bare(mins_ago(2)));
        }
        rows.push(display_bare(mins_ago(0)));
        rows.push(display_kind_row(mins_ago(2), RowKind::Error));
        rows.push(display_kind_row(mins_ago(1), RowKind::FidelityDrift));

        let snap = agg(&rows, 12);
        assert_eq!(snap.window_requests, 10);
        assert_eq!(snap.errors, 1);
        assert_eq!(snap.drift, 1);
        assert!((snap.rate.per_minute - 10.0 / 30.0).abs() < 1e-12);

        // 30 one-minute buckets, oldest first: index = 29 − minutes-ago.
        let requests: Vec<usize> = snap.rate.buckets.iter().map(|b| b.requests).collect();
        assert_eq!(snap.rate.buckets.len(), 30);
        assert_eq!(requests[26], 6);
        assert_eq!(requests[27], 3);
        assert_eq!(requests[29], 1);
        assert_eq!(
            snap.rate.buckets.iter().map(|b| b.requests).sum::<usize>(),
            10
        );

        // The error row flags its own minute's bucket but adds no request.
        assert_eq!(snap.rate.buckets[27].errors, 1);
        assert_eq!(snap.rate.buckets[27].requests, 3);

        // Boundary: a row exactly on the window's oldest edge lands in
        // bucket 0.
        let snap = agg(&[display_bare(NOW - 30 * 60_000)], 1);
        assert_eq!(snap.rate.buckets[0].requests, 1);

        // Clock-skewed future row clamps into the newest bucket.
        let snap = agg(&[display_bare(NOW + 5_000)], 1);
        assert_eq!(snap.rate.buckets[29].requests, 1);
    }

    #[test]
    fn proxy_kinds_are_excluded_from_sessions_spend_and_rate() {
        let rows = vec![
            display_billed(
                mins_ago(1),
                Some("ses-a"),
                "glm",
                "openrouter",
                1,
                1,
                1,
                0.5,
            ),
            display_kind_row(mins_ago(1), RowKind::Blocked),
            display_kind_row(mins_ago(1), RowKind::Released),
            display_kind_row(mins_ago(1), RowKind::Cold),
            display_kind_row(mins_ago(1), RowKind::ColdQuiet),
            display_kind_row(mins_ago(1), RowKind::Awake),
            display_kind_row(mins_ago(1), RowKind::Error),
            display_kind_row(mins_ago(1), RowKind::FidelityDrift),
        ];
        let snap = agg(&rows, 8);

        assert_eq!(snap.window_requests, 1);
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].requests, 1);
        assert_eq!(snap.spend.billed_total, Some(0.5));
        assert_eq!(snap.spend.no_cost_data, 0);
        assert_eq!(snap.errors, 1);
        assert_eq!(snap.drift, 1);
        assert_eq!(
            snap.rate.buckets.iter().map(|b| b.requests).sum::<usize>(),
            1
        );
    }

    #[test]
    fn empty_window_is_no_data_not_zero_data() {
        let snap = agg(&[], 42);
        assert!(snap.window_empty, "no rows at all");
        assert_eq!(snap.total_requests, 42, "the ledger count is still known");
        assert_eq!(snap.window_requests, 0);
        assert!(snap.sessions.is_empty());
        assert_eq!(snap.spend.billed_total, None, "no billed data, not $0");
        assert_eq!(snap.spend.billed_requests, 0);
        assert_eq!(
            snap.spend.no_cost_data, 0,
            "zero rows lacking cost: a real zero"
        );
        assert_eq!(snap.errors, 0);
        assert_eq!(snap.drift, 0);
        assert_eq!(snap.rate.per_minute, 0.0);
        assert_eq!(snap.rate.buckets.len(), 30);

        // The distinction that matters: a window with rows but no cost
        // data is NOT an empty window — absence of billing is visible as
        // a count, and the window is not "no data".
        let snap = agg(&[display_bare(mins_ago(1))], 1);
        assert!(!snap.window_empty);
        assert_eq!(snap.window_requests, 1);
        assert_eq!(snap.spend.billed_total, None, "still no billed data");
        assert_eq!(
            snap.spend.no_cost_data, 1,
            "but explicitly one row without cost"
        );
    }

    #[test]
    fn zero_window_mins_clamps_to_one() {
        let snap = super::aggregate(&[display_bare(NOW)], None, 0, NOW, 1);
        assert_eq!(snap.window_mins, 1);
        assert_eq!(snap.rate.buckets.len(), 1);
        assert_eq!(snap.rate.buckets[0].requests, 1);
        assert_eq!(snap.rate.per_minute, 1.0);
    }

    #[test]
    fn the_quota_section_comes_from_the_lookback_not_the_window() {
        // The wiring this panel rides: the section is aggregated from
        // the meter lookback, which holds rows the display window does
        // not — a burn needs a span a window cannot provide — and rows
        // without meter snapshots leave it absent, never zero-filled.
        let mut in_window = bare(mins_ago(2));
        in_window.rate_limits = Some(json!({"util5h": 0.42, "reset5h": 123, "claim": "five_hour"}));
        let mut older_reading = bare(mins_ago(60));
        older_reading.rate_limits = Some(json!({"util5h": 0.30, "reset5h": 123}));

        let lookback = [older_reading, in_window];
        let quota_section = super::super::quota::aggregate(
            &as_meter_rows(&lookback),
            NOW,
            NOW - 12 * 60 * 60_000,
            NOW.saturating_sub(WINDOW as i64 * 60_000),
        );
        let snap = super::aggregate(
            &as_display_rows(&[lookback[1].clone()]),
            quota_section.as_ref(),
            WINDOW,
            NOW,
            2,
        );
        let quota = snap.quota.expect("the lookback carries meter snapshots");
        assert_eq!(quota.meters.len(), 1, "only the 5h meter is carried");
        // Every figure is from the NEWEST reading, which lives in the
        // window here — and the claim from the same reading.
        assert!((quota.meters[0].util - 0.42).abs() < 1e-9);
        assert_eq!(quota.binding.as_deref(), Some("five_hour"));

        // A lookback of rows without meters: no section at all.
        let meterless = [bare(mins_ago(90))];
        let quota_section = super::super::quota::aggregate(
            &as_meter_rows(&meterless),
            NOW,
            NOW,
            NOW.saturating_sub(WINDOW as i64 * 60_000),
        );
        let snap = super::aggregate(
            &[display_bare(mins_ago(1))],
            quota_section.as_ref(),
            WINDOW,
            NOW,
            2,
        );
        assert_eq!(snap.quota, None);
    }

    #[test]
    fn out_of_order_input_still_picks_latest_by_ts() {
        // Rows deliberately out of ts order: the newer one must win as
        // "latest" for model and input_now.
        let rows = vec![
            display_billed(
                mins_ago(1),
                Some("ses-a"),
                "newer-model",
                "openrouter",
                10,
                20,
                1,
                0.1,
            ),
            display_billed(
                mins_ago(4),
                Some("ses-a"),
                "older-model",
                "openrouter",
                100,
                200,
                2,
                0.2,
            ),
        ];
        let snap = agg(&rows, 2);
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].model.as_deref(), Some("newer-model"));
        assert_eq!(snap.sessions[0].input_now, Some(30));
        assert_eq!(snap.sessions[0].input_peak, Some(300));
        assert_eq!(snap.sessions[0].latest_ts_ms, mins_ago(1));
    }

    // ── the narrow read vs the full-row read (the refactor's proof) ────

    #[test]
    fn the_narrow_read_aggregates_identically_to_the_full_row_read() {
        use crate::store::Store;

        let store = Store::open(":memory:").expect("scratch store");
        // A mixed ledger, every rule the aggregation has:
        // - billed rows across providers and models, several carrying
        //   `extra.serving_provider` (the production openrouter shape):
        //   the full-row read parses that JSON, the narrow read never
        //   fetches it, and the breakdown must not care either way —
        //   its label is the backend `provider` column, never the
        //   serving provider;
        // - a plan-equivalent row (priced, never billed), NULL-cost
        //   rows, and a real 0.0 billed cost;
        // - error and fidelity-drift rows (the error counter, the drift
        //   counter, the sparkline flag — never requests);
        // - NULL sessions (the dash group) and a NULL-provider·model
        //   billed row (the breakdown's dash group);
        // - two rows sharing one timestamp (the id tie-break decides
        //   the session's latest model; both reads must preserve it);
        // - meter snapshots on two measurements, for the precomputed
        //   quota section the loop passes in — columns the display
        //   read does not even fetch.
        //
        // Insertion order is deliberately not ts order, so the reads'
        // (ts, id) ordering is exercised, not the seed's.
        let relace = |row: crate::store::RequestRow| {
            let mut row = row;
            row.extra = Some(json!({"serving_provider": "Relace"}));
            row
        };
        let rows = vec![
            billed(
                mins_ago(12),
                Some("ses-a"),
                "z-ai/glm-5.3",
                "openrouter",
                12_000,
                6_000,
                800,
                2.5,
            ), // id 1
            {
                let mut row = kind_row(mins_ago(7), RowKind::Error);
                row.status = Some(500);
                row.error_type = Some("upstream".to_owned());
                row // id 2
            },
            relace(billed(
                mins_ago(25),
                Some("ses-a"),
                "z-ai/glm-5.3",
                "openrouter",
                10_000,
                5_000,
                700,
                1.5,
            )), // id 3
            {
                let mut row = metered_full(
                    mins_ago(30),
                    json!({"util5h": 0.20, "reset5h": (NOW + 3 * 60 * 60_000) / 1000}),
                );
                row.session_id = Some("ses-d".to_owned());
                row.model = Some("claude-opus-5".to_owned());
                row.provider = Some("anthropic_sub".to_owned());
                row.input = Some(4_000);
                row.cache_read = Some(1_000);
                row.output = Some(200);
                row.cost_usd = Some(1.5);
                row.cost_kind = Some(CostKind::PlanEquivalent);
                row // id 4
            },
            {
                let mut row = billed(
                    mins_ago(15),
                    None,
                    "z-ai/glm-5.3",
                    "lunaroute",
                    200,
                    50,
                    20,
                    1.0,
                );
                row.extra = Some(json!({"serving_provider": "Elsewhere"}));
                row // id 5
            },
            {
                let mut row = bare(mins_ago(10));
                row.session_id = Some("ses-c".to_owned());
                row.model = Some("m-first".to_owned());
                row // id 6 — same ts as the next row
            },
            {
                let mut row = bare(mins_ago(10));
                row.session_id = Some("ses-c".to_owned());
                row.model = Some("m-second".to_owned());
                row // id 7 — the id tie-break makes this the later row
            },
            {
                let mut row = billed(
                    mins_ago(20),
                    None,
                    "z-ai/glm-5.3",
                    "openrouter",
                    1_000,
                    0,
                    100,
                    0.5,
                );
                row.extra = Some(json!({"serving_provider": "Kimi"}));
                row // id 8
            },
            {
                let mut row = bare(mins_ago(8));
                row.session_id = Some("ses-a".to_owned());
                row.model = Some("claude-opus-5".to_owned());
                row.provider = Some("anthropic_sub".to_owned());
                row.input = Some(5_000);
                row.cache_read = Some(2_000);
                row.output = Some(300);
                row.cost_usd = Some(9.0);
                row.cost_kind = Some(CostKind::PlanEquivalent);
                row // id 9
            },
            {
                let mut row = metered_full(
                    mins_ago(4),
                    json!({
                        "util5h": 0.42, "reset5h": (NOW + 3 * 60 * 60_000) / 1000,
                        "status5h": "allowed", "claim": "five_hour",
                    }),
                );
                row.session_id = Some("ses-d".to_owned());
                row.model = Some("claude-opus-5".to_owned());
                row.provider = Some("anthropic_sub".to_owned());
                row.input = Some(6_000);
                row.cache_read = Some(3_000);
                row.output = Some(400);
                row.cost_usd = Some(1.7);
                row.cost_kind = Some(CostKind::PlanEquivalent);
                row.gate_on = Some(true);
                row // id 10
            },
            billed(
                mins_ago(18),
                Some("ses-b"),
                "openai/gpt-5.2",
                "openrouter",
                500,
                100,
                50,
                1.0,
            ), // id 11 — billed, no `extra` at all
            {
                let mut row = kind_row(mins_ago(13), RowKind::FidelityDrift);
                row.drift_digest = Some("sha256:drift1".to_owned());
                row // id 12
            },
            {
                let mut row = bare(mins_ago(6));
                row.input = Some(50); // no cache_read: the sum is unknown
                row // id 13
            },
            relace(billed(
                mins_ago(11),
                Some("ses-a"),
                "z-ai/glm-5.3",
                "openrouter",
                300,
                300,
                0,
                0.0,
            )), // id 14 — a real zero billed cost
            {
                let mut row = bare(mins_ago(9));
                row.session_id = Some("ses-c".to_owned());
                row.input = Some(10);
                row.cache_read = Some(10);
                row.output = Some(5);
                row.cost_usd = Some(3.5);
                row.cost_kind = Some(CostKind::Billed);
                row // id 15 — NULL provider AND model: the dash group
            },
            bare(mins_ago(5)), // id 16
        ];
        let total = rows.len() as i64;
        store
            .record_requests(&rows)
            .expect("seed the scratch ledger");

        // OLD shape: the full-row materialisation the display refresh
        // used to pay — 59 columns, six JSON parses per row —
        // projected onto the ten fields the aggregation reads. The
        // projection is test scaffolding the real old path never ran
        // (it read the fields off the full rows directly), so the old
        // totals below are UPPER bounds.
        let full = store.requests_since(0, 10_000).expect("full-row read");
        assert_eq!(full.len(), rows.len(), "the fixture is under the cap");
        let quota = quota_of(&store);
        let via_old = super::aggregate(&as_display_rows(&full), quota.as_ref(), WINDOW, NOW, total);

        // NEW path: the narrow read the display cadence now uses —
        // ten columns, no JSON parse.
        let narrow = store.display_rows_since(0, 10_000).expect("narrow read");
        assert_eq!(
            narrow.len(),
            full.len(),
            "no filter: the display read keeps every row kind"
        );
        let via_new = super::aggregate(&narrow, quota.as_ref(), WINDOW, NOW, total);

        assert_eq!(
            via_old, via_new,
            "the narrow read must aggregate identically to the full-row read"
        );

        // Spot figures pinning WHICH rules had to survive the switch —
        // equal-but-wrong would still fail here.
        let snap = via_new;
        assert!(!snap.window_empty);
        assert_eq!(
            snap.window_requests, 14,
            "measurements only — proxy kinds never count"
        );
        assert_eq!(snap.errors, 1);
        assert_eq!(snap.drift, 1);
        assert_eq!(snap.spend.billed_total, Some(10.0)); // 1.5+0.5+1+1+2.5+0+3.5, exact in f64
        assert_eq!(snap.spend.billed_requests, 7);
        assert_eq!(
            snap.spend.no_cost_data, 4,
            "NULL cost is counted, never dropped"
        );
        assert_eq!(
            snap.spend.other_cost_kinds, 3,
            "plan-equivalent rows are priced but never billed"
        );
        // Breakdown: largest billed first, the equal-cost pair
        // tie-broken by provider then model, the NULL-provider row
        // grouped under the dash — and every label is the backend
        // `provider` column, never `extra.serving_provider` (no
        // "Relace"/"Elsewhere"/"Kimi" anywhere).
        let breakdown = &snap.spend.breakdown;
        assert_eq!(breakdown.len(), 4);
        assert_eq!(
            (
                breakdown[0].provider.as_deref(),
                breakdown[0].model.as_deref(),
                breakdown[0].billed,
                breakdown[0].requests,
            ),
            (Some("openrouter"), Some("z-ai/glm-5.3"), 4.5, 4)
        );
        assert_eq!(
            (breakdown[1].provider.as_deref(), breakdown[1].billed),
            (None, 3.5),
            "the NULL-provider·model row groups under the dash"
        );
        assert_eq!(
            breakdown[2].provider.as_deref(),
            Some("lunaroute"),
            "equal-cost tie-break is provider-asc"
        );
        assert_eq!(
            breakdown[3].provider.as_deref(),
            Some("openrouter"),
            "…then model-asc within the provider"
        );
        // Sessions, most-recent-first, the NULL-session group under
        // the dash; ses-c's model comes from the LATER id of the
        // equal-ts pair — the narrow read preserved the (ts, id)
        // ordering that decides it.
        let names: Vec<&str> = snap.sessions.iter().map(|s| s.session.as_str()).collect();
        assert_eq!(names, vec!["ses-d", NO_SESSION, "ses-a", "ses-c", "ses-b"]);
        assert_eq!(snap.sessions[3].model.as_deref(), Some("m-second"));
        assert_eq!(
            snap.sessions[1].input_peak,
            Some(1_000),
            "a zero cache_read is a real zero"
        );
        // The error row flagged its own minute's bucket and added no
        // request there.
        assert_eq!(
            snap.rate.buckets[22],
            super::MinuteBucket {
                requests: 0,
                errors: 1
            }
        );
        // The meter-carrying measurements still fed the precomputed
        // quota section — through the meter read, not the display one.
        let quota = snap
            .quota
            .as_ref()
            .expect("the seeded meters make a section");
        assert_eq!(quota.meters.len(), 1, "only the 5h meter is carried");
        assert_eq!(quota.binding.as_deref(), Some("five_hour"));
        assert!(quota.gate_on && !quota.gate_assumed);

        // The absent case is parity too: a window past every row reads
        // empty on both paths — "no data", not "rows that measured
        // nothing".
        let via_old = super::aggregate(
            &as_display_rows(&store.requests_since(NOW, 10).expect("old read")),
            None,
            WINDOW,
            NOW,
            total,
        );
        let via_new = super::aggregate(
            &store.display_rows_since(NOW, 10).expect("narrow read"),
            None,
            WINDOW,
            NOW,
            total,
        );
        assert_eq!(via_old, via_new);
        assert!(via_new.window_empty);
    }

    /// The precomputed quota section over the scratch ledger, the way
    /// the loop's quota cadence builds it (the meter read, the 12 h
    /// lookback, the 30 m window anchor).
    fn quota_of(store: &crate::store::Store) -> Option<super::super::quota::QuotaAgg> {
        super::super::quota::aggregate(
            &store.meter_rows_since(0, 10_000).expect("meter lookback"),
            NOW,
            NOW - 12 * 60 * 60_000,
            NOW.saturating_sub(WINDOW as i64 * 60_000),
        )
    }

    // ── the read's cost, measured ─────────────────────────────────────

    #[test]
    #[ignore = "a timing probe, not a correctness gate: seeds a scratch \
                ledger at display scale (10 000 window rows — the cap, a \
                pathologically hot 30-minute ledger) and times 100 \
                refreshes of both display-tick paths in the debug build; \
                run deliberately with --ignored"]
    fn display_window_timing_narrow_read_vs_full_row_read() {
        use crate::store::{RequestRow, Store};
        use std::time::{Duration, Instant};

        const ITERATIONS: u32 = 100;
        const SEED: i64 = 10_000; // the display window's ROW_CAP
        const WINDOW_MS: i64 = 30 * 60_000;
        const CAP: u64 = 10_000;

        // A scratch ledger under /tmp/opencode — a file DB, WAL on disk,
        // the shape the loop actually reads. Never the live one.
        let dir = std::path::PathBuf::from("/tmp/opencode")
            .join(format!("toker-display-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.join("bench.db")).expect("open the scratch ledger");

        // Production shape: billed openrouter rows carrying
        // `extra.serving_provider` and a `usage_raw` blob (the JSON and
        // text the old read fetched and parsed), anthropic-sub
        // plan-equivalent rows carrying full meter snapshots, NULL-cost
        // measurements, and error/drift rows — across sessions, models,
        // and the whole 30-minute window. Plus pre-window rows the
        // window itself must exclude.
        let now_ms = 2_000_000_000_000;
        let since_ms = now_ms - WINDOW_MS;
        let reset_5h = (now_ms + 3 * 60 * 60_000) / 1000;
        let sessions: [Option<&str>; 4] =
            [Some("ses-alpha"), Some("ses-beta"), Some("ses-gamma"), None];
        let models = ["z-ai/glm-5.3", "openai/gpt-5.2", "anthropic/claude-haiku"];
        let mut batch: Vec<RequestRow> = Vec::new();
        for i in 0..SEED {
            let ts_ms = since_ms + (i * WINDOW_MS) / SEED;
            let row = match i % 20 {
                0..=8 => {
                    // Billed openrouter, serving-provider label attached.
                    let serving = ["Relace", "Kimi", "Elsewhere"][(i % 3) as usize];
                    let mut row = billed(
                        ts_ms,
                        sessions[(i / 20) as usize % sessions.len()],
                        models[(i / 7) as usize % models.len()],
                        "openrouter",
                        1_000 + i % 900,
                        40_000 + i % 5_000,
                        i % 800,
                        0.001 + (i % 50) as f64 / 10_000.0,
                    );
                    row.extra = Some(json!({"serving_provider": serving}));
                    row.usage_raw = Some(
                        r#"{"prompt_tokens":1234,"cost":0.0021,"cost_details":{"upstream":"0.0019"}}"#
                            .to_owned(),
                    );
                    row
                }
                9..=15 => {
                    // Anthropic-sub plan-equivalent, full meter snapshot.
                    let q = |util: f64| (util * 100.0).round() / 100.0;
                    let mut row = metered_full(
                        ts_ms,
                        json!({
                            "util5h": q((i % 900) as f64 / 900.0 * 0.95),
                            "reset5h": reset_5h,
                            "util7d": q(0.10 + i as f64 / SEED as f64 * 0.60),
                            "status5h": "allowed", "claim": "five_hour",
                        }),
                    );
                    row.session_id = Some("ses-anthropic".to_owned());
                    row.model = Some("claude-opus-5".to_owned());
                    row.provider = Some("anthropic_sub".to_owned());
                    row.input = Some(30_000 + i % 4_000);
                    row.cache_read = Some(80_000);
                    row.output = Some(1_000 + i % 300);
                    row.cost_usd = Some(1.5);
                    row.cost_kind = Some(CostKind::PlanEquivalent);
                    row
                }
                16..=17 => {
                    // A NULL-cost measurement.
                    let mut row = bare(ts_ms);
                    row.session_id =
                        sessions[(i / 11) as usize % sessions.len()].map(str::to_owned);
                    row.input = Some(500);
                    row
                }
                18 => {
                    let mut row = kind_row(ts_ms, RowKind::Error);
                    row.status = Some(500);
                    row.error_type = Some("upstream".to_owned());
                    row
                }
                _ => {
                    let mut row = kind_row(ts_ms, RowKind::FidelityDrift);
                    row.drift_digest = Some("sha256:drift".to_owned());
                    row
                }
            };
            batch.push(row);
            if batch.len() == 2_500 {
                store.record_requests(&batch).expect("seed a batch");
                batch.clear();
            }
        }
        // Pre-window rows: the WHERE must exclude them from both reads.
        for i in 0..100 {
            batch.push(bare(since_ms - 1_000 - i));
        }
        store.record_requests(&batch).expect("seed the tail batch");
        let total = store.count_requests().expect("count");
        assert_eq!(total, SEED + 100, "the seed is complete");

        // Warm both paths once — page cache and statement cache — so
        // the timed iterations measure the steady state.
        std::hint::black_box(store.requests_since(since_ms, CAP).expect("warm old"));
        std::hint::black_box(store.display_rows_since(since_ms, CAP).expect("warm new"));
        let narrow_len = store
            .display_rows_since(since_ms, CAP)
            .expect("narrow")
            .len();
        assert_eq!(
            narrow_len, SEED as usize,
            "the window holds the in-window rows only"
        );

        // OLD: the full-row materialisation the display refresh used
        // to pay — 59 columns, six JSON parses per row — plus the
        // aggregation over its projection. The projection is test
        // scaffolding the real path never ran, so this total is an
        // UPPER bound on the old refresh. The quota section and the
        // count query are the same constant work on both paths and
        // are left out of both.
        let mut old_read = Duration::ZERO;
        let mut old_refresh = Duration::ZERO;
        for _ in 0..ITERATIONS {
            let t = Instant::now();
            let full = store.requests_since(since_ms, CAP).expect("full read");
            old_read += t.elapsed();
            let t = Instant::now();
            let snap = super::aggregate(&as_display_rows(&full), None, 30, now_ms, total);
            old_refresh += t.elapsed();
            std::hint::black_box(&snap);
        }

        // NEW: the narrow read the display cadence now pays — ten
        // columns, no JSON parse — plus the same aggregation.
        let mut new_read = Duration::ZERO;
        let mut new_refresh = Duration::ZERO;
        for _ in 0..ITERATIONS {
            let t = Instant::now();
            let narrow = store
                .display_rows_since(since_ms, CAP)
                .expect("narrow read");
            new_read += t.elapsed();
            let t = Instant::now();
            let snap = super::aggregate(&narrow, None, 30, now_ms, total);
            new_refresh += t.elapsed();
            std::hint::black_box(&snap);
        }

        // The timed path must have been doing real work.
        let snap = super::aggregate(
            &store
                .display_rows_since(since_ms, CAP)
                .expect("narrow read"),
            None,
            30,
            now_ms,
            total,
        );
        assert!(snap.window_requests > 0 && !snap.spend.breakdown.is_empty() && snap.errors > 0);

        let ms = |total: Duration| total.as_secs_f64() * 1000.0 / ITERATIONS as f64;
        eprintln!(
            "display window over {SEED} rows, {ITERATIONS} iterations, debug build:\n  \
             old read (requests_since)                 {:8.3} ms/refresh\n  \
             old read + aggregation (upper bound)      {:8.3} ms/refresh\n  \
             new read (display_rows_since)             {:8.3} ms/refresh\n  \
             new read + aggregation (the refresh)      {:8.3} ms/refresh\n  \
             → {:.2}% of a core per 2 s tick (new), {:.2}% (old upper bound)",
            ms(old_read),
            ms(old_read + old_refresh),
            ms(new_read),
            ms(new_read + new_refresh),
            ms(new_read + new_refresh) / 2000.0 * 100.0,
            ms(old_read + old_refresh) / 2000.0 * 100.0,
        );
        assert!(
            new_read + new_refresh < old_read + old_refresh,
            "the narrow refresh must beat the full-row refresh: \
             {:.3} vs {:.3} ms",
            ms(new_read + new_refresh),
            ms(old_read + old_refresh),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
