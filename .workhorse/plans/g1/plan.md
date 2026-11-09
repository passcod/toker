# G1: server safeguards

Carry Claude Code's server safeguards ask and its results through Toker, per the server safeguards spec (WSAFE), the provider extensions section of canonical rendering (WCANON), and the routing, ledger and dashboard hooks they name.
Both Messages response paths in `src/server/anthropic.rs` re-render through `AnthropicResponseStream` / `canonical_turn_from_anthropic` and `AnthropicRenderer` / `anthropic_from_canonical`, so the response half of extensions is the foundation everything else stands on.

## Verifying the Anthropic shape

The Anthropic bindings may declare server safeguards supported only once their request and streaming response shapes have been observed.
The store shows Claude Code already sends the `dangerous-tool-use-2026-09-03` beta on its Messages turns, but nothing records where `safeguard_results` sits in a response, complete or streamed.
Capture one auto-mode turn against a dev build on a spare loopback port, with a temporary dump outside the state dir that is deleted afterwards, and pin the shape as a fixture with invented contents.
Check that `/status` in Claude Code reads the auto mode server as enabled through the native route, and disabled after a turn served on `codex_sub`.

## Where the pieces are today

- Request extensions already replay across the shared Messages replay domain (`src/translate/anthropic_backend.rs`), so `safeguards` reaches both Anthropic and OpenRouter as it stands.
- `CanonEvent::ProviderEvent` and `CanonTurn::output_extensions` exist and are used by the Responses path; the Messages backend never fills them and the Messages frontend drops `ProviderEvent`.
- Request capability flags are merged with candidate requirements as hard skips in `src/policy/resolve.rs`; the safeguards requirement must instead be conditional on the route honouring it, with the second walk when it goes unmet.
- The rejection retry mirrors the thinking-between-tools retry in shape: one extra send, two rows, one attempt group.
- Adding the route field changes which policy documents parse, so the document format version moves with it.

## Notes

- **Response extension positions.** A complete response needs extensions at three levels: top-level message fields, unmodelled fields on a known block, and whole blocks of an unknown kind. `CanonTurn::blocks` is the ordered form, so an unknown block becomes an opaque entry in that sequence rather than a sibling list, which keeps its position. Top-level fields go in `output_extensions` with a wire path naming the message root.
- **Stream positions.** Unmodelled fields on a known event (`message_start`, `message_delta`, block start/delta/stop) ride on the canonical event they belong to; an event kind the interpreter does not know becomes `ProviderEvent` in arrival order. `IncomingBlock::Unknown` today swallows a whole unknown block's start, deltas and stop; it becomes opaque replay of each of those events at its index.
- **Index stability.** The renderer re-numbers content block indices; an opaque block replayed among modelled ones must take its index from the same counter, or the client sees gaps or collisions.
- **Recognition stays in the Anthropic adapter.** One function recognises the `safeguards` field, the `safeguard_results` field or event, and the paired beta (by its `dangerous-tool-use-` prefix). Nothing outside the adapter names them.
- **One retry per send.** The thinking retry and the safeguards retry both hang off the first 400. Fold them into one precommit step that inspects the refusal once and applies whichever rewrite it names; a second refusal is forwarded, whatever it names.
- **Format version.** Gate the route field to format version 4 the way v2 and v3 features are gated in `src/policy/document.rs` and `src/policy/canonical.rs`, with its own route fingerprint domain tag. A v3 document stays v3 and keeps its fingerprints, so existing pins survive until a route actually opts in.
- **Mark lifetime.** Clear on the same catalogue-change signal that clears circuits (`clear_binding_circuits` in `src/policy/runtime.rs`), on a credential edit for the provider, and from the control operation. The store row carries no generation, since generations are per process.

- **Late events.** The renderer closes a turn at `message_delta` and writes `message_stop` with it, so an event the provider sends between the two follows a closed turn and only the fields modelled on `message_delta` survive. Revisit once the observed response shape shows where `safeguard_results` sits.
- **Refusal naming the beta.** The refusal test accepts the beta value as well as the field name, since the API words a refused beta header differently from a refused body field; WSAFE says so.

## Checklist

### Response extensions (WCANON)

