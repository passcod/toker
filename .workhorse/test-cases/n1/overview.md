# Verifying multiple local Anthropic accounts

The card's claim is that every figure toker reports belongs to one account rather than to a pool of them.
Most of what is worth checking is therefore a pair: the figure an account should see, and the figure it should not see because it is another account's.

## Attribution

- [x] A credential is recognised by its digest, and the digest is what an unresolved row is attributed to (verifies spec: ACCT#an-account-not-yet-known)
- [x] Resolving a digest backfills every older row that carried it, and a digest already resolving to the same account sweeps nothing
- [x] A refreshed token presents a new digest and resolves to the same login, without restarting the service
- [x] A half-written local source does not erase a login toker already knows the name of
- [x] A number is assigned in discovery order and never reused (verifies spec: ACCT#identity)
- [x] An account matched by upstream id takes an offered address; one matched by address alone fills an absent id, and refuses a different one
- [x] The ledger export carries the credential digest as a prefix, never the value (verifies spec: SEC#a-credential-is-never-logged-or-displayed)
- [ ] No credential value reaches the store, a log line or the screen on any path this card touches (verifies spec: SEC#a-credential-is-never-logged-or-displayed)

## State keyed by account

- [x] A meter reading lands in its account's slot, and in the provider's where no account is known
- [x] The quota gate reads only the account's own slot, and does not fire where no account is known
- [x] An allowance is keyed by the account it was granted on (verifies spec: GQUOTA#an-allowance-belongs-to-an-account)
- [x] A lane pin names the account that served, and is absent on a provider whose accounts toker cannot tell apart (verifies spec: SLANE#the-pin)
- [x] A lane pinned to one account reads cold when asked about another (verifies spec: SLANE#cache-temperature)
- [x] A ping records the account whose window it opened, read off the rows it produced (verifies spec: GPING#a-window-belongs-to-an-account)
- [x] A ping skips only on its own account's open window, and sends where several accounts make the answer unknowable
- [x] Two accounts both at a five-hour reset of the same instant hold separate allowances, and a release on one does not free a session on the other
- [ ] A session that moves between accounts mid-run is attributed per request, not per session

## Moving a machine onto accounts

- [x] Rows carrying neither an account nor a digest are counted apart from rows merely awaiting resolution
- [x] A history whose windows overlap in time is refused rather than attributed to one account (verifies spec: ACCT#moving-a-machine-onto-accounts)
- [x] Attributing a history moves every such row once, and running it again moves none
- [x] Every command but setup refuses while such rows remain, and says that setup attributes them
- [ ] Setup offers an account a local source names but that has never served, and attributing to it works
- [ ] Interrupting setup part-way through the move leaves the store readable, and running setup again completes it
- [x] A window re-anchored forward while open, and a reading whose window had already ended, are not read as a second account
- [x] The real store on this machine reads as one account's history: 96,013 measurements, 95,981 naming a live window, no conflicts (checked against a copy)
- [ ] `toker setup` attributes the real store's 96,991 pre-account rows, run against a copy first

## The dashboard

- [x] One account renders the panel exactly as it did before accounts existed
- [x] Several accounts render one column each, named, with the accounts that do not fit counted once on the row that names them
- [x] Every bar in a row is drawn at one width, so the row reads across (verifies spec: UTUI#meters-across-accounts)
- [x] A meter one account's reading does not carry reads as absent in its cell, never as an empty bar
- [x] Only accounts that served the window are shown, and where none did, the one that served most recently stands alone
- [x] The busiest leads, and two accounts within a tenth of each other keep a stable order
- [x] An address too wide for its column gives way to the account number rather than being cut
- [x] The arrow keys scroll the strip sideways, come to rest on whole accounts, and stop once the last account is in view
- [ ] The strip reads correctly under `NO_COLOR`, with the modifiers still carrying what colour did
- [ ] The strip at 80, 120 and 200 cells in a real terminal, against the mockups

## The accounts page

- [x] Every account shows with its number, address and when it was last seen (verifies spec: UTUI#the-accounts-page)
- [x] A credential that resolved to no account shows as its own entry, under `?` rather than a number
- [x] Naming an unresolved credential is what gives it an account, recorded as named by the operator
- [x] A page with nothing on it says so rather than rendering empty
- [ ] Naming an account that already has one renames it, through the credential that reached it
- [ ] The naming entry takes every key while open, `q` included, so an address containing one does not quit
- [ ] The page offers no way to add or remove an account (verifies spec: UTUI#the-accounts-page)

## Not broken by any of it

- [x] The narrow meter read still aggregates identically to a full-row read over the same ledger
- [ ] A machine with one account sees no change in the dashboard, the gate, the ping schedule or the ledger
- [ ] A provider with no accounts toker can tell apart — openrouter, the anthropic API — is untouched throughout
