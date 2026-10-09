//! Ratatui dashboard (plan: "TUI").
//!
//! A long-running terminal view over the SQLite ledger, ticking every
//! second and re-reading only when the ledger changed — the replacement
//! for the predecessor's watched log script.
//! Phase 1 rendered the
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
//! - [labels]: session names from Claude Code's own transcripts — the
//!   predecessor's transcript reader, ported. Read-only, tail-only,
//!   newest-wins; a session
//!   with no name keeps its id.
//! - [quota]: the rate & quota section of that snapshot — the
//!   predecessor's meter
//!   forecasting, ported.
//! - [rebuilds]: the CACHE REBUILDS section — the predecessor's lane
//!   walk and cause
//!   classification over the store's rebuild-tail projection.
//! - [view]: snapshot + frame in, pixels out, via ratatui. Rendering is
//!   exercised with ratatui's `TestBackend`, never a real terminal.
//! - [detail]: the session popup a click opens, with the session's
//!   fuller details and its quota gate controls. The one place the
//!   dashboard writes: its grants and revokes go through
//!   [`crate::release`], the writes the release markers make.
//! - [reexec]: the loop's watch on its own binary, so a dashboard left
//!   open across an install restarts as the new one.
//! - [`run`]: the loop wiring them to the store. Reads tolerate a
//!   concurrently-writing daemon — WAL plus the store's busy timeout
//!   already cover it (see `store` module docs).
//!
//! Invariant 3 (absence ≠ zero) runs through the whole dashboard: the
//! aggregation keeps `None` for anything the ledger does not know (no
//! billed cost, unknown token counts) and the view renders those as
//! explicit "no … data" / `?` strings, never as zero.

mod detail;
mod labels;
mod locale;
mod model;
mod quota;
mod rebuilds;
mod reexec;
mod view;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::catalog::fetched::{self, FetchedCatalogs};
use crate::middleware::cold::{OUTLOOK_LOOKBACK_MS, OUTLOOK_ROWS};
use crate::store::Store;

/// The loop's tick. Every tick redraws, so the clock, the idle ages,
/// and the window's aging advance by the second; what it READS is
/// gated on the ledger having changed (see [`Cadence`]) — a quiet
/// ledger costs one `PRAGMA data_version` per tick, not a window read.
const TICK: Duration = Duration::from_secs(1);

/// The display window is re-read at least this often even when the
/// ledger has not moved. The window itself needs no re-read to age:
/// the cached rows are dropped in memory as they cross the boundary
/// ([`DisplayWindow::age_out`]). The floor is for what lives outside
/// the ledger — a session renamed in its transcript while its lane is
/// quiet — so its label still catches up.
const DISPLAY_FLOOR: Duration = Duration::from_secs(30);

/// The least spacing of the heavy passes (quota and rebuilds) while
/// new rows keep landing: a busy ledger moves `data_version` every
/// tick, and the heavy reads must not follow it there.
///
/// The quota section reads a 7-day, 20 000-row window through the
/// store's narrow meter projection (four columns, one JSON parse per
/// row — see [`quota_snapshot`]) and aggregates burn rates over it —
/// far heavier than the display read; meters move on the upstream's
/// window scale (hours), and the predecessor itself refit its quota
/// model every 30 MINUTES. Refreshing it at the display cadence once
/// made the loop spin: the read overran the tick, the next deadline
/// landed in the past, and `event::poll(0)` never blocked — 80% of a
/// core. Hence this spacing, and every reschedule anchoring at the
/// work's completion.
///
/// The CACHE REBUILDS section reads on this same cadence
/// ([`rebuild_snapshot`]): its lane walk needs the 24 h tail that
/// provides each lane's pre-window predecessor (the anti-phantom
/// rule), an order of magnitude more rows than the display window.
const QUOTA_MIN: Duration = Duration::from_secs(10);

/// The heavy passes re-run at least this often with no new data: the
/// quota section's forecasts and resets move with the clock, not only
/// with rows.
const QUOTA_REFRESH: Duration = Duration::from_secs(60);

/// The rebuild walk's tail: 24 hours, strictly longer than the longest
/// display window (`--window-mins` caps at 1440), so every lane whose
/// predecessor predates the window still gets its real predecessor —
/// the anti-phantom rule (as the lane docs state it: a predecessor must
/// be a served request, no matter how far back it sits).
const REBUILD_TAIL_MS: i64 = 24 * 60 * 60 * 1000;