- [x] Add an opaque block variant for responses in `src/ir/canonical.rs`, and a way for known `CanonEvent`s to carry field extensions; keep the `Debug` impl content-free
- [x] `canonical_turn_from_anthropic`: retain unmodelled top-level fields, unmodelled block fields and unknown block kinds, tagged `anthropic_messages` with their wire position
- [x] `AnthropicResponseStream`: retain unmodelled fields on known events, replace `IncomingBlock::Unknown` dropping with opaque events per start/delta/stop, and emit unknown event kinds as `ProviderEvent`
- [x] Tag OpenRouter Messages responses `openrouter_messages` where the interpreter is shared, and share the replay domain check with the request side's `can_replay`
- [x] `anthropic_from_canonical` and `AnthropicRenderer`: replay same-domain extensions in position, with block indices from the renderer's own counter; never let an extension overwrite a modelled key
- [x] Chat and Responses frontends: omit Messages-domain extensions and report content-free losses (the Responses frontend already drops foreign `ProviderEvent`s; check it reports them)
- [x] `ResponseProjection`: confirm `is_reasoning_wire` leaves non-reasoning Messages extensions alone, with a test that `thinking: disabled` keeps a `safeguard_results` extension
- [x] Complete and streamed responses carrying an unknown top-level field, an unknown field on `message_delta`, an unknown block kind and an unknown event kind pass a Messages to Messages route in place (asserted on the parsed events rather than snapshotted, since the renderer zeroes `message_start` usage and echoes its own model)

### The pair and the binding capability (WSAFE, ROUTE)

- [x] Add `server_safeguards` to `Capabilities` in `src/routing.rs`: supported on the Anthropic sub and API Messages bindings, unknown on OpenRouter Messages, unsupported on every other protocol's binding; update the pinned-capability tests in `src/ir/canonical.rs`
- [ ] Capture and fixture the real response shape as described above, with the user's go-ahead before running a live turn, before flipping the Anthropic bindings to supported
- [x] Derive "asks for server safeguards" from the canonical request's `safeguards` extension, beside `required_capabilities` in `src/server/anthropic.rs`, as a separate fact rather than a `CapabilityFlags` member
- [x] Withhold the pair where effective support is unsupported: drop the `safeguards` extension in the licensed-transformation step and strip only the paired beta value from the outgoing `anthropic-beta` header, leaving the others
- [x] Translated routes: confirm the codex and Chat backends report the `safeguards` extension as an incompatible-dialect loss and never forward the beta

### Rejection retry and mark (WSAFE)

- [x] Migration: a `binding_safeguards_marks` table keyed by binding id with the time it was set; read-modify-write state, not ledger
- [x] Generalise `retry_thinking_off` into one precommit refusal step: on a 400 from a binding whose declared support is unknown, whose error names `safeguards`, record the refusal row, set the mark, and send once more with the pair withheld
- [x] Never retry or mark on a binding declared supported
- [x] Apply the mark when building `BindingAvailability` in `src/server/mod.rs`, narrowing unknown to unsupported for both the resolver and rendering
- [x] Clear the mark on a catalogue change for the binding, on a credential edit for its provider, and through the new control operation

### Routing (RDOC, RSOLV, RREV)

- [x] `Route` in `src/policy/document.rs`: an `honours_server_safeguards` flag, gated to format version 4, included in route semantics and the route fingerprint under a v4 domain tag
- [x] Compile the flag into the compiled route in `src/policy/compile.rs`
- [x] `RequestFacts`: add the ask; in the walk, treat it as a required capability only on an honouring route, skipping on effective unsupported and never on unknown
- [x] When that walk ends without a candidate, walk again without the requirement; carry the same rule into the post-send continuation
- [x] Trace events: a skip reason for lacking server safeguards, and a marker for the second walk, stored through `StoredRoutingTrace`
- [x] Preview: accept the ask in `PreviewRequest` and `RoutingPreviewRequest`, and report each candidate's effective support in the preview result
- [x] Policy edit operations and the TUI route form: set and clear the route flag

### Ledger (SLEDGR)

- [x] Migration: nullable request columns for the ask, the serving binding's effective support and whether it was marked, whether results were seen and whether they reached the client, and the routing outcome (skipped a candidate, walked again)
- [x] Fill them from the attempt plan and the response observer; results seen means the adapter's recogniser matched an extension, replayed means the renderer emitted it
- [x] Leave them null on rows for requests outside the Messages frontend and on imported rows; map them in export, and count them as deliberately unmapped in import

### Operator surfaces (DCTRL, UTUI)

- [x] Control: a `safeguards mark clear` mutation behind the usual operation, method and JSON checks, and the marks in the config snapshot
- [x] Providers page: each Messages binding's declared support and mark, with a key to clear the mark; update the page's key overlay and snapshots
- [x] Routing page: the ask as a preview input, and per-candidate support in the preview result

### Verification

- [x] Server tests against mock upstreams: Anthropic binding round-trips the pair and results unchanged; an unknown binding forwards the pair; a rejection retries once without it, writes two rows in one attempt group and marks the binding; a marked binding withholds the pair; a codex route never receives it
- [x] Resolver tests: honouring route skips an unsupported candidate, keeps an unknown one, walks again when nothing capable is reachable through the edges, and leaves a non-honouring route untouched
- [x] A test that no log line, ledger column or translation-loss report carries the contents of the field or of a result
- [ ] Live check in Claude Code: `/status` reads the auto mode server as enabled through an Anthropic route and falls back through `codex_sub`
