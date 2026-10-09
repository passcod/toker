//! `toker import`: ingest the predecessor proxy's `usage.jsonl` into the
//! requests ledger (plan: Storage — "`toker import` ingests the
//! predecessor's usage.jsonl (honoring its field-era
//! notes) so learned model state and 7-day forecasting stay continuous
//! from day one").
//!
//! Streaming and durability:
//! - the source is streamed line-by-line through a `BufReader` — a 54 MB
//!   log is never slurped — and rows land one transaction per batch (a
//!   few thousand), so an interrupted import exposes at most a
//!   re-runnable prefix, never a partial batch;
//! - a malformed line never aborts the import (invariant 6's spirit, and
//!   the schema's "always handle `undefined`"): it is skipped, counted,
//!   and the count is reported;
//! - idempotence (plan: Storage): after every committed batch the
//!   importer writes a checkpoint into the meta table — source path as
//!   the key, plus size, mtime, line count, first/last line hashes, and
//!   the id range this source's rows occupy. Re-running against an
//!   unchanged file is a clean no-op ("already imported, use --force");
//!   a grown file (same first line, boundary line still where the
//!   checkpoint left it) imports only the new tail; a changed prefix
//!   refuses rather than guess, unless `--force` asks for the full
//!   re-import. The checkpoint doubles as the undo path: the id range
//!   lets `toker export`-style tooling address exactly this import's
//!   rows — the ledger is insert-only, so the importer itself never
//!   deletes.
//!
//! Field mapping: the predecessor's rows are camelCase, toker's
//! [RequestRow] is snake_case; [CtpRow] is that row schema as it is
//! really written (the report row plus the kind-row writers) and
//! [map_row] is the explicit mapping. The field-era rule — a missing
//! field means "not recorded then", never zero (the schema's own
//! contract) — is preserved throughout:
//! every field but `ts` is an `Option` that stays `None` (invariant 3).
//! A field [CtpRow] does not map is not an error — the row still
//! imports — but it is not silent either: every unmapped field name is
//! tallied by the number of imported rows that carried it, and the report
//! lists the tally (names only, never values: invariant 1), split into
//! fields the importer leaves out on purpose ([IGNORED_BY_DESIGN]) and
//! fields it has never heard of, which would mean the mapping is behind
//! the source.
//! Imported rows are written to be indistinguishable from toker-written
//! rows: kind-specific payloads ride the `extra` column in the same
//! shapes toker's own writers use, and `awake` rows keep their
//! frontend/provider/route unset like toker's.
//!
//! Cost: the predecessor priced at API list rates on a subscription, so
//! an imported
//! `costUsd` is [CostKind::PlanEquivalent] by default ("what is the plan
//! worth?"); `--cost-kind` re-labels api-era logs.

use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::store::{CostKind, RequestRow, RowKind, Store};

/// Frontend protocol every imported row was served over.
pub const FRONTEND: &str = "anthropic";

/// Provider for rows the predecessor did not name one for — its
/// row-level `provider`
/// exists for compat upstreams; every row of the real log predates it.
pub const DEFAULT_PROVIDER: &str = "anthropic_sub";

/// The predecessor priced at API list rates on a subscription — the
/// plan-equivalent
/// semantics (plan: Storage) — so that is the default cost kind for an
/// imported `costUsd`. `--cost-kind` re-labels api-era logs.
pub const DEFAULT_COST_KIND: CostKind = CostKind::PlanEquivalent;

/// Rows per insert transaction. A few thousand keeps each transaction
/// short without turning a 51 k-row ingest into thousands of them.
const BATCH_ROWS: usize = 4096;

/// The meta-table key prefix under which a source's checkpoint lives
/// (suffixed with the source's canonical path, so the checkpoint travels
/// with the file, not the path string it was first given by).
const META_KEY_PREFIX: &str = "import.ctp:";

/// Source fields the importer leaves out on purpose, with why. Each is
/// counted under "ignored by design" rather than "unknown", so the unknown
/// tally stays the signal that the mapping is behind the source.
const IGNORED_BY_DESIGN: &[(&str, &str)] = &[(
    "compacting",
    "summarising's first name (2026-09-03, eleven minutes), matched \
     anywhere in the body rather than in the last message — a different \
     test, so not mapped onto summarising",
)];

/// `toker import` options, as the CLI resolves them.
pub struct ImportOpts {
    /// The predecessor proxy's `usage.jsonl` to import.
    pub from: PathBuf,
    /// The ledger to import into.
    pub db: PathBuf,
    /// Cost semantics stamped on every imported `costUsd`.
    pub cost_kind: CostKind,
    /// Re-import the whole file even though a checkpoint says otherwise.
    /// The ledger is insert-only, so the rows of the earlier import stay:
    /// a forced re-import duplicates.
    pub force: bool,
    /// Parse and report; insert nothing (no rows, no checkpoint).
    pub dry_run: bool,
}

/// The end-of-run report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    /// Lines the source held, including malformed and already-imported
    /// ones (a tail run reads the prefix to verify it).
    pub lines_read: u64,
    /// Rows inserted (or, on a dry run, that would have been).
    pub rows_imported: u64,
    /// Lines skipped as unparseable or unmappable — never fatal.
    pub skipped_malformed: u64,
    /// Lines skipped because an earlier import already consumed them
    /// (the verified prefix of a grown, resumed, or unchanged source).
    pub skipped_duplicate: u64,
    /// The source was already imported, unchanged — nothing was inserted.
    pub already_imported: bool,
}

/// What [run_] produces for [run] to report.
#[derive(Debug)]
struct Outcome {
    summary: Summary,
    /// The first malformed line, so the report can point at one concrete
    /// reason rather than only a count.
    first_malformed: Option<(u64, String)>,
    /// Source field names [CtpRow] does not map → how many imported rows
    /// carried each. Names only; a value is never kept.
    unmapped: BTreeMap<String, u64>,
}

/// Run one import and print the report (the CLI entry point; the command
/// wiring resolves the db path in `cmds::import`).
pub fn run(opts: ImportOpts) -> Result<()> {
    let from = opts.from.display().to_string();
    let outcome = run_(&opts, BATCH_ROWS).with_context(|| format!("import {from}"))?;
    let s = &outcome.summary;
    println!("db:                   {}", opts.db.display());
    if s.already_imported {
        println!(
            "already imported: {from} (unchanged since the last import) — use --force to re-import"
        );
    }
    println!("lines read:           {}", s.lines_read);
    println!("rows imported:        {}", s.rows_imported);
    println!("skipped (malformed):  {}", s.skipped_malformed);
    println!("skipped (duplicate):  {}", s.skipped_duplicate);
    if let Some((line, reason)) = &outcome.first_malformed {
        println!("first malformed line: {line}: {reason}");
    }
    let (by_design, unknown): (Vec<_>, Vec<_>) = outcome
        .unmapped
        .iter()
        .partition(|(name, _)| ignored_by_design(name).is_some());
    for (name, rows) in by_design {
        let why = ignored_by_design(name).unwrap_or_default();
        println!("unmapped (by design): {name} on {} — {why}", rows_of(*rows));
    }
    for (name, rows) in unknown {
        println!("unmapped (unknown):   {name} on {}", rows_of(*rows));
    }
    if opts.dry_run {
        println!("(dry run — nothing inserted)");
    }
    Ok(())
}

/// The import checkpoint for one source, stored in the meta table under
/// [META_KEY_PREFIX] + canonical path. Written after every committed
/// batch, so an interrupted import resumes from the last committed line
/// instead of duplicating its committed prefix. `first_id`/`last_id` span
/// every row this source ever imported across runs (except after a
/// `--force` re-import, which resets the range to the forced run's rows —
/// the duplicated earlier rows are outside it by construction).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Checkpoint {
    size: u64,
    mtime_ms: i64,
    line_count: u64,
    first_line_hash: Option<String>,
    last_line_hash: Option<String>,
    first_id: Option<i64>,
    last_id: Option<i64>,
}

/// The per-run state a batch flush needs: the pending rows, the counters
/// the report wants, the accumulating id range, and the checkpoint point
/// the flush will write.
struct RunState {
    batch: Vec<RequestRow>,
    rows_imported: u64,
    first_id: Option<i64>,
    last_id: Option<i64>,
    point: Checkpoint,
}

