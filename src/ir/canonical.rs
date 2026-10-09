//! The canonical IR — both directions, protocol-neutral: the request
//! model that frontend adapters parse INTO and backend adapters
//! render OUT OF, and the turn model that backend adapters
//! interpret INTO and frontend adapters render OUT OF
//! ([`crate::translate`] is the layer description; the compositions
//! are [`to_codex`](crate::translate::to_codex) and
//! [`to_anthropic`](crate::translate::to_anthropic)).
//!
//! ## The universal pipeline
//!
//! Every inference route parses into this model and renders from it,
//! including routes whose frontend and backend name the same protocol.
//! Protocol equality may reuse adapter code, but it never bypasses the
//! canonical request or event model. Backend-specific drops and merges
//! are binding capabilities ([`Capabilities`]), never parse decisions
//! baked into a frontend.
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

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Value, json};

pub use crate::routing::Capabilities;
use crate::routing::DialectId;

/// A canonical request: one inference turn, wire-agnostic.
///
/// Built by a frontend adapter (Messages, Chat Completions, or Responses),
/// and rendered by a backend adapter.
/// Prompt-cache identity remains an explicit adapter input because it comes
/// from request headers. Model identity is request semantics and stays in the
/// canonical so routing middleware can transform it before rendering.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CanonicalRequest {
    /// The model named by the current canonical stage. Ingress records the
    /// requested model; routing middleware later replaces it with the
    /// effective backend model. Absence is preserved, never invented.
    pub model: Option<String>,
    /// The top-level system prompt pieces, in order. Text remains semantic;
    /// block-local wire metadata and unrecognized elements remain attached
    /// to their node so structural middleware cannot invalidate their paths.
    /// How text pieces join, and which metadata can replay, is backend policy.
    pub system: Vec<CanonSystemPart>,
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
    /// Wire fields that the ingress adapter does not yet model semantically.
    /// A compatible backend dialect may replay them; every other backend must
    /// report or reject their omission.
    pub extensions: Vec<CanonicalExtension>,
}

/// One top-level system prompt part.
///
/// Anthropic permits either a string or an array of blocks here. A string and
/// a text block have the same canonical meaning, while metadata on the block
/// remains dialect-local and opaque. An array element without a semantic text
/// reading is retained whole rather than invented into an empty string.
#[derive(Debug, Clone, PartialEq)]
pub enum CanonSystemPart {
    Text {
        text: String,
        extensions: Vec<CanonicalExtension>,
    },
    Opaque(CanonicalExtension),
}

impl CanonSystemPart {
    pub fn text(text: impl Into<String>) -> CanonSystemPart {
        CanonSystemPart::Text {
            text: text.into(),
            extensions: Vec::new(),
        }
    }

    pub fn semantic_text(&self) -> Option<&str> {
        match self {
            CanonSystemPart::Text { text, .. } => Some(text),
            CanonSystemPart::Opaque(_) => None,
        }
    }
}

/// One unmodeled wire value retained across the canonical boundary.
#[derive(Clone, PartialEq)]
pub struct CanonicalExtension {
    source: DialectId,
    wire_path: String,
    wire_name: Option<String>,
    value: Value,
}

impl CanonicalExtension {
    pub fn new(
        source: DialectId,
        wire_path: impl Into<String>,
        value: Value,
    ) -> CanonicalExtension {
        CanonicalExtension {
            source,
            wire_path: wire_path.into(),
            wire_name: None,
            value,
        }
    }

    /// Retain an unmodeled field attached to one canonical node. The explicit
    /// wire name avoids reconstructing JSON keys from diagnostic paths.
    pub fn node_field(
        source: DialectId,
        wire_path: impl Into<String>,
        wire_name: impl Into<String>,
        value: Value,
    ) -> CanonicalExtension {
        CanonicalExtension {
            source,
            wire_path: wire_path.into(),
            wire_name: Some(wire_name.into()),
            value,
        }
    }

    pub fn source(&self) -> DialectId {
        self.source
    }

    pub fn wire_path(&self) -> &str {
        &self.wire_path
    }

    pub fn wire_name(&self) -> Option<&str> {
        self.wire_name.as_deref()
    }

    /// The opaque value is available to adapters, but must never be logged or
    /// copied into translation-loss metadata.
    pub fn value(&self) -> &Value {
        &self.value
    }
}

impl fmt::Debug for CanonicalExtension {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanonicalExtension")
            .field("source", &self.source)
            .field("wire_path", &self.wire_path)
            .field("wire_name", &self.wire_name)
            .field("value", &"<opaque>")
            .finish()
    }
}

