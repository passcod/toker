---
id: ACCT
---

# Accounts

An account is one upstream identity that a provider's credentials belong to, and several may be in use on one machine at once.
This spec covers what identifies an account, how Toker comes to know one, what is attributed to it, and how a machine whose history names none is moved onto them.

Which credential reaches which provider is specified by [WCRED](../wire/credentials.md), and the configuration rows beside these by [DCONF](../service/config.md#in-the-store).

## An account is learned, not configured

Toker MUST discover an account from the credentials it can read and the traffic it serves.
Toker MUST NOT require an account to be configured before traffic on it is served.

An account MUST be held as its own row, and MUST NOT be held as a provider row.

> [!NOTE]
> A provider row says how an upstream is reached and the operator edits it; an account row says which identity was seen on it and Toker writes it.
> [DCONF](../service/config.md#in-the-store) requires exactly one row per provider, which a row per login would break, and the route topology [ROUTE](../routing/overview.md#bindings) fixes would multiply with the operator's logins.

Several accounts MAY be known on one provider, and Toker MUST NOT assume there are at most two.

## Identity

An account MUST be identified by the identifier its upstream names it with.

An account whose upstream identifier is not known MUST be identified by its email address instead.
Where an upstream identifier later becomes known for an account held by its address, Toker MUST record it against that account rather than opening a second one.

> [!NOTE]
> A subscription's credentials are refreshed often and an address may change, so neither can be what an account is.
> The upstream's own identifier survives both, and the address is what the operator recognises while nothing has offered one.

An account MUST also carry a number, assigned in the order accounts became known, and never reused.

A display with room for an email address MUST show it, and a display without MUST show the number.
A view showing an account in full MUST show both.

> [!NOTE]
> The number is what fits where the address does not, so every view that has room must also say which address it stands for.

## Coming to know an account

Toker MUST recognise a credential by a digest of its value, and MUST NOT store the value itself.

Toker MUST resolve the account a digest belongs to from the first of these that answers:

1. the credentials another program on the same machine holds for its own several logins
2. the login file of the program whose subscription the credential is, found where [DCONF](../service/config.md#in-the-store) specifies
3. the upstream itself, asked once for whose the credential is

Toker MUST record a resolution against the digest, and MUST NOT ask the upstream again for a digest it has already resolved.

A refreshed credential presents a digest Toker has not seen.
Toker MUST resolve such a digest to the account its predecessor resolved to where a source says so, and MUST NOT open a new account for a credential that is only the same login refreshed.

## An account not yet known

A request whose credential resolves to no account MUST be served.

Such a request MUST be attributed to its credential's digest rather than to any account.
Toker MUST NOT attribute it to an account that merely served nearby traffic.

The meters such a request returns MUST NOT feed any account's reading or fit.

Where a digest is later resolved, Toker MUST attribute every row it already holds against that digest to the account it resolved to.

> [!NOTE]
> A login that outran its resolution is then recovered rather than lost, and the gates are given the history they would otherwise have had.

## What is attributed to an account

A ledger row MUST carry the account its request was served on.

A meter reading MUST be held per account.
A reading from one account MUST NOT replace another's.

An allowance MUST be granted against one account's window, and MUST NOT release a session against another's.

A quota fit MUST read one account's rows alone, as [GQUOTA](../gates/quota.md#scope-of-a-fit) requires of a provider's.

> [!NOTE]
> Two accounts' rate limit windows start when each account first spends after its own reset, so they run independently and overlap in time.
> A fit reading both is comparing windows that have no relation to each other, which is the one thing [GQUOTA](../gates/quota.md#fitting-a-weight) forbids.

## Routing does not select an account

A request MUST be served on the account its own credential names.

Toker MUST NOT send a request to an account other than the one its credential names.

> [!NOTE]
> Accounts are identities the ledger and the gates are keyed on, and a candidate selecting between them is a separate thing that nothing yet does.

## Moving a machine onto accounts

A store holding request rows that name no account is moved by setup, the way a machine whose configuration was a file is moved by it, as [DCONF](../service/config.md#moving-a-machine-off-the-file) specifies.

Every command other than setup, the service included, MUST refuse to run while the store holds a request row naming no account, and MUST say that setup attributes them.

Setup MUST offer the accounts it can discover and MUST ask which of them the unattributed rows belong to.
Setup MUST write that account onto every unattributed row, in one transaction.

An interrupted move MUST be repaired by running setup again.

Setup MUST verify that the unattributed rows carry the meter windows of a single account, and MUST refuse to attribute them to one account where they do not.

> [!NOTE]
> Windows of one account form a single series, each starting where the last reset; two accounts' overlap in time.
> Attributing a mixed history to one account would assert something nothing recorded, and the check costs a single pass over rows Toker already holds.
> Nothing separates a mixed history afterwards, so refusing says so rather than quietly choosing.
