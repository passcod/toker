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

use std::collections::{HashMap, HashSet};

use super::labels::Label;
use super::quota::QuotaAgg;
use super::rebuilds::RebuildAgg;
use crate::catalog::windows::{ContextWindow, resolve_context_window};
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
    /// Where the window's input tokens went (the TOKENS panel).
    pub tokens: TokensAgg,
    /// The cache-rebuild section, from the quota-cadence lane walk over
    /// the 24 h tail — `None` until that pass has run, never a
    /// zero-filled stand-in (the panel says so instead).
    pub rebuilds: Option<RebuildAgg>,
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
///
/// One deliberate deviation from live.mjs, scoped and priced: ctp picks
/// each session's MAIN lane (the largest by prompt size, cold.mjs's
/// `mainLane`) and reads `ctx`/`msgs`/`cmpct`/prompt off its latest row;
/// this aggregation is session-scoped, reading the latest row of the
/// session. Lane-picking needs `tools_hash` plus the write share on the
/// narrow read — a text column on the 2-second tick — and a subagent's
/// turn landing after the main lane's would move every "latest" to it,
/// where ctp's would stay on the main conversation. On the single-lane
/// sessions that dominate real traffic the two read identically; on
/// multi-lane ones the ctp behaviour lands with `toker report`, whose
/// full-row reads can afford the lane walk.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionAgg {
    /// The session id, or [`NO_SESSION`] for the NULL-session group.
    pub session: String,
    /// Measurement rows in this session.
    pub requests: usize,
    /// The latest non-NULL model; `None` when no row reported one.
    pub model: Option<String>,
    /// The latest row's prompt — `input + cache_read + cache writes`,
    /// the measure ctp's "prompt now" and the CONTEXT bars share
    /// (live.mjs:170-175's `PROMPT_FIELDS`). `None` unless every
    /// operand is known: anthropic's `input_tokens` excludes cache
    /// writes, so a row without its write share does not understate
    /// the context — it refuses to guess it (see [`prompt_of`]).
    pub input_now: Option<i64>,
    /// The high-water mark of [`SessionAgg::input_now`] over the rows
    /// where the whole prompt is known; `None` when none are.
    pub input_peak: Option<i64>,
    /// Sum of reported output tokens; `None` when no row reported output.
    pub output_total: Option<i64>,
    /// The latest row's message count — the `msgs` column
    /// (live.mjs:388's `last.reqMessages`); `None` when no row carried
    /// one (`?`, never zero).
    pub req_messages: Option<i64>,
    /// The latest row's compaction generation — the `cmpct` column.
    /// live.mjs:356 reads the lane's LATEST marker, not a count over
    /// the window, and renders zero/absent as `-`; `None` and zero
    /// stay distinct here and render the same.
    pub compact_generations: Option<i64>,
    /// The latest row was served on a rewritten (newer) model — the
    /// bright `↑` (live.mjs:367).
    pub forced_latest: bool,
    /// Some row in the window was — the dim `↑` when not the latest.
    pub forced_any: bool,
    /// The session holds a live allowance for the quota window now
    /// running — the `$` marker (live.mjs:287-296): released past the
    /// armed gate, spending overage where the others stop. Decided by
    /// the caller from the allowances table against the current meter
    /// resets, and passed in as a set.
    pub released: bool,
    /// The context ceiling of the latest model, from the hand-verified
    /// catalogue ([`resolve_context_window`] of the model, as of the
    /// latest row) — `Unknown` renders as `?`, never a guess. The
    /// catalogue carries the claude native-1M/fixed-200k identities and
    /// the gpt-5.6-sol/luna 872k declarations, so this is a pure lookup
    /// with no extra read.
    pub ctx: ContextWindow,
    /// The session's name from Claude Code's own transcript (see
    /// [super::labels]): the working directory and title the tail
    /// carries, newest-wins, read-only at view time and never stored
    /// (invariant 1). `None` when no transcript carries one — the view
    /// falls back to the session id, never an empty cell.
    pub label: Option<Label>,
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

/// One TOKENS-panel bucket (live.mjs's `sumUsage`, 445-447): a sum over
/// the rows that reported the metric, plus how many did not. A sum of
/// zero over rows that all reported is a real zero; the same sum with
/// `unavailable > 0` is a floor, and the panel renders it with the `≥`
/// that says so — never as a complete figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct BucketAgg {
    /// Sum of the reported values.
    pub value: i64,
    /// Measurement rows that did not report the metric.
    pub unavailable: usize,
}

