//! Direct OpenAI API list prices used for estimated Chat costs.
//!
//! Verified against the official OpenAI API pricing and model pages on
//! 2026-10-10. Identities are exact. Unknown models, processing tiers, or
//! incomplete token-category observations produce no estimate.

pub const VERIFIED_ON: &str = "2026-10-10";
pub const LONG_CONTEXT_THRESHOLD: u64 = 272_000;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rates {
    input: f64,
    cached_input: f64,
    cache_write: f64,
    output: f64,
}

const fn rates(input: f64, cached_input: f64, cache_write: f64, output: f64) -> Rates {
    Rates {
        input,
        cached_input,
        cache_write,
        output,
    }
}

const STANDARD: &[(&str, Rates)] = &[
    ("gpt-6-astra", rates(10.0, 1.0, 12.5, 50.0)),
    ("gpt-6.1-sol", rates(2.0, 0.1, 2.5, 10.0)),
    ("gpt-6-luna", rates(0.1, 0.01, 0.125, 0.5)),
    ("gpt-5.6-sol", rates(4.0, 0.4, 5.0, 20.0)),
];

/// Estimate one direct OpenAI API request from provider-observed fields.
/// Every token category is required: treating an absent cache count as zero
/// would silently overcharge reads or undercharge writes.
pub fn estimate(
    model: &str,
    service_tier: Option<&str>,
    prompt_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    completion_tokens: Option<u64>,
) -> Option<f64> {
    let prompt = prompt_tokens?;
    let cached = cached_tokens?;
    let written = cache_write_tokens?;
    let output = completion_tokens?;
    let base = STANDARD
        .iter()
        .find(|(id, _)| *id == model)
        .map(|(_, rates)| *rates)?;

    let tier = match service_tier? {
        "default" | "standard" => 1.0,
        "flex" => 0.5,
        "fast" | "priority" => 2.0,
        "ultrafast" => 6.0,
        _ => return None,
    };
    let long = prompt > LONG_CONTEXT_THRESHOLD;
    let input_multiplier = if long { 2.0 } else { 1.0 };
    let output_multiplier = if long { 1.5 } else { 1.0 };
    let uncached = prompt.saturating_sub(cached).saturating_sub(written);
    let total = uncached as f64 * base.input * input_multiplier
        + cached as f64 * base.cached_input * input_multiplier
        + written as f64 * base.cache_write * input_multiplier
        + output as f64 * base.output * output_multiplier;
    Some(total * tier / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::{LONG_CONTEXT_THRESHOLD, VERIFIED_ON, estimate};

    #[test]
    fn standard_and_long_context_rates_are_exact() {
        let short = estimate(
            "gpt-6.1-sol",
            Some("default"),
            Some(1_100),
            Some(100),
            Some(200),
            Some(50),
        )
        .expect("priced");
        let expected = (800.0 * 2.0 + 100.0 * 0.1 + 200.0 * 2.5 + 50.0 * 10.0) / 1e6;
        assert!((short - expected).abs() < 1e-12);

        let prompt = LONG_CONTEXT_THRESHOLD + 1;
        let long = estimate(
            "gpt-6-luna",
            Some("default"),
            Some(prompt),
            Some(0),
            Some(0),
            Some(100),
        )
        .expect("priced");
        let expected = (prompt as f64 * 0.1 * 2.0 + 100.0 * 0.5 * 1.5) / 1e6;
        assert!((long - expected).abs() < 1e-12);
    }

    #[test]
    fn tiers_apply_only_when_known() {
        let args = (Some(10), Some(0), Some(0), Some(10));
        let standard = estimate(
            "gpt-6-astra",
            Some("standard"),
            args.0,
            args.1,
            args.2,
            args.3,
        )
        .unwrap();
        assert_eq!(
            estimate("gpt-6-astra", Some("flex"), args.0, args.1, args.2, args.3),
            Some(standard * 0.5)
        );
        assert_eq!(
            estimate("gpt-6-astra", Some("fast"), args.0, args.1, args.2, args.3),
            Some(standard * 2.0)
        );
        assert_eq!(
            estimate(
                "gpt-6-astra",
                Some("invented"),
                args.0,
                args.1,
                args.2,
                args.3
            ),
            None
        );
    }

    #[test]
    fn unknown_or_incomplete_evidence_is_not_priced() {
        assert_eq!(
            estimate(
                "unknown",
                Some("default"),
                Some(1),
                Some(0),
                Some(0),
                Some(1)
            ),
            None
        );
        assert_eq!(
            estimate("gpt-6-astra", None, Some(1), Some(0), Some(0), Some(1)),
            None
        );
        assert_eq!(
            estimate(
                "gpt-6-astra",
                Some("default"),
                Some(1),
                None,
                Some(0),
                Some(1)
            ),
            None
        );
        assert_eq!(VERIFIED_ON, "2026-10-10");
    }
}
