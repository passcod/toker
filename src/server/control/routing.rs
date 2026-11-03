//! The live routing control API.
//!
//! Each confirmed TUI edit applies immediately as a new immutable revision.
//! There is no server-side editing state: the caller holds the document it is
//! changing and can restore any prior revision if the active result is wrong.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{MAX_CONTROL_BODY, control_token_ok, forbidden, json_content_type};
use crate::policy::{
    CanonicalError, CanonicalLimits, FrontendScope, LearningView, StableId, decode_policy,
};
use crate::server::{RoutingControlError, RoutingMutationError, Server};
use crate::store::{
    BindingLearning, PolicyRevision, PublicationAdapter, PublicationCasResult, PublicationKey,
    PublicationResultUpdate, PublicationState, RoutingPublication,
};

const MAX_ROUTING_DOCUMENT_BODY: usize = crate::policy::MAX_DOCUMENT_BYTES;

/// `GET /_toker/routing/snapshot` — the active document and its immediately
/// observable model/publication effects. It is deliberately one captured
/// runtime revision, so a catalogue refresh cannot mix a document from one
/// policy revision with offers from another.
pub(crate) async fn snapshot(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "routing-snapshot") || request.method() != Method::GET {
        return forbidden();
    }
    let Some(runtime) = &server.policy_runtime else {
        return control_error("routing-snapshot", StatusCode::CONFLICT, "inactive");
    };
    let Some(snapshot) = runtime.capture() else {
        return control_error("routing-snapshot", StatusCode::CONFLICT, "inactive");
    };
    let Some(revision_id) = snapshot.revision_id() else {
        return control_error("routing-snapshot", StatusCode::CONFLICT, "inactive");
    };
    let revision = match server.store.load_policy_revision(revision_id) {
        Ok(Some(revision)) => revision,
        Ok(None) => {
            return control_error(
                "routing-snapshot",
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
            );
        }
        Err(error) => return internal_error("routing-snapshot", error),
    };
    let publications = match server.store.load_publications() {
        Ok(publications) => publications,
        Err(error) => return internal_error("routing-snapshot", error),
    };
    let circuits = runtime.open_circuits(&snapshot, crate::server::record::now_ms());
    Json(json!({
        "toker": "routing-snapshot",
        "ok": true,
        "active": {
            "revision": revision_value(&revision),
            "document": revision.document.document(),
            "inventory_generation": snapshot.inventory_generation(),
        },
        "offers": snapshot.inventory().iter().map(offer_value).collect::<Vec<_>>(),
        "publications": publications.iter().map(publication_value).collect::<Vec<_>>(),
        "open_circuits": circuits.iter().map(|circuit| json!({
            "route_id": circuit.route_id,
            "candidate_id": circuit.candidate_id,
            "binding_id": circuit.binding_id,
            "model_id": circuit.model_id,
            "failure": circuit.failure,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// `POST /_toker/routing/apply` — validate, compile, and immediately apply a
/// complete authored policy document. The document's optional description is
/// the revision description; keeping it in the document avoids a second,
/// partially-overlapping edit envelope.
pub(crate) async fn apply(State(server): State<Server>, request: Request) -> Response {
    let bytes = match routing_body(request, "routing-apply", MAX_ROUTING_DOCUMENT_BODY).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let document = match decode_policy(&bytes, CanonicalLimits::default()) {
        Ok(document) => document,
        Err(error) => return document_error("routing-apply", error),
    };
    let description = document.document().description.as_deref();
    match server.apply_routing_policy(&document, description) {
        Ok(activation) => activation_response("routing-apply", StatusCode::CREATED, activation),
        Err(error) => mutation_error("routing-apply", error),
    }
}

/// `GET /_toker/routing/revisions` — immutable history in revision order.
pub(crate) async fn revisions(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "routing-revisions") || request.method() != Method::GET
    {
        return forbidden();
    }
    let state = match server.store.load_routing_policy_state() {
        Ok(state) => state,
        Err(error) => return internal_error("routing-revisions", error),
    };
    let revisions = match server.store.load_policy_revisions() {
        Ok(revisions) => revisions,
        Err(error) => return internal_error("routing-revisions", error),
    };
    Json(json!({
        "toker": "routing-revisions",
        "ok": true,
        "active_revision_id": state.active_revision_id,
        "revisions": revisions.iter().map(revision_value).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreRequest {
    revision_id: i64,
    #[serde(default)]
    description: Option<String>,
}

/// `POST /_toker/routing/restore` — apply an earlier configuration as a new
/// immutable revision. This is an auditable forward action, never a pointer
/// rewind or destructive database rollback.
pub(crate) async fn restore(State(server): State<Server>, request: Request) -> Response {
    let bytes = match routing_body(request, "routing-restore", MAX_CONTROL_BODY).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let body: RestoreRequest = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(_) => {
            return control_error(
                "routing-restore",
                StatusCode::BAD_REQUEST,
                "invalid_request",
            );
        }
    };
    if body
        .description
        .as_ref()
        .is_some_and(|description| description.len() > crate::policy::MAX_DESCRIPTION_BYTES)
    {
        return control_error(
            "routing-restore",
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
    }
    match server.restore_routing_policy(body.revision_id, body.description.as_deref()) {
        Ok(activation) => activation_response("routing-restore", StatusCode::CREATED, activation),
        Err(error) => mutation_error("routing-restore", error),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromoteRequest {
    binding_id: crate::policy::BindingId,
    model_id: String,
}

/// `POST /_toker/routing/promote` — promote learning for exactly one binding
/// and model. Provider model names are not globally unique, so the binding is
/// intentionally not inferred from the model spelling.
pub(crate) async fn promote(State(server): State<Server>, request: Request) -> Response {
    let bytes = match routing_body(request, "routing-promote", MAX_CONTROL_BODY).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let body: PromoteRequest = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(_) => {
            return control_error(
                "routing-promote",
                StatusCode::BAD_REQUEST,
                "invalid_request",
            );
        }
    };
    if body.model_id.is_empty() || body.model_id.len() > crate::policy::MAX_STRING_BYTES {
        return control_error(
            "routing-promote",
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
    }
    if server.registry.binding(&body.binding_id).is_none() {
        return control_error(
            "routing-promote",
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
    }
    let learning = match server.store.promote_binding_learning(
        &body.binding_id,
        &body.model_id,
        crate::server::record::now_ms(),
    ) {
        Ok(Some(learning)) => learning,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "toker": "routing-promote",
                    "ok": false,
                    "error": "unseen",
                    "binding_id": body.binding_id,
                    "model_id": body.model_id,
                })),
            )
                .into_response();
        }
        Err(error) => return internal_error("routing-promote", error),
    };
    if let Err(error) = server.binding_learning.refresh_snapshot() {
        tracing::error!(%error, "routing promotion learning refresh failed");
    }
    Json(json!({
        "toker": "routing-promote",
        "ok": true,
        "learning": learning_value(&learning),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationRequest {
    scope: FrontendScope,
    adapter: String,
    expected_revision_id: i64,
    expected_inventory_generation: i64,
    state: PublicationOutcome,
    #[serde(default)]
    owned_row_ids: Option<Vec<String>>,
    #[serde(default)]
    detail_code: Option<StableId>,
}

/// `POST /_toker/routing/publication` — record the result of a user-side
/// reconciler only if it still applies to the desired revision and inventory.
pub(crate) async fn publication(State(server): State<Server>, request: Request) -> Response {
    let bytes = match routing_body(request, "routing-publication", MAX_CONTROL_BODY).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let body: PublicationRequest = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(_) => {
            return control_error(
                "routing-publication",
                StatusCode::BAD_REQUEST,
                "invalid_request",
            );
        }
    };
    let state = PublicationState::from(body.state);
    let applied_identities = match (state, body.owned_row_ids) {
        (PublicationState::Applied, Some(ids)) => {
            if ids.len() > 1_024 {
                return control_error(
                    "routing-publication",
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                );
            }
            let set = ids.into_iter().collect::<BTreeSet<_>>();
            if set.is_empty() || set.len() > 1_024 {
                return control_error(
                    "routing-publication",
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                );
            }
            Some(set)
        }
        (PublicationState::Applied, None) | (_, Some(_)) => {
            return control_error(
                "routing-publication",
                StatusCode::BAD_REQUEST,
                "invalid_request",
            );
        }
        _ => None,
    };
    let adapter = match PublicationAdapter::new(body.adapter) {
        Ok(adapter) => adapter,
        Err(_) => {
            return control_error(
                "routing-publication",
                StatusCode::BAD_REQUEST,
                "invalid_request",
            );
        }
    };
    let key = PublicationKey::new(body.scope, adapter);
    let expected = match server.store.load_publication(&key) {
        Ok(Some(publication)) => publication,
        Ok(None) => {
            return control_error(
                "routing-publication",
                StatusCode::NOT_FOUND,
                "publication_not_found",
            );
        }
        Err(error) => return internal_error("routing-publication", error),
    };
    if expected.state == PublicationState::Applied {
        return control_error(
            "routing-publication",
            StatusCode::CONFLICT,
            "managed_by_daemon",
        );
    }
    let update = PublicationResultUpdate {
        state,
        applied_identities,
        detail_code: body.detail_code,
        updated_ms: crate::server::record::now_ms(),
    };
    match server.store.compare_and_swap_publication_result(
        &key,
        body.expected_revision_id,
        body.expected_inventory_generation,
        &update,
    ) {
        Ok(PublicationCasResult::Updated) => match server.store.load_publication(&key) {
            Ok(Some(publication)) => Json(json!({
                "toker": "routing-publication",
                "ok": true,
                "publication": publication_value(&publication),
            }))
            .into_response(),
            Ok(None) => control_error(
                "routing-publication",
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
            ),
            Err(error) => internal_error("routing-publication", error),
        },
        Ok(PublicationCasResult::Conflict { current }) => (
            StatusCode::CONFLICT,
            Json(json!({
                "toker": "routing-publication",
                "ok": false,
                "error": "stale",
                "current": current.as_ref().map(publication_value),
            })),
        )
            .into_response(),
        Err(error) => internal_error("routing-publication", error),
    }
}

async fn routing_body(
    request: Request,
    verb: &str,
    limit: usize,
) -> Result<axum::body::Bytes, Box<Response>> {
    if !control_token_ok(request.headers(), verb)
        || request.method() != Method::POST
        || !json_content_type(request.headers())
    {
        return Err(Box::new(forbidden()));
    }
    let (_, body) = request.into_parts();
    axum::body::to_bytes(body, limit).await.map_err(|_| {
        Box::new(control_error(
            verb,
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
        ))
    })
}

fn activation_response(
    operation: &str,
    status: StatusCode,
    activation: crate::server::RoutingActivation,
) -> Response {
    (
        status,
        Json(json!({
            "toker": operation,
            "ok": true,
            "active_revision": revision_value(&activation.activation.revision),
            "publications": activation.activation.publications.iter().map(publication_value).collect::<Vec<_>>(),
            "diagnostics": activation.diagnostics,
        })),
    )
        .into_response()
}

fn mutation_error(operation: &str, error: RoutingMutationError) -> Response {
    match error {
        RoutingMutationError::Invalid { diagnostics } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "toker": operation,
                "ok": false,
                "error": "invalid_policy",
                "diagnostics": diagnostics,
            })),
        )
            .into_response(),
        RoutingMutationError::Control(RoutingControlError::MissingRevision(_)) => {
            control_error(operation, StatusCode::NOT_FOUND, "revision_not_found")
        }
        RoutingMutationError::Control(RoutingControlError::Inactive) => {
            control_error(operation, StatusCode::CONFLICT, "inactive")
        }
        RoutingMutationError::Control(RoutingControlError::Store(error)) => {
            internal_error(operation, error)
        }
    }
}

fn document_error(operation: &str, error: CanonicalError) -> Response {
    let status = if matches!(error, CanonicalError::DocumentTooLarge) {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::BAD_REQUEST
    };
    control_error(operation, status, "invalid_document")
}

fn internal_error(operation: &str, error: crate::store::Error) -> Response {
    tracing::error!(%error, operation, "routing control store failure");
    control_error(
        operation,
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
    )
}

fn control_error(operation: &str, status: StatusCode, error: &str) -> Response {
    (
        status,
        Json(json!({"toker": operation, "ok": false, "error": error})),
    )
        .into_response()
}

fn revision_value(revision: &PolicyRevision) -> Value {
    json!({
        "revision_id": revision.revision_id,
        "parent_revision_id": revision.parent_revision_id,
        "created_ms": revision.created_ms,
        "activated_ms": revision.activated_ms,
        "description": revision.description,
        "activation_scope": revision.activation_scope,
        "forced_cache_transition": revision.forced_cache_transition,
        "source": revision.source.as_str(),
        "fingerprint": revision.fingerprint,
    })
}

fn publication_value(publication: &RoutingPublication) -> Value {
    json!({
        "scope": publication.key.scope(),
        "adapter": publication.key.adapter().as_str(),
        "desired_revision_id": publication.desired_revision_id,
        "applied_revision_id": publication.applied_revision_id,
        "desired_inventory_generation": publication.desired_inventory_generation,
        "applied_inventory_generation": publication.applied_inventory_generation,
        "owned_row_ids": publication.applied_identities,
        "state": publication.state.as_str(),
        "detail_code": publication.detail_code,
        "updated_ms": publication.updated_ms,
    })
}

fn offer_value(offer: &crate::catalog::offers::BackendOffer) -> Value {
    json!({
        "binding_id": offer.binding_id,
        "provider": offer.provider,
        "model_id": offer.backend_model_id,
        "context_window": offer.context_window,
        "fetched_at_ms": offer.fetched_at_ms,
    })
}

fn learning_value(learning: &BindingLearning) -> Value {
    json!({
        "binding_id": learning.binding_id,
        "model_id": learning.model_id,
        "days": learning.days,
        "max_prompt": learning.max_prompt,
        "context_window": learning.context_window,
        "explicitly_promoted": learning.explicitly_promoted,
        "updated_ms": learning.updated_ms,
    })
}
