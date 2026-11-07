---
status: draft
---

# Toker's specs in spec voice

Writing toker a set of specs under `.workhorse/specs/`, in the RFC 2119 spec
voice, derived from `docs/internals/` and verified against the code.
`docs/internals/` does not survive: each piece becomes a normative requirement,
a non-normative note, a rule in AGENTS.md, or nothing.

## Behaviour

The deliverable is a spec tree, so "behaviour" here is what the corpus must
contain and how it is arranged.

### Where each kind of internals content lands

Four destinations, decided per sentence rather than per file:

- **Requirement** — normative prose in the owning spec. The bulk of
  `routing.md`, `policy.md`, `ledger-schema.md`, `control.md`, `notices.md`,
  `quota.md`, `cold-gate.md`, `lanes*.md`, `compaction.md`, `models.md`,
  `publication.md`, `attempts.md`, `lineage.md`, `forecasting.md`,
  `migration.md`, `pinging.md`, `sleep-lock.md`.
- **NNN** — a `> [!NOTE]` block in the spec that owns the subject, for a
  justification that would otherwise let someone build a plausible wrong
  variant. The incidents behind the invariants are the main body of this: the
  ctp bugs, the two traps in fitting a quota window, why a cold notice is
  past-tense.
- **AGENTS.md** — rules for changing the code, which specs must not carry:
  `internals/README.md`'s Layout table, `measuring.md`'s "Verification, and its
  limits" and "Control bytes in source", `migration.md`'s setup narration.
- **Cut** — point-in-time material, ctp comparisons kept only as history, and
  anything whose only reader was someone tracking how the code got here.

### The tree

```
.workhorse/specs/
├── overview.md          TOK     notation, external documents, terminology
├── security.md          SEC     the properties, each naming its mechanism
├── frontend/                    what Toker exposes to clients
│   ├── overview.md      FRONT   the protocols and their paths
│   ├── profiles.md      FPROF   profiles and the /f/ prefix
│   └── administrative.md FADMIN model listings, token counting, batches
├── wire/                        what goes out to a provider and comes back
│   ├── canonical.md     WCANON  rendering, the licensed transformations
│   ├── translation.md   WTRANS  Messages <-> Responses, purity
│   └── credentials.md   WCRED   what is kept, removed and added
├── routing/                     which provider serves a request, on which model
│   ├── overview.md      ROUTE   providers, dialects, bindings, scope
│   ├── document.md      RDOC    the collections, parsing, validity
│   ├── revisions.md     RREV    draft, activation, history
│   ├── resolver.md      RSOLV   a route, prefixes, lookup, the walk
│   ├── fallback.md      RFALL   which failures fall back, commitment
│   ├── pins.md          RPIN    revision selection, transition consent
│   ├── compaction.md    RCOMP   where a cold compaction is sent
│   ├── models.md        RMODEL  learning, election, upgrades
│   ├── publication.md   RPUB    what a profile is shown
│   ├── provenance.md    RPROV   what a routed row records
│   └── migration.md     RMIG    routing moving from the file into the store
├── gates/               G       quota, cold, notices, compaction detection,
│                                forecasting, pinging
├── store/               S       ledger, lanes, attempts, lineage, cost
├── service/             D       lifecycle, config, control
└── cli/ tui/            U       commands, setup, readers, dashboard
```

### Root spec

