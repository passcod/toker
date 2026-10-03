//! The `x-codex-*` usage-limit headers, parsed into the quota snapshot
//! the gate reads (codex-rs `rate_limits.rs`, the CLI's own parser).
//!
//! The default family's headers, on every turn's response:
//!
//! - `x-codex-primary-used-percent` (0–100) with
//!   `x-codex-primary-window-minutes` and `x-codex-primary-reset-at`
//!   (unix seconds),
//! - the same three with `-secondary-`,
//! - `x-codex-limit-name`,
//! - `x-codex-credits-has-credits`, `x-codex-credits-unlimited`,
//!   `x-codex-credits-balance`,
//! - `x-codex-rate-limit-reached-type`.
//!
//! Any **other** `x-<id>-primary-used-percent` is another limit family
//! (the codex backend meters per-model limits this way) — each family
//! is kept under its id in `other`, never dropped, the same
//! known-keys-plus-catch-all split the anthropic meters port (the ctp
//! lesson: the unknown headers turned out to be the signals that
//! matter).
//!
//! The shape, stable for the ledger like the anthropic meter's:
//!
//! ```json
//! {
//!   "primary": {"used_percent": 12.5, "window_minutes": 300,
//!               "resets_at": 1769500800},
//!   "secondary": null,
//!   "limit_name": "gpt-5.2-codex",
//!   "credits": {"has_credits": true, "unlimited": false, "balance": "12.00"},
//!   "rate_limit_reached_type": null,
//!   "other": {"codex_bengalfox": {"primary": null, "secondary": null,
//!                                 "limit_name": null}}
//! }
//! ```
//!
//! Every named key is always present in a snapshot, `null` when the
//! response did not carry it (absence is a fact — invariant 3). The
//! numbers keep the header's literal precision (parsed as
//! [`serde_json::Number`], like the anthropic meters' `num`).
//!
//! Deviations from the codex CLI's parser, both deliberate:
//!
//! - `rate-limit-reached-type` is kept as the raw trimmed string, not
//!   validated against the CLI's enum — an unknown value is data, not
//!   an error (the keep-unknowns rule).
//! - A family's snapshot holds only that family's own headers (its
//!   windows and limit name); the CLI's parser folds the *global*
//!   credits headers into every family, which would mirror one object
//!   into every family's entry.
//!
//! `None` when the response carries no meter data at all: the gate's
//! snapshot must never be overwritten by a response that reports no
//! quota (the meter-source rule, [crate::providers::Provider::meters]).

use std::collections::BTreeSet;

use axum::http::HeaderMap;
use serde_json::{Map, Value, json};

/// The known-family prefix: `x-codex-…`.
const DEFAULT_PREFIX: &str = "x-codex";

/// The suffix that names a family's primary window.
const PRIMARY_USED_SUFFIX: &str = "-primary-used-percent";

/// Parse the codex usage-limit snapshot from one response's headers.
/// `None` when the response carries no `x-codex-*` meter data (see the
/// module docs for the shape and the window/credit rules).
pub fn parse_usage_limits(headers: &HeaderMap) -> Option<Value> {
    let primary = window(headers, "primary");
    let secondary = window(headers, "secondary");
    let limit_name = text(headers, "x-codex-limit-name");
    let credits = credits(headers);
    let reached = text(headers, "x-codex-rate-limit-reached-type");

    // Additional families: any other x-<id>-primary-used-percent header.
    let mut other = Map::new();
    for id in family_ids(headers) {
        // The headers spell the family with dashes; the snapshot keys it
        // the codex CLI's way (underscores).
        let prefix = format!("x-{id}");
        let family = json!({
            "primary": window_prefixed(headers, &prefix, "primary"),
            "secondary": window_prefixed(headers, &prefix, "secondary"),
            "limit_name": text(headers, &format!("{prefix}-limit-name")),
        });
        let has_data = !family["primary"].is_null()
            || !family["secondary"].is_null()
            || !family["limit_name"].is_null();
        if has_data {
            other.insert(normalize_limit_id(&id), family);
        }
    }

    if primary.is_none()
        && secondary.is_none()
        && limit_name.is_none()
        && credits.is_none()
        && reached.is_none()
        && other.is_empty()
    {
        return None;
    }
    Some(json!({
        "primary": primary,
        "secondary": secondary,
        "limit_name": limit_name,
        "credits": credits,
        "rate_limit_reached_type": reached,
        "other": Value::Object(other),
    }))
}

