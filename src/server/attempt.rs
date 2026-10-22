//! Adapter-neutral inference-attempt outcomes before frontend commitment.
//!
//! Provider adapters may transiently inspect error bodies and events, but only
//! the content-free classification leaves the adapter. Unknown or
//! request-specific failures remain unclassified and cannot authorize fallback.

use axum::http::StatusCode;
use serde_json::Value;

use crate::observe::{SseEvent, SseSplitter};
use crate::policy::FailureClass;
use crate::providers::codex::{ResponseError, ResponseEvent, ResponsesSse};
use crate::store::{AttemptGroupId, AttemptIndex, AttemptProvenance, AttemptReason};

use super::proxy::{Buffered, PRECOMMIT_BODY_CAP, buffer_up_to};

/// How a failed attempt became observable to the coordinator.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureMechanism {
    HttpStatus,
    InBandEvent,
    TransportBeforeAccept,
    AmbiguousTransport,
    ResponseProtocol,
    LocalPreparation,
}

/// Content-free evidence retained before a frontend response is committed.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttemptFailure {
    pub(crate) class: Option<FailureClass>,
    pub(crate) mechanism: FailureMechanism,
    pub(crate) status: Option<u16>,
}

/// Why an accepted response crossed the commitment boundary.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitEvidence {
    CanonicalEvent,
    OtherValidFrame,
    ByteCapReached,
    NonStreamingComplete,
}

/// Result of one actual send before the coordinator returns a response head.
/// A classified provider failure retains its response for forwarding when no
/// fallback is selected; a local response-protocol failure has no safe response.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum PrecommitOutcome<T> {
    Accepted {
        response: T,
        evidence: CommitEvidence,
    },
    Failure {
        failure: AttemptFailure,
        response: Option<T>,
    },
}

pub(crate) fn transport_failure_type(error: &reqwest::Error) -> &'static str {
    if error.is_connect() {
        "transport_before_accept"
    } else {
        "ambiguous_transport"
    }
}

pub(crate) fn initial_provenance() -> AttemptProvenance {
    AttemptProvenance {
        attempt_group_id: AttemptGroupId::fresh(),
        attempt_index: AttemptIndex::zero(),
        reason: AttemptReason::Initial,
    }
}

#[allow(dead_code)]
pub(crate) fn next_provenance(
    current: &AttemptProvenance,
    failure: FailureClass,
) -> Option<AttemptProvenance> {
    Some(AttemptProvenance {
        attempt_group_id: current.attempt_group_id.clone(),
        attempt_index: current.attempt_index.next()?,
        reason: failure.into(),
    })
}

/// Classify a complete Anthropic-native error without inspecting message text.
#[allow(dead_code)]
pub(crate) fn classify_anthropic_error(status: StatusCode, body: &Value) -> AttemptFailure {
    let kind = anthropic_error_kind(body);
    let class = match kind {
        Some("api_error") if !status.is_server_error() => None,
        _ => classify_anthropic_kind(kind),
    }
    .or_else(|| {
        if status.as_u16() == 529 {
            Some(FailureClass::Overload)
        } else if status.is_server_error() {
            Some(FailureClass::ServerError)
        } else {
            None
        }
    });
    AttemptFailure {
        class,
        mechanism: FailureMechanism::HttpStatus,
        status: Some(status.as_u16()),
    }
}

fn classify_anthropic_event(body: &Value) -> Option<AttemptFailure> {
    (body.get("type").and_then(Value::as_str) == Some("error")).then(|| AttemptFailure {
        class: classify_anthropic_kind(anthropic_error_kind(body)),
        mechanism: FailureMechanism::InBandEvent,
        status: None,
    })
}

fn anthropic_error_kind(body: &Value) -> Option<&str> {
    body.get("error")
        .unwrap_or(body)
        .get("type")
        .and_then(Value::as_str)
}

fn classify_anthropic_kind(kind: Option<&str>) -> Option<FailureClass> {
    match kind {
        Some("authentication_error") => Some(FailureClass::Authentication),
        Some("not_found_error") => Some(FailureClass::ModelNotFound),
        Some("rate_limit_error") => Some(FailureClass::RateLimit),
        Some("overloaded_error") => Some(FailureClass::Overload),
        Some("api_error") => Some(FailureClass::ServerError),
        _ => None,
    }
}

