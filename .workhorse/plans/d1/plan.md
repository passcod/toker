# Routing TUI editing and activation

The current branch has the authoritative policy runtime and a narrow routing
control surface, while `toker tui` remains the dashboard-only loop. Build the
missing control lifecycle first, then make the TUI a page-driven client of that
lifecycle. The daemon remains the sole owner of draft generations and policy
activation.

## Control lifecycle

- [x] Expand `src/server/control/routing.rs` and its server/store integration
  from direct apply/restore operations to draft replacement and typed
  generation-CAS edits.
- [x] Return draft state, compile diagnostics, offer inventory, publication
  state, revision summaries, and lane-impact data from the routing snapshot.
- [x] Add validation and pure preview endpoints with stable, content-free
  diagnostics and trace data.
- [x] Make activation consume the current draft with a selected activation
  scope and explicit warm-cache override, and make reactivation create a new
  revision without moving the active pointer backwards.
- [x] Invoke Claude settings reconciliation after a successful activation while
  leaving the active server policy intact if reconciliation fails.

## Routing page framework

- [x] Pass the configured loopback endpoint from `cmds::tui` into `tui::run`.
- [x] Add a bounded background routing-control client that never blocks the
  terminal event loop while the daemon is slow, unavailable, or restarting.
- [x] Split top-level TUI state into dashboard and routing pages; add the
  header action, `Tab` toggle, page-specific legend, mouse hit map, and
  independent routing refresh cadence.
- [x] Add a routing read model and views for presentations, routes and
  candidates, rewrites/families/upgrades, and preview/revisions/activation.

## Editing and promotion flows

- [x] Add typed modal forms for policy records, matcher and enum selections,
  offer completion, candidate ordering, removal confirmation, and explicit
  billed-boundary confirmation.
- [x] Preserve local draft edits across a CAS conflict, show the daemon copy,
  and support reload or field-level typed reapplication.
- [x] Add preview trace presentation with only content-free request facts and
  routing decisions.
- [x] Add activation, forced warm-lane movement, and reactivation confirmation
  flows, including changed presentations/routes, lane impact, cost boundaries,
  and publication status.
- [x] Add binding-aware learning promotion from the routing page.

## Dashboard routing provenance

- [x] Extend the operational dashboard projection to group only explicit
  parent-child session lineage while retaining distinct child lane/cache rows.
- [x] Render requested, effective, and provider-reported model identity
  separately, rendering a missing reported identity as `-`.

## Coverage and finalisation

- [x] Cover control operation gates, body schemas and limits, draft CAS,
  validation/preview projections, activation/reaction semantics, and
  publication failure isolation.
- [x] Cover routing model transitions, keyboard/mouse page navigation, forms,
  conflict recovery, confirmations, and the dashboard lineage/model projection.
- [ ] Run the final routing review round after all card behaviour is complete.
