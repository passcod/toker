//! Capture-time system-prompt change localisation: ctp's
//! `localiseSystemChange` and `loggableShape`, with the baseline read from
//! the ledger instead of an in-memory map.
//!
//! Each anthropic measurement compares its system prompt with the previous
//! row in its lane (session × tools hash; a session is not a cache entry).
//! Where the prompt changed, the row records where (`system_change`, as
//! `{delta, where}`) and keeps its ladders; where it did not, the row drops
//! them. The ladders are bulky (in ctp they tripled every row) and answer a
//! question that arises on a small fraction of requests, so they are kept
//! only where that question arises, plus on a lane's first row, which is
//! the baseline its first change will need.
//!
//! The baseline lives in the ledger, not the lane table and not memory.
//! ctp kept it in a 256-entry in-process map, so every restart and every
//! evicted lane lost it, and the next change in that lane went unlocalised.
//! The lane table is pruned to 30 days and 4000 lanes; the ledger is
//! insert-only and never pruned. The rungs are looked up by the
//! predecessor's system hash, so they are always the rungs of the very text
//! the predecessor carried, whichever row kept them: a row only drops its
//! ladders once its hash has been seen to match its predecessor's, so the
//! row that introduced a prompt to a lane still has them.
//!
//! Accounting must never break a session: every failure here, a store
//! error or a panic, records no change and keeps the ladders, so the row
//! errs towards carrying more, never towards claiming the prompt held.

use std::panic::AssertUnwindSafe;

use serde_json::{Value, json};

use crate::ir::AnthropicShape;
use crate::ir::anthropic::{LADDER_STEP, prefix_rungs, tail_offsets};
use crate::store::{LaneSystemRow, Store};

/// What capture records about a request's system prompt.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SystemCapture {
    /// The localised change, `{delta, where}`, when the prompt changed
    /// against the lane's previous row and the baseline's rungs were found.
    pub change: Option<Value>,
    /// Whether the row keeps its ladders: false only when the prompt was
    /// seen to match the lane's previous row.
    pub keep_ladders: bool,
}

impl SystemCapture {
    /// Nothing compared: no change claimed, ladders kept.
    const UNCOMPARED: SystemCapture = SystemCapture {
        change: None,
        keep_ladders: true,
    };
}

/// Compare `shape`'s system prompt with the lane's previous row in `store`.
///
/// No session means no lane (the lane rule), so nothing is compared and
/// the ladders stay. Runs under `catch_unwind`; see the module docs.
pub(crate) fn capture(
    store: &Store,
    session_id: Option<&str>,
    shape: &AnthropicShape,
) -> SystemCapture {
    let Some(session_id) = session_id else {
        return SystemCapture::UNCOMPARED;
    };
    match std::panic::catch_unwind(AssertUnwindSafe(|| compare(store, session_id, shape))) {
        Ok(Ok(capture)) => capture,
        Ok(Err(error)) => {
            tracing::error!(%error, "system-change baseline read failed");
            SystemCapture::UNCOMPARED
        }
        Err(_) => {
            tracing::error!("system-change localisation panicked");
            SystemCapture::UNCOMPARED
        }
    }
}

fn compare(
    store: &Store,
    session_id: &str,
    shape: &AnthropicShape,
) -> crate::store::Result<SystemCapture> {
    let tools_hash = shape.tools_hash.as_str();
    let Some(prev) = store.lane_system_predecessor(session_id, tools_hash)? else {
        // The lane's first row: kept whole, as its baseline.
        return Ok(SystemCapture::UNCOMPARED);
    };
    if prev.system_hash == shape.system_hash {
        return Ok(SystemCapture {
            change: None,
            keep_ladders: false,
        });
    }
    let change = baseline(store, session_id, tools_hash, &prev)?
        .map(|baseline| localise(&baseline.side(), &Side::of(shape)));
    Ok(SystemCapture {
        change,
        keep_ladders: true,
    })
}

/// The predecessor's prompt as the localisation needs it.
struct Baseline {
    chars: i64,
    blocks: Vec<(String, i64)>,
    ladder: Vec<String>,
    tail: Vec<String>,
}

impl Baseline {
    fn side(&self) -> Side<'_> {
        Side {
            chars: self.chars,
            blocks: self
                .blocks
                .iter()
                .map(|(hash, chars)| (hash.as_str(), *chars))
                .collect(),
            ladder: &self.ladder,
            tail: &self.tail,
        }
    }
}

