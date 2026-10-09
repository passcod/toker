//! Deterministic canonical request rendering for OpenAI Chat bindings.

use std::collections::BTreeMap;

use serde_json::{Map, Number, Value, json};

use crate::ir::canonical::{
    CanonBlock, CanonError, CanonErrorKind, CanonEvent, CanonMessage, CanonRole, CanonStopReason,
    CanonSystemPart, CanonTool, CanonToolCall, CanonToolChoice, CanonTurn, CanonicalExtension,
    CanonicalRequest, CanonicalUsage, ToolResultContent,
};
use crate::observe::sse::SseEvent;
use crate::routing::DialectId;
use crate::translate::{
    Rendered, TranslateError, TranslationLoss, TranslationLossReason, TranslationReport,
};

const DIALECT: DialectId = DialectId::OpenRouterChatCompletions;

pub fn render_openai_chat(
    canonical: &CanonicalRequest,
    dialect: DialectId,
) -> Result<Rendered<Value>, TranslateError> {
    if dialect != DIALECT {
        return Err(TranslateError::Malformed {
            reason: format!("{dialect} is not an OpenAI Chat dialect"),
        });
    }
    let model = canonical
        .model
        .as_deref()
        .ok_or_else(|| TranslateError::Malformed {
            reason: "canonical request has no model".to_owned(),
        })?;
    let mut report = TranslationReport::default();
    let mut messages = system_messages(&canonical.system, &mut report);
    for message in &canonical.messages {
        messages.extend(message_values(message, &mut report)?);
    }

    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(model.to_owned()));
    body.insert("messages".to_owned(), Value::Array(messages));
    if !canonical.tools.is_empty() {
        body.insert(
            "tools".to_owned(),
            Value::Array(
                canonical
                    .tools
                    .iter()
                    .map(|tool| tool_value(tool, &mut report))
                    .collect(),
            ),
        );
    }
    if canonical.tool_choice != CanonToolChoice::Auto {
        body.insert(
            "tool_choice".to_owned(),
            tool_choice_value(&canonical.tool_choice)?,
        );
    }
    insert_f64(&mut body, "temperature", canonical.sampling.temperature)?;
    insert_f64(&mut body, "top_p", canonical.sampling.top_p)?;
    if let Some(max_tokens) = canonical.sampling.max_tokens {
        body.insert("max_completion_tokens".to_owned(), json!(max_tokens));
    }
    if let Some(stops) = &canonical.sampling.stop_sequences {
        body.insert("stop".to_owned(), json!(stops));
    }
    if let Some(stream) = canonical.stream {
        body.insert("stream".to_owned(), Value::Bool(stream));
    }
    if canonical.thinking.is_some() {
        report.push(TranslationLoss::new(
            "thinking",
            TranslationLossReason::NotRepresentable,
            1,
        ));
    }
    replay_extensions(&mut body, &canonical.extensions, &mut report);
    Ok(Rendered {
        value: Value::Object(body),
        report,
    })
}

fn system_messages(system: &[CanonSystemPart], report: &mut TranslationReport) -> Vec<Value> {
    system
        .iter()
        .filter_map(|part| match part {
            CanonSystemPart::Text { text, extensions } => {
                let mut part = json!({"type": "text", "text": text});
                replay_extensions(
                    part.as_object_mut().expect("a text part is an object"),
                    extensions,
                    report,
                );
                Some(json!({"role": "system", "content": [part]}))
            }
            CanonSystemPart::Opaque(extension) => {
                report_incompatible(extension, report);
                None
            }
        })
        .collect()
}

fn message_values(
    message: &CanonMessage,
    report: &mut TranslationReport,
) -> Result<Vec<Value>, TranslateError> {
    match message.role {
        CanonRole::System | CanonRole::Developer => instruction_message(message, report),
        CanonRole::User => user_messages(message, report),
        CanonRole::Assistant => assistant_message(message, report),
    }
}

fn instruction_message(
    message: &CanonMessage,
    report: &mut TranslationReport,
) -> Result<Vec<Value>, TranslateError> {
    let mut content = Vec::new();
    for block in &message.blocks {
        match block.semantic() {
            CanonBlock::Text(_) => content.push(content_part(block, report)?),
            other => return Err(unsupported(other)),
        }
    }
    let role = if message.role == CanonRole::Developer {
        "developer"
    } else {
        "system"
    };
    let mut value = json!({"role": role, "content": content});
    replay_extensions(
        value.as_object_mut().expect("a message is an object"),
        &message.extensions,
        report,
    );
    Ok(vec![value])
}

