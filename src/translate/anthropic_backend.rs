//! Canonical request rendering for Anthropic Messages backend bindings.
//!
//! This is the request-egress half of the backend adapter. It always builds a
//! deterministic wire value from canonical IR, including on a Messages to
//! Messages route. Anthropic and OpenRouter's Messages dialects share the
//! extension replay domain; extensions from unrelated dialects are omitted
//! with content-free losses.

use std::collections::BTreeMap;

use serde_json::{Map, Number, Value, json};

use crate::ir::canonical::{
    CanonBlock, CanonError, CanonErrorKind, CanonEvent, CanonMessage, CanonRole, CanonStopReason,
    CanonSystemPart, CanonTool, CanonToolCall, CanonToolChoice, CanonTurn, CanonicalExtension,
    CanonicalRequest, CanonicalUsage, ToolResultContent,
};
use crate::observe::sse::SseEvent;
use crate::routing::DialectId;
use crate::translate::anthropic_frontend::content_blocks_of;
use crate::translate::{
    Rendered, TranslateError, TranslationLoss, TranslationLossReason, TranslationReport,
};

/// Render a canonical request onto one Messages provider dialect.
pub fn render_anthropic(
    canonical: &CanonicalRequest,
    dialect: DialectId,
) -> Result<Rendered<Value>, TranslateError> {
    if !matches!(
        dialect,
        DialectId::AnthropicMessages | DialectId::OpenRouterMessages
    ) {
        return Err(TranslateError::Malformed {
            reason: format!("{dialect} is not an Anthropic Messages dialect"),
        });
    }
    let model = canonical
        .model
        .as_deref()
        .ok_or_else(|| TranslateError::Malformed {
            reason: "canonical request has no model".to_owned(),
        })?;
    if canonical.tool_choice == CanonToolChoice::None {
        return Err(TranslateError::Malformed {
            reason: "tool_choice type \"none\" has no messages equivalent".to_owned(),
        });
    }

    let mut report = TranslationReport::default();
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(model.to_owned()));
    body.insert(
        "messages".to_owned(),
        Value::Array(
            canonical
                .messages
                .iter()
                .map(|message| message_value(message, dialect, &mut report))
                .collect(),
        ),
    );
    if !canonical.system.is_empty() {
        body.insert(
            "system".to_owned(),
            Value::Array(
                canonical
                    .system
                    .iter()
                    .filter_map(|part| system_value(part, dialect, &mut report))
                    .collect(),
            ),
        );
    }
    if !canonical.tools.is_empty() {
        body.insert(
            "tools".to_owned(),
            Value::Array(
                canonical
                    .tools
                    .iter()
                    .map(|tool| tool_value(tool, dialect, &mut report))
                    .collect(),
            ),
        );
    }
    if canonical.tool_choice != CanonToolChoice::Auto {
        body.insert(
            "tool_choice".to_owned(),
            tool_choice_value(&canonical.tool_choice),
        );
    }
    if let Some(max_tokens) = canonical.sampling.max_tokens {
        body.insert("max_tokens".to_owned(), json!(max_tokens));
    }
    insert_f64(&mut body, "temperature", canonical.sampling.temperature)?;
    insert_f64(&mut body, "top_p", canonical.sampling.top_p)?;
    if let Some(stops) = &canonical.sampling.stop_sequences {
        body.insert("stop_sequences".to_owned(), json!(stops));
    }
    if let Some(thinking) = &canonical.thinking {
        body.insert(
            "thinking".to_owned(),
            json!({"type": "enabled", "budget_tokens": thinking.budget_tokens}),
        );
    }
    if let Some(stream) = canonical.stream {
        body.insert("stream".to_owned(), Value::Bool(stream));
    }
    replay_extensions(&mut body, &canonical.extensions, dialect, &mut report);

    Ok(Rendered {
        value: Value::Object(body),
        report,
    })
}

