---
id: USETUP
---

# The setup wizard

The wizard is the one place that writes Toker's configuration, installs its units, and points a client at the proxy.
This spec covers the state it establishes before asking anything, the checks it makes before changing anything, and the units it installs.

## Before asking

Setup MUST read the store read-only before it asks anything.

A store that exists but cannot be read MUST stop setup before it changes anything, as [RMIG](../routing/migration.md#migrating-the-file-and-the-store-together) requires.

Setup MUST report the machine's current state, including whether its routing is owned by the store, before offering to change it.

## Verifying before patching

Setup MUST verify that the listener answers, and that each frontend profile's prefix is stripped, before it patches any client.

Verifying a usage path alone MUST NOT be accepted as proof that a prefix is stripped.

> [!NOTE]
> A proxy predating profile prefixes forwards the prefixed path upstream unchanged and relays the upstream's refusal, which is exactly what a client patched to that prefix would then receive on every request.
> Asking Toker's own status through the prefix is the check only a proxy that strips it can pass.

Setup MUST NOT point a client's base address at a listener that is not answering.

> [!NOTE]
> A client reloads its settings into running sessions, so pointing it at nothing ends every live session at once, including the operator's own.

## Writing

Setup MUST replace the configuration file atomically.

Setup MUST write the file without routing fields, and MUST name any it dropped.

Where the operator keeps the configuration as it is, setup MUST leave the file byte for byte alone.

The order in which setup writes the store and the file is specified by [RMIG](../routing/migration.md#migrating-the-file-and-the-store-together).

Where setup found an active listener on the configured port, it MUST replace the running instance by the path [DLIFE](../service/lifecycle.md#replacing-the-running-instance) specifies.

## Installing units

Setup MUST install a unit for the service and one holding its listening socket, so that connections queue while no instance runs.

Setup MUST install the units that reconcile a client's settings whenever that client is a configured frontend profile, rather than only when a particular provider is enabled.

> [!NOTE]
> The rows come from the policy, not from one provider.

Setup MUST sandbox each installed unit to the directories it needs and no others, as [DCONF](../service/config.md#state) specifies.

Setup MUST run the settings reconciliation once itself, through the unit the schedule will use.

> [!NOTE]
> Setup then exercises the sandboxed path rather than a path only it takes.

Setup MUST retire the units of any schedule slot the operator dropped.
