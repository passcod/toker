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
    /// A session's allowances were deleted from the TUI: its gate applies again.
    Revoked,
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
            Self::Revoked => "revoked",
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
            "revoked" => Some(Self::Revoked),
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

/// CLI-facing parse ([`CostKind::parse`] with the value spelled out in the
/// error, for `--cost-kind`).
impl std::str::FromStr for CostKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| {
            format!("unknown cost kind {s:?} (expected billed, estimated, or plan_equivalent)")
        })
    }
}

/// Invariant 3: proxy-written rows are never API measurements. A row is an
/// API measurement iff its kind is `None` — that is the whole rule, kept in
/// one place so every consumer (TUI, export) classifies identically.
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
    /// Where the system prompt changed against the lane's previous row, as
    /// `{delta, where}` (counts and offsets, no content), computed at
    /// capture. NULL is not "unchanged": it is also every row before
    /// capture-time localisation existed, a lane's first row, and a change
    /// whose baseline ladders could not be found.
    pub system_change: Option<Value>,
    /// Cumulative system-text prefix digests (a JSON array as text). Kept
    /// only on a lane's first row and on rows whose system prompt changed;
    /// a row whose prompt matched its lane predecessor drops it, so the
    /// latest row in the lane carrying the same `system_hash` holds it.
    /// Earlier toker rows carry it on every row.
    pub system_ladder: Option<String>,
    /// System-text suffix digests (a JSON array as text), kept on the same
    /// rows as `system_ladder`.
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
/// SQL columns and the bound fields aligned. The statement is cached on the
/// connection, so the batch path ([insert_batch]) parses it once.
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

    let mut stmt = conn.prepare_cached(
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

/// Insert many rows in one transaction — the `toker import` batch path
/// (plan: Storage). One transaction per batch keeps a 54 MB ingest from
/// holding one giant write transaction, while a failure can never expose
/// a partial batch: the transaction commits whole or not at all. Returns
/// the first and last assigned row ids (`(None, None)` for an empty
/// batch), so the importer can checkpoint the id range its rows occupy —
/// the undo path for export-style tooling; the insert-only convention
/// means the importer itself never deletes.
pub(super) fn insert_batch(
    conn: &mut Connection,
    rows: &[RequestRow],
) -> Result<(Option<i64>, Option<i64>)> {
    if rows.is_empty() {
        return Ok((None, None));
    }
    let tx = conn.transaction()?;
    let mut first_id = None;
    for row in rows {
        insert(&tx, row)?;
        first_id.get_or_insert_with(|| tx.last_insert_rowid());
    }
    let last_id = tx.last_insert_rowid();
    tx.commit()?;
    Ok((first_id, Some(last_id)))
}

/// The narrow projection of a `requests` row for the quota panel's
/// meter lookback: the four columns the aggregation consumes, nothing
/// else. It exists so that read CANNOT regress into materialising full
/// rows — the full-row reader casts 59 columns and parses six JSON
/// values per row, while the meter read parses one, once, here — so
/// adding a field to this type must be justified against the
/// per-refresh cost of fetching and parsing it across up to the quota
/// lookback's 20 000-row cap (the read runs on the TUI's quota
/// cadence). If the aggregation needs another column, widen it
/// consciously and say why.
///
/// Absence stays absence (invariant 3): the gate-flag arm of the
/// filter fetches rows that carry no snapshot at all, so `rate_limits`
/// is `None` there — never an empty object — and the scalars round-trip
/// NULL as `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct MeterRow {
    /// Epoch milliseconds — the window column, from the shared index.
    pub ts_ms: i64,
    /// Proxy-written kind; `None` marks a real API measurement.
    pub kind: Option<RowKind>,
    /// Quota-gate state when the row was written.
    pub gate_on: Option<bool>,
    /// Parsed meter snapshot as of the response; `None` on rows the
    /// gate-flag arm of the filter fetches. Parsed once here and never
    /// re-serialised.
    pub rate_limits: Option<Value>,
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

/// The meter lookback's narrow read ([`MeterRow`]s): rows within the
/// window that carry a meter snapshot OR a gate flag, oldest first, cap
/// keeping the newest exactly like [`requests_since`].
///
/// The OR is load-bearing, and the aggregation's own rules fix it:
/// a meter-bearing row needs no other column to contribute (a reading
/// is the snapshot plus `ts_ms`, and `kind` only ever decides whether a
/// *snapshot-carrying* row may baseline a span total), while the
/// gate-seen rule reads `gate_on` from ANY row in the lookback — and
/// the rows that carry the flag without a snapshot are exactly the
/// proxy-written ones (cold notices, releases). A
/// `rate_limits IS NOT NULL` filter would flip an observed gate back
/// to the assumed default. Rows with neither field contribute nothing
/// the aggregation can read, so the filter skips them and the cap
/// keeps the newest rows of what the aggregation can actually consume.
pub(super) fn meter_rows_since(conn: &Connection, ts_ms: i64, limit: i64) -> Result<Vec<MeterRow>> {
    // The inner projection carries `id` only so the outer re-sort can
    // tie-break equal timestamps the same way `requests_since` does;
    // the row reader never reads it.
    rows_of(
        conn,
        "SELECT * FROM (
            SELECT id, ts_ms, kind, gate_on, rate_limits FROM requests
            WHERE ts_ms >= ?1 AND (rate_limits IS NOT NULL OR gate_on IS NOT NULL)
            ORDER BY ts_ms DESC, id DESC LIMIT ?2
        ) ORDER BY ts_ms ASC, id ASC",
        [ts_ms, limit],
        read_meter_row,
    )
}