fn user_messages(
    message: &CanonMessage,
    report: &mut TranslationReport,
) -> Result<Vec<Value>, TranslateError> {
    let mut out = Vec::new();
    let mut content = Vec::new();
    for block in &message.blocks {
        match block.semantic() {
            CanonBlock::Text(_) | CanonBlock::Image { .. } => {
                content.push(content_part(block, report)?);
            }
            CanonBlock::ToolResult {
                tool_use_id,
                content: result,
            } => {
                flush_message(&mut out, "user", &mut content);
                let mut value = json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": tool_result_value(result, report)?,
                });
                apply_tool_result_annotations(block, &mut value, report);
                out.push(value);
            }
            other => return Err(unsupported(other)),
        }
    }
    flush_message(&mut out, "user", &mut content);
    if out.is_empty() {
        out.push(json!({"role": "user", "content": null}));
    }
    replay_extensions(
        out[0].as_object_mut().expect("a message is an object"),
        &message.extensions,
        report,
    );
    Ok(out)
}

fn assistant_message(
    message: &CanonMessage,
    report: &mut TranslationReport,
) -> Result<Vec<Value>, TranslateError> {
    let mut content = Vec::new();
    let mut calls = Vec::new();
    for block in &message.blocks {
        match block.semantic() {
            CanonBlock::Text(_) => content.push(content_part(block, report)?),
            CanonBlock::ToolUse { id, name, input } => {
                let arguments =
                    serde_json::to_string(input).expect("a parsed JSON value always serialises");
                let mut call = json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                });
                apply_tool_call_annotations(block, &mut call, report);
                calls.push(call);
            }
            CanonBlock::Thinking { .. } | CanonBlock::RedactedThinking { .. } => {
                report.push(TranslationLoss::new(
                    "messages[].blocks[].thinking",
                    TranslationLossReason::NotRepresentable,
                    1,
                ));
            }
            other => return Err(unsupported(other)),
        }
    }
    let mut object = Map::new();
    object.insert("role".to_owned(), Value::String("assistant".to_owned()));
    object.insert(
        "content".to_owned(),
        if content.is_empty() {
            Value::Null
        } else {
            Value::Array(content)
        },
    );
    if !calls.is_empty() {
        object.insert("tool_calls".to_owned(), Value::Array(calls));
    }
    replay_extensions(&mut object, &message.extensions, report);
    Ok(vec![Value::Object(object)])
}

fn content_part(
    block: &CanonBlock,
    report: &mut TranslationReport,
) -> Result<Value, TranslateError> {
    let semantic = block.semantic();
    let mut value = match semantic {
        CanonBlock::Text(text) => json!({"type": "text", "text": text}),
        CanonBlock::Image { url } => json!({
            "type": "image_url",
            "image_url": {"url": url},
        }),
        other => return Err(unsupported(other)),
    };
    let CanonBlock::Annotated { extensions, .. } = block else {
        return Ok(value);
    };
    for extension in extensions {
        if extension.source() != DIALECT {
            report_incompatible(extension, report);
            continue;
        }
        let Some(name) = extension.wire_name() else {
            report_incompatible(extension, report);
            continue;
        };
        if extension.wire_path().contains(".image_url.") {
            value
                .get_mut("image_url")
                .and_then(Value::as_object_mut)
                .expect("an image part has an image_url object")
                .insert(name.to_owned(), extension.value().clone());
        } else {
            value
                .as_object_mut()
                .expect("a content part is an object")
                .insert(name.to_owned(), extension.value().clone());
        }
    }
    Ok(value)
}

fn apply_tool_call_annotations(
    block: &CanonBlock,
    value: &mut Value,
    report: &mut TranslationReport,
) {
    let CanonBlock::Annotated { extensions, .. } = block else {
        return;
    };
    for extension in extensions {
        if extension.source() != DIALECT {
            report_incompatible(extension, report);
            continue;
        }
        let Some(name) = extension.wire_name() else {
            report_incompatible(extension, report);
            continue;
        };
        if extension.wire_path().contains(".function.") {
            value
                .get_mut("function")
                .and_then(Value::as_object_mut)
                .expect("a tool call has a function object")
                .insert(name.to_owned(), extension.value().clone());
        } else {
            value
                .as_object_mut()
                .expect("a tool call is an object")
                .insert(name.to_owned(), extension.value().clone());
        }
    }
}

