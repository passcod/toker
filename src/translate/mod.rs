//! Cross-protocol translation, layered, in BOTH directions:
//! **frontend adapters** speak a frontend's wire and the canonical
//! IR; **backend adapters** speak a backend's wire and the canonical
//! IR. Request side, the frontend adapter parses the wire body INTO
//! the canonical ([`crate::ir::canonical`]) and the backend adapter
//! renders the canonical OUT onto its wire; response side, the
//! backend adapter interprets its wire's turn INTO the canonical
//! and the frontend adapter renders the canonical OUT onto its
//! wire. A pair is the composition of the two adapters — adding a
//! frontend or a backend is one adapter, not a new pair, in either
//! direction. Same-protocol routes never come through here at all:
//! they keep the [`Value`-wrapped protocol IR](crate::ir) and its
//! byte-exact passthrough; translation is the cross-protocol
//! machinery only.
//!
//! One rule over both directions: **pure**. A translation is a
//! function of its explicit inputs only — no clock, no counters, no
//! state beyond the event under translation. That is invariant 4
//! applied cross-protocol, and it buys invariant 5 for free
//! (docs/plans/toker-toolsuite.md:96): a translated request
//! reproduces byte-identically wherever the conversation did not
//! change, so the upstream's cacheable prefix stays stable even
//! though it never existed in the frontend's wire format. The model
//! slug and the prompt cache key are CALLER-derived and passed in as
//! explicit parameters for exactly that reason — translation never
//! reaches for anything the request did not carry.
//!
//! ## The request direction (composed today: anthropic → codex)
//!
//! - [`from_anthropic`](crate::translate::anthropic_frontend): the
//!   frontend adapter — one Anthropic Messages body → one
//!   [`CanonicalRequest`](crate::ir::canonical::CanonicalRequest).
//!   Wire-shape problems are ITS domain
//!   ([`TranslateError::Malformed`] for shape
//!   violations, [`TranslateError::UnsupportedBlock`] for a block
//!   with no faithful parse) — and nothing backend-shaped happens
//!   there: system-role messages stay messages, thinking blocks stay
//!   blocks, sampling rides as specs.
//! - [`codex_backend::codex_from_canonical`]: the backend adapter —
//!   one [`CanonicalRequest`](crate::ir::canonical::CanonicalRequest)
//!   → one
//!   [`ResponsesRequest`](crate::providers::codex::ResponsesRequest).
//!   What this backend refuses is its declared property
//!   ([`Capabilities`](crate::ir::canonical::Capabilities)); its
//!   live-verified cost table lives in its module docs.
//! - [`to_codex`]: the composition of the two — the public entry
//!   the server calls (unit C).
//!
//! ## The request table (Anthropic Messages → Responses, as composed)
//!
//! | Anthropic | Responses |
//! |---|---|
//! | `system` (string or block array) | `instructions` — the text pieces (each block's `text`, each bare string element, `""` when a block carries none) joined on blank lines, then any LEADING system-role message texts appended the same way |
//! | user `text` (string content or text blocks) | `message{role:"user", content:[{type:"input_text", text}]}` |
//! | user `image` (base64 source) | `message` content part `{type:"input_image", image_url:"data:<media_type>;base64,<data>"}` |
//! | user `image` (url source) | `input_image` with the url verbatim |
//! | user `tool_result` | `function_call_output{call_id: tool_use_id, output}` |
//! | assistant `text` | `message{role:"assistant", content:[{type:"output_text", text}]}` |
//! | assistant `tool_use` | `function_call{name, arguments: <input serialised as a JSON string>, call_id: id}` |
//! | assistant `thinking` / `redacted_thinking` | **DROPPED** — this backend's declared cost (see below) |
//! | `role:"system"` messages (claude Code's mid-conversation reminders) | merged into the PRECEDING user turn as `[PROMPT_INJECTION]`-prefixed text parts (this backend refuses system-role input items — live-verified); with no preceding user item, the text joins `instructions` instead |
//! | tools `{name, description, input_schema}` | `{type:"function", name, description, strict:false, parameters: input_schema}` (`description` `""` when absent) |
//! | `max_tokens` / `temperature` / `top_p` | **DROPPED** — this backend's declared cost: it refuses sampling outright (live-verified: "Unsupported parameter: temperature") |
//! | `tool_choice` absent / `{type:"auto"}` | `tool_choice:"auto"` (the wire constant) |
//! | `tool_choice {type:"any"}` | `tool_choice:"required"` (the Responses equivalent) |
//! | `tool_choice {type:"tool"}` or an unknown type | reported — no faithful string form on this wire |
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
//! ### Dropped, loudly — per-BACKEND costs, not toker policy
//!
//! What a backend refuses is its declared property
//! ([`Capabilities`](crate::ir::canonical::Capabilities)), enforced in its adapter and documented there
//! (the codex backend's cost table lives in [`codex_backend`]). The
//! canonical IR carries the intent — thinking blocks, sampling
//! specs, system-role messages — so a backend that supports a thing
//! loses nothing. The cross-cutting facts, either way:
//!
//! - **`thinking` and `redacted_thinking` blocks are DROPPED on
//!   replay** by the codex backend. Claude's reasoning cannot cross
//!   providers: the Responses reasoning items carry OpenAI's own
//!   encrypted reasoning (`include: ["reasoning.encrypted_content"]`,
//!   unit A's constant), and claude's thinking signatures verify
//!   against Anthropic's keys, so neither direction's reasoning is
//!   legible to the other. Replaying claude thinking as plain text
//!   would leak chain-of-thought the source protocol deliberately
//!   redacts. The drop is coherent both ways: the request direction
//!   drops thinking blocks, and the response direction synthesises
//!   **unsigned** thinking blocks from the codex summaries —
//!   display-only, never replayed (and anthropic-side replay of
//!   thinking needs a signature this translation never mints).
//! - **`stop_sequences`** — no Responses equivalent, so the codex
//!   backend drops it; the canonical carries it with the sampling
//!   specs for a backend that takes it. **`top_k`** and
//!   **`metadata`** (incl. `user_id`) have no canonical form (and no
//!   backend has asked for one — the pair module dropped them too).
//!   The thinking **request** crosses as the codex reasoning effort
//!   (see [`codex_backend`]).
//! - **`tool_result.is_error` / `cache_control`** and other block
//!   metadata — metadata, not content; the output text carries what
//!   the model needs.
//! - Unmapped **top-level fields** are dropped: cross-protocol
//!   translation maps the table above, not the raw-preservation rule
//!   ([`crate::ir`] keeps unknown fields for same-protocol routes; a
//!   foreign protocol has nowhere to put them).
//! - The release marker `$#$BURN$#$` rides through untouched —
//!   stripping is middleware's job
//!   ([`crate::ir::AnthropicBodyMut::strip_release`]), which runs
//!   before translation in the pipeline.
//!
//! ## The response direction (composed today: codex → anthropic)
//!
//! The mirrored layering — the backend adapter interprets, the
//! frontend adapter renders:
//!
//! - [`codex_backend::CanonStream`]: the backend adapter's
//!   interpretation half — the codex wire's
//!   [`ResponseEvent`](crate::providers::codex::ResponseEvent)s →
//!   the canonical turn model
//!   ([`CanonEvent`](crate::ir::canonical::CanonEvent) — the
//!   interpretation table lives in its module docs), plus unit A's
//!   [`TurnCapture`](crate::providers::codex::TurnCapture) → the
//!   canonical final turn
//!   ([`CanonTurn`](crate::ir::canonical::CanonTurn)) for the
//!   non-streaming path, and the error table's interpretation half
//!   (the upstream's `code`/`kind` → the canonical error).
//! - [`anthropic_frontend`]: the frontend adapter's rendering half —
//!   canonical turn events → Anthropic SSE (the
//!   [`AnthropicRenderer`](crate::translate::anthropic_frontend::AnthropicRenderer)
//!   block-index state machine), the canonical final turn → the
//!   complete non-streaming Message JSON, and the error table's
//!   rendering half (the canonical error kind → anthropic's
//!   `error.type`).
//! - [`to_anthropic`]: the composition of the two — the public entry
//!   the server calls (unit C):
//!   [`AnthropicStream`] fed
//!   [`ResponseEvent`](crate::providers::codex::ResponseEvent)s,
//!   [`message_from_capture`] over the capture, and the composed
//!   error mapping.
//!
//! The tables below describe the COMPOSED pair — what a codex turn
//! renders as on the anthropic wire.
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
//! Stop reasons, through the composition: any completed
//! `function_call` → `tool_use`; otherwise `end_turn`. Incomplete:
//! `max_output_tokens` → `max_tokens`, `content_filter` → `refusal`,
//! anything else → `max_tokens` (an incomplete turn stopped at its
//! budget — the only budget-shaped anthropic stop reason). No `ping`
//! events — claude tolerates their absence and nothing upstream
//! produces them.
//!
//! ### Usage table
//!
//! The canonical buckets (the backend fills them; `raw` rides the
//! backend's own usage object verbatim for whoever wants the wire's
//! shape) → the anthropic field names:
//!
//! | Responses | canonical bucket | Anthropic |
//! |---|---|---|
//! | `input_tokens` | `input` | `input_tokens` |
//! | `input_tokens_details.cached_tokens` | `cache_read` | `cache_read_input_tokens` |
//! | `input_tokens_details.cache_write_tokens` | `cache_write` | `cache_creation_input_tokens` |
//! | `output_tokens` | `output` | `output_tokens` |
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
//! Best-effort, first match wins. The table is split with the layers:
//! the interpretation half (upstream `code`/`kind` → the canonical
//! [`CanonErrorKind`](crate::ir::canonical::CanonErrorKind), plus the
//! message's stand-in chain resolved on the backend's own fields)
//! lives in [`codex_backend`]; the rendering half (canonical kind →
//! anthropic's `error.type`) lives in [`anthropic_frontend`].
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
//! placeholder says nothing the upstream did not; the chain resolves
//! backend-side, where the wire's own fields live).

pub mod anthropic_frontend;
pub mod codex_backend;
pub mod to_anthropic;
pub mod to_codex;

pub use anthropic_frontend::from_anthropic;
pub use codex_backend::codex_from_canonical;
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