impl BucketAgg {
    /// Add one row's reading: reported values sum, absent ones count.
    fn add(&mut self, value: Option<i64>) {
        match value {
            Some(value) => self.value += value,
            None => self.unavailable += 1,
        }
    }
}

/// Where the window's input tokens went (live.mjs's TOKENS panel,
/// 436-504): the four input buckets, output, and the cache-metric
/// counters the hit-rate lines need. `requests` is the window's
/// measurement-row count — the per-request averages' denominator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TokensAgg {
    /// Fresh input: the new turn's content, never a cache hit.
    pub fresh_input: BucketAgg,
    /// Cache read: the reusable prefix, served from cache.
    pub cache_read: BucketAgg,
    /// Cache write, 1-hour TTL — the tier Claude Code uses.
    pub write_1h: BucketAgg,
    /// Cache write, 5-minute TTL.
    pub write_5m: BucketAgg,
    /// Output tokens (no shared denominator with the input buckets —
    /// a bar against them would be meaningless).
    pub output: BucketAgg,
    /// The window's measurement rows — the averages' denominator.
    pub requests: usize,
    /// Rows missing any cache metric (read, either write tier):
    /// hit-rate lines cannot be computed over them (live.mjs:483-489).
    pub cache_unknown: usize,
    /// Rows whose cache read was not above zero — "reused nothing"
    /// (live.mjs:493's `!(r.cacheRead > 0)`).
    pub cold: usize,
}

impl TokensAgg {
    /// Total rewritten tokens: the two write tiers' known sums.
    pub(crate) fn written(&self) -> i64 {
        self.write_1h.value + self.write_5m.value
    }

    /// The input buckets' total — the shares' denominator. Sums the
    /// known values only; the panel renders shares only when nothing
    /// is missing, so this is never a guessed denominator.
    pub(crate) fn input_total(&self) -> i64 {
        self.fresh_input.value + self.cache_read.value + self.write_1h.value + self.write_5m.value
    }

    /// Whether any input bucket has unavailable rows — the panel then
    /// replaces every share with the reason it cannot be computed
    /// (live.mjs:449-459: "N req unknown" / "share unknown").
    pub(crate) fn input_incomplete(&self) -> bool {
        self.fresh_input.unavailable > 0
            || self.cache_read.unavailable > 0
            || self.write_1h.unavailable > 0
            || self.write_5m.unavailable > 0
    }

    /// The hit rate over the REUSABLE prefix — hits plus rewrites, not
    /// all input: fresh input is the new turn's content, which was
    /// never going to be a hit, so including it would drag the rate
    /// down permanently and make an improving cache look static
    /// (live.mjs:478-481).
    ///
    /// Three states, all explicit: [`HitRate::Unknown`] when rows are
    /// missing cache metrics (rendered `?`, never a rate);
    /// [`HitRate::NothingReusable`] when nothing was read or rewritten
    /// (live.mjs renders no line at all — no denominator, no claim);
    /// [`HitRate::Rate`] otherwise.
    pub(crate) fn hit_rate(&self) -> HitRate {
        if self.cache_unknown > 0 {
            return HitRate::Unknown;
        }
        let reusable = self.cache_read.value + self.written();
        if reusable == 0 {
            return HitRate::NothingReusable;
        }
        HitRate::Rate(self.cache_read.value as f64 / reusable as f64)
    }
}

/// The hit rate's three explicit states (see [`TokensAgg::hit_rate`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum HitRate {
    /// Some row is missing a cache metric — `?`, never a rate.
    Unknown,
    /// Nothing was read or rewritten — no line, no claim.
    NothingReusable,
    /// `cache read / (cache read + written)`.
    Rate(f64),
}

/// The pre-refresh placeholder: an empty window of the right shape.
pub(crate) fn empty(window_mins: u64) -> Snapshot {
    aggregate(
        &[],
        None,
        &HashSet::new(),
        &HashMap::new(),
        None,
        window_mins,
        0,
        0,
    )
}

