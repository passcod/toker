---
id: GNOTE
---

# Gate notices

A gate stops a request by answering it with a notice rather than with an error.
This spec covers the form a notice takes, why its bytes are fixed, the styles it renders in, and what each gate's notice states.

## A notice is an answer, not an error

A gate MUST answer 200 with a synthetic assistant turn whose text is the notice.
A gate MUST render that turn in the wire form the request asked for.

A gate MUST NOT answer with an error status.

> [!NOTE]
> Clients handle error statuses on the user's behalf: a 529 is retried silently, a 429 is reported as a rate limit, and a 403 as broken credentials.
> An assistant turn is the only answer every client shows the user as written.

## A notice is byte-stable

A client keeps a notice in its transcript and replays it on every later request, and compaction preserves the recent tail, so a notice outlives the turn it was written for.

Rendering a notice MUST be a pure function of its style, its level and its content.
A composer MUST take the clock and the timezone as arguments rather than reading them.

A number in a notice MUST be grouped by a fixed rule, and MUST NOT be formatted to the host's locale.
The block style's width MUST be fixed.

A notice MUST NOT contain an em dash.

> [!NOTE]
> A terminal renders an em dash two cells wide, so the surrounding lines read as misaligned.

Changing a notice's text changes a model-visible byte in every conversation that later carries it.

## A notice is a record

A notice is live for one turn and historical for the rest of the conversation.
A notice MUST be written as an event that happened, and MUST be stamped with the time it fired.

A notice MUST NOT contain a release marker.

> [!NOTE]
> A marker in notice text would sit in the transcript as assistant words and be read back as though the user had written it.

The quota notice MAY name the overage marker while the blocked meter's plan still has room, and MUST NOT name it once the plan is spent or overage is drawn.

## Styles

A request's frontend profile selects its notice style, as [ROUTE](../routing.md#frontend-profiles) specifies.

| style | rendering |
| --- | --- |
| `gfm` | a GitHub alert of the notice's level |
| `toker` | a single Toker-named alert whatever the level |
| `block` | a fixed header and footer around the notice's lines |
| `plain` | the content in brackets |

The `gfm` style MUST be the default for a profile with no style configured.

A notice's level MUST be chosen by the composer rather than the renderer.
The quota notice MUST carry the caution level, and the cold notice the warning level.

> [!NOTE]
> A quota block stops a session until the operator acts, where a cold notice is advice the operator may act on or ignore.

## The quota notice

The quota notice is the only place the operator learns that the gate fired, so it MUST state which meter was hit, when that meter clears, and how to resume.

Where the reading carried no reset time, the notice MUST say the reset is unknown, and MUST NOT state a guessed time.

The notice MUST state the session's context size where the lane table holds it, and MUST omit it otherwise rather than state a zero.

The quota gate is specified by [GQUOTA](quota.md).

## The cold notice

The cold notice MUST name the model a compaction would move to only where the active policy resolves one, and only where that model differs from the model the session is on.

Toker MUST answer that question by resolving a compaction-class plan against the same facts as the turn it is stopping, and that resolution MUST change no lane state.

The cold notice MUST state the re-read's cache-write multiple only where the lane recorded its cache tier.

A cold notice on a route that does not retarget compactions MUST NOT name a compaction target.

> [!NOTE]
> Naming a cheaper compaction the policy has not resolved would promise a configuration that does not exist.

The cold gate is specified by [GCOLD](cold.md).
