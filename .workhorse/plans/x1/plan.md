# Redesign the routing and configuration pages

The design is converged in `design/` (start with `design/how-it-works.md`), and the specs carry it.

## Changes underneath the screens

The spec review settled four changes to how Toker works, which the implementation has to make beside the new pages.

- The draft is retired.
  The draft replace, draft edit and activate control operations go, with the draft table's writers; a save (edit applied to the active revision, activated for new and cold lanes) and a reactivation are the only ways a revision becomes active after the first.
  The draft table stays in the schema, since migrations are append-only, but nothing reads or writes it.
- The deliberate import of `toker.toml` routing goes, with `src/tui/routing/import.rs` and its entry in the routing view.
  First-revision creation in setup and at startup is unchanged apart from no longer copying into the draft.
- Provenance records the resolver's facts and the default that matched, through a new append-only migration.
  Rows written before it are not replayable; nothing backfills them.
- A provider prefix naming a disabled provider walks its rewrite's route like any other, so a fallback can serve it; the resolver's special case for disabled prefixes goes.

The README's routing paragraph describes the draft, validate and activate flow, and needs rewriting once the pages ship.
