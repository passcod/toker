---
id: DCONF
---

# Configuration and state

Toker's configuration lives in its store, beside the policy revisions and the ledger, and is edited from the dashboard.
This spec covers the two settings that cannot live there, what the store holds instead, where a credential may be kept, and which directories the service may write.

Toker MUST NOT write a configuration file.
One left by an earlier version is read only to move the machine off it.

Routing is not configuration.
It lives in the store as policy revisions, as [RREV](../routing/revisions.md) specifies, and how a machine's routing moved there is specified by [RMIG](../routing/migration.md).

## Before the store

Two settings are needed before the store can be opened or reached, and so cannot be held in it.

| setting | default | override |
| --- | --- | --- |
| the store's path | the user data directory's `toker/toker.db` | the `TOKER_DB` environment variable |
| the listener's port | a fixed loopback port | the `PORT` environment variable |

Each MUST resolve the same way in every process, so that the service, the command line and the dashboard agree on where the store is and which port answers.

Setup MUST write configuration into the store the environment resolves, and MUST carry an overridden store path into the service's unit.

> [!NOTE]
> The service does not share the shell setup was run from, so without that the two resolve different stores and the service reads one setup never wrote.

Where a command cannot reach the service, it MUST name the port it tried.

> [!NOTE]
> The service takes its environment from its unit and a command takes its from the operator's shell, so a `PORT` exported for something else sends that command to a port nothing is listening on.
> Naming the port turns that into something the operator can see at once.

## In the store

Everything else is held in the store as typed rows:

- a row per provider Toker knows, carrying whether it is enabled, the endpoint it is reached at, where its credential comes from, and the settings particular to that provider
- the gate settings: each gate's toggle, and the cold gate's thresholds
- the notice style for each frontend profile, and the default for a profile with none
- the session and ping header names
- whether the idle-sleep lock is held
- the extra transcript roots the dashboard searches for session labels

Toker MUST hold a row for every provider it knows, whether or not that provider is enabled.
Enablement MUST be a field of that row.

> [!NOTE]
> Switching a provider off then keeps the endpoint and the credential source it was configured with, for when it is switched back on.

Every provider's endpoint MUST be configurable, the subscription providers included.

> [!NOTE]
> Toker is itself a proxy, so an operator may reasonably want another one between it and a provider.

A value that identifies Toker to a provider MUST NOT be configurable.

> [!NOTE]
> Such a value is a fact about the provider rather than a preference, and a different one breaks it rather than tuning it.

Where Toker reads another program's file, it MUST find that program's directory the way that program itself does, and MUST NOT hold a path to it.

The service MUST resolve that directory the same way a command run in the operator's shell does.

> [!NOTE]
> The path then follows from how the program is installed rather than from what Toker was told, so the two cannot be configured into disagreeing.
> The service does not share that shell, so setup has to carry the program's own environment into the unit for them to agree.

A notice style naming a profile outside the permitted character set MUST be refused, as [FPROF](../frontend/profiles.md) requires.

### Editing

An edit MUST be made through the service's control surface, and the service MUST apply it to its own running state in the same operation.

> [!NOTE]
> The service is then the only writer, so an edit takes effect without a restart and without anything watching the store for it.

The control surface MUST refuse a value it cannot use, naming the setting it refused.

> [!NOTE]
> Refusing at the edit is refusing where there is someone to tell.

Enabling a provider MUST, in the same operation, give each protocol that provider serves a default route to it where the active revision has none for that protocol, and MUST [save](../routing/revisions.md#saving-an-edit) that as a revision.
A protocol that already has a default MUST keep it.

> [!NOTE]
> A machine set up before any provider was found then serves once one is turned on, without a trip to the routing page.

### Configuration never refuses a start

A stored setting the service cannot use MUST disable only what that setting describes, and MUST NOT prevent the service from starting.

A provider whose credential does not resolve MUST be treated as not configured, and a candidate reaching it MUST answer that rather than the request failing.

A machine with no provider enabled MUST still start and MUST still serve, answering each request that nothing is configured.

> [!NOTE]
> The dashboard reaches configuration through the service, so a service that refuses to start over a setting leaves no way to correct that setting.
> Routing refuses instead, as [RMIG](../routing/migration.md#ownership) requires, because serving by rules the operator has replaced is worse than not serving at all.
> A provider that cannot be reached is not that.

## Credentials

A credential Toker holds MUST live in the operating system's keyring, in an environment variable the service can read, or as a literal in the store.

Toker MUST report which source supplies a credential and MUST NOT report its value.

A debug rendering of a credential-bearing value MUST redact it.

The dashboard MUST show whether a credential is set, and MUST NOT show what it is.

Exporting the ledger MUST NOT carry a credential, which it upholds by exporting the ledger's own columns and nothing beside them.

Which credential reaches which provider is specified by [ROUTE](../routing/overview.md).

## State

The store's directory is the state directory.

The state directory MUST be readable only by its owner, and Toker MUST enforce that each time it opens the store.

> [!NOTE]
> The store holds credential literals, and the database engine writes those through sidecar files beside it, so closing the database file alone would leave them readable.
> Enforcing it on open rather than at setup closes a directory created before Toker kept a credential there.

The installed service MUST be permitted to write only the state directory.

Toker MUST write anything new it keeps into the state directory.

> [!NOTE]
> A test is given a temporary directory and never the real store.

### The one write outside it

Refreshing a login that Toker shares with another program on the same machine rewrites that program's own credential file, which lies outside the state directory.

Where such a provider is enabled, setup MUST add the directory it discovered to the paths the service may write.
Where such a provider is enabled, setup MUST add nothing else.

The refresh MUST re-read the file immediately before writing and MUST adopt the other program's tokens where they moved.

> [!NOTE]
> The other program refreshes the same file on its own schedule.

That write MUST replace the file atomically, which is why the directory rather than the file must be writable.

A unit written before that permission existed lacks it, and every refresh from the service then fails to persist, which setup MUST correct when it is run again.

## Moving a machine off the file

A machine whose configuration is a file is moved by setup, which MUST be the only command that may run while such a file exists.

Every other command, the service included, MUST refuse to run while a configuration file exists, and MUST say that setup moves it.

A command MUST look for that file where the operator's environment names it, and otherwise in the user configuration directory.

> [!NOTE]
> Refusing is what makes the move safe to interrupt.
> Nothing can read the store's configuration while the file is there, so nothing can have edited a row between the import and the file's removal.

A file naming a store other than the one the environment resolves MUST stop setup before it changes anything.

> [!NOTE]
> A path the file carried is not an override the store can hold, so importing it would write the configuration into one store and leave every later command reading another.

Setup MUST import the file in one transaction, and MUST remove the file only after that transaction commits.

An interrupted move MUST be repaired by running setup again, which imports the same values over themselves.

The file's routing fields MUST NOT be stored as configuration, being read only to author the first revision, as [RMIG](../routing/migration.md#the-first-revision) specifies.
