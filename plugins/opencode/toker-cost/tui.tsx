import { Plugin } from "@opencode/plugin/tui"
import { createMemo, createSignal, For, Match, Show, Switch } from "solid-js"
import { appendFileSync } from "node:fs"
import path from "node:path"
import os from "node:os"

// toker owns attribution now. opencode sends its session id on every request
// (x-session-id), toker records it per ledger row, and this plugin asks
// toker for the per-session aggregate — no ledger tail to watch, no
// token-vector join, and no unmatched remainder: title and compaction
// requests are auxiliary LLM calls with no transcript step, but they carry
// the session header too, so they are attributed exactly like steps.

// One entry of the reply's `lanes`: a tool-set lane of this session, with a
// state toker worked out at query time. `state` is "ping" for a ping lane
// (never conversation cache state), else "warm" or "cold", and null when
// toker could not tell. Every other null is a field the lane never recorded.
type TokerLane = {
  tools_id: string | null
  state: "warm" | "cold" | "ping" | null
  ping: boolean
  updated_ms: number
  idle_ms: number | null
  prompt_tokens: number | null
  ttl_ms: number | null
  warm_remaining_ms: number | null
  noticed_ms: number | null
  forced: { from: string | null; to: string | null } | null
}

// The `/_toker/session` reply shape. Nulls are toker's absence rule — "no
// row carried the metric", never a zero — and a missing cost kind is the
// difference between a subscription the plan already covers and spend.
// `lanes` is empty when toker holds no lane for the session.
type TokerSession = {
  requests: number
  first_ts_ms: number | null
  last_ts_ms: number | null
  tokens: {
    input: number | null
    output: number | null
    reasoning: number | null
    cache_read: number | null
    cache_write_total: number | null
  }
  cost: {
    billed_total: number | null
    per_provider: Array<{ provider: string; requests: number; cost_usd: number | null }>
    per_model: Array<{ model: string; requests: number; cost_usd: number | null }>
  }
  lanes: TokerLane[]
}

const TOKER_SESSION_URL = "http://127.0.0.1:18123/_toker/session"

// toker's session aggregate, TTL-cached. The fetch is loopback but async,
// so memos read the last resolved value and a version signal re-runs every
// reading memo the moment a fetch lands. The TTL plays the role the ledger
// file's mtime used to: memos re-run on store events and may read as often
// as they like, toker is asked at most once per window per session.
const TOKER_TTL_MS = 2_000
const [tokerVersion, setTokerVersion] = createSignal(0)
let tokerCache: { session?: string; at: number; data?: TokerSession } = { at: 0 }

function tokerSession(sessionID: string): TokerSession | undefined {
  tokerVersion() // tracked: a fetch that lands re-runs every reading memo
  const now = Date.now()
  if (tokerCache.session !== sessionID) {
    tokerCache = { session: sessionID, at: now, data: undefined }
    fetchTokerSession(sessionID)
  } else if (now - tokerCache.at >= TOKER_TTL_MS) {
    tokerCache.at = now // keep the last data while the refresh is in flight
    fetchTokerSession(sessionID)
  }
  return tokerCache.session === sessionID ? tokerCache.data : undefined
}

async function fetchTokerSession(sessionID: string) {
  try {
    const response = await fetch(`${TOKER_SESSION_URL}?session=${encodeURIComponent(sessionID)}`, {
      headers: { "x-toker-control": "session" },
    })
    if (!response.ok) throw new Error(`toker answered ${response.status}`)
    const data = (await response.json()) as TokerSession
    if (tokerCache.session === sessionID) tokerCache.data = data
  } catch {
    // toker down, gated, or not yet listening: absence renders nothing,
    // never zeros — a fetch failure must not invent a free session.
    if (tokerCache.session === sessionID) tokerCache.data = undefined
  } finally {
    setTokerVersion((version) => version + 1)
  }
}

// This log records every server event type the TUI receives, first-seen, so
// the forwarding surface stays observable from outside the process.
const EVENTS_LOG = path.join(os.homedir(), ".local/share/toker/plugin-events.log")
const seenTypes = new Set()
function logEventType(type) {
  if (seenTypes.has(type)) return
  seenTypes.add(type)
  try {
    appendFileSync(EVENTS_LOG, `${Date.now()} ${type}\n`)
  } catch {}
}

const numberFormat = new Intl.NumberFormat("en-US")

