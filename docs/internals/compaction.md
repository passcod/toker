# Detecting a compaction

The detection lives in `ir/anthropic.rs` (`compaction_of`,
`AnthropicShape::is_compaction`) and is a port of ctp's, including the fixes
below. Everything here was learned on ctp's log.

## `summarising` is not "compaction"

The flag means "this request carries the summarisation prompt". Claude Code
sends that same prompt for session titles and resume metadata, on a small model
with no tools, several times a minute: **145 of those against 6 real
compactions** over ctp's first fortnight. ctp's code carried a comment saying
exactly that from the day the marker was added, and its rewrite gate read the
flag as "compaction" anyway.

`is_compaction` asks the tool set as well. The separator is not size, which
would be a threshold to tune: a compaction *continues a session*, so it is sent
with that session's tools, because the summary has to be usable by the session
that resumes. The title/resume summariser is a standalone one-shot that cannot
call a tool and is sent with none. Measured, the two do not overlap (0 tools
against 46–181, 9 output tokens against 10k–22k), and they cannot, because one
is a continuation and the other is not.

Getting this wrong inverts the feature rather than weakening it. The routine
summariser's lane re-reads one big prefix every time (**20M cache reads against
552k fresh**, measured), so treating it as a dead end and stripping its
breakpoints would turn the cheapest traffic the proxy sees into the most
expensive. `summarising` alone is safe to *report*; nothing may act on it. The
TUI's rebuild walk does not read it at all, and counts compactions from the
generation marker instead (below).

ctp answered `null`, not `false`, where the tool count was absent, because its
rows predated the field. A shape toker extracts always knows its tool count, so
`is_compaction` is a plain bool. Rows are another matter: on a ledger row,
including every row imported from ctp, test `req_tools` for absence before
reading `summarising` as routine. `lanes_from_rows` reads a row with no tool
count as not a compaction, the verdict ctp gave rows it could not classify.

## A compaction is asked for in more than one wording

There is no single compaction prompt. Claude Code has a full compaction and a
partial one, which summarises one end of a conversation and keeps the other, and
the partial has a wording per direction:

| | opening |
| --- | --- |
| full | "Your task is to create a detailed summary of **the conversation so far**" |
| partial, from | "…of the **RECENT portion of the conversation**" |
| partial, up to | "…of **this conversation**" |

All three are prefixed with `CRITICAL: Respond with TEXT ONLY. Do NOT call any
tools.`, and nothing else in the client uses that preamble.

ctp matched the full wording only, for five days. Over that window **four of
nine compactions** carried a partial wording instead and were invisible: 1.76M
fresh tokens on Opus, about $10 the rewrite would have saved, one of them two
minutes after a notice recommending exactly that `/compact`. All four were on
cold lanes, so all four qualified.

So `COMPACT_PERFORMING` is a list, and neither half of the prompt is matched
whole: the instruction by the opening all three share, the preamble
independently, either one sufficient. A rewording of one does not take the
detection with it.

This is "absence of instrumentation must never read as absence of the
phenomenon" with a literal standing in for a missing field. A fixed string is
safe *because* it is chosen in advance, and that is exactly what makes it go
quiet when the other side adds a second path. ctp's smoke test did not catch it,
because it drove the wording that already matched. What caught it was counting
compactions a second way, by the lane message count collapsing with
`compact_generations` incrementing behind it, and finding nine where the view
said six.

Rows from before 2026-09-22 could not carry what did not exist, so any count of
compactions by `summarising` that reaches back past that date is a floor; see
[ledger-schema.md](ledger-schema.md). ctp's `--compactions` view said so through
a `PARTIAL_MARKER_SINCE` constant. toker has no such view yet; one that counts
by `summarising` must carry the same cutoff, and move it when the matching
changes again.

### Scoping to the last message is not an anchor

Widening the match immediately cost a false positive, and writing the table
above is what caused it: the last message is exactly where file listings land,
so a tool result quoting this document flagged an ordinary turn (132 messages,
138 tools, 281 output tokens) as `summarising`. That shape passes
`is_compaction`, so on a cold lane the proxy would have downgraded a live
conversation and dropped every breakpoint on it, which invariant 6 licenses only
for a dead end. It survived only because the lane was warm.

The prompt is assembled `preamble + wording + suffix`, and the preamble ends
with a blank line, so in a real compaction the preamble sits at offset 0 and the
instruction opens a line. Quoted prose and listings carry them mid-line. So the
markers are matched at a **line start** (`begins_line`), and a last message
carrying a `tool_result` is refused outright: Claude Code builds that message
from the prompt string alone, so the test can only exclude the wrong thing.

The line anchor is also what offset-zero anchoring could not be: message text is
joined across blocks on a newline, so a prepended block still leaves the marker
beginning a line. That is the failure that forced the continuation marker
(`COMPACT_RESUMED`) to give up its anchor and settle for a scope: it is counted
anywhere in the *first* message, and a first message that quotes it over-counts,
a limitation carried over from ctp.

Each guard has its own negative test (in `ir/anthropic.rs`), because either
alone leaves a hole: the line anchor misses a tool-result turn that quotes the
prompt faithfully, and the tool-result test misses a plain message that does.

### The last message is not the last turn

From the cutover until 2026-10-06 no row carried `summarising`, so every
compaction ran on the model it asked for with its breakpoints intact. One was a
485k-token Opus compaction a minute after a cold notice recommended the cheap
one. Claude Code still sent both markers at a line start in a plain user
message, but sessions with hooks carry mid-conversation `system` messages (hook
output, attachments), and one trailing the prompt was what the detector read.

So the markers are matched in the last message that is not a `system` message.
A system message is never the prompt itself: one carrying the wording does not
count, and skipping past them does not skip the tool-result refusal.

The miss was silent for the same reason the partial wordings were: a fixed
string matched at a fixed position reads as "no compactions" when the position
moves. Rows now carry `extra.compactMarker` whenever a wording appears anywhere
in the last four messages, whether or not it matched, recording how far from the
end it sat, the roles of the carrier and of the messages after it, and whether
it began a line or shared its message with a tool result. Positions and booleans
only, never text. A compaction the detector misses again leaves that object on
a row with `summarising` false; the count of such rows is the check.
