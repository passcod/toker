//! The force-newest model rewrite (plan: Middleware — "Force-newest model
//! rewrite"): transparently move a request onto the newest version of its
//! model's family that the log has actually proven — but only where no
//! cache can be lost by the move.
//!
//! A faithful port of ctp's `forceTarget`/`stickyTarget`/`idleForTtl`/
//! `promptBound` (models.mjs:415-469) and the sequencing block that calls
//! them (proxy.mjs:1444-1500), measured over weeks of production traffic —
//! ported, not improved. The decision is the server's slot to fill; the
//! body edit itself is [`crate::ir::AnthropicBodyMut::set_model`] — the
//! ONLY value this middleware changes, so unlike the compaction retarget
//! every `cache_control` survives: that rewrite ends a conversation whose
//! cache writes would never be read, while this one *starts* a
//! conversation that should cache its prefix on the model it is actually
//! going to use.
//!
//! The three no-cache-loss conditions (ctp proxy.mjs:1476-1487), each of
//! which answers "moving this request cannot cost a cached prefix":
//!
//! - **a cold lane** — its cache is already gone, so the rewrite is free.
//!   Coldness here is the cache question ([`lane_is_cold`] with
//!   `min_tokens: 0` — a new conversation is worth upgrading at any size),
//!   never the notice's "should the user be interrupted";
//! - **an unknown lane with ≤ [`NEW_CONVERSATION_MESSAGES`] messages** —
//!   the table forgets lanes (a restart before the flush, an eviction),
//!   so an unknown lane is not the same as a new session; a short request
//!   bounds what a mistake could cost to a system prompt and a tool list;
//! - **nothing served on the asked model within a full cache TTL**
//!   ([`idle_for_ttl`]) — a cache belongs to the model it was written on,
//!   so if nothing has been served on that model for the longest TTL, no
//!   cache it could read exists. This is what admits a subagent that
//!   opens deep into an inherited conversation (measured 2026-09-25: a
//!   45-tool lane asking for claude-opus-5 was never upgraded in any
//!   session because its first request always carried a history).
//!
//! Never a downgrade ([`force_target_of`]'s `newer_than`): without that
//! condition the feature eats itself — the first request naming a newer
//! model arrives BEFORE that model is in the store, so "force to the
//! newest known" would rewrite it down to the established version, which
//! would then never accumulate a single day and pin the account below the
//! new model permanently.
//!
//! **Sticky once moved** ([`sticky_target`]): an upgrade is decided once,
//! where no cache can be lost by it, and the conversation's cache then
//! lives on the new model. The client never learns of the rewrite and
//! keeps asking for the old one, so honouring that request is what would
//! lose the cache — every upgraded conversation paid its prefix twice,
//! once on each model, before this rule. Sticky is consulted only while
//! the lane is *warm* (ctp `known && !cold`): a lane that has gone cold
//! re-decides from scratch, because its cache is gone either way. Only a
//! request still naming the model that was rewritten sticks — one naming
//! anything else is the user choosing, and is left alone.
//!
//! The maxPrompt guard is empirical, never declared (invariant 3): a
//! conversation is only sent to a model that has been OBSERVED holding a
//! prompt that large. Context-window ceilings are display metadata and
//! are never rewrite evidence.
//!
//! Everything here is pure — a function of its inputs, with no clock
//! beyond the `now_ms` passed in (invariant 4). Store errors read as
//! absence: a missed upgrade, never a lost request (ctp's `try/catch`,
//! proxy.mjs:1499).

use crate::catalog::windows::model_identity;
use crate::middleware::cold::lane_is_cold;
use crate::middleware::lanes::Forced;
use crate::middleware::models::{
    ModelStore, family_of, fits_context, newer_than, newest_in_family,
};
use crate::store::{Lane, ModelEntry};

/// What counts as "a conversation that has barely started" for a lane the
/// table has no record of (ctp `NEW_CONVERSATION_MESSAGES`, proxy.mjs:140
/// — Claude Code opens with one user message; two allows for a
/// system-reminder turn without admitting a real history).
pub const NEW_CONVERSATION_MESSAGES: u64 = 2;

/// The longest a cache survives untouched: the 1-hour tier (ctp
/// `CACHE_TTL_MAX_MS`, models.mjs:425). The idle-for-TTL condition reads
/// the asked model's recency against this horizon, never the lane's own
/// tier — warmth can come from places no lane records.
pub const CACHE_TTL_MAX_MS: i64 = 3_600_000;

/// Bytes a token, from below (ctp `MIN_BYTES_PER_TOKEN`, models.mjs:462):
/// 2.385 was the least observed over 17,608 single-iteration requests
/// above 200k tokens (2026-09-25; median 2.83). Smaller requests have run
/// denser, but they are nowhere near a context ceiling.
pub const MIN_BYTES_PER_TOKEN: u64 = 2;

