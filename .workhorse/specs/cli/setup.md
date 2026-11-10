---
id: USETUP
---

# The setup wizard

The wizard is the one place that writes Toker's configuration, installs its units, and points a client at the proxy.
This spec covers the state it establishes before asking anything, the checks it makes before changing anything, and the units it installs.

## Before asking

Setup MUST be the only command that may run while a configuration file exists.

Setup MUST read the store read-only before it asks anything.

A store that exists but cannot be read MUST stop setup before it changes anything, as [RMIG](../routing/migration.md#migrating-the-file-and-the-store-together) requires.

Setup MUST report the machine's current state, including whether a configuration file is present and whether its routing is owned by the store, before offering to change it.

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

Setup MUST write configuration as store rows, and MUST NOT write a configuration file.

Where a configuration file exists, setup MUST report what it read from that file before importing it.

The order in which setup writes the store and removes the file is specified by [RMIG](../routing/migration.md#migrating-the-file-and-the-store-together).

Setup MUST ask only what the dashboard cannot later change.
That is the listener port and the schedule slots, which reach the units rather than the store.

> [!NOTE]
> A setting the dashboard can change is a setting setup asking for is a second surface to keep agreeing with the first.
> The port is not one of those: the socket unit is what listens, so a port the dashboard could change would disagree with the unit it was installed with.

On a store with no provider configured, setup MUST enable the providers whose credential it finds without asking: a provider whose keyring entry exists, and the codex subscription where the codex CLI's login is present.
Setup MUST NOT ask for a credential, and MUST NOT read a keyring entry's value to find out whether it exists.
A store that already configures a provider MUST keep its providers as they are.

Setup MUST complete whether or not it found a provider: it writes the configuration, installs the units, offers the schedule slots where they apply and points the clients at the proxy all the same.

> [!NOTE]
> Everything else about providers is edited on the dashboard's providers page, which setup hands off to, so a machine set up once never needs setup again for a provider to be added.

Where setup found an active listener on the configured port, it MUST replace the running instance by the path [DLIFE](../service/lifecycle.md#replacing-the-running-instance) specifies.

## Installing units

Setup MUST install a unit for the service and one holding its listening socket, so that connections queue while no instance runs.

Setup MUST install the units that reconcile a client's settings whenever that client is a configured frontend profile, rather than only when a particular provider is enabled.

> [!NOTE]
> The rows come from the policy, not from one provider.

Setup MUST reach the system manager through `systemctl` itself, leaving authorisation to the platform, and MUST NOT invoke `sudo`.

> [!NOTE]
> Some machines authorise through polkit and have no working sudo.

Setup MUST sandbox each installed unit to the directories it needs and no others, as [DCONF](../service/config.md#state) specifies.

Setup MUST run the settings reconciliation once itself, through the unit the schedule will use.

> [!NOTE]
> Setup then exercises the sandboxed path rather than a path only it takes.

Setup MUST retire the units of any schedule slot the operator dropped.

## Handing off

Setup MUST end by replacing itself with the dashboard.

> [!NOTE]
> The machine is serving by then, and everything still to configure is edited there, so the wizard ends at the next step rather than describing it.
