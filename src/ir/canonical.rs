//! The canonical IR: the protocol-neutral request model that
//! frontend adapters parse INTO and backend adapters render OUT OF
//! ([`crate::translate`] is the layer description; the request
//! direction's composition is
//! [`to_codex`](crate::translate::to_codex)).
//!
//! ## When canonical engages — and when it never does
//!
//! **Same-protocol routes keep the byte-passthrough machinery**: the
//! [`Value`-wrapped protocol IR](crate::ir), whose re-serialisation is
//! byte-exact by construction, so an untransformed request forwards
//! identical bytes. **Canonical engages ONLY for cross-protocol
//! routes** — a body that must change shape changes it exactly once,
//! into a model that is nobody's wire: no frontend's extensions, no
//! backend's dialect, just what the request means. The drops and
//! merges of a cross-protocol route are then BACKEND properties
//! ([`Capabilities`]), declared per backend adapter — never parse
//! decisions baked into a frontend.
//!
//! ## What parsing decides, and what it must not
//!
//! The frontend parse normalises SHAPE, never MEANING: a base64
//! image becomes its `data:` URL (a URL either way), an absent tool
//! description becomes `""` — but a system-role MESSAGE stays a
//! message (what to do with it is backend policy), a thinking block
//! stays a block (the drop is backend policy — cross-provider
//! reasoning is opaque), and sampling parameters stay carried specs
//! (a backend's refusal is its policy, reported as a capability, not
//! a parse-time discard). Content is never silently dropped: what a
//! frontend cannot parse faithfully is
//! [`TranslateError::UnsupportedBlock`](crate::translate::TranslateError),
//! never a truncation.
//!
//! [`CanonBlock::wire_value`] is the parse's inverse where the parse
//! is exact: the frontend engages the typed [`ToolResultContent`]
//! path only when the wire values reproduce the source bytes
//! byte-identically, so anything exotic (extra fields, unknown
//! kinds) keeps its raw-JSON string form instead — byte preservation
//! outranks typing, always.

use serde_json::{Value, json};

/// A canonical request: one cross-protocol turn, wire-agnostic.
///
/// Built by a frontend adapter (today:
/// [`from_anthropic`](crate::translate::from_anthropic)), rendered by
/// a backend adapter (today:
/// [`codex_from_canonical`](crate::translate::codex_from_canonical)).
/// The model slug and prompt-cache key are CALLER-derived facts and
/// stay out of the canonical: they are explicit parameters of the
/// backend adapter (purity — see [`crate::translate`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CanonicalRequest {
    /// The top-level system prompt pieces, in order (the
    /// string-or-blocks system read: each block's `text`, each bare
    /// string element, `""` for a textless block). How they join is
    /// backend policy.
    pub system: Vec<String>,
    /// The conversation, in order. System-role messages STAY
    /// messages — merging them, or hoisting them into the system
    /// prompt, is backend policy ([`Capabilities::system_in_messages`]).
    pub messages: Vec<CanonMessage>,
    /// The callable tools.
    pub tools: Vec<CanonTool>,
    /// The sampling intent — CARRIED, not dropped: a backend that
    /// refuses sampling declares so ([`Capabilities::sampling`]); a
    /// backend that takes it reads the specs.
    pub sampling: SamplingSpec,
    /// The request-side thinking intent (parse `enabled` only;
    /// anything else means "the client did not ask").
    pub thinking: Option<ThinkingSpec>,
    /// The stream flag, tri-state: `Some(true)`/`Some(false)` when the
    /// frontend wire said, `None` when it did not. A backend whose
    /// turns always stream reads nothing here — but it still knows.
    pub stream: Option<bool>,
    /// The tool-choice intent (absent reads [`CanonToolChoice::Auto`]
    /// — the wire default). A backend that cannot express a shape
    /// reports it; the canonical never guesses one into expressibility.
    pub tool_choice: CanonToolChoice,
}

/// One canonical message: a role plus its content blocks, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonMessage {
    pub role: CanonRole,
    pub blocks: Vec<CanonBlock>,
}

/// A message role. `System` is a ROLE here, not a policy: the
/// frontend reports what the wire said; what a backend does with a
/// system-role message is its own concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonRole {
    User,
    Assistant,
    System,
}

