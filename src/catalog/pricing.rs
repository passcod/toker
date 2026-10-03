//! API list prices, USD per million tokens — port of ctp's `pricing.mjs`
//! (verified table, fast mode, geo multiplier, normalisation).
//!
//! Cache-read is stored explicitly per model rather than derived as 0.1×
//! input: Fable 5.1 / Mythos 5.1 read at 0.025×, and deriving them would
//! silently overcharge those models 4×. Every rate the ledger needs is a
//! table entry, never arithmetic on another entry.
//!
//! Not ported from `pricing.mjs` (plan: the plan/overage layer died with
//! ctp): `PLAN_USD_PER_MONTH`, `PLAN_PRO_MULTIPLE`, `OVERAGE_RATE_MULTIPLIER`,
//! `ALT_PLAN`. Only per-token prices, the fast-mode table, the web-search
//! per-call price, and the US-geo multiplier survive into toker.

/// The date this table was last verified against the provider's pricing
/// page (ctp `PRICING_VERIFIED_ON`; the banner prints it, and re-verifying
/// is a human duty that moves this date).
pub const VERIFIED_ON: &str = "2026-09-03";

/// Server-side web search bills per call, not per token
/// (ctp `WEB_SEARCH_USD_PER_REQUEST`: $10 per 1,000 requests).
pub const WEB_SEARCH_USD_PER_REQUEST: f64 = 10.0 / 1000.0;

/// `inference_geo: "us"` bills 1.1× across every token category
/// (ctp `US_GEO_MULTIPLIER`).
pub const US_GEO_MULTIPLIER: f64 = 1.1;

/// One model's rates: USD per million tokens, per category. The constructor
/// shape mirrors ctp's `T(input, output, write5m, write1h, read)` helper.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    /// Base input tokens.
    pub input: f64,
    /// Output tokens (thinking included in the output count).
    pub output: f64,
    /// Cache writes at the 5-minute TTL.
    pub write_5m: f64,
    /// Cache writes at the 1-hour TTL.
    pub write_1h: f64,
    /// Cache reads — explicit per model, never a ratio of `input`.
    pub read: f64,
}

/// ctp's `T(...)`: the table reads in the same argument order.
const fn rates(input: f64, output: f64, write_5m: f64, write_1h: f64, read: f64) -> Rates {
    Rates {
        input,
        output,
        write_5m,
        write_1h,
        read,
    }
}

/// The hand-verified price table (ctp `PRICING`), keyed by normalised model
/// id (see [`normalise_model_id`]).
static PRICING: &[(&str, Rates)] = &[
    ("claude-fable-5-1", rates(10.0, 50.0, 12.5, 20.0, 0.25)),
    ("claude-mythos-5-1", rates(10.0, 50.0, 12.5, 20.0, 0.25)),
    ("claude-fable-5", rates(10.0, 50.0, 12.5, 20.0, 1.0)),
    ("claude-mythos-5", rates(10.0, 50.0, 12.5, 20.0, 1.0)),
    ("claude-opus-5", rates(5.0, 25.0, 6.25, 10.0, 0.5)),
    ("claude-opus-4-8", rates(5.0, 25.0, 6.25, 10.0, 0.5)),
    ("claude-opus-4-7", rates(5.0, 25.0, 6.25, 10.0, 0.5)),
    ("claude-opus-4-6", rates(5.0, 25.0, 6.25, 10.0, 0.5)),
    ("claude-opus-4-5", rates(5.0, 25.0, 6.25, 10.0, 0.5)),
    // $2/$10 is the standard price. The increase to $3/$15 announced for
    // 2026-09-01 was cancelled; the launch rate became permanent.
    ("claude-sonnet-5", rates(2.0, 10.0, 2.5, 4.0, 0.2)),
    ("claude-sonnet-4-6", rates(3.0, 15.0, 3.75, 6.0, 0.3)),
    ("claude-sonnet-4-5", rates(3.0, 15.0, 3.75, 6.0, 0.3)),
    ("claude-sonnet-4-0", rates(3.0, 15.0, 3.75, 6.0, 0.3)),
    ("claude-haiku-4-5", rates(1.0, 5.0, 1.25, 2.0, 0.1)),
    ("claude-haiku-3-5", rates(0.8, 4.0, 1.0, 1.6, 0.08)),
    ("claude-opus-4-1", rates(15.0, 75.0, 18.75, 30.0, 1.5)),
    ("claude-opus-4-0", rates(15.0, 75.0, 18.75, 30.0, 1.5)),
];

/// Fast mode (`/fast` in Claude Code) reprices the base rates; the cache
/// multipliers then apply on top of the fast base (ctp `FAST_PRICING`).
static FAST_PRICING: &[(&str, Rates)] = &[
    ("claude-opus-5", rates(10.0, 50.0, 12.5, 20.0, 1.0)),
    ("claude-opus-4-8", rates(10.0, 50.0, 12.5, 20.0, 1.0)),
];