fn apply_tool_result_annotations(
    block: &CanonBlock,
    value: &mut Value,
    report: &mut TranslationReport,
) {
    let CanonBlock::Annotated {
        is_error,
        extensions,
        ..
    } = block
    else {
        return;
    };
    if is_error.is_some() {
        report.push(TranslationLoss::new(
            "messages[].blocks[].tool_result.is_error",
            TranslationLossReason::NotRepresentable,
            1,
        ));
    }
    replay_extensions(
        value.as_object_mut().expect("a tool message is an object"),
        extensions,
        report,
    );
}

fn flush_message(out: &mut Vec<Value>, role: &str, content: &mut Vec<Value>) {
    if !content.is_empty() {
        out.push(json!({
            "role": role,
            "content": Value::Array(std::mem::take(content)),
        }));
    }
}

fn tool_result_value(
    content: &ToolResultContent,
    report: &mut TranslationReport,
) -> Result<Value, TranslateError> {
    match content {
        ToolResultContent::String(text) => Ok(Value::String(text.clone())),
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| content_part(block, report))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
    }
}

fn tool_value(tool: &CanonTool, report: &mut TranslationReport) -> Value {
    let mut value = json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        },
    });
    for extension in &tool.extensions {
        if extension.source() != DIALECT {
            report_incompatible(extension, report);
            continue;
        }
        let Some(name) = extension.wire_name() else {
            report_incompatible(extension, report);
            continue;
        };
        if extension.wire_path().contains(".function.") {
            value
                .get_mut("function")
                .and_then(Value::as_object_mut)
                .expect("a tool has a function object")
                .insert(name.to_owned(), extension.value().clone());
        } else {
            value
                .as_object_mut()
                .expect("a tool is an object")
                .insert(name.to_owned(), extension.value().clone());
        }
    }
    value
}

fn tool_choice_value(choice: &CanonToolChoice) -> Result<Value, TranslateError> {
    match choice {
        CanonToolChoice::Auto => Ok(Value::String("auto".to_owned())),
        CanonToolChoice::Any => Ok(Value::String("required".to_owned())),
        CanonToolChoice::None => Ok(Value::String("none".to_owned())),
        CanonToolChoice::Tool { name: Some(name) } => {
            Ok(json!({"type": "function", "function": {"name": name}}))
        }
        CanonToolChoice::Tool { name: None } => Err(TranslateError::Malformed {
            reason: "canonical forced tool choice has no name".to_owned(),
        }),
        CanonToolChoice::Other { kind } => Ok(Value::String(kind.clone())),
    }
}

fn insert_f64(
    body: &mut Map<String, Value>,
    field: &str,
    value: Option<f64>,
) -> Result<(), TranslateError> {
    let Some(value) = value else {
        return Ok(());
    };
    let value = Number::from_f64(value).ok_or_else(|| TranslateError::Malformed {
        reason: format!("canonical {field} is not finite"),
    })?;
    body.insert(field.to_owned(), Value::Number(value));
    Ok(())
}

fn replay_extensions(
    object: &mut Map<String, Value>,
    extensions: &[CanonicalExtension],
    report: &mut TranslationReport,
) {
    for extension in extensions {
        if extension.source() == DIALECT
            && let Some(name) = extension.wire_name()
        {
            object.insert(name.to_owned(), extension.value().clone());
        } else {
            report_incompatible(extension, report);
        }
    }
}

fn report_incompatible(extension: &CanonicalExtension, report: &mut TranslationReport) {
    report.push(TranslationLoss::new(
        extension.wire_path(),
        TranslationLossReason::IncompatibleExtensionDialect,
        1,
    ));
}

fn unsupported(block: &CanonBlock) -> TranslateError {
    let kind = match block {
        CanonBlock::Text(_) => "text",
        CanonBlock::Image { .. } => "image",
        CanonBlock::ToolUse { .. } => "tool_use",
        CanonBlock::ToolResult { .. } => "tool_result",
        CanonBlock::Thinking { .. } => "thinking",
        CanonBlock::RedactedThinking { .. } => "redacted_thinking",
        CanonBlock::Annotated { block, .. } => return unsupported(block),
    };
    TranslateError::UnsupportedBlock {
        kind: kind.to_owned(),
    }
}

// ── response interpretation ────────────────────────────────────────

/// Stateful interpretation of one Chat Completions SSE turn.
///
/// Chat reports its finish reason before the optional usage-only chunk. The
/// interpreter therefore holds the terminal event until that chunk or
/// `[DONE]`, so canonical absence never masquerades as zero usage.
#[derive(Debug, Clone, Default)]
pub struct OpenAiChatResponseStream {
    started: bool,
    text_open: bool,
    calls: BTreeMap<u64, IncomingToolCall>,
    usage: Option<CanonicalUsage>,
    pending_stop: Option<CanonStopReason>,
    serving_provider: Option<String>,
    ended: bool,
}

