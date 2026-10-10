//! The codex wire types: the request body, the input items, the tools,
//! the usage, and the responses-dialect SSE event types the parser
//! yields.
//!
//! The item and tool types follow the IR's stance ([crate::ir]): a
//! preserved [`serde_json::Value`] with typed views layered on top, so
//! every kind and field toker does not model survives the trip
//! verbatim — the codex CLI's item kinds (web search calls, custom tool
//! calls, …) and per-item extensions round-trip unharmed while the
//! kinds the proxy understands parse typed.
//!
//! The request body is typed to the wire the codex client sends, field
//! order included: the routing fields (`model`, `stream`) lead, and
//! `store: false` / `include: ["reasoning.encrypted_content"]` are
//! constants of this backend — the constructor sets them and the tests
//! pin them. Everything else a body carries rides in `extra`, emitted
//! after the pinned fields (the IR's raw-preservation rule, applied to
//! an outbound body).

use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

// ── the request body ─────────────────────────────────────────────────

/// The Responses request body toker sends upstream — the codex client's
/// `ResponsesApiRequest` shape. Field order is the wire's: the routing
/// fields (`model`, `stream`) first, then instructions, input, tools,
/// tool_choice, parallel_tool_calls, reasoning, `store`, `include`,
/// `prompt_cache_key`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponsesRequest {
    /// The model slug.
    pub model: String,
    /// Whether to stream — always `true` for this backend's turns.
    pub stream: bool,
    /// The base instructions (the system prompt).
    pub instructions: String,
    /// The conversation: message, function_call, function_call_output,
    /// and reasoning items ([`Item`]).
    pub input: Vec<Item>,
    /// The callable tools ([`Tool`]).
    pub tools: Vec<Tool>,
    /// Always `"auto"` on this wire.
    pub tool_choice: String,
    /// Always `false` on this wire (the codex client sends parallel
    /// tool calls off).
    pub parallel_tool_calls: bool,
    pub reasoning: Reasoning,
    /// **Always `false`** — a wire constant of the codex backend, set
    /// by [`ResponsesRequest::new`] and pinned by test.
    pub store: bool,
    /// **Always `["reasoning.encrypted_content"]`** — the reasoning
    /// items must round-trip through a stateless proxy (`store` is off,
    /// so the encrypted content is the only reasoning that survives).
    pub include: Vec<String>,
    /// The prompt cache key — also the `session-id` header (see
    /// [`super::CodexSub::turn_headers`]).
    pub prompt_cache_key: Option<String>,
    /// Every other field the body carries (the IR's raw preservation:
    /// `text`, `service_tier`, `stream_options`, …), re-emitted after
    /// the pinned fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ResponsesRequest {
    /// The request for one turn: the wire constants set — `stream`,
    /// `tool_choice: "auto"`, `parallel_tool_calls: false`,
    /// `store: false`, `include: ["reasoning.encrypted_content"]`.
    pub fn new(model: &str, prompt_cache_key: &str) -> ResponsesRequest {
        ResponsesRequest {
            model: model.to_owned(),
            stream: true,
            instructions: String::new(),
            input: Vec::new(),
            tools: Vec::new(),
            tool_choice: "auto".to_owned(),
            parallel_tool_calls: false,
            reasoning: Reasoning::default(),
            store: false,
            include: vec!["reasoning.encrypted_content".to_owned()],
            prompt_cache_key: Some(prompt_cache_key.to_owned()),
            extra: Default::default(),
        }
    }
}

/// The `reasoning` parameter: `{effort, summary?}` (the codex client
/// sends `summary` only when the model supports summaries).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reasoning {
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Future Responses reasoning fields, retained for compatible replay.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// ── input items ──────────────────────────────────────────────────────

/// One `input` item: the wire object verbatim, with typed views for the
/// kinds toker understands. Unknown kinds and unknown fields on known
/// kinds round-trip untouched (the IR stance — see the module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item(pub Value);

impl Item {
    /// A message: `{"type":"message","role","content"}` — content parts
    /// are `input_text`/`output_text` pairs.
    pub fn message(role: &str, content: Vec<ContentPart>) -> Item {
        Item(json!({
            "type": "message",
            "role": role,
            "content": content,
        }))
    }

