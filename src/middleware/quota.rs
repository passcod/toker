//! The quota gate: meters → block-or-forward, plus the synthetic assistant
//! turn a block is answered with (plan: Middleware — "Quota gate + release
//! marker", the anthropic_sub backend being today's only meter source).
//!
//! Everything here is a faithful port of the predecessor proxy's quota
//! gate, measured over weeks of live traffic — ported, not improved.
//! Pieces:
//!
//! - the exhaustion test, window expiry, grant snapshot, and the
//!   block-or-forward walk ([`THRESHOLD`], [`expired`],
//!   [`exhausted_meters`], [`grant_for`], [`decide`]);
//! - `carriesRelease` / `stripSentinel` (the frozen marker rule) — the IR
//!   already ports those ([`crate::ir::anthropic`]: [`SENTINEL`],
//!   `release_marker`, `strip_release`), plus toker's own plan-only
//!   marker ([`crate::ir::PLAN_SENTINEL`], [`plan_room`]);
//! - the block notice and the synthetic turn it is answered with
//!   ([`Blocking`]);
//! - the request-pipeline sequencing (release check on the original body
//!   before the strip; the gate after it; blocked requests answered 200
//!   with a synthetic turn, never an error status) — see
//!   [`crate::server::anthropic`].
//!
//! Everything in this module is **pure**: state (the last meter snapshot,
//! the allowances) lives in the store, and every function here only
//! decides. The one deliberate impurity in the predecessor — the notice
//! names a
//! wall-clock time — is made an *input* here: the caller passes the
//! [`jiff::tz::TimeZone`] to render in, so the notice text stays a pure
//! function of its arguments (invariant 4 — a gate notice enters replayed
//! history and must be byte-stable forever).
//!
//! Units: meter resets are **epoch seconds** (the wire form in the
//! `anthropic-ratelimit-unified-*-reset` headers); `now` is **epoch
//! milliseconds** (a `Date.now()`-style convention, kept so the vendored
//! contract fixture's `now` values dispatch unchanged).
//!
//! Absence ≠ a limit (invariant 3): unknown meters forward. A proxy that
//! has seen no response yet knows nothing about the window, and blocking on
//! that would stop every session on a cold start.

use serde_json::Value;

use super::notice::{NoticeLevel, NoticeStyle, TOKER_LINK, render};
use crate::ir::Release;
use crate::store::Allowance;

/// A meter is exhausted when its utilisation reaches this fraction of the
/// window.
pub const THRESHOLD: f64 = 0.99;

/// The synthetic turn's model when the request names none (a stand-in the
/// client renders like any other).
const DEFAULT_MODEL: &str = "claude-opus-5";

/// The id on every synthetic assistant turn, so a reader of the client's
/// transcript or the ledger can tell a proxy answer from a provider one.
/// (The predecessor's rows carry `msg_ctp_blocked`; toker signs its own
/// name — the id is
/// cosmetic to the client, which renders the turn as a normal message
/// either way.)
const SYNTHETIC_ID: &str = "msg_toker_blocked";

/// Which quota window a decision names. Order matters: [`exhausted_meters`]
/// returns them in the order a notice should name them, and [`decide`]
/// walks that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Meter {
    /// The 5-hour window (also the meter an overage draw is booked against,
    /// unless the 7-day window is the cause — see [`exhausted_meters`]).
    FiveHour,
    /// The 7-day window.
    SevenDay,
}

impl Meter {
    /// The stable short name, the ledger spelling (`"5h"`/`"7d"`) — also
    /// the `meter` key of an [`Allowance`] row and of the vendored
    /// contract fixture.
    pub fn as_str(self) -> &'static str {
        match self {
            Meter::FiveHour => "5h",
            Meter::SevenDay => "7d",
        }
    }

    /// Parse the stable short name (the fixture's `meter` field, the
    /// allowances table's `meter` column).
    pub fn parse(name: &str) -> Option<Meter> {
        match name {
            "5h" => Some(Meter::FiveHour),
            "7d" => Some(Meter::SevenDay),
            _ => None,
        }
    }

    /// The human spelling the notice uses.
    pub fn notice_name(self) -> &'static str {
        match self {
            Meter::FiveHour => "5-hour",
            Meter::SevenDay => "7-day",
        }
    }
}

/// A typed view over the parsed rate-limits JSON (the store keeps the
/// snapshot whole as a [`Value`], the stable wire shape — see
/// [`crate::providers::anthropic::parse_rate_limits`]). This view reads
/// only the fields the gate decides on: util/reset per window, whether
/// spend has shifted to overage, and the binding claim.
///
/// Every read is absent-when-missing, never zero (invariant 3). Resets are
/// epoch seconds, always integral on the wire; a non-integral reset value
/// reads as absent, which the gate treats as a window it cannot see (a
/// spent util with an unreadable reset blocks with `resets_at: None`, the
/// same verdict as a reset the header never carried).
#[derive(Debug, Clone, Copy)]
pub struct Meters<'a> {
    snapshot: &'a Value,
}

impl<'a> Meters<'a> {
    /// View over one parsed meter snapshot (a request's `rate_limits`, or
    /// the store's `meters_state` snapshot).
    pub fn over(snapshot: &'a Value) -> Meters<'a> {
        Meters { snapshot }
    }

    fn util(&self, key: &str) -> Option<f64> {
        self.snapshot.get(key).and_then(Value::as_f64)
    }

    fn reset(&self, key: &str) -> Option<i64> {
        self.snapshot.get(key).and_then(Value::as_i64)
    }

    /// 5-hour window utilisation, as reported.
    pub fn util5h(&self) -> Option<f64> {
        self.util("util5h")
    }

    /// 5-hour window reset, epoch seconds.
    pub fn reset5h(&self) -> Option<i64> {
        self.reset("reset5h")
    }

    /// 7-day window utilisation, as reported.
    pub fn util7d(&self) -> Option<f64> {
        self.util("util7d")
    }

    /// 7-day window reset, epoch seconds.
    pub fn reset7d(&self) -> Option<i64> {
        self.reset("reset7d")
    }

    /// Whether this reading already draws on overage rather than plan
    /// quota (exactly the literal `true` on the header, so anything but
    /// that is false).
    pub fn overage_in_use(&self) -> bool {
        self.snapshot.get("overageInUse") == Some(&Value::Bool(true))
    }

    /// Which claim is currently binding (the `representative-claim`
    /// header). The decision functions do not read this — it is the
    /// forecast and TUI's signal — but the view is where a meter-sourced
    /// snapshot is read, so it is read here.
    pub fn claim(&self) -> Option<&'a str> {
        self.snapshot.get("claim").and_then(Value::as_str)
    }
}

