//! Typed views over OpenAI Chat request bodies.
//!
//! Shared by the openai_chat frontend and the openai/openrouter/lunaroute
//! backends. The IR is a [`serde_json::Value`] (see the module docs), so a
//! view is a read-only lens ([`ChatBody`]) plus the typed mutations
//! middleware needs ([`ChatBodyMut`]: [`ChatBodyMut::set_model`],
//! [`ChatBodyMut::push_message`]).
//!
//! Views never remove or reorder fields they do not understand: every
//! accessor reads one path, and every mutation touches one key position —
//! with `preserve_order`, replacing an existing key keeps its position and
//! inserting a new one appends at the end, so the rest of the body never
//! moves. That is what makes a middleware edit the deliberate,
//! once-per-change byte event invariant 4 requires.

use serde_json::Value;

use super::{Request, short_hash};

/// Read-only OpenAI Chat view over a [`Request`].
#[derive(Debug, Clone, Copy)]
pub struct ChatBody<'a> {
    request: &'a Request,
}

/// Mutable OpenAI Chat view over a [`Request`]: the typed mutations.
/// Reads stay on [`ChatBody`] (get one via [`Request::openai_chat`]).
#[derive(Debug)]
pub struct ChatBodyMut<'a> {
    request: &'a mut Request,
}

impl Request {
    /// Read-only OpenAI Chat view: model, messages, tools, stream, shape.
    pub fn openai_chat(&self) -> ChatBody<'_> {
        ChatBody { request: self }
    }

    /// Mutable OpenAI Chat view: the typed mutations ([`set_model`],
    /// [`push_message`]).
    pub fn openai_chat_mut(&mut self) -> ChatBodyMut<'_> {
        ChatBodyMut { request: self }
    }
}

impl<'a> ChatBody<'a> {
    /// The top-level `model`, when it is a string.
    pub fn model(&self) -> Option<&'a str> {
        self.request.value.get("model").and_then(Value::as_str)
    }

    /// The `messages` array as a typed view. Missing or non-array reads as
    /// empty; [`Shape::req_messages`] distinguishes the two for the ledger.
    pub fn messages(&self) -> Messages<'a> {
        Messages::over(&self.request.value)
    }

    /// The `tools` array as a typed view. Missing or non-array reads as
    /// empty.
    pub fn tools(&self) -> Tools<'a> {
        Tools::over(&self.request.value)
    }

    /// `stream` is true iff the body says so; absent, false, or a non-bool
    /// all read as false (the request is non-streaming in every one of
    /// those cases).
    pub fn stream(&self) -> bool {
        self.request.value.get("stream") == Some(&Value::Bool(true))
    }

    /// The content-free request shape for the ledger and lanes
    /// (row parity): counts, lengths, and digests only, never content
    /// (invariant 1).
    pub fn shape(&self) -> Shape {
        let value = &self.request.value;
        let messages = Messages::over(value);
        let tools = Tools::over(value);

        // System text: every system-family message — role `system` or
        // `developer`, the o-series replacement — in order, each textual
        // piece one block (per-block system digests, mapped from
        // Anthropic's top-level `system` array to Chat's in-band system
        // messages).
        let mut system = String::new();
        let mut system_blocks = Vec::new();
        for message in messages.iter() {
            if !message.is_system() {
                continue;
            }
            for piece in message.text_parts() {
                system_blocks.push(BlockDigest {
                    chars: piece.chars().count() as u64,
                    hash: short_hash(piece.as_bytes()),
                });
                system.push_str(piece);
            }
        }

        let tool_names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();

        Shape {
            req_bytes: self.request.req_bytes,
            // Row parity: null when `messages` is missing or not an array,
            // not zero (absence ≠ zero, invariant 3).
            req_messages: value
                .get("messages")
                .and_then(Value::as_array)
                .map(|messages| messages.len() as u64),
            req_tools: tool_names.len() as u64,
            // Hashed even when empty, as the anthropic view and the
            // predecessor do: a tool-less request is a lane of its own,
            // never laneless.
            tools_hash: short_hash(tool_names.join("\0").as_bytes()),
            system_chars: system.chars().count() as u64,
            // Always present, even for an empty system (row parity: the
            // digest of "" is a valid, comparable identity).
            system_hash: short_hash(system.as_bytes()),
            system_blocks,
        }
    }
}

