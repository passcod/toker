//! The CACHE REBUILDS panel's aggregation (plan: TUI — "the panel to
//! watch"): a port of ctp's lane walk, cause classification, and
//! system-change localisation.
//!
//! The walk ([`classify`]) is ctp live.mjs:205-243's lane walk with
//! summarise.mjs:512-561's careful lane rules:
//!
//! - **A session is not a cache entry** (docs/internals/lanes.md). One
//!   session interleaves the main agent, subagents, and utility calls,
//!   each an independent prefix with its own cache; comparing a
//!   request against whatever preceded it in wall-clock order invents
//!   rebuilds that never happened. The walk compares within a lane —
//!   the previous request sharing `session_id` + `tools_hash`.
//! - **The anti-phantom rule**: lanes are walked over every row read,
//!   not just the windowed ones, and only then filtered to the window.
//!   A lane whose predecessor falls outside the window would otherwise
//!   look like a brand-new prefix, and every window would open with
//!   phantom "new prefix" rebuilds. The 24 h tail the TUI reads exists
//!   to provide those predecessors.
//! - **The abandoned-lane test** (summarise.mjs:546-557): the first
//!   request of a lane is either a concurrent lane that will alternate
//!   with the old one (a subagent — nothing was invalidated) or a real
//!   tool-set change that never comes back. Whether the previous lane
//!   returns tells them apart — a real change is one-way.
//!
//! The causes follow the live.mjs table (README "Interpreting the
//! causes"); the tests for each follow summarise.mjs:442-492's
//! `classifyRebuild`, in its order — the first rule that fires wins:
//!
//! 1. no predecessor in the lane → `new prefix / first turn` (a
//!    session's first request, or a concurrent lane), or `tool set
//!    changed` when the previous lane was abandoned;
//! 2. a gap longer than the 1-hour cache TTL → `idle — 1h cache TTL
//!    expired` (summarise.mjs:445-446);
//! 3. a compaction generation bump → `compaction` — a fact Claude Code
//!    stamps into the conversation, not an inference
//!    (summarise.mjs:468-470);
//! 4. a message count collapsing to a handful → `subagent started`
//!    (summarise.mjs:474-480's `NEW_CONVERSATION_MESSAGES` rule);
//! 5. a changed system hash → `system prompt changed`, localised;
//! 6. anything else → `mid-history change` — an earlier turn differs.
//!
//! Deviations from summarise, deliberate and scoped: its `history
//! shrank` and `unknown` labels fold into `mid-history change` (the
//! live.mjs table has neither), and a NULL `tools_hash` keys its own
//! lane rather than aborting classification (live.mjs's `?` lane).
//! `summarising` is carried on the row but never consulted — the same
//! prompt shape serves routine background summaries, so treating it
//! as a compaction would fire constantly (summarise.mjs:481-483).
//!
//! The localisation ([`localise`]) ports summarise.mjs:374-434 /
//! proxy.mjs:671-710: which block changed (per-block digests), then
//! the ladder window (cumulative digests every 8 192 bytes), then the
//! tail (8-byte steps over the last 256, 64-byte steps to 1 024 — the
//! geometry [`crate::ir::anthropic`] cuts the stored rungs to). The
//! capture-time `system_change.where` is preferred when the row
//! carries it (summarise.mjs:385-387 — toker itself never writes that
//! column, but the imported ctp history does). A localisation never
//! names a position the rungs do not bound: no rungs, no claim.
//!
//! Absence ≠ zero (invariant 3) throughout: a NULL `cache_write_total`
//! is an unmeasurable rewrite (counted, never zero-filled), and the
//! panel's denominators keep the measured count separate from the
//! unknown one.

use std::collections::HashMap;

use crate::ir::anthropic::{LADDER_STEP, tail_offsets};
use crate::store::{LocalisationRow, RebuildRow};
use serde_json::Value;

/// Tokens of cache-write in one request before it counts as a rebuild
/// rather than a turn (ctp live.mjs:205, summarise.mjs:49-57 — the
/// default `--min` of `summarise.mjs --rebuilds`).
pub(crate) const REBUILD_MIN: i64 = 50_000;

/// A gap longer than the 1-hour cache TTL explains a rewrite on its
/// own: anything older has nothing left to lose (summarise.mjs:444-446;
/// live.mjs's `idle N min` row of the causes table).
const IDLE_TTL_MS: i64 = 60 * 60 * 1000;

/// A conversation restarting from at most this many messages is a new
/// agent — a subagent or sidechain, not the same prompt mutating
/// (summarise.mjs:55's `NEW_CONVERSATION_MESSAGES`). The full test is
/// summarise.mjs:474-480's (`messages ≤ 8 && prev > 2×messages`);
/// live.mjs:238 tests the same collapse with the fixed bound
/// `prev > 16` — the port keeps summarise's ratio, which also fires
/// on a 9-message restart after a 20-message lane.
const NEW_CONVERSATION_MESSAGES: i64 = 8;

/// The cause table keeps at most this many rows (live.mjs:243's
/// `slice(0, 5)`).
pub(crate) const MAX_CAUSE_ROWS: usize = 5;

/// A rebuild's cause — the stable label the by-cause table groups on.
/// The variable detail lives beside it ([`RebuildEvent::detail`]), not
/// inside it: every time the detail has been folded into the label
/// string, something silently split into one row per value (ctp
/// summarise.mjs:436-439's hard-won rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Cause {
    /// No predecessor in the lane: the session's first request, or a
    /// concurrent lane with its own cache (live.mjs:235).
    NewPrefix,
    /// The lane went quiet longer than the cache lives (live.mjs's
    /// `idle N min — 1h cache TTL expired`).
    Idle,
    /// The previous lane never came back — a one-way tool-set change
    /// (summarise.mjs:551).
    ToolSet,
    /// Claude Code stamped a compaction continuation into the
    /// conversation (live.mjs:236).
    Compaction,
    /// A message count collapsed to a handful: a subagent started, not
    /// a compaction (live.mjs:238; the compaction marker is the fact
    /// that tells them apart — lanes.md).
    Subagent,
    /// The system prompt changed, invalidating all history
    /// (live.mjs:237) — localised in the detail.
    SystemPrompt,
    /// An earlier turn in the conversation differs: a rewind, an edit,
    /// a tool result changing retroactively (live.mjs:239).
    MidHistory,
}