/// Stream the source, map every new line, and insert the mapped rows one
/// batch per transaction, checkpointing after each commit.
fn run_(opts: &ImportOpts, batch_rows: usize) -> Result<Outcome> {
    let batch_rows = batch_rows.max(1);
    let file = File::open(&opts.from).with_context(|| format!("open {}", opts.from.display()))?;
    let metadata = file.metadata()?;
    let key = meta_key(&opts.from)?;
    let store = Store::open(&opts.db).with_context(|| format!("open db {}", opts.db.display()))?;

    // The previous import's checkpoint for this source, if any. `--force`
    // ignores it and re-imports everything (duplicating: insert-only).
    let checkpoint = if opts.force {
        None
    } else {
        read_checkpoint(&store, &key)
    };
    let tail = checkpoint.is_some();
    let boundary = checkpoint.as_ref().map_or(0, |c| c.line_count);
    let cp_first = checkpoint.as_ref().and_then(|c| c.first_line_hash.clone());
    let cp_last = checkpoint.as_ref().and_then(|c| c.last_line_hash.clone());

    let mut state = RunState {
        batch: Vec::with_capacity(batch_rows),
        rows_imported: 0,
        first_id: checkpoint.as_ref().and_then(|c| c.first_id),
        last_id: checkpoint.as_ref().and_then(|c| c.last_id),
        point: Checkpoint {
            size: metadata.len(),
            mtime_ms: mtime_ms(&metadata),
            line_count: 0,
            first_line_hash: None,
            last_line_hash: None,
            first_id: None,
            last_id: None,
        },
    };
    let mut skipped_malformed = 0u64;
    let mut skipped_duplicate = 0u64;
    let mut first_malformed: Option<(u64, String)> = None;
    let mut unmapped = BTreeMap::<String, u64>::new();

    let mut reader = BufReader::new(file);
    let mut raw = String::new();
    let mut line_no = 0u64;
    loop {
        raw.clear();
        if reader.read_line(&mut raw)? == 0 {
            break;
        }
        line_no += 1;
        // The terminator is transport, not content; everything else —
        // including an empty line — is the line as the predecessor
        // appended it.
        let line = raw
            .strip_suffix('\n')
            .map_or(raw.as_str(), |l| l.strip_suffix('\r').unwrap_or(l));
        let hash = line_hash(line);
        state.point.line_count = line_no;
        state.point.last_line_hash = Some(hash.clone());
        if line_no == 1 {
            state.point.first_line_hash = Some(hash.clone());
            if tail && boundary >= 1 && state.point.first_line_hash != cp_first {
                bail!(
                    "the source changed since the last import (the first line \
                     differs) — use --force for a full re-import"
                );
            }
        }
        if tail && line_no == boundary && state.point.last_line_hash != cp_last {
            bail!(
                "the source changed since the last import (line {boundary} no \
                     longer matches the checkpoint) — use --force for a full re-import"
            );
        }
        if tail && line_no <= boundary {
            // The verified prefix an earlier run already consumed.
            skipped_duplicate += 1;
            continue;
        }
        match serde_json::from_str::<CtpRow>(line) {
            Ok(ctp) => match map_row(&ctp, opts.cost_kind) {
                Ok(row) => {
                    for name in ctp.unmapped.keys() {
                        *unmapped.entry(name.clone()).or_default() += 1;
                    }
                    state.batch.push(row);
                }
                Err(reason) => {
                    skipped_malformed += 1;
                    first_malformed.get_or_insert((line_no, reason));
                }
            },
            Err(reason) => {
                skipped_malformed += 1;
                first_malformed.get_or_insert((line_no, format!("json: {reason}")));
            }
        }
        if state.batch.len() >= batch_rows {
            commit_batch(&store, &key, &mut state, opts.dry_run)?;
        }
    }

    if tail && line_no < boundary {
        bail!(
            "the source shrank since the last import (the checkpoint covers \
             {boundary} lines, the file holds {line_no}) — use --force for a \
             full re-import"
        );
    }
    // Flush the remainder; a no-op when the batch is empty (an unchanged
    // source, or a tail whose new lines were all malformed — in which case
    // no checkpoint moves, and the malformed tail is simply re-read next
    // time).
    commit_batch(&store, &key, &mut state, opts.dry_run)?;

    Ok(Outcome {
        summary: Summary {
            lines_read: line_no,
            rows_imported: state.rows_imported,
            skipped_malformed,
            skipped_duplicate,
            already_imported: tail && line_no == boundary,
        },
        first_malformed,
        unmapped,
    })
}

/// "1 row", "26 rows".
fn rows_of(n: u64) -> String {
    if n == 1 {
        "1 row".to_owned()
    } else {
        format!("{n} rows")
    }
}

/// Why `name` is left out on purpose, when it is.
fn ignored_by_design(name: &str) -> Option<&'static str> {
    IGNORED_BY_DESIGN
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, why)| *why)
}

/// Flush one batch: rows are inserted in a single transaction (never a
/// partial batch on disk), a checkpoint is written per commit so an
/// interrupted run resumes from exactly here, and the row count advances
/// whether or not a dry run is skipping the writes.
fn commit_batch(store: &Store, key: &str, state: &mut RunState, dry_run: bool) -> Result<()> {
    if state.batch.is_empty() {
        return Ok(());
    }
    state.rows_imported += state.batch.len() as u64;
    if !dry_run {
        let (first_id, last_id) = store.record_requests(&state.batch).context(
            "insert batch failed — this batch rolled back; committed \
                      batches are checkpointed, so re-running resumes",
        )?;
        state.first_id = state.first_id.or(first_id);
        state.last_id = last_id;
        state.point.first_id = state.first_id;
        state.point.last_id = state.last_id;
        store.set_meta(key, &serde_json::to_string(&state.point)?)?;
    }
    state.batch.clear();
    Ok(())
}

/// The meta key for this source's checkpoint: the canonical path, so the
/// same file reached by a different path string still finds its own
/// checkpoint.
fn meta_key(from: &Path) -> Result<String> {
    let canonical =
        std::fs::canonicalize(from).with_context(|| format!("resolve {}", from.display()))?;
    Ok(format!("{META_KEY_PREFIX}{}", canonical.display()))
}

/// Read (and warn-and-ignore if unreadable) the checkpoint for this source.
fn read_checkpoint(store: &Store, key: &str) -> Option<Checkpoint> {
    let value = store.get_meta(key).ok().flatten()?;
    match serde_json::from_str(&value) {
        Ok(checkpoint) => Some(checkpoint),
        Err(_) => {
            eprintln!(
                "warning: unreadable import checkpoint for this source — \
                 importing from scratch"
            );
            None
        }
    }
}

