//! Typed, identifier-addressed edits for policy documents.
//!
//! This deliberately is not JSON Patch.  An edit names records by their stable
//! ids and carries a typed replacement, so callers cannot mutate an incidental
//! JSON path or leave a partially edited document behind.  Applying an edit is
//! pure: it clones the supplied document, changes the clone, then validates the
//! complete result.

use std::fmt;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use super::{
    Candidate, CandidateEdge, CandidateId, CompactionPolicy, DefaultRoute, DocumentError,
    FamilyClassifier, FrontendScope, PolicyDocument, Presentation, PresentationId, RewriteId,
    RewriteRule, Route, RouteId, RouteStep, UpgradePolicy, UpgradePolicyId,
    ValidatedPolicyDocument,
};

/// The largest batch accepted by the typed edit surface.
///
/// The bound prevents a control request from turning one edit into an
/// unbounded amount of work.  The policy document's own collection bounds are
/// still checked after every batch.
pub const MAX_POLICY_EDIT_OPERATIONS: usize = 128;

/// A bounded, atomic sequence of typed policy mutations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyEdit {
    #[serde(deserialize_with = "deserialize_operations")]
    pub operations: Vec<PolicyEditOperation>,
}

impl PolicyEdit {
    /// Applies this edit to an editable document and validates the result.
    ///
    /// The input is never changed, including when a later operation fails.
    pub fn apply(&self, document: &PolicyDocument) -> Result<PolicyDocument, PolicyEditError> {
        self.check_bound()?;
        let mut edited = document.clone();
        for operation in &self.operations {
            operation.apply(&mut edited)?;
        }
        edited.validate()?;
        Ok(edited)
    }

    /// Applies this edit to a validated document and returns another validated
    /// document.
    pub fn apply_validated(
        &self,
        document: &ValidatedPolicyDocument,
    ) -> Result<ValidatedPolicyDocument, PolicyEditError> {
        self.apply(document.document())?
            .validate()
            .map_err(PolicyEditError::from)
    }

    fn check_bound(&self) -> Result<(), PolicyEditError> {
        if self.operations.len() > MAX_POLICY_EDIT_OPERATIONS {
            return Err(PolicyEditError::TooManyOperations {
                limit: MAX_POLICY_EDIT_OPERATIONS,
            });
        }
        Ok(())
    }
}

/// One typed, high-level policy mutation.
///
/// Replacements preserve their collection position and must retain the record
/// id they address.  Renaming is intentionally expressed as remove then add,
/// which makes changes to stable identities explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PolicyEditOperation {
    SetDescription {
        description: Option<String>,
    },

    AddDefault {
        default: DefaultRoute,
    },
    ReplaceDefault {
        scope: FrontendScope,
        default: DefaultRoute,
    },
    RemoveDefault {
        scope: FrontendScope,
    },

    AddPresentation {
        presentation: Presentation,
    },
    ReplacePresentation {
        id: PresentationId,
        presentation: Presentation,
    },
    RemovePresentation {
        id: PresentationId,
    },

    AddRoute {
        route: Route,
    },
    ReplaceRoute {
        id: RouteId,
        route: Route,
    },
    RemoveRoute {
        id: RouteId,
    },

    AddRewrite {
        rewrite: RewriteRule,
    },
    ReplaceRewrite {
        id: RewriteId,
        rewrite: RewriteRule,
    },
    RemoveRewrite {
        id: RewriteId,
    },

    AddClassifier {
        classifier: FamilyClassifier,
    },
    ReplaceClassifier {
        id: super::ClassifierId,
        classifier: FamilyClassifier,
    },
    RemoveClassifier {
        id: super::ClassifierId,
    },

    AddUpgrade {
        upgrade: UpgradePolicy,
    },
    ReplaceUpgrade {
        id: UpgradePolicyId,
        upgrade: UpgradePolicy,
    },
    RemoveUpgrade {
        id: UpgradePolicyId,
    },

    AddCompaction {
        compaction: CompactionPolicy,
    },
    ReplaceCompaction {
        compaction: CompactionPolicy,
    },
    RemoveCompaction,

    /// Inserts a fallback candidate immediately after an existing candidate.
    AddCandidate {
        route_id: RouteId,
        after: CandidateId,
        edge: CandidateEdge,
        candidate: Candidate,
    },
    /// Replaces a candidate without changing its position or incoming edge.
    ReplaceCandidate {
        route_id: RouteId,
        candidate_id: CandidateId,
        candidate: Candidate,
    },
    /// Removes a candidate and its incoming edge.
    ///
    /// Removing the first candidate promotes the next candidate and discards
    /// that candidate's now-irrelevant incoming edge.
    RemoveCandidate {
        route_id: RouteId,
        candidate_id: CandidateId,
    },
    /// Moves a candidate while making any newly required transition explicit.
    MoveCandidate {
        route_id: RouteId,
        candidate_id: CandidateId,
        destination: CandidateMoveDestination,
    },
    /// Sets whether a route requires server safeguards of its candidates for a
    /// request that asks for them. Honouring them needs format version 4, so
    /// setting the flag raises an older document to it.
    SetRouteServerSafeguards {
        route_id: RouteId,
        honours: bool,
    },
}