/// Has this meter's window already ended? (The rule that un-wedges the
/// gate.)
///
/// A reading describes the window it was taken in. Once that window's
/// reset instant passes, the reading says nothing about the one now
/// running — and with no traffic there is nothing to replace it: measured
/// gaps between a reset and the first reading of the new window run from
/// 30 seconds to 22 hours. Reading a spent figure across that gap wedges
/// the gate: a blocked request never reaches upstream, so it never brings
/// back fresh meters, so the next request is blocked on the same stale
/// figure, forever. Forwarding instead is self-correcting — being more
/// restrictive than the API about a window we cannot see is not this
/// gate's job. An absent reset reads as not-expired (non-numbers
/// never expire), so a spent util with no reset still blocks, with
/// `resets_at: None` naming the ignorance.
pub fn expired(reset_seconds: Option<i64>, now_ms: i64) -> bool {
    matches!(reset_seconds, Some(reset) if reset.saturating_mul(1000) <= now_ms)
}

/// Which meters are currently exhausted, in the order a notice should name
/// them.
///
/// `overageInUse` counts as the 5-hour meter being gone — it means spend
/// has already shifted off plan quota, which is the thing being prevented —
/// *unless* the 7-day meter is spent, in which case the overage is its
/// doing. (Observed 2026-09-25 with the 5-hour meter at 0.21: blaming it
/// named the wrong meter and reset time, and pinned the release to the
/// 5-hour reset, so it lapsed every five hours while the weekly overage
/// ran on.) Unknown meters — `None` — exhaust nothing.
pub fn exhausted_meters(meters: Option<Meters<'_>>, now_ms: i64) -> Vec<Meter> {
    let Some(meters) = meters else {
        return Vec::new();
    };
    let seven_gone =
        !expired(meters.reset7d(), now_ms) && meters.util7d().is_some_and(|util| util >= THRESHOLD);
    let mut out = Vec::new();
    if !expired(meters.reset5h(), now_ms)
        && (meters.util5h().is_some_and(|util| util >= THRESHOLD)
            || (meters.overage_in_use() && !seven_gone))
    {
        out.push(Meter::FiveHour);
    }
    if seven_gone {
        out.push(Meter::SevenDay);
    }
    out
}

/// The allowance a release grants right now: the current reset value for
/// each meter that is exhausted, and `None` for each that is not.
///
/// Storing the reset **value** rather than a timestamp is what makes the
/// allowance expire without a clock: when the window rolls, the reported
/// reset changes and no longer matches, so the allowance is simply gone.
/// Granting only for meters that are exhausted is what stops a release for
/// the afternoon from quietly becoming a release for the week — a fresh
/// `None` defers to whatever allowance is already held (the merge happens
/// in the server wiring).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    /// The 5-hour window a release covers, by reset value; `None` when the
    /// 5-hour meter was not exhausted.
    pub five_hour: Option<i64>,
    /// The 7-day window a release covers, by reset value; `None` when the
    /// 7-day meter was not exhausted.
    pub seven_day: Option<i64>,
}

/// See [`Grant`].
pub fn grant_for(meters: Option<Meters<'_>>, now_ms: i64) -> Grant {
    let gone = exhausted_meters(meters, now_ms);
    Grant {
        five_hour: if gone.contains(&Meter::FiveHour) {
            meters.and_then(|meters| meters.reset5h())
        } else {
            None
        },
        seven_day: if gone.contains(&Meter::SevenDay) {
            meters.and_then(|meters| meters.reset7d())
        } else {
            None
        },
    }
}

/// The grant a TUI release makes, ahead of time: the 5-hour window's
/// current reset whenever that window has not ended, exhausted or not, so
/// a session can be opened before it is stopped; the 7-day window's only
/// when it is exhausted, as with [`grant_for`], so a release for the
/// afternoon cannot quietly become one for the week. A window whose
/// reading has ended names no reset worth granting for: the next one's
/// is not known until a response reports it.
pub fn grant_ahead(meters: Option<Meters<'_>>, now_ms: i64) -> Grant {
    let five_hour = meters
        .and_then(|meters| meters.reset5h())
        .filter(|&reset| !expired(Some(reset), now_ms));
    Grant {
        five_hour,
        seven_day: grant_for(meters, now_ms).seven_day,
    }
}

/// Block or forward.
///
/// `allowances` are the session's stored [`Allowance`] rows — the caller
/// loads and session-filters them. An allowance un-gates the exhausted
/// meter it covers **iff its stored reset value is the meter's currently
/// reported reset**: an allowance cannot outlive its window, because a
/// rolled window reports a different reset and no longer matches. Stale
/// rows from rolled windows are inert by the same rule (enforcement needs
/// no clock; pruning them, `Store::prune_allowances` on the server's
/// state-prune tick as in the predecessor's load-time prune, is only
/// housekeeping).
///
/// Unknown meters forward (see the module docs): absence of
/// instrumentation must never read as presence of the phenomenon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// No exhausted meter, or every exhausted one is released for the
    /// window it reports.
    Forward,
    /// Answer with a synthetic turn; do not forward. `resets_at` is the
    /// blocked meter's currently reported reset (epoch seconds), `None`
    /// when the reading carried none.
    Block {
        /// Which meter hit its limit.
        meter: Meter,
        /// When that meter's window resets, epoch seconds — the row's
        /// `resetsAt`.
        resets_at: Option<i64>,
    },
}

