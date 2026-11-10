//! Gated, local routing-control endpoints.
//!
//! The handlers decode bounded, strict control DTOs and project only policy
//! metadata. The daemon owns compilation, activation and publication state,
//! so this boundary cannot split an active revision from its durable state.
//! Every operation that changes anything names the active revision it was
//! made against and is refused, changing nothing, where that has moved.

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::revision_diff;
use super::{MAX_CONTROL_BODY, control_token_ok, forbidden, json_content_type, store_error};
use crate::policy::{
    ActivationScope, BindingId, CapabilityFlags, FrontendProtocol, FrontendScope, PolicyEdit,
    PolicyEditError, ProfileId, RecordedFacts, StableId, policy_bytes,
};
use crate::server::routing_ops::{
    InventedRequest, NotReplayable, PreviewAgainst, PreviewOutcome, PreviewSubject, ReplayChange,
    ReplayOutcome, RoutingPreviewRequest,
};
use crate::server::{RoutingControlError, RoutingControlSnapshot, RoutingLaneImpacts, Server};
use crate::store::{
    HitCounts, HitCoverage, PolicyRevision, PublicationAdapter, PublicationCasResult,
    PublicationKey, PublicationState, RecentRequest, RoutingPublication,
};

/// A save carries an edit of up to the policy module's own operation bound,
/// which is independently limited, so its envelope gets a little room above
/// the generic control cap.
const MAX_ROUTING_EDIT_BODY: usize = crate::policy::MAX_DOCUMENT_BYTES + 16 * 1024;

/// The most recent requests one listing returns when it names no limit, and
/// the most it will.
const DEFAULT_RECENT_REQUESTS: u32 = 50;
const MAX_RECENT_REQUESTS: u32 = 200;

/// The revisions one page of the history returns when it names no limit, and
/// the most it will. Nothing prunes the table.
const DEFAULT_REVISIONS: u32 = 50;
const MAX_REVISIONS: u32 = 200;

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
    let query = request.uri().query().unwrap_or_default();
    let before = match query_number(query, "before") {
        Ok(before) => before,
        Err(()) => return invalid_request("routing-revisions"),
    };
    let limit = match query_number(query, "limit") {
        Ok(limit) => limit
            .and_then(|limit| u32::try_from(limit).ok())
            .unwrap_or(DEFAULT_REVISIONS)
            .clamp(1, MAX_REVISIONS),
        Err(()) => return invalid_request("routing-revisions"),
    };
    // One more than the page: the extra, older revision is what the oldest
    // in the page is diffed against, and says there is another page.
    match server.routing_revisions(before, limit.saturating_add(1)) {
        Ok(mut revisions) => {
            let more = revisions.len() > limit as usize;
            let baseline = more.then(|| revisions.pop()).flatten();
            let next_before = revisions.last().map(|revision| revision.revision_id);
            (
                StatusCode::OK,
                axum::Json(json!({
                    "toker": "routing-revisions",
                    "ok": true,
                    "revisions": revision_summaries(revisions, baseline.as_ref()),
                    "has_more": more,
                    "next_before": next_before,
                })),
            )
                .into_response()
        }
        Err(error) => store_error(error),
    }
}

/// One numeric query parameter, or `Err` where it is present and is not a
/// number: a mistyped parameter fails rather than being ignored.
fn query_number(query: &str, name: &str) -> Result<Option<i64>, ()> {
    let Some(value) = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
    else {
        return Ok(None);
    };
    value.parse().map(Some).map_err(|_| ())
}

/// An edit and the revision it was made against. The edit is the policy
/// module's closed typed vocabulary, which keeps an editor from smuggling
/// arbitrary JSON-pointer writes through the control plane.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveRequest {
    expected_revision_id: i64,
    edit: PolicyEdit,
    #[serde(default)]
    description: Option<String>,
}

