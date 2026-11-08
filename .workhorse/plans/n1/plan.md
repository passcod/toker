# Multiple local Anthropic accounts

Implement the account model described by ACCT and thread it through credential resolution, request attribution, meter state, gates, pings, setup and the TUI.

## Implementation

- [ ] Add append-only schema migrations and store types for accounts, credential digests and account-aware meter, allowance, ping and ledger state.
- [ ] Resolve Anthropic subscription credentials from local accounts and capture a non-secret digest for every credential sent upstream.
- [ ] Discover and resolve account identities without delaying requests, then retroactively attribute rows for a resolved digest.
- [ ] Key cache identity, meter state, quota fitting, allowances and pings by account.
- [ ] Migrate historical unattributed rows through setup and refuse other commands until setup completes.
- [ ] Add account snapshots and controls to the service and render the account-aware dashboard and accounts page.
- [ ] Add focused tests for migration, attribution, account isolation and account-aware rendering.