impl Cause {
    /// The panel's label (live.mjs's exact cause strings).
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NewPrefix => "new prefix / first turn",
            Self::Idle => "idle — 1h cache TTL expired",
            Self::ToolSet => "tool set changed",
            Self::Compaction => "compaction",
            Self::Subagent => "subagent started",
            Self::SystemPrompt => "system prompt changed",
            Self::MidHistory => "mid-history change",
        }
    }
}

/// One rebuild: when, which session, why, and how many tokens it
/// rewrote. `detail` is the variable part of the cause line — the gap
/// length, the compaction generation, the localised system change.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RebuildEvent {
    /// When the rewrite happened (epoch ms).
    pub ts_ms: i64,
    /// The session (or the NULL-session group's dash).
    pub session: String,
    /// The stable cause the table groups on.
    pub cause: Cause,
    /// The cause's variable detail, rendered as `label (detail)`.
    pub detail: Option<String>,
    /// How many tokens the request rewrote (`cache_write_total`).
    pub rewritten: i64,
    /// The localisation inputs for a system-prompt change; `None` for
    /// every other cause.
    pub system: Option<SystemChange>,
}

/// A system-prompt change's localisation inputs: the two rows' block
/// maps (already on the narrow rows) and their ids (the heavy ladders
/// are fetched for these, by id, in the panel's second query).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SystemChange {
    /// The row that rewrote (current system prompt).
    pub row_id: i64,
    /// The lane predecessor it changed against.
    pub prev_id: i64,
    /// `system_chars` of the predecessor.
    pub prev_chars: Option<i64>,
    /// `system_chars` of the changed row.
    pub chars: Option<i64>,
    /// `system_blocks` of the predecessor.
    pub prev_blocks: Option<Value>,
    /// `system_blocks` of the changed row.
    pub blocks: Option<Value>,
}

/// The lane walk's raw output: the window's denominators plus the
/// classified events, in walk order (oldest first).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Walk {
    /// Measurement rows the walk saw inside the window — the panel's
    /// denominator (live.mjs's `usage.length`).
    pub window_rows: usize,
    /// In-window rows whose rewritten total is unknown (live.mjs's
    /// `rebuildUnknown`): counted, never zero-filled.
    pub unmeasured: usize,
    /// The classified rebuilds, oldest first.
    pub events: Vec<RebuildEvent>,
}

/// Walk lanes over the tail's rows and classify the window's rewrites
/// (live.mjs:205-243, plus summarise.mjs:512-561's lane rules).
///
/// `window_since_ms` filters which rows are *counted*; every row read
/// still walks its lane first — that ordering is the anti-phantom rule.
/// Rows arrive (ts, id)-ordered from the store read; the walk relies
/// on that and re-sorts nothing.
pub(crate) fn classify(rows: &[RebuildRow], window_since_ms: i64) -> Walk {
    // Group rows by session, preserving the read's order within each
    // group (a Vec per session keyed by an index map; group order is
    // insertion order, which no rule here depends on).
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<Vec<&RebuildRow>> = Vec::new();
    for row in rows {
        let key = row
            .session_id
            .clone()
            .unwrap_or_else(|| super::model::NO_SESSION.to_owned());
        let slot = *index.entry(key).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[slot].push(row);
    }

    let mut walk = Walk::default();
    for mut group in groups {
        // summarise.mjs:525-526 sorts each session's rows by timestamp;
        // the store read already returns (ts, id) order, but the walk's
        // every rule is predecessor-based, so the sort is cheap
        // insurance rather than a silent order dependency.
        group.sort_by_key(|row| (row.ts_ms, row.id));
        // The last position each lane appears at, for the
        // abandoned-lane test: a real tool-set change never comes back,
        // so the previous lane's last sighting at or before the row
        // that replaced it is what tells the change from a concurrent
        // lane (summarise.mjs:553-557's `rows.slice(i + 1).some(…)`).
        let mut last_seen: HashMap<Option<&str>, usize> = HashMap::new();
        for (pos, row) in group.iter().enumerate() {
            last_seen.insert(row.tools_hash.as_deref(), pos);
        }

        let mut lanes: HashMap<Option<&str>, usize> = HashMap::new();
        let mut prev_overall: Option<usize> = None;
        for (pos, row) in group.iter().enumerate() {
            let lane = row.tools_hash.as_deref();
            let prev_in_lane = lanes.get(&lane).copied();
            lanes.insert(lane, pos);
            // The walk updates lane state for EVERY row read — the
            // pre-window rows are the anti-phantom baselines — and only
            // then decides whether this row is counted.
            let in_window = row.ts_ms >= window_since_ms;
            if in_window {
                walk.window_rows += 1;
            }
            let Some(rewritten) = row.cache_write_total else {
                // An unknown rewrite is not a zero rewrite: counted
                // separately, classified never (live.mjs:228-231).
                if in_window {
                    walk.unmeasured += 1;
                }
                prev_overall = Some(pos);
                continue;
            };
            if in_window && rewritten >= REBUILD_MIN {
                let event = classify_one(
                    row,
                    prev_in_lane.map(|prev| group[prev]),
                    prev_overall.map(|prev| group[prev]),
                    |lane| last_seen.get(&lane).copied(),
                    pos,
                );
                walk.events.push(event);
            }
            prev_overall = Some(pos);
        }
    }
    walk
}

