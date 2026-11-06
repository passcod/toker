# Lane identity, cache identity and pins

[lanes.md](lanes.md) says a lane is the unit to compare requests within. This
file is what policy routing adds on top: a lane also remembers which backend and
model its cache lives on (the pin), and every move of that pin goes through one
decision. Resolution itself is in [policy.md](policy.md).

## Two identities

The **lane** is the key `session_id|tools_hash` (`lanes::lane_key`). It says
which conversation-shaped stream a request belongs to, and it is all the
request can say about itself: the client names neither the backend nor the
cache.

The **cache identity** is what the provider keyed the prompt cache on, and toker
can only know it from where it sent the request: the binding and the exact model
sent upstream (`PinIdentity::same_cache_as`). A lane has one cache identity at a
time. The same lane sent to another backend, or to the same backend on another
model, reads nothing of what was written, however stable the client's own model
string stayed. That is the failure the pin exists to prevent: a virtual slug
(`fast`, say) whose route was edited to another provider makes two unrelated
provider contexts look like one warm lane if only the slug is compared.

A binding may declare a **cache domain** (`CacheDomain`: an id and a TTL),
meaning prompt-cache continuity verified for it. Today only
`openrouter.openrouter_chat_completions` has one, a 10-minute sticky session.
The compiler uses the domain id to decide whether an edge between two bindings
crosses a cache; at run time the cache identity is the binding id and effective
model. A binding with no domain is not "uncached": it means no verified
duration, so its lanes read their TTL from what the lane recorded (below).

The pin also carries the **route fingerprint** ([policy.md](policy.md)) because
the effective model alone is not enough to know a lane is still on the route it
was resolved under. A pinned lane starts its candidate walk from its pinned
candidate only while the route's fingerprint is the one pinned; a route whose
semantics changed starts from its first candidate again.

## The pin

A pin is seven columns on the lane row (`LanePin`, migration v7): the policy
revision, the route id and fingerprint, the candidate, the binding, the baseline
model (before upgrade, alias or retarget) and the effective model, plus a
`pin_generation`. They are all null or all set (triggers enforce it, and that
the revision exists).

**Written once per accepted response.** A pin is persisted only when a response
is recorded as served (`persist_lane_pin`), never for a failed attempt, a
cancelled one or a preflight failure, so an attempt that fell back pins only the
candidate the client actually got bytes from. Ordinary lane observations
(`upsert_lane`) deliberately omit every pin column, so a response never
overwrites a routing decision made concurrently.

**Compare-and-swap.** `compare_and_swap_lane_pin` updates the pin only if the
generation and the whole previous pin (or its absence) still match what the
request saw when it started, and bumps the generation. The first of two
concurrent requests to finish pins; the second records its own row and loses the
swap (`PinPersistence::Conflict`), and is not retried or re-resolved, because
the winner's decision was made on the same evidence. A first pin needs the lane
row to exist, so the writer inserts it if absent
(`insert_lane_if_absent`) without replacing anything written concurrently.

**Activation fence.** A pin that is a first pin of a new lane, or a replace
caused by activation, is written under `PolicyRuntime::with_activation_fence`,
which runs it only while its revision is still the active one. A request that
resolved under revision 5 and finishes after revision 6 became active cannot
write revision 5 back. Separately, `decide_transition` refuses any move to a
revision older than the lane's (`StaleRevision`).

**A lane with no pin** predates policy routing (or was created by an observation
that was never a routed response). It resolves under the active revision exactly
like a new lane, whatever the scope, and is pinned by its next response. This
costs at most one cold re-read per lane that was warm at the upgrade; nothing
seeds a pin from guesswork, because the cache such a lane may hold cannot be
attributed to a binding. A proven-cold temperature is the one thing kept,
because the compaction route needs it.

