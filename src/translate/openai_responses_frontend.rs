//! OpenAI Responses frontend adapter: Responses JSON into canonical IR, and
//! canonical response events/turns back onto the Responses wire.
//!
//! This adapter describes the frontend wire only. It preserves named
//! reasoning effort without inventing an Anthropic token budget, keeps
//! provider reasoning input items opaque and source-tagged, and retains
//! unmodelled fields at their nearest canonical node. Response rendering is
//! deterministic: wire-required identities that canonical events do not carry
//! use stable, position-derived placeholders rather than clocks or randomness.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::ir::canonical::{
    CanonBlock, CanonError, CanonErrorKind, CanonEvent, CanonMessage, CanonRole, CanonStopReason,
    CanonSystemPart, CanonTool, CanonToolChoice, CanonTurn, CanonicalExtension, CanonicalRequest,
    CanonicalUsage, SamplingSpec, ThinkingSpec, ToolResultContent,
};
use crate::observe::sse::SseEvent;
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
        .chain(tool_extensions(body))
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
    match kind {
        Some("function_call") => function_call_of(item, index),
        Some("function_call_output") => function_output_of(item, index),
        None | Some("message") => message_of(item, index),
        Some(kind) => {
            // Responses grows provider-owned conversation item kinds (for
            // example encrypted reasoning, custom tool calls, and Codex's
            // incremental `additional_tools` declaration). Their semantics
            // are not portable, but rejecting a newly introduced item breaks
            // even a Responses-to-Responses route. Keep the complete item at
            // its exact position so a compatible binding can replay it and a
            // foreign binding can report the opaque loss without inspecting
            // or storing its value.
            Ok(CanonMessage {
                role: CanonRole::Assistant,
                blocks: Vec::new(),
                extensions: vec![CanonicalExtension::new(
                    DIALECT,
                    format!("$.input[].{kind}"),
                    item.clone(),
                )],
            })
        }
    }
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
        .filter(|(_, tool)| tool.get("type").and_then(Value::as_str) == Some("function"))
        .map(|(index, tool)| {
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

fn tool_extensions(body: &Value) -> Vec<CanonicalExtension> {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };
    if tools
        .iter()
        .all(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
    {
        return Vec::new();
    }
    // Preserve the complete collection when it contains a provider-owned
    // tool kind. The semantic function subset remains canonical for foreign
    // bindings; a compatible Responses binding replaces its rendered subset
    // with this exact ordered array.
    vec![CanonicalExtension::node_field(
        DIALECT,
        "$.tools",
        "tools",
        Value::Array(tools.clone()),
    )]
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

// ── response rendering ─────────────────────────────────────────────

const UNNAMED_RESPONSE: &str = "resp";

#[derive(Debug, Clone)]
pub struct OpenAiResponsesRenderer {
    model: String,
    response_id: Option<String>,
    sequence: u64,
    next_output_index: u64,
    next_message_id: u64,
    text: Option<TextItem>,
    reasoning: Option<ReasoningItem>,
    started: bool,
    ended: bool,
}

#[derive(Debug, Clone)]
struct TextItem {
    output_index: u64,
    id: String,
    text: String,
}

#[derive(Debug, Clone)]
struct ReasoningItem {
    output_index: u64,
    id: String,
    summaries: BTreeMap<u64, String>,
    encrypted_content: Option<String>,
}

impl OpenAiResponsesRenderer {
    pub fn turn_ended(&self) -> bool {
        self.ended
    }

    pub fn new(model: &str) -> OpenAiResponsesRenderer {
        OpenAiResponsesRenderer {
            model: model.to_owned(),
            response_id: None,
            sequence: 0,
            next_output_index: 0,
            next_message_id: 0,
            text: None,
            reasoning: None,
            started: false,
            ended: false,
        }
    }

    pub fn feed(&mut self, event: &CanonEvent) -> Vec<SseEvent> {
        if self.ended {
            return Vec::new();
        }
        let mut out = Vec::new();
        match event {
            CanonEvent::TurnStarted { turn_id } => {
                if self.started {
                    return out;
                }
                self.started = true;
                self.response_id.clone_from(turn_id);
                let response = json!({
                    "id": self.response_id(),
                    "object": "response",
                    "status": "in_progress",
                    "model": self.model,
                });
                out.push(self.sse("response.created", json!({"response": response})));
            }
            CanonEvent::TextDelta { delta } => {
                self.ensure_text(&mut out);
                let item = self.text.as_mut().expect("text item opened");
                item.text.push_str(delta);
                let output_index = item.output_index;
                let item_id = item.id.clone();
                out.push(self.sse(
                    "response.output_text.delta",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "delta": delta,
                    }),
                ));
            }
            CanonEvent::ThinkingDelta { part, delta } => {
                self.ensure_reasoning(&mut out);
                let item = self.reasoning.as_mut().expect("reasoning item opened");
                item.summaries.entry(*part).or_default().push_str(delta);
                let output_index = item.output_index;
                let item_id = item.id.clone();
                out.push(self.sse(
                    "response.reasoning_summary_text.delta",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "summary_index": part,
                        "delta": delta,
                    }),
                ));
            }
            CanonEvent::ThinkingSignature { .. } => {}
            CanonEvent::RedactedThinking { data } => {
                self.ensure_reasoning(&mut out);
                self.reasoning
                    .as_mut()
                    .expect("reasoning item opened")
                    .encrypted_content = Some(data.clone());
            }
            CanonEvent::ToolCall(call) => {
                let output_index = self.take_output_index();
                let id = format!("fc_{output_index}");
                out.push(self.sse(
                    "response.output_item.added",
                    json!({
                        "output_index": output_index,
                        "item": {
                            "type": "function_call",
                            "id": id,
                            "call_id": call.id,
                            "name": call.name,
                            "arguments": "",
                            "status": "in_progress",
                        },
                    }),
                ));
                out.push(self.sse(
                    "response.output_item.done",
                    json!({
                        "output_index": output_index,
                        "item": {
                            "type": "function_call",
                            "id": id,
                            "call_id": call.id,
                            "name": call.name,
                            "arguments": call.arguments,
                            "status": "completed",
                        },
                    }),
                ));
            }
            CanonEvent::TextEnded => self.finish_text(&mut out),
            CanonEvent::ThinkingEnded => self.finish_reasoning(&mut out),
            CanonEvent::TurnEnded { stop_reason, usage } => {
                self.finish_reasoning(&mut out);
                self.finish_text(&mut out);
                let response =
                    terminal_response(self.response_id(), stop_reason, usage.as_ref(), &self.model);
                let kind = if matches!(
                    stop_reason,
                    CanonStopReason::MaxTokens
                        | CanonStopReason::ContextWindowExceeded
                        | CanonStopReason::Refusal
                        | CanonStopReason::Incomplete(_)
                ) {
                    "response.incomplete"
                } else {
                    "response.completed"
                };
                out.push(self.sse(kind, json!({"response": response})));
                self.ended = true;
            }
            CanonEvent::TurnFailed { error } => {
                let response = json!({
                    "id": self.response_id(),
                    "object": "response",
                    "status": "failed",
                    "model": self.model,
                    "error": responses_error(error),
                });
                out.push(self.sse("response.failed", json!({"response": response})));
                self.ended = true;
            }
            CanonEvent::Error { error } => {
                out.push(self.sse("error", json!({"error": responses_error(error)})));
            }
        }
        out
    }

    fn ensure_text(&mut self, out: &mut Vec<SseEvent>) {
        if self.text.is_some() {
            return;
        }
        let output_index = self.take_output_index();
        let id = format!("msg_{}", self.next_message_id);
        self.next_message_id += 1;
        out.push(self.sse(
            "response.output_item.added",
            json!({
                "output_index": output_index,
                "item": {"type": "message", "id": id, "role": "assistant",
                         "content": [], "status": "in_progress"},
            }),
        ));
        self.text = Some(TextItem {
            output_index,
            id,
            text: String::new(),
        });
    }

    fn ensure_reasoning(&mut self, out: &mut Vec<SseEvent>) {
        if self.reasoning.is_some() {
            return;
        }
        let output_index = self.take_output_index();
        let id = format!("rs_{output_index}");
        out.push(self.sse(
            "response.output_item.added",
            json!({
                "output_index": output_index,
                "item": {"type": "reasoning", "id": id, "summary": []},
            }),
        ));
        self.reasoning = Some(ReasoningItem {
            output_index,
            id,
            summaries: BTreeMap::new(),
            encrypted_content: None,
        });
    }

    fn finish_text(&mut self, out: &mut Vec<SseEvent>) {
        let Some(item) = self.text.take() else {
            return;
        };
        out.push(self.sse(
            "response.output_item.done",
            json!({
                "output_index": item.output_index,
                "item": {"type": "message", "id": item.id, "role": "assistant",
                         "content": [{"type": "output_text", "text": item.text}],
                         "status": "completed"},
            }),
        ));
    }

    fn finish_reasoning(&mut self, out: &mut Vec<SseEvent>) {
        let Some(item) = self.reasoning.take() else {
            return;
        };
        let summary = item
            .summaries
            .into_values()
            .map(|text| json!({"type": "summary_text", "text": text}))
            .collect::<Vec<_>>();
        let mut reasoning = json!({"type": "reasoning", "id": item.id, "summary": summary});
        if let Some(data) = item.encrypted_content {
            reasoning
                .as_object_mut()
                .expect("reasoning item is an object")
                .insert("encrypted_content".to_owned(), json!(data));
        }
        out.push(self.sse(
            "response.output_item.done",
            json!({"output_index": item.output_index, "item": reasoning}),
        ));
    }

    fn take_output_index(&mut self) -> u64 {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
    }

    fn response_id(&self) -> &str {
        self.response_id.as_deref().unwrap_or(UNNAMED_RESPONSE)
    }

    fn sse(&mut self, kind: &str, fields: Value) -> SseEvent {
        let sequence = self.sequence;
        self.sequence += 1;
        let mut value = fields.as_object().cloned().unwrap_or_default();
        value.insert("type".to_owned(), json!(kind));
        value.insert("sequence_number".to_owned(), json!(sequence));
        SseEvent {
            event: Some(kind.to_owned()),
            data_lines: vec![
                serde_json::to_string(&Value::Object(value)).expect("responses event serialises"),
            ],
        }
    }
}

