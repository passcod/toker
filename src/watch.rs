//! `toker watch-context-window`: report, once each, the served responses
//! that prove a context window. The port of ctp's
//! `watch-context-window.mjs`, meant to be run repeatedly by a monitor:
//! each invocation is one pass over the ledger that prints a `PROOF`
//! line per new proof and exits.
//!
//! Two things count as proof, and nothing else does:
//!
//! - an exact 1M window (native, or selected by a captured beta in the
//!   phases where a beta selected it) serving a prompt over 200k, which a
//!   200k window could not have held;
//! - a prompt over a provider-declared ceiling, which shows the ceiling
//!   is not the limit it was declared as.
//!
//! The window comes from
//! [`resolve_context_window`](crate::catalog::windows::resolve_context_window)
//! in its usual precedence (hand-verified catalogue, the provider's
//! fetched listing, the learned store's declaration, unknown). A model
//! with no known window, or a prompt below the threshold, produces no
//! claim.
//!
//! Only rows at or after `since` are read: without that bound the whole
//! ledger replays as if it were happening now. A seen-set in the state
//! file makes each proof print once across passes.
//!
//! There is deliberately no compaction detector. Within a lane, a real
//! compaction and a subagent starting both show up as the prompt
//! collapsing, and nothing else in the ledger separates them. The two
//! tests above make only claims the served response can prove, so
//! ambiguous negative evidence is left unreported.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{Value, json};

use crate::catalog::FetchedCatalogs;
use crate::catalog::windows::{ContextWindow, model_identity, resolve_context_window};
use crate::store::{self, KindFilter, RequestFilter, RequestRow, Store};

/// How many seen keys the state file keeps, newest last (ctp's cap).
pub const SEEN_CAP: usize = 400;

/// The prompt an exact 1M window must exceed to prove itself: the
/// 200k window it replaced.
pub const PROOF_OVER: u64 = 200_000;

/// The state file's name in the state dir, when no path is given.
pub const STATE_FILE: &str = "watch-context-window.json";

/// The default state file: beside the ledger, in the state dir.
pub fn default_state_path(db: &Path) -> PathBuf {
    db.parent()
        .map(|dir| dir.join(STATE_FILE))
        .unwrap_or_else(|| PathBuf::from(STATE_FILE))
}

/// What persists between passes: when the watch began, and which
/// proofs it has already printed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WatchState {
    /// The `since` the first pass used, so a monitor that passes no
    /// `--since` keeps one window rather than starting afresh each run.
    pub since_ms: Option<i64>,
    /// Proof keys, oldest first.
    seen: Vec<String>,
}

impl WatchState {
    /// Whether `key` has printed before.
    pub fn has_seen(&self, key: &str) -> bool {
        self.seen.iter().any(|seen| seen == key)
    }

    /// Record `key`; false when it was already there.
    fn insert(&mut self, key: String) -> bool {
        if self.has_seen(&key) {
            return false;
        }
        self.seen.push(key);
        true
    }

    /// Read the state file. An absent file is a fresh state; one that
    /// does not parse is an error, because starting afresh would print
    /// every proof in the window again.
    pub fn load(path: &Path) -> anyhow::Result<WatchState> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(WatchState::default());
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not JSON", path.display()))?;
        let seen = value
            .get("seen")
            .and_then(Value::as_array)
            .with_context(|| format!("{} has no `seen` array", path.display()))?
            .iter()
            .map(|key| key.as_str().map(str::to_owned))
            .collect::<Option<Vec<String>>>()
            .with_context(|| {
                format!("{} has a `seen` entry that is not a string", path.display())
            })?;
        let since_ms = match value.get("since_ms") {
            None | Some(Value::Null) => None,
            Some(since) => Some(
                since
                    .as_i64()
                    .with_context(|| format!("{} has a non-integer since_ms", path.display()))?,
            ),
        };
        Ok(WatchState { since_ms, seen })
    }

    /// Write the state file atomically, keeping the newest
    /// [`SEEN_CAP`] keys.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let keep = self.seen.len().saturating_sub(SEEN_CAP);
        let value = json!({"since_ms": self.since_ms, "seen": &self.seen[keep..]});
        crate::setup::atomic::atomic_write_json(path, &value)
    }
}

