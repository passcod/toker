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

The name of the game is to optimise for cache hit rate; how that works out depends on your frontend (claude, opencode, codex...) and your backend (anthropic, openai, openrouter...). A "cache rebuilds" view explains how you lost your cache; you can then figure out how to optimise your tool loadout, your system prompt generation, etc.

When you leave a context-heavy session for too long and cache goes cold, Toker knows. If you message a session with more than 175k tokens in its context, with a cache that's likely cold, and loading it into cache would make the window (if applicable) run out faster than normal, Toker first stops you; you can choose to start a new session, compact (see below), or keep going and eat the uncached writes. If cache writes are free at the backend, this doesn't apply.

When using an Anthropic subscription (e.g. Pro, Max, Team), you also have 5-hour and weekly limits, after which you run into expensive overage. Toker shows where you're at, and prevents you from running out, so you can max out on concurrency and the proxy will stop your sessions before they cost money... unless you provide the release token (`$#$BURN$#$`), which lets you deliberately burn overage.

When enabled, you can also let Toker wake your computer from sleep just to start the 5-hour window early, when you're still sleeping. By default, it's set to open a window at 07:30 weekdays, which ends it at 12:30, and then re-open the next one at 12:31 (lasting until 17:31) if active sessions haven't already done it. That means you get a "free" 15% of the morning window (assuming you start at 09:00), which is plenty to load all of yesterday's work into cache / start new work, and then the afternoon session usually doesn't need to spend that, as the caches are already hot.

Compaction management is also important. If cache is hot, a compaction is basically free at the current model. But if cache is cold, loading your entire context into an expensive model just to get a summary out is costly. Toker detects that and rewrites cold-cache compactions to a lower-cost model, and also disables the cache writes where applicable.

New models come out regularly, but harnesses don't always follow suit. Toker learns which model families you use, and transparently upgrades requests to newer versions of your models once you'vetried out the latest versions for a little while, and you can also promote a model version manually.

When using opencode, a custom sidebar and footer plugin augments the context and spend view with accurate cost information rather than opencost's default "multiply tokens by nominal $/token" which is often wildly out of whack, and shows the provider breakdown when using a router.