pub(crate) async fn save(State(server): State<Server>, request: Request) -> Response {
    let request = match decode_request::<SaveRequest>(
        request,
        "routing-save",
        Method::POST,
        MAX_ROUTING_EDIT_BODY,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return *response,
    };
    if request.edit.operations.is_empty() || !description_fits(request.description.as_deref()) {
        return invalid_request("routing-save");
    }
    match server.save_routing_edit(
        request.expected_revision_id,
        &request.edit,
        request.description.as_deref(),
    ) {
        Ok(activation) => activated_response("routing-save", activation),
        Err(error) => activation_error("routing-save", error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WidenRequest {
    expected_revision_id: i64,
    scope: ActivationScope,
    #[serde(default)]
    force_cache_transition: bool,
}

pub(crate) async fn widen(State(server): State<Server>, request: Request) -> Response {
    let request = match decode_request::<WidenRequest>(
        request,
        "routing-widen",
        Method::POST,
        MAX_CONTROL_BODY,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return *response,
    };
    match server.widen_routing_activation(
        request.expected_revision_id,
        request.scope,
        request.force_cache_transition,
    ) {
        Ok(widening) => (
            StatusCode::OK,
            axum::Json(json!({
                "toker": "routing-widen",
                "ok": true,
                "revision_id": widening.revision_id,
                "activation": {
                    "scope": widening.scope,
                    "forced_cache_transition": widening.forced_cache_transition,
                    "widened_ms": widening.widened_ms,
                },
                "lane_impacts": lane_impacts_body(&widening.lane_impacts),
            })),
        )
            .into_response(),
        Err(error) => activation_error("routing-widen", error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidateRequest {
    expected_revision_id: i64,
    edit: PolicyEdit,
}

pub(crate) async fn validate(State(server): State<Server>, request: Request) -> Response {
    let request = match decode_request::<ValidateRequest>(
        request,
        "routing-validate",
        Method::POST,
        MAX_ROUTING_EDIT_BODY,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return *response,
    };
    match server.validate_routing_edit(request.expected_revision_id, &request.edit) {
        Ok(validation) => (
            StatusCode::OK,
            axum::Json(json!({
                "toker": "routing-validate",
                "ok": true,
                "active_revision_id": validation.active_revision_id,
                "valid": validation.valid,
                "diagnostics": diagnostics_body(validation.diagnostics),
            })),
        )
            .into_response(),
        Err(error) => activation_error("routing-validate", error),
    }
}

/// What a preview does. `resolve` traces one request, `replay_recent`
/// compares the last 24 hours' routing under an unsaved edit with how it
/// stands, and `recent_requests` lists what a preview can be taken from.
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum PreviewRequest {
    Resolve {
        #[serde(default)]
        expected_revision_id: Option<i64>,
        #[serde(default)]
        against: AgainstBody,
        request: SubjectBody,
    },
    ReplayRecent {
        #[serde(default)]
        expected_revision_id: Option<i64>,
        edit: PolicyEdit,
    },
    RecentRequests {
        #[serde(default)]
        limit: Option<u32>,
    },
}

#[derive(Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum AgainstBody {
    #[default]
    Active,
    Edit {
        edit: PolicyEdit,
    },
    Revision {
        revision_id: i64,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SubjectBody {
    Invented {
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
        asks_server_safeguards: bool,
        #[serde(default)]
        binding_affinity: Option<BindingId>,
    },
    Recorded {
        request_id: i64,
    },
}

pub(crate) async fn preview(State(server): State<Server>, request: Request) -> Response {
    let request = match decode_request::<PreviewRequest>(
        request,
        "routing-preview",
        Method::POST,
        MAX_ROUTING_EDIT_BODY,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return *response,
    };
    match request {
        PreviewRequest::Resolve {
            expected_revision_id,
            against,
            request,
        } => {
            let against = match against {
                AgainstBody::Active => PreviewAgainst::Active,
                AgainstBody::Edit { edit } => PreviewAgainst::Edit(edit),
                AgainstBody::Revision { revision_id } => PreviewAgainst::Revision(revision_id),
            };
            let subject = match request {
                SubjectBody::Invented {
                    protocol,
                    profile,
                    model,
                    lane_key,
                    prompt_bound,
                    message_count,
                    tool_count,
                    required_capabilities,
                    asks_server_safeguards,
                    binding_affinity,
                } => PreviewSubject::Invented(InventedRequest {
                    protocol,
                    profile,
                    model,
                    lane_key,
                    prompt_bound,
                    message_count,
                    tool_count,
                    required_capabilities,
                    asks_server_safeguards,
                    binding_affinity,
                }),
                SubjectBody::Recorded { request_id } => PreviewSubject::Recorded(request_id),
            };
            match server.preview_routing(RoutingPreviewRequest {
                expected_revision_id,
                against,
                subject,
            }) {
                Ok(preview) => {
                    let (request, result) = match preview.outcome {
                        PreviewOutcome::Resolved { facts, result } => (facts_body(&facts), result),
                        PreviewOutcome::NotReplayable { reason } => (
                            Value::Null,
                            json!({ "outcome": "not_replayable", "reason": reason.code() }),
                        ),
                    };
                    (
                        StatusCode::OK,
                        axum::Json(json!({
                            "toker": "routing-preview",
                            "ok": true,
                            "mode": "resolve",
                            "active_revision_id": preview.active_revision_id,
                            "against_revision_id": preview.against_revision_id,
                            "diagnostics": diagnostics_body(preview.diagnostics),
                            "request": request,
                            "result": result,
                        })),
                    )
                        .into_response()
                }
                Err(error) => activation_error("routing-preview", error),
            }
        }
        PreviewRequest::ReplayRecent {
            expected_revision_id,
            edit,
        } => match server.replay_recent_routing(expected_revision_id, &edit) {
            Ok(replay) => (
                StatusCode::OK,
                axum::Json(json!({
                    "toker": "routing-preview",
                    "ok": true,
                    "mode": "replay_recent",
                    "active_revision_id": replay.active_revision_id,
                    "window": { "since_ms": replay.since_ms, "until_ms": replay.until_ms },
                    "replayed": replay.replayed,
                    "unreplayable": replay.unreplayable,
                    "truncated": replay.truncated,
                    "changed_total": replay.changed_total,
                    "changed": replay.changed.iter().map(replay_change_body).collect::<Vec<_>>(),
                    "diagnostics": diagnostics_body(replay.diagnostics),
                })),
            )
                .into_response(),
            Err(error) => activation_error("routing-preview", error),
        },
        PreviewRequest::RecentRequests { limit } => {
            let limit = limit
                .unwrap_or(DEFAULT_RECENT_REQUESTS)
                .clamp(1, MAX_RECENT_REQUESTS);
            match server.recent_routing_requests(limit) {
                Ok(requests) => (
                    StatusCode::OK,
                    axum::Json(json!({
                        "toker": "routing-preview",
                        "ok": true,
                        "mode": "recent_requests",
                        "requests": requests.iter().map(recent_request_body).collect::<Vec<_>>(),
                    })),
                )
                    .into_response(),
                Err(error) => store_error(error),
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReactivateRequest {
    expected_revision_id: i64,
    revision_id: i64,
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
    if !description_fits(request.description.as_deref()) {
        return invalid_request("routing-reactivate");
    }
    match server.reactivate_routing_policy(
        request.expected_revision_id,
        request.revision_id,
        request.description.as_deref(),
    ) {
        Ok(activation) => activated_response("routing-reactivate", activation),
        Err(error) => activation_error("routing-reactivate", error),
    }
}

/// How an operation made against the active revision fails. Every one of
/// these changed nothing.
fn activation_error(operation: &str, error: RoutingControlError) -> Response {
    match error {
        RoutingControlError::StaleRevision { live_revision_id } => routing_error(
            StatusCode::CONFLICT,
            operation,
            "stale_revision",
            Some(json!({ "live_revision_id": live_revision_id })),
        ),
        RoutingControlError::NotAWidening {
            scope,
            forced_cache_transition,
        } => routing_error(
            StatusCode::CONFLICT,
            operation,
            "not_a_widening",
            Some(json!({
                "current": { "scope": scope, "forced_cache_transition": forced_cache_transition },
            })),
        ),
        RoutingControlError::MissingRevision(revision_id) => routing_error(
            StatusCode::NOT_FOUND,
            operation,
            "revision_not_found",
            Some(json!({ "revision_id": revision_id })),
        ),
        RoutingControlError::RequestNotFound(request_id) => routing_error(
            StatusCode::NOT_FOUND,
            operation,
            "request_not_found",
            Some(json!({ "request_id": request_id })),
        ),
        RoutingControlError::ForcedTransitionRequired { impacts } => routing_error(
            StatusCode::CONFLICT,
            operation,
            "forced_cache_transition_required",
            Some(json!({ "lane_impacts": lane_impacts_body(&impacts) })),
        ),
        RoutingControlError::Compilation(diagnostics) => invalid_policy(diagnostics),
        RoutingControlError::Edit(error) => edit_error_response(operation, &error),
        error => routing_internal_error(error),
    }
}

/// Edit errors name record kinds, ids and policy paths only, so the text is
/// safe to return and is the only way a terminal can say what was wrong.
fn edit_refusal_body(error: &PolicyEditError) -> Value {
    let code = match error {
        PolicyEditError::RecordNotFound { .. }
        | PolicyEditError::DefaultNotFound
        | PolicyEditError::CompactionNotFound
        | PolicyEditError::CandidateNotFound { .. } => "record_not_found",
        PolicyEditError::InvalidDocument(_) => "invalid_policy",
        _ => "invalid_policy_edit",
    };
    json!({ "error": code, "detail": error.to_string() })
}

fn edit_error_response(operation: &str, error: &PolicyEditError) -> Response {
    let body = edit_refusal_body(error);
    let code = body["error"].as_str().unwrap_or("invalid_policy_edit");
    let (status, operation) = match code {
        "record_not_found" => (StatusCode::NOT_FOUND, operation),
        "invalid_policy" => (StatusCode::UNPROCESSABLE_ENTITY, "routing-control"),
        _ => (StatusCode::UNPROCESSABLE_ENTITY, operation),
    };
    routing_error(
        status,
        operation,
        code,
        Some(json!({ "detail": error.to_string() })),
    )
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
            "activation": activation_body(&snapshot.state, &snapshot.active_revision),
            "inventory_generation": snapshot.inventory_generation,
            "offers": snapshot.inventory.iter().map(offer_body).collect::<Vec<_>>(),
            "lane_impacts": lane_impacts_body(&snapshot.lane_impacts),
            "publications": snapshot.publications.into_iter().map(publication_body).collect::<Vec<_>>(),
            "derived": snapshot.derived,
            "hit_counts": hit_counts_body(&snapshot.hit_counts),
        })),
    )
        .into_response()
}

/// The scope and force consent the active revision's lanes are governed by:
/// its own, as last widened where it was.
fn activation_body(state: &crate::store::RoutingPolicyState, revision: &PolicyRevision) -> Value {
    let (scope, forced_cache_transition) = state.effective_activation(revision);
    json!({
        "scope": scope,
        "forced_cache_transition": forced_cache_transition,
        "widened_ms": state.widened.map(|widening| widening.widened_ms),
    })
}

fn activated_response(operation: &str, activation: crate::server::RoutingActivation) -> Response {
    (
        StatusCode::OK,
        axum::Json(json!({
            "toker": operation,
            "ok": true,
            "active_revision": revision_body(&activation.activation.revision),
            "activation": activation_body(
                &activation.activation.state,
                &activation.activation.revision,
            ),
            "lane_impacts": lane_impacts_body(&activation.lane_impacts),
            "publications": activation.activation.publications.into_iter().map(publication_body).collect::<Vec<_>>(),
        })),
    )
        .into_response()
}

fn hit_counts_body(counts: &HitCounts) -> Value {
    json!({
        "since_ms": counts.since_ms,
        "until_ms": counts.until_ms,
        "coverage": match counts.coverage {
            HitCoverage::Complete => "complete",
            HitCoverage::Floor => "floor",
            HitCoverage::NotRecorded => "not_recorded",
        },
        "recorded_requests": counts.recorded_requests,
        "unrecorded_requests": counts.unrecorded_requests,
        "presentations": counts
            .presentations
            .iter()
            .map(|(id, hits)| (id.to_string(), json!(hits)))
            .collect::<serde_json::Map<_, _>>(),
        "rewrites": counts
            .rewrites
            .iter()
            .map(|(id, hits)| (id.to_string(), json!(hits)))
            .collect::<serde_json::Map<_, _>>(),
        "defaults": counts
            .defaults
            .iter()
            .map(|default| json!({ "scope": default.scope, "hits": default.hits }))
            .collect::<Vec<_>>(),
        "compactions": counts.compactions,
        "unrouted": counts.unrouted,
    })
}

/// The facts the resolver was given, content-free.
fn facts_body(facts: &RecordedFacts) -> Value {
    json!({
        "profile": facts.profile.as_ref().map(ProfileId::as_str),
        "protocol": facts.protocol,
        "model": facts.requested_model,
        "prompt_bound": facts.prompt_bound,
        "message_count": facts.message_count,
        "tool_count": facts.tool_count,
        "required_capabilities": facts.required_capabilities,
        "asks_server_safeguards": facts.asks_server_safeguards,
        "binding_affinity": facts.binding_affinity,
        "class": crate::policy::facts::class_name(facts.class),
        "lane": lane_body(&facts.lane),
    })
}

/// The lane's state as the resolver was given it; null where the row
/// predates its recording.
fn lane_body(lane: &crate::policy::facts::RecordedLane) -> Value {
    let Some(state) = crate::policy::facts::lane_name(lane) else {
        return Value::Null;
    };
    let crate::policy::facts::RecordedLane::Read(lane) = lane else {
        return json!({ "state": state });
    };
    json!({
        "state": state,
        "pin": lane.pin.as_ref().map(|pin| json!({
            "route_fingerprint": pin.route_fingerprint.to_string(),
            "candidate_id": pin.candidate_id.to_string(),
            "binding_id": pin.binding_id.to_string(),
            "effective_model": pin.effective_model,
        })),
        "forced_from": lane.forced_from,
        "forced_to": lane.forced_to,
    })
}

fn replay_outcome_body(outcome: &ReplayOutcome) -> Value {
    json!({
        "outcome": outcome.outcome,
        "route_id": outcome.route_id,
        "candidate_id": outcome.candidate_id,
        "binding_id": outcome.binding_id,
        "effective_model": outcome.effective_model,
        "failure": outcome.failure,
        "matched": outcome.matched,
    })
}

fn replay_change_body(change: &ReplayChange) -> Value {
    json!({
        "request_id": change.request_id,
        "ts_ms": change.ts_ms,
        "request": facts_body(&change.facts),
        "under_active": replay_outcome_body(&change.under_active),
        "under_edit": replay_outcome_body(&change.under_edit),
    })
}

fn recent_request_body(request: &RecentRequest) -> Value {
    let reason = NotReplayable::of(request.facts.as_ref());
    json!({
        "id": request.id,
        "ts_ms": request.ts_ms,
        "policy_revision_id": request.policy_revision_id,
        "replayable": request.replayable(),
        "not_replayable_reason": reason.map(NotReplayable::code),
        "kind": request.kind,
        "requested_model": request.requested_model,
        "routed": {
            "route_id": request.route_id,
            "candidate_id": request.candidate_id,
            "binding_id": request.binding_id,
            "effective_model": request.effective_model,
        },
        "matched": request.matched.as_ref().map(|matched| json!({
            "kind": matched.kind.as_str(),
            "scope": matched.scope.map(|scope| scope.as_str()),
            "id": matched.id,
        })),
        "request": request.facts.as_ref().map(facts_body),
    })
}

fn invalid_policy(diagnostics: Vec<crate::policy::Diagnostic>) -> Response {
    routing_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "routing-control",
        "invalid_policy",
        Some(json!({ "diagnostics": diagnostics_body(diagnostics) })),
    )
}

/// The revision's own description is not part of the document, so it is
/// bounded here, by the bound the descriptions inside the document carry.
fn description_fits(description: Option<&str>) -> bool {
    description.is_none_or(|value| value.len() <= crate::policy::MAX_DESCRIPTION_BYTES)
}

fn invalid_request(operation: &str) -> Response {
    routing_error(StatusCode::BAD_REQUEST, operation, "invalid_request", None)
}

fn diagnostics_body(diagnostics: Vec<crate::policy::Diagnostic>) -> Vec<Value> {
    diagnostics
        .into_iter()
        .map(|diagnostic| {
            // A sentence and a fix for the form to show beside the field the
            // path names (spec: UTUI#editing).
            let explanation = crate::policy::explain(diagnostic.code.0.as_str());
            json!({
                "code": diagnostic.code,
                "severity": diagnostic.severity,
                "record_id": diagnostic.record_id,
                "path": diagnostic.path.as_str(),
                "sentence": explanation.map(|explanation| explanation.sentence),
                "fix": explanation.map(|explanation| explanation.fix),
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

/// Each revision with what it changed from the one before it, which is the
/// one with the next lower number: the active pointer never moves backwards,
/// so that is what was live when it was made. A revision's own `parent` is
/// what it was made from, which for a reactivation is the one copied.
fn revision_summaries(
    mut revisions: Vec<PolicyRevision>,
    baseline: Option<&PolicyRevision>,
) -> Vec<Value> {
    revisions.sort_by_key(|revision| revision.revision_id);
    let mut bodies = Vec::with_capacity(revisions.len());
    // The page's oldest is diffed against the revision below it, which is
    // outside the page; without one, it is the first revision there is.
    let mut before: Option<&PolicyRevision> = baseline;
    for revision in &revisions {
        let after = revision.document.document();
        let changes =
            before.map(|before| revision_diff::changes(before.document.document(), after));
        let summary = match (&changes, revision.source, revision.parent_revision_id) {
            (None, ..) => revision_diff::first_summary(after),
            (Some(_), crate::store::PolicyRevisionSource::Reactivation, Some(parent)) => {
                format!("reactivate rev {parent}")
            }
            (Some(changes), ..) => revision_diff::summary(changes),
        };
        let mut body = revision_summary_body(revision);
        body["summary"] = json!(summary);
        body["changes"] = json!(changes.unwrap_or_default());
        bodies.push(body);
        before = Some(revision);
    }
    bodies
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

pub(super) fn protocol_name(protocol: FrontendProtocol) -> &'static str {
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
        crate::store::PolicyRevisionSource::Save => "save",
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
