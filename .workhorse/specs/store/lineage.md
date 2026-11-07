---
id: SLIN
---

# Session lineage

Some requests belong to a session without being its conversation, such as a sub-session a client opens beside the main one.
This spec covers the two identities a request carries, what may establish a relation between them, and what the relation may be read for.

Toker MUST record such a relation only where the wire states it, and MUST NOT infer one.

## Two identities

A request's raw session is the identity it arrived with: the first of the configured session headers it carries, or, on a protocol that supplies one, the body's prompt cache key.

The raw session MUST be stored whole, and MUST be the key the lane table, the allowances and the prompt cache use.

> [!NOTE]
> A cache belongs to one conversation's prefix, and two sessions never share one. [SLANE](lanes.md) has the consequences.

A display parent is a second, optional identity, carried with the kind of child it is and the evidence that established it.
Those three values MUST be all present or all absent.

A parent MUST NOT replace the raw session.

> [!NOTE]
> A child shown under its parent still has its own lane, its own cache and its own allowance.

## Admissible evidence

Toker MUST recognise only a relation a client states in a field its protocol defines for the purpose.

Where a session header carries a recognised prefix naming a kind of child, followed by the session it belongs to, Toker MUST record that relation, keeping the whole header value as the raw session and the suffix as the parent.

Every one of these conditions MUST hold:

| condition |
| --- |
| the request came in on the frontend profile that defines the convention |
| the identity came from the session header, not from the body |
| the suffix naming the parent is not empty |
| the role and evidence are a pair Toker recognises |
| the parent differs from the child |
| the relation does not close a cycle through rows already stored |

A reader MUST re-check those conditions, so that a stored value violating them fails the read rather than being displayed.

A parent with no row of its own MUST be accepted.

> [!NOTE]
> The relation is kept so a display can resolve it later, and until then the child shows under the parent's identity.

A relation MUST ride only the rows of requests that carried it.
A row Toker writes about itself, such as a notice or a release, MUST NOT carry one.

### Inadmissible evidence

None of these MUST be treated as evidence of a relation: when a request arrived, which model it named, how many messages or tools it carried, whether a transcript exists for it, which connection it used, or its body's contents.

> [!NOTE]
> Each is a guess that is right until the day it is wrong, and a wrong lineage is worse than none, because it moves one conversation's tokens and cost into another's.

A request carrying a session identity and no parent identifier MUST be a session of its own, however much it resembles a side call of another.

A header naming a request's class MAY label a role, and MUST NOT establish a parent unless the wire also supplies a relation that can be verified.

### Routing never reads the role

Policy MUST resolve a request from content-free facts alone, and MUST NOT recognise a request by its role or by its prompt's contents.

> [!NOTE]
> A client's classifier, title and helper calls are therefore routed exactly as any other request naming the same model is.

## Lineage is for display and accounting only

Routing MUST read the raw session.

The resolver, the lane pin, the gates and the allowances MUST NOT read the parent.

> [!NOTE]
> A child session therefore has its own lane, goes cold on its own clock, and is gated on its own allowances.

A display MAY group a child's rows under its parent, so that the parent's request count, token totals and spend include its linked children, and the parent names its children by role.

Lanes MUST stay keyed by raw session even where rows are grouped.

> [!NOTE]
> A child's large prompt would otherwise become the parent's context size.

A display session MUST read its main lane from its own raw session where it has one, and MUST fall back to a child's lane only where the parent has none.

A child's title and working directory MUST be resolved through the parent's identity.

A query answering a client's question about a session MUST match that session exactly.

> [!NOTE]
> A client asking about a session gets that session, not its children.

Rows written before lineage was recorded carry none, so a session that was a child before then shows as its own.
That is an absence of evidence rather than evidence of absence, as [SLEDGR](ledger.md) requires of every such field.
