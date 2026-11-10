//! Checking a value as it is typed.
//!
//! The service is the only judge of whether a value can be used, and it
//! refuses on save. What can be told from the text alone is told here as
//! the text changes, so a form that cannot be saved says why beside its
//! field instead of after the request (spec: UTUI#the-configuration-pages).

/// A token count as the dashboard writes it: `175k`, `1M`, `1.5M`.
///
/// Rounded figures read short and everything else in full, so what the
/// field shows is what is stored.
pub(super) fn tokens_shown(tokens: u64) -> String {
    let short = crate::tui::view::short_tokens(tokens);
    if parse_tokens(&short) == Ok(tokens) {
        short
    } else {
        group(tokens)
    }
}

/// `175000` as `175,000`.
pub(super) fn group(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A token count in full or in shorthand: `175000`, `175,000`, `175k`, `1M`
/// or `1.5m`. A shorthand that does not come to a whole number of tokens is
/// refused rather than rounded.
pub(super) fn parse_tokens(text: &str) -> Result<u64, String> {
    let how = || format!("{text:?} is not a token count: write 175000, 175k or 1M");
    let cleaned: String = text
        .chars()
        .filter(|c| !matches!(c, ',' | '_' | ' '))
        .collect::<String>()
        .to_ascii_lowercase();
    let (number, scale) = match cleaned.chars().last() {
        Some('k') => (&cleaned[..cleaned.len() - 1], 1_000u64),
        Some('m') => (&cleaned[..cleaned.len() - 1], 1_000_000),
        _ => (cleaned.as_str(), 1),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty() {
        return Err(how());
    }
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(whole) || !digits(fraction) || fraction.len() > 6 {
        return Err(how());
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| how())?
    };
    let fraction_value: u64 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().map_err(|_| how())?
    };
    let denominator = 10u64.pow(fraction.len() as u32);
    let parts = fraction_value * scale;
    if !parts.is_multiple_of(denominator) {
        return Err(format!(
            "{text:?} is not a whole number of tokens: write the full figure, such as 175000"
        ));
    }
    whole
        .checked_mul(scale)
        .and_then(|whole| whole.checked_add(parts / denominator))
        .ok_or_else(how)
}

/// A number of minutes, fractions allowed.
pub(super) fn parse_minutes(text: &str) -> Result<f64, String> {
    match text.trim().parse::<f64>() {
        Ok(minutes) if minutes.is_finite() && minutes > 0.0 => Ok(minutes),
        _ => Err(format!(
            "{text:?} is not a number of minutes: write one above zero, such as 45 or 0.5"
        )),
    }
}

/// An `http` or `https` address, which is all a provider is reached at.
pub(super) fn parse_url(text: &str) -> Result<String, String> {
    match reqwest::Url::parse(text) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.has_host() => {
            Ok(text.to_owned())
        }
        Ok(_) => Err("must be http:// or https:// with a host".to_owned()),
        Err(_) => Err("not an address: start it with http:// or https://".to_owned()),
    }
}

/// One header name, as the router will compare it: lower case, so what is
/// typed in capitals is folded rather than refused.
pub(super) fn parse_header(text: &str) -> Result<String, String> {
    let name = text.trim().to_ascii_lowercase();
    match reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
        Ok(_) => Ok(name),
        Err(_) => Err(format!(
            "{text:?} is not a header name: letters, digits and - only"
        )),
    }
}

/// Header names, comma separated.
pub(super) fn parse_headers(text: &str) -> Result<Vec<String>, String> {
    pieces(text).into_iter().map(parse_header).collect()
}

/// Directories, comma separated. A leading `~/` is expanded by the service
/// where it is read, so it is kept as typed.
pub(super) fn parse_paths(text: &str) -> Result<Vec<String>, String> {
    let paths = pieces(text);
    match paths.iter().find(|path| !path.starts_with(['/', '~'])) {
        Some(path) => Err(format!(
            "{path:?} is not an absolute path: start it with / or ~/"
        )),
        None => Ok(paths.into_iter().map(str::to_owned).collect()),
    }
}

