//! Console quota events: the service's own log line when a meter-source
//! backend's quota moves in a way an operator would want to hear about
//! without opening the TUI (`journalctl --user -u toker`). A port of ctp's
//! `noteQuota`, which wrote the same three events to its stderr:
//!
//! - the short window's utilisation crossing 80%, 90% and 100%
//!   ([`UTIL_THRESHOLDS`]), each announced once per window;
//! - the binding claim changing (`five_hour` → `seven_day`, with the
//!   overage utilisation beside it);
//! - any per-window status reading something other than `allowed`,
//!   announced once per distinct set.
//!
//! The status values other than `allowed` are deliberately not
//! enumerated: ctp wrote this when it had only ever seen `allowed`, and
//! the ledger has since recorded `allowed_warning` and `rejected`, so
//! anything else is reported verbatim rather than matched against a list
//! that would miss the next one.
//!
//! The latches are in memory, one per backend id, so the anthropic sub's
//! window and the codex sub's never re-arm or silence each other. ctp
//! started every process with empty latches, so a restart at 85%
//! announced 80% again. toker seeds a backend's latch from the meter
//! snapshot the store kept from the last response before the restart
//! (read once, on the first reading after it, before that reading
//! overwrites the snapshot), so a restart re-announces nothing the
//! previous process already said about the same window. A stored
//! snapshot whose short window has ended seeds only the claim: its
//! crossings and statuses described a window that is gone. A snapshot
//! the store cannot read seeds nothing, which is ctp's behaviour.
//!
//! Only meter values, claim names and status strings are logged
//! (invariants 1 and 2). Nothing here may break a request (invariant 3):
//! the server calls [`QuotaEvents::note`] under `catch_unwind`, and the
//! latch lock recovers from poisoning.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use crate::middleware::quota::expired;

/// The utilisation fractions announced as the short window fills.
pub const UTIL_THRESHOLDS: [f64; 3] = [0.8, 0.9, 1.0];

/// Below this utilisation the window has certainly rolled, and the
/// threshold latch re-arms (ctp's rule, kept beside the reset-change
/// re-arm for readings that carry no reset).
pub const REARM_BELOW: f64 = 0.5;

/// One announcement.
#[derive(Debug, Clone, PartialEq)]
pub enum QuotaEvent {
    /// The short window's utilisation reached `threshold`.
    Crossed {
        /// Which window: `5h` for the anthropic sub, `primary` for codex.
        window: &'static str,
        /// The threshold crossed, a fraction.
        threshold: f64,
        /// The utilisation the crossing reading reported, a fraction.
        util: f64,
    },
    /// The binding claim moved.
    ClaimChanged {
        from: String,
        to: String,
        /// The overage utilisation the same reading reported, a fraction.
        overage_util: Option<f64>,
    },
    /// The set of non-`allowed` statuses changed to a non-empty one.
    Status {
        /// `window=status` pairs, comma-separated, in a fixed order.
        statuses: String,
    },
}

/// What one meter snapshot says, whatever its backend's shape.
#[derive(Debug, Default, PartialEq)]
struct Reading {
    window: &'static str,
    util: Option<f64>,
    /// The short window's reset, epoch seconds.
    reset_s: Option<i64>,
    claim: Option<String>,
    overage_util: Option<f64>,
    /// The non-`allowed` statuses as `window=status` pairs; `None` when
    /// the reading carried no status at all, which says nothing (a
    /// reading without status headers must not clear the latch and
    /// re-announce the same set on the next one that has them).
    flagged: Option<String>,
}

impl Reading {
    /// Read the anthropic sub's shape (`providers::parse_rate_limits`) or
    /// the codex sub's (`providers::codex::meters`); anything else reads
    /// as empty and announces nothing.
    fn of(snapshot: &Value) -> Reading {
        if snapshot.get("util5h").is_some() || snapshot.get("status").is_some() {
            Self::anthropic(snapshot)
        } else if snapshot.get("primary").is_some() {
            Self::codex(snapshot)
        } else {
            Reading::default()
        }
    }