// The money thresholds are the old plugin's, unchanged; a cost toker did
// not report reads as an em dash, not a fake $0.
const formatMoney = (value) =>
  typeof value !== "number" || !Number.isFinite(value)
    ? "—"
    : value >= 1
      ? `$${value.toFixed(2)}`
      : value >= 0.01
        ? `$${value.toFixed(3)}`
        : `$${value.toFixed(4)}`

// A span as one short unit pair: "45s", "4m12s", "2h05m".
function formatSpan(ms: number) {
  const seconds = Math.max(0, Math.floor(ms / 1000))
  if (seconds < 60) return `${seconds}s`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes}m${String(seconds % 60).padStart(2, "0")}s`
  return `${Math.floor(minutes / 60)}h${String(minutes % 60).padStart(2, "0")}m`
}

const formatTokens = (value: number) =>
  value >= 1_000_000 ? `${(value / 1_000_000).toFixed(1)}M` : value >= 1_000 ? `${Math.round(value / 1_000)}k` : `${value}`

// One lane as the sidebar's compact row. A ping lane is labelled as one and
// shows neither prompt size nor a warm countdown, so it cannot pass for the
// conversation's cache. Anything toker did not know is left out, not zeroed.
function laneRow(lane: TokerLane, index: number) {
  const id = lane.tools_id ?? `lane ${index + 1}`
  const idle = lane.idle_ms === null ? undefined : formatSpan(lane.idle_ms)
  const parts =
    lane.state === "ping"
      ? [`ping ${id}`, idle === undefined ? undefined : `${idle} ago`]
      : [
          id,
          lane.state ?? "unknown",
          lane.prompt_tokens === null ? undefined : formatTokens(lane.prompt_tokens),
          lane.state === "warm" && lane.warm_remaining_ms !== null
            ? `${formatSpan(lane.warm_remaining_ms)} left`
            : idle === undefined
              ? undefined
              : `idle ${idle}`,
        ]
  return parts.filter((part) => part !== undefined).join(" · ")
}

// Current context = the last assistant message with usage before the revert
// boundary; its per-category tokens are exactly what sits in context now.
function lastUsage(messages, boundary) {
  let last
  for (const message of messages) {
    if (boundary !== undefined && message.id === boundary) break
    if (message.type === "assistant" && message.tokens) last = message
  }
  return last
}

function contextTotal(tokens) {
  return tokens.input + tokens.output + tokens.reasoning + tokens.cache.read + tokens.cache.write
}

// Everything the sidebar and footer need for one session: context from
// opencode's own message data (unchanged from the ledger plugin), spend
// from toker's exact per-session ledger.
function sessionAttribution(context, sessionID) {
  const session = context.data.session.get(sessionID)
  const messages = context.data.session.message.list(sessionID)
  if (!session) return undefined

  const toker = tokerSession(sessionID)

  const last = lastUsage(messages, session.revert?.messageID)
  const models = context.data.location.model.list(session.location)
  const limit = last
    ? models?.find((model) => model.providerID === last.model?.providerID && model.id === last.model?.id)?.limit
        ?.context
    : undefined
  const tokens = last ? contextTotal(last.tokens) : 0
  const billed = toker?.cost?.billed_total
  return {
    requests: toker?.requests,
    total: typeof billed === "number" ? billed : 0,
    providers: (toker?.cost?.per_provider ?? []).map((row) => ({
      provider: row.provider,
      requests: row.requests,
      cost: row.cost_usd,
    })),
    // Absent or empty renders no block: toker down, an older toker without
    // the field, and a session it holds no lane for all read the same.
    lanes: (toker?.lanes ?? []).map((lane, index) => ({
      row: laneRow(lane, index),
      moved: lane.forced && lane.forced.from && lane.forced.to ? `${lane.forced.from} → ${lane.forced.to}` : undefined,
    })),
    context: last
      ? {
          tokens,
          percent: limit ? Math.round((tokens / limit) * 100) : undefined,
          breakdown: last.tokens,
        }
      : undefined,
  }
}

function PromptFooter(props) {
  const context = props.context
  const subagents = createMemo(() => {
    if (!props.sessionID) return 0
    return (
      context.data.session
        .family(props.sessionID)
        .filter((id) => id !== props.sessionID && context.data.session.status(id) === "running").length ?? 0
    )
  })
  const shells = createMemo(() => {
    if (!props.sessionID) return 0
    return (
      context.data.shell.list(context.location).filter((shell) => shell.metadata?.sessionID === props.sessionID)
        .length ?? 0
    )
  })
  const spend = createMemo(() => (props.sessionID ? sessionAttribution(context, props.sessionID) : undefined))
  const status = createMemo(() => {
    const info = spend()
    const parts = []
    if (info?.context && info.context.tokens > 0) {
      parts.push(
        info.context.percent === undefined
          ? `${numberFormat.format(info.context.tokens)} tokens`
          : `${numberFormat.format(info.context.tokens)} (${info.context.percent}%)`,
      )
    }
    if (info && info.total > 0) parts.push(formatMoney(info.total))
    return parts
  })
  const live = createMemo(() => Boolean(subagents() || shells()))

  return (
    <Switch>
      <Match when={props.mode === "normal"}>
        <box flexDirection="row" flexShrink={1} minWidth={0}>
          <Show when={live()}>
            <box flexShrink={0}>
              <text fg={context.theme.text.muted} wrapMode="none">
                <Show when={subagents() > 0}>
                  {subagents()} subagent{subagents() === 1 ? "" : "s"}
                </Show>
                <Show when={subagents() > 0 && shells() > 0}> · </Show>
                <Show when={shells() > 0}>
                  {shells()} shell{shells() === 1 ? "" : "s"}
                </Show>
              </text>
            </box>
          </Show>
          <Show when={props.showDetails && status().length > 0}>
            <text fg={context.theme.text.muted} wrapMode="none" flexShrink={0}>
              <Show when={live()}> · </Show>
              {status().join(" · ")}
            </text>
          </Show>
        </box>
      </Match>
      <Match when={props.mode === "shell"}>
        <text fg={context.theme.text.base} wrapMode="none" flexShrink={0}>
          esc <span style={{ fg: context.theme.text.muted }}>exit shell mode</span>
        </text>
      </Match>
    </Switch>
  )
}

function SidebarContext(props) {
  const context = props.context
  const theme = context.theme
  const info = createMemo(() => sessionAttribution(context, props.sessionID))

  return (
    <Show
      when={
        info() &&
        (info().context ||
          info().total > 0 ||
          info().providers.length > 0 ||
          info().lanes.length > 0 ||
          (info().requests ?? 0) > 0)
      }
    >
      <box>
        <text fg={theme.text.base}>
          <b>Context</b>
        </text>
        <Show when={info().context}>
          {(value) => (
            <>
              <text fg={theme.text.muted}>{numberFormat.format(value().tokens)} tokens</text>
              <Show when={value().percent !== undefined}>
                <text fg={theme.text.muted}>{value().percent}% used</text>
              </Show>
              <text fg={theme.text.muted}>input {numberFormat.format(value().breakdown.input)}</text>
              <text fg={theme.text.muted}>output {numberFormat.format(value().breakdown.output)}</text>
              <text fg={theme.text.muted}>reasoning {numberFormat.format(value().breakdown.reasoning)}</text>
              <text fg={theme.text.muted}>cache read {numberFormat.format(value().breakdown.cache.read)}</text>
              <text fg={theme.text.muted}>cache write {numberFormat.format(value().breakdown.cache.write)}</text>
            </>
          )}
        </Show>
        <Show when={info().total > 0 || info().providers.length > 0}>
          <text fg={theme.text.base}>
            <b>Spend</b>
          </text>
          <text fg={theme.text.muted}>{formatMoney(info().total)} billed</text>
          <For each={info().providers}>
            {(row) => (
              <text fg={theme.text.muted}>
                {row.provider} · {row.requests} · {formatMoney(row.cost)}
              </text>
            )}
          </For>
        </Show>
        <Show when={info().lanes.length > 0}>
          <text fg={theme.text.base}>
            <b>Lanes</b>
          </text>
          <For each={info().lanes}>
            {(lane) => (
              <>
                <text fg={theme.text.muted}>{lane.row}</text>
                <Show when={lane.moved}>
                  <text fg={theme.text.muted}>{`  ${lane.moved}`}</text>
                </Show>
              </>
            )}
          </For>
        </Show>
      </box>
    </Show>
  )
}

export default Plugin.define({
  id: "passcod.toker-cost",
  setup(context) {
    const stop = context.data.listen((event) => {
      logEventType(String(event?.details?.type))
    })
    context.ui.slot({
      append: "prompt.footer",
      render: (props) => (
        <PromptFooter
          context={context}
          sessionID={props.sessionID}
          mode={props.mode}
          showDetails={props.showDetails}
        />
      ),
    })
    context.ui.slot({
      append: "sidebar.content",
      render: (props) => <SidebarContext context={context} sessionID={props.sessionID} />,
    })
    return () => stop()
  },
})
