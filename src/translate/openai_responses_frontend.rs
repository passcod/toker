//! OpenAI Responses request ingress: Responses JSON into canonical IR.
//!
//! This adapter describes the frontend wire only. It preserves named
//! reasoning effort without inventing an Anthropic token budget, keeps
//! provider reasoning input items opaque and source-tagged, and retains
//! unmodelled fields at their nearest canonical node.

use serde_json::Value;

use crate::ir::canonical::{
    CanonBlock, CanonMessage, CanonRole, CanonSystemPart, CanonTool, CanonToolChoice,
    CanonicalExtension, CanonicalRequest, SamplingSpec, ThinkingSpec, ToolResultContent,
};
use crate::routing::DialectId;
use crate::translate::TranslateError;

const DIALECT: DialectId = DialectId::CodexResponses;

/// Parse one OpenAI Responses request body into canonical IR.
pub fn from_openai_responses(body: &Value) -> Result<CanonicalRequest, TranslateError> {
    validate_backend_controls(body)?;
    let system = instructions_of(body)?;
    let messages = input_of(body)?;
    let tools = tools_of(body)?;
    let tool_choice = tool_choice_of(body)?;
    let thinking = thinking_of(body)?;
    Ok(CanonicalRequest {
        model: body.get("model").and_then(Value::as_str).map(str::to_owned),
        system,
        messages,
        tools,
        sampling: SamplingSpec::default(),
        thinking,
        stream: body.get("stream").and_then(Value::as_bool),
        tool_choice,
        extensions: extensions(
            body,
            "$",
            &[
                "model",
                "instructions",
                "input",
                "tools",
                "tool_choice",
                "reasoning",
                "stream",
                "prompt_cache_key",
            ],
        )
        .into_iter()
        .chain(reasoning_extensions(body))
        .collect(),
    })
}

fn validate_backend_controls(body: &Value) -> Result<(), TranslateError> {
    for field in ["store", "parallel_tool_calls"] {
        if body
            .get(field)
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
        {
            return Err(TranslateError::Malformed {
                reason: format!("{field} is not a boolean"),
            });
        }
    }
    if let Some(include) = body.get("include").filter(|value| !value.is_null()) {
        let Some(include) = include.as_array() else {
            return Err(TranslateError::Malformed {
                reason: "include is not an array".to_owned(),
            });
        };
        if include.iter().any(|value| !value.is_string()) {
            return Err(TranslateError::Malformed {
                reason: "include contains a non-string value".to_owned(),
            });
        }
    }
    Ok(())
}

fn instructions_of(body: &Value) -> Result<Vec<CanonSystemPart>, TranslateError> {
    match body.get("instructions") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![CanonSystemPart::Text {
            text: text.clone(),
            extensions: Vec::new(),
        }]),
        Some(_) => Err(TranslateError::Malformed {
            reason: "instructions is not a string".to_owned(),
        }),
    }
}

fn input_of(body: &Value) -> Result<Vec<CanonMessage>, TranslateError> {
    match body.get("input") {
        Some(Value::String(text)) => Ok(vec![CanonMessage {
            role: CanonRole::User,
            blocks: vec![CanonBlock::Text(text.clone())],
            extensions: Vec::new(),
        }]),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(index, item)| input_item_of(item, index))
            .collect(),
        None => Err(TranslateError::Malformed {
            reason: "input is missing (not a Responses body)".to_owned(),
        }),
        Some(_) => Err(TranslateError::Malformed {
            reason: "input is neither a string nor an array".to_owned(),
        }),
    }
}

fn input_item_of(item: &Value, index: usize) -> Result<CanonMessage, TranslateError> {
    let kind = item.get("type").and_then(Value::as_str);
    if kind == Some("reasoning") {
        // Reasoning items contain provider-encrypted state whose semantics
        // cannot be flattened into portable thinking text. Keep the whole
        // item opaque at its exact conversation position; the Codex dialect
        // renderer can replay it, while every foreign binding reports it.
        return Ok(CanonMessage {
            role: CanonRole::Assistant,
            blocks: Vec::new(),
            extensions: vec![CanonicalExtension::new(
                DIALECT,
                "$.input[].reasoning",
                item.clone(),
            )],
        });
    }
    if kind == Some("function_call") {
        return function_call_of(item, index);
    }
    if kind == Some("function_call_output") {
        return function_output_of(item, index);
    }
    if kind.is_none() || kind == Some("message") {
        return message_of(item, index);
    }
    Err(TranslateError::UnsupportedBlock {
        kind: kind.unwrap_or("input_item").to_owned(),
    })
}

