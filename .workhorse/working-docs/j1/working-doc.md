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
├── overview.md          notation, external documents, terminology
├── security.md          the properties, each naming its upholding mechanism
├── routing.md           protocols, providers, bindings, the /f/<name> prefix
├── canonical.md         canonical rendering, what may alter a body
├── translation.md       Messages <-> Responses, purity, prefix stability
├── gates/
│   ├── quota.md         windows, weights, releases, allowances
│   ├── cold.md          the cold-cache gate, two clocks, the recap
│   ├── notices.md       notice styles per profile, byte stability
│   ├── compaction.md    detecting a compaction, the wordings
│   ├── forecasting.md   projecting a window, totalling a span
│   └── pinging.md       opening a window buys phase
├── policy/
│   ├── overview.md      the document, lifecycle, the resolver, fallback
│   ├── models.md        learning, election, upgrades, context ceilings
│   └── publication.md   what each profile lists, generations, Claude's settings
├── ledger/
│   ├── schema.md        row kinds, fields, old rows, imported rows
│   ├── migration.md     append-only migrations, revision one, activation
│   ├── lanes.md         lane identity, cache identity, pins
│   ├── attempts.md      a logical request and its attempts
│   └── lineage.md       what counts as evidence of descent
├── service/
│   ├── lifecycle.md     socket activation, drain, restart, the sleep lock
│   ├── config.md        toker.toml: providers, credential sources, the listener
│   └── control.md       /_toker/ endpoints and the header gate
├── cli/
│   ├── commands.md      the CLI surface
│   ├── setup.md         the wizard, frontend patching, units
│   └── readers.md       import, export, watch-context-window
└── tui/
    └── dashboard.md     views, session detail, gate controls
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

**dialect** (`DialectId`) is a fourth level under protocol, not yet understood
well enough to gloss. Check it during the pilot.

Coined terms still to check for the same defects: *release*, *allowance*,
*claim*, *outlook*, *recap*, *generation*, *election*, *promotion*, *offer*,
*inventory*, *meter*, *pin*, *lane*, *attempt*.

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
| What toker is | none |
| Gates | `G` |
| Routing policy | `R` |
| Ledger and accounting | `L` |
| Daemon | `D` |
| Interfaces | `U` |

| Spec | Id |
| --- | --- |
| `overview.md` | `TOK` |
| `security.md` | `SEC` |
| `routing.md` | `ROUTE` |
| `canonical.md` | `CANON` |
| `translation.md` | `TRANS` |
| `gates/quota.md` | `GQUOTA` |
| `gates/cold.md` | `GCOLD` |
| `gates/notices.md` | `GNOTE` |
| `gates/compaction.md` | `GCOMP` |
| `gates/forecasting.md` | `GCAST` |
| `gates/pinging.md` | `GPING` |
| `policy/overview.md` | `RPOL` |
| `policy/models.md` | `RMODEL` |
| `policy/publication.md` | `RPUB` |
| `ledger/schema.md` | `LROWS` |
| `ledger/migration.md` | `LMIG` |
| `ledger/lanes.md` | `LLANE` |
| `ledger/attempts.md` | `LATT` |
| `ledger/lineage.md` | `LLIN` |
| `service/lifecycle.md` | `DLIFE` |
| `service/config.md` | `DCONF` |
| `service/control.md` | `DCTRL` |
| `cli/commands.md` | `UCLI` |
| `cli/setup.md` | `USETUP` |
| `cli/readers.md` | `UREAD` |
| `tui/dashboard.md` | `UTUI` |

`LLANE` and `LLIN` are the weakest pair: both open `LL` and sit on adjacent
subjects. Watch for one being cited where the other was meant.

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
and lineage are accounting identity, which `ledger/schema.md` already carries
sections for, so they join the ledger. `/_toker/` is service administration and
never reaches an upstream, so `DCTRL`.

## Open questions

- [ ] The area tree: how the subjects group into directories. Three joints are
      uncertain: whether `routing.md` splits three ways, whether `session/` is
      a real grouping, and whether `ledger/schema.md` wants splitting once
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
