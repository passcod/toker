# Protocol and provider mux

Status: guiding architecture; phase 1 complete, canonical migration and first
live cross-protocol cutover underway.

Toker's purpose is not a fixed set of frontend-to-backend pairs. It is a mux:
any configured frontend protocol should be able to reach any backend for which
toker has a complete canonical adapter. A nominal protocol match is not a
different execution path: Messages to Messages is a translation through the
same canonical model as Messages to Responses.

The initial implementation proved the individual routes, but still names many
of them as vendor pairs. That makes `codex_sub` look like the Responses
protocol and `openrouter` look like the OpenAI Chat protocol even though those
are independent facts. OpenRouter already demonstrates the problem: one
provider exposes both OpenAI Chat and Anthropic Messages, and may expose a
Responses dialect as well.

This document is the guide for separating those concerns. During migration,
`AGENTS.md` and `docs/internals/` continue to govern the running byte-forwarding
implementation. The cutover to universal canonical rendering must update those
current-code rules in the same commit that removes the old path; a plan cannot
pretend already-running code has changed.

## The layers

```text
frontend profile
    -> frontend protocol adapter
    -> canonical request and events
    -> shared middleware
    -> backend protocol adapter
    -> provider binding
```

Each layer owns one kind of fact.

### Frontend profile

A frontend profile describes a configured client, not a wire protocol. It
owns:

- the stable name carried by `/f/<name>`;
- the inference protocol the configured client speaks;
- the catalogue shape it expects from its models endpoint;
- its notice style and any client-specific presentation facts.

Inference paths remain authoritative. A request to `/v1/messages` is an
Anthropic Messages request even if it arrives under a surprising frontend
name. The profile matters where the path is ambiguous: both OpenAI Chat and
Responses clients use `/v1/models`, but do not necessarily consume the same
catalogue shape.

An unprefixed or unknown frontend has no inferred profile. It keeps the current
path-driven behavior rather than being guessed into a known client.

### Protocol adapters

A protocol is an identity such as `anthropic_messages`, `openai_chat`, or
`openai_responses`. In Rust it is a closed `ProtocolId` value. Behavior belongs
to adapters rather than to the identity itself.

A frontend adapter:

- parses every request into canonical IR;
- interprets every frontend event or body into canonical events;
- renders canonical response events, errors, and synthetic notices;
- renders the frontend's model-catalogue shape.

A backend adapter:

- renders canonical requests onto its wire;
- interprets response events into canonical events;
- declares live-verified capabilities and translation costs.

One implementation may serve both sides of a protocol, but the roles stay
separate. There is no identity or passthrough adapter: even equal nominal
protocols parse and render. This matters when a provider implements a dialect
rather than every feature of a nominal protocol.

### Provider bindings

A provider is a vendor/account boundary. It owns:

- endpoint mapping;
- authentication and foreign-credential removal;
- quota meters and cost semantics;
- its catalogue source and parser;
- the protocol bindings that have been verified against it.

A binding says that one configured provider can be reached using one backend
adapter. It is deliberately narrower than "the provider has an endpoint with
this spelling". A Responses endpoint is a binding only after representative
Codex requests, streaming events, tools, errors, and usage have been verified.

Capabilities belong to a binding, dialect, or model offer, not to a vendor in
the abstract. A provider can support a parameter on one protocol and reject it
on another; a routed model can be narrower than the provider's endpoint.

## Routing

Routing resolves to a concrete target:

```text
ModelTarget {
    provider,
    backend binding,
    effective model,
}
```

The requested model may carry an outer provider prefix. For example,
`openrouter/anthropic/claude-sonnet-x` selects the `openrouter` provider and
leaves `anthropic/claude-sonnet-x` as the provider's model id. Provider ids and
model ids are never conflated.

The routing decision always selects a backend adapter. The frontend adapter has
already produced canonical IR; shared middleware changes that IR; the selected
backend adapter renders it. The response always takes the reverse path through
canonical events. A route exists only when both halves are implemented and
tested.

```text
frontend bytes
    -> canonical request
    -> middleware
    -> model target
    -> backend bytes

backend bytes/events
    -> canonical events
    -> observation
    -> frontend bytes/events
```

There is no same-wire fast path. Protocol equality can let two bindings reuse
adapter code, but it never bypasses canonical IR.

## Canonical fidelity and explicit loss

