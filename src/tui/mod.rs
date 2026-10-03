//! Ratatui dashboard (plan: "TUI").
//!
//! A long-running terminal view over the SQLite ledger, refreshing ~every
//! 2 s — the replacement for `watch … live.mjs`. Phase 1 renders the
//! sessions, spend, and rate panels; the full panel set (context bars,
//! tokens, cache rebuilds, quota) lands with phase 5, and the split into a
//! pure aggregation [model] plus a [view] over it exists so those panels
//! can grow without touching terminal plumbing.
//!
//! - [model]: ledger rows in, dashboard snapshot out. No terminal types —
//!   testable against synthetic [`RequestRow`] sets alone.
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
mod view;

use std::path::Path;
use std::time::{Duration, Instant};

use crate::store::Store;

/// Refresh cadence: the plan's "~2 s refresh from SQLite".
const REFRESH: Duration = Duration::from_secs(2);

/// Per-refresh row cap. `requests_since` keeps the newest rows; a 30-minute
/// single-user window is nowhere near this, so the cap only guards a
/// pathologically hot ledger.
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

    let mut next_refresh = Instant::now(); // first pass refreshes immediately
    let mut snapshot = model::empty(window_mins);
    loop {
        let now = Instant::now();
        if now >= next_refresh {
            snapshot = refresh(&store, window_mins)?;
            next_refresh = now + REFRESH;
        }
        terminal.draw(|frame| view::render(frame, &snapshot, &clock()))?;

        // Block until the next refresh is due or an event arrives — no
        // busy loop.
        let timeout = next_refresh.saturating_duration_since(Instant::now());
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
                // r forces an immediate refresh on the next pass.
                crossterm::event::KeyCode::Char('r') => next_refresh = Instant::now(),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Reload the window's rows and total, then aggregate. Errors propagate —
/// with WAL and the store's 5 s busy timeout a read failure is real
/// trouble, not a blip worth hiding behind a stale frame.
fn refresh(store: &Store, window_mins: u64) -> anyhow::Result<model::Snapshot> {
    let now_ms = jiff::Timestamp::now().as_millisecond();
    let since = now_ms.saturating_sub(window_mins.saturating_mul(60_000) as i64);
    let rows = store.requests_since(since, ROW_CAP)?;
    let total = store.count_requests()?;
    Ok(model::aggregate(&rows, window_mins, now_ms, total))
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
    //! lives once, here.

    use crate::store::{CostKind, RequestRow, RowKind};

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

    /// A proxy-written row of `kind` at `ts_ms` (never an API measurement).
    pub(crate) fn kind_row(ts_ms: i64, kind: RowKind) -> RequestRow {
        let mut row = bare(ts_ms);
        row.kind = Some(kind);
        row
    }
}
