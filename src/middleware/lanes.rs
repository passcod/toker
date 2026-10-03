//! The lane table: per `sessionId|toolsHash` cache state (plan: Middleware —
//! "Lane tracking + sleep lock"; the ctp lane rule,
//! claude-token-proxy/docs/internals/lanes.md).
//!
//! **A session is not a cache entry.** One session interleaves the main
//! agent, its subagents, and small utility calls, each with its own cached
//! prefix — so everything that compares a request against what preceded it
//! must compare within a lane, never across the session. Keyed on the
//! session alone, a two-token title summariser would stand in for the main
//! agent's 400k prefix *and* keep resetting its idle clock.
//!
//! A faithful port of ctp's lane table, measured over weeks of production
//! traffic — ported, not improved. Sources:
//!
//! - `laneKey` — ctp cold.mjs:391 (toker keeps ctp's `?`-collapse out: no
//!   session or no tools-hash means no lane at all, per this unit's plan);
//! - `laneTtl` (the TTL stickiness rule) — cold.mjs:519-523;
//! - `noteLaneResponse` — proxy.mjs:407-416;
//! - `lanesFromRows` (restart reseed) — cold.mjs:459-500;
//! - `pruneLanes` (the 4000-lane / 30-day policy) — cold.mjs:534-573, as
//!   SQL in the store ([`Store::prune_lanes`]) because toker has a database
//!   where ctp rewrote a whole file;
//! - `isPing` — proxy.mjs:446;
//! - the 30-second flush cadence — proxy.mjs:380-394.
//!
//! Where ctp kept an in-memory `Map` flushed to `lanes.json` every 30 s,
//! toker upserts straight into the `lanes` table on every response (one
//! single-row write, the same rate the ledger already writes at) and keeps
//! only the *prune* on ctp's 30-second timer — a cheap SQL statement run
//! periodically, never per request.
//!
//! Units: `updated_ms`/`noticed_at` are epoch milliseconds (ctp `Date.now()`
//! convention). The TTL tier is stored as its duration in milliseconds
//! (`300_000` = the 5-minute tier, `3_600_000` = the 1-hour tier), the same
//! shape the store's v1 schema already fixed.
//!
//! Absence ≠ zero (invariant 3): `prompt_tokens` and `ttl` are `None` until
//! a lane has been observed carrying them, and `ping` is `Some(true)` only
//! for a ping lane — never `Some(false)`.

use std::collections::BTreeMap;

use axum::http::HeaderMap;

use crate::catalog::windows::model_identity;
use crate::store::{Lane, RequestRow, RowKind, Store};

/// How many lanes the table keeps (ctp `LANE_MAX`, cold.mjs:377): 908
/// accumulated over a fortnight of heavy use, so this is generous rather
/// than tight.
pub const LANE_MAX: usize = 4000;

/// Beyond this a lane is not a session anyone is going to resume (ctp
/// `LANE_MAX_AGE_MS`, cold.mjs:380).
pub const LANE_MAX_AGE_MS: i64 = 30 * 24 * 3600 * 1000;

/// How often the lane table prunes (ctp `LANE_FLUSH_MS`, proxy.mjs:380): a
/// threshold measured in hours does not miss up to this much staleness, and
/// real I/O never lands on the request path.
pub const LANE_FLUSH_MS: u64 = 30_000;

/// The default header the window pinger tags its requests with (plan:
/// "Ping tagging — marks ping lanes so they never hold the sleep lock";
/// ctp's is `x-ctp-ping`, read by name like every request header —
/// invariant 2). Setup can rename it via config.
pub const DEFAULT_PING_HEADER: &str = "x-toker-ping";

/// The cache TTL tier a lane's cached prefix survives on (ctp `laneTtl`'s
/// two tiers). Stored in the lane row as its duration in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    /// The 5-minute cache-write tier.
    FiveMinutes,
    /// The 1-hour tier — the longest a cache survives untouched.
    Hour,
}

impl Ttl {
    /// The tier's duration in milliseconds, the store's column shape.
    pub fn as_ms(self) -> i64 {
        match self {
            Ttl::FiveMinutes => 300_000,
            Ttl::Hour => 3_600_000,
        }
    }

    /// The tier a stored `ttl` value names; `None` for an unrecorded or
    /// unrecognised value. ctp's `ttlOf` treats an unrecorded tier as the
    /// long one when reading (guessing short would fire on lanes whose
    /// cache is still live); that reading is the cold gate's, the next
    /// unit's — here absence stays absence.
    pub fn from_ms(ms: Option<i64>) -> Option<Ttl> {
        match ms {
            Some(300_000) => Some(Ttl::FiveMinutes),
            Some(3_600_000) => Some(Ttl::Hour),
            _ => None,
        }
    }

