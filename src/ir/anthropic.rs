//! Typed views over Anthropic Messages request bodies.
//!
//! Shared by the Anthropic Messages frontend and the anthropic backends.
//! The IR is a [`serde_json::Value`] (see the parent module's docs), so a
//! view is a read-only lens ([`AnthropicBody`]) plus the typed mutations
//! middleware needs ([`AnthropicBodyMut`]: [`AnthropicBodyMut::set_model`],
//! [`AnthropicBodyMut::push_message`], [`AnthropicBodyMut::strip_release`]).
//! Like [`super::openai_chat`], views never remove or reorder fields they
//! do not understand: every accessor reads one path, and every mutation
//! touches one key position.
//!
//! The shape extraction ([`AnthropicBody::shape`]) and the release-marker
//! semantics ([`SENTINEL`], [`AnthropicBody::release_marker`],
//! [`AnthropicBodyMut::strip_release`]) are ports of the measured
//! production behaviours of the predecessor proxy, ctp — the Node proxy this
//! toolsuite replaces. They are ported, not improved: the behaviours were
//! measured over weeks of live traffic, and the marker rule is a frozen
//! public API (invariant 4). The ported pieces:
//!
//! - the request shape (counts, lengths, digests) and the prefix ladders,
//!   compaction markers, and detection helpers behind it;
//! - the release-marker carriage test and strip;
//! - the compaction-vs-summariser test;
//! - the field semantics the reference parity fixture pins.
//!
//! The `/v1/messages/count_tokens` and `/v1/messages/batches` bodies nest
//! their payloads differently (batches under `requests[].params`), so the
//! views never assume a top-level `messages` exists: a missing or
//! non-array `messages` reads as absent ([`AnthropicBody::messages`] is
//! empty, [`AnthropicShape::req_messages`] is `None`), never as zero
//! (invariant 3).

use std::borrow::Cow;

use serde_json::Value;

use super::openai_chat::Content;
use super::{Request, short_hash};

/// The quota-gate release marker. **A frozen public API**:
/// users type it into a conversation to override a quota block, and the
/// rule that removes it must stay byte-stable forever — the literal lands
/// in user messages, which the client replays on every later turn, so a
/// changed stripping rule is a changed cached prefix, and a changed
/// prefix is a full rebuild on every live conversation.
pub const SENTINEL: &str = "$#$BURN$#$";

/// The plan-only release marker: past the quota gate, but only while the
/// plan still has room, never into overage. Frozen under the same rule
/// as [`SENTINEL`], and stripped by the same function.
pub const PLAN_SENTINEL: &str = "$#$OVER$#$";

/// Which release a fresh marker asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// [`SENTINEL`]: continue on overage until the window resets.
    Overage,
    /// [`PLAN_SENTINEL`]: use what is left of the plan quota, and stop
    /// again before overage.
    Plan,
}

impl Release {
    /// The marker text this release is typed as.
    pub fn marker(self) -> &'static str {
        match self {
            Release::Overage => SENTINEL,
            Release::Plan => PLAN_SENTINEL,
        }
    }
}

// Fixed strings Claude Code itself emits around a
// compaction. Only whether they matched is recorded, never the surrounding
// text (invariant 1). The instruction is matched by the opening all three
// compaction wordings share, and the tool-refusal preamble every one of
// them carries, independently: a rewording of one does not take the
// detection with it. Either is enough (measured against the predecessor's
// documented detection rules).
pub(crate) const COMPACT_PERFORMING: &[&str] = &[
    "Your task is to create a detailed summary of",
    "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.",
];
const COMPACT_RESUMED: &str = "This session is being continued from a previous conversation";
// The opening sentence of Claude Code's recap prompt (its "away summary"),
// matched at a line start in the last turn like the compaction wordings.
// Only whether it matched is recorded (invariant 1).
const RECAP_PROMPT: &str = "The user stepped away and is coming back.";

// Ladder geometry. `pub(crate)`: the TUI's
// rebuild localisation re-derives rung offsets from the same geometry the
// stored rungs were cut to (the walk stores digests, never offsets).
pub(crate) const LADDER_STEP: usize = 8192;
const TAIL_FINE_STEP: usize = 8;
const TAIL_FINE_SPAN: usize = 256;
const TAIL_STEP: usize = 64;
const TAIL_SPAN: usize = 1024;

/// Read-only Anthropic Messages view over a [`Request`].
#[derive(Debug, Clone, Copy)]
pub struct AnthropicBody<'a> {
    request: &'a Request,
}

/// Mutable Anthropic Messages view over a [`Request`]: the typed mutations.
/// Reads stay on [`AnthropicBody`] (get one via [`Request::anthropic`]).
#[derive(Debug)]
pub struct AnthropicBodyMut<'a> {
    request: &'a mut Request,
}

impl Request {
    /// Read-only Anthropic Messages view: model, system, messages, tools,
    /// stream, shape, release marker.
    pub fn anthropic(&self) -> AnthropicBody<'_> {
        AnthropicBody { request: self }
    }

    /// Mutable Anthropic Messages view: the typed mutations
    /// ([`AnthropicBodyMut::set_model`], [`AnthropicBodyMut::push_message`],
    /// [`AnthropicBodyMut::strip_release`]).
    pub fn anthropic_mut(&mut self) -> AnthropicBodyMut<'_> {
        AnthropicBodyMut { request: self }
    }
}