Toker is a semantic mux, not a transparent proxy. Its contract is that the
selected backend can accept the rendered request and the frontend can accept
the rendered response. Byte equality with either side's original wire is not a
goal.

Canonical IR carries all semantics toker understands plus opaque extensions:

```text
CanonicalRequest {
    model,
    system,
    messages,
    tools,
    sampling,
    thinking,
    extensions,
}

CanonicalExtension {
    source dialect,
    wire path,
    optional known semantic kind,
    opaque value,
}
```

An adapter may render a known extension, replay an opaque extension whose
dialect it understands, omit it with an explicit loss, or reject when omission
would make the request invalid. Unknown content blocks are never silently
flattened or truncated. Unknown optional fields may be omitted, but their field
paths and loss reason are observable without storing their values.

This matters even where an upstream would accept the omission: cache controls,
reasoning effort, tool choice, thinking signatures, and encrypted reasoning can
change behavior without producing an error.

Translation produces a content-free loss report beside the rendered request or
response. It may record field paths, block kinds, counts, and digests, never
prompt, completion, tool, or opaque extension values.

## Determinism replaces byte round-tripping

Prefix stability remains mandatory, but its reference is the backend rendering,
not the frontend's bytes. For an append-only conversation, rendering turn N+1
must reproduce turn N's backend prefix byte-for-byte. Backend adapters are pure
functions of canonical input, target binding, and explicit configuration; they
do not read clocks, meters, or mutable state.

The migration causes one real cache rebuild per existing lane whose new
canonical rendering differs from the old forwarded body. After that bounded
cutover, deterministic rendering keeps the new backend prefix stable. The
migration must measure this effect against a copied ledger and should not be
deployed casually into many warm sessions.

The old byte-round-trip corpus remains useful while migrating ingress parsers,
but it stops being the acceptance criterion. The replacement suite pins:

- canonical semantics extracted from captured requests;
- deterministic request rendering and append-only prefix stability;
- canonical event interpretation and frontend rendering;
- explicit translation-loss reports;
- representative backend and frontend acceptance;
- unknown fields and events staying quiet unless they affect compatibility.

## The models endpoint is a frontend projection

Toker owns frontend model discovery. It must not proxy `/v1/models` to whichever
backend happens to be the default.

Provider catalogues are normalized into offers while retaining their raw entry
and evidence:

```text
ModelOffer {
    provider,
    backend model id,
    routed model id,
    protocol bindings,
    context window if declared,
    capabilities if declared,
    catalogue provenance and age,
}
```

For one frontend, toker joins:

```text
enabled provider catalogues
    JOIN verified backend bindings
    JOIN available translation paths
    JOIN frontend compatibility
    -> frontend-visible model catalogue
```

The frontend adapter renders that normalized result in the client's expected
shape. The join follows these rules:

- every offer has a provider-qualified routed id;
- the configured default may additionally have bare aliases for compatibility;
- equal model slugs from different providers never merge;
- an offer is visible only when a complete route exists;
- context windows, prices, parameters, and capabilities remain unknown when
  their evidence is absent;
- stale provenance remains visible internally and is never converted into a
  fresh claim;
- provider raw catalogue entries are retained for later renderers but never
  copied blindly between incompatible catalogue shapes.

This makes the models endpoint a materialized view of the route graph. Adding a
binding can make existing catalogue entries reachable without adding another
frontend-specific picker pipeline.

## Rust direction

The closed identities begin as ordinary data:

```rust
enum ProtocolId {
    AnthropicMessages,
    OpenAiChat,
    OpenAiResponses,
}

struct FrontendProfile {
    name: String,
    protocol: Option<ProtocolId>,
}

struct BackendBinding {
    protocol: ProtocolId,
    dialect: DialectId,
    adapter: BackendAdapterId,
    capabilities: Capabilities,
}
```

Adapters become traits when more than one binding needs runtime dispatch. The
first refactor must not introduce boxed async streams or erase the existing
typed state machines merely to make the diagram look complete. Stateful stream
interpreters and renderers may remain concrete objects created by a small
object-safe adapter factory.

The provider trait grows binding declarations first. Endpoint, auth, meter,
and cost behavior remain on the provider. Later, a binding can carry a typed
dialect/adapter id and live-verified capabilities without changing provider
identity.

## Invariants through the refactor

- Every request and response goes through canonical IR or canonical events.
- Every backend and frontend rendering is pure and deterministic.
- A new binding does not imply support until live verification says it does.
- Provider auth stays provider-owned; protocol code never handles credential
  values.
