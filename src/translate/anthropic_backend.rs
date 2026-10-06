//! Canonical request rendering for Anthropic Messages backend bindings.
//!
//! This is the request-egress half of the backend adapter. It always builds a
//! deterministic wire value from canonical IR, including on a Messages to
//! Messages route. Anthropic and OpenRouter's Messages dialects share the
//! extension replay domain; extensions from unrelated dialects are omitted
//! with content-free losses.

use serde_json::{Map, Number, Value, json};

use crate::ir::canonical::{
    CanonBlock, CanonMessage, CanonRole, CanonSystemPart, CanonTool, CanonToolChoice,
    CanonicalExtension, CanonicalRequest, ToolResultContent,
};
use crate::routing::DialectId;
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
    };
    json!({
        "role": role,
        "content": message
            .blocks
            .iter()
            .filter_map(|block| block_value(block, dialect, report))
            .collect::<Vec<_>>(),
    })
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

#[cfg(test)]
mod tests {
    use super::render_anthropic;
    use crate::ir::canonical::{CanonBlock, CanonMessage, CanonRole, CanonicalExtension};
    use crate::routing::DialectId;
    use crate::translate::anthropic_frontend::from_anthropic;
    use crate::translate::{TranslateError, TranslationLoss, TranslationLossReason};
    use serde_json::json;

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