/// The rebuild tail's row cap, sized like the predecessor dashboard's
/// effective
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
///
/// `extra_transcript_roots` is the config's `transcript_roots` — the
/// harness config directories whose transcripts the session labels also
/// look under (see [labels]; the same colon-list idea the predecessor
/// used).
pub fn run(
    db_path: &Path,
    window_mins: u64,
    extra_transcript_roots: &[PathBuf],
    gates: &crate::config::GatesConfig,
) -> anyhow::Result<()> {
    let store = Store::open(db_path)?;
    // Taken before the terminal: the file this process was started
    // from, while it is still the one on disk.
    let mut exe = reexec::ExeWatch::current();
    let mut terminal = ratatui::try_init()?;
    let restore = RestoreGuard;
    crossterm::execute!(std::io::stdout(), WheelCapture(true))?;

    // The system zone, read once: every local clock the panels render
    // (the quota resets and runout labels) anchors here.
    let tz = jiff::tz::TimeZone::system();
    // The display locale, resolved once (see [locale]): clocks and
    // grouped counts follow it; nothing model-visible does.
    let mut ui = view::Ui {
        fmt: locale::Fmt::from_env(),
        no_color: view::no_color(std::env::var_os("NO_COLOR")),
        legend: false,
        scroll: view::Scroll::default(),
        gate_armed: gates.quota_enabled,
        detail: None,
    };
    let mut drawn = view::Drawn::default();
    // The transcript roots, resolved once: session labels read only
    // these, read-only, one tail per session per display read.
    let mut labels = labels::Labels::new(labels::transcript_roots(extra_transcript_roots));
    let mut cadence = Cadence::default();
    // The tick deadline starts in the past: the first pass reads
    // immediately. It is rescheduled from the COMPLETION of the tick's
    // work, never its start — a read that overruns the tick delays the
    // next one instead of collapsing the loop into a back-to-back
    // refresh spin.
    let mut next_tick = Instant::now();
    let mut quota: Option<quota::QuotaAgg> = None;
    let mut rebuilds: Option<rebuilds::RebuildAgg> = None;
    let mut released = model::Released::new();
    let mut window: Option<DisplayWindow> = None;
    // The daemon's models caches, read-only: loaded (or not) on the
    // quota cadence below, never fetched, never written.
    let mut model_caches = match fetched::cache_dir() {
        Ok(dir) => ModelCaches::new(dir),
        Err(error) => {
            // No data home: nothing to read — the CTX column falls back
            // to the hand-verified catalogue alone, absence rather than
            // zeros.
            eprintln!("tui: no models caches ({error})");
            ModelCaches::disabled()
        }
    };
    let mut snapshot = model::empty(window_mins);
    let mut upgraded = false;
    loop {
        if Instant::now() >= next_tick {
            // One stat a tick: the binary was replaced (an install), so
            // hand over to the new one rather than keep showing the
            // old code's view.
            if exe.as_mut().is_some_and(reexec::ExeWatch::check) {
                upgraded = true;
                break;
            }
            let due = cadence.due(Instant::now(), store.data_version()?);
            if due.heavy {
                quota = quota_snapshot(&store, window_mins)?;
                rebuilds = rebuild_snapshot(&store, window_mins, &ui.fmt)?;
                // The cache read rides the quota cadence: three stat
                // calls are free against it, and the files themselves
                // only move on the daemon's 24 h cycle.
                model_caches.refresh();
                cadence.heavy_done(Instant::now());
            }
            if due.read {
                window = Some(read_window(&store, window_mins, &mut labels)?);
                cadence.read_done(Instant::now());
            }
            // The allowances table changes only with the ledger, and a
            // release's liveness only with the quota section's resets.
            if due.read || due.heavy {
                released = released_sessions(&store, quota.as_ref())?;
            }
            if let Some(window) = &mut window {
                snapshot = window.snapshot(
                    window_mins,
                    quota.as_ref(),
                    &released,
                    rebuilds.as_ref(),
                    model_caches.catalogs(),
                );
            }
            if let Some(popup) = &mut ui.detail {
                // An unconfirmed BURN lapses.
                if popup
                    .burn_armed_until
                    .is_some_and(|until| Instant::now() >= until)
                {
                    popup.burn_armed_until = None;
                }
                // The popup follows the ledger like the panels do.
                if due.read || due.heavy {
                    popup.detail = load_detail(&store, &snapshot, &popup.detail.session);
                }
            }
            next_tick = Instant::now() + TICK;
        }
        terminal.draw(|frame| {
            drawn = view::render(frame, &snapshot, &clock(&ui.fmt), &tz, &ui);
        })?;
        // The list may have shrunk under its offset since the last
        // frame: hold the offset at what the frame could show.
        ui.scroll.sessions = ui.scroll.sessions.min(drawn.sessions.max_offset);
        ui.scroll.context = ui.scroll.context.min(drawn.context.max_offset);
        ui.scroll.rebuilds = ui.scroll.rebuilds.min(drawn.rebuilds.max_offset);
        ui.scroll.spend = ui.scroll.spend.min(drawn.spend.max_offset);

        // Block until the next tick or an input event. The deadline was
        // set after the tick's work, so it is in the future unless the
        // draw itself overran it; the floor keeps even that case a
        // wait rather than a `poll(0)` spin.
        let timeout = next_tick
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(50));
        if !crossterm::event::poll(timeout)? {
            continue;
        }
        let event = crossterm::event::read()?;
        // The wheel scrolls the list under the pointer, a row a notch; a
        // click opens a session's detail, or acts in the open one.
        if let crossterm::event::Event::Mouse(mouse) = event {
            let at = ratatui::layout::Position::new(mouse.column, mouse.row);
            if mouse.kind
                == crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
            {
                match (&mut ui.detail, &drawn.popup) {
                    (Some(popup), Some(drawn_popup)) => match drawn_popup.control_at(at) {
                        Some(control) => {
                            if act(&store, gates, popup, control, &snapshot) {
                                ui.detail = None;
                            }
                            cadence.force();
                            next_tick = Instant::now();
                        }
                        None if !drawn_popup.area.contains(at) => ui.detail = None,
                        None => {}
                    },
                    _ => {
                        let clicked = drawn
                            .sessions
                            .session_at(at)
                            .or_else(|| drawn.context.session_at(at));
                        if let Some(session) = clicked {
                            ui.legend = false;
                            ui.detail = Some(view::Popup {
                                detail: load_detail(&store, &snapshot, session),
                                burn_armed_until: None,
                                message: None,
                            });
                        }
                    }
                }
                continue;
            }
            let delta = match mouse.kind {
                crossterm::event::MouseEventKind::ScrollDown => 1,
                crossterm::event::MouseEventKind::ScrollUp => -1,
                _ => 0,
            };
            for (list, offset) in [
                (&drawn.sessions, &mut ui.scroll.sessions),
                (&drawn.context, &mut ui.scroll.context),
                (&drawn.rebuilds, &mut ui.scroll.rebuilds),
                (&drawn.spend, &mut ui.scroll.spend),
            ] {
                if delta != 0 && list.area.contains(at) {
                    *offset = offset.saturating_add_signed(delta).min(list.max_offset);
                }
            }
            continue;
        }
        // Everything else that is not a key press (resize, key release)
        // falls through: the next draw re-renders at the new size
        // (ratatui auto-resizes in draw).
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
                // ? toggles the legend over the frame; Esc closes it, or
                // the detail popup. The two are never open together.
                crossterm::event::KeyCode::Char('?') => {
                    ui.legend = !ui.legend;
                    ui.detail = None;
                }
                crossterm::event::KeyCode::Esc => {
                    ui.legend = false;
                    ui.detail = None;
                }
                // The detail popup's controls, by key.
                crossterm::event::KeyCode::Char(key @ ('o' | 'b' | 'x')) if ui.detail.is_some() => {
                    let control = match key {
                        'o' => detail::Control::Over,
                        'b' => detail::Control::Burn,
                        _ => detail::Control::Close,
                    };
                    if let Some(popup) = &mut ui.detail
                        && act(&store, gates, popup, control, &snapshot)
                    {
                        ui.detail = None;
                    }
                    cadence.force();
                    next_tick = Instant::now();
                }
                // r forces an immediate full reload on the next pass —
                // the window and the heavy sections, changed or not.
                crossterm::event::KeyCode::Char('r') => {
                    cadence.force();
                    next_tick = Instant::now();
                }
                _ => {}
            }
        }
    }
    if upgraded && let Some(exe) = exe {
        // The terminal goes back first, so the new binary's own init
        // starts from a clean one and a failed exec leaves the shell
        // usable.
        drop(terminal);
        drop(restore);
        let error = reexec::exec(exe.path());
        return Err(anyhow::Error::new(error).context(format!(
            "tui: re-exec of the upgraded {} failed",
            exe.path().display()
        )));
    }
    Ok(())
}

