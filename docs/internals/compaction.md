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

## Choosing the model a cold compaction runs on

Detection only says a request is a compaction. What it then runs on is routing's
decision, and it is made by the active policy revision, not by a rule in the
handler. The handler gives the resolver one content-free fact, the request class
`Compaction` (`policy::RequestClass`), with the lane's own evidence of how cold
it is; the marker text never reaches policy.

**Coldness is proven from the lane's record.** `cold::lane_is_cold` reads the
lane's `updated_ms` and its recorded TTL (and the configured minimum prompt and
idle floor), the same clock the notice uses and not the notice's toggle, so the
`/compact` a user runs after reading a cold notice still finds the lane cold. A
compaction on a lane that is not cold by its own record has unknown temperature,
which is enough to rule out every move below. It is also deliberately not the
pin's job: once the recorded TTL has run out, no cache remains on any binding
for a pin to protect.

A revision's `compaction` section (`CompactionPolicy`) then has one of two
targets, each with its own lane and cache transition policy:

- **A route.** An authored revision can name a route. When the request is a
  compaction and the lane is proven cold, that route replaces the ordinary
  lookup, with provenance `compaction` (trace `lookup` source `compaction`), and
  its candidates resolve like any other chain, fallback included. It can
  therefore move a compaction to a different backend, not only a different
  model. The route applies only to a lane proven cold; its transition policies
  bound what else it may do.
- **The revision-one overlay.** The generated revision one carries the behaviour
  toker had before routing was a policy, as data: the document holds a
  `RevisionOneLegacy` target (`model_spec`, from `[gates] compact_model` and
  `sonnet` by default, and the Messages bindings it may apply to, which exclude
  OpenRouter). Authored policy cannot select it, but it stays in revision one
  and in any reactivation of it. It leaves the route the request would have
  taken and only changes the model on it, and only when all of these hold: the
  lane is cold, the body transform below would succeed, and the model it names
  fits and is cheaper. A bare family name picks that family's newest model the
  binding has learned under the same adaptive bar the upgrade election uses (see
  [models.md](models.md)) and the catalogue can price; a full id is taken as
  given. The target must hold the lane's recorded prompt (the offer's context
  window, else the largest prompt the binding has served it) and its input rate
  must be strictly below the source's, after aliasing. A model with no known
  price is never a move. If any condition fails the compaction keeps its model
  and only the transform applies.

**The transform follows the choice.** After policy has picked, the compaction
body is rewritten (`middleware::canonical::retarget_compaction`): cache
breakpoints are stripped, because a cold lane would pay to write a cache nobody
will read, and system messages are merged into a safe host. It is
all-or-nothing, and the handler probes it on a clone before resolution so the
overlay never moves a model for a body it could not then rewrite. Cache
stripping is recorded in `cache_stripped` and `system_merged`; the model move is
read off the attempt plan (`AttemptPlan::compaction_move`) into
`downgraded_from` and `downgraded_to`, because by the time the transform runs
the request is already on the new model and could not report it. A same-model
strip records the flags and no move. A fallback attempt on a compaction
re-applies the transform for the model it was given.

Which wording and position were detected is unchanged by any of this: a
compaction missed by detection is not retargeted, whatever the policy says.