/// Sha256 of one line, hex — the checkpoint's content fingerprints.
fn line_hash(line: &str) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(line.as_bytes()) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The source's mtime in epoch ms (0 when unavailable — informational
/// checkpoint data; the content hashes are the verification).
fn mtime_ms(metadata: &Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One imported `usage.jsonl` row, exactly as the predecessor's row
/// writers produce it (the measurement row plus the kind-row writers).
/// Every field
/// but `ts` is optional: fields were added over time and a row that
/// predates one simply lacks it — absence means "not recorded then",
/// never zero (the schema's own contract), and it stays `None`
/// (invariant 3).
///
/// JSON `null` and an absent field both deserialize to `None` here,
/// which is the right collapse everywhere the source format writes null
/// as "no value"
/// (`rateLimits`, `costUsd`, `geo`, `until`, …) — and where the two would
/// mean different things (`betas`: null means "not captured", absence
/// means "before the field existed"), both still mean "no captured
/// betas", so the distinction the format preserves is against the
/// *empty list*,
/// and that survives: `Some([])` is a captured-empty header, `None` is
/// not captured.
///
/// Mapping to [RequestRow] (camelCase → snake_case unless noted):
///
/// | imported field | RequestRow field | note |
/// | --- | --- | --- |
/// | `ts` | `ts_ms` | ISO 8601 string → epoch ms (jiff) |
/// | `durationMs` | `duration_ms` | |
/// | `kind` | `kind` | unknown kind ⇒ malformed line, never a guess |
/// | `model` / `rawModel` | `model` / `raw_model` | |
/// | `fast` / `geo` | `fast` / `geo` | |
/// | `input` | `input` | |
/// | `cacheRead` | `cache_read` | |
/// | `cacheCreateTotal` | `cache_write_total` | |
/// | `write5m` / `write1h` | `cache_write_5m` / `cache_write_1h` | |
/// | `output` | `output` | |
/// | `thinking` | `reasoning` | renamed bucket |
/// | `webSearches` / `codeExecs` / `iterations` | `web_searches` / `code_execs` / `iterations` | |
/// | `ttlSplitKnown` | `ttl_split_known` | |
/// | `costUsd` | `cost_usd` + `cost_kind` | kind set only when the cost is |
/// | `rateLimits` | `rate_limits` | ported faithfully per kind — the kind rule is already encoded in what was written |
/// | `gateOn` / `coldOn` | `gate_on` / `cold_on` | |
/// | `forcedFrom`/`forcedTo` | `forced_from`/`forced_to` | |
/// | `downgradedFrom`/`downgradedTo` | `downgraded_from`/`downgraded_to` | |
/// | `cacheStripped` | `cache_stripped` | the source's count collapses to whether (the count rides the source log; toker's column is boolean) |
/// | `systemMerged` | `system_merged` | ditto |
/// | `requestedModel`/`effectiveModel` | `requested_model`/`effective_model` | |
/// | `modelMappings` | `model_mappings` | JSON verbatim |
/// | `sessionId` | `session_id` | |
/// | `ping` | `ping` | written only when true; absence stays `None` |
/// | `betas` | `betas` | JSON array as text, toker's own storage form |
/// | `reqBytes`/`reqMessages`/`reqTools` | `req_bytes`/`req_messages`/`req_tools` | |
/// | `toolsHash` | `tools_hash` | |
/// | `systemChars`/`systemHash` | `system_chars`/`system_hash` | |
/// | `systemBlocks` | `system_blocks` | JSON verbatim |
/// | `systemMessages` | `system_messages` | |
/// | `compactGenerations` | `compact_generations` | |
/// | `summarising` | `summarising` | |
/// | `systemChange` | `system_change` | JSON verbatim |
/// | `systemLadder`/`systemTail` | `system_ladder`/`system_tail` | JSON array as text, toker's ladder column form |
/// | `usagePresence` | `usage_presence` | JSON verbatim |
/// | `provider` | `provider` | absent ⇒ [`DEFAULT_PROVIDER`] |
/// | (derived) | `frontend`, `route` | [`FRONTEND`] / `"{FRONTEND}:{provider}"` |
/// | (kind payloads) | `extra` | see [map_row] |
///
/// Fields with no imported source stay `None`: `usage_raw` (imported rows
/// carry
/// folded buckets, not the response's raw usage JSON) and `drift_digest`
/// (a toker-only kind).
///
/// Any other source field lands in `unmapped`, so the importer can count
/// it instead of dropping it unseen (the struct has no
/// `deny_unknown_fields`: an unmapped field never fails a row).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CtpRow {
    ts: String,
    duration_ms: Option<i64>,
    kind: Option<String>,
    model: Option<String>,
    raw_model: Option<String>,
    fast: Option<bool>,
    geo: Option<String>,
    input: Option<i64>,
    cache_read: Option<i64>,
    cache_create_total: Option<i64>,
    write_5m: Option<i64>,
    write_1h: Option<i64>,
    output: Option<i64>,
    thinking: Option<i64>,
    web_searches: Option<i64>,
    code_execs: Option<i64>,
    iterations: Option<i64>,
    ttl_split_known: Option<bool>,
    cost_usd: Option<f64>,
    rate_limits: Option<Value>,
    gate_on: Option<bool>,
    cold_on: Option<bool>,
    forced_from: Option<String>,
    forced_to: Option<String>,
    downgraded_from: Option<String>,
    downgraded_to: Option<String>,
    /// The source format records counts (breakpoints dropped / prompts
    /// merged); toker's
    /// columns record whether.
    cache_stripped: Option<Value>,
    system_merged: Option<Value>,
    requested_model: Option<String>,
    effective_model: Option<String>,
    model_mappings: Option<Value>,
    session_id: Option<String>,
    ping: Option<bool>,
    betas: Option<Vec<String>>,
    req_bytes: Option<i64>,
    req_messages: Option<i64>,
    req_tools: Option<i64>,
    tools_hash: Option<String>,
    system_chars: Option<i64>,
    system_hash: Option<String>,
    system_blocks: Option<Value>,
    system_messages: Option<i64>,
    compact_generations: Option<i64>,
    summarising: Option<bool>,
    system_change: Option<Value>,
    system_ladder: Option<Vec<String>>,
    system_tail: Option<Vec<String>>,
    usage_presence: Option<Value>,
    /// Row-level provider for compat upstreams; every pre-field row was
    /// the anthropic subscription.
    provider: Option<String>,

    // kind: "blocked" — `resetsAt` is epoch
    // seconds; `contextTokens` null means the lane
    // table had forgotten the session, not "empty conversation".
    meter: Option<String>,
    resets_at: Option<i64>,
    context_tokens: Option<i64>,
    // kind: "released" — reset values, epoch seconds.
    five_hour: Option<i64>,
    seven_day: Option<i64>,
    // kind: "cold" / "cold-quiet".
    idle_ms: Option<i64>,
    last_prompt: Option<i64>,
    compact_target: Option<String>,
    quota_extra: Option<f64>,
    quota_bound: Option<bool>,
    quota_meter: Option<String>,
    util_5h: Option<f64>,
    // kind: "awake" — `until` is an ISO string.
    held: Option<bool>,
    want: Option<bool>,
    until: Option<String>,
    reason: Option<String>,
    // kind: "error" — `retryAfter` is the response
    // header verbatim (seconds as a string).
    status: Option<i64>,
    error_type: Option<String>,
    error_message: Option<String>,
    retry_after: Option<Value>,

    /// Every field not named above, by name. [IgnoredAny] skips the value
    /// without building it: the importer counts names and never keeps a
    /// value (invariant 1).
    #[serde(flatten)]
    unmapped: BTreeMap<String, IgnoredAny>,
}