`.workhorse/specs/overview.md` is a meta spec and nothing else: one sentence
saying what toker is, the BCP 14 notation paragraph worded for the whole
corpus, an `## External documents` table (Anthropic Messages, OpenAI Chat
Completions, OpenAI Responses, SSE, SQLite, systemd socket activation, BCP 14),
and the global `## Terminology

The code draws these distinctions well; the internals prose is what blurs them.
`BackendBinding` carries `{ id, provider, protocol, dialect, cost_class,
cache_domain, canonical, administrative }`, with `provider` a separate field
matching `[providers.X]`.

Settled:

- **frontend profile** (short form *profile*) is a named variant of toker's
  frontend API, selected by `/f/<name>`, tuned to make a particular client's
  life easier. It picks the notice style, lands on `extra.frontend`, and scopes
  policy. An unprefixed request is the `unprefixed` profile.
- **protocol** is the wire: Anthropic Messages, OpenAI Chat, OpenAI Responses.
  Where the prose says "the OpenAI frontends" it means these, and says so.
- **provider** is the configured upstream account and credential, one
  `[providers.X]` block: `anthropic_sub`, `anthropic_api`, `openrouter`,
  `openai_api`, `codex_sub`.
- **backend adapter** is the upstream wire implementation: `anthropic_messages`,
  `openai_chat_completions`, `codex_responses`, `openrouter_responses`.
- **binding** is a verified declaration pairing a provider with a protocol and
  dialect, carrying its capabilities and cost class. Closed static topology, not
  inferred from an endpoint's spelling. A route resolves onto one.
- **canonical** means parsed to IR and deterministically re-rendered, rather
  than forwarded verbatim.

`routing.md` heads a column "Backends" over a list of providers, which is the
kind of error the rewrite is for.

- **dialect** is a provider's concrete rendering and interpretation rules
  within a protocol: `anthropic_messages`, `openrouter_messages`,
  `openrouter_chat_completions`, `openrouter_responses`,
  `openai_chat_completions`, `codex_responses`. A dialect is never inferred
  from a protocol alone. Distinct from backend adapter, which has four
  variants to dialect's six.

**claim** is borrowed, not coined. It comes from Anthropic's
`anthropic-ratelimit-unified-representative-claim` header, whose values are
window names such as `5h`. It goes in a `## Borrowed terms` table in the quota
spec citing that header, never in the root glossary.

### Compound terms