    /// The longest a cache on this tier survives untouched, in milliseconds.
    pub fn duration_ms(self) -> i64 {
        self.as_ms()
    }
}

/// The lane key: `sessionId|toolsHash` (ctp `laneKey`, cold.mjs:391).
///
/// `None` when either half is absent: a request without a session cannot be
/// attributed to a conversation, and one without a tools-hash has no lane
/// identity to compare against — ctp folded both into a shared `"?"` lane,
/// which exists for its pre-`toolsHash` log rows; toker's rows always carry
/// both when they carry either, so no lane is the cleaner answer.
pub fn lane_key(session_id: Option<&str>, tools_hash: Option<&str>) -> Option<String> {
    let session = session_id?;
    let tools = tools_hash?;
    Some(format!("{session}|{tools}"))
}

/// The cache TTL tier a lane carries, given what this request wrote (ctp
/// `laneTtl`, cold.mjs:519-523 — the stickiness rule, ported exactly).
///
/// The **longest-lived tier the lane has been observed writing**, and
/// sticky: a lane's cached prefix survives as long as its longest-lived
/// breakpoint, and a later short write does not shorten what is already
/// stored. This was originally the tier of the latest write, which is a
/// different and wrong quantity — measured on the live log, a warm turn
/// writes a small 5m delta on top of a prefix written earlier at 1h, and
/// reading it as a 5-minute lane fired notices on caches that were plainly
/// still live.
///
/// A turn that wrote nothing says nothing about the TTL of what is already
/// cached, so it leaves the tier alone.
pub fn lane_ttl(prev: Option<Ttl>, write_5m: u64, write_1h: u64) -> Option<Ttl> {
    if prev == Some(Ttl::Hour) || write_1h > 0 {
        return Some(Ttl::Hour);
    }
    if write_5m > 0 {
        return Some(Ttl::FiveMinutes);
    }
    prev
}

/// Does this request carry the ping header? (ctp `isPing`, proxy.mjs:446 —
/// `headers["x-ctp-ping"] === "1"`, read by name only: request headers are
/// never captured wholesale, they carry credentials — invariant 2.) The
/// exact literal `"1"` is ctp's, kept verbatim.
pub fn is_ping(headers: &HeaderMap, name: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "1")
}

/// A sticky model upgrade the lane must keep honouring (ctp's
/// `forced: {from, to}`): the upgrade is decided once, where no cache can be
/// lost by it, and the conversation's cache then lives on `to`. This unit
/// only carries the record — the force-newest rewrite that produces it is
/// the next unit's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forced {
    pub from: String,
    pub to: String,
}

impl Forced {
    /// From a lane row's two columns.
    fn from_lane(lane: &Lane) -> Option<Forced> {
        Some(Forced {
            from: lane.forced_from.clone()?,
            to: lane.forced_to.clone()?,
        })
    }

    fn into_lane_parts(self) -> (Option<String>, Option<String>) {
        (Some(self.from), Some(self.to))
    }
}

/// What one served response teaches the lane table (ctp `noteLaneResponse`'s
/// inputs, proxy.mjs:407-416 and 1767-1777).
#[derive(Debug, Clone)]
pub struct LaneResponse<'a> {
    pub session_id: Option<&'a str>,
    pub tools_hash: Option<&'a str>,
    /// When the response completed — epoch ms. ctp's `at` moves only here,
    /// because it means "when this lane's cache was last touched", not
    /// "when the proxy last saw a request for it".
    pub at_ms: i64,
    /// What a cold resume would have to re-read: the whole prefix, however
    /// it was billed this time — fresh input + cache read + cache writes.
    pub prompt: i64,
    /// This response's 5-minute-tier cache writes.
    pub write_5m: u64,
    /// This response's 1-hour-tier cache writes.
    pub write_1h: u64,
    /// The request carried the ping header: recorded, but excluded from
    /// liveness (a ping lane never holds the sleep lock).
    pub ping: bool,
    /// An upgrade this response made; `None` when it rewrote nothing.
    pub forced: Option<Forced>,
    /// Whether this response served a real compaction (the tool-set test,
    /// not `summarising` alone — see [`crate::ir::anthropic`]).
    pub compaction: bool,
}