/// One canonical message: a role plus its content blocks, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonMessage {
    pub role: CanonRole,
    pub blocks: Vec<CanonBlock>,
    /// Dialect-local fields attached to this message rather than one of its
    /// content blocks. Keeping them node-local lets middleware move a whole
    /// message without invalidating an indexed wire path.
    pub extensions: Vec<CanonicalExtension>,
}

/// A message role. `System` is a ROLE here, not a policy: the
/// frontend reports what the wire said; what a backend does with a
/// system-role message is its own concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonRole {
    User,
    Assistant,
    System,
    /// OpenAI Chat's higher-priority instruction role. It stays distinct
    /// from `System`; any backend that lacks the distinction must make its
    /// deterministic transformation explicit in its adapter.
    Developer,
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
    /// The assistant's visible reasoning and its provider signature. Whether
    /// it can replay is a binding capability, never a parse decision.
    Thinking {
        text: String,
        signature: Option<String>,
    },
    /// Provider-encrypted reasoning that must remain opaque and distinct from
    /// visible thinking.
    RedactedThinking { data: String },
    /// Node-local wire metadata decorating a semantic block. Structural
    /// middleware moves this wrapper with the block, so no array index becomes
    /// canonical identity. `is_error` is semantic tool-result state; all other
    /// unmodeled fields remain dialect-local opaque extensions.
    Annotated {
        block: Box<CanonBlock>,
        is_error: Option<bool>,
        extensions: Vec<CanonicalExtension>,
    },
}

impl CanonBlock {
    pub fn annotated(
        self,
        is_error: Option<bool>,
        extensions: Vec<CanonicalExtension>,
    ) -> CanonBlock {
        if is_error.is_none() && extensions.is_empty() {
            self
        } else {
            CanonBlock::Annotated {
                block: Box::new(self),
                is_error,
                extensions,
            }
        }
    }

    /// The semantic block beneath any node-local wire annotation.
    pub fn semantic(&self) -> &CanonBlock {
        match self {
            CanonBlock::Annotated { block, .. } => block.semantic(),
            block => block,
        }
    }

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
            CanonBlock::Thinking { text, signature } => {
                let mut value = json!({"type": "thinking", "thinking": text});
                if let Some(signature) = signature {
                    value
                        .as_object_mut()
                        .expect("a built thinking block is an object")
                        .insert("signature".to_owned(), json!(signature));
                }
                value
            }
            CanonBlock::RedactedThinking { data } => {
                json!({"type": "redacted_thinking", "data": data})
            }
            CanonBlock::Annotated {
                block,
                is_error,
                extensions,
            } => {
                let mut value = block.wire_value();
                let object = value
                    .as_object_mut()
                    .expect("a canonical content block renders as an object");
                if let Some(is_error) = is_error {
                    object.insert("is_error".to_owned(), json!(is_error));
                }
                for extension in extensions {
                    if let Some(name) = extension.wire_name() {
                        object.insert(name.to_owned(), extension.value().clone());
                    }
                }
                value
            }
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
    /// Dialect-local fields attached to this tool, such as prompt cache
    /// controls. Compatible backends can replay them; others report them.
    pub extensions: Vec<CanonicalExtension>,
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
    /// Tool use is forbidden (OpenAI Chat `"none"`).
    None,
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

/// The request-side reasoning intent, in the frontend wire's own units.
///
/// Keeping the forms distinct is deliberate: a Responses effort tier has no
/// evidence-backed inverse token budget, so ingress never invents one. A
/// backend either accepts the same form, translates it by documented policy,
/// or reports that it cannot represent it.
#[derive(Debug, Clone, PartialEq)]
pub enum ThinkingSpec {
    /// Anthropic Messages' explicit reasoning-token budget.
    BudgetTokens(u64),
    /// OpenAI Responses' named reasoning-effort tier.
    Effort(String),
}

// ── the turn model (the response direction) ─────────────────────────

/// One complete tool call, as the canonical carries it in both the
/// stream ([`CanonEvent::ToolCall`]) and the final turn
/// ([`CanonTurn::tool_calls`]): the id, the name, and the **whole**
/// arguments — the canonical contract. The codex backend's reality
/// (arguments arrive complete via `output_item.done`) becomes the
/// model's law: a backend whose wire streams them in pieces buffers
/// them before emitting, and a frontend renders one whole-arguments
/// event (anthropic's single `input_json_delta`).
///
/// `arguments` is the backend's own JSON-string form, verbatim — a
/// frontend that wants the object parses it, and a string that does
/// not parse is the frontend's degradation to report, never a
/// canonical guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Why the turn ended. The codex backend's reading of its wire: any
/// completed function call →
/// [`ToolUse`](CanonStopReason::ToolUse), else
/// [`EndTurn`](CanonStopReason::EndTurn); an incomplete turn's
/// `content_filter` → [`Refusal`](CanonStopReason::Refusal), any
/// other incompleteness →
/// [`Incomplete`](CanonStopReason::Incomplete) with the reason
/// carried. A backend whose wire speaks a stop reason natively
/// (anthropic's own `max_tokens`) emits
/// [`MaxTokens`](CanonStopReason::MaxTokens) directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonStopReason {
    /// The turn ran to its natural end.
    EndTurn,
    /// The turn ended on completed tool calls, expecting results.
    ToolUse,
    /// The turn stopped at a token budget.
    MaxTokens,
    /// A configured stop sequence matched.
    StopSequence,
    /// The provider paused a resumable turn.
    PauseTurn,
    /// The model's context window, rather than the output budget, was reached.
    ContextWindowExceeded,
    /// The turn was refused (content filtering).
    Refusal,
    /// The turn stopped short, for the reason this carries — the
    /// backend's own reason string, verbatim. A frontend with no
    /// shape for it renders its own budget-shaped default
    /// (anthropic's is `max_tokens`: an incomplete turn stopped at
    /// its budget, the only budget-shaped stop reason that wire
    /// has).
    Incomplete(String),
}

