//! Reading a matcher as an operator writes it, and finding what an earlier
//! record already catches of a later one (spec: UTUI#reading-a-table).
//!
//! A document holds three matcher kinds, and a prefix is not one of them: it
//! is a regular expression of a shape that is written in its own notation
//! (`anthropic_sub/${model}`). [`Shape`] recognises that shape so a
//! prefix and an exact name can be compared by what they say. A glob or a
//! regular expression cannot, so those are compared against the names a client
//! could send, which are the offers the catalogues list.

use std::collections::{BTreeMap, BTreeSet};

use super::{CompiledMatcher, MatcherSpec};

/// A matcher in the notation it is shown in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    Exact(String),
    /// Everything starting with `prefix`, with the rest captured as
    /// `capture` where it is named. `min` is the fewest characters the rest
    /// must hold: one for `.+`, none for `.*`.
    Prefix {
        prefix: String,
        capture: Option<String>,
        min: usize,
    },
    Glob(String),
    Regex(String),
}

impl Shape {
    pub fn of(spec: &MatcherSpec) -> Self {
        match spec {
            MatcherSpec::Exact(value) => Self::Exact(value.clone()),
            MatcherSpec::Glob(value) => Self::Glob(value.clone()),
            MatcherSpec::Regex(value) => {
                prefix_of(value).unwrap_or_else(|| Self::Regex(value.clone()))
            }
        }
    }

    /// The word for the kind: what the operator reads as the matcher's type.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Exact(_) => "exact",
            Self::Prefix { .. } => "prefix",
            Self::Glob(_) => "glob",
            Self::Regex(_) => "regex",
        }
    }

    /// The matcher written out: a prefix with the part it captures, rather
    /// than the regular expression it compiles to.
    pub fn text(&self) -> String {
        match self {
            Self::Exact(value) | Self::Glob(value) | Self::Regex(value) => value.clone(),
            Self::Prefix {
                prefix, capture, ..
            } => match capture {
                Some(name) => format!("{prefix}${{{name}}}"),
                None => format!("{prefix}…"),
            },
        }
    }
}

/// `^LITERAL(?P<name>.+)$`, the shape a provider prefix is written in, with
/// `.*` and an unnamed group accepted too. The literal is read back through
/// regex escaping, so `anthropic\.sub/` is `anthropic.sub/`.
fn prefix_of(pattern: &str) -> Option<Shape> {
    let body = pattern.strip_prefix('^')?.strip_suffix('$')?;
    let mut prefix = String::new();
    let mut characters = body.char_indices();
    while let Some((at, character)) = characters.next() {
        match character {
            '\\' => {
                let (_, escaped) = characters.next()?;
                // Only punctuation is escaped to itself; `\d` and its kin
                // are classes, not literals.
                if escaped.is_ascii_alphanumeric() {
                    return None;
                }
                prefix.push(escaped);
            }
            '(' => {
                let group = &body[at..];
                let (capture, rest) = if let Some(named) = group
                    .strip_prefix("(?P<")
                    .or_else(|| group.strip_prefix("(?<"))
                {
                    let (name, rest) = named.split_once('>')?;
                    if name.is_empty()
                        || !name
                            .chars()
                            .all(|character| character.is_ascii_alphanumeric() || character == '_')
                    {
                        return None;
                    }
                    (Some(name.to_owned()), rest)
                } else {
                    (None, &group[1..])
                };
                let min = match rest {
                    ".+)" => 1,
                    ".*)" => 0,
                    _ => return None,
                };
                return Some(Shape::Prefix {
                    prefix,
                    capture,
                    min,
                });
            }
            '.' | '*' | '+' | '?' | '[' | ']' | '{' | '}' | '|' | ')' | '^' | '$' => return None,
            literal => prefix.push(literal),
        }
    }
    None
}

/// How much of a later matcher's catch an earlier one takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catches {
    /// Everything the later matcher would catch.
    All,
    /// Some of it.
    Part,
}