/// Aggregate one window of display rows (the narrow projection the
/// display tick reads) into a [`Snapshot`]. `now_ms` is
/// the frame's reference time (bucket edges anchor to it); `total_requests` is
/// the ledger's total row count, kept distinct from the window so the
/// header can show both. `quota` is the PRECOMPUTED quota section (the
/// meter lookback aggregation, [`super::quota::aggregate`] over the
/// 7-day read), `rebuilds` the precomputed cache-rebuild section (the
/// lane walk over the 24 h tail, [`super::rebuilds`]), `released`
/// the sessions holding a live allowance for the window now running
/// (read from the allowances table against the quota section's
/// current resets, live.mjs:287-296), and `labels` the
/// transcript-derived session labels resolved by the display tick
/// (one read per session per refresh, [`super::labels::Labels`]) —
/// all built on their own slower cadences or the tick itself, passed
/// in to keep this function pure over cheap inputs. Eight positional
/// parameters is the honest shape of a frame's inputs: the rows plus
/// the precomputed/resolved sections and the window anchors. Rows may
/// arrive in any order — "latest" is decided by `ts_ms` throughout.
#[allow(clippy::too_many_arguments)]
pub(crate) fn aggregate(
    rows: &[DisplayRow],
    quota: Option<&QuotaAgg>,
    released: &HashSet<String>,
    labels: &HashMap<String, Label>,
    rebuilds: Option<RebuildAgg>,
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
    let mut tokens = TokensAgg {
        requests: 0,
        ..TokensAgg::default()
    };
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

        // TOKENS: every measurement row reports into the buckets it
        // carries and counts against the ones it does not.
        tokens.requests += 1;
        tokens.fresh_input.add(row.input);
        tokens.cache_read.add(row.cache_read);
        tokens.write_1h.add(row.cache_write_1h);
        tokens.write_5m.add(row.cache_write_5m);
        tokens.output.add(row.output);
        if row.cache_read.is_none() || row.cache_write_1h.is_none() || row.cache_write_5m.is_none()
        {
            tokens.cache_unknown += 1;
        }
        if !row.cache_read.is_some_and(|read| read > 0) {
            tokens.cold += 1;
        }

        // Sessions: group by id (NULL under the dash), accumulate per row
        // in ts order so the last write is the latest row's value.
        let key = row
            .session_id
            .clone()
            .unwrap_or_else(|| NO_SESSION.to_owned());
        let slot = *index.entry(key.clone()).or_insert_with(|| {
            sessions.push(SessionAgg {
                session: key.clone(),
                requests: 0,
                model: None,
                input_now: None,
                input_peak: None,
                output_total: None,
                req_messages: None,
                compact_generations: None,
                forced_latest: false,
                forced_any: false,
                released: released.contains(&key),
                ctx: ContextWindow::Unknown,
                label: labels.get(&key).cloned(),
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
        // The prompt needs every operand; a missing one is unknown.
        session.input_now = prompt_of(row);
        if let Some(now) = session.input_now
            && session.input_peak.is_none_or(|peak| now > peak)
        {
            session.input_peak = Some(now);
        }
        if let Some(output) = row.output {
            session.output_total = Some(session.output_total.unwrap_or(0) + output);
        }
        // The latest row decides `msgs`, `cmpct`, and the bright `↑`;
        // any row's rewrite lights the dim one (live.mjs:356-367).
        session.req_messages = row.req_messages;
        session.compact_generations = row.compact_generations;
        session.forced_latest = row.forced_to.is_some();
        session.forced_any |= row.forced_to.is_some();

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

    // The context ceiling is a pure catalogue lookup off each session's
    // latest model, resolved as of the latest row (live.mjs's `ctxOf`
    // passes `at: r.ts`, so a phased capability resolves to the phase
    // that applied, never to today's against historical rows).
    for session in &mut sessions {
        session.ctx = session_ctx(session.model.as_deref(), session.latest_ts_ms);
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
        tokens,
        // Precomputed by the caller on the QUOTA cadence — the lane
        // walk's 24 h read is far too heavy for the display tick.
        rebuilds,
        errors,
        drift,
        // Precomputed by the caller on the QUOTA cadence — the meter
        // lookback's 20k-row read and its aggregation are far too heavy
        // for the display tick (the spin this split fixed).
        quota: quota.cloned(),
    }
}

/// One row's prompt size — the measure the sessions panel's prompt
/// columns and the CONTEXT bars share: `input + cache_read + cache
/// writes` (live.mjs:167-175's `PROMPT_FIELDS`). `None` unless every
/// operand is known — a missing operand is unknown, not zero.
///
/// The write share is protocol arithmetic, and the row's provider
/// carries the protocol: anthropic's `input_tokens` EXCLUDES both
/// cache buckets, so a row from an anthropic backend that reports no
/// write has an unknown prompt — a write may have gone unreported.
/// The openai-chat family has no cache-write metric at all
/// (`input + cache_read` already is the whole `prompt_tokens`), so a
/// NULL write there is structural, and the prompt is complete without
/// it. Anything not identifiable as an anthropic backend gets the
/// openai-chat arithmetic; a future anthropic-shaped backend must
/// carry the `anthropic` prefix to keep its prompts honest here.
fn prompt_of(row: &DisplayRow) -> Option<i64> {
    let input = row.input?;
    let cache_read = row.cache_read?;
    let writes = match (row.cache_write_5m, row.cache_write_1h) {
        (Some(five), Some(one)) => five + one,
        (None, None) if anthropic_shaped(row.provider.as_deref()) => return None,
        (None, None) => 0,
        _ => return None,
    };
    Some(input + cache_read + writes)
}

/// Whether a provider speaks the anthropic usage shape, whose
/// `input_tokens` excludes the cache buckets (see [`prompt_of`]).
fn anthropic_shaped(provider: Option<&str>) -> bool {
    provider.is_some_and(|provider| provider.starts_with("anthropic"))
}

/// The session's context ceiling: the catalogue lookup of its latest
/// model, resolved as of the latest row (live.mjs:177-179's `ctxOf`
/// with `at: r.ts`). No betas and no learned declaration: the
/// hand-verified catalogue carries the exact identities that matter
/// (claude native-1M/fixed-200k, the gpt-5.6-sol/luna 872k
/// declarations), a beta-selectable phase without captured betas stays
/// `Unknown` exactly as ctp leaves it, and a model outside the
/// catalogue renders `?` rather than inheriting a family's ceiling.
fn session_ctx(model: Option<&str>, latest_ts_ms: i64) -> ContextWindow {
    let Some(model) = model else {
        return ContextWindow::Unknown;
    };
    let at = jiff::Timestamp::from_millisecond(latest_ts_ms)
        .ok()
        .map(|ts| ts.strftime("%Y-%m-%d").to_string());
    resolve_context_window(model, None, None, at.as_deref())
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

    /// The tests' shared "no precomputed sections" inputs: no quota
    /// section, no rebuild section, no released sessions — absence, not
    /// zeros, exactly what the loop passes before the quota cadence's
    /// first pass.
    fn no_sections() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    /// The tests' shared "no transcript labels" input — the absent
    /// state, never a fabricated one.
    fn no_labels() -> std::collections::HashMap<String, super::Label> {
        std::collections::HashMap::new()
    }

    fn agg(rows: &[DisplayRow], total: i64) -> Snapshot {
        // The tests' display rows carry no meter snapshots (the narrow
        // display shape has none to carry), so the loop's quota
        // section over this window is None — absence, not zeros. The
        // precomputed-section wiring has its own test below.
        super::aggregate(
            rows,
            None,
            &no_sections(),
            &no_labels(),
            None,
            WINDOW,
            NOW,
            total,
        )
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
        let snap = super::aggregate(
            &[display_bare(NOW)],
            None,
            &no_sections(),
            &no_labels(),
            None,
            0,
            NOW,
            1,
        );
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
            &no_sections(),
            &no_labels(),
            None,
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
            &no_sections(),
            &no_labels(),
            None,
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
                row.req_messages = Some(3);
                row.compact_generations = Some(1);
                row // id 6 — same ts as the next row
            },
            {
                let mut row = bare(mins_ago(10));
                row.session_id = Some("ses-c".to_owned());
                row.model = Some("m-second".to_owned());
                row.req_messages = Some(4);
                row.compact_generations = Some(1);
                row.forced_to = Some("z-ai/glm-5.3".to_owned());
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
                row.cache_write_5m = Some(100);
                row.cache_write_1h = Some(200);
                row.output = Some(300);
                row.req_messages = Some(31);
                row.compact_generations = Some(2);
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
                row.cache_write_5m = Some(0);
                row.cache_write_1h = Some(400);
                row.output = Some(400);
                row.req_messages = Some(67);
                row.compact_generations = Some(1);
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
                row.req_messages = Some(5);
                row.compact_generations = Some(0);
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
        let via_old = super::aggregate(
            &as_display_rows(&full),
            quota.as_ref(),
            &no_sections(),
            &no_labels(),
            None,
            WINDOW,
            NOW,
            total,
        );

        // NEW path: the narrow read the display cadence now uses —
        // ten columns, no JSON parse.
        let narrow = store.display_rows_since(0, 10_000).expect("narrow read");
        assert_eq!(
            narrow.len(),
            full.len(),
            "no filter: the display read keeps every row kind"
        );
        let via_new = super::aggregate(
            &narrow,
            quota.as_ref(),
            &no_sections(),
            &no_labels(),
            None,
            WINDOW,
            NOW,
            total,
        );

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
        // The phase-5 columns survived the narrow read too: the
        // latest row decides `msgs`/`cmpct`/the bright `↑` (id 7 wins
        // the equal-ts pair for the model; id 15 is later still, so
        // ses-c's `msgs` is ITS value and the `↑` goes dim — an
        // earlier rewrite, not the latest turn), the prompt sums the
        // anthropic write share (id 9: 5 000 + 2 000 + 100 + 200),
        // and the context ceiling is the catalogue's native-1M for
        // claude-opus-5.
        assert_eq!(snap.sessions[3].req_messages, Some(5));
        assert_eq!(snap.sessions[3].compact_generations, Some(0));
        assert!(!snap.sessions[3].forced_latest);
        assert!(snap.sessions[3].forced_any);
        assert_eq!(snap.sessions[2].input_now, Some(7_300));
        assert_eq!(snap.sessions[2].req_messages, Some(31));
        assert_eq!(snap.sessions[2].compact_generations, Some(2));
        assert_eq!(snap.sessions[0].input_now, Some(9_400));
        assert_eq!(
            snap.sessions[0].ctx,
            crate::catalog::windows::ContextWindow::Exact { tokens: 1_000_000 }
        );
        assert_eq!(
            snap.sessions[3].ctx,
            crate::catalog::windows::ContextWindow::Unknown
        );
        // Where the TOKENS panel's numbers come from: every
        // measurement row's buckets, absence counted not zeroed.
        let tokens = &snap.tokens;
        assert_eq!(tokens.requests, 14);
        assert_eq!(tokens.fresh_input.value, 39_060);
        assert_eq!(tokens.cache_read.value, 17_460);
        assert_eq!(tokens.write_1h.value, 600);
        assert_eq!(tokens.write_5m.value, 100);
        assert_eq!(tokens.fresh_input.unavailable, 3);
        assert_eq!(tokens.cache_read.unavailable, 4);
        assert_eq!(
            tokens.cache_unknown, 12,
            "only the two anthropic rows carry every cache metric"
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
            &no_sections(),
            &no_labels(),
            None,
            WINDOW,
            NOW,
            total,
        );
        let via_new = super::aggregate(
            &store.display_rows_since(NOW, 10).expect("narrow read"),
            None,
            &no_sections(),
            &no_labels(),
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
            let snap = super::aggregate(
                &as_display_rows(&full),
                None,
                &no_sections(),
                &no_labels(),
                None,
                30,
                now_ms,
                total,
            );
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
            let snap = super::aggregate(
                &narrow,
                None,
                &no_sections(),
                &no_labels(),
                None,
                30,
                now_ms,
                total,
            );
            new_refresh += t.elapsed();
            std::hint::black_box(&snap);
        }

        // The timed path must have been doing real work.
        let snap = super::aggregate(
            &store
                .display_rows_since(since_ms, CAP)
                .expect("narrow read"),
            None,
            &no_sections(),
            &no_labels(),
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

    // ── the phase-5 panels' aggregation rules ─────────────────────

    /// The prompt's write share is protocol arithmetic: an anthropic
    /// backend's `input_tokens` excludes the cache buckets, so a row
    /// without its write share refuses to guess the prompt; an
    /// openai-chat backend has no cache-write metric at all, so the
    /// NULL there is structural and `input + cache_read` is already
    /// the whole prompt.
    #[test]
    fn the_prompt_sums_writes_only_where_the_protocol_has_them() {
        // anthropic: writes known → summed in; unknown → the prompt is
        // unknown, never understated.
        let mut writes = display_bare(mins_ago(3));
        writes.session_id = Some("ses-anthropic".into());
        writes.provider = Some("anthropic_sub".into());
        writes.input = Some(5_000);
        writes.cache_read = Some(2_000);
        writes.cache_write_5m = Some(100);
        writes.cache_write_1h = Some(200);
        let mut no_writes = display_bare(mins_ago(2));
        no_writes.session_id = Some("ses-anthropic".into());
        no_writes.provider = Some("anthropic_sub".into());
        no_writes.input = Some(5_000);
        no_writes.cache_read = Some(2_000);
        // openai-chat: NULL writes are the metric not existing.
        let mut openai = display_bare(mins_ago(1));
        openai.session_id = Some("ses-openai".into());
        openai.provider = Some("openrouter".into());
        openai.input = Some(1_000);
        openai.cache_read = Some(3_000);

        let snap = agg(&[writes, no_writes, openai], 3);
        let by_session = |name: &str| {
            snap.sessions
                .iter()
                .find(|s| s.session == name)
                .unwrap_or_else(|| panic!("no session {name}"))
        };
        assert_eq!(by_session("ses-anthropic").input_peak, Some(7_300));
        assert_eq!(
            by_session("ses-anthropic").input_now,
            None,
            "the latest row's unreported write makes the prompt unknown"
        );
        assert_eq!(
            by_session("ses-openai").input_now,
            Some(4_000),
            "input + cache_read is the whole openai-chat prompt"
        );
    }

    /// The context ceiling is a catalogue lookup of the latest model —
    /// exact, declared, or explicitly unknown — resolved as of the
    /// latest row, and never a family guess.
    #[test]
    fn ctx_ceilings_resolve_exact_declared_and_unknown() {
        use crate::catalog::windows::ContextWindow;
        let mut opus = display_bare(mins_ago(4));
        opus.session_id = Some("ses-opus".into());
        opus.model = Some("claude-opus-5".into());
        let mut sol = display_bare(mins_ago(3));
        sol.session_id = Some("ses-sol".into());
        sol.model = Some("gpt-5.6-sol".into());
        let mut unknown = display_bare(mins_ago(2));
        unknown.session_id = Some("ses-unknown".into());
        unknown.model = Some("gpt-5.6-terra".into());
        let mut modelless = display_bare(mins_ago(1));
        modelless.session_id = Some("ses-modelless".into());

        let snap = agg(&[opus, sol, unknown, modelless], 4);
        let ctx_of = |name: &str| {
            snap.sessions
                .iter()
                .find(|s| s.session == name)
                .unwrap_or_else(|| panic!("no session {name}"))
                .ctx
        };
        assert_eq!(
            ctx_of("ses-opus"),
            ContextWindow::Exact { tokens: 1_000_000 }
        );
        assert_eq!(
            ctx_of("ses-sol"),
            ContextWindow::Declared { tokens: 872_000 }
        );
        assert_eq!(ctx_of("ses-unknown"), ContextWindow::Unknown);
        assert_eq!(ctx_of("ses-modelless"), ContextWindow::Unknown);
    }

    /// The `↑` marks rewrites (bright on the latest row, dim when only
    /// an earlier one) and the `$` marks a released session — both
    /// from inputs the loop owns, passed in as data.
    #[test]
    fn markers_follow_the_latest_row_and_the_allowance_set() {
        let mut latest = display_bare(mins_ago(2));
        latest.session_id = Some("ses-live".into());
        latest.forced_to = Some("z-ai/glm-5.3".into());
        let mut earlier_only = display_bare(mins_ago(5));
        earlier_only.session_id = Some("ses-dim".into());
        earlier_only.forced_to = Some("z-ai/glm-5.3".into());
        let mut after = display_bare(mins_ago(1));
        after.session_id = Some("ses-dim".into());
        let mut released_row = display_bare(mins_ago(3));
        released_row.session_id = Some("ses-free".into());

        let mut released = std::collections::HashSet::new();
        released.insert("ses-free".to_owned());
        let snap = super::aggregate(
            &[latest, earlier_only, after, released_row],
            None,
            &released,
            &no_labels(),
            None,
            WINDOW,
            NOW,
            4,
        );
        let session = |name: &str| {
            snap.sessions
                .iter()
                .find(|s| s.session == name)
                .unwrap_or_else(|| panic!("no session {name}"))
        };
        assert!(session("ses-live").forced_latest && session("ses-live").forced_any);
        assert!(
            !session("ses-dim").forced_latest && session("ses-dim").forced_any,
            "an earlier rewrite, not the latest turn"
        );
        assert!(session("ses-free").released);
        assert!(!session("ses-live").released);
    }

    /// Session labels arrive as data, keyed by session id exactly like
    /// the released set: a label names its session, an id with no label
    /// stays label-less (absence, never a fabricated name), and the
    /// NULL-session group never gets one — it is not a session, and the
    /// id the view would fall back to is the dash.
    #[test]
    fn labels_attach_by_session_id_and_absence_stays_absent() {
        let mut rows = Vec::new();
        for at in [3, 2] {
            let mut row = display_bare(mins_ago(at));
            row.session_id = Some("ses-named".into());
            row.model = Some("claude-opus-5".into());
            rows.push(row);
        }
        let mut bare = display_bare(mins_ago(1));
        bare.session_id = Some("ses-plain".into());
        rows.push(bare);
        rows.push(display_bare(mins_ago(4))); // the NULL-session group

        let mut labels = std::collections::HashMap::new();
        labels.insert(
            "ses-named".to_owned(),
            super::Label {
                cwd: Some("/home/u/code/toker".into()),
                title: Some("TUI session labels".into()),
                prompt: None,
            },
        );
        let snap = super::aggregate(&rows, None, &no_sections(), &labels, None, WINDOW, NOW, 4);
        let session = |name: &str| {
            snap.sessions
                .iter()
                .find(|s| s.session == name)
                .unwrap_or_else(|| panic!("no session {name}"))
        };
        assert_eq!(
            session("ses-named").label,
            Some(super::Label {
                cwd: Some("/home/u/code/toker".into()),
                title: Some("TUI session labels".into()),
                prompt: None,
            })
        );
        assert_eq!(session("ses-plain").label, None);
        assert_eq!(
            session(NO_SESSION).label,
            None,
            "the NULL-session group never carries a name"
        );
    }

    /// The TOKENS buckets and their three hit-rate states: a real rate
    /// over the reusable prefix, `?` when cache metrics are missing,
    /// and no line at all when nothing was reused.
    #[test]
    fn tokens_buckets_sums_unknowns_and_the_three_hit_rate_states() {
        use super::HitRate;
        let mut hit = display_bare(mins_ago(2));
        hit.session_id = Some("ses-a".into());
        hit.input = Some(1_000);
        hit.cache_read = Some(9_000);
        hit.cache_write_1h = Some(1_000);
        hit.cache_write_5m = Some(0);
        hit.output = Some(500);

        let snap = agg(&[hit], 1);
        let tokens = &snap.tokens;
        assert_eq!(tokens.fresh_input.value, 1_000);
        assert_eq!(tokens.cache_read.value, 9_000);
        assert_eq!(tokens.write_1h.value, 1_000);
        assert_eq!(tokens.write_5m.value, 0);
        assert_eq!(tokens.output.value, 500);
        assert_eq!(tokens.requests, 1);
        assert_eq!(tokens.cache_unknown, 0);
        assert_eq!(tokens.cold, 0, "the one row reused its prefix");
        assert!(!tokens.input_incomplete());
        assert_eq!(tokens.input_total(), 11_000);
        assert_eq!(tokens.written(), 1_000);
        match tokens.hit_rate() {
            HitRate::Rate(rate) => assert!((rate - 0.9).abs() < 1e-12),
            other => panic!("expected a rate, got {other:?}"),
        }

        // The openai shape: no cache-write metrics on any row — the
        // buckets are floors and the rate is a `?`, never a guess.
        let mut openai = display_bare(mins_ago(1));
        openai.session_id = Some("ses-b".into());
        openai.provider = Some("openrouter".into());
        openai.input = Some(1_000);
        openai.cache_read = Some(3_000);
        openai.output = Some(100);
        let snap = agg(&[openai], 1);
        let tokens = &snap.tokens;
        assert_eq!(tokens.cache_unknown, 1);
        assert!(tokens.input_incomplete());
        assert_eq!(tokens.write_1h.unavailable, 1);
        assert_eq!(tokens.hit_rate(), HitRate::Unknown);

        // Nothing read, nothing written, everything reported: no rate
        // is claimable either way — live.mjs renders no line.
        let mut cold = display_bare(mins_ago(1));
        cold.session_id = Some("ses-c".into());
        cold.input = Some(1_000);
        cold.cache_read = Some(0);
        cold.cache_write_1h = Some(0);
        cold.cache_write_5m = Some(0);
        let snap = agg(&[cold], 1);
        assert_eq!(snap.tokens.cold, 1);
        assert_eq!(snap.tokens.hit_rate(), HitRate::NothingReusable);
    }
}