/// The furthest 5-hour reset, in epoch seconds, that any API-measured
/// row from `provider` logged at or after `ts_ms` reports; with
/// `ping_only`, only rows the ping header tagged. `None` when no such row
/// carries a numeric `reset5h`.
///
/// The furthest, not the newest: requests finish out of order, each
/// response carrying the figure from when it was served, so any row
/// claiming a window runs until T is proof that it does. And only
/// `kind IS NULL` rows: a blocked or released row's `rate_limits` is the
/// proxy's own last-seen copy, so reading it would report the proxy's
/// staleness as the API's (both learned in the predecessor's log).
pub(super) fn furthest_reset5h(
    conn: &Connection,
    provider: &str,
    ts_ms: i64,
    ping_only: bool,
) -> Result<Option<i64>> {
    let furthest = conn.query_row(
        "SELECT MAX(CAST(json_extract(rate_limits, '$.reset5h') AS INTEGER)) FROM requests
         WHERE ts_ms >= ?1 AND kind IS NULL AND provider = ?2 AND rate_limits IS NOT NULL
           AND json_type(rate_limits, '$.reset5h') IN ('integer', 'real')
           AND (?3 = 0 OR ping = 1)",
        (ts_ms, provider, ping_only),
        |row| row.get::<_, Option<i64>>(0),
    )?;
    Ok(furthest)
}

/// Whether any API-measured row from `provider` logged at or after
/// `ts_ms` carries the ping tag — the readback's "did the ping land at
/// all", separate from whether it carried a reading.
pub(super) fn ping_landed(conn: &Connection, provider: &str, ts_ms: i64) -> Result<bool> {
    let landed = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM requests
         WHERE ts_ms >= ?1 AND kind IS NULL AND provider = ?2 AND ping = 1)",
        (ts_ms, provider),
        |row| row.get::<_, bool>(0),
    )?;
    Ok(landed)
}

/// Read one narrow meter row by column name. Unknown `kind` values are
/// an error, not a silent `None` — the kind column drives the span
/// total's proxy-row exclusion, and a corrupted value must never
/// reclassify a row as an API measurement (invariant 3), same as the
/// full-row read.
fn read_meter_row(row: &rusqlite::Row<'_>) -> Result<MeterRow> {
    Ok(MeterRow {
        ts_ms: row.get("ts_ms")?,
        kind: parse_stored(
            "kind",
            row.get::<_, Option<String>>("kind")?,
            RowKind::parse,
        )?,
        gate_on: row.get("gate_on")?,
        rate_limits: super::opt_json_from_text(row.get("rate_limits")?)?,
    })
}

/// The narrow projection of a `requests` row for the display tick (the
/// sessions/spend/rate/context/tokens refresh): the eighteen columns the
/// dashboard's aggregation consumes, nothing else. Like [`MeterRow`], it
/// exists so that read CANNOT regress into materialising full rows —
/// the full-row reader casts 59 columns and parses six JSON values per
/// row, while the display read parses none (no JSON column is among
/// the eighteen) — so adding a field to this type must be justified
/// against the per-refresh cost of fetching it across up to the
/// display window's 10 000-row cap, re-read on every ledger change
/// (at most once a second): a field added here is a per-read cost
/// decision, made in the open.
///
/// The field set is the display aggregation's ACTUAL reads: the
/// provider·model breakdown labels itself with the backend `provider`
/// column verbatim (the serving-provider → backend → `unknown`
/// fallback chain belongs to [`session_summary`]'s SQL and the
/// `/_toker/session` endpoint, not to the display aggregation), and
/// the token buckets it sums are input, cache_read and output alone —
/// `reasoning` and `cache_write_total` are other consumers' columns.
///
/// The phase-5 growth (display parity with the reference dashboard) adds
/// six columns, each a
/// scalar fetched for exactly one panel read:
///
/// - `req_messages`, `compact_generations` — the sessions table's
///   `msgs`/`cmpct` columns, the latest row's values.
///   Two INTEGERs.
/// - `forced_to` — the `↑` marker's evidence (bright on the latest row,
///   dim when only an earlier one was rewritten). A TEXT
///   column, but written only on the rare rewrite row and read as one
///   short string per row.
/// - `cache_write_5m`, `cache_write_1h` — the tokens panel's two
///   write tiers and the hit-rate denominator's second term.
///   Two more INTEGERs. The context-occupancy sums
///   read them too: anthropic's `input_tokens` excludes cache writes,
///   so a prompt without its write share understates the context.
///   `cache_write_total` alone would not do — the tokens panel
///   renders the TTL tiers separately.
/// - `tools_hash` — the lane key's second half (`session_id` +
///   `tools_hash`), so each session's row reads its MAIN lane: the one
///   whose latest row holds the largest prompt. Without it the latest
///   row of any lane stood in for the session, and a subagent or a
///   utility call landing last moved ctx, model, prompt and idle to a
///   lane that is not the conversation — on the real log 95 of 129
///   multi-request sessions held more than one lane. A TEXT column,
///   but a short digest read as one string per row.
///
/// Absence stays absence (invariant 3): every field except `ts_ms`
/// round-trips NULL as `None`, never as zero or `""`. That includes
/// `cache_write_*` — an openai-chat backend has no cache-write metric,
/// and a NULL there is an explicit "not reported", which the
/// aggregation renders as unknown, never as a zero write.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayRow {
    /// Epoch milliseconds — the window column, from the shared index.
    pub ts_ms: i64,
    /// Proxy-written kind; `None` marks a real API measurement.
    pub kind: Option<RowKind>,
    /// Frontend-provided session identity, when known.
    pub session_id: Option<String>,
    /// The request's tool-set digest — with the session, the lane key.
    /// `None` rows share one lane, as the rebuild walk's do.
    pub tools_hash: Option<String>,
    /// Model as reported back by the provider.
    pub model: Option<String>,
    /// Backend provider id.
    pub provider: Option<String>,
    /// Input token bucket.
    pub input: Option<i64>,
    /// Cache-read token bucket.
    pub cache_read: Option<i64>,
    /// 5-minute-TTL cache-write share (the tokens panel's write tier).
    pub cache_write_5m: Option<i64>,
    /// 1-hour-TTL cache-write share (the tokens panel's write tier).
    pub cache_write_1h: Option<i64>,
    /// Output token bucket.
    pub output: Option<i64>,
    /// Reasoning token bucket — the tokens panel's reasoning row (an
    /// INTEGER; rendered when the provider reports
    /// thinking tokens).
    pub reasoning: Option<i64>,
    /// Cost in USD, in the kind below.
    pub cost_usd: Option<f64>,
    /// Which of the three cost semantics produced `cost_usd`.
    pub cost_kind: Option<CostKind>,
    /// The upstream endpoint the backend named (openrouter's
    /// `extra.serving_provider`) — the SPEND panel's per-provider
    /// label. Extracted in SQL (one json_extract per row), so the row
    /// never carries the whole `extra` object.
    pub serving_provider: Option<String>,
    /// Message count in the request (the sessions panel's `msgs`).
    pub req_messages: Option<i64>,
    /// Compaction generation count (the sessions panel's `cmpct`).
    pub compact_generations: Option<i64>,
    /// Force-newest rewrite target — the `↑` marker's evidence.
    pub forced_to: Option<String>,
}