/// Map one parsed imported row onto the ledger row. Errors are per-line
/// reasons — the caller skips and counts the line, never aborts the
/// import (invariant 6's spirit). An unknown `kind` is an error, not a
/// default: `kind IS NULL` is what marks a real API measurement, so an
/// unmappable row must never silently become one (invariant 3).
fn map_row(row: &CtpRow, cost_kind: CostKind) -> Result<RequestRow, String> {
    let ts_ms = parse_ts_ms(&row.ts)?;
    let kind = match row.kind.as_deref() {
        None => None,
        Some(k) => Some(RowKind::parse(k).ok_or_else(|| format!("unknown kind {k:?}"))?),
    };

    // The source format names a provider only on compat-upstream rows;
    // every other row
    // was the anthropic subscription. Awake rows carry neither route
    // nor provider in toker's own writer — the lock is not a route — so
    // imported ones match.
    let (frontend, provider, route) = if kind == Some(RowKind::Awake) {
        (None, None, None)
    } else {
        let provider = row
            .provider
            .clone()
            .unwrap_or_else(|| DEFAULT_PROVIDER.to_owned());
        let route = format!("{FRONTEND}:{provider}");
        (Some(FRONTEND.to_owned()), Some(provider), Some(route))
    };

    let stamped_cost_kind = row.cost_usd.map(|_| cost_kind);
    let retry_after_ms = row
        .retry_after
        .as_ref()
        .and_then(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        })
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        .map(|secs| (secs * 1000.0) as i64);

    // Kind-specific payloads, in the shapes toker's own writers use, so
    // an imported row is indistinguishable from a toker-written one:
    // - blocked: the gate's own fields, snake_case like toker's writer;
    //   its stale rateLimits ride the `rate_limits` column, as the kind
    //   explicitly allowed to carry a stale copy;
    // - released: reset values under the source format's own names, like
    //   toker's writer;
    // - cold / cold-quiet: the source format's camelCase names, like
    //   toker's writer;
    // - awake: `until` converted to epoch ms, toker's ts_ms convention;
    // - error: the message half of the error pair in `extra` (there is no
    //   error_message column), plus a retry-after that would not parse as
    //   seconds, kept raw rather than dropped.
    let extra = match kind {
        Some(RowKind::Blocked) => Some(json!({
            "meter": row.meter,
            "resets_at": row.resets_at,
            "context_tokens": row.context_tokens,
        })),
        Some(RowKind::Released) => Some(json!({
            "fiveHour": row.five_hour,
            "sevenDay": row.seven_day,
        })),
        Some(RowKind::Cold) => Some(json!({
            "idleMs": row.idle_ms,
            "lastPrompt": row.last_prompt,
            "reqMessages": row.req_messages,
            "compactTarget": row.compact_target,
            "quotaExtra": row.quota_extra,
            "quotaBound": row.quota_bound,
            "quotaMeter": row.quota_meter,
            "util5h": row.util_5h,
        })),
        Some(RowKind::ColdQuiet) => Some(json!({
            "idleMs": row.idle_ms,
            "lastPrompt": row.last_prompt,
            "quotaExtra": row.quota_extra,
            "quotaBound": row.quota_bound,
            "util5h": row.util_5h,
        })),
        Some(RowKind::Awake) => {
            let until_ms = match &row.until {
                None => None,
                Some(iso) => Some(parse_ts_ms(iso)?),
            };
            Some(json!({
                "held": row.held,
                "want": row.want,
                "until": until_ms,
                "reason": row.reason,
            }))
        }
        Some(RowKind::Error) => {
            let mut extra = serde_json::Map::new();
            if let Some(message) = &row.error_message {
                extra.insert("error_message".into(), json!(message));
            }
            if let Some(raw) = &row.retry_after
                && !raw.is_null()
                && retry_after_ms.is_none()
            {
                extra.insert("retry_after_raw".into(), raw.clone());
            }
            (!extra.is_empty()).then(|| Value::Object(extra))
        }
        _ => None,
    };

    Ok(RequestRow {
        id: None,
        ts_ms,
        duration_ms: row.duration_ms,
        kind,
        frontend,
        provider,
        route,
        session_id: row.session_id.clone(),
        ping: row.ping,
        model: row.model.clone(),
        raw_model: row.raw_model.clone(),
        requested_model: row.requested_model.clone(),
        effective_model: row.effective_model.clone(),
        input: row.input,
        cache_read: row.cache_read,
        cache_write_total: row.cache_create_total,
        cache_write_5m: row.write_5m,
        cache_write_1h: row.write_1h,
        output: row.output,
        reasoning: row.thinking,
        iterations: row.iterations,
        web_searches: row.web_searches,
        code_execs: row.code_execs,
        ttl_split_known: row.ttl_split_known,
        usage_presence: row.usage_presence.clone(),
        // Imported rows carry folded buckets, not the response's raw usage
        // JSON — nothing to store verbatim.
        usage_raw: None,
        cost_usd: row.cost_usd,
        cost_kind: stamped_cost_kind,
        rate_limits: row.rate_limits.clone(),
        req_bytes: row.req_bytes,
        req_messages: row.req_messages,
        req_tools: row.req_tools,
        tools_hash: row.tools_hash.clone(),
        system_chars: row.system_chars,
        system_hash: row.system_hash.clone(),
        system_blocks: row.system_blocks.clone(),
        system_messages: row.system_messages,
        compact_generations: row.compact_generations,
        summarising: row.summarising,
        system_change: row.system_change.clone(),
        system_ladder: ladder_text(&row.system_ladder),
        system_tail: ladder_text(&row.system_tail),
        gate_on: row.gate_on,
        cold_on: row.cold_on,
        forced_from: row.forced_from.clone(),
        forced_to: row.forced_to.clone(),
        downgraded_from: row.downgraded_from.clone(),
        downgraded_to: row.downgraded_to.clone(),
        cache_stripped: count_or_bool("cacheStripped", &row.cache_stripped)?,
        system_merged: count_or_bool("systemMerged", &row.system_merged)?,
        model_mappings: row.model_mappings.clone(),
        drift_digest: None,
        status: row.status,
        error_type: row.error_type.clone(),
        retry_after_ms,
        extra,
        betas: row.betas.as_ref().map(|betas| json!(betas).to_string()),
        geo: row.geo.clone(),
        fast: row.fast,
    })
}

/// An ISO 8601 timestamp (the row's `ts`, and `awake`'s `until`) as epoch ms.
/// Unparseable means the row's spine is broken: the caller skips the
/// line, it never guesses a time (invariant 3).
fn parse_ts_ms(ts: &str) -> Result<i64, String> {
    jiff::Timestamp::from_str(ts)
        .map(|t| t.as_millisecond())
        .map_err(|error| format!("unparseable ts {ts:?}: {error}"))
}

/// A ladder column, in toker's own storage form: a JSON array of digests
/// as text, `None` for an empty ladder — an empty ladder localises
/// nothing, and absence stays absence (invariant 3).
fn ladder_text(rungs: &Option<Vec<String>>) -> Option<String> {
    match rungs {
        Some(rungs) if !rungs.is_empty() => Some(json!(rungs).to_string()),
        _ => None,
    }
}

