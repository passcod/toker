//! The learned model store: which version of each family is the newest,
//! learned from what the log has actually served (plan: Model catalogues —
//! "Learned-newest store"), never from a list kept here or a query to the
//! API. A model the proxy has never forwarded does not exist as far as this
//! module is concerned, and the day a newer one is used it learns of it
//! without anyone editing anything.
//!
//! This is the **learned** store — days served + maxPrompt observed per
//! exact identity — not the hand-verified context-window catalogue (that is
//! [`crate::catalog::windows`], already ported). The two meet at
//! [`MergeIncoming`]/[`ModelEntry::context_window_json`]: a provider
//! declaration may round-trip through the store, but the hand-verified
//! catalogue wins at read time (a precedence held by
//! [`crate::catalog::windows::resolve_context_window`] instead of being
//! copied into learned entries on every write).
//!
//! A faithful port of the predecessor proxy's learned model store, measured
//! over weeks of production
//! traffic — ported, not improved. Pieces:
//!
//! - the family split and version comparison ([`family_of`],
//!   [`newer_than`]);
//! - `requirement` / `activeDaysOf` / `newestInFamily` (the day-based
//!   election);
//! - [`ModelStore::note_seen`] (local-day union, maxPrompt max);
//! - `mergeSeen` with `only: true` (the control endpoint's semantics);
//! - `controlMerge` (the endpoint's validation and reply) —
//!   wired in [`crate::server::control`];
//! - the in-memory recently-served map ([`ModelStore::note_served`] /
//!   [`ModelStore::last_served`], seeded from the ledger tail at startup).
//!
//! Days are **local calendar days** (a `toLocaleDateString("en-CA")`-style
//! local day —
//! "seen on seven separate days" is a statement about how someone works,
//! not about UTC), so the timezone is an input to [`ModelStore::note_seen`],
//! and tests pin it; the server passes the system zone.
//!
//! Absence ≠ zero (invariant 3): `max_prompt` is `None` until a model has
//! been observed holding a prompt, and an unknown family member is never
//! guessed into existence by the election.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use jiff::tz::TimeZone;

use crate::catalog::windows::model_identity;
use crate::store::{ModelEntry, RequestRow, Store};

/// The ceiling on the requirement: a fortnight of daily use proves enough.
pub const MAX_REQUIRED_DAYS: f64 = 7.0;

/// A model id split into the family it belongs to and the version within
/// it.
///
/// The family is the non-numeric remainder and the version is the trailing
/// numbers, so `claude-opus-4-8` is opus [4, 8] and `claude-opus-5` is opus
/// [5]. Published snapshot aliases fold first (via
/// [`model_identity`]), while an unpublished dated identity has no family
/// and can never become a rewrite target — a future model is not evidence
/// that it inherited an earlier version's anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Family {
    pub name: String,
    pub version: Vec<u64>,
}

/// Is `a` a strictly newer version than `b`, within the same family?
///
/// Segment-wise and numeric, because string order gets this wrong twice
/// over: `claude-opus-5` sorts before `claude-opus-4-8`, and `4-10` sorts
/// before `4-8`. A missing segment counts as lower, so [5] beats [4, 8]
/// and [5, 1] beats [5].
pub fn newer_than(a: &str, b: &str) -> bool {
    let (Some(fa), Some(fb)) = (family_of(a), family_of(b)) else {
        return false;
    };
    if fa.name != fb.name {
        return false;
    }
    for i in 0..fa.version.len().max(fb.version.len()) {
        // A missing segment is lower than any present one.
        let x = fa.version.get(i).map(|&v| v as i64).unwrap_or(-1);
        let y = fb.version.get(i).map(|&v| v as i64).unwrap_or(-1);
        if x != y {
            return x > y;
        }
    }
    false
}

/// An id ending in a dated suffix the alias table does not know: an
/// *unpublished* snapshot. Matched only after alias folding, so a published
/// dated identity like `claude-3-7-sonnet-20250219` folds to its dateless
/// form and keeps its family.
fn is_unpublished_claude_snapshot(model: &str) -> bool {
    match model_identity(model) {
        Some(id) => {
            let bytes = id.as_bytes();
            id.starts_with("claude-")
                && bytes.len() >= 9
                && bytes[bytes.len() - 9] == b'-'
                && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit)
        }
        None => false,
    }
}

/// The family a model belongs to
/// (ported exactly over the normalised identity; unknown stays `None`,
/// never guessed).
pub fn family_of(model: &str) -> Option<Family> {
    if is_unpublished_claude_snapshot(model) {
        return None;
    }
    let id = model_identity(model)?;
    let stripped = id.strip_prefix("claude-").unwrap_or(&id);
    let mut name = Vec::new();
    let mut version = Vec::new();
    for part in stripped.split('-') {
        // Only a run of digits is a version segment.
        if !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) {
            version.push(part.parse::<u64>().unwrap_or(u64::MAX));
        } else {
            name.push(part);
        }
    }
    if name.is_empty() {
        return None;
    }
    Some(Family {
        name: name.join("-"),
        version,
    })
}