/// Remember what a lane holds, after a response the API actually served
/// (ctp `noteLaneResponse`, proxy.mjs:407-416, over the store instead of an
/// in-memory `Map`).
///
/// The forced-keep rule is ctp's verbatim: a request served **unrewritten**
/// ends an upgrade (the user chose a model, or the lane went cold and was
/// decided afresh); a **compaction** does not — a cold one is rewritten on
/// its own terms and says nothing about the model the conversation resumes
/// on. `noticed_at` never moves here: nothing was measured and nothing
/// reached upstream when a notice fired, so the lane's prefix and cache age
/// are exactly what they were (see ctp `noteLaneNotice`, proxy.mjs:425-430).
///
/// Returns the stored lane, or `None` when there is no lane to key on.
/// A store error propagates; the caller keeps the request alive (invariant
/// 6) and logs.
pub fn note_lane_response(
    store: &Store,
    response: LaneResponse<'_>,
) -> crate::store::Result<Option<Lane>> {
    let Some(key) = lane_key(response.session_id, response.tools_hash) else {
        return Ok(None);
    };
    let prev = store.load_lane(&key)?;
    let lane = merge_lane(prev.as_ref(), &key, &response);
    store.upsert_lane(&lane)?;
    Ok(Some(lane))
}

/// The pure core of [`note_lane_response`]: `{ ...prev, at, prompt, ttl,
/// ping, forced: keep }` (ctp proxy.mjs:413-414). Exposed for the reseed's
/// tests; the prev-spread is what keeps `noticed_at` across responses.
fn merge_lane(prev: Option<&Lane>, key: &str, response: &LaneResponse<'_>) -> Lane {
    let keep = if response.compaction {
        // A compaction neither starts nor ends an upgrade.
        prev.and_then(Forced::from_lane)
    } else {
        response.forced.clone()
    };
    let (forced_from, forced_to) = keep.map(Forced::into_lane_parts).unwrap_or((None, None));
    Lane {
        key: key.to_owned(),
        session_id: response.session_id.map(str::to_owned),
        tools_hash: response.tools_hash.map(str::to_owned),
        updated_ms: response.at_ms,
        prompt_tokens: Some(response.prompt),
        ttl: lane_ttl(
            prev.and_then(|lane| Ttl::from_ms(lane.ttl)),
            response.write_5m,
            response.write_1h,
        )
        .map(Ttl::as_ms),
        // ctp's `ping || undefined`: only `true` is a ping — a lane
        // wrongly marked one is a session the machine may sleep through,
        // so a non-ping response clears the flag rather than sticking.
        ping: response.ping.then_some(true),
        // The prev-spread: a notice already given for this idle spell
        // survives every later response, and only the notice path sets it.
        noticed_at: prev.and_then(|lane| lane.noticed_at),
        forced_from,
        forced_to,
    }
}

/// Rebuild the lane table from ledger rows (ctp `lanesFromRows`,
/// cold.mjs:459-500 — the same per-row derivation, the same keying, the
/// same last-wins by ts).
///
/// The gate cannot fire for a lane it has no record of, and an empty table
/// is the state after every start — so without this the notice is blind to
/// exactly the sessions it exists for: the ones that went quiet before the
/// proxy last restarted. Measured: a session idle from 23:20 to 03:27
/// holding 508,188 tokens got no notice on resume, because the proxy had
/// never seen that lane speak. The log had known about it for four hours.
///
/// Only rows the API answered move the cache clock. A `cold` row is the
/// record that the notice was already given for this idle spell, and
/// restores `noticed_at` without touching `updated_ms` — the same
/// separation the live path keeps, for the same reason. Any other kind is a
/// row the proxy wrote about itself, or a request that failed: no usage, so
/// no evidence about the cache either way.
///
/// Rows must arrive in time order; the ledger reads oldest-first, so they do.
pub fn lanes_from_rows(rows: &[RequestRow]) -> BTreeMap<String, Lane> {
    let mut lanes: BTreeMap<String, Lane> = BTreeMap::new();
    for row in rows {
        if row.kind == Some(RowKind::Cold) {
            if let Some(key) = lane_key(row.session_id.as_deref(), row.tools_hash.as_deref())
                && let Some(lane) = lanes.get_mut(&key)
            {
                lane.noticed_at = Some(row.ts_ms);
            }
            continue;
        }
        // Any other kind is a row the proxy wrote about itself, or a request
        // that failed: no usage, no evidence about the cache.
        if row.kind.is_some() {
            continue;
        }
        // Lanes are an Anthropic cache concept (plan: wire only the
        // anthropic path for now): openai-chat rows carry a session id
        // (opencode sends `x-session-id`) and a tools hash, but their TTL
        // semantics are undefined — an unrecorded tier reads as the 1-hour
        // one, which would hold the sleep lock for an hour per request.
        if row.frontend.as_deref() != Some("anthropic") {
            continue;
        }
        let Some(key) = lane_key(row.session_id.as_deref(), row.tools_hash.as_deref()) else {
            continue;
        };
        let prev = lanes.get(&key);
        // An upgrade the proxy made, so a restart does not send the lane's
        // next request back to the model it was moved off; a compaction
        // neither starts nor ends one. Before host mapping, the response
        // model was the adaptive target; `forced_to` records it explicitly.
        let forced = if is_compaction_row(row) {
            prev.and_then(Forced::from_lane)
        } else {
            forced_from_row(row)
        };
        let (forced_from, forced_to) = forced.map(Forced::into_lane_parts).unwrap_or((None, None));
        let lane = Lane {
            updated_ms: row.ts_ms,
            prompt_tokens: Some(prompt_of(row)),
            ttl: lane_ttl(
                prev.and_then(|lane| Ttl::from_ms(lane.ttl)),
                row.cache_write_5m.unwrap_or(0).max(0) as u64,
                row.cache_write_1h.unwrap_or(0).max(0) as u64,
            )
            .map(Ttl::as_ms),
            ping: (row.ping == Some(true)).then_some(true),
            session_id: row.session_id.clone(),
            tools_hash: row.tools_hash.clone(),
            noticed_at: prev.and_then(|lane| lane.noticed_at),
            forced_from,
            forced_to,
            key,
        };
        lanes.insert(lane.key.clone(), lane);
    }
    lanes
}