fn insert_f64(
    body: &mut Map<String, Value>,
    field: &str,
    value: Option<f64>,
) -> Result<(), TranslateError> {
    let Some(value) = value else {
        return Ok(());
    };
    let number = Number::from_f64(value).ok_or_else(|| TranslateError::Malformed {
        reason: format!("canonical {field} is not finite"),
    })?;
    body.insert(field.to_owned(), Value::Number(number));
    Ok(())
}

fn message_value(
    message: &CanonMessage,
    dialect: DialectId,
    report: &mut TranslationReport,
) -> Value {
    let role = match message.role {
        CanonRole::User => "user",
        CanonRole::Assistant => "assistant",
        CanonRole::System => "system",
        CanonRole::Developer => "system",
    };
    if message.role == CanonRole::Developer {
        report.push(TranslationLoss::new(
            "messages[].role.developer",
            TranslationLossReason::NotRepresentable,
            1,
        ));
    }
    let mut value = json!({
        "role": role,
        "content": message
            .blocks
            .iter()
            .filter_map(|block| block_value(block, dialect, report))
            .collect::<Vec<_>>(),
    });
    replay_extensions(
        value
            .as_object_mut()
            .expect("a rendered message is an object"),
        &message.extensions,
        dialect,
        report,
    );
    value
}

fn block_value(
    block: &CanonBlock,
    dialect: DialectId,
    report: &mut TranslationReport,
) -> Option<Value> {
    if let CanonBlock::Annotated {
        block,
        is_error,
        extensions,
    } = block
    {
        let Some(mut value) = block_value(block, dialect, report) else {
            if is_error.is_some() {
                report.push(TranslationLoss::new(
                    "messages[].blocks[].tool_result.is_error",
                    TranslationLossReason::NotRepresentable,
                    1,
                ));
            }
            for extension in extensions {
                report.push(TranslationLoss::new(
                    extension.wire_path(),
                    if can_replay(extension.source(), dialect) {
                        TranslationLossReason::NotRepresentable
                    } else {
                        TranslationLossReason::IncompatibleExtensionDialect
                    },
                    1,
                ));
            }
            return None;
        };
        let object = value
            .as_object_mut()
            .expect("a canonical block renders as an object");
        if let Some(is_error) = is_error {
            object.insert("is_error".to_owned(), Value::Bool(*is_error));
        }
        replay_extensions(object, extensions, dialect, report);
        return Some(value);
    }

    Some(match block {
        CanonBlock::Text(text) => json!({"type": "text", "text": text}),
        CanonBlock::Image { url } => {
            json!({"type": "image", "source": {"type": "url", "url": url}})
        }
        CanonBlock::ToolUse { id, name, input } => {
            json!({"type": "tool_use", "id": id, "name": name, "input": input})
        }
        CanonBlock::ToolResult {
            tool_use_id,
            content,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": tool_result_value(content, dialect, report),
        }),
        CanonBlock::Thinking {
            text,
            signature: Some(signature),
        } => json!({"type": "thinking", "thinking": text, "signature": signature}),
        CanonBlock::Thinking {
            signature: None, ..
        } => {
            report.push(TranslationLoss::new(
                "messages[].blocks[].thinking.signature",
                TranslationLossReason::NotRepresentable,
                1,
            ));
            return None;
        }
        CanonBlock::RedactedThinking { data } => {
            json!({"type": "redacted_thinking", "data": data})
        }
        CanonBlock::Annotated { .. } => unreachable!("annotations handled above"),
    })
}

fn tool_result_value(
    content: &ToolResultContent,
    dialect: DialectId,
    report: &mut TranslationReport,
) -> Value {
    match content {
        ToolResultContent::String(text) => Value::String(text.clone()),
        ToolResultContent::Blocks(blocks) => Value::Array(
            blocks
                .iter()
                .filter_map(|block| block_value(block, dialect, report))
                .collect(),
        ),
    }
}