/// Classify one rewrite against its lane predecessor (the cause order
/// is summarise.mjs:442-492's; see the module docs for the mapping).
fn classify_one(
    row: &RebuildRow,
    prev_in_lane: Option<&RebuildRow>,
    prev_overall: Option<&RebuildRow>,
    last_seen: impl Fn(Option<&str>) -> Option<usize>,
    pos: usize,
) -> RebuildEvent {
    let session = row
        .session_id
        .clone()
        .unwrap_or_else(|| super::model::NO_SESSION.to_owned());
    let event = |cause, detail, system| RebuildEvent {
        ts_ms: row.ts_ms,
        session,
        cause,
        detail,
        rewritten: row.cache_write_total.unwrap_or(0),
        system,
    };

    let Some(prev) = prev_in_lane else {
        // First request in this lane within the tail. Either the tool
        // set genuinely changed and the old lane is gone, or this is a
        // concurrent lane that will alternate with it; whether the old
        // lane ever comes back tells them apart.
        return match prev_overall {
            // Nothing before it in the session at all: the session's
            // first request — nothing was cached yet.
            None => event(Cause::NewPrefix, None, None),
            Some(prev) => {
                let abandoned =
                    last_seen(prev.tools_hash.as_deref()).is_some_and(|last| last < pos);
                if abandoned {
                    event(Cause::ToolSet, None, None)
                } else {
                    event(Cause::NewPrefix, None, None)
                }
            }
        };
    };

    // Idle: the lane went quiet past the cache lifetime. Checked first
    // (summarise's order): a resume after two hours is an expiry
    // whatever else changed alongside it.
    let gap = row.ts_ms - prev.ts_ms;
    if gap > IDLE_TTL_MS {
        let detail = format!("{} min", ((gap as f64) / 60_000.0).round());
        return event(Cause::Idle, Some(detail), None);
    }
    // Compaction: the generation marker is a fact, and it must be
    // checked before the message-count tests, which cannot tell a
    // compaction from a subagent (summarise.mjs:468-470).
    if row.compact_generations.unwrap_or(0) > prev.compact_generations.unwrap_or(0) {
        let detail = row
            .compact_generations
            .map(|generation| format!("generation {generation}"));
        return event(Cause::Compaction, detail, None);
    }
    // A conversation restarting from a handful of messages is a new
    // agent — its prefix was never cached, so nothing was invalidated
    // (summarise.mjs:474-480). Checked before the system test, which
    // would otherwise claim a prompt "changed" between two unrelated
    // conversations that merely share a tool set.
    if let (Some(messages), Some(prev_messages)) = (row.req_messages, prev.req_messages)
        && messages <= NEW_CONVERSATION_MESSAGES
        && prev_messages > messages * 2
    {
        let detail = format!("{prev_messages} → {messages} messages");
        return event(Cause::Subagent, Some(detail), None);
    }
    // System prompt change: invalidates all history, and is the one
    // cause the panel localises.
    if row.system_hash != prev.system_hash {
        let system = SystemChange {
            row_id: row.id,
            prev_id: prev.id,
            prev_chars: prev.system_chars,
            chars: row.system_chars,
            prev_blocks: prev.system_blocks.clone(),
            blocks: row.system_blocks.clone(),
        };
        return event(Cause::SystemPrompt, None, Some(system));
    }
    // summarise's `history shrank` folds here: the live.mjs table has
    // no such row, and both cases say "an earlier turn differs".
    event(Cause::MidHistory, None, None)
}

/// Fill every system-prompt event's detail (summarise.mjs:475-481's
/// `chars … where` line): `43,696 → 43,801 chars; block 2, in the last
/// 8 bytes`. The position clause prefers the capture-time
/// `system_change.where` the row may carry (summarise.mjs:385-387) and
/// otherwise re-derives it from the ladders ([`where_changed`]).
pub(crate) fn localise(events: &mut [RebuildEvent], by_id: &HashMap<i64, LocalisationRow>) {
    for event in events {
        let Some(system) = &event.system else {
            continue;
        };
        let stored = by_id
            .get(&system.row_id)
            .and_then(|row| row.system_change.as_ref())
            .and_then(|change| change.get("where"))
            .and_then(Value::as_str);
        let where_changed = stored
            .map(str::to_owned)
            .or_else(|| where_changed(system, by_id));
        let chars = format!(
            "{} → {} chars",
            grouped(system.prev_chars),
            grouped(system.chars)
        );
        event.detail = Some(match where_changed {
            Some(where_changed) => format!("{chars}; {where_changed}"),
            None => chars,
        });
    }
}

/// One row's ladder rungs, if it carried any.
fn ladder_of(loc: Option<&LocalisationRow>) -> Option<&[String]> {
    loc.and_then(|row| row.system_ladder.as_deref())
}

/// One row's tail rungs, if it carried any.
fn tail_of(loc: Option<&LocalisationRow>) -> Option<&[String]> {
    loc.and_then(|row| row.system_tail.as_deref())
}

/// Where the system prompt changed, from the block map and the rungs
/// (summarise.mjs:374-434 / proxy.mjs:679-702): which block, then the
/// ladder window, then the tail bound. `None` when the rungs bound
/// nothing — never a guessed position.
fn where_changed(system: &SystemChange, by_id: &HashMap<i64, LocalisationRow>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    // Which block (proxy.mjs:679-686): naming the block is not enough
    // on its own — the bulk of a prompt is one block — but it is the
    // first half of the answer.
    let blocks =
        |value: &Option<Value>| value.as_ref().and_then(|blocks| blocks.as_array().cloned());
    if let (Some(prev_blocks), Some(row_blocks)) =
        (blocks(&system.prev_blocks), blocks(&system.blocks))
    {
        if prev_blocks.len() != row_blocks.len() {
            parts.push(format!(
                "block count {} → {}",
                prev_blocks.len(),
                row_blocks.len()
            ));
        } else {
            for (i, (prev, row)) in prev_blocks.iter().zip(&row_blocks).enumerate() {
                if prev.get("hash") != row.get("hash") {
                    parts.push(format!(
                        "block {i} ({} → {} chars)",
                        chars_of(prev),
                        chars_of(row)
                    ));
                }
            }
        }
    }

    // The ladder window (proxy.mjs:690-692): the first prefix rung
    // that differs bounds the change to one 8 192-byte step. Every
    // shared rung matching puts the change past the last complete step
    // — the ladder's blind spot, which the tail covers. (The step is
    // the geometry the stored rungs were cut to, proxy.mjs:591's
    // `LADDER_STEP = 8192`; summarise.mjs:428's re-analysis hardcodes
    // a stale 2048, which would mislabel a live rung window 4×.)
    let prev_loc = by_id.get(&system.prev_id);
    let row_loc = by_id.get(&system.row_id);
    let ladder = ladder_of;
    let tail = tail_of;
    let ladder_window = ladder(prev_loc)
        .zip(ladder(row_loc))
        .and_then(|(prev, row)| {
            prev.iter().zip(row).position(|(a, b)| a != b).map(|pi| {
                format!(
                    "between bytes {} and {}",
                    pi * LADDER_STEP,
                    (pi + 1) * LADDER_STEP
                )
            })
        });
    match ladder_window {
        Some(window) => parts.push(window),
        None => {
            // The tail bound (proxy.mjs:694-701): suffixes compare by
            // length, so a change P bytes from the end leaves every
            // shorter suffix identical — the first differing rung
            // bounds it, and the offsets (8-byte steps over the last
            // 256, then 64-byte steps to 1 024) name the byte range.
            // Without both rows' lengths the offsets cannot be named,
            // so no claim is made rather than a wrong one.
            let lengths = system.prev_chars.zip(system.chars);
            let tail_bound = tail(prev_loc).zip(tail(row_loc)).zip(lengths).map(
                |((prev, row), (prev_chars, chars))| {
                    let offsets = tail_offsets(prev_chars.min(chars).max(0) as usize);
                    match prev.iter().zip(row).position(|(a, b)| a != b) {
                        Some(ti) => {
                            let to = offsets.get(ti).copied().unwrap_or(0);
                            let from = if ti == 0 {
                                0
                            } else {
                                offsets.get(ti - 1).copied().unwrap_or(0)
                            };
                            if from == 0 {
                                format!("in the last {to} bytes")
                            } else {
                                format!("{from}-{to} bytes from the end")
                            }
                        }
                        // Every shared suffix matching puts the change
                        // beyond the tail's reach — the honest bound.
                        None => format!(
                            "beyond the last {} bytes",
                            offsets.last().copied().unwrap_or(0)
                        ),
                    }
                },
            );
            if let Some(bound) = tail_bound {
                parts.push(bound);
            } else if let (Some(prev), Some(row)) = (ladder(prev_loc), ladder(row_loc)) {
                // Ladders on both rows but no tails: the change is
                // after the last complete step either of them measured
                // (summarise.mjs:430-432's fallback, at the geometry
                // the stored rungs actually follow).
                parts.push(format!(
                    "after byte {}",
                    prev.len().min(row.len()) * LADDER_STEP
                ));
            }
        }
    }

    (!parts.is_empty()).then(|| parts.join(", "))
}