/// Re-seed the lane table on startup (ctp `loadLanes`, proxy.mjs:226-246):
/// derive lanes from the ledger tail, then take the later reading wherever
/// the stored row disagrees, and remember a notice recorded by either.
///
/// Neither source is a superset of the other: the ledger is written per
/// request, the lanes table per response — the same either-can-be-fresher
/// property that made ctp merge the log seed with its file, except toker's
/// table is already durable, so the merge only has to reconcile.
///
/// Idempotent: running it over an already-seeded store changes nothing.
pub fn reseed(store: &Store, rows: &[RequestRow]) -> crate::store::Result<()> {
    let mut merged = lanes_from_rows(rows);
    for stored in store.load_lanes()? {
        let key = stored.key.clone();
        let derived = merged.remove(&key);
        // ctp: take the later reading, remember a notice recorded by either.
        let updated_ms = derived.as_ref().map_or(stored.updated_ms, |derived| {
            derived.updated_ms.max(stored.updated_ms)
        });
        let noticed_at = match (
            derived.as_ref().and_then(|l| l.noticed_at),
            stored.noticed_at,
        ) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let mut base = match derived {
            // The log-derived reading is the fresher of the two.
            Some(derived) if derived.updated_ms >= stored.updated_ms => derived,
            _ => stored,
        };
        base.updated_ms = updated_ms;
        base.noticed_at = noticed_at;
        store.upsert_lane(&base)?;
    }
    // Whatever the ledger knows that the table does not, yet.
    for lane in merged.into_values() {
        store.upsert_lane(&lane)?;
    }
    Ok(())
}

/// A row's prompt: what a cold resume of that request would re-read (ctp
/// `lanesFromRows`: `(r.input || 0) + (r.cacheRead || 0) +
/// (r.cacheCreateTotal || 0)` — missing metrics contribute 0, the sum is a
/// measurement that exists once any one of them does).
fn prompt_of(row: &RequestRow) -> i64 {
    row.input
        .unwrap_or(0)
        .saturating_add(row.cache_read.unwrap_or(0))
        .saturating_add(row.cache_write_total.unwrap_or(0))
}

/// ctp `isCompaction` over a ledger row (cold.mjs:701-706): the separator
/// between a compaction and the routine title summariser is the session's
/// tool set, not size. A row with no `req_tools` cannot say (`null` in ctp),
/// which reads as not-a-compaction here exactly as `isCompaction(r) === true`
/// reads false there.
fn is_compaction_row(row: &RequestRow) -> bool {
    row.summarising == Some(true) && row.req_tools.unwrap_or(0) > 0
}

/// The sticky-upgrade record from a ledger row (ctp `lanesFromRows`'s
/// `forced` derivation, cold.mjs:488-496).
fn forced_from_row(row: &RequestRow) -> Option<Forced> {
    let from = model_identity(row.forced_from.as_deref()?)?;
    // Before host mapping, the response model was the adaptive target.
    let to_source = row
        .forced_to
        .as_deref()
        .or(row.raw_model.as_deref())
        .or(row.model.as_deref())?;
    let to = model_identity(to_source)?;
    Some(Forced { from, to })
}

