# Direction B: one page, one tree

Mockups: `.workhorse/design/mockups/x1/b-01-first-open.html`, `b-02-policy.html`, `b-03-narrow.html`, `b-04-states.html` (16 frames; 120x40 except frames 11 and 12 at 100x24 and 13 and 14 at 80x24).

Names in the frames come from the live captures.
Counts and ids that the captures do not show are illustrative: the split of the 1,399 offers across the seven bindings (the total, 1,416 less 17 other records, is derived), the fifteen presentation scopes (only `claude/anthropic_messages` was captured), the matchers of the rewrites after the first, the model ids in the offers, and the "reached by 3 rewrites" count, which is read off the `legacy-prefix-*-anthropic_sub` ids.
Account addresses are invented.

## The structure, in one sentence

The two pages and six tabs become one CONFIGURATION page: a tree of everything configurable on the left, whose branches carry counts and health, and the detail of whatever is selected on the right, which leads with the few facts that matter and keeps the rest one arrow deeper.

## What the design does

**One tree, five top-level branches.**
`providers`, `settings`, `policy`, `revisions`, `accounts`.
The accounts page is folded in, so Tab cycles dashboard and configuration only.
Bindings sit under their provider and offers under their binding, so the 1,399 offers are not a list: they are `38 offers` on a row, folded.

**Policy's children are in lookup order, not alphabetical.**
`1 presentations`, `2 rewrites`, `3 defaults`, `4 routes`, then `classifiers`, `upgrades` and `compaction`, which are not stages and carry no number.
Selecting `policy` leads with that order in words (frame 4).
Rewrites sit under their scope, because priority is only unique inside one, and read in priority order with the rank leading.
A route's candidates are the detail, an ordered list with the fall rule between steps.

**Default expansion.**
Open what needs the operator: `providers`, because one is on but not configured, and `policy` one level so its stages show.
Everything else is closed and says what it holds.
The cursor always starts on the top row, so position is predictable.
After that the page remembers what the operator opened.
A healthy install opens nothing but `policy`.

**Depth.**
Policy goes branch, collection, record, which is three levels, and the candidate list stops there.
Catalogue goes provider, binding, offer, also three.
Candidates are detail, not branches, because a candidate has no life apart from its route, its id means nothing, and its order is its meaning.
A branch would also add a fourth level of indentation for nothing the operator selects on its own.

**Cells.**
No guide lines: two cells of indent, a one-cell marker, a one-cell left margin for change marks.
Hoisting a shared id prefix onto the branch (`classifiers  5  legacy-family-*`) removes about 14 to 20 cells per row.

**Draft and live, with no validate step.**
The first line of every detail says when an edit takes effect: `live` for providers and settings, `on apply` for policy, `observed` for learned things, `history` for revisions.
Each field is validated while it is edited and says what is wrong beside itself (frames 3 and 12), and its save control is dim with the reason.
Because an invalid field cannot be saved, a change that exists is a valid one, so there is nothing to review.
The tree shows what differs from the live revision with `~` and `+` in the margin, carried down to every collapsed parent, and the header says `3 changes not applied   [a] apply`.
`a` applies to new lanes, which needs no consent.
The detail's `applies to` row is where the other scopes and the warm-cache consent would live, validated in the same way.
`V` is gone; diagnostics become messages beside the record they concern.

**No JSON.**
No frame shows a record as JSON or asks for it to be typed.
Regexes appear as matchers, which is what they are.

**Narrow.**
At 100 columns the tree sheds its right-hand counts and clips long names, and the detail keeps its width.
At 80 they cannot share, so the detail takes the whole page when the operator goes deeper, with the same key (right), and left or esc comes back to the same cursor.
Below 100 the current routing page stacks list over detail instead; a tree is too tall to stack.

## What an operator sees on the main frame

First, the tree, with `providers 5 · 1 not configured` in yellow: where to look, before opening anything.
Second, the header, `revision 2 live · no changes`: whether anything is pending.
Third, the detail of the selected row: what it is and the first thing to press.
That order is attention, then state, then action.

## Steps, this direction against today

Counted as key presses from the dashboard, not counting typed values, and approximate.
Today's figures are the survey's.
"Find" is `/`, which jumps to any node by name.

| task | today | here |
| --- | --- | --- |
| enable a provider | about 5 | 4 (Tab, down, down, space) |
| set an endpoint | about 8 | 5, and the entry starts with the current value |
| put in a key | about 9 | 5 (Tab, down, down, k, enter) |
| turn a gate on or off | 4 to 5 | 4 with find; about 10 by walking, 2 on the second visit |
| change a route's model | 12 to 20 | about 9 (Tab, find, enter, right, down, enter, enter, a, and one more if the scope is changed) |
| add a fallback | 30 or more (22 downs) | about 9, and the new step starts armed to fire |
| promote a model | about 5 | 4 with find, and the control says why it is refused when the model was never served |
| validate | 3 | 0, it is continuous |
| activate | 3 to 5 | 1 to 2 |
| see what the draft changes | no view | 0, it is on the tree |
| reactivate a revision | about 5 | about 6 |
| import toker.toml | about 4 | 3, only offered on an empty policy |