/// One model's resolved pricing: the rates that apply (fast and/or geo
/// adjustments already folded in), the normalised id they belong to, and
/// whether fast mode actually repriced this model (ctp `ratesFor`'s return
/// shape).
#[derive(Debug, Clone, PartialEq)]
pub struct Pricing {
    /// The applicable rates, per Mtok, geo multiplier included.
    pub rates: Rates,
    /// The normalised model id the rates were found under.
    pub model: String,
    /// Whether the fast-mode table supplied the rates (a fast request on a
    /// model with no fast entry prices at base, `fast: false`).
    pub fast: bool,
}

/// The token buckets a cost is computed from. ctp `costOf` folds missing
/// metrics in as 0 — cost is an estimate, so an unreported bucket
/// contributes nothing rather than poisoning the total; the caller decides
/// what a `None` observation means (the presence map carries that verdict).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostBuckets {
    /// Base input tokens.
    pub input: u64,
    /// Cache-read tokens.
    pub cache_read: u64,
    /// Cache writes at the 5-minute TTL.
    pub cache_write_5m: u64,
    /// Cache writes at the 1-hour TTL.
    pub cache_write_1h: u64,
    /// Output tokens (thinking included).
    pub output: u64,
    /// Server-side web searches, billed per call.
    pub web_searches: u64,
}

impl Pricing {
    /// Cost in USD for one request's token buckets (ctp `costOf`):
    /// Σ bucket × rate / 1e6, plus web searches at the per-call price.
    pub fn cost_usd(&self, buckets: &CostBuckets) -> f64 {
        let rates = &self.rates;
        let per_token = buckets.input as f64 * rates.input
            + buckets.cache_read as f64 * rates.read
            + buckets.cache_write_5m as f64 * rates.write_5m
            + buckets.cache_write_1h as f64 * rates.write_1h
            + buckets.output as f64 * rates.output;
        per_token / 1e6 + buckets.web_searches as f64 * WEB_SEARCH_USD_PER_REQUEST
    }
}