/// A candidate's new position in a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CandidateMoveDestination {
    /// Make the candidate primary.
    ///
    /// The previous primary becomes the first fallback and needs this explicit
    /// edge.
    First { edge: CandidateEdge },
    /// Put the candidate immediately after another candidate.
    After {
        candidate_id: CandidateId,
        edge: CandidateEdge,
    },
}

impl PolicyEditOperation {
    fn apply(&self, document: &mut PolicyDocument) -> Result<(), PolicyEditError> {
        match self {
            Self::SetDescription { description } => {
                document.description.clone_from(description);
            }

            Self::AddDefault { default } => {
                if document
                    .defaults
                    .iter()
                    .any(|item| item.scope == default.scope)
                {
                    return Err(PolicyEditError::DefaultAlreadyExists);
                }
                document.defaults.push(default.clone());
            }
            Self::ReplaceDefault { scope, default } => {
                if &default.scope != scope {
                    return Err(PolicyEditError::DefaultScopeMismatch);
                }
                let item = document
                    .defaults
                    .iter_mut()
                    .find(|item| item.scope == *scope)
                    .ok_or(PolicyEditError::DefaultNotFound)?;
                *item = default.clone();
            }
            Self::RemoveDefault { scope } => {
                let index = document
                    .defaults
                    .iter()
                    .position(|item| item.scope == *scope)
                    .ok_or(PolicyEditError::DefaultNotFound)?;
                document.defaults.remove(index);
            }

            Self::AddPresentation { presentation } => {
                add_record(
                    &mut document.presentations,
                    presentation,
                    "presentation",
                    |item| &item.id,
                )?;
            }
            Self::ReplacePresentation { id, presentation } => {
                replace_record(
                    &mut document.presentations,
                    id,
                    presentation,
                    "presentation",
                    |item| &item.id,
                )?;
            }
            Self::RemovePresentation { id } => {
                remove_record(&mut document.presentations, id, "presentation", |item| {
                    &item.id
                })?;
            }

            Self::AddRoute { route } => {
                add_record(&mut document.routes, route, "route", |item| &item.id)?;
            }
            Self::ReplaceRoute { id, route } => {
                replace_record(&mut document.routes, id, route, "route", |item| &item.id)?;
            }
            Self::RemoveRoute { id } => {
                remove_record(&mut document.routes, id, "route", |item| &item.id)?;
            }

            Self::AddRewrite { rewrite } => {
                add_record(&mut document.rewrites, rewrite, "rewrite", |item| &item.id)?;
            }
            Self::ReplaceRewrite { id, rewrite } => {
                replace_record(&mut document.rewrites, id, rewrite, "rewrite", |item| {
                    &item.id
                })?;
            }
            Self::RemoveRewrite { id } => {
                remove_record(&mut document.rewrites, id, "rewrite", |item| &item.id)?;
            }

            Self::AddClassifier { classifier } => {
                add_record(
                    &mut document.classifiers,
                    classifier,
                    "classifier",
                    |item| &item.id,
                )?;
            }
            Self::ReplaceClassifier { id, classifier } => {
                replace_record(
                    &mut document.classifiers,
                    id,
                    classifier,
                    "classifier",
                    |item| &item.id,
                )?;
            }
            Self::RemoveClassifier { id } => {
                remove_record(&mut document.classifiers, id, "classifier", |item| &item.id)?;
            }

            Self::AddUpgrade { upgrade } => {
                add_record(&mut document.upgrades, upgrade, "upgrade", |item| &item.id)?;
            }
            Self::ReplaceUpgrade { id, upgrade } => {
                replace_record(&mut document.upgrades, id, upgrade, "upgrade", |item| {
                    &item.id
                })?;
            }
            Self::RemoveUpgrade { id } => {
                remove_record(&mut document.upgrades, id, "upgrade", |item| &item.id)?;
            }

            Self::AddCompaction { compaction } => {
                if document.compaction.is_some() {
                    return Err(PolicyEditError::CompactionAlreadyExists);
                }
                document.compaction = Some(compaction.clone());
            }
            Self::ReplaceCompaction { compaction } => {
                if document.compaction.is_none() {
                    return Err(PolicyEditError::CompactionNotFound);
                }
                document.compaction = Some(compaction.clone());
            }
            Self::RemoveCompaction => {
                document
                    .compaction
                    .take()
                    .ok_or(PolicyEditError::CompactionNotFound)?;
            }

            Self::AddCandidate {
                route_id,
                after,
                edge,
                candidate,
            } => {
                let route = find_route_mut(document, route_id)?;
                if candidate_index(route, &candidate.id).is_some() {
                    return Err(PolicyEditError::RecordAlreadyExists {
                        record: "candidate",
                        id: candidate.id.to_string(),
                    });
                }
                let index = candidate_index(route, after).ok_or_else(|| {
                    PolicyEditError::CandidateNotFound {
                        route_id: route_id.to_string(),
                        candidate_id: after.to_string(),
                    }
                })?;
                route.chain.steps.insert(
                    index,
                    RouteStep {
                        edge: edge.clone(),
                        candidate: candidate.clone(),
                    },
                );
            }
            Self::ReplaceCandidate {
                route_id,
                candidate_id,
                candidate,
            } => {
                if candidate.id != *candidate_id {
                    return Err(PolicyEditError::ReplacementIdMismatch {
                        record: "candidate",
                        expected: candidate_id.to_string(),
                        actual: candidate.id.to_string(),
                    });
                }
                let route = find_route_mut(document, route_id)?;
                let index = candidate_index(route, candidate_id).ok_or_else(|| {
                    PolicyEditError::CandidateNotFound {
                        route_id: route_id.to_string(),
                        candidate_id: candidate_id.to_string(),
                    }
                })?;
                if index == 0 {
                    route.chain.first = candidate.clone();
                } else {
                    route.chain.steps[index - 1].candidate = candidate.clone();
                }
            }
            Self::RemoveCandidate {
                route_id,
                candidate_id,
            } => {
                let route = find_route_mut(document, route_id)?;
                let index = candidate_index(route, candidate_id).ok_or_else(|| {
                    PolicyEditError::CandidateNotFound {
                        route_id: route_id.to_string(),
                        candidate_id: candidate_id.to_string(),
                    }
                })?;
                remove_candidate(route, index, route_id)?;
            }
            Self::MoveCandidate {
                route_id,
                candidate_id,
                destination,
            } => {
                let route = find_route_mut(document, route_id)?;
                move_candidate(route, route_id, candidate_id, destination)?;
            }
            Self::SetRouteServerSafeguards { route_id, honours } => {
                find_route_mut(document, route_id)?.honours_server_safeguards = *honours;
                if *honours {
                    document.format_version = document.format_version.max(4);
                }
            }
        }
        Ok(())
    }
}