## Keys

Routing bound 18 top-level keys plus 8 in a form, and config 5 more.
Here: 14 rows of keys, of which the always-learned set is 12 (select, close or open, enter, space, n, d, a, `/`, r, esc, tab, q); 7 keys that act on one kind of node only (`k e c` on a provider, `p` on an offer, `x` on a revision, `v` on policy, `i` on an empty policy), which also show as controls on the node; and 4 rows in the detail.
`e`, `V`, `1-4`, `ctrl+s`, `ctrl+d`, `pgup/pgdn` and the add chooser are gone.
`n` knows the kind from the branch it is pressed on.

## What this direction is worse at, plainly

**A tree expresses containment, not ordered evaluation, and the operator's model is a routing table.**
This is the main limit.
I carry order three ways: stage numbers on policy's children, rank and sibling order inside a rewrite scope with `alt+↑/↓` to move a row, and an ordered numbered list in a route's detail.
That covers order within a table.
It does not show order across tables.
A name passing through presentations, then rewrites, then defaults, then candidates is a pipeline, and the tree shows it only as four numbered siblings plus a paragraph in the policy detail.
The question "which rule catches this name" is answered by `v`, a trace, not by looking.
Profile-before-protocol precedence is also only a sentence.
Where order is the whole point, a table per stage, not a tree, is the right shape: rewrites across scopes, and a route's candidates with their fall rules.
I put the candidates in a detail pane because it is the nearest thing to a table this design has.
A direction built on tables would show that better.

**The detail pane still holds the editing.**
Candidates, edges and field editing live in it.
It is quieter than today's forms, because core facts lead and the rest is one arrow in, but the tree has not removed the need for a place to edit a record.

**Walking costs.**
Reaching a distant node is more presses than switching a tab, and a gate toggle walked to by hand is about 10 presses.
`/` is the answer and has to be learned.
Because everything is one tree, the first open shows only a summary, and an operator who wants two things side by side, a route and the provider it uses, has to move between them.
References across branches (a classifier used by an upgrade, a candidate's provider) are text in the detail, not rows, because a record has one parent.

**Removed records have no row.**
A removed default appears only in the policy detail's list of changes, as `-`, and in the branch count.

**Choices that look arbitrary.**
Offers sit under provider and binding while classifiers, which are also per binding, sit under policy, because one is observed and the other configured.
Scope is a branch for rewrites and a column for presentations, because rewrites have several rows per scope and the fifteen presentations have one.

**Deleting the accounts page** costs a dedicated surface for something that is observed, not configured, and puts it beside things that are configured.

**Three words on the tag line** (`live`, `on apply`, `observed`) plus `history` and `editing` are new vocabulary that has to be learned and are in the overlay's marks.

## Marks added, and their legend rows

`▸ ▾` a branch, closed or open.
`1 2 3 4` the order a name is looked up.
`name-*` the id prefix the rows below share.
`+ ~ -` added, changed or holding a change, removed.
The tag words `live`, `on apply`, `observed`, `history`.
`editing`, used in frame 12, is not yet in the overlay and should join the tag row.
All are in the overlay in frame 16 except `editing`.

## Internal changes that would make this materially easier

1. **Apply each valid edit at once, on new lanes.**
   The draft would stay internal, and lanes would keep pinning to revisions, but the operator would never meet an unapplied change, so `[a]`, the changes list and the `~` marks would go, leaving only a separate path for the two scopes that need consent.
   This is the fullest answer to "no validate-then-review-then-activate".
   Without it, the page can only hide the ritual, not remove the wait.
2. **Per-field validation in the control operations.**
   A compile of the draft with an edit applied, returning the field path and a message, lets a field be validated before it is saved.
   Today diagnostics come back after a compile of the whole draft, with a path and a code.
3. **The snapshot should carry the diff against the live revision**, and refresh `derived` facts after every edit.
   The page currently derives `changed records` itself, and `derived` goes stale after an edit.
4. **A credential operation that takes a source (keyring or an environment variable)**, so the provider fix can be finished on the page, not sent to `toker setup`.
5. **Settings edits should merge, not replace.**
   Today a whole-row write drops `session_headers`, `transcript_roots` and `refresh_url`.
6. **Names for records.**
   Routes and rewrites have ids that carry their kind and their target.
   A display name field the page may show would remove the need to hoist prefixes.
7. **Server-side filtering of offers.**
   1,399 offers in every snapshot is only needed for the `/` find.
