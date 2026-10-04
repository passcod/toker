//! The rate & quota panel's aggregation (plan: TUI — "rate & quota"), a
//! faithful port of the ctp pieces `live.mjs` renders that panel from:
//!
//! - `METERS` (ctp forecast.mjs:31-35) — the three panel meters. The
//!   burn specs already live in [crate::middleware::cold]
//!   ([`METER_5H`]/[`METER_7D`]/[`METER_OVERAGE`], ported for the cold
//!   outlook), so this module adds only the fields rendering reads
//!   (label, status key);
//! - `burnRate` / `projectTo` — REUSED from
//!   [crate::middleware::cold], not duplicated: the four-state burn,
//!   the window-restart fall detection, the zero-reading anchor, the
//!   day-scale-span rule (a reset more than a day out is measured
//!   across at least a full day, which carries the duty cycle without
//!   modelling it) and the five verdicts are all there;
//! - `meterUsed` / `periods` / `readingsOf` (ctp forecast.mjs:290-425) —
//!   a span TOTAL rather than a rate, measured from the reading BEFORE
//!   the span (the first in-span reading already includes that
//!   request's consumption, so measuring from it silently drops a
//!   request's worth of spend);
//! - the gate-aware target and the newest-reading pick (ctp live.mjs
//!   537-626): `stops` counts down to the gate's threshold where `out`
//!   counts down to exhaustion, a spent reading stops counting once its
//!   window's reset passes ([`exhausted_meters`] carries `now`), and
//!   the `?` marks a gate state assumed from rows predating `gate_on`.
//!
//! Absence ≠ zero (invariant 3) decides the panel's presence too: rows
//! without meter snapshots produce no section at all — nothing renders,
//! never zeros — which is also the per-backend panel rule (an openai
//! window has no quota meters and no quota panel).
//!
//! The aggregation runs over [`MeterRow`]s — the store's narrow
//! meter-lookback projection (timestamp, kind, gate flag, snapshot),
//! four columns where the full row carries 59 and one JSON parse where
//! the full row carries six — so the quota refresh cannot quietly
//! regress into materialising full rows. The burn math stays shared
//! with the cold gate: this module extracts each meter's
//! [`MeterSample`]s at the call boundary and feeds the same ladder
//! ([`burn_rate_samples`]) the cold outlook runs over full rows.

use serde_json::Value;

use crate::middleware::cold::{
    EPS, METER_5H, METER_7D, METER_OVERAGE, MeterSample, MeterSpec, QUANTUM, REORDER_MS, Verdict,
    burn_rate_samples, project_to,
};
use crate::middleware::quota::{Meters, THRESHOLD, exhausted_meters};
use crate::store::MeterRow;

/// One ctp `METERS` entry for the panel: the burn spec plus the fields
/// only rendering reads. Order is ctp's render order (5h, 7d, overage).
struct PanelMeter {
    /// ctp `meter.key` — also the spelling of the gate's `gone` set.
    key: &'static str,
    /// ctp `meter.label` — the line's left-aligned name.
    label: &'static str,
    spec: &'static MeterSpec,
    /// ctp `meter.status` — the per-window status field (`allowed` is
    /// the non-event and never renders).
    status_key: &'static str,
}

const PANEL_METERS: [PanelMeter; 3] = [
    PanelMeter {
        key: "5h",
        label: "5-hour",
        spec: &METER_5H,
        status_key: "status5h",
    },
    PanelMeter {
        key: "7d",
        label: "7-day",
        spec: &METER_7D,
        status_key: "status7d",
    },
    PanelMeter {
        key: "overage",
        label: "overage",
        spec: &METER_OVERAGE,
        status_key: "statusOverage",
    },
];

/// The quota section of the dashboard snapshot. `None` when the
/// lookback holds no meter snapshot at all — the panel renders nothing
/// rather than zeros (invariant 3's per-backend panel rule).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QuotaAgg {
    /// The meters the newest reading carries, in ctp's render order.
    /// A meter the reading does not carry is absent, not zero-filled.
    pub meters: Vec<MeterPanel>,
    /// Overage allowance points spent since the start of the local day
    /// (`None` when the reading carries no overage meter — the `spent`
    /// line is the overage meter's).
    pub spent_today: Option<Spent>,
    /// Overage points spent over the running display window.
    pub spent_window: Option<Spent>,
    /// The representative-claim — which limit is in force (ctp's
    /// `binding` line). `None` when the reading carried none.
    pub binding: Option<String>,
    /// Whether the newest reading already draws on overage rather than
    /// plan quota (ctp `overageInUse`, read through the same typed view
    /// the gate decides on).
    pub overage_in_use: bool,
    /// Whether the quota gate is armed: the newest `gate_on` reading,
    /// or ctp's default of armed when no row carries one.
    pub gate_on: bool,
    /// No row in the lookback carries `gate_on`: the state above was
    /// assumed, not observed, and a gate-aware countdown renders `?`.
    pub gate_assumed: bool,
}

/// One meter's line: the newest reading's figures plus the forecast.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MeterPanel {
    /// ctp `METERS[key]` — "5h", "7d", "overage".
    pub key: &'static str,
    /// The line's label (ctp `meter.label`).
    pub label: &'static str,
    /// The reading's utilisation, as reported.
    pub util: f64,
    /// The reading's reset, epoch seconds; `None` when it carried none
    /// — rendered as an explicit `?`, never as zero or "now".
    pub reset_s: Option<i64>,
    /// The reading's per-window status field, when it carried one.
    pub status: Option<String>,
    /// Whether the gate counts this meter exhausted right now (ctp's
    /// `gone` set): a reading whose window's reset has passed stops
    /// counting, so a stale spent figure can never wedge the panel.
    pub exhausted: bool,
    /// Whether the gate's threshold applies to this meter (armed gate,
    /// and not the overage window — ctp `gated`).
    pub gated: bool,
    /// The wall the forecast counts down to: the gate's threshold when
    /// armed and not already past it, real exhaustion otherwise.
    pub target: f64,
    /// When the burn reaches `target`, against the reset (ctp
    /// `projectTo`; [`Verdict::Runout`] carries the instant).
    pub verdict: Verdict,
}

/// One `spent` span's answer (ctp `meterUsed`'s states — the five the
/// README's `spent` table names, mapped to what renders them).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Spent {
    /// No reading for the meter at all — "no data", not the same as
    /// zero.
    NoData,
    /// Readings exist, none inside the span: nothing ran, so nothing
    /// was spent — "idle".
    Idle,
    /// `points` of the allowance consumed in the span. `floor` when
    /// the span reaches back further than the readings do, so the
    /// figure is a lower bound and renders `≥+N%`.
    Measured { points: f64, floor: bool },
}

