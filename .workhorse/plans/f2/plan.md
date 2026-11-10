# Requests in flight on the dashboard

## Technical notes

- The in-flight record is a store table, inserted when the server begins handling a request, updated at each stage change, and deleted once the request's last ledger row is written.
  It is not a ledger table, so no ledger reader has to learn about it.
  Chosen over polling the service's memory through a control operation because the dashboard already re-reads on `PRAGMA data_version` moving, so an insert there reaches the next tick with no new plumbing.
- The atomic in-flight count behind the `status` operation stays as it is; `toker restart` and the sleep lock read it, and nothing here needs them to change.
- Every stage change moves `data_version`, which triggers a display read and marks the heavy passes stale.
  The heavy passes are already spaced by `QUOTA_MIN`, so the extra writes should not make the loop spin, but check it under a busy stream.
- The service clears the table on start, before it accepts connections.
- A new ledger column records when the server began handling the request, so completed requests can be bucketed by arrival.
  Rows from before it lack the column and fall back to their group's first row.
- Session rows today come only from measurement rows (`is_api_measurement` gates the per-session pass in `tui/model.rs`).
  Gated, failed and in-flight-only sessions need the session map seeded from every row naming a session, not only measurements.
- The glyphs for the stages, the failure mark and a not-yet-served model are design decisions to record in the design system alongside the existing marks, and each needs a legend entry.
- The quota gate's row (`record_quota_blocked` in `server/record_anthropic.rs`) writes no requested or effective model, though it already knows the provider and account.
  It needs both from the recording context, as error rows already take them.
  Older gate rows lack them, so a gated-only session from before the change still reads `?`.