    fn anthropic(snapshot: &Value) -> Reading {
        let text = |key: &str| snapshot.get(key).and_then(Value::as_str);
        let statuses = [
            ("5h", text("status5h")),
            ("7d", text("status7d")),
            ("overage", text("statusOverage")),
            ("overall", text("status")),
        ];
        let flagged = statuses
            .iter()
            .any(|(_, status)| status.is_some())
            .then(|| {
                statuses
                    .iter()
                    .filter_map(|(window, status)| {
                        status
                            .filter(|status| *status != "allowed")
                            .map(|status| format!("{window}={status}"))
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            });
        Reading {
            window: "5h",
            util: snapshot.get("util5h").and_then(Value::as_f64),
            reset_s: snapshot.get("reset5h").and_then(Value::as_i64),
            claim: text("claim").map(str::to_owned),
            overage_util: snapshot.get("utilOverage").and_then(Value::as_f64),
            flagged,
        }
    }

    /// The codex shape: the primary window is the short one, its
    /// `used_percent` on a 0-100 scale. Codex has no claim. Its one status
    /// is `rate_limit_reached_type`, which the upstream sends only when a
    /// limit is reached, so a reading that carries a window and no reached
    /// type is the codex spelling of "allowed".
    fn codex(snapshot: &Value) -> Reading {
        let primary = snapshot.get("primary").filter(|primary| !primary.is_null());
        let reached = snapshot
            .get("rate_limit_reached_type")
            .and_then(Value::as_str);
        let has_window = primary.is_some()
            || snapshot
                .get("secondary")
                .is_some_and(|secondary| !secondary.is_null());
        let flagged = match reached {
            Some(reached) => Some(format!("reached={reached}")),
            None => has_window.then(String::new),
        };
        Reading {
            window: "primary",
            util: primary
                .and_then(|primary| primary.get("used_percent"))
                .and_then(Value::as_f64)
                .map(|percent| percent / 100.0),
            reset_s: primary
                .and_then(|primary| primary.get("resets_at"))
                .and_then(Value::as_i64),
            claim: None,
            overage_util: None,
            flagged,
        }
    }
}

/// One backend's announced state.
#[derive(Debug, Default, Clone, PartialEq)]
struct Latch {
    /// The highest threshold announced in the current window; 0 when
    /// armed.
    crossed: f64,
    /// The short window's reset as last seen, so a roll re-arms even
    /// when no reading below [`REARM_BELOW`] arrived in between.
    reset_s: Option<i64>,
    /// The binding claim as last seen.
    claim: Option<String>,
    /// The non-`allowed` set last seen, `""` for none.
    flagged: String,
}

impl Latch {
    /// The state the previous process left, from its last stored
    /// snapshot (see the module docs).
    fn seeded(prior: &Reading, now_ms: i64) -> Latch {
        let claim = prior.claim.clone();
        // An absent reset never expires (the gate's rule): the seed then
        // trusts the reading, and the reset-change re-arm still applies.
        if expired(prior.reset_s, now_ms) {
            return Latch {
                claim,
                ..Latch::default()
            };
        }
        let crossed = prior.util.map_or(0.0, |util| {
            UTIL_THRESHOLDS
                .iter()
                .copied()
                .filter(|threshold| util >= *threshold)
                .fold(0.0, f64::max)
        });
        Latch {
            crossed,
            reset_s: prior.reset_s,
            claim,
            flagged: prior.flagged.clone().unwrap_or_default(),
        }
    }

    /// Fold one reading in and say what it announces.
    fn step(&mut self, reading: &Reading) -> Vec<QuotaEvent> {
        let mut events = Vec::new();

        // A new reset is a new window: re-arm before testing it.
        if let Some(reset) = reading.reset_s {
            if self.reset_s.is_some_and(|seen| seen != reset) {
                self.crossed = 0.0;
            }
            self.reset_s = Some(reset);
        }
        if let Some(util) = reading.util {
            for threshold in UTIL_THRESHOLDS {
                if util >= threshold && self.crossed < threshold {
                    self.crossed = threshold;
                    events.push(QuotaEvent::Crossed {
                        window: reading.window,
                        threshold,
                        util,
                    });
                }
            }
            if util < REARM_BELOW {
                self.crossed = 0.0;
            }
        }

        if let Some(to) = &reading.claim {
            if let Some(from) = &self.claim
                && from != to
            {
                events.push(QuotaEvent::ClaimChanged {
                    from: from.clone(),
                    to: to.clone(),
                    overage_util: reading.overage_util,
                });
            }
            self.claim = Some(to.clone());
        }

        if let Some(flagged) = &reading.flagged {
            if !flagged.is_empty() && *flagged != self.flagged {
                events.push(QuotaEvent::Status {
                    statuses: flagged.clone(),
                });
            }
            self.flagged = flagged.clone();
        }
        events
    }
}

/// The per-backend latches. One per server.
#[derive(Debug, Default)]
pub struct QuotaEvents {
    latches: Mutex<HashMap<String, Latch>>,
}

impl QuotaEvents {
    /// Fold one response's meter snapshot for `backend` into its latch
    /// and return what it announces. `prior` is asked once per backend,
    /// on its first reading, for the snapshot the store kept from before
    /// (so the caller must call this before saving the new snapshot).
    pub fn note(
        &self,
        backend: &str,
        snapshot: &Value,
        prior: impl FnOnce() -> Option<Value>,
        now_ms: i64,
    ) -> Vec<QuotaEvent> {
        let reading = Reading::of(snapshot);
        let mut latches = self
            .latches
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let latch = latches.entry(backend.to_owned()).or_insert_with(|| {
            prior().map_or_else(Latch::default, |prior| {
                Latch::seeded(&Reading::of(&prior), now_ms)
            })
        });
        latch.step(&reading)
    }
}

/// Whole percent, as ctp printed it.
fn pct(fraction: f64) -> i64 {
    (fraction * 100.0).round() as i64
}

/// Log one event: `info` on the way up, `warn` at the limit, on a claim
/// change, and on a non-`allowed` status.
pub fn emit(backend: &str, event: &QuotaEvent) {
    match event {
        QuotaEvent::Crossed {
            window,
            threshold,
            util,
        } => {
            let (util_pct, threshold_pct) = (pct(*util), pct(*threshold));
            if *threshold >= 1.0 {
                tracing::warn!(
                    backend,
                    window,
                    util_pct,
                    threshold_pct,
                    "quota: {window} window at {util_pct}%, limit reached"
                );
            } else {
                tracing::info!(
                    backend,
                    window,
                    util_pct,
                    threshold_pct,
                    "quota: {window} window at {util_pct}%"
                );
            }
        }
        QuotaEvent::ClaimChanged {
            from,
            to,
            overage_util,
        } => {
            let overage_pct = overage_util.map(pct);
            tracing::warn!(
                backend,
                from = from.as_str(),
                to = to.as_str(),
                overage_pct,
                "quota: binding limit changed: {from} -> {to}"
            );
        }
        QuotaEvent::Status { statuses } => {
            tracing::warn!(
                backend,
                statuses = statuses.as_str(),
                "quota: status {statuses}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};

    use super::{QuotaEvent, QuotaEvents, emit};

    const NOW_MS: i64 = 1_790_000_000_000;
    const RESET: i64 = NOW_MS / 1000 + 3600;

    fn anthropic(util: f64) -> Value {
        anthropic_with(util, RESET, "five_hour", "allowed")
    }

    fn anthropic_with(util: f64, reset: i64, claim: &str, status5h: &str) -> Value {
        json!({
            "util5h": util, "reset5h": reset, "util7d": 0.1, "reset7d": RESET + 86_400,
            "utilOverage": 0.0, "status": "allowed", "status5h": status5h,
            "status7d": "allowed", "statusOverage": "allowed", "claim": claim,
            "overageInUse": false, "fallbackPct": 0.5, "other": {},
        })
    }

    fn note(events: &QuotaEvents, snapshot: &Value) -> Vec<QuotaEvent> {
        events.note("anthropic_sub", snapshot, || None, NOW_MS)
    }

    fn crossed(events: &[QuotaEvent]) -> Vec<f64> {
        events
            .iter()
            .filter_map(|event| match event {
                QuotaEvent::Crossed { threshold, .. } => Some(*threshold),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn each_threshold_fires_once_per_window() {
        let events = QuotaEvents::default();
        assert!(note(&events, &anthropic(0.5)).is_empty());
        assert_eq!(crossed(&note(&events, &anthropic(0.81))), vec![0.8]);
        assert!(note(&events, &anthropic(0.85)).is_empty(), "latched");
        assert!(note(&events, &anthropic(0.8)).is_empty(), "latched");
        // A jump past two thresholds names both, as ctp did.
        assert_eq!(crossed(&note(&events, &anthropic(1.0))), vec![0.9, 1.0]);
        assert!(note(&events, &anthropic(1.0)).is_empty());
        // Falling back without leaving the window re-arms nothing.
        assert!(note(&events, &anthropic(0.6)).is_empty());
        assert!(note(&events, &anthropic(0.95)).is_empty());
    }

    #[test]
    fn the_latch_re_arms_below_half_or_on_a_new_reset() {
        let events = QuotaEvents::default();
        assert_eq!(crossed(&note(&events, &anthropic(0.92))), vec![0.8, 0.9]);
        assert!(note(&events, &anthropic(0.3)).is_empty());
        assert_eq!(crossed(&note(&events, &anthropic(0.8))), vec![0.8]);

        // The window rolled with no low reading in between (no traffic
        // across the reset): the new reset re-arms.
        let next = RESET + 5 * 3600;
        let rolled = anthropic_with(0.83, next, "five_hour", "allowed");
        assert_eq!(crossed(&note(&events, &rolled)), vec![0.8]);
        assert!(note(&events, &rolled).is_empty());
    }

    #[test]
    fn a_claim_change_is_announced_with_the_overage() {
        let events = QuotaEvents::default();
        // The first claim seen is not a change.
        assert!(note(&events, &anthropic(0.2)).is_empty());
        let mut moved = anthropic_with(0.2, RESET, "seven_day", "allowed");
        moved["utilOverage"] = json!(0.12);
        assert_eq!(
            note(&events, &moved),
            vec![QuotaEvent::ClaimChanged {
                from: "five_hour".to_owned(),
                to: "seven_day".to_owned(),
                overage_util: Some(0.12),
            }]
        );
        assert!(note(&events, &moved).is_empty());
        // A reading without a claim leaves the last one standing.
        let mut unclaimed = anthropic(0.2);
        unclaimed["claim"] = Value::Null;
        assert!(note(&events, &unclaimed).is_empty());
        assert!(note(&events, &moved).is_empty());
    }

    #[test]
    fn a_status_set_is_announced_once_per_distinct_set() {
        let events = QuotaEvents::default();
        assert!(note(&events, &anthropic(0.2)).is_empty(), "all allowed");
        let rejected = anthropic_with(0.99, RESET, "five_hour", "rejected");
        let mut both = rejected.clone();
        both["status"] = json!("rejected");
        let statuses = |snapshot: &Value| -> Vec<String> {
            note(&events, snapshot)
                .into_iter()
                .filter_map(|event| match event {
                    QuotaEvent::Status { statuses } => Some(statuses),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(statuses(&rejected), vec!["5h=rejected".to_owned()]);
        assert!(statuses(&rejected).is_empty());
        assert_eq!(
            statuses(&both),
            vec!["5h=rejected,overall=rejected".to_owned()]
        );
        // A reading with no status at all says nothing and keeps the set.
        let mut silent = both.clone();
        for key in ["status", "status5h", "status7d", "statusOverage"] {
            silent[key] = Value::Null;
        }
        assert!(statuses(&silent).is_empty());
        assert!(statuses(&both).is_empty());
        // Back to allowed clears it, so the next refusal speaks again.
        assert!(statuses(&anthropic(0.2)).is_empty());
        assert_eq!(statuses(&rejected), vec!["5h=rejected".to_owned()]);
    }

    #[test]
    fn nothing_is_announced_without_meter_values() {
        let events = QuotaEvents::default();
        for snapshot in [
            json!({}),
            json!({"util5h": null, "reset5h": null, "status": null, "claim": null}),
            json!({"primary": null, "secondary": null, "rate_limit_reached_type": null}),
            json!({"unrelated": 1}),
        ] {
            assert!(note(&events, &snapshot).is_empty(), "{snapshot}");
        }
        // And the empty readings armed nothing that a real one trips.
        assert_eq!(crossed(&note(&events, &anthropic(0.81))), vec![0.8]);
    }

    #[test]
    fn backends_keep_separate_latches() {
        let events = QuotaEvents::default();
        let codex = |percent: f64, reached: Value| {
            json!({
                "primary": {"used_percent": percent, "window_minutes": 300, "resets_at": RESET},
                "secondary": null, "limit_name": null, "credits": null,
                "rate_limit_reached_type": reached, "other": {},
            })
        };
        assert_eq!(crossed(&note(&events, &anthropic(0.85))), vec![0.8]);
        let codex_events = events.note("codex_sub", &codex(85.0, Value::Null), || None, NOW_MS);
        assert_eq!(
            codex_events,
            vec![QuotaEvent::Crossed {
                window: "primary",
                threshold: 0.8,
                util: 0.85,
            }]
        );
        assert_eq!(
            events.note(
                "codex_sub",
                &codex(100.0, json!("primary")),
                || None,
                NOW_MS
            ),
            vec![
                QuotaEvent::Crossed {
                    window: "primary",
                    threshold: 0.9,
                    util: 1.0,
                },
                QuotaEvent::Crossed {
                    window: "primary",
                    threshold: 1.0,
                    util: 1.0,
                },
                QuotaEvent::Status {
                    statuses: "reached=primary".to_owned(),
                },
            ]
        );
        assert!(note(&events, &anthropic(0.85)).is_empty());
    }

    #[test]
    fn a_restart_re_announces_nothing_the_stored_snapshot_already_said() {
        let rejected = anthropic_with(1.0, RESET, "seven_day", "rejected");
        let events = QuotaEvents::default();
        let mut asked = 0;
        let first = events.note(
            "anthropic_sub",
            &rejected,
            || {
                asked += 1;
                Some(rejected.clone())
            },
            NOW_MS,
        );
        assert!(first.is_empty(), "{first:?}");
        // The seed is read once; later readings never ask again.
        events.note(
            "anthropic_sub",
            &rejected,
            || {
                asked += 1;
                None
            },
            NOW_MS,
        );
        assert_eq!(asked, 1);

        // From 85%, a reading at 92% names only 90%.
        let events = QuotaEvents::default();
        let seed = anthropic(0.85);
        assert_eq!(
            crossed(&events.note("anthropic_sub", &anthropic(0.92), || Some(seed), NOW_MS)),
            vec![0.9]
        );

        // A stored claim makes the first reading's change visible.
        let events = QuotaEvents::default();
        let seed = anthropic(0.2);
        let moved = anthropic_with(0.2, RESET, "seven_day", "allowed");
        assert_eq!(
            events.note("anthropic_sub", &moved, || Some(seed), NOW_MS),
            vec![QuotaEvent::ClaimChanged {
                from: "five_hour".to_owned(),
                to: "seven_day".to_owned(),
                overage_util: Some(0.0),
            }]
        );
    }

    #[test]
    fn a_stored_snapshot_from_an_ended_window_seeds_only_the_claim() {
        let ended = anthropic_with(1.0, NOW_MS / 1000 - 60, "five_hour", "rejected");
        let events = QuotaEvents::default();
        let fresh = anthropic_with(0.85, RESET, "five_hour", "rejected");
        let announced = events.note("anthropic_sub", &fresh, || Some(ended), NOW_MS);
        assert_eq!(
            announced,
            vec![
                QuotaEvent::Crossed {
                    window: "5h",
                    threshold: 0.8,
                    util: 0.85,
                },
                QuotaEvent::Status {
                    statuses: "5h=rejected".to_owned(),
                },
            ]
        );
    }

    /// A writer the test subscriber logs into.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buffer").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn events_log_at_their_level_with_structured_fields() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for event in [
                QuotaEvent::Crossed {
                    window: "5h",
                    threshold: 0.8,
                    util: 0.814,
                },
                QuotaEvent::Crossed {
                    window: "5h",
                    threshold: 1.0,
                    util: 1.0,
                },
                QuotaEvent::ClaimChanged {
                    from: "five_hour".to_owned(),
                    to: "seven_day".to_owned(),
                    overage_util: Some(0.123),
                },
                QuotaEvent::Status {
                    statuses: "5h=rejected".to_owned(),
                },
            ] {
                emit("anthropic_sub", &event);
            }
        });
        let text = String::from_utf8(captured.0.lock().expect("buffer").clone()).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(lines[0].starts_with(" INFO"), "{}", lines[0]);
        assert!(
            lines[0].contains("quota: 5h window at 81%")
                && lines[0].contains("backend=\"anthropic_sub\"")
                && lines[0].contains("util_pct=81")
                && lines[0].contains("threshold_pct=80"),
            "{}",
            lines[0]
        );
        assert!(lines[1].starts_with(" WARN") && lines[1].contains("limit reached"));
        assert!(
            lines[2].starts_with(" WARN")
                && lines[2].contains("five_hour -> seven_day")
                && lines[2].contains("overage_pct=12"),
            "{}",
            lines[2]
        );
        assert!(lines[3].starts_with(" WARN") && lines[3].contains("statuses=\"5h=rejected\""));
    }
}
