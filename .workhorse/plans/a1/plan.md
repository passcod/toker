# External Claude settings reconciliation

## Review fixes

- [x] Append a migration that makes pre-v13 applied publications safely pending when their exact ownership set is unknowable.
- [x] Render every Anthropic profile targeting the shared Claude settings file and reject incompatible projections without writing settings.
- [x] Keep legacy `picker sync --dry-run` read-only when no state database exists.
- [x] Tie a picker acknowledgement to the cached catalogue snapshot that produced it, so it cannot acknowledge a newer in-memory inventory generation.
- [x] Add regression coverage and run formatting, tests, clippy, and a follow-up review.
