//! The cold-cache gate and the compaction retarget (plan: Middleware —
//! "Cold gate" + "Compaction retarget"), the second gate the predecessor
//! grew and the only transform that changes model-visible prompt structure.
//!
//! A faithful port of the predecessor proxy's cold gate plus the two
//! pieces its quota outlook calls (the burn-rate ladder and the
//! weight fit), measured over weeks of production traffic —
//! ported, not improved. Pieces:
//!
//! - the cold bar, TTL tier, coldness, and lane-cold verdict
//!   ([`DEFAULT_MIN_TOKENS`], [`ttl_of`], [`coldness`], [`lane_is_cold`]);
//! - the gate decision ([`decide_cold`] — the once-per-idle-spell rule,
//!   the summarising refusal, the outlook suppression);
//! - the quota outlook ([`quota_outlook`] / [`outlook_target`]);
//! - the notice rendering ([`human_idle`], [`ColdBlocking`],
//!   [`outlook_line`]);
//! - the compaction retarget ([`retarget_compaction`] — model swap,
//!   cache_control strip, system merge, the all-or-nothing rule);
//! - `isCompaction` — already ported ([`crate::ir::AnthropicShape::is_compaction`]);
//! - the burn ladder ([`burn_rate`] / [`project_to`]) — the two pieces
//!   the outlook calls (the span/total verdicts are the TUI's, not
//!   ported here);
//! - the weight fit ([`fit_quota_model`] / [`quota_for`]) — what the
//!   outlook prices the re-read with, each weight with its leave-one-out
//!   spread (the rest of the predecessor's diagnostics surface — worst
//!   window, unattributed bound — is not ported);
//! - the pipeline sequencing (quota gate first; the cold notice exempting
//!   summarising requests; the retarget's lane-cold licence) —
//!   see [`crate::server::anthropic`].
//!
//! **Two clocks, two questions.** The notice asks "should the user be
//! interrupted" ([`decide_cold`]: refuses once it has spoken this idle
//! spell, refuses a summarising request outright). The retarget asks "is
//! the cache gone" ([`lane_is_cold`]: no once-per-spell rule, no
//! summarising exemption). The predecessor conflated them once and the
//! compaction the
//! notice had promised ran on the wrong model a keystroke later — the
//! separation is load-bearing and is ported as two functions.
//!
//! Everything here is pure except the store-backed conveniences
//! ([`outlook_over`], [`note_lane_notice`]); state lives in the store, and
//! every function only decides. The one deliberate impurity in the
//! predecessor — the
//! notice names a wall-clock time — is an *input* here like the quota
//! gate's port (invariant 4): the caller passes the timestamp and the
//! [`jiff::tz::TimeZone`].
//!
//! Units: `now` is **epoch milliseconds** (a `Date.now()`-style convention,
//! kept so the vendored contract fixture's `now` values dispatch
//! unchanged); meter resets are **epoch seconds** (the wire form).
//!
//! Absence ≠ zero (invariant 3): a lane with no recorded prompt is not a
//! zero-prompt lane, an unrecorded TTL tier reads as the LONG one (guessing
//! short would fire on lanes whose cache is still live — a false alarm
//! costs the user a turn; guessing long only ever delays a true one), and
//! a suppressed notice is *logged* (`cold-quiet`) so silence is
//! distinguishable from breakage.

use std::collections::{BTreeMap, BTreeSet};

use jiff::tz::TimeZone;
use serde_json::Value;

use super::notice::{NoticeLevel, NoticeStyle, TOKER_LINK, render};
use super::quota::{Meter, THRESHOLD, group};
use crate::catalog::pricing::{normalise_model_id, price};
use crate::catalog::windows::model_identity;
use crate::ir::Request;
use crate::middleware::model_map::{ModelMap, preview_mapped_model};
use crate::store::{Lane, RequestRow, Store};

/// Below this the rebuild is too cheap for the interruption to be worth it.
/// Chosen against the log rather
/// than picked round: of 15 notices fired over a fortnight, a 175k bar
/// keeps 10 and drops the small ones, sitting just under the cluster of
/// real cold rebuilds at 199,416 / 200,621 / 200,956 tokens.
pub const DEFAULT_MIN_TOKENS: u64 = 175_000;

/// How far back the outlook's store query reads. The burn ladder's longest
/// rung is 4 days, and the weight fit needs whole 5-hour
/// windows; seven days covers both with the margin a quiet weekend needs.
/// The predecessor read its whole in-memory log tail — toker's equivalent is the
/// newest rows, capped like the startup seed.
pub const OUTLOOK_LOOKBACK_MS: i64 = 7 * 24 * 3600 * 1000;

/// How many rows the outlook's store query reads (the predecessor's
/// 16 MiB log tail,
/// as a row count — the same cap the startup seed uses).
pub const OUTLOOK_ROWS: u64 = 20_000;

const MIN_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MIN_MS;

// ── lane coldness ────────────────────────────────────────────────────

/// How long this lane's cache survives without traffic: read from the
/// tier the lane was last observed writing
/// rather than pinned, and an unrecorded or unrecognised tier is the LONG
/// one — see the module docs. The one addition over the predecessor: the
/// openai lane's own provider window (openrouter's sticky session,
/// [`crate::middleware::lanes::OPENAI_LANE_TTL_MS`]) is a recognised
/// duration, so an openai lane expires on its true 10-minute clock —
/// reading it as the unrecognised LONG one would hold the sleep lock
/// for an hour per openai request.
pub fn ttl_of(lane: &Lane) -> i64 {
    if lane.ttl == Some(300_000) {
        5 * MIN_MS
    } else if lane.ttl == Some(super::lanes::OPENAI_LANE_TTL_MS) {
        super::lanes::OPENAI_LANE_TTL_MS
    } else {
        HOUR_MS
    }
}

/// The idle measurement a cold lane carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coldness {
    /// How long since the lane's cache was last touched.
    pub idle_ms: i64,
    /// What a cold resume of that lane would re-read.
    pub prompt: u64,
}

/// Is this lane's cache gone, and is its prefix big enough to care about?
///
/// Every uncertain case is `None`: an absent lane has no idle time to
/// measure, and a lane without a recorded prompt (or one in the future — a
/// clock that moved, not a session idle for negative time) must lose the
/// notice rather than force one.
pub fn coldness(
    lane: Option<&Lane>,
    min_tokens: u64,
    min_idle_ms: Option<i64>,
    now_ms: i64,
) -> Option<Coldness> {
    let lane = lane?;
    let prompt = lane.prompt_tokens?;
    if prompt <= min_tokens as i64 {
        return None;
    }
    let idle_ms = now_ms - lane.updated_ms;
    if idle_ms < 0 {
        return None;
    }
    let floor = min_idle_ms.unwrap_or_else(|| ttl_of(lane));
    if idle_ms < floor {
        return None;
    }
    Some(Coldness {
        idle_ms,
        prompt: prompt.max(0) as u64,
    })
}

/// Whether the cache is gone and the prefix is worth acting on. **Not**
/// [`decide_cold`]: this one has no
/// once-per-spell rule and no summarising refusal — it answers the
/// retarget's question, "is the cache gone", not the notice's.
pub fn lane_is_cold(
    lane: Option<&Lane>,
    min_tokens: u64,
    min_idle_ms: Option<i64>,
    now_ms: i64,
) -> bool {
    coldness(lane, min_tokens, min_idle_ms, now_ms).is_some()
}

// ── the decision ─────────────────────────────────────────────────────

/// What kind of turn a request is, as far as the cold gate cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// Anything the client means to continue the conversation with.
    Ordinary,
    /// Carries the summarisation prompt: forwarded, never interrupted.
    Summarising,
    /// Claude Code's recap ([`crate::ir::AnthropicShape::recap`]): a
    /// fork of the whole conversation for one UI-only line.
    Recap,
}

/// Notice, withheld notice, held recap, or forward — the cold gate's
/// whole answer.
#[derive(Debug, Clone, PartialEq)]
pub enum ColdDecision {
    /// Not this gate's business: warm, small, already spoken about,
    /// summarising, or laneless.
    Forward,
    /// The notice would have fired, but the quota outlook says the window
    /// can absorb this re-read (the caller-side "forward AND fired
    /// notice" verdict,
    /// folded here so the suppression rule lives in one place —
    /// the drift the predecessor's comment warned about cannot happen). The row and the
    /// re-armed lane are the caller's; the figures ride along so a
    /// `cold-quiet` row can say what the decision rested on.
    Quiet {
        idle_ms: i64,
        prompt: u64,
        outlook: Outlook,
    },
    /// Serve the synthetic turn. `outlook` is `None` when none was computed
    /// (the toggle off, a young log) — the fixture's `outlook: null`.
    Notice {
        idle_ms: i64,
        prompt: u64,
        outlook: Option<Outlook>,
    },
    /// A recap on a cold lane: answered with [`RECAP_HELD`], never sent
    /// upstream, and the lane left exactly as it was.
    Recap { idle_ms: i64, prompt: u64 },
}

/// The held recap's reply. Claude Code shows a recap as one dim line
/// outside the transcript, so this is plain text with no notice frame;
/// fixed, so it is byte-stable like every gate's text.
pub const RECAP_HELD: &str =
    "Recap held by toker: the cache is cold, and a recap would re-read the whole session.";

/// Notice or forward (ported exactly,
/// with the suppression's verdict folded into [`ColdDecision::Quiet`]).
///
/// - A **recap** on a cold lane is held ([`ColdDecision::Recap`]),
///   before the once-per-spell rule and the outlook. Forwarding one buys
///   the whole re-read for a sentence of UI text: on 2026-10-06 a recap
///   fired three seconds after a notice (the synthetic turn's end resets
///   Claude Code's own staleness clock), the gate had already spoken, and
///   it rewrote 405k of cache for $3.26. Holding it never spends the
///   notice either, which a recap arriving first would otherwise take,
///   letting the real prompt after it through unwarned.
/// - A **summarising** request forwards: the notice exists to advise
///   `/compact`, and stopping one would halt the user a keystroke after
///   telling them to go ahead. This refusal is about interrupting, not
///   about the cache — which is why the retarget uses [`lane_is_cold`] and
///   not this.
/// - Already spoken about **this idle spell** (`noticed_at >= at`):
///   `at` moves only when the lane reaches upstream and the notice records
///   `noticed_at` instead, so a second notice would carry the identical
///   number to the first. What re-arms it is the lane being active again.
/// - The outlook runs **deliberately last**: a warm, small or already
///   spoken-about lane needs no quota estimate to stay quiet, and the
///   caller only measures one when this returns [`ColdDecision::Notice`]
///   (the caller asks twice for the same reason). Suppression records
///   nothing — marking the lane noticed here would spend its one notice on
///   a turn the user never saw.
pub fn decide_cold(
    lane: Option<&Lane>,
    turn: Turn,
    min_tokens: u64,
    min_idle_ms: Option<i64>,
    now_ms: i64,
    outlook: Option<&Outlook>,
    request_bound: Option<u64>,
) -> ColdDecision {
    if turn == Turn::Summarising {
        return ColdDecision::Forward;
    }
    // The lane says what was cached; only the request says what this send
    // would re-read. Subagents share their parent's session id, so a fresh
    // subagent with a sibling's toolset lands in that sibling's lane: on
    // 2026-10-05 one was stopped on its first request over a 484k re-read
    // its predecessor had left four hours earlier. A request whose upper
    // bound is under the threshold cannot be that re-read, whatever the
    // lane held. An unknown bound decides nothing.
    if request_bound.is_some_and(|bound| bound < min_tokens) {
        return ColdDecision::Forward;
    }
    let Some(lane) = lane else {
        return ColdDecision::Forward;
    };
    let Some(cold) = coldness(Some(lane), min_tokens, min_idle_ms, now_ms) else {
        return ColdDecision::Forward;
    };
    if turn == Turn::Recap {
        return ColdDecision::Recap {
            idle_ms: cold.idle_ms,
            prompt: cold.prompt,
        };
    }
    if lane
        .noticed_at
        .is_some_and(|noticed| noticed >= lane.updated_ms)
    {
        return ColdDecision::Forward;
    }
    if let Some(outlook) = outlook
        && outlook.known
        && outlook.on_track
    {
        return ColdDecision::Quiet {
            idle_ms: cold.idle_ms,
            prompt: cold.prompt,
            outlook: outlook.clone(),
        };
    }
    ColdDecision::Notice {
        idle_ms: cold.idle_ms,
        prompt: cold.prompt,
        outlook: outlook.cloned(),
    }
}

/// Remember that the notice has already been given for this idle spell:
/// **`at` does not move** —
/// nothing was measured and nothing reached upstream, so the lane's prefix
/// and cache age are exactly what they were, and the compaction the user
/// runs after reading the notice is still seen as cold. A store error
/// propagates; the caller loses the notice memory, never the request.
pub fn note_lane_notice(store: &Store, key: &str, now_ms: i64) -> crate::store::Result<()> {
    let Some(mut lane) = store.load_lane(key)? else {
        return Ok(());
    };
    lane.noticed_at = Some(now_ms);
    store.upsert_lane(&lane)
}

// ── the burn ladder ──────────────────────────────────────────────────

/// One meter's fields on a row's `rate_limits`, and the window's length
/// where known. The 5h/7d windows have lengths; the
/// overage window's is not known, and the outlook does not need it.
pub struct MeterSpec {
    pub util_key: &'static str,
    pub reset_key: &'static str,
    pub length_ms: Option<i64>,
}

/// The 5-hour window.
pub const METER_5H: MeterSpec = MeterSpec {
    util_key: "util5h",
    reset_key: "reset5h",
    length_ms: Some(5 * 3600 * 1000),
};

/// The 7-day window.
pub const METER_7D: MeterSpec = MeterSpec {
    util_key: "util7d",
    reset_key: "reset7d",
    length_ms: Some(7 * 24 * 3600 * 1000),
};

/// The overage window. Its length is NOT known —
/// the reset is a monthly instant, but utilisation has been seen to
/// restart at other times — so nothing may assume where it began: the
/// burn declines its zero-anchor for it and a span total resting on its
/// first reading can only give a floor. The TUI's quota panel reads it;
/// the cold outlook does not (the weights were fitted against 5-hour
/// windows).
pub const METER_OVERAGE: MeterSpec = MeterSpec {
    util_key: "utilOverage",
    reset_key: "resetOverage",
    length_ms: None,
};

/// The reporting step. Movement below two of these is not a measurement.
/// Shared with the TUI
/// quota panel's span totals, which run the same fall/restart tests.
pub(crate) const QUANTUM: f64 = 0.01;
const FLOOR: f64 = 2.0 * QUANTUM;

/// Binary-float tolerance: 0.57 - 0.55 is
/// 0.019999999999999907, which fails a bare `>= 0.02`.
pub(crate) const EPS: f64 = 1e-9;

/// Lookback rungs, shortest first: the
/// shortest rung that clears the floor wins, so a burst after a quiet hour
/// reads as a burst. 15, 30, 60, 120, 240, 480, 1440, 2880, 5760 minutes.
const LADDER_MS: [i64; 9] = [
    900_000,
    1_800_000,
    3_600_000,
    7_200_000,
    14_400_000,
    28_800_000,
    86_400_000,
    172_800_000,
    345_600_000,
];

/// Below this the span is too short for the quantisation to survive.
const MIN_SPAN_MS: i64 = 60_000;

/// How long a fall must hold before it counts as a restart rather than a
/// late-arriving reading — measured
/// p99.9 request duration is 164 s, so ten minutes clears reordering.
pub(crate) const REORDER_MS: i64 = 10 * 60_000;

/// A projection that spans nights must be measured across at least one
/// night.
const DIURNAL_MS: i64 = 24 * 3600 * 1000;

fn min_span_for(reset_ms: i64, now_ms: i64) -> i64 {
    if reset_ms - now_ms > DIURNAL_MS {
        DIURNAL_MS
    } else {
        MIN_SPAN_MS
    }
}

/// One reading of a meter: when, the utilisation, and the window it
/// describes (reset in epoch seconds, the wire form). The narrow shape
/// every consumer of the burn math extracts — the cold gate's outlook
/// from full ledger rows ([`burn_rate`]), the TUI's quota panel from
/// its meter-only read ([`burn_rate_samples`]) — so the ladder below
/// and the panel's span totals share one sample type, never a wider
/// row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterSample {
    /// When the reading was taken, epoch milliseconds.
    pub at_ms: i64,
    /// The meter's utilisation, as reported.
    pub util: f64,
    /// The window's reset, epoch seconds.
    pub reset_s: i64,
}

/// How fast a meter is being consumed
/// (the four states and the distinction between the last two are the point
/// of the module, ported verbatim):
///
/// - `None` — no reading for this meter at all;
/// - `Stale` — the last reading describes a window that has since rolled;
/// - `Insufficient` — too little to say anything, even a bound;
/// - `Bounded` — it has not moved; no rate, but a ceiling that is often
///   enough to answer the question;
/// - `Measured` — a rate, from a span that out-measures the quantisation.
///
/// Readings come from **every** row that carries the meter fields,
/// proxy-written kinds included (kept on purpose: after a block
/// they are the only rows there are, and their reset value is what says
/// whether the window is still live). The weight fit is the one that
/// excludes kinds — see [`fit_quota_model`].
#[derive(Debug, Clone, PartialEq)]
pub enum Burn {
    None,
    Stale {
        util: f64,
        reset_s: i64,
        observed_at: i64,
    },
    Insufficient {
        util: f64,
        reset_s: i64,
        observed_at: i64,
        samples: usize,
        anchored: bool,
    },
    Bounded {
        util: f64,
        reset_s: i64,
        observed_at: i64,
        samples: usize,
        anchored: bool,
        /// The earliest the target could be reached, as a rate ceiling
        /// (per ms) — `(max(delta, 0) + QUANTUM) / span`.
        rate_max: f64,
        delta: f64,
        span_ms: i64,
    },
    Measured {
        util: f64,
        reset_s: i64,
        observed_at: i64,
        samples: usize,
        anchored: bool,
        /// The measured rate (per ms): `delta / span`.
        rate: f64,
        delta: f64,
        span_ms: i64,
    },
}

impl Burn {
    /// The measured figure the decision rests on — the envelope's current
    /// utilisation for the live states, absent for no reading at all
    /// (invariant 3: absence ≠ zero).
    fn util(&self) -> Option<f64> {
        match self {
            Burn::None => None,
            burn => Some(burn.util_ref()),
        }
    }

