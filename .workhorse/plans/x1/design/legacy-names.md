# What "legacy" means on these records, and whether it has to stay

Every record on the operator's machine is named `legacy-route-…`, `legacy-prefix-…`, `legacy-family-…` or `legacy-force-newest-…`.
This note answers what they are legacy against, why they were not rewritten, and what the redesign should do about them.

## Legacy against an authored policy, not against an older Toker

Routing used to be fields in `toker.toml`, interpreted per request.
It is now a series of revisions in the store.
[RMIG](../../specs/routing/migration.md) is the seam between the two: on the machine's first start after the move, revision one is **generated from the file's routing fields**, so that existing behaviour survives.

Everything named `legacy-` is a record that generator wrote.
`legacy` therefore means "machine-generated from the old configuration file", and the contrast is with a record the operator authored.

The names themselves carry nothing.
`legacy.rs:547` builds a route id as `format!("legacy-route-{}", stable_part(binding.id_str()))`, and the other prefixes are the same kind of convention.
No spec requires them, nothing matches on them, and they could be renamed today.

## Why they were not rewritten into ordinary records

Because five of the things revision one generates **cannot be expressed in an authored document**, and the compiler refuses each of them unless the document's origin is the generated first revision:

| generated-only | refused as | what it is |
| --- | --- | --- |
| `legacy_hyphenated` on a classifier | `legacy-classifier-forbidden` | family and version split by `family_of()` rather than by a matcher |
| `RevisionOneModel`, `RevisionOneFamily` matchers | `legacy-matcher-forbidden` | matchers with no authored equivalent |
| `allow_revision_one_legacy_learning` | `legacy-learning-forbidden` | an upgrade policy that accepts the old evidence bar |
| `MinimumLearning::RevisionOneAdaptive` | `legacy-adaptive-learning-forbidden` | an evidence bar that moves with what has been served |
| `CompactionTarget::RevisionOneLegacy` | `legacy-compaction-forbidden` | the old compaction target shape |

So a wholesale rewrite into authored records is not a rename.
It would change behaviour, because the authored vocabulary cannot say what these say.

The clearest case is the classifier.
`family_of()` (`src/policy/family.rs:59`) does three things a regex cannot:

- it **refuses an unpublished dated snapshot**, so a Claude id ending in eight digits is not given a family at all;
- it **folds aliases first**, so a published dated identity like `claude-3-7-sonnet-20250219` reduces to its dateless form and keeps its family;
- it **compares versions segment-wise and numerically**, because string order gets it wrong twice over: `claude-opus-5` sorts before `claude-opus-4-8`, and `4-10` sorts before `4-8`.

## The part that is a defect

A legacy classifier still carries a `model_matcher` regex in its document, and **that regex does not run**.
`CompiledClassifier::classify` branches on `legacy_family` and calls `family_of()` instead, as the comment at `src/policy/compile.rs:585` says in as many words: the family split is read "in place of its stored regex".

The routing detail pane does not know this.
`src/tui/routing/detail.rs:445-470` renders `matches: {kind} {value}` straight from `model_matcher`, with no test for `legacy_hyphenated`.

So on the operator's machine, all five classifiers state a rule that is not the rule.

## What the redesign should do

1. **Drop the prefix from what is displayed.** The ids are arbitrary and the prefix is thirteen characters of every row that says only "this machine was migrated". Show the distinguishing tail, and keep the full id where it is edited or copied. Direction D already elides it to `…anthropic_sub-anthropic_messages`.
2. **Say that a record is generated, once, where it matters** — on the record's own detail, as a property, not in its name. A generated record is one the operator cannot reproduce by authoring, and that is worth knowing when they try to edit it.
3. **Never show a rule that does not run.** Where a classifier is `legacy_hyphenated`, the detail must describe `family_of()`'s actual behaviour and must not print the stored regex as though it were the matcher.
4. **Renaming is possible but is not free.** [RREV#draft](../../specs/routing/revisions.md) requires a rename to be expressed as a removal and an addition, so renaming every record rewrites the whole policy into one new revision. It is a reasonable thing to offer, and it is not a side effect of the redesign.