fn add_record<T, Id>(
    records: &mut Vec<T>,
    replacement: &T,
    record: &'static str,
    id: impl Fn(&T) -> &Id,
) -> Result<(), PolicyEditError>
where
    T: Clone,
    Id: Eq + fmt::Display,
{
    if records.iter().any(|item| id(item) == id(replacement)) {
        return Err(PolicyEditError::RecordAlreadyExists {
            record,
            id: id(replacement).to_string(),
        });
    }
    records.push(replacement.clone());
    Ok(())
}

fn replace_record<T, Id>(
    records: &mut [T],
    target: &Id,
    replacement: &T,
    record: &'static str,
    id: impl Fn(&T) -> &Id,
) -> Result<(), PolicyEditError>
where
    T: Clone,
    Id: Eq + fmt::Display,
{
    if id(replacement) != target {
        return Err(PolicyEditError::ReplacementIdMismatch {
            record,
            expected: target.to_string(),
            actual: id(replacement).to_string(),
        });
    }
    let item = records
        .iter_mut()
        .find(|item| id(item) == target)
        .ok_or_else(|| PolicyEditError::RecordNotFound {
            record,
            id: target.to_string(),
        })?;
    *item = replacement.clone();
    Ok(())
}

fn remove_record<T, Id>(
    records: &mut Vec<T>,
    target: &Id,
    record: &'static str,
    id: impl Fn(&T) -> &Id,
) -> Result<(), PolicyEditError>
where
    Id: Eq + fmt::Display,
{
    let index = records
        .iter()
        .position(|item| id(item) == target)
        .ok_or_else(|| PolicyEditError::RecordNotFound {
            record,
            id: target.to_string(),
        })?;
    records.remove(index);
    Ok(())
}

