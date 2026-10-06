//! OpenAI Chat request ingress: Chat Completions JSON into canonical IR.
//!
//! Parsing normalises wire shape but keeps semantic distinctions. In
//! particular, `developer` remains distinct from `system`, tool-result
//! messages become addressed canonical tool results, and unmodelled fields
//! stay attached to their nearest canonical node.

use serde_json::Value;

use crate::ir::canonical::{
    CanonBlock, CanonMessage, CanonRole, CanonTool, CanonToolChoice, CanonicalExtension,
    CanonicalRequest, SamplingSpec, ToolResultContent,
};
use crate::routing::DialectId;
use crate::translate::TranslateError;

const DIALECT: DialectId = DialectId::OpenRouterChatCompletions;

pub fn from_openai_chat(body: &Value) -> Result<CanonicalRequest, TranslateError> {
    if let (Some(legacy), Some(current)) = (
        body.get("max_tokens").filter(|value| !value.is_null()),
        body.get("max_completion_tokens")
            .filter(|value| !value.is_null()),
    ) && legacy != current
    {
        return Err(TranslateError::Malformed {
            reason: "max_tokens and max_completion_tokens disagree".to_owned(),
        });
    }
    let messages = messages_of(body)?;
    let tools = tools_of(body)?;
    let tool_choice = tool_choice_of(body)?;
    Ok(CanonicalRequest {
        model: body.get("model").and_then(Value::as_str).map(str::to_owned),
        system: Vec::new(),
        messages,
        tools,
        sampling: sampling_of(body),
        thinking: None,
        stream: body.get("stream").and_then(Value::as_bool),
        tool_choice,
        extensions: extensions(
            body,
            "$",
            &[
                "model",
                "messages",
                "tools",
                "tool_choice",
                "temperature",
                "top_p",
                "max_tokens",
                "max_completion_tokens",
                "stop",
                "stream",
            ],
        ),
    })
}

fn messages_of(body: &Value) -> Result<Vec<CanonMessage>, TranslateError> {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return Err(TranslateError::Malformed {
            reason: "messages is missing or not an array (not a chat completions body)".to_owned(),
        });
    };
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| message_of(message, index))
        .collect()
}

fn message_of(message: &Value, index: usize) -> Result<CanonMessage, TranslateError> {
    let role =
        message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| TranslateError::Malformed {
                reason: format!("messages[{index}]: role is missing or not a string"),
            })?;
    let (role, blocks, modeled): (CanonRole, Vec<CanonBlock>, &[&str]) = match role {
        "system" => (
            CanonRole::System,
            content_of(message.get("content"), CanonRole::System, index)?,
            &["role", "content"],
        ),
        "developer" => (
            CanonRole::Developer,
            content_of(message.get("content"), CanonRole::Developer, index)?,
            &["role", "content"],
        ),
        "user" => (
            CanonRole::User,
            content_of(message.get("content"), CanonRole::User, index)?,
            &["role", "content"],
        ),
        "assistant" => {
            let mut blocks = content_of(message.get("content"), CanonRole::Assistant, index)?;
            blocks.extend(tool_calls_of(message, index)?);
            (
                CanonRole::Assistant,
                blocks,
                &["role", "content", "tool_calls"],
            )
        }
        "tool" => {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| TranslateError::Malformed {
                    reason: format!("messages[{index}]: tool message has no tool_call_id"),
                })?;
            let content = match message.get("content") {
                Some(Value::String(text)) => ToolResultContent::String(text.clone()),
                None | Some(Value::Null) => ToolResultContent::String(String::new()),
                Some(Value::Array(parts)) => ToolResultContent::Blocks(
                    parts
                        .iter()
                        .enumerate()
                        .map(|(part_index, part)| {
                            content_part_of(part, CanonRole::User, index, part_index)
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                ),
                Some(other) => {
                    return Err(TranslateError::Malformed {
                        reason: format!(
                            "messages[{index}]: tool content is neither a string, null, nor an array ({})",
                            json_kind(other)
                        ),
                    });
                }
            };
            (
                CanonRole::User,
                vec![CanonBlock::ToolResult {
                    tool_use_id: call_id.to_owned(),
                    content,
                }],
                &["role", "tool_call_id", "content"],
            )
        }
        other => {
            return Err(TranslateError::Malformed {
                reason: format!("messages[{index}]: unsupported role {other:?}"),
            });
        }
    };
    Ok(CanonMessage {
        role,
        blocks,
        extensions: extensions(message, "$.messages[]", modeled),
    })
}

fn content_of(
    content: Option<&Value>,
    role: CanonRole,
    message_index: usize,
) -> Result<Vec<CanonBlock>, TranslateError> {
    match content {
        Some(Value::String(text)) => Ok(vec![CanonBlock::Text(text.clone())]),
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(parts)) => parts
            .iter()
            .enumerate()
            .map(|(part_index, part)| content_part_of(part, role, message_index, part_index))
            .collect(),
        Some(other) => Err(TranslateError::Malformed {
            reason: format!(
                "messages[{message_index}]: content is neither a string, null, nor an array ({})",
                json_kind(other)
            ),
        }),
    }
}