The limit this leaves: a session already mid-conversation at the upgrade whose
history carries provider-owned state (signed or redacted thinking, OpenRouter
`reasoning_details`, Codex encrypted reasoning) has no pin to say which binding
produced it, so its next request is refused with a 400 ("unknown binding
affinity") on every protocol. Only the binding that made the state can replay
it, and nothing records which that was. Starting a new session clears it; a
plain conversation with no such state is unaffected.

Pins live in the lane row, so they go with it: a lane pruned at 4000 lanes or 30
days has no pin, and the next request is a new lane.

## Which revision a lane resolves under

`Server::resolve_policy_attempt` picks the revision before resolving:

| Pinned revision | Chosen revision |
| --- | --- |
| none, or the active one | active |
| newer than the active one | error (`StaleRevision`) |
| older, scope `new_lanes` | the pinned revision |
| older, scope `new_and_cold_lanes` | active if the lane is provably cold, else pinned |
| older, scope `immediate` | active; pinned again if that move would lose a warm cache and the activation did not force it |

Older revisions compile on demand against the *current* inventory and
availability, so a pinned lane keeps its policy but not stale evidence.

## One decision for every move

`decide_transition` (`policy/transition.rs`) is pure. It takes the lane state
captured when the request began (absent, or existing with generation, pin and
cache temperature), the pin the resolver prospectively produced, and a cause,
and returns one of: `FirstPin` (new lane, or a lane with no pin), `Keep`,
`Replace` (with the expected generation and pin, and a source) or `Denied` with
a reason. The caller applies it with one compare-and-swap after success
(`persist_transition`). A request that is denied is not sent: the handler fails
it, because routing it anyway would lose the cache the policy said to keep.

The cause is chosen by `policy_transition` in `server/mod.rs`, from the plan:

| Cause | When | What must hold |
| --- | --- | --- |
| compaction | the plan is a compaction under the same revision | the plan's lane transition policy, and its cache transition policy if the cache changes |
| upgrade | same revision, route, candidate, binding and baseline as the pin, but a different effective model | the plan's lane transition policy: `preserve_pinned` always denies, `move_when_cold` needs a cold lane, `allow_immediate` allows |
| fallback | the plan fell past its starting candidate, same revision | the crossed edges' strictest cache transition: `preserve_warm` denies a cache change, `allow_when_cold` needs a cold lane, `allow_cache_loss` allows |
| explicit user change | same revision, and the client chose another route or candidate, or another model where the candidate takes the client's own spelling | always allowed: the user's choice carries the consent to lose the cache it leaves |
| explicit user change, route-owned | same revision, anything else that differs (an offer selector's winner moved) | the route's own cache transition, `preserve_warm` by default |
| activation | the pin is from another revision | the scope below |

An activation under `new_lanes` keeps the pin; under `new_and_cold_lanes` keeps
it unless the lane is cold; under `immediate` replaces it, but a move that
changes the cache identity of a lane that is not cold is denied
(`WarmCacheLoss`) unless the activation was forced. Every one of those rows is
consent from the mechanism causing the move: a forced activation, the upgrade
policy, or the crossed edge. Where no consent is recorded, the answer is keep.

The upgrade policy's own sticky record (`forced_from`, `forced_to` on the lane)
is separate: it is what makes a warm lane keep resolving to the upgraded model,
and it ends when the lane is served unrewritten (the user chose a model) or goes
cold ([policy.md](policy.md)).

`TransitionSource` (new lane, activation, upgrade, fallback, explicit user
change, compaction) is a value of the decision. It decides whether the
activation fence applies; it is not stored. What the ledger keeps of a move is
the row's identities: the pin's new binding and models, the policy revision, the
attempt reason for a fallback, and `forced_from`/`forced_to` or the downgrade
pair for an upgrade or a compaction.

## Coldness evidence

Three questions get asked about "cold", and they do not share a source.

**The transition's temperature** (`cache_temperature`, `server/mod.rs`) is
`Warm`, `Cold` or `Unknown` for the lane's pinned binding, and exists to answer
"would moving this lose a live cache":

- the lane must be pinned to the binding asked about, else `Unknown`;
- the TTL is the binding's cache domain TTL when it has one; otherwise the TTL
  the lane recorded from the usage the provider reported (`cold::ttl_of`, so 5
  minutes, 10 minutes for OpenRouter Chat, otherwise the 1-hour tier);
- a lane that has recorded no TTL is `Unknown`, as is one stamped in the future;
- otherwise idle time against that TTL: `Cold` once it is reached, else `Warm`.

**Unknown is never cold.** Every consumer treats `Unknown` as warm: it denies
cache-losing moves, counts as a warm lane for activation consent, and is not
upgraded. A wrong "cold" rebuilds a whole prefix; a wrong "warm" costs one
upgrade or one move that waits for the next idle spell. A pin-only lane (a Chat
binding other than OpenRouter records a pin but no TTL) stays unknown for this
reason until its cache lifetime can be represented per binding without guessing.

**The cold gate and the compaction retarget** read the lane's own record
instead: `coldness` and `lane_is_cold` need a recorded prompt, a prompt above
`cold_min_tokens` and idle time past the TTL, where an unrecorded tier reads as
the long one ([cold-gate.md](cold-gate.md)). That is the opposite default from
the temperature above, on purpose: the notice interrupts a user, so a guess that
the cache is live costs less than one that it is gone; a transition moves a
cache, so the reverse holds. For a compaction request, `Server::lane_facts`
gives the resolver `Cold` only when that record says the lane is cold and
`Unknown` otherwise, whatever the pin and binding TTL would say.

**Activation consent** counts lanes with the temperature above, with lanes that
have no pin counted warm-or-unknown. An `immediate` activation needs the force
flag whenever any lane is warm or unknown; a ledger that cannot be read refuses
instead of reading as zero warm lanes.

## What fallback does to a pin

A fallback to a candidate on another binding is a cache-identity change. On a
warm lane it needs every crossed edge to say `allow_cache_loss`; on a cold lane
`allow_when_cold` is enough; a lane the request has never seen (no lane) may
move freely subject to the billed-boundary consent. The compiler refuses
`preserve_warm` on an edge that crosses cache identities, since such an edge
could never be taken. Once a fallback response is accepted its candidate is
pinned, so later turns stay there and do not bounce back when the earlier
candidate recovers.