/// Read one session's detail, with its row from the current snapshot.
fn load_detail(store: &Store, snapshot: &model::Snapshot, session: &str) -> detail::Detail {
    let agg = snapshot.sessions.iter().find(|agg| agg.session == session);
    detail::Detail::load(store, session, agg)
}

/// How long an armed BURN waits for its confirmation.
const BURN_CONFIRM: Duration = Duration::from_secs(5);

/// Act on one of the detail popup's controls, returning whether the
/// popup should close. A control that cannot act says why; BURN arms on
/// its first press and grants on a second inside [`BURN_CONFIRM`]; a
/// grant or a revoke goes through [`crate::release`], the writes the
/// markers make, and its outcome or error lands in the popup's message
/// rather than ending the dashboard. The popup's data is re-read after
/// a write so it shows what the session now holds.
fn act(
    store: &Store,
    gates: &crate::config::GatesConfig,
    popup: &mut view::Popup,
    control: detail::Control,
    snapshot: &model::Snapshot,
) -> bool {
    let now_ms = jiff::Timestamp::now().as_millisecond();
    if control == detail::Control::Dismiss {
        return true;
    }
    if let Some(reason) = detail::availability(&popup.detail, control, gates.quota_enabled, now_ms)
    {
        popup.message = Some(format!("{}: {reason}", control.key()));
        return false;
    }
    if control == detail::Control::Burn && popup.burn_armed_until.is_none() {
        popup.burn_armed_until = Some(Instant::now() + BURN_CONFIRM);
        popup.message = Some("press burn again to release into overage".to_owned());
        return false;
    }
    popup.burn_armed_until = None;
    let session = popup.detail.session.clone();
    popup.message = Some(match control.release() {
        Some(release) => {
            match crate::release::grant_from_tui(store, &session, release, gates, now_ms) {
                Ok(_) => match release {
                    crate::ir::Release::Plan => "released to the end of the plan".to_owned(),
                    crate::ir::Release::Overage => "released into overage".to_owned(),
                },
                Err(error) => format!("release failed: {error}"),
            }
        }
        None => match crate::release::revoke_from_tui(store, &session, gates, now_ms) {
            Ok(_) => "gate closed".to_owned(),
            Err(error) => format!("close failed: {error}"),
        },
    });
    popup.detail = load_detail(store, snapshot, &session);
    false
}

/// What the loop decides each tick (see [`Cadence::due`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Due {
    /// Re-read the display window.
    read: bool,
    /// Re-run the heavy passes (quota, rebuilds, models caches).
    heavy: bool,
}

/// When the loop reads. The ledger's `PRAGMA data_version` (on the
/// store's one connection — the counter is per-connection, and moves
/// only for OTHER connections' commits, which every daemon write is)
/// says whether anything landed since the last tick: the display
/// window is re-read when it moved, or past [`DISPLAY_FLOOR`]; the
/// heavy passes when it moved since they last ran and [`QUOTA_MIN`]
/// has passed, or past [`QUOTA_REFRESH`] regardless. Pure over the
/// instants it is handed, so the schedule is testable without a
/// terminal or a clock.
#[derive(Debug, Default)]
struct Cadence {
    /// The `data_version` the last tick saw; `None` before the first.
    version: Option<i64>,
    /// When the display window was last read.
    read_at: Option<Instant>,
    /// When the heavy passes last completed.
    heavy_at: Option<Instant>,
    /// Data landed since the heavy passes last ran.
    heavy_stale: bool,
}

impl Cadence {
    /// This tick's reads, given the version seen now.
    fn due(&mut self, now: Instant, version: i64) -> Due {
        let moved = self.version != Some(version);
        self.version = Some(version);
        self.heavy_stale |= moved;
        let since = |at: Option<Instant>| at.map(|at| now.saturating_duration_since(at));
        let read = moved || since(self.read_at).is_none_or(|age| age >= DISPLAY_FLOOR);
        let heavy = since(self.heavy_at)
            .is_none_or(|age| age >= QUOTA_REFRESH || (self.heavy_stale && age >= QUOTA_MIN));
        Due { read, heavy }
    }

    /// The display window was read at `at` (its completion).
    fn read_done(&mut self, at: Instant) {
        self.read_at = Some(at);
    }

    /// The heavy passes completed at `at`; they have seen every row up
    /// to the version this tick read.
    fn heavy_done(&mut self, at: Instant) {
        self.heavy_at = Some(at);
        self.heavy_stale = false;
    }

    /// The `r` key: the next tick reads everything.
    fn force(&mut self) {
        self.read_at = None;
        self.heavy_at = None;
    }
}

/// One display read, kept across ticks: the window's rows, the
/// ledger's total and newest timestamp, and the session labels
/// resolved for the rows' sessions. Between reads the loop only
/// re-aggregates it against the advancing clock.
#[derive(Debug)]
struct DisplayWindow {
    /// The window's rows, oldest first (the read's order).
    rows: Vec<crate::store::DisplayRow>,
    /// The ledger's total row count.
    total: i64,
    /// The newest ledger row's timestamp, of any kind.
    latest: Option<i64>,
    /// Labels for the rows' sessions, as of the read.
    labels: HashMap<String, labels::Label>,
}