/// The predecessor's length, blocks and rungs, or `None` where any of them
/// is missing or its rungs were cut to another geometry.
fn baseline(
    store: &Store,
    session_id: &str,
    tools_hash: &str,
    prev: &LaneSystemRow,
) -> crate::store::Result<Option<Baseline>> {
    let Some(chars) = prev.system_chars else {
        return Ok(None);
    };
    let Some(blocks) = prev.system_blocks.as_ref().and_then(parse_blocks) else {
        return Ok(None);
    };
    let length = usize::try_from(chars).unwrap_or(0);
    let want_ladder = prefix_rungs(length);
    let want_tail = tail_offsets(length).len();
    let (ladder, tail) = if want_ladder == 0 && want_tail == 0 {
        // A prompt too short to cut a rung stores none: nothing to find.
        (Vec::new(), Vec::new())
    } else {
        let Some((ladder, tail)) = store.lane_system_ladders(
            session_id,
            tools_hash,
            &prev.system_hash,
            (prev.ts_ms, prev.id),
        )?
        else {
            return Ok(None);
        };
        (ladder.unwrap_or_default(), tail.unwrap_or_default())
    };
    if ladder.len() != want_ladder || tail.len() != want_tail {
        return Ok(None);
    }
    Ok(Some(Baseline {
        chars,
        blocks,
        ladder,
        tail,
    }))
}

/// A stored block map, `[{hash, chars}]`; `None` if any entry lacks either.
fn parse_blocks(value: &Value) -> Option<Vec<(String, i64)>> {
    value
        .as_array()?
        .iter()
        .map(|block| {
            Some((
                block.get("hash")?.as_str()?.to_owned(),
                block.get("chars")?.as_i64()?,
            ))
        })
        .collect()
}

/// One side of a comparison.
pub(crate) struct Side<'a> {
    /// Total length in UTF-16 units.
    pub chars: i64,
    /// Per-block `(hash, chars)`, in order.
    pub blocks: Vec<(&'a str, i64)>,
    /// Prefix rungs.
    pub ladder: &'a [String],
    /// Suffix rungs.
    pub tail: &'a [String],
}

impl<'a> Side<'a> {
    fn of(shape: &'a AnthropicShape) -> Side<'a> {
        Side {
            chars: i64::try_from(shape.system_chars).unwrap_or(i64::MAX),
            blocks: shape
                .system_blocks
                .iter()
                .map(|block| {
                    (
                        block.hash.as_str(),
                        i64::try_from(block.chars).unwrap_or(i64::MAX),
                    )
                })
                .collect(),
            ladder: &shape.system_ladder,
            tail: &shape.system_tail,
        }
    }
}

/// Bound a system-prompt change between two prompts known to differ: which
/// blocks changed, then the first differing 8 KiB prefix step, or failing
/// that the tail window. ctp's `localiseSystemChange`, string for string,
/// except where its tail index ran past the shorter prompt's offsets and it
/// printed `undefined`; that case is the change lying beyond every shared
/// suffix, and says so.
pub(crate) fn localise(prev: &Side<'_>, cur: &Side<'_>) -> Value {
    let mut parts = Vec::new();
    if prev.blocks.len() != cur.blocks.len() {
        parts.push(format!(
            "block count {} \u{2192} {}",
            prev.blocks.len(),
            cur.blocks.len()
        ));
    } else {
        for (i, (a, b)) in prev.blocks.iter().zip(&cur.blocks).enumerate() {
            if a.0 != b.0 {
                parts.push(format!("block {i} ({} \u{2192} {} chars)", a.1, b.1));
            }
        }
    }

    // The first of the predecessor's rungs the new prompt does not repeat.
    // A rung the new prompt lacks (it shrank) differs too: the text up to
    // it no longer reaches that far unchanged.
    let first_diff = |a: &[String], b: &[String]| {
        a.iter()
            .enumerate()
            .position(|(k, rung)| b.get(k) != Some(rung))
    };
    if let Some(pi) = first_diff(prev.ladder, cur.ladder) {
        parts.push(format!(
            "between bytes {} and {}",
            pi * LADDER_STEP,
            (pi + 1) * LADDER_STEP
        ));
    } else {
        let shorter = usize::try_from(prev.chars.min(cur.chars)).unwrap_or(0);
        let offsets = tail_offsets(shorter);
        let beyond = || format!("beyond the last {} bytes", offsets.last().unwrap_or(&0));
        parts.push(match first_diff(prev.tail, cur.tail) {
            None => beyond(),
            Some(ti) => match offsets.get(ti) {
                None => beyond(),
                Some(&to) => {
                    let from = if ti == 0 { 0 } else { offsets[ti - 1] };
                    if from == 0 {
                        format!("in the last {to} bytes")
                    } else {
                        format!("{from}-{to} bytes from the end")
                    }
                }
            },
        });
    }

    json!({
        "delta": cur.chars - prev.chars,
        "where": parts.join(", "),
    })
}

#[cfg(test)]
mod tests {
    use super::{Side, SystemCapture, capture, localise};
    use crate::ir::{AnthropicShape, Request};
    use crate::store::{RequestRow, Store};
    use serde_json::json;

