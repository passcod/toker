# G1: server safeguards

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