fn system_value(
    part: &CanonSystemPart,
    dialect: DialectId,
    report: &mut TranslationReport,
) -> Option<Value> {
    match part {
        CanonSystemPart::Text { text, extensions } => {
            let mut object = Map::new();
            object.insert("type".to_owned(), Value::String("text".to_owned()));
            object.insert("text".to_owned(), Value::String(text.clone()));
            replay_extensions(&mut object, extensions, dialect, report);
            Some(Value::Object(object))
        }
        CanonSystemPart::Opaque(extension) if can_replay(extension.source(), dialect) => {
            Some(extension.value().clone())
        }
        CanonSystemPart::Opaque(extension) => {
            report_incompatible(extension, report);
            None
        }
    }
}

fn tool_value(tool: &CanonTool, dialect: DialectId, report: &mut TranslationReport) -> Value {
    let mut object = Map::new();
    object.insert("name".to_owned(), Value::String(tool.name.clone()));
    object.insert(
        "description".to_owned(),
        Value::String(tool.description.clone()),
    );
    object.insert("input_schema".to_owned(), tool.parameters.clone());
    replay_extensions(&mut object, &tool.extensions, dialect, report);
    Value::Object(object)
}

fn tool_choice_value(choice: &CanonToolChoice) -> Value {
    match choice {
        CanonToolChoice::Auto => json!({"type": "auto"}),
        CanonToolChoice::Any => json!({"type": "any"}),
        CanonToolChoice::None => unreachable!("rejected before rendering"),
        CanonToolChoice::Tool { name } => {
            let mut object = Map::new();
            object.insert("type".to_owned(), Value::String("tool".to_owned()));
            if let Some(name) = name {
                object.insert("name".to_owned(), Value::String(name.clone()));
            }
            Value::Object(object)
        }
        CanonToolChoice::Other { kind } => json!({"type": kind}),
    }
}

fn replay_extensions(
    object: &mut Map<String, Value>,
    extensions: &[CanonicalExtension],
    dialect: DialectId,
    report: &mut TranslationReport,
) {
    for extension in extensions {
        if can_replay(extension.source(), dialect)
            && let Some(name) = extension.wire_name()
        {
            object.insert(name.to_owned(), extension.value().clone());
            continue;
        }
        report_incompatible(extension, report);
    }
}

fn can_replay(source: DialectId, target: DialectId) -> bool {
    matches!(
        (source, target),
        (
            DialectId::AnthropicMessages | DialectId::OpenRouterMessages,
            DialectId::AnthropicMessages | DialectId::OpenRouterMessages,
        )
    )
}

fn report_incompatible(extension: &CanonicalExtension, report: &mut TranslationReport) {
    report.push(TranslationLoss::new(
        extension.wire_path(),
        TranslationLossReason::IncompatibleExtensionDialect,
        1,
    ));
}

// ── response interpretation ─────────────────────────────────────────

/// Stateful interpretation of one Anthropic Messages SSE turn.
#[derive(Debug, Clone, Default)]
pub struct AnthropicResponseStream {
    blocks: BTreeMap<u64, IncomingBlock>,
    start_usage: Option<Map<String, Value>>,
}

#[derive(Debug, Clone)]
enum IncomingBlock {
    Text,
    Thinking,
    Redacted,
    ToolUse {
        id: String,
        name: String,
        initial_input: Value,
        arguments: String,
    },
    Unknown,
}

impl AnthropicResponseStream {
    pub fn new() -> AnthropicResponseStream {
        AnthropicResponseStream::default()
    }

    /// Interpret one already-split SSE event. Invalid JSON and unknown event
    /// shapes stay quiet; response observation must never break the stream.
    pub fn feed_sse(&mut self, event: &SseEvent) -> Vec<CanonEvent> {
        serde_json::from_str::<Value>(&event.data())
            .ok()
            .map(|value| self.feed(&value))
            .unwrap_or_default()
    }

