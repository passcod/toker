---
id: UTUI
---

# The dashboard

The dashboard is where an operator reads what Toker has recorded and edits the policy.
This spec covers the views it renders, the controls it offers, and the rules its readings follow.
It renders as pages: the dashboard proper, routing, configuration and accounts.

## Rendering

Rendering MUST be a pure function of a captured snapshot, the clock and the view's own state.

> [!NOTE]
> A renderer that reads the time or the environment cannot be asserted against at several terminal sizes without a real terminal.

A number shown to a person MAY be formatted to the host's locale.
That formatting MUST NOT reach a gate notice, as [GNOTE](../gates/notices.md#a-notice-is-byte-stable) requires.

### What is shed first

Every page MUST render into whatever terminal size it is given, shedding in a fixed order rather than clipping.

Panel framing MUST be shed before any content is.
A table MUST shed columns before its content is truncated.
A panel MUST shed its own rows down to the reading it exists to give before any panel is dropped whole.
The quota block MUST NOT be shed.
A list cut short MUST say how many of its entries it left out, on each side that hides any.

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
Every page MUST honour `NO_COLOR`, and MUST remain readable with every colour removed.

> [!NOTE]
> Rendering without colour is a supported mode, so a distinction carried by hue alone vanishes in it. This spec therefore names no colour and no threshold; which hue carries which reading, and where each band falls, are design decisions recorded in [the design guide](../../design/design-system.md).

### Marks

A session MUST be marked where it was served on a model newer than the one it asked for, and that mark MUST distinguish the latest turn from an earlier one.
A session MUST be marked where it holds a release, and the two kinds of release MUST be distinguishable from each other.

A value that was not reported MUST be marked as such, and MUST be distinguishable both from one that is unknown and from a floor, as [STORE](../store/overview.md#not-recorded-is-not-zero) requires.

Every mark the dashboard renders MUST be explained in the legend, and the legend MUST explain no mark the dashboard does not render.

> [!NOTE]
> The legend is the operator's dictionary. A mark missing from it is undocumented, and an entry with nothing behind it is a claim about a screen that does not exist. Which glyph carries which mark is a design decision, recorded in the design guide.

### Finding the keys

Every page MUST offer an overlay listing the keys it acts on, opened by the same key on every page.
The header of every page MUST carry a control that opens that overlay, at every terminal size.

> [!NOTE]
> Hints held on screen cost a row on every frame of a page an operator long since learned, and the pages that need the most keys have the least room for them. The overlay keeps them a key away instead.

A modal or a text entry that takes every key MUST say the keys it acts on itself, since the overlay cannot open over it.

Every page MUST be able to move to the next page by the same key.

The header of every page MUST carry a control for each page, marking the current one, that goes to that page when clicked.
Those controls MUST keep their labels at every terminal size, and a page's own tabs MUST be shed before them.

## The views

The dashboard MUST show the sessions active within its window, the cache rebuilds within it, spend, and the state of each meter.

Counting rules for requests, attempts and failures are specified by [SATT](../store/attempts.md#reading-the-rows), and the measurement rules by [GCAST](../gates/forecasting.md).

### Sessions

A session MUST have a row where any row within the window names it, of any kind, and where it has a request in flight.

> [!NOTE]
> A session that has only been stopped by a gate is the one most in need of a release, and a release is granted from the session's row.

A session grouped with its children MUST follow [SLIN](../store/lineage.md#lineage-is-for-display-and-accounting-only).

A session's request count MUST count its logical requests as [SATT](../store/attempts.md#reading-the-rows) counts them, whether they completed, failed or are still in flight.

A session's row MUST take its model and the context ceiling derived from it from the [served model](../routing/models.md#served-model) of the latest row that named one.
Where no row of the session named one, the row MUST take them from the newest [effective model](../routing/models.md#effective-model) the session was routed to, whether on a request in flight, a gate's row or a failed attempt's, and MUST mark them as not yet served.
A session's row MUST NOT take them from the requested model.

> [!NOTE]
> A context window belongs to the model that served, not to the one that was asked for.
> The model a request was routed to is the nearest thing to it before any response names one.

A session detail MUST show the requested, effective and served models separately.
A session detail MUST show the served one as absent where the response named no model.

> [!NOTE]
> Showing all three is only useful because they can disagree, which is exactly what a fallback or an alias produces.

A session MUST be marked where its newest decision was a gate stopping it, and that mark MUST be cleared by a later measurement or release.
An error, a sleep-lock change and a withheld notice are not decisions, and MUST neither set nor clear it.

A session MUST be marked where the newest of its requests to reach a final outcome failed, and that mark MUST be cleared by a later request whose outcome is not a failure.
A request still in flight MUST neither set nor clear it.

> [!NOTE]
> A failure a fallback recovered from is not its group's final outcome, so it does not mark the session.
> It shows in the error count instead, as [SATT](../store/attempts.md#counting-within-a-window) specifies.

### Requests in flight

A request MUST show on the dashboard from the first refresh after Toker begins handling it, read from its [in-flight record](../store/attempts.md#while-a-request-is-in-flight).

> [!NOTE]
> Waiting for a row would show nothing of a turn until its response ends, which on a long stream is minutes after the operator sent it.

A request in flight MUST count as its session's latest activity, from when Toker began handling it.

The header's freshness MUST read from the newest request Toker began handling, whether it is in flight or recorded.
The request rate MUST count each request in the minute Toker began handling it, a request in flight included, and a request that began before the window in the window's first minute.
A recorded request whose rows predate the [time it began](../store/ledger.md#fields-that-need-care) MUST be placed by its first row instead.

While a session has a request in flight, its row MUST show that request's stage and how long it has been in flight, in place of how long the session has been idle.
Where a session has more than one request in flight, its row MUST show the one in flight longest, and how many there are.

A request in flight on the session's own conversation, rather than on a child's, MUST update the row's message count.
Figures only a response reports, such as the prompt and the output, MUST keep their values from the session's latest completed request.

A session detail MUST list each of the session's requests in flight, with its stage, how long it has been in flight, its attempt, and the provider and model its current attempt was sent on.

### Meters across accounts

The dashboard MUST show the meters of each account that served traffic within its window, and MUST NOT show the meters of an account that served none.
Where no account served within the window, the dashboard MUST show the one that served most recently, alone.

> [!NOTE]
> Otherwise a machine left alone for longer than its window blanks the panel, which reads as a quota Toker has stopped measuring rather than as one nothing has spent.

> [!NOTE]
> The accounts worth a place are the ones the window is already about, the same rule the session rows follow, so an account the operator stopped using does not hold space forever.

Meters MUST be laid out across the width available, showing as many accounts as fit.
The accounts that do not fit MUST be reachable by scrolling sideways.

Scrolling MUST come to rest on whole accounts, and MUST NOT leave a meter showing in part.

> [!NOTE]
> A bar cut partway along reads as a smaller share than it holds, which is the comparison below read wrong rather than merely interrupted.

The dashboard MUST say how many accounts are hidden, on each side hiding any.

> [!NOTE]
> The dashboard writes every other absence rather than computing it away, and a strip that quietly ends offers the accounts in view as though they were all of them.

One meter's readings MUST be comparable by eye across the accounts shown together.

> [!NOTE]
> Showing accounts side by side is for reading one window across them, which a reading drawn at a size answering to its own line's spare room rather than to the quantity defeats.
> Which dimension carries the comparison is a design decision, recorded in [the design guide](../../design/design-system.md).

Accounts MUST be ordered by how much of the window each served, the busiest first.

That order MUST resist changing while two accounts are close.

> [!NOTE]
> Two accounts taking turns otherwise swap places under the operator's eye each time the lead changes hands, which is motion carrying no information.
> Comparing shares coarsely enough that a close pair falls on the same side of the comparison is one way, and needs nothing carried between refreshes.

An account MUST be shown by its email address where there is room for one, and by its number otherwise.
A view showing an account in full MUST show both, as [ACCT](../store/accounts.md#identity) requires.

## Gate controls

The dashboard MUST be able to grant a session the same releases a marker grants.
The dashboard MUST write them by the same path, as [GQUOTA](../gates/quota.md#granting-a-release) specifies.

Granting the wider release MUST require a second confirmation.

The dashboard MUST be able to revoke a session's releases.

## The routing page

The routing page shows the active revision and edits it.

It MUST present the policy as tabs in the order a request is resolved, as [RSOLV](../routing/resolver.md#lookup-order) specifies: presentations, rewrites, defaults and routes, then models and the revision history.
The four lookup tabs MUST read as a sequence, set apart from models and history, which shape and record a route rather than find one.
Each tab MUST show how many records it holds.

> [!NOTE]
> The policy is ordered rules across several tables, the way a router's or a firewall's is, and the tab strip is the one place the whole order can be read at a glance.

The page MUST open on the first tab holding a record whose [reach](#reading-a-table) is refused, and on the first tab otherwise.

Each tab MUST show its records as a table, with the selected record's detail beneath it.
A panel MUST be no taller than its content, except a list that scrolls, which takes the rows left over.

Records MUST NOT be shown or edited as a serialised document.
A matcher MUST be shown in its own notation, such as a prefix with the part it captures, rather than as the regular expression it compiles to, unless it is a regular expression.

> [!NOTE]
> The document is the store's representation of the policy, not the operator's.
> A rule an operator has to read as JSON is a rule they will not check.

### Reading a table

Rewrites MUST be grouped by scope, each profile ahead of its protocol, with each group in ascending priority.
A profile's group MUST end by passing to its protocol's group, since what a profile's rules miss is tried against the protocol's rules before any default.
A protocol's group MUST end with the defaults that catch what no rule did, each profile's own default ahead of the protocol's, and MUST say that the request is refused wherever none would catch it.

A value every row of a group shares MUST be stated once for the group rather than repeated as a column.

A rewrite MUST be shown by what it matches and the route it picks, and MUST NOT need a name of its own.

Each rewrite and default MUST show its reach, being what a request it catches gets now, as one of:

- served by the route's first candidate
- served by a later candidate, saying whether it is billed
- refused at an edge, saying which consent or setting stopped the walk
- refused because every candidate is unavailable
- refused for want of a route

Reach MUST be worked out from the same availability [preview](../routing/revisions.md#preview) uses.

> [!NOTE]
> Whether the first candidate is up is the wrong question: a rule whose first choice is off but whose fallback serves is spending money, not failing, and the operator needs to see which.

Each rewrite and default MUST show how many requests it caught in the last 24 hours, counted from the provenance [RPROV](../routing/provenance.md) records, by the record's identifier across every revision the window spans.
That count MUST read as a floor where some of the window predates rows recording what matched, and as not recorded where all of it does.

A rule's detail MUST show:

- what it matches, with a worked example
- what it sends on
- the route and its candidates
- its reach, with the reason for it
- anything earlier in the lookup order that already catches all or part of what it matches, being a presentation, a rule of a profile ahead of its scope, or an earlier rule in its own
- its recent catches

### Routes

The routes tab MUST show each route with the providers its candidates reach in order, what refers to it, the lanes it holds and its reach.

Opening a route MUST show it whole, reading in the direction a request travels:

- the candidates it is sent to, in order, with the failures and consents on each edge between them
- how the route moves its lanes, which every route sets, so it MUST NOT be shown as a block that could be off
- the blocks that handle it, naming each one that is enabled with its setting, and naming the ones that are not
- every rewrite, presentation and default that sends a request to it

The route view MUST be able to step to the other routes without returning to the table.

### Tracing a request

Every routing tab MUST be able to trace a request, invented or taken from a recent one, through the active revision, by the same [preview](../routing/revisions.md#preview) the editor uses.

A trace MUST give its outcome first, being the binding and model that would serve and whether that is billed, then each stage of the lookup order and the route walk with its verdict and the reason for it.
Each table the trace passed through MUST mark the row that caught the request and the rows it fell past.
A recent request that cannot be replayed MUST be shown as such rather than traced.

### Editing

A record MUST be edited in place, in the table that shows it, as a form of fields.
A record MUST be able to be added by the same form, in the table that will show it, and to be removed.
A rewrite MUST be able to be moved earlier or later within its scope, and a route's candidates added, removed and reordered.

Each field MUST be checked as it changes, against the same compilation an activation runs.
A field that is refused MUST say beside itself why, and how to make it acceptable.
A field that is accepted MAY say beside itself what it does.

A field the compilation reports on without refusing, such as a candidate whose provider is not configured, MUST say so beside itself and MUST NOT block the save.

A refusal that belongs to no one field, such as removing a route another record still sends to, MUST be shown with the form and MUST name the records it concerns.

A form holding a refusal MUST NOT be saveable, and its save control MUST name the field or record in the way.

> [!NOTE]
> There is no separate validation step for the operator to remember, because a form that would not compile cannot be saved in the first place.

While a record is being edited, the page MUST show how the last 24 hours of requests it bears on would route with the change, beside how they routed, and MUST say how many of them could not be replayed.

Saving MUST [save](../routing/revisions.md#saving-an-edit) the form as one edit, activated for new and cold lanes.
Where a save fails because the live revision moved, the form MUST be kept as entered, and the page MUST say which revision is now live.

The page MUST always show which revision is live.
Where lanes are pinned to an earlier revision, it MUST say how many.
Where a client's settings are not applied for the live revision, it MUST say whether that is pending, failed or unsupported, with the detail [RPUB](../routing/publication.md#publication-state) records, and MUST NOT wait for reconciliation.

### History

The history tab MUST list the revisions newest first, each with a summary of what it changed from its predecessor and the lanes pinned to it.
The history is read a page at a time, so the tab MUST say when it holds fewer revisions than there are, and MUST read the page behind it rather than stopping there.

A revision's detail MUST list each record it added, changed or removed, in words.

The history tab MUST be able to:

- [reactivate](../routing/revisions.md#history-and-reactivation) the selected revision, for new and cold lanes as a save is
- move running lanes onto the live revision, by [widening](../routing/revisions.md#widening-an-activation) its activation
- trace a request under the selected revision

A move that would lose a warm cache MUST ask for a second confirmation, and the first MUST name the lanes whose cache it would lose.

### Models

The models tab MUST show the classifiers, the upgrade policies with the evidence bar each requires and the move each would make now, where cold compactions go and on which bindings, and the offers in each binding's catalogue.

It MUST be able to [promote](../routing/models.md#promotion) a learned model.

## The configuration pages

Configuration is edited on two pages, providers and settings.

Both MUST write through the control operations [DCTRL](../service/control.md#the-operations) specifies, and an edit MUST take effect without a restart.

A value MUST be edited in place, in the row that shows it, and checked as it changes.
A refused value MUST be reported beside the field that holds it, and MUST leave the stored value as it was.

Where the service is not answering, both pages MUST report that rather than offer values to edit.

> [!NOTE]
> The service is the only writer, so a form the operator can fill in while it is down is a form whose edits go nowhere.

### Providers

The providers page MUST show every provider Toker knows, enabled or not, with the endpoint it is reached at and which source supplies its credential.

The page MUST open on a provider that is enabled but not configured, being one whose credential does not resolve as [DCONF](../service/config.md#configuration-never-refuses-a-start) specifies, where there is one.

The page MUST show what in routing depends on each provider, counting the presentations, rewrites, defaults and routes that reach it through any offer or candidate, a fallback included.

> [!NOTE]
> Counting only the routes a provider comes first in understates what turning it off would change.

The page MUST be able to enable and disable a provider, set its endpoint, choose where its credential comes from, and set the settings particular to that provider.

The page MUST show whether a credential is set and MUST NOT show what it is, as [SEC](../security.md#a-credential-is-never-logged-or-displayed) requires.

The page MUST show a provider whose credential does not resolve as not configured, naming the source that did not supply one.

The page MUST show, for each of a provider's Messages bindings, its declared support for server safeguards and whether a rejection mark is held against it.
The page MUST be able to clear that mark, as [WSAFE](../wire/safeguards.md#a-rejection-mark) specifies.

> [!NOTE]
> That is the answer a request reaching the provider gets, so a page showing anything else would disagree with what the proxy does.

### Settings

The settings page MUST show and edit the gate settings, the notice style of each frontend profile, the session and ping header names, whether the idle-sleep lock is held, and the extra transcript roots.

A token count MUST be accepted in full, and in the same shorthand the dashboard displays it in.

A setting left unset MUST say what it falls back to.

The page MUST warn beside the cold-gate minimum where the minimum in force is out of every resolvable window's reach, as [GCOLD](../gates/cold.md#only-a-request-that-could-be-the-re-read) specifies.

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
