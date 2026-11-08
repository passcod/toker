# Toker

## Install

```console
cargo install --git https://github.com/passcod/toker toker
```

## Setup

```console
toker setup
```

Linux-only currently.

Then:

```console
toker tui
```

to see the dashboard.

## What to watch

The name of the game is to optimise for cache hit rate; how that works out depends on your frontend (claude, opencode, codex...) and your backend (anthropic, openai, openrouter...).
A "cache rebuilds" view explains how you lost your cache; you can then figure out how to optimise your tool loadout, your system prompt generation, etc.

When you leave a context-heavy session for too long and cache goes cold, Toker knows.
If you message a session with more than 175k tokens in its context, with a cache that's likely cold, and loading it into cache would make the window (if applicable) run out faster than normal, Toker first stops you; you can choose to start a new session, compact (see below), or keep going and eat the uncached writes.
If cache writes are free at the backend, this doesn't apply.

When using an Anthropic subscription (e.g. Pro, Max, Team), you also have 5-hour and weekly limits, after which you run into expensive overage.
Toker shows where you're at, and prevents you from running out, so you can max out on concurrency and the proxy will stop your sessions before they cost money... unless you provide the release token (`$#$BURN$#$`), which lets you deliberately burn overage.
When sessions have been released, they are marked with a `$` sign in the TUI.
To use up the last of a window without going into overage, provide `$#$OVER$#$` instead: the session continues until the plan quota is spent, then stops again, and is marked with a `%` sign.
A session that a notice has stopped, and that is still waiting on you, is marked with a red `!`.

When enabled, you can also let Toker wake your computer from sleep just to start the 5-hour window early, when you're still sleeping.
By default, it's set to open a window at 07:30 weekdays, which ends it at 12:30, and then re-open the next one at 12:31 (lasting until 17:31) if active sessions haven't already done it.
That means you get a "free" 15% of the morning window (assuming you start at 09:00), which is plenty to load all of yesterday's work into cache / start new work, and then the afternoon session usually doesn't need to spend that, as the caches are already hot.

Compaction management is also important.
If cache is hot, a compaction is basically free at the current model.
But if cache is cold, loading your entire context into an expensive model just to get a summary out is costly.
Toker detects that and rewrites cold-cache compactions to a lower-cost model, and also disables the cache writes where applicable.

New models come out regularly, but harnesses don't always follow suit.
Toker learns which model families you use, and transparently upgrades requests to newer versions of your models once you'vetried out the latest versions for a little while, and you can also promote a model version manually.
A `↑ ` symbol in the TUI indicates sessions that have been transparently upgraded.

When using opencode, a custom sidebar and footer plugin augments the context and spend view with accurate cost information rather than opencost's default "multiply tokens by nominal $/token" which is often wildly out of whack, and shows the provider breakdown when using a router.

## Routing

Which backend serves a model, which models appear in Claude Code's `/model` picker, and the rest of Toker's routing live in the database.
Edit them in the routing page of `toker tui`: changes go into a draft, which you validate, preview and then activate, without restarting anything.
The providers, their credentials and the gates live there too, on the providers and settings pages.

`toker setup` writes that configuration and, the first time, compiles your routing choices (and any routing fields a `toker.toml` from an older version carried: protocol defaults, model maps, picker rules) into the first active revision.
After that it never replaces the active routing.

Claude Code's `/model` picker follows the active routing.
Setup installs a daily timer that keeps the rows current, and the routing page syncs them after each activation.
Run `toker picker sync --dry-run` to see what it would offer.
Rows for OpenRouter models route that session through OpenRouter on your OpenRouter key; everything else stays on your subscription.
Only models that take tools are offered, but OpenRouter only guarantees Claude Code's tool use on Anthropic's models, so others may stumble on its more advanced requests.

## Direct OpenAI API

The setup wizard can enable `openai_api` and place its key in the OS keyring,
read it from `OPENAI_API_KEY`, or leave authentication to the frontend.
You can
also switch it on afterwards from the providers page of `toker tui`.

Make it the Chat default in the routing page: in the routes view (`2`), select
the default whose scope is `openai_chat`, press `e`, set its route to
`legacy-route-openai_api-openai_chat_completions` (Tab completes it) and save
with Ctrl+S.
Setup writes a second default for the `unprefixed` profile, which is what a
frontend without a `/f/<name>` prefix uses and which wins over the protocol
default, so change that one the same way.
If there is no default to change,
`n` adds one.
Then `V` validates and `a` activates.
Bare Chat model names then
use `https://api.openai.com/v1`; with several Chat backends enabled,
`openai_api/<model>` selects it for one request.
Toker asks
streaming completions to include terminal usage and estimates cost only when
the exact model, service tier, and every required token category are known.
Unknown pricing evidence leaves the cost blank.
`/v1/models` is authenticated
with the same key and feeds the shared model catalogue.

## Codex subscription from OpenAI Chat clients