/// Classify OpenAI-shaped errors. Native codes take precedence over broad
/// types and HTTP status so quota and model absence remain distinct.
#[allow(dead_code)]
pub(crate) fn classify_openai_error(status: StatusCode, body: &Value) -> AttemptFailure {
    let error = body.get("error").unwrap_or(body);
    let code = error.get("code").and_then(value_code);
    let kind = error.get("type").and_then(Value::as_str);
    let class = classify_symbolic(code.or(kind)).or_else(|| match status.as_u16() {
        401 => Some(FailureClass::Authentication),
        429 => Some(FailureClass::RateLimit),
        500 | 502 | 503 | 504 => Some(FailureClass::ServerError),
        _ => None,
    });
    AttemptFailure {
        class,
        mechanism: FailureMechanism::HttpStatus,
        status: Some(status.as_u16()),
    }
}

/// Classify a Responses HTTP or in-band error through the same closed table.
#[allow(dead_code)]
pub(crate) fn classify_responses_error(
    status: Option<StatusCode>,
    error: &ResponseError,
    in_band: bool,
) -> AttemptFailure {
    let class = classify_symbolic(error.code.as_deref().or(error.kind.as_deref())).or_else(|| {
        status.and_then(|status| match status.as_u16() {
            401 => Some(FailureClass::Authentication),
            429 => Some(FailureClass::RateLimit),
            500 | 502 | 503 | 504 => Some(FailureClass::ServerError),
            _ => None,
        })
    });
    AttemptFailure {
        class,
        mechanism: if in_band {
            FailureMechanism::InBandEvent
        } else {
            FailureMechanism::HttpStatus
        },
        status: status.map(|status| status.as_u16()),
    }
}

fn classify_openai_event(body: &Value) -> Option<AttemptFailure> {
    body.get("error").map(|_| {
        let mut failure = classify_openai_error(StatusCode::OK, body);
        failure.mechanism = FailureMechanism::InBandEvent;
        failure.status = None;
        failure
    })
}

fn value_code(value: &Value) -> Option<&str> {
    value.as_str()
}

/// Retain a bounded streaming prefix until one complete non-keepalive SSE
/// frame is available, or the byte cap forces commitment. The returned
/// response remains positioned immediately after `bytes`.
#[allow(dead_code)]
pub(crate) async fn precommit_sse(
    mut response: reqwest::Response,
) -> Result<(Buffered, Option<SseEvent>, CommitEvidence), reqwest::Error> {
    let mut bytes = Vec::new();
    let mut splitter = SseSplitter::new();
    loop {
        let Some(chunk) = response.chunk().await? else {
            let event = splitter.finish();
            let evidence = if event.is_some() {
                CommitEvidence::OtherValidFrame
            } else {
                CommitEvidence::NonStreamingComplete
            };
            return Ok((
                Buffered {
                    bytes,
                    overflow: None,
                    rest: None,
                },
                event,
                evidence,
            ));
        };
        let retained = PRECOMMIT_BODY_CAP
            .saturating_sub(bytes.len())
            .min(chunk.len());
        bytes.extend_from_slice(&chunk[..retained]);

        if let Some(event) = splitter.feed(&chunk[..retained]).into_iter().next() {
            return Ok((
                Buffered {
                    bytes,
                    overflow: (retained < chunk.len()).then(|| chunk.slice(retained..)),
                    rest: Some(response),
                },
                Some(event),
                CommitEvidence::OtherValidFrame,
            ));
        }
        if bytes.len() == PRECOMMIT_BODY_CAP {
            return Ok((
                Buffered {
                    bytes,
                    overflow: (retained < chunk.len()).then(|| chunk.slice(retained..)),
                    rest: Some(response),
                },
                None,
                CommitEvidence::ByteCapReached,
            ));
        }
    }
}