/// An upper bound on a request's prompt size, before it is sent (ctp
/// `promptBound`, models.mjs:469): the byte count is all there is until
/// the API reports usage. Server-side tool iterations add tokens after
/// this, which no pre-flight figure can see.
pub fn prompt_bound(bytes: u64) -> u64 {
    bytes.div_ceil(MIN_BYTES_PER_TOKEN)
}

/// Has nothing been served on this model for a full cache TTL? (ctp
/// `idleForTtl`, models.mjs:449-454.)
///
/// The lane table forgets lanes, so an unknown lane deep in a
/// conversation may be a warm session the proxy has lost track of — and
/// warmth can come from places no lane records. What a cache cannot
/// survive is a change of model: it belongs to the model it was written
/// on. So if nothing has been served on the model this request asks for
/// within [`CACHE_TTL_MAX_MS`], no cache it could read exists, and moving
/// it loses nothing.
///
/// Only as good as the record behind `last_seen` (ctp `coveredSince`):
/// silence counts for nothing unless the record reaches back past the
/// TTL, so a record that starts later than the horizon answers false.
/// Every doubtful value answers false, which costs an upgrade; a wrong
/// yes costs a rebuild of the whole prefix.
///
/// `covered_since` is [`ModelStore::covered_since`]'s reading: `None`
/// means the whole ledger was seeded (ctp's `-Infinity`, never fails the
/// horizon check); `Some(ts)` is the oldest row a cut tail kept.
pub fn idle_for_ttl(last_seen: Option<i64>, covered_since: Option<i64>, now_ms: i64) -> bool {
    let horizon = now_ms - CACHE_TTL_MAX_MS;
    if let Some(covered) = covered_since
        && covered > horizon
    {
        return false;
    }
    match last_seen {
        // Never served, in a record that reaches back past the TTL: no
        // cache exists. (ctp's `lastSeen === undefined || null` — a
        // last-seen in the future, a clock that moved, reads as warm via
        // the numeric arm below.)
        None => true,
        Some(last_seen) => last_seen <= horizon,
    }
}

/// The model this request should be sent to instead, or `None` to leave
/// it be (ctp `forceTarget`, models.mjs:415-422 — without ctp's optional
/// `accept` hook: the proxy's only call site passes none).
///
/// Strictly newer, always ([`newer_than`]); the family's learned-newest
/// ([`newest_in_family`]); and proven at `prompt` ([`fits_context`]).
pub fn force_target_of(entries: &[ModelEntry], model: &str, prompt: u64) -> Option<String> {
    let family = family_of(model)?;
    let best = newest_in_family(entries, &family.name)?;
    if !newer_than(&best, model) {
        return None;
    }
    if !fits_context(entries, &best, prompt) {
        return None;
    }
    Some(best)
}

/// The model a lane the proxy has already upgraded must stay on (ctp
/// `stickyTarget`, models.mjs:483-487).
///
/// Only a request still asking for the model that was rewritten sticks;
/// one naming anything else is the user choosing, and is left alone.
/// Identities normalise first, so a published snapshot id of the moved
/// model is recognised.
pub fn sticky_target(lane: &Lane, model: &str) -> Option<String> {
    let from = lane.forced_from.as_deref().and_then(model_identity)?;
    let to = lane.forced_to.as_deref().and_then(model_identity)?;
    let asked = model_identity(model)?;
    (asked == from).then_some(to)
}

/// Everything the decision reads about one request (ctp
/// proxy.mjs:1468-1487's block inputs): the asked model, the lane record,
/// the request shape, and the idle floor the cold gate shares. `now_ms`
/// is the only clock (invariant 4).
#[derive(Debug, Clone, Copy)]
pub struct ForceContext<'a> {
    /// The model the request asks for, as it stands after routing (ctp
    /// `asked`, proxy.mjs:1469 — `clientWants(body).model`). `None` when
    /// the body names no model: nothing to rewrite.
    pub model: Option<&'a str>,
    /// The identity the upstream will actually receive for `model` once
    /// the routing map is applied — ctp proxy.mjs:1480's
    /// `previewMappedModel(MODEL_MAP, asked)`, the preview hook into this
    /// decision. The served-recency condition reads THIS, never the asked
    /// model: a mapped request's cache lives on the target identity
    /// upstream, so warmth is the target's warmth — without the preview,
    /// a claimed model (never served as itself, only ever as its target)
    /// would always read as idle and force-newest would move requests the
    /// map is about to claim. `None` when no map is configured: the
    /// preview is then the asked model, which is what the decision
    /// assumed before the map existed.
    pub served_as: Option<&'a str>,
    /// The lane record, when the table holds one (ctp `known`). `None` is
    /// the unknown-lane case with its own, stricter eligibility.
    pub lane: Option<&'a Lane>,
    /// The request's message count (ctp `shape?.reqMessages`); `None`
    /// when the body carries no `messages` array — an unknown lane
    /// without one is never eligible, never guessed as short.
    pub req_messages: Option<u64>,
    /// The request is a compaction (ctp `isCompaction(shape) === true`):
    /// excluded from the *first* decision — a warm compaction reads the
    /// same cache, so it still sticks.
    pub compaction: bool,
    /// The request body's byte length (ctp `body.length`): the prompt
    /// bound's input when the lane is unknown and has no measured size.
    pub body_bytes: u64,
    /// The idle floor override (ctp `COLD_IDLE_MS`); `None` follows the
    /// TTL tier the lane was last seen writing.
    pub min_idle_ms: Option<i64>,
    /// Now, epoch milliseconds.
    pub now_ms: i64,
}