/// One content block, wire-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub enum CanonBlock {
    /// A text block. `input_text` or `output_text` on a backend wire
    /// is a rendering decision (role-relative), not a parse fact.
    Text(String),
    /// An image, as a URL — parse-time normalisation only: a base64
    /// source is already its `data:<media_type>;base64,<data>` URL, a
    /// url source is the wire URL verbatim.
    Image { url: String },
    /// The assistant called a tool. `input` is the raw JSON object
    /// (key order and number literals preserved by the Value IR).
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// A tool's result, addressed by the tool-use id.
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
    },
    /// The assistant's reasoning — THINKING STAYS IN THE CANONICAL:
    /// whether reasoning replays is backend policy
    /// ([`Capabilities::thinking_replay`]), never a parse decision.
    /// `text` is the `thinking` field, or a `redacted_thinking`'s
    /// `data` (the only content it has); the block's own signature
    /// metadata is frontend-wire replay machinery this IR does not
    /// carry.
    Thinking { text: String },
}

impl CanonBlock {
    /// The frontend-wire block JSON this canonical block renders as:
    /// the parse's inverse with its normalisations applied (a base64
    /// image is its data-URL form, a thinking block is the plain
    /// shape). The tool_result output-string form and the frontend's
    /// byte-exactness guard both read it — the guard only takes the
    /// typed path when these bytes reproduce the source array's.
    pub fn wire_value(&self) -> Value {
        match self {
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
            } => {
                json!({"type": "tool_result", "tool_use_id": tool_use_id,
                       "content": content.wire_value()})
            }
            CanonBlock::Thinking { text } => json!({"type": "thinking", "thinking": text}),
        }
    }
}

/// A tool result's content: the wire's two shapes.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResultContent {
    /// The output text — the anthropic string content verbatim, or
    /// the raw-JSON string form of a block array the typed path
    /// cannot reproduce byte-identically (extra fields, unknown
    /// kinds; the only lossless form those have).
    String(String),
    /// A block array the typed path reproduces exactly — engaged
    /// only when [`CanonBlock::wire_value`] round-trips the source
    /// bytes, so a backend's string form of it is byte-identical to
    /// the source array's.
    Blocks(Vec<CanonBlock>),
}

impl ToolResultContent {
    /// The output-string form: the text, or the JSON of the blocks'
    /// wire values (the wire's own content-item array has no string
    /// form; this is the lossless one).
    pub fn output_text(&self) -> String {
        match self {
            ToolResultContent::String(text) => text.clone(),
            ToolResultContent::Blocks(blocks) => serde_json::to_string(&Value::Array(
                blocks.iter().map(CanonBlock::wire_value).collect(),
            ))
            .expect("wire values always serialise"),
        }
    }

    /// The wire JSON of the content (the string as a JSON string, the
    /// blocks as their wire array) — [`CanonBlock::wire_value`]'s
    /// content arm.
    fn wire_value(&self) -> Value {
        match self {
            ToolResultContent::String(text) => Value::String(text.clone()),
            ToolResultContent::Blocks(blocks) => {
                Value::Array(blocks.iter().map(CanonBlock::wire_value).collect())
            }
        }
    }
}

/// One callable tool.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonTool {
    /// The tool's name — required: an unnamed entry is a frontend-wire
    /// shape violation, reported, never fabricated into a callable.
    pub name: String,
    /// The description — `""` when the wire carried none (parse-time
    /// normalisation: absence and emptiness read the same).
    pub description: String,
    /// The input schema, the raw JSON object verbatim.
    pub parameters: Value,
}

/// The tool-choice intent.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CanonToolChoice {
    /// The model picks whether to call (anthropic `auto`; absent and
    /// `null` read the same — the wire default).
    #[default]
    Auto,
    /// The model must call some tool (anthropic `any`).
    Any,
    /// The model must call the named tool (anthropic `{type:"tool"}`).
    /// The name when the wire carried a string one — a backend that
    /// cannot express a forced tool reports the shape; this IR never
    /// flattens it into `Any`/`Auto`.
    Tool { name: Option<String> },
    /// A `type` this frontend does not know — carried, never guessed
    /// into a known shape, so a backend reports what the wire said.
    Other { kind: String },
}

/// The sampling intent, CARRIED — never a parse-time discard.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SamplingSpec {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// Omitted when the request omits it (a backend reads absence as
    /// its own default, never a minted number).
    pub max_tokens: Option<u64>,
    pub stop_sequences: Option<Vec<String>>,
}

/// The request-side thinking intent: parse `enabled` only (absent,
/// `null`, or any other shape means "the client did not ask", and
/// the model's own default governs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThinkingSpec {
    /// The requested reasoning budget, the wire's own knob-speak —
    /// how a backend maps it (the codex effort ladder) is backend
    /// policy.
    pub budget_tokens: u64,
}

