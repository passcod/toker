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

A number shown to a person MAY be formatted to the host's locale.
That formatting MUST NOT reach a gate notice, as [GNOTE](../gates/notices.md#a-notice-is-byte-stable) requires.

### What is shed first

The dashboard MUST render into whatever terminal size it is given, shedding in a fixed order rather than clipping.

Panel framing MUST be shed before any content is.
A table MUST shed columns before its content is truncated.
A panel MUST shed its own rows down to the reading it exists to give before any panel is dropped whole.
The quota block MUST NOT be shed.

> [!NOTE]
> A border is the cheapest thing on the page to lose and the quota block is the part worth watching, so the order is a statement of what the dashboard is for, written as what it gives up last.

The column naming a session MUST keep a width it can be read at in preference to any column of figures, and MUST fall back to an identifier short enough to tell sessions apart where it cannot.

> [!NOTE]
> A session's title is what identifies the row; a peak or an output total is not.

### Distinguishing a reading

A meter approaching exhaustion MUST read as distinct from one on track, and an exhausted one from both.
A forecast that reaches an armed gate MUST read as distinct from one that reaches true exhaustion.

> [!NOTE]
> A gate is recoverable, a release spending past it, where exhaustion is not. They are different walls, and an operator acts differently on each.

A reading taken from a window that has rolled over, or one there is too little data to make, MUST say so in words rather than render a figure that looks measured.

A reading MUST NOT be distinguished by colour alone.
The dashboard MUST honour `NO_COLOR`, and MUST remain readable with every colour removed.

> [!NOTE]
> Rendering without colour is a supported mode, so a distinction carried by hue alone vanishes in it. This spec therefore names no colour and no threshold; which hue carries which reading, and where each band falls, are design decisions recorded in [the design guide](../../design/design-system.md).

### Marks

A session MUST be marked where it was served on a model newer than the one it asked for, and that mark MUST distinguish the latest turn from an earlier one.
A session MUST be marked where it holds a release, and the two kinds of release MUST be distinguishable from each other.

A value that was not reported MUST be marked as such, and MUST be distinguishable both from one that is unknown and from a floor, as [STORE](../store/overview.md#not-recorded-is-not-zero) requires.

Every mark the dashboard renders MUST be explained in the legend, and the legend MUST explain no mark the dashboard does not render.

> [!NOTE]
> The legend is the operator's dictionary. A mark missing from it is undocumented, and an entry with nothing behind it is a claim about a screen that does not exist. Which glyph carries which mark is a design decision, recorded in the design guide.

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

### Meters across accounts

The dashboard MUST show the meters of each account that served traffic within its window, and MUST NOT show the meters of an account that served none.

> [!NOTE]
> The accounts worth a place are the ones the window is already about, the same rule the session rows follow, so an account the operator stopped using does not hold space forever.

Meters MUST be laid out across the width available, showing as many accounts as fit.
The accounts that do not fit MUST be reachable by cycling through them.

Accounts MUST be ordered by how much of the window each served, the busiest first.

That order MUST resist changing while two accounts are close, and MUST NOT be recomputed into a new order on every refresh.

> [!NOTE]
> Two accounts taking turns otherwise swap places under the operator's eye each time the lead changes hands, which is motion carrying no information.

An account MUST be shown by its email address where there is room for one, and by its number otherwise.
A view showing an account in full MUST show both, as [ACCT](../store/accounts.md#identity) requires.

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

## The configuration pages

Configuration is edited on two pages, providers and settings.

Both MUST write through the control operations [DCTRL](../service/control.md#the-operations) specifies, and an edit MUST take effect without a restart.

A refused value MUST be reported on the page, leaving the stored value as it was.

Where the service is not answering, both pages MUST report that rather than offer values to edit.

> [!NOTE]
> The service is the only writer, so a form the operator can fill in while it is down is a form whose edits go nowhere.

### Providers

The providers page MUST show every provider Toker knows, enabled or not, with the endpoint it is reached at and which source supplies its credential.

The page MUST be able to enable and disable a provider, set its endpoint, choose where its credential comes from, and set the settings particular to that provider.

The page MUST show whether a credential is set and MUST NOT show what it is, as [SEC](../security.md#a-credential-is-never-logged-or-displayed) requires.

The page MUST show a provider whose credential does not resolve as not configured, naming the source that did not supply one.

> [!NOTE]
> That is the answer a request reaching the provider gets, so a page showing anything else would disagree with what the proxy does.

### Settings

The settings page MUST show and edit the gate settings, the notice style of each frontend profile, the session and ping header names, whether the idle-sleep lock is held, and the extra transcript roots.

The page MUST show where the store is and which port the listener answers on, and MUST NOT offer to edit either, those being settled before the store is opened as [DCONF](../service/config.md#before-the-store) specifies.

## The accounts page

The accounts page MUST show every account Toker knows, whether or not it served traffic in the window, with its number, its email address and when it was last seen.

The page MUST show a credential that resolved to no account as its own entry, so that a login Toker could not name is visible rather than missing.

The page MUST be able to give such an entry an email address, as [ACCT](../store/accounts.md#the-operator-naming-one) specifies, and MUST show which of its accounts were named that way.

Accounts are otherwise not configuration, so the page MUST NOT offer to add or remove one, those being learned as [ACCT](../store/accounts.md#an-account-is-learned-not-configured) specifies.

> [!NOTE]
> An account the operator could create would be one no credential names, which nothing could ever be served on.
> Naming one Toker already found is the opposite: the account is there and only its label is missing.

## Cost

The dashboard MUST NOT sum costs of different kinds together.
The dashboard MUST report rows with no recorded cost as such rather than as zero.