/// The display tick's window read ([`DisplayRow`]s): every row with
/// `ts_ms >= ts_ms`, oldest first, cap keeping the newest exactly like
/// [`requests_since`].
///
/// No kind filter, deliberately — unlike [`meter_rows_since`], whose
/// aggregation can consume only snapshot-or-flag rows. The display
/// aggregation consumes every row in the window: measurements for the
/// sessions/spend/rate panels, `error` rows for the error counter and
/// their minute's sparkline flag, `fidelity-drift` rows for the drift
/// counter — and the `window_empty` verdict (true iff the window holds
/// no row of ANY kind) needs the remaining proxy kinds fetched too: a
/// window holding only a cold notice is "rows that measured nothing",
/// not "no data", and filtering those kinds would flip that verdict.
/// So the WHERE is the window alone; the win over the full-row read is
/// the projection, not the filter.
pub(super) fn display_rows_since(
    conn: &Connection,
    ts_ms: i64,
    limit: i64,
) -> Result<Vec<DisplayRow>> {
    // The inner projection carries `id` only so the outer re-sort can
    // tie-break equal timestamps the same way `requests_since` does;
    // the row reader never reads it.
    rows_of(
        conn,
        "SELECT * FROM (
            SELECT id, ts_ms, kind, session_id, tools_hash, model, provider,
                   input, cache_read, cache_write_5m, cache_write_1h, output,
                   reasoning,
                   cost_usd, cost_kind, req_messages, compact_generations, forced_to,
                   json_extract(extra, '$.serving_provider') AS serving_provider
            FROM requests
            WHERE ts_ms >= ?1
            ORDER BY ts_ms DESC, id DESC LIMIT ?2
        ) ORDER BY ts_ms ASC, id ASC",
        [ts_ms, limit],
        read_display_row,
    )
}

/// Read one narrow display row by column name. Unknown `kind`/
/// `cost_kind` strings are an error, not a silent `None` — the kind
/// column decides whether a row is an API measurement at all, and the
/// cost kind decides billed vs priced-but-not-billed, so a corrupted
/// value must never silently reclassify a row (invariant 3); same rule
/// as the full-row and meter reads.
fn read_display_row(row: &rusqlite::Row<'_>) -> Result<DisplayRow> {
    Ok(DisplayRow {
        ts_ms: row.get("ts_ms")?,
        kind: parse_stored(
            "kind",
            row.get::<_, Option<String>>("kind")?,
            RowKind::parse,
        )?,
        session_id: row.get("session_id")?,
        tools_hash: row.get("tools_hash")?,
        model: row.get("model")?,
        provider: row.get("provider")?,
        input: row.get("input")?,
        cache_read: row.get("cache_read")?,
        cache_write_5m: row.get("cache_write_5m")?,
        cache_write_1h: row.get("cache_write_1h")?,
        output: row.get("output")?,
        reasoning: row.get("reasoning")?,
        serving_provider: row.get("serving_provider")?,
        cost_usd: row.get("cost_usd")?,
        cost_kind: parse_stored(
            "cost_kind",
            row.get::<_, Option<String>>("cost_kind")?,
            CostKind::parse,
        )?,
        req_messages: row.get("req_messages")?,
        compact_generations: row.get("compact_generations")?,
        forced_to: row.get("forced_to")?,
    })
}

