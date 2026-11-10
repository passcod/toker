//! Reading the routing snapshot's JSON.
//!
//! The daemon computes every fact the page shows (see its
//! `control::derived`), so the page only picks them out. A fact that is
//! absent reads as empty or as a dash rather than as a value that was never
//! there (spec: UTUI#rendering).

use serde_json::Value;

pub(super) fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// The string at `key`, or the empty string where there is none.
pub(super) fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

pub(super) fn count(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

pub(super) fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// The facts the daemon derived for the live revision, when it compiled.
pub(super) fn active(snapshot: &Value) -> Option<&Value> {
    snapshot
        .get("derived")
        .and_then(|derived| derived.get("active"))
        .filter(|active| !active.is_null())
}

/// The live revision's policy document.
pub(super) fn policy(snapshot: &Value) -> Option<&Value> {
    snapshot
        .get("active_revision")
        .and_then(|revision| revision.get("policy"))
        .filter(|policy| !policy.is_null())
}

/// Whether the live revision holds no record at all, as a fresh install's
/// does, as the daemon says. A snapshot with nothing derived in it says
/// nothing either way.
pub(super) fn holds_nothing(snapshot: &Value) -> bool {
    snapshot
        .get("derived")
        .and_then(|derived| derived.get("holds_nothing"))
        == Some(&Value::Bool(true))
}

/// The lanes still pinned to a revision that is no longer live, as the
/// daemon worked them out against it: how many, how many of those moving
/// would cost a cache, some of their names, and the revisions holding them.
pub(super) fn earlier_lanes(snapshot: &Value) -> &Value {
    snapshot
        .get("derived")
        .and_then(|derived| derived.get("earlier_lanes"))
        .unwrap_or(&Value::Null)
}

pub(super) fn revision_id(snapshot: &Value) -> Option<i64> {
    snapshot
        .get("active_revision")
        .and_then(|revision| revision.get("revision_id"))
        .and_then(Value::as_i64)
}

/// `1 rule` or `4 rules`.
pub(super) fn counted(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}
