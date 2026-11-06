# Where routing lives, and how it moved

Routing used to be configuration: protocol defaults, per-provider model maps,
picker rules and two gate settings in `toker.toml`, interpreted per request. It
is now data in the ledger: a numbered series of policy revisions, one of them
active, edited from `toker tui` and changed without a restart (see
[control.md](control.md)). The config file keeps what the ledger cannot
sensibly hold, and this topic is about the seam between the two: how a machine
that had routing in its file got a revision, what happens if that is
interrupted, and what setup does afterwards.

## What stays in the file

`toker.toml` is bootstrap: the listener port, the ledger path, which providers
are enabled and where their endpoints are, where each credential comes from
(env var, keyring, a literal in the 0600 file), the session and ping header
names, `awake`, the gate toggles and thresholds that are not routing, and the
notice styles. None of that is a per-request decision, and enabling a provider
is the one fact policy cannot supply: a revision may route to a provider that
config has since switched off, and the candidate answers not-configured rather
than the revision failing to load.

The fields that were routing are still accepted when a file has them, because a
file written by an older toker has to load. They are the two
`default_backend_*` keys, `gates.force_newest`, `gates.compact_model`, and each
provider's `model_map` and `picker`. Nothing at request time reads them. They
are read in two places only, both of which turn them into a policy document: the
creation of revision one, and the explicit import below. A file written by
setup no longer contains them (`Config::to_owned_file`), and
`config_writer::legacy_routing_fields` names the ones a rewrite dropped so setup
can say so.

## Ownership

A machine's routing is database-owned once `routing_policy_state` has an active
revision. That pointer is the whole ownership marker; the config file carries no
flag. There is no state in which both the file and the ledger decide: if a
revision is active, the file's routing fields are ignored, edited or not, and a
change to them does nothing. The only way the file affects routing again is the
explicit import, which makes a draft the user must validate and activate.

The ledger migration that creates the policy tables (version 7) never touches
the pointer. The pointer moves only inside the transaction below.

A service that cannot establish or load an active revision refuses to start. A
pointer to a revision that is missing, or one that no longer compiles, is a
startup error. Falling back to the file would route a machine by rules the user
had since replaced, and would do so silently.

## Revision one

Revision one is the policy document generated from the legacy fields
(`policy::legacy`). It is built to route exactly as the interpreted config did:
a rewrite per provider prefix, the protocol aliases, each provider's model map
as that candidate's alias rules, the protocol defaults (one scoped to the
protocol, one to the `unprefixed` profile), the OpenRouter picker rules as
Claude presentations, a family classifier and upgrade policy per binding that
supports families (enabled by `gates.force_newest`), and the compaction route
from `compact_model`. A machine with no Anthropic default configured and a model
map on an Anthropic-protocol binding also gets a rule sending bare `claude-*`
names to that binding, so the helper calls a harness makes under Claude model
names still land where the map was written for. The generated Chat and Responses
model lists are described in [publication.md](publication.md).

It is stored as an ordinary revision with source `legacy`, activation scope
`new_lanes` (no lane is pinned yet, so nothing is stranded) and compiler origin
`generated_revision_one`. The compiler origin is a separate column from the
source because the two answer different questions. The source says what created
the revision; the origin says which compiler its document needs. A few
constructs (the legacy family split, the adaptive learning bar, the model and
family matchers, the compaction target taken from a family spec) are rejected by
the compiler unless the origin is generated revision one, and a reactivation of
revision one is a `reactivation` that must still compile as revision one.
Migration 15 added the column and backfilled it for revision one and every
reactivation chained from it.

Activation inherits the origin of the revision it replaces, and reactivation
takes the selected revision's. Nothing creates an `authored` revision, so every
revision on every machine is compiled as generated revision one today. The
column is there so that a future from-scratch document can be compiled strictly
without disturbing the history that cannot be.

## Creating it

`Store::activate_revision_one` does the work in one `IMMEDIATE` transaction. It
returns what is already active if a pointer exists, and otherwise inserts the
revision, sets the active pointer, copies the document into the draft (so the
draft starts equal to the active revision, at generation one), stamps
`legacy_imported_ms`, and sets the desired publication handoff for every
adapter. If the marker exists without a pointer the transaction refuses: that is
a state nothing writes, and it would otherwise be silently re-established.

Two callers reach it, and they converge because the first one wins and the other
reads what it wrote:

- The service, at start, when the pointer is empty (`Server::with_seams`). A
  machine that upgrades without running setup is migrated by its first start.
- `toker setup`, so that the file can be written without the routing fields
  before the service ever starts on it (`wizard::activate_revision_one`, which
  computes the catalogue generation from the same cached view the picker sync
  uses).

Both go through `legacy::establish_revision_one`, which validates the legacy
config first. An ambiguous default (several enabled backends for one protocol
with no `default_backend_*` naming one) is an error naming the key, at either
caller, before anything is written.

## The config and ledger activation, and its recovery

On a machine that is not yet owned, setup must do two things to two different
stores that cannot share a transaction: put routing in the ledger, and rewrite
the file without it. The order is the whole design:

1. Read the existing file and apply the wizard's choices to it, in memory.
2. Activate revision one in the ledger from that result.
3. Atomically replace the file with the version that has no routing fields.

The activation runs inside the config writer's callback, after the choices are
applied and validated and before the file is replaced. So a failure to
validate or to activate stops before anything is written, and the old file is
untouched. If the file write then fails, or setup is killed between steps 2 and
3, what remains is an owned ledger and a file that still has routing fields.
That is a consistent state, because the service ignores those fields once a
revision is active. The next `toker setup` finds the machine owned, writes the
owned file, and reports the fields it dropped. There is nothing to repair by
hand and no half state to detect.

The reverse order would not recover. A file rewritten first and a crash before
activation leaves a machine with neither the old rules nor revision one, and the
service would build revision one from a file that no longer says what the rules
were.

Setup reads the ledger read-only before it asks anything. No ledger, or one
from before the policy tables, is not owned. One that exists but cannot be read
stops setup before it changes anything, because writing a config over a database
in an unknown state could strand the machine.

## Setup on a database-owned machine

Setup does not replace or edit an active revision. It:

- reports `routing: database-owned, revision N` and says edits go through the
  routing page;
- does not ask which backend is the default for each protocol, since that
  question has no effect there;
- on "keep the config as is", leaves the file byte for byte alone, legacy
  fields included;
- on reconfigure, rewrites the file with the provider and listener choices and
  without routing fields, naming any it dropped.

Anything that changes the routing of an owned machine goes through the draft.

## Importing a legacy file on purpose

The routing page's status view can import `toker.toml` into the draft (the
`i` key, behind a confirmation). It loads the file as the service would, reads
its routing fields as input (so an ambiguous default is an error here too),
generates a revision-one-shaped document and replaces the draft with it at the
generation the page holds. The active revision is untouched. The result is an
ordinary draft: it can be validated, previewed, edited, and activated with a
scope and cache consent like any other, and a failure to load or compile the
file leaves the draft as it was.

Import is for a file that still carries routing. A file setup has rewritten has
none, and importing it yields a document built from the enabled providers alone,
which is revision one for a fresh machine and not the user's edited policy. That
is the intended meaning of "the database owns it".
