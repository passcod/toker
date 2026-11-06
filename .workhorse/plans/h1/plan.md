# H1: routing follow-ups

The gaps against the live-model-routing design that F1 left open: the routing
page's derived content and typed forms, virtual Responses presentations,
declared context windows, legacy learning, the streaming untranslatable-body
shape, and attempt read models. Same prime directive as F1: two users, no live
fleet, take the expedient path.

The retired plans are in git at `9153f09^:docs/plans/` and the F1 audit at
`a94024e^:.workhorse/plans/f1/reconcile.md`. Paths below are on
`live-model-routing`.

## Decisions

- **Base.** All the routing work is on `live-model-routing` (A1 to F1 merged
  there), not `main`. H1 is based on it and its PR targets it.
- **Virtual Responses presentations** get policy-owned codex metadata.
  `AdapterMetadata::Codex` gains an optional typed model record carrying the
  fields the codex client's catalogue parser requires. A virtual Responses
  presentation that has one is published from it. One that lacks it is left
  out of the listing, with a publication detail code and a compiler
  diagnostic. A virtual presentation never makes the listing 503; a missing
  exact offer still does, as `publication.md` explains.
- **Declared context windows** are dropped from `learned_models_by_binding`.
  The context-fit check uses the inventory window, then the largest prompt
  served. `DeclaredWindow` stays for `watch-context-window`, which reads the
  old `models` table.
- **Legacy learning** is not carried over. `replay_attributable_binding_learning`
  goes. `allow_revision_one_legacy_learning` is no longer generated. Stored
  revision-one documents still decode and verify with it, since both
  `deny_unknown_fields` and the fingerprints see it. The old `models` table
  stays for `watch-context-window` only.
- **Unroutable requests** write a `route-unavailable` row for no-route and for
  a pin-denied move as well as for all-candidates-skipped. Each row carries
  lineage where the handler has it, which today is the Responses path.
- **Final outcome reader** (my call): the TUI window counts a group as a
  request whichever of its rows it sees first, including error and terminal
  rows. A group that only failed is therefore a request. A failed request is a
  group whose final row is an error or a terminal: the row with the greatest
  attempt index, or the terminal row when there is no attempt. The window
  shows failed requests beside the per-attempt error count, so a failure a
  fallback recovered from is visible as such.
- **Untranslatable bodies** (my call) write the row whether or not the request
  streamed. Only the response shape depends on `stream`. A body that is not
  JSON at all cannot say whether it streams, so it keeps the JSON 400; it does
  still get the row.

## Steps

### 0. Base

- [ ] Rebase `workhorse/h1` onto `origin/live-model-routing` and point the
      PR at it

### 1. Drop declared context windows

- [ ] Schema migration (next version after v16) dropping
      `learned_models_by_binding.context_window_json`, in `store/schema.rs`
- [ ] Remove `context_window` from `BindingLearning` and
      `BindingLearningUpdate` (`store/state.rs`), from the merge and the read
      (including its malformed-declaration error), and remove
      `LearnedEvidence::declared_window` (`policy/learning.rs`)
- [ ] `fits_context` (`policy/resolve.rs`) becomes inventory window, then
      `max_prompt`
- [ ] Update the tests that wrote a declaration: `store/mod.rs`
      (merge, malformed declaration, cross-connection atomicity),
      `policy/learning.rs`, `resolve.rs`
      `context_fit_prefers_inventory_then_declaration_then_empirical_maximum`,
      and the `learn` helper in `cmds.rs`. Delete the malformed-declaration
      test
- [ ] Docs: `models.md`, `ledger-schema.md`, `policy.md`

### 2. Remove legacy learning carry-over

- [ ] Delete `Store::replay_attributable_binding_learning` and its
      implementation in `store/state.rs`, with its three tests in
      `store/mod.rs`
- [ ] `policy/legacy.rs` stops writing `allow_revision_one_legacy_learning`
- [ ] Find out whether stored revisions are checked against
      `policy_fingerprint` on load, and whether lane pins compare against
      route fingerprints recomputed at compile
      (`compile.rs:924`, `transition.rs:256`). Keep the field deserialisable
      and in the canonical bytes and `SemanticUpgrade` when present, so a
      stored revision one still verifies and its pinned lanes keep their
      fingerprints. Drop it from those only if neither holds
