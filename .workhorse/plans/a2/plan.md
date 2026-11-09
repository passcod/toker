# Retire revision one's generated-only semantics

## Notes

The new vocabulary (numeric tuple with any component count, the scaled bar, the compaction model substitution) changes which documents are accepted, so it needs a format version bump under RDOC's parsing rules.

The two promoted capabilities keep their behaviour and lose their names: `RevisionOneAdaptive` becomes the scaled bar, and `CompactionTarget::RevisionOneLegacy` becomes the model substitution, which classifies through the reached candidate's upgrade policy rather than a binding-wide legacy split.

The legacy-learning flag, the hyphenated classifier, both revision-one matchers, the legacy parser, `PolicyOrigin::GeneratedRevisionOne`, the compiler guards and the origin dispatch in `compile_stored_revision` all go.
The `compiler_origin` column stays, since dropping a column is a destructive migration; new rows write `authored`.

Derived Chat and Responses listings move out of `legacy::compile_document` into the ordinary compile, with a route check so a slug is listed only where the revision routes it to the offer's binding.

Retirement runs inside the first-revision transaction, which the service now reaches on every start; that transaction takes the first-revision document optionally, since a config that cannot route only matters where no revision is active.

A store-only config resolves protocol defaults with the historical preferences (`anthropic_sub`, `openrouter`), and the compaction model falls back to Sonnet, so on the machines that exist a reset rebuilds a near-identical first revision.

The live store held two generated revisions with identical documents, eight lanes pinned to them (all cold), and no model maps.

## Build

- [x] Document vocabulary: numeric tuple with optional component count, `MinimumLearning::Scaled`, compaction `Substitution { model: exact | family, bindings }`, format version 5 gating them; drop `LegacyNumericTuple`, `legacy_hyphenated`, `allow_revision_one_legacy_learning`, `RevisionOneAdaptive`, `RevisionOneLegacy`, both revision-one matchers
- [x] Canonical identity matching: classifiers and alias rules match `model_identity`; delete `policy/family.rs`, with the config model map's family-selector check made local
- [x] Compiler: drop `PolicyOrigin` and every guard; compile the substitution; derive Chat/Responses listings for every revision with the route check, replacing `add_generated_publication`
- [x] Resolver: family substitution through the reached candidate's upgrade policy classifier and bar; rename the legacy compaction facts
- [x] Generator: emit authored vocabulary (Anthropic-only regex classifiers, scaled bar, exact/regex alias rules, substitution); `compile_document` and `compile_stored_revision` become the ordinary compile
- [x] Store: drop `PolicyCompilerOrigin` (column stays, defaulting to authored); retire generated revisions inside the first-revision transaction (clear every pin to them, delete, reset pointer/draft/marker or rebase the draft, repair publications)
- [x] Tests updated for the new vocabulary, retirement and derived listings; test-cases file (new and rewritten tests run by name, the full suite left to CI)
- [x] `cargo check --all-targets`, `cargo clippy --all-targets`, `cargo fmt`

The generated record ids drop their `legacy-` prefix, since every machine rebuilds its first revision on retirement anyway.
`compact_model = "off"` becomes a substitution naming no model, which keeps the cold transform licensed on those bindings.