/// Whether `meter`'s plan quota still has room: its utilisation reads
/// under 1.0 and spend has not shifted to overage. A plan release
/// ([`Release::Plan`]) un-gates an exhausted meter only while this holds,
/// so the session stops again at the point the next request would be
/// billed as overage. An unreadable utilisation has no room: a plan
/// release is a promise not to reach overage, and a window it cannot see
/// is one it cannot keep that promise in.
pub fn plan_room(meters: Meters<'_>, meter: Meter) -> bool {
    let util = match meter {
        Meter::FiveHour => meters.util5h(),
        Meter::SevenDay => meters.util7d(),
    };
    !meters.overage_in_use() && util.is_some_and(|util| util < 1.0)
}

/// See [`GateDecision`].
pub fn decide(meters: Option<Meters<'_>>, allowances: &[Allowance], now_ms: i64) -> GateDecision {
    let Some(meters) = meters else {
        return GateDecision::Forward;
    };
    for meter in exhausted_meters(Some(meters), now_ms) {
        let current = match meter {
            Meter::FiveHour => meters.reset5h(),
            Meter::SevenDay => meters.reset7d(),
        };
        // Released for this window: a stored reset that is present and
        // equals the meter's currently reported one, and, for a plan
        // release, a plan that still has room.
        let released = current.is_some_and(|reset| {
            allowances.iter().any(|allowance| {
                allowance.meter == meter.as_str()
                    && allowance.reset_value == reset
                    && match allowance.release {
                        Release::Overage => true,
                        Release::Plan => plan_room(meters, meter),
                    }
            })
        });
        if !released {
            return GateDecision::Block {
                meter,
                resets_at: current,
            };
        }
    }
    GateDecision::Forward
}

/// Which wire form a blocked request is answered in (a client that
/// *explicitly* asked for a plain JSON Message cannot parse an
/// event stream; everyone else — `stream: true` or the field omitted — gets
/// the SSE turn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rendering {
    /// An SSE event stream.
    Sse,
    /// A plain JSON Message body.
    Json,
}

/// The blocked answer: the notice text, and the synthetic assistant turn
/// that carries it. Shared shape, measured against a real client on
/// 2026-09-10 with the predecessor proxy — a 200 renders the notice verbatim, exits clean, and
/// cannot be retried; an error status is worse than useless (529 retried
/// eight times over 46 s and printed nothing, so control never returned to
/// the keyboard and the release marker could never be typed; 429 is
/// captioned "not your usage limit"; 403 reads as broken credentials).
pub struct Blocking;

impl Blocking {
    /// The text the user sees (natively rendered — plan:
    /// "Native rendering for gate notices"). It is the entire interface of
    /// this feature — the only place they learn which meter they hit, when
    /// it clears, and how to resume — so it says all three.
    ///
    /// Third-person on purpose, and framed by the style (brackets in the
    /// plain style), so the model reads it as harness output rather than
    /// as its own words. The
    /// marker itself is deliberately not embedded — it would then sit in
    /// conversation history as assistant text.
    ///
    /// The content is then wrapped per `style` ([`render`]) at the
    /// [`NoticeLevel::Caution`] level — a stopped session — decided here,
    /// where the notice is written. The style is the frontend's (the
    /// server resolves it from the request's `/f/<frontend>` prefix
    /// through `[notices]`); here it is just one more input.
    ///
    /// Pure function of its inputs, style included (invariant 4): the
    /// reset time renders in the passed timezone, to the minute (`%H:%M`,
    /// 24-hour — the predecessor followed the process locale's hour
    /// cycle, which has
    /// no jiff equivalent; the local timezone is the part of that which
    /// matters, the hour convention is pinned instead of guessed). The
    /// context size is stated and nothing is concluded from it; dropped
    /// entirely when unknown or zero rather than printed as a zero (a
    /// notice reading "0 tokens" would be read as a measurement). A reset
    /// the reading did not carry is "an unknown time" — the same verdict
    /// as a null `resetsAt`. `plan_left` ([`plan_room`] for the blocked
    /// meter) adds the plan-only marker to the resume line.
    pub fn notice(
        meter: Meter,
        resets_at: Option<i64>,
        plan_left: bool,
        context_tokens: Option<u64>,
        tz: &jiff::tz::TimeZone,
        style: NoticeStyle,
    ) -> String {
        let when = resets_at
            .and_then(|seconds| jiff::Timestamp::from_second(seconds).ok())
            .map(|timestamp| timestamp.to_zoned(tz.clone()).strftime("%H:%M").to_string());
        // With plan left the quota is at the gate, not past it: "spent"
        // would contradict the resume line offering the rest of it.
        let spent = match (when, plan_left) {
            (Some(when), false) => format!("is spent until {when}"),
            (None, false) => "is spent, and its reset time is unknown".to_owned(),
            (Some(when), true) => format!("is almost spent, and resets at {when}"),
            (None, true) => "is almost spent, and its reset time is unknown".to_owned(),
        };
        // Comma-grouped, never locale-moving — these figures
        // land in notices the tests assert on.
        let size = match context_tokens {
            Some(tokens) if tokens > 0 => {
                format!(" This session's context is {} tokens.", group(tokens))
            }
            _ => String::new(),
        };
        // The over marker is offered only where it would get somewhere:
        // with the plan spent or overage drawn, it would stop at once.
        let resume = if plan_left {
            "Reply with the over marker to use the rest without overage, \
             or the release marker to continue on overage until then."
        } else {
            "Reply with the release marker to continue on overage until then."
        };
        let content = format!(
            "Stopped by {TOKER_LINK}: the {} quota {spent}.{size}\n{resume}",
            meter.notice_name(),
        );
        // A Caution: the session is stopped until the operator acts.
        render(style, NoticeLevel::Caution, &content)
    }

