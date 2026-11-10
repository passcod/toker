# Working on Toker

Toker is a hobby project: one user, running it on a couple of machines.
Prefer the simplest change that does the job, and don't build compatibility layers, options or abstractions for users or deployments that don't exist.
The stores on those machines are real history, though, so migrations stay append-only and never lose rows.

Toker is a local proxy and ledger for LLM traffic.
Claude Code, opencode and the codex CLI talk to it on loopback, and it forwards to the configured providers while recording what each request cost, gating sessions that would spend quota or re-read a cold cache, and showing it all in `toker tui`.

`README.md` says what it does and how to install it.
`.workhorse/specs/` says what it must do.
This file is the rules for changing it.

## The specs

The specs are the normative description of Toker, in RFC 2119 voice.
Read the specs covering a subject before changing the code that implements it.

| area | subject |
| --- | --- |
| `frontend/` | the API Toker offers clients: protocols, profiles, administrative paths |
| `wire/` | what goes out to a provider and comes back: rendering, translation, credentials |
| `routing/` | which provider serves a request, on which model |
| `gates/` | what may stop a request, and the evidence it reads |
| `store/` | the database, the ledger of requests, and the state beside it |
| `service/` | running the daemon, its configuration and its control endpoints |
| `cli/`, `tui/` | the command surface and the dashboard |

`specs/overview.md` carries the requirements notation and the glossary.
`specs/security.md` gathers the properties the proxy upholds, each naming the spec that owns its mechanism.
Read it before touching credentials, logging, recording or the control endpoints.

Each spec carries a short id in its frontmatter.
Code points at the requirement it implements with `// spec: ID#section-slug`, at the rule rather than in a module header, where a reader auditing that requirement would want it.

Changing product behaviour means changing a spec first.
A bug fix does not, unless the bug existed because a spec was unclear or missing.

Other Workhorse artefacts live beside the specs: `.workhorse/plans/` for a card's implementation notes, `.workhorse/test-cases/` for its verification checklist, and `.workhorse/working-docs/` for drafting that has not yet split into those.
None of them is normative.

## State

State lives in the SQLite store at the config's `db_path` (default `$XDG_DATA_HOME/toker/toker.db`).
Its directory is the state dir, and the only path the installed service may write.
The ledger of requests, the lanes, the learned models, the allowances, the pings, the meter snapshots and the routing policy are all tables there, not files.

Config is `~/.config/toker/toker.toml` (`$XDG_CONFIG_HOME`, or `TOKER_CONFIG`), written by `toker setup` and read by the service.
It holds providers, credential sources and the listener; model routing lives in the store and is edited from `toker tui`.

Anything new the service writes goes in the state dir, and tests get a temp directory, never the real store.
The one write outside it is the codex login refresh, which rewrites the codex CLI's own `~/.codex/auth.json`; setup makes that directory writable in the unit whether or not `codex_sub` is enabled.

## Changing the proxy

A change reaches live traffic only once the service runs the new binary; forgetting looks like the change not working.

```sh
cargo install --path . && toker restart
```

The unit's `ExecStart` is whichever binary ran `toker setup`; if that was not the cargo-installed one, re-run setup.
`toker restart` waits for a quiet moment, then has the service drain and exit for systemd to start the new binary, so no stream is cut.
If that does not work out in time it asks the service again to stop without waiting for connections: still a clean exit, with destructors run, but whatever was streaming is cut.
That is safe, because the socket unit keeps listening so new connections queue, and harnesses retry a cut stream.
`systemctl --user restart toker.service` still works and cuts every live stream.

**Never point a frontend's `ANTHROPIC_BASE_URL` at a listener that is not up.**
`settings.json` is hot-reloaded into running sessions, so pointing it at nothing kills every live session, including your own.
`toker setup` verifies the listener, and each profile's `/f/<name>` prefix, before it patches anything; do the same by hand.

## Verification

Run `cargo test`, `cargo clippy --all-targets` and `cargo fmt --check` at the end of a batch of changes and before pushing.
CI runs the same three, plus a check that every link and heading anchor in the specs resolves.

The server tests drive the real router against mock upstreams and spend no quota.
Rendered output is pinned by assertions and by insta snapshots: review a changed snapshot with `cargo insta review`, or accept all with `INSTA_UPDATE=always cargo test`, and only once you have read the diff.
A changed notice snapshot is a changed model-visible byte.

A green run is not enough on its own.
Bugs have shipped past it because the tests confirmed the code did what had been written.
So:

- **Run new analysis against a copy of the real store**, never the live file, and sanity-check the numbers before believing them.
- **Test that it stays quiet when it should.**
  Every instrumentation bug so far made a view *more* confident, never blank.
- **Absence of instrumentation must never read as absence of the phenomenon.**
  Earlier rows lack later fields; say so rather than report a zero.
- **Compare within a lane, never across a session.**

## Conventions

- Commit as you work, one logical change per commit, with conventional-commit subjects (`feat(scope): …`, `fix`, `refactor`, `test`, `docs`).
  No `!` breaking-change marker.
- Comments explain *why*, especially where something was learned the hard way.
  A comment that records a specific bug stays.
- Comments do not reference plans, which are deleted when their work lands.
- Markdown is one sentence per line, with no hard wrapping.
  Consecutive lines with no blank line between them are one paragraph, so group sentences into paragraphs as you would in ordinary prose.
- Code comments wrap at 80 columns.
  The markdown rule above is for markdown only; a doc comment is prose in a source file and wraps like one.
- Toker is capitalised in prose and in TUI chrome, and lowercase as a command, a path, a header or an identifier.
- No raw control bytes in source; write the escape.
  A literal NUL once made ugrep treat a whole source file as binary and skip it silently.
- Command-line flags are parsed strictly through clap, so a mistyped flag fails rather than being ignored.
- Toker is public (github.com/passcod/toker).
  Its predecessor was private and shared with colleagues, so its docs could name clients and their repos; Toker's must not.
  Use invented names in examples, comments, commits and fixtures.