pub fn openai_responses_from_canonical(model: &str, turn: &CanonTurn) -> Value {
    if let Some(error) = &turn.error {
        return json!({
            "id": turn.turn_id.as_deref().unwrap_or(UNNAMED_RESPONSE),
            "object": "response",
            "status": "failed",
            "model": model,
            "output": [],
            "error": responses_error(error),
        });
    }

    let output = response_output(turn);
    let mut response = terminal_response(
        turn.turn_id.as_deref().unwrap_or(UNNAMED_RESPONSE),
        &turn.stop_reason,
        turn.usage.as_ref(),
        model,
    );
    response
        .as_object_mut()
        .expect("terminal response is an object")
        .insert("output".to_owned(), Value::Array(output));
    response
}

fn response_output(turn: &CanonTurn) -> Vec<Value> {
    if let Some(blocks) = &turn.blocks {
        let mut output = Vec::new();
        let mut index = 0;
        while index < blocks.len() {
            if matches!(
                blocks[index].semantic(),
                CanonBlock::Thinking { .. } | CanonBlock::RedactedThinking { .. }
            ) {
                let output_index = output.len();
                let mut summary = Vec::new();
                let mut encrypted_content = None;
                while index < blocks.len() {
                    match blocks[index].semantic() {
                        CanonBlock::Thinking { text, .. } => {
                            summary.push(json!({"type": "summary_text", "text": text}));
                        }
                        CanonBlock::RedactedThinking { data } => {
                            encrypted_content = Some(data);
                        }
                        _ => break,
                    }
                    index += 1;
                }
                let mut reasoning = json!({
                    "type": "reasoning",
                    "id": format!("rs_{output_index}"),
                    "summary": summary,
                });
                if let Some(data) = encrypted_content {
                    reasoning
                        .as_object_mut()
                        .expect("reasoning output is an object")
                        .insert("encrypted_content".to_owned(), json!(data));
                }
                output.push(reasoning);
                continue;
            }
            if let Some(value) = output_of_block(blocks[index].semantic(), output.len()) {
                output.push(value);
            }
            index += 1;
        }
        return output;
    }
    let mut output = Vec::new();
    if !turn.thinking.is_empty() {
        output.push(json!({
            "type": "reasoning",
            "id": "rs_0",
            "summary": turn.thinking.values()
                .map(|text| json!({"type": "summary_text", "text": text}))
                .collect::<Vec<_>>(),
        }));
    }
    if !turn.text.is_empty() {
        output.push(json!({
            "type": "message",
            "id": "msg_0",
            "role": "assistant",
            "content": [{"type": "output_text", "text": turn.text}],
            "status": "completed",
        }));
    }
    output.extend(turn.tool_calls.iter().enumerate().map(|(index, call)| {
        json!({
            "type": "function_call",
            "id": format!("fc_{index}"),
            "call_id": call.id,
            "name": call.name,
            "arguments": call.arguments,
            "status": "completed",
        })
    }));
    output
}

