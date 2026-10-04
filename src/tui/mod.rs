//! Ratatui dashboard (plan: "TUI").
//!
//! A long-running terminal view over the SQLite ledger, refreshing ~every
//! 2 s — the replacement for `watch … live.mjs`. Phase 1 rendered the
//! sessions, spend, and rate panels; phase 2 grows the rate panel into
//! **rate & quota** (meter bars, forecasts, spent, binding — [quota]),
//! and the rest of the panel set (context bars, tokens, cache rebuilds)
//! lands with phase 5. The split into a pure aggregation [model] plus a
//! [view] over it exists so those panels can grow without touching
//! terminal plumbing.
//!
//! - [model]: narrow display rows in, dashboard snapshot out. No
//!   terminal types — testable against synthetic [`crate::store::DisplayRow`]
//!   sets alone (the store's display-window projection, invariant 7).
//! - [quota]: the rate & quota section of that snapshot — ctp's meter
//!   forecasting, ported from forecast.mjs/live.mjs.
//! - [rebuilds]: the CACHE REBUILDS section — ctp's lane walk and cause
//!   classification over the store's rebuild-tail projection.
//! - [view]: snapshot + frame in, pixels out, via ratatui. Rendering is
//!   exercised with ratatui's `TestBackend`, never a real terminal.
//! - [`run`]: the loop wiring them to the store. Reads tolerate a
//!   concurrently-writing daemon — WAL plus the store's busy timeout
//!   already cover it (see `store` module docs).
//!
//! Invariant 3 (absence ≠ zero) runs through the whole dashboard: the
//! aggregation keeps `None` for anything the ledger does not know (no
//! billed cost, unknown token counts) and the view renders those as
//! explicit "no … data" / `?` strings, never as zero.

mod model;
mod quota;
mod rebuilds;
mod view;

use std::path::Path;
use std::time::{Duration, Instant};

use crate::middleware::cold::{OUTLOOK_LOOKBACK_MS, OUTLOOK_ROWS};
use crate::store::Store;

/// Refresh cadence: the plan's "~2 s refresh from SQLite" — for the
/// DISPLAY window (sessions, spend, rate, context, tokens), which reads
/// the store's narrow display projection (fifteen columns, no JSON
/// parse per row — see [`refresh_display`]).
const REFRESH: Duration = Duration::from_secs(2);

/// The meter-lookback cadence. The quota section reads a 7-day,
/// 20 000-row window through the store's narrow meter projection (four
/// columns, one JSON parse per row — see [`quota_snapshot`]) and
/// aggregates burn rates over it — far heavier than the display read,
/// and nothing about it changes on a 2-second scale: meters move on
/// the upstream's window scale (hours), and ctp itself refit its quota
/// model every 30 MINUTES (QUOTA_REFIT_MS). Refreshing it at the
/// display cadence made the loop spin: the read overran the tick, the
/// next deadline landed in the past, and `event::poll(0)` never
/// blocked — 80% of a core, fixed here.
///
/// The CACHE REBUILDS section reads on this same cadence
/// ([`rebuild_snapshot`]): its lane walk needs the 24 h tail that
/// provides each lane's pre-window predecessor (the anti-phantom
/// rule), an order of magnitude more rows than the display window —
/// and rebuild causes move on the conversation's scale, not the
/// 2-second one.
const QUOTA_REFRESH: Duration = Duration::from_secs(60);

/// The rebuild walk's tail: 24 hours, strictly longer than the longest
/// display window (`--window-mins` caps at 1440), so every lane whose
/// predecessor predates the window still gets its real predecessor —
/// the anti-phantom rule (ctp lanes.md / live.mjs:208-215).
const REBUILD_TAIL_MS: i64 = 24 * 60 * 60 * 1000;

/// The rebuild tail's row cap, sized like ctp live.mjs's effective
/// tail (16 MB of JSONL ≈ 20-30 k rows): enough to cover the tail many
/// times over at any plausible request rate — a single day's traffic
/// is nowhere near it — so the cap only guards a pathologically hot
/// ledger. Like the meter cap, it keeps the NEWEST rows.
const REBUILD_ROWS: u64 = 20_000;

