---
id: RCTL
---

# Live routing configuration

Toker applies completed routing edits immediately and retains every applied
configuration as an immutable revision.

## Applying configuration

- [ ] The routing TUI applies a completed configuration edit without restarting the proxy.
- [ ] The service validates and compiles a configuration before replacing the active revision.
- [ ] A configuration that cannot compile leaves the active routing configuration unchanged and identifies the invalid policy records.
- [ ] A successful configuration edit creates a new immutable revision with the previously active revision as its parent.
- [ ] Requests already in progress continue with the routing revision captured when they began.
- [ ] Existing lanes retain their pinned routing target according to the configured lane transition policy.
- [ ] A successful configuration edit records desired frontend publication state without reverting active routing when an external publication fails.

## Revision history and restore

- [ ] The routing TUI shows the active revision and prior applied revisions.
- [ ] A prior routing configuration can be restored without restarting the proxy.
- [ ] Restoring a configuration creates a new immutable revision whose parent identifies the restored revision.
- [ ] Restoring a configuration does not modify existing revisions.

## Routing control API

- [ ] The loopback control API exposes routing state, configuration apply, revision history, revision restore, binding-scoped promotion, and publication-result operations.
- [ ] Each routing control operation requires its own `x-toker-control` value.
- [ ] Routing control mutations require a JSON request with an operation-appropriate bounded body.
- [ ] Routing control responses and diagnostics exclude credentials, prompts, completions, tool arguments, and upstream error bodies.

## Binding-scoped operations

- [ ] A manual model promotion identifies both its binding and provider-owned model identity.
- [ ] A stale publication acknowledgement cannot replace a newer desired publication state.
