---
id: UREAD
---

# Reading the ledger

Three commands read the ledger without serving traffic: one ingests a predecessor's log, one emits rows, and one reports proof of a context window.
This spec covers what each guarantees.

A command that does not write MUST open the store read-only, as [SROWS](../store/ledger.md#opening-the-store) requires.

## Export

Export MUST emit the ledger's rows oldest first, one object per row, streamed rather than gathered.

Export MUST be driven by the columns the query returns rather than by a fixed row type.

> [!NOTE]
> A column a later migration adds is then exported the day it exists, and a stored value this binary does not understand is shown rather than refused.

Keys MUST be the ledger's own column names.

> [!NOTE]
> Using a predecessor's vocabulary would make Toker's rows look like that predecessor's rows while meaning something else.

An absent value MUST be omitted, and MUST NOT be written as a null or a zero.

> [!NOTE]
> A missing key is then not recorded, with the same caution owed to earlier rows.

Values MUST be emitted as stored.
A column holding JSON text MUST be nested rather than quoted, and its exact bytes MUST stay in the ledger.

Export MAY add a human-readable form of the row's timestamp, and MUST add nothing else that is not a column.

> [!NOTE]
> The export can then carry no content and no credential the ledger does not.

## Import

Import MUST map a predecessor's log into the ledger as [SROWS](../store/ledger.md#imported-rows) specifies.

Import MUST keep a checkpoint, so that a repeated run continues rather than duplicates.

## Reporting a context window

This command MUST claim only the window a served response proves: a prompt exceeding a provider-declared ceiling, or a prompt large enough to prove an exact larger window.

An unknown window and a prompt below the threshold MUST say nothing.

The command MUST read only rows after the bound it was given, or after the point its own state recorded.

> [!NOTE]
> Without a bound the whole ledger replays as news on every run.

The command MUST keep a record of what it has already reported, so that each proof prints once.

The command MUST make one pass per run, so that a monitor can re-run it.

A collapsing prompt within a lane MUST NOT be reported.

> [!NOTE]
> A compaction and a subagent starting both look like a prompt collapsing, and nothing in the ledger separates them, so that negative evidence is left unreported rather than guessed at.