An OpenAI Chat frontend such as opencode can use the Codex subscription
backend through the canonical mux.
Enable `codex_sub` on the providers page of
`toker tui`.

Then make it the Chat default in the routing page the same way as above, with
`legacy-route-codex_sub-codex_responses` as the route (`e` on the
`openai_chat` and `unprefixed/openai_chat` defaults, or `n` to add one).
Bare Chat model names then route to the subscription.
With several Chat
backends enabled, a single request can instead select it with a
`codex_sub/<model>` model id.
Streaming, tools, usage, errors and plain JSON
responses translate in both directions and are recorded under
`openai_chat:codex_sub`.

## TUI

```
last 60m · 10 sessions · 6 idle · 327 requests                                  last req 2s ago · 22:55:46
┌SESSIONS────────────────────────────────────────────────────────────────────────────────────────────────┐
│session                                        ctx model      reqs prompt now    peak msgs ↺    out idle│
│canopy/f4 · Investigate bestool 403 errors f…   1M opus-5-5     91    428,886 428,886  355 - 92,188  now│
│toker · Toker missing workhorse session titl…   1M opus-5-5     37    152,171 152,171   77 - 11,227  15s│
│canopy/d4 · File a check reported under an a…   1M opus-5-5     82    327,199 327,199  273 - 53,884  35s│
│bestool/q3 · Name the application on canopy …   1M opus-5-5 ↑   77    239,395 239,395  124 - 57,943   1m│
│b025d009                                      200k haiku-4-5     1     36,110  36,110    1 -  2,323   3m│
│60fd57a7                                      200k haiku-4-5     1     38,608  38,608    1 -  2,722   3m│
│89221e39                                      200k haiku-4-5     1     36,087  36,087    1 -  4,620   9m│
│canopy/c4 · Clarify check list logic on appl…   1M opus-5-5 ↑   23    158,983 158,983  144 -  8,620  10m│
│60fbfcc6                                      200k haiku-4-5     1     37,263  37,263    1 -  1,925  10m│
│canopy/e4 · Remove plain IDs from human read…   1M opus-5-5 ↑   13    349,533 349,533  564 -  7,511  48m│
│                                                                                                        │
└────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌CONTEXT─────────────────────────────────────────────────────────────────────────────────────────────────┐
│canopy/f4 · Investigate…  ████████████████░░░░░░░░░░░░░░░░░░░░░░       428,886 / 1M    43%              │
│toker · Toker missing w…  ██████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░       152,171 / 1M    15%              │
│canopy/d4 · File a chec…  ████████████░░░░░░░░░░░░░░░░░░░░░░░░░░       327,199 / 1M    33%              │
│bestool/q3 · Name the a…  █████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░       239,395 / 1M    24%              │
│canopy/c4 · Clarify che…  ██████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░       158,983 / 1M    16%  idle 10m    │
│canopy/e4 · Remove plai…  █████████████░░░░░░░░░░░░░░░░░░░░░░░░░       349,533 / 1M    35%  idle 48m    │
│                                                                                                        │
└────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌TOKENS──────────────────────────────────────────────────────────────────────────────────────────────────┐
│fresh input            1,881                                                               0%           │
│cache read        77,486,216  ▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬   99%           │
│cache write 1h     1,031,677  ▬                                                            1%           │
│cache write 5m             0                                                               0%           │
│output               242,963  743/req                                                                   │
│reasoning             74,240  227/req                                                                   │
│hit rate               98.7%  ████████████████████████████████████████████████████░ of reusable prefix  │
│missed             1,031,677  rewritten · 3,155/req · 4 of 327 req reused nothing                       │
└────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌CACHE REBUILDS──────────────────────────────────────────────────────────────────────────────────────────┐
│4 of 326 requests rewrote ≥50,000 tokens                                                                │
│tool set changed           2  ▬▬                                                                        │
│mid-history change         1  ▬                                                                         │
│new prefix / first turn    1  ▬                                                                         │
└────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌RATE & QUOTA────────────────────────────────────────────────────────────────────────────────────────────┐
│  requests ▃▃·▃▂▂██▄▂▅▄▂▁▄▄▃▇▂▂·▂▂▅▄█▅▂▃▄ 5.5/min  1 error                                              │
│  5-hour   ███████████████████░░░  87%                                       resets 00:10 · stops ~23:29│
│  7-day    █████░░░░░░░░░░░░░░░░░  23%                                        resets 12 Oct · estimating│
│  overage  ░░░░░░░░░░░░░░░░░░░░░░   0%                                           resets 1 Nov · on track│
│  spent    today <1%  ·  60m <1%                                                                        │
│  binding  five_hour                                                                                    │
└────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Click a session in SESSIONS or CONTEXT to see it in full: its id and title, the model it asked for against the one it was sent on, its context and totals, and its quota gate.
From there you can open the gate without typing a marker: `o` releases it to the end of the plan, `b` (pressed twice) releases it into overage, and `x` closes it again.
A release from the TUI applies to the current 5-hour window even before the session is stopped.
