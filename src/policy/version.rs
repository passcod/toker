//! Explicit policy-selected model version parsing and ordering.

use std::cmp::Ordering;
use std::fmt;

use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The two accepted release-date spellings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateFormat {
    YyyyMmDd,
    Yyyymmdd,
}

/// An explicit parser for the version emitted by one family classifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum VersionParserSpec {
    Semver,
    Date { format: DateFormat },
    NumericTuple { components: u8, separator: String },
}

impl VersionParserSpec {
    pub const MIN_TUPLE_COMPONENTS: u8 = 1;
    pub const MAX_TUPLE_COMPONENTS: u8 = 8;

    pub fn parse(&self, value: &str) -> Result<ParsedVersion, VersionError> {
        match self {
            Self::Semver => Version::parse(value)
                .map(ParsedVersion::Semver)
                .map_err(|_| VersionError::Malformed),
            Self::Date { format } => ReleaseDate::parse(value, *format).map(ParsedVersion::Date),
            Self::NumericTuple {
                components,
                separator,
            } => NumericVersion::parse(value, *components, separator)
                .map(ParsedVersion::NumericTuple),
        }
    }

    pub fn validate(&self) -> Result<(), VersionError> {
        match self {
            Self::NumericTuple {
                components,
                separator,
            } if !(Self::MIN_TUPLE_COMPONENTS..=Self::MAX_TUPLE_COMPONENTS)
                .contains(components) =>
            {
                Err(VersionError::ComponentCount)
            }
            Self::NumericTuple { separator, .. } if separator.is_empty() => {
                Err(VersionError::EmptySeparator)
            }
            _ => Ok(()),
        }
    }
}

/// A parsed release date with calendar validity checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReleaseDate {
    year: u16,
    month: u8,
    day: u8,
}

impl ReleaseDate {
    pub fn new(year: u16, month: u8, day: u8) -> Result<Self, VersionError> {
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return Err(VersionError::Malformed);
        }
        Ok(Self { year, month, day })
    }

    pub const fn year(self) -> u16 {
        self.year
    }

    pub const fn month(self) -> u8 {
        self.month
    }

    pub const fn day(self) -> u8 {
        self.day
    }

    fn parse(value: &str, format: DateFormat) -> Result<Self, VersionError> {
        if !value.is_ascii() {
            return Err(VersionError::Malformed);
        }
        let (year, month, day) = match format {
            DateFormat::YyyyMmDd => {
                if value.len() != 10
                    || value.as_bytes().get(4) != Some(&b'-')
                    || value.as_bytes().get(7) != Some(&b'-')
                {
                    return Err(VersionError::Malformed);
                }
                (
                    parse_digits(&value[0..4])?,
                    parse_digits(&value[5..7])?,
                    parse_digits(&value[8..10])?,
                )
            }
            DateFormat::Yyyymmdd => {
                if value.len() != 8 {
                    return Err(VersionError::Malformed);
                }
                (
                    parse_digits(&value[0..4])?,
                    parse_digits(&value[4..6])?,
                    parse_digits(&value[6..8])?,
                )
            }
        };
        let year = u16::try_from(year).map_err(|_| VersionError::Overflow)?;
        let month = u8::try_from(month).map_err(|_| VersionError::Overflow)?;
        let day = u8::try_from(day).map_err(|_| VersionError::Overflow)?;
        Self::new(year, month, day)
    }
}

fn parse_digits(value: &str) -> Result<u64, VersionError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(VersionError::Malformed);
    }
    value.parse().map_err(|_| VersionError::Overflow)
}

const fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

/// A numeric version whose arity was declared by policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NumericVersion(Vec<u64>);

impl NumericVersion {
    fn parse(value: &str, components: u8, separator: &str) -> Result<Self, VersionError> {
        if !(VersionParserSpec::MIN_TUPLE_COMPONENTS..=VersionParserSpec::MAX_TUPLE_COMPONENTS)
            .contains(&components)
        {
            return Err(VersionError::ComponentCount);
        }
        if separator.is_empty() {
            return Err(VersionError::EmptySeparator);
        }
        let values: Vec<u64> = value
            .split(separator)
            .map(parse_digits)
            .collect::<Result<_, _>>()?;
        if values.len() != usize::from(components) {
            return Err(VersionError::ComponentCount);
        }
        Ok(Self(values))
    }

    pub fn components(&self) -> &[u64] {
        &self.0
    }
}

impl PartialOrd for NumericVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        (self.0.len() == other.0.len()).then(|| self.0.cmp(&other.0))
    }
}

/// A structured version. Values of different parser kinds are incomparable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedVersion {
    Semver(Version),
    Date(ReleaseDate),
    NumericTuple(NumericVersion),
}

impl PartialOrd for ParsedVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Semver(a), Self::Semver(b)) => a.partial_cmp(b),
            (Self::Date(a), Self::Date(b)) => a.partial_cmp(b),
            (Self::NumericTuple(a), Self::NumericTuple(b)) => a.partial_cmp(b),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum VersionError {
    #[error("malformed version")]
    Malformed,
    #[error("version component overflow")]
    Overflow,
    #[error("numeric tuple component count does not match policy")]
    ComponentCount,
    #[error("numeric tuple separator must not be empty")]
    EmptySeparator,
}

impl fmt::Display for ReleaseDate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:04}-{:02}-{:02}",
            self.year, self.month, self.day
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_orders_semver_without_lexical_guessing() {
        let parser = VersionParserSpec::Semver;
        assert!(parser.parse("1.10.0").unwrap() > parser.parse("1.9.0").unwrap());
        assert_eq!(parser.parse("v1.2"), Err(VersionError::Malformed));
    }

    #[test]
    fn parses_both_explicit_date_formats_and_checks_calendar() {
        let dashed = VersionParserSpec::Date {
            format: DateFormat::YyyyMmDd,
        };
        let compact = VersionParserSpec::Date {
            format: DateFormat::Yyyymmdd,
        };
        assert_eq!(dashed.parse("2024-02-29"), compact.parse("20240229"));
        assert_eq!(dashed.parse("2023-02-29"), Err(VersionError::Malformed));
        assert_eq!(compact.parse("20241301"), Err(VersionError::Malformed));
    }

    #[test]
    fn numeric_tuples_have_fixed_arity_and_checked_components() {
        let parser = VersionParserSpec::NumericTuple {
            components: 3,
            separator: ".".to_owned(),
        };
        let ten = parser.parse("2.10.0").unwrap();
        let nine = parser.parse("2.9.99").unwrap();
        assert!(ten > nine);
        assert_eq!(parser.parse("2.10"), Err(VersionError::ComponentCount));
        assert_eq!(parser.parse("2.1.0x"), Err(VersionError::Malformed));
        assert_eq!(
            parser.parse("2.18446744073709551616.0"),
            Err(VersionError::Overflow)
        );
    }

    #[test]
    fn parser_kinds_are_not_cross_compared() {
        let semver = VersionParserSpec::Semver.parse("1.0.0").unwrap();
        let date = VersionParserSpec::Date {
            format: DateFormat::Yyyymmdd,
        }
        .parse("20240101")
        .unwrap();
        assert_eq!(semver.partial_cmp(&date), None);
    }
}
