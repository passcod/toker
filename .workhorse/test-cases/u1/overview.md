# Copy overage utilisation into the monthly header

## Copying

- [x] A Messages response carrying overage utilisation and no monthly header reaches the client with the monthly header set to the same bytes (verifies spec: WCANON)
- [x] A response carrying both headers reaches the client with the provider's monthly value, not the overage one (verifies spec: WCANON)
- [x] A response carrying neither header reaches the client with neither (verifies spec: WCANON)
- [x] A value that does not parse as a number is copied as sent (verifies spec: WCANON)

## Every forwarded response

- [x] A streamed Messages response gets the copy (verifies spec: WCANON)
- [x] An error status from the provider, such as a 401, gets the copy (verifies spec: WCANON)
- [x] A passthrough path such as `count_tokens` gets the copy (verifies spec: WCANON)
- [x] A translated route serving a Chat or Responses client from Anthropic gets the copy (verifies spec: WCANON)

## Staying quiet

- [x] A quota block answered by Toker carries neither header (verifies spec: WCANON)
- [x] The ledger row's meter snapshot carries no monthly key where the provider sent none (verifies spec: SLEDGR)

## By hand

- [ ] With overage drawn, a Claude Code session through Toker reports `overagePeriodMonthly` on its rate limit events
