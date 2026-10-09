//! `toker export`: the ledger's `requests` rows as JSONL on stdout, one
//! object per row, oldest first and newest last, so `tail`, `grep` and
//! `jq` work over it the way they did over the predecessor's
//! `usage.jsonl`.
//!
//! The keys are the ledger's own column names (`cache_read`, not ctp's
//! `cacheRead`). The export is a view of the ledger, so it uses the
//! ledger's vocabulary: a key means exactly what the column means in
//! `docs/internals/ledger-schema.md`, and a query written against the
//! export reads the same against the SQL. Renaming to ctp's camelCase
//! would make the rows look like ctp's while meaning toker's, which is
//! worse than looking different: an imported row and a ctp row would
//! share names across fields whose eras and semantics differ.
//!
//! Absent values are omitted, never written as `null` or zero: a missing
//! key is a NULL column, "not recorded" (see
//! [`Store::for_each_export`](crate::store::Store::for_each_export) for
//! the value rules, and the one derived key, `ts`).
//!
//! Content and credentials: the ledger holds neither (invariants 1 and
//! 2), and the export adds nothing that is not a column. `usage_raw` is
//! exported because it is the provider's `usage` object, counts and
//! costs only; the recorders store that object and nothing around it.

use std::io::Write;

use crate::store::{self, KindFilter, RequestFilter, RowKind, Store};

/// Write every row `filter` admits to `out` as JSONL. Returns how many
/// rows were written. An I/O error (a closed pipe included) ends the
/// walk and comes back as [`store::Error::Io`]; the caller decides
/// which ones are quiet.
pub fn write_jsonl(
    store: &Store,
    filter: &RequestFilter,
    out: &mut impl Write,
) -> store::Result<u64> {
    let mut written = 0;
    store.for_each_export(filter, |object| {
        serde_json::to_writer(&mut *out, object).map_err(std::io::Error::from)?;
        out.write_all(b"\n")?;
        written += 1;
        Ok(())
    })?;
    out.flush()?;
    Ok(written)
}

/// Whether a store error is the reader going away (`toker export | head`):
/// the one failure that is not a failure.
pub fn is_broken_pipe(error: &store::Error) -> bool {
    matches!(error, store::Error::Io(io) if io.kind() == std::io::ErrorKind::BrokenPipe)
}

/// Parse a `--since`/`--until` instant against `now_ms`: an RFC 3339
/// timestamp with an offset (`2026-10-05T09:00:00Z`,
/// `2026-10-05T21:00:00+12:00`), or a span before now, a whole number
/// with one unit, `s`, `m`, `h`, `d` or `w` (`90m`, `2h`, `3d`). A day is
/// 24 hours here, not a calendar day. Anything else is an error, so a
/// typo never widens the window to the whole ledger.
pub fn parse_instant(text: &str, now_ms: i64) -> Result<i64, String> {
    let expected = "expected an RFC 3339 timestamp (2026-10-05T09:00:00Z) \
                    or a span before now (90m, 2h, 3d, 1w)";
    if let Some(unit) = text.chars().last()
        && let Some(unit_ms) = match unit {
            's' => Some(1_000),
            'm' => Some(60_000),
            'h' => Some(3_600_000),
            'd' => Some(86_400_000),
            'w' => Some(7 * 86_400_000),
            _ => None,
        }
    {
        let digits = &text[..text.len() - 1];
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return digits
                .parse::<i64>()
                .ok()
                .and_then(|count| count.checked_mul(unit_ms))
                .and_then(|span| now_ms.checked_sub(span))
                .ok_or_else(|| format!("{text:?} is too far back"));
        }
    }
    text.parse::<jiff::Timestamp>()
        .map(|ts| ts.as_millisecond())
        .map_err(|_| format!("{text:?} is not a time: {expected}"))
}

/// The clap value parser for instants: [`parse_instant`] against the
/// clock at parse time.
pub fn instant_arg(text: &str) -> Result<i64, String> {
    parse_instant(text, jiff::Timestamp::now().as_millisecond())
}

