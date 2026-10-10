# Anthropic subscription signing: on-host test

Status: implementation complete; the real-subscription checks below have not
yet been run on a suitable host.

This is a manual verification script for a machine with a Claude Code login.
It remains after the mux implementation plan because it records on-host
verification, not unfinished implementation. Run it only when a small real
subscription request is acceptable and no live session will be disrupted by
installing or restarting toker.

## Before changing the service

1. Confirm the current toker service and Claude Code are working. Save a copy
   of `toker.toml` and note its mode; do not put a token in a world-readable
   file. Use `toker status` to confirm `anthropic_sub` is enabled.
2. Confirm Claude's login exists at `$CLAUDE_CONFIG_DIR/.credentials.json`, or
   `~/.claude/.credentials.json` when `CLAUDE_CONFIG_DIR` is unset. Do not print
   the file or its token. If Claude stores its login only in a keychain on this
   host, use the configured toker token source instead and record that the
   file fallback could not be exercised.
3. Build and install this revision, then use `toker restart` to let the
   service drain. Do not point any frontend at a listener that is not ready.

## Login fallback

With no `oauth_token` literal, `oauth_token_env` value, or configured
`oauth_token_keyring` entry, send one synthetic request through the local
Responses route. Replace `<model>` with a model the subscription accepts:

```sh
curl --fail-with-body --no-progress-meter \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer foreign-test-token' \
  -d '{"model":"anthropic_sub/<model>","input":"Reply with hello.","max_output_tokens":64,"stream":false}' \
  http://127.0.0.1:18123/f/codex/v1/responses
```

Use the configured port if it is not 18123. The expected result is a Responses
JSON completion. A 401 with the provider's own error means the local login was
unavailable, expired, or not accepted; do not infer success from the route
existing. A local 400 means translation or the output limit failed before
the provider was reached. Confirm the ledger route is
`openai_responses:anthropic_sub`, cost kind is `plan_equivalent`, and no
foreign bearer or prompt content appears in logs or rows.

## Toker-held precedence

Configure `oauth_token_env = "TOKER_ANTHROPIC_SUB_TOKEN"` with a separate valid
token visible to the service, or set the `anthropic_sub` keyring entry and
`oauth_token_keyring = true`. Restart after changing the config. Repeat the
synthetic request. Where the two token sources identify distinguishable
accounts, verify the toker-held account was charged, not Claude's local login.
Do not print either token or paste a real prompt into the test. Remove the
temporary token source afterward and restart to confirm the Claude fallback
works again.

## Native pass-through and failure cases

Run a normal Claude Code turn through `/f/claude/v1/messages`; it should keep
using Claude's own bearer and should not require a toker-held token. Then test
an absent or expired local login *only after* removing the toker-held source:
a foreign Responses request must not succeed with its dummy bearer. Finally,
check a streamed request with a tool call; it should emit Responses tool-item
events, a terminal event, and a row with provider-observed usage. Restore the
original config and verify normal frontend traffic before ending the test.