/// Read the display window's rows and total, and resolve the labels of
/// the sessions in it. The read is the store's narrow display
/// projection ([`Store::display_rows_since`]): the eighteen columns the
/// aggregation consumes, no JSON parse per row — the full-row read
/// this path used to pay cast 59 columns and parsed six JSON values
/// per row (invariant 7). Errors propagate — with WAL and the store's
/// 5 s busy timeout a read failure is real trouble, not a blip worth
/// hiding behind a stale frame.
///
/// Session labels resolve here, once per session per read
/// ([`labels::Labels`]) — never per render or per tick, which is why
/// the label state is loop state passed in. A read follows every
/// ledger change, so a title that regenerates mid-session stays
/// current with the session's own traffic.
fn read_window(
    store: &Store,
    window_mins: u64,
    labels: &mut labels::Labels,
) -> anyhow::Result<DisplayWindow> {
    let now_ms = jiff::Timestamp::now().as_millisecond();
    let rows = store.display_rows_since(window_start(now_ms, window_mins), ROW_CAP)?;
    let total = store.count_requests()?;
    // The header's freshness: the newest row of ANY kind in the whole
    // ledger, not the window's — an empty window must still say how
    // long ago the proxy last wrote anything.
    let latest = store.latest_ts_ms()?;
    labels.start_refresh();
    let mut session_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for session_id in rows.iter().filter_map(|row| row.session_id.as_deref()) {
        session_ids.insert(session_id);
    }
    let mut resolved = HashMap::new();
    for session_id in session_ids {
        if let Some(label) = labels.resolve(session_id) {
            resolved.insert(session_id.to_owned(), label);
        }
    }
    Ok(DisplayWindow {
        rows,
        total,
        latest,
        labels: resolved,
    })
}

/// The display window's oldest instant at `now_ms`.
fn window_start(now_ms: i64, window_mins: u64) -> i64 {
    now_ms.saturating_sub(window_mins.saturating_mul(60_000) as i64)
}

impl DisplayWindow {
    /// Drop the rows that have crossed the window's start since the
    /// read. The ledger is insert-only, so between reads the window
    /// can only lose rows off its old end — and a new row, at any
    /// timestamp, moves `data_version` and brings a fresh read.
    fn age_out(&mut self, since: i64) {
        let aged = self.rows.partition_point(|row| row.ts_ms < since);
        self.rows.drain(..aged);
    }

    /// This tick's snapshot: the window aged to now, aggregated with
    /// the CACHED quota and rebuild sections, the live-allowance set,
    /// and the loaded models catalogues (`catalogs`, the loop's mirror
    /// of the daemon's cache files — see [`ModelCaches`]).
    fn snapshot(
        &mut self,
        window_mins: u64,
        quota: Option<&quota::QuotaAgg>,
        released: &model::Released,
        rebuilds: Option<&rebuilds::RebuildAgg>,
        catalogs: &FetchedCatalogs,
    ) -> model::Snapshot {
        let now_ms = jiff::Timestamp::now().as_millisecond();
        self.age_out(window_start(now_ms, window_mins));
        let mut snapshot = model::aggregate(
            &self.rows,
            quota,
            released,
            &self.labels,
            catalogs,
            rebuilds.cloned(),
            window_mins,
            now_ms,
            self.total,
        );
        snapshot.latest_row_ts_ms = self.latest;
        snapshot
    }
}

/// The TUI's read-only mirror of the daemon's models caches — the
/// ownership decision, made concrete: the daemon
/// ([`crate::server::Server::spawn_catalog_refresh`]) is the ONLY
/// writer (atomic rewrites on its 24 h cycle); this side loads whatever
/// exists and never fetches, never writes. Each source's cache file is
/// re-read only when its mtime moved since the last load — the daemon's
/// atomic replace always moves it, and an untouched file is free. A
/// file that vanishes or stops parsing leaves whatever was last loaded
/// (a stale ceiling beats none); a source never seen simply stays
/// absent — absence, never zeros (invariant 3).
struct ModelCaches {
    /// [`fetched::cache_dir`], or `None` when no data home resolves —
    /// every refresh is then a no-op.
    dir: Option<PathBuf>,
    /// Per source: the mtime the loaded catalogue was read at.
    mtimes: HashMap<String, Option<std::time::SystemTime>>,
    /// The loaded catalogues — what [`DisplayWindow::snapshot`] consults.
    catalogs: FetchedCatalogs,
    /// How many cache files were (re)loaded — the tests' proof the
    /// mtime gate holds.
    loads: usize,
}

impl ModelCaches {
    fn new(dir: PathBuf) -> ModelCaches {
        ModelCaches {
            dir: Some(dir),
            mtimes: HashMap::new(),
            catalogs: FetchedCatalogs::default(),
            loads: 0,
        }
    }

    /// The no-data-home shape: nothing to read, ever.
    fn disabled() -> ModelCaches {
        ModelCaches {
            dir: None,
            mtimes: HashMap::new(),
            catalogs: FetchedCatalogs::default(),
            loads: 0,
        }
    }

    /// Reload any source whose cache file moved since the last load.
    fn refresh(&mut self) {
        let Some(dir) = &self.dir else {
            return;
        };
        for source in fetched::SOURCES {
            let path = fetched::cache_path(dir, source);
            let mtime = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok();
            if mtime == self.mtimes.get(*source).copied().flatten() {
                continue; // unchanged (or still absent) since the last load
            }
            self.mtimes.insert((*source).to_owned(), mtime);
            let Some((fetched_at, response)) = fetched::load_cache(dir, source) else {
                continue; // absent or unreadable: keep whatever was loaded
            };
            let Some(parse) = fetched::parse_for(source) else {
                continue;
            };
            if let Ok(catalog) = parse(&response, fetched_at) {
                self.catalogs.set(source, catalog);
                self.loads += 1;
            }
        }
    }