fn output_of_block(block: &CanonBlock, index: usize) -> Option<Value> {
    match block {
        CanonBlock::Text(text) => Some(json!({
            "type": "message", "id": format!("msg_{index}"), "role": "assistant",
            "content": [{"type": "output_text", "text": text}], "status": "completed",
        })),
        CanonBlock::Thinking { text, .. } => Some(json!({
            "type": "reasoning", "id": format!("rs_{index}"),
            "summary": [{"type": "summary_text", "text": text}],
        })),
        CanonBlock::RedactedThinking { data } => Some(json!({
            "type": "reasoning", "id": format!("rs_{index}"), "summary": [],
            "encrypted_content": data,
        })),
        CanonBlock::ToolUse { id, name, input } => Some(json!({
            "type": "function_call", "id": format!("fc_{index}"), "call_id": id,
            "name": name, "arguments": serde_json::to_string(input)
                .expect("tool input serialises"), "status": "completed",
        })),
        CanonBlock::Image { .. } | CanonBlock::ToolResult { .. } | CanonBlock::Annotated { .. } => {
            None
        }
    }
}

fn terminal_response(
    id: &str,
    stop_reason: &CanonStopReason,
    usage: Option<&CanonicalUsage>,
    model: &str,
) -> Value {
    let incomplete = incomplete_reason(stop_reason);
    let mut response = Map::new();
    response.insert("id".to_owned(), json!(id));
    response.insert("object".to_owned(), json!("response"));
    response.insert(
        "status".to_owned(),
        json!(if incomplete.is_some() {
            "incomplete"
        } else {
            "completed"
        }),
    );
    response.insert("model".to_owned(), json!(model));
    if let Some(reason) = incomplete {
        response.insert("incomplete_details".to_owned(), json!({"reason": reason}));
    } else {
        response.insert(
            "end_turn".to_owned(),
            json!(!matches!(stop_reason, CanonStopReason::ToolUse)),
        );
    }
    if let Some(usage) = usage {
        response.insert("usage".to_owned(), responses_usage(usage));
    }
    Value::Object(response)
}

