---
id: DCONF
---

# Configuration and state

Toker reads a configuration file for what it needs before it can reach its store, and keeps everything else in the store.
This spec covers what the file holds, where a credential may live, and which directories the service may write.

Routing is not configuration.
It lives in the store as policy revisions, as [RREV](../routing/revisions.md) specifies, and how a machine's routing moved there is specified by [RMIG](../routing/migration.md).

## The file

The file MUST hold only the settings that cannot be read from the store, or that must be known before the store is opened.

| setting | why it is here |
| --- | --- |
| the listener's port | needed before anything is served |
| the store's path | cannot be stored in the store |
| which providers are enabled, and where each is reached | policy cannot supply enablement |
| where each provider's credential comes from | a credential is deliberately not stored where Toker's data is |
| the session and ping header names | read per request, before any lookup |
| the gate toggles and thresholds | not a per-request routing decision |
| the notice styles per frontend profile | not a per-request routing decision |

A provider MUST be enabled by the presence of its block and disabled by its absence.

The file MUST be parsed strictly, refusing a value it does not recognise rather than ignoring it.

A notice style naming a profile outside the permitted character set MUST be refused when the file is read, as [ROUTE](../frontend/profiles.md) requires.

Where a setting may also be given by environment variable, naming both MUST be a configuration error rather than a silent precedence.

## Credentials

A credential Toker holds MUST live in the operating system's keyring, in an environment variable the service can read, or as a literal in the configuration file.

Where a credential is written as a literal, the file MUST be created readable only by its owner.

Toker MUST report which source supplies a credential and MUST NOT report its value.

A debug rendering of a credential-bearing value MUST redact it.

Which credential reaches which provider is specified by [ROUTE](../routing/overview.md).

## State

The store's directory is the state directory.

The installed service MUST be permitted to write only the state directory.

Toker MUST write anything new it keeps into the state directory.

> [!NOTE]
> A test is given a temporary directory and never the real store.

### The one write outside it

Refreshing a login that Toker shares with another program on the same machine rewrites that program's own credential file, which lies outside the state directory.

Where such a provider is enabled, setup MUST add that directory to the paths the service may write.
Where such a provider is enabled, setup MUST add nothing else.

The refresh MUST re-read the file immediately before writing and MUST adopt the other program's tokens where they moved.

> [!NOTE]
> The other program refreshes the same file on its own schedule.

That write MUST replace the file atomically, which is why the directory rather than the file must be writable.

A unit written before that permission existed lacks it, and every refresh from the service then fails to persist, which setup MUST correct when it is run again.
