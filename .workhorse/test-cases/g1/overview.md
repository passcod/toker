# Server safeguards

## Provider extensions on responses

- [x] A complete Messages response with an unknown top-level field, an unknown block kind and a known block with an extra field reaches a Messages client with all three in their original places (verifies spec: WCANON)
- [x] A streamed response with an unknown field on `message_start`, a block start, a delta and `message_delta` reaches a Messages client with each field on the event it came on (verifies spec: WCANON)
- [x] A streamed unknown block kind and an unknown delta kind replay at the renderer's own block index, and later blocks follow it (verifies spec: WCANON)
- [x] An unknown event kind between blocks replays where it arrived (verifies spec: WCANON)
- [x] An extension never overwrites a modelled field, stop reason or tool-use id (verifies spec: WCANON)
- [x] `thinking: disabled` removes reasoning and leaves a provider extension on the same turn in place, for a complete response and a stream (verifies spec: WCANON)
- [x] A Chat or Responses frontend omits every Messages extension and reports each by wire path and reason, never its value (verifies spec: WCANON)
- [ ] A provider event that arrives between `message_delta` and `message_stop` is carried (needs the observed response shape)

## The ask and its capability

- [x] A request with the `safeguards` body field asks; the beta value alone does not (verifies spec: WSAFE)
- [x] The Anthropic subscription and API bindings declare support, OpenRouter Messages declares unknown, and every other binding unsupported (verifies spec: WSAFE)
- [x] A supported binding receives the field and the beta together, with the client's other betas intact (verifies spec: WSAFE)
- [x] An unknown binding receives the field and the beta together (verifies spec: WSAFE)
- [x] A binding whose effective support is unsupported receives neither, and keeps the client's other betas (verifies spec: WSAFE)
- [x] A Codex route reports the ask as an incompatible-dialect loss and sends no beta (verifies spec: WSAFE)
- [x] No verdict is synthesised for a response that carries none (verifies spec: WSAFE)

## Rejection and the mark

- [x] An unknown binding that refuses the ask is sent the request once more without the pair, writing two rows in one attempt group (verifies spec: WSAFE)
- [x] A binding declaring support that refuses the ask is forwarded the refusal, not retried and not marked (verifies spec: WSAFE)
- [x] A refusal that names the announcing beta rather than the field is retried the same way (verifies spec: WSAFE)
- [x] A mark withholds the pair from later requests (verifies spec: WSAFE)
- [x] A mark survives a restart (verifies spec: WSAFE)
- [x] A catalogue refresh, a credential change and the control operation each clear a mark (verifies spec: WSAFE)
- [x] A refusal and a thinking rewrite named by the same response are applied in one extra send (verifies spec: WSAFE)

## Routing

- [x] An honouring route skips a candidate whose effective support is unsupported (verifies spec: RSOLV)
- [x] An unknown effective support never skips a candidate (verifies spec: RSOLV)
- [x] A route with no capable reachable candidate is walked again without the requirement, and the trace says so (verifies spec: RSOLV)
- [x] A route that does not honour the ask, and a request that does not ask, are untouched (verifies spec: RSOLV)
- [x] A fall after send continues under the same rule (verifies spec: RSOLV)
- [x] A route that never honours keeps its fingerprint across format versions, and one that does has its own (verifies spec: RDOC)
- [x] Honouring needs format version 4, and the edit operation raises an older document (verifies spec: RDOC)
- [ ] The preview shows each candidate's effective support (verifies spec: RREV)

## Ledger and operator surfaces

- [x] A row records the ask, effective support, mark, results seen and replayed, skipped candidate and walked again, and none of their contents (verifies spec: SLEDGR)
- [x] A request that did not ask records only that it did not
- [ ] The export carries the columns and the import leaves them empty (verifies spec: SLEDGR)
- [ ] The providers page shows each Messages binding's declared support and mark, and clears the mark (verifies spec: UTUI)
- [ ] The routing page previews an asking request and sets or clears a route's flag (verifies spec: UTUI)

## By hand

- [ ] Capture one real auto-mode turn against a dev build on a spare port and pin the response shape as a fixture with invented contents
- [ ] `/status` in Claude Code reads the auto mode server as enabled through an Anthropic route, and disabled after a turn served on `codex_sub`