fn content_part_of(
    part: &Value,
    role: CanonRole,
    message_index: usize,
    part_index: usize,
) -> Result<CanonBlock, TranslateError> {
    let at = format!("messages[{message_index}] part {part_index}");
    let kind =
        part.get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| TranslateError::Malformed {
                reason: format!("{at}: no type"),
            })?;
    match kind {
        "text" => {
            let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                TranslateError::Malformed {
                    reason: format!("{at}: text is missing or not a string"),
                }
            })?;
            Ok(CanonBlock::Text(text.to_owned()).annotated(
                None,
                extensions(part, "$.messages[].content[]", &["type", "text"]),
            ))
        }
        "image_url" if role == CanonRole::User => {
            let image = part
                .get("image_url")
                .and_then(Value::as_object)
                .ok_or_else(|| TranslateError::Malformed {
                    reason: format!("{at}: image_url is missing or not an object"),
                })?;
            let url = image.get("url").and_then(Value::as_str).ok_or_else(|| {
                TranslateError::Malformed {
                    reason: format!("{at}: image_url.url is missing or not a string"),
                }
            })?;
            let mut retained = extensions(part, "$.messages[].content[]", &["type", "image_url"]);
            retained.extend(extensions(
                part.get("image_url").expect("checked above"),
                "$.messages[].content[].image_url",
                &["url"],
            ));
            Ok(CanonBlock::Image {
                url: url.to_owned(),
            }
            .annotated(None, retained))
        }
        other => Err(TranslateError::UnsupportedBlock {
            kind: other.to_owned(),
        }),
    }
}

fn tool_calls_of(message: &Value, message_index: usize) -> Result<Vec<CanonBlock>, TranslateError> {
    let Some(calls) = message.get("tool_calls").filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let Value::Array(calls) = calls else {
        return Err(TranslateError::Malformed {
            reason: format!("messages[{message_index}]: tool_calls is not an array"),
        });
    };
    calls
        .iter()
        .enumerate()
        .map(|(call_index, call)| {
            let at = format!("messages[{message_index}] tool_calls[{call_index}]");
            let id = call.get("id").and_then(Value::as_str).ok_or_else(|| {
                TranslateError::Malformed {
                    reason: format!("{at}: id is missing or not a string"),
                }
            })?;
            if call.get("type").and_then(Value::as_str) != Some("function") {
                return Err(TranslateError::UnsupportedBlock {
                    kind: call
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("tool_call")
                        .to_owned(),
                });
            }
            let function = call
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| TranslateError::Malformed {
                    reason: format!("{at}: function is missing or not an object"),
                })?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| TranslateError::Malformed {
                    reason: format!("{at}: function.name is missing or not a string"),
                })?;
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| TranslateError::Malformed {
                    reason: format!("{at}: function.arguments is missing or not a string"),
                })?;
            let input: Value =
                serde_json::from_str(arguments).map_err(|_| TranslateError::Malformed {
                    reason: format!("{at}: function.arguments is not JSON"),
                })?;
            if !input.is_object() {
                return Err(TranslateError::Malformed {
                    reason: format!("{at}: function.arguments is not a JSON object"),
                });
            }
            let mut retained = extensions(
                call,
                "$.messages[].tool_calls[]",
                &["id", "type", "function"],
            );
            retained.extend(extensions(
                call.get("function").expect("checked above"),
                "$.messages[].tool_calls[].function",
                &["name", "arguments"],
            ));
            Ok(CanonBlock::ToolUse {
                id: id.to_owned(),
                name: name.to_owned(),
                input,
            }
            .annotated(None, retained))
        })
        .collect()
}

fn tools_of(body: &Value) -> Result<Vec<CanonTool>, TranslateError> {
    let Some(tools) = body.get("tools").filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let Value::Array(tools) = tools else {
        return Err(TranslateError::Malformed {
            reason: "tools is not an array".to_owned(),
        });
    };
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err(TranslateError::Malformed {
                    reason: format!("tools[{index}] is not a function tool"),
                });
            }
            let function = tool
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| TranslateError::Malformed {
                    reason: format!("tools[{index}].function is missing or not an object"),
                })?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| TranslateError::Malformed {
                    reason: format!("tools[{index}].function.name is missing"),
                })?;
            let parameters = function
                .get("parameters")
                .filter(|value| value.is_object())
                .ok_or_else(|| TranslateError::Malformed {
                    reason: format!(
                        "tools[{index}].function.parameters is missing or not an object"
                    ),
                })?;
            let description = match function.get("description") {
                None | Some(Value::Null) => "",
                Some(Value::String(value)) => value,
                Some(_) => {
                    return Err(TranslateError::Malformed {
                        reason: format!("tools[{index}].function.description is not a string"),
                    });
                }
            };
            let mut retained = extensions(tool, "$.tools[]", &["type", "function"]);
            retained.extend(extensions(
                tool.get("function").expect("checked above"),
                "$.tools[].function",
                &["name", "description", "parameters"],
            ));
            Ok(CanonTool {
                name: name.to_owned(),
                description: description.to_owned(),
                parameters: parameters.clone(),
                extensions: retained,
            })
        })
        .collect()
}

