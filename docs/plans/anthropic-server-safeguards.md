# Anthropic server-side safeguards

Status: planned; implementation has not started.

## Purpose

Let Claude Code use Anthropic's server-side auto-mode classifier through toker
when the selected backend supports it. Claude Code then sends the action review
as part of the ordinary Messages request instead of issuing a separately billed
classifier request.

The feature is an Anthropic protocol capability, not a model-name heuristic.
Toker must preserve it without reading, logging, storing, translating, or
fabricating a safety verdict. A route to a backend that does not implement the
capability continues to receive no server verdict, and Claude Code may fall
back to its own classifier.

Anthropic documents the gateway contract at
<https://code.claude.com/docs/en/auto-mode-classifier-billing> and the generic
pass-through requirements at
<https://code.claude.com/docs/en/llm-gateway-protocol#feature-pass-through>.
The observed Claude Code 2.1.295 request pair is the
`dangerous-tool-use-2026-09-03` beta and the `safeguards` body field; response
support is carried in `safeguard_results`. These spellings are provider-owned
wire data, not a schema to freeze into routing policy.

## Protocol contract

Treat server safeguards as an atomic request and response capability:

- Preserve the incoming `anthropic-beta` values and the opaque `safeguards`
  request field together on compatible Anthropic Messages bindings.
- Preserve `safeguard_results`, including its streaming representation and
  placement, on the way back to an Anthropic Messages frontend.
- Preserve the tool-use ids to which verdicts refer. Never regenerate,
  normalise, or translate those ids on a same-dialect route.
- Do not replay safeguards extensions onto a different protocol dialect.
- Do not claim support merely because a backend accepts Anthropic-shaped JSON.
  A binding capability records verified upstream support.
- Do not synthesize an allow, deny, unsupported, or no-result verdict.
- Keep request and result contents out of logs and the ledger. Content-free
  presence, capability, and outcome-category instrumentation is permitted.

Unknown future fields and event kinds in the same provider-owned extension
must cross the canonical boundary opaquely. Recognition exists to associate
the request and response with a capability, not to make toker an interpreter
of Anthropic's safety policy.

## Canonical response extensions

Complete the response half of canonical extension preservation. Requests
already retain unmodelled top-level Messages fields and replay them onto a
compatible Messages binding; complete Anthropic responses currently discard
unmodelled top-level fields, and the streaming interpreter models only known
event fields.

Add dialect-tagged opaque response extensions for:

- unmodelled fields on a complete response;
- unmodelled fields attached to known streaming events;
- provider-owned streaming event kinds that have no portable semantic form.

Retain their wire location and ordering information needed for deterministic
same-dialect replay. A compatible frontend renderer reproduces them in their
original structural position. Other frontend dialects report a content-free
translation loss and omit them. Existing modeled text, thinking, tools,
errors, stop reasons, and usage remain authoritative and cannot be overwritten
by an opaque extension.

The `thinking: disabled` response projection applies only to reasoning wire
material. It must not remove safeguard results or unrelated provider-owned
events.

## Binding capabilities

Add a `server_safeguards` capability to the verified binding description.
Initial support is enabled only for Anthropic API and subscription bindings
after their request and streaming response shapes have been observed. An
OpenRouter or future Messages binding remains unknown or unsupported until
verified independently.

Capability state is distinct from temporary availability. A provider error or
a response with no verdict does not rewrite the binding's declared support.
Claude Code owns its fallback and repeated-no-verdict behavior.

Translated Codex, OpenAI, and other non-Anthropic bindings do not receive the
request extension and cannot return the result extension. Toker must keep the
header/body pair coherent when rendering those requests: it must not forward
the safeguards beta while dropping its paired body field.

## Routing integration

Expose the presence of a safeguards request as a required route capability.
Integrate it with the candidate capability mechanism in
[`live-model-routing-implementation.md`](live-model-routing-implementation.md)
rather than adding a separate classifier-specific router.

A policy may require `server_safeguards` and select an Anthropic candidate for
the entire model turn. This is explicit because the server check is embedded
in that turn; toker cannot send only the classifier portion to Anthropic while
serving the model response from Codex. Candidate selection and fallback must
also respect the route's subscription-to-billed boundary rules.

Without such a policy, normal model routing wins. A safeguards-bearing request
routed to `codex_sub` therefore falls back inside Claude Code to its separately
billed classifier. Users who intentionally never route these turns to a
supporting backend can set `CLAUDE_CODE_AUTO_MODE_SERVER=0`; toker does not
silently write that frontend setting because a later live policy may make the
capability available.

## Observation and operator surfaces

Record only content-free facts needed to explain behavior:

- whether the frontend requested server safeguards;
- whether the selected binding declared support;
- whether a result extension was observed and replayed;
- whether capability selection changed the chosen candidate.

The TUI and route preview should distinguish `supported`, `unsupported`, and
`unknown`. They must not display or retain a verdict or the action being
classified. Translation-loss reporting continues to show a dropped extension
by path and reason only.

## Completion criteria

The implementation is complete when:

- canonical Messages request and response adapters preserve provider-owned
  extensions symmetrically for complete and streaming traffic;
- safeguards beta/body coherence is enforced on every binding;
- verified Anthropic bindings round-trip `safeguards`, `safeguard_results`,
  event ordering, and referenced tool-use ids without semantic mutation;
- translated bindings neither receive nor fabricate the capability;
- binding and routing capability state can select an explicitly configured
  safeguards-capable candidate through the live routing policy;
- the ledger and logs contain only the content-free observation fields above;
- Claude Code reports its auto-mode server as enabled through a supported
  native route and falls back normally through an unsupported translated
  route.
