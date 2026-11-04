//! Gated, local routing-control endpoints.
//!
//! The handlers only decode and project control-plane state. Compilation,
//! activation, publication CAS, and runtime swaps belong to `Server` and
//! `Store`, so this boundary cannot split a live revision from its persistence.

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{MAX_CONTROL_BODY, control_token_ok, forbidden, json_content_type, store_error};
use crate::policy::{
    BindingId, CanonicalLimits, FrontendProtocol, FrontendScope, StableId, decode_policy,
    policy_bytes,
};
use crate::server::{RoutingControlError, Server};
use crate::store::{
    PolicyRevision, PublicationAdapter, PublicationCasResult, PublicationKey, PublicationState,
    RoutingPublication,
};

pub(crate) async fn snapshot(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "routing-snapshot") || request.method() != Method::GET {
        return forbidden();
    }
    let snapshot = match server.routing_snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => return routing_internal_error(error),
    };
    (
        StatusCode::OK,
        axum::Json(json!({
            "toker": "routing-snapshot",
            "ok": true,
            "active_revision": revision_body(&snapshot.active_revision),
            "inventory_generation": snapshot.inventory_generation,
            "publications": snapshot.publications.into_iter().map(publication_body).collect::<Vec<_>>(),
        })),
    )
        .into_response()
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
                "revisions": revisions.iter().map(revision_body).collect::<Vec<_>>(),
            })),
        )
            .into_response(),
        Err(error) => store_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyRequest<'a> {
    #[serde(borrow)]
    policy: &'a serde_json::value::RawValue,
    #[serde(default)]
    description: Option<String>,
}

pub(crate) async fn apply(State(server): State<Server>, request: Request) -> Response {
    let bytes = match decode_body(request, "routing-apply").await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let request: ApplyRequest<'_> = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(_) => {
            return routing_error(
                StatusCode::BAD_REQUEST,
                "routing-apply",
                "invalid_request",
                None,
            );
        }
    };
    let policy = match decode_policy(request.policy.get().as_bytes(), CanonicalLimits::default()) {
        Ok(policy) => policy,
        Err(_) => return invalid_policy(Vec::new()),
    };
    match server.apply_routing_policy(policy, request.description.as_deref()) {
        Ok(activation) => activated_response("routing-apply", activation),
        Err(RoutingControlError::Compilation(diagnostics)) => invalid_policy(diagnostics),
        Err(error) => routing_internal_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreRequest {
    revision_id: i64,
    #[serde(default)]
    description: Option<String>,
}

pub(crate) async fn restore(State(server): State<Server>, request: Request) -> Response {
    let request = match decode_request::<RestoreRequest>(request, "routing-restore").await {
        Ok(request) => request,
        Err(response) => return *response,
    };
    match server.restore_routing_policy(request.revision_id, request.description.as_deref()) {
        Ok(activation) => activated_response("routing-restore", activation),
        Err(RoutingControlError::MissingRevision(revision_id)) => routing_error(
            StatusCode::NOT_FOUND,
            "routing-restore",
            "revision_not_found",
            Some(json!({ "revision_id": revision_id })),
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
    let request = match decode_request::<PromoteRequest>(request, "routing-promote").await {
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
    let request = match decode_request::<PublicationRequest>(request, "routing-publication").await {
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

async fn decode_request<T>(request: Request, operation: &str) -> Result<T, Box<Response>>
where
    T: serde::de::DeserializeOwned,
{
    let bytes = decode_body(request, operation).await?;
    serde_json::from_slice(&bytes).map_err(|_| {
        Box::new(routing_error(
            StatusCode::BAD_REQUEST,
            operation,
            "invalid_request",
            None,
        ))
    })
}

async fn decode_body(request: Request, operation: &str) -> Result<Bytes, Box<Response>> {
    if !control_token_ok(request.headers(), operation) {
        return Err(Box::new(forbidden()));
    }
    let (parts, body) = request.into_parts();
    if parts.method != Method::POST || !json_content_type(&parts.headers) {
        return Err(Box::new(forbidden()));
    }
    axum::body::to_bytes(body, MAX_CONTROL_BODY)
        .await
        .map_err(|_| {
            Box::new(routing_error(
                StatusCode::BAD_REQUEST,
                operation,
                "invalid_request",
                None,
            ))
        })
}

fn activated_response(
    operation: &str,
    activation: crate::store::ActivatedPolicyRevision,
) -> Response {
    (
        StatusCode::OK,
        axum::Json(json!({
            "toker": operation,
            "ok": true,
            "active_revision": revision_body(&activation.revision),
            "publications": activation.publications.into_iter().map(publication_body).collect::<Vec<_>>(),
        })),
    )
        .into_response()
}

fn invalid_policy(diagnostics: Vec<crate::policy::Diagnostic>) -> Response {
    routing_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "routing-control",
        "invalid_policy",
        Some(json!({
            "diagnostics": diagnostics.into_iter().map(|diagnostic| json!({
                "code": diagnostic.code,
                "severity": diagnostic.severity,
                "record_id": diagnostic.record_id,
                "path": diagnostic.path.as_str(),
            })).collect::<Vec<_>>(),
        })),
    )
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
