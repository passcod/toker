//! Display formatting that follows the user's locale settings.
//!
//! The dashboard's clocks and grouped counts are for a person to read, so
//! they follow the POSIX locale variables — through ICU4X, which carries
//! the CLDR data the predecessor got from Node's ICU. What a model reads
//! (the gate notices) does not come through here: those strings are
//! asserted byte for byte and must not move with the host, so they keep
//! their pinned forms in `middleware::cold`.
//!
//! The variables are read in their own precedence order, per category:
//! times from `LC_ALL` > `LC_TIME` > `LANG`, numbers from `LC_ALL` >
//! `LC_NUMERIC` > `LANG`. Reading `LANG` alone is the predecessor's
//! recorded bug (`fmt.mjs`): a machine with `LANG=en_NZ.UTF-8` and
//! `LC_TIME=en_GB.UTF-8` resolved to en-NZ, whose CLDR clock is 12-hour,
//! and printed "9:30:00 pm" against an explicit 24-hour preference.
//!
//! [`Fmt`] is resolved once at startup and passed into rendering, so the
//! view stays a pure function of its inputs and a test never depends on
//! the host's locale. With no usable variable, [`Fmt::fixed`] is the
//! pinned format the dashboard used before: comma grouping and 24-hour
//! `strftime` forms.

use icu_datetime::{
    DateTimeFormatter, NoCalendarFormatter, fieldsets,
    input::{DateTime, Time},
};
use icu_decimal::{DecimalFormatter, input::Decimal};
use icu_locale_core::Locale;
use jiff::{Zoned, tz::TimeZone};
use jiff_icu::ConvertFrom as _;

use crate::middleware::cold;

/// `en_GB.UTF-8` → `en-GB`; `None` for anything ICU cannot use. The
/// codeset and any `@modifier` go first — ICU tags have no place for
/// either.
fn to_tag(value: Option<&str>) -> Option<Locale> {
    let value = value?;
    let tag = value.split('.').next().unwrap_or("");
    let tag = tag.split('@').next().unwrap_or("").replace('_', "-");
    // C and POSIX name no language or region. Treated as tags they get a
    // silent fallback to something arbitrary, so they are skipped and
    // the next variable answers.
    if tag.is_empty() || tag == "C" || tag == "POSIX" {
        return None;
    }
    Locale::try_from_str(&tag).ok()
}

/// The first of `names` that holds a usable tag.
fn first_tag(env: &dyn Fn(&str) -> Option<String>, names: [&str; 3]) -> Option<Locale> {
    names
        .into_iter()
        .find_map(|name| to_tag(env(name).as_deref()))
}

/// The time locale and the number locale, from an environment lookup —
/// injected, so the precedence is testable without touching the
/// process environment.
pub(crate) fn resolve(env: &dyn Fn(&str) -> Option<String>) -> (Option<Locale>, Option<Locale>) {
    (
        first_tag(env, ["LC_ALL", "LC_TIME", "LANG"]),
        first_tag(env, ["LC_ALL", "LC_NUMERIC", "LANG"]),
    )
}

/// The ICU formatters for one time locale: the header's seconds clock
/// and the three resolutions a future instant renders at
/// ([`cold::reset_label`]'s scales).
struct Times {
    clock_sec: NoCalendarFormatter<fieldsets::T>,
    clock: NoCalendarFormatter<fieldsets::T>,
    weekday_clock: DateTimeFormatter<fieldsets::ET>,
    day_month: DateTimeFormatter<fieldsets::MD>,
}

impl Times {
    fn new(locale: &Locale) -> Option<Self> {
        let prefs = || locale.into();
        Some(Self {
            clock_sec: NoCalendarFormatter::try_new(prefs(), fieldsets::T::hms()).ok()?,
            clock: NoCalendarFormatter::try_new(prefs(), fieldsets::T::hm()).ok()?,
            weekday_clock: DateTimeFormatter::try_new(
                prefs(),
                fieldsets::E::short().with_time_hm(),
            )
            .ok()?,
            day_month: DateTimeFormatter::try_new(prefs(), fieldsets::MD::medium()).ok()?,
        })
    }
}

/// The display formatter: locale-following where a locale resolved,
/// the pinned forms where none did.
pub(crate) struct Fmt {
    numbers: Option<DecimalFormatter>,
    times: Option<Times>,
}

impl Fmt {
    /// The pinned forms: comma grouping and 24-hour clocks. The fallback
    /// when no variable names a locale, and the tests' default, so
    /// their expected strings hold on any host.
    pub(crate) fn fixed() -> Self {
        Self {
            numbers: None,
            times: None,
        }
    }