/// The narrow projection of a `requests` row for the cache-rebuilds
/// walk (the TUI's third narrow row, beside [`MeterRow`] and
/// [`DisplayRow`]): the classifier's actual reads, nothing else. The
/// walk runs on the TUI's QUOTA cadence (on new data at most every
/// 10 s, and every 60 s regardless), not the display read —
/// a lane walk needs the 24 h tail that provides each lane's
/// pre-window predecessor (the anti-phantom rule, as the lane
/// docs state it), and
/// that tail is an order of magnitude more rows than the display
/// window holds. Adding a field here is therefore a per-heavy-pass
/// cost decision across up to the rebuild tail's 20 000-row cap.
///
/// Field-by-field, against the classifier's reads:
///
/// - `id` — the row's identity: the lane predecessor reference for the
///   localisation pass, and the tie-break the walk's order relies on.
/// - `ts_ms` — windowing and the idle-gap measurement.
/// - `session_id`, `tools_hash` — the lane key (`session | tools_hash`;
///   `None` hashes share one lane, the shared `?` lane the predecessor's
///   walk used).
/// - `system_hash`, `system_chars`, `system_blocks` — the
///   system-prompt-change test and the "which block changed" half of
///   its localisation. `system_blocks` is the row's ONLY JSON column:
///   a handful of `{hash, chars}` objects, the lightest of the three
///   localisation columns, needed on every system-change rebuild.
///   The heavy ones — the ladders — stay behind the targeted second
///   query ([`localisation_rows`]).
/// - `req_messages` — the subagent-started collapse test
///   (at most ~8 messages after a much larger predecessor).
/// - `compact_generations` — the compaction test, a fact not an
///   inference (a compaction continues a session, so its first message
///   carries the continuation preamble).
/// - `cache_write_total` — the rewritten-token measure against the
///   panel's `REBUILD_MIN` cutoff (tui::rebuilds). The predecessor's
///   walk summed the
///   two TTL shares; the capture folds those shares
///   to this total and the import stores all three, so the one column
///   carries the same number the predecessor's walk computes.
/// - `cache_read`, `input` — the rebuild's prompt shape, for the
///   panel's detail lines.
/// - `model` — the lane's served model, for the detail lines.
///
/// `kind` is absent by construction: the read filters `kind IS NULL`,
/// because a lane's predecessor must be a request the API served — a
/// proxy-written row has no system hash, and read as a predecessor it
/// attributes the next rebuild to a changed system prompt
/// (a proxy-written row is not evidence about any prompt).
/// Absence stays absence on every other column
/// (invariant 3): a NULL `cache_write_total` is an unmeasurable
/// rewrite, counted as such, never a zero.
#[derive(Debug, Clone, PartialEq)]
pub struct RebuildRow {
    /// Row id — never NULL on a read (assigned at insert).
    pub id: i64,
    /// Epoch milliseconds — the window column, from the shared index.
    pub ts_ms: i64,
    /// Frontend-provided session identity, when known.
    pub session_id: Option<String>,
    /// Digest of the tool list — the lane key's second half.
    pub tools_hash: Option<String>,
    /// Digest of the full system prompt.
    pub system_hash: Option<String>,
    /// System prompt length in characters (digest/length only).
    pub system_chars: Option<i64>,
    /// Per-block system digests/lengths as JSON — the row's only JSON
    /// column; parsed once here, never re-serialised.
    pub system_blocks: Option<Value>,
    /// Message count in the request.
    pub req_messages: Option<i64>,
    /// Compaction generation count for the lane.
    pub compact_generations: Option<i64>,
    /// Cache-read token bucket (the rebuild's prompt shape).
    pub cache_read: Option<i64>,
    /// Total cache-write tokens — the rewritten-token measure.
    pub cache_write_total: Option<i64>,
    /// Input token bucket (the rebuild's prompt shape).
    pub input: Option<i64>,
    /// Model as reported back by the provider.
    pub model: Option<String>,
}

/// The rebuild walk's tail read ([`RebuildRow`]s): measurement rows
/// (`kind IS NULL`) with `ts_ms >= ts_ms`, oldest first, cap keeping
/// the newest exactly like [`requests_since`].
///
/// The kind filter is the only filter, and it is load-bearing (see
/// [`RebuildRow`]'s docs). The tail must reach back before the display
/// window so every lane whose predecessor predates the window gets
/// its real predecessor — the anti-phantom rule; the caller owns the
/// tail length (the TUI uses 24 h against a ≤ 24 h display window).
pub(super) fn rebuild_rows_since(
    conn: &Connection,
    ts_ms: i64,
    limit: i64,
) -> Result<Vec<RebuildRow>> {
    rows_of(
        conn,
        "SELECT * FROM (
            SELECT id, ts_ms, session_id, tools_hash, system_hash, system_chars,
                   system_blocks, req_messages, compact_generations,
                   cache_read, cache_write_total, input, model
            FROM requests
            WHERE ts_ms >= ?1 AND kind IS NULL
            ORDER BY ts_ms DESC, id DESC LIMIT ?2
        ) ORDER BY ts_ms ASC, id ASC",
        [ts_ms, limit],
        read_rebuild_row,
    )
}

