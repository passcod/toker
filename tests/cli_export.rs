//! `toker export` as a process: strict flags, a missing ledger that
//! stays missing, and a reader that goes away mid-stream.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use toker::store::{RequestRow, Store};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("toker-cli-export-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn toker() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_toker"));
    // Nothing here may read the real config or ledger.
    command
        .env("TOKER_CONFIG", "/nonexistent/toker.toml")
        .env_remove("TOKER_DB");
    command
}

/// A measurement row with every optional column NULL.
fn bare(ts_ms: i64) -> RequestRow {
    RequestRow {
        id: None,
        ts_ms,
        duration_ms: None,
        kind: None,
        frontend: None,
        provider: None,
        route: None,
        session_id: None,
        ping: None,
        model: None,
        raw_model: None,
        requested_model: None,
        effective_model: None,
        input: None,
        cache_read: None,
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

#[test]
fn bad_flags_fail_and_print_nothing_on_stdout() {
    let dir = scratch("flags");
    let db = dir.join("toker.db");
    Store::open(&db).expect("ledger");
    for args in [
        vec!["--since", "yesterday"],
        vec!["--since", "2026-10-05"],
        vec!["--until", "2.5h"],
        vec!["--kind", "measurements"],
        vec!["--sinse", "2h"],
    ] {
        let output = toker()
            .arg("export")
            .arg("--db")
            .arg(&db)
            .args(&args)
            .output()
            .expect("run");
        assert!(!output.status.success(), "{args:?} must fail");
        assert!(output.stdout.is_empty(), "{args:?} wrote to stdout");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_missing_ledger_fails_and_is_not_created() {
    let dir = scratch("missing");
    let db = dir.join("toker.db");
    let output = toker()
        .arg("export")
        .arg("--db")
        .arg(&db)
        .output()
        .expect("run");
    assert!(!output.status.success());
    assert!(!db.exists(), "export must never create a ledger");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_reader_that_goes_away_ends_the_export_quietly() {
    let dir = scratch("pipe");
    let db = dir.join("toker.db");
    let store = Store::open(&db).expect("ledger");
    // Far more than a pipe buffer holds, so the writer is still going
    // when the reader leaves.
    let rows: Vec<RequestRow> = (0..20_000).map(|i| bare(1_791_158_400_000 + i)).collect();
    store.record_requests(&rows).expect("rows");
    drop(store);

    let mut child = toker()
        .arg("export")
        .arg("--db")
        .arg(&db)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdout = child.stdout.take().expect("stdout");
    let mut first = [0u8; 64];
    stdout.read_exact(&mut first).expect("some output");
    drop(stdout); // `| head -c 64`
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("stderr");
    let status = child.wait().expect("wait");
    assert!(status.success(), "exit {status:?}, stderr: {stderr}");
    assert!(
        stderr.is_empty(),
        "a closed pipe is not worth a word: {stderr}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