    /// The loaded catalogues, as the aggregation consumes them.
    fn catalogs(&self) -> &FetchedCatalogs {
        &self.catalogs
    }
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
    fmt: &locale::Fmt,
) -> anyhow::Result<Option<rebuilds::RebuildAgg>> {
    let now_ms = jiff::Timestamp::now().as_millisecond();
    let since = window_start(now_ms, window_mins);
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
    let mut by_id: std::collections::HashMap<_, _> =
        localisation.into_iter().map(|row| (row.id, row)).collect();
    // A predecessor that dropped its ladders: its prompt's rungs, by hash.
    // A failed lookup localises less, never fails the panel.
    rebuilds::fill_baselines(&walk.events, &mut by_id, |session, tools, hash, at| {
        store
            .lane_system_ladders(session, tools, hash, at)
            .ok()
            .flatten()
    });
    rebuilds::localise(&mut walk.events, &by_id, fmt);
    Ok(Some(rebuilds::aggregate(walk)))
}

/// The sessions holding a live allowance for the quota window now
/// running — the `$` marker's set. A release names
/// the window it was for, so it only counts while that window is the
/// current one: a stored `(meter, reset)` allowance is live iff the
/// newest meter reading still reports that reset. Nothing is live
/// without a reading to match against (no meter, no window to be
/// released past), and the gate disarmed means nobody is being
/// released past anything. The allowances table is small and
/// self-expiring by its reset-keyed design, so the read is the whole
/// table on each display read — cheap, and filtered here.
fn released_sessions(
    store: &Store,
    quota: Option<&quota::QuotaAgg>,
) -> anyhow::Result<model::Released> {
    let Some(quota) = quota else {
        return Ok(model::Released::new());
    };
    if !quota.gate_on {
        return Ok(model::Released::new());
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
    let mut released = model::Released::new();
    if resets.is_empty() {
        return Ok(released);
    }
    for allowance in store.load_allowances()? {
        if resets
            .iter()
            .any(|(meter, reset)| allowance.meter == *meter && allowance.reset_value == *reset)
        {
            // A session holding both kinds (one per meter) shows the
            // wider one: overage is what it may spend somewhere.
            let held = released
                .entry(allowance.session_id)
                .or_insert(allowance.release);
            if allowance.release == crate::ir::Release::Overage {
                *held = crate::ir::Release::Overage;
            }
        }
    }
    Ok(released)
}

/// Reload the meter lookback and aggregate the quota section — the
/// heavy read, on its own cadence. A burn rate and a spent span need
/// history a display window cannot hold — "readings are taken
/// from every row read, not just the windowed ones" (the predecessor's
/// rule);
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
/// UTC's (the local clock's midnight). A clock outside
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

/// The local-clock string for the header (to the second, the system zone,
/// in the display locale). Kept out of [view] so rendering stays a pure
/// function of its inputs and the tests stay deterministic.
fn clock(fmt: &locale::Fmt) -> String {
    fmt.clock_sec(&jiff::Zoned::now())
}

/// Restores the terminal (raw mode off, alternate screen left) on drop —
/// the RAII half of teardown, covering `?` returns and panics. The panic
/// hook `try_init` installed covers the other half, restoring before the
/// panic message prints.
///
/// Mouse reporting is not the panic hook's to undo, so on a panic it
/// goes off here, as the guard unwinds; left on, the shell after would
/// read every click and wheel notch as escape-sequence garbage.
struct RestoreGuard;

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), WheelCapture(false));
        let _ = ratatui::try_restore();
    }
}

/// Mouse reporting for the wheel alone: xterm's normal tracking (1000:
/// buttons, and the wheel as buttons 4 and 5) in SGR encoding (1006).
/// Not crossterm's `EnableMouseCapture`, which also turns on any-motion
/// tracking (1003): every pointer move over the dashboard would then be
/// an event and a redraw, for nothing it uses.
struct WheelCapture(bool);