- Translation losses are content-free and visible, never silently asserted as
  support.
- Accounting failure loses accounting, never a request; translation failure is
  a compatibility error and reaches no backend.
- The catalogue join never guesses missing facts.
- Frontend profile names affect presentation and ambiguous discovery, never
  reinterpret a usage path's wire protocol.

## Phases

### 1. Name the graph without changing it

- Add `ProtocolId`, `FrontendProfile`, and `BackendBinding`.
- Make configured frontend definitions return `ProtocolId` rather than string
  literals.
- Carry a resolved frontend profile in the `/f/<name>` request extension.
- Replace the Codex-name check on `/v1/models` with the profile's protocol.
- Make each provider declare its currently implemented protocol bindings.
- Pin the declarations and existing routing behavior with tests.

No endpoint, body, credential, catalogue, or ledger behavior changes.

### 2. Make canonical IR universal

In progress. The Messages adapter is complete in both directions. OpenAI Chat
request ingress, deterministic OpenRouter Chat request egress, and streaming
and complete OpenRouter response interpretation are implemented. OpenAI
Responses request ingress now preserves messages, tools, named reasoning
effort, extensions, and opaque provider reasoning items canonically. The Codex
Responses backend replays compatible request extensions through that IR, with
deterministic and append-prefix-stable rendering. Its frontend adapter now
renders canonical streaming events and complete turns back to Responses,
including provider-encrypted reasoning. Provider-owned Responses input items
also cross the canonical boundary opaquely and replay to compatible bindings,
so protocol evolution does not make same-dialect routes brittle. The Messages
and OpenRouter Chat provider bindings declare canonical readiness, and the
Chat frontend renders canonical streaming and complete responses. The Codex
Responses handler and every Chat route now use those adapters live; the
remaining Messages provider routes stay on the legacy path until phase 3 cuts
each one over.

- Extend canonical requests and events for every semantic shape the three
  current frontend protocols carry.
- Add opaque extensions and content-free translation-loss reports.
- Implement ingress adapters for Messages, Chat, and Responses.
- Implement deterministic egress adapters for the currently live provider
  bindings.
- Pin semantic fixtures, rendered bytes, event streams, and prefix stability.
- Keep the old handlers live until both sides of a route are complete.

### 3. Resolve concrete model targets and cut routes over

Underway: OpenAI Chat to `codex_sub` was the first live canonical route. It can
be selected as the Chat default or per request with `codex_sub/<model>` and
translates request, streaming response, complete response, errors and usage.
OpenAI Responses to `codex_sub` now takes the same path despite matching wire
protocols, including compatible extension replay and encrypted reasoning.
OpenAI Chat to OpenRouter has also moved: requests and streaming, complete, and
error responses cross canonical IR while accounting observes OpenRouter's
provider bytes before translation.

- Replace per-handler provider selection with a route registry.
- Add a `ModelTarget` carrying provider, binding, requested model, and effective
  model.
- Move routing, gates, rewrites, and shape extraction onto canonical middleware.
- Cut one complete request-and-response route at a time onto the common
  pipeline.
- Remove the old route only after its canonical replacement passes acceptance
  and quietness tests.
- Once every usage route has moved, remove fidelity drift and atomically update
  `AGENTS.md` plus `docs/internals/` to the universal-rendering invariant.

### 4. Own model discovery

- Normalize the existing fetched catalogues into `ModelOffer`s.
- Join offers against the route graph per frontend profile.
- Render `/v1/models` locally for OpenAI Chat and Codex Responses clients.
- Move Claude's picker generation onto the same joined source.

### 5. Add the OpenRouter Responses binding

- Live-verify a representative Codex workload against OpenRouter Responses.
- Declare the binding only for verified request and event shapes.
- Route `openrouter/<model>` through the universal canonical pipeline.
- Record only provider-attested billed cost and serving-provider fields.

### 6. Complete the adapter matrix

- Finish any adapter capabilities the first live route did not exercise.
- Compose Codex-to-Anthropic API traffic through the same canonical pipeline.
- Add Anthropic subscription signing before exposing that route to frontends
  that do not bring Claude's bearer.
- Add OpenAI Chat canonical adapters for the remaining opencode routes.

The matrix is complete by implemented protocol paths, not by enumerating every
frontend-product and provider-vendor pair.
