---
id: GATE
---

# Gates

A gate stops a request Toker would otherwise forward.
This spec covers what may stop a request, what a gate reads, and how the gates relate to the measurement and detection that feed them.

## Only a gate stops a session

Only a gate MUST be able to stop a session.

Observing and recording a request MUST NOT be able to fail it.

> [!NOTE]
> Observation rides a byte stream that has already been forwarded, so a parser failure loses a measurement rather than a request.

Every gate decision MUST be taken at one place.

> [!NOTE]
> A request dropped anywhere else is a defect, and one place to look for the decisions makes it findable.

There are two gates.

| gate | stops a request that |
| --- | --- |
| quota | would spend past a subscription's rate limit window |
| cold cache | would re-read a prompt cache that has expired |

The quota gate is specified by [GQUOTA](quota.md), and the cold-cache gate by [GCOLD](cold.md).

## A gate reads a resolved request

Policy MUST resolve a request's route before any gate sees it.

A gate MUST read the resolved binding and effective model, and MUST NOT route.

A gate MUST decide before the request is forwarded.

## A gate's answer

A gate MUST answer with a notice rather than an error, as [GNOTE](notices.md) specifies.

A notice enters the conversation and is replayed on every later turn, so its bytes are fixed.

## Evidence a gate reads

A gate decides from evidence produced elsewhere.
A spec that supplies such evidence MUST NOT stop a request itself.

## Arming

Each gate MAY be armed or disarmed by configuration.

Whether a gate was armed MUST be recorded on each request's row.

> [!NOTE]
> The toggles live in configuration, which a later reader of the store cannot see.
