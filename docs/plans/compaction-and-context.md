# Compaction detection and context fit

Session 260907aa compacted on Opus at 485k tokens on 2026-10-06, minutes after
a cold notice, for $2.17 plus a $3.26 rewrite on the turn before it. Three
things went wrong, each enough on its own.

## 1. Detection never fires

No row since the cutover has `summarising = 1` or `compact_generations > 0`.
Claude Code 2.1.280 still sends both markers, at line start, in a plain user
message (reproduced against a fake upstream). The live session carries
mid-conversation `system` messages (hook output, attachments), and the
detector reads only the literal last message, so a trailing `system` message
hides the prompt.

- `compaction_of` reads the last **non-system** message for the performing
  markers (tool-result refusal and line anchor unchanged).
- Record where a performing marker sits when it is found in any of the last
  few messages, matched or not: its distance from the end, the role of the
  message carrying it, the roles trailing it, whether it began a line, whether
  that message carries a tool result. Positions and booleans only (invariant
  1), in `extra.compactMarker`, so the next layout change leaves evidence
  instead of silence.
- Tests: a trailing system message, several, a trailing system message after
  a tool-result turn (still refused), and the diagnostic staying absent on an
  ordinary turn.
- Docs: `compaction.md` gains the trailing-system section; `ledger-schema.md`
  the extra field and the date before which `summarising` is a floor.

## 2. Context fit uses the provider's declared window

`fits_context` consulted only the learned `max_prompt`, so `claude-sonnet-5`
(248k learned) was refused for a 485k compaction though anthropic's
`/v1/models` lists it at 1,000,000.

- The fit check takes the fetched listing's window for the identity the
  backend will actually send (`FetchedCatalogs::context_window_of(provider,
  model)`), and that is authoritative: a prompt over it declines, a prompt
  within it fits, whatever was learned.
- Only when the listing names no window does the learned `max_prompt` decide,
  as now.
- Applies to every caller: the compaction retarget, the cold notice's named
  target, force-newest.
- Docs: `models.md` "Two context facts" rewritten for the new order.

## 3. The learned ceiling forgets older history

`max_prompt` and served days are learned at record time and reseeded at
startup from only the newest `SEED_ROWS` (20,000) rows, so the ctp-era rows
that proved `claude-sonnet-5` at 398k never reached the table.

- Reseed the learned table from an aggregate over the whole ledger (per served
  identity, per 15-minute bucket: every real UTC offset is a multiple of 15
  minutes, so local days stay exact), keeping the 20k tail only for the lanes
  and the served-recency map.
