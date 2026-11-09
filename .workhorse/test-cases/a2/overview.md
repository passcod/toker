# Retiring revision one's generated-only semantics

## Authored vocabulary

- [x] A numeric tuple with no declared count parses any count up to the bound and compares segment by segment (verifies spec: RDOC)
- [x] A document using the scaled bar, an undeclared tuple arity or a compaction substitution compiles as an ordinary authored document (verifies spec: RREV, RMODEL)
- [x] Those three features are refused in a document declaring a format before five (verifies spec: RDOC)
- [x] A classifier reads the canonical identity: a published snapshot and a bracketed suffix take the undated form's family, an unpublished snapshot has none (verifies spec: RMODEL)
- [ ] An alias rule naming an undated model rewrites its published dated snapshot (verifies spec: RDOC)

## Compaction substitution

- [x] A family substitution keeps the reached binding and picks the newest learned member under the candidate's upgrade policy bar, even with the upgrade switched off (verifies spec: RCOMP)
- [x] A candidate with no upgrade policy gets no family substitution, but an exact one still applies (verifies spec: RCOMP)
- [ ] A substitution naming no model keeps the model and still licenses the cold transform (verifies spec: RCOMP)

## The first revision

- [x] Only Anthropic-identity bindings get a classifier and upgrade policy, with the scaled bar and promotion allowed (verifies spec: RMIG)
- [x] The configured compaction model becomes a family, an exact model, or no model for `off` and empty (verifies spec: RMIG)
- [x] A model map becomes an exact rule and a family regex matching what the generated classifier places in that family (verifies spec: RMIG)

## Derived listings

- [x] Chat lists the prefixed slug for each configured provider's offers and the bare slug only for the default's (verifies spec: RPUB)
- [ ] An authored revision without a provider's prefix rewrite does not list that provider's prefixed slugs (verifies spec: RPUB)

## Retirement

- [x] A generated active revision is deleted and the first revision is established afresh, with publications repointed to it (verifies spec: RMIG)
- [x] Generated history under an authored active revision is deleted, its draft rebased onto the active revision, and a lineage pointer to it cleared (verifies spec: RMIG)
- [x] A lane pinned to a generated revision refuses the transaction, naming the lanes and revisions, and changes nothing (verifies spec: RMIG)
- [x] A machine whose config cannot build a first revision keeps its generated revisions when retirement would leave it with nothing (verifies spec: RMIG)
- [ ] The service starts on a copy of a real store holding generated revisions, once no lane is pinned to them, and routes as before
