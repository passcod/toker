//! The routing header's status: which revision is live, which lanes are left
//! on earlier ones, and where a client's settings stand
//! (spec: UTUI#editing).
//!
//! It says what the daemon recorded and waits for nothing: a publication that
//! is pending, failed or unsupported is named with the detail the row holds,
//! whether or not a reconciliation is on its way.

use serde_json::Value;

use super::data::{self, array, count, text};

/// The adapter's name as an operator knows it: the client it publishes to.
fn adapter_name(adapter: &str) -> &str {
    match adapter {
        "claude_settings" => "claude",
        "dynamic_chat_models" => "chat",
        "dynamic_responses_models" => "responses",
        other => other,
    }
}

/// `2 lanes on rev 9`, for the lanes still pinned to a revision that is no
/// longer live. Nothing where there are none.
fn earlier_lanes(snapshot: &Value, live: Option<i64>) -> Option<String> {
    let revisions = snapshot
        .get("derived")
        .map(|derived| array(derived, "revisions"))
        .unwrap_or(&[]);
    let mut total = 0;
    let mut on = Vec::new();
    for revision in revisions {
        let id = revision.get("revision_id").and_then(Value::as_i64);
        let lanes = count(revision, "total");
        if id != live && lanes > 0 {
            total += lanes;
            on.extend(id);
        }
    }
    if total == 0 {
        return None;
    }
    on.sort_unstable();
    let on: Vec<String> = on.iter().map(i64::to_string).collect();
    Some(format!(
        "{total} {} on rev {}",
        if total == 1 { "lane" } else { "lanes" },
        on.join(", ")
    ))
}

/// Each client whose settings are not applied for the live revision, with
/// the state and detail its row records.
fn unapplied(snapshot: &Value) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for publication in array(snapshot, "publications") {
        let state = text(publication, "state");
        if state == "applied" || state.is_empty() {
            continue;
        }
        let mut said = format!("{} {state}", adapter_name(text(publication, "adapter")));
        if let Some(code) = publication
            .get("detail_code")
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
        {
            said.push_str(&format!(": {}", code.replace('_', " ")));
        }
        if !found.contains(&said) {
            found.push(said);
        }
    }
    found
}

/// The header status of the page.
pub(super) fn text_of(
    snapshot: Option<&Value>,
    revision: Option<i64>,
    working: bool,
    lead: Option<&str>,
    note: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.extend(lead.map(str::to_owned));
    if let Some(revision) = revision {
        parts.push(format!("rev {revision} live"));
    }
    if let Some(snapshot) = snapshot {
        parts.extend(earlier_lanes(snapshot, data::revision_id(snapshot)));
        parts.extend(unapplied(snapshot));
    }
    if working {
        parts.push("working".to_owned());
    }
    parts.extend(note.map(str::to_owned));
    parts.join(" · ")
}