pub(crate) async fn precommit_responses(
    response: reqwest::Response,
) -> Result<PrecommitOutcome<reqwest::Response>, reqwest::Error> {
    let status = response.status();
    let headers = response.headers().clone();
    if super::proxy::is_compressed(&headers) {
        return Ok(PrecommitOutcome::Accepted {
            response,
            evidence: CommitEvidence::NonStreamingComplete,
        });
    }
    if !status.is_success() {
        let buffered = buffer_up_to(response, PRECOMMIT_BODY_CAP).await?;
        let complete = buffered.rest.is_none();
        let failure = complete.then(|| {
            let body = serde_json::from_slice::<Value>(&buffered.bytes).unwrap_or(Value::Null);
            let error: ResponseError =
                serde_json::from_value(body.get("error").cloned().unwrap_or(body))
                    .unwrap_or_default();
            classify_responses_error(Some(status), &error, false)
        });
        let response = super::proxy::rebuilt_response(status, headers, buffered);
        return Ok(match failure {
            Some(failure) => PrecommitOutcome::Failure {
                failure,
                response: Some(response),
            },
            None => PrecommitOutcome::Accepted {
                response,
                evidence: CommitEvidence::ByteCapReached,
            },
        });
    }
    if super::proxy::is_event_stream(&headers) {
        let (buffered, event, evidence) = precommit_sse(response).await?;
        let parsed = event.as_ref().and_then(|event| {
            let mut parser = ResponsesSse::new();
            parser.feed(&sse_event_bytes(event)).into_iter().next()
        });
        let failure = parsed.as_ref().and_then(|event| match event {
            ResponseEvent::Failed { error, .. } | ResponseEvent::Error { error } => {
                Some(classify_responses_error(None, error, true))
            }
            _ => None,
        });
        if failure.is_none()
            && evidence != CommitEvidence::ByteCapReached
            && (event.is_none() || parsed.is_none())
        {
            return Ok(protocol_failure(status));
        }
        let response = super::proxy::rebuilt_response(status, headers, buffered);
        return Ok(match failure {
            Some(failure) => PrecommitOutcome::Failure {
                failure,
                response: Some(response),
            },
            None => PrecommitOutcome::Accepted {
                response,
                evidence: if parsed.is_some() {
                    CommitEvidence::CanonicalEvent
                } else {
                    evidence
                },
            },
        });
    }
    Ok(PrecommitOutcome::Accepted {
        response,
        evidence: CommitEvidence::NonStreamingComplete,
    })
}

fn sse_event_bytes(event: &SseEvent) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(name) = &event.event {
        bytes.extend_from_slice(b"event: ");
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(b'\n');
    }
    for line in &event.data_lines {
        bytes.extend_from_slice(b"data: ");
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
    }
    bytes.push(b'\n');
    bytes
}

pub(crate) async fn precommit_openai_chat(
    response: reqwest::Response,
) -> Result<PrecommitOutcome<reqwest::Response>, reqwest::Error> {
    let status = response.status();
    let headers = response.headers().clone();
    if super::proxy::is_compressed(&headers) {
        return Ok(PrecommitOutcome::Accepted {
            response,
            evidence: CommitEvidence::NonStreamingComplete,
        });
    }
    if !status.is_success() {
        let buffered = buffer_up_to(response, PRECOMMIT_BODY_CAP).await?;
        let complete = buffered.rest.is_none();
        let failure = complete.then(|| {
            let body = serde_json::from_slice::<Value>(&buffered.bytes).unwrap_or(Value::Null);
            classify_openai_error(status, &body)
        });
        let response = super::proxy::rebuilt_response(status, headers, buffered);
        return Ok(match failure {
            Some(failure) => PrecommitOutcome::Failure {
                failure,
                response: Some(response),
            },
            None => PrecommitOutcome::Accepted {
                response,
                evidence: CommitEvidence::ByteCapReached,
            },
        });
    }
    if super::proxy::is_event_stream(&headers) {
        let (buffered, event, evidence) = precommit_sse(response).await?;
        let parsed = event
            .as_ref()
            .map(|event| serde_json::from_str::<Value>(&event.data()));
        let failure = parsed
            .as_ref()
            .and_then(|parsed| parsed.as_ref().ok())
            .and_then(classify_openai_event);
        let canonical = failure.is_none()
            && event.as_ref().is_some_and(|event| {
                !crate::translate::openai_chat_backend::OpenAiChatResponseStream::new()
                    .feed_sse(event)
                    .is_empty()
            });
        if failure.is_none()
            && evidence != CommitEvidence::ByteCapReached
            && (event.is_none() || parsed.as_ref().is_some_and(Result::is_err))
        {
            return Ok(protocol_failure(status));
        }
        let response = super::proxy::rebuilt_response(status, headers, buffered);
        return Ok(match failure {
            Some(failure) => PrecommitOutcome::Failure {
                failure,
                response: Some(response),
            },
            None => PrecommitOutcome::Accepted {
                response,
                evidence: if canonical {
                    CommitEvidence::CanonicalEvent
                } else {
                    evidence
                },
            },
        });
    }
    Ok(PrecommitOutcome::Accepted {
        response,
        evidence: CommitEvidence::NonStreamingComplete,
    })
}