fn incomplete_reason(reason: &CanonStopReason) -> Option<&str> {
    match reason {
        CanonStopReason::MaxTokens => Some("max_output_tokens"),
        CanonStopReason::ContextWindowExceeded => Some("context_window_exceeded"),
        CanonStopReason::Refusal => Some("content_filter"),
        CanonStopReason::Incomplete(reason) => Some(reason),
        CanonStopReason::EndTurn
        | CanonStopReason::ToolUse
        | CanonStopReason::StopSequence
        | CanonStopReason::PauseTurn => None,
    }
}

fn responses_usage(usage: &CanonicalUsage) -> Value {
    let mut value = Map::new();
    if let Some(input) = usage.input {
        value.insert("input_tokens".to_owned(), json!(input));
    }
    if usage.cache_read.is_some() || usage.cache_write.is_some() {
        let mut details = Map::new();
        if let Some(cached) = usage.cache_read {
            details.insert("cached_tokens".to_owned(), json!(cached));
        }
        if let Some(written) = usage.cache_write {
            details.insert("cache_write_tokens".to_owned(), json!(written));
        }
        value.insert("input_tokens_details".to_owned(), Value::Object(details));
    }
    if let Some(output) = usage.output {
        value.insert("output_tokens".to_owned(), json!(output));
    }
    if let Some(reasoning) = usage.reasoning {
        value.insert(
            "output_tokens_details".to_owned(),
            json!({"reasoning_tokens": reasoning}),
        );
    }
    if let (Some(input), Some(output)) = (usage.input, usage.output) {
        value.insert("total_tokens".to_owned(), json!(input + output));
    }
    Value::Object(value)
}

