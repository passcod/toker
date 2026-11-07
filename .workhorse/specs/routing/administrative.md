---
id: ADMIN
---

# Administrative paths

Some paths are not inference: they list models, count tokens, or create and read batches.
This spec covers how each is answered.

## Model listings

A `/v1/models` response MUST be projected from the active policy and the offer inventory, and MUST NOT be a provider's listing passed through.

Each model listed MUST have a complete route to the profile that asked for it.
Where no usable catalogue has been loaded, or a presented offer is absent from the inventory, the endpoint MUST answer 503 rather than an empty list.

What each protocol's listing contains is specified by [RPUB](../policy/publication.md).

## Token counting and batches

Token counting and batch creation MUST resolve only their first candidate, and MUST require a binding that declares the operation.

Toker MUST retain the client's bytes on these requests, and MUST replace only the model at the positions each operation documents.
A batch whose requests resolve to more than one binding MUST be refused with a 400.

Batch reads, batch cancellation and unmatched administrative paths MUST stream through without inference observation.