/// The canonical usage buckets of one turn: the numbers every
/// protocol's usage MEANS, plus the backend's raw usage JSON carried
/// verbatim alongside — a frontend renders the buckets, and `raw`
/// is for whoever wants the wire's own shape (the ledger's verbatim
/// column, a future frontend that passes usage through).
///
/// Absence ≠ zero (invariant 3): every bucket is an `Option`
/// because a backend may not report it — the codex wire always
/// carries `input`/`output` as plain numbers, but the canonical
/// never assumes any backend's reporting shape. `reasoning` rides
/// INSIDE `output` on both the anthropic and codex protocols (the
/// rendered usage does not break it out — see the pair docs); the
/// canonical carries it as a bucket for whoever wants the split.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalUsage {
    pub input: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub output: Option<u64>,
    pub reasoning: Option<u64>,
    /// The provider that actually served the turn, when the backend attests
    /// one separately from its own provider identity (OpenRouter's
    /// top-level `provider`). This is routing/accounting metadata, never
    /// inferred from the model id.
    pub serving_provider: Option<String>,
    /// The backend's own usage object, re-serialised verbatim (every
    /// member the wire carried, modelled and unmodelled alike).
    pub raw: Value,
}

/// The canonical error taxonomy — the kinds every protocol's error
/// table maps onto.
///
/// The current table's "verbatim" kinds are already anthropic's own
/// type names (`overloaded_error`, `request_too_large`, …), so each
/// has a typed variant that renders to the identical string — no
/// passthrough variant is needed. A future upstream kind that must
/// pass VERBATIM without a typed rendering is the case that adds a
/// `Backend(String)` variant, and the `&'static str` rendering
/// signatures that today forbid it get revisited then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonErrorKind {
    /// A rate limit — the only kind that carries a reset
    /// ([`CanonError::resets_at`]).
    RateLimit,
    /// A request the upstream refused as malformed.
    InvalidRequest,
    /// A credential the upstream rejected.
    Authentication,
    /// A permission the upstream withheld.
    Permission,
    /// A thing the upstream does not have.
    NotFound,
    /// A request beyond the upstream's size limits.
    TooLarge,
    /// An upstream at capacity.
    Overloaded,
    /// Everything else — the generic.
    Api,
}

/// One canonical error: the kind, the message, and — for a rate
/// limit — the reset. The message is already RESOLVED when it gets
/// here: the backend stands its wire's own message in on its code,
/// then its kind, then the constant, so the fallback chain never
/// crosses the canonical boundary and the message renders verbatim
/// downstream. `resets_at` is the **absolute** reset epoch, verbatim
/// — a relative retry-after would need a clock, and translation is
/// pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonError {
    pub kind: CanonErrorKind,
    pub message: String,
    /// Unix seconds: when the rate limit resets, when the upstream
    /// named it.
    pub resets_at: Option<i64>,
}

