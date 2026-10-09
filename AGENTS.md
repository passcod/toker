# Working on toker

A local proxy and ledger for LLM traffic: Claude Code, opencode and the codex
CLI talk to it on loopback, and it forwards to the configured backends
(anthropic subscription or API, openrouter, codex subscription) while recording
what each request cost, gating sessions that would spend quota or re-read a cold
cache, and showing it all in `toker tui`. `README.md` says what it does and how
to install it; `docs/plans/toker-toolsuite.md` is the design. This file is the
rules for changing it; [`docs/internals/`](docs/internals/README.md) has the
layout and, topic by topic, the reasons behind the rules. Read the topic before
changing the code it covers: most of it records a bug already paid for, much of
it in claude-token-proxy (ctp), the Node proxy toker replaces.

State lives in the SQLite ledger at the config's `db_path` (default
`$XDG_DATA_HOME/toker/toker.db`); its directory is the state dir, and the only
path the installed service may write. Lanes, learned models, allowances, pings
and meter snapshots are tables there, not files. Config is
`~/.config/toker/toker.toml` (`$XDG_CONFIG_HOME`, or `TOKER_CONFIG`), written by
`toker setup` and read by the service. Anything new the service writes goes in
the state dir, and tests get a temp directory, never the real ledger. The one
write outside it is the codex login refresh, which rewrites the codex CLI's own
`~/.codex/auth.json`; setup makes that directory writable in the unit only when
codex_sub is enabled (see [routing.md](docs/internals/routing.md)).

## Invariants

The full text and the incidents behind them are in
[invariants.md](docs/internals/invariants.md).

1. **Never log or store prompt or completion content.** Only counts, lengths,
   digests, and whether a fixed marker string matched.
2. **Credentials pass through, or are stored where the design says, and are
   never logged or printed.** A request that brings its own credential keeps it
   on a native provider wire. Foreign OpenAI credentials are removed before
   Anthropic signing; codex always signs itself. The anthropic models fetch
   may borrow a passing subscription bearer for one GET. A key
   handed to toker lives in the keyring, an env var, or as a literal in the
   0600 `toker.toml`. Request headers are read by name, never captured
   wholesale.
3. **Accounting must never break a session; only a gate may stop one.**
   Observation runs under `catch_unwind`, and recording logs and swallows its
   errors. Gate decisions go through `quota::decide` or `cold::decide_cold`, and
   every row goes through `Store::record_request`.
4. **Bind `127.0.0.1` only.** Control paths live under `/_toker/` and each
   demands its verb in the `x-toker-control` header, so a web page cannot drive
   one. Do not add one without the same gate.
5. **Never guess prices.** Verify against the published page and move
   `catalog::pricing::VERIFIED_ON`. Store cache-read rates explicitly.
6. **Every inference body is canonical-rendered; a legacy administrative body
   is the client's.** Before Messages rendering, only `strip_release`, the
   `provider/model` prefix strip, force-newest `set_model`,
   `retarget_compaction`, `strip_message_effort`, `thinking_between_tools`, and
   `rewrite_mapped_models` may alter semantics. Editing it anywhere else is a
   bug.
7. **Never pin a quota weight.** Weights are fitted from the ledger on every run
   and carry their spread; one the data cannot separate from zero is no weight.
8. **Gate notices are model-visible and byte-stable.** A notice enters the
   conversation and is replayed on every later turn, so its text is a pure
   function of its inputs. No em dashes: they render two cells wide.

## Changing the proxy

A change reaches live traffic only once the service runs the new binary;
forgetting looks like the change not working.

```sh
cargo install --path . && toker restart
```

The unit's `ExecStart` is whichever binary ran `toker setup`; if that was not
the cargo-installed one, re-run setup. `toker restart` waits for a quiet moment,
then has the service drain and exit for systemd to start the new binary, so no
stream is cut. If that does not work out in time (no quiet moment within 30s, a
drain that does not end) it asks the service again to stop without waiting for
connections: still a clean exit, with destructors run, but whatever was
streaming is cut. That is safe: the socket unit keeps listening so new
connections queue, and harnesses retry a cut stream, which is how restarts were
done with `systemctl --user restart toker.service` before `toker restart`
existed, and that still works too. See [routing.md](docs/internals/routing.md).

**Never point a frontend's `ANTHROPIC_BASE_URL` at a listener that is not up.**
`settings.json` is hot-reloaded into running sessions, so pointing it at nothing
kills every live session, including your own. `toker setup` verifies the
listener, and each frontend's `/f/<name>` prefix, before it patches anything; do
the same by hand.

## Verification

Run `cargo test`, `cargo clippy --all-targets` and `cargo fmt --check` before
committing. The server tests drive the real router against mock upstreams and
spend no quota. Rendered output is pinned by assertions, and increasingly by
insta snapshots: review a changed snapshot with `cargo insta review`, or accept
all with `INSTA_UPDATE=always cargo test`, and only once you have read the diff.
A changed notice snapshot is a changed model-visible byte.

A green run is not enough on its own. Bugs have shipped past it because the
tests confirmed the code did what had been written. So:

- **Run new analysis against a copy of the real ledger** (never the live file)
  and sanity-check the numbers before believing them.
- **Test that it stays quiet when it should.** Every instrumentation bug so far
  made a view *more* confident, never blank.
- **Absence of instrumentation must never read as absence of the phenomenon.**
  Rows imported from ctp, and older toker rows, lack later fields; say so rather
  than report a zero. See [ledger-schema.md](docs/internals/ledger-schema.md).
- **Compare within a lane, never across a session.** See
  [lanes.md](docs/internals/lanes.md).

## Conventions

- Use **jj**, not git. Commit as you work; bookmark `main` at the tip and leave
  an empty working commit on top.
- Comments explain *why*, especially where something was learned the hard way.
  Comments that record a specific bug stay.
- Prose lines wrap at 80 columns; tables and code blocks may exceed it.
- No raw control bytes in source; write the escape. A literal NUL once made
  ugrep treat a whole source file as binary and skip it silently.
- Command-line flags are parsed strictly through clap, so a mistyped flag fails
  rather than being ignored.
- toker is public (github.com/passcod/toker). ctp was private and shared with
  colleagues, so its docs could name clients and their repos; toker's must not.
  Use invented names in examples, comments, commits and fixtures.
