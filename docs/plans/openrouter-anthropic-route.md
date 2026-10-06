# OpenRouter models in Claude Code's `/model` picker

Claude Code stays on the anthropic subscription for everything it picks by
default, and offers OpenRouter models in `/model` beside the built-in lineup,
chosen by rules matched against OpenRouter's live model listing. Picking one
sends `model: "openrouter/<id>"` on `/v1/messages`; toker strips the prefix and
forwards the request, unchanged otherwise, to OpenRouter's Anthropic-compatible
endpoint with the stored OpenRouter key in place of the client's OAuth bearer.

## Why `modelPicker`, not a custom `/v1/models`

Read out of the Claude Code 2.1.280 binary (2026-10-06):

- Gateway discovery (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`) fetches
  `<ANTHROPIC_BASE_URL>/v1/models?limit=1000` at launch, but only with a
  credential from `ANTHROPIC_AUTH_TOKEN`, an API key, or `apiKeyHelper`. The
  claude.ai OAuth login does not count, and setting any of the three replaces
  the OAuth bearer on `/v1/messages` too, which ends subscription passthrough.
  It also drops every id not matching `/(claude|anthropic)/i`.
- The subscription's own picker additions come from
  `/api/claude_cli/bootstrap`, which goes to the OAuth host, not to
  `ANTHROPIC_BASE_URL`, so toker never sees it.
- `modelPicker` in `~/.claude/settings.json` adds picker rows
  (`model`, `label`, `description`, `behavesAs`) with no credential and no id
  filter. Claude Code honours it from user, managed and `--settings` sources
  only, never from a project's settings, so the Workhorse repo-root
  settings file cannot carry it. `behavesAs` names a model this Claude Code
  knows whose client-side handling (prompt profile, effort and capability
  defaults) applies; without it a row for an unknown model is not offered.
- A row's model passes `/model` validation without a probe request, because
  it is listed.

## Steps

1. **Probe OpenRouter's Anthropic endpoint** with a real key (a cheap model,
   one short turn each) and record the answers in `routing.md`:
   - whether the `usage` object carries `cost` on a streamed and on a plain
     JSON response, and in what unit;
   - whether `/v1/messages/count_tokens` is served;
   - whether a non-Anthropic model tolerates what Claude Code sends: the
     `anthropic-beta` flags (including the OAuth one), `cache_control`,
     `thinking`, `output_config.effort`, `context_management`.
   The findings decide step 4's cost source and are the documented caveats.
   The base is `https://openrouter.ai/api`; the existing provider's
   `endpoint` already maps `/v1/messages` onto `…/api/v1/messages`.

2. **Route `openrouter/` on the Anthropic protocol.** Add the arm to
   `strip_anthropic_prefix` (`src/server/anthropic.rs`) returning
   `server.openrouter`, so a prefix naming an absent block is answered with
   the not-configured error, as for the other prefixes. The existing
   `OpenRouter` provider is reused as is: `strip_foreign_credentials` drops the
   `sk-ant-` bearer and `x-api-key`, and `inject_auth` puts the stored key in.
   Server tests against a mock upstream pin: the prefix is stripped, the path
   reaches the mock as `/v1/messages`, the OAuth bearer never reaches it, the
   stored key does, and the row records `requested_model` with the prefix,
   `effective_model` without, provider `openrouter`, route
   `anthropic:openrouter`.

3. **Keep the Anthropic-catalogue rewrites off the OpenRouter route.** The
   compaction retarget and force-newest pick targets from the Anthropic model
   catalogue, and either would move an OpenRouter conversation onto a bare
   `claude-*` id at a different provider. Gate both on the backend being
   `anthropic_sub` or `anthropic_api` (one helper, used by both), with tests
   that an `openrouter/` compaction on a cold lane and a short `openrouter/`
   conversation forward their model untouched. The quota gate is already
   sub-only. The cold gate stays on: a re-read on OpenRouter is billed, and
   `is_meter_source` is already false, so the notice says so.

4. **Record cost.** `cost_kind_of` in `record_anthropic.rs` gives `Estimated`
   to every non-sub backend, priced from the Anthropic catalogue. For
   `openrouter` use `Billed` from `usage.cost` when step 1 shows it is there,
   and no cost at all otherwise (absence, never an estimate from a catalogue
   that does not price these models). Tests for both.

5. **Picker rules.** The picker is a list of rules applied to OpenRouter's
   model listing (`GET <upstream>/models`, public, read without the key), so
   new upstream versions appear without anyone editing a list. A rule:

   ```toml
   [[providers.openrouter.picker]]
   match = ["moonshotai/kimi-k*"]        # globs over OpenRouter ids
   exclude = ["*-code", "*-thinking"]    # optional
   behaves_as = "sonnet"                 # a family, or a full Claude model id
   variant = ":floor"                    # optional, appended to the id sent
   keep = 1                              # newest N matches by `created`; default 1
   ```

   - Before any rule, the listing is cut to models whose
     `supported_parameters` include `tools` and that take text input (Claude
     Code is unusable without tools), and ids already carrying a `:variant`
     are dropped, since variants are the rule's choice.
   - A model matched by two rules gets the first one's row.
   - `behaves_as` as a family (`opus`, `sonnet`, `haiku`, `fable`) resolves at
     sync time to toker's newest known model of that family
     (`middleware/models.rs`); a full id is passed through. Claude Code needs
     an id it knows, so the docs say a toker that knows a newer model than
     the installed Claude Code should pin a full id.
   - A row is `model = "openrouter/<id><variant>"`, `label` the listing's
     `name` plus the variant, `description` `"OpenRouter · <ctx> ctx ·
     $<in>/$<out> per Mtok"` from the listing's own `context_length` and
     `pricing` (provider facts, shown as given, never toker's estimate).
   - A rule matching nothing logs a warning and offers nothing.

   Globs via `globset` (added with `cargo add`). Parsed into
   `OpenRouterConfig`, written back by `config_writer`, round-trip tested.

   **Defaults.** With no `picker` key the built-in rules apply; `picker = []`
   offers nothing; a list replaces the defaults wholesale.
   `toker picker defaults` prints the built-in rules as TOML to copy and edit.
   The built-in set, one flagship and at most one fast model per well-known
   lab (xAI deliberately left out) plus Anthropic's four families, resolved
   against the 2026-10-06 listing:

   | Match | Exclude | Behaves as | Resolves to today |
   | --- | --- | --- | --- |
   | `openai/gpt-*-sol` | `*-pro` | opus | `openai/gpt-6.1-sol` |
   | `openai/gpt-*-luna` | `*-pro` | haiku | `openai/gpt-6-luna` |
   | `google/gemini-*-flash` | | sonnet | `google/gemini-3.8-flash` |
   | `moonshotai/kimi-k*` | `*-code`, `*-thinking` | sonnet | `moonshotai/kimi-k3` |
   | `z-ai/glm-*` | `*-flash*`, `*v*`, `*-turbo`, `*-prime` | sonnet | `z-ai/glm-5.3` |
   | `z-ai/glm-*-flash` | | haiku | `z-ai/glm-5.3-flash` |
   | `deepseek/deepseek-v*-pro*` | | sonnet | `deepseek/deepseek-v4-pro-0813` |
   | `deepseek/deepseek-v*-flash*` | `*-vision*` | haiku | `deepseek/deepseek-v4.1-flash` |
   | `qwen/qwen*-max*` | `*-prime` | sonnet | `qwen/qwen3.8-max-0902` |
   | `minimax/minimax-m*` | | sonnet | `minimax/minimax-m3` |
   | `anthropic/claude-opus-*` | | opus | `anthropic/claude-opus-5.5` |
   | `anthropic/claude-sonnet-*` | | sonnet | `anthropic/claude-sonnet-5.5` |
   | `anthropic/claude-haiku-*` | | haiku | `anthropic/claude-haiku-4.5` |
   | `anthropic/claude-fable-*` | | fable | `anthropic/claude-fable-5.1` |

   No default variant. The four `anthropic/*` rules are part of the defaults
   only when neither `anthropic_sub` nor `anthropic_api` is enabled: with one
   of those, Claude Code's built-in lineup already reaches the same models,
   and a second row per model would only crowd the picker. A user's own list
   can always name `anthropic/*` (a fallback for when the subscription is
   spent, say). The class choices are a starting point; step 1's probe shows
   what each `behaves_as` makes Claude Code send, and may move rows. A trimmed
   copy of that listing is vendored as a test fixture, and an insta snapshot
   pins the rows the defaults produce from it.

6. **Sync the picker into `~/.claude/settings.json`.** The service cannot write
   there, so a user-side command does: `toker picker sync` fetches the
   listing, applies the rules, and patches `modelPicker.options`. Rows whose
   `model` starts with `openrouter/` are toker's and are replaced; every other
   row is kept in place, and `replaceBuiltInOptions` is never touched. No
   resulting rows, or no openrouter block, removes toker's rows, and a
   `modelPicker` left with no options and no other keys is removed. A present
   non-object `modelPicker` or non-array `options` is refused, as `env` is.
   A failed fetch changes nothing and exits non-zero. `--dry-run` prints the
   rows instead. Only `~/.claude/settings.json` is patched: Claude Code
   ignores `modelPicker` in a project's settings, which is what the Workhorse
   repo-root file is.
   - `toker setup` runs the sync after patching the claude frontend (after
     its existing `/f/claude` listener check).
   - Setup installs `toker-picker.timer` and `toker-picker.service` as user
     units, beside the existing hold units: a daily `OnCalendar` oneshot
     running `toker picker sync`, sandboxed `ProtectHome=read-only` with
     `ReadWritePaths=%h/.claude`. They are installed only with an openrouter
     block, and removed by setup when it goes.
   - Patcher tests pin each ownership case; a test drives the sync against a
     mock listing.

7. **Docs.** `routing.md`: OpenRouter on both protocols in the table, the
   credential swap, which rewrites the route skips and why, step 1's findings,
   and the `modelPicker` reasoning above (so nobody rebuilds `/v1/models`
   discovery). README: the default picker, how to customise the rules
   (`toker picker defaults`), what `behaves_as` does, the timer, and
   OpenRouter's own caveat that tool use is only guaranteed on Anthropic's
   models. `internals/README.md` for the new picker module.