/// One canonical turn event — the response direction's streaming
/// unit: a backend adapter interprets its wire's turn into these,
/// in stream order; a frontend adapter renders them onto its wire
/// (the composition of the two is
/// [`to_anthropic`](crate::translate::to_anthropic)).
/// Protocol-neutral and frontend-agnostic: no SSE, no
/// `message_start`, no content blocks — a text part is a text part,
/// wherever it streams to. Everything is a pure function of the
/// events the backend emits (invariant 4): no ids minted, no parts
/// invented, no ordering but the arrival order.
#[derive(Debug, Clone, PartialEq)]
pub enum CanonEvent {
    /// The turn opened. `turn_id` is the backend's own response id,
    /// when it named one — never invented; a stream that never named
    /// one leaves it `None` and a frontend renders its own
    /// placeholder, never a minted id (purity).
    TurnStarted { turn_id: Option<String> },
    /// One text delta, in stream order. One text part streams at a
    /// time — [`CanonEvent::TextEnded`] is the boundary that closes
    /// it, so a renderer knows a new part began when the next delta
    /// arrives after one.
    TextDelta { delta: String },
    /// One reasoning delta, of the part `part` names — the
    /// BACKEND's own part identity (codex's `summary_index`): the
    /// semantic "which reasoning part" is canonical even if backends
    /// number their parts differently, and a renderer groups by it.
    /// Deltas of one part continue it; a different part is a new
    /// one.
    ThinkingDelta { part: u64, delta: String },
    /// The provider signature completing one visible reasoning part. It stays
    /// opaque: compatible frontends replay it so the block can be submitted
    /// on a later turn; incompatible frontends report or omit it.
    ThinkingSignature { part: u64, signature: String },
    /// One complete provider-encrypted reasoning block. Unlike visible
    /// reasoning it has no text deltas and must remain opaque end to end.
    RedactedThinking { data: String },
    /// A COMPLETE tool call — the arguments whole (see
    /// [`CanonToolCall`]).
    ToolCall(CanonToolCall),
    /// The current text part completed — no more
    /// [`CanonEvent::TextDelta`]s belong to it.
    TextEnded,
    /// The current reasoning part completed — no more
    /// [`CanonEvent::ThinkingDelta`]s belong to it.
    ThinkingEnded,
    /// The turn ended — normally or short — with the stop reason and
    /// the usage when the turn reported one (absent stays absent,
    /// never zeroed — invariant 3).
    TurnEnded {
        stop_reason: CanonStopReason,
        usage: Option<CanonicalUsage>,
    },
    /// The turn FAILED — a terminal error; the stream ends here.
    TurnFailed { error: CanonError },
    /// A non-terminal error mid-turn; the stream continues (a
    /// backend whose error ENDS the turn emits
    /// [`CanonEvent::TurnFailed`] instead).
    Error { error: CanonError },
}

/// One canonical turn, complete — the non-streaming path's unit: a
/// backend adapter folds its whole turn into this (today: the codex
/// backend, from unit A's `TurnCapture`); a frontend adapter renders
/// the complete response (today: the anthropic message JSON).
#[derive(Debug, Clone, PartialEq)]
pub struct CanonTurn {
    /// The backend's own turn id, when it named one — never invented.
    pub turn_id: Option<String>,
    /// Why the turn ended. An errored turn carries the stop reason it
    /// would have had; a renderer that answers the error instead
    /// never reads it.
    pub stop_reason: CanonStopReason,
    /// The usage, when the turn reported one.
    pub usage: Option<CanonicalUsage>,
    /// The first error the turn hit, when it errored — an errored
    /// turn renders as the error, never a partial message.
    pub error: Option<CanonError>,
    /// The complete tool calls, in completion order, arguments whole.
    pub tool_calls: Vec<CanonToolCall>,
    /// Exact canonical response blocks when the backend supplied a complete
    /// ordered message. Streaming-only backends and older aggregators leave
    /// this absent and use the flattened fields below.
    pub blocks: Option<Vec<CanonBlock>>,
    /// The assistant text, the deltas joined.
    pub text: String,
    /// The reasoning parts, keyed by the backend's part identity,
    /// each part's deltas joined. Ordered by that identity — the
    /// codex summary indices are non-negative array positions, so
    /// the order is the arrival order.
    pub thinking: BTreeMap<u64, String>,
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

    #[test]
    fn messages_capabilities_pin_the_verified_shared_wire() {
        let caps = Capabilities::MESSAGES;
        assert!(caps.sampling);
        assert!(caps.system_in_messages);
        assert!(caps.thinking_replay);
        assert!(caps.images);
    }

    #[test]
    fn chat_capabilities_pin_the_verified_openrouter_wire() {
        let caps = Capabilities::CHAT;
        assert!(caps.sampling);
        assert!(caps.system_in_messages);
        assert!(!caps.thinking_replay);
        assert!(caps.images);
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
                text: "why".to_owned(),
                signature: Some("sig".to_owned()),
            }
            .wire_value(),
            json!({"type": "thinking", "thinking": "why", "signature": "sig"})
        );
        assert_eq!(
            CanonBlock::RedactedThinking {
                data: "opaque".to_owned()
            }
            .wire_value(),
            json!({"type": "redacted_thinking", "data": "opaque"})
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
