# Holding recaps on a cold lane

Claude Code's recap ("The user stepped away and is coming back. Recap in under
40 words…") is a fork of the main conversation: same tools, same cache
parameters, main model, sent with `skipTranscript` so its reply is shown in the
UI and never enters the conversation. Its own staleness check runs on Claude
Code's clock, which the end of any turn resets, a synthetic cold notice
included.

On 2026-10-06 that is what happened to session 260907aa: the notice answered
the user's prompt, a recap followed three seconds later, the gate had already
spoken that spell so it forwarded, and the recap rewrote 405k of cache ($3.26)
for a reply nobody needed. A recap arriving *before* the prompt would instead
spend the lane's one notice on a UI-only line and let the real prompt through
unwarned.

## Detection

- `AnthropicShape::recap`: the last non-system message begins a line with the
  recap prompt's opening sentence and carries no tool result (the compaction
  detector's position rules).
- Ordinary rows carry `extra.recap: true` when set, so what forwarded (warm)
  recaps cost stays measurable.

## The hold

- `cold::decide_cold` gains the recap flag and a `ColdDecision::Recap` verdict:
  a recap on a lane that is cold (the `coldness` test, after the request-bound
  check) is held, before the once-per-spell rule and the outlook, and whether
  or not the cache writes are free. Its value is a sentence of UI text.
- Held means a synthetic reply, one fixed plain line (no notice frame: the
  client shows it as a dim recap line, not a transcript turn), in the wire form
  asked for. The lane is not touched: neither `at` nor `noticed_at` moves, so
  the notice stays armed for the real prompt and the compaction still sees the
  lane cold.
- A new `cold-recap` row kind records it, with the idle and prompt figures the
  `cold` row carries.

## Docs

- `cold-gate.md`: the recap section (the incident, why hold rather than
  forward, why it never spends the notice).
- `ledger-schema.md`: the row kind and `extra.recap`.