impl ChatBodyMut<'_> {
    /// Set the top-level `model`. With `preserve_order` this is the
    /// deliberate once-per-change byte edit invariant 4 requires: an
    /// existing key keeps its position (only its value's bytes change), an
    /// absent key is inserted at the end, and nothing else moves. A body
    /// whose top level is not an object is left untouched (the protocol
    /// adapter rejects it long before middleware runs).
    pub fn set_model(&mut self, model: &str) {
        if let Some(map) = self.request.value.as_object_mut() {
            map.insert("model".to_owned(), Value::String(model.to_owned()));
        }
    }

    /// Append one message (`role` + string content) to the end of
    /// `messages`, creating an empty array if the key is absent. A
    /// `messages` that exists but is not an array is left untouched —
    /// never guessed into shape. Gates will use this to append synthetic
    /// assistant turns.
    pub fn push_message(&mut self, role: &str, content: &str) {
        let Some(map) = self.request.value.as_object_mut() else {
            return;
        };
        if !map.contains_key("messages") {
            map.insert("messages".to_owned(), Value::Array(Vec::new()));
        }
        let Some(parts) = map.get_mut("messages").and_then(Value::as_array_mut) else {
            return;
        };
        parts.push(serde_json::json!({"role": role, "content": content}));
    }
}

/// The `messages` array as a typed slice view.
#[derive(Debug, Clone, Copy)]
pub struct Messages<'a> {
    parts: &'a [Value],
}

impl<'a> Messages<'a> {
    fn over(value: &'a Value) -> Self {
        Messages {
            parts: match value.get("messages").and_then(Value::as_array) {
                Some(parts) => parts.as_slice(),
                None => &[],
            },
        }
    }

    /// Message count (0 when the key is missing or not an array).
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// The message at `index`, if present.
    pub fn get(&self, index: usize) -> Option<Message<'a>> {
        self.parts.get(index).map(|value| Message { value })
    }

    /// Every message, in order.
    pub fn iter(&self) -> impl Iterator<Item = Message<'a>> {
        self.parts.iter().map(|value| Message { value })
    }
}

/// One message object in the `messages` array.
#[derive(Debug, Clone, Copy)]
pub struct Message<'a> {
    value: &'a Value,
}

impl<'a> Message<'a> {
    /// The `role`, when it is a string.
    pub fn role(&self) -> Option<&'a str> {
        self.value.get("role").and_then(Value::as_str)
    }

    /// System-family role: `system`, or `developer` — the o-series
    /// replacement; system extraction handles both (see [`ChatBody::shape`]).
    pub fn is_system(&self) -> bool {
        matches!(self.role(), Some("system" | "developer"))
    }

    /// The message content, typed.
    pub fn content(&self) -> Content<'a> {
        match self.value.get("content") {
            Some(Value::String(text)) => Content::Text(text),
            Some(Value::Array(parts)) => Content::Parts(parts.as_slice()),
            other => Content::Other(other),
        }
    }

    /// The textual pieces of the content, in order: string content is one
    /// piece; an array contributes the `text` field of every part that has
    /// one. Absent, `null`, and non-textual shapes contribute nothing.
    pub fn text_parts(&self) -> Vec<&'a str> {
        self.content().text_parts()
    }
}

/// A message's content, typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content<'a> {
    /// Plain string content.
    Text(&'a str),
    /// Array-of-parts content (multimodal or structured).
    Parts(&'a [Value]),
    /// Absent, `null`, or a shape the view does not model; the raw value,
    /// when there is one.
    Other(Option<&'a Value>),
}

impl<'a> Content<'a> {
    /// The textual pieces, in order (see [`Message::text_parts`]).
    pub fn text_parts(&self) -> Vec<&'a str> {
        match *self {
            Content::Text(text) => vec![text],
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect(),
            Content::Other(_) => Vec::new(),
        }
    }
}

/// The `tools` array as a typed slice view.
#[derive(Debug, Clone, Copy)]
pub struct Tools<'a> {
    parts: &'a [Value],
}

impl<'a> Tools<'a> {
    fn over(value: &'a Value) -> Self {
        Tools {
            parts: match value.get("tools").and_then(Value::as_array) {
                Some(parts) => parts.as_slice(),
                None => &[],
            },
        }
    }

    /// Tool count (0 when the key is missing or not an array).
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Every tool, in order.
    pub fn iter(&self) -> impl Iterator<Item = Tool<'a>> {
        self.parts.iter().map(|value| Tool { value })
    }
}

/// One tool definition in the `tools` array.
#[derive(Debug, Clone, Copy)]
pub struct Tool<'a> {
    value: &'a Value,
}

