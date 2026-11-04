//! Gated, local routing-control endpoints.
//!
//! The handlers decode bounded, strict control DTOs and project only policy
//! metadata. The daemon owns draft generations, compilation, activation, and
//! publication state, so this boundary cannot split an active revision from
//! its durable state.

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{MAX_CONTROL_BODY, control_token_ok, forbidden, json_content_type, store_error};
use crate::policy::{
    BindingId, CanonicalLimits, CapabilityFlags, FrontendProtocol, FrontendScope, PolicyEdit,
    PolicyEditError, ProfileId, StableId, decode_policy, policy_bytes,
};
use crate::server::{RoutingControlError, RoutingControlSnapshot, RoutingLaneImpacts, Server};
use crate::store::{
    DraftCasResult, PolicyDraft, PolicyRevision, PublicationAdapter, PublicationCasResult,
    PublicationKey, PublicationState, RoutingPolicyState, RoutingPublication,
};

/// A policy document has its own cap. Promotion requests remain subject to the
/// small generic control cap; an envelope gets a little room around the
/// document's independently bounded canonical form.
const MAX_ROUTING_POLICY_BODY: usize = crate::policy::MAX_DOCUMENT_BYTES + 16 * 1024;

pub(crate) async fn snapshot(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "routing-snapshot") || request.method() != Method::GET {
        return forbidden();
    }
    let snapshot = match server.routing_snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => return routing_internal_error(error),
    };
    routing_snapshot_response("routing-snapshot", snapshot)
}

pub(crate) async fn revisions(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "routing-revisions") || request.method() != Method::GET
    {
        return forbidden();
    }
    match server.routing_revisions() {
        Ok(revisions) => (
            StatusCode::OK,
            axum::Json(json!({
                "toker": "routing-revisions",
                "ok": true,
                "revisions": revisions.iter().map(revision_summary_body).collect::<Vec<_>>(),
            })),
        )
            .into_response(),
        Err(error) => store_error(error),
    }
}

/// Replace the daemon-owned draft. The JSON policy itself stays a raw value
/// until it reaches the canonical bounded decoder, which is the one parser for
/// control, persistence, and activation documents.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceDraftRequest<'a> {
    expected_generation: i64,
    #[serde(borrow)]
    policy: &'a serde_json::value::RawValue,
}

pub(crate) async fn draft(State(server): State<Server>, request: Request) -> Response {
    match *request.method() {
        Method::PUT => replace_draft(State(server), request).await,
        Method::PATCH => patch_draft(State(server), request).await,
        _ => forbidden(),
    }
}

async fn replace_draft(State(server): State<Server>, request: Request) -> Response {
    let bytes = match decode_body(
        request,
        "routing-draft-replace",
        Method::PUT,
        MAX_ROUTING_POLICY_BODY,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let request: ReplaceDraftRequest<'_> = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(_) => return invalid_request("routing-draft-replace"),
    };
    let policy = match decode_policy(request.policy.get().as_bytes(), CanonicalLimits::default()) {
        Ok(policy) => policy,
        Err(_) => return invalid_policy(Vec::new()),
    };
    match server.replace_routing_draft(request.expected_generation, policy) {
        Ok(DraftCasResult::Updated { generation, draft }) => draft_updated_response(
            "routing-draft-replace",
            generation,
            draft.expect("a replacement always retains a draft"),
        ),
        Ok(DraftCasResult::Conflict { current }) => {
            draft_conflict_response("routing-draft-replace", current)
        }
        Err(error) => routing_internal_error(error),
    }
}

/// The edit is the policy module's closed typed vocabulary. It keeps an editor
/// from smuggling arbitrary JSON-pointer writes through the control plane.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchDraftRequest {
    expected_generation: i64,
    edit: PolicyEdit,
}

async fn patch_draft(State(server): State<Server>, request: Request) -> Response {
    let request = match decode_request::<PatchDraftRequest>(
        request,
        "routing-draft-edit",
        Method::PATCH,
        MAX_ROUTING_POLICY_BODY,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return *response,
    };
    if request.edit.operations.is_empty() {
        return invalid_request("routing-draft-edit");
    }
    match server.patch_routing_draft(request.expected_generation, &request.edit) {
        Ok(DraftCasResult::Updated { generation, draft }) => draft_updated_response(
            "routing-draft-edit",
            generation,
            draft.expect("a successful patch always retains a draft"),
        ),
        Ok(DraftCasResult::Conflict { current }) => {
            draft_conflict_response("routing-draft-edit", current)
        }
        Err(RoutingControlError::DraftMissing) => routing_error(
            StatusCode::CONFLICT,
            "routing-draft-edit",
            "draft_missing",
            None,
        ),
        Err(RoutingControlError::StaleDraft { current }) => {
            draft_conflict_response("routing-draft-edit", *current)
        }
        Err(RoutingControlError::DraftEdit(error)) => draft_edit_error_response(error),
        Err(error) => routing_internal_error(error),
    }
}