/// Read one narrow rebuild row by column name. No enum columns to
/// guard (the kind filter fixed the only one); the JSON column parses
/// or errors — a corrupted `system_blocks` must fail the walk rather
/// than silently degrading every localisation.
fn read_rebuild_row(row: &rusqlite::Row<'_>) -> Result<RebuildRow> {
    Ok(RebuildRow {
        id: row.get("id")?,
        ts_ms: row.get("ts_ms")?,
        session_id: row.get("session_id")?,
        tools_hash: row.get("tools_hash")?,
        system_hash: row.get("system_hash")?,
        system_chars: row.get("system_chars")?,
        system_blocks: super::opt_json_from_text(row.get("system_blocks")?)?,
        req_messages: row.get("req_messages")?,
        compact_generations: row.get("compact_generations")?,
        cache_read: row.get("cache_read")?,
        cache_write_total: row.get("cache_write_total")?,
        input: row.get("input")?,
        model: row.get("model")?,
    })
}

/// One row of the targeted localisation fetch: the heavy text columns
/// the rebuild walk refuses to carry per row. Ladders and tails are
/// JSON arrays of digests kept at rung-per-8 KiB / rung-per-8-to-64
/// byte density — roughly 1.7 KB per row against a ~0.4 KB block map —
/// so they are fetched only for the rows a system-prompt change was
/// actually attributed to (typically none at all; the handful at most).
///
/// `system_change` rides the same query: the capture-time localisation,
/// which toker writes on a changed row and imported ctp rows carry too.
/// The walk prefers it and re-derives from the ladders only where it is
/// absent (older toker rows, and a change whose baseline capture could not
/// find).
#[derive(Debug, Clone, PartialEq)]
pub struct LocalisationRow {
    /// The row id the fetch was keyed on.
    pub id: i64,
    /// Cumulative system-text digests every 8 KiB, oldest first.
    pub system_ladder: Option<Vec<String>>,
    /// Digests of the system text's last 8…256 bytes in 8-byte steps,
    /// then 320…1024 in 64-byte steps.
    pub system_tail: Option<Vec<String>>,
    /// The capture-time localisation as JSON (`{delta, where}`), when
    /// the row carries one.
    pub system_change: Option<Value>,
}

/// Fetch the localisation columns for a specific set of row ids — the
/// rebuild panel's second, targeted query. Rows are returned in the
/// order SQLite visits them; the caller indexes by `id`. Ids that do
/// not exist (or whose columns are NULL) either read back with `None`
/// fields or are absent from the result; the caller treats both as "no
/// rungs", never as evidence of no change.
pub(super) fn localisation_rows(conn: &Connection, ids: &[i64]) -> Result<Vec<LocalisationRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // ids come from row ids the store itself assigned, so they are
    // integers by construction — interpolated as parameters, never
    // formatted into the SQL.
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id, system_ladder, system_tail, system_change
         FROM requests WHERE id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let params = rusqlite::params_from_iter(ids.iter());
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let ladder = super::opt_json_from_text(row.get::<_, Option<String>>("system_ladder")?)?;
        let tail = super::opt_json_from_text(row.get::<_, Option<String>>("system_tail")?)?;
        let change = super::opt_json_from_text(row.get::<_, Option<String>>("system_change")?)?;
        out.push(LocalisationRow {
            id: row.get("id")?,
            // A rung list that is not an array of strings is a broken
            // ladder: it localises nothing, so it reads as absent — the
            // line then claims no position rather than a wrong one
            // (invariant 3). Invalid JSON already failed the read above.
            system_ladder: string_array(ladder).ok().flatten(),
            system_tail: string_array(tail).ok().flatten(),
            system_change: change,
        });
    }
    Ok(out)
}

/// A stored rung list: JSON `null` stays `None`; a JSON array of
/// strings reads as the rungs; anything else is an error (bad shape),
/// which the callers above deliberately degrade to absence.
fn string_array(
    value: Option<Value>,
) -> std::result::Result<Option<Vec<String>>, serde_json::Error> {
    match value {
        None => Ok(None),
        Some(value) => Ok(Some(serde_json::from_value(value)?)),
    }
}

/// The newest measurement row in a lane that carries a system prompt: the
/// baseline a new request's system prompt is compared with at capture.
/// The lane is `session_id` × `tools_hash` (the lane rule; a session is
/// not a cache entry).
#[derive(Debug, Clone, PartialEq)]
pub struct LaneSystemRow {
    /// Row id.
    pub id: i64,
    /// Epoch milliseconds.
    pub ts_ms: i64,
    /// Digest of the full system prompt; never NULL here (the read
    /// filters on it).
    pub system_hash: String,
    /// System prompt length in UTF-16 units.
    pub system_chars: Option<i64>,
    /// Per-block system digests/lengths as JSON.
    pub system_blocks: Option<Value>,
}

/// The lane's newest measurement row with a system hash ([`LaneSystemRow`]).
///
/// Read from the ledger rather than the lane table: the ledger is never
/// pruned, so a lane idle past the lane table's 30 days still has its
/// baseline, and a restart loses nothing. Proxy-written rows carry no
/// system hash and are excluded by kind as well, since a proxy row is not
/// evidence about any prompt. Ordered by time, not id: imported rows were
/// inserted after toker's own first rows.
pub(super) fn lane_system_predecessor(
    conn: &Connection,
    session_id: &str,
    tools_hash: &str,
) -> Result<Option<LaneSystemRow>> {
    super::row_of(
        conn,
        "SELECT id, ts_ms, system_hash, system_chars, system_blocks
         FROM requests
         WHERE session_id = ?1 AND tools_hash = ?2
           AND kind IS NULL AND system_hash IS NOT NULL
         ORDER BY ts_ms DESC, id DESC LIMIT 1",
        rusqlite::params![session_id, tools_hash],
        |row| {
            Ok(LaneSystemRow {
                id: row.get("id")?,
                ts_ms: row.get("ts_ms")?,
                system_hash: row.get("system_hash")?,
                system_chars: row.get("system_chars")?,
                system_blocks: super::opt_json_from_text(row.get("system_blocks")?)?,
            })
        },
    )
}