/// One window of the default family: `{used-percent, window-minutes,
/// reset-at}` (codex-rs `parse_rate_limit_window` — a window exists
/// only when its used-percent parses, and only when it shows data:
/// non-zero used, a non-zero window length, or a reset).
fn window(headers: &HeaderMap, which: &str) -> Option<Value> {
    window_prefixed(headers, DEFAULT_PREFIX, which)
}

/// One window of an arbitrary family prefix (`x-codex`, `x-codex-bengalfox`).
fn window_prefixed(headers: &HeaderMap, prefix: &str, which: &str) -> Option<Value> {
    let used = number(headers, &format!("{prefix}-{which}-used-percent"))?;
    let minutes = number(headers, &format!("{prefix}-{which}-window-minutes"));
    let resets_at = number(headers, &format!("{prefix}-{which}-reset-at"));
    let has_data = used.as_f64() != Some(0.0)
        || minutes
            .as_ref()
            .and_then(|minutes| minutes.as_i64())
            .is_some_and(|minutes| minutes != 0)
        || resets_at.is_some();
    has_data.then(|| {
        json!({
            "used_percent": used,
            "window_minutes": minutes,
            "resets_at": resets_at,
        })
    })
}

/// The credits object: only when BOTH booleans parse (codex-rs
/// `parse_credits_snapshot`); the balance is the trimmed non-empty
/// string when carried.
fn credits(headers: &HeaderMap) -> Option<Value> {
    let has_credits = boolean(headers, "x-codex-credits-has-credits")?;
    let unlimited = boolean(headers, "x-codex-credits-unlimited")?;
    let balance = text(headers, "x-codex-credits-balance");
    Some(json!({
        "has_credits": has_credits,
        "unlimited": unlimited,
        "balance": balance,
    }))
}

/// The dashed ids of every additional limit family: each header shaped
/// `x-<id>-primary-used-percent` with `<id> != "codex"` (the id is
/// normalised only when it becomes the snapshot key — the headers
/// themselves spell it with dashes).
fn family_ids(headers: &HeaderMap) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for name in headers.keys() {
        let Some(id) = name
            .as_str()
            .strip_suffix(PRIMARY_USED_SUFFIX)
            .and_then(|prefix| prefix.strip_prefix("x-"))
            .filter(|id| *id != "codex")
        else {
            continue;
        };
        ids.insert(id.to_owned());
    }
    ids.into_iter().collect()
}

/// The codex CLI's limit-id normalisation.
fn normalize_limit_id(id: &str) -> String {
    id.trim().to_ascii_lowercase().replace('-', "_")
}

/// One header's value parsed as a JSON number, its literal preserved
/// (the anthropic meters' `num`), finite only.
fn number(headers: &HeaderMap, name: &str) -> Option<Value> {
    let text = headers.get(name)?.to_str().ok()?;
    let number = serde_json::from_str::<serde_json::Number>(text.trim()).ok()?;
    // The CLI filters non-finite floats; NaN/Infinity are not meter data.
    number
        .as_f64()
        .is_some_and(f64::is_finite)
        .then_some(Value::Number(number))
}

/// One header's value as a bool: exactly the codex CLI's spelling —
/// `true`/`1` (case-insensitive) or `false`/`0`, else no parse.
fn boolean(headers: &HeaderMap, name: &str) -> Option<bool> {
    let text = headers.get(name)?.to_str().ok()?;
    if text.eq_ignore_ascii_case("true") || text == "1" {
        Some(true)
    } else if text.eq_ignore_ascii_case("false") || text == "0" {
        Some(false)
    } else {
        None
    }
}

