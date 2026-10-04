//! The TUI's steady-state CPU probe: the real `toker tui` binary under
//! a pseudo-terminal, against a scratch ledger at display scale.
//!
//! Not a correctness gate — a re-run of the steady-state CPU story
//! after the display tick's read was narrowed to the ten-column
//! projection (invariant 7): the loop must spend its 2 s ticks mostly
//! blocked on the event poll, never spinning. The pre-fix quota-tick
//! spin measured 80% of a core; the quota fix took steady state to
//! ~1.4%. This probe pins that the display tick holds that line even
//! at the pathological 10 000-row window the cap guards, in the debug
//! build.
//!
//! Needs `/proc` and util-linux `script` (the pty); run deliberately:
//! `cargo test --test tui_pty -- --ignored --nocapture`.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use toker::store::{CostKind, RequestRow, RowKind, Store};

/// The display window's row cap (`tui::ROW_CAP`), restated here so the
/// probe does not reach into the crate's private TUI internals.
const ROW_CAP: usize = 10_000;

/// A measurement row with every optional column NULL, at `ts_ms`.
fn bare_row(ts_ms: i64) -> RequestRow {
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

/// Seed the scratch ledger with one 30-minute display window at the
/// row cap, production-shaped: billed openrouter rows (with the
/// `extra.serving_provider` and `usage_raw` the full-row read used to
/// fetch), anthropic-sub plan-equivalent rows carrying meter snapshots,
/// NULL-cost measurements, and error/drift rows.
fn seed_display_window(store: &Store, now_ms: i64) {
    let since_ms = now_ms - 30 * 60_000;
    let reset_5h = (now_ms + 3 * 60 * 60_000) / 1000;
    let sessions = ["ses-alpha", "ses-beta", "ses-gamma", "ses-delta"];
    let mut batch: Vec<RequestRow> = Vec::new();
    for i in 0..ROW_CAP as i64 {
        let ts_ms = since_ms + (i * 30 * 60_000) / ROW_CAP as i64;
        let row = match i % 20 {
            0..=8 => {
                let mut row = bare_row(ts_ms);
                row.session_id = Some(sessions[(i as usize / 20) % sessions.len()].to_owned());
                row.provider = Some("openrouter".to_owned());
                row.model = Some(["z-ai/glm-5.3", "openai/gpt-5.2"][i as usize % 2].to_owned());
                row.input = Some(1_000 + i % 900);
                row.cache_read = Some(40_000 + i % 5_000);
                row.output = Some(i % 800);
                row.cost_usd = Some(0.001 + (i % 50) as f64 / 10_000.0);
                row.cost_kind = Some(CostKind::Billed);
                let serving = ["Relace", "Kimi", "Elsewhere"][i as usize % 3];
                row.extra = Some(serde_json::json!({"serving_provider": serving}));
                row.usage_raw = Some(
                    r#"{"prompt_tokens":1234,"cost":0.0021,"cost_details":{"upstream":"0.0019"}}"#
                        .to_owned(),
                );
                row
            }
            9..=15 => {
                let q = |util: f64| (util * 100.0).round() / 100.0;
                let mut row = bare_row(ts_ms);
                row.session_id = Some("ses-anthropic".to_owned());
                row.provider = Some("anthropic_sub".to_owned());
                row.model = Some("claude-opus-5".to_owned());
                row.input = Some(30_000 + i % 4_000);
                row.cache_read = Some(80_000);
                row.output = Some(1_000 + i % 300);
                row.cost_usd = Some(1.5);
                row.cost_kind = Some(CostKind::PlanEquivalent);
                row.rate_limits = Some(serde_json::json!({
                    "util5h": q((i % 900) as f64 / 900.0 * 0.95),
                    "reset5h": reset_5h,
                    "util7d": q(0.10 + i as f64 / ROW_CAP as f64 * 0.60),
                    "status5h": "allowed", "claim": "five_hour",
                }));
                row
            }
            16..=17 => {
                let mut row = bare_row(ts_ms);
                row.session_id = Some(sessions[i as usize % sessions.len()].to_owned());
                row.input = Some(500);
                row
            }
            18 => {
                let mut row = bare_row(ts_ms);
                row.kind = Some(RowKind::Error);
                row.status = Some(500);
                row.error_type = Some("upstream".to_owned());
                row
            }
            _ => {
                let mut row = bare_row(ts_ms);
                row.kind = Some(RowKind::FidelityDrift);
                row.drift_digest = Some("sha256:drift".to_owned());
                row
            }
        };
        batch.push(row);
        if batch.len() == 2_500 {
            store.record_requests(&batch).expect("seed a batch");
            batch.clear();
        }
    }
    store.record_requests(&batch).expect("seed the tail batch");
}

/// The TUI process to probe: the one whose command line names THIS
/// run's scratch db AND whose executable is the toker binary (the
/// `script`/shell wrappers around it carry the same command line, so
/// the comm check is what tells them apart). The live `toker serve`
/// never matches the scratch db path.
fn find_tui(db_marker: &str) -> Option<u32> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read_to_string(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
            continue;
        };
        if comm.trim() == "toker" && cmdline.contains("tui") && cmdline.contains(db_marker) {
            return Some(pid);
        }
    }
    None
}

