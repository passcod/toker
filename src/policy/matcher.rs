//! Bounded matching and checked `${name}` / `${1}` templates.

use std::collections::{BTreeMap, BTreeSet};

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::document::{MAX_MATCHER_BYTES, MAX_TEMPLATE_BYTES, StableId};

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
        }
    }

    pub fn value(&self) -> &str {
        match self {
            Self::Exact(value) | Self::Glob(value) | Self::Regex(value) => value,
        }
    }
}

/// The captures that one matcher can supply to templates.
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

    pub fn captures(&self, input: &str) -> Option<MatcherCaptures> {
        match self {
            Self::Exact(value) => (value == input).then(MatcherCaptures::default),
            Self::Glob(glob) => glob.is_match(input).then(MatcherCaptures::default),
            Self::Regex(regex) => {
                let captures = regex.captures(input)?;
                let mut result = MatcherCaptures::default();
                for index in 1..captures.len() {
                    if let Some(value) = captures.get(index) {
                        result.numbered.insert(index, value.as_str().to_owned());
                    }
                }
                for name in regex.capture_names().flatten() {
                    if let Some(value) = captures.name(name) {
                        result
                            .named
                            .insert(name.to_owned(), value.as_str().to_owned());
                    }
                }
                Some(result)
            }
        }
    }

    pub fn is_match(&self, input: &str) -> bool {
        self.captures(input).is_some()
    }
}

/// Captures from one successful regular-expression match.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatcherCaptures {
    numbered: BTreeMap<usize, String>,
    named: BTreeMap<String, String>,
}

impl MatcherCaptures {
    pub fn get(&self, reference: &str) -> Option<&str> {
        if let Ok(number) = reference.parse::<usize>() {
            return self.numbered.get(&number).map(String::as_str);
        }
        self.named.get(reference).map(String::as_str)
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
    Literal(String),
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
            let route_parameter = StableId::new(reference)
                .ok()
                .is_some_and(|id| route_parameters.contains(&id));
            if !route_parameter && !captures.contains(reference) {
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
        captures: &MatcherCaptures,
    ) -> Result<String, TemplateError> {
        let mut rendered = String::new();
        for part in &self.parts {
            match part {
                TemplatePart::Literal(value) => rendered.push_str(value),
                TemplatePart::Reference(reference) => {
                    let parameter = StableId::new(reference)
                        .ok()
                        .and_then(|id| route_parameters.get(&id))
                        .map(String::as_str);
                    let value = parameter
                        .or_else(|| captures.get(reference))
                        .ok_or_else(|| TemplateError::MissingValue(reference.clone()))?;
                    rendered.push_str(value);
                }
            }
        }
        Ok(rendered)
    }
}

fn parse_parts(source: &str) -> Result<Vec<TemplatePart>, TemplateError> {
    let mut parts = Vec::new();
    let mut remaining = source;
    while let Some(start) = remaining.find("${") {
        if start > 0 {
            parts.push(TemplatePart::Literal(remaining[..start].to_owned()));
        }
        let after_start = &remaining[start + 2..];
        let end = after_start
            .find('}')
            .ok_or(TemplateError::UnclosedReference)?;
        let reference = &after_start[..end];
        if reference.is_empty() || reference.contains("${") {
            return Err(TemplateError::InvalidReference);
        }
        parts.push(TemplatePart::Reference(reference.to_owned()));
        remaining = &after_start[end + 1..];
    }
    if !remaining.is_empty() {
        parts.push(TemplatePart::Literal(remaining.to_owned()));
    }
    Ok(parts)
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MatcherError {
    #[error("matcher exceeds its byte limit")]
    TooLong,
    #[error("invalid glob: {0}")]
    InvalidGlob(String),
    #[error("invalid regular expression: {0}")]
    InvalidRegex(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TemplateError {
    #[error("template exceeds its byte limit")]
    TooLong,
    #[error("template reference is not closed")]
    UnclosedReference,
    #[error("template reference is malformed")]
    InvalidReference,
    #[error("template references unknown value {0}")]
    UnknownReference(String),
    #[error("template value {0} was not supplied")]
    MissingValue(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_glob_match_without_exposing_captures() {
        let exact = MatcherSpec::Exact("model-1".to_owned()).compile().unwrap();
        let glob = MatcherSpec::Glob("model-*".to_owned()).compile().unwrap();
        assert!(exact.is_match("model-1"));
        assert!(glob.is_match("model-2"));
        assert_eq!(exact.capture_schema(), CaptureSchema::default());
        assert_eq!(glob.capture_schema(), CaptureSchema::default());
    }

    #[test]
    fn regex_exposes_named_and_numbered_captures_to_templates() {
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
        let captures = matcher.captures("model-alpha-12").unwrap();
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
