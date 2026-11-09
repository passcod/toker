---
id: RDOC
---

# The policy document

A policy is one document holding the collections that describe every route Toker can take.
This spec covers those collections, how a document is parsed and fingerprinted, and what makes one valid.

How a document becomes the active revision is specified by [RREV](revisions.md), and how a request is resolved against it by [RSOLV](resolver.md).

## Collections

A policy holds eight collections.

| collection | what it holds |
| --- | --- |
| defaults | one route per frontend scope, for a name nothing else matched |
| presentations | the models a profile is shown |
| routes | named ordered chains of candidates, with the edge granting each fall to the next |
| rewrites | matchers on the requested model that pick a route, for names never presented |
| classifiers | per binding, how a model identifier splits into a family and a version |
| upgrades | which newer version of a candidate's family a request may move to |
| compaction | where a cold compaction goes |
| description | free text |

A collection scoped to a frontend follows the precedence [ROUTE](overview.md#frontend-scope) specifies, and names a binding as [ROUTE](overview.md#naming-a-binding) specifies.

Every reference a collection makes MUST resolve within the document.

A rewrite's priority MUST be unique within its scope.
A default MUST be unique within its scope.

A classifier and an upgrade policy that work together MUST name the same binding.

## Parsing

The parser MUST refuse unknown fields, duplicate keys and non-integer numbers before typed parsing.
Identifiers MUST be bounded in length and drawn from a restricted character set, and the document, its nesting depth, its collections and its strings MUST each be bounded in size.

Because unknown fields are refused, adding even an optional field changes which documents are accepted, so the format MUST carry a version.
A document using a feature of a later version than it declares MUST be refused.

## Fingerprints

A document's fingerprint MUST be a cryptographic hash over a version-specific domain tag and a canonical encoding, with object keys sorted and arrays in their semantic order.
Each format version MUST have its own domain tag, so that one fingerprint never means two things.

A route fingerprint MUST cover one route's semantics alone: its candidates, edges, alias policies, health circuits, upgrade policies, classifiers, lane transition policy and whether it honours server safeguards.
Labels, descriptions and the offer an inventory happens to select MUST NOT take part.

> [!NOTE]
> A catalogue refresh must not change a route fingerprint, because lane pins carry it and every pinned lane would otherwise be invalidated whenever a provider published a model.

## Routes and candidates

A route declares its parameters, a first candidate, and steps that each pair an edge with the candidate it leads to.
A route MAY declare that it honours server safeguards, as [RSOLV](resolver.md#honouring-server-safeguards) specifies.
One parameter name is reserved for the model the client requested.

Every reference to a route MUST supply one value for each declared parameter, being a literal, the requested model, or a capture from the matcher that selected the route.
A capture MUST be supplied only by a rewrite whose matcher can produce one.

A candidate's model MUST be an exact identifier, the requested model, a template over the route's parameters, or a selector that keeps exactly one winner.

A candidate MAY declare an upgrade policy, an alias policy applied after the upgrade, the capabilities it requires, whether it is enabled, and the failures that open a circuit against it.

An alias rule's matcher MUST be matched against the model's [canonical identity](models.md#identities-are-exact), so that a rule naming an undated model also covers its published dated snapshots.

A candidate's required capabilities MUST be within those its binding declares.
A candidate's binding MUST be reachable from every scope the route is used in.

## Classifiers

A classifier names a binding, a matcher, a template for the family and one for the version, both over the matcher's captures, and the parser that reads the version.

A classifier's matcher MUST be matched against the model's [canonical identity](models.md#identities-are-exact).
An identity the matcher does not match, or whose version the parser cannot read, has no family.

A version parser MUST be one of these.

| parser | reads |
| --- | --- |
| semver | a semantic version |
| date | a release date, in one of two spellings |
| numeric tuple | numeric segments joined by a separator, either exactly a declared count of them or any count up to a bound |

A declared count MUST be within that bound, and a separator MUST NOT be empty.

How classified versions are compared, and how a family's target is elected, is specified by [RMODEL](models.md#electing-a-familys-target).

## Presentations

A presentation names a slug a profile is shown, and sources it as one exact offer on one binding, as a selector over a binding's offers, or as a virtual model backed by a route.

A selector MUST be expanded when the policy is compiled, so that a selection tracking the newest matching offer follows the inventory as catalogues refresh.
An identifier a classifier cannot parse is not an older version, and MUST NOT be selected where the ordering is by classified version.

A slug MUST be unique within its scope.
A slug equal to a native model identifier reachable from that profile MUST be refused unless the document consents to shadowing it.

> [!NOTE]
> Without that consent a presented name silently hides a real model of the same name.

## Validation

Validation compiles a document, being the draft or the result of an edit not yet saved, against one captured inventory and returns diagnostics, each carrying a code, a severity, the record's identifier and a path.

A diagnostic's path MUST be built only from static field names and bounded indices, so that a request or a model string can never appear in one.

Validation MUST report every requirement of this spec the document fails to meet.

An error MUST block activation.

Provider configuration and catalogue presence are evidence rather than validity.
An unconfigured binding, an offer absent from the inventory and an unresolved selector MUST be reported without blocking.

> [!NOTE]
> A fallback chain is legitimately prepared before the provider serving it exists.

## Edge rules

An edge says whether a fall to the next candidate is allowed before the request is sent, which classified failures allow it once the request has been sent, and which consequences it consents to.

A fall from a subscription binding to one that is billed, or whose cost is unknown, MUST require the edge's consent to crossing that boundary.
An edge listing an ambiguous transport failure among those it falls on MUST require consent to that separately.

> [!NOTE]
> An ambiguous transport failure leaves open that the provider accepted and billed the request, so falling to another provider may pay twice.

An edge crossing a cache identity MUST NOT declare that it preserves a warm cache.

