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
/// does. A snapshot with no policy in it says nothing either way.
pub(super) fn holds_nothing(snapshot: &Value) -> bool {
    policy(snapshot).is_some_and(|policy| {
        [
            "presentations",
            "routes",
            "defaults",
            "rewrites",
            "classifiers",
            "upgrades",
        ]
        .iter()
        .all(|table| array(policy, table).is_empty())
            && policy.get("compaction").is_none_or(Value::is_null)
    })
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
