---
id: UTUI
---

# The dashboard

The dashboard is where an operator reads what Toker has recorded and edits the policy.
This spec covers the views it renders, the controls it offers, and the rules its readings follow.

## Rendering

Rendering MUST be a pure function of a captured snapshot, the clock and the view's own state.

> [!NOTE]
> A renderer that reads the time or the environment cannot be asserted against at several terminal sizes without a real terminal.

A table MUST shed columns before its content is truncated.

A number shown to a person MAY be formatted to the host's locale.
That formatting MUST NOT reach a gate notice, as [GNOTE](../gates/notices.md#a-notice-is-byte-stable) requires.

## The views

The dashboard MUST show the sessions active within its window, the cache rebuilds within it, spend, and the state of each meter.

A session's row MUST take its model and the context ceiling derived from it from the [served model](../routing/models.md#served-model) of the latest row that named one.
A session's row MUST NOT take them from the requested model.

> [!NOTE]
> A context window belongs to the model that served, not to the one that was asked for.

A session detail MUST show the requested, effective and served models separately.
A session detail MUST show the served one as absent where the response named no model.

> [!NOTE]
> Showing all three is only useful because they can disagree, which is exactly what a fallback or an alias produces.

Counting rules for requests, attempts and failures are specified by [SATT](../store/attempts.md#reading-the-rows), and the measurement rules by [GCAST](../gates/forecasting.md).

A session MUST be marked where its newest decision was a gate stopping it, and that mark MUST be cleared by a later measurement or release.
An error, a sleep-lock change and a withheld notice are not decisions, and MUST neither set nor clear it.

A session grouped with its children MUST follow [SLIN](../store/lineage.md#lineage-is-for-display-and-accounting-only).

## Gate controls

The dashboard MUST be able to grant a session the same releases a marker grants.
The dashboard MUST write them by the same path, as [GQUOTA](../gates/quota.md#granting-a-release) specifies.

Granting the wider release MUST require a second confirmation.

The dashboard MUST be able to revoke a session's releases.

## The routing page

The routing page MUST show the active revision, the draft, the revision history and the publication state.

The page MUST be able to validate, preview, activate and reactivate, through the control operations [DCTRL](../service/control.md) specifies.

An activation MUST NOT wait on reconciling a client's settings, and a slow or failed reconciliation MUST be reported on the page without undoing or delaying the activation.

The page MAY import a configuration file into the draft, behind a confirmation, as [RMIG](../routing/migration.md#importing-a-file-deliberately) specifies.

## Cost

The dashboard MUST NOT sum costs of different kinds together.
The dashboard MUST report rows with no recorded cost as such rather than as zero.