impl Catches {
    pub fn code(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Part => "part",
        }
    }
}

/// How the comparison was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Basis {
    /// Exact and prefix matchers are compared by what they say.
    Comparison,
    /// A glob or a regular expression is tried against the names a client
    /// could send. `compared` is how many of those the later matcher catches;
    /// `examples` are some the earlier one catches as well.
    Names {
        compared: usize,
        examples: Vec<String>,
    },
}

/// What an earlier record catches.
pub enum Earlier<'a> {
    /// A matcher.
    Matcher(&'a MatcherSpec),
    /// A closed set of names, as a presentation's slugs are.
    Names(&'a BTreeSet<String>),
}

const EXAMPLES: usize = 3;

/// Matchers prepared once for comparison against the names a client could
/// send.
///
/// Each distinct matcher is compiled once and run over the names once, so
/// comparing any two of them is a set intersection, however many pairs a
/// table holds.
///
/// `names` are the ones a glob or a regular expression is judged against:
/// none of them is not a refusal to compare but an absence of anything to
/// compare, and reads as catching nothing.
pub struct Shadowing<'a> {
    names: &'a BTreeSet<String>,
    prepared: BTreeMap<(u8, &'a str), Prepared<'a>>,
}

/// One matcher, compiled, with the names it catches.
struct Prepared<'a> {
    /// Absent where the matcher does not compile, which compares as catching
    /// nothing.
    compiled: Option<CompiledMatcher>,
    caught: BTreeSet<&'a str>,
}

impl<'a> Prepared<'a> {
    fn new(spec: &'a MatcherSpec, names: &'a BTreeSet<String>) -> Self {
        let compiled = spec.compile().ok();
        let mut caught = BTreeSet::new();
        if let Some(compiled) = &compiled {
            let mut judge = |name: &'a str| {
                if compiled.is_match(name).unwrap_or(false) {
                    caught.insert(name);
                }
            };
            names.iter().map(String::as_str).for_each(&mut judge);
            // A matcher naming one thing is judged on that thing even where
            // no catalogue lists it.
            if let MatcherSpec::Exact(name) = spec {
                judge(name);
            }
        }
        Self { compiled, caught }
    }
}

enum Held<'s, 'a> {
    Shared(&'s Prepared<'a>),
    Own(Prepared<'a>),
}

impl<'a> std::ops::Deref for Held<'_, 'a> {
    type Target = Prepared<'a>;

    fn deref(&self) -> &Prepared<'a> {
        match self {
            Self::Shared(prepared) => prepared,
            Self::Own(prepared) => prepared,
        }
    }
}

fn key(spec: &MatcherSpec) -> (u8, &str) {
    match spec {
        MatcherSpec::Exact(value) => (0, value),
        MatcherSpec::Glob(value) => (1, value),
        MatcherSpec::Regex(value) => (2, value),
    }
}

impl<'a> Shadowing<'a> {
    /// Prepare every matcher that will be compared.
    pub fn new(
        names: &'a BTreeSet<String>,
        specs: impl IntoIterator<Item = &'a MatcherSpec>,
    ) -> Self {
        let mut prepared = BTreeMap::new();
        for spec in specs {
            prepared
                .entry(key(spec))
                .or_insert_with(|| Prepared::new(spec, names));
        }
        Self { names, prepared }
    }

    /// The matcher as prepared, or prepared now where it was not named up
    /// front.
    fn prepared(&self, spec: &'a MatcherSpec) -> Held<'_, 'a> {
        match self.prepared.get(&key(spec)) {
            Some(prepared) => Held::Shared(prepared),
            None => Held::Own(Prepared::new(spec, self.names)),
        }
    }