fn message_of(item: &Value, index: usize) -> Result<CanonMessage, TranslateError> {
    let role =
        item.get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| TranslateError::Malformed {
                reason: format!("input[{index}]: message role is missing or not a string"),
            })?;
    let role = match role {
        "user" => CanonRole::User,
        "assistant" => CanonRole::Assistant,
        "system" => CanonRole::System,
        "developer" => CanonRole::Developer,
        other => {
            return Err(TranslateError::Malformed {
                reason: format!("input[{index}]: unsupported message role {other:?}"),
            });
        }
    };
    let blocks = match item.get("content") {
        Some(Value::String(text)) => vec![CanonBlock::Text(text.clone())],
        Some(Value::Array(parts)) => parts
            .iter()
            .enumerate()
            .map(|(part_index, part)| content_part_of(part, index, part_index))
            .collect::<Result<Vec<_>, _>>()?,
        None | Some(Value::Null) => Vec::new(),
        Some(_) => {
            return Err(TranslateError::Malformed {
                reason: format!("input[{index}]: message content is not a string or array"),
            });
        }
    };
    Ok(CanonMessage {
        role,
        blocks,
        extensions: extensions(item, "$.input[]", &["type", "role", "content"]),
    })
}

fn content_part_of(
    part: &Value,
    item_index: usize,
    part_index: usize,
) -> Result<CanonBlock, TranslateError> {
    let at = format!("input[{item_index}].content[{part_index}]");
    let kind =
        part.get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| TranslateError::Malformed {
                reason: format!("{at}: type is missing or not a string"),
            })?;
    match kind {
        "input_text" | "output_text" => {
            let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                TranslateError::Malformed {
                    reason: format!("{at}: text is missing or not a string"),
                }
            })?;
            Ok(CanonBlock::Text(text.to_owned()).annotated(
                None,
                extensions(part, "$.input[].content[]", &["type", "text"]),
            ))
        }
        "input_image" => {
            let url = part
                .get("image_url")
                .and_then(Value::as_str)
                .ok_or_else(|| TranslateError::Malformed {
                    reason: format!("{at}: image_url is missing or not a string"),
                })?;
            Ok(CanonBlock::Image {
                url: url.to_owned(),
            }
            .annotated(
                None,
                extensions(part, "$.input[].content[]", &["type", "image_url"]),
            ))
        }
        other => Err(TranslateError::UnsupportedBlock {
            kind: other.to_owned(),
        }),
    }
}

fn function_call_of(item: &Value, index: usize) -> Result<CanonMessage, TranslateError> {
    let name = required_string(item, "name", index, "function_call")?;
    let call_id = required_string(item, "call_id", index, "function_call")?;
    let arguments = required_string(item, "arguments", index, "function_call")?;
    let input: Value = serde_json::from_str(arguments).map_err(|_| TranslateError::Malformed {
        reason: format!("input[{index}]: function_call arguments is not JSON"),
    })?;
    if !input.is_object() {
        return Err(TranslateError::Malformed {
            reason: format!("input[{index}]: function_call arguments is not a JSON object"),
        });
    }
    Ok(CanonMessage {
        role: CanonRole::Assistant,
        blocks: vec![
            CanonBlock::ToolUse {
                id: call_id.to_owned(),
                name: name.to_owned(),
                input,
            }
            .annotated(
                None,
                extensions(item, "$.input[]", &["type", "name", "call_id", "arguments"]),
            ),
        ],
        extensions: Vec::new(),
    })
}

fn function_output_of(item: &Value, index: usize) -> Result<CanonMessage, TranslateError> {
    let call_id = required_string(item, "call_id", index, "function_call_output")?;
    let output = required_string(item, "output", index, "function_call_output")?;
    Ok(CanonMessage {
        role: CanonRole::User,
        blocks: vec![
            CanonBlock::ToolResult {
                tool_use_id: call_id.to_owned(),
                content: ToolResultContent::String(output.to_owned()),
            }
            .annotated(
                None,
                extensions(item, "$.input[]", &["type", "call_id", "output"]),
            ),
        ],
        extensions: Vec::new(),
    })
}

