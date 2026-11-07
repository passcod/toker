---
id: GCOLD
---

# The cold-cache gate

The cold-cache gate warns an operator before a request re-reads a prompt cache that has expired.
This spec covers which requests it considers, when it stays quiet, how a recap is held, and the two clocks it keeps.

## Only a request that could be the re-read

A lane records how large a prefix was cached and how long ago it was touched.
It does not establish that this request is the one about to re-read it.

The gate MUST take the request's own upper bound on its prompt size.
Where that bound falls below the configured minimum, the gate MUST forward the request before consulting the lane.
Where the bound is unknown, it MUST decide nothing either way.

A bound MUST be able to rule a request out and MUST NOT be able to rule one in.
The lane must still be both cold and large.

> [!NOTE]
> Subagents share their parent's session identifier, so a fresh subagent carrying a sibling's tool set lands in that sibling's lane and would otherwise be stopped over a re-read it was never going to perform.

## Staying quiet

On a provider that reports meters, the gate MUST stay quiet unless the short window is already projected to run out before it resets, with the re-read's cost added.

This MUST NOT be expressed as a test of whether the re-read tips the window over.

> [!NOTE]
> A re-read is a small fraction of a window, so it could only tip a projection already landing within that fraction of the target, and none observed has.
> It brings an existing wall forward by minutes, against walls hours out, and the notice states that effect.

Where any input to the outlook is uncertain, the gate MUST behave as though the outlook did not exist.
Where any input to the outlook is uncertain, the gate MUST speak.
An absent fit, an absent weight, a weight the data cannot separate from zero, a rolled window and too little history for a burn MUST all count as uncertain.

> [!NOTE]
> A withheld notice is a re-read the operator never hears about, so this may be less useful than it could be and may not be silently wrong.

A withheld notice MUST be recorded as its own row kind, carrying the estimate the decision rested on.

> [!NOTE]
> A gate that has stopped working looks exactly like a gate with nothing to say, and that row is the only thing separating them.

The outlook MUST read only the routed provider's own meters.
The outlook MUST price the re-read as the effective model policy resolved, after any upgrade, retarget and alias.

> [!NOTE]
> The fit's weights are keyed on the identities a provider served, and the name a client requested is not one of them.

The long window MAY veto a notice and MUST NOT be given an estimate.

> [!NOTE]
> The weights are fitted against short windows and say nothing about a weekly one, but whether the weekly meter is heading for its own wall needs no weight.
> Treating an unmeasurable weekly meter as a reason to fire would silence the feature whenever the ledger is young.

Weight fitting and pricing are specified by [GQUOTA](quota.md), and projection by [GCAST](forecasting.md).

### Where cache writes are free

Before consulting the outlook, the gate MUST check the fetched catalogue for the model's cache write price.
Where the catalogue reports that writes cost nothing, the gate MUST record a withheld notice instead of speaking.

Only a positive report of free writes MUST exempt a request.
A model the catalogue does not know MUST NOT exempt one.

On a protocol with no meter source, this MUST be the only check.

## Holding a recap

A recap is a fork of the conversation whose reply is shown to the user but never enters the transcript.

The gate MUST hold a recap on a cold lane rather than forward it.
This MUST be decided before the once-per-spell rule, before the outlook, and before the free-writes exemption.

> [!NOTE]
> Forwarding a recap is never the operator choosing to pay for the re-read, and a recap's worth does not depend on the re-read's cost.
> A client decides whether to send a recap on its own clock, which the end of any turn resets, including a turn carrying a notice.

A held recap MUST be answered as plain text without a notice frame, since it is not a transcript turn.
A held recap MUST be recorded under its own row kind.

A held recap MUST leave the lane exactly as it found it, moving neither clock.

> [!NOTE]
> A recap arriving before the prompt would otherwise take the notice meant for the prompt, spending it on a line the transcript never keeps and letting the real request through unwarned.

A recap MUST be detected under the same position rules as a compaction, which [GCOMP](compaction.md#where-a-marker-is-matched) specifies.

## Two clocks

A lane carries two timestamps.

| clock | meaning | moves |
| --- | --- | --- |
| touched | when this lane's cache was last written or read | only on a response the provider actually served |
| noticed | that this idle spell has already been spoken about | only when a cold notice fires |

A cold notice MUST record the noticed clock and MUST NOT move the touched clock.

> [!NOTE]
> Coldness for retargeting is judged by the touched clock, so a notice that moved it would make the lane read as active, and the compaction the notice just recommended would no longer qualify.

The notice MUST re-arm only when the lane becomes active and then goes idle again.
The notice MUST NOT re-arm on a timer.

A withheld notice MUST move neither clock, so that a later request in the same spell is judged afresh against meters that may have tightened.

### Two questions, not one

Whether the operator should be interrupted and whether the cache is gone are different questions.

Anything deciding what to do with the cache MUST ask whether the cache is gone.
Only the notice MUST ask whether to interrupt.

Retargeting a compaction MUST be gated on the compaction test and the lane's coldness alone.
Retargeting a compaction MUST NOT be gated on whether the notice is enabled or has already fired.

> [!NOTE]
> The licence to treat a conversation as cold belongs to the lane, not to the notice.
> Asking the interrupt question in the cache's place has already caused a compaction to run on the expensive model one keystroke after a notice promised the cheap one.

## A lane's cache lifetime

A lane's cache lifetime MUST be taken from the longest-lived cache tier the lane has been seen writing, and that tier MUST be sticky.

> [!NOTE]
> A warm turn writes a small delta at a short tier on top of a prefix written at a long one, so reading the delta's tier as the prefix's lifetime fires the notice minutes into an idle spell on a cache that is plainly live.

An unrecorded tier MUST be read as the longest.

> [!NOTE]
> Guessing short fires on a live cache and costs the operator a turn; guessing long only delays a notice that was true.
> The policy's lane transitions answer a different question and default the other way, as [SLANE](../store/lanes.md) specifies, because a wrong verdict there loses a cache rather than costing a turn.
