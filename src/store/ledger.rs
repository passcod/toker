//! The `requests` ledger: one row per request, insert-only (plan: Storage).
//!
//! There is no `UPDATE` on `requests` anywhere in toker and the [Store API
//! exposed for it](super::Store) has no update or delete method — the ledger
//! only grows. The column list lives once, as the nullable [RequestRow]
//! struct plus one insert function; there is deliberately no `Default` or
//! builder, so callers must spell out every field and a new column is a
//! compile error at every call site.

use serde_json::Value;

use super::{Error, Result, rows_of};
use rusqlite::Connection;

/// Proxy-written row kinds (plan: Storage). A real API measurement is **not**
/// a variant — it is `None`, which is why [RequestRow::kind] is
/// `Option<RowKind>` and why [is_api_measurement] takes the `Option`.
/// Keep the string forms in sync with the `kind` column comment in the
/// `schema` submodule's migration list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Quota gate stopped the request (synthetic 200, meter snapshot in `extra`).
    Blocked,
    /// Release-marker gate let a request through with the marker stripped.
    Released,
    /// Cold-gate notice served to the frontend.
    Cold,
    /// Cold gate active but the notice was withheld — silence ≠ breakage.
    ColdQuiet,
    /// Sleep-lock wake/sleep transition.
    Awake,
    /// Non-2xx on a usage path; `status`/`error_type`/`retry_after_ms` set, never priced.
    Error,
    /// Re-serialisation drifted from the request bytes (invariant 5); `drift_digest` set.
    FidelityDrift,
}

impl RowKind {
    /// The stored string form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Released => "released",
            Self::Cold => "cold",
            Self::ColdQuiet => "cold-quiet",
            Self::Awake => "awake",
            Self::Error => "error",
            Self::FidelityDrift => "fidelity-drift",
        }
    }

    /// Parse the stored string form; `None` for unknown values.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "blocked" => Some(Self::Blocked),
            "released" => Some(Self::Released),
            "cold" => Some(Self::Cold),
            "cold-quiet" => Some(Self::ColdQuiet),
            "awake" => Some(Self::Awake),
            "error" => Some(Self::Error),
            "fidelity-drift" => Some(Self::FidelityDrift),
            _ => None,
        }
    }
}

/// Cost in three explicit kinds, never conflated (plan: Storage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostKind {
    /// Provider-reported cost (openrouter today).
    Billed,
    /// Catalog-priced (API backends).
    Estimated,
    /// List-price on a subscription — "what is the plan worth?"
    PlanEquivalent,
}

impl CostKind {
    /// The stored string form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Billed => "billed",
            Self::Estimated => "estimated",
            Self::PlanEquivalent => "plan_equivalent",
        }
    }

    /// Parse the stored string form; `None` for unknown values.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "billed" => Some(Self::Billed),
            "estimated" => Some(Self::Estimated),
            "plan_equivalent" => Some(Self::PlanEquivalent),
            _ => None,
        }
    }
}

/// Invariant 3: proxy-written rows are never API measurements. A row is an
/// API measurement iff its kind is `None` — that is the whole rule, kept in
/// one place so every consumer (TUI, report, export) classifies identically.
pub fn is_api_measurement(kind: Option<RowKind>) -> bool {
    kind.is_none()
}