/// Aggregate the quota section over the meter lookback (the display
/// window's rows are a subset of it, but a burn needs a span a display
/// window cannot provide). `rows` are the narrow [`MeterRow`]s the
/// store's meter-lookback read returns. `today_start_ms` is the local
/// day's start and `window_since_ms` the display window's start — both
/// inputs, so the model stays pure over its rows. `None` when no row
/// carries a meter snapshot (ctp live.mjs:537's `if (lastRl)`).
pub(crate) fn aggregate(
    rows: &[MeterRow],
    now_ms: i64,
    today_start_ms: i64,
    window_since_ms: i64,
) -> Option<QuotaAgg> {
    // Latest-by-timestamp, as everywhere in the model: the ledger is
    // insert-only and rows arrive slightly out of order.
    let mut sorted: Vec<&MeterRow> = rows.iter().collect();
    sorted.sort_by_key(|row| row.ts_ms);

    // The newest rate-limit reading, from every row that carries one,
    // proxy-written kinds included (ctp live.mjs:273: "Error and
    // blocked rows carry the headers too, and are as good an
    // observation as any" — after a block the stale copy is the only
    // row there is, and its reset value is what says whether the
    // window is still live).
    let last_rl = sorted
        .iter()
        .rev()
        .find_map(|row| row.rate_limits.as_ref())?;

    // The gate's state is logged per row because it cannot be worked
    // out here (ctp live.mjs:279-280): the newest `gateOn` reading
    // wins, and a lookback with none falls back to the default of
    // armed — marked assumed, because a gate that is assumed must not
    // read the same as one that was observed.
    let gate_seen = sorted.iter().rev().find_map(|row| row.gate_on);
    let gate_on = gate_seen.unwrap_or(true);
    let gate_assumed = gate_seen.is_none();

    // Which meters the gate counts as exhausted right now, gate armed
    // only (ctp live.mjs:541: `gone = gateOn ? exhaustedMeters(...) :
    // []`). `exhausted_meters` takes `now`, so a reading whose window
    // has rolled stops counting as exhaustion — without it the line
    // would read "gated · window rolled over", asserting a current
    // state from a reading it has just called stale.
    let gone: Vec<&'static str> = if gate_on {
        exhausted_meters(Some(Meters::over(last_rl)), now_ms)
            .into_iter()
            .map(|meter| meter.as_str())
            .collect()
    } else {
        Vec::new()
    };

    let meters: Vec<MeterPanel> = PANEL_METERS
        .iter()
        .filter_map(|panel| {
            // A meter the newest reading does not carry renders
            // nothing (ctp q(): `if (v == null) return null`).
            let util = last_rl.get(panel.spec.util_key)?.as_f64()?;
            let reset_s = last_rl.get(panel.spec.reset_key).and_then(Value::as_i64);
            let status = last_rl
                .get(panel.status_key)
                .and_then(Value::as_str)
                .map(str::to_owned);
            let gated = gate_on && panel.key != "overage";
            let exhausted = gone.contains(&panel.key);
            // Under an armed gate the threshold is the nearer wall and
            // the one that actually stops the session, so that is what
            // to count down to — until it is passed, after which the
            // session is already stopping and the question becomes how
            // long the released overage lasts.
            let target = if gated && !exhausted { THRESHOLD } else { 1.0 };
            let burn = burn_rate_samples(&burn_samples(rows, panel.spec), panel.spec, now_ms);
            let verdict = project_to(&burn, target, now_ms);
            Some(MeterPanel {
                key: panel.key,
                label: panel.label,
                util,
                reset_s,
                status,
                exhausted,
                gated,
                target,
                verdict,
            })
        })
        .collect();

    // `spent` answers what the overage bar cannot — a meter sitting at
    // 64% says nothing about whether it got there this morning or a
    // week ago — and is the overage meter's line, rendered only where
    // that meter rendered (ctp live.mjs:602).
    let (spent_today, spent_window) = if meters.iter().any(|m| m.key == "overage") {
        (
            Some(meter_used(rows, &METER_OVERAGE, today_start_ms, now_ms)),
            Some(meter_used(rows, &METER_OVERAGE, window_since_ms, now_ms)),
        )
    } else {
        (None, None)
    };

    Some(QuotaAgg {
        meters,
        spent_today,
        spent_window,
        binding: last_rl
            .get("claim")
            .and_then(Value::as_str)
            .map(str::to_owned),
        overage_in_use: Meters::over(last_rl).overage_in_use(),
        gate_on,
        gate_assumed,
    })
}

// ── the span total (ctp meterUsed / periods / readingsOf) ────────────────

/// The burn's samples for one meter over the narrow lookback: every row
/// that carries the meter's fields, proxy-written kinds included —
/// cold's `burn_rate` extracts the same set from full rows (after a
/// block the stale copy is the only row there is, and its reset value
/// is what says whether the window is still live). Feeds
/// [`burn_rate_samples`], the ladder shared with the cold outlook.
fn burn_samples(rows: &[MeterRow], spec: &MeterSpec) -> Vec<MeterSample> {
    rows.iter()
        .filter_map(|row| {
            let limits = row.rate_limits.as_ref()?;
            Some(MeterSample {
                at_ms: row.ts_ms,
                util: limits.get(spec.util_key)?.as_f64()?,
                reset_s: limits.get(spec.reset_key)?.as_i64()?,
            })
        })
        .collect()
}

/// The readings for one meter, oldest first — deliberately unlike the
/// burn's own extraction ([`burn_samples`]) in one respect: rows the
/// proxy wrote about itself are dropped. A `blocked` row's
/// `rate_limits` is this process's last-seen copy rather than a
/// response header — an old figure wearing a fresh timestamp, harmless
/// as the newest reading of a rate and ruinous as the baseline of a
/// total. (ctp `readingsOf`, forecast.mjs:290-304: excluded by the
/// presence of `kind`, never by listing the kinds — listing them is how
/// `released` slipped into the quota fit.)
fn readings_of(rows: &[MeterRow], spec: &MeterSpec) -> Vec<MeterSample> {
    let mut out: Vec<MeterSample> = rows
        .iter()
        .filter(|row| row.kind.is_none())
        .filter_map(|row| {
            let limits = row.rate_limits.as_ref()?;
            Some(MeterSample {
                at_ms: row.ts_ms,
                util: limits.get(spec.util_key)?.as_f64()?,
                reset_s: limits.get(spec.reset_key)?.as_i64()?,
            })
        })
        .collect();
    out.sort_by_key(|reading| reading.at_ms);
    out
}

