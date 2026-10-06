# Protocol and provider mux

Status: guiding architecture; phase 1 complete.

Toker's purpose is not a fixed set of frontend-to-backend pairs. It is a mux:
any configured frontend protocol should be able to reach any backend protocol
for which toker has either a compatible same-wire binding or a complete
translation path.

The initial implementation proved the individual routes, but still names many
of them as vendor pairs. That makes `codex_sub` look like the Responses
protocol and `openrouter` look like the OpenAI Chat protocol even though those
are independent facts. OpenRouter already demonstrates the problem: one
provider exposes both OpenAI Chat and Anthropic Messages, and may expose a
Responses dialect as well.

This document is the guide for separating those concerns without weakening the
byte, credential, accounting, or absence invariants in `AGENTS.md`.

## The layers

```text
frontend profile
    -> frontend protocol adapter
    -> protocol-local IR or canonical IR
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

- reads routing and observation metadata from a request;
- converts a cross-protocol request into canonical IR;
- renders canonical response events, errors, and synthetic notices;
- renders the frontend's model-catalogue shape.

A backend adapter:

- renders canonical requests onto its wire;
- interprets response events into canonical events;
- declares live-verified capabilities and translation costs.

One implementation may serve both sides of a protocol, but the roles stay
separate. This matters when a provider implements a dialect rather than every
feature of a nominal protocol.

### Provider bindings

A provider is a vendor/account boundary. It owns:

- endpoint mapping;
- authentication and foreign-credential removal;
- quota meters and cost semantics;
- its catalogue source and parser;
- the protocol bindings that have been verified against it.

A binding says that one configured provider can be reached using one protocol
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

The routing decision then chooses one of two paths.

### Same-wire path

When the frontend protocol and backend binding are compatible, the client's
body remains the forwarded body. Toker parses only protocol-local, lossless
views for routing, gates, and observation. With no licensed rewrite it forwards
the original buffer. A provider-prefix strip serialises the protocol-local IR
once, deterministically, under the existing body-rewrite licence.

Canonical IR never enters this path.

### Cross-protocol path

When the protocols differ, the frontend adapter parses into canonical IR and
the backend adapter renders from it. The response takes the reverse path.
Translation is pure and its losses are declared by the target binding's
capabilities.

A route exists only when both halves are implemented and tested. There is no
best-effort translation that silently drops unknown content.

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
}
```

Adapters become traits when more than one composition needs their behavior.
The first refactor must not introduce boxed async streams or erase the existing
typed state machines merely to make the diagram look complete. The route
registry can initially name the existing handlers and acquire adapter objects
as translation seams are generalized.

The provider trait grows binding declarations first. Endpoint, auth, meter,
and cost behavior remain on the provider. Later, a binding can carry a typed
dialect/adapter id and live-verified capabilities without changing provider
identity.

## Invariants through the refactor

- Same-protocol requests retain the original-buffer fast path.
- Cross-protocol serialisation remains pure and deterministic.
- A new binding does not imply support until live verification says it does.
- Provider auth stays provider-owned; protocol code never handles credential
  values.
- Observation failure loses accounting, never a request.
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

### 2. Resolve concrete model targets

- Replace per-handler provider selection with a route registry.
- Add a `ModelTarget` carrying provider, binding, requested model, and effective
  model.
- Generalize provider-prefix stripping across protocol-local IRs.
- Keep existing defaults and error shapes byte-pinned.

### 3. Own model discovery

- Normalize the existing fetched catalogues into `ModelOffer`s.
- Join offers against the route graph per frontend profile.
- Render `/v1/models` locally for OpenAI Chat and Codex Responses clients.
- Move Claude's picker generation onto the same joined source.

### 4. Add the OpenRouter Responses binding

- Live-verify a representative Codex workload against OpenRouter Responses.
- Declare the binding only for verified request and event shapes.
- Route `openrouter/<model>` from the Responses frontend through the
  same-wire path.
- Record only provider-attested billed cost and serving-provider fields.

### 5. Complete the adapter matrix

- Add a Responses frontend adapter and an Anthropic Messages backend adapter.
- Compose Codex-to-Anthropic API traffic through canonical IR.
- Add Anthropic subscription signing before exposing that route to frontends
  that do not bring Claude's bearer.
- Add OpenAI Chat canonical adapters for the remaining opencode routes.

The matrix is complete by implemented protocol paths, not by enumerating every
frontend-product and provider-vendor pair.