fn required_string<'a>(
    item: &'a Value,
    field: &str,
    index: usize,
    kind: &str,
) -> Result<&'a str, TranslateError> {
    item.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| TranslateError::Malformed {
            reason: format!("input[{index}]: {kind} {field} is missing or not a string"),
        })
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
                return Err(TranslateError::UnsupportedBlock {
                    kind: tool
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned(),
                });
            }
            let name = tool.get("name").and_then(Value::as_str).ok_or_else(|| {
                TranslateError::Malformed {
                    reason: format!("tools[{index}].name is missing or not a string"),
                }
            })?;
            let parameters = tool
                .get("parameters")
                .filter(|value| value.is_object())
                .ok_or_else(|| TranslateError::Malformed {
                    reason: format!("tools[{index}].parameters is missing or not an object"),
                })?;
            let description = match tool.get("description") {
                None | Some(Value::Null) => "",
                Some(Value::String(description)) => description,
                Some(_) => {
                    return Err(TranslateError::Malformed {
                        reason: format!("tools[{index}].description is not a string"),
                    });
                }
            };
            Ok(CanonTool {
                name: name.to_owned(),
                description: description.to_owned(),
                parameters: parameters.clone(),
                extensions: extensions(
                    tool,
                    "$.tools[]",
                    &["type", "name", "description", "parameters"],
                ),
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
                        .get("name")
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

fn thinking_of(body: &Value) -> Result<Option<ThinkingSpec>, TranslateError> {
    let Some(reasoning) = body.get("reasoning").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = reasoning
        .as_object()
        .ok_or_else(|| TranslateError::Malformed {
            reason: "reasoning is not an object".to_owned(),
        })?;
    match object.get("effort") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(effort)) => Ok(Some(ThinkingSpec::Effort(effort.clone()))),
        Some(_) => Err(TranslateError::Malformed {
            reason: "reasoning.effort is not a string".to_owned(),
        }),
    }
}