/// One accounting period: a stretch over which the meter counted up
/// from a single start. Two things end one, and the difference between
/// them is the whole point of the split (ctp `periods`,
/// forecast.mjs:326-364):
///
/// - a changed reset value: the window rolled, and the new period
///   opened at zero at the OLD window's reset instant — reconstructable,
///   so `opens_at_ms` names it;
/// - a fall that never comes back and has outlasted reordering: the
///   meter restarted somewhere the reset field does not describe, so
///   where the period began is exactly what is not known — no opening
///   instant, and a span resting on it can only give a floor.
struct Period {
    rows: Vec<MeterSample>,
    /// When this period provably opened, where that is known; `None`
    /// for a restart, and for a first window of unknown length.
    opens_at_ms: Option<i64>,
}

/// See [`Period`].
fn periods(readings: &[MeterSample], spec: &MeterSpec) -> Vec<Period> {
    // Phase 1 — split at reset changes (ctp `byReset`). The first
    // period's start is known only where the window has a known length
    // (the burn's anchor rule); a later one opened at the previous
    // window's reset instant.
    let mut by_reset: Vec<(i64, Vec<MeterSample>)> = Vec::new();
    for reading in readings {
        match by_reset.last_mut() {
            Some((reset, rows)) if *reset == reading.reset_s => rows.push(*reading),
            _ => by_reset.push((reading.reset_s, vec![*reading])),
        }
    }

    let mut out = Vec::new();
    for (index, (reset, rows)) in by_reset.iter().enumerate() {
        let opens_at = if index == 0 {
            spec.length_ms.map(|length| reset * 1000 - length)
        } else {
            Some(by_reset[index - 1].0 * 1000)
        };
        // Phase 2 — split at held falls, the burn's fell/held test for
        // the same reasons: utilisation is not monotonic in log order
        // (requests finish out of order), so a bare fall is jitter and
        // a dip in the newest reading has nothing after it to return.
        let last_at = rows[rows.len() - 1].at_ms;
        let mut peak_after = vec![f64::NEG_INFINITY; rows.len()];
        let mut running = f64::NEG_INFINITY;
        for i in (0..rows.len()).rev() {
            running = running.max(rows[i].util);
            peak_after[i] = running;
        }
        let mut start = 0usize;
        let mut peak_before = rows[0].util;
        let mut opens = opens_at;
        for i in 1..rows.len() {
            let fell = rows[i].util + EPS < rows[i - 1].util - QUANTUM;
            let held = last_at - rows[i].at_ms >= REORDER_MS;
            if fell && held && peak_after[i] + QUANTUM + EPS < peak_before {
                out.push(Period {
                    rows: rows[start..i].to_vec(),
                    opens_at_ms: opens,
                });
                start = i;
                peak_before = rows[i].util;
                opens = None; // a restart names no start instant
            } else {
                peak_before = peak_before.max(rows[i].util);
            }
        }
        out.push(Period {
            rows: rows[start..].to_vec(),
            opens_at_ms: opens,
        });
    }
    out
}