#[derive(Debug, Clone, Default)]
struct IncomingToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl OpenAiChatResponseStream {
    pub fn new() -> OpenAiChatResponseStream {
        OpenAiChatResponseStream::default()
    }

    pub fn feed_sse(&mut self, event: &SseEvent) -> Vec<CanonEvent> {
        let data = event.data();
        if data.trim() == "[DONE]" {
            return self.finish_done();
        }
        serde_json::from_str::<Value>(&data)
            .ok()
            .map(|value| self.feed(&value))
            .unwrap_or_default()
    }

    /// Finish an EOF-terminated stream. The shared SSE splitter deliberately
    /// omits `[DONE]`, so the response pump calls this once after its final
    /// event to flush a finish reason, usage, or truncated turn.
    pub fn finish(&mut self) -> Vec<CanonEvent> {
        self.finish_done()
    }

    pub fn feed(&mut self, chunk: &Value) -> Vec<CanonEvent> {
        if self.ended {
            return Vec::new();
        }
        if let Some(error) = chunk.get("error") {
            self.ended = true;
            return vec![CanonEvent::TurnFailed {
                error: canonical_chat_error(error),
            }];
        }
        let mut out = Vec::new();
        if let Some(provider) = chunk.get("provider").and_then(Value::as_str) {
            self.serving_provider = Some(provider.to_owned());
        }
        if !self.started {
            self.started = true;
            out.push(CanonEvent::TurnStarted {
                turn_id: chunk.get("id").and_then(Value::as_str).map(str::to_owned),
            });
        }
        if let Some(usage) = chunk.get("usage").and_then(chat_usage) {
            self.usage = Some(usage);
        }
        for choice in chunk
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if choice.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                continue;
            }
            if let Some(delta) = choice.get("delta") {
                if let Some(text) = delta.get("content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    self.text_open = true;
                    out.push(CanonEvent::TextDelta {
                        delta: text.to_owned(),
                    });
                }
                self.collect_tool_deltas(delta);
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.pending_stop = Some(chat_stop_reason(reason));
                out.extend(self.close_content());
            }
        }
        if self.pending_stop.is_some() && chunk.get("usage").is_some() {
            out.extend(self.finish_pending());
        }
        out
    }

    fn collect_tool_deltas(&mut self, delta: &Value) {
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(index) = call.get("index").and_then(Value::as_u64) else {
                continue;
            };
            let incoming = self.calls.entry(index).or_default();
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                incoming.id.push_str(id);
            }
            if let Some(function) = call.get("function") {
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    incoming.name.push_str(name);
                }
                if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                    incoming.arguments.push_str(arguments);
                }
            }
        }
    }

    fn close_content(&mut self) -> Vec<CanonEvent> {
        let mut out = Vec::new();
        if self.text_open {
            self.text_open = false;
            out.push(CanonEvent::TextEnded);
        }
        for (_, call) in std::mem::take(&mut self.calls) {
            if !call.id.is_empty() && !call.name.is_empty() {
                out.push(CanonEvent::ToolCall(CanonToolCall {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                }));
            }
        }
        out
    }

    fn finish_pending(&mut self) -> Vec<CanonEvent> {
        let Some(stop_reason) = self.pending_stop.take() else {
            return Vec::new();
        };
        self.ended = true;
        if let Some(usage) = self.usage.as_mut() {
            usage.serving_provider.clone_from(&self.serving_provider);
        }
        vec![CanonEvent::TurnEnded {
            stop_reason,
            usage: self.usage.take(),
        }]
    }

    fn finish_done(&mut self) -> Vec<CanonEvent> {
        if self.ended {
            return Vec::new();
        }
        let mut out = self.close_content();
        if self.pending_stop.is_none() {
            self.pending_stop = Some(CanonStopReason::Incomplete("stream_ended".to_owned()));
        }
        out.extend(self.finish_pending());
        out
    }
}

