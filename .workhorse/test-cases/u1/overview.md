# Copy overage utilisation into the monthly header

## Copying

- [ ] A Messages response carrying overage utilisation and no monthly header reaches the client with the monthly header set to the same bytes (verifies spec: WCANON)
- [ ] A response carrying both headers reaches the client with the provider's monthly value, not the overage one (verifies spec: WCANON)
- [ ] A response carrying neither header reaches the client with neither (verifies spec: WCANON)
- [ ] A value that does not parse as a number is copied as sent (verifies spec: WCANON)

## Every forwarded response

- [ ] A streamed Messages response gets the copy (verifies spec: WCANON)
- [ ] A rejected 429 from the provider gets the copy (verifies spec: WCANON)
- [ ] A passthrough path such as `count_tokens` gets the copy (verifies spec: WCANON)
- [ ] A translated route serving a Chat or Responses client from Anthropic gets the copy (verifies spec: WCANON)

## Staying quiet

- [ ] A quota block answered by Toker carries neither header (verifies spec: WCANON)
- [ ] The ledger row's meter snapshot carries no monthly key where the provider sent none (verifies spec: SLEDGR)

## By hand

- [ ] With overage drawn, a Claude Code session through Toker reports `overagePeriodMonthly` on its rate limit events