/// The source format writes `cacheStripped`/`systemMerged` as counts,
/// toker's columns as
/// booleans: n ≥ 1 happened. A non-number, non-bool value is a schema
/// surprise the caller must not paper over.
fn count_or_bool(field: &'static str, value: &Option<Value>) -> Result<Option<bool>, String> {
    match value {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::Number(n)) => Ok(Some(n.as_i64().unwrap_or(0) != 0)),
        Some(other) => Err(format!("{field}: expected bool or count, got {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// The checkpoint a source's import has written, parsed.
    fn checkpoint_of(store: &Store, from: &Path) -> Checkpoint {
        let key = meta_key(from).expect("key");
        let value = store
            .get_meta(&key)
            .expect("meta read")
            .expect("checkpoint written");
        serde_json::from_str(&value).expect("parse checkpoint")
    }

    /// A fresh scratch directory under /tmp/opencode, unique per call so
    /// parallel tests never collide.
    fn test_dir(name: &str) -> crate::test_support::TestDir {
        crate::test_support::tempdir(&format!("import-{name}-"))
    }

    /// Write a JSONL fixture (each slice one line, trailing newline).
    fn write_jsonl(path: &Path, lines: &[&str]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create dir");
        }
        std::fs::write(path, lines.join("\n") + "\n").expect("write fixture");
    }

    fn opts(from: PathBuf, db: PathBuf) -> ImportOpts {
        ImportOpts {
            from,
            db,
            cost_kind: CostKind::PlanEquivalent,
            force: false,
            dry_run: false,
        }
    }

    /// Map one fixture row, which must use only mapped fields: the
    /// fixtures span every era and kind, so a mapped field the flattened
    /// catch-all swallowed would show up here.
    fn map(line: &str) -> RequestRow {
        let ctp: CtpRow = serde_json::from_str(line).expect("parse fixture row");
        assert!(
            ctp.unmapped.is_empty(),
            "unmapped: {:?}",
            ctp.unmapped.keys()
        );
        map_row(&ctp, CostKind::PlanEquivalent).expect("map fixture row")
    }

    fn ts_ms(iso: &str) -> i64 {
        jiff::Timestamp::from_str(iso)
            .expect("parse iso")
            .as_millisecond()
    }

    // ── the golden mapping ──────────────────────────────────────────────

    /// A full-era measurement row exercising every mapped field once.
    const GOLDEN: &str = r#"{"ts":"2026-09-26T16:49:22.799Z","durationMs":4392,"model":"claude-opus-5","rawModel":"claude-opus-5-20261001","fast":true,"geo":"us","input":673,"cacheRead":66944,"write5m":10,"write1h":20,"cacheCreateTotal":30,"ttlSplitKnown":true,"thinking":12,"webSearches":2,"codeExecs":1,"iterations":3,"costUsd":0.039712,"rateLimits":{"util5h":0.42,"reset5h":1790000000},"gateOn":true,"coldOn":false,"forcedFrom":"claude-sonnet-4-5","forcedTo":"claude-opus-5","downgradedFrom":"claude-opus-5-200k","downgradedTo":"claude-opus-5","cacheStripped":2,"systemMerged":true,"requestedModel":"claude-sonnet-4-5","effectiveModel":"claude-opus-5","modelMappings":[{"requestIndex":0,"requestedModel":"a","effectiveModel":"b"}],"sessionId":"58cb4d0d-9d45-472f-9874-efd5cff579c0","ping":true,"betas":["claude-code-20250219","context-1m-2025-08-07"],"reqBytes":362736,"reqMessages":67,"reqTools":39,"toolsHash":"27023a35ec81","systemChars":2873,"systemHash":"b8060eba6beb","systemBlocks":[{"chars":91,"hash":"d8ea107d4a57"}],"systemMessages":22,"compactGenerations":1,"summarising":true,"systemChange":{"delta":-2,"where":"block 0 (91 → 90 chars)"},"systemLadder":["r1","r2"],"systemTail":["t1","t2"],"usagePresence":{"input":true,"output":false},"provider":"compat-upstream"}"#;

    #[test]
    fn golden_full_era_row_maps_field_by_field() {
        let row = map(GOLDEN);
        assert_eq!(
            row.ts_ms,
            ts_ms("2026-09-26T16:49:22.799Z"),
            "ISO → epoch ms"
        );
        assert_eq!(row.duration_ms, Some(4392));
        assert_eq!(row.kind, None, "no kind = a real API measurement");
        assert_eq!(row.frontend.as_deref(), Some("anthropic"));
        assert_eq!(row.provider.as_deref(), Some("compat-upstream"));
        assert_eq!(row.route.as_deref(), Some("anthropic:compat-upstream"));
        assert_eq!(
            row.session_id.as_deref(),
            Some("58cb4d0d-9d45-472f-9874-efd5cff579c0")
        );
        assert_eq!(row.ping, Some(true));
        assert_eq!(row.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(row.raw_model.as_deref(), Some("claude-opus-5-20261001"));
        assert_eq!(row.requested_model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(row.effective_model.as_deref(), Some("claude-opus-5"));
        assert_eq!(row.input, Some(673));
        assert_eq!(row.cache_read, Some(66944));
        assert_eq!(row.cache_write_total, Some(30), "cacheCreateTotal");
        assert_eq!(row.cache_write_5m, Some(10), "write5m");
        assert_eq!(row.cache_write_1h, Some(20), "write1h");
        assert_eq!(row.output, None, "absent output stays None");
        assert_eq!(row.reasoning, Some(12), "thinking → reasoning");
        assert_eq!(row.web_searches, Some(2));
        assert_eq!(row.code_execs, Some(1));
        assert_eq!(row.iterations, Some(3));
        assert_eq!(row.ttl_split_known, Some(true));
        assert_eq!(
            row.usage_presence,
            Some(json!({"input": true, "output": false}))
        );
        assert_eq!(
            row.usage_raw, None,
            "imported rows carry folded buckets, no raw usage"
        );
        assert_eq!(row.cost_usd, Some(0.039712));
        assert_eq!(row.cost_kind, Some(CostKind::PlanEquivalent));
        assert_eq!(
            row.rate_limits,
            Some(json!({"util5h": 0.42, "reset5h": 1790000000}))
        );
        assert_eq!(row.req_bytes, Some(362736));
        assert_eq!(row.req_messages, Some(67));
        assert_eq!(row.req_tools, Some(39));
        assert_eq!(row.tools_hash.as_deref(), Some("27023a35ec81"));
        assert_eq!(row.system_chars, Some(2873));
        assert_eq!(row.system_hash.as_deref(), Some("b8060eba6beb"));
        assert_eq!(
            row.system_blocks,
            Some(json!([{"chars": 91, "hash": "d8ea107d4a57"}]))
        );
        assert_eq!(row.system_messages, Some(22));
        assert_eq!(row.compact_generations, Some(1));
        assert_eq!(row.summarising, Some(true));
        assert_eq!(
            row.system_change,
            Some(json!({"delta": -2, "where": "block 0 (91 → 90 chars)"}))
        );
        assert_eq!(row.system_ladder.as_deref(), Some(r#"["r1","r2"]"#));
        assert_eq!(row.system_tail.as_deref(), Some(r#"["t1","t2"]"#));
        assert_eq!(row.gate_on, Some(true));
        assert_eq!(row.cold_on, Some(false));
        assert_eq!(row.forced_from.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(row.forced_to.as_deref(), Some("claude-opus-5"));
        assert_eq!(row.downgraded_from.as_deref(), Some("claude-opus-5-200k"));
        assert_eq!(row.downgraded_to.as_deref(), Some("claude-opus-5"));
        assert_eq!(
            row.cache_stripped,
            Some(true),
            "the source count collapses to whether"
        );
        assert_eq!(row.system_merged, Some(true));
        assert_eq!(
            row.model_mappings,
            Some(json!([{"requestIndex": 0, "requestedModel": "a", "effectiveModel": "b"}]))
        );
        assert_eq!(row.drift_digest, None);
        assert_eq!(row.status, None);
        assert_eq!(row.error_type, None);
        assert_eq!(row.retry_after_ms, None);
        assert_eq!(row.extra, None, "measurement rows carry no kind payload");
        assert_eq!(
            row.betas.as_deref(),
            Some(r#"["claude-code-20250219","context-1m-2025-08-07"]"#),
            "betas: captured array as JSON text"
        );
        assert_eq!(row.geo.as_deref(), Some("us"));
        assert_eq!(row.fast, Some(true));
        assert_eq!(row.id, None);
    }

    #[test]
    fn absent_fields_stay_none_and_null_is_absence() {
        // An old-era row: only the fields that existed then. Everything
        // later stays None — "not recorded then", never zero.
        let row = map(
            r#"{"ts":"2026-09-26T16:49:18.414Z","durationMs":100,"model":"claude-opus-5","rawModel":"claude-opus-5","input":5,"cacheRead":10,"write5m":0,"write1h":0,"cacheCreateTotal":0,"output":1,"costUsd":0.01}"#,
        );
        assert_eq!(row.kind, None);
        assert_eq!(
            row.provider.as_deref(),
            Some(DEFAULT_PROVIDER),
            "absent → default provider"
        );
        assert_eq!(row.route.as_deref(), Some("anthropic:anthropic_sub"));
        assert_eq!(row.fast, None);
        assert_eq!(row.geo, None);
        assert_eq!(row.reasoning, None, "absent thinking stays None");
        assert_eq!(row.web_searches, None);
        assert_eq!(row.ttl_split_known, None);
        assert_eq!(row.gate_on, None);
        assert_eq!(row.cold_on, None);
        assert_eq!(row.betas, None, "betas absent = not captured, never none");
        assert_eq!(row.ping, None);
        assert_eq!(row.session_id, None);
        assert_eq!(row.rate_limits, None);
        assert_eq!(row.req_bytes, None);
        assert_eq!(row.system_blocks, None);
        assert_eq!(row.usage_presence, None);
        assert_eq!(row.extra, None);

        // Explicit nulls mean "no value" in the source format, and map to
        // absence.
        let row = map(
            r#"{"ts":"2026-09-26T16:49:18.414Z","geo":null,"costUsd":null,"rateLimits":null,"sessionId":null,"betas":null,"cacheStripped":null}"#,
        );
        assert_eq!(row.geo, None);
        assert_eq!(row.cost_usd, None);
        assert_eq!(row.cost_kind, None, "unpriced rows claim no cost semantics");
        assert_eq!(row.rate_limits, None);
        assert_eq!(row.session_id, None);
        assert_eq!(row.betas, None, "betas null = not captured");
        assert_eq!(row.cache_stripped, None);

        // A captured empty betas list is a real value, distinct from both.
        let row = map(r#"{"ts":"2026-09-26T16:49:18.414Z","betas":[]}"#);
        assert_eq!(row.betas.as_deref(), Some("[]"));
    }

    #[test]
    fn cost_kind_flag_relabels_imported_costs() {
        for (kind, expect) in [
            (CostKind::PlanEquivalent, CostKind::PlanEquivalent),
            (CostKind::Estimated, CostKind::Estimated),
            (CostKind::Billed, CostKind::Billed),
        ] {
            let ctp: CtpRow =
                serde_json::from_str(r#"{"ts":"2026-09-26T16:49:18.414Z","costUsd":0.5}"#)
                    .expect("parse");
            let row = map_row(&ctp, kind).expect("map");
            assert_eq!(row.cost_usd, Some(0.5));
            assert_eq!(row.cost_kind, Some(expect));
        }
    }

    #[test]
    fn every_kind_row_maps_its_payload() {
        // blocked: the gate's own fields into extra (snake_case, toker's
        // writer's shape), the stale rateLimits into rate_limits.
        let row = map(
            r#"{"kind":"blocked","ts":"2026-09-27T01:02:03.000Z","durationMs":12,"meter":"5h","resetsAt":1790000000,"sessionId":"ses-1","contextTokens":null,"rateLimits":{"util5h":1.0},"gateOn":true}"#,
        );
        assert_eq!(row.kind, Some(RowKind::Blocked));
        assert_eq!(row.duration_ms, Some(12));
        assert_eq!(row.session_id.as_deref(), Some("ses-1"));
        assert_eq!(row.gate_on, Some(true));
        assert_eq!(
            row.rate_limits,
            Some(json!({"util5h": 1.0})),
            "the stale copy ports"
        );
        assert_eq!(
            row.extra,
            Some(json!({"meter": "5h", "resets_at": 1790000000, "context_tokens": null})),
            "null contextTokens = the lane table forgot, kept as null"
        );

        // released: reset values under the source format's own names.
        let row = map(
            r#"{"kind":"released","ts":"2026-09-27T01:02:03.000Z","sessionId":"ses-1","fiveHour":1790000600,"sevenDay":null,"gateOn":true,"rateLimits":null}"#,
        );
        assert_eq!(row.kind, Some(RowKind::Released));
        assert_eq!(row.rate_limits, None, "null rateLimits is absence");
        assert_eq!(
            row.extra,
            Some(json!({"fiveHour": 1790000600, "sevenDay": null}))
        );

        // cold: the whole notice payload; reqMessages rides both the
        // column and extra, like toker's writer.
        let row = map(
            r#"{"kind":"cold","ts":"2026-09-27T01:02:03.000Z","durationMs":3,"sessionId":"ses-1","toolsHash":"abc123","idleMs":3600000,"lastPrompt":50000,"reqMessages":12,"gateOn":true,"coldOn":true,"compactTarget":"claude-haiku-4-5","quotaExtra":0.06,"quotaBound":true,"quotaMeter":"5h","util5h":null}"#,
        );
        assert_eq!(row.kind, Some(RowKind::Cold));
        assert_eq!(row.tools_hash.as_deref(), Some("abc123"));
        assert_eq!(row.req_messages, Some(12));
        assert_eq!(row.rate_limits, None, "cold rows carry no rateLimits");
        assert_eq!(
            row.extra,
            Some(json!({
                "idleMs": 3600000, "lastPrompt": 50000, "reqMessages": 12,
                "compactTarget": "claude-haiku-4-5", "quotaExtra": 0.06,
                "quotaBound": true, "quotaMeter": "5h", "util5h": null,
            }))
        );

        // cold-quiet: no reqMessages/compactTarget/quotaMeter, util5h on.
        let row = map(
            r#"{"kind":"cold-quiet","ts":"2026-09-27T01:02:03.000Z","durationMs":3,"sessionId":"ses-1","toolsHash":"abc","idleMs":120000,"lastPrompt":40,"quotaExtra":0.05,"quotaBound":null,"util5h":0.3,"gateOn":true,"coldOn":true}"#,
        );
        assert_eq!(row.kind, Some(RowKind::ColdQuiet));
        assert_eq!(
            row.extra,
            Some(json!({
                "idleMs": 120000, "lastPrompt": 40,
                "quotaExtra": 0.05, "quotaBound": null, "util5h": 0.3,
            }))
        );
        assert!(row.extra.as_ref().unwrap().get("reqMessages").is_none());

        // awake: no route at all (the lock is not a route), until → ms.
        let row = map(
            r#"{"kind":"awake","ts":"2026-09-27T01:02:03.000Z","held":true,"want":true,"until":"2026-09-27T09:53:01.870Z","reason":"1 in flight"}"#,
        );
        assert_eq!(row.kind, Some(RowKind::Awake));
        assert_eq!(row.frontend, None);
        assert_eq!(row.provider, None);
        assert_eq!(row.route, None);
        assert_eq!(
            row.extra,
            Some(json!({
                "held": true, "want": true,
                "until": ts_ms("2026-09-27T09:53:01.870Z"),
                "reason": "1 in flight",
            }))
        );
        // and an awake row without an expiry holds without one.
        let row = map(
            r#"{"kind":"awake","ts":"2026-09-27T01:02:03.000Z","held":false,"want":true,"until":null,"reason":"no live lanes"}"#,
        );
        assert_eq!(
            row.extra,
            Some(json!({"held": false, "want": true, "until": null, "reason": "no live lanes"}))
        );

        // error: the status/type/retry-after columns, the message half and
        // an unparseable retry-after in extra, routing provenance kept.
        let row = map(
            r#"{"kind":"error","ts":"2026-09-27T08:57:34.340Z","durationMs":242181,"status":503,"errorType":"api_error","errorMessage":"Codex completed without producing output","retryAfter":"60","rateLimits":null,"gateOn":true,"sessionId":"ses-2","requestedModel":"claude-opus-5","effectiveModel":"gpt-5.6-sol"}"#,
        );
        assert_eq!(row.kind, Some(RowKind::Error));
        assert_eq!(row.status, Some(503));
        assert_eq!(row.error_type.as_deref(), Some("api_error"));
        assert_eq!(row.retry_after_ms, Some(60_000), "header seconds → ms");
        assert_eq!(row.session_id.as_deref(), Some("ses-2"));
        assert_eq!(row.requested_model.as_deref(), Some("claude-opus-5"));
        assert_eq!(row.effective_model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(row.rate_limits, None);
        assert_eq!(
            row.extra,
            Some(json!({"error_message": "Codex completed without producing output"}))
        );

        // A numeric retry-after parses too; a non-second one is kept raw
        // rather than dropped; null is absence.
        let row =
            map(r#"{"kind":"error","ts":"2026-09-27T08:57:34.340Z","status":429,"retryAfter":30}"#);
        assert_eq!(row.retry_after_ms, Some(30_000));
        let row = map(
            r#"{"kind":"error","ts":"2026-09-27T08:57:34.340Z","status":429,"retryAfter":"next tuesday"}"#,
        );
        assert_eq!(row.retry_after_ms, None);
        assert_eq!(row.extra, Some(json!({"retry_after_raw": "next tuesday"})));
        let row = map(
            r#"{"kind":"error","ts":"2026-09-27T08:57:34.340Z","status":500,"retryAfter":null}"#,
        );
        assert_eq!(row.retry_after_ms, None);
        assert_eq!(row.extra, None);
    }

    #[test]
    fn unmappable_rows_error_never_default_to_measurements() {
        // Unknown kind: a row toker cannot classify must not become an
        // API measurement by accident (invariant 3).
        let ctp: CtpRow =
            serde_json::from_str(r#"{"ts":"2026-09-26T16:49:18.414Z","kind":"mystery"}"#)
                .expect("parse");
        let err = map_row(&ctp, CostKind::PlanEquivalent).expect_err("unknown kind");
        assert!(err.contains("mystery"), "{err}");

        // Unparseable ts: the row's spine is broken.
        let ctp: CtpRow = serde_json::from_str(r#"{"ts":"not a timestamp"}"#).expect("parse");
        assert!(map_row(&ctp, CostKind::PlanEquivalent).is_err());

        // A wrong-typed field is a schema surprise, not a null: the
        // deserializer rejects the line, so the importer counts it as
        // malformed rather than papering over the value.
        assert!(
            serde_json::from_str::<CtpRow>(r#"{"ts":"2026-09-26T16:49:18.414Z","input":"lots"}"#)
                .is_err()
        );
    }

    // ── end-to-end over a scratch db ───────────────────────────────────

    fn measurement(ts: i64) -> String {
        format!(
            r#"{{"ts":"2026-09-26T16:49:22.{ts:03}Z","durationMs":100,"model":"claude-opus-5","rawModel":"claude-opus-5","input":5,"cacheRead":10,"write5m":0,"write1h":0,"cacheCreateTotal":0,"output":1,"costUsd":0.01,"sessionId":"ses-1","betas":["context-1m-2025-08-07"],"gateOn":true,"coldOn":true}}"#
        )
    }

    #[test]
    fn import_reports_counts_and_writes_the_checkpoint() {
        let dir = test_dir("reports");
        let from = dir.join("usage.jsonl");
        write_jsonl(
            &from,
            &[
                &measurement(1),
                "not json at all",
                &measurement(2),
                &measurement(3),
            ],
        );

        let db = dir.join("toker.db");
        let outcome = run_(&opts(from.clone(), db.clone()), 2).expect("import");
        assert_eq!(
            outcome.summary,
            Summary {
                lines_read: 4,
                rows_imported: 3,
                skipped_malformed: 1,
                skipped_duplicate: 0,
                already_imported: false,
            }
        );
        assert_eq!(outcome.first_malformed.map(|(line, _)| line), Some(2));

        let store = Store::open(&db).expect("reopen");
        assert_eq!(store.count_requests().expect("count"), 3);
        // The checkpoint records the full file and the undo id range.
        let checkpoint = checkpoint_of(&store, &from);
        assert_eq!(checkpoint.line_count, 4);
        assert_eq!(checkpoint.first_id, Some(1));
        assert_eq!(checkpoint.last_id, Some(3));
        assert_eq!(
            checkpoint.size,
            std::fs::metadata(&from).expect("meta").len()
        );
        assert!(checkpoint.first_line_hash.is_some());
        assert!(checkpoint.last_line_hash.is_some());
    }

    #[test]
    fn unmapped_fields_are_counted_by_name_and_never_fail_a_row() {
        let dir = test_dir("unmapped");
        let from = dir.join("usage.jsonl");
        // The early `compacting` rows of the real log, a field no era of
        // the source wrote (a mapping gap), and an unknown-kind row whose
        // unmapped field must not count: the row did not import.
        let compacting = measurement(1).replace("{\"ts\"", "{\"compacting\":true,\"ts\"");
        let unknown = measurement(2).replace(
            "\"gateOn\"",
            "\"frobnicate\":{\"secret\":\"do not echo\"},\"compacting\":true,\"gateOn\"",
        );
        let bad_kind =
            measurement(3).replace("{\"ts\"", "{\"kind\":\"mystery\",\"frobnicate\":1,\"ts\"");
        write_jsonl(&from, &[&compacting, &unknown, &bad_kind, &measurement(4)]);

        let db = dir.join("toker.db");
        let outcome = run_(&opts(from, db), 8).expect("import");
        assert_eq!(
            outcome.summary.rows_imported, 3,
            "unmapped never fails a row"
        );
        assert_eq!(outcome.summary.skipped_malformed, 1);
        assert_eq!(
            outcome.unmapped,
            BTreeMap::from([("compacting".to_owned(), 2), ("frobnicate".to_owned(), 1)]),
            "names tallied per imported row, values never kept"
        );
        assert!(ignored_by_design("compacting").is_some());
        assert!(ignored_by_design("frobnicate").is_none());
        // The mapped half is untouched by the extra field.
        let ctp: CtpRow = serde_json::from_str(&compacting).expect("parse");
        let row = map_row(&ctp, CostKind::PlanEquivalent).expect("map");
        assert_eq!(row, map(&measurement(1)));
    }

    #[test]
    fn mapped_fields_never_count_as_unmapped() {
        let dir = test_dir("mapped");
        let from = dir.join("usage.jsonl");
        write_jsonl(&from, &[&measurement(1), &measurement(2)]);
        let outcome = run_(&opts(from, dir.join("toker.db")), 8).expect("import");
        assert!(outcome.unmapped.is_empty(), "{:?}", outcome.unmapped);
    }

    #[test]
    fn same_file_twice_is_a_clean_noop() {
        let dir = test_dir("noop");
        let from = dir.join("usage.jsonl");
        write_jsonl(&from, &[&measurement(1), &measurement(2)]);
        let db = dir.join("toker.db");
        run_(&opts(from.clone(), db.clone()), 2).expect("first import");

        let outcome = run_(&opts(from, db.clone()), 2).expect("second import");
        assert!(outcome.summary.already_imported);
        assert_eq!(outcome.summary.rows_imported, 0);
        assert_eq!(outcome.summary.skipped_duplicate, 2);
        assert_eq!(outcome.summary.lines_read, 2);
        assert_eq!(
            Store::open(&db)
                .expect("reopen")
                .count_requests()
                .expect("count"),
            2
        );
    }

    #[test]
    fn grown_file_imports_only_the_tail() {
        let dir = test_dir("grown");
        let from = dir.join("usage.jsonl");
        write_jsonl(&from, &[&measurement(1), &measurement(2)]);
        let db = dir.join("toker.db");
        run_(&opts(from.clone(), db.clone()), 8).expect("first import");
        let first_ids: Vec<i64> = Store::open(&db)
            .expect("reopen")
            .requests_since(0, 10)
            .expect("rows")
            .into_iter()
            .map(|row| row.id.expect("id"))
            .collect();

        // The file grows (append-only, as the source format promises:
        // "There is no rotation"): only the new tail imports, and the
        // prefix counts as
        // skipped-duplicate, verified by hash, not by trust.
        write_jsonl(
            &from,
            &[
                &measurement(1),
                &measurement(2),
                &measurement(3),
                &measurement(4),
            ],
        );
        let outcome = run_(&opts(from.clone(), db.clone()), 8).expect("tail import");
        assert_eq!(outcome.summary.rows_imported, 2);
        assert_eq!(outcome.summary.skipped_duplicate, 2);
        assert_eq!(outcome.summary.lines_read, 4);
        assert!(!outcome.summary.already_imported);

        let store = Store::open(&db).expect("reopen");
        assert_eq!(store.count_requests().expect("count"), 4);
        let new_ids: Vec<i64> = store
            .requests_since(0, 10)
            .expect("rows")
            .into_iter()
            .map(|row| row.id.expect("id"))
            .collect();
        assert_eq!(new_ids.len(), 4);
        assert!(
            first_ids.iter().all(|id| new_ids.contains(id)),
            "old rows stay"
        );
        let checkpoint = checkpoint_of(&store, &from);
        assert_eq!(checkpoint.line_count, 4);
        assert_eq!(
            checkpoint.first_id,
            Some(1),
            "the undo range spans every run"
        );
        assert_eq!(checkpoint.last_id, Some(4));
    }

    #[test]
    fn changed_prefix_refuses_without_force() {
        let dir = test_dir("refuse-first");
        let from = dir.join("usage.jsonl");
        write_jsonl(&from, &[&measurement(1), &measurement(2)]);
        let db = dir.join("toker.db");
        run_(&opts(from.clone(), db.clone()), 8).expect("first import");

        // A different first line: the prefix changed, and guessing where
        // the new content starts would duplicate or drop rows silently.
        write_jsonl(&from, &[&measurement(9), &measurement(2), &measurement(3)]);
        let err = run_(&opts(from.clone(), db.clone()), 8).expect_err("refuse");
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(
            Store::open(&db)
                .expect("reopen")
                .count_requests()
                .expect("count"),
            2,
            "a refused import inserts nothing"
        );

        // A mid-prefix edit that moves the boundary line: refused too.
        write_jsonl(&from, &[&measurement(1), &measurement(8), &measurement(3)]);
        let err = run_(&opts(from.clone(), db.clone()), 8).expect_err("refuse");
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(
            Store::open(&db)
                .expect("reopen")
                .count_requests()
                .expect("count"),
            2
        );

        // --force re-imports the whole file; insert-only means the old
        // rows stay and the new ones are duplicates, which is the point
        // of the flag.
        let outcome = run_(
            &ImportOpts {
                force: true,
                ..opts(from, db.clone())
            },
            8,
        )
        .expect("forced import");
        assert_eq!(outcome.summary.rows_imported, 3);
        assert_eq!(outcome.summary.skipped_duplicate, 0);
        assert_eq!(
            Store::open(&db)
                .expect("reopen")
                .count_requests()
                .expect("count"),
            5
        );
    }

    #[test]
    fn shrunk_file_refuses() {
        let dir = test_dir("shrunk");
        let from = dir.join("usage.jsonl");
        write_jsonl(&from, &[&measurement(1), &measurement(2), &measurement(3)]);
        let db = dir.join("toker.db");
        run_(&opts(from.clone(), db.clone()), 8).expect("first import");

        write_jsonl(&from, &[&measurement(1)]);
        let err = run_(&opts(from, db), 8).expect_err("refuse a shrunk source");
        assert!(err.to_string().contains("--force"), "{err}");
    }

    #[test]
    fn dry_run_inserts_nothing_and_leaves_no_checkpoint() {
        let dir = test_dir("dry-run");
        let from = dir.join("usage.jsonl");
        write_jsonl(&from, &[&measurement(1), &measurement(2)]);
        let db = dir.join("toker.db");
        let outcome = run_(
            &ImportOpts {
                dry_run: true,
                ..opts(from.clone(), db.clone())
            },
            8,
        )
        .expect("dry run");
        assert_eq!(outcome.summary.rows_imported, 2, "counted, not inserted");
        assert!(!outcome.summary.already_imported);

        let store = Store::open(&db).expect("reopen");
        assert_eq!(store.count_requests().expect("count"), 0);
        assert!(
            store
                .get_meta(&meta_key(&from).expect("key"))
                .expect("meta")
                .is_none(),
            "a dry run leaves no checkpoint"
        );

        // So the real run afterwards imports everything.
        let outcome = run_(&opts(from, db), 8).expect("real import");
        assert_eq!(outcome.summary.rows_imported, 2);
    }

    #[test]
    fn import_preserves_file_order_not_ts_order() {
        let dir = test_dir("order");
        let from = dir.join("usage.jsonl");
        // Deliberately descending ts: the ledger's insertion order must
        // follow the file, not sort by time (window anchoring reads the
        // rows back sorted; the ids record the truth).
        write_jsonl(&from, &[&measurement(5), &measurement(3), &measurement(1)]);
        let db = dir.join("toker.db");
        run_(&opts(from, db.clone()), 8).expect("import");

        let mut conn = rusqlite::Connection::open(&db).expect("raw connection");
        let by_id: Vec<i64> = store_rows_by_id(&mut conn);
        let want: Vec<i64> = vec![
            ts_ms("2026-09-26T16:49:22.005Z"),
            ts_ms("2026-09-26T16:49:22.003Z"),
            ts_ms("2026-09-26T16:49:22.001Z"),
        ];
        assert_eq!(by_id, want, "ids follow the file, not the ts");
    }

    fn store_rows_by_id(conn: &mut rusqlite::Connection) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT ts_ms FROM requests ORDER BY id")
            .expect("prepare");
        stmt.query_map([], |row| row.get::<_, i64>(0))
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("rows")
    }

    #[test]
    fn batch_rollback_leaves_db_consistent_and_resumes() {
        let dir = test_dir("rollback");
        let from = dir.join("usage.jsonl");
        write_jsonl(
            &from,
            &[
                &measurement(1),
                &measurement(2),
                &measurement(3),
                &measurement(4),
                &measurement(5),
            ],
        );
        let db = dir.join("toker.db");

        // A store failure mid-file, simulated exactly where a batch
        // boundary hides it: the trigger aborts the second row of the
        // second batch, after the first row of that batch is already
        // inserted — the transaction must roll the whole batch back.
        drop(Store::open(&db).expect("create the schema first"));
        let boom = ts_ms("2026-09-26T16:49:22.004Z");
        {
            let conn = rusqlite::Connection::open(&db).expect("open raw");
            conn.execute_batch(&format!(
                "CREATE TRIGGER abort_mid_batch BEFORE INSERT ON requests
                 WHEN NEW.ts_ms = {boom}
                 BEGIN SELECT RAISE(ABORT, 'simulated mid-file failure'); END;",
            ))
            .expect("arm the failure");
        }

        let err = run_(&opts(from.clone(), db.clone()), 2).expect_err("mid-file failure");
        assert!(err.to_string().contains("rolled back"), "{err}");
        let store = Store::open(&db).expect("reopen after failure");
        // Batch 1 (lines 1-2) committed; batch 2 (lines 3-4) rolled back
        // whole — neither row 3 nor row 4 is in the ledger.
        assert_eq!(store.count_requests().expect("count"), 2);
        // And the checkpoint matches what is committed, so re-running
        // resumes rather than duplicating.
        let checkpoint = checkpoint_of(&store, &from);
        assert_eq!(checkpoint.line_count, 2);
        assert_eq!(checkpoint.first_id, Some(1));
        assert_eq!(checkpoint.last_id, Some(2));
        drop(store);

        // Heal the store, re-run: only the tail imports.
        rusqlite::Connection::open(&db)
            .expect("open raw")
            .execute("DROP TRIGGER abort_mid_batch", [])
            .expect("heal");
        let outcome = run_(&opts(from.clone(), db.clone()), 2).expect("resume");
        assert_eq!(outcome.summary.rows_imported, 3);
        assert_eq!(outcome.summary.skipped_duplicate, 2);
        let store = Store::open(&db).expect("reopen");
        assert_eq!(store.count_requests().expect("count"), 5);
        let checkpoint = checkpoint_of(&store, &from);
        assert_eq!(checkpoint.line_count, 5);
        assert_eq!(checkpoint.first_id, Some(1));
        assert_eq!(checkpoint.last_id, Some(5));
    }

    /// The real 54 MB predecessor log: a full import into a scratch db,
    /// read-only
    /// with respect to the source. Manual host verification:
    /// `cargo test -p toker --lib import -- --ignored --nocapture`.
    #[test]
    #[ignore = "imports the real 54 MB predecessor usage.jsonl — manual host verification"]
    fn real_usage_jsonl_smoke() {
        let from = PathBuf::from("/home/passcod/.local/share/claude-token-proxy/usage.jsonl");
        if !from.exists() {
            eprintln!("source not present on this host; nothing to verify");
            return;
        }
        let dir = test_dir("real-smoke");
        let db = dir.join("toker.db");
        let started = std::time::Instant::now();
        let outcome = run_(&opts(from.clone(), db.clone()), BATCH_ROWS).expect("real import");
        let elapsed = started.elapsed();
        let s = &outcome.summary;
        eprintln!(
            "real import: lines {}, imported {}, malformed {}, duplicate {}, in {:.2}s",
            s.lines_read,
            s.rows_imported,
            s.skipped_malformed,
            s.skipped_duplicate,
            elapsed.as_secs_f64()
        );
        if let Some((line, reason)) = &outcome.first_malformed {
            eprintln!("first malformed line: {line}: {reason}");
        }

        let store = Store::open(&db).expect("reopen");
        let count = store.count_requests().expect("count");
        assert_eq!(count as u64, s.rows_imported);
        assert_eq!(
            s.lines_read,
            s.rows_imported + s.skipped_malformed + s.skipped_duplicate
        );

        // Max ts sane: within (2020, now + 24h). A garbage ts would have
        // been a malformed line, so this holds by construction, but the
        // assert also proves every real line mapped without panicking.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis() as i64;
        let newest = store
            .requests_since(0, 1)
            .expect("newest row")
            .pop()
            .expect("at least one row");
        assert!(
            newest.ts_ms > 1_577_836_800_000,
            "after 2020: {}",
            newest.ts_ms
        );
        assert!(
            newest.ts_ms < now + 24 * 3600 * 1000,
            "not far future: {}",
            newest.ts_ms
        );

        // And the second run is a clean no-op.
        let again = run_(&opts(from, db), BATCH_ROWS).expect("second run");
        assert!(again.summary.already_imported);
        assert_eq!(again.summary.rows_imported, 0);
    }
}