impl<'a> AnthropicBody<'a> {
    /// The top-level `model`, when it is a string.
    pub fn model(&self) -> Option<&'a str> {
        self.request.value.get("model").and_then(Value::as_str)
    }

    /// `stream` is true iff the body says so; absent, false, or a non-bool
    /// all read as false (the request is non-streaming in every one of
    /// those cases).
    pub fn stream(&self) -> bool {
        self.request.value.get("stream") == Some(&Value::Bool(true))
    }

    /// `stream` is **explicitly** `false`
    /// (`parsed?.stream === false`). Not the
    /// negation of [`AnthropicBody::stream`]: a client that omitted the
    /// field cannot be assumed to parse a plain JSON body, so the gates
    /// answer everyone else with the SSE turn.
    pub fn stream_explicitly_false(&self) -> bool {
        self.request.value.get("stream") == Some(&Value::Bool(false))
    }

    /// The `system` field, which the Messages API accepts as a plain string
    /// or as a content-block array. Anything else — absent, `null`, a
    /// number — reads as [`System::Other`], never guessed into shape.
    pub fn system(&self) -> System<'a> {
        match self.request.value.get("system") {
            Some(Value::String(text)) => System::Text(text),
            Some(Value::Array(blocks)) => System::Blocks(blocks.as_slice()),
            other => System::Other(other),
        }
    }

    /// The `messages` array as a typed view. Missing or non-array reads
    /// as empty; [`AnthropicShape::req_messages`] distinguishes the two
    /// for the ledger. Batches bodies (payloads under
    /// `requests[].params`) legitimately have no top-level `messages`.
    pub fn messages(&self) -> Messages<'a> {
        Messages::over(&self.request.value)
    }

    /// The `tools` array as a typed view. Missing or non-array reads as
    /// empty.
    pub fn tools(&self) -> Tools<'a> {
        Tools::over(&self.request.value)
    }

    /// The content-free request shape for the ledger and lanes:
    /// counts, lengths, and digests
    /// only, never prompt, message, or tool text (invariant 1).
    ///
    /// The predecessor gates this on `POST` + `/v1/messages*`; that is the
    /// server's
    /// routing decision, not the body's — the extraction itself is pure
    /// and callable on any body, including `count_tokens` and `batches`
    /// bodies, whose top-level `messages` is absent by design.
    ///
    /// **Unit parity:** lengths count UTF-16 code units, because a JS
    /// proxy measures JS strings and the ledger must stay comparable with
    /// the predecessor's imported rows (`toker import`, phase 2). An
    /// astral character
    /// (emoji, 𝄞) counts as two units, exactly as in a JS string.
    pub fn shape(&self) -> AnthropicShape {
        let value = &self.request.value;
        let messages = self.messages();
        let tools = self.tools();

        // The system text pieces, in order.
        // A string system is one piece; a block array contributes each
        // string element and each block's `text` ("" when absent or not a
        // string — the API requires `text` to be a string, so a JS
        // stringification of non-string values is a malformed-body
        // artefact toker does not reproduce).
        let pieces = system_pieces(value);

        let mut system_blocks = Vec::with_capacity(pieces.len());
        let mut system_units: Vec<u16> = Vec::new();
        let mut system_bytes: Vec<u8> = Vec::new();
        for piece in &pieces {
            system_blocks.push(SystemBlockDigest {
                chars: piece.encode_utf16().count() as u64,
                hash: short_hash(piece.as_bytes()),
            });
            system_bytes.extend_from_slice(piece.as_bytes());
            system_units.extend(piece.encode_utf16());
        }

        let tool_names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        let (compact_generations, summarising, compact_marker) = compaction_of(messages.parts);
        let recap = last_turn_begins_line(messages.parts, &[RECAP_PROMPT]);
        let system_messages = {
            let count = messages
                .iter()
                .filter(|message| message.role() == Some("system"))
                .count() as u64;
            (count > 0).then_some(count)
        };

        AnthropicShape {
            req_bytes: self.request.req_bytes,
            // Row parity: null when `messages` is missing or not an array,
            // not zero (absence ≠ zero, invariant 3).
            req_messages: value
                .get("messages")
                .and_then(Value::as_array)
                .map(|messages| messages.len() as u64),
            req_tools: tool_names.len() as u64,
            // Hashed even when empty, as the predecessor did: a tool-less
            // request (the title summariser, a one-shot) is a lane of its
            // own, `session|e3b0c44298fc`, and the predecessor's imported
            // rows already carry that key — recording `None` instead left
            // those requests laneless and their imported lanes orphaned.
            tools_hash: short_hash(tool_names.join("\0").as_bytes()),
            system_chars: system_units.len() as u64,
            // Always present, even for an empty system (row parity: the
            // digest of "" is a valid, comparable identity).
            system_hash: short_hash(&system_bytes),
            system_blocks,
            system_messages,
            compact_generations,
            summarising,
            compact_marker,
            recap,
            system_ladder: prefix_ladder(&system_units),
            system_tail: suffix_ladder(&system_units),
        }
    }

    /// Which fresh release marker this request carries, if any.
    ///
    /// Anchored to the request, not to the string: the marker must OPEN
    /// the last user message's first text block. That fires on exactly
    /// the turn the human typed it and not on the tool-loop follow-ups
    /// after it, whose last user message holds `tool_result` blocks. A
    /// marker is only ever last once, which is why no occurrence counting
    /// is needed.
    pub fn release_marker(&self) -> Option<Release> {
        let text = self
            .messages()
            .iter()
            .rev()
            .find(|message| message.role() == Some("user"))
            .and_then(|message| message.first_text())?;
        [Release::Overage, Release::Plan]
            .into_iter()
            .find(|release| text.starts_with(release.marker()))
    }
}

