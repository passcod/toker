# Retire revision one's generated-only semantics

## Notes

The new vocabulary (numeric tuple with any component count, the scaled bar, the compaction model substitution) changes which documents are accepted, so it needs a format version bump under RDOC's parsing rules.

The two promoted capabilities keep their behaviour and lose their names: `RevisionOneAdaptive` becomes the scaled bar, and `CompactionTarget::RevisionOneLegacy` becomes the model substitution, which classifies through the reached candidate's upgrade policy rather than a binding-wide legacy split.

The legacy-learning flag, the hyphenated classifier, both revision-one matchers, the legacy parser, `PolicyOrigin::GeneratedRevisionOne`, the compiler guards and the origin dispatch in `compile_stored_revision` all go.
The `compiler_origin` column stays, since dropping a column is a destructive migration; new rows write `authored`.

Derived Chat and Responses listings move out of `legacy::compile_document` into the ordinary compile, with a route check so a slug is listed only where the revision routes it to the offer's binding.

Retirement runs at startup, before the first-revision transaction and the active-revision load.

A store-only config resolves protocol defaults with the historical preferences (`anthropic_sub`, `openrouter`), and the compaction model falls back to Sonnet, so on the machines that exist a reset rebuilds a near-identical first revision.

The live store held two generated revisions with identical documents, eight lanes pinned to them (all cold), and no model maps.