fn reasoning_extensions(body: &Value) -> Vec<CanonicalExtension> {
    body.get("reasoning")
        .into_iter()
        .flat_map(|reasoning| extensions(reasoning, "$.reasoning", &["effort"]))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::from_openai_responses;
    use crate::ir::canonical::{CanonBlock, CanonRole, CanonToolChoice, ThinkingSpec};
    use crate::translate::{TranslateError, render_codex};
    use serde_json::json;

    #[test]
    fn rich_responses_input_preserves_semantics_and_opaque_reasoning() {
        let body = json!({
            "model": "gpt-example",
            "stream": true,
            "instructions": "Be concise.",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "Look", "cache_control": {"type": "ephemeral"}},
                    {"type": "input_image", "image_url": "data:image/png;base64,AA=="}
                ], "item_meta": 1},
                {"type": "function_call", "name": "lookup", "call_id": "call-1",
                 "arguments": "{\"id\":1}"},
                {"type": "function_call_output", "call_id": "call-1", "output": "done"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "opaque"}],
                 "encrypted_content": "ciphertext"}
            ],
            "tools": [{"type": "function", "name": "lookup", "description": "Lookup",
                       "parameters": {"type": "object"}, "strict": false}],
            "tool_choice": "required",
            "reasoning": {"effort": "xhigh", "summary": "auto"},
            "store": false
        });
        let canonical = from_openai_responses(&body).expect("parse");
        assert_eq!(canonical.model.as_deref(), Some("gpt-example"));
        assert_eq!(canonical.system[0].semantic_text(), Some("Be concise."));
        assert_eq!(canonical.messages[0].role, CanonRole::User);
        assert!(matches!(
            canonical.messages[1].blocks[0].semantic(),
            CanonBlock::ToolUse { id, name, .. } if id == "call-1" && name == "lookup"
        ));
        assert!(matches!(
            canonical.messages[2].blocks[0].semantic(),
            CanonBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call-1"
        ));
        assert!(canonical.messages[3].blocks.is_empty());
        assert_eq!(canonical.messages[3].extensions.len(), 1);
        assert_eq!(canonical.tools.len(), 1);
        assert_eq!(canonical.tools[0].extensions[0].wire_name(), Some("strict"));
        assert_eq!(canonical.tool_choice, CanonToolChoice::Any);
        assert_eq!(
            canonical.thinking,
            Some(ThinkingSpec::Effort("xhigh".to_owned()))
        );
        assert!(
            canonical
                .extensions
                .iter()
                .any(|extension| extension.wire_path() == "$.reasoning.summary")
        );
        assert!(
            canonical
                .extensions
                .iter()
                .any(|extension| extension.wire_path() == "$.store")
        );
    }

    #[test]
    fn compatible_codex_rendering_replays_responses_extensions_and_reasoning() {
        let body = json!({
            "model": "gpt-example",
            "stream": true,
            "instructions": "Be concise.",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "Look", "cache_control": {"type": "ephemeral"}}
                ], "item_meta": 1},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "opaque"}],
                 "encrypted_content": "ciphertext"}
            ],
            "tools": [{"type": "function", "name": "lookup", "description": "Lookup",
                       "parameters": {"type": "object"}, "strict": true}],
            "reasoning": {"effort": "xhigh", "summary": "auto"},
            "parallel_tool_calls": true,
            "store": true,
            "include": ["reasoning.encrypted_content", "message.output_text.logprobs"],
            "service_tier": "flex"
        });
        let canonical = from_openai_responses(&body).expect("parse");
        let rendered = render_codex(&canonical, "cache-key").expect("render");
        assert!(rendered.report.is_empty(), "{:?}", rendered.report);
        let value = serde_json::to_value(rendered.value).expect("wire value");
        assert_eq!(value["input"][0]["item_meta"], json!(1));
        assert_eq!(
            value["input"][0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(value["input"][1], body["input"][1]);
        assert_eq!(value["tools"][0]["strict"], json!(true));
        assert_eq!(value["reasoning"], body["reasoning"]);
        assert_eq!(value["parallel_tool_calls"], json!(true));
        assert_eq!(value["store"], json!(true));
        assert_eq!(value["include"], body["include"]);
        assert_eq!(value["service_tier"], json!("flex"));
    }

    #[test]
    fn compatible_codex_rendering_is_deterministic_and_append_prefix_stable() {
        let first_body = json!({
            "model": "gpt-example",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "Look", "cache_control": {"type": "ephemeral"}}
                ], "item_meta": 1},
                {"type": "reasoning", "encrypted_content": "ciphertext"}
            ],
            "reasoning": {"effort": "xhigh", "summary": "auto"},
            "store": true
        });
        let first = from_openai_responses(&first_body).expect("parse first turn");
        let first_render = render_codex(&first, "cache-key").expect("render first turn");
        let repeated = render_codex(&first, "cache-key").expect("render first turn again");
        assert_eq!(first_render, repeated);

        let mut appended_body = first_body;
        appended_body["input"]
            .as_array_mut()
            .expect("input array")
            .push(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Again"}]
            }));
        let appended = from_openai_responses(&appended_body).expect("parse appended turn");
        let appended_render = render_codex(&appended, "cache-key").expect("render appended turn");
        assert_eq!(
            first_render.value.input,
            appended_render.value.input[..first_render.value.input.len()]
        );
    }

    #[test]
    fn merged_same_dialect_message_metadata_is_not_mislabeled_foreign() {
        let canonical = from_openai_responses(&json!({
            "model": "gpt-example",
            "input": [{
                "type": "message",
                "role": "system",
                "content": [{"type": "input_text", "text": "Stay concise."}],
                "item_meta": "cannot survive the merge"
            }]
        }))
        .expect("parse");
        let rendered = render_codex(&canonical, "cache-key").expect("render");
        assert_eq!(
            rendered.report.losses(),
            &[crate::translate::TranslationLoss::new(
                "$.input[].item_meta",
                crate::translate::TranslationLossReason::NotRepresentable,
                1,
            )]
        );
    }

    #[test]
    fn string_input_is_one_user_message_and_effort_stays_absent() {
        let canonical = from_openai_responses(&json!({
            "model": "gpt-example",
            "input": "hello"
        }))
        .expect("parse");
        assert_eq!(canonical.messages.len(), 1);
        assert_eq!(canonical.messages[0].role, CanonRole::User);
        assert_eq!(canonical.thinking, None);
    }

    #[test]
    fn unknown_input_kinds_and_malformed_arguments_are_typed_failures() {
        let unknown = from_openai_responses(&json!({
            "input": [{"type": "computer_call"}]
        }));
        assert!(matches!(
            unknown,
            Err(TranslateError::UnsupportedBlock { kind }) if kind == "computer_call"
        ));
        let malformed = from_openai_responses(&json!({
            "input": [{"type": "function_call", "name": "f", "call_id": "c",
                       "arguments": "[]"}]
        }));
        assert!(matches!(malformed, Err(TranslateError::Malformed { .. })));
    }
}