    /// What `earlier` catches of what `later` matches, where it catches any.
    pub fn compare(
        &self,
        earlier: &Earlier<'a>,
        later: &'a MatcherSpec,
    ) -> Option<(Catches, Basis)> {
        let later_shape = Shape::of(later);
        match (earlier, &later_shape) {
            (Earlier::Matcher(earlier), Shape::Exact(_) | Shape::Prefix { .. })
                if matches!(Shape::of(earlier), Shape::Exact(_) | Shape::Prefix { .. }) =>
            {
                structural(&Shape::of(earlier), &later_shape)
                    .map(|catches| (catches, Basis::Comparison))
            }
            (Earlier::Names(set), Shape::Exact(name)) => set
                .contains(name)
                .then_some((Catches::All, Basis::Comparison)),
            (Earlier::Names(set), _) => {
                // A closed set cannot cover a matcher that accepts unboundedly
                // many names, so it catches part where it catches any.
                let later = self.prepared(later);
                let later = later.compiled.as_ref()?;
                let caught: Vec<String> = set
                    .iter()
                    .filter(|name| later.is_match(name).unwrap_or(false))
                    .cloned()
                    .collect();
                (!caught.is_empty()).then(|| {
                    (
                        Catches::Part,
                        Basis::Names {
                            compared: caught.len(),
                            examples: caught.into_iter().take(EXAMPLES).collect(),
                        },
                    )
                })
            }
            (Earlier::Matcher(earlier), _) => {
                let (earlier, later) = (self.prepared(earlier), self.prepared(later));
                if earlier.compiled.is_none() || later.compiled.is_none() {
                    return None;
                }
                let compared = later.caught.len();
                let overlap: Vec<&str> = later
                    .caught
                    .intersection(&earlier.caught)
                    .copied()
                    .collect();
                if overlap.is_empty() {
                    return None;
                }
                let catches = if overlap.len() == compared {
                    Catches::All
                } else {
                    Catches::Part
                };
                Some((
                    catches,
                    Basis::Names {
                        compared,
                        examples: overlap
                            .into_iter()
                            .take(EXAMPLES)
                            .map(str::to_owned)
                            .collect(),
                    },
                ))
            }
        }
    }
}

/// What `earlier` catches of what `later` matches, where it catches any.
///
/// For one comparison; a table of them prepares its matchers once with
/// [`Shadowing`].
pub fn compare(
    earlier: &Earlier<'_>,
    later: &MatcherSpec,
    names: &BTreeSet<String>,
) -> Option<(Catches, Basis)> {
    let specs = match earlier {
        Earlier::Matcher(earlier) => vec![*earlier, later],
        Earlier::Names(_) => vec![later],
    };
    Shadowing::new(names, specs).compare(earlier, later)
}