/// A system prompt's stored rungs: `(ladder, tail)`, each `None` where the
/// row stored none (an empty ladder is stored as NULL).
pub type StoredLadders = (Option<Vec<String>>, Option<Vec<String>>);

/// The rungs of the newest row in a lane, at or before `(ts_ms, id)`, that
/// carried the system prompt `system_hash` and kept its ladders.
///
/// Keyed on the hash, so the rungs found are always the rungs of that
/// exact prompt: whichever row kept them, it measured the same text. Rows
/// keep ladders only where the prompt changed or the lane began, so the
/// predecessor itself usually has none and this finds the row that
/// introduced its prompt. `None` when no such row exists, which says
/// nothing about where a change was.
pub(super) fn lane_system_ladders(
    conn: &Connection,
    session_id: &str,
    tools_hash: &str,
    system_hash: &str,
    at_or_before: (i64, i64),
) -> Result<Option<StoredLadders>> {
    let (ts_ms, id) = at_or_before;
    super::row_of(
        conn,
        "SELECT system_ladder, system_tail
         FROM requests
         WHERE session_id = ?1 AND tools_hash = ?2 AND system_hash = ?3
           AND kind IS NULL
           AND (system_ladder IS NOT NULL OR system_tail IS NOT NULL)
           AND (ts_ms < ?4 OR (ts_ms = ?4 AND id <= ?5))
         ORDER BY ts_ms DESC, id DESC LIMIT 1",
        rusqlite::params![session_id, tools_hash, system_hash, ts_ms, id],
        |row| {
            let ladder = super::opt_json_from_text(row.get("system_ladder")?)?;
            let tail = super::opt_json_from_text(row.get("system_tail")?)?;
            // A rung list of the wrong shape localises nothing: absent,
            // as in `localisation_rows`.
            Ok((
                string_array(ladder).ok().flatten(),
                string_array(tail).ok().flatten(),
            ))
        },
    )
}

/// Total row count — cheap enough for the TUI footer and `toker status`.
pub(super) fn count_requests(conn: &Connection) -> Result<i64> {
    let count = conn.query_row("SELECT COUNT(*) FROM requests", [], |row| {
        row.get::<_, i64>(0)
    })?;
    Ok(count)
}

/// The newest row's timestamp, of any kind; `None` on an empty ledger.
/// `MAX` over the indexed `ts_ms` is answered from the index's last
/// entry, so this stays cheap however large the ledger grows — the TUI
/// asks it on every display read.
pub(super) fn latest_ts_ms(conn: &Connection) -> Result<Option<i64>> {
    let ts = conn.query_row("SELECT MAX(ts_ms) FROM requests", [], |row| {
        row.get::<_, Option<i64>>(0)
    })?;
    Ok(ts)
}

/// One `per_provider`/`per_model` group of a session's billed cost: the
/// group's label, how many billed rows are in it, and their summed cost.
/// A group exists because at least one billed row is in it, so `None`
/// cost means every row in the group carried a NULL cost — not "no
/// rows".
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCostGroup {
    /// The group's label: the serving provider (with the backend id as
    /// fallback) or the model.
    pub label: String,
    /// Billed rows in the group.
    pub requests: i64,
    /// Summed `cost_usd` over the group's billed rows.
    pub cost_usd: Option<f64>,
}

/// The `/_toker/session` aggregate: one session's ledger rows read in
/// one pass for the opencode sidebar/footer. Absence ≠ zero holds
/// throughout (invariant 3): every token sum is `None` when no
/// measurement row carried the metric, and the billed total is `None`
/// when no row was billed — a real zero sum reads back as `Some(0.0)`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    /// The session's API-measurement rows (kind `None`) — proxy-written
    /// kinds (a blocked gate, an error) are not requests the provider
    /// measured.
    pub requests: i64,
    /// Earliest/latest measurement timestamps; `None` when there are no
    /// rows.
    pub first_ts_ms: Option<i64>,
    pub last_ts_ms: Option<i64>,
    /// Token sums over the measurement rows; `None` when no row carried
    /// the bucket.
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub reasoning: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_write_total: Option<i64>,
    /// Sum of `cost_usd` where `cost_kind = 'billed'` — the openrouter
    /// reported cost, never the plan-equivalent or estimated kinds.
    pub billed_total: Option<f64>,
    /// The billed cost broken down by who served it — the row's
    /// `extra.serving_provider` (the upstream endpoint openrouter names)
    /// with the backend id as fallback — cost-desc. Only rows with a
    /// billed cost appear, so the breakdown partitions `billed_total`
    /// exactly; a subscription backend's plan-equivalent rows are
    /// measured (in `requests`/`tokens`) but never billed.
    pub per_provider: Vec<SessionCostGroup>,
    /// The same billed rows by `model`, cost-desc.
    pub per_model: Vec<SessionCostGroup>,
}