impl AnthropicBodyMut<'_> {
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
    /// never guessed into shape. Gates use this to append synthetic
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

    /// Remove the release markers from position 0 of user text blocks, so
    /// the model never sees them: [`SENTINEL`] first, then
    /// [`PLAN_SENTINEL`], each under the whole rule below on its own.
    /// The rule is a frozen public API: it stays byte-stable forever.
    ///
    /// **Byte equivalence with a raw splice.** The predecessor
    /// byte-spliced the raw buffer because Node's `JSON.stringify` may reorder keys and
    /// renormalise escapes. toker's IR round-trip is byte-exact for
    /// canonical input (covered by the wire-preserving corpus), and the marker
    /// contains no characters JSON escapes — so removing it from the
    /// parsed text blocks and re-serialising produces the identical bytes
    /// to a raw splice, with key order and every other byte preserved.
    /// The tests pin this equivalence against a hand-done splice.
    ///
    /// **On any ambiguity, the body is left untouched** — the cost is that
    /// the model sees the marker once; the alternative is corrupting a
    /// request. The three guards, in order:
    ///
    /// - a block the marker would empty (the API rejects empty AND
    ///   whitespace-only text blocks, and the message persists in
    ///   history, so emptying it would invalidate every later request in
    ///   the conversation);
    /// - a raw scan of the serialised bytes that disagrees with the
    ///   parsed decision (a marker elsewhere in the bytes — an assistant
    ///   echo, an escaped quote — means we cannot say which bytes to
    ///   remove);
    /// - a `messages` that is absent or not an array (nothing to iterate;
    ///   the guard against the non-iterable that once took the whole
    ///   listener down).
    ///
    /// A marker not at position 0 is never stripped, in either
    /// implementation: anything else would rewrite pasted code —
    /// `if (!burn) x()` would have become `if () x()`.
    pub fn strip_release(&mut self) {
        for release in [Release::Overage, Release::Plan] {
            self.strip_marker(release.marker());
        }
    }

    /// Remove mid-conversation effort changes: the `output_config` a
    /// `role: "system"` message in `messages[]` carries to move the
    /// reasoning effort partway through a session. A message left with no
    /// content (the effort-only form) goes entirely; one that also carries
    /// text keeps the text. Returns whether anything changed. System
    /// messages sit outside the user/assistant alternation, so dropping
    /// one cannot break it.
    ///
    /// Backends whose models reject the field (see
    /// `Provider::accepts_message_effort`) would 400 the whole turn, and
    /// the client replays the message on every later turn.
    pub fn strip_message_effort(&mut self) -> bool {
        let Some(messages) = self
            .request
            .value
            .get_mut("messages")
            .and_then(Value::as_array_mut)
        else {
            return false;
        };
        let before = messages.len();
        let mut changed = false;
        messages.retain_mut(|message| {
            if message.get("role").and_then(Value::as_str) != Some("system") {
                return true;
            }
            let Some(map) = message.as_object_mut() else {
                return true;
            };
            if map.shift_remove("output_config").is_none() {
                return true;
            }
            changed = true;
            match map.get("content") {
                None | Some(Value::Null) => false,
                Some(Value::String(text)) => !text.trim().is_empty(),
                Some(Value::Array(blocks)) => !blocks.is_empty(),
                Some(_) => true,
            }
        });
        changed || messages.len() != before
    }

    /// Turn `"thinking": {"type": "disabled"}` into
    /// `{"type": "between_tools"}`, the form a model that cannot switch
    /// thinking off names in its 400. Any other `thinking` (absent,
    /// enabled, adaptive) is left alone. Returns whether anything changed.
    ///
    /// The key keeps its position, so only the value's bytes move. Extra
    /// keys beside `type` go too: the upstream rejects them on a disabled
    /// block already, and `between_tools` takes none.
    pub fn thinking_between_tools(&mut self) -> bool {
        let Some(thinking) = self.request.value.get_mut("thinking") else {
            return false;
        };
        if thinking.get("type").and_then(Value::as_str) != Some("disabled") {
            return false;
        }
        *thinking = serde_json::json!({ "type": "between_tools" });
        true
    }

    /// [`Self::strip_release`] for one marker.
    fn strip_marker(&mut self, marker: &str) {
        // Phase 1 — the parsed decision, as mutation targets (message
        // index, block index; a string-content message has no block
        // index, mirroring the synthesised `{type: "text"}` block).
        let mut targets: Vec<(usize, Option<usize>)> = Vec::new();
        if let Some(messages) = self.request.value.get("messages").and_then(Value::as_array) {
            for (message_index, message) in messages.iter().enumerate() {
                if message.get("role").and_then(Value::as_str) != Some("user") {
                    continue;
                }
                match message.get("content") {
                    Some(Value::String(text)) => {
                        if strippable(text, marker) {
                            targets.push((message_index, None));
                        }
                    }
                    Some(Value::Array(blocks)) => {
                        for (block_index, block) in blocks.iter().enumerate() {
                            if block.get("type").and_then(Value::as_str) != Some("text") {
                                continue;
                            }
                            if block
                                .get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|text| strippable(text, marker))
                            {
                                targets.push((message_index, Some(block_index)));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if targets.is_empty() {
            return; // nothing to strip: body untouched
        }

        // The raw scan: the marker needs no JSON escaping, so at
        // position 0 of a string it is always preceded by the opening
        // quote — that is what makes the scan unambiguous. The scan runs
        // over the serialised bytes, which match the wire bytes for
        // canonical bodies. Inference normalises non-canonical bodies
        // when the backend adapter renders them.
        let bytes = self.request.serialise();
        let needle = format!("\"{marker}");
        let hits = bytes
            .windows(needle.len())
            .filter(|window| *window == needle.as_bytes())
            .count();

        // If the raw scan and the parsed decision disagree, we cannot say
        // which bytes to remove. Leave the request alone.
        if hits != targets.len() {
            return;
        }

        // Phase 2 — remove the marker from exactly the blocks phase 1
        // chose (the same conditions, by construction).
        let Some(messages) = self
            .request
            .value
            .get_mut("messages")
            .and_then(Value::as_array_mut)
        else {
            return;
        };
        for (message_index, block_index) in targets {
            let Some(message) = messages.get_mut(message_index) else {
                continue;
            };
            match block_index {
                None => {
                    if let Some(Value::String(text)) = message.get_mut("content") {
                        *text = text[marker.len()..].to_owned();
                    }
                }
                Some(block_index) => {
                    if let Some(Value::String(text)) = message
                        .get_mut("content")
                        .and_then(Value::as_array_mut)
                        .and_then(|blocks| blocks.get_mut(block_index))
                        .and_then(|block| block.get_mut("text"))
                    {
                        *text = text[marker.len()..].to_owned();
                    }
                }
            }
        }
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

    /// Every message, in order (reversible, for last-user-message scans).
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = Message<'a>> {
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

    /// The message content, typed ([`Content`] is shared with the OpenAI
    /// view: string, block array, or anything else).
    pub fn content(&self) -> Content<'a> {
        match self.value.get("content") {
            Some(Value::String(text)) => Content::Text(text),
            Some(Value::Array(parts)) => Content::Parts(parts.as_slice()),
            other => Content::Other(other),
        }
    }

    /// Plain text of the message:
    /// string content is itself; an array contributes each block's
    /// `text` ("" for string elements, the element itself) joined on
    /// `\n`; anything else is "". The newline join is load-bearing — it
    /// is what keeps the compaction markers line-anchored when blocks are
    /// prepended.
    pub fn text(&self) -> Cow<'a, str> {
        match self.content() {
            Content::Text(text) => Cow::Borrowed(text),
            Content::Parts(blocks) => {
                let pieces: Vec<&str> = blocks
                    .iter()
                    .map(|block| match block {
                        Value::String(text) => text,
                        other => other.get("text").and_then(Value::as_str).unwrap_or(""),
                    })
                    .collect();
                Cow::Owned(pieces.join("\n"))
            }
            Content::Other(_) => Cow::Borrowed(""),
        }
    }

    /// The first text block's text, for a message whose content may be a
    /// string. A content array yields
    /// its FIRST `text`-typed block's `text` — and nothing if that block
    /// has no string `text`: this does not keep
    /// searching past it.
    pub fn first_text(&self) -> Option<&'a str> {
        match self.content() {
            Content::Text(text) => Some(text),
            Content::Parts(blocks) => blocks
                .iter()
                .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .and_then(|block| block.get("text"))
                .and_then(Value::as_str),
            Content::Other(_) => None,
        }
    }

    /// Whether the message carries a `tool_result` block.
    /// A turn carrying a tool
    /// result is a continuation, never a summarisation prompt — Claude
    /// Code builds the compaction request's last message from the prompt
    /// string alone, so this can only exclude the wrong thing.
    pub fn has_tool_result(&self) -> bool {
        matches!(self.content(), Content::Parts(blocks)
            if blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result")))
    }
}

/// The `system` field, typed: a plain string, a content-block array, or
/// anything else (absent, `null`, a shape the view does not model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System<'a> {
    /// `system` as a plain string.
    Text(&'a str),
    /// `system` as a content-block array.
    Blocks(&'a [Value]),
    /// Absent, `null`, or another shape; the raw value, when there is one.
    Other(Option<&'a Value>),
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
    /// The tool's name:
    /// `name`, then `type`, then a constant `?`. A tool never reads as
    /// absent — the `?` keeps the count and the join aligned the way
    /// the predecessor's did.
    pub fn name(&self) -> &'a str {
        self.value
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| self.value.get("type").and_then(Value::as_str))
            .unwrap_or("?")
    }
}

/// The content-free request shape for the ledger and lanes:
/// counts, lengths, and digests only —
/// never prompt, message, or tool text (invariant 1). The server unit
/// copies these into the `requests` row.
///
/// Length unit parity: JS strings are UTF-16, and the ledger must
/// stay comparable with the predecessor's imported rows, so every `chars`
/// count here is UTF-16 code units (see [`AnthropicBody::shape`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicShape {
    /// The original wire buffer's length, passed through from parse.
    pub req_bytes: u64,
    /// Message count; `None` when `messages` is missing or not an array
    /// (absence ≠ zero, invariant 3) — including batches bodies.
    pub req_messages: Option<u64>,
    /// Tool count (0 when `tools` is absent — row parity).
    pub req_tools: u64,
    /// Digest of the tool-name list joined with `\0`, in order (order
    /// matters as much as membership: tools render first, so any
    /// reordering invalidates the entire prefix). An empty list hashes
    /// the empty join, exactly as the predecessor's `shortHash("")`, so
    /// tool-less requests key the same lane before and after import.
    pub tools_hash: String,
    /// Total system text length in UTF-16 code units.
    pub system_chars: u64,
    /// Digest of the concatenated system text; always present, even when
    /// empty (row parity).
    pub system_hash: String,
    /// Per-block digests/lengths: one per system piece, in order.
    /// Localises a system-prompt change to a block; the
    /// ladders localise it further.
    pub system_blocks: Vec<SystemBlockDigest>,
    /// How many mid-conversation `system` messages the request carries —
    /// a count, never the text. Recorded because it decides whether the
    /// compaction rewrite can send a conversation to Sonnet untouched.
    /// `None` when zero (the row omits the field, so rows that carry none
    /// are unchanged).
    pub system_messages: Option<u64>,
    /// Compaction generations: occurrences of the continuation preamble in
    /// the FIRST message's text. Scoped to the
    /// first message, not anchored to its start — Claude Code prepends
    /// system-reminder blocks, so requiring offset zero missed every real
    /// case. `None` when zero.
    pub compact_generations: Option<u64>,
    /// Whether the last non-system message begins a line with a
    /// summarisation instruction. NOT a compaction flag: Claude Code
    /// issues the same summarisation prompt for session titles and
    /// resume metadata, on a small model with no tools, several times a
    /// minute. Which kind it is takes the tool set too — ask
    /// [`AnthropicShape::is_compaction`], never this field alone.
    pub summarising: bool,
    /// Where a summarisation wording sits near the end, matched or not
    /// ([`CompactMarker`]). Diagnostic only: nothing acts on it.
    pub compact_marker: Option<CompactMarker>,
    /// Whether this is Claude Code's recap: the last non-system message
    /// begins a line with the recap prompt. A recap forks the whole
    /// conversation, tools and cache parameters included, for one line of
    /// UI text that never enters the transcript, so the cold gate holds it
    /// on a cold lane ([`crate::middleware::cold::Turn::Recap`]).
    pub recap: bool,
    /// Cumulative system-text digests every 8 KiB (`LADDER_STEP`):
    /// the first rung that differs between two requests bounds the
    /// change to one 8 KiB window. Complete steps only, so the final
    /// partial step is unmeasured — the tail ladder covers that gap.
    pub system_ladder: Vec<String>,
    /// Digests of the system text's last 8, 16, … 256 bytes (8-byte
    /// steps), then 320, 384, … 1024 (64-byte steps):
    /// every observed system-prompt change landed within the last few
    /// hundred bytes, so resolution is spent there. Suffixes are compared
    /// by length, so this survives the prompt changing size.
    pub system_tail: Vec<String>,
}

impl AnthropicShape {
    /// Whether this request is a real compaction.
    ///
    /// The separator between a compaction and the routine summariser is
    /// not size, which would be a threshold to tune: a compaction
    /// *continues a session*, so Claude Code sends it with that
    /// session's tool set, while the title/resume summariser is a
    /// standalone one-shot that cannot call a tool and is sent with none.
    /// Measured, the two do not overlap.
    ///
    /// The predecessor answered `null` where `reqTools` is absent, because
    /// its log
    /// rows predate the field; a freshly extracted shape always knows its
    /// tool count, so the three-valued logic collapses to a bool here
    /// (the historical-row case is `toker import`'s concern, phase 2).
    pub fn is_compaction(&self) -> bool {
        self.summarising && self.req_tools > 0
    }
}

/// One system block's digest and length (no content — invariant 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemBlockDigest {
    /// The block's length in UTF-16 code units (unit parity — JS strings).
    pub chars: u64,
    /// The block's sha256/12 digest.
    pub hash: String,
}

// ── predecessor ports, private ────────────────────────────────────────

/// The system text pieces, in order:
/// (a string system is one piece; a block array
/// contributes each string element and each block's `text`.
fn system_pieces(value: &Value) -> Vec<&str> {
    match value.get("system") {
        Some(Value::String(text)) => vec![text],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match block {
                Value::String(text) => text,
                other => other.get("text").and_then(Value::as_str).unwrap_or(""),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Compaction markers, anchored by position rather than presence.
/// Searching the whole body is wrong:
/// any conversation that *discusses* compaction contains the marker text
/// and reports itself as compacted. Claude Code puts the continuation
/// preamble in the first message and the summarisation instruction in the
/// last turn, so both are matched at those positions only — and the
/// summarisation wordings are additionally line-anchored within it, so
/// quoted prose and file listings do not count.
///
/// "The last turn" is the last message that is not a mid-conversation
/// `system` message. Claude Code sends hook output and attachments that
/// way, and one trailing the prompt hid every compaction from the cutover
/// to 2026-10-06: the detector read the system message, found nothing, and
/// the retarget never ran.
fn compaction_of(messages: &[Value]) -> (Option<u64>, bool, Option<CompactMarker>) {
    let Some(first) = messages.first() else {
        return (None, false, None);
    };
    // A first message that quotes the preamble (its own summary)
    // over-counts — a limitation carried over from the predecessor. The
    // first message is the
    // tightest scope that still works.
    let generations = count_of(&Message { value: first }.text(), COMPACT_RESUMED);
    let compact_generations = (generations > 0).then_some(generations);

    let summarising = last_turn_begins_line(messages, COMPACT_PERFORMING);
    (compact_generations, summarising, compact_marker(messages))
}

/// Whether the last non-system message begins a line with any of
/// `markers` and carries no tool result: the position rules a fixed
/// prompt Claude Code appends to the conversation is matched under
/// ([`compaction_of`] has the reasons).
fn last_turn_begins_line(messages: &[Value], markers: &[&str]) -> bool {
    messages
        .iter()
        .rposition(|value| Message { value }.role() != Some("system"))
        .is_some_and(|at| {
            let last = Message {
                value: &messages[at],
            };
            !last.has_tool_result() && begins_line_any(&last.text(), markers)
        })
}

/// How many messages from the end [`compact_marker`] looks at: enough to
/// see past a few trailing system messages, few enough that an ordinary
/// turn quoting the wording further back stays out of it.
const MARKER_SCAN: usize = 4;

/// Where a summarisation wording sits near the end of the conversation,
/// matched or not, so the next change to Claude Code's layout leaves
/// evidence in the ledger instead of silence. Positions and booleans
/// only, never text (invariant 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactMarker {
    /// Messages between the carrier and the end: 0 when it is the last.
    pub from_end: u64,
    /// The carrying message's role.
    pub role: Option<String>,
    /// The roles of the messages after it, in order.
    pub trailing: Vec<String>,
    /// Whether a wording began a line (the detector's anchor).
    pub line_start: bool,
    /// Whether the carrying message holds a tool result (the detector's
    /// refusal).
    pub tool_result: bool,
}

impl CompactMarker {
    /// The row's `extra.compactMarker` object.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "fromEnd": self.from_end,
            "role": self.role,
            "trailing": self.trailing,
            "lineStart": self.line_start,
            "toolResult": self.tool_result,
        })
    }
}