    /// The shape of a request whose system prompt is `system`, with a
    /// fixed tool set (one lane per session).
    fn shape(system: &str) -> AnthropicShape {
        let body = serde_json::to_vec(&json!({
            "model": "m",
            "system": system,
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .expect("serialise");
        Request::parse(&body)
            .expect("test body parses")
            .anthropic()
            .shape()
    }

    /// A long prompt (two prefix rungs, a full tail) with `end` 21 units
    /// before its end.
    fn prompt(end: &str) -> String {
        format!("{}{end}{}", "x".repeat(20_000), "y".repeat(20))
    }

    /// Capture `shape` in `session`, then record its row as the anthropic
    /// measurement row would: ladders only where capture kept them.
    fn record(store: &Store, ts_ms: i64, session: &str, shape: &AnthropicShape) -> SystemCapture {
        let system = capture(store, Some(session), shape);
        let mut row: RequestRow = crate::tui::testrows::bare(ts_ms);
        row.session_id = Some(session.to_owned());
        row.tools_hash = Some(shape.tools_hash.clone());
        row.system_hash = Some(shape.system_hash.clone());
        row.system_chars = Some(shape.system_chars as i64);
        row.system_blocks = Some(json!(
            shape
                .system_blocks
                .iter()
                .map(|block| json!({"hash": block.hash, "chars": block.chars}))
                .collect::<Vec<_>>()
        ));
        row.system_change = system.change.clone();
        if system.keep_ladders {
            row.system_ladder = Some(json!(shape.system_ladder).to_string());
            row.system_tail = Some(json!(shape.system_tail).to_string());
        }
        store.record_request(&row).expect("record");
        system
    }

    #[test]
    fn a_lane_keeps_ladders_on_its_first_row_and_where_the_prompt_changed() {
        let store = Store::open(":memory:").expect("store");
        let first = record(&store, 1_000, "ses-a", &shape(&prompt("A")));
        assert_eq!(first, SystemCapture::UNCOMPARED, "the lane's first row");

        let same = record(&store, 2_000, "ses-a", &shape(&prompt("A")));
        assert_eq!(
            same,
            SystemCapture {
                change: None,
                keep_ladders: false
            },
            "an unchanged prompt drops its ladders and claims no change"
        );

        // The predecessor dropped its ladders; the first row's are found
        // by hash.
        let changed = record(&store, 3_000, "ses-a", &shape(&prompt("B")));
        assert!(changed.keep_ladders);
        insta::assert_snapshot!(
            changed.change.expect("localised").to_string(),
            @r#"{"delta":0,"where":"block 0 (20021 → 20021 chars), 16-24 bytes from the end"}"#
        );

        // Another session's lane is its own: a first row again.
        let other = record(&store, 4_000, "ses-b", &shape(&prompt("B")));
        assert_eq!(other, SystemCapture::UNCOMPARED);
    }

    #[test]
    fn a_reopened_ledger_still_holds_the_baseline() {
        let dir = std::env::temp_dir().join(format!(
            "toker-system-change-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("toker.db");
        {
            let store = Store::open(&path).expect("store");
            record(&store, 1_000, "ses-a", &shape(&prompt("A")));
            record(&store, 2_000, "ses-a", &shape(&prompt("A")));
        }
        let store = Store::open(&path).expect("reopened store");
        let changed = record(&store, 3_000, "ses-a", &shape(&prompt("B")));
        assert!(
            changed.change.is_some(),
            "the baseline survives the restart"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_baseline_cut_to_another_geometry_localises_nothing() {
        // ctp's first ladders stepped every 2 KiB: the same prompt's rungs
        // are more numerous, and comparing them with today's would name
        // windows they never bounded.
        let store = Store::open(":memory:").expect("store");
        let old = shape(&prompt("A"));
        let mut row = crate::tui::testrows::bare(1_000);
        row.session_id = Some("ses-a".into());
        row.tools_hash = Some(old.tools_hash.clone());
        row.system_hash = Some(old.system_hash.clone());
        row.system_chars = Some(old.system_chars as i64);
        row.system_blocks = Some(json!([{"hash": old.system_blocks[0].hash, "chars": 20_021}]));
        row.system_ladder = Some(json!(["a", "b", "c", "d", "e", "f", "g", "h", "i"]).to_string());
        row.system_tail = Some(json!(old.system_tail).to_string());
        store.record_request(&row).expect("record");

        let changed = capture(&store, Some("ses-a"), &shape(&prompt("B")));
        assert_eq!(changed, SystemCapture::UNCOMPARED);
    }

    #[test]
    fn no_session_or_a_failed_read_records_nothing_and_keeps_the_ladders() {
        let store = Store::open(":memory:").expect("store");
        record(&store, 1_000, "ses-a", &shape(&prompt("A")));
        assert_eq!(
            capture(&store, None, &shape(&prompt("A"))),
            SystemCapture::UNCOMPARED,
            "no session, no lane"
        );
        store
            .execute_for_test("ALTER TABLE requests RENAME TO requests_gone")
            .expect("break the ledger");
        assert_eq!(
            capture(&store, Some("ses-a"), &shape(&prompt("A"))),
            SystemCapture::UNCOMPARED,
            "a read failure claims no change and drops nothing"
        );
    }

    fn rungs(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_tail_change_names_its_window_and_block() {
        let ladder = rungs(&["r1", "r2"]);
        let prev_tail = rungs(&["t8", "t16", "t24"]);
        let cur_tail = rungs(&["t8", "t16*", "t24*"]);
        let prev = Side {
            chars: 20_000,
            blocks: vec![("b0", 100), ("b1", 19_900)],
            ladder: &ladder,
            tail: &prev_tail,
        };
        let cur = Side {
            chars: 20_007,
            blocks: vec![("b0", 100), ("b1*", 19_907)],
            ladder: &ladder,
            tail: &cur_tail,
        };
        insta::assert_snapshot!(
            localise(&prev, &cur).to_string(),
            @r#"{"delta":7,"where":"block 1 (19900 → 19907 chars), 8-16 bytes from the end"}"#
        );
    }

    #[test]
    fn a_prefix_change_names_its_step_and_a_block_count_change_its_counts() {
        let prev_ladder = rungs(&["r1", "r2", "r3"]);
        let cur_ladder = rungs(&["r1", "r2*", "r3*"]);
        let tail = rungs(&["t8"]);
        let prev = Side {
            chars: 30_000,
            blocks: vec![("b0", 30_000)],
            ladder: &prev_ladder,
            tail: &tail,
        };
        let cur = Side {
            chars: 29_000,
            blocks: vec![("b0*", 100), ("b1", 28_900)],
            ladder: &cur_ladder,
            tail: &tail,
        };
        insta::assert_snapshot!(
            localise(&prev, &cur).to_string(),
            @r#"{"delta":-1000,"where":"block count 1 → 2, between bytes 8192 and 16384"}"#
        );
    }

    #[test]
    fn a_shrunk_prompt_whose_rungs_all_match_is_bounded_at_its_end() {
        // ctp's semantics: the predecessor's rung the shorter prompt lacks
        // counts as differing.
        let prev_ladder = rungs(&["r1", "r2"]);
        let cur_ladder = rungs(&["r1"]);
        let tail: Vec<String> = Vec::new();
        let prev = Side {
            chars: 20_000,
            blocks: vec![("b0", 20_000)],
            ladder: &prev_ladder,
            tail: &tail,
        };
        let cur = Side {
            chars: 12_000,
            blocks: vec![("b0*", 12_000)],
            ladder: &cur_ladder,
            tail: &tail,
        };
        insta::assert_snapshot!(
            localise(&prev, &cur).to_string(),
            @r#"{"delta":-8000,"where":"block 0 (20000 → 12000 chars), between bytes 8192 and 16384"}"#
        );
    }

    #[test]
    fn matching_suffixes_put_the_change_beyond_the_shorter_prompts_reach() {
        // Every shared suffix matches: beyond the tail. And where the
        // predecessor's tail outruns the shorter prompt's offsets, ctp
        // printed `undefined`; the bound is the same "beyond".
        let ladder: Vec<String> = Vec::new();
        let prev_tail = rungs(&["t8", "t16", "t24"]);
        let cur_tail = rungs(&["t8", "t16"]);
        let prev = Side {
            chars: 24,
            blocks: vec![("b0", 24)],
            ladder: &ladder,
            tail: &prev_tail,
        };
        let cur = Side {
            chars: 16,
            blocks: vec![("b0*", 16)],
            ladder: &ladder,
            tail: &cur_tail,
        };
        insta::assert_snapshot!(
            localise(&prev, &cur).to_string(),
            @r#"{"delta":-8,"where":"block 0 (24 → 16 chars), beyond the last 16 bytes"}"#
        );
        let same = Side {
            chars: 24,
            blocks: vec![("b0*", 24)],
            ladder: &ladder,
            tail: &prev_tail,
        };
        insta::assert_snapshot!(
            localise(&prev, &same).to_string(),
            @r#"{"delta":0,"where":"block 0 (24 → 24 chars), beyond the last 24 bytes"}"#
        );
    }
}