/// Cumulative CPU time of `pid`, in clock ticks (utime + stime, fields
/// 14/15 — taken after the LAST `)` so a parenthesised comm cannot
/// shift the parse).
fn cpu_ticks(pid: u32) -> std::io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let after_comm = stat.rsplit_once(')').map(|(_, rest)| rest).unwrap_or(&stat);
    let mut fields = after_comm.split_whitespace();
    // state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt
    // cmajflt — then utime (14) and stime (15).
    for _ in 0..11 {
        fields.next();
    }
    let missing = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "truncated /proc/<pid>/stat",
        )
    };
    let utime: u64 = fields.next().ok_or_else(missing)?.parse().expect("utime");
    let stime: u64 = fields.next().ok_or_else(missing)?.parse().expect("stime");
    Ok(utime + stime)
}

#[test]
#[ignore = "a pty probe, not a correctness gate: runs the real TUI \
            binary under a pseudo-terminal against a scratch ledger at \
            display scale (10 000 window rows) and samples its \
            steady-state CPU across ~20 s of 2 s ticks; needs /proc \
            and util-linux script. Run deliberately with --ignored"]
fn tui_steady_state_cpu_over_the_2s_tick_at_display_scale() {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_millis() as i64;

    // A scratch ledger under /tmp/opencode — never the live one, whose
    // daemon is proxying the session supervising this very test run.
    let dir =
        PathBuf::from("/tmp/opencode").join(format!("toker-pty-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = dir.join("bench.db");
    let store = Store::open(&db).expect("open the scratch ledger");
    seed_display_window(&store, now_ms);
    assert_eq!(
        store.count_requests().expect("count"),
        ROW_CAP as i64,
        "the seed is complete"
    );
    drop(store);

    // The real binary under a real pty: `script` allocates the
    // pseudo-terminal so ratatui/crossterm init the way the user's
    // terminal behaves, and the typescript goes to /dev/null.
    let bin = env!("CARGO_BIN_EXE_toker");
    let cmd = format!("{bin} tui --window-mins 30 --db {}", db.display());
    let mut script = Command::new("script")
        .arg("-qec")
        .arg(&cmd)
        .arg("/dev/null")
        .env("TERM", "xterm-256color")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the TUI under a pty");

    // Wait for the TUI to come up, then let it settle past its first
    // ticks before sampling.
    let pid = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pid) = find_tui(&format!("{}", db.display())) {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "the TUI never appeared under the pty"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    std::thread::sleep(Duration::from_secs(3));

    // Sample CPU across ~20 s of steady-state ticking: 10 display
    // ticks plus draw cost, with the quota tick amortized over its own
    // 60 s cadence.
    let ticks0 = cpu_ticks(pid).expect("sample the TUI's CPU");
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(20));
    let ticks1 = cpu_ticks(pid).expect("sample the TUI's CPU again");
    let elapsed = start.elapsed();
    // The TUI must still be alive and rendering — a crash would have
    // been a failure regardless of its CPU bill.
    assert!(cpu_ticks(pid).is_ok(), "the TUI died mid-probe");

    let clk_tck: u64 = String::from_utf8_lossy(
        &Command::new("getconf")
            .arg("CLK_TCK")
            .output()
            .expect("read CLK_TCK")
            .stdout,
    )
    .trim()
    .parse()
    .expect("CLK_TCK is a number");
    let cpu_ms = ticks1.saturating_sub(ticks0) * 1000 / clk_tck;
    let percent = cpu_ms as f64 / elapsed.as_millis() as f64 * 100.0;

    eprintln!(
        "TUI steady state, {ROW_CAP}-row display window, debug build, {:.1} s sampled:\n  \
         {:.1}% of a core ({cpu_ms} ms CPU) — the 2 s tick stays mostly blocked;\n  \
         for reference, the pre-narrowing full-row read at this scale \
         cost ~740 ms per tick ({:.1}%) and the old quota-tick spin \
         measured 80%.",
        elapsed.as_secs_f64(),
        percent,
        740.0 / 2000.0 * 100.0,
    );
    assert!(
        percent < 15.0,
        "the TUI is burning {percent}% of a core at steady state — \
         the 2 s tick has regressed into a spin (the pre-fix bug \
         measured 80%)"
    );

    // Tear down: the TUI first (its q key cannot be sent through the
    // null stdin), then the pty wrapper that exits with it.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("stop the TUI");
    let _ = script.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