/// The whole answer for one request: leave it on the model it asked for,
/// or the sticky/learned move to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForceDecision {
    /// No rewrite: something would be lost by it, or nothing was learned.
    Leave,
    /// Rewrite onto `to` — `from` is the asked model (ctp's
    /// `forcedFrom`/`forcedTo`, proxy.mjs:1493-1494).
    Move(Forced),
}

/// Decide the force-newest rewrite for one request (ctp
/// proxy.mjs:1465-1500's decision, minus the body edit and the row
/// provenance, which are the server's slot).
///
/// The order is ctp's exactly: sticky first and only while the lane is
/// warm, then — when there is nothing to stick to and the request is not
/// a compaction — eligibility, the prompt figure, and the family
/// election. The prompt a known lane carries is its own measured size,
/// never the byte bound; an unknown lane has only the byte bound, because
/// a zero there waved every one past the context check once a deep
/// conversation could qualify.
///
/// A store error reads as absence (ctp's `try/catch`): a missed upgrade,
/// never a lost request.
pub fn decide(request: &ForceContext<'_>, models: &ModelStore) -> ForceDecision {
    let known = request.lane;
    // Whether the cache is gone, not whether the user has been spoken to
    // (ctp proxy.mjs:1472-1474): `minTokens: 0` because a new
    // conversation is worth upgrading at any size.
    let cold = known.is_some_and(|_| lane_is_cold(known, 0, request.min_idle_ms, request.now_ms));

    let asked = request.model;
    // Sticky once moved, and only while the lane's cache may still be live
    // (ctp proxy.mjs:1475: `known && !cold ? stickyTarget(...) : null`) —
    // a lane that has gone cold re-decides, since its cache is gone
    // either way. A warm compaction still sticks: it reads the same cache.
    let mut target = None;
    if known.is_some()
        && !cold
        && let Some(asked) = asked
    {
        target = known.and_then(|lane| sticky_target(lane, asked));
    }

    // The first decision excludes compactions; sticky above did not.
    if target.is_none()
        && !request.compaction
        && let Some(asked) = asked
    {
        // Eligibility (ctp proxy.mjs:1477-1482): a known lane qualifies
        // by coldness alone; an unknown one by a short conversation or by
        // the asked model having sat unserved for a full TTL — in which
        // case no cache it could read exists.
        let eligible = match known {
            Some(_) => cold,
            None => request.req_messages.is_some_and(|messages| {
                messages <= NEW_CONVERSATION_MESSAGES || {
                    // ctp proxy.mjs:1480: the recency lookup runs on the
                    // map preview, never the asked model — the cache a
                    // mapped request could read lives on the target.
                    let cache_identity = request.served_as.unwrap_or(asked);
                    let last_seen = models.last_served(cache_identity);
                    idle_for_ttl(last_seen, models.covered_since(), request.now_ms)
                }
            }),
        };
        if eligible {
            // An unknown lane has no measured size, and a zero here waved
            // every one past the context check: harmless while only
            // two-message conversations qualified, not once a 300-message
            // one could (ctp proxy.mjs:1483-1486).
            let prompt = match known {
                Some(lane) => lane.prompt_tokens.unwrap_or(0).max(0) as u64,
                None => prompt_bound(request.body_bytes),
            };
            target = models.force_target(asked, prompt).ok().flatten();
        }
    }

    match (asked, target) {
        (Some(asked), Some(to)) => ForceDecision::Move(Forced {
            from: asked.to_owned(),
            to,
        }),
        _ => ForceDecision::Leave,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CACHE_TTL_MAX_MS, ForceContext, ForceDecision, NEW_CONVERSATION_MESSAGES, decide,
        force_target_of, idle_for_ttl, prompt_bound, sticky_target,
    };
    use crate::middleware::lanes::Forced;
    use crate::middleware::models::ModelStore;
    use crate::store::{Lane, ModelEntry, Store};
    use serde_json::json;
    use std::sync::Arc;

    fn mem_store() -> Arc<Store> {
        Arc::new(Store::open(":memory:").expect("open in-memory store"))
    }

    fn entry(model_id: &str, days: &[&str], max_prompt: Option<i64>) -> ModelEntry {
        ModelEntry {
            model_id: model_id.to_owned(),
            days_json: Some(json!(days)),
            max_prompt,
            context_window_json: None,
        }
    }

    /// Nine days of history on every entry (ctp test/models-force.mjs's
    /// `SEEN`): comfortably above the election bar (needed = 4.5).
    const D: [&str; 9] = [
        "2026-09-01",
        "2026-09-02",
        "2026-09-03",
        "2026-09-04",
        "2026-09-05",
        "2026-09-06",
        "2026-09-07",
        "2026-09-08",
        "2026-09-09",
    ];

    /// The learned store ctp's force tests use: opus-5 and opus-4-8 both
    /// proven, opus-5 the elected newest.
    fn seeded_opus(store: &Arc<Store>) -> ModelStore {
        let models = ModelStore::seeded(store.clone(), &[], None);
        for (model, max_prompt) in [("claude-opus-5", 500_000i64), ("claude-opus-4-8", 500_000)] {
            store
                .upsert_model(&entry(model, &D, Some(max_prompt)))
                .expect("seed entry");
        }
        models
    }

    /// A lane record for the decision's inputs.
    fn lane(updated_ms: i64, prompt: Option<i64>) -> Lane {
        Lane {
            key: "ses|t1".to_owned(),
            session_id: Some("ses".to_owned()),
            tools_hash: Some("t1".to_owned()),
            updated_ms,
            prompt_tokens: prompt,
            ttl: None,
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
        }
    }

    /// The decision context for `model` on `lane`.
    fn context<'a>(
        model: Option<&'a str>,
        lane: Option<&'a Lane>,
        req_messages: Option<u64>,
        now_ms: i64,
    ) -> ForceContext<'a> {
        ForceContext {
            model,
            served_as: None,
            lane,
            req_messages,
            compaction: false,
            body_bytes: 1_000,
            min_idle_ms: None,
            now_ms,
        }
    }

    const NOW: i64 = 1_800_000_000_000; // a fixed, arbitrary now
    const HOUR: i64 = 3_600_000;

    // ── the election core (ctp forceTarget) ───────────────────────────

    #[test]
    fn an_older_version_is_upgraded_to_the_newest_in_its_family() {
        // The case this exists for (ctp test/models-force.mjs). maxPrompt
        // defaults to 1e9 there (`maxPrompt ?? 1e9`): proven at any size
        // the tests ask for.
        let entries = [
            entry("claude-opus-5", &D, Some(1_000_000_000)),
            entry("claude-opus-4-8", &D, Some(1_000_000_000)),
        ];
        assert_eq!(
            force_target_of(&entries, "claude-opus-4-8", 5_000),
            Some("claude-opus-5".to_owned())
        );
    }

    #[test]
    fn never_downgrade() {
        // What stops the feature eating itself: the first request naming
        // a newer model arrives BEFORE that model is in the store, so an
        // unconditional "force to newest known" would rewrite it down to
        // the established version — which would then never let it
        // accumulate a single day.
        let entries = [
            entry("claude-opus-5", &D, Some(1_000_000_000)),
            entry("claude-opus-4-8", &D, Some(1_000_000_000)),
            entry("claude-sonnet-5", &D, Some(1_000_000_000)),
        ];
        // An asked model newer than the elected newest is left alone.
        assert_eq!(force_target_of(&entries, "claude-opus-6", 5_000), None);
        // So is a request already on the newest.
        assert_eq!(force_target_of(&entries, "claude-opus-5", 5_000), None);
        // Families never borrow from each other, and a family nothing
        // served has no target.
        assert_eq!(force_target_of(&entries, "claude-sonnet-5", 5_000), None);
        assert_eq!(force_target_of(&entries, "claude-mythos-5", 5_000), None);
        // The election's bar still governs which family member counts as
        // newest: a one-day newcomer stays a trial, so the move lands on
        // the proven opus-5, never the barred opus-5-5.
        let young = [
            entry("claude-opus-5", &D, Some(1_000_000_000)),
            entry("claude-opus-5-5", &D[..1], Some(1_000_000_000)),
        ];
        assert_eq!(
            force_target_of(&young, "claude-opus-4-8", 5_000),
            Some("claude-opus-5".to_owned()),
            "the one-day 5-5 is barred; 5 holds the family"
        );
        // An identity with no family (an unpublished snapshot, a blank)
        // is never rewritten.
        assert_eq!(
            force_target_of(&entries, "claude-opus-4-9-20261225", 5_000),
            None
        );
        assert_eq!(force_target_of(&entries, "", 5_000), None);
    }

    #[test]
    fn a_model_must_be_observed_holding_a_conversation_this_size() {
        // Learned, not listed: the log records every prompt size against
        // every model, so an unproven model declines — which costs an
        // upgrade, where the alternative costs a failed request at the
        // worst possible moment.
        let small = [
            entry("claude-opus-5", &D, Some(50_000)),
            entry("claude-opus-4-8", &D, Some(500_000)),
        ];
        assert_eq!(
            force_target_of(&small, "claude-opus-4-8", 20_000),
            Some("claude-opus-5".to_owned()),
            "a prompt well inside what the target has served"
        );
        assert_eq!(
            force_target_of(&small, "claude-opus-4-8", 400_000),
            None,
            "a 400k conversation was sent to a model never seen serving more than 50k"
        );
        // Absence reads as zero, never a free pass (invariant 3).
        let unproven = [
            entry("claude-opus-5", &D, None),
            entry("claude-opus-4-8", &D, Some(500_000)),
        ];
        assert_eq!(force_target_of(&unproven, "claude-opus-4-8", 1), None);
    }

    // ── sticky (ctp stickyTarget) ────────────────────────────────────

    #[test]
    fn a_lane_stays_on_its_upgrade_while_its_cache_is_warm() {
        // ctp test/models-force.mjs's lane: moved off claude-opus-4-5.
        let mut upgraded = lane(NOW, Some(1));
        upgraded.forced_from = Some("claude-opus-4-5".to_owned());
        upgraded.forced_to = Some("claude-opus-5".to_owned());
        // The client never learns of the rewrite and keeps asking for
        // the old model: honouring that would rebuild the cache on it.
        assert_eq!(
            sticky_target(&upgraded, "claude-opus-4-5"),
            Some("claude-opus-5".to_owned())
        );
        // A published snapshot id of the moved model is recognised
        // (the alias table folds it to the same identity).
        assert_eq!(
            sticky_target(&upgraded, "claude-opus-4-5-20251101"),
            Some("claude-opus-5".to_owned())
        );
        // A request naming anything else is the user choosing.
        assert_eq!(sticky_target(&upgraded, "claude-sonnet-5"), None);
        // A lane never upgraded, an upgrade with no destination, and a
        // record with no identity never stick.
        assert_eq!(sticky_target(&lane(NOW, Some(1)), "claude-opus-4-8"), None);
        let mut no_to = lane(NOW, Some(1));
        no_to.forced_from = Some("claude-opus-4-8".to_owned());
        assert_eq!(sticky_target(&no_to, "claude-opus-4-8"), None);
        let mut unidentifiable = lane(NOW, Some(1));
        unidentifiable.forced_from = Some("claude-opus-4-8".to_owned());
        unidentifiable.forced_to = Some("".to_owned());
        assert_eq!(sticky_target(&unidentifiable, "claude-opus-4-8"), None);
    }

    // ── the served-recency condition (ctp idleForTtl) ────────────────

    #[test]
    fn a_model_nothing_has_been_served_on_for_an_hour_has_no_cache_to_lose() {
        // ctp test/models-force.mjs: a model idle three hours (or never
        // served, in a record reaching back far enough) may be moved.
        assert!(idle_for_ttl(Some(NOW - 3 * HOUR), None, NOW));
        assert!(idle_for_ttl(None, Some(NOW - 2 * HOUR), NOW));
        // A model served on within the hour, by any session, may be warm.
        assert!(!idle_for_ttl(Some(NOW - 12_000), None, NOW));
        // The record can only vouch for the span it covers.
        assert!(
            !idle_for_ttl(None, Some(NOW - 10 * 60_000), NOW),
            "a ten-minute-old record's silence is not idleness"
        );
        // A last-seen in the future is a clock that moved, not idleness.
        assert!(!idle_for_ttl(Some(NOW + HOUR), None, NOW));
        // Exactly the horizon is idle (ctp's `<=`).
        assert!(idle_for_ttl(Some(NOW - CACHE_TTL_MAX_MS), None, NOW));
    }

    #[test]
    fn the_prompt_bound_never_falls_below_what_was_measured() {
        // Measured 2026-09-25 over single-iteration requests: none of
        // 17,608 above 200k tokens ran below 2.385 bytes a token, so half
        // the byte count bounds the prompt from above.
        assert!(prompt_bound(2.385_f64 as u64 * 900_000) >= 900_000);
        assert_eq!(prompt_bound(0), 0);
        assert_eq!(prompt_bound(1), 1, "odd byte counts round up");
        assert_eq!(prompt_bound(4), 2);
        assert_eq!(NEW_CONVERSATION_MESSAGES, 2);
    }

    // ── the decision (ctp proxy.mjs:1465-1500) ───────────────────────

    #[test]
    fn a_cold_lane_is_moved_no_matter_how_big_its_conversation() {
        // Condition (a) alone: the lane is known and its cache is gone.
        // `minTokens: 0` — a new conversation is worth upgrading at any
        // size — so a 1000-token prefix still qualifies once the idle
        // spell has outlasted the TTL tier (an unrecorded tier is the
        // long one).
        let store = mem_store();
        let models = seeded_opus(&store);
        let cold = lane(NOW - 2 * HOUR, Some(1_000));
        assert_eq!(
            decide(
                &context(Some("claude-opus-4-8"), Some(&cold), None, NOW),
                &models
            ),
            ForceDecision::Move(Forced {
                from: "claude-opus-4-8".to_owned(),
                to: "claude-opus-5".to_owned(),
            })
        );
        // The idle floor override is the cold gate's config knob, shared.
        let soon = lane(NOW - 10 * 60_000, Some(1_000));
        let mut gated = context(Some("claude-opus-4-8"), Some(&soon), None, NOW);
        gated.min_idle_ms = Some(5 * 60_000);
        assert_eq!(
            decide(&gated, &models),
            ForceDecision::Move(Forced {
                from: "claude-opus-4-8".to_owned(),
                to: "claude-opus-5".to_owned(),
            }),
            "a ten-minute idle spell clears a five-minute floor"
        );
        gated.min_idle_ms = Some(HOUR);
        assert_eq!(decide(&gated, &models), ForceDecision::Leave);
    }

    #[test]
    fn an_unknown_lane_with_a_short_conversation_is_moved() {
        // Condition (b) alone: no lane record, but the request itself
        // shows a conversation that has barely started — which bounds
        // what a mistake could cost to a system prompt and a tool list.
        // The asked model is marked recently served so condition (c)
        // cannot be what qualifies (its case is the next test).
        let store = mem_store();
        let models = seeded_opus(&store);
        models.note_served(Some("claude-opus-4-8"), NOW - 12_000);
        assert_eq!(
            decide(
                &context(
                    Some("claude-opus-4-8"),
                    None,
                    Some(NEW_CONVERSATION_MESSAGES),
                    NOW
                ),
                &models
            ),
            ForceDecision::Move(Forced {
                from: "claude-opus-4-8".to_owned(),
                to: "claude-opus-5".to_owned(),
            })
        );
        // One message more is a real history, and a recently-served
        // model may be warm: not eligible either way.
        assert_eq!(
            decide(
                &context(
                    Some("claude-opus-4-8"),
                    None,
                    Some(NEW_CONVERSATION_MESSAGES + 1),
                    NOW
                ),
                &models
            ),
            ForceDecision::Leave
        );
        // No messages array at all: an unknown lane without a shape is
        // never guessed short.
        assert_eq!(
            decide(&context(Some("claude-opus-4-8"), None, None, NOW), &models),
            ForceDecision::Leave
        );
        // A body without a model has nothing to rewrite.
        assert_eq!(
            decide(&context(None, None, Some(1), NOW), &models),
            ForceDecision::Leave
        );
    }

    #[test]
    fn an_unknown_lane_whose_model_has_sat_unserved_for_a_ttl_is_moved() {
        // Condition (c) alone: the lane is unknown and the conversation
        // long, but nothing has been served on the asked model within a
        // full TTL — no cache it could read exists. This is what admits
        // a subagent opening deep into an inherited conversation. The
        // entries are proven at 2M tokens so the maxPrompt guard never
        // declines for a reason this test is not about.
        let store = mem_store();
        for (model, max_prompt) in [
            ("claude-opus-5", 2_000_000i64),
            ("claude-opus-4-8", 2_000_000),
        ] {
            store
                .upsert_model(&entry(model, &D, Some(max_prompt)))
                .expect("seed entry");
        }
        let models = ModelStore::seeded(store, &[], None);
        let mut deep = context(Some("claude-opus-4-8"), None, Some(300), NOW);
        deep.body_bytes = 2_400_000; // a bound of 1.2M tokens
        assert_eq!(
            decide(&deep, &models),
            ForceDecision::Move(Forced {
                from: "claude-opus-4-8".to_owned(),
                to: "claude-opus-5".to_owned(),
            }),
            "300 messages, but the asked model is cold everywhere"
        );

        // Serve on the asked model within the TTL and the same request
        // stays put: warmth no lane records is still warmth.
        models.note_served(Some("claude-opus-4-8"), NOW - 12_000);
        assert_eq!(decide(&deep, &models), ForceDecision::Leave);

        // A served-model record that does not reach back a full TTL
        // vouches for nothing, even in silence: the cut tail cannot say
        // the model was not served on fifteen minutes ago.
        let cut_store = mem_store();
        for (model, max_prompt) in [
            ("claude-opus-5", 2_000_000i64),
            ("claude-opus-4-8", 2_000_000),
        ] {
            cut_store
                .upsert_model(&entry(model, &D, Some(max_prompt)))
                .expect("seed entry");
        }
        let cut = ModelStore::seeded(cut_store, &[], Some(NOW - 10 * 60_000));
        assert_eq!(
            decide(&deep, &cut),
            ForceDecision::Leave,
            "the election would move it; the short record's silence must not"
        );
        // The same entries with whole-ledger coverage DO move — the
        // refusal above was the coverage's, never the election's.
        let whole_store = mem_store();
        for (model, max_prompt) in [
            ("claude-opus-5", 2_000_000i64),
            ("claude-opus-4-8", 2_000_000),
        ] {
            whole_store
                .upsert_model(&entry(model, &D, Some(max_prompt)))
                .expect("seed entry");
        }
        let whole = ModelStore::seeded(whole_store, &[], None);
        assert_eq!(
            decide(&deep, &whole),
            ForceDecision::Move(Forced {
                from: "claude-opus-4-8".to_owned(),
                to: "claude-opus-5".to_owned(),
            })
        );
    }

    #[test]
    fn the_eligibility_matrix_over_known_and_unknown_lanes() {
        let store = mem_store();
        let models = seeded_opus(&store);
        // A KNOWN lane qualifies by coldness alone — never by the
        // unknown lane's message-count or served-recency conditions.
        let warm_small = lane(NOW, Some(1_000));
        assert_eq!(
            decide(
                &context(Some("claude-opus-4-8"), Some(&warm_small), Some(1), NOW),
                &models
            ),
            ForceDecision::Leave,
            "a warm lane with a one-message follow-up still has its cache"
        );
        // An unknown lane never borrows the cold-lane condition: with no
        // record, a short conversation or TTL-idle model are all it has.
        // (Covered piecewise above; the matrix pins the cross products.)
        let cold = lane(NOW - 2 * HOUR, Some(1_000));
        assert_eq!(
            decide(
                &context(Some("claude-opus-4-8"), Some(&cold), Some(500), NOW),
                &models
            ),
            ForceDecision::Move(Forced {
                from: "claude-opus-4-8".to_owned(),
                to: "claude-opus-5".to_owned(),
            }),
            "a cold lane qualifies whatever the message count"
        );
        // A known lane with no measured prompt is never cold (absence ≠
        // zero, invariant 3) and never eligible.
        let unmeasured = lane(NOW - 2 * HOUR, None);
        assert_eq!(
            decide(
                &context(Some("claude-opus-4-8"), Some(&unmeasured), None, NOW),
                &models
            ),
            ForceDecision::Leave
        );
    }

    #[test]
    fn a_warm_lane_with_a_forced_record_sticks_to_the_recorded_target() {
        // Sticky beats re-election: the lane was moved onto opus-5 and its
        // cache lives there now, so it keeps opus-5 even once the
        // election has moved on to opus-5-5. A cold forced lane
        // re-decides instead — its cache is gone either way — and the
        // fresh election wins.
        let store = mem_store();
        for (model, max_prompt) in [
            ("claude-opus-5", 500_000i64),
            ("claude-opus-4-8", 500_000),
            ("claude-opus-5-5", 500_000),
        ] {
            store
                .upsert_model(&entry(model, &D, Some(max_prompt)))
                .expect("seed entry");
        }
        let models = ModelStore::seeded(store.clone(), &[], None);
        let mut moved = lane(NOW, Some(1_000));
        moved.forced_from = Some("claude-opus-4-8".to_owned());
        moved.forced_to = Some("claude-opus-5".to_owned());
        assert_eq!(
            decide(
                &context(Some("claude-opus-4-8"), Some(&moved), None, NOW),
                &models
            ),
            ForceDecision::Move(Forced {
                from: "claude-opus-4-8".to_owned(),
                to: "claude-opus-5".to_owned(),
            }),
            "the recorded target, not the newer election"
        );
        // A request naming a different model is the user choosing.
        assert_eq!(
            decide(
                &context(Some("claude-opus-5-5"), Some(&moved), None, NOW),
                &models
            ),
            ForceDecision::Leave
        );
        // Cold: sticky is not consulted, the lane re-decides, and the
        // election's newest wins.
        let mut gone = lane(NOW - 2 * HOUR, Some(1_000));
        gone.forced_from = Some("claude-opus-4-8".to_owned());
        gone.forced_to = Some("claude-opus-5".to_owned());
        assert_eq!(
            decide(
                &context(Some("claude-opus-4-8"), Some(&gone), None, NOW),
                &models
            ),
            ForceDecision::Move(Forced {
                from: "claude-opus-4-8".to_owned(),
                to: "claude-opus-5-5".to_owned(),
            }),
            "a cold lane re-decides; its cache is gone either way"
        );
    }

    #[test]
    fn a_compaction_never_gets_the_first_decision_but_a_warm_one_sticks() {
        let store = mem_store();
        let models = seeded_opus(&store);
        // A cold compaction on a lane with no upgrade: excluded from the
        // first decision (the compaction retarget owns cold compactions,
        // and when it moved the model this block does not run at all).
        let cold = lane(NOW - 2 * HOUR, Some(1_000));
        let mut compaction = context(Some("claude-opus-4-8"), Some(&cold), None, NOW);
        compaction.compaction = true;
        assert_eq!(decide(&compaction, &models), ForceDecision::Leave);
        // A warm compaction on a lane with an upgrade still sticks: it
        // reads the same cache, so it stays on the model that cache
        // lives on.
        let mut moved = lane(NOW, Some(1_000));
        moved.forced_from = Some("claude-opus-4-8".to_owned());
        moved.forced_to = Some("claude-opus-5".to_owned());
        let mut warm_compaction = context(Some("claude-opus-4-8"), Some(&moved), None, NOW);
        warm_compaction.compaction = true;
        assert_eq!(
            decide(&warm_compaction, &models),
            ForceDecision::Move(Forced {
                from: "claude-opus-4-8".to_owned(),
                to: "claude-opus-5".to_owned(),
            })
        );
    }

    #[test]
    fn the_prompt_figure_is_the_lane_s_own_measurement_never_a_guess() {
        // A KNOWN lane's size is its measured prompt — a 20k-prefix lane
        // moves even though its body bytes bound far higher (the byte
        // bound would have refused the target and cost the upgrade);
        // an UNKNOWN lane with the same bytes uses the bound.
        let store = mem_store();
        let models = seeded_opus(&store);
        let lane_20k = lane(NOW - 2 * HOUR, Some(20_000));
        let mut known = context(Some("claude-opus-4-8"), Some(&lane_20k), None, NOW);
        known.body_bytes = 2_400_000; // a bound of 1.2M tokens
        assert_eq!(
            decide(&known, &models),
            ForceDecision::Move(Forced {
                from: "claude-opus-4-8".to_owned(),
                to: "claude-opus-5".to_owned(),
            }),
            "the lane's measured 20k decided, not the 1.2M byte bound"
        );
        let mut unknown = context(Some("claude-opus-4-8"), None, Some(2), NOW);
        unknown.body_bytes = 2_400_000;
        assert_eq!(
            decide(&unknown, &models),
            ForceDecision::Leave,
            "the byte bound refused the 500k-proven target"
        );
    }

    #[test]
    fn the_map_preview_informs_the_recency_lookup_never_the_election() {
        // ctp proxy.mjs:1480: `idleForTtl` reads
        // `servedOn.get(modelIdentity(previewMappedModel(MODEL_MAP, asked)))`
        // — the cache a mapped request could read lives on the TARGET
        // identity upstream, so warmth is the target's warmth. Without
        // the preview a claimed model (only ever served as its target,
        // never as itself) would always read as idle and every unknown
        // lane with a history would be moved before the map could claim
        // it.
        let store = mem_store();
        for (model, max_prompt) in [
            ("claude-opus-5", 2_000_000i64),
            ("claude-opus-4-8", 2_000_000),
        ] {
            store
                .upsert_model(&entry(model, &D, Some(max_prompt)))
                .expect("seed entry");
        }
        let models = ModelStore::seeded(store, &[], None);

        // The map claims opus-4-8 → gpt-5.6-sol, and the TARGET has been
        // served within the TTL: the request is warm through the map, and
        // force-newest must not move it — the map is about to claim it.
        let mut deep = context(Some("claude-opus-4-8"), None, Some(300), NOW);
        deep.body_bytes = 2_400_000;
        deep.served_as = Some("gpt-5.6-sol");
        models.note_served(Some("gpt-5.6-sol"), NOW - 12_000);
        assert_eq!(
            decide(&deep, &models),
            ForceDecision::Leave,
            "the mapped target's warmth is the request's warmth"
        );

        // The same request with the target never served: no cache it
        // could read exists, and the move happens — the election still
        // runs on the ASKED model (the claude family's newest), never
        // the mapped target's family.
        let fresh_store = mem_store();
        for (model, max_prompt) in [
            ("claude-opus-5", 2_000_000i64),
            ("claude-opus-4-8", 2_000_000),
        ] {
            fresh_store
                .upsert_model(&entry(model, &D, Some(max_prompt)))
                .expect("seed entry");
        }
        let fresh = ModelStore::seeded(fresh_store, &[], None);
        assert_eq!(
            decide(&deep, &fresh),
            ForceDecision::Move(Forced {
                from: "claude-opus-4-8".to_owned(),
                to: "claude-opus-5".to_owned(),
            }),
            "the election is the asked model's family, the preview only \
             the recency lookup"
        );

        // No preview (no map configured): the recency lookup falls back
        // to the asked model — the committed decision's original
        // behaviour, which a mapped target's recent service must not
        // disturb.
        let mut unmapped = context(Some("claude-opus-4-8"), None, Some(300), NOW);
        unmapped.body_bytes = 2_400_000;
        assert_eq!(
            decide(&unmapped, &models),
            ForceDecision::Move(Forced {
                from: "claude-opus-4-8".to_owned(),
                to: "claude-opus-5".to_owned(),
            }),
            "without a preview the asked model's own recency decides"
        );
    }

    #[test]
    fn the_decision_is_a_pure_function_of_its_inputs() {
        // Same inputs, same answer — twice, and with an unrelated store
        // update in between that the decision must not read (only the
        // asked model's recency and the learned entries matter).
        let store = mem_store();
        let models = seeded_opus(&store);
        let cold = lane(NOW - 2 * HOUR, Some(1_000));
        let request = context(Some("claude-opus-4-8"), Some(&cold), None, NOW);
        assert_eq!(decide(&request, &models), decide(&request, &models));
        // A different lane with the same key shape but a warmer clock is
        // a different answer: nothing is cached across calls.
        let warm = lane(NOW, Some(1_000));
        let warmer = context(Some("claude-opus-4-8"), Some(&warm), None, NOW);
        assert_eq!(decide(&warmer, &models), ForceDecision::Leave);
    }
}