    /// A function call: `{"type":"function_call","name","arguments",
    /// "call_id"}` — `arguments` is a **JSON string**, not an object
    /// (the Responses wire).
    pub fn function_call(name: &str, arguments: &str, call_id: &str) -> Item {
        Item(json!({
            "type": "function_call",
            "name": name,
            "arguments": arguments,
            "call_id": call_id,
        }))
    }

    /// A function call's result: `{"type":"function_call_output",
    /// "call_id","output"}`.
    pub fn function_call_output(call_id: &str, output: &str) -> Item {
        Item(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        }))
    }

    /// A reasoning item: `{"type":"reasoning","summary":[{"type":
    /// "summary_text","text"}…],"encrypted_content"?}` — the summary
    /// texts in order, the opaque encrypted content when present.
    pub fn reasoning(summary: Vec<String>, encrypted_content: Option<&str>) -> Item {
        let summary: Vec<Value> = summary
            .into_iter()
            .map(|text| json!({"type": "summary_text", "text": text}))
            .collect();
        let mut item = json!({
            "type": "reasoning",
            "summary": summary,
        });
        if let Some(encrypted_content) = encrypted_content {
            item["encrypted_content"] = json!(encrypted_content);
        }
        Item(item)
    }

    /// The item's `type` tag, when it carries one.
    pub fn kind(&self) -> Option<&str> {
        self.0.get("type").and_then(Value::as_str)
    }

    /// The provider-owned item identity, when the item carries one.
    pub fn id(&self) -> Option<&str> {
        self.0.get("id").and_then(Value::as_str)
    }

    /// The typed view, when this is a `function_call` item (see
    /// [`FunctionCall`] — `arguments` stays the raw JSON string).
    pub fn as_function_call(&self) -> Option<FunctionCall> {
        self.typed("function_call")
    }

    /// The typed view, when this is a `function_call_output` item.
    pub fn as_function_call_output(&self) -> Option<FunctionCallOutput> {
        self.typed("function_call_output")
    }

    /// The typed view, when this is a `message` item.
    pub fn as_message(&self) -> Option<MessageItem> {
        self.typed("message")
    }

    /// The typed view, when this is a `reasoning` item.
    pub fn as_reasoning(&self) -> Option<ReasoningItem> {
        self.typed("reasoning")
    }

    /// Parse the typed view for `kind`, or `None` when the item is
    /// another kind or its fields do not fit (the raw item survives
    /// either way).
    fn typed<T: DeserializeOwned>(&self, kind: &str) -> Option<T> {
        if self.kind() != Some(kind) {
            return None;
        }
        serde_json::from_value(self.0.clone()).ok()
    }
}

/// The typed `function_call` view (the wire keeps `arguments` a JSON
/// string — parsed as text, never as an object).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
    pub call_id: String,
}

/// The typed `function_call_output` view.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FunctionCallOutput {
    pub call_id: String,
    pub output: String,
}

/// The typed `message` view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageItem {
    pub role: String,
    pub content: Vec<ContentPart>,
}

/// One message content part: `{type, text}` with `type` one of
/// `input_text`/`output_text`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

impl ContentPart {
    pub fn input_text(text: &str) -> ContentPart {
        ContentPart {
            kind: "input_text".to_owned(),
            text: text.to_owned(),
        }
    }

    pub fn output_text(text: &str) -> ContentPart {
        ContentPart {
            kind: "output_text".to_owned(),
            text: text.to_owned(),
        }
    }
}

/// The typed `reasoning` view: the summary texts in order, plus the
/// opaque encrypted content when present.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ReasoningItem {
    pub id: Option<String>,
    pub summary: Vec<ReasoningSummary>,
    pub encrypted_content: Option<String>,
}

/// One reasoning summary part: `{type, text}`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ReasoningSummary {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

// ── tools ────────────────────────────────────────────────────────────

/// One tool: the wire object verbatim with a typed view for the
/// function shape (the codex client's flat Responses tool — `type` is
/// a sibling of `name`, not a nested `function` object like
/// openai-chat's).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool(pub Value);

impl Tool {
    /// A function tool: `{"type":"function","name","description",
    /// "strict","parameters"}` — the codex client sends `strict: false`
    /// (non-strict schemas accept looser JSON schemas than strict).
    pub fn function(name: &str, description: &str, strict: bool, parameters: Value) -> Tool {
        Tool(json!({
            "type": "function",
            "name": name,
            "description": description,
            "strict": strict,
            "parameters": parameters,
        }))
    }