    /// Formatters for explicit locales; either may be absent, and that
    /// category keeps its pinned form.
    pub(crate) fn new(times: Option<&Locale>, numbers: Option<&Locale>) -> Self {
        let mut fmt = Self::fixed();
        fmt.numbers = numbers
            .and_then(|locale| DecimalFormatter::try_new(locale.into(), Default::default()).ok());
        fmt.times = times.and_then(Times::new);
        fmt
    }

    /// From the process environment — read once, at startup: these are
    /// process-lifetime settings, and the loop would otherwise re-read
    /// them every frame.
    pub(crate) fn from_env() -> Self {
        let (times, numbers) = resolve(&|name| std::env::var(name).ok());
        Self::new(times.as_ref(), numbers.as_ref())
    }

    /// A grouped count: `12,213,961` pinned, the locale's grouping and
    /// separator otherwise (`12 213 961` with U+202F in fr, `1,22,13,961`
    /// in en-IN) — which is why every column is measured from the
    /// formatted text, never from a digit count.
    pub(crate) fn count(&self, value: i64) -> String {
        match &self.numbers {
            Some(formatter) => formatter.format(&Decimal::from(value)).to_string(),
            None => fixed_grouped(value),
        }
    }

    /// [`Fmt::count`] for a count that may be unknown: `-`, never a zero.
    pub(crate) fn grouped(&self, value: Option<i64>) -> String {
        value.map_or_else(|| "-".to_owned(), |value| self.count(value))
    }

    /// The header clock, to the second.
    pub(crate) fn clock_sec(&self, now: &Zoned) -> String {
        match &self.times {
            Some(times) => times
                .clock_sec
                .format(&Time::convert_from(now.time()))
                .to_string(),
            None => now.strftime("%H:%M:%S").to_string(),
        }
    }

    /// A future instant at the coarsest resolution that still identifies
    /// it — [`cold::reset_label`]'s rule, in this locale.
    pub(crate) fn reset_label(&self, at_ms: f64, now_ms: i64, tz: &TimeZone) -> String {
        self.at_scale(at_ms, tz, cold::scale_of(at_ms, now_ms))
    }

    /// `at`, labelled so it cannot be misread as belonging to `other`'s
    /// day — [`cold::alongside`]'s rule, in this locale.
    pub(crate) fn alongside(
        &self,
        at_ms: f64,
        other_ms: f64,
        now_ms: i64,
        tz: &TimeZone,
    ) -> String {
        self.at_scale(at_ms, tz, cold::alongside_scale(at_ms, other_ms, now_ms))
    }

    fn at_scale(&self, at_ms: f64, tz: &TimeZone, scale: usize) -> String {
        let Some(times) = &self.times else {
            return cold::at_scale(at_ms, tz, scale);
        };
        let Some(zoned) = cold::zoned_of(at_ms, tz) else {
            return "?".to_owned();
        };
        match scale {
            0 => times
                .clock
                .format(&Time::convert_from(zoned.time()))
                .to_string(),
            1 => times
                .weekday_clock
                .format(&DateTime::convert_from(zoned.datetime()))
                .to_string(),
            _ => times
                .day_month
                .format(&DateTime::convert_from(zoned.datetime()))
                .to_string(),
        }
    }
}

/// Comma grouping by hand, for the pinned path.
fn fixed_grouped(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.bytes().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit as char);
    }
    if value < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