/// How many days a model must have been seen on to be trusted as its
/// family's newest, given how many days there are to judge against.
///
/// Scaled rather than absolute: an absolute bar asks a young log for
/// evidence it cannot possibly contain — on a fresh install nothing would
/// qualify for a week, so the feature would be dormant exactly when someone
/// is setting it up and watching it. Scaled, day one accepts whatever is
/// present, and the bar tightens as history arrives. What keeps the weak
/// early bar safe is not this function: it only decides WHICH model is a
/// family's target; whether anything is rewritten at all is decided
/// elsewhere, by rules that do not relax (the next unit's).
pub fn requirement(active_days: usize) -> f64 {
    (active_days as f64 / 2.0).min(MAX_REQUIRED_DAYS)
}

/// The parsed days of a learned entry: the JSON column's strings, deduped
/// and sorted. Duplicates would inflate the count that clears the gate, so
/// a single busy day could stand in for a week of use. A non-array or
/// absent column reads as no days.
pub fn days_of(entry: &ModelEntry) -> Vec<String> {
    let Some(days) = entry.days_json.as_ref().and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = days
        .iter()
        .filter_map(|day| day.as_str().map(str::to_owned))
        .filter(|day| !day.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Days on which the log saw any traffic at all — the denominator of the
/// election's bar: the union of
/// every model's days.
pub fn active_days_of(entries: &[ModelEntry]) -> usize {
    let mut days = BTreeSet::new();
    for entry in entries {
        days.extend(days_of(entry));
    }
    days.len()
}

/// The newest model in a family that has proven itself, or `None`
/// (without the optional `accept`
/// hook; see [`newest_in_family_accepting`]).
pub fn newest_in_family(entries: &[ModelEntry], family: &str) -> Option<String> {
    newest_in_family_accepting(entries, family, None)
}

/// [`newest_in_family`] with the `accept` hook:
/// a
/// caller refuses a candidate it cannot use — the compaction retarget
/// needs a model it can price, and an unpriced newcomer must fall back to
/// the newest priced version rather than silently switching the feature
/// off.
pub fn newest_in_family_accepting(
    entries: &[ModelEntry],
    family: &str,
    accept: Option<&dyn Fn(&str) -> bool>,
) -> Option<String> {
    let needed = requirement(active_days_of(entries));
    let mut best: Option<String> = None;
    for entry in entries {
        let Some(f) = family_of(&entry.model_id) else {
            continue;
        };
        if f.name != family {
            continue;
        }
        if (days_of(entry).len() as f64) <= needed {
            continue;
        }
        if let Some(accept) = accept
            && !accept(&entry.model_id)
        {
            continue;
        }
        if best
            .as_deref()
            .is_none_or(|best| newer_than(&entry.model_id, best))
        {
            best = Some(entry.model_id.clone());
        }
    }
    best
}

/// Has a model been observed holding a conversation at least `prompt`
/// tokens? (a learned context check:
/// an unproven model declines, which costs an upgrade where the
/// alternative costs a failed request at the worst possible moment.)
/// Absence reads as zero, never a free pass.
///
/// Shared by the two rewrites that need it: the compaction retarget
/// ([`compaction_target_of`]) and the force-newest move
/// ([`crate::middleware::force_newest`]) — one shared check, both callers.
pub(crate) fn fits_context(entries: &[ModelEntry], model: &str, prompt: u64) -> bool {
    entries
        .iter()
        .find(|entry| entry.model_id == model)
        .and_then(|entry| entry.max_prompt)
        .unwrap_or(0)
        >= prompt as i64
}

/// The model a cold compaction should be rewritten onto, from a spec that
/// is either a family name (`"sonnet"`), an explicit id
/// (`"claude-sonnet-5"`), or `"off"`. A family is resolved through the same election as
/// everything else, so the target follows what is actually in use rather
/// than being pinned to a literal that goes stale the day a newer Sonnet
/// ships. The price filter is the `accept` hook: a candidate the table
/// cannot price is no use to a rewrite that decides "cheaper" from it.
pub fn compaction_target_of(entries: &[ModelEntry], spec: &str, prompt: u64) -> Option<String> {
    if spec.is_empty() || spec == "off" {
        return None;
    }
    let f = family_of(spec)?;
    // A spec carrying a version is an explicit pin; one without is a family.
    let target = if !f.version.is_empty() {
        model_identity(spec)
    } else {
        newest_in_family_accepting(
            entries,
            &f.name,
            Some(&|model: &str| crate::catalog::pricing::price(model, false, None).is_some()),
        )
    }?;
    if !fits_context(entries, &target, prompt) {
        return None;
    }
    Some(target)
}

/// The local calendar day of an epoch-millisecond timestamp, `YYYY-MM-DD`
/// (the same shape an `en-CA` locale day renders as). The timezone is the
/// caller's, so the grouping is a pure function of its inputs; the server
/// passes the system zone, and tests pin theirs.
pub fn local_day(at_ms: i64, tz: &TimeZone) -> Option<String> {
    let zoned = jiff::Timestamp::from_millisecond(at_ms)
        .ok()?
        .to_zoned(tz.clone());
    Some(zoned.date().to_string())
}

/// One incoming entry for a control-merge (the merge's `from` side,
/// already validated by the endpoint): days to union in, and a maxPrompt to
/// max in, for one exact model identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeIncoming {
    pub model_id: String,
    /// Days the log already has, granted as-is (a promotion grants
    /// only days the log already holds — inventing dates would also enlarge
    /// `activeDaysOf`, the denominator of the bar being cleared).
    pub days: Vec<String>,
    /// An empirical prompt ceiling to raise the stored one to.
    pub max_prompt: Option<i64>,
}

/// The outcome of one control-merge: either the model was already served and
/// the store now holds the union, or nothing happened at all.
#[derive(Debug, Clone, PartialEq)]
pub enum MergeOutcome {
    /// The entry was merged: days unioned, the higher maxPrompt kept, and
    /// `target` is what the family's election now names (the effect, not
    /// the intent — a promotion does not guarantee the slot, and a newer
    /// version may already hold it).
    Merged {
        /// The stored entry after the merge (boxed: `Unseen` is the common
        /// case on the refusal path, and the entry only exists once merged).
        entry: Box<ModelEntry>,
        target: Option<String>,
    },
    /// The model has never been served: refused, never invented (the
    /// `only: true`). The worst an outsider can do is what promote-model
    /// does on purpose — a typo here must not redirect a whole family's
    /// traffic to an id the API rejects.
    Unseen,
}

/// The learned store plus the in-memory recently-served map, over one
/// database (plan: "State as tables"). Cheap to clone-free: the server holds
/// one `Arc<ModelStore>`; every method is either an in-memory map touch or
/// a read-modify-write the store's single connection serialises.
pub struct ModelStore {
    store: Arc<Store>,
    /// Exact model identity → when it was last sent upstream, from any
    /// session. Kept apart from the lane table because it
    /// must never forget within the hour it answers for: the table is
    /// capped and moves only on a served response; this is a handful of
    /// entries, moved when a request is sent, so a request still in flight
    /// counts.
    served_on: Mutex<BTreeMap<String, i64>>,
    /// How far back `served_on` vouches: `None` when it was seeded from the
    /// whole ledger; else the ts of the oldest seed row
    /// (the tail cut a beginning off, and silence before it counts for
    /// nothing unless the record reaches back past the TTL).
    covered_since: Option<i64>,
}

impl ModelStore {
    /// Build the store, seeding the recently-served map from the ledger
    /// tail. `covered` is [`Self::covered_since`]'s input: `None` when `rows` is
    /// the whole ledger, else the tail's oldest ts.
    pub fn seeded(store: Arc<Store>, rows: &[RequestRow], covered: Option<i64>) -> ModelStore {
        let mut served_on = BTreeMap::new();
        for row in rows {
            let model = row.raw_model.as_deref().or(row.model.as_deref());
            if let Some(id) = model.and_then(model_identity)
                && let Some(at) = served_on.get(&id)
                && *at >= row.ts_ms
            {
                continue;
            }
            if let Some(id) = model.and_then(model_identity) {
                served_on.insert(id, row.ts_ms);
            }
        }
        ModelStore {
            store,
            served_on: Mutex::new(served_on),
            covered_since: covered,
        }
    }

    /// Remember that `model` is being sent upstream **now**
    /// (called BEFORE the request goes
    /// upstream, so a lane deciding while this one is still in flight sees
    /// the model as in use; the response may later name a different served
    /// identity, and that remains authoritative for observations).
    ///
    /// In-memory and infallible, exactly like an in-memory map. `model` is
    /// normalised to the exact identity first.
    pub fn note_served(&self, model: Option<&str>, at_ms: i64) {
        let Some(id) = model.and_then(model_identity) else {
            return;
        };
        if let Ok(mut served_on) = self.served_on.lock() {
            let at = served_on.get(&id).map_or(at_ms, |prev| (*prev).max(at_ms));
            served_on.insert(id, at);
        }
    }

    /// When `model` was last sent upstream, when this process or its seed
    /// knows. The identity is normalised first.
    pub fn last_served(&self, model: &str) -> Option<i64> {
        let id = model_identity(model)?;
        self.served_on.lock().ok()?.get(&id).copied()
    }

    /// How far back the served map vouches: `None` = the whole ledger was
    /// seeded (never forgets); `Some(ts)` = the tail cut a beginning off at
    /// `ts`.
    pub fn covered_since(&self) -> Option<i64> {
        self.covered_since
    }

    /// Record that `model` served a request of `prompt` tokens at `at_ms`
    /// ("a model is *seen* when a
    /// response named it").
    ///
    /// The day is the LOCAL calendar day of `at_ms` in `tz`; the prompt
    /// ceiling is the largest this model has been observed actually
    /// holding. A read-modify-write over the models table, one upsert;
    /// the caller owns the timezone and the error (invariant 6: a lost
    /// update never loses the request).
    pub fn note_seen(
        &self,
        model: &str,
        at_ms: i64,
        prompt: u64,
        tz: &TimeZone,
    ) -> crate::store::Result<()> {
        let Some(id) = model_identity(model) else {
            return Ok(());
        };
        let Some(day) = local_day(at_ms, tz) else {
            return Ok(());
        };
        let mut entry = self.store.load_model(&id)?.unwrap_or(ModelEntry {
            model_id: id.clone(),
            days_json: None,
            max_prompt: None,
            context_window_json: None,
        });
        let mut days: BTreeSet<String> = days_of(&entry).into_iter().collect();
        days.insert(day);
        entry.days_json = Some(serde_json::Value::Array(
            days.into_iter().map(serde_json::Value::String).collect(),
        ));
        let prompt = i64::try_from(prompt).unwrap_or(i64::MAX);
        entry.max_prompt = Some(entry.max_prompt.map_or(prompt, |prev| prev.max(prompt)));
        self.store.upsert_model(&entry)
    }

    /// The day-based election over the store's learned entries: the newest
    /// model in `family` that has been served on more than
    /// `min(7, activeDays/2)` distinct days (with
    /// `activeDays` the union of the days the whole store holds).
    pub fn family_newest(&self, family: &str) -> crate::store::Result<Option<String>> {
        let entries = self.store.load_models()?;
        Ok(newest_in_family(&entries, family))
    }

    /// The model a cold compaction should be rewritten onto, resolved per
    /// request against what is actually in use
    /// (see [`compaction_target_of`]). The size guard needs
    /// the prompt this lane is carrying. A store error propagates; the
    /// caller loses the target, never the request (invariant 6).
    pub fn compaction_target(
        &self,
        spec: &str,
        prompt: u64,
    ) -> crate::store::Result<Option<String>> {
        let entries = self.store.load_models()?;
        Ok(compaction_target_of(&entries, spec, prompt))
    }

    /// The strictly-newer learned member of `model`'s family that a
    /// request could be rewritten onto, proven at `prompt`
    /// (the decision core lives in
    /// [`crate::middleware::force_newest::force_target_of`], where the
    /// sequencing that calls it is ported). A store error propagates; the
    /// caller loses the upgrade, never the request (invariant 6).
    pub fn force_target(&self, model: &str, prompt: u64) -> crate::store::Result<Option<String>> {
        let entries = self.store.load_models()?;
        Ok(crate::middleware::force_newest::force_target_of(
            &entries, model, prompt,
        ))
    }

    /// Every exact identity the store has learned, sorted (for the
    /// control endpoint's "what is known" reply).
    pub fn known_models(&self) -> crate::store::Result<Vec<String>> {
        Ok(self
            .store
            .load_models()?
            .into_iter()
            .map(|entry| entry.model_id)
            .collect())
    }

    /// The models-merge semantics
    /// (`mergeSeen(into, from, {only: true})` — the control endpoint's one power):
    /// **only adds** days and maxPrompt for a model **already served**,
    /// never creates one. Days union; the maxPrompt keeps the higher of
    /// the two, so neither side can erase what the other has seen — which
    /// is what lets a promotion apply to a running process without racing
    /// its own writes.
    ///
    /// The stored context-window declaration survives a merge untouched:
    /// the hand-verified catalogue wins at read time anyway
    /// ([`crate::catalog::windows::resolve_context_window`]), and the
    /// "never creates or widens declared context capability" rule holds here
    /// because nothing here touches it.
    pub fn merge(&self, incoming: &MergeIncoming) -> crate::store::Result<MergeOutcome> {
        let Some(mut entry) = self.store.load_model(&incoming.model_id)? else {
            return Ok(MergeOutcome::Unseen);
        };
        let mut days: BTreeSet<String> = days_of(&entry).into_iter().collect();
        days.extend(incoming.days.iter().cloned());
        entry.days_json = Some(serde_json::Value::Array(
            days.into_iter().map(serde_json::Value::String).collect(),
        ));
        if let Some(max_prompt) = incoming.max_prompt {
            entry.max_prompt = Some(
                entry
                    .max_prompt
                    .map_or(max_prompt, |prev| prev.max(max_prompt)),
            );
        }
        self.store.upsert_model(&entry)?;
        let target = match family_of(&entry.model_id) {
            Some(family) => self.family_newest(&family.name)?,
            None => None,
        };
        Ok(MergeOutcome::Merged {
            entry: Box::new(entry),
            target,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MergeIncoming, MergeOutcome, ModelStore, compaction_target_of, family_of, local_day,
        newer_than, newest_in_family, requirement,
    };
    use crate::store::{ModelEntry, RequestRow, Store};
    use jiff::tz::TimeZone;
    use serde_json::json;
    use std::sync::Arc;

    fn mem_store() -> Arc<Store> {
        Arc::new(Store::open(":memory:").expect("open in-memory store"))
    }

    fn models() -> ModelStore {
        ModelStore::seeded(mem_store(), &[], None)
    }

    /// A row with only `ts_ms` — every other column NULL.
    fn bare_row() -> RequestRow {
        RequestRow {
            id: None,
            ts_ms: 0,
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

    fn entry(model_id: &str, days: &[&str], max_prompt: Option<i64>) -> ModelEntry {
        ModelEntry {
            model_id: model_id.to_owned(),
            days_json: Some(json!(days)),
            max_prompt,
            context_window_json: None,
        }
    }

    /// Pacific/Auckland via a fixed +13:00 offset — local-day grouping
    /// pinned without a tzdb dependency in the unit tests.
    fn tz() -> TimeZone {
        TimeZone::fixed(jiff::tz::Offset::from_hours(13).expect("offset"))
    }

    // ── family parsing ──────────────────────────────────────────────

    #[test]
    fn families_parse_from_normalised_ids() {
        let family = |model: &str| {
            family_of(model).map(|f| {
                assert!(
                    f.version.iter().all(|v| *v < u64::MAX),
                    "a sane id never saturates"
                );
                (f.name, f.version)
            })
        };
        // The canonical trio, with the segment-wise versions.
        assert_eq!(
            family("claude-opus-4-8"),
            Some(("opus".to_owned(), vec![4, 8]))
        );
        assert_eq!(family("claude-opus-5"), Some(("opus".to_owned(), vec![5])));
        assert_eq!(
            family("claude-sonnet-5"),
            Some(("sonnet".to_owned(), vec![5]))
        );
        assert_eq!(
            family("claude-haiku-4-5"),
            Some(("haiku".to_owned(), vec![4, 5]))
        );
        assert_eq!(
            family("claude-fable-5-1"),
            Some(("fable".to_owned(), vec![5, 1]))
        );
        assert_eq!(
            family("claude-mythos-preview"),
            Some(("mythos-preview".to_owned(), vec![]))
        );
        // The claude- prefix folds; a non-claude id keeps its leading part.
        assert_eq!(
            family("claude-3-5-sonnet"),
            Some(("sonnet".to_owned(), vec![3, 5]))
        );
        assert_eq!(
            family("gpt-5.6-sol"),
            Some(("gpt-5.6-sol".to_owned(), vec![])),
            "5.6 is not a pure digit run: it stays family, not a version segment"
        );
        // Published snapshots fold to their dateless form and keep a family.
        assert_eq!(
            family("claude-3-7-sonnet-20250219"),
            Some(("sonnet".to_owned(), vec![3, 7]))
        );
        // An unpublished dated identity has no family, never a rewrite
        // target: a future model is not evidence about an earlier one.
        assert_eq!(family("claude-opus-4-9-20261225"), None);
        // Unknown stays unknown, never guessed (invariant 3).
        assert_eq!(family(""), None);
        assert_eq!(family("   "), None, "blank normalises to no identity");
    }

    #[test]
    fn newer_is_segmentwise_and_numeric_never_stringwise() {
        // String order gets this wrong: "claude-opus-5" < "claude-opus-4-8".
        assert!(newer_than("claude-opus-5", "claude-opus-4-8"));
        assert!(!newer_than("claude-opus-4-8", "claude-opus-5"));
        // "4-10" must beat "4-8" numerically.
        assert!(newer_than("claude-opus-4-10", "claude-opus-4-8"));
        // A missing segment counts as lower: [5, 1] beats [5].
        assert!(newer_than("claude-opus-5-1", "claude-opus-5"));
        // Equal is not newer, and a family never crosses.
        assert!(!newer_than("claude-opus-5", "claude-opus-5"));
        assert!(!newer_than("claude-opus-5", "claude-sonnet-5"));
        assert!(!newer_than("no-family-at-all", "claude-opus-5"));
    }

    // ── the election bar ────────────────────────────────────────────

    #[test]
    fn the_bar_scales_with_the_log_both_sides() {
        // Day one: whatever is present qualifies — a young log is not
        // dormant (an absolute bar would ask a fresh install for a week of
        // evidence it cannot contain).
        assert_eq!(requirement(0), 0.0);
        assert_eq!(requirement(1), 0.5);
        assert_eq!(requirement(2), 1.0);
        // It tightens as history arrives.
        assert_eq!(requirement(6), 3.0);
        // And caps at a fortnight of daily use.
        assert_eq!(requirement(14), 7.0);
        assert_eq!(requirement(400), 7.0);
    }

    #[test]
    fn the_election_respects_the_bar_and_picks_the_newest() {
        // Six days of history: needed = 3.
        let days6 = [
            "2026-09-28",
            "2026-09-29",
            "2026-09-30",
            "2026-10-01",
            "2026-10-02",
            "2026-10-03",
        ];
        let entries = vec![
            entry("claude-opus-4-8", &days6[..4], Some(150_000)),
            // Served one day only: below the bar, never the target.
            entry("claude-opus-5-5", &days6[..1], Some(180_000)),
            entry("claude-sonnet-5", &days6, Some(90_000)),
        ];
        assert_eq!(
            newest_in_family(&entries, "opus"),
            Some("claude-opus-4-8".to_owned()),
            "the young 5-5 is barred; 4-8 holds the family"
        );
        assert_eq!(
            newest_in_family(&entries, "sonnet"),
            Some("claude-sonnet-5".to_owned())
        );
        // A family nothing served has no target.
        assert_eq!(newest_in_family(&entries, "mythos"), None);

        // Five more days for 5-5: above the bar, and strictly newer wins
        // on segments, not strings.
        let entries = vec![
            entry("claude-opus-4-8", &days6, Some(150_000)),
            entry("claude-opus-5-5", &days6, Some(180_000)),
        ];
        assert_eq!(
            newest_in_family(&entries, "opus"),
            Some("claude-opus-5-5".to_owned())
        );

        // The bar uses the UNION of all days (activeDays = 7 across both
        // models → needed 3.5), and `days.length <= needed` is the
        // refusal: 3 days is still a trial, not an adoption.
        let entries = vec![
            entry("claude-opus-4-8", &days6, None),
            entry(
                "claude-opus-5",
                &["2026-10-01", "2026-10-02", "2026-10-03"],
                None,
            ),
        ];
        assert_eq!(
            newest_in_family(&entries, "opus"),
            Some("claude-opus-4-8".to_owned()),
            "3 days against a 3.5 bar is still a trial, not an adoption"
        );
    }

    // ── local-day grouping ──────────────────────────────────────────

    #[test]
    fn days_group_by_local_calendar_day_never_utc() {
        let tz = tz();
        // 2026-10-02 14:00 UTC is 2026-10-03 03:00 at +13 — same instant,
        // different local day, and the LOCAL one is what counts.
        let utc_day = local_day(1_769_954_400_000, &TimeZone::UTC).expect("parses");
        assert_eq!(utc_day, "2026-02-01");
        let local = local_day(1_769_954_400_000, &tz).expect("parses");
        assert_eq!(local, "2026-02-02", "+13:00 is already the next day");

        // A timestamp that cannot exist answers None, never a guess.
        assert_eq!(local_day(i64::MAX, &tz), None);
    }

    #[test]
    fn note_seen_unions_local_days_and_maxes_the_prompt() {
        let models = models();
        let tz = tz();
        // Two timestamps on the same local day: one day.
        models
            .note_seen("claude-opus-5", 1_769_954_400_000, 100_000, &tz)
            .expect("note");
        models
            .note_seen("claude-opus-5", 1_769_954_400_000 + 3_600_000, 150_000, &tz)
            .expect("note");
        let stored = models
            .store
            .load_model("claude-opus-5")
            .expect("load")
            .expect("seen creates the entry");
        assert_eq!(stored.days_json, Some(json!(["2026-02-02"])));
        assert_eq!(stored.max_prompt, Some(150_000), "the ceiling is a max");
        // A smaller prompt never lowers it.
        models
            .note_seen("claude-opus-5", 1_769_954_400_000, 90_000, &tz)
            .expect("note");
        assert_eq!(
            models
                .store
                .load_model("claude-opus-5")
                .expect("load")
                .expect("entry")
                .max_prompt,
            Some(150_000)
        );

        // A later local day unions in, sorted and deduped.
        models
            .note_seen("claude-opus-5", 1_769_954_400_000 + 30 * 3_600_000, 1, &tz)
            .expect("note");
        models
            .note_seen("claude-opus-5", 1_769_954_400_000 + 30 * 3_600_000, 1, &tz)
            .expect("note");
        assert_eq!(
            models
                .store
                .load_model("claude-opus-5")
                .expect("load")
                .expect("entry")
                .days_json,
            Some(json!(["2026-02-02", "2026-02-03"])),
            "duplicate days would inflate the count that clears the gate"
        );

        // An identity that normalises to nothing records nothing.
        models.note_seen("", 0, 5, &tz).expect("note");
        models.note_seen("   ", 0, 5, &tz).expect("note");
        assert_eq!(
            models.store.load_models().expect("models").len(),
            1,
            "absence never becomes a model"
        );
    }

    // ── the recently-served map ─────────────────────────────────────

    #[test]
    fn note_served_takes_the_latest_and_normalises_the_identity() {
        let store = mem_store();
        let mut row = bare_row();
        row.ts_ms = 5_000;
        row.raw_model = Some("claude-opus-5".to_owned());
        let models = ModelStore::seeded(store.clone(), std::slice::from_ref(&row), None);

        // Seeded from the tail.
        assert_eq!(models.last_served("claude-opus-5"), Some(5_000));
        // The map keeps the later of two marks.
        models.note_served(Some("claude-opus-5"), 7_000);
        models.note_served(Some("claude-opus-5"), 6_000);
        assert_eq!(models.last_served("claude-opus-5"), Some(7_000));
        // A different model starts its own entry, normalised.
        models.note_served(Some("claude-3-7-sonnet-20250219"), 8_000);
        assert_eq!(models.last_served("claude-3-7-sonnet"), Some(8_000));
        // Absence stays absence.
        assert_eq!(models.last_served("claude-fable-5"), None);
        assert_eq!(models.covered_since(), None, "the whole ledger was seeded");

        // The raw model wins over the normalised column when seeding.
        let mut row2 = bare_row();
        row2.ts_ms = 6_000;
        row2.raw_model = Some("claude-opus-5".to_owned());
        row2.model = Some("claude-opus-5".to_owned());
        let models = ModelStore::seeded(store, &[row, row2], Some(5_000));
        assert_eq!(models.last_served("claude-opus-5"), Some(6_000));
        assert_eq!(
            models.covered_since(),
            Some(5_000),
            "a cut tail vouches only from its first row"
        );
    }

    // ── merge (only: true) ──────────────────────────────────────────

    #[test]
    fn merge_only_adds_for_models_already_served_and_never_invents() {
        let models = models();
        models
            .note_seen("claude-opus-5", 1_769_954_400_000, 100_000, &tz())
            .expect("note");

        // Additive: days union, maxPrompt maxes.
        let outcome = models
            .merge(&MergeIncoming {
                model_id: "claude-opus-5".to_owned(),
                days: vec!["2026-09-20".to_owned(), "2026-09-21".to_owned()],
                max_prompt: Some(180_000),
            })
            .expect("merge");
        let MergeOutcome::Merged { entry, target } = outcome else {
            panic!("a served model merges");
        };
        assert_eq!(
            entry.days_json,
            Some(json!(["2026-02-02", "2026-09-20", "2026-09-21"])),
            "days union, sorted"
        );
        assert_eq!(
            entry.max_prompt,
            Some(180_000),
            "the higher ceiling is kept"
        );
        assert_eq!(
            target,
            Some("claude-opus-5".to_owned()),
            "alone in its family"
        );

        // A merge cannot LOWER the ceiling — neither side erases the other.
        models
            .merge(&MergeIncoming {
                model_id: "claude-opus-5".to_owned(),
                days: vec![],
                max_prompt: Some(90_000),
            })
            .expect("merge");
        assert_eq!(
            models
                .store
                .load_model("claude-opus-5")
                .expect("load")
                .expect("entry")
                .max_prompt,
            Some(180_000)
        );

        // Never invents: a model the store has not served is refused, and
        // the store never grows one.
        assert_eq!(
            models
                .merge(&MergeIncoming {
                    model_id: "claude-opus-5-5".to_owned(),
                    days: vec!["2026-10-03".to_owned()],
                    max_prompt: Some(200_000),
                })
                .expect("merge"),
            MergeOutcome::Unseen
        );
        assert_eq!(
            models.store.load_model("claude-opus-5-5").expect("load"),
            None
        );
        assert_eq!(
            models.known_models().expect("known"),
            vec!["claude-opus-5".to_owned()]
        );
    }

    #[test]
    fn a_valid_merge_moves_the_election() {
        let models = models();
        let tz = tz();
        // Eight days of history on the incumbent, one on the newcomer:
        // activeDays 8 → needed 4; the newcomer's single day is a trial.
        let incumbent_days: Vec<String> = (0..8)
            .map(|i| format!("2026-09-{}", 20 + i))
            .collect::<Vec<_>>();
        let incumbent = incumbent_days
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        models
            .store
            .upsert_model(&entry("claude-opus-5", &incumbent, Some(100_000)))
            .expect("upsert incumbent");
        models
            .note_seen("claude-opus-5-5", 1_769_954_400_000, 180_000, &tz)
            .expect("note the newcomer once");

        assert_eq!(
            models.family_newest("opus").expect("election"),
            Some("claude-opus-5".to_owned()),
            "the newcomer is below the bar"
        );

        // The promotion hands over days the log already has; granting
        // them clears the bar and the election moves.
        let days = incumbent_days.clone();
        let MergeOutcome::Merged { target, .. } = models
            .merge(&MergeIncoming {
                model_id: "claude-opus-5-5".to_owned(),
                days,
                max_prompt: None,
            })
            .expect("merge")
        else {
            panic!("the newcomer is served, so it merges");
        };
        assert_eq!(
            target,
            Some("claude-opus-5-5".to_owned()),
            "the effect, not the intent"
        );
        assert_eq!(
            models.family_newest("opus").expect("election"),
            Some("claude-opus-5-5".to_owned())
        );
    }

    #[test]
    fn the_context_window_declaration_survives_a_merge_untouched() {
        let models = models();
        models
            .note_seen("gpt-5.6-sol", 1_769_954_400_000, 10_000, &tz())
            .expect("note");
        let mut entry = models
            .store
            .load_model("gpt-5.6-sol")
            .expect("load")
            .expect("entry");
        entry.context_window_json = Some(json!({"default": 272_000, "max": 872_000}));
        models.store.upsert_model(&entry).expect("declare");

        let MergeOutcome::Merged { entry, .. } = models
            .merge(&MergeIncoming {
                model_id: "gpt-5.6-sol".to_owned(),
                days: vec!["2026-10-01".to_owned()],
                max_prompt: None,
            })
            .expect("merge")
        else {
            panic!("merges");
        };
        assert_eq!(
            entry.context_window_json,
            Some(json!({"default": 272_000, "max": 872_000})),
            "a merge never creates or widens a declared capability"
        );
    }

    // ── the compaction target ───────────────────────────────────────

    #[test]
    fn compaction_target_resolves_family_pin_and_context() {
        // The compaction target, over the learned
        // store: a family name follows what is actually in use, an
        // explicit id is a pin, "off"/empty is nothing, and an unproven
        // context window declines — a failed request at the worst moment.
        let days: Vec<&str> = vec!["2026-09-20", "2026-09-21", "2026-09-22", "2026-09-23"];
        let entries = vec![
            entry("claude-sonnet-5", &days, Some(1_000_000)),
            entry("claude-sonnet-4-6", &days, Some(200_000)),
        ];
        // A family spec resolves to the family's elected newest, and the
        // size guard passes on the model that has actually held the
        // prompt.
        assert_eq!(
            compaction_target_of(&entries, "sonnet", 400_000),
            Some("claude-sonnet-5".to_owned())
        );
        // The same spec at a prompt only the 1M window has held: still
        // sonnet-5. At a prompt nothing has held: decline.
        assert_eq!(
            compaction_target_of(&entries, "sonnet", 900_000),
            Some("claude-sonnet-5".to_owned())
        );
        assert_eq!(compaction_target_of(&entries, "sonnet", 1_200_000), None);
        // An explicit id is a pin, not an election.
        assert_eq!(
            compaction_target_of(&entries, "claude-sonnet-4-6", 150_000),
            Some("claude-sonnet-4-6".to_owned())
        );
        // "off" and "" are nothing (a falsy spec).
        assert_eq!(compaction_target_of(&entries, "off", 1), None);
        assert_eq!(compaction_target_of(&entries, "", 1), None);
        // A family nothing served has no target, and an unknown name has
        // no family.
        assert_eq!(compaction_target_of(&entries, "haiku", 1), None);
        assert_eq!(compaction_target_of(&entries, "gpt-5.6-sol", 1), None);

        // The price filter (the `accept` hook): a family whose elected
        // newest cannot be priced is no use to a rewrite that judges
        // "cheaper" from the price table — it falls back to nothing
        // rather than switching the feature off on an unpriced candidate.
        let mixed = vec![
            entry("gpt-5.6-sol", &days, Some(1_000_000)),
            entry("claude-sonnet-5", &days, Some(1_000_000)),
        ];
        assert_eq!(
            compaction_target_of(&mixed, "gpt-5.6-sol", 1),
            None,
            "the election refuses a candidate it cannot price"
        );

        // Over the store: the same resolution, loading the entries.
        let models = models();
        models
            .store
            .upsert_model(&entry("claude-sonnet-5", &days, Some(1_000_000)))
            .expect("upsert");
        assert_eq!(
            models
                .compaction_target("sonnet", 400_000)
                .expect("resolve"),
            Some("claude-sonnet-5".to_owned())
        );
        assert_eq!(
            models.compaction_target("off", 400_000).expect("resolve"),
            None
        );
    }
}
