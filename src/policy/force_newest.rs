//! The inputs force-newest's resolver rules read: the no-cache-loss horizon
//! and the prompt-size bound.
//!
//! Force-newest moves a request onto the newest version of its model's family
//! that the log has actually proven, but only where no cache can be lost by
//! the move. The decision itself is the upgrade election in
//! [`super::resolve`]; this module holds the constants and the two pure
//! functions it consumes. The three no-cache-loss conditions, each of which
//! answers "moving this request cannot cost a cached prefix":
//!
//! - **a cold lane**: its cache is already gone, so the move is free;
//! - **an unknown lane with at most [`NEW_CONVERSATION_MESSAGES`] messages**:
//!   the table forgets lanes (a restart before the flush, an eviction), so an
//!   unknown lane is not the same as a new session, and a short request bounds
//!   what a mistake could cost to a system prompt and a tool list;
//! - **nothing sent on the asked model within a full cache TTL**
//!   ([`idle_for_ttl`]): a cache belongs to the model it was written on, so if
//!   nothing has been sent on that model for the longest TTL, no cache it
//!   could read exists. This is what admits a subagent that opens deep into an
//!   inherited conversation (measured 2026-09-25: a 45-tool lane asking for
//!   claude-opus-5 was never upgraded in any session because its first
//!   request always carried a history).
//!
//! Never a downgrade: without that condition the feature eats itself. The
//! first request naming a newer model arrives BEFORE that model is learned, so
//! "force to the newest known" would rewrite it down to the established
//! version, which would then never accumulate a single day and pin the account
//! below the new model permanently.
//!
//! **Sticky once moved**: an upgrade is decided once, where no cache can be
//! lost by it, and the conversation's cache then lives on the new model. The
//! client never learns of the rewrite and keeps asking for the old one, so
//! honouring that request is what would lose the cache; every upgraded
//! conversation paid its prefix twice, once on each model, before this rule.
//! Sticky is consulted only while the lane is warm: a lane that has gone cold
//! re-decides from scratch, because its cache is gone either way.
//!
//! The maxPrompt guard is empirical: a
//! conversation is only sent to a model that has been OBSERVED holding a
//! prompt that large, or whose catalogue lists a window that large.

/// What counts as "a conversation that has barely started" for a lane the
/// table has no record of (Claude Code opens with one user message; two
/// allows for a system-reminder turn without admitting a real history).
pub const NEW_CONVERSATION_MESSAGES: u64 = 2;

/// The longest a cache survives untouched: the 1-hour tier. The idle-for-TTL
/// condition reads the asked model's recency against this horizon, never the
/// lane's own tier, because warmth can come from places no lane records.
pub const CACHE_TTL_MAX_MS: i64 = 3_600_000;

/// Bytes a token, from below: 2.385 was the least observed over 17,608
/// single-iteration requests above 200k tokens (2026-09-25; median 2.83).
/// Smaller requests have run denser, but they are nowhere near a context
/// ceiling.
pub const MIN_BYTES_PER_TOKEN: u64 = 2;

/// An upper bound on a request's prompt size, before it is sent: the byte
/// count is all there is until the API reports usage. Server-side tool
/// iterations add tokens after this, which no pre-flight figure can see.
pub fn prompt_bound(bytes: u64) -> u64 {
    bytes.div_ceil(MIN_BYTES_PER_TOKEN)
}

/// Has nothing been sent on this model for a full cache TTL?
///
/// The lane table forgets lanes, so an unknown lane deep in a conversation may
/// be a warm session the proxy has lost track of, and warmth can come from
/// places no lane records. What a cache cannot survive is a change of model:
/// it belongs to the model it was written on. So if nothing has been sent on
/// the model this request asks for within [`CACHE_TTL_MAX_MS`], no cache it
/// could read exists, and moving it loses nothing.
///
/// Only as good as the record behind `last_seen`: silence counts for nothing
/// unless the record reaches back past the TTL, so a record that starts later
/// than the horizon answers false. Every doubtful value answers false, which
/// costs an upgrade; a wrong yes costs a rebuild of the whole prefix.
///
/// `covered_since` is the recency record's own reading: `None` means the
/// whole ledger was seeded (never fails the horizon check); `Some(ts)` is the
/// oldest row a cut tail kept.
pub fn idle_for_ttl(last_seen: Option<i64>, covered_since: Option<i64>, now_ms: i64) -> bool {
    let horizon = now_ms - CACHE_TTL_MAX_MS;
    if let Some(covered) = covered_since
        && covered > horizon
    {
        return false;
    }
    match last_seen {
        // Never sent, in a record that reaches back past the TTL: no cache
        // exists. (A last-seen in the future, a clock that moved, reads as
        // warm via the numeric arm below.)
        None => true,
        Some(last_seen) => last_seen <= horizon,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;
    const HOUR: i64 = 3_600_000;

    #[test]
    fn a_model_nothing_has_been_sent_on_for_an_hour_has_no_cache_to_lose() {
        // A model idle three hours (or never sent, in a record reaching back
        // far enough) may be moved.
        assert!(idle_for_ttl(Some(NOW - 3 * HOUR), None, NOW));
        assert!(idle_for_ttl(None, Some(NOW - 2 * HOUR), NOW));
        // A model sent on within the hour, by any session, may be warm.
        assert!(!idle_for_ttl(Some(NOW - 12_000), None, NOW));
        // The record can only vouch for the span it covers.
        assert!(
            !idle_for_ttl(None, Some(NOW - 10 * 60_000), NOW),
            "a ten-minute-old record's silence is not idleness"
        );
        // A last-seen in the future is a clock that moved, not idleness.
        assert!(!idle_for_ttl(Some(NOW + HOUR), None, NOW));
        // Exactly the horizon is idle (`<=`).
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
}