/// What a backend supports — the BACKEND property, declared per
/// backend adapter. Every `false` is a live-verified fact about that
/// upstream, not an assumption: the adapter's drops are the
/// enforcement of these declarations, and a future backend that
/// supports a thing simply declares `true` and carries the canonical
/// across — no frontend ever changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Sampling parameters (`temperature`, `top_p`, `max_tokens`,
    /// `stop_sequences`).
    pub sampling: bool,
    /// System-role messages among the conversation's items.
    pub system_in_messages: bool,
    /// Replaying another provider's reasoning blocks.
    pub thinking_replay: bool,
    /// Image content blocks.
    pub images: bool,
}

impl Capabilities {
    /// The codex backend's declared capabilities — LIVE-VERIFIED:
    ///
    /// - `sampling: false` — the backend refuses the parameters
    ///   outright ("Unsupported parameter: temperature"), and its
    ///   own client never sends any; the `temperature`/`top_p` its
    ///   responses echo are the backend's defaults, not knobs it
    ///   accepts.
    /// - `system_in_messages: false` — the backend refuses
    ///   system-role input items ("System messages are not allowed");
    ///   its system content rides `instructions` (leading) and the
    ///   preceding-user merge (mid-conversation, ctp's pattern).
    /// - `thinking_replay: false` — protocol-forced out:
    ///   cross-provider reasoning is opaque (claude's thinking blocks
    ///   carry no `encrypted_content` legible to the codex wire);
    ///   the backend still reasons with its own effort, which
    ///   [`ThinkingSpec`] maps onto.
    /// - `images: true` — `input_image` parts, data-URL and wire-URL
    ///   alike.
    pub const CODEX: Capabilities = Capabilities {
        sampling: false,
        system_in_messages: false,
        thinking_replay: false,
        images: true,
    };
}

#[cfg(test)]
mod tests {
    use super::{CanonBlock, CanonToolChoice, Capabilities, ToolResultContent};
    use serde_json::json;

    /// The capabilities the corpus and the backend adapter's drops
    /// both assume — pinned so a declaration change is a visible,
    /// deliberate act (the adapter's tests fail until they re-verify).
    #[test]
    fn the_codex_capabilities_pin_the_live_verified_facts() {
        let caps = Capabilities::CODEX;
        assert!(
            !caps.sampling,
            "refused live: \"Unsupported parameter: temperature\""
        );
        assert!(
            !caps.system_in_messages,
            "refused live: \"System messages are not allowed\""
        );
        assert!(
            !caps.thinking_replay,
            "protocol-forced: cross-provider reasoning is opaque"
        );
        assert!(caps.images, "the backend takes input_image parts");
    }

    /// The wire values are the parse's inverse: the exact shapes the
    /// frontend's byte-exactness guard compares against, and the
    /// tool_result string form a backend emits.
    #[test]
    fn wire_values_render_the_frontend_block_shapes() {
        assert_eq!(
            CanonBlock::Text("Hi".to_owned()).wire_value(),
            json!({"type": "text", "text": "Hi"})
        );
        assert_eq!(
            CanonBlock::Image {
                url: "https://x.test/i.png".to_owned()
            }
            .wire_value(),
            json!({"type": "image",
                   "source": {"type": "url", "url": "https://x.test/i.png"}})
        );
        assert_eq!(
            CanonBlock::Thinking {
                text: "why".to_owned()
            }
            .wire_value(),
            json!({"type": "thinking", "thinking": "why"})
        );
        // The tool_result string form: the text verbatim, or the JSON
        // of the blocks' wire values — the lossless string shape.
        assert_eq!(
            ToolResultContent::String("done".to_owned()).output_text(),
            "done"
        );
        assert_eq!(
            ToolResultContent::Blocks(vec![
                CanonBlock::Text("src holds".to_owned()),
                CanonBlock::Text("two modules.".to_owned()),
            ])
            .output_text(),
            "[{\"type\":\"text\",\"text\":\"src holds\"},{\"type\":\"text\",\"text\":\"two modules.\"}]",
        );
        // Tool choice is carried, never flattened: a forced tool and
        // an unknown type keep their own shapes.
        assert_eq!(
            CanonToolChoice::Tool {
                name: Some("read_file".to_owned())
            },
            CanonToolChoice::Tool {
                name: Some("read_file".to_owned())
            }
        );
        assert_ne!(CanonToolChoice::Any, CanonToolChoice::default());
    }
}