impl crossterm::Command for WheelCapture {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        let set = if self.0 { 'h' } else { 'l' };
        write!(f, "\x1b[?1000{set}\x1b[?1006{set}")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::other("wheel capture is ANSI-only"))
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
    /// `ts_ms` — the parsed rate-limits shape (util/reset per window plus
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
            tools_hash: None,
            model: None,
            provider: None,
            input: None,
            cache_read: None,
            cache_write_5m: None,
            cache_write_1h: None,
            output: None,
            reasoning: None,
            cost_usd: None,
            cost_kind: None,
            serving_provider: None,
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
    /// full rows and read these eighteen fields off them, so aggregating
    /// this projection of a full-row read must equal aggregating the
    /// narrow read.
    pub(crate) fn as_display_rows(rows: &[RequestRow]) -> Vec<DisplayRow> {
        rows.iter()
            .map(|row| DisplayRow {
                ts_ms: row.ts_ms,
                kind: row.kind,
                session_id: row.session_id.clone(),
                tools_hash: row.tools_hash.clone(),
                model: row.model.clone(),
                provider: row.provider.clone(),
                input: row.input,
                cache_read: row.cache_read,
                cache_write_5m: row.cache_write_5m,
                cache_write_1h: row.cache_write_1h,
                output: row.output,
                reasoning: row.reasoning,
                cost_usd: row.cost_usd,
                cost_kind: row.cost_kind,
                serving_provider: row
                    .extra
                    .as_ref()
                    .and_then(|extra| extra.get("serving_provider"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
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
    //! work-shaped data the imported predecessor history is. Read-only,
    //! run
    //! deliberately with `--ignored` (the service is live; the probe
    //! reads exactly the way the TUI's own ticks do and writes
    //! nothing).

    use super::{detail, model, testrows, view};
    use crate::tui::model::Released;
    use std::collections::HashMap;

    use crate::store::{Store, is_api_measurement};

    /// The whole ledger as the probe's window: every imported row
    /// classified, every figure summed. The caps sit above the
    /// ledger's size by construction (checked, not assumed).
    const EVERYTHING: u64 = 1_000_000;

    /// One cache file in the shape the daemon writes: the wrapper
    /// (`fetched_at_ms` + the provider's raw response), pretty JSON
    /// with a trailing newline — written here directly because the
    /// shape is the reader's contract, pinned from the reading side.
    fn write_cache(
        dir: &std::path::Path,
        provider: &str,
        fetched_at_ms: i64,
        response: &serde_json::Value,
    ) {
        let value = serde_json::json!({
            "fetched_at_ms": fetched_at_ms,
            "response": response,
        });
        let mut text = serde_json::to_string_pretty(&value).expect("serialise");
        text.push('\n');
        std::fs::write(crate::catalog::fetched::cache_path(dir, provider), text)
            .expect("write cache");
    }

    /// The openrouter listing shape, two models — the second variant
    /// swaps the window so a reload is visible.
    fn openrouter_response(window: u64) -> serde_json::Value {
        serde_json::json!({
            "data": [
                {"id": "z-ai/glm-5.3", "context_length": window, "pricing": {"prompt": "0"}},
                {"id": "openai/gpt-6-luna", "context_length": 250_000}
            ]
        })
    }

    /// The popup's controls through the loop's own handler: BURN arms
    /// on its first press and grants on its second, OVER grants at once,
    /// Close revokes, and a control that cannot act says why and writes
    /// nothing. Each outcome lands in the popup's message, and its data
    /// is re-read to show what the session now holds.
    #[test]
    fn the_popup_controls_grant_arm_and_revoke() {
        use crate::ir::Release;
        use crate::store::MetersSnapshot;
        let store = Store::open(":memory:").expect("store");
        let now = jiff::Timestamp::now();
        let mut row = testrows::bare(now.as_millisecond() - 60_000);
        row.session_id = Some("ses-act".to_owned());
        row.provider = Some("anthropic_sub".to_owned());
        store.record_request(&row).expect("record");
        store
            .save_meters(
                "anthropic_sub",
                &MetersSnapshot {
                    updated_ms: now.as_millisecond(),
                    snapshot: serde_json::json!({
                        "util5h": 0.5, "reset5h": now.as_second() + 3600,
                        "util7d": 0.2, "reset7d": now.as_second() + 86_400,
                        "overageInUse": false,
                    }),
                },
            )
            .expect("meters");
        let snapshot = model::empty(30);
        let gates = crate::config::GatesConfig {
            quota_enabled: true,
            ..Default::default()
        };
        let mut popup = view::Popup {
            detail: super::load_detail(&store, &snapshot, "ses-act"),
            burn_armed_until: None,
            message: None,
        };
        let held = |store: &Store| -> Vec<Release> {
            store
                .load_session_allowances("ses-act")
                .expect("allowances")
                .into_iter()
                .map(|allowance| allowance.release)
                .collect()
        };

        // Nothing held: Close cannot act, and says so.
        assert!(!super::act(
            &store,
            &gates,
            &mut popup,
            detail::Control::Close,
            &snapshot
        ));
        assert_eq!(popup.message.as_deref(), Some("x: nothing held"));

        // BURN's first press only arms it.
        assert!(!super::act(
            &store,
            &gates,
            &mut popup,
            detail::Control::Burn,
            &snapshot
        ));
        assert!(popup.burn_armed_until.is_some());
        assert!(held(&store).is_empty(), "an armed burn grants nothing");
        // The second grants, and disarms.
        super::act(&store, &gates, &mut popup, detail::Control::Burn, &snapshot);
        assert_eq!(popup.burn_armed_until, None);
        assert_eq!(held(&store), vec![Release::Overage]);
        assert_eq!(popup.message.as_deref(), Some("released into overage"));
        assert_eq!(popup.detail.allowances.len(), 1, "the data was re-read");

        // Close revokes; OVER then grants the plan release on one press.
        super::act(
            &store,
            &gates,
            &mut popup,
            detail::Control::Close,
            &snapshot,
        );
        assert!(held(&store).is_empty());
        assert_eq!(popup.message.as_deref(), Some("gate closed"));
        super::act(&store, &gates, &mut popup, detail::Control::Over, &snapshot);
        assert_eq!(held(&store), vec![Release::Plan]);

        // Dismiss asks the loop to close the popup.
        assert!(super::act(
            &store,
            &gates,
            &mut popup,
            detail::Control::Dismiss,
            &snapshot
        ));

        // With the gate off, nothing acts.
        let off = crate::config::GatesConfig {
            quota_enabled: false,
            ..Default::default()
        };
        super::act(&store, &off, &mut popup, detail::Control::Over, &snapshot);
        assert_eq!(popup.message.as_deref(), Some("o: gate off"));
    }

    /// The header's freshness comes from the whole ledger, not the
    /// window: a row older than the window still dates the proxy's last
    /// write while the window itself reads empty, and an empty ledger
    /// leaves it absent.
    #[test]
    fn the_display_read_dates_freshness_past_the_window() {
        let store = Store::open(":memory:").expect("open in-memory store");
        let catalogs = crate::catalog::fetched::FetchedCatalogs::default();
        let mut labels = super::labels::Labels::new(Vec::new());
        let refresh = |labels: &mut super::labels::Labels| {
            super::read_window(&store, 30, labels)
                .expect("read")
                .snapshot(30, None, &Released::new(), None, &catalogs)
        };

        let empty = refresh(&mut labels);
        assert!(empty.window_empty);
        assert_eq!(empty.latest_row_ts_ms, None, "empty ledger: no freshness");

        // An hour old: well outside the 30-minute window.
        let old = jiff::Timestamp::now().as_millisecond() - 3_600_000;
        store
            .record_request(&super::testrows::bare(old))
            .expect("record");
        let snap = refresh(&mut labels);
        assert!(snap.window_empty, "the row is outside the window");
        assert_eq!(snap.latest_row_ts_ms, Some(old));
    }

    /// The tick's schedule: a quiet ledger reads nothing until the
    /// display floor; a moved `data_version` re-reads the window that
    /// tick, and the heavy passes no sooner than `QUOTA_MIN` after
    /// their last run however often it moves — but at least every
    /// `QUOTA_REFRESH` with no data at all.
    #[test]
    fn the_cadence_reads_on_change_and_spaces_the_heavy_passes() {
        use super::{Cadence, DISPLAY_FLOOR, Due, QUOTA_MIN, QUOTA_REFRESH};
        let t0 = std::time::Instant::now();
        let at = |secs: u64| t0 + std::time::Duration::from_secs(secs);
        let mut cadence = Cadence::default();
        let both = Due {
            read: true,
            heavy: true,
        };
        let idle = Due {
            read: false,
            heavy: false,
        };

        assert_eq!(cadence.due(at(0), 7), both, "the first tick reads all");
        cadence.read_done(at(0));
        cadence.heavy_done(at(0));
        assert_eq!(cadence.due(at(1), 7), idle, "a quiet ledger read");

        // New data each second: the window follows it, the heavy
        // passes wait out their spacing, then catch up once.
        for second in 2..QUOTA_MIN.as_secs() {
            let due = cadence.due(at(second), 7 + second as i64);
            assert!(due.read && !due.heavy, "second {second}: {due:?}");
            cadence.read_done(at(second));
        }
        let due = cadence.due(at(QUOTA_MIN.as_secs()), 100);
        assert_eq!(due, both, "the spacing passed with new data waiting");
        cadence.read_done(at(QUOTA_MIN.as_secs()));
        cadence.heavy_done(at(QUOTA_MIN.as_secs()));

        // Data that landed before a heavy pass does not call another.
        let quiet = QUOTA_MIN.as_secs() * 2 + 1;
        assert_eq!(cadence.due(at(quiet), 100), idle);

        // Nothing new: the floors alone bring the reads back.
        let floor = QUOTA_MIN.as_secs() + DISPLAY_FLOOR.as_secs();
        assert_eq!(
            cadence.due(at(floor), 100),
            Due {
                read: true,
                heavy: false
            }
        );
        cadence.read_done(at(floor));
        let refresh = QUOTA_MIN.as_secs() + QUOTA_REFRESH.as_secs();
        assert!(cadence.due(at(refresh), 100).heavy, "the heavy floor");

        // The r key: everything, now.
        let mut forced = Cadence::default();
        forced.due(at(0), 1);
        forced.read_done(at(0));
        forced.heavy_done(at(0));
        forced.force();
        assert_eq!(forced.due(at(1), 1), both);
    }

    /// Between reads the window ages in memory: rows that cross its
    /// start drop out on the tick that passes them, without a re-read.
    #[test]
    fn the_cached_window_ages_out_rows_past_its_start() {
        let mut window = super::DisplayWindow {
            rows: [100, 200, 200, 300]
                .into_iter()
                .map(super::testrows::display_bare)
                .collect(),
            total: 4,
            latest: Some(300),
            labels: HashMap::new(),
        };
        window.age_out(50);
        assert_eq!(window.rows.len(), 4);
        window.age_out(200);
        let left: Vec<i64> = window.rows.iter().map(|row| row.ts_ms).collect();
        assert_eq!(left, vec![200, 200, 300], "the start is inclusive");
        window.age_out(301);
        assert!(window.rows.is_empty());
        assert_eq!(window.total, 4, "the ledger total is not the window's");
    }

    /// The mtime gate: a cache file is read when it first appears and
    /// again only when its mtime moves — an unchanged file (or an
    /// absent one) costs nothing, and the loaded catalogue survives a
    /// rewrite whose mtime claims nothing changed.
    #[test]
    fn model_caches_reload_only_when_the_mtime_moves() {
        let dir = crate::setup::test_dir("model-caches");
        let mut caches = super::ModelCaches::new(dir.to_path_buf());
        // The stamp the rewritten caches carry: any constant later than
        // the first write's.
        const REWRITTEN_AT: i64 = 1_800_000_000_000;

        // Nothing on disk yet: three absent sources, nothing loaded.
        caches.refresh();
        assert_eq!(caches.loads, 0, "no cache files exist yet");
        assert_eq!(
            caches
                .catalogs()
                .context_window_of("openrouter", "z-ai/glm-5.3"),
            None,
            "an absent source has no ceilings"
        );

        // The daemon's first write: one load, and the ceilings answer.
        write_cache(
            &dir,
            "openrouter",
            1_700_000_000_000,
            &openrouter_response(200_000),
        );
        caches.refresh();
        assert_eq!(caches.loads, 1);
        assert_eq!(
            caches
                .catalogs()
                .context_window_of("openrouter", "z-ai/glm-5.3"),
            Some(200_000)
        );
        assert_eq!(
            caches
                .catalogs()
                .context_window_of("openrouter", "openai/gpt-6-luna"),
            Some(250_000)
        );

        // An unchanged mtime is not re-read — even when the CONTENT
        // moved underneath (the gate is the daemon's own atomic
        // replace moving the mtime, and the pinned clock proves the
        // gate is the mtime, not the bytes).
        let recorded = *caches.mtimes.get("openrouter").expect("mtime recorded");
        std::fs::write(
            crate::catalog::fetched::cache_path(&dir, "openrouter"),
            serde_json::to_string_pretty(&serde_json::json!({
                "fetched_at_ms": REWRITTEN_AT,
                "response": openrouter_response(999_999),
            }))
            .expect("serialise"),
        )
        .expect("rewrite the cache");
        let file = std::fs::File::options()
            .write(true)
            .open(crate::catalog::fetched::cache_path(&dir, "openrouter"))
            .expect("open for set_times");
        file.set_times(std::fs::FileTimes::new().set_modified(recorded.expect("a time")))
            .expect("pin the mtime back");
        drop(file);
        caches.refresh();
        assert_eq!(
            caches.loads, 1,
            "an unchanged mtime is not re-read, whatever the bytes say"
        );
        assert_eq!(
            caches
                .catalogs()
                .context_window_of("openrouter", "z-ai/glm-5.3"),
            Some(200_000),
            "the last good parse stands"
        );

        // The daemon's atomic replace: a moved mtime reloads, and the
        // new listing answers.
        std::fs::write(
            crate::catalog::fetched::cache_path(&dir, "openrouter"),
            serde_json::to_string_pretty(&serde_json::json!({
                "fetched_at_ms": REWRITTEN_AT,
                "response": openrouter_response(400_000),
            }))
            .expect("serialise"),
        )
        .expect("rewrite the cache");
        caches.refresh();
        assert_eq!(caches.loads, 2, "a moved mtime reloads");
        assert_eq!(
            caches
                .catalogs()
                .context_window_of("openrouter", "z-ai/glm-5.3"),
            Some(400_000),
            "the fresh listing answers"
        );

        // A source that appears later is picked up independently, and
        // both anthropic backends read the anthropic listing.
        write_cache(
            &dir,
            "anthropic",
            REWRITTEN_AT,
            &serde_json::json!({
                "data": [{
                    "type": "model",
                    "id": "claude-opus-5",
                    "display_name": "Claude Opus 5",
                    "max_input_tokens": 1_000_000
                }]
            }),
        );
        caches.refresh();
        assert_eq!(caches.loads, 3);
        for provider in ["anthropic_sub", "anthropic_api"] {
            assert_eq!(
                caches
                    .catalogs()
                    .context_window_of(provider, "claude-opus-5"),
                Some(1_000_000),
                "{provider} reads the anthropic listing's max_input_tokens"
            );
        }
        assert_eq!(
            caches
                .catalogs()
                .context_window_of("openrouter", "z-ai/glm-5.3"),
            Some(400_000),
            "the other sources are untouched"
        );
    }

    /// No data home at all: the disabled mirror reads nothing and the
    /// aggregation runs on the hand-verified catalogue alone.
    #[test]
    fn model_caches_without_a_data_home_stay_empty() {
        let mut caches = super::ModelCaches::disabled();
        caches.refresh();
        assert_eq!(caches.loads, 0);
        assert_eq!(caches.dir, None);
        assert_eq!(
            caches
                .catalogs()
                .context_window_of("openrouter", "z-ai/glm-5.3"),
            None
        );
    }

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
        // The caches the way the loop reads them: the daemon's cache
        // files, read-only (nothing is fetched — the probe reads what
        // the TUI would actually consult).
        let mut caches = super::ModelCaches::new(
            crate::catalog::fetched::cache_dir().expect("resolve the data home"),
        );
        caches.refresh();
        let snap = crate::tui::model::aggregate(
            &display,
            None,
            &Released::new(),
            &HashMap::new(),
            caches.catalogs(),
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
        let mut by_id: HashMap<_, _> = localisation.into_iter().map(|row| (row.id, row)).collect();
        crate::tui::rebuilds::fill_baselines(
            &walk.events,
            &mut by_id,
            |session, tools, hash, at| {
                store
                    .lane_system_ladders(session, tools, hash, at)
                    .expect("read a baseline's rungs")
            },
        );
        crate::tui::rebuilds::localise(&mut walk.events, &by_id, &crate::tui::locale::Fmt::fixed());
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
        let system: Vec<_> = rebuilds
            .events
            .iter()
            .filter(|event| event.cause == crate::tui::rebuilds::Cause::SystemPrompt)
            .collect();
        let unknown = system
            .iter()
            .filter(|event| {
                event
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.ends_with("where unknown"))
            })
            .count();
        eprintln!(
            "system prompt changes: {} ({unknown} with no position)",
            system.len()
        );
        for event in system.iter().take(3) {
            eprintln!(
                "  · {} — system prompt changed ({})",
                event.session,
                event.detail.as_deref().unwrap_or("")
            );
        }
    }

    /// The real-transcripts probe: the 5 newest anthropic sessions in
    /// the production ledger, resolved against the REAL `~/.claude`
    /// (and `$CLAUDE_CONFIG_DIR`) on this machine — read-only, the
    /// same tail-only reads the dashboard's own display tick performs.
    /// Prints only what the dashboard itself would show (whether a
    /// label was found, and its cwd/title strings — the user's own
    /// session names, as they render in their own TUI), never
    /// transcript content.
    #[test]
    #[ignore = "reads the REAL ~/.claude transcripts (read-only, the \
                same tail-only read the TUI's display tick performs) \
                plus the live production ledger's newest rows; prints \
                only the labels' own cwd/title strings, never \
                transcript content; run deliberately with --ignored"]
    fn the_newest_production_sessions_resolve_real_transcript_labels() {
        let path = crate::store::default_db_path().expect("resolve the data home");
        assert!(path.exists(), "no production ledger at {path:?}");
        let store = Store::open(&path).expect("open the production ledger");
        let rows = store
            .display_rows_since(0, EVERYTHING)
            .expect("read the display window");

        // The anthropic sessions, newest first: Claude Code names its
        // own sessions in transcripts, and it speaks the anthropic
        // protocol — those are the ids a transcript can exist for.
        let mut latest: HashMap<&str, i64> = HashMap::new();
        for row in &rows {
            let Some(session_id) = row.session_id.as_deref() else {
                continue;
            };
            if !is_api_measurement(row.kind) {
                continue;
            }
            if !row
                .provider
                .as_deref()
                .is_some_and(|provider| provider.starts_with("anthropic"))
            {
                continue;
            }
            latest.insert(session_id, row.ts_ms); // rows arrive ts-ascending
        }
        let mut sessions: Vec<(&str, i64)> = latest.into_iter().collect();
        sessions.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

        let roots = crate::tui::labels::transcript_roots(&[]);
        eprintln!("== the real-transcripts label probe ==");
        eprintln!("roots: {}", {
            let mut text = String::new();
            for root in &roots {
                text.push_str(&format!("\n  {}", root.display()));
            }
            text
        });
        if sessions.is_empty() {
            eprintln!("no anthropic sessions in the ledger");
            return;
        }
        eprintln!(
            "{} anthropic sessions in the ledger; the 5 newest:",
            sessions.len()
        );
        let mut labels = crate::tui::labels::Labels::new(roots);
        for (session_id, latest_ts_ms) in sessions.iter().take(5) {
            let when = jiff::Timestamp::from_millisecond(*latest_ts_ms)
                .map(|ts| ts.strftime("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_else(|_| latest_ts_ms.to_string());
            let found = match labels.resolve(session_id) {
                Some(label) => match (&label.cwd, &label.title) {
                    (Some(cwd), Some(title)) => {
                        format!("found cwd+title — {cwd} · {title}")
                    }
                    (Some(cwd), None) => format!("found cwd only — {cwd}"),
                    (None, Some(title)) => format!("found title only — {title}"),
                    (None, None) => "found prompt only".to_owned(),
                },
                None => "no transcript".to_owned(),
            };
            eprintln!("  {session_id} (latest row {when}): {found}");
        }
    }
}