- [ ] Keep the compiler's `legacy-learning-forbidden` refusal for authored
      documents that set it
- [ ] Docs: `policy.md` (the flag paragraph), `ledger-schema.md` (the
      "service does not call it" note), `models.md`

### 3. Route-unavailable rows and the final-outcome reader

- [ ] Schema migration relaxing `requests_route_terminal_shape_check` and
      `requests_terminal_trace_check` so a no-route terminal may have no
      `policy_route_id` and no `route_fingerprint`, keeping the revision id.
      Mirror the change in the Rust validation in `store/ledger.rs`
      (around `:1026` and `:2362`)
- [ ] Trace: add a closed reason for no route matched and one for a
      pin-denied move (carrying the `TransitionDenied` reason), validated on
      insert and read like the existing variants
- [ ] `record_route_unavailable` (`server/record.rs`) takes an
      optional route and lineage. Call it from the `NoRoute` arm and the
      `PolicyAttemptError::Transition` arm in Messages (`anthropic.rs`), Chat
      (`proxy.rs`) and Responses (`codex.rs`), keeping each answer's status,
      shape and `x-toker-not-configured` header. Pass the computed lineage on
      the Responses path
- [ ] Final outcome: a ledger helper that picks each group's final row
      (greatest `attempt_index`, else the terminal row); the TUI window
      (`tui/model.rs`) counts a group as a request on any of its rows, and
      shows failed requests by final outcome beside the attempt error count
- [ ] Tests: no-route and pin-denied rows per wire with lineage on Responses,
      the migration over a v16 fixture, a group that recovered by fallback
      counts zero failed requests, and a group that only failed counts one
      request and one failure
- [ ] Docs: `ledger-schema.md` (replace the "deliberate asymmetry"
      paragraph), `routing.md`, `attempts.md` (the final-outcome sentence and
      the read-model table), `lineage.md`

### 4. Untranslatable bodies: stream shape and a row

- [ ] One pre-send error responder per wire that writes the wire's own
      stream error: Messages `event: error` (`anthropic_error_event_data`),
      Chat the `data:` error chunk (`chat_error_value`), Responses
      `event: error` (`responses_error`). Use it in place of the JSON 400s at
      `inference.rs` `coordinate_messages`, the AnthropicMessages adapter arm,
      `prepare_messages` and `prepare_chat`, `proxy.rs:133` Chat
      canonicalisation, and `invalid_responses_body` plus the
      `frontend_error_response` bare `data:` shape (`codex.rs`)
- [ ] Ledger row for each: kind `error`, `error_type = local_preparation`
      (construct `FailureMechanism::LocalPreparation`, dropping its
      `allow(dead_code)`), frontend, session, requested model, lineage where
      known, no attempt provenance, written through `Store::record_request`
- [ ] Fix `translation_error_response` (`codex.rs`): it records with an
      Attempt-outcome `policy_routing` and no attempt, which ledger validation
      rejects, so the row is silently lost. Write the same `local_preparation`
      shape instead
- [ ] Rewrite the tests that pin today's behaviour:
      `malformed_streaming_messages_are_rejected_as_json_without_a_row`
      (`server_anthropic.rs`, `server_codex.rs`),
      `an_untranslatable_body_never_reaches_the_upstream` and the one at
      `server_codex.rs:2360`; add Chat and the codex-backend case
- [ ] Docs: `attempts.md` / `ledger-schema.md` on `local_preparation`, and
      the relevant translate topic

### 5. Virtual Responses presentations

- [ ] Check the codex client's model-info parser locally (agent-repo cache)
      for its required and optional fields, rather than relying on the fixture
      in `catalog/fetched.rs`
- [ ] `AdapterMetadata::Codex` (`policy/document.rs`) gains an optional typed
      record with those fields. Unknown fields are refused and an absent
      record decodes as today, so stored documents and their fingerprints are
      unchanged. Canonicalise it in `policy/canonical.rs` as presentation
      display data, outside route fingerprints
- [ ] `render_policy` (`catalog/offers.rs`): a virtual Responses presentation
      with a codex record renders from it with the presented slug. One
      without a record is left out, and the publication row gets a detail
      code naming it. A virtual slug no longer makes the listing `Unresolved`
- [ ] Compiler info diagnostic for a virtual presentation in Responses scope
      with no codex record; a warning for a codex record on a presentation
      that never reaches Responses
