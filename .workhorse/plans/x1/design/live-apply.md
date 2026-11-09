# Applying live, and what the specs allow

The operator's steer is that there should be no validate-then-apply ritual: validation happens continuously, an invalid form cannot be saved, and a change goes live without a review step.
This note works out how much of that the specs already permit, because the answer decides what every direction draws for the draft and activation surface.

The short answer is that almost all of it is already permitted, and the validate command is redundant today.

## Validation is already continuous, server-side

[RREV#draft](../../specs/routing/revisions.md) requires that a draft be edited by a bounded atomic list of typed operations, and that **the whole result be validated before it is stored**.

So an invalid draft cannot exist.
The control surface already refuses an invalid edit: the committed form snapshot shows the refusal arriving as

> `! draft-edit: invalid_policy_edit (HTTP 422): id already exists`

The separate `V` command therefore adds nothing to validity.
Its response is `{"kind":"validation","valid":true,"diagnostics":[…]}`, and against the real store the diagnostics are three `binding-unconfigured` advisories at `info`.
Those advisories are the only content `V` carries that the editing path does not.

**So `V` is deleted, and its diagnostics move onto the records they are about.**
A candidate naming an unconfigured binding says so on the candidate, continuously, rather than in a report the operator has to think to ask for.

What has to change is not the validation but where the refusal appears:

- today it appears after `ctrl+s`, in transport language, on the last line of a modal;
- it must appear beside the field as the value is entered, in the operator's language, with the save unavailable until it is fixed.

The design language already has the form for an unavailable control: it stays visible and dim with the reason in place of its label.

## Activation cannot be deleted, and should not be

[RREV#activation](../../specs/routing/revisions.md) makes activation a single transaction that inserts a revision, makes it active, replaces the draft at the next generation, and hands every publication adapter the new desired revision.
Lanes pin to revisions, a pinned revision must be kept and compiled on demand, and the active pointer must never move backwards.

So a revision is not a save point that can be created per keystroke.
Activating on every edit would mint a revision per keystroke, and each one a lane pins to has to be kept forever.

Activation also carries a choice the operator cannot be spared, because it decides what happens to work already in flight:

| scope | what becomes of a lane already pinned to an earlier revision |
| --- | --- |
| new lanes | it keeps resolving under the revision it is pinned to |
| new and cold lanes | a lane provably cold moves; the rest stay |
| immediate | the next request resolves under the new revision |

An immediate activation that would lose a warm cache must require explicit consent, and must be refused while any lane is warm or not provably cold and that consent is absent.

## What this means for the design

The web paradigm the operator named maps onto this cleanly, with the draft read as unsaved changes rather than as a document:

- **Editing is live and validated.** A field says what is wrong beside itself, and a save that would be refused is unavailable before it is attempted, not after.
- **The draft is "changes not yet live", and is stated as a count, not as a generation number.** `draft 2` tells the operator nothing; "3 changes not live" tells them what to do.
- **Putting changes live is one key, with no modal and no report**, at the default scope of new lanes, which needs no consent and disturbs nothing in flight.
- **The consent survives, for the one case that earns it.** An immediate activation that would lose a warm cache is a destructive action, and the design language already requires a destructive action to arm rather than fire. That is the only modal left in the flow.
- **Diagnostics are continuous and attached**, shown on the record they concern, rather than gathered into a report behind a command.

No spec change is needed for any of this.
`V` is a command the TUI chose to bind; the spec never required the operator to run one.

## The one internal change worth proposing

`RREV` requires exactly one draft, held in the store so that two editors share it.
With one operator on a couple of machines, the draft is always theirs, and its generation number is an implementation detail that has leaked into the header of every routing screen.

Nothing needs to change in the store for the number to stop being shown.
Whether the draft should also auto-activate after a quiet period, so that "changes not live" cannot be forgotten, is a real question, but it conflicts with the warm-cache consent and should be left alone until the rest is built.