/// Interpret one complete non-streaming Chat Completions response.
pub fn canonical_turn_from_openai_chat(body: &Value) -> Result<CanonTurn, TranslateError> {
    if let Some(error) = body.get("error") {
        return Ok(CanonTurn {
            turn_id: None,
            stop_reason: CanonStopReason::EndTurn,
            usage: None,
            error: Some(canonical_chat_error(error)),
            tool_calls: Vec::new(),
            blocks: None,
            text: String::new(),
            thinking: BTreeMap::new(),
        });
    }
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| TranslateError::Malformed {
            reason: "chat response has no first choice".to_owned(),
        })?;
    let message = choice
        .get("message")
        .ok_or_else(|| TranslateError::Malformed {
            reason: "chat response choice has no message".to_owned(),
        })?;
    let parsed = crate::translate::openai_chat_frontend::from_openai_chat(&json!({
        "messages": [message]
    }))?;
    let blocks = parsed
        .messages
        .into_iter()
        .next()
        .expect("one supplied message parses to one canonical message")
        .blocks;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in &blocks {
        match block.semantic() {
            CanonBlock::Text(part) => text.push_str(part),
            CanonBlock::ToolUse { id, name, input } => tool_calls.push(CanonToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: serde_json::to_string(input)
                    .expect("a parsed tool input always serialises"),
            }),
            _ => {}
        }
    }
    let mut usage = body.get("usage").and_then(chat_usage);
    if let Some(usage) = usage.as_mut() {
        usage.serving_provider = body
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    Ok(CanonTurn {
        turn_id: body.get("id").and_then(Value::as_str).map(str::to_owned),
        stop_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(chat_stop_reason)
            .unwrap_or_else(|| CanonStopReason::Incomplete("missing_finish_reason".to_owned())),
        usage,
        error: None,
        tool_calls,
        blocks: Some(blocks),
        text,
        thinking: BTreeMap::new(),
    })
}

fn chat_stop_reason(reason: &str) -> CanonStopReason {
    match reason {
        "stop" => CanonStopReason::EndTurn,
        "tool_calls" | "function_call" => CanonStopReason::ToolUse,
        "length" => CanonStopReason::MaxTokens,
        "content_filter" => CanonStopReason::Refusal,
        other => CanonStopReason::Incomplete(other.to_owned()),
    }
}

fn chat_usage(value: &Value) -> Option<CanonicalUsage> {
    let object = value.as_object()?;
    Some(CanonicalUsage {
        input: object.get("prompt_tokens").and_then(Value::as_u64),
        cache_read: object
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64),
        cache_write: None,
        output: object.get("completion_tokens").and_then(Value::as_u64),
        reasoning: object
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64),
        serving_provider: None,
        raw: value.clone(),
    })
}

