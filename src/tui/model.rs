//! The dashboard's aggregation model: ledger rows in, snapshot out.
//!
//! Pure over [`RequestRow`]s — no terminal types, no clocks, no store — so
//! every panel number is testable against synthetic row sets. The loop in
//! the parent module owns time and I/O; [`aggregate`] receives `now_ms`,
//! the window length, the meter lookback (a superset of the window — a
//! burn rate needs a span a display window cannot hold) and the local
//! day's start as data, and the quota section is [super::quota]'s
//! aggregation of that lookback.
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
use crate::store::{CostKind, RequestRow, RowKind, is_api_measurement};

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
    aggregate(&[], &[], window_mins, 0, 0, 0)
}

/// Aggregate one window of ledger rows into a [`Snapshot`]. `now_ms` is
/// the frame's reference time (bucket edges anchor to it); `total_requests` is
/// the ledger's total row count, kept distinct from the window so the
/// header can show both. `quota_rows` is the meter lookback (the
/// 7-day read; a superset of the window) and `today_start_ms` the local
/// day's start — the quota section's `spent today` span anchors there,
/// passed in so the model stays pure over its inputs. Rows may arrive
/// in any order — "latest" is decided by `ts_ms` throughout.
pub(crate) fn aggregate(
    rows: &[RequestRow],
    quota_rows: &[RequestRow],
    window_mins: u64,
    now_ms: i64,
    total_requests: i64,
    today_start_ms: i64,
) -> Snapshot {
    let window_mins = window_mins.max(1) as usize;

    // Latest-by-timestamp needs ts order; sort a copy of references so the
    // caller's slice is untouched.
    let mut sorted: Vec<&RequestRow> = rows.iter().collect();
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
        quota: super::quota::aggregate(
            quota_rows,
            now_ms,
            today_start_ms,
            now_ms.saturating_sub(window_mins.max(1) as i64 * 60_000),
        ),
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
    use crate::store::{CostKind, RequestRow, RowKind};
    use crate::tui::testrows::{bare, billed, kind_row};
    use serde_json::json;

    /// A fixed frame time: 2026-01-21T22:13:20Z-ish, arbitrary but stable.
    const NOW: i64 = 1_769_000_000_000;
    const WINDOW: u64 = 30;

    fn agg(rows: &[RequestRow], total: i64) -> Snapshot {
        // The meter lookback is a superset of the window; the tests pass
        // the same rows, which is the shape of a real window that fits
        // inside the lookback.
        super::aggregate(rows, rows, WINDOW, NOW, total, NOW - 12 * 60 * 60_000)
    }

    /// A measurement row a given number of minutes before `NOW`.
    fn mins_ago(mins: i64) -> i64 {
        NOW - mins * 60_000
    }

    #[test]
    fn sessions_group_order_and_latest_values() {
        let rows = vec![
            billed(
                mins_ago(5),
                Some("ses-a"),
                "model-a",
                "openrouter",
                100,
                900,
                50,
                0.001,
            ),
            billed(
                mins_ago(2),
                Some("ses-a"),
                "model-b",
                "openrouter",
                200,
                800,
                60,
                0.002,
            ),
            billed(mins_ago(1), None, "model-c", "openrouter", 30, 0, 10, 0.003),
            billed(
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
        let mut no_cache = bare(mins_ago(3));
        no_cache.session_id = Some("ses-x".into());
        no_cache.input = Some(500);
        let mut both = bare(mins_ago(2));
        both.session_id = Some("ses-x".into());
        both.input = Some(100);
        both.cache_read = Some(50);
        both.output = Some(7);
        let mut neither = bare(mins_ago(1)); // latest row: no tokens at all
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
            billed(
                mins_ago(9),
                Some("ses-a"),
                "glm",
                "openrouter",
                1,
                1,
                1,
                1.5,
            ),
            billed(
                mins_ago(8),
                Some("ses-a"),
                "glm",
                "openrouter",
                1,
                1,
                1,
                2.5,
            ),
            billed(mins_ago(7), Some("ses-b"), "gpt", "lunaroute", 1, 1, 1, 1.0),
            billed(mins_ago(6), None, "glm", "openrouter", 1, 1, 1, 0.25),
            bare(mins_ago(5)), // measurement without cost: no cost data
            bare(mins_ago(4)), // …and another
        ];
        let mut priced_unkinded = bare(mins_ago(3));
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
        let mut no_provider = bare(mins_ago(2));
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
        let mut row = bare(mins_ago(1));
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
            rows.push(bare(mins_ago(3)));
        }
        for _ in 0..3 {
            rows.push(bare(mins_ago(2)));
        }
        rows.push(bare(mins_ago(0)));
        rows.push(kind_row(mins_ago(2), RowKind::Error));
        rows.push(kind_row(mins_ago(1), RowKind::FidelityDrift));

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
        let snap = agg(&[bare(NOW - 30 * 60_000)], 1);
        assert_eq!(snap.rate.buckets[0].requests, 1);

        // Clock-skewed future row clamps into the newest bucket.
        let snap = agg(&[bare(NOW + 5_000)], 1);
        assert_eq!(snap.rate.buckets[29].requests, 1);
    }

    #[test]
    fn proxy_kinds_are_excluded_from_sessions_spend_and_rate() {
        let rows = vec![
            billed(
                mins_ago(1),
                Some("ses-a"),
                "glm",
                "openrouter",
                1,
                1,
                1,
                0.5,
            ),
            kind_row(mins_ago(1), RowKind::Blocked),
            kind_row(mins_ago(1), RowKind::Released),
            kind_row(mins_ago(1), RowKind::Cold),
            kind_row(mins_ago(1), RowKind::ColdQuiet),
            kind_row(mins_ago(1), RowKind::Awake),
            kind_row(mins_ago(1), RowKind::Error),
            kind_row(mins_ago(1), RowKind::FidelityDrift),
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
        let snap = agg(&[bare(mins_ago(1))], 1);
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
        let snap = super::aggregate(&[bare(NOW)], &[], 0, NOW, 1, NOW);
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

        let snap = super::aggregate(
            &[in_window.clone()],
            &[older_reading, in_window],
            WINDOW,
            NOW,
            2,
            NOW - 12 * 60 * 60_000,
        );
        let quota = snap.quota.expect("the lookback carries meter snapshots");
        assert_eq!(quota.meters.len(), 1, "only the 5h meter is carried");
        // Every figure is from the NEWEST reading, which lives in the
        // window here — and the claim from the same reading.
        assert!((quota.meters[0].util - 0.42).abs() < 1e-9);
        assert_eq!(quota.binding.as_deref(), Some("five_hour"));

        // A lookback of rows without meters: no section at all.
        let snap = super::aggregate(
            &[bare(mins_ago(1))],
            &[bare(mins_ago(90))],
            WINDOW,
            NOW,
            2,
            NOW,
        );
        assert_eq!(snap.quota, None);
    }

    #[test]
    fn out_of_order_input_still_picks_latest_by_ts() {
        // Rows deliberately out of ts order: the newer one must win as
        // "latest" for model and input_now.
        let rows = vec![
            billed(
                mins_ago(1),
                Some("ses-a"),
                "newer-model",
                "openrouter",
                10,
                20,
                1,
                0.1,
            ),
            billed(
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
}