fn draft_edit_error_response(error: PolicyEditError) -> Response {
    match error {
        PolicyEditError::RecordNotFound { .. }
        | PolicyEditError::DefaultNotFound
        | PolicyEditError::CompactionNotFound
        | PolicyEditError::CandidateNotFound { .. } => routing_error(
            StatusCode::NOT_FOUND,
            "routing-draft-edit",
            "record_not_found",
            None,
        ),
        PolicyEditError::InvalidDocument(_) => invalid_policy(Vec::new()),
        _ => routing_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "routing-draft-edit",
            "invalid_policy_edit",
            None,
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidateRequest {
    #[serde(default)]
    expected_generation: Option<i64>,
}

pub(crate) async fn validate(State(server): State<Server>, request: Request) -> Response {
    let request = match decode_request::<ValidateRequest>(
        request,
        "routing-validate",
        Method::POST,
        MAX_CONTROL_BODY,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return *response,
    };
    match server.validate_routing_draft(request.expected_generation) {
        Ok(validation) => (
            StatusCode::OK,
            axum::Json(json!({
                "toker": "routing-validate",
                "ok": true,
                "generation": validation.generation,
                "valid": validation.diagnostics.is_empty(),
                "diagnostics": diagnostics_body(validation.diagnostics),
            })),
        )
            .into_response(),
        Err(RoutingControlError::DraftMissing) => routing_error(
            StatusCode::CONFLICT,
            "routing-validate",
            "draft_missing",
            None,
        ),
        Err(RoutingControlError::StaleDraft { current }) => {
            draft_conflict_response("routing-validate", *current)
        }
        Err(error) => routing_internal_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewRequest {
    #[serde(default)]
    expected_generation: Option<i64>,
    protocol: FrontendProtocol,
    #[serde(default)]
    profile: Option<ProfileId>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    lane_key: Option<String>,
    #[serde(default)]
    prompt_bound: u64,
    #[serde(default)]
    message_count: Option<u64>,
    #[serde(default)]
    tool_count: u64,
    #[serde(default)]
    required_capabilities: CapabilityFlags,
    #[serde(default)]
    binding_affinity: Option<BindingId>,
}

pub(crate) async fn preview(State(server): State<Server>, request: Request) -> Response {
    let request = match decode_request::<PreviewRequest>(
        request,
        "routing-preview",
        Method::POST,
        MAX_CONTROL_BODY,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return *response,
    };
    match server.preview_routing_draft(crate::server::RoutingPreviewRequest {
        expected_generation: request.expected_generation,
        protocol: request.protocol,
        profile: request.profile,
        model: request.model,
        lane_key: request.lane_key,
        prompt_bound: request.prompt_bound,
        message_count: request.message_count,
        tool_count: request.tool_count,
        required_capabilities: request.required_capabilities,
        binding_affinity: request.binding_affinity,
    }) {
        Ok(preview) => (
            StatusCode::OK,
            axum::Json(json!({
                "toker": "routing-preview",
                "ok": true,
                "generation": preview.generation,
                "diagnostics": diagnostics_body(preview.diagnostics),
                "result": preview.result,
            })),
        )
            .into_response(),
        Err(RoutingControlError::DraftMissing) => routing_error(
            StatusCode::CONFLICT,
            "routing-preview",
            "draft_missing",
            None,
        ),
        Err(RoutingControlError::StaleDraft { current }) => {
            draft_conflict_response("routing-preview", *current)
        }
        Err(RoutingControlError::Compilation(diagnostics)) => invalid_policy(diagnostics),
        Err(error) => routing_internal_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivateRequest {
    expected_generation: i64,
    scope: crate::policy::ActivationScope,
    #[serde(default)]
    force_cache_transition: bool,
    #[serde(default)]
    description: Option<String>,
}

pub(crate) async fn activate(State(server): State<Server>, request: Request) -> Response {
    let request = match decode_request::<ActivateRequest>(
        request,
        "routing-activate",
        Method::POST,
        MAX_CONTROL_BODY,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return *response,
    };
    match server.activate_routing_draft(
        request.expected_generation,
        request.scope,
        request.force_cache_transition,
        request.description.as_deref(),
    ) {
        Ok(activation) => activated_response("routing-activate", activation),
        Err(RoutingControlError::DraftMissing) => routing_error(
            StatusCode::CONFLICT,
            "routing-activate",
            "draft_missing",
            None,
        ),
        Err(RoutingControlError::StaleDraft { current }) => {
            draft_conflict_response("routing-activate", *current)
        }
        Err(RoutingControlError::ForcedTransitionRequired { impacts }) => routing_error(
            StatusCode::CONFLICT,
            "routing-activate",
            "forced_cache_transition_required",
            Some(json!({ "lane_impacts": lane_impacts_body(&impacts) })),
        ),
        Err(RoutingControlError::Compilation(diagnostics)) => invalid_policy(diagnostics),
        Err(error) => routing_internal_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReactivateRequest {
    expected_generation: i64,
    revision_id: i64,
    scope: crate::policy::ActivationScope,
    #[serde(default)]
    force_cache_transition: bool,
    #[serde(default)]
    description: Option<String>,
}

pub(crate) async fn reactivate(State(server): State<Server>, request: Request) -> Response {
    let request = match decode_request::<ReactivateRequest>(
        request,
        "routing-reactivate",
        Method::POST,
        MAX_CONTROL_BODY,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return *response,
    };
    match server.reactivate_routing_policy(
        request.expected_generation,
        request.revision_id,
        request.scope,
        request.force_cache_transition,
        request.description.as_deref(),
    ) {
        Ok(activation) => activated_response("routing-reactivate", activation),
        Err(RoutingControlError::MissingRevision(revision_id)) => routing_error(
            StatusCode::NOT_FOUND,
            "routing-reactivate",
            "revision_not_found",
            Some(json!({ "revision_id": revision_id })),
        ),
        Err(RoutingControlError::StaleDraft { current }) => {
            draft_conflict_response("routing-reactivate", *current)
        }
        Err(RoutingControlError::ForcedTransitionRequired { impacts }) => routing_error(
            StatusCode::CONFLICT,
            "routing-reactivate",
            "forced_cache_transition_required",
            Some(json!({ "lane_impacts": lane_impacts_body(&impacts) })),
        ),
        Err(RoutingControlError::Compilation(diagnostics)) => invalid_policy(diagnostics),
        Err(error) => routing_internal_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromoteRequest {
    binding_id: BindingId,
    model_id: String,
}

pub(crate) async fn promote(State(server): State<Server>, request: Request) -> Response {
    let request = match decode_request::<PromoteRequest>(
        request,
        "routing-promote",
        Method::POST,
        MAX_CONTROL_BODY,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return *response,
    };
    match server.promote_routing_learning(&request.binding_id, &request.model_id) {
        Ok(Some(learning)) => (
            StatusCode::OK,
            axum::Json(json!({
                "toker": "routing-promote",
                "ok": true,
                "learning": {
                    "binding_id": learning.binding_id,
                    "model_id": learning.model_id,
                    "explicitly_promoted": learning.explicitly_promoted,
                    "updated_ms": learning.updated_ms,
                },
            })),
        )
            .into_response(),
        Ok(None) => routing_error(
            StatusCode::NOT_FOUND,
            "routing-promote",
            "unseen_binding_model",
            None,
        ),
        Err(error) => store_error(error),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PublicationOutcome {
    Applied,
    Failed,
    Unsupported,
}

impl From<PublicationOutcome> for PublicationState {
    fn from(value: PublicationOutcome) -> Self {
        match value {
            PublicationOutcome::Applied => Self::Applied,
            PublicationOutcome::Failed => Self::Failed,
            PublicationOutcome::Unsupported => Self::Unsupported,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationRequest {
    scope: FrontendScope,
    adapter: String,
    expected_revision_id: i64,
    expected_inventory_generation: i64,
    outcome: PublicationOutcome,
    #[serde(default)]
    detail_code: Option<StableId>,
}

pub(crate) async fn publication(State(server): State<Server>, request: Request) -> Response {
    let request = match decode_request::<PublicationRequest>(
        request,
        "routing-publication",
        Method::POST,
        MAX_CONTROL_BODY,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return *response,
    };
    let state = PublicationState::from(request.outcome);
    if state == PublicationState::Applied {
        return routing_error(
            StatusCode::BAD_REQUEST,
            "routing-publication",
            "adapter_owned_result_required",
            None,
        );
    }
    let key = match PublicationAdapter::new(request.adapter) {
        Ok(adapter) => PublicationKey::new(request.scope, adapter),
        Err(_) => {
            return routing_error(
                StatusCode::BAD_REQUEST,
                "routing-publication",
                "invalid_publication_adapter",
                None,
            );
        }
    };
    match server.record_routing_publication(
        &key,
        request.expected_revision_id,
        request.expected_inventory_generation,
        state,
        request.detail_code,
    ) {
        Ok(PublicationCasResult::Updated) => (
            StatusCode::OK,
            axum::Json(json!({ "toker": "routing-publication", "ok": true })),
        )
            .into_response(),
        Ok(PublicationCasResult::Conflict { current }) => routing_error(
            StatusCode::CONFLICT,
            "routing-publication",
            "stale_publication",
            current.map(|publication| json!({ "current": publication_body(publication) })),
        ),
        Err(error) => store_error(error),
    }
}

async fn decode_request<T>(
    request: Request,
    operation: &str,
    method: Method,
    limit: usize,
) -> Result<T, Box<Response>>
where
    T: serde::de::DeserializeOwned,
{
    let bytes = decode_body(request, operation, method, limit).await?;
    serde_json::from_slice(&bytes).map_err(|_| Box::new(invalid_request(operation)))
}

async fn decode_body(
    request: Request,
    operation: &str,
    method: Method,
    limit: usize,
) -> Result<Bytes, Box<Response>> {
    if !control_token_ok(request.headers(), operation) {
        return Err(Box::new(forbidden()));
    }
    let (parts, body) = request.into_parts();
    if parts.method != method || !json_content_type(&parts.headers) {
        return Err(Box::new(forbidden()));
    }
    axum::body::to_bytes(body, limit)
        .await
        .map_err(|_| Box::new(invalid_request(operation)))
}

fn routing_snapshot_response(operation: &str, snapshot: RoutingControlSnapshot) -> Response {
    (
        StatusCode::OK,
        axum::Json(json!({
            "toker": operation,
            "ok": true,
            "active_revision": revision_body(&snapshot.active_revision),
            "draft_generation": snapshot.state.draft_generation,
            "draft": snapshot.state.draft.map(draft_body),
            "draft_diagnostics": diagnostics_body(snapshot.draft_diagnostics),
            "inventory_generation": snapshot.inventory_generation,
            "offers": snapshot.inventory.iter().map(offer_body).collect::<Vec<_>>(),
            "lane_impacts": lane_impacts_body(&snapshot.lane_impacts),
            "publications": snapshot.publications.into_iter().map(publication_body).collect::<Vec<_>>(),
        })),
    )
        .into_response()
}

fn draft_updated_response(operation: &str, generation: i64, draft: PolicyDraft) -> Response {
    (
        StatusCode::OK,
        axum::Json(json!({
            "toker": operation,
            "ok": true,
            "generation": generation,
            "draft": draft_body(draft),
        })),
    )
        .into_response()
}

fn draft_conflict_response(operation: &str, current: RoutingPolicyState) -> Response {
    routing_error(
        StatusCode::CONFLICT,
        operation,
        "stale_draft",
        Some(json!({
            "generation": current.draft_generation,
            "draft": current.draft.map(draft_body),
        })),
    )
}

fn activated_response(operation: &str, activation: crate::server::RoutingActivation) -> Response {
    (
        StatusCode::OK,
        axum::Json(json!({
            "toker": operation,
            "ok": true,
            "active_revision": revision_body(&activation.activation.revision),
            "generation": activation.activation.state.draft_generation,
            "draft": activation.activation.state.draft.map(draft_body),
            "lane_impacts": lane_impacts_body(&activation.lane_impacts),
            "publications": activation.activation.publications.into_iter().map(publication_body).collect::<Vec<_>>(),
        })),
    )
        .into_response()
}

fn invalid_policy(diagnostics: Vec<crate::policy::Diagnostic>) -> Response {
    routing_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "routing-control",
        "invalid_policy",
        Some(json!({ "diagnostics": diagnostics_body(diagnostics) })),
    )
}

fn invalid_request(operation: &str) -> Response {
    routing_error(StatusCode::BAD_REQUEST, operation, "invalid_request", None)
}

fn diagnostics_body(diagnostics: Vec<crate::policy::Diagnostic>) -> Vec<Value> {
    diagnostics
        .into_iter()
        .map(|diagnostic| {
            json!({
                "code": diagnostic.code,
                "severity": diagnostic.severity,
                "record_id": diagnostic.record_id,
                "path": diagnostic.path.as_str(),
            })
        })
        .collect()
}

fn revision_body(revision: &PolicyRevision) -> Value {
    let document = policy_bytes(&revision.document)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    json!({
        "revision_id": revision.revision_id,
        "created_ms": revision.created_ms,
        "activated_ms": revision.activated_ms,
        "description": revision.description,
        "activation_scope": revision.activation_scope,
        "forced_cache_transition": revision.forced_cache_transition,
        "source": revision_source_name(revision.source),
        "parent_revision_id": revision.parent_revision_id,
        "fingerprint": revision.fingerprint.to_string(),
        "policy": document,
    })
}

fn revision_summary_body(revision: &PolicyRevision) -> Value {
    json!({
        "revision_id": revision.revision_id,
        "created_ms": revision.created_ms,
        "activated_ms": revision.activated_ms,
        "description": revision.description,
        "activation_scope": revision.activation_scope,
        "forced_cache_transition": revision.forced_cache_transition,
        "source": revision_source_name(revision.source),
        "parent_revision_id": revision.parent_revision_id,
        "fingerprint": revision.fingerprint.to_string(),
    })
}

fn draft_body(draft: PolicyDraft) -> Value {
    let policy = policy_bytes(&draft.document)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    json!({
        "base_revision_id": draft.base_revision_id,
        "fingerprint": draft.fingerprint.to_string(),
        "updated_ms": draft.updated_ms,
        "policy": policy,
    })
}

fn offer_body(offer: &crate::catalog::offers::BackendOffer) -> Value {
    // Deliberately do not project `offer.raw`: fetched provider records can
    // gain opaque fields, while this API promises content-free safe facts.
    json!({
        "binding_id": offer.binding_id,
        "provider": offer.provider,
        "model_id": offer.backend_model_id,
        "context_window": offer.context_window,
        "fetched_at_ms": offer.fetched_at_ms,
        "chronology_ms": offer.chronology.map(|chronology| chronology.unix_ms()),
    })
}

fn lane_impacts_body(impacts: &RoutingLaneImpacts) -> Value {
    json!({
        "total": impacts.total,
        "unpinned": impacts.unpinned,
        "warm": impacts.warm,
        "cold": impacts.cold,
        "unknown": impacts.unknown,
    })
}

fn publication_body(publication: RoutingPublication) -> Value {
    let scope = match publication.key.scope() {
        FrontendScope::Profile { profile, protocol } => json!({
            "kind": "profile",
            "profile": profile.as_str(),
            "protocol": protocol_name(*protocol),
        }),
        FrontendScope::Protocol { protocol } => json!({
            "kind": "protocol",
            "protocol": protocol_name(*protocol),
        }),
    };
    json!({
        "scope": scope,
        "adapter": publication.key.adapter().as_str(),
        "desired_revision_id": publication.desired_revision_id,
        "applied_revision_id": publication.applied_revision_id,
        "desired_inventory_generation": publication.desired_inventory_generation,
        "applied_inventory_generation": publication.applied_inventory_generation,
        "applied_identities": publication.applied_identities,
        "state": publication_state_name(publication.state),
        "detail_code": publication.detail_code.map(|code| code.to_string()),
        "updated_ms": publication.updated_ms,
    })
}

fn protocol_name(protocol: FrontendProtocol) -> &'static str {
    match protocol {
        FrontendProtocol::AnthropicMessages => "anthropic_messages",
        FrontendProtocol::OpenaiChat => "openai_chat",
        FrontendProtocol::OpenaiResponses => "openai_responses",
    }
}

fn revision_source_name(source: crate::store::PolicyRevisionSource) -> &'static str {
    match source {
        crate::store::PolicyRevisionSource::Legacy => "legacy",
        crate::store::PolicyRevisionSource::Draft => "draft",
        crate::store::PolicyRevisionSource::Reactivation => "reactivation",
    }
}

fn publication_state_name(state: PublicationState) -> &'static str {
    match state {
        PublicationState::Pending => "pending",
        PublicationState::Applied => "applied",
        PublicationState::Failed => "failed",
        PublicationState::Unsupported => "unsupported",
    }
}

fn routing_internal_error(error: impl std::fmt::Display) -> Response {
    tracing::error!(%error, "routing control operation failed");
    routing_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "routing-control",
        "routing_control_failure",
        None,
    )
}

fn routing_error(
    status: StatusCode,
    operation: &str,
    error: &str,
    extra: Option<Value>,
) -> Response {
    let mut body = json!({ "toker": operation, "ok": false, "error": error });
    if let Some(Value::Object(values)) = extra {
        body.as_object_mut().expect("object literal").extend(values);
    }
    (status, axum::Json(body)).into_response()
}