fn find_route_mut<'a>(
    document: &'a mut PolicyDocument,
    route_id: &RouteId,
) -> Result<&'a mut Route, PolicyEditError> {
    document
        .routes
        .iter_mut()
        .find(|route| route.id == *route_id)
        .ok_or_else(|| PolicyEditError::RecordNotFound {
            record: "route",
            id: route_id.to_string(),
        })
}

fn candidate_index(route: &Route, candidate_id: &CandidateId) -> Option<usize> {
    route
        .chain
        .candidates()
        .position(|candidate| candidate.id == *candidate_id)
}

fn remove_candidate(
    route: &mut Route,
    index: usize,
    route_id: &RouteId,
) -> Result<Candidate, PolicyEditError> {
    if index == 0 {
        let step = route.chain.steps.first().ok_or_else(|| {
            PolicyEditError::CannotRemoveFinalCandidate {
                route_id: route_id.to_string(),
            }
        })?;
        let promoted = step.candidate.clone();
        route.chain.steps.remove(0);
        Ok(std::mem::replace(&mut route.chain.first, promoted))
    } else {
        Ok(route.chain.steps.remove(index - 1).candidate)
    }
}

fn move_candidate(
    route: &mut Route,
    route_id: &RouteId,
    candidate_id: &CandidateId,
    destination: &CandidateMoveDestination,
) -> Result<(), PolicyEditError> {
    let index =
        candidate_index(route, candidate_id).ok_or_else(|| PolicyEditError::CandidateNotFound {
            route_id: route_id.to_string(),
            candidate_id: candidate_id.to_string(),
        })?;

    match destination {
        CandidateMoveDestination::First { edge } => {
            if index == 0 {
                return Err(PolicyEditError::CandidateAlreadyFirst {
                    route_id: route_id.to_string(),
                    candidate_id: candidate_id.to_string(),
                });
            }
            let candidate = remove_candidate(route, index, route_id)?;
            let former_first = std::mem::replace(&mut route.chain.first, candidate);
            route.chain.steps.insert(
                0,
                RouteStep {
                    edge: edge.clone(),
                    candidate: former_first,
                },
            );
        }
        CandidateMoveDestination::After {
            candidate_id: after,
            edge,
        } => {
            if after == candidate_id {
                return Err(PolicyEditError::CandidateCannotFollowItself {
                    route_id: route_id.to_string(),
                    candidate_id: candidate_id.to_string(),
                });
            }
            let candidate = remove_candidate(route, index, route_id)?;
            let after_index = candidate_index(route, after).ok_or_else(|| {
                PolicyEditError::CandidateNotFound {
                    route_id: route_id.to_string(),
                    candidate_id: after.to_string(),
                }
            })?;
            route.chain.steps.insert(
                after_index,
                RouteStep {
                    edge: edge.clone(),
                    candidate,
                },
            );
        }
    }
    Ok(())
}