/// The last of the final [`MARKER_SCAN`] messages carrying a
/// summarisation wording anywhere in its text.
fn compact_marker(messages: &[Value]) -> Option<CompactMarker> {
    let start = messages.len().saturating_sub(MARKER_SCAN);
    let (at, value) = messages
        .iter()
        .enumerate()
        .skip(start)
        .rev()
        .find(|(_, value)| {
            let text = Message { value }.text();
            COMPACT_PERFORMING
                .iter()
                .any(|marker| text.contains(marker))
        })?;
    let message = Message { value };
    let text = message.text();
    Some(CompactMarker {
        from_end: (messages.len() - 1 - at) as u64,
        role: message.role().map(str::to_owned),
        trailing: messages[at + 1..]
            .iter()
            .map(|value| Message { value }.role().unwrap_or("?").to_owned())
            .collect(),
        line_start: begins_line_any(&text, COMPACT_PERFORMING),
        tool_result: message.has_tool_result(),
    })
}

/// Whether any of `markers` begins a line of `haystack`.
fn begins_line_any(haystack: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| begins_line(haystack, marker))
}

/// Non-overlapping occurrence count.
fn count_of(haystack: &str, needle: &str) -> u64 {
    let mut count = 0u64;
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        count += 1;
        from += at + needle.len();
    }
    count
}

/// `needle` at the start of `hay`, or at the start of a line within it.
/// In the assembled prompt the
/// preamble is at offset 0 and the instruction follows a blank line, so
/// both begin a line; quoted prose and listings carry them mid-line. The
/// line anchor survives blocks being prepended — [`Message::text`] joins
/// on a newline.
pub(crate) fn begins_line(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        let at = from + at;
        if at == 0 || bytes[at - 1] == b'\n' {
            return true;
        }
        from = at + 1;
    }
    false
}

/// Cumulative digests every 8 KiB of UTF-16 text. Rungs at complete steps
/// only (`end < length`), so
/// an exact multiple contributes no rung for its final step.
fn prefix_ladder(units: &[u16]) -> Vec<String> {
    let mut rungs = Vec::new();
    let mut end = LADDER_STEP;
    while end < units.len() {
        rungs.push(short_hash(&utf16_bytes(&units[..end])));
        end += LADDER_STEP;
    }
    rungs
}

/// How many prefix rungs [`prefix_ladder`] cuts for a text of
/// `text_length` UTF-16 units: one per complete step strictly inside the
/// text. `pub(crate)`: a stored ladder whose length disagrees was cut to
/// another geometry (ctp's first ladders stepped every 2 KiB, and its first
/// tail reached 4 KiB), so comparing its rungs with today's would name
/// windows the rungs never bounded.
pub(crate) fn prefix_rungs(text_length: usize) -> usize {
    text_length.saturating_sub(1) / LADDER_STEP
}

/// The tail-ladder offsets for a text of `text_length` UTF-16 units:
/// fine steps close to the end, coarser
/// further back — every observed change has been within the last few
/// hundred bytes, so resolution is spent there. Offsets longer than the
/// text are dropped. `pub(crate)`: the rebuild localisation walks the
/// stored rungs by these offsets.
pub(crate) fn tail_offsets(text_length: usize) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(44);
    let mut len = TAIL_FINE_STEP;
    while len <= TAIL_FINE_SPAN {
        offsets.push(len);
        len += TAIL_FINE_STEP;
    }
    // The conditional start
    // (coarse run beginning right after the fine span only when
    // `TAIL_SPAN >= TAIL_FINE_SPAN`) holds
    // for these constants, so the coarse run begins right after the fine
    // span.
    let mut len = TAIL_FINE_SPAN + TAIL_STEP;
    while len <= TAIL_SPAN {
        offsets.push(len);
        len += TAIL_STEP;
    }
    offsets.retain(|&len| len <= text_length);
    offsets
}