    /// Interpret one Anthropic event data object.
    pub fn feed(&mut self, event: &Value) -> Vec<CanonEvent> {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => self.message_start(event),
            Some("content_block_start") => self.block_start(event),
            Some("content_block_delta") => self.block_delta(event),
            Some("content_block_stop") => self.block_stop(event),
            Some("message_delta") => self.message_delta(event),
            Some("error") => vec![CanonEvent::TurnFailed {
                error: canonical_error(event.get("error").unwrap_or(event)),
            }],
            Some("message_stop") | Some("ping") | None | Some(_) => Vec::new(),
        }
    }

    fn message_start(&mut self, event: &Value) -> Vec<CanonEvent> {
        let Some(message) = event.get("message") else {
            return Vec::new();
        };
        self.start_usage = message.get("usage").and_then(Value::as_object).cloned();
        vec![CanonEvent::TurnStarted {
            turn_id: message.get("id").and_then(Value::as_str).map(str::to_owned),
        }]
    }

    fn block_start(&mut self, event: &Value) -> Vec<CanonEvent> {
        let Some(index) = event.get("index").and_then(Value::as_u64) else {
            return Vec::new();
        };
        let Some(block) = event.get("content_block") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let incoming = match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    out.push(CanonEvent::TextDelta {
                        delta: text.to_owned(),
                    });
                }
                IncomingBlock::Text
            }
            Some("thinking") => {
                if let Some(thinking) = block.get("thinking").and_then(Value::as_str)
                    && !thinking.is_empty()
                {
                    out.push(CanonEvent::ThinkingDelta {
                        part: index,
                        delta: thinking.to_owned(),
                    });
                }
                IncomingBlock::Thinking
            }
            Some("redacted_thinking") => {
                if let Some(data) = block.get("data").and_then(Value::as_str) {
                    out.push(CanonEvent::RedactedThinking {
                        data: data.to_owned(),
                    });
                }
                IncomingBlock::Redacted
            }
            Some("tool_use") => match (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            ) {
                (Some(id), Some(name)) => IncomingBlock::ToolUse {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    initial_input: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    arguments: String::new(),
                },
                _ => IncomingBlock::Unknown,
            },
            _ => IncomingBlock::Unknown,
        };
        self.blocks.insert(index, incoming);
        out
    }

    fn block_delta(&mut self, event: &Value) -> Vec<CanonEvent> {
        let Some(index) = event.get("index").and_then(Value::as_u64) else {
            return Vec::new();
        };
        let Some(delta) = event.get("delta") else {
            return Vec::new();
        };
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => delta
                .get("text")
                .and_then(Value::as_str)
                .map(|text| {
                    vec![CanonEvent::TextDelta {
                        delta: text.to_owned(),
                    }]
                })
                .unwrap_or_default(),
            Some("thinking_delta") => delta
                .get("thinking")
                .and_then(Value::as_str)
                .map(|thinking| {
                    vec![CanonEvent::ThinkingDelta {
                        part: index,
                        delta: thinking.to_owned(),
                    }]
                })
                .unwrap_or_default(),
            Some("signature_delta") => delta
                .get("signature")
                .and_then(Value::as_str)
                .map(|signature| {
                    vec![CanonEvent::ThinkingSignature {
                        part: index,
                        signature: signature.to_owned(),
                    }]
                })
                .unwrap_or_default(),
            Some("input_json_delta") => {
                if let Some(IncomingBlock::ToolUse { arguments, .. }) = self.blocks.get_mut(&index)
                    && let Some(fragment) = delta.get("partial_json").and_then(Value::as_str)
                {
                    arguments.push_str(fragment);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn block_stop(&mut self, event: &Value) -> Vec<CanonEvent> {
        let Some(index) = event.get("index").and_then(Value::as_u64) else {
            return Vec::new();
        };
        match self.blocks.remove(&index) {
            Some(IncomingBlock::Text) => vec![CanonEvent::TextEnded],
            Some(IncomingBlock::Thinking) => vec![CanonEvent::ThinkingEnded],
            Some(IncomingBlock::ToolUse {
                id,
                name,
                initial_input,
                arguments,
            }) => vec![CanonEvent::ToolCall(CanonToolCall {
                id,
                name,
                arguments: if arguments.is_empty() {
                    serde_json::to_string(&initial_input)
                        .expect("a parsed tool input always serialises")
                } else {
                    arguments
                },
            })],
            Some(IncomingBlock::Redacted | IncomingBlock::Unknown) | None => Vec::new(),
        }
    }

    fn message_delta(&mut self, event: &Value) -> Vec<CanonEvent> {
        let reason = event
            .get("delta")
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let usage = merged_usage(self.start_usage.as_ref(), event.get("usage"));
        vec![CanonEvent::TurnEnded {
            stop_reason: stop_reason(reason),
            usage,
        }]
    }
}

fn stop_reason(reason: &str) -> CanonStopReason {
    match reason {
        "end_turn" => CanonStopReason::EndTurn,
        "stop_sequence" => CanonStopReason::StopSequence,
        "pause_turn" => CanonStopReason::PauseTurn,
        "tool_use" => CanonStopReason::ToolUse,
        "max_tokens" => CanonStopReason::MaxTokens,
        "model_context_window_exceeded" => CanonStopReason::ContextWindowExceeded,
        "refusal" => CanonStopReason::Refusal,
        other => CanonStopReason::Incomplete(other.to_owned()),
    }
}

fn merged_usage(
    start: Option<&Map<String, Value>>,
    final_usage: Option<&Value>,
) -> Option<CanonicalUsage> {
    let mut merged = start.cloned().unwrap_or_default();
    if let Some(final_usage) = final_usage.and_then(Value::as_object) {
        for (field, value) in final_usage {
            merged.insert(field.clone(), value.clone());
        }
    }
    if merged.is_empty() {
        return None;
    }
    let raw = Value::Object(merged.clone());
    Some(CanonicalUsage {
        input: u64_field(&merged, "input_tokens"),
        cache_read: u64_field(&merged, "cache_read_input_tokens"),
        cache_write: u64_field(&merged, "cache_creation_input_tokens"),
        output: u64_field(&merged, "output_tokens"),
        reasoning: merged
            .get("output_tokens_details")
            .and_then(|details| details.get("thinking_tokens"))
            .and_then(Value::as_u64),
        raw,
    })
}

fn u64_field(object: &Map<String, Value>, field: &str) -> Option<u64> {
    object.get(field).and_then(Value::as_u64)
}

fn canonical_error(error: &Value) -> CanonError {
    let kind = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    CanonError {
        kind: match kind {
            "rate_limit_error" => CanonErrorKind::RateLimit,
            "invalid_request_error" => CanonErrorKind::InvalidRequest,
            "authentication_error" => CanonErrorKind::Authentication,
            "permission_error" => CanonErrorKind::Permission,
            "not_found_error" => CanonErrorKind::NotFound,
            "request_too_large" => CanonErrorKind::TooLarge,
            "overloaded_error" => CanonErrorKind::Overloaded,
            _ => CanonErrorKind::Api,
        },
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(kind)
            .to_owned(),
        resets_at: error.get("retry_after").and_then(Value::as_i64),
    }
}

/// Interpret one complete non-streaming Anthropic Messages response.
pub fn canonical_turn_from_anthropic(body: &Value) -> Result<CanonTurn, TranslateError> {
    if body.get("type").and_then(Value::as_str) == Some("error") {
        return Ok(CanonTurn {
            turn_id: None,
            stop_reason: CanonStopReason::EndTurn,
            usage: None,
            error: Some(canonical_error(body.get("error").unwrap_or(body))),
            tool_calls: Vec::new(),
            blocks: None,
            text: String::new(),
            thinking: BTreeMap::new(),
        });
    }

    let content = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| TranslateError::Malformed {
            reason: "anthropic response content is missing or not an array".to_owned(),
        })?;
    let blocks = content_blocks_of(CanonRole::Assistant, content, 0)?;
    let mut text = String::new();
    let mut thinking = BTreeMap::new();
    let mut tool_calls = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        match block.semantic() {
            CanonBlock::Text(part) => text.push_str(part),
            CanonBlock::Thinking { text, .. } => {
                thinking.insert(index as u64, text.clone());
            }
            CanonBlock::ToolUse { id, name, input } => tool_calls.push(CanonToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: serde_json::to_string(input)
                    .expect("a parsed tool input always serialises"),
            }),
            CanonBlock::Image { .. }
            | CanonBlock::ToolResult { .. }
            | CanonBlock::RedactedThinking { .. } => {}
            CanonBlock::Annotated { .. } => unreachable!("semantic() removes annotations"),
        }
    }

    Ok(CanonTurn {
        turn_id: body.get("id").and_then(Value::as_str).map(str::to_owned),
        stop_reason: body
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(stop_reason)
            .unwrap_or(CanonStopReason::EndTurn),
        usage: merged_usage(None, body.get("usage")),
        error: None,
        tool_calls,
        blocks: Some(blocks),
        text,
        thinking,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AnthropicResponseStream, canonical_turn_from_anthropic, render_anthropic, stop_reason,
    };
    use crate::ir::canonical::{
        CanonBlock, CanonEvent, CanonMessage, CanonRole, CanonStopReason, CanonicalExtension,
    };
    use crate::observe::sse::SseSplitter;
    use crate::routing::DialectId;
    use crate::translate::anthropic_frontend::anthropic_from_canonical;
    use crate::translate::anthropic_frontend::from_anthropic;
    use crate::translate::{TranslateError, TranslationLoss, TranslationLossReason};
    use serde_json::json;

    fn fixture_events(source: &str) -> Vec<CanonEvent> {
        let mut splitter = SseSplitter::new();
        let mut stream = AnthropicResponseStream::new();
        let mut events: Vec<_> = splitter
            .feed(source.as_bytes())
            .into_iter()
            .flat_map(|event| stream.feed_sse(&event))
            .collect();
        if let Some(event) = splitter.finish() {
            events.extend(stream.feed_sse(&event));
        }
        events
    }

    #[test]
    fn the_tool_use_fixture_interprets_complete_arguments_and_usage() {
        let events = fixture_events(include_str!(
            "../../tests/fixtures/anthropic_sse/02_tool_use.sse"
        ));
        assert_eq!(events.len(), 5, "message_stop is quiet");
        assert!(matches!(
            &events[0],
            CanonEvent::TurnStarted { turn_id: Some(id) } if id == "msg_02T9kQ"
        ));
        assert!(matches!(
            &events[1],
            CanonEvent::TextDelta { delta } if delta == "Checking the weather."
        ));
        assert_eq!(events[2], CanonEvent::TextEnded);
        assert!(matches!(
            &events[3],
            CanonEvent::ToolCall(call)
                if call.id == "toolu_02Wx"
                    && call.name == "get_weather"
                    && call.arguments == r#"{"city":"Wellington","units":"celsius"}"#
        ));
        let CanonEvent::TurnEnded { stop_reason, usage } = &events[4] else {
            panic!("fifth event is the turn end: {:?}", events[4]);
        };
        assert_eq!(stop_reason, &CanonStopReason::ToolUse);
        let usage = usage.as_ref().expect("usage merges start and final");
        assert_eq!(usage.input, Some(4));
        assert_eq!(usage.output, Some(65));
        assert_eq!(usage.reasoning, Some(22));
        assert_eq!(usage.raw["service_tier"], json!("standard"));
    }

    #[test]
    fn signed_and_redacted_reasoning_interpret_without_exposing_payloads() {
        let mut stream = AnthropicResponseStream::new();
        let values = [
            json!({"type": "content_block_start", "index": 3,
                   "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 3,
                   "delta": {"type": "thinking_delta", "thinking": "reason"}}),
            json!({"type": "content_block_delta", "index": 3,
                   "delta": {"type": "signature_delta", "signature": "signature"}}),
            json!({"type": "content_block_stop", "index": 3}),
            json!({"type": "content_block_start", "index": 4,
                   "content_block": {"type": "redacted_thinking", "data": "encrypted"}}),
            json!({"type": "content_block_stop", "index": 4}),
        ];
        let events: Vec<_> = values.iter().flat_map(|value| stream.feed(value)).collect();
        assert_eq!(
            events,
            vec![
                CanonEvent::ThinkingDelta {
                    part: 3,
                    delta: "reason".to_owned(),
                },
                CanonEvent::ThinkingSignature {
                    part: 3,
                    signature: "signature".to_owned(),
                },
                CanonEvent::ThinkingEnded,
                CanonEvent::RedactedThinking {
                    data: "encrypted".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn terminal_anthropic_errors_map_to_the_canonical_taxonomy() {
        let events = fixture_events(include_str!(
            "../../tests/fixtures/anthropic_sse/06_error_event.sse"
        ));
        assert!(matches!(
            events.last(),
            Some(CanonEvent::TurnFailed { error })
                if error.kind == crate::ir::canonical::CanonErrorKind::Overloaded
                    && error.message == "Overloaded"
        ));
    }

    #[test]
    fn every_anthropic_stop_reason_keeps_its_distinction() {
        assert_eq!(stop_reason("end_turn"), CanonStopReason::EndTurn);
        assert_eq!(stop_reason("tool_use"), CanonStopReason::ToolUse);
        assert_eq!(stop_reason("max_tokens"), CanonStopReason::MaxTokens);
        assert_eq!(stop_reason("stop_sequence"), CanonStopReason::StopSequence);
        assert_eq!(stop_reason("pause_turn"), CanonStopReason::PauseTurn);
        assert_eq!(stop_reason("refusal"), CanonStopReason::Refusal);
        assert_eq!(
            stop_reason("model_context_window_exceeded"),
            CanonStopReason::ContextWindowExceeded
        );
        assert_eq!(
            stop_reason("future_reason"),
            CanonStopReason::Incomplete("future_reason".to_owned())
        );
    }

    #[test]
    fn complete_messages_keep_order_signatures_redaction_and_metadata() {
        let body = json!({
            "id": "msg_complete",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [
                {"type": "thinking", "thinking": "reason", "signature": "signature"},
                {"type": "redacted_thinking", "data": "encrypted"},
                {"type": "text", "text": "Answer", "cache_control": {"type": "ephemeral"}},
                {"type": "tool_use", "id": "t1", "name": "read", "input": {"path": "x"}}
            ],
            "stop_reason": "pause_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 7, "output_tokens": 9,
                      "output_tokens_details": {"thinking_tokens": 3}}
        });
        let turn = canonical_turn_from_anthropic(&body).expect("interprets");
        assert_eq!(turn.stop_reason, CanonStopReason::PauseTurn);
        assert_eq!(turn.text, "Answer");
        assert_eq!(turn.thinking.get(&0).map(String::as_str), Some("reason"));
        assert_eq!(turn.tool_calls[0].arguments, r#"{"path":"x"}"#);
        assert_eq!(
            turn.usage.as_ref().and_then(|usage| usage.reasoning),
            Some(3)
        );

        let rendered = anthropic_from_canonical("claude-opus-5", &turn);
        assert_eq!(rendered["content"], body["content"]);
        assert_eq!(rendered["stop_reason"], json!("pause_turn"));
    }

    #[test]
    fn complete_error_bodies_interpret_as_failed_turns() {
        let turn = canonical_turn_from_anthropic(&json!({
            "type": "error",
            "error": {"type": "authentication_error", "message": "bad token"}
        }))
        .expect("interprets error");
        assert!(matches!(
            turn.error,
            Some(crate::ir::canonical::CanonError {
                kind: crate::ir::canonical::CanonErrorKind::Authentication,
                ..
            })
        ));
        assert!(turn.blocks.is_none());
    }

    #[test]
    fn rich_anthropic_input_round_trips_through_the_canonical() {
        let source = json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "Hi", "cache_control": {"type": "ephemeral"}},
                    {"type": "tool_result", "tool_use_id": "t1", "content": "failed",
                     "is_error": true}
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "reason", "signature": "sig"},
                    {"type": "tool_use", "id": "t2", "name": "read", "input": {}}
                ]}
            ],
            "system": [
                {"type": "text", "text": "System", "cache_control": {"type": "ephemeral"}},
                {"future_system": true}
            ],
            "tools": [{"name": "read", "description": "", "input_schema": {"type": "object"},
                       "cache_control": {"type": "ephemeral"}}],
            "tool_choice": {"type": "tool", "name": "read"},
            "max_tokens": 2048,
            "temperature": 0.2,
            "top_p": 0.9,
            "stop_sequences": ["STOP"],
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "stream": true,
            "metadata": {"user_id": "opaque"}
        });
        let canonical = from_anthropic(&source).expect("parses");
        let rendered = render_anthropic(&canonical, DialectId::AnthropicMessages)
            .expect("renders deterministically");

        assert!(rendered.report.is_empty());
        assert_eq!(
            from_anthropic(&rendered.value).expect("rendered body reparses"),
            canonical
        );
        assert_eq!(
            serde_json::to_string(&rendered.value).expect("serialises"),
            serde_json::to_string(
                &render_anthropic(&canonical, DialectId::AnthropicMessages)
                    .expect("renders again")
                    .value
            )
            .expect("serialises again")
        );
    }

    #[test]
    fn unsigned_thinking_and_foreign_extensions_are_content_free_losses() {
        let canonical = crate::ir::canonical::CanonicalRequest {
            model: Some("claude-opus-5".to_owned()),
            messages: vec![CanonMessage {
                role: CanonRole::Assistant,
                blocks: vec![
                    CanonBlock::Thinking {
                        text: "must not leak into the report".to_owned(),
                        signature: None,
                    }
                    .annotated(
                        None,
                        vec![CanonicalExtension::node_field(
                            DialectId::AnthropicMessages,
                            "$.messages[].content[].cache_control",
                            "cache_control",
                            json!({"secret": "reasoning-cache-value"}),
                        )],
                    ),
                ],
                extensions: Vec::new(),
            }],
            extensions: vec![CanonicalExtension::node_field(
                DialectId::CodexResponses,
                "$.foreign",
                "foreign",
                json!({"secret": "opaque-value"}),
            )],
            ..Default::default()
        };
        let rendered = render_anthropic(&canonical, DialectId::AnthropicMessages)
            .expect("renders with losses");

        assert_eq!(rendered.value["messages"][0]["content"], json!([]));
        assert_eq!(
            rendered.report.losses(),
            &[
                TranslationLoss::new(
                    "messages[].blocks[].thinking.signature",
                    TranslationLossReason::NotRepresentable,
                    1,
                ),
                TranslationLoss::new(
                    "$.messages[].content[].cache_control",
                    TranslationLossReason::NotRepresentable,
                    1,
                ),
                TranslationLoss::new(
                    "$.foreign",
                    TranslationLossReason::IncompatibleExtensionDialect,
                    1,
                ),
            ]
        );
        let report = format!("{:?}", rendered.report);
        assert!(!report.contains("must not leak"));
        assert!(!report.contains("reasoning-cache-value"));
        assert!(!report.contains("opaque-value"));
    }

    #[test]
    fn openrouter_messages_shares_the_anthropic_extension_replay_domain() {
        let canonical = from_anthropic(&json!({
            "model": "provider/model",
            "messages": [{"role": "user", "content": "Hi"}],
            "metadata": {"opaque": true}
        }))
        .expect("parses");
        let rendered = render_anthropic(&canonical, DialectId::OpenRouterMessages)
            .expect("renders for OpenRouter");
        assert_eq!(rendered.value["metadata"], json!({"opaque": true}));
        assert!(rendered.report.is_empty());
    }

    #[test]
    fn a_non_messages_target_and_missing_model_are_rejected() {
        let canonical = crate::ir::canonical::CanonicalRequest::default();
        assert!(matches!(
            render_anthropic(&canonical, DialectId::CodexResponses),
            Err(TranslateError::Malformed { .. })
        ));
        assert!(matches!(
            render_anthropic(&canonical, DialectId::AnthropicMessages),
            Err(TranslateError::Malformed { .. })
        ));
    }
}