    /// A synthetic assistant turn carrying `text`, in the requested
    /// rendering. `model` is the client's own request model (read
    /// before any rewrite), defaulting to [`DEFAULT_MODEL`] when the
    /// request named none.
    pub fn blocked_turn(text: &str, model: Option<&str>, rendering: Rendering) -> Vec<u8> {
        match rendering {
            Rendering::Sse => Blocking::sse_turn(text, model),
            Rendering::Json => Blocking::json_turn(text, model),
        }
    }

    /// The turn as an event stream — byte for byte
    /// in event shape, order, and zeroed usage (nothing
    /// reached upstream, and a synthetic row that claimed tokens would be
    /// counted by every view that reads the ledger): `message_start`,
    /// `content_block_start`, `content_block_delta` carrying the notice,
    /// `content_block_stop`, `message_delta` with `end_turn`, `message_stop`.
    pub fn sse_turn(text: &str, model: Option<&str>) -> Vec<u8> {
        let model = model
            .filter(|model| !model.is_empty())
            .unwrap_or(DEFAULT_MODEL);
        let mut out = String::new();
        sse_event(
            &mut out,
            "message_start",
            &serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": SYNTHETIC_ID,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                    },
                },
            }),
        );
        sse_event(
            &mut out,
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            }),
        );
        sse_event(
            &mut out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text},
            }),
        );
        sse_event(
            &mut out,
            "content_block_stop",
            &serde_json::json!({"type": "content_block_stop", "index": 0}),
        );
        sse_event(
            &mut out,
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": 0},
            }),
        );
        sse_event(
            &mut out,
            "message_stop",
            &serde_json::json!({"type": "message_stop"}),
        );
        out.into_bytes()
    }

    /// The same turn for a request that asked for `"stream": false`:
    /// a client that asked for a
    /// plain JSON Message cannot parse an event stream. Same zeroed usage.
    pub fn json_turn(text: &str, model: Option<&str>) -> Vec<u8> {
        let model = model
            .filter(|model| !model.is_empty())
            .unwrap_or(DEFAULT_MODEL);
        serde_json::to_vec(&serde_json::json!({
            "id": SYNTHETIC_ID,
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
            },
        }))
        .expect("a json! object always serialises")
    }
}

/// One SSE event block: `event:` line, `data:` line, blank separator —
/// the joined lines plus trailing
/// newline produce exactly this per-event shape.
fn sse_event(out: &mut String, event: &str, data: &Value) {
    out.push_str("event: ");
    out.push_str(event);
    out.push('\n');
    out.push_str("data: ");
    out.push_str(&serde_json::to_string(data).expect("a json! value always serialises"));
    out.push_str("\n\n");
}

