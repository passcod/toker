# Setup ownership and legacy import

## Setup activation

- [x] Fresh setup activates revision one from the wizard's choices before it
  writes a config, and the written config has no routing fields.
- [x] A revision-one compile or activation failure leaves the config file
  unwritten and the database without an active revision.
- [x] A config write failure after activation leaves the machine
  database-owned, and the next setup run writes the owned config.
- [x] Setup on a legacy machine with no active revision compiles the existing
  model maps and picker rules together with the newly chosen defaults.
- [x] A database that cannot be read stops setup before anything is written.

## Database ownership

- [x] Setup on a database-owned machine reports the active revision and asks
  no default-backend question.
- [x] A reconfigure on a database-owned machine drops legacy routing fields from
  the config, says so, and leaves the active revision unchanged.
- [x] Keeping the config as is leaves its legacy fields untouched.
- [x] Provider enable, endpoint, credential, and listener fields survive an
  owned rewrite.
- [x] Verification passes against the daemon serving the setup-activated
  revision before any frontend is patched.

## Import to draft

- [x] Import replaces the draft with the compiled current config after
  confirmation, and leaves the active revision unchanged.
- [x] Import with a stale generation takes the conflict path and keeps the
  daemon draft.
- [x] A config that fails to load leaves the draft unchanged and reports why.
- [x] An imported draft activates through normal validation and activation.

## Picker timer

- [x] Picker units are installed when a Claude frontend is detected, with or
  without an OpenRouter backend.
- [x] `toker picker sync` with no active revision fails without touching Claude
  settings.
- [x] Unit descriptions and sync output name Claude `/model` rows, not
  OpenRouter.
