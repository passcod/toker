---
id: RMIG
---

# Migrating routing into the store

Routing was once configuration interpreted per request, and is now a series of revisions in the store.
This spec covers the seam between the two: how a machine whose routing was in its file gets a revision, what survives an interrupted migration, how a file may be read deliberately afterwards, and how revisions an earlier version generated are retired.

## The routing fields

Enabling a provider is configuration, held in the store as [DCONF](../service/config.md#in-the-store) specifies, being the one fact policy cannot supply.

> [!NOTE]
> A revision may route to a provider that has since been switched off, and the candidate then answers that it is not configured, rather than the revision failing to load.

A file carrying the routing fields of an earlier version MUST still parse.

> [!NOTE]
> Setup reads such a file to move a machine off it, and an import reads one deliberately, so those fields must still be understood long after nothing writes them.

Nothing at request time MUST read those fields.
They MUST be read only when the first revision is created, and when a file is imported deliberately.

Toker MUST NOT write a configuration file.

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
The pointer MUST be cleared only by [retiring a generated revision](#retiring-generated-revisions).

## The first revision

The first revision is generated from the file's routing fields, so that a machine's existing behaviour survives the move.

It MUST generate each of these.

| generated | from |
| --- | --- |
| a route per binding, carrying that binding's model map as its alias policy | enabled only where the provider is configured |
| a default per protocol, and one for the unprefixed profile | the configured default for that protocol |
| a rewrite per provider prefix | the known providers |
| presentation selectors | the configured picker rules |
| a classifier and an upgrade policy per binding serving Anthropic's model identities | enabled only where the equivalent toggle was |
| a compaction model substitution over the bindings serving Anthropic's model identities | the configured compaction model |

Every known provider MUST own its prefix even while disabled.

> [!NOTE]
> A prefix naming an unconfigured provider then lands on a disabled candidate and gets a local answer, instead of falling through to a default that would send it somewhere the operator did not name.

Where no default exists for the Messages protocol but a configured binding has a model map, it MUST generate a lowest-precedence rewrite over the client's own model names, so that a client's side calls still reach a configured provider through that binding's alias policy.

> [!NOTE]
> Those calls name the client's own bare model identifiers whatever the operator picked, and policy never recognises them by their prompt.

### In authored vocabulary

The first revision MUST be written in the vocabulary of an authored document, and MUST be compiled by the same compiler as any other revision.

> [!NOTE]
> Toker has one user on a couple of machines, and a parallel set of matchers, parsers, bars and targets kept only to reproduce one generated file's quirks cost more than the quirks were worth.

A model map's entry naming a model MUST become an alias rule with an exact matcher on that identity.
A model map's entry naming a family MUST become an alias rule matching the identities the generated classifier places in that family.

The generated classifier MUST match an identity of the form `claude-`, a family name, then a trailing run of numeric version segments, and MUST read that run as a numeric tuple of any length.
It MUST give no family to an identity carrying a date the catalogue does not know.

> [!NOTE]
> Its observed days and prompt sizes would otherwise authorise routing traffic to a different identity.

An identity whose version segments precede its family name has no family under the generated classifier.

> [!NOTE]
> Only Anthropic's earlier naming took that shape, so a request naming one of those models keeps it rather than being upgraded.
> Reading those identities needs a classifier that pulls digits out of the middle of a name, which is what misread every other provider's identities.

The generated upgrade policies MUST use the scaled evidence bar, and MUST let a promotion stand in for evidence, as [RMODEL](models.md#electing-a-familys-target) specifies.

The generated compaction substitution MUST name the configured compaction model, the Sonnet family where none is configured, and no model where the configuration turns the model change off, as [RCOMP](compaction.md#choosing-the-target) specifies.

### Creating it

Creation MUST happen in one immediate transaction.

Where a pointer already exists, that transaction MUST return the active revision unchanged.
Otherwise it MUST insert the revision, set the pointer, copy the document into the draft at its first generation, stamp the import marker, and set the desired publication handoff for every adapter.

A marker present without a pointer MUST refuse the transaction.

> [!NOTE]
> Nothing writes that state, and re-establishing from it would silently discard whatever produced it.

Both the service at startup and setup MUST reach that transaction, and MUST converge because the first writer wins and the other reads the revision it wrote.

> [!NOTE]
> A machine with no file to read establishes its first revision from the providers it has enabled, whichever of the two starts first.

Where setup reads a file, it MUST validate that file's routing fields first.
An ambiguous default, being several enabled bindings for one protocol with nothing naming one, MUST be an error naming the field, raised before anything is written.

## Migrating the file and the store together

A machine not yet owned needs its store and its file changed, and the two cannot share a transaction.
The order MUST be:

1. read the existing file and apply the operator's choices to it in memory
2. in one store transaction, write the configuration rows and activate the first revision from that result
3. remove the file

Activation MUST run after the choices are validated and before the file is removed, so that a failure to validate or activate stops before anything is written.

An interruption between the second and third steps leaves an owned store and a file still in place.
Running setup again MUST repair it, by importing the same values over themselves, as [DCONF](../service/config.md#moving-a-machine-off-the-file) specifies.

> [!NOTE]
> Nothing but setup runs while the file is there, so no row can have been edited between the transaction and the removal, and importing twice cannot overwrite a later choice.

> [!NOTE]
> The reverse order does not recover: a file rewritten first, with a crash before activation, leaves a machine with neither the old rules nor a revision, and the first revision would then be built from a file that no longer carries those rules.

Setup MUST read the store read-only before asking anything.
An absent store, or one predating the policy tables, is not owned.
A store that exists but cannot be read MUST stop setup before it changes anything.

> [!NOTE]
> Writing a configuration over a database in an unknown state could strand the machine.

## Setup on an owned machine

Setup MUST NOT replace or edit an active revision.

It MUST report that routing is owned by the store and name the active revision.
It MUST NOT ask which binding is a protocol's default, that question having no effect.

## Importing a file deliberately

An import MUST name the file it reads, there being no configuration file to take by default.

An import MUST load that file, read its routing fields as input, generate a document of the same shape as the first revision, and replace the draft at the generation the caller holds.

An import MUST leave the active revision untouched.
An import MUST leave the draft as it was where the file fails to load or compile.

The result MUST be an ordinary draft, validated, previewed, edited and activated like any other.

> [!NOTE]
> Import is for a file that carries routing.
> One that carries none yields a document built from the enabled providers alone, which is the first revision for a fresh machine rather than the operator's edited policy.

## Retiring generated revisions

An earlier version stored revisions whose documents carried semantics only its generated first revision could use, and recorded them with a generated compiler origin.
Such a revision no longer decodes, and MUST be retired in the transaction that [creates the first revision](#creating-it), before it reads the pointer.
The service MUST reach that transaction on every start, whether or not a revision is active, and setup reaches it as before.

That transaction MUST clear the pin of every lane pinned to a generated revision, and MUST delete every generated revision.

> [!NOTE]
> A lane warm at that moment loses its cache once, as an activation forced past warm lanes would.
> Refusing instead would hold the service down until every such lane went idle and was pruned, and on the machines that exist those lanes were long cold anyway.

Where the active pointer named one, the same transaction MUST clear the pointer, the draft and the import marker, and MUST then establish the first revision as a machine with no file to read does, as [creating it](#creating-it) specifies.
Where the draft was based on a generated revision, or no longer decodes, the same transaction MUST replace the draft with the active revision's document at the next generation.

> [!NOTE]
> An imported draft carries the generated semantics whichever revision it was based on.

> [!NOTE]
> Deleting these rows is the one exception to keeping the store's history, because a revision that cannot be decoded keeps nothing a reader could use.

A ledger row naming a retired revision MUST keep that revision's number, and the number MUST NOT be reused.

## Adopting rows written before ownership was recorded

Where a publication adapter has applied rows without recording which identities it owned, that state MUST NOT be read as owning none.

Such a row MUST be returned to pending with a stable detail code, and the next reconciliation MUST adopt, once, only the rows matching the pattern the earlier version used.

Every reconciliation after that MUST work from recorded identities alone, as [RPUB](publication.md#ownership-is-exact) requires.
