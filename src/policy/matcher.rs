//! Bounded matching and checked `${name}` / `${1}` templates.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::document::{MAX_MATCHER_BYTES, MAX_TEMPLATE_BYTES, StableId};

pub const MAX_MATCH_INPUT_BYTES: usize = 4_096;
pub const MAX_RENDERED_TEMPLATE_BYTES: usize = 4_096;

/// A serialisable matcher. Only regular expressions expose captures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MatcherSpec {
    Exact(String),
    Glob(String),
    Regex(String),
    /// Revision-one model-map matching after canonical identity folding.
    #[serde(rename = "revision_one_model")]
    RevisionOneModel(String),
    /// Revision-one family matching through the legacy classifier.
    #[serde(rename = "revision_one_family")]
    RevisionOneFamily(String),
}

impl MatcherSpec {
    pub fn compile(&self) -> Result<CompiledMatcher, MatcherError> {
        let value = self.value();
        if value.len() > MAX_MATCHER_BYTES {
            return Err(MatcherError::TooLong);
        }
        match self {
            Self::Exact(value) => Ok(CompiledMatcher::Exact(value.clone())),
            Self::Glob(value) => GlobBuilder::new(value)
                .literal_separator(true)
                .backslash_escape(true)
                .build()
                .map(|glob| glob.compile_matcher())
                .map(CompiledMatcher::Glob)
                .map_err(|error| MatcherError::InvalidGlob(error.to_string())),
            Self::Regex(value) => Regex::new(value)
                .map(CompiledMatcher::Regex)
                .map_err(|error| MatcherError::InvalidRegex(error.to_string())),
            Self::RevisionOneModel(value) => Ok(CompiledMatcher::RevisionOneModel(value.clone())),
            Self::RevisionOneFamily(value) => Ok(CompiledMatcher::RevisionOneFamily(value.clone())),
        }
    }

    pub fn value(&self) -> &str {
        match self {
            Self::Exact(value)
            | Self::Glob(value)
            | Self::Regex(value)
            | Self::RevisionOneModel(value)
            | Self::RevisionOneFamily(value) => value,
        }
    }
}

/// The captures that one matcher can supply to templates. `optional` names a
/// capture which the regex can syntactically expose but may not populate for a
/// particular match; rendering reports that case as a typed resolution error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureSchema {
    numbered: BTreeSet<usize>,
    named: BTreeSet<String>,
}

impl CaptureSchema {
    pub fn numbered(&self) -> impl Iterator<Item = usize> + '_ {
        self.numbered.iter().copied()
    }

    pub fn named(&self) -> impl Iterator<Item = &str> + '_ {
        self.named.iter().map(String::as_str)
    }

    fn contains(&self, reference: &str) -> bool {
        reference
            .parse::<usize>()
            .ok()
            .is_some_and(|number| self.numbered.contains(&number))
            || self.named.contains(reference)
    }
}

/// A compiled exact, glob, or Rust regex matcher.
#[derive(Debug, Clone)]
pub enum CompiledMatcher {
    Exact(String),
    Glob(GlobMatcher),
    Regex(Regex),
    RevisionOneModel(String),
    RevisionOneFamily(String),
}

impl CompiledMatcher {
    pub fn capture_schema(&self) -> CaptureSchema {
        let Self::Regex(regex) = self else {
            return CaptureSchema::default();
        };
        let mut schema = CaptureSchema::default();
        for (index, name) in regex.capture_names().enumerate() {
            if index > 0 {
                schema.numbered.insert(index);
            }
            if let Some(name) = name {
                schema.named.insert(name.to_owned());
            }
        }
        schema
    }

    pub fn captures<'a>(
        &self,
        input: &'a str,
    ) -> Result<Option<MatcherCaptures<'a>>, MatcherError> {
        if input.len() > MAX_MATCH_INPUT_BYTES {
            return Err(MatcherError::InputTooLong);
        }
        match self {
            Self::Exact(value) => Ok((value == input).then(|| MatcherCaptures::empty(input))),
            Self::Glob(glob) => Ok(glob.is_match(input).then(|| MatcherCaptures::empty(input))),
            Self::Regex(regex) => {
                let Some(captures) = regex.captures(input) else {
                    return Ok(None);
                };
                let mut result = MatcherCaptures::empty(input);
                for index in 1..captures.len() {
                    if let Some(value) = captures.get(index) {
                        result.numbered.insert(index, value.range());
                    }
                }
                for name in regex.capture_names().flatten() {
                    if let Some(value) = captures.name(name) {
                        result.named.insert(name.to_owned(), value.range());
                    }
                }
                Ok(Some(result))
            }
            Self::RevisionOneModel(expected) => Ok(crate::catalog::model_identity(input)
                .is_some_and(|identity| identity == *expected)
                .then(|| MatcherCaptures::empty(input))),
            Self::RevisionOneFamily(expected) => Ok(super::family::family_of(input)
                .is_some_and(|family| family.name == *expected)
                .then(|| MatcherCaptures::empty(input))),
        }
    }

    pub fn is_match(&self, input: &str) -> Result<bool, MatcherError> {
        Ok(self.captures(input)?.is_some())
    }
}