    fn util_ref(&self) -> f64 {
        match self {
            Burn::None => unreachable!("the None state carries no utilisation"),
            Burn::Stale { util, .. }
            | Burn::Insufficient { util, .. }
            | Burn::Bounded { util, .. }
            | Burn::Measured { util, .. } => *util,
        }
    }

    fn reset_s(&self) -> Option<i64> {
        match self {
            Burn::None => None,
            Burn::Stale { reset_s, .. }
            | Burn::Insufficient { reset_s, .. }
            | Burn::Bounded { reset_s, .. }
            | Burn::Measured { reset_s, .. } => Some(*reset_s),
        }
    }

    /// The same burn
    /// with the re-read's share added to its utilisation, for the
    /// with-and-without pair of projections.
    fn plus_util(&self, extra: f64) -> Burn {
        match self {
            Burn::None => Burn::None,
            Burn::Stale {
                util,
                reset_s,
                observed_at,
            } => Burn::Stale {
                util: util + extra,
                reset_s: *reset_s,
                observed_at: *observed_at,
            },
            Burn::Insufficient {
                util,
                reset_s,
                observed_at,
                samples,
                anchored,
            } => Burn::Insufficient {
                util: util + extra,
                reset_s: *reset_s,
                observed_at: *observed_at,
                samples: *samples,
                anchored: *anchored,
            },
            Burn::Bounded {
                util,
                reset_s,
                observed_at,
                samples,
                anchored,
                rate_max,
                delta,
                span_ms,
            } => Burn::Bounded {
                util: util + extra,
                reset_s: *reset_s,
                observed_at: *observed_at,
                samples: *samples,
                anchored: *anchored,
                rate_max: *rate_max,
                delta: *delta,
                span_ms: *span_ms,
            },
            Burn::Measured {
                util,
                reset_s,
                observed_at,
                samples,
                anchored,
                rate,
                delta,
                span_ms,
            } => Burn::Measured {
                util: util + extra,
                reset_s: *reset_s,
                observed_at: *observed_at,
                samples: *samples,
                anchored: *anchored,
                rate: *rate,
                delta: *delta,
                span_ms: *span_ms,
            },
        }
    }
}

/// Measure how fast a meter is being consumed
/// (ported step for step — the window-restart fall
/// detection, the zero-reading anchor, the envelope over late-arriving
/// readings, and the shortest-clearing rung).
///
/// This is the row-taking entry the cold gate's outlook calls; the
/// ladder itself lives in [`burn_rate_samples`], shared with the TUI's
/// quota panel.
pub fn burn_rate(rows: &[RequestRow], spec: &MeterSpec, now_ms: i64) -> Burn {
    burn_rate_samples(&samples_of(rows, spec), spec, now_ms)
}

/// [`burn_rate`]'s per-row extraction: one sample per row that carries
/// the meter's fields, kinds included (see [`burn_rate`] for why those
/// rows stay).
fn samples_of(rows: &[RequestRow], spec: &MeterSpec) -> Vec<MeterSample> {
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

/// The burn ladder over pre-extracted samples — the entry the TUI's
/// quota panel uses. Its meter lookback reads the ledger through the
/// store's narrow meter row (four columns, one JSON parse), extracts
/// each meter's samples at the call boundary, and feeds them here, so
/// the ladder stays one copy of the math shared with the cold gate's
/// outlook ([`burn_rate`] is the same ladder over full rows). `spec`
/// supplies the window's length for the zero-reading anchor.
pub fn burn_rate_samples(samples: &[MeterSample], spec: &MeterSpec, now_ms: i64) -> Burn {
    let mut readings: Vec<MeterSample> = samples.to_vec();
    if readings.is_empty() {
        return Burn::None;
    }
    readings.sort_by_key(|reading| reading.at_ms);
    let latest = readings[readings.len() - 1];
    let reset_ms = latest.reset_s.saturating_mul(1000);
    if reset_ms <= now_ms {
        // The window rolled and nothing has reported the new one yet; the
        // last reading describes a window that no longer exists.
        return Burn::Stale {
            util: latest.util,
            reset_s: latest.reset_s,
            observed_at: latest.at_ms,
        };
    }

    // Readings of the current window only; a restart inside it is a fall
    // that never comes back and has outlasted reordering (both halves are
    // needed — the fall alone is jitter, "never returns" alone walks the
    // start forward to the last row).
    let same_window: Vec<MeterSample> = readings
        .iter()
        .filter(|reading| reading.reset_s == latest.reset_s)
        .copied()
        .collect();
    let mut peak_after = vec![f64::NEG_INFINITY; same_window.len()];
    let mut running = f64::NEG_INFINITY;
    for index in (0..same_window.len()).rev() {
        running = running.max(same_window[index].util);
        peak_after[index] = running;
    }
    let last_at = same_window[same_window.len() - 1].at_ms;
    let mut from = 0usize;
    let mut peak_before = same_window[0].util;
    for index in 1..same_window.len() {
        let fell = same_window[index].util + EPS < same_window[index - 1].util - QUANTUM;
        let held = last_at - same_window[index].at_ms >= REORDER_MS;
        if fell && held && peak_after[index] + QUANTUM + EPS < peak_before {
            from = index;
        }
        peak_before = peak_before.max(same_window[index].util);
    }
    let observed = &same_window[from..];
    let samples = observed.len();

    // The start of the window is itself a reading of zero where the
    // window's length is known — worth reconstructing, and never after a
    // restart (where the window opened is exactly what is not known).
    let mut anchored = false;
    let mut win: Vec<(i64, f64)> = observed
        .iter()
        .map(|reading| (reading.at_ms, reading.util))
        .collect();
    if from == 0
        && let Some(length) = spec.length_ms
    {
        let opened_at = reset_ms - length;
        if opened_at < now_ms && opened_at < observed[0].at_ms {
            win.insert(0, (opened_at, 0.0));
            anchored = true;
        }
    }
    if win.len() < 2 {
        return Burn::Insufficient {
            util: latest.util,
            reset_s: latest.reset_s,
            observed_at: latest.at_ms,
            samples,
            anchored,
        };
    }

    // The envelope: utilisation is cumulative within a window, so the
    // running maximum is what has been consumed and a lower later reading
    // is an older request reporting late.
    let mut peak = f64::NEG_INFINITY;
    for reading in &mut win {
        peak = peak.max(reading.1);
        reading.1 = peak;
    }
    let current = win[win.len() - 1].1;
    let min_span = min_span_for(reset_ms, now_ms);

    // The shortest rung whose measured span clears both floors — the span
    // MEASURED, never the rung asked for (a rung whose earliest reading is
    // far newer than its nominal start silently becomes a short one).
    for rung in LADDER_MS {
        let Some(pos) = win.iter().position(|reading| reading.0 >= now_ms - rung) else {
            continue;
        };
        if pos == win.len() - 1 {
            continue; // only the latest reading is inside: nothing to subtract
        }
        let (at, from_util) = win[pos];
        let delta = current - from_util;
        let span_ms = now_ms - at;
        if span_ms < min_span || delta + EPS < FLOOR {
            continue;
        }
        return Burn::Measured {
            util: current,
            reset_s: latest.reset_s,
            observed_at: latest.at_ms,
            samples,
            anchored,
            rate: delta / span_ms as f64,
            delta,
            span_ms,
        };
    }

    // Nothing moved enough to divide by — but over the widest span
    // available the true movement is under the observed step plus one
    // quantum of rounding at each end, a genuine ceiling on the rate.
    let (at, from_util) = win[0];
    let delta = current - from_util;
    let span_ms = now_ms - at;
    if span_ms < min_span {
        return Burn::Insufficient {
            util: current,
            reset_s: latest.reset_s,
            observed_at: latest.at_ms,
            samples,
            anchored,
        };
    }
    Burn::Bounded {
        util: current,
        reset_s: latest.reset_s,
        observed_at: latest.at_ms,
        samples,
        anchored,
        rate_max: (delta.max(0.0) + QUANTUM) / span_ms as f64,
        delta,
        span_ms,
    }
}

/// When a measured burn reaches `target`, against when the window resets.
/// `target` is 1 for exhaustion,
/// or the gate's threshold when the gate is on — the gate is the nearer
/// wall and the one that actually stops the session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// Not measurable, and the bound does not settle it either.
    Unknown,
    /// The window rolled; there is nothing to project.
    Stale,
    /// Already at or past the target.
    Reached,
    /// The window resets first.
    OnTrack,
    /// Reaches the target before the reset, at `at_ms` (epoch ms).
    Runout { at_ms: f64 },
}

pub fn project_to(burn: &Burn, target: f64, now_ms: i64) -> Verdict {
    let (util, reset_s, bounded, measured) = match burn {
        Burn::None | Burn::Insufficient { .. } => return Verdict::Unknown,
        Burn::Stale { .. } => return Verdict::Stale,
        Burn::Bounded { util, reset_s, .. } => (*util, *reset_s, true, None),
        Burn::Measured {
            util,
            reset_s,
            rate,
            ..
        } => (*util, *reset_s, false, Some(*rate)),
    };
    if util >= target {
        return Verdict::Reached;
    }
    let reset_at = (reset_s.saturating_mul(1000)) as f64;
    let remaining = target - util;
    if bounded {
        // The EARLIEST the target could be reached. If even that lands
        // after the reset, "on track" holds for every rate consistent with
        // the data — a real conclusion.
        let soonest = now_ms as f64 + remaining / burn_rate_max(burn);
        return if soonest >= reset_at {
            Verdict::OnTrack
        } else {
            Verdict::Unknown
        };
    }
    let Some(rate) = measured.filter(|rate| *rate > 0.0) else {
        return Verdict::Unknown;
    };
    let at = now_ms as f64 + remaining / rate;
    if at >= reset_at {
        Verdict::OnTrack
    } else {
        Verdict::Runout { at_ms: at }
    }
}

fn burn_rate_max(burn: &Burn) -> f64 {
    match burn {
        Burn::Bounded { rate_max, .. } => *rate_max,
        _ => unreachable!("only the bounded state carries a rate ceiling"),
    }
}

// ── the quota outlook ────────────────────────────────────────────────

/// What the quota window has to say about this re-read. Every uncertain
/// input answers `known: false`,
/// and the caller fires as it always did — a suppressed notice is a re-read
/// the user never hears about, so the feature is allowed to be less useful
/// and not to be silently wrong.
#[derive(Debug, Clone, PartialEq)]
pub struct Outlook {
    /// Whether the figures below are a measurement at all.
    pub known: bool,
    /// False = the window is heading for a wall (or already spent) — the
    /// notice fires and names it. True = the window can absorb this — the
    /// notice is withheld (the `cold-quiet` row).
    pub on_track: bool,
    /// Which window vetoed the suppression (`None` while on track).
    pub meter: Option<Meter>,
    /// The re-read's share of a 5-hour window (the only window the weights
    /// were fitted against).
    pub extra: Option<f64>,
    /// Whether `extra` was measured for this model group or borrowed from
    /// the host it was folded into — an upper bound, printed as "at most".
    pub bound: Option<bool>,
    /// The measured utilisation the decision rested on, from the burn —
    /// NOT the proxy's last-seen copy of the meters, which would report
    /// the proxy's own staleness as the API's.
    pub util: Option<f64>,
    /// The window's reset, epoch ms.
    pub reset_at_ms: Option<i64>,
    /// When the window hits its wall, epoch ms (`None` when it already
    /// has — nothing left to project).
    pub wall_at_ms: Option<f64>,
    /// How much the re-read pulls an existing wall in by (`None` where no
    /// wall existed beforehand — a number that would be the whole distance
    /// must not be printed as the cost of the re-read).
    pub pulled_in_ms: Option<f64>,
}

impl Outlook {
    fn unknown() -> Outlook {
        Outlook {
            known: false,
            on_track: false,
            meter: None,
            extra: None,
            bound: None,
            util: None,
            reset_at_ms: None,
            wall_at_ms: None,
            pulled_in_ms: None,
        }
    }
}

/// Whether the quota window can absorb this re-read without noticing.
///
/// NOT "would this re-read push the window over" — measured across every
/// notice in the log, it never would: a 200k re-read is about 2% of a
/// 5-hour window, so it can only tip a projection that already lands
/// within 2% of the target, and nothing ever did. What it does is bring an
/// existing wall forward by 1-6 minutes. So: quiet unless the window is
/// ALREADY projected to run out before it resets, with the re-read added.
pub fn quota_outlook(
    burn5h: &Burn,
    burn7d: Option<&Burn>,
    extra: f64,
    bound: bool,
    target: f64,
    now_ms: i64,
) -> Outlook {
    let unknown = Outlook::unknown();
    if !extra.is_finite() || extra < 0.0 {
        return unknown;
    }
    // No weight for this model's group reads as absence, never zero —
    // zero would read as "this re-read is free".
    let Some(util5h) = burn5h.util() else {
        return unknown;
    };
    let reset5h_ms = burn5h.reset_s().map(|reset| reset.saturating_mul(1000));

    let plain = project_to(burn5h, target, now_ms);
    if matches!(plain, Verdict::Unknown | Verdict::Stale) {
        return unknown;
    }
    if matches!(plain, Verdict::Reached) {
        // Nothing left to project: the meter is already at the wall.
        return Outlook {
            known: true,
            on_track: false,
            meter: Some(Meter::FiveHour),
            extra: Some(extra),
            bound: Some(bound),
            util: Some(util5h),
            reset_at_ms: reset5h_ms,
            wall_at_ms: None,
            pulled_in_ms: None,
        };
    }

    let after = project_to(&burn5h.plus_util(extra), target, now_ms);
    if matches!(after, Verdict::Unknown | Verdict::Stale) {
        // A bounded burn that cleared the reset without the re-read and
        // cannot be separated with it is not an answer. Fire.
        return unknown;
    }
    if !matches!(after, Verdict::OnTrack) {
        let wall_at_ms = match after {
            Verdict::Runout { at_ms } => Some(at_ms),
            _ => None,
        };
        // Only where a wall existed beforehand.
        let pulled_in_ms = match (plain, wall_at_ms) {
            (Verdict::Runout { at_ms }, Some(after_at)) => Some(at_ms - after_at),
            _ => None,
        };
        return Outlook {
            known: true,
            on_track: false,
            meter: Some(Meter::FiveHour),
            extra: Some(extra),
            bound: Some(bound),
            util: Some(util5h),
            reset_at_ms: reset5h_ms,
            wall_at_ms,
            pulled_in_ms,
        };
    }

    // The weekly meter takes no `extra` (the weights were fitted against
    // 5-hour windows and say nothing about a weekly one), but whether it is
    // already heading for its own wall needs no weight. An unmeasurable
    // one is not — demanding both be measurable would silence the feature
    // whenever the log is young.
    if let Some(burn7d) = burn7d
        && let Some(util7d) = burn7d.util()
    {
        let week = project_to(burn7d, target, now_ms);
        if matches!(week, Verdict::Runout { .. } | Verdict::Reached) {
            return Outlook {
                known: true,
                on_track: false,
                meter: Some(Meter::SevenDay),
                extra: Some(extra),
                bound: Some(bound),
                util: Some(util7d),
                reset_at_ms: burn7d.reset_s().map(|reset| reset.saturating_mul(1000)),
                wall_at_ms: match week {
                    Verdict::Runout { at_ms } => Some(at_ms),
                    _ => None,
                },
                pulled_in_ms: None,
            };
        }
    }

    // The estimate travels with the verdict even when nothing is said,
    // because the suppression is logged and a row that cannot say what the
    // decision rested on cannot be argued with later.
    Outlook {
        known: true,
        on_track: true,
        meter: None,
        extra: Some(extra),
        bound: Some(bound),
        util: Some(util5h),
        reset_at_ms: None,
        wall_at_ms: None,
        pulled_in_ms: None,
    }
}

/// The wall the projection is measured against: the gate's threshold when
/// the gate is armed, actual exhaustion otherwise.
pub fn outlook_target(gate_on: bool) -> f64 {
    if gate_on { THRESHOLD } else { 1.0 }
}

// ── the weight fit ───────────────────────────────────────────────────

/// The 5-hour window is not a dollar amount and not a token count:
/// consumption tracks fresh prompt tokens (uncached input + cache writes)
/// and output tokens, weighted per model. These weights are MEASUREMENTS
/// re-derived from the ledger (ported
/// for what the outlook calls; the predecessor's diagnostics surface
/// is not ported).
///
/// A window contributes nothing if utilisation barely moved across it.
const MIN_WINDOW_UTIL: f64 = 0.05;
const MIN_WINDOW_REQUESTS: usize = 20;
const MIN_WINDOWS: usize = 4;

/// A model group must appear in at least this many windows before its
/// weight is worth trying to separate.
const MIN_GROUP_WINDOWS: usize = 3;
const MIN_GROUP_SHARE: f64 = 0.01;

/// A window whose traffic is largely from groups that cannot be weighted
/// tells us nothing about the groups that can.
const MAX_UNATTRIBUTED_SHARE: f64 = 0.1;

/// Presence is not identifiability: a group whose volume moves in lockstep
/// with another's has no weight the data can separate.
const MAX_COLLINEARITY_R2: f64 = 0.98;

/// The weaker net for groups that are separable in principle but unstable
/// in this particular log: leave one window out, and demote any group
/// whose own contribution swings by more than this fraction of itself.
const MAX_CONTRIBUTION_SWING: f64 = 0.5;

/// One model group's fitted weight: window fraction per million fresh /
/// output tokens, each with its spread — the least and greatest value the
/// weight took across the leave-one-window-out refits (the predecessor's
/// `freshSpread` / `outputSpread`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupWeight {
    pub fresh: f64,
    pub output: f64,
    pub fresh_spread: Spread,
    pub output_spread: Spread,
}

/// The range one weight took across the leave-one-out refits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spread {
    pub min: f64,
    pub max: f64,
}

/// Where a refit's weight counts as zero. The solver stops once no step
/// moves a weight by 1e-14, so a weight it is driving to zero from above
/// can stop a few ulps short of it (1.9e-15 in the test that pins this)
/// rather than clamping to exactly 0.0 (the live ledger's case). A
/// billionth of a window per million tokens is no weight at any scale
/// this meters.
const SEPARATION_FLOOR: f64 = 1e-9;