fn responses_error(error: &CanonError) -> Value {
    let kind = match error.kind {
        CanonErrorKind::RateLimit => "rate_limit_error",
        CanonErrorKind::InvalidRequest => "invalid_request_error",
        CanonErrorKind::Authentication => "authentication_error",
        CanonErrorKind::Permission => "permission_error",
        CanonErrorKind::NotFound => "not_found_error",
        CanonErrorKind::TooLarge => "request_too_large",
        CanonErrorKind::Overloaded => "overloaded_error",
        CanonErrorKind::Api => "api_error",
    };
    let mut value = Map::new();
    value.insert("type".to_owned(), json!(kind));
    value.insert("code".to_owned(), json!(kind));
    value.insert("message".to_owned(), json!(error.message));
    if let Some(resets_at) = error.resets_at {
        value.insert("resets_at".to_owned(), json!(resets_at));
    }
    Value::Object(value)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{OpenAiResponsesRenderer, from_openai_responses, openai_responses_from_canonical};
    use crate::ir::canonical::{
        CanonBlock, CanonEvent, CanonRole, CanonStopReason, CanonToolCall, CanonToolChoice,
        CanonTurn, CanonicalUsage, ThinkingSpec,
    };
    use crate::providers::codex::ResponsesSse;
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
    fn canonical_events_render_as_responses_sse_and_round_trip() {
        let usage = CanonicalUsage {
            input: Some(12),
            cache_read: Some(4),
            cache_write: None,
            output: Some(7),
            reasoning: Some(3),
            serving_provider: None,
            raw: json!({
                "input_tokens": 12,
                "input_tokens_details": {"cached_tokens": 4},
                "output_tokens": 7,
                "output_tokens_details": {"reasoning_tokens": 3},
                "total_tokens": 19
            }),
        };
        let canonical = vec![
            CanonEvent::TurnStarted {
                turn_id: Some("resp_1".to_owned()),
            },
            CanonEvent::ThinkingDelta {
                part: 0,
                delta: "consider".to_owned(),
            },
            CanonEvent::RedactedThinking {
                data: "ciphertext".to_owned(),
            },
            CanonEvent::ThinkingEnded,
            CanonEvent::TextDelta {
                delta: "hello".to_owned(),
            },
            CanonEvent::TextEnded,
            CanonEvent::ToolCall(CanonToolCall {
                id: "call_1".to_owned(),
                name: "lookup".to_owned(),
                arguments: "{\"id\":1}".to_owned(),
            }),
            CanonEvent::TurnEnded {
                stop_reason: CanonStopReason::ToolUse,
                usage: Some(usage),
            },
        ];
        let mut renderer = OpenAiResponsesRenderer::new("visible-model");
        let rendered = canonical
            .iter()
            .flat_map(|event| renderer.feed(event))
            .collect::<Vec<_>>();
        let kinds = rendered
            .iter()
            .map(|event| event.event.as_deref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                "response.created",
                "response.output_item.added",
                "response.reasoning_summary_text.delta",
                "response.output_item.done",
                "response.output_item.added",
                "response.output_text.delta",
                "response.output_item.done",
                "response.output_item.added",
                "response.output_item.done",
                "response.completed",
            ]
        );
        for (sequence, event) in rendered.iter().enumerate() {
            let value: serde_json::Value = serde_json::from_str(&event.data()).expect("event JSON");
            assert_eq!(value["sequence_number"], json!(sequence));
        }
        let reasoning_done: serde_json::Value =
            serde_json::from_str(&rendered[3].data()).expect("reasoning done JSON");
        assert_eq!(
            reasoning_done["item"]["encrypted_content"],
            json!("ciphertext")
        );

        let wire = rendered
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {}\n\n",
                    event.event.as_deref().unwrap(),
                    event.data()
                )
            })
            .collect::<String>();
        let mut parser = ResponsesSse::new();
        let parsed = parser.feed(wire.as_bytes());
        let mut interpreter = crate::translate::codex_backend::CanonStream::new();
        let round_tripped = parsed
            .iter()
            .flat_map(|event| interpreter.feed(event))
            .collect::<Vec<_>>();
        assert_eq!(round_tripped, canonical);
    }

    #[test]
    fn complete_canonical_turn_renders_as_a_responses_body() {
        let turn = CanonTurn {
            turn_id: Some("resp_1".to_owned()),
            stop_reason: CanonStopReason::Incomplete("max_output_tokens".to_owned()),
            usage: None,
            error: None,
            tool_calls: vec![CanonToolCall {
                id: "call_1".to_owned(),
                name: "lookup".to_owned(),
                arguments: "{\"id\":1}".to_owned(),
            }],
            blocks: Some(vec![
                CanonBlock::Thinking {
                    text: "consider".to_owned(),
                    signature: None,
                },
                CanonBlock::RedactedThinking {
                    data: "ciphertext".to_owned(),
                },
                CanonBlock::Text("partial".to_owned()),
                CanonBlock::ToolUse {
                    id: "call_1".to_owned(),
                    name: "lookup".to_owned(),
                    input: json!({"id": 1}),
                },
            ]),
            text: "partial".to_owned(),
            thinking: BTreeMap::from([(0, "consider".to_owned())]),
        };
        let response = openai_responses_from_canonical("visible-model", &turn);
        assert_eq!(response["status"], json!("incomplete"));
        assert_eq!(
            response["incomplete_details"]["reason"],
            json!("max_output_tokens")
        );
        assert_eq!(response["output"][0]["type"], json!("reasoning"));
        assert_eq!(
            response["output"][0]["encrypted_content"],
            json!("ciphertext")
        );
        assert_eq!(response["output"][1]["type"], json!("message"));
        assert_eq!(response["output"][2]["call_id"], json!("call_1"));
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
    fn provider_input_items_replay_opaquely_and_malformed_arguments_still_fail() {
        let body = json!({
            "model": "gpt-example",
            "tools": [
                {"type": "function", "name": "lookup", "description": "Lookup",
                 "parameters": {"type": "object"}},
                {"type": "custom", "name": "exec", "format": {"type": "grammar"}}
            ],
            "input": [
                {"type": "additional_tools", "id": "at_1", "role": "developer",
                 "tools": [{"type": "custom", "name": "exec"}]},
                {"type": "custom_tool_call", "call_id": "call_1", "name": "exec",
                 "input": "opaque input"}
            ]
        });
        let canonical = from_openai_responses(&body).expect("opaque items parse");
        assert_eq!(canonical.messages.len(), 2);
        assert_eq!(canonical.tools.len(), 1);
        assert!(
            canonical
                .messages
                .iter()
                .all(|message| message.blocks.is_empty())
        );
        let rendered = render_codex(&canonical, "cache-key").expect("opaque items render");
        assert!(rendered.report.is_empty(), "{:?}", rendered.report);
        let value = serde_json::to_value(rendered.value).expect("wire value");
        assert_eq!(value["input"], body["input"]);
        assert_eq!(value["tools"], body["tools"]);

        let malformed = from_openai_responses(&json!({
            "input": [{"type": "function_call", "name": "f", "call_id": "c",
                       "arguments": "[]"}]
        }));
        assert!(matches!(malformed, Err(TranslateError::Malformed { .. })));
    }
}