fn canonical_chat_error(error: &Value) -> CanonError {
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let kind = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let joined = format!("{code} {kind}").to_ascii_lowercase();
    CanonError {
        kind: if joined.contains("rate_limit") {
            CanonErrorKind::RateLimit
        } else if joined.contains("auth") || joined.contains("api_key") {
            CanonErrorKind::Authentication
        } else if joined.contains("permission") {
            CanonErrorKind::Permission
        } else if joined.contains("not_found") {
            CanonErrorKind::NotFound
        } else if joined.contains("too_large") || joined.contains("context_length") {
            CanonErrorKind::TooLarge
        } else if joined.contains("invalid") {
            CanonErrorKind::InvalidRequest
        } else {
            CanonErrorKind::Api
        },
        message: error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| (!code.is_empty()).then_some(code))
            .or_else(|| (!kind.is_empty()).then_some(kind))
            .unwrap_or("upstream error")
            .to_owned(),
        resets_at: error.get("resets_at").and_then(Value::as_i64),
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenAiChatResponseStream, canonical_turn_from_openai_chat, render_openai_chat};
    use crate::ir::canonical::{CanonEvent, CanonStopReason};
    use crate::observe::sse::SseSplitter;
    use crate::routing::DialectId;
    use crate::translate::openai_chat_frontend::from_openai_chat;
    use serde_json::Value;

    fn stream_events(fixture: &str) -> Vec<CanonEvent> {
        let mut splitter = SseSplitter::new();
        let mut stream = OpenAiChatResponseStream::new();
        let mut events = splitter
            .feed(fixture.as_bytes())
            .into_iter()
            .flat_map(|event| stream.feed_sse(&event))
            .collect::<Vec<_>>();
        events.extend(stream.finish());
        events
    }

    #[test]
    fn rich_chat_request_round_trips_semantically() {
        let source: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_chat/03_tools.json"
        ))
        .unwrap();
        let canonical = from_openai_chat(&source).unwrap();
        let rendered =
            render_openai_chat(&canonical, DialectId::OpenRouterChatCompletions).unwrap();
        assert!(rendered.report.is_empty());
        let reparsed = from_openai_chat(&rendered.value).unwrap();
        assert_eq!(reparsed, canonical);
    }

    #[test]
    fn replays_nested_and_top_level_chat_extensions() {
        let source: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_chat/07_nested_content.json"
        ))
        .unwrap();
        let canonical = from_openai_chat(&source).unwrap();
        let rendered =
            render_openai_chat(&canonical, DialectId::OpenRouterChatCompletions).unwrap();
        assert!(rendered.report.is_empty());
        assert_eq!(
            rendered.value["messages"][0]["content"][1]["image_url"]["detail"],
            "high"
        );
        assert_eq!(
            rendered.value["messages"][0]["content"][1]["cache_control"]["ttl"],
            "5m"
        );
    }

    #[test]
    fn preserves_developer_role_and_response_format() {
        let source: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_chat/04_developer_roles.json"
        ))
        .unwrap();
        let canonical = from_openai_chat(&source).unwrap();
        let rendered =
            render_openai_chat(&canonical, DialectId::OpenRouterChatCompletions).unwrap();
        assert!(rendered.report.is_empty());
        assert_eq!(rendered.value["messages"][0]["role"], "developer");
        assert_eq!(rendered.value["response_format"]["type"], "json_object");
    }

    #[test]
    fn simple_stream_waits_for_the_usage_only_chunk() {
        let events = stream_events(include_str!(
            "../../tests/fixtures/openai_chat_sse/01_simple_content.sse"
        ));
        assert_eq!(
            events,
            vec![
                CanonEvent::TurnStarted {
                    turn_id: Some("gen-1760000000-3f2a".to_owned()),
                },
                CanonEvent::TextDelta {
                    delta: "Hello".to_owned(),
                },
                CanonEvent::TextDelta {
                    delta: "!".to_owned(),
                },
                CanonEvent::TextEnded,
                CanonEvent::TurnEnded {
                    stop_reason: CanonStopReason::EndTurn,
                    usage: Some(crate::ir::canonical::CanonicalUsage {
                        input: Some(128),
                        cache_read: None,
                        cache_write: None,
                        output: Some(16),
                        reasoning: None,
                        serving_provider: Some("z-ai".to_owned()),
                        raw: serde_json::json!({
                            "prompt_tokens": 128,
                            "completion_tokens": 16,
                            "total_tokens": 144,
                            "cost": 0.000192,
                            "cost_details": {
                                "upstream": 0.00016,
                                "router": 0.000032
                            }
                        }),
                    }),
                },
            ]
        );
    }

    #[test]
    fn fragmented_tool_calls_emit_once_with_complete_arguments() {
        let events = stream_events(include_str!(
            "../../tests/fixtures/openai_chat_sse/02_tool_calls.sse"
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            CanonEvent::ToolCall(call)
                if call.id == "call_kJ4n"
                    && call.name == "get_weather"
                    && call.arguments == "{\"city\":\"Wellington\",\"units\":\"metric\"}"
        )));
        let CanonEvent::TurnEnded { stop_reason, usage } = events.last().unwrap() else {
            panic!("stream should terminate")
        };
        assert_eq!(*stop_reason, CanonStopReason::ToolUse);
        let usage = usage.as_ref().unwrap();
        assert_eq!(usage.cache_read, Some(384));
        assert_eq!(usage.reasoning, Some(12));
    }

    #[test]
    fn truncated_stream_finishes_incomplete_without_inventing_usage() {
        let events = stream_events(include_str!(
            "../../tests/fixtures/openai_chat_sse/05_no_usage.sse"
        ));
        assert!(matches!(
            events.last(),
            Some(CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::Incomplete(reason),
                usage: None,
            }) if reason == "stream_ended"
        ));
    }

    #[test]
    fn complete_chat_response_preserves_order_tools_and_usage() {
        let body = serde_json::json!({
            "id": "gen-complete",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "checking",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "lookup", "arguments": "{\"id\":1}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 4}
        });
        let turn = canonical_turn_from_openai_chat(&body).unwrap();
        assert_eq!(turn.turn_id.as_deref(), Some("gen-complete"));
        assert_eq!(turn.stop_reason, CanonStopReason::ToolUse);
        assert_eq!(turn.text, "checking");
        assert_eq!(turn.tool_calls[0].arguments, "{\"id\":1}");
        assert_eq!(turn.blocks.as_ref().unwrap().len(), 2);
        assert_eq!(turn.usage.unwrap().input, Some(20));
    }
}