impl Spread {
    /// Whether the data separates this weight from zero. The solver is
    /// non-negative, so a refit that reaches zero is a refit in which the
    /// weight could be dropped and the windows still explained: the point
    /// weight is then whatever the other columns left over, not a
    /// measurement. A weight whose spread reaches zero is no weight.
    pub fn separated(&self) -> bool {
        self.min.is_finite() && self.max.is_finite() && self.min > SEPARATION_FLOOR
    }
}

/// The fitted quota model, or the reason there is none. `ok: false` means
/// the log cannot support any weight — a young log, a log whose windows
/// are dominated by traffic that cannot be attributed, or one whose
/// utilisation did not track volume. The caller treats that as "no
/// estimate" and fires the notice exactly as it always did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaFit {
    pub ok: bool,
    /// Why the fit declined, when it did (for the log line).
    pub reason: Option<&'static str>,
    weights: BTreeMap<String, GroupWeight>,
    folded: BTreeSet<String>,
}

impl QuotaFit {
    /// What `fresh` prompt tokens on `model` would cost a window, with the
    /// provenance of the figure:
    /// `(fresh / 1e6) × weight`, and `bound` true where the group
    /// borrowed its weight from the host it was folded into — an upper
    /// bound rather than a measurement, which a caller that prints it must
    /// say ("up to"). A group with no weight (unattributed, or a model
    /// the fit never saw) answers `None`, never zero.
    ///
    /// A fresh weight the data cannot separate from zero (its spread
    /// reaches zero) also answers `None`, so the outlook is blind and the
    /// notice fires. Measured over the whole live log: opus-5-5's fresh
    /// weight fitted at ~0.009 a million with a spread of 0.000-0.018,
    /// while live re-reads of 260k and 434k cache-written tokens each moved
    /// the 5-hour meter about two points — five to eight times the point
    /// figure, which priced each as a fraction of a point.
    ///
    /// A separated weight is priced at the TOP of its spread, not the
    /// point fit: over the 7-day window opus-5-5 fitted 0.029 (0.014-0.055)
    /// and still priced those re-reads at a third of what the meter moved.
    /// The outlook decides whether to stay quiet, so an underestimate costs
    /// a missed warning and an overestimate costs one extra notice; the
    /// cautious end is the right one to be wrong at.
    pub fn quota_for(&self, model: &str, fresh: u64) -> Option<(f64, bool)> {
        let weight = self.weights.get(&price_key(model))?;
        if !weight.fresh_spread.separated() {
            return None;
        }
        Some((
            fresh as f64 / 1e6 * weight.fresh_spread.max.max(weight.fresh),
            self.folded.contains(&price_key(model)),
        ))
    }

    /// Every weighted group, keyed by price signature, with whether its
    /// weight was borrowed from the host it was folded into — the fit's
    /// own evidence, for a reader that wants to disbelieve it.
    pub fn groups(&self) -> impl Iterator<Item = (&str, &GroupWeight, bool)> {
        self.weights
            .iter()
            .map(|(key, weight)| (key.as_str(), weight, self.folded.contains(key)))
    }
}

/// Fresh prompt tokens: everything the model had to read that was not
/// cached.
fn fresh_of(row: &RequestRow) -> f64 {
    (row.input.unwrap_or(0).max(0) + row.cache_write_total.unwrap_or(0).max(0)) as f64
}

/// Is this a row the API answered, rather than one the proxy wrote about
/// itself? (Any `kind` disqualifies.)
fn is_response_row(row: &RequestRow) -> bool {
    row.kind.is_none() && row.model.as_deref().is_some_and(|model| !model.is_empty())
}

/// Models sharing a price row plausibly share a quota weight; pooling by
/// price signature rather than name needs no maintenance when a model is
/// added. Routing aliases are not price evidence: the fit keys rows on the
/// model the RESPONSE named, which is the identity the host served after
/// any model map, and a caller asking [`QuotaFit::quota_for`] about a
/// request must ask about the model the map will send, never the alias
/// the client named.
fn price_key(model: &str) -> String {
    match price(model, false, None) {
        Some(priced) => format!(
            "{}/{}/{}/{}/{}",
            priced.rates.input,
            priced.rates.output,
            priced.rates.write_5m,
            priced.rates.write_1h,
            priced.rates.read
        ),
        None => format!(
            "unpriced:{}",
            normalise_model_id(model).unwrap_or_else(|| model.to_owned())
        ),
    }
}

#[derive(Debug, Clone)]
struct GroupAgg {
    fresh: f64,
    output: f64,
    models: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct Window {
    du: f64,
    groups: BTreeMap<String, GroupAgg>,
}

/// Split the log into 5-hour windows and measure how far utilisation
/// advanced across each. The first
/// row anchors the window: its own usage is already reflected in the
/// utilisation it reports, so it is not part of the advance.
fn build_windows(rows: &[RequestRow]) -> Vec<Window> {
    let mut by_reset: BTreeMap<i64, Vec<(&RequestRow, f64)>> = BTreeMap::new();
    for row in rows.iter().filter(|row| is_response_row(row)) {
        let Some(limits) = row.rate_limits.as_ref() else {
            continue;
        };
        let (Some(util), Some(reset)) = (
            limits.get("util5h").and_then(Value::as_f64),
            limits.get("reset5h").and_then(Value::as_i64),
        ) else {
            continue;
        };
        by_reset.entry(reset).or_default().push((row, util));
    }
    let mut out = Vec::new();
    for (reset, mut all) in by_reset {
        let _ = reset; // the window's identity; only the grouping needs it
        all.sort_by_key(|(row, _)| row.ts_ms);
        let du = all.last().expect("non-empty").1 - all[0].1;
        let body = &all[1..];
        if du < MIN_WINDOW_UTIL || body.len() < MIN_WINDOW_REQUESTS {
            continue;
        }
        let mut groups: BTreeMap<String, GroupAgg> = BTreeMap::new();
        for (row, _) in body {
            let model = row.model.as_deref().expect("is_response_row checked");
            let group = groups.entry(price_key(model)).or_insert(GroupAgg {
                fresh: 0.0,
                output: 0.0,
                models: BTreeSet::new(),
            });
            group.fresh += fresh_of(row);
            group.output += row.output.unwrap_or(0).max(0) as f64;
            group
                .models
                .insert(model_identity(model).unwrap_or_else(|| model.to_owned()));
        }
        out.push(Window { du, groups });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupStatus {
    Fitted,
    Folded,
    Unattributed,
}

#[derive(Debug, Clone)]
struct GroupInfo {
    key: String,
    status: GroupStatus,
    present: usize,
    fresh: f64,
    output: f64,
    folded_into: Option<String>,
}

/// Fraction of `target`'s variation explained by a least-squares
/// combination of `others`, through the normal equations with a small
/// ridge term — enough for the
/// handful of columns here and cannot blow up when two are identical, the
/// case this exists to detect.
fn explained_by(target: &[f64], others: &[Vec<f64>]) -> f64 {
    if others.is_empty() {
        return 0.0;
    }
    let k = others.len();
    let mut g = vec![vec![0.0; k]; k];
    let mut b = vec![0.0; k];
    for (i, oi) in others.iter().enumerate() {
        for (j, oj) in others.iter().enumerate() {
            g[i][j] = oi.iter().zip(oj).map(|(x, y)| x * y).sum();
        }
        b[i] = oi.iter().zip(target).map(|(x, t)| x * t).sum();
    }
    let scale = {
        let mut diag = 0.0f64;
        for (i, row) in g.iter().enumerate() {
            diag = diag.max(row[i]);
        }
        if diag == 0.0 { 1.0 } else { diag }
    };
    for (i, row) in g.iter_mut().enumerate() {
        row[i] += scale * 1e-9;
    }
    // Gauss-Jordan on [G | b].
    let mut m: Vec<Vec<f64>> = g
        .into_iter()
        .zip(&b)
        .map(|(mut row, bi)| {
            row.push(*bi);
            row
        })
        .collect();
    for c in 0..k {
        let mut pivot = c;
        for r in (c + 1)..k {
            if m[r][c].abs() > m[pivot][c].abs() {
                pivot = r;
            }
        }
        m.swap(c, pivot);
        if m[c][c] == 0.0 {
            continue;
        }
        for r in 0..k {
            if r == c {
                continue;
            }
            let factor = m[r][c] / m[c][c];
            // r != c, so the pivot row is never the row being reduced;
            // copy it out to keep the borrows apart (k is a handful).
            let pivot_row: Vec<f64> = m[c].clone();
            for (j, slot) in m[r].iter_mut().enumerate().take(k + 1).skip(c) {
                *slot -= factor * pivot_row[j];
            }
        }
    }
    let coef: Vec<f64> = (0..k)
        .map(|i| {
            if m[i][i] != 0.0 {
                m[i][k] / m[i][i]
            } else {
                0.0
            }
        })
        .collect();
    let mean = target.iter().sum::<f64>() / target.len() as f64;
    let mut ss = 0.0f64;
    let mut tss = 0.0f64;
    for (t, others_t) in target.iter().zip((0..target.len()).map(|t| {
        others
            .iter()
            .zip(&coef)
            .map(|(column, c)| column[t] * c)
            .sum::<f64>()
    })) {
        ss += (t - others_t) * (t - others_t);
        tss += (t - mean) * (t - mean);
    }
    if tss == 0.0 {
        0.0
    } else {
        (1.0 - ss / tss).max(0.0)
    }
}

/// Non-negative least squares by projected coordinate descent.
fn nnls(a: &[Vec<f64>], y: &[f64]) -> Vec<f64> {
    let m = a.len();
    let k = a.first().map_or(0, Vec::len);
    let mut x = vec![0.0; k];
    let mut colsq = vec![0.0; k];
    for (j, column_sq) in colsq.iter_mut().enumerate() {
        let sum: f64 = (0..m).map(|i| a[i][j] * a[i][j]).sum();
        *column_sq = if sum == 0.0 { 1e-12 } else { sum };
    }
    let mut resid = y.to_vec();
    for _ in 0..5000 {
        let mut moved = 0.0f64;
        for j in 0..k {
            let gradient: f64 = (0..m).map(|i| a[i][j] * resid[i]).sum();
            let mut step = gradient / colsq[j];
            if x[j] + step < 0.0 {
                step = -x[j];
            }
            if step == 0.0 {
                continue;
            }
            x[j] += step;
            for i in 0..m {
                resid[i] -= a[i][j] * step;
            }
            moved = moved.max(step.abs());
        }
        if moved < 1e-14 {
            break;
        }
    }
    x
}

/// Decide, per group, whether its weight can be separated from the others':
/// fitted, folded (present throughout
/// but moving in lockstep — no weight of its own), or unattributed (too
/// few windows to say anything).
fn classify(windows: &[Window]) -> BTreeMap<String, GroupInfo> {
    let mut keys: BTreeSet<String> = BTreeSet::new();
    for window in windows {
        keys.extend(window.groups.keys().cloned());
    }
    let mut info: BTreeMap<String, GroupInfo> = BTreeMap::new();
    for key in keys {
        let mut shares: Vec<f64> = Vec::with_capacity(windows.len());
        let mut present = 0;
        let mut fresh = 0.0;
        let mut output = 0.0;
        for window in windows {
            let total: f64 = window
                .groups
                .values()
                .map(|group| group.fresh + group.output)
                .sum();
            let group = window.groups.get(&key);
            let volume = group.map_or(0.0, |group| group.fresh + group.output);
            let share = if total == 0.0 { 0.0 } else { volume / total };
            shares.push(share);
            if share >= MIN_GROUP_SHARE {
                present += 1;
            }
            if let Some(group) = group {
                fresh += group.fresh;
                output += group.output;
            }
        }
        info.insert(
            key.clone(),
            GroupInfo {
                key,
                status: GroupStatus::Unattributed,
                present,
                fresh,
                output,
                folded_into: None,
            },
        );
    }
    for group in info.values_mut() {
        group.status = if group.present < MIN_GROUP_WINDOWS {
            GroupStatus::Unattributed
        } else {
            GroupStatus::Fitted
        };
    }
    // Fold groups that move in lockstep with the rest, smallest first,
    // until what remains is mutually separable — folding the smallest is
    // what makes the rider ride, not the group carrying the traffic.
    let volume_of = |info: &GroupInfo| info.fresh + info.output;
    let volume =
        |info: &BTreeMap<String, GroupInfo>, key: &str| info.get(key).map_or(0.0, volume_of);
    loop {
        let live: Vec<String> = info
            .values()
            .filter(|group| group.status == GroupStatus::Fitted)
            .map(|group| group.key.clone())
            .collect();
        if live.len() < 2 {
            break;
        }
        let vectors: BTreeMap<&str, Vec<f64>> = live
            .iter()
            .map(|key| {
                (
                    key.as_str(),
                    windows
                        .iter()
                        .map(|window| {
                            window
                                .groups
                                .get(key)
                                .map_or(0.0, |group| group.fresh + group.output)
                        })
                        .collect(),
                )
            })
            .collect();
        let worst = live
            .iter()
            .filter_map(|key| {
                let others: Vec<Vec<f64>> = live
                    .iter()
                    .filter(|other| other != &key)
                    .filter_map(|other| vectors.get(other.as_str()))
                    .cloned()
                    .collect();
                let r2 = explained_by(vectors.get(key.as_str())?, &others);
                (r2 >= MAX_COLLINEARITY_R2).then_some((key, r2))
            })
            .min_by(|a, b| {
                volume(&info, a.0)
                    .total_cmp(&volume(&info, b.0))
                    .then(b.1.total_cmp(&a.1))
            });
        let Some((key, _)) = worst else {
            break;
        };
        info.get_mut(key).expect("key came from the map").status = GroupStatus::Folded;
    }
    info
}

/// One solve: the NNLS over the fitted groups' fresh/output columns, plus
/// how much each group's measured contribution swings when any one window
/// is left out.
struct Solve {
    x: Vec<f64>,
    /// Columns in order: (group key, 0 = fresh | 1 = output).
    columns: Vec<(String, usize)>,
    swing: BTreeMap<String, f64>,
    /// Per column, the range its weight took across the leave-one-out
    /// refits (indexed like `columns`).
    spread: Vec<Spread>,
}

fn solve(
    windows: &[Window],
    fitted: &[String],
    fold_map: &BTreeMap<String, Option<String>>,
) -> Solve {
    let mut columns: Vec<(String, usize)> = Vec::with_capacity(fitted.len() * 2);
    for key in fitted {
        columns.push((key.clone(), 0));
        columns.push((key.clone(), 1));
    }
    let idx = |key: &str, field: usize| {
        columns
            .iter()
            .position(|(k, f)| k == key && *f == field)
            .expect("the column set covers every fitted key")
    };
    // A folded group's volume rides on the group it was folded into — but
    // no fold has a host until the demotion loop ends, so during the
    // solves a folded group contributes to nobody (ported as-is).
    let row_of = |window: &Window| -> Vec<f64> {
        columns
            .iter()
            .map(|(key, field)| {
                let mut v = window.groups.get(key).map_or(0.0, |group| {
                    if *field == 0 {
                        group.fresh
                    } else {
                        group.output
                    }
                });
                for (from, into) in fold_map {
                    if *into == Some(key.clone()) {
                        v += window.groups.get(from).map_or(0.0, |group| {
                            if *field == 0 {
                                group.fresh
                            } else {
                                group.output
                            }
                        });
                    }
                }
                v / 1e6
            })
            .collect()
    };
    let a: Vec<Vec<f64>> = windows.iter().map(row_of).collect();
    let y: Vec<f64> = windows.iter().map(|window| window.du).collect();
    let x = nnls(&a, &y);

    let contribution = |x: &[f64], key: &str| -> f64 {
        a.iter()
            .map(|row| row[idx(key, 0)] * x[idx(key, 0)] + row[idx(key, 1)] * x[idx(key, 1)])
            .sum()
    };
    let mut contributions: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for key in fitted {
        contributions.insert(key.clone(), Vec::new());
    }
    let mut spread = vec![
        Spread {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        };
        columns.len()
    ];
    for leave_out in 0..windows.len() {
        let a_reduced: Vec<Vec<f64>> = a
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != leave_out)
            .map(|(_, row)| row.clone())
            .collect();
        let y_reduced: Vec<f64> = y
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != leave_out)
            .map(|(_, v)| *v)
            .collect();
        let x_loo = nnls(&a_reduced, &y_reduced);
        for (column, range) in spread.iter_mut().enumerate() {
            range.min = range.min.min(x_loo[column]);
            range.max = range.max.max(x_loo[column]);
        }
        for key in fitted {
            contributions
                .get_mut(key)
                .expect("key came from the map")
                .push(contribution(&x_loo, key));
        }
    }
    let mut swing = BTreeMap::new();
    for key in fitted {
        let vals = contributions.get(key).expect("key came from the map");
        let mean = vals.iter().sum::<f64>() / vals.len().max(1) as f64;
        swing.insert(
            key.clone(),
            if mean > 0.0 {
                (vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                    - vals.iter().cloned().fold(f64::INFINITY, f64::min))
                    / mean
            } else {
                f64::INFINITY
            },
        );
    }
    Solve {
        x,
        columns,
        swing,
        spread,
    }
}

/// Fit the model: weights when
/// the log can support them, a reason when it cannot — never a number
/// without the evidence behind it.
pub fn fit_quota_model(rows: &[RequestRow]) -> QuotaFit {
    let unfit = |reason| QuotaFit {
        ok: false,
        reason: Some(reason),
        ..QuotaFit::default()
    };
    let all = build_windows(rows);
    if all.len() < MIN_WINDOWS {
        return unfit("too few usable 5-hour windows");
    }
    let mut info = classify(&all);

    // Drop windows the unattributed groups dominate: they cannot be
    // explained, so including them drags every other weight to cover the
    // gap.
    let unattributed: BTreeSet<String> = info
        .values()
        .filter(|group| group.status == GroupStatus::Unattributed)
        .map(|group| group.key.clone())
        .collect();
    let windows: Vec<Window> = all
        .into_iter()
        .filter(|window| {
            let mut total = 0.0;
            let mut unattributed_volume = 0.0;
            for (key, group) in &window.groups {
                total += group.fresh + group.output;
                if unattributed.contains(key) {
                    unattributed_volume += group.fresh + group.output;
                }
            }
            total == 0.0 || unattributed_volume / total <= MAX_UNATTRIBUTED_SHARE
        })
        .collect();
    if windows.len() < MIN_WINDOWS {
        return unfit("the usable windows are dominated by unattributable traffic");
    }

    // Every folded group maps to a host that is not known
    // until the demotion loop ends, so they carry `None` through all the
    // solves and borrow the host's weight only at the end.
    let mut fold_map: BTreeMap<String, Option<String>> = info
        .values()
        .filter(|group| group.status == GroupStatus::Folded)
        .map(|group| (group.key.clone(), None))
        .collect();

    // Demote unstable groups one at a time, worst first, refitting each
    // time — smallest-volume candidate first: the rider moves, not the
    // host. One pass per group at most, so this terminates.
    let mut fitted: Vec<String> = info
        .values()
        .filter(|group| group.status == GroupStatus::Fitted)
        .map(|group| group.key.clone())
        .collect();
    let volume = |info: &BTreeMap<String, GroupInfo>, key: &str| {
        info.get(key)
            .map_or(0.0, |group| group.fresh + group.output)
    };
    let mut sol = (!fitted.is_empty()).then(|| solve(&windows, &fitted, &fold_map));
    while let Some(current) = &sol
        && fitted.len() > 1
    {
        let unstable = fitted
            .iter()
            .filter(|key| {
                current
                    .swing
                    .get(*key)
                    .is_some_and(|swing| *swing > MAX_CONTRIBUTION_SWING)
            })
            .min_by(|a, b| volume(&info, a).total_cmp(&volume(&info, b)));
        let Some(unstable) = unstable else {
            break;
        };
        let unstable = unstable.clone();
        fitted.retain(|key| key != &unstable);
        let group = info.get_mut(&unstable).expect("key came from the map");
        group.status = GroupStatus::Folded;
        fold_map.insert(unstable.clone(), None);
        sol = Some(solve(&windows, &fitted, &fold_map));
    }
    // A sole remaining group that is still unstable means the log cannot
    // support any weight at all; say so rather than reporting the number.
    if let Some(current) = &sol
        && fitted.len() == 1
        && current
            .swing
            .get(fitted[0].as_str())
            .is_some_and(|swing| *swing > MAX_CONTRIBUTION_SWING)
    {
        return unfit("utilisation did not track token volume consistently");
    }
    let Some(sol) = sol else {
        return unfit("no model group had enough presence and independent variation");
    };

    // The host every folded group borrows from: the largest fitted group.
    let host = fitted
        .iter()
        .max_by(|a, b| volume(&info, a).total_cmp(&volume(&info, b)))
        .cloned();
    let Some(host) = host else {
        return unfit("no model group had enough presence and independent variation");
    };
    for group in info.values_mut() {
        if group.status == GroupStatus::Folded && group.folded_into.is_none() {
            group.folded_into = Some(host.clone());
        }
    }
    for into in fold_map.values_mut() {
        if into.is_none() {
            *into = Some(host.clone());
        }
    }

    // Fitted groups carry their own weight; folded groups borrow the
    // host's (that is the `bound` provenance); unattributed groups get
    // none.
    let mut weights: BTreeMap<String, GroupWeight> = BTreeMap::new();
    for key in &fitted {
        let column = |field: usize| {
            sol.columns
                .iter()
                .position(|(k, f)| k == key && *f == field)
                .expect("the column set covers every fitted key")
        };
        let (fresh, output) = (sol.x[column(0)], sol.x[column(1)]);
        weights.insert(
            key.clone(),
            GroupWeight {
                fresh: if fresh.is_finite() { fresh } else { 0.0 },
                output: if output.is_finite() { output } else { 0.0 },
                fresh_spread: sol.spread[column(0)],
                output_spread: sol.spread[column(1)],
            },
        );
    }
    let mut folded: BTreeSet<String> = BTreeSet::new();
    for group in info.values() {
        if group.status == GroupStatus::Folded
            && let Some(weight) = group
                .folded_into
                .as_ref()
                .and_then(|host| weights.get(host).copied())
        {
            weights.insert(group.key.clone(), weight);
            folded.insert(group.key.clone());
        }
    }
    QuotaFit {
        ok: true,
        reason: None,
        weights,
        folded,
    }
}

/// What the quota window has to say about re-reading `fresh` tokens on
/// `model`, over `rows` (the pure
/// core; the store fetch is [`outlook_over`]). `None` is the "blind"
/// verdict: the
/// the toggle off, a young log that cannot fit weights, or a model whose
/// group never separated — and the notice fires exactly as it did before
/// this existed.
pub fn outlook_of(
    rows: &[RequestRow],
    model: Option<&str>,
    fresh: u64,
    gate_on: bool,
    now_ms: i64,
) -> Option<Outlook> {
    let fit = fit_quota_model(rows);
    if !fit.ok {
        return None;
    }
    let model = model?;
    let (extra, bound) = fit.quota_for(model, fresh)?;
    let burn5h = burn_rate(rows, &METER_5H, now_ms);
    let burn7d = burn_rate(rows, &METER_7D, now_ms);
    Some(quota_outlook(
        &burn5h,
        Some(&burn7d),
        extra,
        bound,
        outlook_target(gate_on),
        now_ms,
    ))
}

/// [`outlook_of`] over the store's recent rows from `provider` — the
/// backend the request is routed to — the newest [`OUTLOOK_ROWS`] rows
/// within [`OUTLOOK_LOOKBACK_MS`]. A store error is "blind" — the notice
/// fires, the safe direction.
///
/// Only the routed backend's rows: a window is one backend's meter, and
/// reading every row once let a request bound for a backend with no
/// 5-hour meter at all (the plain API, codex's differently-shaped limits)
/// be quietened by the subscription's burn and weights. A backend that is
/// not a 5-hour meter source has no such rows, so it has no burn and no
/// fit — blind, and the notice fires. `model` must be the identity the
/// backend's model map will send (see [`price_key`]).
pub fn outlook_over(
    store: &Store,
    provider: &str,
    model: Option<&str>,
    fresh: u64,
    gate_on: bool,
    now_ms: i64,
) -> Option<Outlook> {
    let rows: Vec<RequestRow> = store
        .requests_since(now_ms - OUTLOOK_LOOKBACK_MS, OUTLOOK_ROWS)
        .ok()?
        .into_iter()
        .filter(|row| row.provider.as_deref() == Some(provider))
        .collect();
    outlook_of(&rows, model, fresh, gate_on, now_ms)
}

// ── the notice ──────────────────────────────────────────────────────

/// `2` for 2.0, `1.25` for 1.25: a multiplier as a person writes it.
fn trim_float(value: f64) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// "47m", "2h 6m", "3h" — the shape a human uses for "how long was I away".
pub fn human_idle(ms: i64) -> String {
    let minutes = ((ms as f64) / 60_000.0).round() as i64;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let minutes = minutes % 60;
    // A defensive `if (m === 60)` is unrepresentable here: minutes is
    // already rounded and remaindered, so it is always 0..59.
    if minutes != 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{hours}h")
    }
}

