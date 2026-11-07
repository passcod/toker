---
id: UCLI
---

# The command surface

Toker is one binary offering the service, the dashboard, the setup wizard and several one-shot verbs.
This spec covers what each command is for and which store it speaks to.

## Parsing

Every flag MUST be parsed strictly, so that a mistyped flag fails rather than being ignored.

A command taking a duration or a timestamp MUST parse it strictly and MUST reject a value it cannot interpret, rather than substitute a default.

## The commands

| command | what it does |
| --- | --- |
| serve | run the proxy |
| tui | run the dashboard |
| setup | run the interactive wizard |
| status | show the resolved configuration and each provider's state |
| restart | replace the running service without cutting a response |
| promote | mark a served model eligible for policy-managed upgrades |
| import | ingest a predecessor's usage log into the ledger |
| export | emit the ledger's rows |
| watch-context-window | report served responses that prove a context window |
| picker sync | write the active policy's rows into a client's settings |

Toker MUST also offer the verbs its scheduled units invoke, being the sleep-lock hold and the window ping.

A verb that a platform owns elsewhere MUST remain as a documented no-op rather than be removed.

> [!NOTE]
> A unit installed by an earlier version still invokes it.

## Store access

A command that changes the running service's behaviour MUST go through the control endpoints, which [DCTRL](../service/control.md) specifies.

A command that only records evidence MAY write the store directly without talking to the service.

> [!NOTE]
> Promotion changes learned evidence rather than policy, so it needs no revision and no running service.
> The equivalent control operation additionally refreshes the service's published learning snapshot.

Promotion MUST infer the binding where exactly one has learned the exact identity.
Promotion MUST require the binding to be named otherwise.

Promotion MUST refuse a model that the active revision's upgrade policies do not classify, or that more than one classifies.

Promotion MUST offer a mode that reports what it would do without doing it.

## Status

Status MUST report which source supplies each credential and MUST NOT report any credential's value.