/// Per-refresh row cap. `display_rows_since` keeps the newest rows; a
/// 30-minute single-user window is nowhere near this, so the cap only
/// guards a pathologically hot ledger.
const ROW_CAP: u64 = 10_000;

/// `toker tui --window-mins <m>`: open the ledger and run the dashboard
/// loop until the user quits. Terminal setup/teardown goes through
/// `ratatui::try_init` (alternate screen, raw mode, a panic hook that
/// restores first) plus the [`RestoreGuard`] below, so raw mode is restored
/// on every exit path — `?` returns, `q`, and panics via the guard's drop.
pub fn run(db_path: &Path, window_mins: u64) -> anyhow::Result<()> {
    let store = Store::open(db_path)?;
    let mut terminal = ratatui::try_init()?;
    let _restore = RestoreGuard;

    // The system zone, read once: every local clock the panels render
    // (the quota resets and runout labels) anchors here.
    let tz = jiff::tz::TimeZone::system();
    // Both deadlines start in the past: the first pass refreshes
    // immediately. Every reschedule below anchors at the COMPLETION of
    // the work, never its start — a read that overruns its interval
    // delays the next one instead of collapsing the loop into a
    // back-to-back refresh spin.
    let mut next_display = Instant::now();
    let mut next_quota = Instant::now();
    let mut quota: Option<quota::QuotaAgg> = None;
    let mut rebuilds: Option<rebuilds::RebuildAgg> = None;
    let mut snapshot = model::empty(window_mins);
    loop {
        let now = Instant::now();
        if now >= next_quota {
            quota = quota_snapshot(&store, window_mins)?;
            rebuilds = rebuild_snapshot(&store, window_mins)?;
            next_quota = Instant::now() + QUOTA_REFRESH;
        }
        if now >= next_display {
            let released = released_sessions(&store, quota.as_ref())?;
            snapshot = refresh_display(
                &store,
                window_mins,
                quota.as_ref(),
                &released,
                rebuilds.as_ref(),
            )?;
            next_display = Instant::now() + REFRESH;
        }
        terminal.draw(|frame| view::render(frame, &snapshot, &clock(), &tz))?;

        // Block until the sooner of the two deadlines or an input event
        // — no busy loop, by construction (both deadlines are in the
        // future after the reschedules above).
        let timeout = next_display
            .min(next_quota)
            .saturating_duration_since(Instant::now());
        if !crossterm::event::poll(timeout)? {
            continue;
        }
        let event = crossterm::event::read()?;
        // Everything that is not a key press (resize, mouse-move, key
        // release) falls through: the next draw re-renders at the new
        // size (ratatui auto-resizes in draw).
        if let crossterm::event::Event::Key(key) = event {
            if key.kind != crossterm::event::KeyEventKind::Press {
                continue;
            }
            match key.code {
                // q (or the habitual Ctrl+C in raw mode) quits.
                crossterm::event::KeyCode::Char('q') => break,
                crossterm::event::KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    break;
                }
                // r forces an immediate refresh on the next pass — both
                // cadences, so a full reload is one keypress away.
                crossterm::event::KeyCode::Char('r') => {
                    next_display = Instant::now();
                    next_quota = Instant::now();
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Reload the display window's rows and total, then aggregate with the
/// CACHED quota and rebuild sections plus the live-allowance set. The
/// read is the store's narrow display projection
/// ([`Store::display_rows_since`]): the fifteen columns the
/// aggregation consumes, no JSON parse per row — the full-row read
/// this path used to pay cast 59 columns and parsed six JSON values
/// per row, every 2 s (invariant 7). Errors propagate — with WAL and
/// the store's 5 s busy timeout a read failure is real trouble, not a
/// blip worth hiding behind a stale frame.
fn refresh_display(
    store: &Store,
    window_mins: u64,
    quota: Option<&quota::QuotaAgg>,
    released: &std::collections::HashSet<String>,
    rebuilds: Option<&rebuilds::RebuildAgg>,
) -> anyhow::Result<model::Snapshot> {
    let now_ms = jiff::Timestamp::now().as_millisecond();
    let since = now_ms.saturating_sub(window_mins.saturating_mul(60_000) as i64);
    let rows = store.display_rows_since(since, ROW_CAP)?;
    let total = store.count_requests()?;
    Ok(model::aggregate(
        &rows,
        quota,
        released,
        rebuilds.cloned(),
        window_mins,
        now_ms,
        total,
    ))
}

/// The CACHE REBUILDS section, on the quota cadence (the heavy read's
/// own tick): the 24 h tail through the store's narrow rebuild
/// projection, the lane walk over it, then the targeted second query
/// that fetches the heavy ladder columns for — and only for — the rows
/// a system-prompt change was attributed to (typically none at all).
/// The tail reaches back past the display window so lanes get their
/// real predecessors (the anti-phantom rule); the walk classifies only
/// the window's rows.
fn rebuild_snapshot(
    store: &Store,
    window_mins: u64,
) -> anyhow::Result<Option<rebuilds::RebuildAgg>> {
    let now_ms = jiff::Timestamp::now().as_millisecond();
    let since = now_ms.saturating_sub(window_mins.saturating_mul(60_000) as i64);
    let tail = now_ms.saturating_sub(REBUILD_TAIL_MS);
    let rows = store.rebuild_rows_since(tail, REBUILD_ROWS)?;
    let mut walk = rebuilds::classify(&rows, since);
    // The localisation's second query: the changed rows and their
    // predecessors, by id — never the whole tail.
    let mut ids = Vec::new();
    for event in &walk.events {
        if let Some(system) = &event.system {
            ids.push(system.row_id);
            ids.push(system.prev_id);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    let localisation = store.localisation_rows(&ids)?;
    let by_id: std::collections::HashMap<_, _> =
        localisation.into_iter().map(|row| (row.id, row)).collect();
    rebuilds::localise(&mut walk.events, &by_id);
    Ok(Some(rebuilds::aggregate(walk)))
}

/// The sessions holding a live allowance for the quota window now
/// running — the `$` marker's set (live.mjs:282-296). A release names
/// the window it was for, so it only counts while that window is the
/// current one: a stored `(meter, reset)` allowance is live iff the
/// newest meter reading still reports that reset. Nothing is live
/// without a reading to match against (no meter, no window to be
/// released past), and the gate disarmed means nobody is being
/// released past anything. The allowances table is small and
/// self-expiring by its reset-keyed design, so the read is the whole
/// table on the display tick — cheap, and filtered here.
fn released_sessions(
    store: &Store,
    quota: Option<&quota::QuotaAgg>,
) -> anyhow::Result<std::collections::HashSet<String>> {
    let Some(quota) = quota else {
        return Ok(std::collections::HashSet::new());
    };
    if !quota.gate_on {
        return Ok(std::collections::HashSet::new());
    }
    // The resets the newest reading reports, per meter key — the
    // windows a release can be live for.
    let mut resets: Vec<(&str, i64)> = Vec::new();
    for meter in &quota.meters {
        if let Some(reset) = meter.reset_s
            && matches!(meter.key, "5h" | "7d")
        {
            resets.push((meter.key, reset));
        }
    }
    let mut released = std::collections::HashSet::new();
    if resets.is_empty() {
        return Ok(released);
    }
    for allowance in store.load_allowances()? {
        if resets
            .iter()
            .any(|(meter, reset)| allowance.meter == *meter && allowance.reset_value == *reset)
        {
            released.insert(allowance.session_id);
        }
    }
    Ok(released)
}

/// Reload the meter lookback and aggregate the quota section — the
/// heavy read, on its own cadence. A burn rate and a spent span need
/// history a display window cannot hold, ctp's "readings are taken
/// from every row read, not just the windowed ones" (live.mjs:269-272);
/// the lookback reuses the cold outlook's constants: same 7-day span,
/// same row cap.
///
/// The read is the store's narrow meter projection
/// ([`Store::meter_rows_since`]): rows that carry neither a snapshot
/// nor a gate flag never reach the aggregation (a row without
/// `rate_limits` contributes no reading, and only the gate flag is
/// read off meter-less rows — the gate-seen rule reads it from any row
/// in the lookback). Semantics change against the old full-row read,
/// documented here deliberately: the [`OUTLOOK_ROWS`] cap now applies
/// to the snapshot-or-flag rows the aggregation can consume, so in an
/// over-cap lookback the newest 20 000 such rows are kept rather than
/// the newest 20 000 rows of any kind — strictly more of the lookback's
/// signal fits under the cap, and the rows the filter skips are
/// exactly the ones the old aggregation read and could not use.
fn quota_snapshot(store: &Store, window_mins: u64) -> anyhow::Result<Option<quota::QuotaAgg>> {
    let now_ms = jiff::Timestamp::now().as_millisecond();
    let quota_rows =
        store.meter_rows_since(now_ms.saturating_sub(OUTLOOK_LOOKBACK_MS), OUTLOOK_ROWS)?;
    Ok(quota::aggregate(
        &quota_rows,
        now_ms,
        local_day_start_ms(now_ms),
        now_ms.saturating_sub(window_mins.max(1) as i64 * 60_000),
    ))
}

/// The local day's start (midnight, the system zone) in epoch ms — the
/// quota panel's `spent today` anchor. "Today" is the user's day, not
/// UTC's (ctp live.mjs:604's `setHours(0,0,0,0)`). A clock outside
/// jiff's representable range cannot name a day, so `now` stands in:
/// the today span degenerates to empty rather than guessing.
fn local_day_start_ms(now_ms: i64) -> i64 {
    jiff::Timestamp::from_millisecond(now_ms)
        .ok()
        .and_then(|ts| {
            ts.to_zoned(jiff::tz::TimeZone::system())
                .start_of_day()
                .ok()
                .map(|day| day.timestamp().as_millisecond())
        })
        .unwrap_or(now_ms)
}

/// The local-clock string for the header (HH:MM:SS, the system zone). Kept
/// out of [view] so rendering stays a pure function of its inputs and the
/// tests stay deterministic.
fn clock() -> String {
    jiff::Zoned::now().strftime("%H:%M:%S").to_string()
}

/// Restores the terminal (raw mode off, alternate screen left) on drop —
/// the RAII half of teardown, covering `?` returns and panics. The panic
/// hook `try_init` installed covers the other half, restoring before the
/// panic message prints.
struct RestoreGuard;

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        let _ = ratatui::try_restore();
    }
}

#[cfg(test)]
pub(crate) mod testrows {
    //! Shared synthetic-row builder for the model and view tests. A bare
    //! row (every optional column NULL) plus the mutators each test needs —
    //! the store deliberately has no `Default`, so the full field list
    //! lives once, here. Both narrow shapes have their own builders here
    //! too: each aggregation's tests build its native input directly,
    //! and the `as_*_rows` projections are the parity bridges the
    //! narrow-read proofs run through.

    use crate::store::{
        CostKind, DisplayRow, MeterRow, RebuildRow, RequestRow, RowKind, is_api_measurement,
    };

    /// A measurement row with every optional column NULL, at `ts_ms`.
    pub(crate) fn bare(ts_ms: i64) -> RequestRow {
        RequestRow {
            id: None,
            ts_ms,
            duration_ms: None,
            kind: None,
            frontend: None,
            provider: None,
            route: None,
            session_id: None,
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
            rate_limits: None,
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
            gate_on: None,
            cold_on: None,
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

    /// A billed measurement row: session, model, provider, tokens, cost.
    /// Eight positional fields is the honest shape of a ledger row; a
    /// builder struct would just rename them.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn billed(
        ts_ms: i64,
        session: Option<&str>,
        model: &str,
        provider: &str,
        input: i64,
        cache_read: i64,
        output: i64,
        cost: f64,
    ) -> RequestRow {
        let mut row = bare(ts_ms);
        row.session_id = session.map(str::to_string);
        row.model = Some(model.to_string());
        row.provider = Some(provider.to_string());
        row.input = Some(input);
        row.cache_read = Some(cache_read);
        row.output = Some(output);
        row.cost_usd = Some(cost);
        row.cost_kind = Some(CostKind::Billed);
        row
    }

    /// A meter-lookback row with every field absent — the shape of a
    /// ledger row the narrow read would fetch only for its gate flag
    /// (or not at all), and the base every meter fixture below builds
    /// on.
    pub(crate) fn meter_bare(ts_ms: i64) -> MeterRow {
        MeterRow {
            ts_ms,
            kind: None,
            gate_on: None,
            rate_limits: None,
        }
    }

    /// A meter-lookback row carrying an anthropic meter snapshot at
    /// `ts_ms` — ctp's `rateLimits` shape (util/reset per window plus
    /// the status/claim/overage fields), for the quota panel's tests.
    /// The quota aggregation's native input: the narrow read returns
    /// these, so its tests build them directly.
    pub(crate) fn metered(ts_ms: i64, limits: serde_json::Value) -> MeterRow {
        let mut row = meter_bare(ts_ms);
        row.rate_limits = Some(limits);
        row
    }

    /// A FULL ledger row carrying an anthropic meter snapshot at
    /// `ts_ms` — for fixtures that feed BOTH the quota aggregation (via
    /// [`as_meter_rows`]) and the display aggregation (via
    /// [`as_display_rows`]): the full row is the shape the OLD reads
    /// materialised, so the parity tests project it onto each narrow
    /// shape.
    pub(crate) fn metered_full(ts_ms: i64, limits: serde_json::Value) -> RequestRow {
        let mut row = bare(ts_ms);
        row.rate_limits = Some(limits);
        row
    }

    /// Project full ledger rows onto the meter lookback's narrow
    /// shape, keeping EVERY row (no filter). The parity bridge: the
    /// old quota path materialised full rows and read these four fields
    /// off them, so aggregating this projection of a full-row read must
    /// equal aggregating the narrow read — including over rows the
    /// narrow read's filter skips, which is exactly what the parity
    /// test proves the aggregation is insensitive to.
    pub(crate) fn as_meter_rows(rows: &[RequestRow]) -> Vec<MeterRow> {
        rows.iter()
            .map(|row| MeterRow {
                ts_ms: row.ts_ms,
                kind: row.kind,
                gate_on: row.gate_on,
                rate_limits: row.rate_limits.clone(),
            })
            .collect()
    }

    /// A display-window row with every optional column NULL, at `ts_ms`
    /// — the narrow shape the display tick reads, and the display
    /// aggregation's native input, so its tests build these directly.
    pub(crate) fn display_bare(ts_ms: i64) -> DisplayRow {
        DisplayRow {
            ts_ms,
            kind: None,
            session_id: None,
            model: None,
            provider: None,
            input: None,
            cache_read: None,
            cache_write_5m: None,
            cache_write_1h: None,
            output: None,
            cost_usd: None,
            cost_kind: None,
            req_messages: None,
            compact_generations: None,
            forced_to: None,
        }
    }

    /// A billed display row: session, model, provider, tokens, cost —
    /// the same eight positional fields as the full-row builder.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn display_billed(
        ts_ms: i64,
        session: Option<&str>,
        model: &str,
        provider: &str,
        input: i64,
        cache_read: i64,
        output: i64,
        cost: f64,
    ) -> DisplayRow {
        let mut row = display_bare(ts_ms);
        row.session_id = session.map(str::to_string);
        row.model = Some(model.to_string());
        row.provider = Some(provider.to_string());
        row.input = Some(input);
        row.cache_read = Some(cache_read);
        row.output = Some(output);
        row.cost_usd = Some(cost);
        row.cost_kind = Some(CostKind::Billed);
        row
    }

    /// A proxy-written display row of `kind` at `ts_ms` (never an API
    /// measurement).
    pub(crate) fn display_kind_row(ts_ms: i64, kind: RowKind) -> DisplayRow {
        let mut row = display_bare(ts_ms);
        row.kind = Some(kind);
        row
    }

    /// Project full ledger rows onto the display window's narrow
    /// shape, keeping EVERY row (no filter — the display read has
    /// none). The parity bridge: the old display path materialised
    /// full rows and read these fifteen fields off them, so aggregating
    /// this projection of a full-row read must equal aggregating the
    /// narrow read.
    pub(crate) fn as_display_rows(rows: &[RequestRow]) -> Vec<DisplayRow> {
        rows.iter()
            .map(|row| DisplayRow {
                ts_ms: row.ts_ms,
                kind: row.kind,
                session_id: row.session_id.clone(),
                model: row.model.clone(),
                provider: row.provider.clone(),
                input: row.input,
                cache_read: row.cache_read,
                cache_write_5m: row.cache_write_5m,
                cache_write_1h: row.cache_write_1h,
                output: row.output,
                cost_usd: row.cost_usd,
                cost_kind: row.cost_kind,
                req_messages: row.req_messages,
                compact_generations: row.compact_generations,
                forced_to: row.forced_to.clone(),
            })
            .collect()
    }

    /// A rebuild-walk row with every optional column NULL, at `ts_ms`
    /// — the narrow shape the quota-cadence rebuild read returns, and
    /// the lane walk's native input, so its tests build these
    /// directly. `id` is a placeholder the walk's tests override; the
    /// store assigns real ids at insert.
    pub(crate) fn rebuild_bare(ts_ms: i64) -> RebuildRow {
        RebuildRow {
            id: 0,
            ts_ms,
            session_id: None,
            tools_hash: None,
            system_hash: None,
            system_chars: None,
            system_blocks: None,
            req_messages: None,
            compact_generations: None,
            summarising: None,
            cache_read: None,
            cache_write_total: None,
            input: None,
            model: None,
        }
    }

    /// Project full ledger rows onto the rebuild walk's narrow shape,
    /// keeping only measurement rows — the projection of the OLD
    /// full-row path this walk's parity tests run through (the narrow
    /// read's `kind IS NULL` filter, applied to rows the full read
    /// would have returned).
    pub(crate) fn as_rebuild_rows(rows: &[RequestRow]) -> Vec<RebuildRow> {
        rows.iter()
            .filter(|row| is_api_measurement(row.kind))
            .map(|row| RebuildRow {
                id: row.id.unwrap_or(0),
                ts_ms: row.ts_ms,
                session_id: row.session_id.clone(),
                tools_hash: row.tools_hash.clone(),
                system_hash: row.system_hash.clone(),
                system_chars: row.system_chars,
                system_blocks: row.system_blocks.clone(),
                req_messages: row.req_messages,
                compact_generations: row.compact_generations,
                summarising: row.summarising,
                cache_read: row.cache_read,
                cache_write_total: row.cache_write_total,
                input: row.input,
                model: row.model.clone(),
            })
            .collect()
    }

    /// A proxy-written row of `kind` at `ts_ms` (never an API measurement).
    pub(crate) fn kind_row(ts_ms: i64, kind: RowKind) -> RequestRow {
        let mut row = bare(ts_ms);
        row.kind = Some(kind);
        row
    }
}

#[cfg(test)]
mod tests {
    //! The parity probe against the live production ledger — the real
    //! work-shaped data the imported ctp history is. Read-only, run
    //! deliberately with `--ignored` (the service is live; the probe
    //! reads exactly the way the TUI's own ticks do and writes
    //! nothing).

    use std::collections::{HashMap, HashSet};

    use crate::store::Store;

    /// The whole ledger as the probe's window: every imported row
    /// classified, every figure summed. The caps sit above the
    /// ledger's size by construction (checked, not assumed).
    const EVERYTHING: u64 = 1_000_000;

    #[test]
    #[ignore = "reads the live production ledger (the toker service's own \
                DB, read the same way the TUI reads it, including the \
                rebuild tail and the localisation second query); run \
                deliberately with --ignored"]
    fn the_production_ledger_classifies_and_aggregates_the_imported_history() {
        let path = crate::store::default_db_path().expect("resolve the data home");
        assert!(path.exists(), "no production ledger at {path:?}");
        // The same open the TUI itself performs (WAL + busy timeout let
        // this read run while the daemon writes); nothing here writes.
        let store = Store::open(&path).expect("open the production ledger");

        let now_ms = jiff::Timestamp::now().as_millisecond();
        let display = store
            .display_rows_since(0, EVERYTHING)
            .expect("read the display window");
        let earliest = display.iter().map(|row| row.ts_ms).min().unwrap_or(now_ms);
        let window_mins = ((now_ms - earliest) / 60_000).max(1) as u64;
        let snap = crate::tui::model::aggregate(
            &display,
            None,
            &HashSet::new(),
            None,
            window_mins,
            now_ms,
            store.count_requests().expect("count"),
        );

        eprintln!("== the real-data parity probe ==");
        eprintln!(
            "sessions in window: {} ({} measurement rows in window, {} in ledger)",
            snap.sessions.len(),
            snap.window_requests,
            snap.total_requests
        );

        // The ctx-ceiling distribution over the window's sessions:
        // exact/declared/unknown, counted per session.
        let mut ceilings: HashMap<String, usize> = HashMap::new();
        for session in &snap.sessions {
            let label = match session.ctx {
                crate::catalog::windows::ContextWindow::Exact { tokens }
                | crate::catalog::windows::ContextWindow::Declared { tokens } => {
                    format!("{} ({})", tokens, session.ctx.kind())
                }
                _ => "? (unknown)".to_owned(),
            };
            *ceilings.entry(label).or_default() += 1;
        }
        eprintln!("ctx-ceiling distribution (per session):");
        let mut sorted: Vec<_> = ceilings.into_iter().collect();
        sorted.sort();
        for (label, count) in &sorted {
            eprintln!("  {label}: {count}");
        }

        // The tokens panel over the same span.
        let tokens = &snap.tokens;
        let hit = match tokens.hit_rate() {
            crate::tui::model::HitRate::Rate(rate) => format!("{rate:.3}"),
            crate::tui::model::HitRate::Unknown => "? (cache metrics unavailable)".to_owned(),
            crate::tui::model::HitRate::NothingReusable => "nothing reusable".to_owned(),
        };
        eprintln!(
            "tokens: fresh input {} (+{} unknown), cache read {} (+{}), \
             write 1h {} (+{}), write 5m {} (+{}), output {} (+{}), \
             {} req; hit rate {hit}; {} cold req; written {}",
            tokens.fresh_input.value,
            tokens.fresh_input.unavailable,
            tokens.cache_read.value,
            tokens.cache_read.unavailable,
            tokens.write_1h.value,
            tokens.write_1h.unavailable,
            tokens.write_5m.value,
            tokens.write_5m.unavailable,
            tokens.output.value,
            tokens.output.unavailable,
            tokens.requests,
            tokens.cold,
            tokens.written(),
        );

        // The rebuild walk over the whole imported history: the 24 h
        // tail's read with the window opened to everything, then the
        // targeted localisation fetch for the rows a system-prompt
        // change was attributed to.
        let rebuild_rows = store
            .rebuild_rows_since(0, EVERYTHING)
            .expect("read the rebuild tail");
        let mut walk = crate::tui::rebuilds::classify(&rebuild_rows, 0);
        let mut ids = Vec::new();
        for event in &walk.events {
            if let Some(system) = &event.system {
                ids.push(system.row_id);
                ids.push(system.prev_id);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        let localisation = store
            .localisation_rows(&ids)
            .expect("read the localisations");
        eprintln!(
            "rebuild read: {} measurement rows, {} localisation columns fetched for {} ids",
            rebuild_rows.len(),
            localisation.len(),
            ids.len(),
        );
        let by_id: HashMap<_, _> = localisation.into_iter().map(|row| (row.id, row)).collect();
        crate::tui::rebuilds::localise(&mut walk.events, &by_id);
        let rebuilds = crate::tui::rebuilds::aggregate(walk);
        eprintln!(
            "rebuilds: {} of {} measured requests rewrote ≥{} tokens ({} unknown)",
            rebuilds.rebuilds,
            rebuilds.measured,
            crate::tui::rebuilds::REBUILD_MIN,
            rebuilds.unmeasured,
        );
        eprintln!("causes:");
        for (cause, count) in &rebuilds.causes {
            eprintln!("  {:<24} {}", cause.label(), count);
        }
        for event in rebuilds
            .events
            .iter()
            .filter(|event| event.cause == crate::tui::rebuilds::Cause::SystemPrompt)
            .take(3)
        {
            eprintln!(
                "  · {} — system prompt changed ({})",
                event.session,
                event.detail.as_deref().unwrap_or("")
            );
        }
    }
}