/// One `requests` row: the complete column list as a single struct. Field
/// names match their SQL columns one-to-one. Every field except `ts_ms` is
/// an `Option` — absence is stored and read back as NULL, never as zero or
/// `""` (invariant 3) — and no field carries prompt/completion/system/tool
/// text, only digests, counts, and lengths (invariant 1).
///
/// Where a kind carries a kind-specific payload, the common kinds get
/// explicit columns (gate provenance, `drift_digest`, the `error` triple)
/// so they are queryable; payloads whose shape is meter-source-dependent
/// go in the JSON `extra` column instead — e.g. a `blocked` row's meter
/// snapshot and context tokens, which mirror whatever source fed the gate.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestRow {
    /// Row id, assigned by SQLite on insert; `None` while inserting.
    pub id: Option<i64>,
    /// Epoch milliseconds; the only required column — every row has a time.
    pub ts_ms: i64,
    /// Wall-clock request duration in milliseconds.
    pub duration_ms: Option<i64>,
    /// Proxy-written kind; `None` marks a real API measurement.
    pub kind: Option<RowKind>,
    /// Frontend protocol: 'anthropic' | 'openai_chat' | 'openai_responses'.
    pub frontend: Option<String>,
    /// Backend provider id.
    pub provider: Option<String>,
    /// Route, `frontend:backend`.
    pub route: Option<String>,
    /// Frontend-provided session identity, when known.
    pub session_id: Option<String>,
    /// Ping-lane marker; ping lanes never hold the sleep lock.
    pub ping: Option<bool>,
    /// Model as reported back by the provider.
    pub model: Option<String>,
    /// Model string verbatim from the provider, pre-normalisation.
    pub raw_model: Option<String>,
    /// Model the frontend asked for, `provider/model` routing included.
    pub requested_model: Option<String>,
    /// Model that actually served the request, after middleware rewrites.
    pub effective_model: Option<String>,
    /// Input token bucket.
    pub input: Option<i64>,
    /// Cache-read token bucket.
    pub cache_read: Option<i64>,
    /// Total cache-write tokens, when only the total is known.
    pub cache_write_total: Option<i64>,
    /// 5-minute-TTL cache-write share.
    pub cache_write_5m: Option<i64>,
    /// 1-hour-TTL cache-write share.
    pub cache_write_1h: Option<i64>,
    /// Output token bucket.
    pub output: Option<i64>,
    /// Reasoning token bucket.
    pub reasoning: Option<i64>,
    /// Agentic iterations used.
    pub iterations: Option<i64>,
    /// Web search executions used.
    pub web_searches: Option<i64>,
    /// Code executions used.
    pub code_execs: Option<i64>,
    /// Whether the 5m/1h split above is a known split, not an apportioned guess.
    pub ttl_split_known: Option<bool>,
    /// Which usage metrics the provider's usage object actually contained,
    /// metric→bool; distinguishes "reported zero" from "absent".
    pub usage_presence: Option<Value>,
    /// Provider usage JSON verbatim, byte-exact — openrouter's
    /// `usage.cost`/`cost_details` live here. Deliberately a `String`, not a
    /// `Value`, so nothing ever re-serialises it (ledger parity).
    pub usage_raw: Option<String>,
    /// Cost in USD, in the kind below — one number, one meaning.
    pub cost_usd: Option<f64>,
    /// Which of the three cost semantics produced `cost_usd`.
    pub cost_kind: Option<CostKind>,
    /// Parsed meter snapshot as of the response; only kinds explicitly
    /// allowed to carry a stale copy ever set this.
    pub rate_limits: Option<Value>,
    /// Request body size in bytes.
    pub req_bytes: Option<i64>,
    /// Message count in the request.
    pub req_messages: Option<i64>,
    /// Tool definitions in the request.
    pub req_tools: Option<i64>,
    /// Digest of the tool list.
    pub tools_hash: Option<String>,
    /// System prompt length in characters (digest/length only — no content).
    pub system_chars: Option<i64>,
    /// Digest of the full system prompt.
    pub system_hash: Option<String>,
    /// Per-block system digests/lengths as JSON (no content).
    pub system_blocks: Option<Value>,
    /// Number of system messages after normalisation.
    pub system_messages: Option<i64>,
    /// Compaction generation count for the lane.
    pub compact_generations: Option<i64>,
    /// Request was a summarisation pass (compaction), 0/1.
    pub summarising: Option<bool>,
    /// System-prompt change event as JSON (digests/counts, no content).
    pub system_change: Option<Value>,
    /// System ladder summary, e.g. blocks/tools/messages tiers.
    pub system_ladder: Option<String>,
    /// Digest of the system prompt tail.
    pub system_tail: Option<String>,
    /// Quota gate was on when this row was written.
    pub gate_on: Option<bool>,
    /// Cold gate was on when this row was written.
    pub cold_on: Option<bool>,
    /// Force-newest rewrite moved the model from here.
    pub forced_from: Option<String>,
    /// …to here.
    pub forced_to: Option<String>,
    /// Safety downgrade moved the model from here.
    pub downgraded_from: Option<String>,
    /// …to here.
    pub downgraded_to: Option<String>,
    /// Cache-control markers were stripped from the request, 0/1.
    pub cache_stripped: Option<bool>,
    /// System prompts were merged for this request, 0/1.
    pub system_merged: Option<bool>,
    /// Batch model routing provenance, JSON array of from→to mappings.
    pub model_mappings: Option<Value>,
    /// Re-serialisation divergence digest (kind='fidelity-drift').
    pub drift_digest: Option<String>,
    /// HTTP status (kind='error'; also gate-relevant statuses).
    pub status: Option<i64>,
    /// Error type string (kind='error').
    pub error_type: Option<String>,
    /// Retry-after in milliseconds (kind='error').
    pub retry_after_ms: Option<i64>,
    /// Kind-specific payload as JSON, for the kinds whose data is
    /// meter-source-dependent — see the struct docs for the split.
    pub extra: Option<Value>,
    /// Beta/modifier flags from the request.
    pub betas: Option<String>,
    /// Geographic serving edge, when the provider reports one.
    pub geo: Option<String>,
    /// Provider's fast/low-latency tier flag, when reported.
    pub fast: Option<bool>,
}