fn protocol_failure<T>(status: StatusCode) -> PrecommitOutcome<T> {
    PrecommitOutcome::Failure {
        failure: AttemptFailure {
            class: None,
            mechanism: FailureMechanism::ResponseProtocol,
            status: Some(status.as_u16()),
        },
        response: None,
    }
}

pub(crate) async fn precommit_anthropic(
    response: reqwest::Response,
) -> Result<PrecommitOutcome<reqwest::Response>, reqwest::Error> {
    let status = response.status();
    let headers = response.headers().clone();
    if super::proxy::is_compressed(&headers) {
        return Ok(PrecommitOutcome::Accepted {
            response,
            evidence: CommitEvidence::NonStreamingComplete,
        });
    }
    if !status.is_success() {
        let buffered = buffer_up_to(response, PRECOMMIT_BODY_CAP).await?;
        let complete = buffered.rest.is_none();
        let failure = complete.then(|| {
            let body = serde_json::from_slice::<Value>(&buffered.bytes).unwrap_or(Value::Null);
            classify_anthropic_error(status, &body)
        });
        let rebuilt = super::proxy::rebuilt_response(status, headers, buffered);
        return Ok(match failure {
            Some(failure) => PrecommitOutcome::Failure {
                failure,
                response: Some(rebuilt),
            },
            None => PrecommitOutcome::Accepted {
                response: rebuilt,
                evidence: CommitEvidence::ByteCapReached,
            },
        });
    }
    if super::proxy::is_event_stream(&headers) {
        let (buffered, event, evidence) = precommit_sse(response).await?;
        let parsed = event
            .as_ref()
            .map(|event| serde_json::from_str::<Value>(&event.data()));
        let failure = parsed
            .as_ref()
            .and_then(|parsed| parsed.as_ref().ok())
            .and_then(classify_anthropic_event);
        let canonical = failure.is_none()
            && event.as_ref().is_some_and(|event| {
                !crate::translate::AnthropicResponseStream::new()
                    .feed_sse(event)
                    .is_empty()
            });
        if failure.is_none()
            && evidence != CommitEvidence::ByteCapReached
            && (event.is_none() || parsed.as_ref().is_some_and(Result::is_err))
        {
            return Ok(protocol_failure(status));
        }
        let response = super::proxy::rebuilt_response(status, headers, buffered);
        return Ok(match failure {
            Some(failure) => PrecommitOutcome::Failure {
                failure,
                response: Some(response),
            },
            None => PrecommitOutcome::Accepted {
                response,
                evidence: if canonical {
                    CommitEvidence::CanonicalEvent
                } else {
                    evidence
                },
            },
        });
    }
    Ok(PrecommitOutcome::Accepted {
        response,
        evidence: CommitEvidence::NonStreamingComplete,
    })
}

