# Routing TUI editing and activation

## Control lifecycle

- [x] Draft replacement rejects a stale generation and returns the daemon draft.
- [x] Typed draft edit updates only the requested policy field and advances the generation.
- [x] Draft validation returns stable diagnostics without changing active routing.
- [x] Preview uses only content-free request facts and reports match, selection,
  skips, upgrade, cost transition, and final binding.
- [x] Activation retains a compiled policy revision captured by requests already
  in progress.
- [x] A reactivation creates a new active revision whose parent is the selected
  historical revision.
- [x] An activation scope preserves warm pins unless an explicit cache-safe
  transition permits movement.
- [x] Forced warm-lane movement requires explicit confirmation.
- [ ] A failed Claude settings reconciliation leaves the activated server policy
  in place.

## Routing terminal

- [ ] `Tab` and the header routing action switch between dashboard and routing
  pages without losing loaded draft or validation state.
- [ ] Routing refresh failure does not block dashboard input or terminal redraw.
- [ ] A presentation, route, rewrite/family/upgrade, and revision/activation
  view are each navigable with keyboard controls.
- [ ] A policy form supports text, enum, matcher, offer completion, and ordered
  candidate editing.
- [ ] Removal, billed-boundary fallback, activation, reactivation, and forced
  cache movement each require confirmation.
- [ ] A draft conflict preserves local edits and supports reload or typed
  field-level reapplication.
- [ ] Promotion identifies a binding and model before recording learning.

## Dashboard provenance

- [ ] Explicitly linked child sessions aggregate beneath their display parent
  while their lanes remain separate.
- [ ] A child detail uses its parent's transcript title and working directory
  while showing the child raw id and role.
- [ ] Unlinked sessions remain separate.
- [ ] The model detail renders requested, effective, and provider-reported
  identities, with `reported -` when the provider omitted it.