fn tool_choice_of(body: &Value) -> Result<CanonToolChoice, TranslateError> {
    match body.get("tool_choice") {
        None | Some(Value::Null) => Ok(CanonToolChoice::Auto),
        Some(Value::String(value)) if value == "auto" => Ok(CanonToolChoice::Auto),
        Some(Value::String(value)) if value == "required" => Ok(CanonToolChoice::Any),
        Some(Value::String(value)) if value == "none" => Ok(CanonToolChoice::None),
        Some(Value::String(value)) => Ok(CanonToolChoice::Other {
            kind: value.clone(),
        }),
        Some(Value::Object(choice)) => {
            let kind = choice.get("type").and_then(Value::as_str).ok_or_else(|| {
                TranslateError::Malformed {
                    reason: "tool_choice has no type".to_owned(),
                }
            })?;
            if kind == "function" {
                Ok(CanonToolChoice::Tool {
                    name: choice
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                })
            } else {
                Ok(CanonToolChoice::Other {
                    kind: kind.to_owned(),
                })
            }
        }
        Some(_) => Err(TranslateError::Malformed {
            reason: "tool_choice is neither a string nor an object".to_owned(),
        }),
    }
}

fn sampling_of(body: &Value) -> SamplingSpec {
    let stop_sequences = match body.get("stop") {
        Some(Value::String(stop)) => Some(vec![stop.clone()]),
        Some(Value::Array(stops)) => Some(
            stops
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        ),
        _ => None,
    };
    SamplingSpec {
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        max_tokens: body
            .get("max_completion_tokens")
            .and_then(Value::as_u64)
            .or_else(|| body.get("max_tokens").and_then(Value::as_u64)),
        stop_sequences,
    }
}

fn extensions(source: &Value, path: &str, modeled: &[&str]) -> Vec<CanonicalExtension> {
    source
        .as_object()
        .into_iter()
        .flat_map(|object| object.iter())
        .filter(|(field, _)| !modeled.contains(&field.as_str()))
        .map(|(field, value)| {
            CanonicalExtension::node_field(DIALECT, format!("{path}.{field}"), field, value.clone())
        })
        .collect()
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::from_openai_chat;
    use crate::ir::canonical::{CanonBlock, CanonRole, CanonToolChoice};
    use serde_json::Value;

    fn fixture(name: &str) -> Value {
        serde_json::from_str(include_str!(concat!(
            "../../tests/fixtures/openai_chat/",
            "03_tools.json"
        )))
        .map(|value| {
            if name == "03_tools" {
                value
            } else {
                unreachable!()
            }
        })
        .unwrap()
    }

    #[test]
    fn parses_tool_conversation() {
        let canonical = from_openai_chat(&fixture("03_tools")).unwrap();
        assert_eq!(canonical.tool_choice, CanonToolChoice::Auto);
        assert_eq!(canonical.tools.len(), 2);
        assert!(matches!(
            canonical.messages[2].blocks[0].semantic(),
            CanonBlock::ToolUse { id, name, .. }
                if id == "call_abc123" && name == "get_weather"
        ));
        assert!(matches!(
            canonical.messages[3].blocks[0].semantic(),
            CanonBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_abc123"
        ));
    }

    #[test]
    fn keeps_developer_distinct_and_top_level_extensions_opaque() {
        let body: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_chat/04_developer_roles.json"
        ))
        .unwrap();
        let canonical = from_openai_chat(&body).unwrap();
        assert_eq!(canonical.messages[0].role, CanonRole::Developer);
        assert_eq!(canonical.messages[1].role, CanonRole::System);
        assert_eq!(canonical.extensions.len(), 1);
        assert_eq!(canonical.extensions[0].wire_path(), "$.response_format");
    }

    #[test]
    fn keeps_nested_part_metadata_node_local() {
        let body: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_chat/07_nested_content.json"
        ))
        .unwrap();
        let canonical = from_openai_chat(&body).unwrap();
        let CanonBlock::Annotated { extensions, .. } = &canonical.messages[0].blocks[1] else {
            panic!("image metadata should annotate its block")
        };
        assert_eq!(extensions.len(), 2);
        assert!(
            extensions
                .iter()
                .any(|extension| extension.wire_name() == Some("detail"))
        );
        assert!(
            extensions
                .iter()
                .any(|extension| extension.wire_name() == Some("cache_control"))
        );
    }
}