/// Digests of the last 8, 16, … bytes of the system text.
fn suffix_ladder(units: &[u16]) -> Vec<String> {
    let total = units.len();
    tail_offsets(total)
        .into_iter()
        .map(|len| short_hash(&utf16_bytes(&units[total - len..])))
        .collect()
}

/// The UTF-8 bytes of a UTF-16 slice, with Node semantics: an
/// unpaired surrogate (a rung boundary that splits an astral character)
/// encodes as U+FFFD, exactly as Node's `Buffer.from` does.
fn utf16_bytes(units: &[u16]) -> Vec<u8> {
    String::from_utf16_lossy(units).into_bytes()
}

/// Whether a text block starting with `marker` can lose it
/// (the would-empty-it test): the remainder must survive `trim`
/// non-empty, because the API rejects empty AND whitespace-only text
/// blocks. JS `trim` removes Unicode White_Space plus U+FEFF;
/// `char::is_whitespace` covers every member but U+FEFF.
fn strippable(text: &str, marker: &str) -> bool {
    let Some(rest) = text.strip_prefix(marker) else {
        return false;
    };
    !rest
        .trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
        .is_empty()
}

#[cfg(test)]
mod tests {
    use super::super::short_hash;
    use super::{MARKER_SCAN, PLAN_SENTINEL, Release, SENTINEL, System};
    use crate::ir::Request;

    fn parse(body: &[u8]) -> Request {
        Request::parse(body).expect("test body parses")
    }