/// Render `at_ms` in `tz` at one of the three resolutions a future instant
/// needs: a bare clock within 20 h
/// (a future 06:43 read at 20:45 can only be tomorrow), a weekday within
/// 6 d, day + month beyond. The forms are pinned, not locale-derived
/// (invariant 4): `%H:%M`, `%a %H:%M`, day-then-month.
pub(crate) fn at_scale(at_ms: f64, tz: &TimeZone, scale: usize) -> String {
    let Some(zoned) = zoned_of(at_ms, tz) else {
        return "?".to_owned();
    };
    match scale {
        0 => zoned.strftime("%H:%M").to_string(),
        1 => zoned.strftime("%a %H:%M").to_string(),
        _ => format!("{} {}", zoned.day(), zoned.strftime("%b")),
    }
}

pub(crate) fn scale_of(at_ms: f64, now_ms: i64) -> usize {
    let delta = at_ms - now_ms as f64;
    if delta < 20.0 * 3600e3 {
        0
    } else if delta < 6.0 * 86400e3 {
        1
    } else {
        2
    }
}

/// `at`, labelled so it cannot be misread as belonging to `other`'s day:
/// "resets Mon 05:00 · stops ~06:43"
/// reads as 06:43 on Monday when in fact it is Thursday. `d` carries at
/// least a weekday whenever `other` carries one. The TUI's quota panel
/// shares this for its meter lines — one labelling rule, everywhere two
/// instants sit side by side.
pub(crate) fn alongside(at_ms: f64, other_ms: f64, now_ms: i64, tz: &TimeZone) -> String {
    at_scale(at_ms, tz, alongside_scale(at_ms, other_ms, now_ms))
}

/// [`alongside`]'s resolution, apart from its pinned forms: the TUI
/// renders the same rule in the user's locale.
pub(crate) fn alongside_scale(at_ms: f64, other_ms: f64, now_ms: i64) -> usize {
    // The scale is max(own, min(other, 1)); the
    // final 0 is defensive (scales are non-negative here), so it has no
    // Rust equivalent to carry.
    scale_of(at_ms, now_ms).max(scale_of(other_ms, now_ms).min(1))
}

/// A future instant at the coarsest resolution that still identifies it
/// (the same three scales `alongside`
/// picks from, chosen on the instant alone): the TUI quota panel's
/// `resets` clause.
pub fn reset_label(at_ms: f64, now_ms: i64, tz: &TimeZone) -> String {
    at_scale(at_ms, tz, scale_of(at_ms, now_ms))
}

pub(crate) fn zoned_of(at_ms: f64, tz: &TimeZone) -> Option<jiff::Zoned> {
    // jiff's Timestamp range is ±9999 years; clamp rather than guess.
    let ms = at_ms.clamp(-253_402_300_799_000.0, 253_402_300_799_000.0) as i64;
    Some(
        jiff::Timestamp::from_millisecond(ms)
            .ok()?
            .to_zoned(tz.clone()),
    )
}

/// The quota line of the cold notice
/// — the part that says the window was ALREADY heading for its wall,
/// never that the re-read caused one.
fn outlook_line(outlook: Option<&Outlook>, now_ms: i64, tz: &TimeZone) -> Option<String> {
    let o = outlook.filter(|o| o.known && !o.on_track)?;
    let label = match o.meter {
        Some(Meter::SevenDay) => "7-day",
        _ => "5-hour",
    };
    // The share is always of a 5-HOUR window — the only window the weights
    // were fitted against — named twice where the sentence is already
    // about the 5-hour meter reads as a stutter, and omitted where the
    // weekly meter vetoed the suppression only invites reading the
    // percentage as a share of the week.
    let share = o.extra.filter(|extra| extra.is_finite()).map(|extra| {
        format!(
            // "up to": the weight is the top of its spread (see
            // `QuotaFit::quota_for`), and a borrowed one is an upper bound
            // besides.
            "this re-read is up to about {:.1}% of a {}window",
            extra * 100.0,
            if o.meter == Some(Meter::SevenDay) {
                "5-hour "
            } else {
                ""
            }
        )
    });
    let share_clause = share
        .as_ref()
        .map(|share| format!(", and {share}"))
        .unwrap_or_default();
    let Some(wall_at) = o.wall_at_ms else {
        // Nothing left to project: the meter is already at the wall.
        return Some(format!(
            "The {label} window was already spent{share_clause}."
        ));
    };
    let wall = alongside(
        wall_at,
        o.reset_at_ms.map(|reset| reset as f64).unwrap_or(wall_at),
        now_ms,
        tz,
    );
    let reset = o
        .reset_at_ms
        .map(|reset_at| {
            format!(
                ", {} before it resets at {}",
                human_idle((reset_at as f64 - wall_at) as i64),
                alongside(reset_at as f64, wall_at, now_ms, tz)
            )
        })
        .unwrap_or_default();
    // Under a minute the clause reads "brings that forward by 0m", which
    // spends a line to report no effect — it happens exactly when the
    // notice fires.
    let moves = o
        .pulled_in_ms
        .filter(|pulled| *pulled >= MIN_MS as f64)
        .map(|pulled| format!(" and brings that forward by {}", human_idle(pulled as i64)))
        .unwrap_or_default();
    let share_tail = share
        .as_ref()
        .map(|share| format!("; {share}{moves}"))
        .unwrap_or_default();
    Some(format!(
        "The {label} window was already heading for its wall at {wall}{reset}{share_tail}."
    ))
}

/// The cold notice: the synthetic turn's text,
/// wrapped per `style` like the quota gate's.
pub struct ColdBlocking;

impl ColdBlocking {
    /// The text the user sees — the whole interface of this feature.
    ///
    /// Third-person and framed by its style (brackets only in the plain
    /// style), for the same reason the quota block notice is: so the model
    /// reads it as harness output rather than its own words.
    /// Written as a record of an event and stamped, because it is live for
    /// one turn and historical for the rest of the conversation. The
    /// options arrive as a markdown list, not a run-on line.
    ///
    /// Pure function of its inputs (invariant 4): the wall-clock stamp
    /// renders in the passed zone, and `target` is named only when the
    /// compaction retarget actually resolved a model — an unarmed proxy
    /// promising a cheap compaction would be the feature lying about its
    /// own configuration.
    ///
    /// This is the notice for a backend whose quota is a rate-limit window
    /// (see [`ColdBlocking::notice_for`] for one that is not).
    pub fn notice(
        idle_ms: i64,
        prompt: u64,
        target: Option<&str>,
        outlook: Option<&Outlook>,
        at_ms: i64,
        tz: &TimeZone,
        style: NoticeStyle,
    ) -> String {
        Self::notice_for(None, idle_ms, prompt, target, outlook, at_ms, tz, style)
    }

    /// [`ColdBlocking::notice`], with the cost of the re-read stated where
    /// it is known: `write_multiplier` is what a cache write costs against
    /// plain input on this lane's tier (2 for the 1-hour tier, 1.25 for
    /// the 5-minute one), and an unknown one is left out rather than
    /// guessed.
    #[allow(clippy::too_many_arguments)]
    pub fn notice_for(
        write_multiplier: Option<f64>,
        idle_ms: i64,
        prompt: u64,
        target: Option<&str>,
        outlook: Option<&Outlook>,
        at_ms: i64,
        tz: &TimeZone,
        style: NoticeStyle,
    ) -> String {
        let stamp = at_scale(at_ms as f64, tz, 0);
        let quota = outlook_line(outlook, at_ms, tz);
        let cheap = target
            .map(|target| format!(" The proxy would run it on {target}."))
            .unwrap_or_default();
        let cost = write_multiplier
            .filter(|multiplier| multiplier.is_finite() && *multiplier > 0.0)
            .map(|multiplier| format!(" (\u{d7}{} usage)", trim_float(multiplier)))
            .unwrap_or_default();
        // What the reader can learn by doing (that the message is still
        // there, that this shows once per idle spell) is left out: said
        // here it only made the notice harder to parse.
        let mut lines: Vec<String> = vec![
            format!(
                "Paused by {TOKER_LINK} at {stamp}: the prompt cache expired after {} \
                 idle, so the next message re-reads {} tokens into the prompt \
                 cache{cost}. You can:",
                human_idle(idle_ms),
                group(prompt),
            ),
            format!("- `/compact`: pay the re-read once for a small prefix.{cheap}"),
            "- Start a new session: pay nothing, keep none of this context.".to_owned(),
            "- Reply: carry on and pay the re-read.".to_owned(),
        ];
        if let Some(quota) = quota {
            // Markdown would fold an unseparated line into the last item.
            lines.push(String::new());
            lines.push(quota);
        }
        // A Warning: advice the operator may act on or ignore.
        render(style, NoticeLevel::Warning, &lines.join("\n"))
    }

    /// The synthetic assistant turn id on the openai wire, so a reader of
    /// the client's transcript or the ledger can tell a proxy answer
    /// from a provider one (the openai-chat spelling of the quota gate's
    /// [`SYNTHETIC_ID`-shaped marker](crate::middleware::quota::Blocking)).
    const OPENAI_SYNTHETIC_ID: &str = "chatcmpl-toker-cold";

    /// The synthetic turn's model when the request named none — a
    /// stand-in the client renders like any other (the id says toker; a
    /// chat body without a model never reaches the gate, which needs a
    /// parsed shape to key the lane).
    const OPENAI_DEFAULT_MODEL: &str = "z-ai/glm-5.3";

    /// The same notice as an OpenAI-chat turn — the openai wire's answer
    /// to the anthropic [`blocked_turn`](crate::middleware::quota::Blocking::blocked_turn):
    /// non-stream a chat-completion JSON with one assistant message,
    /// stream a single content delta then `data: [DONE]`. Usage is zeroed
    /// (nothing reached upstream, and a synthetic turn that claimed
    /// tokens would be counted by every view that reads the ledger).
    ///
    /// Pure function of its inputs like [`ColdBlocking::notice`]
    /// (invariant 4): `created` is the caller's clock, an input here for
    /// the same reason the notice's stamp is — the outer envelope never
    /// enters replayed history (the client keeps the rendered content,
    /// not the wire bytes), but the bytes are still a function of the
    /// arguments alone.
    pub fn openai_turn(text: &str, model: Option<&str>, stream: bool, now_ms: i64) -> Vec<u8> {
        let model = model
            .filter(|model| !model.is_empty())
            .unwrap_or(Self::OPENAI_DEFAULT_MODEL);
        let created = (now_ms / 1000).max(0);
        if !stream {
            return serde_json::to_vec(&serde_json::json!({
                "id": Self::OPENAI_SYNTHETIC_ID,
                "object": "chat.completion",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": text},
                    "finish_reason": "stop",
                }],
                "usage": {
                    "prompt_tokens": 0,
                    "completion_tokens": 0,
                    "total_tokens": 0,
                },
            }))
            .expect("a json! object always serialises");
        }
        // One content delta, then the terminator — the minimal stream a
        // chat client renders (the dialect of the captured fixtures:
        // `data: {chunk}` blocks, blank-line separated, `[DONE]` last).
        let chunk = serde_json::to_string(&serde_json::json!({
            "id": Self::OPENAI_SYNTHETIC_ID,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": text},
                "finish_reason": null,
            }],
        }))
        .expect("a json! object always serialises");
        format!("data: {chunk}\n\ndata: [DONE]\n").into_bytes()
    }
}

// ── the compaction retarget ──────────────────────────────────────────

