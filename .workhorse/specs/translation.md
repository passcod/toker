---
id: TRANS
---

# Translated routes

A translated route carries a request to a provider that speaks a different protocol from the client.
This spec covers which routes translate, what a translation may and may not depend on, and how a capability the target lacks is resolved.
The canonical request every translation reads from is specified by [CANON](canonical.md).

## Routes that translate

A route whose binding's protocol differs from the client's MUST translate between the two.

A route whose binding's protocol matches the client's MUST render canonically all the same.
Matching protocols MAY let a route replay the provider's own opaque extensions.
Matching protocols MUST NOT let a route bypass canonical rendering or canonical event handling.

Every other stage of the request pipeline MUST run before translation, so that the translated request carries the model the route finally resolved on.

## Purity

A translation MUST read no clock and no stored state.
A translation MUST take the model identifier and the prompt cache key as inputs rather than resolving them itself.

A translated conversation that only appends MUST reproduce its earlier upstream bytes exactly, as [CANON](canonical.md#rendering) requires of any rendering.

The first turn an existing conversation sends over a translated route rebuilds the upstream's cache once.

> [!NOTE]
> The upstream's prefix genuinely changed, because the conversation's earlier turns were never sent in this protocol.

## Resolving a capability the target lacks

Where the target binding declares a semantic unsupported, the translation MUST resolve it rather than send it and let the provider refuse.

| semantic the target refuses | resolution |
| --- | --- |
| a system role among the conversation's items | merge the message into the user turn before it |
| sampling parameters | drop them |
| another provider's reasoning blocks | drop them |

Which semantics a binding can represent is declared by the binding, as [ROUTE](routing.md#bindings) specifies.

## Opaque replay

A translation MUST replay a provider's own input items, tool kinds and response items opaquely through the canonical request, without interpreting their contents.
This MUST include encrypted reasoning and incremental tool declarations.

A terminal end-of-turn flag on a provider's own response item MUST remain authoritative even where that item has no portable tool-call shape.

> [!NOTE]
> Opaque replay carries a newer client shape to a compatible provider without Toker being taught the shape's meaning.

Where a translation cannot carry a field, it MUST report the loss without reproducing the field's contents.
A translation MUST refuse a tool collection or an input item it does not recognise, rather than send it stripped.

## Reasoning effort

Where a Messages request carries an explicit thinking budget and the target expresses reasoning as named tiers, the translation MUST map the budget onto the target's documented tiers.

Where a Messages request leaves thinking adaptive, the translation MUST leave the target's effort at the model's default.
Where the client supplied a named effort alongside adaptive thinking, the translation MUST carry that tier across unchanged.

## Errors across a translation

A provider may report an error code as a number or as a symbol.
An error parser MUST accept both, so that the accompanying message survives into the client's protocol whether it arrived over HTTP or in an event stream.

An error a provider returns MUST be interpreted canonically and rendered into the client's protocol, as any other response is.