/// A block map entry's `chars`, rendered as ctp's `n()` renders a
/// missing count (`-`), never as a zero.
fn chars_of(block: &Value) -> String {
    grouped(block.get("chars").and_then(Value::as_i64))
}

/// The panel's aggregate over a localised walk: the summary counts, the
/// capped cause table, and the events newest-first for the detail
/// lines.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RebuildAgg {
    /// Rewrites ≥ [`REBUILD_MIN`] in the window.
    pub rebuilds: usize,
    /// Measured requests in the window (`window_rows − unmeasured`).
    pub measured: usize,
    /// In-window requests whose rewritten total is unknown.
    pub unmeasured: usize,
    /// The cause table: count desc, label asc, capped at
    /// [`MAX_CAUSE_ROWS`] (live.mjs:243).
    pub causes: Vec<(Cause, usize)>,
    /// The classified events, newest first — the panel's localised
    /// detail lines render from these.
    pub events: Vec<RebuildEvent>,
}

/// Summarise a localised walk into the panel's data (live.mjs:507-521).
pub(crate) fn aggregate(walk: Walk) -> RebuildAgg {
    let mut counts: HashMap<Cause, usize> = HashMap::new();
    for event in &walk.events {
        *counts.entry(event.cause).or_default() += 1;
    }
    let mut causes: Vec<(Cause, usize)> = counts.into_iter().collect();
    causes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.label().cmp(b.0.label())));
    causes.truncate(MAX_CAUSE_ROWS);

    let mut events = walk.events;
    events.sort_by_key(|event| std::cmp::Reverse(event.ts_ms));

    RebuildAgg {
        rebuilds: events.len(),
        measured: walk.window_rows.saturating_sub(walk.unmeasured),
        unmeasured: walk.unmeasured,
        causes,
        events,
    }
}