/// The prefix of the merged block: Sonnet 5 does not
/// accept mid-conversation `role: "system"` entries in `messages[]`, and
/// Claude Code emits them routinely. Model-visible text, so it is the
/// predecessor's `[system]`, which says what the block was. A port once
/// wrote `[PROMPT_INJECTION]` here, labelling the client's own
/// instructions as an injection for the model to distrust.
pub(crate) const MERGED_SYSTEM: &str = "[system]";

/// What one retarget did. `to == from` is a same-model strip: a real
/// transform (the breakpoints went) but not a downgrade — recording one
/// would put a model in `downgradedFrom` that also served the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetargetOutcome {
    /// The model the request arrived on (`None` when the body named none).
    pub from: Option<String>,
    /// The model the request leaves on (`to || from`).
    pub to: Option<String>,
    /// How many mid-conversation system messages were merged.
    pub merged: u64,
    /// How many cache_control breakpoints were dropped, anywhere in the
    /// body — the difference between a rewrite that saved something and
    /// one that had nothing to save.
    pub stripped: u64,
}

/// `target` if it is strictly cheaper to prompt than `from`, else `None`
/// (judged on the published input rate
/// rather than a hand-kept ordering, so a new model needs no edit here).
/// Both sides are priced as the identity the backend's model map will
/// actually send (the predecessor's `priceAs`): routing policy is not
/// price evidence, and an alias that maps onto something dearer must not
/// be called cheaper because of its own name. An identity that cannot be
/// priced cannot be judged. Never sideways, never upward: that would buy
/// nothing and cost the quality difference.
pub(crate) fn cheaper_of(
    from: Option<&str>,
    target: Option<&str>,
    map: Option<&ModelMap>,
) -> Option<String> {
    let target = target?;
    let from = from?;
    let target_input = price(preview_mapped_model(map, target)?, false, None)?
        .rates
        .input;
    let from_input = price(preview_mapped_model(map, from)?, false, None)?
        .rates
        .input;
    (target_input < from_input).then(|| target.to_owned())
}

/// A message's content as a block array, whatever shape it arrived in:
/// string content synthesises one text
/// block; anything but a string or an array is not convertible.
fn blocks_of(message: &Value) -> Option<Vec<Value>> {
    match message.get("content") {
        Some(Value::String(text)) => Some(vec![serde_json::json!({
            "type": "text",
            "text": text,
        })]),
        Some(Value::Array(parts)) => Some(parts.clone()),
        _ => None,
    }
}

/// Strip every cache_control breakpoint, anywhere in the tree, and report
/// how many went.
fn drop_cache_control(node: &mut Value) -> u64 {
    match node {
        Value::Array(parts) => parts.iter_mut().map(drop_cache_control).sum(),
        Value::Object(map) => {
            let mut count = 0;
            if map.remove("cache_control").is_some() {
                count += 1;
            }
            for (_, value) in map.iter_mut() {
                count += drop_cache_control(value);
            }
            count
        }
        _ => 0,
    }
}

/// JS `trim` (the merged text must survive it): Unicode White_Space plus
/// the BOM, which `char::is_whitespace` alone does not cover.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