#[cfg(test)]
mod tests {
    use super::{
        Forced, LANE_MAX_AGE_MS, LaneResponse, Ttl, is_ping, lane_key, lane_ttl, lanes_from_rows,
        note_lane_response, reseed,
    };
    use crate::store::{Lane, RequestRow, RowKind, Store};
    use std::collections::BTreeMap;

    fn mem_store() -> Store {
        Store::open(":memory:").expect("open in-memory store")
    }

    fn response<'a>(
        session: Option<&'a str>,
        tools: Option<&'a str>,
        at_ms: i64,
    ) -> LaneResponse<'a> {
        LaneResponse {
            session_id: session,
            tools_hash: tools,
            at_ms,
            prompt: 1_000,
            write_5m: 0,
            write_1h: 0,
            ping: false,
            forced: None,
            compaction: false,
        }
    }

    fn measurement_row(session: Option<&str>, tools: Option<&str>, ts_ms: i64) -> RequestRow {
        let mut row = bare_row();
        row.ts_ms = ts_ms;
        row.session_id = session.map(str::to_owned);
        row.tools_hash = tools.map(str::to_owned);
        // Real measurement rows always carry the frontend; these lanes are
        // an anthropic concept (see the filter in `lanes_from_rows`).
        row.frontend = Some("anthropic".to_owned());
        row
    }

    /// A row with only `ts_ms` — every other column NULL (the store test's
    /// shape, local copy for brevity).
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

    // ── keying (ctp laneKey) ──────────────────────────────────────────

    #[test]
    fn a_lane_needs_both_a_session_and_a_tools_hash() {
        assert_eq!(
            lane_key(Some("ses-1"), Some("sha256:t1")),
            Some("ses-1|sha256:t1".to_owned())
        );
        // No lane without both: a request without a session cannot be
        // attributed, and one without a tools-hash has no lane identity.
        assert_eq!(lane_key(Some("ses-1"), None), None);
        assert_eq!(lane_key(None, Some("sha256:t1")), None);
        assert_eq!(lane_key(None, None), None);
    }

    #[test]
    fn responses_without_a_key_write_no_lane() {
        let store = mem_store();
        note_lane_response(&store, response(None, None, 1_000)).expect("note");
        note_lane_response(&store, response(Some("ses"), None, 2_000)).expect("note");
        note_lane_response(&store, response(None, Some("sha256:t1"), 3_000)).expect("note");
        assert!(store.load_lanes().expect("lanes").is_empty());
    }

    // ── TTL stickiness (ctp laneTtl) ──────────────────────────────────

    #[test]
    fn the_longest_lived_tier_wins_and_stays() {
        use Ttl::{FiveMinutes, Hour};
        // No writes, no prior: nothing learned, absence stays absence.
        assert_eq!(lane_ttl(None, 0, 0), None);
        // A 1h write sets the hour tier from anywhere.
        assert_eq!(lane_ttl(None, 0, 1), Some(Hour));
        assert_eq!(lane_ttl(Some(FiveMinutes), 5, 1), Some(Hour));
        // A 5m write sets the short tier only when nothing longer is held.
        assert_eq!(lane_ttl(None, 5, 0), Some(FiveMinutes));
        // THE stickiness rule: a later short write on top of a 1h prefix
        // must not shorten what is already stored — the measured case
        // (write5m 5,912, write1h 0, cacheRead 158,241) that once fired
        // notices at five minutes idle on plainly live caches.
        assert_eq!(lane_ttl(Some(Hour), 5, 0), Some(Hour));
        // A turn that wrote nothing leaves the tier alone.
        assert_eq!(lane_ttl(Some(FiveMinutes), 0, 0), Some(FiveMinutes));
        assert_eq!(lane_ttl(Some(Hour), 0, 0), Some(Hour));
    }

    #[test]
    fn ttl_round_trips_through_the_store_column() {
        assert_eq!(Ttl::from_ms(Some(300_000)), Some(Ttl::FiveMinutes));
        assert_eq!(Ttl::from_ms(Some(3_600_000)), Some(Ttl::Hour));
        assert_eq!(Ttl::from_ms(None), None, "unrecorded stays unrecorded");
        assert_eq!(
            Ttl::from_ms(Some(0)),
            None,
            "an unrecognised value is not a tier"
        );
        assert_eq!(Ttl::Hour.duration_ms(), 3_600_000);
    }

    // ── ping tagging (ctp isPing) ─────────────────────────────────────

    #[test]
    fn the_ping_header_is_read_by_name_for_the_exact_literal_one() {
        let mut headers = axum::http::HeaderMap::new();
        assert!(!is_ping(&headers, "x-toker-ping"));
        headers.insert("x-toker-ping", axum::http::HeaderValue::from_static("0"));
        assert!(!is_ping(&headers, "x-toker-ping"), "only \"1\" is a ping");
        headers.insert("x-toker-ping", axum::http::HeaderValue::from_static("true"));
        assert!(!is_ping(&headers, "x-toker-ping"));
        headers.insert("x-toker-ping", axum::http::HeaderValue::from_static("1"));
        assert!(is_ping(&headers, "x-toker-ping"));
    }

    // ── note_lane_response (ctp noteLaneResponse) ──────────────────────

    #[test]
    fn a_second_response_updates_the_lane_it_does_not_duplicate() {
        let store = mem_store();
        note_lane_response(&store, response(Some("ses-1"), Some("t1"), 1_000))
            .expect("note")
            .expect("a keyed response makes a lane");
        let mut second = response(Some("ses-1"), Some("t1"), 2_000);
        second.prompt = 250_000;
        second.write_1h = 82_420;
        let lane = note_lane_response(&store, second)
            .expect("note")
            .expect("lane");
        assert_eq!(
            store.load_lanes().expect("lanes").len(),
            1,
            "upsert, not insert"
        );
        assert_eq!(
            lane.updated_ms, 2_000,
            "`at` moves with the served response"
        );
        assert_eq!(lane.prompt_tokens, Some(250_000));
        assert_eq!(lane.ttl, Some(Ttl::Hour.as_ms()));
        // A different tools-hash is a different conversation, not an update.
        note_lane_response(&store, response(Some("ses-1"), Some("t2"), 3_000)).expect("note");
        assert_eq!(store.load_lanes().expect("lanes").len(), 2);
    }

    #[test]
    fn a_non_ping_response_clears_the_ping_flag() {
        // ctp `ping: ping || undefined` — the flag describes the latest
        // request, so a lane wrongly marked one stops being one.
        let store = mem_store();
        let mut pinged = response(Some("ses-1"), Some("t1"), 1_000);
        pinged.ping = true;
        let lane = note_lane_response(&store, pinged)
            .expect("note")
            .expect("lane");
        assert_eq!(lane.ping, Some(true));
        let lane = note_lane_response(&store, response(Some("ses-1"), Some("t1"), 2_000))
            .expect("note")
            .expect("lane");
        assert_eq!(lane.ping, None);
    }

    #[test]
    fn an_unrewritten_response_ends_an_upgrade_a_compaction_does_not() {
        let store = mem_store();
        let mut upgraded = response(Some("ses-1"), Some("t1"), 1_000);
        upgraded.forced = Some(Forced {
            from: "claude-opus-5".to_owned(),
            to: "claude-opus-5-5".to_owned(),
        });
        let lane = note_lane_response(&store, upgraded)
            .expect("note")
            .expect("lane");
        assert_eq!(lane.forced_from.as_deref(), Some("claude-opus-5"));
        assert_eq!(lane.forced_to.as_deref(), Some("claude-opus-5-5"));

        // A compaction neither starts nor ends an upgrade: it is rewritten
        // on its own terms and says nothing about the resumed model.
        let mut compaction = response(Some("ses-1"), Some("t1"), 2_000);
        compaction.compaction = true;
        let lane = note_lane_response(&store, compaction)
            .expect("note")
            .expect("lane");
        assert_eq!(lane.forced_from.as_deref(), Some("claude-opus-5"));

        // A request served unrewritten ends the upgrade: the user chose a
        // model, or the lane went cold and was decided afresh.
        let lane = note_lane_response(&store, response(Some("ses-1"), Some("t1"), 3_000))
            .expect("note")
            .expect("lane");
        assert_eq!(lane.forced_from, None);
        assert_eq!(lane.forced_to, None);
    }

    // ── reseed from rows (ctp lanesFromRows) ───────────────────────────

    #[test]
    fn rows_rebuild_lanes_last_wins_by_ts() {
        let mut rows = vec![
            measurement_row(Some("ses-1"), Some("t1"), 1_000),
            measurement_row(Some("ses-1"), Some("t2"), 2_000),
        ];
        rows[0].input = Some(100);
        rows[0].cache_read = Some(50);
        rows[0].cache_write_total = Some(25);
        // Same lane, later ts: the later reading is the lane.
        rows.push(measurement_row(Some("ses-1"), Some("t1"), 3_000));
        rows[2].input = Some(7);
        rows[2].cache_read = Some(3);
        rows[2].cache_write_5m = Some(1_200);

        let lanes = lanes_from_rows(&rows);
        assert_eq!(lanes.len(), 2, "a session's two tool sets are two lanes");
        let main = &lanes["ses-1|t1"];
        assert_eq!(main.updated_ms, 3_000, "last-wins by ts");
        assert_eq!(
            main.prompt_tokens,
            Some(10),
            "input + cacheRead + cacheCreateTotal"
        );
        assert_eq!(main.ttl, Some(Ttl::FiveMinutes.as_ms()));
        // Rows without a key seed nothing.
        rows.push(measurement_row(None, Some("t1"), 4_000));
        rows.push(measurement_row(Some("ses-1"), None, 5_000));
        assert_eq!(lanes_from_rows(&rows).len(), 2);
    }

    #[test]
    fn proxy_written_rows_never_move_the_cache_clock_except_the_cold_notice() {
        let mut rows = vec![measurement_row(Some("ses-1"), Some("t1"), 1_000)];
        rows[0].input = Some(40_000);
        for (ts, kind) in [
            (2_000, RowKind::Blocked),
            (3_000, RowKind::Awake),
            (4_000, RowKind::Error),
            (5_000, RowKind::FidelityDrift),
        ] {
            let mut row = measurement_row(Some("ses-1"), Some("t1"), ts);
            row.kind = Some(kind);
            rows.push(row);
        }
        let lanes = lanes_from_rows(&rows);
        let lane = &lanes["ses-1|t1"];
        assert_eq!(
            lane.updated_ms, 1_000,
            "only the API-answered row moves `at`"
        );
        assert_eq!(lane.prompt_tokens, Some(40_000));

        // A cold row is the record the notice already fired: it restores
        // noticedAt without touching `at` or the prompt.
        let mut cold = measurement_row(Some("ses-1"), Some("t1"), 6_000);
        cold.kind = Some(RowKind::Cold);
        rows.push(cold);
        let lanes = lanes_from_rows(&rows);
        let lane = &lanes["ses-1|t1"];
        assert_eq!(lane.updated_ms, 1_000);
        assert_eq!(lane.noticed_at, Some(6_000));
    }

    #[test]
    fn a_cold_row_for_an_unseen_lane_is_dropped_not_invented() {
        let mut cold = measurement_row(Some("ses-9"), Some("t9"), 1_000);
        cold.kind = Some(RowKind::Cold);
        let lanes = lanes_from_rows(&[cold]);
        assert!(
            lanes.is_empty(),
            "a notice cannot seed a lane the API never answered"
        );
    }

    #[test]
    fn openai_rows_grow_no_lanes_even_with_session_and_tools() {
        // opencode sends `x-session-id` and the openai shape carries a tools
        // hash, so its rows have both lane-key halves — but the lane TTL is
        // an anthropic cache concept; an unrecorded tier would read as the
        // 1-hour one and hold the sleep lock for an hour per request.
        let mut row = measurement_row(Some("ses-1"), Some("t1"), 1_000);
        row.frontend = Some("openai_chat".to_owned());
        let lanes = lanes_from_rows(&[row]);
        assert!(
            lanes.is_empty(),
            "reseed must not build lanes from openai-path rows"
        );
    }

    #[test]
    fn forced_is_rebuilt_from_the_row_and_survives_a_compaction() {
        let mut rows = vec![
            measurement_row(Some("ses-1"), Some("t1"), 1_000),
            measurement_row(Some("ses-1"), Some("t1"), 2_000),
        ];
        rows[0].input = Some(10);
        rows[1].input = Some(20);
        rows[1].forced_from = Some("claude-opus-5".to_owned());
        rows[1].forced_to = Some("claude-opus-5-5".to_owned());
        let lanes = lanes_from_rows(&rows);
        let lane = &lanes["ses-1|t1"];
        assert_eq!(lane.forced_from.as_deref(), Some("claude-opus-5"));
        assert_eq!(lane.forced_to.as_deref(), Some("claude-opus-5-5"));

        // A compaction keeps the record; a later unrewritten row ends it.
        let mut compaction = measurement_row(Some("ses-1"), Some("t1"), 3_000);
        compaction.input = Some(5);
        compaction.summarising = Some(true);
        compaction.req_tools = Some(17);
        compaction.forced_from = None;
        rows.push(compaction);
        let lanes = lanes_from_rows(&rows);
        assert_eq!(
            lanes["ses-1|t1"].forced_to.as_deref(),
            Some("claude-opus-5-5")
        );

        rows.push(measurement_row(Some("ses-1"), Some("t1"), 4_000));
        let lanes = lanes_from_rows(&rows);
        assert_eq!(lanes["ses-1|t1"].forced_to, None);

        // Without forced_to the response model is the target (ctp's
        // pre-host-mapping rule: rawModel, then model).
        let mut rows = vec![measurement_row(Some("ses-1"), Some("t1"), 1_000)];
        rows[0].forced_from = Some("claude-opus-5".to_owned());
        rows[0].raw_model = Some("claude-opus-5-5".to_owned());
        let lanes = lanes_from_rows(&rows);
        assert_eq!(
            lanes["ses-1|t1"].forced_to.as_deref(),
            Some("claude-opus-5-5")
        );
    }

    #[test]
    fn a_ping_row_tags_its_lane_and_a_later_row_clears_it() {
        let mut rows = vec![measurement_row(Some("ses-1"), Some("t1"), 1_000)];
        rows[0].input = Some(2);
        rows[0].ping = Some(true);
        let lanes = lanes_from_rows(&rows);
        assert_eq!(lanes["ses-1|t1"].ping, Some(true));

        rows.push(measurement_row(Some("ses-1"), Some("t1"), 2_000));
        let lanes = lanes_from_rows(&rows);
        assert_eq!(lanes["ses-1|t1"].ping, None);
    }

    // ── the reseed merge (ctp loadLanes) ──────────────────────────────

    #[test]
    fn reseed_takes_the_later_reading_and_remember_either_s_notice() {
        let store = mem_store();
        // A stored lane FRESHER than the ledger's last word for it.
        store
            .upsert_lane(&Lane {
                key: "ses-1|t1".to_owned(),
                session_id: Some("ses-1".to_owned()),
                tools_hash: Some("t1".to_owned()),
                updated_ms: 5_000,
                prompt_tokens: Some(90_000),
                ttl: Some(Ttl::Hour.as_ms()),
                ping: None,
                noticed_at: Some(4_500),
                forced_from: None,
                forced_to: None,
            })
            .expect("upsert stored lane");
        // A ledger-only lane the table never saw.
        let mut ledger_only = measurement_row(Some("ses-2"), Some("t2"), 3_000);
        ledger_only.input = Some(1_234);
        // A ledger reading FRESHER than the stored row.
        let mut fresher = measurement_row(Some("ses-3"), Some("t3"), 9_000);
        fresher.input = Some(42);
        store
            .upsert_lane(&Lane {
                key: "ses-3|t3".to_owned(),
                session_id: Some("ses-3".to_owned()),
                tools_hash: Some("t3".to_owned()),
                updated_ms: 7_000,
                prompt_tokens: Some(1),
                ttl: None,
                ping: None,
                noticed_at: None,
                forced_from: None,
                forced_to: None,
            })
            .expect("upsert stored lane");

        let seed = vec![ledger_only, fresher];
        reseed(&store, &seed).expect("reseed");
        let lanes: BTreeMap<String, Lane> = store
            .load_lanes()
            .expect("lanes")
            .into_iter()
            .map(|lane| (lane.key.clone(), lane))
            .collect();
        // The stored fresher reading wins as the base, and the notice either
        // side recorded survives.
        assert_eq!(lanes["ses-1|t1"].updated_ms, 5_000);
        assert_eq!(lanes["ses-1|t1"].prompt_tokens, Some(90_000));
        assert_eq!(lanes["ses-1|t1"].noticed_at, Some(4_500));
        // Ledger-only knowledge lands in the table.
        assert_eq!(lanes["ses-2|t2"].prompt_tokens, Some(1_234));
        // The ledger's later `at` wins where it is the fresher of the two.
        assert_eq!(lanes["ses-3|t3"].updated_ms, 9_000);
        assert_eq!(lanes["ses-3|t3"].prompt_tokens, Some(42));

        // Idempotent: re-running over the same inputs changes nothing.
        reseed(&store, &seed).expect("reseed again");
        assert_eq!(store.load_lanes().expect("lanes").len(), 3);
    }

    // ── the prune policy (ctp pruneLanes) is asserted at the store level;
    //    the constants it runs with are pinned here ─────────────────────

    #[test]
    fn the_prune_constants_are_ctps() {
        assert_eq!(LANE_MAX_AGE_MS, 30 * 24 * 3600 * 1000);
        assert_eq!(super::LANE_MAX, 4000);
        assert_eq!(super::LANE_FLUSH_MS, 30_000);
    }
}