/// Borrowed captures from one successful regular-expression match. Overlapping
/// numbered and named captures share the same input rather than cloning it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatcherCaptures<'a> {
    input: &'a str,
    numbered: BTreeMap<usize, Range<usize>>,
    named: BTreeMap<String, Range<usize>>,
}

impl<'a> MatcherCaptures<'a> {
    fn empty(input: &'a str) -> Self {
        Self {
            input,
            numbered: BTreeMap::new(),
            named: BTreeMap::new(),
        }
    }

    pub fn get(&self, reference: &str) -> Option<&'a str> {
        let range = if let Ok(number) = reference.parse::<usize>() {
            self.numbered.get(&number)
        } else {
            self.named.get(reference)
        }?;
        self.input.get(range.clone())
    }
}

/// A template checked against route parameters and one matcher's captures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    source: String,
    parts: Vec<TemplatePart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplatePart {
    Literal(Range<usize>),
    Reference(String),
}

impl Template {
    pub fn parse(
        source: impl Into<String>,
        route_parameters: &BTreeSet<StableId>,
        captures: &CaptureSchema,
    ) -> Result<Self, TemplateError> {
        let source = source.into();
        if source.len() > MAX_TEMPLATE_BYTES {
            return Err(TemplateError::TooLong);
        }
        let parts = parse_parts(&source)?;
        for part in &parts {
            let TemplatePart::Reference(reference) = part else {
                continue;
            };
            let parameter = StableId::new(reference)
                .ok()
                .filter(|id| route_parameters.contains(id));
            if parameter.is_some() && captures.contains(reference) {
                return Err(TemplateError::AmbiguousReference(reference.clone()));
            }
            if parameter.is_none() && !captures.contains(reference) {
                return Err(TemplateError::UnknownReference(reference.clone()));
            }
        }
        Ok(Self { source, parts })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn render(
        &self,
        route_parameters: &BTreeMap<StableId, String>,
        captures: &MatcherCaptures<'_>,
    ) -> Result<String, TemplateError> {
        let mut length = 0usize;
        let mut values = Vec::with_capacity(self.parts.len());
        for part in &self.parts {
            let value = match part {
                TemplatePart::Literal(range) => self
                    .source
                    .get(range.clone())
                    .ok_or(TemplateError::InvalidReference)?,
                TemplatePart::Reference(reference) => {
                    let parameter = StableId::new(reference)
                        .ok()
                        .and_then(|id| route_parameters.get(&id))
                        .map(String::as_str);
                    parameter
                        .or_else(|| captures.get(reference))
                        .ok_or_else(|| TemplateError::MissingCapture(reference.clone()))?
                }
            };
            length = length
                .checked_add(value.len())
                .ok_or(TemplateError::RenderedTooLong)?;
            if length > MAX_RENDERED_TEMPLATE_BYTES {
                return Err(TemplateError::RenderedTooLong);
            }
            values.push(value);
        }
        let mut rendered = String::with_capacity(length);
        for value in values {
            rendered.push_str(value);
        }
        Ok(rendered)
    }
}

fn parse_parts(source: &str) -> Result<Vec<TemplatePart>, TemplateError> {
    let mut parts = Vec::new();
    let mut position = 0;
    while let Some(relative_start) = source[position..].find("${") {
        let start = position + relative_start;
        if start > position {
            parts.push(TemplatePart::Literal(position..start));
        }
        let reference_start = start + 2;
        let relative_end = source[reference_start..]
            .find('}')
            .ok_or(TemplateError::UnclosedReference)?;
        let end = reference_start + relative_end;
        let reference = &source[reference_start..end];
        if reference.is_empty() || reference.contains("${") {
            return Err(TemplateError::InvalidReference);
        }
        parts.push(TemplatePart::Reference(reference.to_owned()));
        position = end + 1;
    }
    if position < source.len() {
        parts.push(TemplatePart::Literal(position..source.len()));
    }
    Ok(parts)
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MatcherError {
    #[error("matcher exceeds its byte limit")]
    TooLong,
    #[error("match input exceeds its byte limit")]
    InputTooLong,
    #[error("invalid glob: {0}")]
    InvalidGlob(String),
    #[error("invalid regular expression: {0}")]
    InvalidRegex(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TemplateError {
    #[error("template exceeds its byte limit")]
    TooLong,
    #[error("rendered template exceeds its byte limit")]
    RenderedTooLong,
    #[error("template reference is not closed")]
    UnclosedReference,
    #[error("template reference is malformed")]
    InvalidReference,
    #[error("template references unknown value {0}")]
    UnknownReference(String),
    #[error("template reference {0} collides between a parameter and capture")]
    AmbiguousReference(String),
    #[error("template capture {0} did not participate in this match")]
    MissingCapture(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_glob_match_without_exposing_captures() {
        let exact = MatcherSpec::Exact("model-1".to_owned()).compile().unwrap();
        let glob = MatcherSpec::Glob("model-*".to_owned()).compile().unwrap();
        assert!(exact.is_match("model-1").unwrap());
        assert!(glob.is_match("model-2").unwrap());
        assert_eq!(exact.capture_schema(), CaptureSchema::default());
        assert_eq!(glob.capture_schema(), CaptureSchema::default());
    }

    #[test]
    fn regex_exposes_borrowed_named_and_numbered_captures() {
        let matcher = MatcherSpec::Regex(r"^model-(?P<family>[a-z]+)-(\d+)$".to_owned())
            .compile()
            .unwrap();
        let route_parameters = BTreeSet::from([StableId::new("requested_model").unwrap()]);
        let template = Template::parse(
            "${family}/${2}/${requested_model}",
            &route_parameters,
            &matcher.capture_schema(),
        )
        .unwrap();
        let captures = matcher.captures("model-alpha-12").unwrap().unwrap();
        let values = BTreeMap::from([(
            StableId::new("requested_model").unwrap(),
            "original".to_owned(),
        )]);
        assert_eq!(
            template.render(&values, &captures).unwrap(),
            "alpha/12/original"
        );
    }

    #[test]
    fn rejects_parameter_capture_name_collisions() {
        let matcher = MatcherSpec::Regex(r"^(?P<model>.*)$".to_owned())
            .compile()
            .unwrap();
        let parameters = BTreeSet::from([StableId::new("model").unwrap()]);
        assert_eq!(
            Template::parse("${model}", &parameters, &matcher.capture_schema()),
            Err(TemplateError::AmbiguousReference("model".to_owned()))
        );
    }

    #[test]
    fn optional_unmatched_capture_is_a_typed_resolution_error() {
        let matcher = MatcherSpec::Regex(r"^(?P<optional>x)?$".to_owned())
            .compile()
            .unwrap();
        let template =
            Template::parse("${optional}", &BTreeSet::new(), &matcher.capture_schema()).unwrap();
        let captures = matcher.captures("").unwrap().unwrap();
        assert_eq!(
            template.render(&BTreeMap::new(), &captures),
            Err(TemplateError::MissingCapture("optional".to_owned()))
        );
    }

    #[test]
    fn bounds_match_input_and_template_amplification() {
        let exact = MatcherSpec::Exact("x".to_owned()).compile().unwrap();
        assert_eq!(
            exact.is_match(&"x".repeat(MAX_MATCH_INPUT_BYTES + 1)),
            Err(MatcherError::InputTooLong)
        );

        let matcher = MatcherSpec::Regex(r"^(?P<x>.*)$".to_owned())
            .compile()
            .unwrap();
        let template =
            Template::parse("${x}${x}", &BTreeSet::new(), &matcher.capture_schema()).unwrap();
        let input = "x".repeat(MAX_RENDERED_TEMPLATE_BYTES / 2 + 1);
        let captures = matcher.captures(&input).unwrap().unwrap();
        assert_eq!(
            template.render(&BTreeMap::new(), &captures),
            Err(TemplateError::RenderedTooLong)
        );
    }

    #[test]
    fn rejects_invalid_matchers_and_unknown_template_references() {
        assert!(matches!(
            MatcherSpec::Glob("[".to_owned()).compile(),
            Err(MatcherError::InvalidGlob(_))
        ));
        assert!(matches!(
            MatcherSpec::Regex("(".to_owned()).compile(),
            Err(MatcherError::InvalidRegex(_))
        ));
        let exact = MatcherSpec::Exact("x".to_owned()).compile().unwrap();
        assert_eq!(
            Template::parse("${1}", &BTreeSet::new(), &exact.capture_schema()),
            Err(TemplateError::UnknownReference("1".to_owned()))
        );
    }
}