impl<'a> Tool<'a> {
    /// The tool's name: OpenAI nests it at `function.name`; the fallback
    /// chain (`name`, then `type`, then a constant `?`) covers non-standard
    /// bodies. A tool never reads as absent — the `?` keeps the count and
    /// the join aligned the way the ledger's always been.
    pub fn name(&self) -> &'a str {
        self.value
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .or_else(|| self.value.get("name").and_then(Value::as_str))
            .or_else(|| self.value.get("type").and_then(Value::as_str))
            .unwrap_or("?")
    }
}

/// The content-free request shape for the ledger and lanes (row parity):
/// counts, lengths, and digests only — never message, system, or tool text
/// (invariant 1). The server unit copies these into the `requests` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    /// The original wire buffer's length, passed through from parse.
    pub req_bytes: u64,
    /// Message count; `None` when `messages` is missing or not an array
    /// (absence ≠ zero, invariant 3).
    pub req_messages: Option<u64>,
    /// Tool count (0 when `tools` is absent — row parity).
    pub req_tools: u64,
    /// Digest of the tool-name list joined with `\0`, in order (order
    /// matters as much as membership: tools render first, so any reordering
    /// invalidates the entire prefix). An empty list hashes the empty
    /// join, as the anthropic view does.
    pub tools_hash: String,
    /// Total system text length in characters (Unicode scalar values).
    pub system_chars: u64,
    /// Digest of the concatenated system/developer text; always present,
    /// even when empty (row parity).
    pub system_hash: String,
    /// Per-block digests/lengths: one per textual piece of every
    /// system-family message, in order (mapped from
    /// Anthropic's top-level array to Chat's in-band system messages).
    pub system_blocks: Vec<BlockDigest>,
}

/// One system block's digest and length (no content — invariant 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDigest {
    /// The block's length in characters (Unicode scalar values).
    pub chars: u64,
    /// The block's sha256/12 digest.
    pub hash: String,
}

#[cfg(test)]
mod tests {
    use super::super::short_hash;
    use super::{BlockDigest, Content};
    use crate::ir::Request;

    fn parse(body: &[u8]) -> Request {
        Request::parse(body).expect("test body parses")
    }