#[cfg(test)]
mod tests {
    use super::{Fmt, resolve, to_tag};
    use icu_locale_core::Locale;
    use std::collections::HashMap;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| vars.get(name).cloned()
    }

    fn tag(value: &str) -> Option<String> {
        to_tag(Some(value)).map(|locale| locale.to_string())
    }

    fn locale(tag: &str) -> Locale {
        Locale::try_from_str(tag).expect("a valid tag")
    }

    #[test]
    fn posix_values_map_to_tags() {
        assert_eq!(tag("en_GB.UTF-8").as_deref(), Some("en-GB"));
        assert_eq!(tag("de_DE@euro").as_deref(), Some("de-DE"));
        assert_eq!(tag("sr_RS.UTF-8@latin").as_deref(), Some("sr-RS"));
        assert_eq!(tag("fr").as_deref(), Some("fr"));
        // C and POSIX name nothing, and neither does junk.
        assert_eq!(tag("C"), None);
        assert_eq!(tag("C.UTF-8"), None);
        assert_eq!(tag("POSIX"), None);
        assert_eq!(tag(""), None);
        assert_eq!(tag("not a locale!"), None);
    }

    #[test]
    fn times_prefer_lc_time_over_lang() {
        // This machine's shape: a 12-hour LANG and a 24-hour LC_TIME. A
        // LANG-only reading shows "pm" clocks; LC_TIME must win.
        let (times, numbers) =
            resolve(&env(&[("LANG", "en_NZ.UTF-8"), ("LC_TIME", "en_GB.UTF-8")]));
        assert_eq!(times, Some(locale("en-GB")));
        assert_eq!(numbers, Some(locale("en-NZ")), "LC_TIME is not LC_NUMERIC");

        let fmt = Fmt::new(times.as_ref(), numbers.as_ref());
        let at: jiff::Zoned = "2026-01-21T21:30:05[UTC]".parse().expect("a zoned");
        assert_eq!(fmt.clock_sec(&at), "21:30:05");
        // The LANG-only reading, for contrast: the 12-hour clock.
        let lang_only = Fmt::new(Some(&locale("en-NZ")), None);
        assert!(
            lang_only.clock_sec(&at).contains("pm"),
            "{}",
            lang_only.clock_sec(&at)
        );
    }

    #[test]
    fn lc_all_overrides_and_c_falls_through() {
        let (times, numbers) = resolve(&env(&[
            ("LC_ALL", "de_DE.UTF-8"),
            ("LC_TIME", "en_GB.UTF-8"),
            ("LC_NUMERIC", "fr_FR.UTF-8"),
            ("LANG", "en_NZ.UTF-8"),
        ]));
        assert_eq!(times, Some(locale("de-DE")));
        assert_eq!(numbers, Some(locale("de-DE")));

        // LC_ALL=C names nothing: the category variable answers.
        let (times, numbers) = resolve(&env(&[
            ("LC_ALL", "C"),
            ("LC_NUMERIC", "fr_FR.UTF-8"),
            ("LANG", "POSIX"),
        ]));
        assert_eq!(times, None, "nothing usable: the pinned clock");
        assert_eq!(numbers, Some(locale("fr-FR")));
        assert_eq!(resolve(&env(&[])), (None, None));
    }

    #[test]
    fn numbers_group_by_locale() {
        let at = |tag: &str| Fmt::new(None, Some(&locale(tag)));
        assert_eq!(Fmt::fixed().count(12_213_961), "12,213,961");
        assert_eq!(Fmt::fixed().count(-1_000), "-1,000");
        assert_eq!(at("en-GB").count(1_234_567), "1,234,567");
        assert_eq!(at("en-IN").count(1_234_567), "12,34,567");
        assert_eq!(at("de-DE").count(1_234_567), "1.234.567");
        // fr groups with U+202F: one cell wide, but not ASCII.
        assert_eq!(at("fr-FR").count(1_234_567), "1\u{202f}234\u{202f}567");
        assert_eq!(Fmt::fixed().grouped(None), "-");
    }

    #[test]
    fn reset_labels_follow_the_time_locale() {
        let tz = jiff::tz::TimeZone::UTC;
        let now: jiff::Timestamp = "2026-01-21T12:00:00Z".parse().expect("a timestamp");
        let now_ms = now.as_millisecond();
        let hours = |h: i64| (now_ms + h * 3_600_000) as f64;
        let gb = Fmt::new(Some(&locale("en-GB")), None);
        assert_eq!(gb.reset_label(hours(3), now_ms, &tz), "15:00");
        assert_eq!(gb.reset_label(hours(48), now_ms, &tz), "Fri 12:00");
        assert_eq!(gb.reset_label(hours(20 * 24), now_ms, &tz), "10 Feb");
        // The pinned forms, unchanged, where no locale resolved.
        let fixed = Fmt::fixed();
        assert_eq!(fixed.reset_label(hours(3), now_ms, &tz), "15:00");
        assert_eq!(fixed.reset_label(hours(48), now_ms, &tz), "Fri 12:00");
        assert_eq!(fixed.reset_label(hours(20 * 24), now_ms, &tz), "10 Feb");
        // A 12-hour locale keeps its day period.
        let us = Fmt::new(Some(&locale("en-US")), None);
        assert!(us.reset_label(hours(3), now_ms, &tz).contains("PM"));
        // alongside carries the weekday when the other instant does.
        assert_eq!(gb.alongside(hours(3), hours(48), now_ms, &tz), "Wed 15:00");
    }
}