/// Compare two matchers each an exact name or a prefix.
fn structural(earlier: &Shape, later: &Shape) -> Option<Catches> {
    match (earlier, later) {
        (Shape::Exact(earlier), Shape::Exact(later)) => (earlier == later).then_some(Catches::All),
        (Shape::Exact(name), Shape::Prefix { prefix, min, .. }) => {
            (name.starts_with(prefix) && name.len() - prefix.len() >= *min).then_some(Catches::Part)
        }
        (
            Shape::Prefix {
                prefix, min: need, ..
            },
            Shape::Exact(name),
        ) => {
            (name.starts_with(prefix) && name.len() - prefix.len() >= *need).then_some(Catches::All)
        }
        (
            Shape::Prefix {
                prefix: earlier_prefix,
                min: earlier_min,
                ..
            },
            Shape::Prefix {
                prefix: later_prefix,
                min: later_min,
                ..
            },
        ) => {
            if let Some(rest) = later_prefix.strip_prefix(earlier_prefix.as_str()) {
                // Every name the later one catches starts with the earlier
                // prefix, and holds at least this much after it.
                if rest.len() + later_min >= *earlier_min {
                    Some(Catches::All)
                } else {
                    Some(Catches::Part)
                }
            } else if earlier_prefix.starts_with(later_prefix.as_str()) {
                // The earlier one catches the later one's names that go on to
                // spell its longer prefix.
                Some(Catches::Part)
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regex(value: &str) -> MatcherSpec {
        MatcherSpec::Regex(value.to_owned())
    }

    fn prefix(value: &str) -> MatcherSpec {
        regex(&format!("^{}(?P<model>.+)$", regex::escape(value)))
    }

    fn names(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn catches(
        earlier: &MatcherSpec,
        later: &MatcherSpec,
        names: &BTreeSet<String>,
    ) -> Option<Catches> {
        compare(&Earlier::Matcher(earlier), later, names).map(|(catches, _)| catches)
    }

    #[test]
    fn a_provider_prefix_reads_as_a_prefix_with_its_capture() {
        let shape = Shape::of(&prefix("anthropic_sub/"));
        assert_eq!(shape.kind(), "prefix");
        assert_eq!(shape.text(), "anthropic_sub/${model}");
        // The literal comes back through regex escaping.
        let dotted = Shape::of(&prefix("vendor.v2/"));
        assert_eq!(dotted.text(), "vendor.v2/${model}");
    }

    #[test]
    fn only_a_literal_then_one_open_group_is_a_prefix() {
        for pattern in [
            r"^claude-(?P<family>[a-z]+)-(?P<version>[0-9]+)$",
            r"^a|b(?P<model>.+)$",
            r"^abc$",
            r"^\d+(?P<model>.+)$",
            r"^abc(?P<model>.+)x$",
            r"abc(?P<model>.+)$",
            r"^a.b(?P<model>.+)$",
        ] {
            assert_eq!(Shape::of(&regex(pattern)).kind(), "regex", "{pattern}");
        }
        assert_eq!(Shape::of(&regex(r"^abc(.*)$")).kind(), "prefix");
        assert_eq!(Shape::of(&regex(r"^abc(.*)$")).text(), "abc…");
    }

    #[test]
    fn an_exact_name_is_caught_whole_by_an_equal_one_and_by_a_prefix_over_it() {
        let none = names(&[]);
        let exact = MatcherSpec::Exact("openrouter/auto".to_owned());
        assert_eq!(catches(&exact, &exact, &none), Some(Catches::All));
        assert_eq!(
            catches(&prefix("openrouter/"), &exact, &none),
            Some(Catches::All)
        );
        assert_eq!(catches(&prefix("codex_sub/"), &exact, &none), None);
        // A prefix whose rest is required cannot catch the bare prefix.
        let bare = MatcherSpec::Exact("openrouter/".to_owned());
        assert_eq!(catches(&prefix("openrouter/"), &bare, &none), None);
    }

    #[test]
    fn prefixes_are_compared_by_which_starts_with_which() {
        let none = names(&[]);
        let outer = prefix("openrouter/");
        let inner = prefix("openrouter/anthropic/");
        // The wider earlier one takes all of the narrower later one.
        assert_eq!(catches(&outer, &inner, &none), Some(Catches::All));
        // The narrower earlier one takes part of the wider later one.
        assert_eq!(catches(&inner, &outer, &none), Some(Catches::Part));
        assert_eq!(catches(&outer, &outer, &none), Some(Catches::All));
        assert_eq!(catches(&prefix("codex_sub/"), &outer, &none), None);
        // An exact name inside a prefix is part of it.
        let exact = MatcherSpec::Exact("openrouter/auto".to_owned());
        assert_eq!(catches(&exact, &outer, &none), Some(Catches::Part));
        // Same prefix, but the earlier one needs a character the later one
        // does not.
        let anything = regex("^openrouter/(.*)$");
        assert_eq!(catches(&outer, &anything, &none), Some(Catches::Part));
        assert_eq!(catches(&anything, &outer, &none), Some(Catches::All));
    }

    #[test]
    fn a_glob_is_judged_against_the_names_it_would_catch() {
        let offers = names(&[
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-sonnet-4-5",
            "gpt-5",
        ]);
        let earlier = MatcherSpec::Glob("claude-sonnet-*".to_owned());
        let later = MatcherSpec::Glob("claude-*".to_owned());
        let (catches, basis) = compare(&Earlier::Matcher(&earlier), &later, &offers).unwrap();
        assert_eq!(catches, Catches::Part);
        assert_eq!(
            basis,
            Basis::Names {
                compared: 3,
                examples: vec!["claude-sonnet-4-5".to_owned(), "claude-sonnet-5".to_owned()],
            }
        );
        // The wider one takes everything the narrower one would.
        let (catches, _) = compare(&Earlier::Matcher(&later), &earlier, &offers).unwrap();
        assert_eq!(catches, Catches::All);
        // Nothing in common.
        let other = MatcherSpec::Glob("gpt-*".to_owned());
        assert!(compare(&Earlier::Matcher(&other), &earlier, &offers).is_none());
    }

    #[test]
    fn a_regex_earlier_than_a_prefix_is_judged_against_the_names_too() {
        let offers = names(&["openrouter/a", "openrouter/b", "codex_sub/c"]);
        let earlier = regex(r"^openrouter/a$");
        let (catches, basis) =
            compare(&Earlier::Matcher(&earlier), &prefix("openrouter/"), &offers).unwrap();
        assert_eq!(catches, Catches::Part);
        assert!(matches!(basis, Basis::Names { compared: 2, .. }));
    }

    #[test]
    fn nothing_to_compare_against_catches_nothing() {
        let earlier = MatcherSpec::Glob("claude-*".to_owned());
        let later = MatcherSpec::Glob("claude-opus-*".to_owned());
        assert!(compare(&Earlier::Matcher(&earlier), &later, &names(&[])).is_none());
    }

    #[test]
    fn a_closed_set_of_names_catches_part_of_a_matcher_and_all_of_an_equal_name() {
        let slugs = names(&["openrouter/auto", "openrouter/other"]);
        let (catches, basis) =
            compare(&Earlier::Names(&slugs), &prefix("openrouter/"), &names(&[])).unwrap();
        assert_eq!(catches, Catches::Part);
        assert!(matches!(basis, Basis::Names { compared: 2, .. }));
        let exact = MatcherSpec::Exact("openrouter/auto".to_owned());
        let (catches, basis) = compare(&Earlier::Names(&slugs), &exact, &names(&[])).unwrap();
        assert_eq!((catches, basis), (Catches::All, Basis::Comparison));
        let elsewhere = MatcherSpec::Exact("codex_sub/x".to_owned());
        assert!(compare(&Earlier::Names(&slugs), &elsewhere, &names(&[])).is_none());
    }

    #[test]
    fn a_table_prepared_once_judges_each_pair_as_a_lone_comparison_does() {
        let known = names(&["gpt-5", "gpt-5-mini", "claude-opus", "o/claude-opus"]);
        let specs = [
            MatcherSpec::Glob("gpt-*".to_owned()),
            regex("^gpt-5.*$"),
            regex("claude"),
            MatcherSpec::Exact("claude-opus".to_owned()),
            MatcherSpec::Exact("unlisted".to_owned()),
            regex("("),
        ];
        let shadowing = Shadowing::new(&known, specs.iter());
        let mut caught = 0;
        for earlier in &specs {
            for later in &specs {
                let lone = compare(&Earlier::Matcher(earlier), later, &known);
                caught += usize::from(lone.is_some());
                assert_eq!(
                    shadowing.compare(&Earlier::Matcher(earlier), later),
                    lone,
                    "{earlier:?} over {later:?}"
                );
            }
        }
        assert!(caught > 0, "the fixture should shadow something");
        assert_eq!(
            shadowing.compare(&Earlier::Matcher(&specs[0]), &specs[1]),
            Some((
                Catches::All,
                Basis::Names {
                    compared: 2,
                    examples: vec!["gpt-5".to_owned(), "gpt-5-mini".to_owned()],
                }
            ))
        );
        // A matcher that does not compile catches and is caught by nothing.
        assert_eq!(
            shadowing.compare(&Earlier::Matcher(&specs[5]), &specs[0]),
            None
        );
    }
}