    /// The typed view, when this is a function tool.
    pub fn as_function(&self) -> Option<FunctionTool> {
        (self.0.get("type").and_then(Value::as_str) == Some("function"))
            .then(|| serde_json::from_value(self.0.clone()).ok())
            .flatten()
    }
}

/// The typed function-tool view.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FunctionTool {
    pub name: String,
    pub description: String,
    pub strict: bool,
    pub parameters: Value,
}

// ── usage ────────────────────────────────────────────────────────────

/// The usage object of `response.completed` (invariant 3: the detail
/// fields are `Option` — an absent or `null` detail is `None`, never a
/// fabricated `0`).
///
/// Unknown fields ride in `extra` (the IR's raw-preservation rule, so a
/// wire detail toker does not model survives a round trip): serialising
/// the usage reproduces every member the response carried, which is what
/// the ledger's `usage_raw` column wants — the verbatim
/// `response.completed` usage JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub input_tokens_details: Option<InputTokensDetails>,
    pub output_tokens: u64,
    pub output_tokens_details: Option<OutputTokensDetails>,
    pub total_tokens: u64,
    /// Every usage member the wire carried beyond the modeled five —
    /// preserved and re-emitted after them.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The input-token details: `{cached_tokens, cache_write_tokens?}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputTokensDetails {
    pub cached_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

/// The output-token details: `{reasoning_tokens}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputTokensDetails {
    pub reasoning_tokens: Option<u64>,
}

// ── the SSE event types ──────────────────────────────────────────────

/// One parsed responses-dialect SSE event (see [super::sse]). The raw
/// payload survives on the unknown kinds — never dropped, because the
/// dialect grows faster than any client's enum.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseEvent {
    /// `response.created` — the response id and the model slug the
    /// response names, when the payload carries them.
    Created {
        response_id: Option<String>,
        /// The response object's `model` — the identity the upstream
        /// actually engaged (the routing unit's `model`/`raw_model`
        /// columns read it through [`TurnCapture`]).
        model: Option<String>,
    },
    /// `response.output_item.added` — an item mid-assembly (a function
    /// call's arguments may still be partial here).
    OutputItemAdded { item: Item, data: Value },
    /// `response.output_item.done` — the item complete: a function
    /// call's arguments arrive whole here, never via the deltas.
    OutputItemDone { item: Item, data: Value },
    /// `response.output_text.delta`.
    OutputTextDelta { delta: String },
    /// `response.reasoning_summary_text.delta` — one summary, indexed.
    ReasoningSummaryDelta { delta: String, summary_index: i64 },
    /// `response.completed` — the terminal event, carrying usage.
    Completed { response: CompletedResponse },
    /// `response.incomplete` — the other terminal event: why the turn
    /// stopped short (`incomplete_details.reason`), when said.
    Incomplete {
        reason: Option<String>,
        usage: Option<Usage>,
    },
    /// `response.failed` — `response.error`.
    Failed { error: ResponseError },
    /// A top-level `error` event — `error`.
    Error { error: ResponseError },
    /// Any other kind, verbatim.
    Unknown { kind: String, data: Value },
}

impl ResponseEvent {
    /// The event's kind string (the JSON `type`, or the `event:` line
    /// when the JSON carried none).
    pub fn kind(&self) -> &str {
        match self {
            ResponseEvent::Created { .. } => "response.created",
            ResponseEvent::OutputItemAdded { .. } => "response.output_item.added",
            ResponseEvent::OutputItemDone { .. } => "response.output_item.done",
            ResponseEvent::OutputTextDelta { .. } => "response.output_text.delta",
            ResponseEvent::ReasoningSummaryDelta { .. } => "response.reasoning_summary_text.delta",
            ResponseEvent::Completed { .. } => "response.completed",
            ResponseEvent::Incomplete { .. } => "response.incomplete",
            ResponseEvent::Failed { .. } => "response.failed",
            ResponseEvent::Error { .. } => "error",
            ResponseEvent::Unknown { kind, .. } => kind,
        }
    }