/// The session's totals in one row: `COUNT`/`MIN`/`MAX` and the token /
/// billed-cost sums over its measurement rows. `SUM` skips NULLs and
/// yields NULL when no row carried the metric — exactly the absence ≠
/// zero rule, so the sums need no post-processing.
const SESSION_TOTALS_SQL: &str = concat!(
    "SELECT COUNT(*) AS requests,",
    " MIN(ts_ms) AS first_ts_ms, MAX(ts_ms) AS last_ts_ms,",
    " SUM(input) AS input, SUM(output) AS output,",
    " SUM(reasoning) AS reasoning, SUM(cache_read) AS cache_read,",
    " SUM(cache_write_total) AS cache_write_total,",
    " SUM(CASE WHEN cost_kind = 'billed' THEN cost_usd END) AS billed_total",
    " FROM requests WHERE session_id = ?1 AND kind IS NULL",
);

/// The billed cost by serving provider: `extra.serving_provider` first
/// (openrouter names the upstream endpoint there), the backend id as
/// fallback, `unknown` when a row carries neither. The GROUP BY/ORDER BY
/// repeat the label expression rather than aliasing it — `provider` is
/// also a column name, and resolution by alias is a rule not worth
/// leaning on. The name ASC tiebreaker keeps equal-cost groups in a
/// deterministic order.
const SESSION_PROVIDER_SQL: &str = concat!(
    "SELECT COALESCE(json_extract(extra, '$.serving_provider'), provider, 'unknown') AS label,",
    " COUNT(*) AS requests, SUM(cost_usd) AS cost_usd",
    " FROM requests",
    " WHERE session_id = ?1 AND kind IS NULL AND cost_kind = 'billed'",
    " GROUP BY COALESCE(json_extract(extra, '$.serving_provider'), provider, 'unknown')",
    " ORDER BY SUM(cost_usd) DESC,",
    " COALESCE(json_extract(extra, '$.serving_provider'), provider, 'unknown') ASC",
);

/// The billed cost by model, same shape and ordering as the provider
/// breakdown.
const SESSION_MODEL_SQL: &str = concat!(
    "SELECT COALESCE(model, 'unknown') AS label,",
    " COUNT(*) AS requests, SUM(cost_usd) AS cost_usd",
    " FROM requests",
    " WHERE session_id = ?1 AND kind IS NULL AND cost_kind = 'billed'",
    " GROUP BY COALESCE(model, 'unknown')",
    " ORDER BY SUM(cost_usd) DESC, COALESCE(model, 'unknown') ASC",
);

/// Read one breakdown row, by column alias.
fn read_cost_group(row: &rusqlite::Row<'_>) -> Result<SessionCostGroup> {
    Ok(SessionCostGroup {
        label: row.get("label")?,
        requests: row.get("requests")?,
        cost_usd: row.get("cost_usd")?,
    })
}

/// The `/_toker/session` aggregate for one session id — three statements
/// under the caller's lock: the totals over its measurement rows, then
/// the billed-cost breakdowns. A session with no rows (or no session at
/// all — the endpoint cannot tell them apart, by design) answers
/// `requests: 0` with `None` everywhere else.
pub(super) fn session_summary(conn: &Connection, session_id: &str) -> Result<SessionSummary> {
    // COUNT(*) always yields exactly one row, even over an empty set, so
    // the only failure mode here is a genuine SQLite error.
    let (
        requests,
        first_ts_ms,
        last_ts_ms,
        input,
        output,
        reasoning,
        cache_read,
        cache_write_total,
        billed_total,
    ) = conn.query_row(SESSION_TOTALS_SQL, [session_id], |row| {
        Ok((
            row.get::<_, i64>("requests")?,
            row.get::<_, Option<i64>>("first_ts_ms")?,
            row.get::<_, Option<i64>>("last_ts_ms")?,
            row.get::<_, Option<i64>>("input")?,
            row.get::<_, Option<i64>>("output")?,
            row.get::<_, Option<i64>>("reasoning")?,
            row.get::<_, Option<i64>>("cache_read")?,
            row.get::<_, Option<i64>>("cache_write_total")?,
            row.get::<_, Option<f64>>("billed_total")?,
        ))
    })?;
    let per_provider = rows_of(conn, SESSION_PROVIDER_SQL, [session_id], read_cost_group)?;
    let per_model = rows_of(conn, SESSION_MODEL_SQL, [session_id], read_cost_group)?;
    Ok(SessionSummary {
        requests,
        first_ts_ms,
        last_ts_ms,
        input,
        output,
        reasoning,
        cache_read,
        cache_write_total,
        billed_total,
        per_provider,
        per_model,
    })
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

/// Which rows a full-ledger walk ([`for_each_export`], [`for_each_row`])
/// visits. Every bound is optional; the default visits every row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestFilter {
    /// Only rows with `ts_ms >= since_ms`.
    pub since_ms: Option<i64>,
    /// Only rows with `ts_ms < until_ms` (exclusive, so consecutive
    /// windows never share a row).
    pub until_ms: Option<i64>,
    /// Only rows whose `session_id` starts with this. A row with no
    /// session id never matches a prefix.
    pub session_prefix: Option<String>,
    /// Which row kinds.
    pub kind: KindFilter,
}

/// The kind half of a [`RequestFilter`]. Measurement and proxy rows are
/// split by the presence of a kind, never by listing kinds (the
/// [`is_api_measurement`] rule), so a kind added later lands on the
/// proxy side without anyone remembering to list it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KindFilter {
    /// Every row.
    #[default]
    All,
    /// API measurements only (`kind IS NULL`).
    Measurement,
    /// Proxy-written rows only (`kind IS NOT NULL`).
    Proxy,
    /// One proxy-written kind.
    Is(RowKind),
}