/// Comma-grouped digit rendering: deliberately
/// NOT locale-moving — these figures land in notices whose bytes are
/// pinned by tests and replayed in history (invariant 4). Shared with the
/// cold gate's notice.
pub(crate) fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Allowance, Blocking, GateDecision, Grant, Meter, Meters, Rendering, THRESHOLD, decide,
        exhausted_meters, expired, grant_ahead, grant_for, plan_room,
    };
    use crate::ir::Release;
    use crate::middleware::notice::{BLOCK_FOOTER, BLOCK_HEADER, NoticeStyle};
    use serde_json::json;

    /// A spent 5-hour window with its reset comfortably in the future, and
    /// a healthy 7-day one — the fixture's shape.
    fn spent_5h() -> serde_json::Value {
        json!({
            "util5h": 0.995,
            "reset5h": 2_000_000_600,
            "util7d": 0.5,
            "reset7d": 2_000_600_000,
        })
    }

    /// `now` just after the fixture's `now` (2_000_000_000_000 ms), which
    /// sits inside both windows above.
    const NOW_MS: i64 = 2_000_000_000_000;

    fn meters(snapshot: &serde_json::Value) -> Option<Meters<'_>> {
        Some(Meters::over(snapshot))
    }

    fn allowance(meter: &str, reset: i64) -> Allowance {
        Allowance {
            session_id: "ses-test".to_owned(),
            meter: meter.to_owned(),
            reset_value: reset,
            release: Release::Overage,
        }
    }

    fn plan_allowance(meter: &str, reset: i64) -> Allowance {
        Allowance {
            release: Release::Plan,
            ..allowance(meter, reset)
        }
    }

    /// Ahead of time: the 5-hour window though it is healthy, the 7-day
    /// one only when exhausted, and nothing for a window that has ended
    /// or a reading that names no reset.
    #[test]
    fn a_grant_ahead_opens_the_five_hour_window_early_but_not_the_week() {
        let healthy = json!({
            "util5h": 0.30, "reset5h": 2_000_000_600,
            "util7d": 0.40, "reset7d": 2_000_600_000,
        });
        assert_eq!(
            grant_ahead(meters(&healthy), NOW_MS),
            Grant {
                five_hour: Some(2_000_000_600),
                seven_day: None
            }
        );
        let weekly = json!({
            "util5h": 0.30, "reset5h": 2_000_000_600,
            "util7d": 0.995, "reset7d": 2_000_600_000,
        });
        assert_eq!(
            grant_ahead(meters(&weekly), NOW_MS).seven_day,
            Some(2_000_600_000)
        );
        let ended = json!({"util5h": 0.30, "reset5h": 1_999_999_000});
        assert_eq!(grant_ahead(meters(&ended), NOW_MS).five_hour, None);
        assert_eq!(
            grant_ahead(None, NOW_MS),
            Grant {
                five_hour: None,
                seven_day: None
            }
        );
    }

    /// A plan release forwards past the threshold while the plan has
    /// room, and blocks again once it is spent or overage is drawn; an
    /// overage release forwards through all of it.
    #[test]
    fn a_plan_release_stops_before_overage() {
        let reading = |util: f64, overage: bool| json!({"util5h": util, "reset5h": 2_000_000_600, "overageInUse": overage});
        let plan = [plan_allowance("5h", 2_000_000_600)];
        let overage = [allowance("5h", 2_000_000_600)];
        let block = GateDecision::Block {
            meter: Meter::FiveHour,
            resets_at: Some(2_000_000_600),
        };

        let room = reading(0.99, false);
        assert_eq!(decide(meters(&room), &plan, NOW_MS), GateDecision::Forward);
        assert!(plan_room(Meters::over(&room), Meter::FiveHour));

        for spent in [
            reading(1.0, false),
            reading(0.99, true),
            reading(0.40, true),
        ] {
            assert_eq!(decide(meters(&spent), &plan, NOW_MS), block, "{spent}");
            assert!(!plan_room(Meters::over(&spent), Meter::FiveHour));
            assert_eq!(
                decide(meters(&spent), &overage, NOW_MS),
                GateDecision::Forward,
                "the burn marker spends overage: {spent}"
            );
        }

        // A plan allowance for a rolled window is inert like any other.
        let rolled = json!({"util5h": 0.995, "reset5h": 2_000_018_600, "overageInUse": false});
        assert_eq!(
            decide(meters(&rolled), &plan, NOW_MS),
            GateDecision::Block {
                meter: Meter::FiveHour,
                resets_at: Some(2_000_018_600),
            }
        );

        // The 7-day meter: plan room is its own utilisation.
        let weekly = json!({
            "util5h": 0.20, "reset5h": 2_000_000_600,
            "util7d": 0.99, "reset7d": 2_000_600_000,
            "overageInUse": false,
        });
        assert_eq!(
            decide(
                meters(&weekly),
                &[plan_allowance("7d", 2_000_600_000)],
                NOW_MS
            ),
            GateDecision::Forward
        );
        // An unreadable utilisation is no room.
        let blind = json!({"reset5h": 2_000_000_600, "overageInUse": false});
        assert!(!plan_room(Meters::over(&blind), Meter::FiveHour));
    }

    // ── the threshold edge ─────────────────────────────────────────────

    #[test]
    fn the_threshold_edge_blocks_at_0_99_and_above_only() {
        assert_eq!(THRESHOLD, 0.99, "the measured threshold, verbatim");
        for util in [1.0, 0.995, 0.99] {
            let snapshot = json!({"util5h": util, "reset5h": 2_000_000_600});
            assert_eq!(
                decide(meters(&snapshot), &[], NOW_MS),
                GateDecision::Block {
                    meter: Meter::FiveHour,
                    resets_at: Some(2_000_000_600)
                },
                "util {util} is spent"
            );
        }
        let snapshot = json!({"util5h": 0.98, "reset5h": 2_000_000_600});
        assert_eq!(
            decide(meters(&snapshot), &[], NOW_MS),
            GateDecision::Forward,
            "0.98 is below the threshold — the window still has room"
        );
    }

    #[test]
    fn a_util_below_one_can_still_block_because_the_threshold_is_fractional() {
        // 0.99 of a window is spent even though 0.01 remains: the residual
        // is smaller than any single large request, so the gate treats the
        // window as gone (the predecessor's measured rationale for 0.99).
        let snapshot = json!({"util5h": 0.994, "reset5h": 2_000_000_600});
        assert!(matches!(
            decide(meters(&snapshot), &[], NOW_MS),
            GateDecision::Block { .. }
        ));
    }

    // ── expired: a spent reading stops counting once its window passed ──

    #[test]
    fn a_spent_reading_fails_open_once_its_own_window_has_passed() {
        // Both windows spent, both resets in the past: the reading says
        // nothing about the windows now running, so it must not block —
        // this is the rule that un-wedges the gate (a blocked request
        // never reaches upstream and can never refresh meters).
        let snapshot = json!({
            "util5h": 1.0, "reset5h": 1_999_999_000,
            "util7d": 1.0, "reset7d": 1_999_999_000,
        });
        assert_eq!(
            decide(meters(&snapshot), &[], NOW_MS),
            GateDecision::Forward,
            "an expired reading is not a reading of the current window"
        );

        // Expired at exactly the reset instant (`reset * 1000 <= now`).
        assert!(expired(Some(2_000_000), 2_000_000_000));
        // One epoch-second before the reset: still the old window.
        assert!(!expired(Some(2_000_000), 1_999_999_999));
        // Absent resets never expire (non-numbers), so a spent util
        // with no reset still blocks — naming its own ignorance.
        assert!(!expired(None, NOW_MS));
        let snapshot = json!({"util5h": 1.0});
        assert_eq!(
            decide(meters(&snapshot), &[], NOW_MS),
            GateDecision::Block {
                meter: Meter::FiveHour,
                resets_at: None
            },
            "no reset carried: block, with resets_at naming the absence"
        );
    }

    #[test]
    fn an_expired_window_does_not_mask_a_live_one() {
        // 5h expired, 7d live and spent: the 5h reading stops counting,
        // the 7d one does not.
        let snapshot = json!({
            "util5h": 1.0, "reset5h": 1_999_999_000,
            "util7d": 1.0, "reset7d": 2_000_600_000,
        });
        assert_eq!(
            decide(meters(&snapshot), &[], NOW_MS),
            GateDecision::Block {
                meter: Meter::SevenDay,
                resets_at: Some(2_000_600_000)
            }
        );
    }

    // ── the overageInUse rule ──────────────────────────────────────────

    #[test]
    fn overage_in_use_counts_as_the_5h_meter_unless_the_7d_is_the_cause() {
        // Overage drawn with a healthy 5h window and a spent 7d one: the
        // overage is the 7-day window's doing. Blaming 5h (observed
        // 2026-09-25) named the wrong meter and pinned the release to a
        // reset that lapsed every five hours while the weekly overage ran.
        let snapshot = json!({
            "util5h": 0.21, "reset5h": 2_000_000_600,
            "util7d": 1.0, "reset7d": 2_000_600_000,
            "overageInUse": true,
        });
        assert_eq!(
            decide(meters(&snapshot), &[], NOW_MS),
            GateDecision::Block {
                meter: Meter::SevenDay,
                resets_at: Some(2_000_600_000)
            },
            "the 7d meter is the cause of the overage"
        );

        // Overage drawn with both windows healthy: the spend has shifted
        // off plan quota, and that is the thing being prevented — the 5h
        // meter is named.
        let snapshot = json!({
            "util5h": 0.21, "reset5h": 2_000_000_600,
            "util7d": 0.5, "reset7d": 2_000_600_000,
            "overageInUse": true,
        });
        assert_eq!(
            decide(meters(&snapshot), &[], NOW_MS),
            GateDecision::Block {
                meter: Meter::FiveHour,
                resets_at: Some(2_000_000_600)
            }
        );

        // Anything but the literal `true` is false.
        let snapshot = json!({"util5h": 0.21, "reset5h": 2_000_000_600, "overageInUse": "TRUE"});
        assert_eq!(
            decide(meters(&snapshot), &[], NOW_MS),
            GateDecision::Forward
        );
    }

    // ── allowances: keyed by reset value ───────────────────────────────

    #[test]
    fn an_allowance_is_bound_to_the_reset_value_it_was_granted_against() {
        let snapshot = spent_5h();
        assert_eq!(
            decide(meters(&snapshot), &[], NOW_MS),
            GateDecision::Block {
                meter: Meter::FiveHour,
                resets_at: Some(2_000_000_600)
            },
            "no allowance: block"
        );

        // A release grants the reset VALUE of the exhausted window…
        let grant = grant_for(meters(&snapshot), NOW_MS);
        assert_eq!(
            grant,
            super::Grant {
                five_hour: Some(2_000_000_600),
                seven_day: None
            },
            "only the exhausted meter, keyed by its reset value"
        );

        // …and that value is what un-gates the window it names.
        assert_eq!(
            decide(meters(&snapshot), &[allowance("5h", 2_000_000_600)], NOW_MS),
            GateDecision::Forward,
            "held == current: released for this window"
        );

        // The window rolled: the reported reset changed, the old
        // allowance no longer matches, and the gate arms again — an
        // allowance cannot outlive its window, with no clock involved.
        let rolled = json!({
            "util5h": 0.995, "reset5h": 2_002_000_600,
            "util7d": 0.5, "reset7d": 2_000_600_000,
        });
        assert_eq!(
            decide(
                meters(&rolled),
                &[allowance("5h", 2_000_000_600)],
                2_002_000_000_000
            ),
            GateDecision::Block {
                meter: Meter::FiveHour,
                resets_at: Some(2_002_000_600)
            },
            "an allowance from a rolled window is inert"
        );

        // An allowance for the OTHER meter never releases this one.
        assert_eq!(
            decide(meters(&snapshot), &[allowance("7d", 2_000_600_000)], NOW_MS),
            GateDecision::Block {
                meter: Meter::FiveHour,
                resets_at: Some(2_000_000_600)
            },
            "the meters are independent"
        );
    }

    #[test]
    fn a_released_5h_window_does_not_release_a_spent_7d_one() {
        let both_spent = json!({
            "util5h": 1.0, "reset5h": 2_000_000_600,
            "util7d": 1.0, "reset7d": 2_000_600_000,
        });
        // decide walks the notice order: 5h released, 7d not → block on 7d.
        assert_eq!(
            decide(
                meters(&both_spent),
                &[allowance("5h", 2_000_000_600)],
                NOW_MS
            ),
            GateDecision::Block {
                meter: Meter::SevenDay,
                resets_at: Some(2_000_600_000)
            }
        );
        // Both released: forward.
        assert_eq!(
            decide(
                meters(&both_spent),
                &[
                    allowance("5h", 2_000_000_600),
                    allowance("7d", 2_000_600_000),
                ],
                NOW_MS
            ),
            GateDecision::Forward
        );
    }

    #[test]
    fn a_grant_covers_only_exhausted_meters_and_only_when_they_carry_a_reset() {
        // Healthy meters grant nothing — a release for the afternoon must
        // not quietly become a release for the week.
        let healthy = json!({
            "util5h": 0.2, "reset5h": 2_000_000_600,
            "util7d": 0.3, "reset7d": 2_000_600_000,
        });
        assert_eq!(
            grant_for(meters(&healthy), NOW_MS),
            super::Grant {
                five_hour: None,
                seven_day: None
            }
        );

        // An exhausted meter that carries no reset has no value to key an
        // allowance on: the grant is null (a missing reset grants nothing).
        let resetless = json!({"util5h": 1.0});
        assert_eq!(
            grant_for(meters(&resetless), NOW_MS),
            super::Grant {
                five_hour: None,
                seven_day: None
            }
        );

        // No meters at all (cold start): grant nothing.
        assert_eq!(
            grant_for(None, NOW_MS),
            super::Grant {
                five_hour: None,
                seven_day: None
            }
        );

        // Both windows spent: both granted.
        let both_spent = json!({
            "util5h": 1.0, "reset5h": 2_000_000_600,
            "util7d": 1.0, "reset7d": 2_000_600_000,
        });
        assert_eq!(
            grant_for(meters(&both_spent), NOW_MS),
            super::Grant {
                five_hour: Some(2_000_000_600),
                seven_day: Some(2_000_600_000)
            }
        );
    }

    // ── absence is not a limit (invariant 3) ───────────────────────────

    #[test]
    fn unknown_meters_forward() {
        assert_eq!(decide(None, &[], NOW_MS), GateDecision::Forward);
        assert!(exhausted_meters(None, NOW_MS).is_empty());

        // An empty snapshot (the header set parsed, nothing carried): no
        // window is known to be spent.
        let empty = json!({});
        assert_eq!(decide(meters(&empty), &[], NOW_MS), GateDecision::Forward);
        assert_eq!(Meters::over(&empty).util5h(), None);
        assert_eq!(Meters::over(&empty).reset5h(), None);
        assert!(!Meters::over(&empty).overage_in_use());
        assert_eq!(Meters::over(&empty).claim(), None);
    }

    #[test]
    fn the_typed_view_reads_the_stable_wire_shape() {
        let snapshot = json!({
            "util5h": 0.4127, "reset5h": 1_769_500_800,
            "util7d": 0.2214, "reset7d": 1_769_846_400,
            "overageInUse": true,
            "claim": "5h",
        });
        let view = Meters::over(&snapshot);
        assert_eq!(view.util5h(), Some(0.4127));
        assert_eq!(view.reset5h(), Some(1_769_500_800));
        assert_eq!(view.util7d(), Some(0.2214));
        assert_eq!(view.reset7d(), Some(1_769_846_400));
        assert!(view.overage_in_use());
        assert_eq!(view.claim(), Some("5h"));

        assert_eq!(Meter::parse("5h"), Some(Meter::FiveHour));
        assert_eq!(Meter::parse("7d"), Some(Meter::SevenDay));
        assert_eq!(Meter::parse("overage"), None);
        assert_eq!(Meter::FiveHour.as_str(), "5h");
        assert_eq!(Meter::SevenDay.notice_name(), "7-day");
    }

    // ── the notice: pure, and pinned ───────────────────────────────────

    fn utc() -> jiff::tz::TimeZone {
        // A fixed named zone keeps the pinned bytes independent of the
        // machine running the tests; UTC keeps them readable.
        jiff::tz::TimeZone::get("UTC").expect("UTC is always present in the tzdb")
    }

    #[test]
    fn the_notice_names_the_meter_the_reset_and_the_resume_path() {
        // Rendered Plain so these snapshots pin the notice's CONTENT —
        // the meter, the reset, the resume path — independent of any
        // wrapping style; the styles themselves are pinned below.
        let tz = utc();
        insta::assert_snapshot!(
            "plain_five_hour",
            Blocking::notice(
                Meter::FiveHour,
                Some(1_769_500_800),
                false,
                None,
                &tz,
                NoticeStyle::Plain
            )
        );
        // The context size rides in the same sentence, comma-grouped,
        // and is dropped entirely when unknown (never printed as zero).
        let with_context = Blocking::notice(
            Meter::SevenDay,
            Some(1_769_500_800),
            false,
            Some(123_456),
            &tz,
            NoticeStyle::Plain,
        );
        assert!(with_context.contains("123,456 tokens"), "{with_context}");
        insta::assert_snapshot!("plain_seven_day_with_context", with_context);
        // A zero context is not a measurement: dropped like an absent one.
        assert_eq!(
            Blocking::notice(
                Meter::FiveHour,
                Some(1_769_500_800),
                false,
                Some(0),
                &tz,
                NoticeStyle::Plain
            ),
            Blocking::notice(
                Meter::FiveHour,
                Some(1_769_500_800),
                false,
                None,
                &tz,
                NoticeStyle::Plain
            ),
            "a zero context is not a measurement: the known check is finite and > 0"
        );
        // At the gate with plan left: the over marker is offered, and
        // the quota is "almost spent", not spent.
        insta::assert_snapshot!(
            "plain_five_hour_plan_left",
            Blocking::notice(
                Meter::FiveHour,
                Some(1_769_500_800),
                true,
                None,
                &tz,
                NoticeStyle::Plain
            )
        );
        insta::assert_snapshot!(
            "plain_reset_unknown_plan_left",
            Blocking::notice(Meter::FiveHour, None, true, None, &tz, NoticeStyle::Plain)
        );
        // No reset carried: name the ignorance, in the frozen wording.
        insta::assert_snapshot!(
            "plain_reset_unknown",
            Blocking::notice(Meter::FiveHour, None, false, None, &tz, NoticeStyle::Plain)
        );
    }

    #[test]
    fn the_notice_is_a_pure_function_of_its_inputs() {
        // Invariant 4: a gate notice enters replayed history, so the same
        // inputs (style included) must render the same bytes, forever, on
        // every call. Pinned in the DEFAULT style, the generic GFM alert —
        // the wrapper the client actually carries is part of the pinned
        // bytes now.
        let tz = utc();
        let render = || {
            Blocking::notice(
                Meter::FiveHour,
                Some(1_769_500_800),
                false,
                Some(9_872_344),
                &tz,
                NoticeStyle::default(),
            )
        };
        let first = render();
        for _ in 0..3 {
            assert_eq!(render(), first);
        }
        insta::assert_snapshot!("default_style", first);
        // The timezone is an input: a different zone renders different
        // bytes for the same instant, deterministically — inside the
        // same frozen wrapper.
        let auckland = jiff::tz::TimeZone::get("Pacific/Auckland").expect("IANA zone");
        let in_auckland = Blocking::notice(
            Meter::FiveHour,
            Some(1_769_500_800),
            false,
            None,
            &auckland,
            NoticeStyle::default(),
        );
        assert!(
            in_auckland.contains("until 21:00."),
            "2026-01-27 08:00 UTC is 21:00 NZDT the same day: {in_auckland}"
        );
        insta::assert_snapshot!("default_style_auckland", in_auckland);
    }

    #[test]
    fn the_notice_renders_in_the_configured_style() {
        // One decision, every style: the CONTENT is identical, only the
        // wrapping differs. Plain is the content in brackets (its bytes
        // are pinned above); gfm is the generic default (the block is
        // claude's rendering).
        let tz = utc();
        let notice = |style| {
            Blocking::notice(
                Meter::FiveHour,
                Some(1_769_500_800),
                false,
                None,
                &tz,
                style,
            )
        };
        let plain = notice(NoticeStyle::Plain);
        let content = plain
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .expect("plain: the content in brackets, its only frame");
        let block = notice(NoticeStyle::Block);
        assert_eq!(
            block,
            format!("{BLOCK_HEADER}\n{content}\n{BLOCK_FOOTER}"),
            "block: the frozen block around the same content"
        );
        let toker = notice(NoticeStyle::Toker);
        assert_eq!(
            toker,
            format!("> [!TOKER]\n> {}", content.replace('\n', "\n> ")),
            "toker: Workhorse's alert"
        );
        let gfm = notice(NoticeStyle::Gfm);
        assert_eq!(
            gfm,
            format!("> [!CAUTION]\n> {}", content.replace('\n', "\n> ")),
            "gfm: the alert form, at the quota block's level"
        );
        insta::assert_snapshot!("style_block", block);
        insta::assert_snapshot!("style_toker", toker);
        insta::assert_snapshot!("style_gfm", gfm);
    }

    #[test]
    fn no_notice_carries_an_em_dash() {
        // The notices hold no em dashes: every rendering the gate can
        // write, in every style, is held to that outside any snapshot.
        let tz = utc();
        for meter in [Meter::FiveHour, Meter::SevenDay] {
            for resets_at in [Some(1_769_500_800), None] {
                for context in [None, Some(0), Some(123_456)] {
                    for style in [
                        NoticeStyle::Plain,
                        NoticeStyle::Gfm,
                        NoticeStyle::Toker,
                        NoticeStyle::Block,
                    ] {
                        for plan_left in [false, true] {
                            let notice =
                                Blocking::notice(meter, resets_at, plan_left, context, &tz, style);
                            assert!(!notice.contains('\u{2014}'), "{notice}");
                        }
                    }
                }
            }
        }
    }

    // ── the synthetic turn: byte-pinned, both renderings ────────────────

    const PINNED_NOTICE: &str = "[Stopped: quota is spent.]";

    #[test]
    fn the_sse_turn_is_byte_pinned() {
        // The exact event shape: a client renders it as a normal
        // assistant message.
        let bytes = Blocking::sse_turn(PINNED_NOTICE, Some("claude-sonnet-5"));
        let text = std::str::from_utf8(&bytes).expect("utf-8");
        // A snapshot ignores trailing whitespace: the closing blank line
        // that ends the last event is pinned here.
        assert!(text.ends_with("\n\n"), "{text:?}");
        insta::assert_snapshot!(text);

        // No model named: the "claude-opus-5" default.
        let bytes = Blocking::sse_turn("x", None);
        assert!(
            std::str::from_utf8(&bytes)
                .expect("utf-8")
                .contains("\"model\":\"claude-opus-5\"")
        );
        // The default also covers an EMPTY model.
        let bytes = Blocking::sse_turn("x", Some(""));
        assert!(
            std::str::from_utf8(&bytes)
                .expect("utf-8")
                .contains("\"model\":\"claude-opus-5\"")
        );
    }

    #[test]
    fn the_json_turn_is_byte_pinned() {
        // A client that asked for a plain JSON Message gets exactly one.
        let bytes = Blocking::json_turn(PINNED_NOTICE, Some("claude-sonnet-5"));
        insta::assert_snapshot!(std::str::from_utf8(&bytes).expect("utf-8"));
        let bytes = Blocking::json_turn("x", None);
        assert!(
            std::str::from_utf8(&bytes)
                .expect("utf-8")
                .contains("\"model\":\"claude-opus-5\"")
        );
    }

    #[test]
    fn the_dispatch_selects_the_rendering_and_purity_holds() {
        let sse = Blocking::blocked_turn("same text", Some("claude-opus-5"), Rendering::Sse);
        assert_eq!(sse, Blocking::sse_turn("same text", Some("claude-opus-5")));
        let json = Blocking::blocked_turn("same text", Some("claude-opus-5"), Rendering::Json);
        assert_eq!(
            json,
            Blocking::json_turn("same text", Some("claude-opus-5"))
        );
        assert_ne!(sse, json);

        // The turn is pure too: identical inputs, identical bytes, any
        // number of calls (invariant 4 — the turn enters the client's
        // transcript, which replays it on every later request).
        for _ in 0..3 {
            assert_eq!(
                Blocking::blocked_turn("same text", Some("claude-opus-5"), Rendering::Sse),
                sse.clone()
            );
        }
    }

    #[test]
    fn the_rendered_notice_rides_in_both_turn_renderings() {
        // The wiring the server does — notice(style) → blocked_turn —
        // carries the RENDERED notice, block and all: the toker block
        // is part of both the SSE and the JSON body bytes (invariant 4).
        // JSON escapes the block's newlines as `\n`; everything else
        // (the star, the dashes) rides the text field raw.
        let tz = utc();
        let rendered = Blocking::notice(
            Meter::FiveHour,
            Some(1_769_500_800),
            false,
            None,
            &tz,
            NoticeStyle::Block,
        );
        let sse_bytes = Blocking::sse_turn(&rendered, Some("claude-opus-5"));
        let json_bytes = Blocking::json_turn(&rendered, Some("claude-opus-5"));
        let sse = std::str::from_utf8(&sse_bytes).expect("utf-8");
        let json = std::str::from_utf8(&json_bytes).expect("utf-8");
        for body in [&sse, &json] {
            assert!(
                body.contains(&rendered.replace('\n', "\\n")),
                "the block rides the turn body, its newlines JSON-escaped"
            );
            assert!(
                body.contains(BLOCK_HEADER),
                "the frozen header is in the body"
            );
            assert!(
                body.contains(BLOCK_FOOTER),
                "the frozen footer is in the body"
            );
        }
    }

    #[test]
    fn counts_group_with_commas_and_never_move_with_locale() {
        assert_eq!(super::group(0), "0");
        assert_eq!(super::group(999), "999");
        assert_eq!(super::group(1_000), "1,000");
        assert_eq!(super::group(123_456), "123,456");
        assert_eq!(super::group(123_456_789), "123,456,789");
    }
}
