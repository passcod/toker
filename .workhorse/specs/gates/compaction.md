---
id: GCOMP
---

# Detecting a compaction

A compaction is a request that summarises a conversation so the session can continue from the summary.
This spec covers how a compaction is told apart from other summarising traffic, how it is detected despite the prompt moving, and how a cold compaction is retargeted.

## A summarisation flag is not a compaction

A client sends its summarisation prompt for session titles and resume metadata as well as for compactions, many times more often than it compacts.

Toker MAY report that a request carries a summarisation prompt.
Toker MUST NOT retarget or rewrite a request on that evidence alone.

A compaction continues a session, so it carries that session's tools.
A title or resume summariser is a standalone request that cannot call a tool and carries none.

Toker MUST distinguish the two by the request's tool set.
Toker MUST NOT distinguish them by the size of the request or of its output.

> [!NOTE]
> The routine summariser re-reads one large prefix on every call, so treating it as a dead end and stripping its cache breakpoints would make the cheapest traffic Toker sees into the most expensive.
> Size would be a threshold to tune; the tool set follows from one request being a continuation and the other not.

A ledger row that carries no tool count MUST be read as not a compaction.

## The prompt comes in more than one wording

A client has a full compaction and a partial one, and the partial has a wording for each direction it summarises.
Every wording shares a preamble instructing the model to answer with text only and call no tools.

Toker MUST match a list of wordings rather than a single prompt.
Toker MUST match the shared preamble independently of the wordings.
Either the preamble or a wording MUST be sufficient on its own.

> [!NOTE]
> A fixed string is safe because it is chosen in advance, and that is also what makes it go quiet when the other side adds a second path.
> Matching the two halves independently means a rewording of one does not take detection with it.

## Where a marker is matched

A marker MUST be matched only at the start of a line.

> [!NOTE]
> The prompt is assembled so that its preamble ends with a blank line and the instruction opens a line, where quoted prose and file listings carry the same text mid-line.

Toker MUST match markers in the last message that is not a system message.

> [!NOTE]
> A session with hooks carries mid-conversation system messages, and one trailing the prompt is otherwise read in the prompt's place.

Toker MUST refuse a candidate message that carries a tool result, whether or not its markers begin a line.

> [!NOTE]
> A client builds the compaction message from the prompt string alone, so this test can only exclude the wrong thing.
> The line anchor alone misses a tool result quoting the prompt faithfully, and this test alone misses a plain message that does, so each needs its own negative test.

The continuation marker MUST be counted anywhere within the first message.
A first message quoting that marker therefore over-counts.

## Recording an unmatched wording

Toker MUST record, on every request, whether any wording appeared anywhere in the last few messages, whether or not it matched.

That record MUST carry how far from the end the wording sat, the role of the message carrying it, the roles of the messages after it, whether it began a line, and whether it shared a message with a tool result.
That record MUST carry positions and flags only, and MUST NOT carry message text.

> [!NOTE]
> A compaction the detector misses leaves this record on a row marked as not a compaction, so the count of such rows is the check that detection still works.

A count of compactions reaching back before the detection field existed is a floor, and MUST be reported as one.
Which rows predate which fields is specified by [SLEDGR](../store/ledger.md).

## Retargeting

Detection establishes only that a request is a compaction.
Where a cold one is sent, and the transform that follows, are specified by [RCOMP](../routing/compaction.md).
