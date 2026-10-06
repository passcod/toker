//! Deterministic canonical request rendering for OpenAI Chat bindings.

use serde_json::{Map, Number, Value, json};

use crate::ir::canonical::{
    CanonBlock, CanonMessage, CanonRole, CanonSystemPart, CanonTool, CanonToolChoice,
    CanonicalExtension, CanonicalRequest, ToolResultContent,
};
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

#[cfg(test)]
mod tests {
    use super::render_openai_chat;
    use crate::routing::DialectId;
    use crate::translate::openai_chat_frontend::from_openai_chat;
    use serde_json::Value;

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
}