/// Insert one row. The only writer to `requests`; named parameters keep the
/// SQL columns and the bound fields aligned.
pub(super) fn insert(conn: &Connection, row: &RequestRow) -> Result<()> {
    // JSON columns are serialised once up front; usage_raw is already text
    // and passes through verbatim.
    let usage_presence = super::opt_json_to_text(&row.usage_presence)?;
    let rate_limits = super::opt_json_to_text(&row.rate_limits)?;
    let system_blocks = super::opt_json_to_text(&row.system_blocks)?;
    let system_change = super::opt_json_to_text(&row.system_change)?;
    let model_mappings = super::opt_json_to_text(&row.model_mappings)?;
    let extra = super::opt_json_to_text(&row.extra)?;
    let kind = row.kind.map(RowKind::as_str);
    let cost_kind = row.cost_kind.map(CostKind::as_str);

    let mut stmt = conn.prepare(
        "INSERT INTO requests (
            ts_ms, duration_ms, kind, frontend, provider, route, session_id, ping,
            model, raw_model, requested_model, effective_model,
            input, cache_read, cache_write_total, cache_write_5m, cache_write_1h,
            output, reasoning, iterations, web_searches, code_execs, ttl_split_known,
            usage_presence, usage_raw, cost_usd, cost_kind, rate_limits,
            req_bytes, req_messages, req_tools, tools_hash,
            system_chars, system_hash, system_blocks, system_messages,
            compact_generations, summarising, system_change, system_ladder, system_tail,
            gate_on, cold_on, forced_from, forced_to, downgraded_from, downgraded_to,
            cache_stripped, system_merged, model_mappings, drift_digest,
            status, error_type, retry_after_ms, extra, betas, geo, fast
        ) VALUES (
            :ts_ms, :duration_ms, :kind, :frontend, :provider, :route, :session_id, :ping,
            :model, :raw_model, :requested_model, :effective_model,
            :input, :cache_read, :cache_write_total, :cache_write_5m, :cache_write_1h,
            :output, :reasoning, :iterations, :web_searches, :code_execs, :ttl_split_known,
            :usage_presence, :usage_raw, :cost_usd, :cost_kind, :rate_limits,
            :req_bytes, :req_messages, :req_tools, :tools_hash,
            :system_chars, :system_hash, :system_blocks, :system_messages,
            :compact_generations, :summarising, :system_change, :system_ladder, :system_tail,
            :gate_on, :cold_on, :forced_from, :forced_to, :downgraded_from, :downgraded_to,
            :cache_stripped, :system_merged, :model_mappings, :drift_digest,
            :status, :error_type, :retry_after_ms, :extra, :betas, :geo, :fast
        )",
    )?;
    stmt.execute(rusqlite::named_params! {
        ":ts_ms": row.ts_ms,
        ":duration_ms": row.duration_ms,
        ":kind": kind,
        ":frontend": row.frontend,
        ":provider": row.provider,
        ":route": row.route,
        ":session_id": row.session_id,
        ":ping": row.ping,
        ":model": row.model,
        ":raw_model": row.raw_model,
        ":requested_model": row.requested_model,
        ":effective_model": row.effective_model,
        ":input": row.input,
        ":cache_read": row.cache_read,
        ":cache_write_total": row.cache_write_total,
        ":cache_write_5m": row.cache_write_5m,
        ":cache_write_1h": row.cache_write_1h,
        ":output": row.output,
        ":reasoning": row.reasoning,
        ":iterations": row.iterations,
        ":web_searches": row.web_searches,
        ":code_execs": row.code_execs,
        ":ttl_split_known": row.ttl_split_known,
        ":usage_presence": usage_presence,
        ":usage_raw": row.usage_raw,
        ":cost_usd": row.cost_usd,
        ":cost_kind": cost_kind,
        ":rate_limits": rate_limits,
        ":req_bytes": row.req_bytes,
        ":req_messages": row.req_messages,
        ":req_tools": row.req_tools,
        ":tools_hash": row.tools_hash,
        ":system_chars": row.system_chars,
        ":system_hash": row.system_hash,
        ":system_blocks": system_blocks,
        ":system_messages": row.system_messages,
        ":compact_generations": row.compact_generations,
        ":summarising": row.summarising,
        ":system_change": system_change,
        ":system_ladder": row.system_ladder,
        ":system_tail": row.system_tail,
        ":gate_on": row.gate_on,
        ":cold_on": row.cold_on,
        ":forced_from": row.forced_from,
        ":forced_to": row.forced_to,
        ":downgraded_from": row.downgraded_from,
        ":downgraded_to": row.downgraded_to,
        ":cache_stripped": row.cache_stripped,
        ":system_merged": row.system_merged,
        ":model_mappings": model_mappings,
        ":drift_digest": row.drift_digest,
        ":status": row.status,
        ":error_type": row.error_type,
        ":retry_after_ms": row.retry_after_ms,
        ":extra": extra,
        ":betas": row.betas,
        ":geo": row.geo,
        ":fast": row.fast,
    })?;
    Ok(())
}