/// Everything the window resolution consults beyond the row itself.
pub struct Windows<'a> {
    /// The providers' fetched listings (read from the cache, never
    /// fetched here).
    pub catalogs: &'a FetchedCatalogs,
    /// The learned store's stored declarations, by model identity.
    pub declared: HashMap<String, Value>,
}

impl<'a> Windows<'a> {
    /// The declarations from the store's models table.
    pub fn from_store(store: &Store, catalogs: &'a FetchedCatalogs) -> store::Result<Self> {
        let declared = store
            .load_models()?
            .into_iter()
            .filter_map(|entry| Some((entry.model_id, entry.context_window_json?)))
            .collect();
        Ok(Windows { catalogs, declared })
    }

    /// The window one served response had, as of when it was served;
    /// `betas` as the row captured them (`None` when not captured).
    fn resolve(&self, row: &RequestRow, model: &str, betas: Option<&[&str]>) -> ContextWindow {
        let fetched = row
            .provider
            .as_deref()
            .and_then(|provider| self.catalogs.context_window_of(provider, model));
        let declared = model_identity(model).and_then(|id| self.declared.get(&id));
        let at = jiff::Timestamp::from_millisecond(row.ts_ms)
            .ok()
            .map(|ts| ts.to_string());
        resolve_context_window(model, betas, fetched, declared, at.as_deref())
    }
}

/// The prompt one row served: fresh input, cache read and cache write.
/// A missing bucket counts as nothing, which can only lower the sum, so
/// a proof built on it understates rather than invents.
fn prompt_of(row: &RequestRow) -> u64 {
    [row.input, row.cache_read, row.cache_write_total]
        .into_iter()
        .map(|bucket| bucket.unwrap_or(0).max(0) as u64)
        .fold(0, u64::saturating_add)
}