    /// A canonical body with the given messages
    /// (`body`/`userText` helper shape).
    fn body_of(messages: impl Into<serde_json::Value>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "model": "claude-opus-5", "messages": messages.into()
        }))
        .expect("serialise test body")
    }

    fn user_text(text: &str) -> serde_json::Value {
        serde_json::json!({"role": "user", "content": [{"type": "text", "text": text}]})
    }

    fn user_string(text: &str) -> serde_json::Value {
        serde_json::json!({"role": "user", "content": text})
    }

    fn assistant_text(text: &str) -> serde_json::Value {
        serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": text}]})
    }

    fn system_message(text: &str) -> serde_json::Value {
        serde_json::json!({"role": "system", "content": [{"type": "text", "text": text}]})
    }

    // ── release marker: detection ─────────────────────────────────────

    #[test]
    fn the_marker_literals_are_unchanged() {
        assert_eq!(SENTINEL, "$#$BURN$#$");
        assert_eq!(PLAN_SENTINEL, "$#$OVER$#$");
    }

    /// The plan marker fires under the same anchoring as the overage one,
    /// and says which it is.
    #[test]
    fn the_plan_marker_fires_as_a_plan_release() {
        let body = body_of(vec![user_text(&format!("{PLAN_SENTINEL} use the rest"))]);
        assert_eq!(
            parse(&body).anthropic().release_marker(),
            Some(Release::Plan)
        );
        let body = body_of(vec![user_text(&format!("later {PLAN_SENTINEL}"))]);
        assert_eq!(parse(&body).anthropic().release_marker(), None);
    }

    /// The plan marker strips exactly like the overage one: a byte splice
    /// at position zero, and only there.
    #[test]
    fn the_plan_marker_strips_like_the_overage_marker() {
        let original = body_of(vec![
            user_text(&format!("{PLAN_SENTINEL} keep going")),
            assistant_text("ok"),
            user_text(&format!("quoting {PLAN_SENTINEL} mid-line")),
        ]);
        let mut request = parse(&original);
        request.anthropic_mut().strip_release();
        let out = request.serialise();
        let needle = format!("\"{PLAN_SENTINEL}");
        let at = original
            .windows(needle.len())
            .position(|window| window == needle.as_bytes())
            .expect("the marker is in the body");
        let mut spliced = original[..at + 1].to_vec();
        spliced.extend_from_slice(&original[at + 1 + PLAN_SENTINEL.len()..]);
        assert_eq!(out, spliced);
        assert_eq!(request.anthropic().release_marker(), None);
    }

    #[test]
    fn fires_when_the_marker_opens_the_last_user_message() {
        let body = body_of(vec![user_text(&format!("{SENTINEL} keep going"))]);
        assert_eq!(
            parse(&body).anthropic().release_marker(),
            Some(Release::Overage)
        );
    }

    // This is the code-fragment case: an earlier design matched anywhere
    // and would have rewritten `if (!burn) ...` to `if () ...`.
    #[test]
    fn does_not_fire_mid_message() {
        let body = body_of(vec![user_text("var burn = true;\nif (!burn) x();")]);
        assert_eq!(parse(&body).anthropic().release_marker(), None);
        let body = body_of(vec![user_text(&format!("please run {SENTINEL} later"))]);
        assert_eq!(parse(&body).anthropic().release_marker(), None);
    }

    // Claude Code appends a trailing mid-conversation system message after
    // the user's turn. Reading the LAST message finds a system block and
    // silently never matches.
    #[test]
    fn the_last_message_is_not_the_last_user_message() {
        let body = body_of(vec![
            user_text(&format!("{SENTINEL} go on")),
            system_message("reminder"),
        ]);
        assert_eq!(
            parse(&body).anthropic().release_marker(),
            Some(Release::Overage)
        );
    }

    #[test]
    fn an_earlier_user_message_no_longer_last_does_not_regrant() {
        let body = body_of(vec![
            user_text(&format!("{SENTINEL} go on")),
            assistant_text("ok"),
            user_text("and now something else"),
        ]);
        assert_eq!(parse(&body).anthropic().release_marker(), None);
    }

    // A tool-loop follow-up ends in tool_result blocks, not typed text.
    #[test]
    fn a_tool_loop_follow_up_does_not_grant() {
        let body = body_of(vec![
            user_text(&format!("{SENTINEL} go on")),
            serde_json::json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}},
            ]}),
            serde_json::json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "hi"},
            ]}),
        ]);
        assert_eq!(parse(&body).anthropic().release_marker(), None);
    }

    #[test]
    fn string_content_is_handled_not_just_block_arrays() {
        let body = body_of(vec![user_string(&format!("{SENTINEL} go"))]);
        assert_eq!(
            parse(&body).anthropic().release_marker(),
            Some(Release::Overage)
        );
    }

    // The first-text read stops at the first text-typed block: a marker in
    // a later block of the same message never fires.
    #[test]
    fn only_the_first_text_block_of_the_last_user_message_is_read() {
        let body = body_of(vec![serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "no marker here"},
            {"type": "text", "text": SENTINEL},
        ]})]);
        assert_eq!(parse(&body).anthropic().release_marker(), None);
        // …and a text-typed block without string text ends the search
        // the same way a first-match find does.
        let body = body_of(vec![serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": 5},
            {"type": "text", "text": SENTINEL},
        ]})]);
        assert_eq!(parse(&body).anthropic().release_marker(), None);
    }

    #[test]
    fn absent_or_malformed_shapes_read_false_never_panic() {
        assert_eq!(parse(b"{}").anthropic().release_marker(), None);
        assert_eq!(
            parse(br#"{"messages":5}"#).anthropic().release_marker(),
            None
        );
        assert_eq!(
            parse(br#"{"messages":{}}"#).anthropic().release_marker(),
            None
        );
        assert_eq!(parse(b"[]").anthropic().release_marker(), None);
        assert_eq!(parse(b"5").anthropic().release_marker(), None);
        assert_eq!(parse(br#""body""#).anthropic().release_marker(), None);
    }

    // ── release marker: stripping ─────────────────────────────────────

    #[test]
    fn strips_at_position_zero_and_is_byte_equal_to_a_hand_splice() {
        let original = body_of(vec![user_text(&format!("{SENTINEL} keep going"))]);
        let mut request = parse(&original);
        request.anthropic_mut().strip_release();

        // The hand-done splice: keep the opening quote, drop the marker.
        let needle = format!("\"{SENTINEL}");
        let at = original
            .windows(needle.len())
            .position(|window| window == needle.as_bytes())
            .expect("needle in original");
        let mut spliced = Vec::with_capacity(original.len() - SENTINEL.len());
        spliced.extend_from_slice(&original[..at + 1]);
        spliced.extend_from_slice(&original[at + 1 + SENTINEL.len()..]);
        assert_eq!(request.serialise(), spliced);
        assert_eq!(request.serialise().len(), original.len() - SENTINEL.len());
        // The model-visible text is the remainder.
        let stripped = request.anthropic();
        assert_eq!(
            stripped.messages().get(0).unwrap().first_text(),
            Some(" keep going")
        );

        // Idempotent: the marker is gone, so a second pass changes nothing.
        let once = request.serialise();
        request.anthropic_mut().strip_release();
        assert_eq!(request.serialise(), once);
    }

    #[test]
    fn string_content_is_stripped_like_a_synthesised_text_block() {
        let original = body_of(vec![user_string(&format!("{SENTINEL} go"))]);
        let mut request = parse(&original);
        request.anthropic_mut().strip_release();
        assert_eq!(
            request.anthropic().messages().get(0).unwrap().first_text(),
            Some(" go")
        );
        assert_eq!(request.serialise().len(), original.len() - SENTINEL.len());
    }

    // THE regression: a strip not anchored at position 0 would reach into
    // the middle of a line and rewrite code.
    #[test]
    fn a_marker_in_the_middle_of_a_code_line_is_not_stripped() {
        let original = body_of(vec![user_text(&format!(
            "the guard is:\nif (tag === \"{SENTINEL}\") burn();"
        ))]);
        let mut request = parse(&original);
        request.anthropic_mut().strip_release();
        assert_eq!(request.serialise(), original);
    }

    #[test]
    fn a_marker_not_at_position_zero_is_left_alone_byte_for_byte() {
        let original = body_of(vec![user_text(&format!("please run {SENTINEL} later"))]);
        let mut request = parse(&original);
        request.anthropic_mut().strip_release();
        assert_eq!(request.serialise(), original);
    }

    // The API rejects empty and whitespace-only text blocks, and the
    // message persists in history, so emptying it would invalidate every
    // later request in the conversation.
    #[test]
    fn a_marker_only_block_cannot_be_stripped() {
        let original = body_of(vec![user_text(SENTINEL)]);
        let mut request = parse(&original);
        request.anthropic_mut().strip_release();
        assert_eq!(request.serialise(), original);

        let ws = body_of(vec![user_text(&format!("{SENTINEL}   "))]);
        let mut request = parse(&ws);
        request.anthropic_mut().strip_release();
        assert_eq!(request.serialise(), ws);

        // JS trim also removes U+FEFF; a BOM-only remainder stays put.
        let bom = body_of(vec![user_text(&format!("{SENTINEL}\u{FEFF}"))]);
        let mut request = parse(&bom);
        request.anthropic_mut().strip_release();
        assert_eq!(request.serialise(), bom);
    }

    #[test]
    fn assistant_blocks_are_never_touched_even_at_position_zero() {
        let original = body_of(vec![
            assistant_text(&format!("{SENTINEL} echoed")),
            user_text("hello"),
        ]);
        let mut request = parse(&original);
        request.anthropic_mut().strip_release();
        assert_eq!(request.serialise(), original);
    }

    // Here an assistant block echoes the marker, so the raw scan finds two
    // sites while only one is strippable. Splicing on that mismatch would
    // cut the wrong bytes, so the body must come back untouched.
    #[test]
    fn a_raw_scan_disagreeing_with_the_parsed_decision_leaves_the_body_untouched() {
        let original = body_of(vec![
            assistant_text(&format!("{SENTINEL} echoed")),
            user_text(&format!("{SENTINEL} go")),
        ]);
        let mut request = parse(&original);
        request.anthropic_mut().strip_release();
        assert_eq!(request.serialise(), original);
    }

    // The regression that took the predecessor down: a truthy non-array
    // `messages` reached a bare `for...of` and severed every in-flight
    // session.
    #[test]
    fn a_non_array_messages_field_returns_the_body_unchanged() {
        for body in [
            &br#"{"model":"x","messages":5}"#[..],
            &br#"{"model":"x","messages":{}}"#[..],
            &br#"{"model":"x"}"#[..],
            &b"[]"[..],
        ] {
            let mut request = parse(body);
            request.anthropic_mut().strip_release();
            assert_eq!(request.serialise(), body);
        }
    }

    #[test]
    fn multiple_strippable_blocks_all_go_in_one_pass() {
        let original = body_of(vec![
            user_text(&format!("{SENTINEL} first")),
            assistant_text("mid"),
            user_text(&format!("{SENTINEL} second")),
            user_string(&format!("{SENTINEL} third")),
        ]);
        let mut request = parse(&original);
        request.anthropic_mut().strip_release();
        assert_eq!(
            request.serialise().len(),
            original.len() - 3 * SENTINEL.len()
        );
        let view = request.anthropic();
        assert_eq!(view.messages().get(0).unwrap().first_text(), Some(" first"));
        assert_eq!(
            view.messages().get(2).unwrap().first_text(),
            Some(" second")
        );
        assert_eq!(view.messages().get(3).unwrap().first_text(), Some(" third"));
    }

    // ── compaction detection ──────────────────────────────────────────

    const RESUMED: &str = "This session is being continued from a previous conversation";
    const PERFORMING: &str = "Your task is to create a detailed summary of";
    const PREAMBLE: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.";

    #[test]
    fn generations_count_occurrences_in_the_first_message_only() {
        let twice = body_of(vec![serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": RESUMED.to_owned() + ". The summary follows."},
            {"type": "text", "text": RESUMED.to_owned() + " (second generation)."},
        ]})]);
        assert_eq!(
            parse(&twice).anthropic().shape().compact_generations,
            Some(2)
        );

        // Not anchored to the message start: Claude Code prepends
        // system-reminder blocks, so offset-zero anchoring missed every
        // real case.
        let prefixed = body_of(vec![serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "[system reminder]"},
            {"type": "text", "text": RESUMED},
        ]})]);
        assert_eq!(
            parse(&prefixed).anthropic().shape().compact_generations,
            Some(1)
        );

        // The same string outside the first message never counts.
        let elsewhere = body_of(vec![
            user_text("Start."),
            assistant_text(&format!("I read that '{RESUMED}' marks a resume.")),
            user_text("Continue."),
        ]);
        assert_eq!(
            parse(&elsewhere).anthropic().shape().compact_generations,
            None
        );
    }

    #[test]
    fn summarising_requires_a_line_anchored_wording_in_the_last_message() {
        let performing = body_of(vec![
            user_text("Earlier."),
            assistant_text("Done."),
            user_text(&format!(
                "{PREAMBLE}\n\n{PERFORMING} the conversation so far."
            )),
        ]);
        let shape = parse(&performing).anthropic().shape();
        assert!(shape.summarising);
        assert!(!shape.is_compaction(), "no tools: not a compaction (below)");

        // Mid-line quotes do not count: this is the file-listing case that
        // once flagged an ordinary 132-message turn.
        let discussed = body_of(vec![
            user_text("Start."),
            assistant_text("Earlier talk."),
            user_text(&format!(
                "I read that '{PERFORMING}' marks a compaction, mid-line."
            )),
        ]);
        assert!(!parse(&discussed).anthropic().shape().summarising);

        // The line anchor survives blocks being prepended: `text()` joins
        // on a newline, so a later block can still begin a line.
        let prepended = body_of(vec![serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "[context]"},
            {"type": "text", "text": PERFORMING.to_owned() + " the recent portion."},
        ]})]);
        assert!(parse(&prepended).anthropic().shape().summarising);

        // Either performing wording is enough, matched independently.
        let preamble_only = body_of(vec![user_text(PREAMBLE)]);
        assert!(parse(&preamble_only).anthropic().shape().summarising);
    }

    // A turn carrying a tool result is a continuation, never a
    // summarisation prompt — even when it quotes the wording faithfully.
    #[test]
    fn a_tool_result_last_message_refuses_the_summarising_flag() {
        let body = body_of(vec![
            user_text("Earlier."),
            serde_json::json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": PREAMBLE},
            ]}),
        ]);
        assert!(!parse(&body).anthropic().shape().summarising);
    }

    fn system_text(text: &str) -> serde_json::Value {
        serde_json::json!({"role": "system", "content": [{"type": "text", "text": text}]})
    }

    // Claude Code sends hook output and attachments as mid-conversation
    // system messages, and one trailing the prompt hid every compaction
    // from the cutover to 2026-10-06.
    #[test]
    fn trailing_system_messages_do_not_hide_the_prompt() {
        let prompt = format!("{PREAMBLE}\n\n{PERFORMING} the conversation so far.");
        for trailing in 1..=3 {
            let mut messages = vec![
                user_text("Earlier."),
                assistant_text("Done."),
                user_text(&prompt),
            ];
            messages.extend((0..trailing).map(|_| system_text("hook output")));
            let shape = parse(&body_of(messages)).anthropic().shape();
            assert!(shape.summarising, "{trailing} trailing system message(s)");
            let marker = shape.compact_marker.expect("marker recorded");
            assert_eq!(marker.from_end, trailing);
            assert_eq!(marker.role.as_deref(), Some("user"));
            assert_eq!(marker.trailing, vec!["system"; trailing as usize]);
            assert!(marker.line_start);
            assert!(!marker.tool_result);
        }

        // Skipping system messages does not skip the tool-result refusal.
        let quoted = body_of(vec![
            user_text("Earlier."),
            serde_json::json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
                {"type": "text", "text": PREAMBLE},
            ]}),
            system_text("hook output"),
        ]);
        let shape = parse(&quoted).anthropic().shape();
        assert!(!shape.summarising);
        // ...but where the wording sat is still recorded.
        let marker = shape.compact_marker.expect("marker recorded");
        assert!(marker.tool_result);
        assert_eq!(marker.from_end, 1);

        // A system message carrying the wording is not the prompt.
        let in_system = body_of(vec![user_text("Earlier."), system_text(PREAMBLE)]);
        assert!(!parse(&in_system).anthropic().shape().summarising);
    }

    #[test]
    fn a_recap_is_its_prompt_opening_the_last_turn() {
        const RECAP: &str = "The user stepped away and is coming back. Recap in under 40 words.";
        let recap = body_of(vec![
            user_text("Earlier."),
            assistant_text("Done."),
            user_text(RECAP),
            system_text("hook output"),
        ]);
        let shape = parse(&recap).anthropic().shape();
        assert!(shape.recap);
        assert!(!shape.summarising);

        // Quoted mid-line, or anywhere but the last turn, it is not one.
        let quoted = body_of(vec![user_text(&format!("It said '{RECAP}' to me."))]);
        assert!(!parse(&quoted).anthropic().shape().recap);
        let earlier = body_of(vec![
            user_text(RECAP),
            assistant_text("A recap."),
            user_text("Carry on."),
        ]);
        assert!(!parse(&earlier).anthropic().shape().recap);
    }

    #[test]
    fn the_marker_diagnostic_stays_quiet_on_an_ordinary_turn() {
        let ordinary = body_of(vec![
            user_text("Start."),
            assistant_text("Earlier talk."),
            user_text("Carry on."),
            system_text("hook output"),
        ]);
        assert_eq!(parse(&ordinary).anthropic().shape().compact_marker, None);

        // A quote further back than the scan is out of it.
        let mut far = vec![user_text(&format!("Quoting: {PERFORMING}"))];
        far.extend((0..MARKER_SCAN).map(|_| user_text("later")));
        assert_eq!(
            parse(&body_of(far)).anthropic().shape().compact_marker,
            None
        );

        // A mid-line quote inside the scan is recorded, and says so.
        let mid = body_of(vec![user_text(&format!(
            "I read that '{PERFORMING}' is it."
        ))]);
        let marker = parse(&mid)
            .anthropic()
            .shape()
            .compact_marker
            .expect("recorded");
        assert!(!marker.line_start);
    }

    #[test]
    fn is_compaction_needs_the_tool_set_not_just_the_wording() {
        // A compaction continues a session, so it carries that
        // session's tools; the routine title summariser is a standalone
        // one-shot with none.
        let compaction = serde_json::json!({
            "model": "claude-opus-5",
            "tools": [{"name": "read_file", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": PERFORMING.to_owned() + " the conversation so far."},
            ]}],
        });
        let body = serde_json::to_vec(&compaction).expect("serialise");
        let shape = parse(&body).anthropic().shape();
        assert!(shape.summarising);
        assert_eq!(shape.req_tools, 1);
        assert!(shape.is_compaction());

        let summariser = serde_json::json!({
            "model": "claude-haiku-4.5",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": PERFORMING.to_owned() + " this conversation for a title."},
            ]}],
        });
        let body = serde_json::to_vec(&summariser).expect("serialise");
        let shape = parse(&body).anthropic().shape();
        assert!(shape.summarising);
        assert_eq!(shape.req_tools, 0);
        assert!(!shape.is_compaction());
    }

    // ── shape basics ────────────────────────────────────────────────────

    // ── stream flag semantics ───────────────────────────────────────────

    #[test]
    fn stream_true_and_explicitly_false_are_distinct_from_an_omitted_field() {
        // `streamFalse` is
        // `stream === false` — only an explicit false. The gates use it to
        // pick the synthetic turn's rendering: a client that omitted the
        // field gets SSE, not a JSON body it may not parse.
        let request = parse(br#"{"model":"m","messages":[],"stream":true}"#);
        let view = request.anthropic();
        assert!(view.stream());
        assert!(!view.stream_explicitly_false());

        let request = parse(br#"{"model":"m","messages":[],"stream":false}"#);
        let view = request.anthropic();
        assert!(!view.stream());
        assert!(view.stream_explicitly_false());

        let request = parse(br#"{"model":"m","messages":[]}"#);
        let view = request.anthropic();
        assert!(!view.stream());
        assert!(!view.stream_explicitly_false(), "omitted is not false");

        let request = parse(br#"{"model":"m","messages":[],"stream":"no"}"#);
        let view = request.anthropic();
        assert!(!view.stream());
        assert!(!view.stream_explicitly_false(), "a non-bool is not false");
    }

    #[test]
    fn system_reads_string_blocks_and_other_shapes() {
        let body = br#"{"model":"m","system":"plain","messages":[]}"#;
        assert_eq!(parse(body).anthropic().system(), System::Text("plain"));

        let body = br#"{"model":"m","system":[{"type":"text","text":"a"}],"messages":[]}"#;
        assert!(matches!(
            parse(body).anthropic().system(),
            System::Blocks(_)
        ));

        assert_eq!(parse(b"{}").anthropic().system(), System::Other(None));
        assert_eq!(
            parse(br#"{"system":5}"#).anthropic().system(),
            System::Other(Some(&serde_json::json!(5)))
        );
    }

    #[test]
    fn system_blocks_digest_each_piece_in_order() {
        let body = br#"{"model":"m","system":[{"type":"text","text":"You are."},{"type":"text","text":"Second."},"bare",{"no_text":true}],"messages":[]}"#;
        let shape = parse(body).anthropic().shape();
        assert_eq!(
            shape.system_blocks,
            vec![
                super::SystemBlockDigest {
                    chars: 8,
                    hash: short_hash(b"You are.")
                },
                super::SystemBlockDigest {
                    chars: 7,
                    hash: short_hash(b"Second.")
                },
                super::SystemBlockDigest {
                    chars: 4,
                    hash: short_hash(b"bare")
                },
                super::SystemBlockDigest {
                    chars: 0,
                    hash: short_hash(b"")
                },
            ]
        );
        assert_eq!(shape.system_chars, 19);
        assert_eq!(shape.system_hash, short_hash(b"You are.Second.bare"));
    }

    #[test]
    fn lengths_count_utf16_units_like_js_strings() {
        // An astral character is two units, as in JS.
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "m", "system": "🎉a", "messages": [],
        }))
        .expect("serialise");
        let shape = parse(&body).anthropic().shape();
        assert_eq!(shape.system_chars, 3);
        assert_eq!(shape.system_blocks[0].chars, 3);
    }

    #[test]
    fn tool_names_follow_the_fallback_chain_in_order() {
        let body = br#"{"model":"m","tools":[{"name":"a"},{"type":"custom"},{"weird":true}],"messages":[]}"#;
        let shape = parse(body).anthropic().shape();
        assert_eq!(shape.req_tools, 3);
        assert_eq!(shape.tools_hash, short_hash(b"a\0custom\0?"));

        // Order matters as much as membership: a reorder is a different lane.
        let reordered = br#"{"model":"m","tools":[{"type":"custom"},{"name":"a"},{"weird":true}],"messages":[]}"#;
        let reordered_hash = parse(reordered).anthropic().shape().tools_hash;
        assert_ne!(
            reordered_hash, shape.tools_hash,
            "the join is order-sensitive"
        );
        assert_eq!(reordered_hash, short_hash(b"custom\0a\0?"));
    }

    #[test]
    fn batches_bodies_shape_without_top_level_messages() {
        let body = br#"{"requests":[{"params":{"model":"m","messages":[{"role":"user","content":"x"}]}}]}"#;
        let shape = parse(body).anthropic().shape();
        assert_eq!(
            shape.req_messages, None,
            "batches nests under requests[].params"
        );
        assert_eq!(shape.req_tools, 0);
        // The empty join's digest — the predecessor's key for a
        // tool-less lane, which its imported rows carry.
        assert_eq!(shape.tools_hash, short_hash(b""));
        assert_eq!(shape.tools_hash, "e3b0c44298fc");
        assert_eq!(shape.system_chars, 0);
        assert_eq!(shape.system_hash, short_hash(b""));
        assert!(shape.system_blocks.is_empty());
        assert!(!shape.is_compaction());
    }

    #[test]
    fn mid_conversation_system_messages_are_counted_and_omitted_when_zero() {
        let body = body_of(vec![
            user_text("hi"),
            system_message("reminder"),
            system_message("another"),
        ]);
        assert_eq!(parse(&body).anthropic().shape().system_messages, Some(2));
        let body = body_of(vec![user_text("hi")]);
        assert_eq!(parse(&body).anthropic().shape().system_messages, None);
    }

    #[test]
    fn strip_message_effort_drops_effort_only_messages_and_keeps_text() {
        let body = br#"{"model":"m","messages":[{"role":"user","content":"hi"},{"role":"system","content":[],"output_config":{"effort":"low"}},{"role":"system","content":[{"type":"text","text":"keep"}],"output_config":{"effort":"high"}},{"role":"user","content":"next"}]}"#;
        let mut request = parse(body);
        assert!(request.anthropic_mut().strip_message_effort());
        let value: serde_json::Value = serde_json::from_slice(&request.serialise()).unwrap();
        let messages = value["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert!(messages[1].get("output_config").is_none());
        assert_eq!(messages[1]["content"][0]["text"], "keep");
    }

    #[test]
    fn strip_message_effort_is_quiet_without_an_effort_message() {
        let body = body_of(vec![user_text("hi"), system_message("reminder")]);
        let mut request = parse(&body);
        assert!(!request.anthropic_mut().strip_message_effort());
        assert_eq!(request.serialise(), body);
    }

    #[test]
    fn thinking_between_tools_replaces_only_a_disabled_block_in_place() {
        let mut request = parse(
            br#"{"model":"m","thinking":{"type":"disabled"},"messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert!(request.anthropic_mut().thinking_between_tools());
        assert_eq!(
            request.serialise(),
            br#"{"model":"m","thinking":{"type":"between_tools"},"messages":[{"role":"user","content":"hi"}]}"#
        );
    }

    #[test]
    fn thinking_between_tools_is_quiet_unless_thinking_is_disabled() {
        for body in [
            &br#"{"model":"m","messages":[]}"#[..],
            br#"{"model":"m","thinking":{"type":"enabled","budget_tokens":2048},"messages":[]}"#,
            br#"{"model":"m","thinking":{"type":"adaptive"},"messages":[]}"#,
            br#"{"model":"m","thinking":{"type":"between_tools"},"messages":[]}"#,
        ] {
            let mut request = parse(body);
            assert!(!request.anthropic_mut().thinking_between_tools());
            assert_eq!(request.serialise(), body);
        }
    }

    #[test]
    fn the_prefix_ladder_covers_complete_steps_only() {
        // Exactly one step: no rung (`end < length`).
        let exact = "x".repeat(8192);
        let body = format!(r#"{{"model":"m","system":"{exact}","messages":[]}}"#);
        assert!(
            parse(body.as_bytes())
                .anthropic()
                .shape()
                .system_ladder
                .is_empty()
        );

        // One unit more: one rung, the digest of the whole 8192-unit prefix.
        let one_more = format!("{exact}y");
        let body = format!(r#"{{"model":"m","system":"{one_more}","messages":[]}}"#);
        assert_eq!(
            parse(body.as_bytes()).anthropic().shape().system_ladder,
            vec![short_hash(exact.as_bytes())]
        );
    }

    #[test]
    fn the_rung_count_matches_the_ladder_cut() {
        for length in [0, 1, 8191, 8192, 8193, 16384, 16385, 40_000] {
            let body = serde_json::to_vec(&serde_json::json!({
                "model": "m", "system": "x".repeat(length), "messages": [],
            }))
            .expect("serialise");
            assert_eq!(
                parse(&body).anthropic().shape().system_ladder.len(),
                super::prefix_rungs(length),
                "{length} units"
            );
        }
    }

    #[test]
    fn a_rung_boundary_that_splits_an_astral_character_hashes_the_replacement() {
        // 8191 ASCII units plus one two-unit astral character: the first
        // rung boundary splits the pair, and both Node
        // (`Buffer.from` on a lone surrogate) and this port hash the
        // U+FFFD replacement.
        let prefix = "x".repeat(8191);
        let system = format!("{prefix}🎉y");
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "m", "system": system, "messages": [],
        }))
        .expect("serialise");
        let expected = format!("{}\u{FFFD}", prefix).into_bytes();
        assert_eq!(
            parse(&body).anthropic().shape().system_ladder,
            vec![short_hash(&expected)]
        );
    }

    #[test]
    fn the_tail_ladder_spends_resolution_close_to_the_end() {
        fn tail_of(system: &str) -> Vec<String> {
            let body = serde_json::to_vec(&serde_json::json!({
                "model": "m", "system": system, "messages": [],
            }))
            .expect("serialise");
            parse(&body).anthropic().shape().system_tail
        }

        assert!(tail_of("").is_empty(), "nothing to measure");
        assert!(
            tail_of("7 chars").is_empty(),
            "shorter than the first offset"
        );
        assert_eq!(tail_of(&"x".repeat(256)).len(), 32, "fine steps 8..=256");
        assert_eq!(
            tail_of(&"x".repeat(1024)).len(),
            44,
            "fine 8..=256 plus coarse 320..=1024"
        );
        assert_eq!(
            tail_of(&"x".repeat(300)).len(),
            32,
            "no coarse step fits in 300"
        );
        // The offsets are suffix lengths: each rung is the digest of the
        // last `len` units — for an 8-unit text, only offset 8 fits.
        let system = "abcdefgh".to_owned();
        assert_eq!(tail_of(&system), vec![short_hash(b"abcdefgh")]);
    }
}