/// Parse `--kind`: `all`, `measurement` (API measurements, `kind` NULL),
/// `proxy` (every proxy-written row), or one proxy kind by its stored
/// name (`blocked`, `cold-quiet`, …).
pub fn kind_arg(text: &str) -> Result<KindFilter, String> {
    match text {
        "all" => Ok(KindFilter::All),
        "measurement" => Ok(KindFilter::Measurement),
        "proxy" => Ok(KindFilter::Proxy),
        other => RowKind::parse(other).map(KindFilter::Is).ok_or_else(|| {
            format!(
                "unknown kind {other:?} (expected all, measurement, proxy, blocked, \
                 released, cold, cold-quiet, awake, error, or fidelity-drift)"
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_broken_pipe, kind_arg, parse_instant, write_jsonl};
    use crate::store::{CostKind, KindFilter, RequestFilter, RequestRow, RowKind, Store};
    use serde_json::{Value, json};

    /// 2026-10-05T00:00:00Z.
    const T0: i64 = 1_791_158_400_000;

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

    /// A measurement with every column set: the export must carry each.
    fn full(ts_ms: i64) -> RequestRow {
        RequestRow {
            id: None,
            ts_ms,
            duration_ms: Some(4_392),
            kind: None,
            frontend: Some("anthropic".to_owned()),
            provider: Some("anthropic_sub".to_owned()),
            route: Some("anthropic:anthropic_sub".to_owned()),
            session_id: Some("5a1e0c2d-0000-4000-8000-000000000001".to_owned()),
            ping: Some(false),
            model: Some("claude-opus-5".to_owned()),
            raw_model: Some("claude-opus-5-20261001".to_owned()),
            requested_model: Some("claude-sonnet-4-5".to_owned()),
            effective_model: Some("claude-opus-5".to_owned()),
            input: Some(673),
            cache_read: Some(66_944),
            cache_write_total: Some(30),
            cache_write_5m: Some(10),
            cache_write_1h: Some(20),
            output: Some(812),
            reasoning: Some(12),
            iterations: Some(3),
            web_searches: Some(2),
            code_execs: Some(1),
            ttl_split_known: Some(true),
            usage_presence: Some(json!({"input": true, "output": true})),
            usage_raw: Some("{\n  \"input_tokens\": 673,\n  \"output_tokens\": 812\n}".to_owned()),
            cost_usd: Some(0.039712),
            cost_kind: Some(CostKind::PlanEquivalent),
            rate_limits: Some(json!({"util5h": 0.42, "reset5h": 1_791_176_400})),
            req_bytes: Some(362_736),
            req_messages: Some(67),
            req_tools: Some(39),
            tools_hash: Some("27023a35ec81".to_owned()),
            system_chars: Some(2_873),
            system_hash: Some("b8060eba6beb".to_owned()),
            system_blocks: Some(json!([{"chars": 91, "hash": "d8ea107d4a57"}])),
            system_messages: Some(2),
            compact_generations: Some(1),
            summarising: Some(false),
            system_change: Some(json!({"delta": -2})),
            system_ladder: Some(r#"["r1","r2"]"#.to_owned()),
            system_tail: Some(r#"["t1","t2"]"#.to_owned()),
            gate_on: Some(true),
            cold_on: Some(false),
            forced_from: Some("claude-sonnet-4-5".to_owned()),
            forced_to: Some("claude-opus-5".to_owned()),
            downgraded_from: Some("claude-opus-5".to_owned()),
            downgraded_to: Some("claude-sonnet-4-5".to_owned()),
            cache_stripped: Some(true),
            system_merged: Some(false),
            model_mappings: Some(json!([{"from": "a", "to": "b"}])),
            drift_digest: Some("d1f7".to_owned()),
            status: Some(200),
            error_type: Some("none".to_owned()),
            retry_after_ms: Some(0),
            extra: Some(json!({"frontend": "claude"})),
            betas: Some(r#"["claude-code-20250219"]"#.to_owned()),
            geo: Some("us".to_owned()),
            fast: Some(false),
        }
    }

    /// The fixture: a full measurement, a bare one, a blocked row, an
    /// error row on another session, inserted out of time order.
    fn fixture() -> Store {
        let store = Store::open(":memory:").expect("open");
        let mut blocked = bare(T0 + 2_000);
        blocked.kind = Some(RowKind::Blocked);
        blocked.session_id = Some("5a1e0c2d-0000-4000-8000-000000000001".to_owned());
        blocked.extra = Some(json!({"meter": "5h", "resets_at": 1_791_176_400}));
        let mut error = bare(T0 + 3_000);
        error.kind = Some(RowKind::Error);
        error.session_id = Some("b0b0b0b0-0000-4000-8000-000000000002".to_owned());
        error.status = Some(529);
        error.error_type = Some("overloaded_error".to_owned());
        store.record_request(&bare(T0 + 1_000)).expect("bare");
        store.record_request(&full(T0)).expect("full");
        store.record_request(&error).expect("error");
        store.record_request(&blocked).expect("blocked");
        store
    }

    fn export(store: &Store, filter: &RequestFilter) -> String {
        let mut out = Vec::new();
        write_jsonl(store, filter, &mut out).expect("export");
        String::from_utf8(out).expect("utf-8")
    }

    fn lines(text: &str) -> Vec<Value> {
        text.lines()
            .map(|line| serde_json::from_str(line).expect("each line is JSON"))
            .collect()
    }

    #[test]
    fn the_ledger_exports_as_jsonl_oldest_first() {
        insta::assert_snapshot!(export(&fixture(), &RequestFilter::default()));
    }

    #[test]
    fn every_column_is_exported_under_its_own_name() {
        // A file ledger, so a second connection can ask SQLite for the
        // columns: a later migration's column fails here until the full
        // fixture sets it.
        let dir = crate::test_support::tempdir("toker-export-cols-");
        let path = dir.join("toker.db");
        let store = Store::open(&path).expect("open");
        store.record_request(&full(T0)).expect("full");
        let row = lines(&export(&store, &RequestFilter::default())).remove(0);
        let keys: Vec<&str> = row
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        let conn = rusqlite::Connection::open(&path).expect("second connection");
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info('requests')")
            .expect("prepare");
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("columns");
        // `kind` is the one column a measurement leaves NULL: its
        // absence is what makes the row a measurement.
        let mut expected = vec!["ts"];
        expected.extend(columns.iter().map(String::as_str).filter(|c| *c != "kind"));
        assert_eq!(keys, expected, "ts, then every column of requests in order");
        for column in crate::store::JSON_TEXT_COLUMNS {
            assert!(
                columns.iter().any(|c| c == column),
                "{column} names a real column"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn absent_values_are_omitted_never_zeroed() {
        let store = Store::open(":memory:").expect("open");
        store.record_request(&bare(T0)).expect("bare");
        let text = export(&store, &RequestFilter::default());
        assert_eq!(
            text,
            "{\"ts\":\"2026-10-05T00:00:00Z\",\"id\":1,\"ts_ms\":1791158400000}\n"
        );
    }

    #[test]
    fn json_text_columns_nest_and_usage_raw_stays_on_one_line() {
        let store = Store::open(":memory:").expect("open");
        store.record_request(&full(T0)).expect("full");
        let text = export(&store, &RequestFilter::default());
        assert_eq!(
            text.lines().count(),
            1,
            "a pretty usage_raw must not split the row"
        );
        let row = lines(&text).remove(0);
        assert_eq!(row["usage_raw"]["input_tokens"], json!(673));
        assert_eq!(row["betas"], json!(["claude-code-20250219"]));
        assert_eq!(row["rate_limits"]["util5h"], json!(0.42));
        assert_eq!(row["ping"], json!(0), "booleans are exported as stored");
    }

    #[test]
    fn filters_select_by_time_session_and_kind() {
        let store = fixture();
        let ids = |filter: RequestFilter| -> Vec<i64> {
            lines(&export(&store, &filter))
                .iter()
                .map(|row| row["id"].as_i64().expect("id"))
                .collect()
        };
        // Insert order was bare(1), full(2), error(3), blocked(4).
        assert_eq!(ids(RequestFilter::default()), vec![2, 1, 4, 3]);
        assert_eq!(
            ids(RequestFilter {
                since_ms: Some(T0 + 1_000),
                until_ms: Some(T0 + 3_000),
                ..RequestFilter::default()
            }),
            vec![1, 4],
            "since inclusive, until exclusive"
        );
        assert_eq!(
            ids(RequestFilter {
                session_prefix: Some("5a1e".to_owned()),
                ..RequestFilter::default()
            }),
            vec![2, 4],
            "a prefix never matches a row with no session"
        );
        assert_eq!(
            ids(RequestFilter {
                session_prefix: Some("5a1_".to_owned()),
                ..RequestFilter::default()
            }),
            Vec::<i64>::new(),
            "the prefix is literal, not a LIKE pattern"
        );
        let kind = |kind| {
            ids(RequestFilter {
                kind,
                ..RequestFilter::default()
            })
        };
        assert_eq!(kind(KindFilter::Measurement), vec![2, 1]);
        assert_eq!(kind(KindFilter::Proxy), vec![4, 3]);
        assert_eq!(kind(KindFilter::Is(RowKind::Error)), vec![3]);
    }

    #[test]
    fn a_closed_reader_is_a_broken_pipe_not_a_store_failure() {
        struct Closed;
        impl std::io::Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error = write_jsonl(&fixture(), &RequestFilter::default(), &mut Closed)
            .expect_err("the write fails");
        assert!(is_broken_pipe(&error), "{error}");
    }

    #[test]
    fn instants_parse_strictly() {
        let now = T0;
        assert_eq!(parse_instant("2026-10-05T00:00:00Z", now), Ok(T0));
        assert_eq!(parse_instant("2026-10-05T12:00:00+12:00", now), Ok(T0));
        assert_eq!(parse_instant("90m", now), Ok(T0 - 90 * 60_000));
        assert_eq!(parse_instant("2h", now), Ok(T0 - 2 * 3_600_000));
        assert_eq!(parse_instant("3d", now), Ok(T0 - 3 * 86_400_000));
        assert_eq!(parse_instant("1w", now), Ok(T0 - 7 * 86_400_000));
        assert_eq!(parse_instant("0s", now), Ok(T0));
        for garbage in [
            "",
            "h",
            "2",
            "-2h",
            "2.5h",
            "2 h",
            "2H",
            "2y",
            "yesterday",
            "2026-10-05",
            "2026-10-05T00:00:00",
            "99999999999999999999d",
        ] {
            assert!(
                parse_instant(garbage, now).is_err(),
                "{garbage:?} must be refused"
            );
        }
    }

    #[test]
    fn kinds_parse_strictly() {
        assert_eq!(kind_arg("all"), Ok(KindFilter::All));
        assert_eq!(kind_arg("measurement"), Ok(KindFilter::Measurement));
        assert_eq!(kind_arg("proxy"), Ok(KindFilter::Proxy));
        assert_eq!(
            kind_arg("cold-quiet"),
            Ok(KindFilter::Is(RowKind::ColdQuiet))
        );
        assert!(kind_arg("measurements").is_err());
        assert!(kind_arg("Blocked").is_err());
    }
}