fn deserialize_operations<'de, D>(deserializer: D) -> Result<Vec<PolicyEditOperation>, D::Error>
where
    D: Deserializer<'de>,
{
    struct OperationsVisitor;

    impl<'de> Visitor<'de> for OperationsVisitor {
        type Value = Vec<PolicyEditOperation>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "at most {MAX_POLICY_EDIT_OPERATIONS} typed policy edit operations"
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut operations = Vec::new();
            while let Some(operation) = sequence.next_element()? {
                if operations.len() == MAX_POLICY_EDIT_OPERATIONS {
                    return Err(de::Error::invalid_length(
                        MAX_POLICY_EDIT_OPERATIONS + 1,
                        &self,
                    ));
                }
                operations.push(operation);
            }
            Ok(operations)
        }
    }

    deserializer.deserialize_seq(OperationsVisitor)
}

/// A typed edit could not be applied.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PolicyEditError {
    #[error("policy edit has more than {limit} operations")]
    TooManyOperations { limit: usize },
    #[error("{record} record {id} already exists")]
    RecordAlreadyExists { record: &'static str, id: String },
    #[error("{record} record {id} does not exist")]
    RecordNotFound { record: &'static str, id: String },
    #[error("replacement {record} id must remain {expected}, not {actual}")]
    ReplacementIdMismatch {
        record: &'static str,
        expected: String,
        actual: String,
    },
    #[error("a default route for that scope already exists")]
    DefaultAlreadyExists,
    #[error("a default route for that scope does not exist")]
    DefaultNotFound,
    #[error("replacement default route must retain the addressed scope")]
    DefaultScopeMismatch,
    #[error("a compaction policy already exists")]
    CompactionAlreadyExists,
    #[error("no compaction policy exists")]
    CompactionNotFound,
    #[error("candidate {candidate_id} does not exist in route {route_id}")]
    CandidateNotFound {
        route_id: String,
        candidate_id: String,
    },
    #[error("route {route_id} must retain at least one candidate")]
    CannotRemoveFinalCandidate { route_id: String },
    #[error("candidate {candidate_id} is already first in route {route_id}")]
    CandidateAlreadyFirst {
        route_id: String,
        candidate_id: String,
    },
    #[error("candidate {candidate_id} cannot follow itself in route {route_id}")]
    CandidateCannotFollowItself {
        route_id: String,
        candidate_id: String,
    },
    #[error(transparent)]
    InvalidDocument(#[from] DocumentError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{
        BindingId, CURRENT_FORMAT_VERSION, CacheTransitionPolicy, CandidateModel, CapabilityFlags,
        FallbackPermission, LaneTransitionPolicy,
    };

    fn candidate(id: &str) -> Candidate {
        Candidate {
            id: CandidateId::new(id).unwrap(),
            binding_id: BindingId::new("provider.dialect").unwrap(),
            model: CandidateModel::RequestedModel,
            upgrade_policy_id: None,
            alias_policy: None,
            required_capabilities: CapabilityFlags::default(),
            enabled: true,
            health_circuits: vec![],
        }
    }

    fn edge() -> CandidateEdge {
        CandidateEdge {
            preflight: FallbackPermission::Enabled,
            post_send: vec![],
            billed_boundary: super::super::BoundaryConsent::Deny,
            ambiguous_transport: super::super::BoundaryConsent::Deny,
            cache_transition: CacheTransitionPolicy::PreserveWarm,
        }
    }

    fn route(id: &str, candidates: &[&str]) -> Route {
        let (first, rest) = candidates.split_first().unwrap();
        Route {
            honours_server_safeguards: false,
            id: RouteId::new(id).unwrap(),
            display_name: id.to_owned(),
            description: None,
            parameters: vec![],
            chain: super::super::RouteChain {
                first: candidate(first),
                steps: rest
                    .iter()
                    .map(|id| RouteStep {
                        edge: edge(),
                        candidate: candidate(id),
                    })
                    .collect(),
            },
            lane_transition: LaneTransitionPolicy::PreservePinned,
        }
    }

    fn document() -> PolicyDocument {
        PolicyDocument {
            format_version: CURRENT_FORMAT_VERSION,
            description: None,
            defaults: vec![],
            presentations: vec![],
            routes: vec![route("first-route", &["one", "two", "three"])],
            rewrites: vec![],
            classifiers: vec![],
            upgrades: vec![],
            compaction: None,
        }
    }

    fn candidate_ids(document: &PolicyDocument) -> Vec<&str> {
        document.routes[0]
            .chain
            .candidates()
            .map(|candidate| candidate.id.as_str())
            .collect()
    }

    fn classifier(id: &str) -> FamilyClassifier {
        FamilyClassifier {
            id: super::super::ClassifierId::new(id).unwrap(),
            binding_id: BindingId::new("provider.dialect").unwrap(),
            model_matcher: crate::policy::MatcherSpec::Exact("model".to_owned()),
            family_template: "family".to_owned(),
            version_template: "version".to_owned(),
            version_parser: crate::policy::VersionParserSpec::Semver,
            legacy_hyphenated: false,
        }
    }

    #[test]
    fn honouring_server_safeguards_is_set_and_cleared_and_raises_the_format_to_four() {
        let mut older = document();
        older.format_version = 3;
        let on = PolicyEdit {
            operations: vec![PolicyEditOperation::SetRouteServerSafeguards {
                route_id: RouteId::new("first-route").unwrap(),
                honours: true,
            }],
        };
        let edited = on.apply(&older).unwrap();
        assert!(edited.routes[0].honours_server_safeguards);
        assert_eq!(edited.format_version, 4);
        assert!(!older.routes[0].honours_server_safeguards);

        let off = PolicyEdit {
            operations: vec![PolicyEditOperation::SetRouteServerSafeguards {
                route_id: RouteId::new("first-route").unwrap(),
                honours: false,
            }],
        };
        let cleared = off.apply(&edited).unwrap();
        assert!(!cleared.routes[0].honours_server_safeguards);
        assert_eq!(cleared.format_version, 4, "the version never moves back");
    }

    #[test]
    fn records_append_replace_in_place_and_remove_without_reordering() {
        let mut document = document();
        document.routes.push(route("last-route", &["last"]));

        let replacement = route("first-route", &["replacement"]);
        let edit = PolicyEdit {
            operations: vec![
                PolicyEditOperation::AddRoute {
                    route: route("middle-route", &["middle"]),
                },
                PolicyEditOperation::ReplaceRoute {
                    id: RouteId::new("first-route").unwrap(),
                    route: replacement,
                },
                PolicyEditOperation::RemoveRoute {
                    id: RouteId::new("last-route").unwrap(),
                },
            ],
        };

        let edited = edit.apply(&document).unwrap();
        assert_eq!(
            edited
                .routes
                .iter()
                .map(|route| route.id.as_str())
                .collect::<Vec<_>>(),
            ["first-route", "middle-route"]
        );
        assert_eq!(candidate_ids(&edited), ["replacement"]);
        assert_eq!(
            document
                .routes
                .iter()
                .map(|route| route.id.as_str())
                .collect::<Vec<_>>(),
            ["first-route", "last-route"]
        );
    }

    #[test]
    fn high_level_entities_apply_by_stable_id() {
        let mut document = document();
        document.classifiers.push(classifier("old"));

        let edit = PolicyEdit {
            operations: vec![
                PolicyEditOperation::AddClassifier {
                    classifier: classifier("new"),
                },
                PolicyEditOperation::ReplaceClassifier {
                    id: super::super::ClassifierId::new("old").unwrap(),
                    classifier: classifier("old"),
                },
                PolicyEditOperation::RemoveClassifier {
                    id: super::super::ClassifierId::new("new").unwrap(),
                },
                PolicyEditOperation::AddCompaction {
                    compaction: CompactionPolicy {
                        target: super::super::CompactionTarget::Route(
                            super::super::RouteReference {
                                route_id: RouteId::new("first-route").unwrap(),
                                parameters: Default::default(),
                            },
                        ),
                        lane_transition: LaneTransitionPolicy::PreservePinned,
                        cache_transition: CacheTransitionPolicy::PreserveWarm,
                    },
                },
                PolicyEditOperation::RemoveCompaction,
            ],
        };

        let edited = edit.apply(&document).unwrap();
        assert_eq!(edited.classifiers, vec![classifier("old")]);
        assert!(edited.compaction.is_none());
    }

    #[test]
    fn candidate_edits_preserve_unmoved_order_and_require_transition_edges() {
        let edit = PolicyEdit {
            operations: vec![
                PolicyEditOperation::ReplaceCandidate {
                    route_id: RouteId::new("first-route").unwrap(),
                    candidate_id: CandidateId::new("two").unwrap(),
                    candidate: candidate("two"),
                },
                PolicyEditOperation::MoveCandidate {
                    route_id: RouteId::new("first-route").unwrap(),
                    candidate_id: CandidateId::new("three").unwrap(),
                    destination: CandidateMoveDestination::First { edge: edge() },
                },
                PolicyEditOperation::MoveCandidate {
                    route_id: RouteId::new("first-route").unwrap(),
                    candidate_id: CandidateId::new("one").unwrap(),
                    destination: CandidateMoveDestination::After {
                        candidate_id: CandidateId::new("two").unwrap(),
                        edge: edge(),
                    },
                },
                PolicyEditOperation::AddCandidate {
                    route_id: RouteId::new("first-route").unwrap(),
                    after: CandidateId::new("two").unwrap(),
                    edge: edge(),
                    candidate: candidate("four"),
                },
            ],
        };

        let edited = edit.apply(&document()).unwrap();
        assert_eq!(candidate_ids(&edited), ["three", "two", "four", "one"]);
        assert_eq!(edited.routes[0].chain.steps.len(), 3);
    }

    #[test]
    fn candidate_errors_keep_identities_and_the_final_candidate_safe() {
        let replace = PolicyEdit {
            operations: vec![PolicyEditOperation::ReplaceCandidate {
                route_id: RouteId::new("first-route").unwrap(),
                candidate_id: CandidateId::new("one").unwrap(),
                candidate: candidate("renamed"),
            }],
        };
        assert!(matches!(
            replace.apply(&document()),
            Err(PolicyEditError::ReplacementIdMismatch {
                record: "candidate",
                ..
            })
        ));

        let single = PolicyDocument {
            routes: vec![route("single", &["only"])],
            ..document()
        };
        let remove = PolicyEdit {
            operations: vec![PolicyEditOperation::RemoveCandidate {
                route_id: RouteId::new("single").unwrap(),
                candidate_id: CandidateId::new("only").unwrap(),
            }],
        };
        assert!(matches!(
            remove.apply(&single),
            Err(PolicyEditError::CannotRemoveFinalCandidate { .. })
        ));
    }

    #[test]
    fn edits_deserialize_strictly_and_with_a_batch_bound() {
        let unknown =
            r#"{"operations":[{"kind":"set_description","description":null,"extra":true}]}"#;
        assert!(serde_json::from_str::<PolicyEdit>(unknown).is_err());

        let operations = std::iter::repeat_n(
            r#"{"kind":"remove_compaction"}"#,
            MAX_POLICY_EDIT_OPERATIONS + 1,
        )
        .collect::<Vec<_>>()
        .join(",");
        let too_many = format!(r#"{{"operations":[{operations}]}}"#);
        assert!(serde_json::from_str::<PolicyEdit>(&too_many).is_err());
    }

    #[test]
    fn apply_validated_returns_a_validated_document_and_rejects_invalid_results() {
        let validated = document().validate().unwrap();
        let edited = PolicyEdit {
            operations: vec![PolicyEditOperation::SetDescription {
                description: Some("edited".to_owned()),
            }],
        }
        .apply_validated(&validated)
        .unwrap();
        assert_eq!(edited.document().description.as_deref(), Some("edited"));

        let invalid = PolicyEdit {
            operations: vec![PolicyEditOperation::SetDescription {
                description: Some("x".repeat(crate::policy::MAX_DESCRIPTION_BYTES + 1)),
            }],
        };
        assert!(matches!(
            invalid.apply(&document()),
            Err(PolicyEditError::InvalidDocument(
                DocumentError::DescriptionTooLong(_)
            ))
        ));
    }
}