/// Strip client-only bracket variants and snapshot-date suffixes from a wire
/// model id (ctp `normaliseModel`): `claude-opus-5[1m]` and
/// `claude-haiku-4-5-20251001` both fold to their price-table identities.
///
/// `None` for an id with nothing left — the caller records no price, never a
/// guess.
pub fn normalise_model_id(model: &str) -> Option<String> {
    let lower = strip_brackets(&model.to_ascii_lowercase())
        .trim()
        .to_owned();
    if lower.is_empty() {
        return None;
    }
    let id = strip_snapshot_date(&lower).trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// The pricing lookup (ctp `ratesFor`). `fast` selects the fast-mode table
/// when the model has a fast entry (otherwise base rates price the request);
/// `geo == Some("us")` multiplies every rate by [`US_GEO_MULTIPLIER`].
///
/// `None` when the model is not in the table — the caller records the
/// tokens with a NULL cost and warns once, rather than inventing a number.
pub fn price(model: &str, fast: bool, geo: Option<&str>) -> Option<Pricing> {
    let id = normalise_model_id(model)?;
    let fast_rates = fast.then(|| lookup(FAST_PRICING, &id)).flatten();
    let base = fast_rates.or_else(|| lookup(PRICING, &id))?;

    let rates = if geo == Some("us") {
        Rates {
            input: base.input * US_GEO_MULTIPLIER,
            output: base.output * US_GEO_MULTIPLIER,
            write_5m: base.write_5m * US_GEO_MULTIPLIER,
            write_1h: base.write_1h * US_GEO_MULTIPLIER,
            read: base.read * US_GEO_MULTIPLIER,
        }
    } else {
        base
    };

    Some(Pricing {
        rates,
        model: id,
        fast: fast_rates.is_some(),
    })
}

fn lookup(table: &[(&str, Rates)], id: &str) -> Option<Rates> {
    table
        .iter()
        .find(|(key, _)| *key == id)
        .map(|(_, rates)| *rates)
}

/// Remove every complete `[...]` group (ctp's `/\[[^\]]*\]/g`); an
/// unterminated `[` is data, not a group, and stays.
fn strip_brackets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        match after.find(']') {
            Some(close) => {
                out.push_str(&rest[..open]);
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[..open + 1]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Drop a trailing snapshot date, `-YYYYMMDD` (ctp's `/-\d{8}$/`); a longer
/// or shorter digit tail is not a snapshot date and stays.
fn strip_snapshot_date(text: &str) -> &str {
    let bytes = text.as_bytes();
    let len = bytes.len();
    if len >= 9 && bytes[len - 9] == b'-' && bytes[len - 8..].iter().all(u8::is_ascii_digit) {
        // The tail is pure ASCII ('-' + 8 digits), so the boundary is safe.
        &text[..len - 9]
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CostBuckets, Pricing, Rates, US_GEO_MULTIPLIER, VERIFIED_ON, WEB_SEARCH_USD_PER_REQUEST,
        normalise_model_id, price, rates,
    };
    use serde_json::json;

    #[test]
    fn verified_on_is_pinned_to_ctps_date() {
        // The freshness discipline (ctp PRICING_VERIFIED_ON): the date must
        // exist and be date-shaped; re-verification moves it deliberately.
        assert_eq!(VERIFIED_ON, "2026-09-03");
        assert!(
            VERIFIED_ON.len() == 10 && VERIFIED_ON.as_bytes()[4] == b'-',
            "YYYY-MM-DD shaped"
        );
    }

    #[test]
    fn per_call_and_geo_constants_match_ctp() {
        assert_eq!(WEB_SEARCH_USD_PER_REQUEST, 10.0 / 1000.0);
        assert_eq!(US_GEO_MULTIPLIER, 1.1);
    }

    #[test]
    fn pinned_rates_come_from_the_ctp_table() {
        let cases = [
            // Cache-read stored explicitly: Fable/Mythos 5.1 read at 0.025x.
            ("claude-fable-5-1", rates(10.0, 50.0, 12.5, 20.0, 0.25)),
            ("claude-mythos-5-1", rates(10.0, 50.0, 12.5, 20.0, 0.25)),
            ("claude-mythos-5", rates(10.0, 50.0, 12.5, 20.0, 1.0)),
            ("claude-opus-5", rates(5.0, 25.0, 6.25, 10.0, 0.5)),
            ("claude-opus-4-1", rates(15.0, 75.0, 18.75, 30.0, 1.5)),
            ("claude-sonnet-5", rates(2.0, 10.0, 2.5, 4.0, 0.2)),
            ("claude-sonnet-4-6", rates(3.0, 15.0, 3.75, 6.0, 0.3)),
            ("claude-haiku-4-5", rates(1.0, 5.0, 1.25, 2.0, 0.1)),
            ("claude-haiku-3-5", rates(0.8, 4.0, 1.0, 1.6, 0.08)),
        ];
        for (model, expected) in cases {
            let priced = price(model, false, None).expect("in the table");
            assert_eq!(priced.rates, expected, "{model}");
            assert!(!priced.fast);
            assert_eq!(priced.model, model);
        }
    }

    #[test]
    fn unknown_models_price_to_none_never_a_guess() {
        // The caller records NULL cost and warns once (ctp's rule); a guess
        // here would defeat it.
        assert_eq!(price("claude-opus-6", false, None), None);
        assert_eq!(price("some-other-provider/model", false, None), None);
        assert_eq!(price("", false, None), None);
    }

    #[test]
    fn normalisation_strips_brackets_snapshots_and_case() {
        let cases = [
            ("claude-opus-5[1m]", "claude-opus-5"),
            ("[1m]claude-opus-5", "claude-opus-5"),
            ("claude-haiku-4-5-20251001", "claude-haiku-4-5"),
            ("claude-opus-4-1-20250805", "claude-opus-4-1"),
            (" Claude-Sonnet-5 ", "claude-sonnet-5"),
            ("Claude-Opus-5[1M]", "claude-opus-5"),
        ];
        for (raw, expected) in cases {
            assert_eq!(normalise_model_id(raw).as_deref(), Some(expected), "{raw}");
        }
        // Nothing left after normalisation reads as absent, not "".
        assert_eq!(normalise_model_id(""), None);
        assert_eq!(normalise_model_id("   "), None);
        assert_eq!(normalise_model_id("[1m]"), None);
        // A 9-digit tail is not a snapshot date (ctp's -\d{8}$ is anchored).
        assert_eq!(
            normalise_model_id("claude-opus-5-202510012").as_deref(),
            Some("claude-opus-5-202510012")
        );
        // An unterminated bracket is data, not a group.
        assert_eq!(
            normalise_model_id("claude-opus-5[1m").as_deref(),
            Some("claude-opus-5[1m")
        );
    }

    #[test]
    fn bracket_and_snapshot_normalisation_reaches_the_table() {
        let priced = price("Claude-Opus-5[1m]", false, None).expect("normalises to the table");
        assert_eq!(priced.model, "claude-opus-5");
        assert_eq!(priced.rates.input, 5.0);
        let priced =
            price("claude-haiku-4-5-20251001", false, None).expect("snapshot folds to the table");
        assert_eq!(priced.model, "claude-haiku-4-5");
        assert_eq!(priced.rates.write_1h, 2.0);
    }

    #[test]
    fn fast_mode_reprices_only_models_with_a_fast_entry() {
        // Opus 5 has a fast entry: the fast table prices it.
        let fast = price("claude-opus-5", true, None).expect("priced");
        assert!(fast.fast);
        assert_eq!(fast.rates, rates(10.0, 50.0, 12.5, 20.0, 1.0));

        let fast48 = price("claude-opus-4-8", true, None).expect("priced");
        assert!(fast48.fast);
        assert_eq!(fast48.rates, rates(10.0, 50.0, 12.5, 20.0, 1.0));

        // Sonnet 5 does not: a fast request prices at base, flagged as not
        // fast-repriced (ctp's Boolean(fast && FAST_PRICING[id])).
        let base = price("claude-sonnet-5", true, None).expect("priced");
        assert!(!base.fast);
        assert_eq!(base.rates, rates(2.0, 10.0, 2.5, 4.0, 0.2));

        // A model with no base entry stays None however it is asked.
        assert_eq!(price("claude-opus-6", true, Some("us")), None);
    }

    #[test]
    fn us_geo_multiplies_every_rate_by_1_1() {
        let priced = price("claude-opus-5", false, Some("us")).expect("priced");
        let base = rates(5.0, 25.0, 6.25, 10.0, 0.5);
        assert_eq!(priced.rates.input, base.input * US_GEO_MULTIPLIER);
        assert_eq!(priced.rates.output, base.output * US_GEO_MULTIPLIER);
        assert_eq!(priced.rates.write_5m, base.write_5m * US_GEO_MULTIPLIER);
        assert_eq!(priced.rates.write_1h, base.write_1h * US_GEO_MULTIPLIER);
        assert_eq!(priced.rates.read, base.read * US_GEO_MULTIPLIER);

        // Only the "us" edge prices up; other geo values pass through.
        let plain = price("claude-opus-5", false, Some("not_available")).expect("priced");
        assert_eq!(plain.rates, base);
        let ungeoed = price("claude-opus-5", false, None).expect("priced");
        assert_eq!(ungeoed.rates, base);

        // Fast and geo stack: the multiplier applies to the fast base.
        let fast_us = price("claude-opus-5", true, Some("us")).expect("priced");
        assert_eq!(fast_us.rates.read, 1.0 * US_GEO_MULTIPLIER);
    }

    #[test]
    fn cost_matches_ctps_cost_of() {
        // ctp's COLD probe scenario: 2 input, 82,420 cache-read at the 1h
        // tier, 13 output, Opus 5 list prices.
        let priced = price("claude-opus-5", false, None).expect("priced");
        let cost = priced.cost_usd(&CostBuckets {
            input: 2,
            cache_read: 82_420,
            cache_write_5m: 0,
            cache_write_1h: 0,
            output: 13,
            web_searches: 0,
        });
        let expected = (2.0 * 5.0 + 82_420.0 * 0.5 + 13.0 * 25.0) / 1e6;
        assert!((cost - expected).abs() < 1e-12, "{cost} vs {expected}");

        // Web searches bill per call on top.
        let with_search = priced.cost_usd(&CostBuckets {
            input: 0,
            cache_read: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            output: 0,
            web_searches: 2,
        });
        assert!((with_search - 0.02).abs() < 1e-12, "{with_search}");

        // A 1h-tier write costs what the table says, not a ratio.
        let cold_write = priced.cost_usd(&CostBuckets {
            input: 2,
            cache_read: 0,
            cache_write_5m: 0,
            cache_write_1h: 82_420,
            output: 13,
            web_searches: 0,
        });
        let expected = (2.0 * 5.0 + 82_420.0 * 10.0 + 13.0 * 25.0) / 1e6;
        assert!((cold_write - expected).abs() < 1e-12);
    }

    #[test]
    fn pricing_serialises_for_row_extra_fields() {
        // The row and TUI render the resolved rates; the struct is plain
        // data so JSON stays stable.
        let priced = price("claude-sonnet-5", false, Some("us")).expect("priced");
        let value = json!({
            "model": priced.model,
            "fast": priced.fast,
            "rates": {
                "input": priced.rates.input,
                "output": priced.rates.output,
                "write_5m": priced.rates.write_5m,
                "write_1h": priced.rates.write_1h,
                "read": priced.rates.read,
            },
        });
        assert_eq!(value["model"], "claude-sonnet-5");
        assert_eq!(value["fast"], false);
    }

    #[test]
    fn pricing_is_plain_data_for_the_row_writer() {
        // The server unit will move a Pricing into row fields; Copy rates
        // and a plain struct keep that lossless.
        let priced: Pricing = price("claude-opus-5", false, None).expect("priced");
        let Rates {
            input,
            output,
            write_5m,
            write_1h,
            read,
        } = priced.rates;
        assert_eq!(
            (input, output, write_5m, write_1h, read),
            (5.0, 25.0, 6.25, 10.0, 0.5)
        );
    }
}