/// Rows with `ts_ms >= ts_ms`, oldest first. When the window holds more
/// than `limit` rows, the *newest* `limit` are kept — a caller refreshing a
/// recent window never wants its newest rows silently truncated away.
pub(super) fn requests_since(conn: &Connection, ts_ms: i64, limit: i64) -> Result<Vec<RequestRow>> {
    rows_of(
        conn,
        "SELECT * FROM (
            SELECT * FROM requests WHERE ts_ms >= ?1
            ORDER BY ts_ms DESC, id DESC LIMIT ?2
        ) ORDER BY ts_ms ASC, id ASC",
        [ts_ms, limit],
        read_row,
    )
}

/// Total row count — cheap enough for the TUI footer and `toker status`.
pub(super) fn count_requests(conn: &Connection) -> Result<i64> {
    let count = conn.query_row("SELECT COUNT(*) FROM requests", [], |row| {
        row.get::<_, i64>(0)
    })?;
    Ok(count)
}

/// Read one row by column name, so the SELECT order can never misalign a
/// field. Unknown `kind`/`cost_kind` strings are an error, not a silent
/// `None` — a corrupted enum value must never reclassify a row as an API
/// measurement (invariant 3).
fn read_row(row: &rusqlite::Row<'_>) -> Result<RequestRow> {
    Ok(RequestRow {
        id: row.get("id")?,
        ts_ms: row.get("ts_ms")?,
        duration_ms: row.get("duration_ms")?,
        kind: parse_stored(
            "kind",
            row.get::<_, Option<String>>("kind")?,
            RowKind::parse,
        )?,
        frontend: row.get("frontend")?,
        provider: row.get("provider")?,
        route: row.get("route")?,
        session_id: row.get("session_id")?,
        ping: row.get("ping")?,
        model: row.get("model")?,
        raw_model: row.get("raw_model")?,
        requested_model: row.get("requested_model")?,
        effective_model: row.get("effective_model")?,
        input: row.get("input")?,
        cache_read: row.get("cache_read")?,
        cache_write_total: row.get("cache_write_total")?,
        cache_write_5m: row.get("cache_write_5m")?,
        cache_write_1h: row.get("cache_write_1h")?,
        output: row.get("output")?,
        reasoning: row.get("reasoning")?,
        iterations: row.get("iterations")?,
        web_searches: row.get("web_searches")?,
        code_execs: row.get("code_execs")?,
        ttl_split_known: row.get("ttl_split_known")?,
        usage_presence: super::opt_json_from_text(row.get("usage_presence")?)?,
        usage_raw: row.get("usage_raw")?,
        cost_usd: row.get("cost_usd")?,
        cost_kind: parse_stored(
            "cost_kind",
            row.get::<_, Option<String>>("cost_kind")?,
            CostKind::parse,
        )?,
        rate_limits: super::opt_json_from_text(row.get("rate_limits")?)?,
        req_bytes: row.get("req_bytes")?,
        req_messages: row.get("req_messages")?,
        req_tools: row.get("req_tools")?,
        tools_hash: row.get("tools_hash")?,
        system_chars: row.get("system_chars")?,
        system_hash: row.get("system_hash")?,
        system_blocks: super::opt_json_from_text(row.get("system_blocks")?)?,
        system_messages: row.get("system_messages")?,
        compact_generations: row.get("compact_generations")?,
        summarising: row.get("summarising")?,
        system_change: super::opt_json_from_text(row.get("system_change")?)?,
        system_ladder: row.get("system_ladder")?,
        system_tail: row.get("system_tail")?,
        gate_on: row.get("gate_on")?,
        cold_on: row.get("cold_on")?,
        forced_from: row.get("forced_from")?,
        forced_to: row.get("forced_to")?,
        downgraded_from: row.get("downgraded_from")?,
        downgraded_to: row.get("downgraded_to")?,
        cache_stripped: row.get("cache_stripped")?,
        system_merged: row.get("system_merged")?,
        model_mappings: super::opt_json_from_text(row.get("model_mappings")?)?,
        drift_digest: row.get("drift_digest")?,
        status: row.get("status")?,
        error_type: row.get("error_type")?,
        retry_after_ms: row.get("retry_after_ms")?,
        extra: super::opt_json_from_text(row.get("extra")?)?,
        betas: row.get("betas")?,
        geo: row.get("geo")?,
        fast: row.get("fast")?,
    })
}

/// Parse a stored enum column, rejecting unknown values instead of silently
/// widening their meaning.
fn parse_stored<T>(
    column: &'static str,
    text: Option<String>,
    parse: fn(&str) -> Option<T>,
) -> Result<Option<T>> {
    match text {
        None => Ok(None),
        Some(text) => parse(&text).map(Some).ok_or(Error::UnknownDbValue {
            column,
            value: text,
        }),
    }
}