fn classify_symbolic(value: Option<&str>) -> Option<FailureClass> {
    match value?.to_ascii_lowercase().as_str() {
        "authentication_error" | "invalid_api_key" | "unauthorized" | "401" => {
            Some(FailureClass::Authentication)
        }
        "model_not_found" | "unknown_model" => Some(FailureClass::ModelNotFound),
        "insufficient_quota" | "provider_quota" | "usage_limit_reached" => {
            Some(FailureClass::ProviderQuota)
        }
        "rate_limit_error" | "rate_limit_exceeded" | "429" => Some(FailureClass::RateLimit),
        "overloaded_error" | "overload" | "529" => Some(FailureClass::Overload),
        "server_error" | "api_error" | "500" | "502" | "503" | "504" => {
            Some(FailureClass::ServerError)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;

    #[test]
    fn fallback_provenance_stays_in_one_group_and_advances_reason() {
        let initial = initial_provenance();
        let fallback = next_provenance(&initial, FailureClass::Overload).unwrap();

        assert_eq!(fallback.attempt_group_id, initial.attempt_group_id);
        assert_eq!(fallback.attempt_index.get(), 1);
        assert_eq!(fallback.reason, AttemptReason::Overload);
    }

    #[test]
    fn anthropic_native_types_classify_without_message_heuristics() {
        for (kind, expected) in [
            ("authentication_error", Some(FailureClass::Authentication)),
            ("not_found_error", Some(FailureClass::ModelNotFound)),
            ("rate_limit_error", Some(FailureClass::RateLimit)),
            ("overloaded_error", Some(FailureClass::Overload)),
            ("api_error", None),
            ("invalid_request_error", None),
        ] {
            let failure = classify_anthropic_error(
                StatusCode::BAD_REQUEST,
                &json!({"error":{"type":kind,"message":"private fixture"}}),
            );
            assert_eq!(failure.class, expected, "{kind}");
        }
        let server = classify_anthropic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"error":{"type":"api_error"}}),
        );
        assert_eq!(server.class, Some(FailureClass::ServerError));
    }

    #[test]
    fn openai_code_precedes_broad_type_and_status() {
        let quota = classify_openai_error(
            StatusCode::TOO_MANY_REQUESTS,
            &json!({"error":{"type":"rate_limit_error","code":"insufficient_quota"}}),
        );
        assert_eq!(quota.class, Some(FailureClass::ProviderQuota));
        let model = classify_openai_error(
            StatusCode::BAD_REQUEST,
            &json!({"error":{"type":"invalid_request_error","code":"model_not_found"}}),
        );
        assert_eq!(model.class, Some(FailureClass::ModelNotFound));
        let request = classify_openai_error(
            StatusCode::BAD_REQUEST,
            &json!({"error":{"type":"invalid_request_error"}}),
        );
        assert_eq!(request.class, None);
    }

    #[test]
    fn responses_http_and_in_band_errors_share_the_symbolic_table() {
        let error = ResponseError {
            code: Some("server_error".to_owned()),
            ..ResponseError::default()
        };
        assert_eq!(
            classify_responses_error(Some(StatusCode::INTERNAL_SERVER_ERROR), &error, false).class,
            Some(FailureClass::ServerError)
        );
        let in_band = classify_responses_error(None, &error, true);
        assert_eq!(in_band.class, Some(FailureClass::ServerError));
        assert_eq!(in_band.mechanism, FailureMechanism::InBandEvent);
    }

    fn streaming_response<S, E>(stream: S) -> reqwest::Response
    where
        S: futures::Stream<Item = Result<bytes::Bytes, E>> + Send + Sync + 'static,
        E: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
    {
        let response = axum::http::Response::builder()
            .status(StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
            .body(reqwest::Body::wrap_stream(stream))
            .unwrap();
        reqwest::Response::from(response)
    }

    #[tokio::test]
    async fn streaming_precommit_returns_on_the_first_complete_frame() {
        let first = futures::stream::iter([
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(b": keepalive\n\nevent: message")),
            Ok(bytes::Bytes::from_static(
                b"_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n",
            )),
        ]);
        let body = first.chain(futures::stream::pending());
        let (buffered, event, evidence) = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            precommit_sse(streaming_response(body)),
        )
        .await
        .expect("first frame commits without waiting for the stream")
        .unwrap();

        assert_eq!(evidence, CommitEvidence::OtherValidFrame);
        assert_eq!(event.unwrap().event.as_deref(), Some("message_start"));
        assert!(buffered.rest.is_some());
        assert!(buffered.overflow.is_none());
    }

    #[tokio::test]
    async fn streaming_precommit_retains_the_cap_crossing_chunk() {
        let chunk = bytes::Bytes::from(vec![b':'; PRECOMMIT_BODY_CAP + 7]);
        let body = futures::stream::iter([Ok::<_, std::io::Error>(chunk)]);
        let (buffered, event, evidence) = precommit_sse(streaming_response(body)).await.unwrap();

        assert_eq!(evidence, CommitEvidence::ByteCapReached);
        assert!(event.is_none());
        assert_eq!(buffered.bytes.len(), PRECOMMIT_BODY_CAP);
        assert_eq!(buffered.overflow.as_ref().map(bytes::Bytes::len), Some(7));
        assert!(buffered.rest.is_some());
    }

    #[tokio::test]
    async fn anthropic_precommit_classifies_first_events_without_changing_bytes() {
        for (frame, expected) in [
            (
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n",
                Ok(CommitEvidence::CanonicalEvent),
            ),
            (
                "event: ping\ndata: {\"type\":\"ping\"}\n\n",
                Ok(CommitEvidence::OtherValidFrame),
            ),
            (
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n",
                Err(FailureClass::Overload),
            ),
        ] {
            let body = futures::stream::iter([Ok::<_, std::io::Error>(
                bytes::Bytes::copy_from_slice(frame.as_bytes()),
            )]);
            let outcome = precommit_anthropic(streaming_response(body)).await.unwrap();
            let response = match (outcome, expected) {
                (PrecommitOutcome::Accepted { response, evidence }, Ok(expected_evidence)) => {
                    assert_eq!(evidence, expected_evidence);
                    response
                }
                (
                    PrecommitOutcome::Failure {
                        failure,
                        response: Some(response),
                    },
                    Err(expected_class),
                ) => {
                    assert_eq!(failure.class, Some(expected_class));
                    assert_eq!(failure.mechanism, FailureMechanism::InBandEvent);
                    response
                }
                (outcome, expected) => panic!("unexpected {outcome:?} for {expected:?}"),
            };
            assert_eq!(response.bytes().await.unwrap(), frame.as_bytes());
        }
    }

    #[tokio::test]
    async fn responses_precommit_classifies_first_events_without_changing_bytes() {
        for (frame, expected) in [
            (
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-1\"}}\n\n",
                Ok(CommitEvidence::CanonicalEvent),
            ),
            (
                "event: response.extension\ndata: {\"type\":\"response.extension\"}\n\n",
                Ok(CommitEvidence::CanonicalEvent),
            ),
            (
                "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"overloaded_error\"}}}\n\n",
                Err(FailureClass::Overload),
            ),
        ] {
            let body = futures::stream::iter([Ok::<_, std::io::Error>(
                bytes::Bytes::copy_from_slice(frame.as_bytes()),
            )]);
            let outcome = precommit_responses(streaming_response(body)).await.unwrap();
            let response = match (outcome, expected) {
                (PrecommitOutcome::Accepted { response, evidence }, Ok(expected_evidence)) => {
                    assert_eq!(evidence, expected_evidence);
                    response
                }
                (
                    PrecommitOutcome::Failure {
                        failure,
                        response: Some(response),
                    },
                    Err(expected_class),
                ) => {
                    assert_eq!(failure.class, Some(expected_class));
                    assert_eq!(failure.mechanism, FailureMechanism::InBandEvent);
                    response
                }
                (outcome, expected) => panic!("unexpected {outcome:?} for {expected:?}"),
            };
            assert_eq!(response.bytes().await.unwrap(), frame.as_bytes());
        }
    }

    #[tokio::test]
    async fn responses_precommit_rejects_invalid_or_absent_first_events() {
        for bytes in [
            &b""[..],
            &b": keepalive\n\n"[..],
            &b"data: {not json}\n\n"[..],
        ] {
            let body = futures::stream::iter([Ok::<_, std::io::Error>(
                bytes::Bytes::copy_from_slice(bytes),
            )]);
            let outcome = precommit_responses(streaming_response(body)).await.unwrap();
            let PrecommitOutcome::Failure { failure, response } = outcome else {
                panic!("invalid first event was accepted: {outcome:?}");
            };
            assert_eq!(failure.class, None);
            assert_eq!(failure.mechanism, FailureMechanism::ResponseProtocol);
            assert!(response.is_none());
        }
    }

    #[tokio::test]
    async fn chat_precommit_classifies_first_events_without_changing_bytes() {
        for (frame, expected) in [
            (
                "data: {\"id\":\"chat-1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
                Ok(CommitEvidence::CanonicalEvent),
            ),
            (
                "data: {\"type\":\"extension\"}\n\n",
                Ok(CommitEvidence::CanonicalEvent),
            ),
            (
                "data: {\"error\":{\"type\":\"rate_limit_error\"}}\n\n",
                Err(FailureClass::RateLimit),
            ),
        ] {
            let body = futures::stream::iter([Ok::<_, std::io::Error>(
                bytes::Bytes::copy_from_slice(frame.as_bytes()),
            )]);
            let outcome = precommit_openai_chat(streaming_response(body))
                .await
                .unwrap();
            let response = match (outcome, expected) {
                (PrecommitOutcome::Accepted { response, evidence }, Ok(expected_evidence)) => {
                    assert_eq!(evidence, expected_evidence);
                    response
                }
                (
                    PrecommitOutcome::Failure {
                        failure,
                        response: Some(response),
                    },
                    Err(expected_class),
                ) => {
                    assert_eq!(failure.class, Some(expected_class));
                    assert_eq!(failure.mechanism, FailureMechanism::InBandEvent);
                    response
                }
                (outcome, expected) => panic!("unexpected {outcome:?} for {expected:?}"),
            };
            assert_eq!(response.bytes().await.unwrap(), frame.as_bytes());
        }
    }

    #[tokio::test]
    async fn chat_precommit_rejects_invalid_or_absent_first_events() {
        for bytes in [
            &b""[..],
            &b": keepalive\n\n"[..],
            &b"data: {not json}\n\n"[..],
        ] {
            let body = futures::stream::iter([Ok::<_, std::io::Error>(
                bytes::Bytes::copy_from_slice(bytes),
            )]);
            let outcome = precommit_openai_chat(streaming_response(body))
                .await
                .unwrap();
            let PrecommitOutcome::Failure { failure, response } = outcome else {
                panic!("invalid first event was accepted: {outcome:?}");
            };
            assert_eq!(failure.class, None);
            assert_eq!(failure.mechanism, FailureMechanism::ResponseProtocol);
            assert!(response.is_none());
        }
    }

    #[tokio::test]
    async fn anthropic_precommit_rejects_invalid_or_absent_first_events() {
        for bytes in [
            &b""[..],
            &b": keepalive\n\n"[..],
            &b"data: {not json}\n\n"[..],
            &b"data: {\"type\":\"message_start\""[..],
        ] {
            let body = futures::stream::iter([Ok::<_, std::io::Error>(
                bytes::Bytes::copy_from_slice(bytes),
            )]);
            let outcome = precommit_anthropic(streaming_response(body)).await.unwrap();
            let PrecommitOutcome::Failure { failure, response } = outcome else {
                panic!("invalid first event was accepted: {outcome:?}");
            };
            assert_eq!(failure.class, None);
            assert_eq!(failure.mechanism, FailureMechanism::ResponseProtocol);
            assert!(response.is_none());
        }
    }

    #[tokio::test]
    async fn streaming_precommit_surfaces_body_failure_before_a_frame() {
        let body = futures::stream::iter([Err::<bytes::Bytes, _>(std::io::Error::other(
            "fixture failure",
        ))]);
        assert!(precommit_sse(streaming_response(body)).await.is_err());
    }

    #[test]
    fn status_only_not_found_and_numeric_bad_request_stay_unclassified() {
        assert_eq!(
            classify_openai_error(StatusCode::NOT_FOUND, &json!({"error":{}})).class,
            None
        );
        let error = ResponseError {
            code: Some("400".to_owned()),
            ..ResponseError::default()
        };
        assert_eq!(
            classify_responses_error(Some(StatusCode::BAD_REQUEST), &error, false).class,
            None
        );
    }
}
