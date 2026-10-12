//! Adapter-neutral inference-attempt outcomes before frontend commitment.
//!
//! Provider adapters may transiently inspect error bodies and events, but only
//! the content-free classification leaves the adapter. Unknown or
//! request-specific failures remain unclassified and cannot authorize fallback.

use axum::http::StatusCode;
use serde_json::Value;

use crate::observe::SseSplitter;
use crate::policy::FailureClass;
use crate::providers::codex::ResponseError;

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
/// Adapter-owned continuations will replace the accepted marker as streaming
/// paths migrate behind this boundary.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum PrecommitOutcome<T> {
    Accepted {
        response: T,
        evidence: CommitEvidence,
    },
    Failure(AttemptFailure),
    Cancelled,
}

/// Classify a complete Anthropic-native error without inspecting message text.
#[allow(dead_code)]
pub(crate) fn classify_anthropic_error(status: StatusCode, body: &Value) -> AttemptFailure {
    let error = body.get("error").unwrap_or(body);
    let kind = error.get("type").and_then(Value::as_str);
    let class = match kind {
        Some("authentication_error") => Some(FailureClass::Authentication),
        Some("not_found_error") => Some(FailureClass::ModelNotFound),
        Some("rate_limit_error") => Some(FailureClass::RateLimit),
        Some("overloaded_error") => Some(FailureClass::Overload),
        Some("api_error") if status.is_server_error() => Some(FailureClass::ServerError),
        _ if status.as_u16() == 529 => Some(FailureClass::Overload),
        _ if status.is_server_error() => Some(FailureClass::ServerError),
        _ => None,
    };
    AttemptFailure {
        class,
        mechanism: FailureMechanism::HttpStatus,
        status: Some(status.as_u16()),
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

fn value_code(value: &Value) -> Option<&str> {
    value.as_str()
}

/// Retain a bounded streaming prefix until one complete non-keepalive SSE
/// frame is available, or the byte cap forces commitment. The returned
/// response remains positioned immediately after `bytes`.
#[allow(dead_code)]
pub(crate) async fn precommit_sse(
    response: reqwest::Response,
) -> Result<(Buffered, CommitEvidence), reqwest::Error> {
    let buffered = buffer_up_to(response, PRECOMMIT_BODY_CAP).await?;
    let mut splitter = SseSplitter::new();
    let has_frame = !splitter.feed(&buffered.bytes).is_empty();
    let evidence = if has_frame {
        CommitEvidence::OtherValidFrame
    } else if buffered.rest.is_some() {
        CommitEvidence::ByteCapReached
    } else {
        CommitEvidence::NonStreamingComplete
    };
    Ok((buffered, evidence))
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
    use serde_json::json;

    #[test]
    fn anthropic_native_types_classify_without_message_heuristics() {
        for (kind, expected) in [
            ("authentication_error", Some(FailureClass::Authentication)),
            ("not_found_error", Some(FailureClass::ModelNotFound)),
            ("rate_limit_error", Some(FailureClass::RateLimit)),
            ("overloaded_error", Some(FailureClass::Overload)),
            ("invalid_request_error", None),
        ] {
            let failure = classify_anthropic_error(
                StatusCode::BAD_REQUEST,
                &json!({"error":{"type":kind,"message":"private fixture"}}),
            );
            assert_eq!(failure.class, expected, "{kind}");
        }
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

    #[tokio::test]
    async fn streaming_precommit_waits_for_a_frame_or_the_fixed_cap() {
        fn response(body: axum::body::Body) -> reqwest::Response {
            let response = axum::http::Response::builder()
                .status(StatusCode::OK)
                .body(reqwest::Body::wrap_stream(body.into_data_stream()))
                .unwrap();
            reqwest::Response::from(response)
        }

        let body = axum::body::Body::from("event: message_start\ndata: {}\n\nrest");
        let (buffered, evidence) = precommit_sse(response(body)).await.unwrap();
        assert_eq!(evidence, CommitEvidence::OtherValidFrame);
        assert!(buffered.rest.is_none());

        let body = axum::body::Body::from(vec![b':'; PRECOMMIT_BODY_CAP + 1]);
        let (buffered, evidence) = precommit_sse(response(body)).await.unwrap();
        assert_eq!(evidence, CommitEvidence::ByteCapReached);
        assert!(buffered.rest.is_some());
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