/// Rewrite a cold compaction onto a cheaper model, or decline (ported
/// exactly, over the IR
/// instead of raw bytes: toker's serialisation purity makes the
/// predecessor's byte-splicing unnecessary, since the IR round-trip is
/// byte-exact and
/// the re-serialised transform is deterministic).
///
/// This is the only transform that changes model-visible prompt structure,
/// and it is confined to one request shape for a reason: a compaction is
/// a dead end — its output is a summary, the body is never replayed, and
/// the client's transcript is untouched — so every usual reason for
/// leaving bytes alone is absent here, and only here.
///
/// `map` is the routed backend's model map: "cheaper" is judged on what
/// the map will send, not on the names in the body.
///
/// Three things happen together, or none do:
///
/// - the model changes to `target`, where one is offered and is cheaper;
/// - every cache_control goes, because a write against a prefix nothing
///   will read back is bought once and read never — and the 1-hour write
///   tier, which is what Claude Code uses, costs twice plain input;
/// - mid-conversation system messages are merged into the **preceding**
///   user turn, prefixed `[system]`, because Sonnet 5 does not
///   accept them inside `messages[]`. Merging rather than converting adds
///   a block to a message that already exists, so the role sequence the
///   API sees is unchanged.
///
/// **Dropping the breakpoints needs a licence, and there are exactly
/// two.** A model change is one: caches are keyed per model, so there is
/// provably no cache for the new one to lose. `cold: true` is the other —
/// the caller asserting the lane's cache has expired, so there is nothing
/// to read and the write would be bought for a body that is never
/// replayed. Without one of the two this declines, because on a WARM lane
/// the breakpoints are what earn the free read.
///
/// Anything unexpected declines the whole rewrite: a partial
/// transformation is how a request gets corrupted, and the failure would
/// land on the compaction the user was just advised to run. The request is
/// left untouched when that happens — all-or-nothing, by construction:
/// the transform is built over a copy and committed only on success.
pub fn retarget_compaction(
    ir: &mut Request,
    target: Option<&str>,
    cold: bool,
    map: Option<&ModelMap>,
) -> Option<RetargetOutcome> {
    let original = ir.value();
    if !original.is_object() {
        return None;
    }
    let messages = original.get("messages")?.as_array()?;
    if messages.is_empty() {
        return None;
    }

    let from = original
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let to = cheaper_of(from.as_deref(), target, map);
    // No cheaper model to move to and no assertion that the cache is gone
    // leaves nothing that justifies touching the body.
    if to.is_none() && !cold {
        return None;
    }

    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut merged = 0u64;
    for message in messages {
        if !message.is_object() {
            return None;
        }
        if message.get("role").and_then(Value::as_str) != Some("system") {
            out.push(message.clone());
            continue;
        }
        // Merge into the PRECEDING user turn, or decline: a system message
        // can also legally follow an assistant turn that ends in
        // server-tool use, and there is nowhere safe to put it then, so
        // the rewrite is abandoned rather than guessed at.
        let host = out.last()?;
        if host.get("role").and_then(Value::as_str) != Some("user") {
            return None;
        }
        let host_blocks = blocks_of(host)?;
        let own_blocks = blocks_of(message)?;
        let joined = own_blocks
            .iter()
            .map(|block| {
                block
                    .as_object()
                    .and_then(|block| block.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = js_trim(&joined);
        if text.is_empty() {
            return None;
        }
        let mut new_host = host.clone();
        let mut content = host_blocks;
        content.push(serde_json::json!({
            "type": "text",
            "text": format!("{MERGED_SYSTEM} {text}"),
        }));
        if let Some(map) = new_host.as_object_mut() {
            map.insert("content".to_owned(), Value::Array(content));
        }
        out.pop();
        out.push(new_host);
        merged += 1;
    }

    // The commit: the whole transform was built above without touching the
    // request, so anything that declined left it untouched. `to || from`
    // with neither present removes the key — a serialised absent model is
    // a body with no `model` at all.
    let resolved = to.clone().or_else(|| from.clone());
    let mut new_value = original.clone();
    let map = new_value.as_object_mut()?;
    match resolved.clone() {
        Some(model) => {
            map.insert("model".to_owned(), Value::String(model));
        }
        None => {
            map.remove("model");
        }
    }
    map.insert("messages".to_owned(), Value::Array(out));
    let stripped = drop_cache_control(&mut new_value);
    ir.replace_value(new_value);
    Some(RetargetOutcome {
        from,
        to: resolved,
        merged,
        stripped,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Burn, ColdBlocking, ColdDecision, DEFAULT_MIN_TOKENS, METER_5H, Outlook, RECAP_HELD,
        RetargetOutcome, Turn, Verdict, burn_rate, burn_rate_samples, cheaper_of, coldness,
        decide_cold, fit_quota_model, human_idle, lane_is_cold, outlook_of, outlook_over,
        outlook_target, project_to, quota_outlook, retarget_compaction, ttl_of,
    };
    use crate::ir::Request;
    use crate::middleware::notice::NoticeStyle;
    use crate::middleware::quota::Meter;
    use crate::store::{Lane, RequestRow};
    use serde_json::{Value, json};

    /// `now` for the synthetic fixtures — the vendored contract's own
    /// epoch-millisecond convention, so every span below is readable.
    const NOW: i64 = 2_000_000_000_000;
    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;

    fn utc() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get("UTC").expect("UTC is always present in the tzdb")
    }

    fn mem_store() -> crate::store::Store {
        crate::store::Store::open(":memory:").expect("open in-memory store")
    }

    /// A row with only `ts_ms` — every other column NULL (the sibling
    /// modules' shape, local copy for brevity).
    fn bare_row(ts_ms: i64) -> RequestRow {
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

    /// A response row: served on `model`, `fresh` input tokens, this
    /// window's utilisation reading attached.
    fn served_row(
        ts_ms: i64,
        model: &str,
        fresh: i64,
        util: Option<f64>,
        reset_s: Option<i64>,
    ) -> RequestRow {
        let mut row = bare_row(ts_ms);
        row.model = Some(model.to_owned());
        row.input = Some(fresh);
        if let Some(util) = util {
            let mut limits = json!({"util5h": util});
            if let Some(reset) = reset_s {
                limits["reset5h"] = json!(reset);
            }
            row.rate_limits = Some(limits);
        }
        row
    }

    fn lane(at_ms: i64, prompt: i64, ttl: Option<i64>) -> Lane {
        Lane {
            key: "ses-1|sha256:t1".to_owned(),
            session_id: Some("ses-1".to_owned()),
            tools_hash: Some("sha256:t1".to_owned()),
            updated_ms: at_ms,
            prompt_tokens: Some(prompt),
            ttl,
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
        }
    }

    /// The synthetic ledger the outlook tests pin (the rule set made
    /// checkable): four complete 5-hour windows the weight fit can price —
    /// each 21 rows (the row the window opens on, plus the 20 body rows
    /// the fit measures) with utilisation advancing 0.10 → 0.40, all
    /// claude-opus-5 at 50k fresh tokens a row — plus the two burn
    /// readings of the window now running (0.50 → 0.52, resetting 3.5h
    /// out, opened 1.5h ago).
    fn fit_rows(now: i64) -> Vec<RequestRow> {
        let mut rows = Vec::new();
        let window_span = 5 * HOUR;
        for window in 0..4i64 {
            let reset_ms = now - 90 * MIN - 300 * MIN * window;
            let reset_s = reset_ms / 1000;
            // The row the window opens on anchors the advance; its own
            // usage is already in the utilisation it reports.
            rows.push(served_row(
                reset_ms - window_span + 10 * MIN,
                "claude-opus-5",
                50_000,
                Some(0.10),
                Some(reset_s),
            ));
            for step in 1..=20 {
                let at = reset_ms - window_span + 10 * MIN + (step * (window_span - 20 * MIN)) / 20;
                let util = 0.10 + 0.30 * step as f64 / 20.0;
                rows.push(served_row(
                    at,
                    "claude-opus-5",
                    50_000,
                    Some(util),
                    Some(reset_s),
                ));
            }
        }
        // The window now running: two readings 20 minutes apart, moving
        // one quantum step (0.02) — too small a window for the fit, exactly
        // enough for the burn's shortest clearing rung.
        let reset_s = (now + 210 * MIN) / 1000;
        rows.push(served_row(
            now - 20 * MIN,
            "claude-opus-5",
            1,
            Some(0.50),
            Some(reset_s),
        ));
        rows.push(served_row(
            now - 2_000,
            "claude-opus-5",
            1,
            Some(0.52),
            Some(reset_s),
        ));
        rows
    }

    // ── the coldness measurement ─────────────────────────────────────

    #[test]
    fn the_min_tokens_bar_is_measured_and_strict() {
        assert_eq!(DEFAULT_MIN_TOKENS, 175_000, "the measured bar, verbatim");
        // At the bar exactly: a rebuild this cheap is not worth the
        // interruption (`prompt <= minTokens`).
        assert_eq!(
            coldness(
                Some(&lane(NOW - 2 * HOUR, 175_000, None)),
                DEFAULT_MIN_TOKENS,
                None,
                NOW
            ),
            None
        );
        // One token over: cold.
        let measured = coldness(
            Some(&lane(NOW - 2 * HOUR, 175_001, None)),
            DEFAULT_MIN_TOKENS,
            None,
            NOW,
        )
        .expect("one token over the bar is cold");
        assert_eq!(measured.idle_ms, 2 * HOUR);
        assert_eq!(measured.prompt, 175_001);
        // Absence never reads as a measurement (invariant 3).
        assert_eq!(coldness(None, DEFAULT_MIN_TOKENS, None, NOW), None);
        assert_eq!(
            coldness(
                Some(&Lane {
                    prompt_tokens: None,
                    ..lane(NOW, 0, None)
                }),
                DEFAULT_MIN_TOKENS,
                None,
                NOW
            ),
            None,
            "a lane with no recorded prompt cannot be judged"
        );
    }

    #[test]
    fn the_ttl_tier_sets_the_floor_and_absence_is_the_long_tier() {
        assert_eq!(ttl_of(&lane(NOW, 1, Some(300_000))), 5 * MIN);
        assert_eq!(ttl_of(&lane(NOW, 1, Some(3_600_000))), HOUR);
        // The openai lane's provider window is a recognised duration —
        // its own clock, not the unrecognised-value LONG one.
        assert_eq!(
            ttl_of(&lane(NOW, 1, Some(600_000))),
            10 * MIN,
            "an openai lane expires on openrouter's sticky window"
        );
        // An unrecorded or unrecognised tier is the LONG one: guessing
        // short would fire on lanes whose cache is still live.
        assert_eq!(ttl_of(&lane(NOW, 1, None)), HOUR);
        assert_eq!(ttl_of(&lane(NOW, 1, Some(123_456))), HOUR);

        // The floor is inclusive: cold AT the TTL, not after it.
        let five_m = lane(NOW - 5 * MIN + 1, 200_000, Some(300_000));
        assert_eq!(
            coldness(Some(&five_m), DEFAULT_MIN_TOKENS, None, NOW),
            None,
            "one ms inside the 5m tier is warm"
        );
        let five_m = lane(NOW - 5 * MIN, 200_000, Some(300_000));
        assert!(coldness(Some(&five_m), DEFAULT_MIN_TOKENS, None, NOW).is_some());
        // The hour tier, both edges.
        let warm = lane(NOW - HOUR + 1, 200_000, Some(3_600_000));
        assert_eq!(coldness(Some(&warm), DEFAULT_MIN_TOKENS, None, NOW), None);
        let cold = lane(NOW - HOUR, 200_000, Some(3_600_000));
        assert!(coldness(Some(&cold), DEFAULT_MIN_TOKENS, None, NOW).is_some());
        // An unrecorded tier is judged as the long one, on both edges.
        let warm = lane(NOW - HOUR + 1, 200_000, None);
        assert_eq!(coldness(Some(&warm), DEFAULT_MIN_TOKENS, None, NOW), None);
        let cold = lane(NOW - HOUR, 200_000, None);
        assert!(coldness(Some(&cold), DEFAULT_MIN_TOKENS, None, NOW).is_some());
    }

    #[test]
    fn a_lane_whose_last_request_is_in_the_future_is_a_moved_clock() {
        assert_eq!(
            coldness(
                Some(&lane(NOW + HOUR, 200_000, None)),
                DEFAULT_MIN_TOKENS,
                None,
                NOW
            ),
            None,
            "negative idle is not idle"
        );
        assert!(!lane_is_cold(
            Some(&lane(NOW + HOUR, 200_000, None)),
            DEFAULT_MIN_TOKENS,
            None,
            NOW
        ));
    }

    #[test]
    fn an_idle_override_replaces_the_tier_floor() {
        // The configured idle floor wins over the tier, in
        // either direction.
        // A 5m-tier lane at 2 minutes: warm by its tier, cold under a
        // 1-minute override.
        let short_tier = lane(NOW - 2 * MIN, 200_000, Some(300_000));
        assert_eq!(
            coldness(Some(&short_tier), DEFAULT_MIN_TOKENS, None, NOW),
            None
        );
        assert!(coldness(Some(&short_tier), DEFAULT_MIN_TOKENS, Some(MIN), NOW).is_some());
        // And the override can delay: a 5m-tier lane at 10 minutes is
        // cold by its tier, warm under a 45-minute override.
        let ten_minutes = lane(NOW - 10 * MIN, 200_000, Some(300_000));
        assert!(coldness(Some(&ten_minutes), DEFAULT_MIN_TOKENS, None, NOW).is_some());
        assert_eq!(
            coldness(Some(&ten_minutes), DEFAULT_MIN_TOKENS, Some(45 * MIN), NOW),
            None
        );
    }

    // ── the decision ────────────────────────────────────────────────

    #[test]
    fn a_recap_on_a_cold_lane_is_held_whether_or_not_the_notice_fired() {
        let cold = lane(NOW - 2 * HOUR, 200_000, None);
        let held = ColdDecision::Recap {
            idle_ms: 2 * HOUR,
            prompt: 200_000,
        };
        assert_eq!(
            decide_cold(
                Some(&cold),
                Turn::Recap,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                None,
                None
            ),
            held
        );
        // Already spoken about this spell: an ordinary turn forwards (the
        // resend is the override), a recap is still held.
        let mut noticed = cold.clone();
        noticed.noticed_at = Some(NOW - 1_000);
        assert_eq!(
            decide_cold(
                Some(&noticed),
                Turn::Ordinary,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                None,
                None
            ),
            ColdDecision::Forward
        );
        assert_eq!(
            decide_cold(
                Some(&noticed),
                Turn::Recap,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                None,
                None
            ),
            held
        );
        // Warm, or too small to matter: a recap forwards like any turn.
        let warm = lane(NOW - 60_000, 200_000, None);
        assert_eq!(
            decide_cold(
                Some(&warm),
                Turn::Recap,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                None,
                None
            ),
            ColdDecision::Forward
        );
        assert_eq!(
            decide_cold(
                Some(&cold),
                Turn::Recap,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                None,
                Some(10)
            ),
            ColdDecision::Forward
        );
    }

    #[test]
    fn the_held_recap_text_is_plain_and_stable() {
        assert!(!RECAP_HELD.contains('\u{2014}'), "no em dash");
        assert!(
            RECAP_HELD.len() < 400,
            "Claude Code caps a recap at 400 chars"
        );
        insta::assert_snapshot!(RECAP_HELD);
    }

    #[test]
    fn a_summarising_request_forwards_even_on_a_cold_lane() {
        // THE compaction-vs-notice answer: the notice exists to advise
        // `/compact`, so it never interrupts one — and the refusal is on
        // the raw summarising flag, which the routine title summariser
        // shares, not on `isCompaction`. The retarget below uses
        // `lane_is_cold` for the cache question instead.
        let cold = lane(NOW - 2 * HOUR, 200_000, None);
        assert_eq!(
            decide_cold(
                Some(&cold),
                Turn::Summarising,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                None,
                None
            ),
            ColdDecision::Forward
        );
        // Without the flag, the same lane notices.
        assert!(matches!(
            decide_cold(
                Some(&cold),
                Turn::Ordinary,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                None,
                None
            ),
            ColdDecision::Notice { .. }
        ));
    }

    #[test]
    fn the_notice_fires_once_per_idle_spell_and_rearms_on_activity() {
        // `noticed_at` at or after `at` = already spoken about this spell.
        let noticed = Lane {
            noticed_at: Some(NOW - HOUR),
            ..lane(NOW - 2 * HOUR, 200_000, None)
        };
        assert_eq!(
            decide_cold(
                Some(&noticed),
                Turn::Ordinary,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                None,
                None
            ),
            ColdDecision::Forward
        );
        // A notice recorded BEFORE the lane was last active belongs to an
        // earlier spell: the lane spoke again since, so it re-arms.
        let re_armed = Lane {
            noticed_at: Some(NOW - 3 * HOUR),
            ..lane(NOW - 2 * HOUR, 200_000, None)
        };
        assert!(matches!(
            decide_cold(
                Some(&re_armed),
                Turn::Ordinary,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                None,
                None
            ),
            ColdDecision::Notice { .. }
        ));
        // No notice recorded: fires, carrying no outlook.
        assert_eq!(
            decide_cold(
                Some(&lane(NOW - 2 * HOUR, 200_000, None)),
                Turn::Ordinary,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                None,
                None
            ),
            ColdDecision::Notice {
                idle_ms: 2 * HOUR,
                prompt: 200_000,
                outlook: None
            }
        );

        // The serving side: after a notice, the lane's noticed_at is at/after
        // `at`, so the resend forwards — sending the request again IS the
        // override.
        let store = mem_store();
        store
            .upsert_lane(&lane(NOW - 2 * HOUR, 200_000, None))
            .expect("upsert");
        super::note_lane_notice(&store, &lane(NOW, 0, None).key, NOW).expect("mark");
        let marked = store
            .load_lane(&lane(NOW, 0, None).key)
            .expect("load")
            .expect("lane");
        assert_eq!(marked.noticed_at, Some(NOW));
        assert_eq!(
            decide_cold(
                Some(&marked),
                Turn::Ordinary,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                None,
                None
            ),
            ColdDecision::Forward
        );
        // note_lane_notice on an unknown key is a no-op, not an invention.
        super::note_lane_notice(&store, "no|such", NOW).expect("mark");
        assert_eq!(store.load_lane("no|such").expect("load"), None);
    }

    #[test]
    fn an_on_track_outlook_withholds_a_would_fire_notice() {
        let rows = fit_rows(NOW);
        // The burn math pinned: the current window measured on the
        // shortest rung that clears the quantisation floor (the two
        // readings 20 minutes apart, NOT the 1.5h-old zero anchor), and
        // the projection says the window resets long before the gate's
        // threshold.
        let burn = burn_rate(&rows, &METER_5H, NOW);
        match &burn {
            Burn::Measured {
                util,
                rate,
                delta,
                span_ms,
                anchored,
                ..
            } => {
                assert!((util - 0.52).abs() < 1e-9, "{util}");
                assert!((delta - 0.02).abs() < 1e-9, "{delta}");
                assert!(*span_ms >= 20 * MIN && *span_ms < 21 * MIN, "{span_ms}");
                assert!(*anchored, "the window opened 1.5h ago, inside the lookback");
                let _ = rate;
            }
            other => panic!("expected a measured burn, got {other:?}"),
        }
        // The fit prices the re-read at (fresh/1e6) × 0.30 — utilisation
        // advanced 0.30 per 1.0M fresh tokens, four consistent windows.
        let fit = fit_quota_model(&rows);
        assert!(fit.ok, "{:?}", fit.reason);
        let (extra, bound) = fit
            .quota_for("claude-opus-5", 200_000)
            .expect("the log's only group is priced");
        assert!((extra - 0.06).abs() < 1e-9, "{extra}");
        assert!(!bound);

        let outlook = outlook_of(&rows, Some("claude-opus-5"), 200_000, true, NOW)
            .expect("a fitted log and a live window produce an outlook");
        assert!(outlook.known && outlook.on_track, "{outlook:?}");
        assert_eq!(outlook.meter, None);
        assert!((outlook.util.unwrap() - 0.52).abs() < 1e-9);
        assert!((outlook.extra.unwrap() - 0.06).abs() < 1e-9);

        // Without the outlook the notice fires; with it, quiet.
        let cold = lane(NOW - 2 * HOUR, 200_000, None);
        assert!(matches!(
            decide_cold(
                Some(&cold),
                Turn::Ordinary,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                None,
                None
            ),
            ColdDecision::Notice { .. }
        ));
        assert_eq!(
            decide_cold(
                Some(&cold),
                Turn::Ordinary,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                Some(&outlook),
                None
            ),
            ColdDecision::Quiet {
                idle_ms: 2 * HOUR,
                prompt: 200_000,
                outlook: outlook.clone()
            }
        );
    }

    #[test]
    fn an_off_track_or_unknown_outlook_rides_the_notice_instead() {
        let cold = lane(NOW - 2 * HOUR, 200_000, None);
        // Off-track: the notice fires and names the wall it carries.
        let off_track = Outlook {
            known: true,
            on_track: false,
            meter: Some(Meter::FiveHour),
            extra: Some(0.06),
            bound: Some(false),
            util: Some(0.95),
            reset_at_ms: Some(NOW + 2 * HOUR),
            wall_at_ms: Some(NOW as f64 + 45.0 * MIN as f64),
            pulled_in_ms: Some(15.0 * MIN as f64),
        };
        assert_eq!(
            decide_cold(
                Some(&cold),
                Turn::Ordinary,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                Some(&off_track),
                None
            ),
            ColdDecision::Notice {
                idle_ms: 2 * HOUR,
                prompt: 200_000,
                outlook: Some(off_track)
            }
        );
        // Unknown: never suppresses — a withheld notice must be less
        // useful, never silently wrong.
        let unknown = Outlook::unknown();
        assert!(matches!(
            decide_cold(
                Some(&cold),
                Turn::Ordinary,
                DEFAULT_MIN_TOKENS,
                None,
                NOW,
                Some(&unknown),
                None
            ),
            ColdDecision::Notice { .. }
        ));
    }

    // ── the burn ladder ─────────────────────────────────────────────

    #[test]
    fn the_samples_entry_is_the_same_ladder_as_the_rows_entry() {
        // burn_rate = extract + burn_rate_samples; the TUI's quota panel
        // feeds the samples entry from the narrow meter row, so the two
        // must be the same ladder over the same readings or the panel
        // and the cold outlook disagree about the same window.
        let rows = vec![
            served_row(
                NOW - 4 * HOUR,
                "claude-opus-5",
                1,
                Some(0.10),
                Some((NOW + HOUR) / 1000),
            ),
            bare_row(NOW - 3 * HOUR), // no meters: no sample either way
            served_row(
                NOW - 2 * HOUR,
                "claude-opus-5",
                1,
                Some(0.30),
                Some((NOW + HOUR) / 1000),
            ),
            served_row(
                NOW - MIN,
                "claude-opus-5",
                1,
                Some(0.52),
                Some((NOW + HOUR) / 1000),
            ),
        ];
        let samples = super::samples_of(&rows, &METER_5H);
        assert_eq!(samples.len(), 3, "the meter-less row yields no sample");
        assert_eq!(
            burn_rate_samples(&samples, &METER_5H, NOW),
            burn_rate(&rows, &METER_5H, NOW),
            "both entries are the same ladder over the same samples"
        );
    }

    #[test]
    fn burn_rate_states_none_stale_insufficient_and_the_anchor() {
        // No readings at all.
        assert_eq!(burn_rate(&[], &METER_5H, NOW), Burn::None);
        assert_eq!(
            burn_rate(&[bare_row(NOW)], &METER_5H, NOW),
            Burn::None,
            "a row without meters is no reading"
        );

        // A rolled window: the last reading describes a window that no
        // longer exists.
        let stale = vec![served_row(
            NOW - MIN,
            "claude-opus-5",
            1,
            Some(0.42),
            Some((NOW - 5 * MIN) / 1000),
        )];
        assert_eq!(
            burn_rate(&stale, &METER_5H, NOW),
            Burn::Stale {
                util: 0.42,
                reset_s: (NOW - 5 * MIN) / 1000,
                observed_at: NOW - MIN
            }
        );

        // A single reading whose window's zero-reading anchor cannot be
        // reconstructed (it opened in the future relative to a reset more
        // than a window-length out): too little to say anything.
        let lone = vec![served_row(
            NOW - MIN,
            "claude-opus-5",
            1,
            Some(0.42),
            Some((NOW + 6 * HOUR) / 1000),
        )];
        assert!(matches!(
            burn_rate(&lone, &METER_5H, NOW),
            Burn::Insufficient {
                util: 0.42,
                samples: 1,
                anchored: false,
                ..
            }
        ));

        // The anchor: a window that opened inside the lookback contributes
        // its opening zero, so a lone reading at 0.42 over an opened window
        // measures 0.42 of burn.
        let anchored = vec![served_row(
            NOW - MIN,
            "claude-opus-5",
            1,
            Some(0.42),
            Some((NOW + 4 * HOUR + 30 * MIN) / 1000),
        )];
        match burn_rate(&anchored, &METER_5H, NOW) {
            Burn::Measured {
                util,
                delta,
                anchored: true,
                ..
            } => {
                assert!((util - 0.42).abs() < 1e-9);
                assert!(
                    (delta - 0.42).abs() < 1e-9,
                    "the anchor's zero is the baseline"
                );
            }
            other => panic!("expected an anchored measurement, got {other:?}"),
        }
    }

    #[test]
    fn the_shortest_clearing_rung_wins_not_the_widest() {
        // 0.10 at 90 minutes out, 0.30 at 45, 0.32 at 10; the window
        // opened 2 hours ago (the anchor's zero rides below them all).
        let rows = vec![
            served_row(
                NOW - 90 * MIN,
                "claude-opus-5",
                1,
                Some(0.10),
                Some((NOW + 3 * HOUR) / 1000),
            ),
            served_row(
                NOW - 45 * MIN,
                "claude-opus-5",
                1,
                Some(0.30),
                Some((NOW + 3 * HOUR) / 1000),
            ),
            served_row(
                NOW - 10 * MIN,
                "claude-opus-5",
                1,
                Some(0.32),
                Some((NOW + 3 * HOUR) / 1000),
            ),
        ];
        match burn_rate(&rows, &METER_5H, NOW) {
            // The 30m and 15m rungs hold only the latest reading; the 60m
            // rung is the shortest whose baseline differs — 0.02 over 45
            // minutes, not 0.22 over 90 or 0.32 over the 2h anchor.
            Burn::Measured { delta, span_ms, .. } => {
                assert!((delta - 0.02).abs() < 1e-9, "{delta}");
                assert_eq!(
                    span_ms,
                    45 * MIN,
                    "the span MEASURED, not the rung asked for"
                );
            }
            other => panic!("expected the shortest clearing rung, got {other:?}"),
        }
    }

    #[test]
    fn a_window_that_did_not_move_gives_a_ceiling_not_a_rate() {
        let rows = vec![
            served_row(
                NOW - 2 * HOUR,
                "claude-opus-5",
                1,
                Some(0.30),
                Some((NOW + 3 * HOUR) / 1000),
            ),
            served_row(
                NOW - 10 * MIN,
                "claude-opus-5",
                1,
                Some(0.30),
                Some((NOW + 3 * HOUR) / 1000),
            ),
        ];
        match burn_rate(&rows, &METER_5H, NOW) {
            Burn::Bounded {
                util,
                rate_max,
                delta,
                ..
            } => {
                assert!((util - 0.30).abs() < 1e-9);
                assert!((delta - 0.0).abs() < 1e-12);
                // max(delta, 0) + one quantum of rounding, over the span.
                assert!(
                    (rate_max - 0.01 / (2.0 * HOUR as f64)).abs() < 1e-12,
                    "{rate_max}"
                );
            }
            other => panic!("expected a bounded burn, got {other:?}"),
        }
        // The ceiling settles the question only when even the earliest
        // arrival lands after the reset.
        let bounded = burn_rate(&rows, &METER_5H, NOW);
        assert_eq!(
            project_to(&bounded, 0.31, NOW),
            Verdict::Unknown,
            "the bound cannot separate the two"
        );
        assert_eq!(project_to(&bounded, 0.99, NOW), Verdict::OnTrack);
        // And the flat window's util is past nothing.
        assert_eq!(project_to(&bounded, 0.30, NOW), Verdict::Reached);
    }

    #[test]
    fn a_held_fall_inside_the_window_is_a_restart_not_jitter() {
        // 0.50, then a fall to 0.20 that never comes back and has outlasted
        // reordering: the window restarted at 0.20, and the burn is
        // measured from there — not from the pre-fall 0.50, which a
        // naive span would report as the baseline and read the meter as
        // refilling.
        let rows = vec![
            served_row(
                NOW - 40 * MIN,
                "claude-opus-5",
                1,
                Some(0.50),
                Some((NOW + 3 * HOUR) / 1000),
            ),
            served_row(
                NOW - 25 * MIN,
                "claude-opus-5",
                1,
                Some(0.20),
                Some((NOW + 3 * HOUR) / 1000),
            ),
            served_row(
                NOW - 10 * MIN,
                "claude-opus-5",
                1,
                Some(0.20),
                Some((NOW + 3 * HOUR) / 1000),
            ),
        ];
        match burn_rate(&rows, &METER_5H, NOW) {
            Burn::Bounded { util, samples, .. } => {
                assert!(
                    (util - 0.20).abs() < 1e-9,
                    "{util}: the fall was detected as a restart"
                );
                assert_eq!(samples, 2, "the pre-fall reading is out of the window");
            }
            other => panic!("expected the restarted window, got {other:?}"),
        }

        // A fall that has NOT outlasted reordering is jitter: the same
        // fall ten minutes from the end is a late-arriving reading, and
        // the envelope keeps the higher figure (the safe direction).
        let rows = vec![
            served_row(
                NOW - 40 * MIN,
                "claude-opus-5",
                1,
                Some(0.50),
                Some((NOW + 3 * HOUR) / 1000),
            ),
            served_row(
                NOW - 9 * MIN,
                "claude-opus-5",
                1,
                Some(0.20),
                Some((NOW + 3 * HOUR) / 1000),
            ),
            served_row(
                NOW - 5 * MIN,
                "claude-opus-5",
                1,
                Some(0.20),
                Some((NOW + 3 * HOUR) / 1000),
            ),
        ];
        match burn_rate(&rows, &METER_5H, NOW) {
            Burn::Measured { util, .. } | Burn::Bounded { util, .. } => {
                assert!(
                    (util - 0.50).abs() < 1e-9,
                    "{util}: jitter never restarts the window"
                );
            }
            other => panic!("expected a live burn, got {other:?}"),
        }
    }

    #[test]
    fn project_to_walks_the_verdicts() {
        assert_eq!(project_to(&Burn::None, 1.0, NOW), Verdict::Unknown);
        let insufficient = Burn::Insufficient {
            util: 0.5,
            reset_s: 0,
            observed_at: NOW,
            samples: 1,
            anchored: false,
        };
        assert_eq!(project_to(&insufficient, 1.0, NOW), Verdict::Unknown);
        let stale = Burn::Stale {
            util: 0.5,
            reset_s: 0,
            observed_at: NOW,
        };
        assert_eq!(project_to(&stale, 1.0, NOW), Verdict::Stale);

        // A rate of zero cannot place the wall anywhere.
        let flat = Burn::Measured {
            util: 0.5,
            reset_s: (NOW + HOUR) / 1000,
            observed_at: NOW,
            samples: 2,
            anchored: false,
            rate: 0.0,
            delta: 0.0,
            span_ms: HOUR,
        };
        assert_eq!(project_to(&flat, 1.0, NOW), Verdict::Unknown);

        // Runout: 0.47 remaining at 0.01/ms puts the wall 47 minutes out,
        // before a reset an hour away.
        let burning = Burn::Measured {
            util: 0.53,
            reset_s: (NOW + HOUR) / 1000,
            observed_at: NOW,
            samples: 2,
            anchored: false,
            rate: 0.01 / MIN as f64,
            delta: 0.47,
            span_ms: 47 * MIN,
        };
        assert_eq!(
            project_to(&burning, 1.0, NOW),
            Verdict::Runout {
                at_ms: NOW as f64 + 47.0 * MIN as f64
            }
        );
        // Slower: the window resets first.
        let calm = Burn::Measured {
            rate: 0.002 / MIN as f64,
            util: 0.53,
            reset_s: (NOW + HOUR) / 1000,
            observed_at: NOW,
            samples: 2,
            anchored: false,
            delta: 0.47,
            span_ms: 47 * MIN,
        };
        assert_eq!(project_to(&calm, 1.0, NOW), Verdict::OnTrack);
        // Already past the target.
        let spent = Burn::Measured {
            util: 1.0,
            reset_s: (NOW + HOUR) / 1000,
            observed_at: NOW,
            samples: 2,
            anchored: false,
            rate: 0.002 / MIN as f64,
            delta: 0.47,
            span_ms: 47 * MIN,
        };
        assert_eq!(project_to(&spent, 0.99, NOW), Verdict::Reached);
    }

    #[test]
    fn quota_outlook_answers_unknown_reached_runout_and_quiet() {
        // No burn at all, or a negative weight: unknown, never a guess.
        assert!(!quota_outlook(&Burn::None, None, 0.02, false, 0.99, NOW).known);
        assert!(!quota_outlook(&Burn::None, None, -0.02, false, 0.99, NOW).known);

        // Already spent: known, off-track, no wall to name.
        let spent = Burn::Measured {
            util: 0.995,
            reset_s: (NOW + HOUR) / 1000,
            observed_at: NOW,
            samples: 2,
            anchored: false,
            rate: 0.01 / MIN as f64,
            delta: 0.4,
            span_ms: 40 * MIN,
        };
        let o = quota_outlook(&spent, None, 0.02, false, 0.99, NOW);
        assert!(o.known && !o.on_track);
        assert_eq!(o.meter, Some(Meter::FiveHour));
        assert_eq!(o.wall_at_ms, None);

        // The runout case, with the re-read's share pulling the wall in:
        // 0.95 util, burning 0.02 per 30 minutes, reset in two hours.
        // Plain: 0.04 remaining → wall in 60 minutes. With the 0.03
        // re-read: 0.01 remaining → wall in 15 minutes, pulled in 45.
        let burning = Burn::Measured {
            util: 0.95,
            reset_s: (NOW + 2 * HOUR) / 1000,
            observed_at: NOW,
            samples: 2,
            anchored: false,
            rate: 0.02 / (30 * MIN) as f64,
            delta: 0.30,
            span_ms: 30 * MIN,
        };
        let o = quota_outlook(&burning, None, 0.03, false, 0.99, NOW);
        assert!(o.known && !o.on_track);
        assert_eq!(o.meter, Some(Meter::FiveHour));
        assert!(
            (o.wall_at_ms.unwrap() - (NOW + 15 * MIN) as f64).abs() < 1e-6,
            "{:?}",
            o.wall_at_ms
        );
        assert!((o.pulled_in_ms.unwrap() - 45.0 * MIN as f64).abs() < 1e-6);
        // The util the decision rested on is the measured one, not bumped.
        assert!((o.util.unwrap() - 0.95).abs() < 1e-9);

        // On track: the weekly meter has nothing to veto and the 5-hour
        // window resets first even with the re-read added.
        let calm = Burn::Measured {
            util: 0.30,
            reset_s: (NOW + 3 * HOUR) / 1000,
            observed_at: NOW,
            samples: 2,
            anchored: false,
            rate: 0.02 / HOUR as f64,
            delta: 0.10,
            span_ms: 5 * HOUR,
        };
        let o = quota_outlook(&calm, None, 0.02, false, 0.99, NOW);
        assert!(o.known && o.on_track, "{o:?}");
        assert_eq!(o.meter, None, "nothing vetoed the suppression");
        assert_eq!(o.reset_at_ms, None);
        assert!((o.util.unwrap() - 0.30).abs() < 1e-9);

        // The weekly veto: a 5-hour window that can absorb the re-read is
        // still not quiet while the 7-day meter is heading for its own
        // wall — and the figures become the weekly ones.
        let week_burning = Burn::Measured {
            util: 0.80,
            reset_s: (NOW + 24 * HOUR) / 1000,
            observed_at: NOW,
            samples: 3,
            anchored: false,
            rate: 0.05 / HOUR as f64,
            delta: 0.60,
            span_ms: 12 * HOUR,
        };
        let o = quota_outlook(&calm, Some(&week_burning), 0.02, true, 0.99, NOW);
        assert!(o.known && !o.on_track, "{o:?}");
        assert_eq!(o.meter, Some(Meter::SevenDay));
        assert!((o.util.unwrap() - 0.80).abs() < 1e-9);
        assert!(o.wall_at_ms.is_some(), "the weekly wall is named");
        assert_eq!(
            o.pulled_in_ms, None,
            "no `extra` applies to the weekly meter"
        );
    }

    #[test]
    fn the_projection_target_follows_the_gate() {
        assert_eq!(outlook_target(true), super::super::quota::THRESHOLD);
        assert_eq!(outlook_target(false), 1.0);
    }

    // ── the weight fit ──────────────────────────────────────────────

    #[test]
    fn a_young_log_declines_to_fit_rather_than_guess() {
        // No rows, or too few complete windows: no weights, and the
        // reason says so — never a number without the evidence behind it.
        assert!(!fit_quota_model(&[]).ok);
        assert!(fit_quota_model(&[]).reason.is_some());
        let three_windows = fit_rows(NOW)
            .into_iter()
            .filter(|row| {
                row.rate_limits
                    .as_ref()
                    .and_then(|l| l.get("reset5h"))
                    .and_then(Value::as_i64)
                    != Some((NOW + 210 * MIN) / 1000)
            })
            .take(63)
            .collect::<Vec<_>>();
        assert!(three_windows.len() >= 60, "{}", three_windows.len());
        assert!(
            !fit_quota_model(&three_windows).ok,
            "three windows cannot separate a weight"
        );
    }

    #[test]
    fn every_weight_carries_its_leave_one_out_spread() {
        // Four identical windows: every refit lands on the same weight, so
        // the spread collapses onto the point and is clear of zero.
        let fit = fit_quota_model(&fit_rows(NOW));
        assert!(fit.ok, "{:?}", fit.reason);
        let (_, weight, folded) = fit.groups().next().expect("one group");
        assert!(!folded);
        assert!((weight.fresh - 0.30).abs() < 1e-9, "{weight:?}");
        assert!((weight.fresh_spread.min - 0.30).abs() < 1e-9, "{weight:?}");
        assert!((weight.fresh_spread.max - 0.30).abs() < 1e-9, "{weight:?}");
        assert!(weight.fresh_spread.separated());
    }

    #[test]
    fn a_fresh_weight_the_data_cannot_separate_from_zero_is_no_weight() {
        // Output explains every window exactly; fresh volume varies
        // independently, and one window advanced a little more than its
        // output says, with the most fresh traffic. The full fit hands
        // fresh a small weight to cover that one window — and the refit
        // without it drives fresh to zero. The point weight is the one
        // window's residue, not a measurement.
        let mut rows = Vec::new();
        let window_span = 5 * HOUR;
        for (window, (fresh, output, extra)) in [
            (400_000i64, 40_000i64, 0.0f64),
            (100_000, 60_000, 0.0),
            (250_000, 50_000, 0.0),
            (50_000, 70_000, 0.0),
            (600_000, 45_000, 0.02),
        ]
        .into_iter()
        .enumerate()
        {
            let window = window as i64;
            let reset_ms = NOW - 90 * MIN - 300 * MIN * window;
            let reset_s = reset_ms / 1000;
            let du = 0.30 * (output * 20) as f64 / 1e6 + extra;
            for step in 0..=20i64 {
                let at = reset_ms - window_span + 10 * MIN + step * MIN;
                let mut row = served_row(
                    at,
                    "claude-opus-5",
                    fresh,
                    Some(0.10 + du * step as f64 / 20.0),
                    Some(reset_s),
                );
                row.output = Some(output);
                rows.push(row);
            }
        }
        let fit = fit_quota_model(&rows);
        assert!(fit.ok, "{:?}", fit.reason);
        let (_, weight, _) = fit.groups().next().expect("one group");
        assert!(
            weight.fresh > 0.0,
            "the full fit gives fresh a weight: {weight:?}"
        );
        assert!(weight.fresh_spread.min < 1e-12, "{weight:?}");
        assert!(!weight.fresh_spread.separated());
        assert_eq!(
            fit.quota_for("claude-opus-5", 260_000),
            None,
            "an unseparated weight is unknown, so the outlook is blind and the notice fires"
        );
        assert_eq!(
            outlook_of(&rows, Some("claude-opus-5"), 260_000, true, NOW),
            None
        );
    }

    #[test]
    fn a_separated_weight_prices_at_the_top_of_its_spread() {
        // The cautious end: the live 7-day opus-5-5 fit (0.029, spread
        // 0.014-0.055) priced re-reads at a third of what the meter moved.
        let key = super::price_key("claude-opus-5");
        let fit = super::QuotaFit {
            ok: true,
            reason: None,
            weights: std::collections::BTreeMap::from([(
                key.clone(),
                super::GroupWeight {
                    fresh: 0.029,
                    output: 0.6,
                    fresh_spread: super::Spread {
                        min: 0.014,
                        max: 0.055,
                    },
                    output_spread: super::Spread { min: 0.5, max: 0.7 },
                },
            )]),
            folded: std::collections::BTreeSet::new(),
        };
        let (extra, bound) = fit.quota_for("claude-opus-5", 1_000_000).expect("weighted");
        assert!((extra - 0.055).abs() < 1e-12, "{extra}");
        assert!(!bound);
    }

    #[test]
    fn lockstep_groups_fold_and_borrow_the_host_weight() {
        // Two model groups whose volumes move in exact proportion: their
        // weights cannot be separated, exactly one survives the fit, and
        // the other borrows the survivor's weight — the figure the notice
        // must print as "at most", never as a measurement.
        //
        // Which one survives is arbitrary — the
        // predecessor's solver "picks one arbitrarily and hands it the
        // other's weight
        // too", following its `seen` map's insertion
        // order, where this port follows the price-key sort, so the survivor is haiku here.
        // What is NOT arbitrary is the invariant: the demoted group
        // rides the survivor's weight, and `bound` says so.
        let mut rows = Vec::new();
        let window_span = 5 * HOUR;
        for window in 0..4i64 {
            let reset_ms = NOW - 90 * MIN - 300 * MIN * window;
            let reset_s = reset_ms / 1000;
            rows.push(served_row(
                reset_ms - window_span + 10 * MIN,
                "claude-opus-5",
                50_000,
                Some(0.10),
                Some(reset_s),
            ));
            for step in 1..=10 {
                let at = reset_ms - window_span + 10 * MIN + step * (window_span / 22);
                let util = 0.10 + 0.30 * step as f64 / 20.0;
                rows.push(served_row(
                    at,
                    "claude-opus-5",
                    50_000,
                    Some(util),
                    Some(reset_s),
                ));
                rows.push(served_row(
                    at + MIN,
                    "claude-haiku-4-5",
                    25_000,
                    Some(util),
                    Some(reset_s),
                ));
            }
        }
        let fit = fit_quota_model(&rows);
        assert!(fit.ok, "{:?}", fit.reason);
        // The survivor: the window advanced du 0.15 over 0.25M fresh
        // tokens a window, so its weight is 0.60.
        let (priced, bound) = fit
            .quota_for("claude-haiku-4-5", 1_000_000)
            .expect("the survivor is priced");
        assert!((priced - 0.60).abs() < 1e-9, "{priced}");
        assert!(!bound, "the survivor's own weight is measured");
        // The rider borrows the survivor's weight, and the provenance says
        // so.
        let (borrowed, bound) = fit
            .quota_for("claude-opus-5", 1_000_000)
            .expect("the rider borrows");
        assert!(
            (borrowed - 0.60).abs() < 1e-9,
            "the rider rides the survivor's weight"
        );
        assert!(
            bound,
            "a borrowed weight is an upper bound, and the caller must say so"
        );
        // A model the log never saw has no weight at all.
        assert_eq!(fit.quota_for("claude-mythos-5", 1_000_000), None);
    }

    #[test]
    fn unattributable_traffic_gets_no_weight_and_can_lose_windows() {
        // A third group that appears in only one window cannot be weighted;
        // its model prices to None rather than zero (zero would read as
        // "this traffic is free").
        let mut rows = fit_rows(NOW);
        let reset_s = (NOW - 90 * MIN - 300 * MIN * 3) / 1000;
        rows.push(served_row(
            NOW - 300 * MIN,
            "claude-fable-5",
            50_000,
            Some(0.35),
            Some(reset_s),
        ));
        let fit = fit_quota_model(&rows);
        assert!(fit.ok);
        assert_eq!(
            fit.quota_for("claude-fable-5", 1_000_000),
            None,
            "one window is not evidence"
        );
        assert!(fit.quota_for("claude-opus-5", 1_000_000).is_some());

        // A window DOMINATED by the unattributable group says nothing
        // about the groups that can be weighted, and is set aside.
        let mut dominated = fit_rows(NOW);
        let reset_s = (NOW - 90 * MIN) / 1000;
        for step in 0..24 {
            dominated.push(served_row(
                NOW - 300 * MIN + step * MIN,
                "claude-fable-5",
                500_000,
                Some(0.10 + 0.30 * step as f64 / 24.0),
                Some(reset_s),
            ));
        }
        // The opus share of that window is ~2%: the window is ~98%
        // unattributable, over the 10% share, so it goes — and with only
        // three usable windows left the fit declines.
        assert!(
            !fit_quota_model(&dominated).ok,
            "a dominated window is not evidence"
        );
    }

    // ── the notice ─────────────────────────────────────────────────

    #[test]
    fn human_idle_takes_the_shape_a_human_uses() {
        assert_eq!(human_idle(47 * MIN), "47m");
        assert_eq!(human_idle(2 * HOUR + 6 * MIN), "2h 6m");
        assert_eq!(human_idle(3 * HOUR), "3h");
        // Rounded, not floored: 59m32s is an hour away.
        assert_eq!(human_idle(59 * MIN + 32_000), "1h");
        assert_eq!(human_idle(90 * MIN), "1h 30m");
    }

    #[test]
    fn the_cold_notice_is_pure_and_byte_pinned() {
        let content = ColdBlocking::notice(
            2 * HOUR + 6 * MIN,
            200_621,
            Some("claude-sonnet-5"),
            None,
            1_769_500_800_000,
            &utc(),
            NoticeStyle::Plain,
        );
        insta::assert_snapshot!(content);
        // A known tier states the write's cost; an unknown one says nothing.
        let hour = ColdBlocking::notice_for(
            Some(2.0),
            2 * HOUR,
            200_621,
            None,
            None,
            1_769_500_800_000,
            &utc(),
            NoticeStyle::Plain,
        );
        assert!(hour.contains("(\u{d7}2 usage)"), "{hour}");
        assert!(!content.contains("usage)"), "{content}");
        insta::assert_snapshot!("write_cost_one_hour_tier", hour);
        let five = ColdBlocking::notice_for(
            Some(1.25),
            2 * HOUR,
            200_621,
            None,
            None,
            1_769_500_800_000,
            &utc(),
            NoticeStyle::Plain,
        );
        assert!(five.contains("(\u{d7}1.25 usage)"), "{five}");
        // Purity (invariant 4): the same inputs render the same bytes,
        // every call, in every style.
        for _ in 0..3 {
            for style in [
                NoticeStyle::Plain,
                NoticeStyle::Gfm,
                NoticeStyle::Toker,
                NoticeStyle::Block,
            ] {
                assert_eq!(
                    ColdBlocking::notice(
                        2 * HOUR + 6 * MIN,
                        200_621,
                        Some("claude-sonnet-5"),
                        None,
                        1_769_500_800_000,
                        &utc(),
                        style
                    ),
                    ColdBlocking::notice(
                        2 * HOUR + 6 * MIN,
                        200_621,
                        Some("claude-sonnet-5"),
                        None,
                        1_769_500_800_000,
                        &utc(),
                        style
                    )
                );
            }
        }
    }

    #[test]
    fn the_notice_renders_in_all_three_styles() {
        let content = ColdBlocking::notice(
            47 * MIN,
            200_000,
            None,
            None,
            1_769_500_800_000,
            &utc(),
            NoticeStyle::Plain,
        );
        // Plain's brackets are its frame; the other styles frame the bare
        // content.
        let content = content
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .expect("plain is bracketed");
        assert_eq!(
            ColdBlocking::notice(
                47 * MIN,
                200_000,
                None,
                None,
                1_769_500_800_000,
                &utc(),
                NoticeStyle::Gfm
            ),
            format!(
                "> [!WARNING]{}",
                content
                    .lines()
                    .map(|line| format!("\n> {line}"))
                    .collect::<String>()
            )
        );
        assert_eq!(
            ColdBlocking::notice(
                47 * MIN,
                200_000,
                None,
                None,
                1_769_500_800_000,
                &utc(),
                NoticeStyle::Block
            ),
            format!(
                "{}\n{content}\n{}",
                crate::middleware::notice::BLOCK_HEADER,
                crate::middleware::notice::BLOCK_FOOTER
            )
        );
        // The whole notice without a compaction target, in each framed
        // style: the relations above hold them to the content, these
        // show the bytes.
        for style in [NoticeStyle::Gfm, NoticeStyle::Toker, NoticeStyle::Block] {
            insta::assert_snapshot!(
                format!("style_{}", style.name()),
                ColdBlocking::notice(
                    47 * MIN,
                    200_000,
                    None,
                    None,
                    1_769_500_800_000,
                    &utc(),
                    style
                )
            );
        }
        // The default style is the generic GFM alert, like the quota gate.
        assert!(
            ColdBlocking::notice(
                47 * MIN,
                200_000,
                None,
                None,
                1_769_500_800_000,
                &utc(),
                NoticeStyle::default()
            )
            .starts_with("> [!WARNING]\n> ")
        );
    }

    #[test]
    fn the_openai_turn_wraps_the_notice_in_both_wire_forms() {
        let content = ColdBlocking::notice(
            47 * MIN,
            200_000,
            None,
            None,
            1_769_500_800_000,
            &utc(),
            NoticeStyle::Plain,
        );
        // Non-stream: one chat completion, one assistant message, zeroed
        // usage, the toker id.
        let json = ColdBlocking::openai_turn(&content, Some("z-ai/glm-5.3"), false, NOW);
        let value: Value =
            serde_json::from_slice(&json).expect("the non-stream turn is a chat completion");
        assert_eq!(value["id"], "chatcmpl-toker-cold");
        assert_eq!(value["object"], "chat.completion");
        assert_eq!(value["created"], json!(NOW / 1000));
        assert_eq!(value["model"], "z-ai/glm-5.3");
        assert_eq!(value["choices"][0]["message"]["role"], "assistant");
        assert_eq!(value["choices"][0]["message"]["content"], json!(content));
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
        assert_eq!(
            value["usage"],
            json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}),
            "nothing reached upstream: a synthetic turn claims no tokens"
        );

        // Stream: a single content delta, then the terminator — the
        // captured-fixture dialect.
        let sse = ColdBlocking::openai_turn(&content, Some("z-ai/glm-5.3"), true, NOW);
        let text = String::from_utf8(sse).expect("utf-8");
        // One content delta then [DONE], byte for byte.
        assert!(text.ends_with("\n\ndata: [DONE]\n"), "{text}");
        insta::assert_snapshot!(text);

        // An absent or empty model falls back to the stand-in; the bytes
        // are a pure function of the inputs.
        assert_eq!(
            ColdBlocking::openai_turn("x", None, false, NOW),
            ColdBlocking::openai_turn("x", Some(""), false, NOW)
        );
        let with_default: Value =
            serde_json::from_slice(&ColdBlocking::openai_turn("x", None, false, NOW))
                .expect("parses");
        assert_eq!(with_default["model"], "z-ai/glm-5.3");
    }

    #[test]
    fn the_notice_names_the_wall_the_window_was_already_heading_for() {
        let off_track = Outlook {
            known: true,
            on_track: false,
            meter: Some(Meter::FiveHour),
            extra: Some(0.062),
            bound: Some(false),
            util: Some(0.95),
            reset_at_ms: Some(1_769_500_800_000 + 2 * HOUR),
            wall_at_ms: Some(1_769_500_800_000_f64 + 45.0 * MIN as f64),
            pulled_in_ms: Some(15.0 * MIN as f64),
        };
        let notice = ColdBlocking::notice(
            2 * HOUR,
            200_000,
            Some("claude-sonnet-5"),
            Some(&off_track),
            1_769_500_800_000,
            &utc(),
            NoticeStyle::Plain,
        );
        assert!(
            notice.contains("08:45") && notice.contains("forward by 15m"),
            "{notice}"
        );
        insta::assert_snapshot!("off_track_five_hour", notice);

        // An already-spent weekly meter, with a borrowed (bound) share:
        // "up to" like any share, and the share stays of a 5-HOUR window even though the
        // sentence is about the week.
        let spent_week = Outlook {
            known: true,
            on_track: false,
            meter: Some(Meter::SevenDay),
            extra: Some(0.062),
            bound: Some(true),
            util: Some(0.999),
            reset_at_ms: Some(1_769_500_800_000 + 24 * HOUR),
            wall_at_ms: None,
            pulled_in_ms: None,
        };
        let notice = ColdBlocking::notice(
            2 * HOUR,
            200_000,
            None,
            Some(&spent_week),
            1_769_500_800_000,
            &utc(),
            NoticeStyle::Plain,
        );
        assert!(
            notice.contains("7-day window was already spent")
                && notice.contains("6.2% of a 5-hour window"),
            "{notice}"
        );
        insta::assert_snapshot!("spent_seven_day", notice);
        // An under-minute pull-in spends no line on "by 0m".
        let tiny = Outlook {
            pulled_in_ms: Some(30_000.0),
            ..off_track
        };
        let notice = ColdBlocking::notice(
            2 * HOUR,
            200_000,
            None,
            Some(&tiny),
            1_769_500_800_000,
            &utc(),
            NoticeStyle::Plain,
        );
        assert!(
            !notice.contains("brings that forward"),
            "{notice}: under a minute is no effect worth a clause"
        );
    }

    #[test]
    fn an_unknown_or_on_track_outlook_leaves_no_quota_line() {
        let notice = ColdBlocking::notice(
            47 * MIN,
            200_000,
            None,
            None,
            1_769_500_800_000,
            &utc(),
            NoticeStyle::Plain,
        );
        assert!(!notice.contains("window was"), "{notice}");
        // A known, on-track outlook is the withheld case: it never rides a
        // notice (it produced a cold-quiet row instead).
        let on_track = Outlook {
            known: true,
            on_track: true,
            extra: Some(0.06),
            bound: Some(false),
            util: Some(0.52),
            ..Outlook::unknown()
        };
        let notice = ColdBlocking::notice(
            47 * MIN,
            200_000,
            None,
            Some(&on_track),
            1_769_500_800_000,
            &utc(),
            NoticeStyle::Plain,
        );
        assert!(!notice.contains("window was"), "{notice}");
    }

    #[test]
    fn no_notice_carries_an_em_dash() {
        // The notices hold no em dashes: every rendering the gate can
        // write, with or without a write cost, a target, and an outlook,
        // in every style, is held to that outside any snapshot. (The
        // cost's `\u{d7}` is fine: only U+2014 is banned.)
        let off_track = Outlook {
            known: true,
            on_track: false,
            meter: Some(Meter::FiveHour),
            extra: Some(0.062),
            bound: Some(false),
            util: Some(0.95),
            reset_at_ms: Some(1_769_500_800_000 + 2 * HOUR),
            wall_at_ms: Some(1_769_500_800_000_f64 + 45.0 * MIN as f64),
            pulled_in_ms: Some(15.0 * MIN as f64),
        };
        let spent_week = Outlook {
            meter: Some(Meter::SevenDay),
            bound: Some(true),
            util: Some(0.999),
            wall_at_ms: None,
            pulled_in_ms: None,
            ..off_track
        };
        for write_multiplier in [None, Some(2.0), Some(1.25)] {
            for target in [None, Some("claude-sonnet-5")] {
                for outlook in [None, Some(&off_track), Some(&spent_week)] {
                    for style in [
                        NoticeStyle::Plain,
                        NoticeStyle::Gfm,
                        NoticeStyle::Toker,
                        NoticeStyle::Block,
                    ] {
                        let notice = ColdBlocking::notice_for(
                            write_multiplier,
                            2 * HOUR + 6 * MIN,
                            200_621,
                            target,
                            outlook,
                            1_769_500_800_000,
                            &utc(),
                            style,
                        );
                        assert!(!notice.contains('\u{2014}'), "{notice}");
                    }
                }
            }
        }
    }

    // ── the compaction retarget ─────────────────────────────────────

    /// A compaction-shaped body with cache_control in all three positions
    /// (system blocks, tool definitions, message content parts) and one
    /// mid-conversation system message to merge.
    fn compaction_body() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model": "claude-opus-5",
            "stream": true,
            "max_tokens": 1024,
            "system": [
                {"type": "text", "text": "You are careful.", "cache_control": {"type": "ephemeral"}},
            ],
            "tools": [
                {"name": "Read", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral"}},
                {"name": "Bash", "input_schema": {"type": "object"}},
            ],
            "messages": [
                {"role": "user", "content": "Earlier work."},
                {"role": "system", "content": [{"type": "text", "text": "[reminder]"}]},
                {"role": "user", "content": [
                    {"type": "text", "text": "Your task is to create a detailed summary of the conversation so far.",
                     "cache_control": {"type": "ephemeral"}},
                ]},
            ],
        }))
        .expect("serialise compaction body")
    }

    #[test]
    fn cheaper_is_judged_on_what_the_model_map_will_send() {
        // The host map sends every Sonnet as Opus 5: the alias in the body
        // is cheaper by its own name, but what the upstream bills is not,
        // so the model stays — the cold licence still strips.
        let dearer =
            crate::middleware::model_map::parse_model_map(r#"{"family:sonnet": "claude-opus-5"}"#)
                .expect("map parses");
        let mut request = Request::parse(&compaction_body()).expect("parse");
        let outcome =
            retarget_compaction(&mut request, Some("claude-sonnet-5"), true, dearer.as_ref())
                .expect("the cold licence alone still rewrites");
        assert_eq!(outcome.to.as_deref(), Some("claude-opus-5"), "no move");
        assert_eq!(outcome.from, outcome.to);
        assert!(outcome.stripped > 0);

        // A target the map sends somewhere unpriced cannot be judged, so
        // it is not called cheaper either.
        let unpriced =
            crate::middleware::model_map::parse_model_map(r#"{"family:sonnet": "z-ai/glm-5.3"}"#)
                .expect("map parses");
        assert_eq!(
            cheaper_of(
                Some("claude-opus-5"),
                Some("claude-sonnet-5"),
                unpriced.as_ref()
            ),
            None
        );
        // Without a map the names are the billed identities.
        assert_eq!(
            cheaper_of(Some("claude-opus-5"), Some("claude-sonnet-5"), None).as_deref(),
            Some("claude-sonnet-5")
        );
    }

    #[test]
    fn the_outlook_reads_only_the_routed_backend_s_rows() {
        // The subscription's ledger can price a re-read; a request routed
        // to the plain API must not be quietened by it — the API has no
        // 5-hour meter, so its outlook is blind and the notice fires.
        let store = crate::store::Store::open(":memory:").expect("store");
        let rows: Vec<RequestRow> = fit_rows(NOW)
            .into_iter()
            .map(|mut row| {
                row.provider = Some("anthropic_sub".to_owned());
                row
            })
            .collect();
        store.record_requests(&rows).expect("seed");
        assert!(
            outlook_over(
                &store,
                "anthropic_sub",
                Some("claude-opus-5"),
                200_000,
                true,
                NOW
            )
            .is_some_and(|outlook| outlook.known && outlook.on_track),
            "the subscription's own rows price its re-read"
        );
        assert_eq!(
            outlook_over(
                &store,
                "anthropic_api",
                Some("claude-opus-5"),
                200_000,
                true,
                NOW
            ),
            None,
            "another backend's meters are not this one's"
        );
    }

    #[test]
    fn the_retarget_rewrites_strips_and_merges_with_pinned_bytes() {
        let mut request = Request::parse(&compaction_body()).expect("parse");
        let outcome = retarget_compaction(&mut request, Some("claude-sonnet-5"), true, None)
            .expect("a cheaper target on a cold lane rewrites");
        assert_eq!(
            outcome,
            RetargetOutcome {
                from: Some("claude-opus-5".to_owned()),
                to: Some("claude-sonnet-5".to_owned()),
                merged: 1,
                stripped: 3,
            }
        );
        // The exact bytes that go upstream: the model region changed, all
        // three cache_control positions went, and the system message
        // merged into the PRECEDING user turn as a [system]
        // block — string content synthesised into a text block first.
        assert_eq!(
            request.serialise(),
            serde_json::to_vec(&json!({
                "model": "claude-sonnet-5",
                "stream": true,
                "max_tokens": 1024,
                "system": [
                    {"type": "text", "text": "You are careful."},
                ],
                "tools": [
                    {"name": "Read", "input_schema": {"type": "object"}},
                    {"name": "Bash", "input_schema": {"type": "object"}},
                ],
                "messages": [
                    {"role": "user", "content": [
                        {"type": "text", "text": "Earlier work."},
                        {"type": "text", "text": "[system] [reminder]"},
                    ]},
                    {"role": "user", "content": [
                        {"type": "text", "text": "Your task is to create a detailed summary of the conversation so far."},
                    ]},
                ],
            }))
            .expect("serialise expected body")
        );
        // The transform is pure: same input, same bytes, any number of
        // fresh parses (invariant 4 — the prefix the upstream builds from
        // these bytes must be reproducible).
        for _ in 0..3 {
            let mut again = Request::parse(&compaction_body()).expect("parse");
            retarget_compaction(&mut again, Some("claude-sonnet-5"), true, None).expect("rewrites");
            assert_eq!(again.serialise(), request.serialise());
        }
    }

    #[test]
    fn anything_unexpected_declines_the_whole_rewrite_untouched() {
        let declines = |body: &[u8], target: Option<&str>, cold: bool| {
            let mut request = Request::parse(body).expect("test bodies parse");
            let before = request.serialise();
            let outcome = retarget_compaction(&mut request, target, cold, None);
            assert!(outcome.is_none(), "{:?}", body);
            assert_eq!(request.serialise(), before, "all-or-nothing: untouched");
        };

        // A system message with no user turn to merge into.
        declines(
            br#"{"model":"claude-opus-5","messages":[{"role":"system","content":"hi"}]}"#,
            Some("claude-sonnet-5"),
            true,
        );
        // A system message after an assistant turn: there is nowhere safe
        // to put it, so the rewrite is abandoned rather than guessed at.
        declines(
            br#"{"model":"claude-opus-5","messages":[
                {"role":"assistant","content":"hi"},
                {"role":"system","content":"reminder"}]}"#,
            Some("claude-sonnet-5"),
            true,
        );
        // A system message whose text is whitespace only.
        declines(
            br#"{"model":"claude-opus-5","messages":[
                {"role":"user","content":"hi"},
                {"role":"system","content":"  \n "}]}"#,
            Some("claude-sonnet-5"),
            true,
        );
        // A system message with unmergeable content (neither string nor
        // array).
        declines(
            br#"{"model":"claude-opus-5","messages":[
                {"role":"user","content":"hi"},
                {"role":"system","content":5}]}"#,
            Some("claude-sonnet-5"),
            true,
        );
        // A host user message with unmergeable content.
        declines(
            br#"{"model":"claude-opus-5","messages":[
                {"role":"user","content":5},
                {"role":"system","content":"reminder"}]}"#,
            Some("claude-sonnet-5"),
            true,
        );
        // A non-object entry in messages.
        declines(
            br#"{"model":"claude-opus-5","messages":["nope"]}"#,
            Some("claude-sonnet-5"),
            true,
        );
        // No messages at all, an empty array, or a non-array.
        declines(
            br#"{"model":"claude-opus-5"}"#,
            Some("claude-sonnet-5"),
            true,
        );
        declines(
            br#"{"model":"claude-opus-5","messages":[]}"#,
            Some("claude-sonnet-5"),
            true,
        );
        declines(
            br#"{"model":"claude-opus-5","messages":5}"#,
            Some("claude-sonnet-5"),
            true,
        );
        // Not an object.
        declines(b"[1,2]", Some("claude-sonnet-5"), true);
        // A warm lane with no cheaper model to move to: nothing justifies
        // touching the body — the two licences are a model change or a
        // cold lane, and neither holds here.
        declines(
            br#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":"hi"}]}"#,
            Some("claude-sonnet-5"),
            false,
        );
        // And an offered target that is not cheaper never moves sideways
        // or upward, cold licence or not.
        declines(
            br#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":"hi"}]}"#,
            Some("claude-opus-5"),
            false,
        );
    }

    #[test]
    fn a_model_change_alone_licenses_the_strip_even_on_a_warm_lane() {
        // The other licence: caches are keyed per model, so a move to a
        // strictly cheaper one provably loses no cache the new model could
        // have read — `cold` is only needed when there is no move to make.
        let mut request = Request::parse(
            br#"{"model":"claude-opus-5",
                "messages":[{"role":"user","content":[
                    {"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}]}"#,
        )
        .expect("parse");
        let outcome = retarget_compaction(&mut request, Some("claude-sonnet-5"), false, None)
            .expect("the model change is the licence");
        assert_eq!(
            outcome,
            RetargetOutcome {
                from: Some("claude-opus-5".to_owned()),
                to: Some("claude-sonnet-5".to_owned()),
                merged: 0,
                stripped: 1,
            }
        );
    }

    #[test]
    fn a_cold_lane_strips_without_a_target_and_without_a_model_key() {
        // The cold licence alone: no model change (there is none to make),
        // but the cache writes are bought-never-read, so the strip goes
        // ahead — and `j.model = to || from` with neither present
        // DROPS the key, ported as-is.
        let mut request = Request::parse(
            br#"{"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral"}}],
                "messages":[{"role":"user","content":[]}]}"#,
        )
        .expect("parse");
        let outcome =
            retarget_compaction(&mut request, None, true, None).expect("cold licence alone");
        assert_eq!(outcome.from, None);
        assert_eq!(outcome.to, None);
        assert_eq!(outcome.merged, 0);
        assert_eq!(outcome.stripped, 1);
        let value: Value = serde_json::from_slice(&request.serialise()).expect("serialises");
        assert_eq!(value.get("model"), None, "the key drops, ported as-is");
        assert!(
            value.get("system").unwrap()[0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn a_same_model_strip_is_not_a_downgrade() {
        // A cold compaction already on the cheapest sensible model: the
        // strip goes ahead, the model does not move, and `to == from` is
        // the marker the wiring reads as "no downgrade".
        let mut request = Request::parse(
            br#"{"model":"claude-sonnet-5",
                "tools":[{"name":"Read","input_schema":{},"cache_control":{"type":"ephemeral"}}],
                "messages":[{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}]}"#,
        )
        .expect("parse");
        let outcome = retarget_compaction(&mut request, Some("claude-opus-5"), true, None)
            .expect("cold licence");
        assert_eq!(outcome.from.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(
            outcome.to, outcome.from,
            "an upward offer is refused; the model stays"
        );
        assert_eq!(outcome.stripped, 2);
        assert_eq!(outcome.merged, 0);
        let value: Value = serde_json::from_slice(&request.serialise()).expect("serialises");
        assert_eq!(
            value.get("model").and_then(Value::as_str),
            Some("claude-sonnet-5")
        );
        assert!(
            request
                .serialise()
                .windows(13)
                .all(|w| w != b"cache_control\"")
        );
    }

    #[test]
    fn the_retarget_keeps_the_untouched_prefix_stable() {
        // Invariant 5's transformed-request clause: the transform is pure
        // and touches only the model region, the stripped breakpoints, and
        // the merge point — the rest of the conversation's bytes are
        // reproduced exactly, in place.
        let body = serde_json::to_vec(&json!({
            "stream": true,
            "temperature": 0.7,
            "model": "claude-opus-5",
            "top_k": 42,
            "system": [{"type": "text", "text": "You are careful."}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Earlier work, untouched."}]},
                {"role": "system", "content": [{"type": "text", "text": "mid reminder"}]},
                {"role": "user", "content": [
                    {"type": "text", "text": "the last turn"},
                    {"type": "text", "text": "spare", "cache_control": {"type": "ephemeral"}},
                ]},
            ],
        }))
        .expect("serialise");
        let mut request = Request::parse(&body).expect("parse");
        retarget_compaction(&mut request, Some("claude-sonnet-5"), true, None).expect("rewrites");
        let retargeted = request.serialise();

        // Every key before `model`, and the model region itself, is the
        // original bytes except the value; everything the transform did not
        // reach is reproduced exactly, in its original position.
        assert!(retargeted.starts_with(
            br#"{"stream":true,"temperature":0.7,"model":"claude-sonnet-5","top_k":42,"system":[{"type":"text","text":"You are careful."}],"messages":[{"role":"user","content":[{"type":"text","text":"Earlier work, untouched."},{"type":"text","text":"[system] mid reminder"}]},{"role":"user","content":[{"type":"text","text":"the last turn"},{"type":"text","text":"spare"}]}"#
        ), "the untouched prefix is byte-stable; only the model value, the stripped breakpoints, and the merge point change");
        // The breakpoint that went is the only byte difference in the last
        // user turn: the blocks sit adjacent, exactly as a removal leaves
        // them.
        assert!(!String::from_utf8_lossy(&retargeted).contains("cache_control"));
        // The transform is deterministic: a fresh parse of the same body
        // produces the identical bytes (invariant 4).
        let mut again = Request::parse(&body).expect("parse");
        retarget_compaction(&mut again, Some("claude-sonnet-5"), true, None).expect("rewrites");
        assert_eq!(again.serialise(), retargeted);
    }
}