impl RequestFilter {
    /// The WHERE clause and its bound parameters. Values are always
    /// bound, never spliced, so a session prefix is data, not SQL.
    fn where_clause(&self) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value as Sql;
        let mut terms = Vec::new();
        let mut params = Vec::new();
        if let Some(since) = self.since_ms {
            params.push(Sql::Integer(since));
            terms.push(format!("ts_ms >= ?{}", params.len()));
        }
        if let Some(until) = self.until_ms {
            params.push(Sql::Integer(until));
            terms.push(format!("ts_ms < ?{}", params.len()));
        }
        if let Some(prefix) = &self.session_prefix {
            // substr rather than LIKE: a prefix holding `%` or `_` must
            // match literally, and LIKE is case-insensitive for ASCII.
            params.push(Sql::Text(prefix.clone()));
            let n = params.len();
            terms.push(format!("substr(session_id, 1, length(?{n})) = ?{n}"));
        }
        match self.kind {
            KindFilter::All => {}
            KindFilter::Measurement => terms.push("kind IS NULL".to_owned()),
            KindFilter::Proxy => terms.push("kind IS NOT NULL".to_owned()),
            KindFilter::Is(kind) => {
                params.push(Sql::Text(kind.as_str().to_owned()));
                terms.push(format!("kind = ?{}", params.len()));
            }
        }
        let clause = if terms.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", terms.join(" AND "))
        };
        (clause, params)
    }
}

/// The columns that hold JSON as text. The export nests these as JSON
/// rather than as strings holding JSON, so `jq .rate_limits.util5h`
/// works without a `fromjson`. `usage_raw` is byte-verbatim in the
/// ledger; the export re-serialises it compactly (same values, same key
/// order, possibly different whitespace), because a pretty-printed
/// provider body would otherwise break the one-row-per-line contract.
pub const JSON_TEXT_COLUMNS: &[&str] = &[
    "usage_presence",
    "usage_raw",
    "rate_limits",
    "system_blocks",
    "system_change",
    "system_ladder",
    "system_tail",
    "model_mappings",
    "extra",
    "betas",
];

/// Walk the filtered rows oldest first (`ts_ms`, then `id`), handing each
/// to `emit` as a JSON object keyed by the ledger's own column names,
/// in column order. Driven by the columns the query returns rather than
/// by [`RequestRow`], so a column added by a later migration is
/// exported the day it exists, and a stored value this binary does not
/// understand (an unknown `kind`) is shown rather than refused.
///
/// NULL is omitted: a key that is absent means the ledger did not
/// record it, and no absent value is ever written as zero or `""`.
/// Values are as stored: integers (including the 0/1 boolean columns)
/// as integers, reals as numbers, text as strings, and the
/// [`JSON_TEXT_COLUMNS`] nested when they parse (kept as a string when
/// they do not, so a malformed value is visible rather than dropped).
/// `ts`, the RFC 3339 UTC rendering of `ts_ms`, leads each object: it is
/// the one key that is not a column, there so a date can be grepped.
///
/// The rows stream from the cursor; nothing but the current row is held.
pub(super) fn for_each_export(
    conn: &Connection,
    filter: &RequestFilter,
    mut emit: impl FnMut(&serde_json::Map<String, Value>) -> std::io::Result<()>,
) -> Result<()> {
    use rusqlite::types::ValueRef;
    let (clause, params) = filter.where_clause();
    let sql = format!("SELECT * FROM requests {clause} ORDER BY ts_ms ASC, id ASC");
    let mut stmt = conn.prepare(&sql)?;
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
    while let Some(row) = rows.next()? {
        let mut object = serde_json::Map::new();
        let ts_ms: i64 = row.get("ts_ms")?;
        if let Ok(ts) = jiff::Timestamp::from_millisecond(ts_ms) {
            object.insert("ts".to_owned(), Value::String(ts.to_string()));
        }
        for (index, name) in names.iter().enumerate() {
            let value = match row.get_ref(index)? {
                ValueRef::Null => continue,
                ValueRef::Integer(n) => Value::from(n),
                // JSON has no infinity; SQLite can hold one (never NaN).
                ValueRef::Real(x) => serde_json::Number::from_f64(x)
                    .map(Value::Number)
                    .unwrap_or_else(|| Value::String(x.to_string())),
                ValueRef::Text(bytes) => {
                    let text = String::from_utf8_lossy(bytes);
                    if JSON_TEXT_COLUMNS.contains(&name.as_str()) {
                        serde_json::from_str(&text).unwrap_or(Value::String(text.into_owned()))
                    } else {
                        Value::String(text.into_owned())
                    }
                }
                // No column is a BLOB; one would be a value nothing in
                // toker wrote. Say so rather than dump its bytes.
                ValueRef::Blob(bytes) => Value::String(format!("<blob of {} bytes>", bytes.len())),
            };
            object.insert(name.clone(), value);
        }
        emit(&object)?;
    }
    Ok(())
}

/// Walk the filtered rows oldest first as typed [`RequestRow`]s, one at
/// a time from the cursor (the context-window watch's read).
pub(super) fn for_each_row(
    conn: &Connection,
    filter: &RequestFilter,
    mut visit: impl FnMut(RequestRow) -> Result<()>,
) -> Result<()> {
    let (clause, params) = filter.where_clause();
    let sql = format!("SELECT * FROM requests {clause} ORDER BY ts_ms ASC, id ASC");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
    while let Some(row) = rows.next()? {
        visit(read_row(row)?)?;
    }
    Ok(())
}