- [ ] Generated revision one is unaffected (it has no virtual Responses
      presentations); assert that
- [ ] Tests: unit tests on the Responses arm of `render_policy` (with a
      record, without one, mixed with exact offers, and still 503 on a missing
      exact offer); a server test in `server_codex.rs`; a Chat virtual entry
      test, which is missing today
- [ ] Docs: `publication.md` (replace "a Responses list cannot" with the
      record rule), `policy.md` adapter metadata

### 6. Routing page: derived content

The daemon computes, the TUI renders. The page has no store handle, so
everything below arrives in the routing snapshot
(`control/routing.rs` `routing_snapshot_response`).

- [ ] Availability: serialise the existing `RoutingControlSnapshot.availability`
      per binding (configured, enabled, credential, catalogue
      fresh/stale/absent, capabilities, cost class), plus open health circuits
- [ ] Pinned lanes per route: `routing_lane_impacts_locked`
      (`server/mod.rs`) also counts by `policy_route_id`, split warm, cold
      and unknown
- [ ] Family/version examples: for each classifier in the active revision
      and the draft, apply `CompiledClassifier::classify` to the offers its
      matcher accepts, capped at a few examples per classifier, including
      ones that fail to parse
- [ ] Upgrade winner: refactor `choose_upgrade` (`policy/resolve.rs`) to
      record why each rejected model lost (family mismatch, not newer,
      learning minimum, context fit) without changing its decision. Expose,
      per route candidate with an upgrade policy, the current winner and the
      rejections for a new lane with no prompt bound
- [ ] Per-presentation publication: for each presentation and adapter, the
      desired state (listed, or left out with the reason, including a virtual
      Responses entry without a codex record) and whether the applied
      identities hold it, from `CompiledPolicy::publication` and
      `render_policy`
- [ ] Cost boundary: per route edge, the cost classes either side and whether
      it crosses from subscription to billed or unknown, with the edge's
      consent
- [ ] `tests/server_routing_control.rs` pins the new snapshot keys and that
      they are present for the active revision without a draft
- [ ] Detail pane (`tui/routing/view.rs`) renders per view instead of pretty
      JSON. Presentations: source, label, desired/applied per adapter, route
      summary. Routes: ordered candidates with availability, edge cost
      marks, fallback conditions, pinned lanes. Rewrites: matcher and
      shadowing diagnostics; classifiers: parse examples; upgrades: winner and
      rejections. The detail scrolls
- [ ] Activation summary (`State::activation_summary`, `model.rs`): list edges
      whose boundary crossing or consent differs between draft and active
- [ ] First insta frame snapshots for the routing page, one per view, plus
      the activation modal

### 7. Routing page: typed forms

- [ ] `src/tui/routing/form.rs`: a form model built per record kind from the
      document types. Text fields with a cursor, enum choice, tagged-enum kind
      switch (matcher kind, presentation source, candidate model, selector
      ordering, version parser, scope, argument binding), booleans, and
      ordered lists (folding in the existing candidate-order modal)
- [ ] Offer completion with search: binding ids from `BACKEND_BINDINGS`,
      model ids from `snapshot.offers` filtered by the chosen binding. A
      typed value that is not currently offered is kept and marked. Replaces
      the promote-only Ctrl+O cycling
- [ ] The form serialises to the same `PolicyEditOperation` JSON
      `add_edit` / `replace_edit` send today, and shows the daemon's
      diagnostics against their field path
- [ ] `e` and `n` open forms for every editable kind; `template.rs` seeds new
      forms instead of JSON text; the raw-JSON `Modal::Record` and
      `text_key` go
- [ ] Mouse on the body (the comment at `tui/mod.rs:290` deferred it until
      the forms landed): select an entry, focus a field
- [ ] Tests: form round trip for every kind against `PolicyEditOperation`
      (extending `template.rs`'s acceptance test), kind switching keeps the
      shared fields, completion keeps an unavailable typed value, and frame
      snapshots of each form
- [ ] Docs: README's routing page section, `control.md`; legend entries and
      `every_key_the_legend_documents_reaches_the_page`

### 8. Finish

- [ ] `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`
- [ ] Run the new routing snapshot and the final-outcome counts against a
      copy of the real ledger and check that the numbers are believable
- [ ] Unplan: fold anything durable into `docs/internals/`, delete this plan
