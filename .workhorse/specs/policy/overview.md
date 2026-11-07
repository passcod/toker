---
id: RPOL
---

# The routing policy

Every inference request is routed by the active revision of the routing policy, a versioned document held in the store.

There MUST be no other router.
A change to the policy MUST reach live traffic by activation, without restarting the service.

## Frontend scope

A frontend scope is a protocol, or a frontend profile together with a protocol.

A profile entry MUST take precedence over a protocol entry of the same kind.

## Naming a binding

A binding is named by its provider and dialect together.

Renaming one MUST be treated as a migration rather than a change of display.

## The compiled policy

Compilation closes a validated document against the route topology and one immutable offer inventory, and the result MUST be immutable.

A document MUST NOT carry an inventory winner.
A winner MUST be looked up when the policy is compiled, for a presentation, or when a request is resolved, for a candidate selector.

> [!NOTE]
> The same revision then follows the catalogue instead of freezing a model list at the moment it was authored.
