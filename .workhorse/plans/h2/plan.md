# H2: issues found configuring a machine from scratch

Issues found while setting up a fresh machine, worked on this card rather than spread over new ones.
Several keys per provider is left for a card of its own.

## Done before this batch

- [x] Read a Responses answer with no content type as the stream it was asked for
- [x] Read the schema version under the write lock before each migration

## Store and service

- [x] Commands other than `serve` and `setup` load configuration from a store opened read-only, so the dashboard, `status` and `restart` never create or migrate a store (STORE#opening-the-store). A missing store reads as the configuration defaults.
- [x] The service checks that the store path still names the file it opened (device and inode) before each control write and on a timer, and stops when it does not, refusing the write (DLIFE#keeping-to-its-store).
- [x] A 2xx Responses answer refused for its content type records the content type it carried (or its absence) in the error detail.

## Setup

- [ ] Setup asks only the port and the schedule slots. On a store with no provider, it enables codex_sub where the codex login is present and each provider whose keyring entry exists, checking presence without reading the value; it never prompts for a credential (USETUP#writing). The key prompt, its retry loop and the keyring re-ask go with it.
- [ ] Setup reaches the system manager with plain `systemctl`, so polkit authorises, and never runs `sudo`.

## Routing form

- [ ] Duplicate a record from its table, opening the add form filled from it with a fresh id and, for a rewrite, the next free priority in its scope.
- [ ] Reference fields offer the records they can name as a pick list, each with enough to tell them apart, and still accept typing; the prefilled id no longer blocks completion.
- [ ] Choosing a route fills one argument per parameter it declares, `requested_model` included.
- [ ] The rewrite form generates the rewrite's id instead of asking for it.

## Verification

- [ ] `cargo fmt --check` and `cargo clippy --all-targets` at the end of the batch; tests left to CI.
- [ ] On chenini: delete nothing; install, restart, run `toker setup`, and check it enables codex_sub and openrouter from the login and keyring without prompting.