A term may be more than one word, and a shared head noun is not a conflation.
*Inventory generation* (a deterministic digest of the catalogue snapshot, so a
second process can detect that the cache moved) and *compaction generation*
(the count of continuation preambles, the TUI's `cmpct` column) are two terms
that happen to share a head, the way sea water and sea foam do. Both get
glossary entries; the bare head noun never appears on its own.

The same shape covers *quota outlook* and *cold outlook*, *lane pin* and
*release marker*. Checked and sound: *release* (the grant) against *allowance*
(the row recording it), *offer* against *inventory*, and *meter*, *recap*,
*election*, *promotion*, *lane*, *attempt*, used consistently throughout.

### Citing specs from code

Citations go at the rule they implement, with a `#section` anchor, not in a
module header: `// spec: ROUTE` atop `routing.rs` says nothing the filename
does not. The six `//!` comments currently pointing at `docs/internals/` paths
are deleted rather than converted, and citations are placed where a reader
auditing a requirement would want them.

### Sweeping the comments

The citation pass is also a cleanup pass over every comment in the code, done
in the same sweep.

**Plan references go.** 143 lines mention a plan, mostly as `(plan: "…")` in a
module header. A plan is deleted when its work lands, so a comment naming one
points at nothing almost immediately. This is a standing rule and not specific
to this card.

**Numbered invariant references go.** 219 lines reference an invariant, most by
number: `invariant 3` alone appears 81 times, `invariant 4` 36 times. The
invariants dissolve into the specs that own their subjects, so the numbering
stops existing and every one of these becomes a `// spec: ID#section` citation
or is rewritten to say the thing directly.

Where a comment records a specific bug that was paid for, the comment stays;
only its reference to a plan or an invariant number changes.

### Sweeping names into code

Code identifiers follow the settled terms, as their own commit, with
`cargo test` run after. The compiler catches renamed identifiers; test fixtures
fail quietly, so the vendored ctp reference fixture
(`tests/fixtures/ctp/node-reference-v1.json`) and the insta snapshots need
checking by eye. The code is already close to the settled terms, so the sweep
is expected to be small.

## Ids

Three to six characters, uppercase. The first letter names a family, as a loose
convention: it makes a citation readable without looking the spec up.

A family is not a folder. `cli/` and `tui/` are both interfaces and share `U`,
so a later GUI joins the family wherever it sits. Unprefixed means the spec is
about what toker is, rather than a part of it, which is why the wire specs
carry no letter: prefixing them with a proxy letter would say nothing, since
the whole project is a proxy.

| Family | Prefix |
| --- | --- |
| What toker is, routing included | none |
| Gates | `G` |
| Routing policy | `R` |
| The store | `S` |
| Daemon | `D` |
| Interfaces | `U` |

| Spec | Id |
| --- | --- |
| `overview.md` | `TOK` |
| `security.md` | `SEC` |
| `routing/overview.md` | `ROUTE` |
| `routing/addressing.md` | `ADDR` |
| `routing/credentials.md` | `CRED` |
| `routing/cost.md` | `COST` |
| `routing/administrative.md` | `ADMIN` |
| `routing/canonical.md` | `CANON` |
| `routing/translation.md` | `TRANS` |
| `gates/overview.md` | `GATE` |
| `gates/quota.md` | `GQUOTA` |
| `gates/cold.md` | `GCOLD` |
| `gates/notices.md` | `GNOTE` |
| `gates/compaction.md` | `GCOMP` |
| `gates/forecasting.md` | `GCAST` |
| `gates/pinging.md` | `GPING` |
| `policy/overview.md` | `RPOL` |
| `policy/document.md` | `RDOC` |
| `policy/revisions.md` | `RREV` |
| `policy/resolver.md` | `RSOLV` |
| `policy/fallback.md` | `RFALL` |
| `policy/provenance.md` | `RPROV` |
| `policy/models.md` | `RMODEL` |
| `policy/publication.md` | `RPUB` |
| `store/overview.md` | `STORE` |
| `store/ledger.md` | `SLEDGR` |
| `store/lanes.md` | `SLANE` |
| `store/attempts.md` | `SATT` |
| `store/lineage.md` | `SLIN` |
| `store/migration.md` | `SMIG` |
| `service/lifecycle.md` | `DLIFE` |
| `service/config.md` | `DCONF` |
| `service/control.md` | `DCTRL` |
| `cli/commands.md` | `UCLI` |
| `cli/setup.md` | `USETUP` |
| `cli/readers.md` | `UREAD` |
| `tui/dashboard.md` | `UTUI` |

`SLEDGR`, `SLANE` and `SLIN` all open `SL`, though their lengths separate them.
`SEC` is unprefixed but opens with the store's letter, which the unprefixed
convention has to carry.

The prefix is a reading aid and not a guarantee. An id never changes, but a
family can be redrawn, so a spec that moves keeps a letter that no longer
matches. That is accepted rather than fixed, because renaming an id breaks
every `// spec:` citation pointing at it.

> [!NOTE]
> This diverges from the spec-voice house style, which bans prefixed ids. Its
> rationale is about project prefixes such as `BLI-CHN`, which record that the
> specs were once part of something larger. A family prefix records live
> structure instead, and trades id stability for readability knowingly.

Settling the families moved four specs out of where a folder-shaped scheme had
put them. Gate notices are gate output, so `GNOTE`. Compaction detection feeds
the cold gate rather than accounting for anything, so `GCOMP`. Lanes, attempts
and lineage are accounting identity, which `store/ledger.md` already carries
sections for, so they join the ledger. `/_toker/` is service administration and
never reaches an upstream, so `DCTRL`.

## Where a cross-reference goes

A pointer to another spec belongs in the summary, where the spec says what it
does not cover. Trailing one at the end of a section puts another spec's
subject under that section's heading, where a reader looking for the section's
own subject meets it.

Watch for the shape that produces one: a section specifying how X is handled,
followed by a sentence saying Y is handled elsewhere. The contrast is usually
an absence too, so both the sentence and the pointer go, and the pointer comes
back in the summary.

## When a note earns its place

A `> [!NOTE]` survives only where cutting it would let someone build a
plausible non-interoperable or insecure variant. A note that explains why the
requirement above it is sensible, or restates its consequence, is cut: the
requirement already says the thing, and the surrounding tables usually show it.

Worked through on the pilot. The note under the dialect rule said that two
providers on one protocol diverge, so guessing the rules produces a refused
body. That is the consequence of the MUST NOT above it, and the dialect table
already shows six dialects over three protocols, so it went. The note under the
credential table stayed: without it a re-implementer forwards a bearer the
client supplied, because nothing else says that a Messages bearer is the
subscription's and that Claude Code sends a placeholder one.

## Capitalisation

Toker is an ordinary name and takes ordinary capitalisation. `toker` lowercase
where it is a token the machine reads: the command, the crate, `toker.toml`,
`/_toker/`, `x-toker-*` headers, `~/.config/toker/`, Rust identifiers. `Toker`
capitalised in prose and in TUI chrome.

In markdown the backtick is the signal, so the rule applies mechanically:
inside backticks it is lowercase, outside it is capitalised. Rust doc comments
have no such marker and need reading, since a bare `toker` there may be either.

The repo is currently lowercase throughout, so the sweep covers README.md (8
mentions), AGENTS.md (20), the module doc comments, and any TUI display string.
Binary names and paths in code, such as `reexec.rs`'s `toker` and `toker.new`,
stay as they are.

## House rules settled in review

Each was a correction to one sentence that turned out to be a rule about a
class of them, and was swept across the corpus.

### Do not enumerate what describes itself

A parent that lists its children restates what each child already says, and
grows every time one is added. The directory is the list.

`RDOC`'s validation section named all nine checks it performs. One of them,
that shadowing a native model is consented to, was already a MUST twelve lines
above in the same spec, which is the proof: a check is a requirement stated
where its subject is. The section now says validation reports every
requirement of the spec the document fails to meet, and names none. New
requirements are covered the day they are written.

The checks that had no home only lacked one because the split dropped the
routes-and-candidates section. Restored, it carries its own constraints: a
route reference supplies a value per declared parameter, a candidate's required
capabilities are within its binding's declared ones, and so on.

`GATE` had a table of which sibling supplies what evidence, which is an index
of its own folder, and which was redundant with "only a gate stops a session"
at the top of the same spec. `STORE` enumerated the tables beside the ledger,
when the point is that everything other than the ledger is read-modify-write.

Enumerate only where the list is the requirement: where the order is
load-bearing, where closing the set is the contract, or where nothing else
describes the items. `RSOLV`'s content-free facts and `RPROV`'s row contents
stay listed for that reason; `UCLI`'s commands stay because no spec describes
them individually.

This is the correction to the previous entry's fix. Turning a 465-character
sentence into a table removed the long line and kept the enumeration.

### One sentence per line is not one sentence per paragraph

Consecutive lines with no blank line between them are one paragraph in
Markdown. A paragraph of four sentences is four lines and renders as one block.
The line breaks are a diff convention and carry no meaning to a reader.

Reading the rule as one sentence per paragraph produces two faults at once:
every sentence stranded as its own paragraph, and sentences fused with commas
and subordinate clauses to keep related ideas together on one line. The corpus
had both. 56 sentences were of the form "X MUST A, and MUST B", which is two
requirements in one sentence and splits cleanly. Six were enumerations written
as prose, the worst at 465 characters, and became tables or numbered lists.

A requirement with a purpose clause ("X MUST A, so that B") is ordinary prose
and was left alone. The test is whether the sentence carries two requirements
or a list, not its length.

The rule as stated in the spec-voice skill was ambiguous enough to invite the
misreading, and has been corrected there.

### Requirements bind the system, not the reader

A requirement that the reader not misunderstand something is not a
requirement. "A row carries three model identities, and they MUST NOT be
conflated" names no actor and constrains no behaviour: the terms belong in a
glossary and the rules about which may be read for what carry the force. The
same sentence appeared in `GCOLD` about its two clocks and in the routing specs
about cost kinds, where it also duplicated a claim the ledger owns.

Distinguish it from a genuine default, which looks similar: "a binding with no
cache domain MUST NOT be treated as uncached" tells an implementation what to
do with a missing value.

### A heading is a noun phrase

A heading names its section's subject. It is not a question the section
answers, and not a stand-in for the subject's name.

`## What is learned` became `## Learned evidence`, fourteen headings in all.
Then the interrogatives, which are the same defect in a different word:
`## Which identity is authoritative` became `## Identity authority`, and nine
more followed. "Which" with its noun is not exempt; the test is whether the
heading is a question.

Renaming exposes positional references. A question-shaped heading invites prose
that points at it by position ("the first question above"), which loses its
antecedent once the heading is a noun, and naming the thing is better prose
anyway.

Still clauses rather than noun phrases, and not yet converted: the conditional
scope headings (`## When a window has restarted`) and the declarative
assertions (`## A notice is byte-stable`).

### Do not refute an objection nobody had

A note that raises a concern only to deny it plants the concern. If the note
has to introduce the idea in order to reject it, the idea was not there.

The test: would a reader have had this thought before reading the note? A note
naming a failure a re-implementer would actually hit is doing its job; one
answering an interpretation only the note proposes is not. The fix is the
positive statement, which is usually shorter and often normative. "A
candidate's alias target is an opaque destination rather than evidence of
capability or price" became "A candidate's alias target is an opaque
identifier", in the body rather than a note.

Five went this way, including a note calling a behaviour "not a defect", one
calling a verdict "a measurement, not an absence of one", and one in `DLIFE`
that raised the operator's browser to justify binding loopback. The browser
belongs to the control spec, where it is the threat the gate exists for; the
real reason loopback matters is that Toker signs with credentials it holds.

This is the inverse of the note test already recorded. That one asks whether a
note prevents a wrong build; this one asks whether anyone would have built it.

There is no lexical tell for it. Scanning for notes whose vocabulary is foreign
to their requirement flags nearly every good note, because a note that earns
its place is precisely one introducing a mechanism the requirement does not
state.

### Do not legislate someone else's API

A spec may state how Toker behaves. It may not state how a third party's API
behaves, beyond what has been verified against the providers Toker serves.

"An upstream charges a cached read only where the bytes it holds match the
request's prefix exactly" is accurate for two providers and is not a property
of prompt caching in general. Four cache-keying claims were scoped this way,
and one was better grounded in Toker's own `cache identity` than in any
provider's behaviour. Claims already hedged with "may" were left: they lean
toward giving the provider more latitude, which costs nothing.

The shape reaches past providers, to clients and to terminals.

### Glossaries are scoped

A spec defines the terms it owns, in its own `## Terminology` section. The root
glossary is not the only one, and a term whose subject is one spec's belongs
there rather than at the root, where a reader meets it away from the rules that
use it. Another spec links a term by its anchor on first use.

`RMODEL` defines requested, effective and served model. Three specs had drifted
to "reported" and "sent" for two of them, which only the move surfaced:
distance from the usage is what lets usage drift.

### The store is not the ledger

The **store** is the SQLite database. The **ledger** is the append-only record
of requests inside it. The store also holds the lanes, the policy revisions,
the learned models, the allowances, the ping runs and the meter snapshots, and
none of that is the ledger.

The repo conflates the two, AGENTS.md included, and the first draft inherited
it: policy revisions were described as living "in the ledger", which would mean
appending a routing document to the record of requests. Around twenty sites
were corrected. The folder became `store/` and `schema.md` became `ledger.md`,
so the structure states the relationship.

### An overview carries little

An `overview.md` holds what is cross-cutting and nothing a sibling could own.
A folder with more than three siblings needs one.

`gates/` and `store/` had none, so their material sat in a sibling: the store's
own description was inside the ledger spec, and four specs each restated that
only a gate may stop a session because there was nowhere for the claim to live.
A missing overview is not just absent orientation; it forces a
one-home-per-claim violation across the family.

The routing overview had the opposite problem at 349 lines, carrying the
document, the lifecycle, the resolver, fallback and provenance. Split, it keeps
the topology and the shared terms. The signal that it worked: nothing links to
it any more, because every cross-reference found a more specific home.

Splitting reliably produces wrapper sections, where a spec repeats its own
title as a heading. That is an artefact of lifting a section out of a parent,
where the heading did real work.

### Routing had no home, so it leaked

The first pass made routing unprefixed, reasoning that toker is a proxy so a
proxy prefix says nothing. Wrong twice: routing is a core part of Toker and not
the whole of it, and treating it as the whole meant everything near it got
swept in.

The audit found routing in two folders, with five non-routing specs in one of
them. The policy family was routing, prefixed `R` for routing policy from the
start. And `routing/` held cost, credentials, the administrative paths,
canonical rendering and translation, none of which decides where a request
goes.

It had leaked outward too. `SLANE` carried revision selection and the
transition decision because the pin is stored on the lane. The migration spec
carried the first revision and ownership because it lives in the store.
`GCOMP` carried retargeting because the gate proves the lane cold. Each put a
routing decision in the spec that happens to hold its data.

Three subjects got their own areas. `routing/` is the decisions, `R` prefixed.
`frontend/` is what Toker exposes: the protocols, the profiles and the `/f/`
prefix, the administrative paths. `wire/` is what goes out and comes back:
rendering, translation, credentials. Cost went to the store as accounting.

The test that stuck: a spec holds the subject it owns, not the subject whose
data it stores.

## Rollout

All 26 specs are written: 3195 lines against 4447 of internals prose. Every
file link and every heading anchor resolves.

Cut rather than carried, throughout: the ctp comparisons, the dated live-probe
narratives (a probe is evidence for a capability claim, and the claim is what
the spec states; the date belongs with the code's `VERIFIED_ON`), the
maintainer guidance about which optimisations not to attempt, and the
repository layout.

### What remains

- The code sweep: `// spec:` citations at the rule, the plan and invariant
  references, and the identifier renames
- The whole-repo markdown reflow, and the AGENTS.md rewrite
- CI, carrying the test suite and the link and anchor check
- Verifying the external documents table's URLs and titles
- `store/ledger.md` held as one spec at 164 lines, so the question of
  splitting it did not arise

### Where the spec leads the code

`GPING` requires that no ping be scheduled where no configured provider anchors
a window to its opening request. The ping verb is already scoped to the
subscription, bounding both its skip decision and its readback, but setup is
not: the wizard offers and installs the wake, hold and ping timers on the
strength of what is already installed, without consulting the configured
providers. A machine running only openrouter or codex_sub can therefore wake on
weekdays to send pings that open nothing. Filed as #K1.

Scoping a spec to the provider behaviour it depends on is worth doing wherever
a topic reads as general and is not. Window anchoring is the clearest case, and
the meter and cost rules in `ROUTE` are already per-provider tables.

### Cards filed from this work

- #K1, the ping timers being offered where no provider anchors a window
- #L1, removing the config file. The refactor behind toker's current shape
  intended configuration to live in the ledger and be managed from the UI.
  Routing made that move and nothing else did, so `toker.toml` is still 2642
  lines of parsing over 63 fields. `DCONF` and `DLIFE` describe the file as it
  stands today and will need revising when that card lands.

### Note density

204 notes across the corpus, and they are not evenly spread. `ROUTE` carries
one in 190 lines; `RMODEL` nineteen in 196, `SLANE` sixteen in 217, `DLIFE`
fifteen in 121.

The split tracks what kind of knowledge a topic holds rather than how carefully
it was written. Topology specs describe arrangements a reader cannot get wrong
for long, because the wrong arrangement fails immediately. The learning,
caching and lifecycle specs encode measurements where the wrong implementation
works, looks right and is quietly false, and there each note names the specific
wrong build a reader would otherwise produce.

Audited on conversion, two kinds were cut: a note restating the consequence of
the requirement above it, and a note repeating a claim another spec owns.
`GCOLD` lost a closing section that cross-referenced `GNOTE` and then restated
`GNOTE`'s own past-tense requirement. Still worth a second opinion, since the
test is meant to be strict and the density is high.

## Open questions

- [ ] The area tree: how the subjects group into directories. Three joints are
      uncertain: whether `routing.md` splits three ways, whether `session/` is
      a real grouping, and whether `store/ledger.md` wants splitting once
      converted.
- [ ] Confirm a link and anchor checker goes in as a test under `tests/`, since
      there is no CI and deep links break silently when a heading is reworded.
- [ ] What `dialect` is, precisely, and whether it needs a glossary entry.

## Trade-offs

Dissolving the invariants follows one-home-per-claim and puts each requirement
where a reader looking for it would go, at the cost of the numbered handle:
"invariant 4" is something you can say in review, and after the dissolve it is
not. The security spec recovers some of that by gathering the properties in one
readable place.

## Testing notes

- `cargo test`, `cargo clippy --all-targets` and `cargo fmt --check` still pass
  after the code citations are swept.
- Every link and heading anchor in the corpus resolves.
- No RFC 2119 keyword inside a note block, and no note inline on a normative
  sentence.
- Every coined term is defined before any spec leans on it.