/// One header's value as a trimmed non-empty string.
fn text(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::parse_usage_limits;
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    use serde_json::json;

    fn metered(headers: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                name.parse::<HeaderName>().expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        map
    }

    #[test]
    fn a_full_meter_set_parses_to_the_stable_shape() {
        let headers = metered(&[
            ("x-codex-primary-used-percent", "12.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1769500800"),
            ("x-codex-secondary-used-percent", "41.87"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-at", "1769846400"),
            ("x-codex-limit-name", "gpt-5.2-codex"),
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-unlimited", "false"),
            ("x-codex-credits-balance", "12.00"),
            ("x-codex-rate-limit-reached-type", "rate_limit_reached"),
        ]);
        assert_eq!(
            parse_usage_limits(&headers),
            Some(json!({
                "primary": {"used_percent": 12.5, "window_minutes": 300,
                            "resets_at": 1769500800},
                "secondary": {"used_percent": 41.87, "window_minutes": 10080,
                              "resets_at": 1769846400},
                "limit_name": "gpt-5.2-codex",
                "credits": {"has_credits": true, "unlimited": false,
                            "balance": "12.00"},
                "rate_limit_reached_type": "rate_limit_reached",
                "other": {},
            })),
            "epoch resets stay integers and utilisations keep their precision"
        );
    }

    #[test]
    fn absent_fields_read_null_and_absent_headers_read_none() {
        // One lone used-percent: a snapshot with every named key
        // present, null where the response did not carry it.
        let headers = metered(&[("x-codex-primary-used-percent", "80")]);
        assert_eq!(
            parse_usage_limits(&headers),
            Some(json!({
                "primary": {"used_percent": 80, "window_minutes": null,
                            "resets_at": null},
                "secondary": null,
                "limit_name": null,
                "credits": null,
                "rate_limit_reached_type": null,
                "other": {},
            }))
        );

        assert_eq!(parse_usage_limits(&HeaderMap::new()), None);
        let unrelated = metered(&[("retry-after", "7"), ("x-codex-promo-message", "hi")]);
        assert_eq!(
            parse_usage_limits(&unrelated),
            None,
            "non-meter x-codex-* headers alone produce no snapshot"
        );
    }

    #[test]
    fn a_window_needs_a_parseable_used_percent_and_some_data() {
        // used-percent present but all zeros with no reset: no window
        // (the codex CLI's has-data rule) — and with nothing else on
        // the response, no snapshot at all.
        let headers = metered(&[
            ("x-codex-primary-used-percent", "0"),
            ("x-codex-primary-window-minutes", "0"),
        ]);
        assert_eq!(parse_usage_limits(&headers), None);

        // A zero used-percent WITH a reset is real data.
        let headers = metered(&[
            ("x-codex-primary-used-percent", "0"),
            ("x-codex-primary-reset-at", "1769500800"),
        ]);
        let parsed = parse_usage_limits(&headers).expect("a reset is data");
        assert_eq!(parsed["primary"]["resets_at"], json!(1769500800));

        // used-percent missing entirely: window-minutes/reset alone do
        // not make a window — and with nothing else on the response,
        // no meter data at all (the codex CLI's family filter does the
        // same: a family with no window reports nothing).
        let headers = metered(&[
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1769500800"),
        ]);
        assert_eq!(
            parse_usage_limits(&headers),
            None,
            "no used-percent → no window, and orphaned siblings are not \
             meter data on their own"
        );

        // Non-numeric used-percent: the window is unparsed (never
        // zero), and — like the codex CLI's family filter — a response
        // whose only meter header cannot parse reports nothing.
        let headers = metered(&[("x-codex-primary-used-percent", "pretty high")]);
        assert_eq!(
            parse_usage_limits(&headers),
            None,
            "an unparseable used-percent is no meter data, never zero"
        );
        let headers = metered(&[("x-codex-primary-used-percent", "NaN")]);
        assert_eq!(parse_usage_limits(&headers), None, "NaN parses no number");
    }

    #[test]
    fn credits_need_both_flags_and_the_exact_bool_spellings() {
        for (has, unlimited, expected) in [
            (
                "true",
                "false",
                Some(json!({"has_credits": true, "unlimited": false, "balance": null})),
            ),
            (
                "1",
                "0",
                Some(json!({"has_credits": true, "unlimited": false, "balance": null})),
            ),
            // "TRUE" is a case-insensitive true, exactly like the CLI's.
            (
                "false",
                "TRUE",
                Some(json!({"has_credits": false, "unlimited": true, "balance": null})),
            ),
            // An unparseable flag means no credits object at all.
            ("yes", "false", None),
            ("true", "maybe", None),
        ] {
            let headers = metered(&[
                ("x-codex-credits-has-credits", has),
                ("x-codex-credits-unlimited", unlimited),
            ]);
            assert_eq!(
                parse_usage_limits(&headers).map(|v| v["credits"].clone()),
                expected,
                "has-credits={has:?} unlimited={unlimited:?}"
            );
        }

        // The balance is trimmed, and blank does not count.
        let headers = metered(&[
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-unlimited", "true"),
            ("x-codex-credits-balance", "  12.50  "),
        ]);
        let parsed = parse_usage_limits(&headers).expect("credits parse");
        assert_eq!(parsed["credits"]["balance"], json!("12.50"));
        let headers = metered(&[
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-unlimited", "true"),
            ("x-codex-credits-balance", "   "),
        ]);
        let parsed = parse_usage_limits(&headers).expect("credits parse");
        assert_eq!(parsed["credits"]["balance"], serde_json::Value::Null);
    }

    #[test]
    fn additional_limit_families_are_kept_under_their_normalised_id() {
        let headers = metered(&[
            // The default family.
            ("x-codex-primary-used-percent", "5"),
            // Another family: windows of its own, name of its own.
            ("x-codex-bengalfox-primary-used-percent", "80"),
            ("x-codex-bengalfox-primary-window-minutes", "1440"),
            ("x-codex-bengalfox-limit-name", "gpt-5.2-codex-sonic"),
            // A family header with no data behind it: not kept.
            ("x-codex-empty-primary-used-percent", "0"),
            // The default family's secondary window is NOT a family
            // (x-codex-secondary-used-percent, not
            // x-codex-secondary-primary-used-percent).
            ("x-codex-secondary-used-percent", "9"),
        ]);
        let parsed = parse_usage_limits(&headers).expect("snapshot");
        assert_eq!(
            parsed["other"],
            json!({
                "codex_bengalfox": {
                    "primary": {"used_percent": 80, "window_minutes": 1440,
                                "resets_at": null},
                    "secondary": null,
                    "limit_name": "gpt-5.2-codex-sonic",
                },
            }),
            "the family is keyed by its normalised id; the empty one is \
             dropped; the default family's secondary is not a family"
        );
        assert_eq!(
            parsed["secondary"],
            json!({"used_percent": 9, "window_minutes": null, "resets_at": null}),
            "the default family's own secondary window still parses"
        );
    }

    #[test]
    fn the_reached_type_keeps_unknown_values_verbatim() {
        // The codex CLI validates this against an enum and drops the
        // unknown; toker keeps them — a new value is data, not noise.
        let headers = metered(&[(
            "x-codex-rate-limit-reached-type",
            "workspace_member_credits_depleted",
        )]);
        let parsed = parse_usage_limits(&headers).expect("snapshot");
        assert_eq!(
            parsed["rate_limit_reached_type"],
            json!("workspace_member_credits_depleted")
        );
    }
}