    #[test]
    fn reads_model_stream_and_content_shapes() {
        let body = br#"{"model":"m1","messages":[{"role":"system","content":"You are."},{"role":"user","content":"Hi."},{"role":"assistant","content":[{"type":"text","text":"Hello"},{"type":"image_url","image_url":{"url":"u"}}]},{"role":"user","content":null},{"role":"user"}],"stream":true}"#;
        let request = parse(body);
        let chat = request.openai_chat();

        assert_eq!(chat.model(), Some("m1"));
        assert!(chat.stream());

        let messages = chat.messages();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages.get(0).unwrap().role(), Some("system"));
        assert_eq!(
            messages.get(0).unwrap().content(),
            Content::Text("You are.")
        );
        assert_eq!(messages.get(1).unwrap().text_parts(), vec!["Hi."]);
        assert_eq!(
            messages.get(2).unwrap().text_parts(),
            vec!["Hello"],
            "array content contributes its text parts only"
        );
        assert_eq!(
            messages.get(3).unwrap().content(),
            Content::Other(Some(&serde_json::Value::Null))
        );
        assert_eq!(
            messages.get(4).unwrap().content(),
            Content::Other(None),
            "absent content reads as Other(None)"
        );
        assert!(messages.get(5).is_none());
        assert!(chat.messages().get(2).unwrap().text_parts().len() == 1);
    }

    #[test]
    fn missing_keys_read_as_absent_never_as_defaults_that_lie() {
        let body = br#"{"messages":[{"role":"user","content":"Hi"}]}"#;
        let request = parse(body);
        let chat = request.openai_chat();
        assert_eq!(chat.model(), None);
        assert!(!chat.stream());
        assert_eq!(chat.tools().len(), 0);
        let shape = chat.shape();
        assert_eq!(shape.req_tools, 0);
        assert_eq!(
            shape.tools_hash,
            short_hash(b""),
            "no tools is the empty list's lane, not no lane"
        );
        assert_eq!(shape.req_messages, Some(1));

        // `stream: false` and a non-bool `stream` both read as false.
        assert!(!parse(br#"{"stream":false}"#).openai_chat().stream());
        assert!(!parse(br#"{"stream":"yes"}"#).openai_chat().stream());
        // A non-string model is absent, not coerced.
        assert_eq!(parse(br#"{"model":5}"#).openai_chat().model(), None);
    }

    #[test]
    fn tool_names_follow_the_openai_path_with_predecessor_fallbacks() {
        let body = br#"{"tools":[{"type":"function","function":{"name":"a"}},{"name":"b"},{"type":"custom"},{"weird":true}]}"#;
        let request = parse(body);
        let tools = request.openai_chat().tools();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, vec!["a", "b", "custom", "?"]);
    }

    #[test]
    fn shape_digests_system_and_developer_text_per_block() {
        let body = br#"{"model":"m","messages":[{"role":"system","content":"You are."},{"role":"developer","content":"JSON only."},{"role":"user","content":"ignored"},{"role":"user","content":[{"type":"text","text":"also ignored"}]}],"tools":[{"type":"function","function":{"name":"a"}},{"weird":true},{"type":"custom"}],"stream":true}"#;
        let request = parse(body);
        let shape = request.openai_chat().shape();

        let system = "You are.JSON only.";
        assert_eq!(shape.req_bytes, body.len() as u64);
        assert_eq!(shape.req_messages, Some(4));
        assert_eq!(shape.req_tools, 3);
        assert_eq!(shape.tools_hash, short_hash(b"a\0?\0custom"));
        assert_eq!(shape.system_chars, system.chars().count() as u64);
        assert_eq!(shape.system_hash, short_hash(system.as_bytes()));
        assert_eq!(
            shape.system_blocks,
            vec![
                BlockDigest {
                    chars: 8,
                    hash: short_hash(b"You are."),
                },
                BlockDigest {
                    chars: 10,
                    hash: short_hash(b"JSON only."),
                },
            ],
            "developer-role text is system text, and non-system text never hashes"
        );
    }

    #[test]
    fn shape_of_no_system_is_the_digest_of_nothing() {
        let body = br#"{"messages":5}"#;
        let shape = parse(body).openai_chat().shape();
        assert_eq!(shape.req_messages, None, "non-array messages is absence");
        assert_eq!(shape.req_bytes, body.len() as u64);
        assert_eq!(shape.system_chars, 0);
        assert_eq!(shape.system_hash, short_hash(b""));
        assert!(shape.system_blocks.is_empty());
    }

    #[test]
    fn set_model_keeps_position_when_present_appends_when_absent() {
        let body = br#"{"messages":[{"role":"user","content":"Hi"}],"model":"old","stream":true}"#;
        let mut request = parse(body);
        request.openai_chat_mut().set_model("new");
        assert_eq!(request.openai_chat().model(), Some("new"));
        // The key stayed in place: bytes before `new` still contain
        // `messages`, so `model` did not move to the front or the end.
        let serialised = request.serialise();
        let at = serialised
            .windows(b"\"new\"".len())
            .position(|window| window == b"\"new\"")
            .expect("new model value present");
        assert!(
            serialised[..at]
                .starts_with(br#"{"messages":[{"role":"user","content":"Hi"}],"model":"#)
        );

        // Absent: inserted at the end.
        let body = br#"{"stream":false,"messages":[]}"#;
        let mut request = parse(body);
        request.openai_chat_mut().set_model("m");
        assert_eq!(
            request.serialise(),
            br#"{"stream":false,"messages":[],"model":"m"}"#
        );

        // Non-object top level: untouched, never guessed into shape.
        let mut request = parse(b"[]");
        request.openai_chat_mut().set_model("m");
        assert_eq!(request.serialise(), b"[]");
    }

    #[test]
    fn push_message_appends_creates_and_never_guesses() {
        let body = br#"{"messages":[{"role":"user","content":"a"}]}"#;
        let mut request = parse(body);
        request.openai_chat_mut().push_message("assistant", "b");
        assert_eq!(request.openai_chat().messages().len(), 2);
        assert_eq!(
            request.openai_chat().messages().get(1).unwrap().role(),
            Some("assistant")
        );
        assert_eq!(
            request.serialise(),
            br#"{"messages":[{"role":"user","content":"a"},{"role":"assistant","content":"b"}]}"#
        );

        // Absent key: created.
        let mut request = parse(b"{}");
        request.openai_chat_mut().push_message("user", "hi");
        assert_eq!(
            request.serialise(),
            br#"{"messages":[{"role":"user","content":"hi"}]}"#
        );

        // Non-array messages: untouched.
        let mut request = parse(br#"{"messages":"nope"}"#);
        request.openai_chat_mut().push_message("user", "hi");
        assert_eq!(request.serialise(), br#"{"messages":"nope"}"#);

        // Non-object top level: untouched.
        let mut request = parse(b"[]");
        request.openai_chat_mut().push_message("user", "hi");
        assert_eq!(request.serialise(), b"[]");
    }
}
