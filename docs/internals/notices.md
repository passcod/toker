# Gate notices

A gate does not return an error. It answers 200 with a synthetic assistant turn
whose text is the notice, in the wire form the request asked for (SSE or JSON;
`quota::Blocking::blocked_turn`, `cold::ColdBlocking::openai_turn`). That was
measured with ctp: a 529 is retried silently, a 429 is mislabelled as a rate
limit, and a 403 looks like broken credentials. A synthetic turn is the only
answer every client shows to the user as written.

## The text is part of the conversation

The client keeps the notice in the transcript and replays it on every later
request, and compaction keeps the recent tail, so a notice can outlive the
moment it was written by days. Two rules follow.

**It is byte-stable.** `notice::render` is a pure function of (style, level,
content), and the composers (`quota::Blocking::notice`,
`cold::ColdBlocking::notice_for`) take the clock and the timezone as arguments.
Numbers go through `quota::group`, a fixed comma grouping, never the host
locale: the TUI's locale formatting (`tui/locale.rs`) is for a person to read
and deliberately does not reach notices. The block style's width is frozen at 50
columns, because a width that varied with anything would change replayed
history. A changed notice string is a changed model-visible byte for every
conversation that later carries it; change one on purpose and say so.

**It is a record, not advice.** It is live for one turn and historical for the
rest of the conversation, so it is written as an event, stamped with the time it
fired. See "The notice is a record" in [cold-gate.md](cold-gate.md) for the
incident behind that.

No em dashes in notice text: a terminal font renders one two cells wide, and the
line reads as misaligned. Colons and commas do the same job. The quota notice
does not embed the release markers either: they would then sit in history as
assistant text. It names the over marker only while the blocked meter's plan
has room (`quota::plan_room`): with the plan spent or overage drawn, that marker
would be stopped again at once.

## One style per frontend

The frontend is named by the `/f/<frontend>` prefix of the base URL it was
pointed at (see [routing.md](routing.md)), and the `[notices]` table maps a name
to a `NoticeStyle`:

| Style | Rendering | Default for |
| --- | --- | --- |
| `gfm` | a GFM alert of the notice's level, `> [!CAUTION]` or `> [!WARNING]` | `default`: any unprefixed or unnamed frontend |
| `toker` | `> [!TOKER]` whatever the level | `workhorse` |
| `block` | the frozen backticked `★ Toker` header and footer around the lines | `claude`, which renders it and nothing else does |
| `plain` | the content in brackets | opt-in only |

The level is the composer's decision, not the renderer's: the quota block is
`NoticeLevel::Caution` (a session stopped until the operator acts), the cold
notice `NoticeLevel::Warning` (advice the operator may act on or ignore).

Brackets survive only in `plain`. When plain text was the only framing, they
were what marked the notice as harness output rather than the model's own words,
the shape the client's own injected notices take; the alerts and the block now
do that job. The old `[gates] notice_style` key still loads as `[notices]
default`, and naming both is a config error rather than a silent precedence.

## What each notice must say

The quota notice is the whole interface of that gate: the only place the user
learns which meter they hit, when it clears, and how to resume, so it says all
three. A reset the reading did not carry is "unknown", never a guessed time. The
session's context size is stated when the lane table knows it (the largest lane,
`lanes::session_prompt`) and dropped when it does not, rather than printed as a
zero that would read as a measurement.

The cold notice names a cheaper compaction model only when the retarget has
actually resolved one, since an unarmed proxy promising a cheap compaction would
be the feature lying about its own configuration. It states the re-read's
cache-write multiple only when the lane recorded its tier. The openai path's
cold notice names no compaction target at all, because that path never retargets
one.