/// One pass: every API measurement at or after `since_ms` (in the
/// order served), filtered to `prefixes` when any are given, checked
/// against its window. Returns the new proof lines and records their
/// keys in `state`; a proof already in `state` is not repeated.
pub fn pass(
    store: &Store,
    windows: &Windows<'_>,
    since_ms: i64,
    prefixes: &[String],
    state: &mut WatchState,
) -> store::Result<Vec<String>> {
    // Proxy-written rows report no served prompt: a gate's row describes
    // a request that never reached upstream, and an error row's request
    // was not served.
    let filter = RequestFilter {
        since_ms: Some(since_ms),
        kind: KindFilter::Measurement,
        ..RequestFilter::default()
    };
    let mut out = Vec::new();
    store.for_each_request(&filter, |row| {
        let session = row.session_id.as_deref();
        if !prefixes.is_empty()
            && !session.is_some_and(|sid| prefixes.iter().any(|p| sid.starts_with(p.as_str())))
        {
            return Ok(());
        }
        let sid: String =
            session.map_or_else(|| "?".to_owned(), |sid| sid.chars().take(8).collect());
        // The served identity is the authority on context.
        let Some(served) = row.raw_model.as_deref().or(row.model.as_deref()) else {
            return Ok(());
        };
        let model = model_identity(served).unwrap_or_else(|| "?".to_owned());
        let betas: Option<Vec<String>> = row
            .betas
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok());
        let betas: Option<Vec<&str>> = betas
            .as_ref()
            .map(|betas| betas.iter().map(String::as_str).collect());
        let prompt = prompt_of(&row);

        match windows.resolve(&row, served, betas.as_deref()) {
            ContextWindow::Exact { tokens: 1_000_000 } if prompt > PROOF_OVER => {
                let key = format!("{sid}:{model}:exact-1m:over-200k");
                if state.insert(key) {
                    let native = windows.resolve(&row, served, None);
                    let selection = if native == (ContextWindow::Exact { tokens: 1_000_000 }) {
                        "a native 1M context window"
                    } else {
                        "the beta-selected 1M context window"
                    };
                    out.push(format!(
                        "PROOF {sid} reached {} prompt tokens with {selection}: the window is real",
                        crate::cmds::thousands(prompt as i64),
                    ));
                }
            }
            ContextWindow::Declared { tokens } if prompt > tokens => {
                let key = format!("{sid}:{model}:declared:{tokens}:over-ceiling");
                if state.insert(key) {
                    out.push(format!(
                        "PROOF {sid} on {model} reached {} prompt tokens: exceeded declared \
                         context ceiling {}",
                        crate::cmds::thousands(prompt as i64),
                        crate::cmds::thousands(tokens as i64),
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{PROOF_OVER, WatchState, Windows, pass};
    use crate::catalog::fetched::parse_openrouter;
    use crate::catalog::{CONTEXT_1M_BETA, FetchedCatalogs};
    use crate::store::{ModelEntry, RequestRow, RowKind, Store};
    use serde_json::json;

    /// 2026-10-05T00:00:00Z.
    const T0: i64 = 1_791_158_400_000;
    /// 2025-10-15T00:00:00Z: inside the sonnet-4-5 beta phase.
    const BETA_ERA: i64 = 1_760_486_400_000;

    const ALPHA: &str = "a1a1a1a1-0000-4000-8000-000000000001";
    const BRAVO: &str = "b2b2b2b2-0000-4000-8000-000000000002";

    /// A served measurement: `prompt` tokens, all of it cache read.
    fn served(ts_ms: i64, session: &str, provider: &str, model: &str, prompt: i64) -> RequestRow {
        RequestRow {
            id: None,
            ts_ms,
            duration_ms: None,
            kind: None,
            frontend: None,
            provider: Some(provider.to_owned()),
            route: None,
            session_id: Some(session.to_owned()),
            ping: None,
            model: Some(model.to_owned()),
            raw_model: Some(model.to_owned()),
            requested_model: None,
            effective_model: None,
            input: Some(2),
            cache_read: Some(prompt - 2),
            cache_write_total: None,
            cache_write_5m: None,
            cache_write_1h: None,
            output: None,
            reasoning: None,
            iterations: None,
            web_searches: None,
            code_execs: None,
            ttl_split_known: None,
            usage_presence: None,
            usage_raw: None,
            cost_usd: None,
            cost_kind: None,
            rate_limits: None,
            req_bytes: None,
            req_messages: None,
            req_tools: None,
            tools_hash: None,
            system_chars: None,
            system_hash: None,
            system_blocks: None,
            system_messages: None,
            compact_generations: None,
            summarising: None,
            system_change: None,
            system_ladder: None,
            system_tail: None,
            gate_on: None,
            cold_on: None,
            forced_from: None,
            forced_to: None,
            downgraded_from: None,
            downgraded_to: None,
            cache_stripped: None,
            system_merged: None,
            model_mappings: None,
            drift_digest: None,
            status: None,
            error_type: None,
            retry_after_ms: None,
            extra: None,
            betas: None,
            geo: None,
            fast: None,
        }
    }

    fn store_with(rows: &[RequestRow]) -> Store {
        let store = Store::open(":memory:").expect("open");
        for row in rows {
            store.record_request(row).expect("record");
        }
        store
    }

    fn no_catalogs() -> FetchedCatalogs {
        FetchedCatalogs::default()
    }

    fn run(
        store: &Store,
        catalogs: &FetchedCatalogs,
        since: i64,
        prefixes: &[&str],
    ) -> Vec<String> {
        let windows = Windows::from_store(store, catalogs).expect("windows");
        let prefixes: Vec<String> = prefixes.iter().map(|p| (*p).to_owned()).collect();
        pass(
            store,
            &windows,
            since,
            &prefixes,
            &mut WatchState::default(),
        )
        .expect("pass")
    }

    #[test]
    fn a_native_1m_window_is_proven_past_200k() {
        let store = store_with(&[served(
            T0,
            ALPHA,
            "anthropic_sub",
            "claude-opus-5-5",
            523_811,
        )]);
        insta::assert_snapshot!(run(&store, &no_catalogs(), T0, &[]).join("\n"), @"PROOF a1a1a1a1 reached 523,811 prompt tokens with a native 1M context window: the window is real");
    }

    #[test]
    fn a_beta_selected_1m_window_is_named_as_such() {
        let mut row = served(
            BETA_ERA,
            ALPHA,
            "anthropic_sub",
            "claude-sonnet-4-5",
            250_000,
        );
        row.betas = Some(json!(["claude-code-20250219", CONTEXT_1M_BETA]).to_string());
        let store = store_with(&[row]);
        insta::assert_snapshot!(run(&store, &no_catalogs(), BETA_ERA, &[]).join("\n"), @"PROOF a1a1a1a1 reached 250,000 prompt tokens with the beta-selected 1M context window: the window is real");
    }

    #[test]
    fn a_declared_ceiling_exceeded_is_proven_from_each_source() {
        // The hand-verified catalogue's own declaration.
        let catalogued = served(T0, ALPHA, "codex_sub", "gpt-5.6-sol", 900_000);
        // A fetched listing's.
        let listed = served(T0 + 1, BRAVO, "openrouter", "acme/long-1", 140_000);
        // The learned store's.
        let learned = served(T0 + 2, BRAVO, "anthropic_api", "acme-learned", 70_000);
        let store = store_with(&[catalogued, listed, learned]);
        store
            .upsert_model(&ModelEntry {
                model_id: "acme-learned".to_owned(),
                days_json: None,
                max_prompt: None,
                context_window_json: Some(json!({"default": 32_000, "max": 64_000})),
            })
            .expect("declare");
        let mut catalogs = FetchedCatalogs::default();
        catalogs.set(
            "openrouter",
            parse_openrouter(
                &json!({"data": [{"id": "acme/long-1", "context_length": 131_072}]}),
                T0,
            )
            .expect("listing"),
        );
        insta::assert_snapshot!(run(&store, &catalogs, T0, &[]).join("\n"), @r"
        PROOF a1a1a1a1 on gpt-5.6-sol reached 900,000 prompt tokens: exceeded declared context ceiling 872,000
        PROOF b2b2b2b2 on acme/long-1 reached 140,000 prompt tokens: exceeded declared context ceiling 131,072
        PROOF b2b2b2b2 on acme-learned reached 70,000 prompt tokens: exceeded declared context ceiling 64,000
        ");
    }

    #[test]
    fn it_stays_quiet_below_the_threshold() {
        let store = store_with(&[
            served(
                T0,
                ALPHA,
                "anthropic_sub",
                "claude-opus-5-5",
                PROOF_OVER as i64,
            ),
            served(T0, ALPHA, "codex_sub", "gpt-5.6-sol", 872_000),
            // A 200k model over 200k proves nothing this watch claims.
            served(T0, ALPHA, "anthropic_sub", "claude-haiku-4-5", 210_000),
        ]);
        assert_eq!(run(&store, &no_catalogs(), T0, &[]), Vec::<String>::new());
    }

    #[test]
    fn it_stays_quiet_for_a_model_with_no_known_window() {
        let store = store_with(&[
            // Neither catalogued, listed nor declared.
            served(T0, ALPHA, "openrouter", "acme/unknown", 2_000_000),
            // A provider with no fetched catalogue at all.
            served(T0, ALPHA, "elsewhere", "acme/long-1", 2_000_000),
            // A family name is not a window.
            served(T0, ALPHA, "anthropic_sub", "claude-opus-9", 2_000_000),
        ]);
        let mut catalogs = FetchedCatalogs::default();
        catalogs.set(
            "openrouter",
            parse_openrouter(
                &json!({"data": [{"id": "acme/long-1", "context_length": 131_072}]}),
                T0,
            )
            .expect("listing"),
        );
        assert_eq!(run(&store, &catalogs, T0, &[]), Vec::<String>::new());
        // And in the beta era, an uncaptured selection proves nothing.
        let store = store_with(&[served(
            BETA_ERA,
            ALPHA,
            "anthropic_sub",
            "claude-sonnet-4-5",
            900_000,
        )]);
        assert_eq!(
            run(&store, &no_catalogs(), BETA_ERA, &[]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn it_stays_quiet_for_rows_before_since_and_proxy_rows() {
        let mut blocked = served(T0 + 10, ALPHA, "anthropic_sub", "claude-opus-5-5", 600_000);
        blocked.kind = Some(RowKind::Blocked);
        let mut error = served(T0 + 20, ALPHA, "anthropic_sub", "claude-opus-5-5", 600_000);
        error.kind = Some(RowKind::Error);
        let store = store_with(&[
            served(T0 - 1, ALPHA, "anthropic_sub", "claude-opus-5-5", 600_000),
            blocked,
            error,
        ]);
        assert_eq!(run(&store, &no_catalogs(), T0, &[]), Vec::<String>::new());
    }

    #[test]
    fn a_proof_prints_once_across_passes() {
        let store = store_with(&[
            served(T0, ALPHA, "anthropic_sub", "claude-opus-5-5", 300_000),
            served(T0 + 1, ALPHA, "anthropic_sub", "claude-opus-5-5", 400_000),
        ]);
        let catalogs = no_catalogs();
        let windows = Windows::from_store(&store, &catalogs).expect("windows");
        let mut state = WatchState::default();
        let first = pass(&store, &windows, T0, &[], &mut state).expect("first");
        assert_eq!(first.len(), 1, "one key per session and model: {first:?}");
        let again = pass(&store, &windows, T0, &[], &mut state).expect("again");
        assert_eq!(again, Vec::<String>::new());
    }

    #[test]
    fn session_prefixes_narrow_the_watch() {
        let store = store_with(&[
            served(T0, ALPHA, "anthropic_sub", "claude-opus-5-5", 300_000),
            served(T0, BRAVO, "anthropic_sub", "claude-opus-5-5", 300_000),
        ]);
        let only = run(&store, &no_catalogs(), T0, &["b2b2"]);
        assert_eq!(only.len(), 1);
        assert!(only[0].starts_with("PROOF b2b2b2b2 "), "{only:?}");
    }

    #[test]
    fn the_state_file_round_trips_and_keeps_the_newest_keys() {
        let dir = std::env::temp_dir().join(format!("toker-watch-state-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(super::STATE_FILE);
        assert_eq!(
            WatchState::load(&path).expect("absent"),
            WatchState::default()
        );

        let mut state = WatchState {
            since_ms: Some(T0),
            ..WatchState::default()
        };
        for n in 0..super::SEEN_CAP + 5 {
            state.insert(format!("key-{n}"));
        }
        state.save(&path).expect("save");
        let loaded = WatchState::load(&path).expect("load");
        assert_eq!(loaded.since_ms, Some(T0));
        assert!(!loaded.has_seen("key-4"), "the oldest keys are dropped");
        assert!(loaded.has_seen("key-5"));
        assert!(loaded.has_seen(&format!("key-{}", super::SEEN_CAP + 4)));

        std::fs::write(&path, "{\"seen\": [1]}").expect("corrupt");
        assert!(
            WatchState::load(&path).is_err(),
            "a corrupt state is an error, not a replay"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
