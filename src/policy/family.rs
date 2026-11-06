//! The family split behind revision one's generated classifiers.
//!
//! A model id splits into the family it belongs to and the version within it.
//! Revision one's classifier and its two matchers (`revision_one_family` and,
//! in the compaction resolver, a family-only model spec) read this split
//! instead of an authored regex, because the split was measured on real
//! traffic: published snapshot aliases fold first, an unpublished dated id has
//! no family, and a name with no version is its own family.

use crate::catalog::windows::model_identity;

/// A model id split into the family it belongs to and the version within
/// it.
///
/// The family is the non-numeric remainder and the version is the trailing
/// numbers, so `claude-opus-4-8` is opus [4, 8] and `claude-opus-5` is opus
/// [5]. Published snapshot aliases fold first (via
/// [`model_identity`]), while an unpublished dated identity has no family
/// and can never become a rewrite target: a future model is not evidence
/// that it inherited an earlier version's anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Family {
    pub name: String,
    pub version: Vec<u64>,
}

impl Family {
    /// Is this a strictly newer version than `other`, within the same family?
    ///
    /// Segment-wise and numeric, because string order gets this wrong twice
    /// over: `claude-opus-5` sorts before `claude-opus-4-8`, and `4-10` sorts
    /// before `4-8`. A missing segment counts as lower, so [5] beats [4, 8]
    /// and [5, 1] beats [5].
    pub fn is_newer_than(&self, other: &Family) -> bool {
        self.name == other.name && self.version > other.version
    }
}

/// An id ending in a dated suffix the alias table does not know: an
/// *unpublished* snapshot. Matched only after alias folding, so a published
/// dated identity like `claude-3-7-sonnet-20250219` folds to its dateless
/// form and keeps its family.
fn is_unpublished_claude_snapshot(model: &str) -> bool {
    match model_identity(model) {
        Some(id) => {
            let bytes = id.as_bytes();
            id.starts_with("claude-")
                && bytes.len() >= 9
                && bytes[bytes.len() - 9] == b'-'
                && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit)
        }
        None => false,
    }
}

/// The family a model belongs to
/// (ported exactly over the normalised identity; unknown stays `None`,
/// never guessed).
pub fn family_of(model: &str) -> Option<Family> {
    if is_unpublished_claude_snapshot(model) {
        return None;
    }
    let id = model_identity(model)?;
    let stripped = id.strip_prefix("claude-").unwrap_or(&id);
    let mut name = Vec::new();
    let mut version = Vec::new();
    for part in stripped.split('-') {
        // Only a run of digits is a version segment.
        if !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) {
            version.push(part.parse::<u64>().unwrap_or(u64::MAX));
        } else {
            name.push(part);
        }
    }
    if name.is_empty() {
        return None;
    }
    Some(Family {
        name: name.join("-"),
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_parse_from_normalised_ids() {
        let family = |model: &str| {
            family_of(model).map(|f| {
                assert!(
                    f.version.iter().all(|v| *v < u64::MAX),
                    "a sane id never saturates"
                );
                (f.name, f.version)
            })
        };
        // The canonical trio, with the segment-wise versions.
        assert_eq!(
            family("claude-opus-4-8"),
            Some(("opus".to_owned(), vec![4, 8]))
        );
        assert_eq!(family("claude-opus-5"), Some(("opus".to_owned(), vec![5])));
        assert_eq!(
            family("claude-sonnet-5"),
            Some(("sonnet".to_owned(), vec![5]))
        );
        assert_eq!(
            family("claude-haiku-4-5"),
            Some(("haiku".to_owned(), vec![4, 5]))
        );
        assert_eq!(
            family("claude-fable-5-1"),
            Some(("fable".to_owned(), vec![5, 1]))
        );
        assert_eq!(
            family("claude-mythos-preview"),
            Some(("mythos-preview".to_owned(), vec![]))
        );
        // The claude- prefix folds; a non-claude id keeps its leading part.
        assert_eq!(
            family("claude-3-5-sonnet"),
            Some(("sonnet".to_owned(), vec![3, 5]))
        );
        assert_eq!(
            family("gpt-5.6-sol"),
            Some(("gpt-5.6-sol".to_owned(), vec![])),
            "5.6 is not a pure digit run: it stays family, not a version segment"
        );
        // Published snapshots fold to their dateless form and keep a family.
        assert_eq!(
            family("claude-3-7-sonnet-20250219"),
            Some(("sonnet".to_owned(), vec![3, 7]))
        );
        // An unpublished dated identity has no family, never a rewrite
        // target: a future model is not evidence about an earlier one.
        assert_eq!(family("claude-opus-4-9-20261225"), None);
        // Unknown stays unknown, never guessed (invariant 3).
        assert_eq!(family(""), None);
        assert_eq!(family("   "), None, "blank normalises to no identity");
    }

    #[test]
    fn newer_is_segmentwise_and_numeric_never_stringwise() {
        let newer_than = |a: &str, b: &str| match (family_of(a), family_of(b)) {
            (Some(a), Some(b)) => a.is_newer_than(&b),
            _ => false,
        };
        // String order gets this wrong: "claude-opus-5" < "claude-opus-4-8".
        assert!(newer_than("claude-opus-5", "claude-opus-4-8"));
        assert!(!newer_than("claude-opus-4-8", "claude-opus-5"));
        // "4-10" must beat "4-8" numerically.
        assert!(newer_than("claude-opus-4-10", "claude-opus-4-8"));
        // A missing segment counts as lower: [5, 1] beats [5].
        assert!(newer_than("claude-opus-5-1", "claude-opus-5"));
        // Equal is not newer, and a family never crosses.
        assert!(!newer_than("claude-opus-5", "claude-opus-5"));
        assert!(!newer_than("claude-opus-5", "claude-sonnet-5"));
        assert!(!newer_than("no-family-at-all", "claude-opus-5"));
    }
}
