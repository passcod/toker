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

## Retargeting a cold compaction

Detection establishes only that a request is a compaction.
Which route and model it runs on MUST be decided by the active policy, as [RSOLV](../policy/resolver.md#lookup-order) specifies.

Toker MUST pass the resolver the request's class and the lane's own evidence of how cold it is.
Toker MUST NOT let the marker text reach policy.

### Proving a lane cold

Coldness MUST be proven from the lane's own record, being the time it was last updated against its recorded cache lifetime, with the configured minimum prompt size and idle floor applied.

Coldness MUST be read from the same clock the cold notice uses, and MUST NOT depend on whether that notice is enabled.

> [!NOTE]
> A user who runs a compaction after reading a cold notice must find the lane cold, or the notice recommended something the proxy then declines to do.

A compaction on a lane that is not cold by its own record has unknown temperature, which MUST rule out every move below.

Coldness here MUST NOT be delegated to the lane's pin.

> [!NOTE]
> Once the recorded lifetime has run out, no cache remains on any binding for a pin to protect.

### Choosing the target

A policy revision names either a route for compactions, or a model substitution applying to the route the request would have taken anyway.

A compaction route MUST apply only to a lane proven cold, and MAY move the compaction to a different provider as well as a different model.

A model substitution MUST apply only where all of these hold:

| condition |
| --- |
| the lane is proven cold |
| the body transform below would succeed |
| the target holds the lane's recorded prompt size |
| the target's input rate is strictly below the source's, after aliasing |
| the target has a known price |

Where the substitution names a family rather than a model, Toker MUST pick that family's newest model the binding has learned, under the same bar the upgrade election uses, which [RMODEL](../policy/models.md) specifies.
Where any condition fails, the compaction MUST keep its model and only the transform applies.

### The transform

Once the target is chosen, Toker MUST strip the compaction's cache breakpoints and merge its mid-conversation system messages into a safe host, together or not at all.

> [!NOTE]
> A cold lane would otherwise pay to write a cache nobody will read.

Toker MUST probe the transform on a copy of the body before resolving, so that a model is never moved for a body the transform would then decline.

Toker MUST record that breakpoints were stripped and that system messages were merged, and MUST record the model moved from and to.

> [!NOTE]
> By the time the transform runs the request is already on the new model, so the move cannot be read off the request itself.

A fallback attempt on a compaction MUST re-apply the transform for the model that attempt was given.

A compaction that detection missed MUST NOT be retargeted, whatever the policy names.