/// How much of `meter` was spent between `since_ms` and `now_ms` (ctp
/// `meterUsed`, forecast.mjs:392-425 — ported step for step). A total,
/// not a rate, so it comes from the lookback rather than the display
/// window: the baseline for a span is the last reading BEFORE it, and
/// "before midnight" is usually older than the window. `delta` can be
/// a legitimate zero — the figure is quantised to 1%, so a busy span
/// may not move it; the caller renders that as `<1%`, distinct from
/// the explicit idle and no-data states.
pub(crate) fn meter_used(rows: &[MeterRow], spec: &MeterSpec, since_ms: i64, now_ms: i64) -> Spent {
    let readings = readings_of(rows, spec);
    if readings.is_empty() {
        return Spent::NoData;
    }
    let mut delta = 0.0;
    let mut any = false;
    let mut floor = false;
    for period in periods(&readings, spec) {
        // Utilisation is cumulative within a period, so the running
        // maximum is what has been consumed and a lower later reading
        // is an older request reporting late. Without the envelope a
        // spell of reordering reads as the quota refilling.
        let mut peak = f64::NEG_INFINITY;
        let enveloped: Vec<(i64, f64)> = period
            .rows
            .iter()
            .map(|reading| {
                peak = peak.max(reading.util);
                (reading.at_ms, peak)
            })
            .collect();

        // This period's rows inside the span; a period that ended
        // before the span contributes nothing (and never as idle —
        // another period may still have run).
        let in_span: Vec<(i64, f64)> = enveloped
            .iter()
            .copied()
            .filter(|(at, _)| *at >= since_ms)
            .collect();
        if in_span.is_empty() {
            continue;
        }
        any = true;

        // The baseline is the last reading BEFORE the span, not the
        // first one inside it: the first in-span reading already
        // includes whatever that request consumed, so measuring from
        // it silently drops a request's worth of spend.
        let base = enveloped
            .iter()
            .rev()
            .find(|(at, _)| *at < since_ms)
            .map(|(_, util)| *util);
        let base = match base {
            Some(base) => base,
            None => match period.opens_at_ms {
                // Anchor at zero only where the period provably opened
                // INSIDE the span: a period that opened earlier may
                // have been spent against before the span began, and
                // counting that here would report yesterday's traffic
                // as today's.
                Some(opens) if opens >= since_ms && opens <= now_ms => 0.0,
                _ => {
                    floor = true;
                    in_span[0].1
                }
            },
        };
        delta += (in_span[in_span.len() - 1].1 - base).max(0.0);
    }
    if !any {
        return Spent::Idle;
    }
    Spent::Measured {
        points: delta,
        floor,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        METER_5H, METER_OVERAGE, QuotaAgg, Spent, THRESHOLD, Verdict, aggregate, meter_used,
    };
    use crate::store::{MeterRow, RowKind};
    use crate::tui::testrows::{meter_bare, metered};
    use serde_json::{Value, json};

    /// A fixed frame time, arbitrary but stable (the cold module's
    /// convention; spans below stay readable in minutes and days).
    const NOW: i64 = 2_000_000_000_000;
    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;

    /// A local-day start comfortably before every span below.
    const TODAY: i64 = NOW - 12 * HOUR;

    /// The display window's start (a 30-minute window, like the TUI's
    /// default frame tests).
    const WINDOW_SINCE: i64 = NOW - 30 * MIN;

    fn quota(rows: &[MeterRow]) -> Option<QuotaAgg> {
        aggregate(rows, NOW, TODAY, WINDOW_SINCE)
    }

    /// Spent comparisons with float tolerance: the deltas are sums of
    /// binary floats (0.30 − 0.10 is 0.19999999999999998), so exact
    /// equality would test the arithmetic, not the aggregation.
    fn assert_spent(actual: Spent, points: f64, floor: bool) {
        match actual {
            Spent::Measured {
                points: got,
                floor: got_floor,
            } => {
                assert!((got - points).abs() < 1e-9, "points: {got} vs {points}");
                assert_eq!(got_floor, floor, "floor");
            }
            other => panic!("expected Measured({points}, floor={floor}), got {other:?}"),
        }
    }

    // ── the section's presence ────────────────────────────────────────

    #[test]
    fn no_meter_rows_leave_the_section_absent() {
        // Rows without meter snapshots: no section at all — the panel
        // renders nothing, never zero-filled meters (invariant 3's
        // per-backend panel rule: an openai window has no quota panel).
        assert_eq!(
            quota(&[meter_bare(NOW - MIN), meter_bare(NOW - 2 * MIN)]),
            None
        );

        // A snapshot with no meter fields is a section with no meters:
        // present (the reading exists) but empty, and `spent`/`binding`
        // stay absent with it.
        let snap = quota(&[metered(NOW - MIN, json!({}))]).expect("the reading exists");
        assert!(snap.meters.is_empty());
        assert_eq!(snap.spent_today, None);
        assert_eq!(snap.binding, None);
    }

    #[test]
    fn the_newest_reading_names_the_meters_the_claim_and_the_gate() {
        let rows = vec![
            {
                let mut row = metered(
                    NOW - 2 * MIN,
                    json!({
                        "util5h": 0.42, "reset5h": (NOW + HOUR) / 1000,
                        "util7d": 0.30, "reset7d": (NOW + DAY) / 1000,
                        "claim": "seven_day",
                    }),
                );
                row.gate_on = Some(true);
                row
            },
            {
                let mut row = metered(
                    NOW - MIN,
                    json!({
                        "util5h": 0.10, "reset5h": (NOW + 3 * HOUR) / 1000,
                        "util7d": 0.22, "reset7d": (NOW + 2 * DAY) / 1000,
                        "status5h": "allowed", "status7d": "allowed",
                        "claim": "five_hour", "overageInUse": true,
                    }),
                );
                row.gate_on = Some(true);
                row
            },
        ];
        let snap = quota(&rows).expect("rows carry meter snapshots");
        // Every figure comes from the NEWEST reading, not the max.
        let five = snap.meters.iter().find(|m| m.key == "5h").expect("5h");
        assert!((five.util - 0.10).abs() < 1e-9);
        assert_eq!(five.reset_s, Some((NOW + 3 * HOUR) / 1000));
        assert_eq!(five.status.as_deref(), Some("allowed"));
        let seven = snap.meters.iter().find(|m| m.key == "7d").expect("7d");
        assert!((seven.util - 0.22).abs() < 1e-9);
        // The overage meter renders only where the reading carries it.
        assert!(snap.meters.iter().all(|m| m.key != "overage"));
        assert_eq!(snap.spent_today, None, "spent is the overage meter's line");
        assert_eq!(snap.binding.as_deref(), Some("five_hour"));
        assert!(snap.overage_in_use);
        assert!(snap.gate_on);
        assert!(!snap.gate_assumed);
        // overageInUse with both plan windows healthy counts as the 5h
        // meter being gone (ctp's rule), so the gate's wall applies.
        assert!(five.exhausted);
        assert!(five.gated);
        assert_eq!(
            five.target, 1.0,
            "already past the gate: the wall is exhaustion"
        );
    }

    // ── the verdict set ─────────────────────────────────────────────

    #[test]
    fn a_rolled_window_says_so_and_stops_counting_as_exhaustion() {
        // A spent reading whose reset has passed: the window rolled and
        // nothing has reported the new one — the verdict says so in
        // words, and the gate must not count it as exhaustion (that is
        // the rule that un-wedges both).
        let mut row = metered(
            NOW - 5 * MIN,
            json!({"util5h": 1.0, "reset5h": (NOW - MIN) / 1000}),
        );
        row.gate_on = Some(true);
        let snap = quota(&[row]).expect("reading exists");
        let five = &snap.meters[0];
        assert_eq!(five.verdict, Verdict::Stale);
        assert!(
            !five.exhausted,
            "an expired reading is not a reading of the current window"
        );
        assert_eq!(five.target, THRESHOLD);
    }

    #[test]
    fn stops_counts_down_to_the_gate_and_out_counts_down_to_exhaustion() {
        // A fast burn: 0.10 an hour ago, 0.90 now, reset three hours
        // out — the meter reaches any wall long before the reset.
        let burning = |gate_on: Option<bool>| {
            let mut rows = vec![
                metered(
                    NOW - HOUR,
                    json!({"util5h": 0.10, "reset5h": (NOW + 3 * HOUR) / 1000}),
                ),
                metered(
                    NOW - 2_000,
                    json!({"util5h": 0.90, "reset5h": (NOW + 3 * HOUR) / 1000}),
                ),
            ];
            for row in &mut rows {
                row.gate_on = gate_on;
            }
            let snap = quota(&rows).expect("readings exist");
            let five = snap
                .meters
                .iter()
                .find(|m| m.key == "5h")
                .expect("5h")
                .clone();
            (snap, five)
        };

        // Gate armed: the gate trips first at 0.99 — `stops`.
        let (snap, five) = burning(Some(true));
        assert!(snap.gate_on && !snap.gate_assumed);
        assert!(five.gated);
        assert_eq!(five.target, super::THRESHOLD);
        match five.verdict {
            Verdict::Runout { at_ms } => {
                // 0.09 remaining at 0.80/hour: 6.75 minutes out.
                assert!(
                    (at_ms - (NOW as f64 + 6.75 * MIN as f64)).abs() < 1e-6,
                    "{at_ms}"
                );
            }
            other => panic!("armed gate: a runout, got {other:?}"),
        }

        // Gate disarmed: real exhaustion at 1.0 — `out`.
        let (_, five) = burning(Some(false));
        assert!(!five.gated);
        assert_eq!(five.target, 1.0);
        assert!(matches!(five.verdict, Verdict::Runout { .. }));
    }

    #[test]
    fn estimating_when_the_meter_has_not_moved_past_quantisation() {
        // Movement below two 1% steps is not a measurement, so the
        // burn can only give a ceiling — and a ceiling that reaches
        // the wall before the reset settles nothing. Never a silent
        // guess: "estimating" is an explicit verdict.
        //
        // The window here opened five minutes ago (reset − 5h), so
        // both readings postdate the opening and the zero-anchor
        // cannot be reconstructed — without that, every 5h reading
        // would measure off its window's opening instead.
        let mut rows = vec![
            metered(
                NOW - 10 * MIN,
                json!({"util5h": 0.95, "reset5h": (NOW + 4 * HOUR + 55 * MIN) / 1000}),
            ),
            metered(
                NOW - MIN,
                json!({"util5h": 0.96, "reset5h": (NOW + 4 * HOUR + 55 * MIN) / 1000}),
            ),
        ];
        for row in &mut rows {
            row.gate_on = Some(true);
        }
        let snap = quota(&rows).expect("readings exist");
        let five = snap.meters.iter().find(|m| m.key == "5h").expect("5h");
        assert_eq!(five.target, THRESHOLD);
        assert_eq!(
            five.verdict,
            Verdict::Unknown,
            "the bound cannot separate the walls — that is 'estimating'"
        );
    }

    #[test]
    fn the_gate_may_be_assumed_and_is_marked_so() {
        // Rows predating `gateOn` (or a lookback without any): the
        // proxy's default is armed, and the countdown must not read the
        // same as one that observed the state — the view renders `?`.
        let rows = vec![metered(
            NOW - MIN,
            json!({"util5h": 0.30, "reset5h": (NOW + HOUR) / 1000}),
        )];
        let snap = quota(&rows).expect("reading exists");
        assert!(snap.gate_on, "ctp's default is armed");
        assert!(snap.gate_assumed);
        let five = snap.meters.iter().find(|m| m.key == "5h").expect("5h");
        assert!(five.gated && five.target < 1.0);

        // One row carrying the flag clears the assumption — even when
        // it is not the newest row.
        let mut seen = metered(
            NOW - 10 * MIN,
            json!({"util5h": 0.30, "reset5h": (NOW + HOUR) / 1000}),
        );
        seen.gate_on = Some(false);
        let snap = quota(&[seen]).expect("reading exists");
        assert!(!snap.gate_on && !snap.gate_assumed);
    }

    #[test]
    fn a_spent_window_under_an_armed_gate_is_reached_not_projected() {
        // Fully spent: nothing left to project — the verdict is
        // "spent", and the target beyond the gate is exhaustion (the
        // release marker spends overage until then).
        let mut row = metered(
            NOW - MIN,
            json!({"util5h": 1.0, "reset5h": (NOW + HOUR) / 1000}),
        );
        row.gate_on = Some(true);
        let snap = quota(&[row]).expect("reading exists");
        let five = snap.meters.iter().find(|m| m.key == "5h").expect("5h");
        assert!(five.exhausted);
        assert_eq!(five.target, 1.0);
        assert_eq!(five.verdict, Verdict::Reached);
    }

    // ── the span totals (ctp meterUsed) ──────────────────────────────

    #[test]
    fn spent_no_data_idle_and_the_quantised_floor() {
        // No reading for the meter at all — not the same as zero.
        assert_eq!(meter_used(&[], &METER_OVERAGE, TODAY, NOW), Spent::NoData);
        assert_eq!(
            meter_used(
                &[metered(NOW - 2 * DAY, overage(0.50, NOW))],
                &METER_OVERAGE,
                TODAY,
                NOW
            ),
            Spent::Idle,
            "readings exist, none inside the span: nothing ran"
        );

        // A busy span that did not move the 1%-quantised figure: a
        // ceiling, not a measurement — the caller renders `<1%`.
        let rows = vec![
            metered(NOW - 4 * HOUR, overage(0.30, NOW)),
            metered(NOW - 2 * HOUR, overage(0.30, NOW)),
            metered(NOW - MIN, overage(0.304, NOW)),
        ];
        assert_spent(
            meter_used(&rows, &METER_OVERAGE, NOW - 3 * HOUR, NOW),
            0.004,
            false,
        );

        // Measured from the reading BEFORE the span: 0.30 before it,
        // 0.33 inside — three points, and dropping the request's worth
        // (0.30 → first in-span 0.31) would read two.
        let rows = vec![
            metered(NOW - 2 * HOUR, overage(0.30, NOW)),
            metered(NOW - 40 * MIN, overage(0.31, NOW)),
            metered(NOW - MIN, overage(0.33, NOW)),
        ];
        assert_spent(
            meter_used(&rows, &METER_OVERAGE, NOW - HOUR, NOW),
            0.03,
            false,
        );

        // The tail does not reach back to the span's start: a floor,
        // rendered `≥+N%`.
        let rows = vec![
            metered(NOW - 20 * MIN, overage(0.10, NOW)),
            metered(NOW - MIN, overage(0.13, NOW)),
        ];
        assert_spent(
            meter_used(&rows, &METER_OVERAGE, NOW - HOUR, NOW),
            0.03,
            true,
        );
    }

    /// An overage-meter reading (`utilOverage`), resetting far out —
    /// the overage window's own reset value is irrelevant to a total
    /// except across a change, and none of these spans rolls it.
    fn overage(util: f64, now: i64) -> Value {
        json!({"utilOverage": util, "resetOverage": (now + 30 * DAY) / 1000})
    }

    #[test]
    fn a_window_that_rolled_inside_the_span_is_summed_across() {
        // Two accounting periods meet the span: the first with a
        // baseline before it (0.20 → 0.30 = +0.10), the second opened
        // at the old window's reset instant — inside the span — so its
        // own start stands in as a zero baseline (0 → 0.15). The
        // total is what the span cost, not what the meter happens to
        // read now.
        let rows = vec![
            metered(
                NOW - 6 * HOUR,
                json!({"utilOverage": 0.20, "resetOverage": (NOW - 4 * HOUR) / 1000}),
            ),
            metered(
                NOW - 4 * HOUR + 5 * MIN,
                json!({"utilOverage": 0.30, "resetOverage": (NOW - 4 * HOUR) / 1000}),
            ),
            metered(
                NOW - 3 * HOUR,
                json!({"utilOverage": 0.05, "resetOverage": (NOW + 30 * DAY) / 1000}),
            ),
            metered(
                NOW - MIN,
                json!({"utilOverage": 0.15, "resetOverage": (NOW + 30 * DAY) / 1000}),
            ),
        ];
        assert_spent(
            meter_used(&rows, &METER_OVERAGE, NOW - 5 * HOUR, NOW),
            0.25,
            false,
        );
    }

    #[test]
    fn a_known_window_opening_inside_the_span_anchors_at_zero() {
        // The 5-hour window opened (reset − 5h) inside the span, so the
        // zero at its opening is a reading — the whole in-span
        // utilisation is the span's cost, no floor.
        let reset_s = (NOW + 2 * HOUR) / 1000; // opened at NOW − 3h
        let rows = vec![
            metered(NOW - 2 * HOUR, json!({"util5h": 0.10, "reset5h": reset_s})),
            metered(NOW - MIN, json!({"util5h": 0.40, "reset5h": reset_s})),
        ];
        assert_spent(
            meter_used(&rows, &METER_5H, NOW - 5 * HOUR, NOW),
            0.40,
            false,
        );

        // The same window, the span starting after its opening: no
        // reading before the span and an opening before it — yesterday
        // may have been spent against, so the answer is a floor,
        // measured from the first reading inside the span.
        let rows = vec![
            metered(NOW - 30 * MIN, json!({"util5h": 0.10, "reset5h": reset_s})),
            metered(NOW - MIN, json!({"util5h": 0.40, "reset5h": reset_s})),
        ];
        assert_spent(meter_used(&rows, &METER_5H, NOW - HOUR, NOW), 0.30, true);
    }

    #[test]
    fn a_held_fall_inside_a_period_is_a_restart_and_names_no_start() {
        // 0.50, then a fall to 0.20 that never comes back and has
        // outlasted reordering: the period restarted at 0.20, and
        // where it began is not known — a span resting on it can only
        // give a floor, measured from the first reading of the new
        // period (0.20 → 0.26).
        let reset_s = (NOW + 3 * HOUR) / 1000;
        let rows = vec![
            metered(
                NOW - 5 * HOUR,
                json!({"utilOverage": 0.50, "resetOverage": reset_s}),
            ),
            metered(
                NOW - 40 * MIN,
                json!({"utilOverage": 0.20, "resetOverage": reset_s}),
            ),
            metered(
                NOW - MIN,
                json!({"utilOverage": 0.26, "resetOverage": reset_s}),
            ),
        ];
        assert_spent(
            meter_used(&rows, &METER_OVERAGE, NOW - 4 * HOUR, NOW),
            0.06,
            true,
        );

        // The same fall that has NOT outlasted reordering is jitter:
        // the envelope keeps the higher figure, and the total measures
        // from before the fall (0.50 → 0.60 = +0.10, where a believed
        // restart would read the rise off the 0.20 base as +0.40).
        let rows = vec![
            metered(
                NOW - 5 * HOUR,
                json!({"utilOverage": 0.50, "resetOverage": reset_s}),
            ),
            metered(
                NOW - 9 * MIN,
                json!({"utilOverage": 0.20, "resetOverage": reset_s}),
            ),
            metered(
                NOW - 5 * MIN,
                json!({"utilOverage": 0.20, "resetOverage": reset_s}),
            ),
            metered(
                NOW - 2 * MIN,
                json!({"utilOverage": 0.60, "resetOverage": reset_s}),
            ),
        ];
        assert_spent(
            meter_used(&rows, &METER_OVERAGE, NOW - 4 * HOUR, NOW),
            0.10,
            false,
        );
    }

    #[test]
    fn proxy_written_rows_never_baseline_a_total_but_do_read_as_the_newest() {
        // The blocked row's `rate_limits` is this process's last-seen
        // copy — an old figure wearing a fresh timestamp. The panel's
        // newest reading still comes from it (ctp: after a block it is
        // the only row there is), but a span total excludes it: picked
        // as a baseline, it would date an hour-old reading to the start
        // of the span and inflate the answer.
        let rows = vec![
            metered(NOW - 2 * HOUR, overage(0.10, NOW)),
            metered(NOW - HOUR, overage(0.12, NOW)),
            {
                let mut blocked = metered(NOW - MIN, overage(0.99, NOW));
                blocked.kind = Some(RowKind::Blocked);
                blocked
            },
        ];
        // The total: 0.10 → 0.12 from the measurements alone, measured
        // from the reading before the span — the blocked row's fresh
        // timestamp never baselines anything.
        assert_spent(
            meter_used(&rows, &METER_OVERAGE, NOW - 90 * MIN, NOW),
            0.02,
            false,
        );
        // The newest reading: the blocked row's stale copy, per ctp's
        // own rule — its reset value is what says whether the window
        // is still live.
        let snap = quota(&rows).expect("rows carry snapshots");
        let overage = snap
            .meters
            .iter()
            .find(|m| m.key == "overage")
            .expect("overage");
        assert!((overage.util - 0.99).abs() < 1e-9);
        assert_spent(
            snap.spent_today.expect("the overage meter renders"),
            0.02,
            true,
        );
    }

    // ── the narrow read vs the full-row read (the refactor's proof) ───

    #[test]
    fn the_narrow_read_aggregates_identically_to_the_full_row_read() {
        use crate::store::Store;
        use crate::tui::testrows::{as_meter_rows, bare, kind_row, metered_full};

        let store = Store::open(":memory:").expect("scratch store");
        // A mixed ledger, every rule the aggregation has:
        // - overage readings across a reset (two accounting periods,
        //   the second opening inside the today span);
        // - a released stale copy and a blocked stale copy as proxy
        //   kinds (the newest reading comes from the blocked one; the
        //   span totals must exclude both);
        // - a cold row carrying the gate flag and NO snapshot — the
        //   production shape from `record_anthropic`, and the newest
        //   gate flag in the set;
        // - meter-less rows with and without kinds, which the narrow
        //   read's filter skips entirely.
        let overage_old = (NOW - 4 * HOUR) / 1000;
        let overage_now = (NOW + 30 * DAY) / 1000;
        let reset_5h = (NOW + 3 * HOUR) / 1000;
        let rows = vec![
            {
                let mut row = metered_full(
                    NOW - 6 * HOUR,
                    json!({"utilOverage": 0.20, "resetOverage": overage_old}),
                );
                row.gate_on = Some(true);
                row
            },
            {
                let mut row = metered_full(
                    NOW - 4 * HOUR + 5 * MIN,
                    json!({"utilOverage": 0.30, "resetOverage": overage_old}),
                );
                row.gate_on = Some(true);
                row
            },
            {
                let mut row = metered_full(
                    NOW - 3 * HOUR,
                    json!({"utilOverage": 0.05, "resetOverage": overage_now}),
                );
                row.gate_on = Some(true);
                row
            },
            {
                let mut row =
                    metered_full(NOW - 2 * HOUR, json!({"util5h": 0.30, "reset5h": reset_5h}));
                row.gate_on = Some(true);
                row
            },
            bare(NOW - 40 * MIN), // meter-less, flag-less: filtered out
            {
                let mut row = kind_row(NOW - 20 * MIN, RowKind::Error);
                row.status = Some(500);
                row // meter-less kind: filtered out
            },
            bare(NOW - 15 * MIN), // meter-less, flag-less: filtered out
            {
                let mut row =
                    metered_full(NOW - 10 * MIN, json!({"util5h": 0.42, "reset5h": reset_5h}));
                row.kind = Some(RowKind::Released);
                row
            },
            {
                let mut row = bare(NOW - 5 * MIN);
                row.kind = Some(RowKind::Cold);
                row.gate_on = Some(false); // the flag-only production shape
                row
            },
            metered_full(
                NOW - MIN,
                json!({"utilOverage": 0.15, "resetOverage": overage_now}),
            ),
            {
                let mut row = metered_full(
                    NOW - MIN / 2,
                    json!({
                        "util5h": 0.42, "reset5h": reset_5h,
                        "utilOverage": 0.99, "resetOverage": overage_now,
                        "claim": "five_hour",
                    }),
                );
                row.kind = Some(RowKind::Blocked);
                row
            },
        ];
        store
            .record_requests(&rows)
            .expect("seed the scratch ledger");

        // OLD shape: the full-row materialisation the quota refresh
        // used to pay, projected onto the four fields the aggregation
        // reads — meter-less rows included, which is the point.
        let full = store.requests_since(0, 10_000).expect("full-row read");
        assert_eq!(full.len(), rows.len(), "the fixture is under the cap");
        let via_old = aggregate(&as_meter_rows(&full), NOW, TODAY, WINDOW_SINCE);

        // NEW path: the narrow read the quota cadence now uses.
        let narrow = store.meter_rows_since(0, 10_000).expect("narrow read");
        assert!(
            narrow
                .iter()
                .all(|row| row.rate_limits.is_some() || row.gate_on.is_some())
        );
        assert!(
            narrow.len() < full.len(),
            "the meter-less flag-less rows are filtered out of the narrow read"
        );
        let via_new = aggregate(&narrow, NOW, TODAY, WINDOW_SINCE);

        assert_eq!(
            via_old, via_new,
            "the narrow read must aggregate identically to the full-row read"
        );

        // Spot figures pinning WHICH rules had to survive the switch —
        // equal-but-wrong would still fail here.
        let agg = via_new.expect("readings exist");
        assert!(
            !agg.gate_on && !agg.gate_assumed,
            "the gate flag was read off the meter-less cold row — a \
             rate_limits-only filter would flip it to the assumed default"
        );
        let overage = agg
            .meters
            .iter()
            .find(|meter| meter.key == "overage")
            .expect("the overage meter renders");
        assert!(
            (overage.util - 0.99).abs() < 1e-9,
            "the blocked stale copy is the newest reading"
        );
        assert_eq!(agg.binding.as_deref(), Some("five_hour"));
        // The span totals: the second period opened inside the today
        // span (anchored at zero, +0.15) on top of the first period's
        // +0.10 floor; the window span baselines off the reading before
        // it. Proxy kinds never baseline either total.
        assert_spent(
            agg.spent_today.expect("the overage meter renders"),
            0.25,
            true,
        );
        assert_spent(
            agg.spent_window.expect("the overage meter renders"),
            0.10,
            false,
        );

        // The absent case is parity too: no snapshots anywhere, both
        // reads leave the section absent.
        let store = Store::open(":memory:").expect("scratch store");
        store.record_request(&bare(NOW - MIN)).expect("record");
        let full = store.requests_since(0, 10).expect("full read");
        let narrow = store.meter_rows_since(0, 10).expect("narrow read");
        assert!(narrow.is_empty(), "a row with neither field is not fetched");
        assert_eq!(
            aggregate(&as_meter_rows(&full), NOW, TODAY, WINDOW_SINCE),
            aggregate(&narrow, NOW, TODAY, WINDOW_SINCE),
            "absence stays absence on both reads"
        );
    }

    // ── against the live production ledger ─────────────────────────

    #[test]
    #[ignore = "reads the live production ledger (the toker service's own \
                DB, read the same way the TUI reads it); run deliberately \
                with --ignored"]
    fn the_production_ledger_aggregates_without_panicking() {
        use crate::middleware::cold::{OUTLOOK_LOOKBACK_MS, OUTLOOK_ROWS};
        use crate::store::Store;

        let path = crate::store::default_db_path().expect("resolve the data home");
        assert!(path.exists(), "no production ledger at {path:?}");
        // The same open the TUI itself performs (WAL + busy timeout let
        // this read run while the daemon writes); nothing here writes.
        let store = Store::open(&path).expect("open the production ledger");

        let now_ms = jiff::Timestamp::now().as_millisecond();
        // The narrow meter-lookback read the loop's quota cadence now
        // uses — same window, same cap, four columns.
        let rows = store
            .meter_rows_since(now_ms.saturating_sub(OUTLOOK_LOOKBACK_MS), OUTLOOK_ROWS)
            .expect("read the meter lookback");
        let today_start_ms = jiff::Timestamp::from_millisecond(now_ms)
            .ok()
            .and_then(|ts| {
                ts.to_zoned(jiff::tz::TimeZone::system())
                    .start_of_day()
                    .ok()
                    .map(|day| day.timestamp().as_millisecond())
            })
            .unwrap_or(now_ms);
        // The display window stays on the full-row read — the display
        // aggregate consumes its session/model/token columns.
        let window_rows = store
            .requests_since(now_ms.saturating_sub(30 * 60_000), 10_000)
            .expect("read the display window");

        // The whole snapshot must aggregate over real rows without
        // panicking, whatever the ledger holds.
        let quota = crate::tui::quota::aggregate(
            &rows,
            now_ms,
            today_start_ms,
            now_ms.saturating_sub(30 * 60_000),
        );
        let snap = crate::tui::model::aggregate(
            &window_rows,
            quota.as_ref(),
            30,
            now_ms,
            store.count_requests().expect("count"),
        );

        let quota = snap.quota;
        eprintln!(
            "{} meter rows in the lookback, {} in the window; quota section: {quota:#?}",
            rows.len(),
            window_rows.len(),
        );
        let Some(quota) = quota else {
            // Absence is a verdict here, not a failure: a ledger whose
            // rows carry no meter snapshots renders no quota panel
            // (invariant 3), which is what production shows today.
            eprintln!(
                "no meter snapshots in the production ledger — the panel \
                 renders nothing (absence ≠ zero)"
            );
            return;
        };
        if let Some(seven) = quota.meters.iter().find(|m| m.key == "7d") {
            assert!(seven.reset_s.unwrap_or(0) > 0, "a 7-day reset is an epoch");
            assert!(
                (0.0..=1.5).contains(&seven.util),
                "utilisation is a fraction: {}",
                seven.util
            );
        }
        for meter in &quota.meters {
            assert!(
                (-1e-9..=1.5).contains(&meter.util),
                "{}: util out of range {}",
                meter.key,
                meter.util
            );
        }
    }

    // ── the read's cost, measured ─────────────────────────────────────

    #[test]
    #[ignore = "a timing probe, not a correctness gate: seeds a scratch \
                ledger at production scale (~25 000 rows, the shape the \
                ctp import left) and times 100 refreshes of both meter-\
                lookback paths in the debug build; run deliberately with \
                --ignored"]
    fn meter_lookback_timing_narrow_read_vs_full_row_read() {
        use crate::middleware::cold::{OUTLOOK_LOOKBACK_MS, OUTLOOK_ROWS};
        use crate::store::{RequestRow, Store};
        use crate::tui::testrows::{as_meter_rows, billed, kind_row, metered_full};
        use std::time::{Duration, Instant};

        const ITERATIONS: u32 = 100;
        const SEED: i64 = 25_000;
        const STEP_MS: i64 = OUTLOOK_LOOKBACK_MS / SEED; // one row ~24 s apart
        const HOUR_MS: i64 = 60 * 60_000;
        const DAY_MS: i64 = 24 * HOUR_MS;

        // A scratch ledger under /tmp/opencode — a file DB, WAL on disk,
        // the shape the loop actually reads. Never the live one.
        let dir = std::path::PathBuf::from("/tmp/opencode")
            .join(format!("toker-meter-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.join("bench.db")).expect("open the scratch ledger");

        // Production shape: 20 000 anthropic measurement rows carrying
        // full meter snapshots — a 1%-quantised 5-hour sawtooth that
        // rolls every 5 h, a creeping 7-day meter, a monthly overage —
        // as FULL rows (session, model, tokens, cost: the columns the
        // old read materialised), every fourth gate flag set; plus
        // meter-less flag-less rows the narrow read filters out
        // (openai-route measurements and error rows carry neither).
        let now_ms = 2_000_000_000_000;
        let since_ms = now_ms - OUTLOOK_LOOKBACK_MS;
        let reset_7d = (now_ms + 5 * DAY_MS) / 1000;
        let reset_overage = (now_ms + 30 * DAY_MS) / 1000;
        let mut batch: Vec<RequestRow> = Vec::new();
        for i in 0..SEED {
            let ts_ms = since_ms + i * STEP_MS;
            let row = if i % 5 == 4 {
                if i % 25 == 24 {
                    let mut row = kind_row(ts_ms, RowKind::Error);
                    row.status = Some(500);
                    row.error_type = Some("upstream".to_owned());
                    row
                } else {
                    billed(
                        ts_ms,
                        Some("ses-openai"),
                        "openai/gpt-5.2",
                        "openrouter",
                        900,
                        4_000,
                        250,
                        0.004,
                    )
                }
            } else {
                // ~741 rows fit in one 5-hour window at this spacing.
                let in_window = i % 741;
                let window_start = since_ms + (i / 741) * 5 * HOUR_MS;
                let q = |util: f64| (util * 100.0).round() / 100.0;
                let mut row = metered_full(
                    ts_ms,
                    json!({
                        "util5h": q(in_window as f64 / 741.0 * 0.95),
                        "reset5h": (window_start + 5 * HOUR_MS) / 1000,
                        "util7d": q(0.10 + i as f64 / SEED as f64 * 0.60),
                        "reset7d": reset_7d,
                        "utilOverage": q(0.55 + i as f64 / SEED as f64 * 0.10),
                        "resetOverage": reset_overage,
                        "status5h": "allowed", "status7d": "allowed",
                        "claim": "five_hour", "overageInUse": false,
                    }),
                );
                row.session_id = Some("ses-bench".to_owned());
                row.model = Some("claude-opus-5".to_owned());
                row.provider = Some("anthropic_sub".to_owned());
                row.input = Some(120_000);
                row.cache_read = Some(80_000);
                row.output = Some(2_000);
                row.cost_usd = Some(1.5);
                row.cost_kind = Some(crate::store::CostKind::PlanEquivalent);
                row.gate_on = Some(true);
                row
            };
            batch.push(row);
            if batch.len() == 2_500 {
                store.record_requests(&batch).expect("seed a batch");
                batch.clear();
            }
        }
        store.record_requests(&batch).expect("seed the tail batch");
        let total = store.count_requests().expect("count");
        assert_eq!(total, SEED, "the seed is complete");

        let today = now_ms - 12 * HOUR_MS;
        let window_since = now_ms - 30 * 60_000;

        // Warm both paths once — page cache and statement cache — so
        // the timed iterations measure the steady state.
        std::hint::black_box(
            store
                .requests_since(since_ms, OUTLOOK_ROWS)
                .expect("warm old"),
        );
        std::hint::black_box(
            store
                .meter_rows_since(since_ms, OUTLOOK_ROWS)
                .expect("warm new"),
        );

        // OLD: the full-row materialisation the quota refresh used to
        // pay — 59 columns, six JSON parses per row — plus the
        // aggregation over its projection. The projection is test
        // scaffolding the real path never ran (it read the fields off
        // the full rows directly), so this total is an UPPER bound on
        // the old refresh.
        let mut old_read = Duration::ZERO;
        let mut old_refresh = Duration::ZERO;
        for _ in 0..ITERATIONS {
            let t = Instant::now();
            let full = store
                .requests_since(since_ms, OUTLOOK_ROWS)
                .expect("full read");
            old_read += t.elapsed();
            let t = Instant::now();
            let agg = aggregate(&as_meter_rows(&full), now_ms, today, window_since);
            old_refresh += t.elapsed();
            std::hint::black_box(&agg);
        }

        // NEW: the narrow read the quota cadence now pays — four
        // columns, one JSON parse per row — plus the same aggregation.
        let mut new_read = Duration::ZERO;
        let mut new_refresh = Duration::ZERO;
        for _ in 0..ITERATIONS {
            let t = Instant::now();
            let narrow = store
                .meter_rows_since(since_ms, OUTLOOK_ROWS)
                .expect("narrow read");
            new_read += t.elapsed();
            let t = Instant::now();
            let agg = aggregate(&narrow, now_ms, today, window_since);
            new_refresh += t.elapsed();
            std::hint::black_box(&agg);
        }
        assert!(
            aggregate(
                &store
                    .meter_rows_since(since_ms, OUTLOOK_ROWS)
                    .expect("narrow read"),
                now_ms,
                today,
                window_since
            )
            .is_some(),
            "the seeded lookback must carry meter snapshots"
        );

        let ms = |total: Duration| total.as_secs_f64() * 1000.0 / ITERATIONS as f64;
        eprintln!(
            "meter lookback over {SEED} rows, {ITERATIONS} iterations, debug build:\n  \
             old read (requests_since)                 {:8.3} ms/refresh\n  \
             old read + aggregation (upper bound)      {:8.3} ms/refresh\n  \
             new read (meter_rows_since)               {:8.3} ms/refresh\n  \
             new read + aggregation (the refresh)      {:8.3} ms/refresh",
            ms(old_read),
            ms(old_read + old_refresh),
            ms(new_read),
            ms(new_read + new_refresh),
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