    /// Whether this event carries the turn's final state — the
    /// `[DONE]`-less stream's own terminators: `response.completed` /
    /// `response.incomplete`, and `response.failed` (a bare `error`
    /// event does not end a turn by itself; the codex client reads on
    /// after one too).
    pub fn ends_turn(&self) -> bool {
        matches!(
            self,
            ResponseEvent::Completed { .. }
                | ResponseEvent::Incomplete { .. }
                | ResponseEvent::Failed { .. }
        )
    }
}

/// The `response` object of `response.completed`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompletedResponse {
    pub id: Option<String>,
    pub usage: Option<Usage>,
    pub end_turn: Option<bool>,
}

/// An error payload: `response.failed`'s `response.error`, or a
/// top-level `error` event's `error`. Every field optional — the
/// upstream's shapes vary (see the codex client's `Error` struct).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResponseError {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    #[serde(default, deserialize_with = "string_or_number")]
    pub code: Option<String>,
    pub message: Option<String>,
    /// When the error is a rate limit: unix seconds.
    pub resets_at: Option<i64>,
}

// OpenRouter sends numeric HTTP error codes, whereas Responses events may use
// symbolic strings. A numeric code must not discard the accompanying message.
fn string_or_number<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value)),
        Some(serde_json::Value::Number(value)) => Ok(Some(value.to_string())),
        Some(_) => Err(serde::de::Error::custom(
            "error code must be a string or number",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CompletedResponse, ContentPart, FunctionCall, Item, Reasoning, ResponseError,
        ResponseEvent, ResponsesRequest, Tool, Usage,
    };
    use serde_json::{Value, json};

    #[test]
    fn the_request_body_pins_the_wire_field_order_and_constants() {
        let mut request = ResponsesRequest::new("gpt-5.2-codex", "cache-key-1");
        request.instructions = "Be terse.".to_owned();
        request.input = vec![Item::message(
            "user",
            vec![ContentPart::input_text("Hello")],
        )];
        request.tools = vec![Tool::function(
            "read_file",
            "Read one file",
            false,
            json!({"type": "object"}),
        )];
        request.reasoning = Reasoning {
            effort: Some("high".to_owned()),
            summary: Some("auto".to_owned()),
            extra: Default::default(),
        };

        let text = serde_json::to_string(&request).expect("serialise");
        let value: Value = serde_json::from_str(&text).expect("round-trip");
        // The routing fields lead, in the wire's order — pinned by
        // position, since the order is the point.
        let keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        let expected = [
            "model",
            "stream",
            "instructions",
            "input",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "reasoning",
            "store",
            "include",
            "prompt_cache_key",
        ];
        assert_eq!(&keys[..expected.len()], &expected, "the pinned order");
        assert_eq!(
            keys.len(),
            expected.len(),
            "no extra keys on a built request"
        );

        // The wire constants.
        assert_eq!(value["store"], json!(false), "store is always false");
        assert_eq!(
            value["include"],
            json!(["reasoning.encrypted_content"]),
            "include is always the encrypted-reasoning content"
        );
        assert_eq!(value["stream"], json!(true));
        assert_eq!(value["tool_choice"], json!("auto"));
        assert_eq!(value["parallel_tool_calls"], json!(false));
        assert_eq!(value["prompt_cache_key"], json!("cache-key-1"));
        // The wire item shapes.
        assert_eq!(
            value["input"][0],
            json!({"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "Hello"}]})
        );
        assert_eq!(
            value["tools"][0],
            json!({"type": "function", "name": "read_file", "description": "Read one file",
                   "strict": false, "parameters": {"type": "object"}}),
            "the responses tool is flat, not the chat protocol's nested shape"
        );
        assert_eq!(
            value["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );
    }

    #[test]
    fn the_request_body_preserves_unknown_fields_through_round_trips() {
        // A body with fields toker does not model (the codex client's
        // text/service_tier/stream_options), parsed and re-emitted.
        let body = r#"{
            "model": "gpt-5.2-codex",
            "stream": true,
            "instructions": "",
            "input": [],
            "tools": [],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "reasoning": {"effort": null, "summary": null},
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "k",
            "text": {"verbosity": "low"},
            "service_tier": "flex"
        }"#;
        let parsed: ResponsesRequest = serde_json::from_str(body).expect("parse");
        assert_eq!(parsed.extra.get("service_tier"), Some(&json!("flex")));
        let text = serde_json::to_string(&parsed).expect("serialise");
        let value: Value = serde_json::from_str(&text).expect("round trip");
        assert_eq!(value["text"], json!({"verbosity": "low"}));
        assert_eq!(value["service_tier"], json!("flex"));
        // And the pinned fields still lead the re-emitted body.
        let keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys[0], "model");
        assert_eq!(keys[1], "stream");
        assert!(
            keys.iter().position(|k| *k == "text").unwrap()
                > keys.iter().position(|k| *k == "prompt_cache_key").unwrap()
        );
    }

    #[test]
    fn items_construct_to_the_wire_shape_and_parse_back_typed() {
        // function_call: arguments stay a JSON string.
        let call = Item::function_call("read_file", r#"{"path":"src/lib.rs"}"#, "call_1");
        assert_eq!(
            serde_json::to_value(&call).expect("serialise"),
            json!({"type": "function_call", "name": "read_file",
                   "arguments": "{\"path\":\"src/lib.rs\"}", "call_id": "call_1"})
        );
        let typed = call.as_function_call().expect("typed view");
        assert_eq!(
            typed,
            FunctionCall {
                name: "read_file".to_owned(),
                arguments: r#"{"path":"src/lib.rs"}"#.to_owned(),
                call_id: "call_1".to_owned(),
            }
        );
        assert!(call.as_message().is_none(), "another kind does not view");

        let output = Item::function_call_output("call_1", "file contents");
        assert_eq!(
            serde_json::to_value(&output).expect("serialise"),
            json!({"type": "function_call_output", "call_id": "call_1",
                   "output": "file contents"})
        );
        assert_eq!(
            output.as_function_call_output().expect("view").output,
            "file contents"
        );

        // reasoning: summary parts and encrypted content.
        let reasoning = Item::reasoning(
            vec!["Thinking".to_owned(), "More".to_owned()],
            Some("opaque-encrypted"),
        );
        assert_eq!(
            serde_json::to_value(&reasoning).expect("serialise"),
            json!({"type": "reasoning",
                   "summary": [
                       {"type": "summary_text", "text": "Thinking"},
                       {"type": "summary_text", "text": "More"}
                   ],
                   "encrypted_content": "opaque-encrypted"})
        );
        let typed = reasoning.as_reasoning().expect("typed view");
        assert_eq!(typed.summary.len(), 2);
        assert_eq!(typed.summary[0].text, "Thinking");
        assert_eq!(typed.summary[1].kind, "summary_text");
        assert_eq!(typed.encrypted_content.as_deref(), Some("opaque-encrypted"));

        let bare = Item::reasoning(vec![], None);
        assert!(
            bare.as_reasoning()
                .expect("view")
                .encrypted_content
                .is_none()
        );

        // message: content parts both ways.
        let message = Item::message(
            "assistant",
            vec![
                ContentPart::input_text("in"),
                ContentPart::output_text("out"),
            ],
        );
        let typed = message.as_message().expect("typed view");
        assert_eq!(typed.role, "assistant");
        assert_eq!(typed.content[0].kind, "input_text");
        assert_eq!(typed.content[1].text, "out");
    }

    #[test]
    fn unknown_item_kinds_and_fields_round_trip_verbatim() {
        // A kind toker does not model, with nested data: byte-identical
        // through the Item.
        let web_search = json!({
            "type": "web_search_call",
            "id": "ws_1",
            "action": {"type": "search", "query": "rust sse"},
            "status": "completed",
        });
        let item = Item(web_search.clone());
        assert_eq!(item.kind(), Some("web_search_call"));
        assert_eq!(serde_json::to_value(&item).expect("serialise"), web_search);

        // Unknown fields on a known kind survive too (the typed view
        // reads what it knows; the Item keeps everything).
        let call = json!({
            "type": "function_call",
            "name": "shell",
            "arguments": "{}",
            "call_id": "call_9",
            "id": "fc_9",
            "namespace": "local",
        });
        let item = Item(call.clone());
        assert_eq!(item.as_function_call().expect("view").name, "shell");
        assert_eq!(serde_json::to_value(&item).expect("serialise"), call);
    }

    #[test]
    fn tools_parse_the_function_view_and_round_trip_other_shapes() {
        let tool = Tool::function("shell", "Run a command", false, json!({"type": "object"}));
        let typed = tool.as_function().expect("typed view");
        assert_eq!(typed.name, "shell");
        assert!(!typed.strict);
        assert_eq!(typed.parameters, json!({"type": "object"}));

        // The freeform/custom tool shape: preserved, not viewed.
        let custom = json!({"type": "custom", "name": "apply_patch", "format": "…"});
        let tool = Tool(custom.clone());
        assert!(tool.as_function().is_none());
        assert_eq!(serde_json::to_value(&tool).expect("serialise"), custom);
    }

    #[test]
    fn usage_parses_full_details_null_details_and_absent_details() {
        let usage: Usage = serde_json::from_value(json!({
            "input_tokens": 1234,
            "input_tokens_details": {"cached_tokens": 512, "cache_write_tokens": 64},
            "output_tokens": 210,
            "output_tokens_details": {"reasoning_tokens": 96},
            "total_tokens": 1444,
        }))
        .expect("full usage parses");
        assert_eq!(usage.input_tokens, 1234);
        assert_eq!(usage.output_tokens, 210);
        assert_eq!(usage.total_tokens, 1444);
        assert_eq!(
            usage.input_tokens_details.expect("details"),
            super::InputTokensDetails {
                cached_tokens: Some(512),
                cache_write_tokens: Some(64),
            }
        );
        assert_eq!(
            usage
                .output_tokens_details
                .expect("details")
                .reasoning_tokens,
            Some(96)
        );

        // Explicit null details — not zero, never fabricated (invariant 3).
        let usage: Usage = serde_json::from_value(json!({
            "input_tokens": 10,
            "input_tokens_details": null,
            "output_tokens": 5,
            "output_tokens_details": null,
            "total_tokens": 15,
        }))
        .expect("null details parse");
        assert_eq!(usage.input_tokens_details, None);
        assert_eq!(usage.output_tokens_details, None);

        // Absent details and the optional cache_write.
        let usage: Usage = serde_json::from_value(json!({
            "input_tokens": 10,
            "input_tokens_details": {"cached_tokens": 4},
            "output_tokens": 5,
            "total_tokens": 15,
        }))
        .expect("absent details parse");
        assert!(usage.output_tokens_details.is_none());
        let details = usage.input_tokens_details.expect("details");
        assert_eq!(details.cached_tokens, Some(4));
        assert_eq!(details.cache_write_tokens, None);
    }

    #[test]
    fn response_errors_parse_every_shape_the_upstream_sends() {
        let error: ResponseError = serde_json::from_value(json!({
            "type": "invalid_request_error",
            "code": "rate_limit_exceeded",
            "message": "Rate limit reached.",
            "resets_at": 1_799_999_999,
        }))
        .expect("full error parses");
        assert_eq!(error.kind.as_deref(), Some("invalid_request_error"));
        assert_eq!(error.code.as_deref(), Some("rate_limit_exceeded"));
        assert_eq!(error.message.as_deref(), Some("Rate limit reached."));
        assert_eq!(error.resets_at, Some(1_799_999_999));

        let bare: ResponseError = serde_json::from_value(json!({})).expect("bare parses");
        assert_eq!(
            bare,
            ResponseError {
                kind: None,
                code: None,
                message: None,
                resets_at: None
            }
        );
    }

    #[test]
    fn event_kinds_name_themselves_and_mark_the_terminators() {
        let created = ResponseEvent::Created {
            response_id: Some("resp_1".to_owned()),
            model: None,
        };
        assert_eq!(created.kind(), "response.created");
        assert!(!created.ends_turn());

        let completed = ResponseEvent::Completed {
            response: CompletedResponse::default(),
        };
        assert_eq!(completed.kind(), "response.completed");
        assert!(completed.ends_turn());
        assert!(
            ResponseEvent::Incomplete {
                reason: None,
                usage: None
            }
            .ends_turn()
        );
        assert!(
            ResponseEvent::Failed {
                error: ResponseError::default()
            }
            .ends_turn()
        );
        assert!(
            !ResponseEvent::Error {
                error: ResponseError::default()
            }
            .ends_turn(),
            "a bare error event is not the turn's end"
        );
        let unknown = ResponseEvent::Unknown {
            kind: "response.new_thing".to_owned(),
            data: json!({}),
        };
        assert_eq!(unknown.kind(), "response.new_thing");
        assert!(!unknown.ends_turn());
    }
}
