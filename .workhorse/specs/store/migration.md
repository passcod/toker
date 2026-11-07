---
id: SMIG
---

# Migrating routing into the store

Routing was once configuration interpreted per request, and is now a series of revisions in the store.
This spec covers the seam between the two: how a machine whose routing was in its file gets a revision, what survives an interrupted migration, and how a file may be read deliberately afterwards.

## The file

The configuration file is bootstrap.
It holds the listener, the store's path, which providers are enabled and where they are reached, where each credential comes from, the header names, and the toggles and styles that are not routing.

None of those is a per-request decision.

Enabling a provider is the one fact policy cannot supply.

> [!NOTE]
> A revision may route to a provider the configuration has since switched off, and the candidate then answers that it is not configured, rather than the revision failing to load.

A file carrying routing fields MUST still load.

> [!NOTE]
> A file written by an earlier version has to be readable by a later one.

Nothing at request time MUST read those fields.
They MUST be read only when the first revision is created, and when a file is imported deliberately.

A file this version writes MUST NOT contain them, and a rewrite MUST name the fields it dropped.

## Ownership

A machine's routing is owned by the store once an active revision exists.
That pointer MUST be the whole ownership marker, and the file MUST carry no flag of its own.

There MUST be no state in which both the file and the store decide.
Where a revision is active, the file's routing fields MUST be ignored whether or not they were edited.

The only way the file may affect routing again is a deliberate import.

A service that cannot establish or load an active revision MUST refuse to start.
A pointer to a revision that is missing, or that no longer compiles, MUST be a startup error.

> [!NOTE]
> Falling back to the file would route a machine by rules the operator had since replaced, and would do so silently.

The migration that creates the policy tables MUST NOT touch the pointer.

## The first revision

The first revision is generated from the file's routing fields, so that a machine's existing behaviour survives the move.

It MUST generate a route per binding carrying that binding's model map as its alias policy, enabled only where the provider is configured; a default per protocol and for the unprefixed profile; a rewrite per provider prefix; presentation selectors for the configured picker rules; a classifier and an upgrade policy per binding, enabled only where the equivalent toggle was; and a compaction target.

Every known provider MUST own its prefix even while disabled.

> [!NOTE]
> A prefix naming an unconfigured provider then lands on a disabled candidate and gets a local answer, instead of falling through to a default that would send it somewhere the operator did not name.

Where no default exists for the Messages protocol but a configured binding has a model map, it MUST generate a lowest-precedence rewrite over the client's own model names, so that a client's side calls still reach a configured provider through that binding's alias policy.

> [!NOTE]
> Those calls name the client's own bare model identifiers whatever the operator picked, and policy never recognises them by their prompt.

### Generated semantics

The first revision needs matchers, classifiers and bars that authored policy cannot express.

Each MUST be marked as generated at compile time, and the compiler MUST refuse every one of them from an authored document.

A revision MUST record its compiler origin, and an edit or a reactivation of a generated revision MUST inherit it, as [RPOL](../policy/overview.md#history-and-reactivation) requires.

A field no longer written MUST be kept in the document type.

> [!NOTE]
> A stored revision carrying it must still decode and keep its fingerprint.

### Creating it

Creation MUST happen in one immediate transaction, which MUST return the active revision unchanged where a pointer already exists, and otherwise insert the revision, set the pointer, copy the document into the draft at its first generation, stamp the import marker, and set the desired publication handoff for every adapter.

A marker present without a pointer MUST refuse the transaction.

> [!NOTE]
> Nothing writes that state, and re-establishing from it would silently discard whatever produced it.

Both the service at startup and the setup wizard MUST reach that transaction, and MUST converge because the first writer wins and the other reads what it wrote.

> [!NOTE]
> A machine upgraded without running setup is then migrated by its first start.

Both MUST validate the file's routing fields first.
An ambiguous default, being several enabled bindings for one protocol with nothing naming one, MUST be an error naming the field, raised before anything is written.

## Migrating the file and the store together

A machine not yet owned needs two stores changed that cannot share a transaction.
The order MUST be:

1. read the existing file and apply the operator's choices to it in memory
2. activate the first revision in the store from that result
3. atomically replace the file with one carrying no routing fields

Activation MUST run after the choices are validated and before the file is replaced, so that a failure to validate or activate stops before anything is written.

An interruption between the second and third steps leaves an owned store and a file that still carries routing fields.
That MUST be a consistent state needing no repair, because the service ignores those fields once a revision is active.

> [!NOTE]
> The reverse order does not recover: a file rewritten first, with a crash before activation, leaves a machine with neither the old rules nor a revision, and the first revision would then be built from a file that no longer says what the rules were.

Setup MUST read the store read-only before asking anything.
An absent store, or one predating the policy tables, is not owned.
A store that exists but cannot be read MUST stop setup before it changes anything.

> [!NOTE]
> Writing a configuration over a database in an unknown state could strand the machine.

## Setup on an owned machine

Setup MUST NOT replace or edit an active revision.

It MUST report that routing is owned by the database and name the active revision.
It MUST NOT ask which binding is a protocol's default, that question having no effect.

Where the operator keeps the configuration as it is, setup MUST leave the file byte for byte alone, routing fields included.
Where the operator reconfigures, setup MUST rewrite the file without routing fields and name any it dropped.

## Importing a file deliberately

An import MUST load the file as the service would, read its routing fields as input, generate a document of the same shape as the first revision, and replace the draft at the generation the caller holds.

An import MUST leave the active revision untouched, and MUST leave the draft as it was where the file fails to load or compile.

The result MUST be an ordinary draft, validated, previewed, edited and activated like any other.

> [!NOTE]
> Import is for a file that still carries routing.
> A file setup has rewritten carries none, so importing it yields a document built from the enabled providers alone, which is the first revision for a fresh machine rather than the operator's edited policy.

## Adopting rows written before ownership was recorded

Where a publication adapter has applied rows without recording which identities it owned, that state MUST NOT be read as owning none.

Such a row MUST be returned to pending with a stable detail code, and the next reconciliation MUST adopt, once, only the rows matching the pattern the earlier version used.

Every reconciliation after that MUST work from recorded identities alone, as [RPUB](../policy/publication.md#ownership-is-exact) requires.