/// How a gate notice can render, as the service accepts them.
pub(super) const NOTICE_STYLES: [&str; 4] = ["gfm", "toker", "block", "plain"];

pub(super) fn parse_style(text: &str) -> Result<String, String> {
    let style = text.trim().to_ascii_lowercase();
    if NOTICE_STYLES.contains(&style.as_str()) {
        Ok(style)
    } else {
        Err(format!(
            "{text:?} is not a style: use {}",
            NOTICE_STYLES.join(", ")
        ))
    }
}

/// A client version to present, which has to be one token.
pub(super) fn parse_version(text: &str) -> Result<String, String> {
    let version = text.trim();
    if version.contains(char::is_whitespace) {
        Err("a version has no spaces in it".to_owned())
    } else {
        Ok(version.to_owned())
    }
}

fn pieces(text: &str) -> Vec<&str> {
    text.split(',')
        .map(str::trim)
        .filter(|piece| !piece.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_count_takes_the_full_figure_and_the_shorthand() {
        for (typed, tokens) in [
            ("175000", 175_000),
            ("175,000", 175_000),
            ("175k", 175_000),
            ("175K", 175_000),
            ("1M", 1_000_000),
            ("1m", 1_000_000),
            ("1.5M", 1_500_000),
            ("0.5k", 500),
            (" 2M ", 2_000_000),
        ] {
            assert_eq!(parse_tokens(typed), Ok(tokens), "{typed:?}");
        }
    }

    #[test]
    fn a_token_count_that_is_not_one_is_refused_with_how_to_write_it() {
        for typed in ["lots", "k", "1.2.3M", "-5", "1.5", "12x", "1.0000001M"] {
            assert!(parse_tokens(typed).is_err(), "{typed:?}");
        }
        assert!(parse_tokens("lots").expect_err("refused").contains("175k"));
    }

    #[test]
    fn a_count_reads_back_as_it_would_be_typed() {
        assert_eq!(tokens_shown(175_000), "175k");
        assert_eq!(tokens_shown(1_000_000), "1M");
        // Rounding would change the stored figure, so it reads in full.
        assert_eq!(tokens_shown(175_500), "175,500");
        assert_eq!(parse_tokens(&tokens_shown(175_500)), Ok(175_500));
    }

    #[test]
    fn minutes_may_be_fractional_but_not_zero_or_negative() {
        assert_eq!(parse_minutes("0.5"), Ok(0.5));
        assert_eq!(parse_minutes(" 45 "), Ok(45.0));
        for typed in ["0", "-3", "soon", "inf", "NaN"] {
            assert!(parse_minutes(typed).is_err(), "{typed:?}");
        }
    }

    #[test]
    fn an_address_is_http_or_https_with_a_host() {
        assert!(parse_url("http://gateway.invalid").is_ok());
        assert!(parse_url("https://gateway.invalid/v1").is_ok());
        for typed in ["gateway.invalid", "ftp://gateway.invalid", "http://"] {
            assert!(parse_url(typed).is_err(), "{typed:?}");
        }
    }

    #[test]
    fn header_names_fold_to_lower_case_and_refuse_what_cannot_be_one() {
        assert_eq!(
            parse_headers("X-Toker-Session, x-session-id"),
            Ok(vec![
                "x-toker-session".to_owned(),
                "x-session-id".to_owned()
            ])
        );
        assert!(parse_headers("two words").is_err());
        assert!(parse_header("bad name").is_err());
    }

    #[test]
    fn paths_must_be_absolute_or_home_relative() {
        assert_eq!(
            parse_paths("/harness/.claude, ~/other"),
            Ok(vec!["/harness/.claude".to_owned(), "~/other".to_owned()])
        );
        assert!(parse_paths("relative/dir").is_err());
    }

    #[test]
    fn a_notice_style_is_one_the_service_renders() {
        assert_eq!(parse_style("Block"), Ok("block".to_owned()));
        assert!(parse_style("fancy").is_err());
    }
}
