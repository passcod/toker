//! Cross-protocol translation: the Anthropic Messages frontend onto the
//! codex backend's OpenAI-Responses wire, and its responses back.
//!
//! Two directions, one submodule each, one rule over both: **pure**. A
//! translation is a function of its explicit inputs only — no clock, no
//! counters, no state beyond the event under translation. That is
//! invariant 4 applied cross-protocol, and it buys invariant 5 for free
//! (docs/plans/toker-toolsuite.md:96): a translated request reproduces
//! byte-identically wherever the conversation did not change, so the
//! upstream's cacheable prefix stays stable even though it never
//! existed in the frontend's wire format. The model slug and the
//! prompt cache key are CALLER-derived and passed in as explicit
//! parameters for exactly that reason — translation never reaches for
//! anything the request did not carry.
//!
//! - [`to_codex`](to_codex::to_codex): one Anthropic Messages
//!   body → one
//!   [`ResponsesRequest`](crate::providers::codex::ResponsesRequest)
//!   (unit A's wire types).
//! - [`to_anthropic`]: a responses-dialect turn → Anthropic SSE —
//!   [`AnthropicStream`] fed
//!   [`ResponseEvent`](crate::providers::codex::ResponseEvent)s — or
//!   the complete non-streaming message JSON,
//!   [`message_from_capture`] over unit A's
//!   [`TurnCapture`](crate::providers::codex::TurnCapture).
//!
//! ## The request table (Anthropic Messages → Responses)
//!
//! | Anthropic | Responses |
//! |---|---|
//! | `system` (string or block array) | `instructions` — the text pieces (each block's `text`, each bare string element, `""` when a block carries none) joined on blank lines |
//! | user `text` (string content or text blocks) | `message{role:"user", content:[{type:"input_text", text}]}` |
//! | user `image` (base64 source) | `message` content part `{type:"input_image", image_url:"data:<media_type>;base64,<data>"}` |
//! | user `image` (url source) | `input_image` with the url verbatim |
//! | user `tool_result` | `function_call_output{call_id: tool_use_id, output}` |
//! | assistant `text` | `message{role:"assistant", content:[{type:"output_text", text}]}` |
//! | assistant `tool_use` | `function_call{name, arguments: <input serialised as a JSON string>, call_id: id}` |
//! | assistant `thinking` / `redacted_thinking` | **DROPPED** — see below |
//! | `role:"system"` messages (claude Code's mid-conversation reminders) | `message{role:"system", content:[input_text]}` (the Responses wire accepts system-role input messages) |
//! | tools `{name, description, input_schema}` | `{type:"function", name, description, strict:false, parameters: input_schema}` (`description` `""` when absent) |
//! | `max_tokens` | `max_output_tokens` (omitted when the request omits it) |
//! | `temperature` / `top_p` | same names, the number verbatim |
//! | `tool_choice` absent / `{type:"auto"}` | `tool_choice:"auto"` (the wire constant) |
//! | `tool_choice {type:"any"}` | `tool_choice:"required"` (the Responses equivalent) |
//! | `tool_result.content` string | `function_call_output.output` text |
//! | `tool_result.content` block array | `function_call_output.output` as the JSON of the array (the only lossless string form; the wire's own content-item array output does not fit unit A's typed view, whose `output` is a string) |
//!
//! Consecutive text/image blocks of one message group into ONE message
//! item; a `tool_use`/`tool_result` switches to its own input item,
//! and any text after it opens a NEW message item — order is preserved
//! at item granularity, never reordered.
//!
//! Unknown block **kinds** (well-typed blocks with no mapping in that
//! position — a `tool_use` in a user message, an `image` in an
//! assistant message, a kind this table has never heard of) fail the
//! translation with [`TranslateError::UnsupportedBlock`]: content is
//! never silently dropped, and the CALLER decides policy — reject, or
//! route the request to a backend that speaks it natively. Shape
//! violations (a message without a role, a `tool_use` without an id)
//! fail with [`TranslateError::Malformed`].
//!
//! ### Dropped, loudly — the known translation costs
//!
//! These are deliberate omissions, each with its reason; none is a
//! silent bug:
//!
//! - **`thinking` and `redacted_thinking` blocks are DROPPED on
//!   replay.** Claude's reasoning cannot cross providers: the
//!   Responses reasoning items carry OpenAI's own encrypted reasoning
//!   (`include: ["reasoning.encrypted_content"]`, unit A's constant),
//!   and claude's thinking signatures verify against Anthropic's keys,
//!   so neither direction's reasoning is legible to the other.
//!   Replaying claude thinking as plain text would leak chain-of-thought
//!   the source protocol deliberately redacts. The drop is coherent
//!   both ways: the request direction drops thinking blocks, and the
//!   response direction synthesises **unsigned** thinking blocks from
//!   the codex summaries — display-only, never replayed (and
//!   anthropic-side replay of thinking needs a signature this
//!   translation never mints).
//! - **`stop_sequences`** — the Responses protocol has no stop
//!   sequences; there is nothing to map onto.
//! - **`top_k`**, **`metadata`** (incl. `user_id`), **`thinking`**
//!   config (the codex backend reasons with its own effort; unit C may
//!   set `reasoning.effort` on the built request) — no Responses
//!   equivalent.
//! - **`tool_result.is_error` / `cache_control`** and other block
//!   metadata — metadata, not content; the output text carries what
//!   the model needs.
//! - Unmapped **top-level fields** are dropped: cross-protocol
//!   translation maps the table above, not the raw-preservation rule
//!   ([crate::ir] keeps unknown fields for same-protocol routes; a
//!   foreign protocol has nowhere to put them).
//! - The release marker `$#$BURN$#$` rides through untouched —
//!   stripping is middleware's job
//!   ([`crate::ir::AnthropicBodyMut::strip_release`]), which runs
//!   before translation in the pipeline.
//!
//! ## The response table (Responses → Anthropic SSE)
//!
//! | Responses event | Anthropic events |
//! |---|---|
//! | `response.created` | `message_start` (the response id as the message id, the model echo, **zeroed** usage — the real usage only exists at the turn's end) |
//! | `reasoning_summary_text.delta` (per summary index) | `content_block_start{thinking}` on the index's first delta, one `thinking_delta` per delta, `content_block_stop` when the next block opens or the turn ends |
//! | `output_text.delta` | `content_block_start{text}` on the first delta, one `text_delta` per delta, `content_block_stop` on the message item's done |
//! | `output_item.done` of a `function_call` | `content_block_start{tool_use{id: call_id, name}}`, ONE `input_json_delta` carrying the whole arguments (they arrive complete — unit A's parser buffers them from the done item), `content_block_stop` |
//! | `response.completed` | open block's `content_block_stop`, `message_delta` (stop reason + usage), `message_stop` |
//! | `response.incomplete` | the same close, `message_delta` (stop reason, no usage — the event carries none), `message_stop` |
//! | `response.failed` / `error` | `error` event; no `message_stop` (anthropic error streams end at the error, mid-block, exactly like toker's own captured fixture 06) |
//! | `output_item.added`, unknown kinds | nothing (never dropped bytes — they simply have no anthropic shape) |
//!
//! Stop reasons: any completed `function_call` → `tool_use`; otherwise
//! `end_turn`. Incomplete: `max_output_tokens` → `max_tokens`,
//! `content_filter` → `refusal`, anything else → `max_tokens` (an
//! incomplete turn stopped at its budget — the only budget-shaped
//! anthropic stop reason). No `ping` events — claude tolerates their
//! absence and nothing upstream produces them.
//!
//! ### Usage table
//!
//! | Responses | Anthropic |
//! |---|---|
//! | `input_tokens` | `input_tokens` |
//! | `input_tokens_details.cached_tokens` | `cache_read_input_tokens` |
//! | `input_tokens_details.cache_write_tokens` | `cache_creation_input_tokens` |
//! | `output_tokens` | `output_tokens` |
//!
//! Absent stays absent (invariant 3): a detail the upstream did not
//! carry is omitted, never zeroed. **Translation note on reasoning
//! tokens**: both protocols count reasoning tokens inside
//! `output_tokens`, so the reported total passes through unchanged;
//! the translated usage does NOT break them out into anthropic's
//! `output_tokens_details.thinking_tokens` — the total is what both
//! sides attest, the split is not (do not invent it).
//!
//! ## The error table (Responses errors → Anthropic error events)
//!
//! Best-effort, first match wins:
//!
//! | upstream (`code`, then `kind`) | anthropic `error.type` |
//! |---|---|
//! | `code` containing `rate_limit`, or `kind` `rate_limit_error` | `rate_limit_error`, plus `retry_after` when the upstream carried `resets_at` — the **absolute reset epoch**, verbatim: a relative duration would need a clock, and translation is pure |
//! | `code` containing `context_length` | `invalid_request_error` |
//! | `code` containing `quota` or `usage_limit` | `invalid_request_error` |
//! | `kind` already one of anthropic's own type names | the kind verbatim |
//! | anything else | `api_error` |
//!
//! The error `message` passes through verbatim; when the upstream sent
//! none, the `code` then the `kind` stands in, else the constant
//! `"upstream error"` (the anthropic shape requires a message — the
//! placeholder says nothing the upstream did not).

pub mod to_anthropic;
pub mod to_codex;

pub use to_anthropic::{AnthropicStream, anthropic_error_type, message_from_capture};
pub use to_codex::to_codex;

/// A translation failure, typed: the CALLER (unit C) decides policy —
/// reject the request, or route it to a backend that speaks the body
/// natively. Translation itself only reports; it never guesses a
/// body into shape and never drops content silently.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TranslateError {
    /// A content block whose kind has no faithful shape on the target
    /// wire (in that position). The kind string is the block's own
    /// `type`, verbatim.
    #[error("unsupported content block: {kind}")]
    UnsupportedBlock { kind: String },

    /// A shape violation: a field the source protocol requires is
    /// missing or of the wrong type, or a value has no equivalent on
    /// the target wire and inventing one would change semantics.
    #[error("malformed request: {reason}")]
    Malformed { reason: String },
}