/// Comma-grouped token counts, ctp `n()`'s rendering: `12,213,961`,
/// and `-` for an unknown, never a zero (the detail lines only ever
/// render a known count or an explicit gap).
pub(crate) fn grouped(value: Option<i64>) -> String {
    match value {
        None => "-".to_owned(),
        Some(value) => {
            let digits = value.unsigned_abs().to_string();
            let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
            for (i, digit) in digits.bytes().enumerate() {
                if i > 0 && (digits.len() - i) % 3 == 0 {
                    grouped.push(',');
                }
                grouped.push(digit as char);
            }
            if value < 0 {
                format!("-{grouped}")
            } else {
                grouped
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cause, REBUILD_MIN, RebuildRow};
    use crate::store::LocalisationRow;
    use crate::tui::testrows::rebuild_bare;
    use serde_json::json;
    use std::collections::HashMap;

    const MIN: i64 = 60_000;
    const NOW: i64 = 1_769_000_000_000;

    /// A walk over rows since `NOW − window_ms`.
    fn walk(rows: &[RebuildRow], window_ms: i64) -> super::Walk {
        super::classify(rows, NOW - window_ms)
    }

    /// A measurement row with every optional column NULL, at `ts_ms`,
    /// carrying a ≥-threshold rewrite — the base each fixture builds on.
    fn rewrite(ts_ms: i64) -> RebuildRow {
        let mut row = rebuild_bare(ts_ms);
        row.cache_write_total = Some(REBUILD_MIN);
        row
    }

    #[test]
    fn a_lane_predecessor_before_the_window_prevents_phantom_new_prefixes() {
        // The anti-phantom rule (lanes.md / live.mjs:208-215): the
        // window's first request shares its lane with a pre-window
        // row, so it must classify against THAT row — not report a
        // fresh prefix every window.
        let mut old = rewrite(NOW - 35 * MIN);
        old.session_id = Some("ses-a".into());
        old.tools_hash = Some("tools-1".into());
        old.system_hash = Some("sys-a".into());
        old.req_messages = Some(40);
        let mut fresh = rewrite(NOW - 5 * MIN);
        fresh.session_id = Some("ses-a".into());
        fresh.tools_hash = Some("tools-1".into());
        fresh.system_hash = Some("sys-a".into());
        fresh.req_messages = Some(42); // mid-history: nothing else differs

        let walked = walk(&[old, fresh], 30 * MIN);
        assert_eq!(walked.window_rows, 1, "only the in-window row counts");
        assert_eq!(walked.events.len(), 1);
        assert_eq!(walked.events[0].cause, Cause::MidHistory);
    }

    #[test]
    fn a_sessions_first_request_is_a_new_prefix() {
        let mut first = rewrite(NOW - 5 * MIN);
        first.session_id = Some("ses-a".into());
        first.tools_hash = Some("tools-1".into());
        let walked = walk(&[first], 30 * MIN);
        assert_eq!(walked.events[0].cause, Cause::NewPrefix);
        assert_eq!(walked.events[0].detail, None);
    }

    #[test]
    fn a_concurrent_lane_is_a_new_prefix_an_abandoned_one_a_tool_change() {
        // summarise.mjs:546-557: the first request of a NEW lane is a
        // tool-set change only when the previous lane never returns;
        // a lane that alternates back is a concurrent subagent lane.
        let mut main = rewrite(NOW - 60 * MIN);
        main.session_id = Some("ses-a".into());
        main.tools_hash = Some("tools-1".into());
        main.req_messages = Some(40);

        let mut side = rewrite(NOW - 50 * MIN);
        side.session_id = Some("ses-a".into());
        side.tools_hash = Some("tools-2".into());
        side.req_messages = Some(4);

        let mut quiet = rewrite(NOW - 40 * MIN);
        quiet.session_id = Some("ses-a".into());
        quiet.cache_write_total = Some(0); // not a rebuild itself

        // tools-1 comes back: the side lane was concurrent, and the
        // main conversation continued — nothing was invalidated.
        let mut back = quiet.clone();
        back.tools_hash = Some("tools-1".into());
        back.req_messages = Some(42);
        let walked = walk(&[main.clone(), side.clone(), back], 90 * MIN);
        assert_eq!(walked.events.len(), 2, "main and side each rewrote");
        assert_eq!(
            walked.events[0].cause,
            Cause::NewPrefix,
            "main's first turn"
        );
        assert_eq!(
            walked.events[1].cause,
            Cause::NewPrefix,
            "the concurrent lane"
        );

        // tools-1 never comes back: a one-way change (the follow-up
        // rides the NEW lane, whose predecessor is the side row).
        let mut stayed = quiet;
        stayed.tools_hash = Some("tools-2".into());
        stayed.req_messages = Some(6);
        let walked = walk(&[main, side, stayed], 90 * MIN);
        assert_eq!(
            walked.events[1].cause,
            Cause::ToolSet,
            "the replaced lane's rewrite is a tool-set change"
        );
    }

    #[test]
    fn a_gap_past_the_cache_ttl_is_an_idle_expiry() {
        let mut earlier = rewrite(NOW - 241 * MIN);
        earlier.session_id = Some("ses-a".into());
        earlier.tools_hash = Some("tools-1".into());
        earlier.system_hash = Some("sys-a".into());
        let mut resume = rewrite(NOW - MIN);
        resume.session_id = Some("ses-a".into());
        resume.tools_hash = Some("tools-1".into());
        resume.system_hash = Some("sys-a".into());

        let walked = walk(&[earlier.clone(), resume.clone()], 30 * MIN);
        assert_eq!(walked.events[0].cause, Cause::Idle);
        assert_eq!(walked.events[0].detail.as_deref(), Some("240 min"));

        // Just inside the TTL: not an expiry, and nothing else differs
        // — a mid-history change. The earlier turn is not itself a
        // rewrite, so the walk yields exactly one event.
        let mut earlier = rebuild_bare(NOW - 59 * MIN);
        earlier.session_id = Some("ses-a".into());
        earlier.tools_hash = Some("tools-1".into());
        earlier.system_hash = Some("sys-a".into());
        earlier.cache_write_total = Some(0);
        let walked = walk(&[earlier, resume], 60 * MIN);
        assert_eq!(walked.events[0].cause, Cause::MidHistory);
    }

    #[test]
    fn a_generation_bump_is_a_compaction_even_mid_collapse() {
        let mut prev = rebuild_bare(NOW - 10 * MIN);
        prev.session_id = Some("ses-a".into());
        prev.tools_hash = Some("tools-1".into());
        prev.req_messages = Some(120);
        prev.compact_generations = Some(2);
        prev.cache_write_total = Some(0);
        let mut compacted = rewrite(NOW - 5 * MIN);
        compacted.session_id = Some("ses-a".into());
        compacted.tools_hash = Some("tools-1".into());
        compacted.req_messages = Some(6); // the collapse would read as a subagent…
        compacted.compact_generations = Some(3); // …but the marker is the fact

        let walked = walk(&[prev, compacted], 30 * MIN);
        assert_eq!(walked.events[0].cause, Cause::Compaction);
        assert_eq!(walked.events[0].detail.as_deref(), Some("generation 3"));
    }

    #[test]
    fn a_message_count_collapse_is_a_subagent_start() {
        // summarise.mjs:474-480: at most 8 messages after more than
        // twice as many — a sidechain with its own prefix, nothing to
        // reuse. The system hash may differ between the two
        // conversations; the subagent test fires first.
        let mut main = rebuild_bare(NOW - 10 * MIN);
        main.session_id = Some("ses-a".into());
        main.tools_hash = Some("tools-1".into());
        main.req_messages = Some(40);
        main.system_hash = Some("sys-main".into());
        main.cache_write_total = Some(0);
        let mut subagent = rewrite(NOW - 5 * MIN);
        subagent.session_id = Some("ses-a".into());
        subagent.tools_hash = Some("tools-1".into());
        subagent.req_messages = Some(4);
        subagent.system_hash = Some("sys-subagent".into());

        let walked = walk(&[main, subagent], 30 * MIN);
        assert_eq!(walked.events[0].cause, Cause::Subagent);
        assert_eq!(walked.events[0].detail.as_deref(), Some("40 → 4 messages"));
    }

    #[test]
    fn a_changed_system_hash_is_localised_by_the_walk() {
        let mut prev = rewrite(NOW - 10 * MIN);
        prev.id = 11;
        prev.session_id = Some("ses-a".into());
        prev.tools_hash = Some("tools-1".into());
        prev.req_messages = Some(40);
        prev.system_hash = Some("sys-1".into());
        prev.system_chars = Some(43_696);
        prev.system_blocks = Some(json!([
            {"hash": "b1", "chars": 1_000},
            {"hash": "b2", "chars": 42_696},
        ]));
        prev.cache_write_total = Some(0); // the earlier turn is not itself a rebuild
        let mut changed = rewrite(NOW - 5 * MIN);
        changed.id = 12;
        changed.session_id = Some("ses-a".into());
        changed.tools_hash = Some("tools-1".into());
        changed.req_messages = Some(42);
        changed.system_hash = Some("sys-2".into());
        changed.system_chars = Some(43_801);
        changed.system_blocks = Some(json!([
            {"hash": "b1", "chars": 1_000},
            {"hash": "b2*", "chars": 42_801},
        ]));

        let mut walked = walk(&[prev.clone(), changed], 30 * MIN);
        assert_eq!(walked.events.len(), 1);
        assert_eq!(walked.events[0].cause, Cause::SystemPrompt);
        assert_eq!(
            walked.events[0].detail, None,
            "the detail lands with localise"
        );

        // No ladders fetched: the detail claims the chars and the
        // block, and no position.
        super::localise(&mut walked.events, &HashMap::new());
        assert_eq!(
            walked.events[0].detail.as_deref(),
            Some("43,696 → 43,801 chars; block 1 (42,696 → 42,801 chars)")
        );

        // With the ladders: block 1, in the last 8 bytes — the README's
        // localised shape. The predecessor's tail matches every rung
        // but the first, so the change lands in the last 8 bytes.
        let ladders: HashMap<_, _> = [
            (
                11,
                LocalisationRow {
                    id: 11,
                    system_ladder: Some(vec!["r1".into(), "r2".into()]),
                    system_tail: Some(vec!["t8".into(), "t16".into(), "t24".into()]),
                    system_change: None,
                },
            ),
            (
                12,
                LocalisationRow {
                    id: 12,
                    system_ladder: Some(vec!["r1".into(), "r2".into()]),
                    system_tail: Some(vec!["t8*".into(), "t16".into(), "t24".into()]),
                    system_change: None,
                },
            ),
        ]
        .into_iter()
        .collect();
        super::localise(&mut walked.events, &ladders);
        assert_eq!(
            walked.events[0].detail.as_deref(),
            Some("43,696 → 43,801 chars; block 1 (42,696 → 42,801 chars), in the last 8 bytes")
        );

        // A stored capture-time localisation wins over the re-derived
        // one (summarise.mjs:385-387) — imported ctp rows carry it.
        let mut stored = HashMap::new();
        stored.insert(
            12,
            LocalisationRow {
                id: 12,
                system_ladder: None,
                system_tail: None,
                system_change: Some(json!({"delta": 105, "where": "block 1, in the last 8 bytes"})),
            },
        );
        super::localise(&mut walked.events, &stored);
        assert_eq!(
            walked.events[0].detail.as_deref(),
            Some("43,696 → 43,801 chars; block 1, in the last 8 bytes")
        );
    }

    #[test]
    fn the_ladder_window_bounds_a_change_to_one_step() {
        let mut prev = rebuild_bare(NOW - 10 * MIN);
        prev.id = 21;
        prev.session_id = Some("ses-a".into());
        prev.tools_hash = Some("tools-1".into());
        prev.req_messages = Some(40);
        prev.system_hash = Some("sys-1".into());
        prev.system_chars = Some(20_000);
        prev.cache_write_total = Some(0);
        let mut changed = prev.clone();
        changed.id = 22;
        changed.ts_ms = NOW - 5 * MIN;
        changed.system_hash = Some("sys-2".into());
        changed.cache_write_total = Some(REBUILD_MIN);
        let ladders: HashMap<_, _> = [
            (
                21,
                LocalisationRow {
                    id: 21,
                    system_ladder: Some(vec!["a".into(), "b".into(), "c".into()]),
                    system_tail: None,
                    system_change: None,
                },
            ),
            (
                22,
                LocalisationRow {
                    id: 22,
                    system_ladder: Some(vec!["a".into(), "b*".into(), "c".into()]),
                    system_tail: None,
                    system_change: None,
                },
            ),
        ]
        .into_iter()
        .collect();

        let mut walked = walk(&[prev, changed], 30 * MIN);
        super::localise(&mut walked.events, &ladders);
        assert_eq!(
            walked.events[0].detail.as_deref(),
            Some("20,000 → 20,000 chars; between bytes 8192 and 16384")
        );
    }

    #[test]
    fn matching_rungs_everywhere_beyond_the_tail_is_named() {
        let mut prev = rebuild_bare(NOW - 10 * MIN);
        prev.id = 31;
        prev.session_id = Some("ses-a".into());
        prev.tools_hash = Some("tools-1".into());
        prev.req_messages = Some(40);
        prev.system_hash = Some("sys-1".into());
        prev.system_chars = Some(130_000);
        prev.system_blocks = Some(json!([{"hash": "b1", "chars": 130_000}]));
        prev.cache_write_total = Some(0);
        let mut changed = prev.clone();
        changed.id = 32;
        changed.ts_ms = NOW - 5 * MIN;
        changed.system_hash = Some("sys-2".into());
        changed.system_blocks = Some(json!([{"hash": "b1*", "chars": 130_000}]));
        changed.cache_write_total = Some(REBUILD_MIN);
        // A full-length tail (44 rungs for 130 000 units) where every
        // rung matches: the change is beyond the tail's 1024-byte reach.
        let rungs: Vec<String> = (0..44).map(|i| format!("t{i}")).collect();
        let ladders: HashMap<_, _> = [
            (
                31,
                LocalisationRow {
                    id: 31,
                    system_ladder: Some(vec!["l1".into()]),
                    system_tail: Some(rungs.clone()),
                    system_change: None,
                },
            ),
            (
                32,
                LocalisationRow {
                    id: 32,
                    system_ladder: Some(vec!["l1".into()]),
                    system_tail: Some(rungs.clone()),
                    system_change: None,
                },
            ),
        ]
        .into_iter()
        .collect();

        let mut walked = walk(&[prev, changed], 30 * MIN);
        super::localise(&mut walked.events, &ladders);
        assert_eq!(
            walked.events[0].detail.as_deref(),
            Some(
                "130,000 → 130,000 chars; block 0 (130,000 → 130,000 chars), beyond the last 1024 bytes"
            )
        );
    }

    #[test]
    fn the_threshold_and_unknown_writes_are_counted_not_guessed() {
        let mut just_under = rewrite(NOW - 9 * MIN);
        just_under.session_id = Some("ses-a".into());
        just_under.tools_hash = Some("tools-1".into());
        just_under.cache_write_total = Some(REBUILD_MIN - 1);
        let mut zero = rewrite(NOW - 8 * MIN);
        zero.session_id = Some("ses-a".into());
        zero.tools_hash = Some("tools-1".into());
        zero.cache_write_total = Some(0); // a real zero is not a rebuild
        let mut unknown = rewrite(NOW - 7 * MIN);
        unknown.session_id = Some("ses-a".into());
        unknown.tools_hash = Some("tools-2".into());
        unknown.cache_write_total = None; // an unknown rewrite is not a zero
        let mut real = rewrite(NOW - 5 * MIN);
        real.session_id = Some("ses-a".into());
        real.tools_hash = Some("tools-1".into());
        real.cache_write_total = Some(REBUILD_MIN);
        real.system_hash = Some("sys-2".into());

        let walked = walk(&[just_under, zero, unknown, real], 30 * MIN);
        assert_eq!(walked.window_rows, 4);
        assert_eq!(walked.unmeasured, 1);
        assert_eq!(walked.events.len(), 1, "only the ≥-threshold rewrite");
        let agg = super::aggregate(walked);
        assert_eq!(agg.rebuilds, 1);
        assert_eq!(agg.measured, 3);
        assert_eq!(agg.unmeasured, 1);
    }

    #[test]
    fn the_cause_table_orders_by_count_and_caps_at_five() {
        let mut rows = Vec::new();
        let mut ts = NOW - 100 * MIN;
        // Two idle expiries: a quiet lane resuming past the TTL, its
        // earlier turn too small to count as a rebuild itself.
        for session in ["ses-idle-1", "ses-idle-2"] {
            let mut prev = rebuild_bare(ts);
            prev.session_id = Some(session.into());
            prev.tools_hash = Some("tools-1".into());
            prev.system_hash = Some("sys-a".into());
            prev.cache_write_total = Some(0);
            rows.push(prev);
            let mut resume = rewrite(ts + 61 * MIN);
            resume.session_id = Some(session.into());
            resume.tools_hash = Some("tools-1".into());
            resume.system_hash = Some("sys-a".into());
            rows.push(resume);
            ts += MIN;
        }
        // One compaction.
        let mut prev = rebuild_bare(NOW - 20 * MIN);
        prev.session_id = Some("ses-compact".into());
        prev.tools_hash = Some("tools-1".into());
        prev.compact_generations = Some(1);
        prev.cache_write_total = Some(0);
        rows.push(prev);
        let mut compacted = rewrite(NOW - 19 * MIN);
        compacted.session_id = Some("ses-compact".into());
        compacted.tools_hash = Some("tools-1".into());
        compacted.compact_generations = Some(2);
        rows.push(compacted);
        // One mid-history change (its lane's first turn is a new
        // prefix, itself a counted rewrite).
        let mut first = rewrite(NOW - 10 * MIN);
        first.session_id = Some("ses-mid".into());
        first.tools_hash = Some("tools-1".into());
        first.system_hash = Some("sys-a".into());
        rows.push(first);
        let mut second = rewrite(NOW - 9 * MIN);
        second.session_id = Some("ses-mid".into());
        second.tools_hash = Some("tools-1".into());
        second.system_hash = Some("sys-a".into());
        rows.push(second);

        let agg = super::aggregate(walk(&rows, 120 * MIN));
        assert_eq!(agg.rebuilds, 5);
        let labels: Vec<_> = agg.causes.iter().map(|(cause, _)| cause.label()).collect();
        assert_eq!(
            labels,
            vec![
                "idle — 1h cache TTL expired",
                "compaction",
                "mid-history change",
                "new prefix / first turn",
            ],
            "count desc, then label asc"
        );

        // The cap: a walk holding all seven causes keeps five rows
        // (live.mjs:243's slice(0, 5)).
        let every_cause = [
            Cause::NewPrefix,
            Cause::Idle,
            Cause::ToolSet,
            Cause::Compaction,
            Cause::Subagent,
            Cause::SystemPrompt,
            Cause::MidHistory,
        ];
        let mut walked = super::Walk::default();
        for cause in every_cause {
            walked.events.push(super::RebuildEvent {
                ts_ms: NOW,
                session: "ses-a".into(),
                cause,
                detail: None,
                rewritten: REBUILD_MIN,
                system: None,
            });
        }
        let agg = super::aggregate(walked);
        assert_eq!(agg.rebuilds, 7);
        assert_eq!(agg.causes.len(), super::MAX_CAUSE_ROWS);
        assert!(
            agg.causes.iter().all(|(_, count)| *count == 1),
            "one row per cause"
        );
    }

    #[test]
    fn a_changed_block_count_is_named_without_a_block_index() {
        // proxy.mjs:681-683: when the block map changes length, the
        // count itself is the localisation — there is no block i to
        // name against a mismatched pair.
        let mut prev = rebuild_bare(NOW - 10 * MIN);
        prev.id = 41;
        prev.session_id = Some("ses-a".into());
        prev.tools_hash = Some("tools-1".into());
        prev.req_messages = Some(40);
        prev.system_hash = Some("sys-1".into());
        prev.system_chars = Some(9_000);
        prev.system_blocks = Some(json!([
            {"hash": "b1", "chars": 1_000},
            {"hash": "b2", "chars": 8_000},
        ]));
        prev.cache_write_total = Some(0);
        let mut changed = rewrite(NOW - 5 * MIN);
        changed.id = 42;
        changed.session_id = Some("ses-a".into());
        changed.tools_hash = Some("tools-1".into());
        changed.req_messages = Some(42);
        changed.system_hash = Some("sys-2".into());
        changed.system_chars = Some(9_500);
        changed.system_blocks = Some(json!([{"hash": "b1", "chars": 9_500}]));

        let mut walked = walk(&[prev, changed], 30 * MIN);
        super::localise(&mut walked.events, &HashMap::new());
        assert_eq!(
            walked.events[0].detail.as_deref(),
            Some("9,000 → 9,500 chars; block count 2 → 1"),
            "no position is claimed without rungs:\n{}",
            walked.events[0].detail.as_deref().unwrap_or("")
        );
    }

    #[test]
    fn null_tool_hashes_share_one_lane_like_live_mjs() {
        // live.mjs keys a NULL toolsHash under `?` — one lane, so a
        // hash-less history still classifies by its other fields
        // instead of every row reading as a fresh prefix.
        let mut prev = rebuild_bare(NOW - 10 * MIN);
        prev.session_id = Some("ses-a".into());
        prev.system_hash = Some("sys-a".into());
        prev.req_messages = Some(40);
        prev.cache_write_total = Some(0);
        let mut next = rewrite(NOW - 5 * MIN);
        next.session_id = Some("ses-a".into());
        next.system_hash = Some("sys-a".into());
        next.req_messages = Some(42);

        let walked = walk(&[prev, next], 30 * MIN);
        assert_eq!(walked.events.len(), 1);
        assert_eq!(walked.events[0].cause, Cause::MidHistory);
    }

    #[test]
    fn out_of_order_arrival_classifies_by_timestamp() {
        // Rows read newest-first (out of order): the walk still sees
        // the older row as the lane predecessor.
        let mut older = rebuild_bare(NOW - 10 * MIN);
        older.session_id = Some("ses-a".into());
        older.tools_hash = Some("tools-1".into());
        older.compact_generations = Some(1);
        older.cache_write_total = Some(0);
        let mut newer = rewrite(NOW - 5 * MIN);
        newer.session_id = Some("ses-a".into());
        newer.tools_hash = Some("tools-1".into());
        newer.compact_generations = Some(2);

        let walked = walk(&[newer, older], 30 * MIN);
        assert_eq!(walked.events.len(), 1);
        assert_eq!(walked.events[0].cause, Cause::Compaction);
    }

    // ── the narrow read vs the full-row read (the read's proof) ──────

    /// The rebuild read cannot quietly regress into materialising full
    /// rows: walking the narrow read must classify identically to
    /// walking the projection of a full-row read over the same ledger —
    /// including the rows the narrow read's `kind IS NULL` filter
    /// skips, which is exactly what this proves the walk is blind to.
    #[test]
    fn the_narrow_read_classifies_identically_to_the_full_row_read() {
        use crate::store::Store;
        use crate::tui::testrows::{as_rebuild_rows, bare, kind_row};

        let store = Store::open(":memory:").expect("scratch store");
        // A lane with every cause the classifier can name, plus the
        // proxy-written rows the narrow read must never see. Timestamps
        // are explicit because the walk's rules are gap-based: the
        // idle resume sits two hours past its lane predecessor, the
        // rest a minute apart, and `early` is the pre-window baseline
        // the anti-phantom rule classifies against.
        let mut rows = Vec::new();
        let lane = |ts: i64, session: &str, tools: &str, writes: Option<i64>| {
            let mut row = bare(ts);
            row.session_id = Some(session.to_owned());
            row.tools_hash = Some(tools.to_owned());
            row.cache_write_total = writes;
            row
        };
        let mut early = lane(0, "ses-a", "tools-1", Some(0));
        early.system_hash = Some("sys-a".into());
        rows.push(early);
        let mut first = lane(60_000, "ses-a", "tools-1", Some(REBUILD_MIN));
        first.system_hash = Some("sys-a".into());
        first.req_messages = Some(40);
        rows.push(first);
        rows.push(lane(120_000, "ses-a", "tools-1", None)); // unknown rewrite
        let mut resumed = lane(120_000 + 120 * MIN, "ses-a", "tools-1", Some(REBUILD_MIN));
        resumed.system_hash = Some("sys-a".into());
        resumed.req_messages = Some(42);
        rows.push(resumed);
        let mut compacted = lane(120_000 + 121 * MIN, "ses-a", "tools-1", Some(REBUILD_MIN));
        compacted.compact_generations = Some(2);
        compacted.req_messages = Some(6);
        compacted.system_hash = Some("sys-a".into());
        rows.push(compacted);
        let mut changed = lane(120_000 + 122 * MIN, "ses-a", "tools-1", Some(REBUILD_MIN));
        changed.system_hash = Some("sys-b".into());
        changed.system_chars = Some(40_105);
        changed.system_blocks = Some(json!([{"hash": "b1", "chars": 40_105}]));
        changed.req_messages = Some(44);
        rows.push(changed);
        let mut mid = lane(120_000 + 123 * MIN, "ses-a", "tools-1", Some(REBUILD_MIN));
        mid.system_hash = Some("sys-b".into());
        mid.req_messages = Some(46);
        rows.push(mid);
        // A concurrent lane that comes back, and one that does not.
        rows.push(lane(
            120_000 + 124 * MIN,
            "ses-a",
            "tools-2",
            Some(REBUILD_MIN),
        ));
        let mut back = lane(120_000 + 125 * MIN, "ses-a", "tools-1", Some(REBUILD_MIN));
        back.system_hash = Some("sys-b".into());
        back.req_messages = Some(48);
        rows.push(back);
        rows.push(lane(
            120_000 + 126 * MIN,
            "ses-a",
            "tools-3",
            Some(REBUILD_MIN),
        ));
        // Another session: a subagent start.
        let mut main = lane(130 * MIN, "ses-b", "tools-1", Some(0));
        main.req_messages = Some(40);
        main.system_hash = Some("sys-main".into());
        rows.push(main);
        let mut subagent = lane(131 * MIN, "ses-b", "tools-1", Some(REBUILD_MIN));
        subagent.req_messages = Some(4);
        subagent.system_hash = Some("sys-sub".into());
        rows.push(subagent);
        // Proxy-written rows: never measurements, never lane rows —
        // their timestamps sit past every lane row and do not matter,
        // because the walk never sees them at all.
        rows.push(kind_row(50_000_000, crate::store::RowKind::Error));
        rows.push(kind_row(50_000_001, crate::store::RowKind::Blocked));

        store
            .record_requests(&rows)
            .expect("seed the scratch ledger");

        // OLD shape: the full-row materialisation, projected onto the
        // walk's fields with the kind filter applied here.
        let full = store.requests_since(0, 1_000).expect("full read");
        let via_old = super::classify(&as_rebuild_rows(&full), 60_000);
        // NEW path: the narrow read (kind already filtered in SQL).
        let narrow = store.rebuild_rows_since(0, 1_000).expect("narrow read");
        assert_eq!(
            narrow.len(),
            rows.len() - 2,
            "the two proxy-written rows never reach the narrow read"
        );
        let via_new = super::classify(&narrow, 60_000);

        assert_eq!(
            via_old, via_new,
            "the narrow read must classify identically"
        );

        // Spot figures pinning WHICH rules had to survive the switch —
        // equal-but-wrong would still fail here.
        assert_eq!(via_new.window_rows, 11);
        assert_eq!(via_new.unmeasured, 1);
        let causes: Vec<_> = via_new.events.iter().map(|event| event.cause).collect();
        assert_eq!(
            causes,
            vec![
                Cause::MidHistory, // ses-a's first IN-WINDOW row, against its pre-window baseline
                Cause::Idle,       // the 120-minute resume
                Cause::Compaction, // generation 0 → 2
                Cause::SystemPrompt,
                Cause::MidHistory,
                Cause::NewPrefix,  // the concurrent tools-2 lane
                Cause::MidHistory, // back on tools-1, nothing changed
                Cause::ToolSet,    // tools-3 replaces a lane that never returns
                Cause::Subagent,   // ses-b's message collapse
            ],
            "equal-but-wrong would still fail here"
        );
        // The pre-window baseline did its job: ses-a's first in-window
        // row is NOT a phantom new prefix — it classified against the
        // row before the window.
        assert_eq!(via_new.events[0].session, "ses-a");
        let system = via_new
            .events
            .iter()
            .find_map(|event| event.system.clone())
            .expect("the system change carries its localisation inputs");
        assert_eq!(system.chars, Some(40_105));
    }
}
