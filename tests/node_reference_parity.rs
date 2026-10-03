//! The vendored ctp parity contract, dispatched: each case in
//! `tests/fixtures/ctp/node-reference-v1.json` (a verbatim copy of
//! claude-token-proxy's fixture) names one pure decision interface and
//! the exact normalised result. This harness drives the **quota gate's**
//! operations (`quota-decision`, `quota-release`) and the **cold gate's**
//! (`cold-decision`, `compaction-decision`) through the Rust ports and
//! asserts the expected outputs byte-for-value; that is the whole point
//! of having vendored the fixture.
//!
//! The other operations (`usage-presence`, `route-identity`,
//! `awake-decision`) belong to their own units and are skipped here;
//! `node_reference_contract.rs` keeps the fixture itself honest.

use std::fs;
use std::path::Path;

use serde_json::Value;

use toker::ir::Request;
use toker::middleware::cold::{self, ColdDecision};
use toker::middleware::lanes::Ttl;
use toker::middleware::quota::{self, GateDecision, Meter, Meters};
use toker::store::{Allowance, Lane};

fn contract() -> Value {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ctp/node-reference-v1.json");
    let bytes = fs::read(path).expect("vendored contract fixture exists");
    serde_json::from_slice(&bytes).expect("vendored contract fixture is valid JSON")
}

/// An `allowance` object from the fixture (`null`, or
/// `{fiveHour, sevenDay}` reset values) as the session's stored
/// [`Allowance`] rows — the keyed form toker's decide consumes.
fn allowances_of(value: Option<&Value>) -> Vec<Allowance> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, meter) in [("fiveHour", "5h"), ("sevenDay", "7d")] {
        if let Some(reset) = value.get(key).and_then(Value::as_i64) {
            out.push(Allowance {
                session_id: "fixture".to_owned(),
                meter: meter.to_owned(),
                reset_value: reset,
            });
        }
    }
    out
}

#[test]
fn cold_gate_cases_pass_against_the_vendored_contract() {
    let contract = contract();
    let mut cold_cases = 0;
    let mut compaction_cases = 0;

    for case in contract
        .get("cases")
        .and_then(Value::as_array)
        .expect("an ordered cases array")
    {
        let id = case.get("id").and_then(Value::as_str).expect("case id");
        let operation = case
            .get("operation")
            .and_then(Value::as_str)
            .expect("case operation");
        let input = case.get("input").expect("case input");
        let expected = case.get("expected").expect("case expected");

        match operation {
            // decideCold(): notice or forward. The fixture's `now` is
            // epoch milliseconds; the lane carries ctp's `at`/`prompt`/
            // `ttl` shape, remapped onto the store's lane row. No
            // outlook is offered (the fixture pins the cheap phase's
            // verdict, outlook null).
            "cold-decision" => {
                let now_ms = input.get("now").and_then(Value::as_i64).expect("case now");
                let lane = input
                    .get("lane")
                    .filter(|lane| lane.is_object())
                    .map(|lane| Lane {
                        key: "fixture|fixture".to_owned(),
                        session_id: Some("fixture".to_owned()),
                        tools_hash: Some("fixture".to_owned()),
                        updated_ms: lane.get("at").and_then(Value::as_i64).expect("lane at"),
                        prompt_tokens: lane.get("prompt").and_then(Value::as_i64),
                        ttl: match lane.get("ttl").and_then(Value::as_str) {
                            Some("5m") => Some(Ttl::FiveMinutes.as_ms()),
                            _ => Some(Ttl::Hour.as_ms()),
                        },
                        ping: None,
                        noticed_at: lane
                            .get("noticedAt")
                            .and_then(Value::as_f64)
                            .map(|noticed| noticed as i64),
                        forced_from: None,
                        forced_to: None,
                    });
                let summarising = input.get("summarising") == Some(&Value::Bool(true));
                let decision = cold::decide_cold(
                    lane.as_ref(),
                    summarising,
                    cold::DEFAULT_MIN_TOKENS,
                    None,
                    now_ms,
                    None,
                );
                match expected.get("action").and_then(Value::as_str) {
                    Some("forward") => assert_eq!(
                        decision,
                        ColdDecision::Forward,
                        "case {id}: expected forward"
                    ),
                    Some("notice") => {
                        let ColdDecision::Notice {
                            idle_ms,
                            prompt,
                            outlook,
                        } = decision
                        else {
                            panic!("case {id}: expected a notice, got {decision:?}");
                        };
                        assert_eq!(
                            idle_ms,
                            expected
                                .get("idleMs")
                                .and_then(Value::as_i64)
                                .expect("case idleMs"),
                            "case {id}: idle measurement"
                        );
                        assert_eq!(
                            prompt.to_string(),
                            expected
                                .get("prompt")
                                .and_then(Value::as_i64)
                                .expect("case prompt")
                                .to_string(),
                            "case {id}: prompt size"
                        );
                        assert_eq!(
                            expected.get("outlook"),
                            Some(&Value::Null),
                            "case {id}: the fixture pins the no-outlook verdict"
                        );
                        assert_eq!(outlook, None, "case {id}: no outlook was offered");
                    }
                    other => panic!("case {id}: unknown expected action {other:?}"),
                }
                cold_cases += 1;
            }
            // retargetCompaction(), over the fixture's synthetic body (a
            // model, one cache_control breakpoint on a system block, one
            // user message): rewrite or decline, with the effective model
            // and the strip count — exactly the fields node-reference.mjs
            // builds its oracle from.
            "compaction-decision" => {
                let model = input
                    .get("model")
                    .and_then(Value::as_str)
                    .expect("case model");
                let cold = input.get("cold") == Some(&Value::Bool(true));
                let body = serde_json::to_vec(&serde_json::json!({
                    "model": model,
                    "system": [{"cache_control": {"type": "ephemeral"}}],
                    "messages": [{"role": "user", "content": []}],
                }))
                .expect("serialise the fixture body");
                let mut request = Request::parse(&body).expect("the fixture body parses");
                let outcome = cold::retarget_compaction(&mut request, None, cold);
                match expected.get("action").and_then(Value::as_str) {
                    Some("forward") => assert!(
                        outcome.is_none(),
                        "case {id}: expected the rewrite to decline"
                    ),
                    Some("rewrite") => {
                        let outcome = outcome.unwrap_or_else(|| {
                            panic!("case {id}: expected the rewrite to go ahead")
                        });
                        assert_eq!(
                            outcome.to.as_deref(),
                            expected.get("effectiveModel").and_then(Value::as_str),
                            "case {id}: effective model"
                        );
                        assert_eq!(
                            outcome.stripped,
                            expected
                                .get("cacheStripped")
                                .and_then(Value::as_i64)
                                .expect("case cacheStripped") as u64,
                            "case {id}: cache breakpoints dropped"
                        );
                    }
                    other => panic!("case {id}: unknown expected action {other:?}"),
                }
                compaction_cases += 1;
            }
            _ => continue,
        }
    }

    assert!(
        cold_cases >= 1,
        "the fixture carries cold-decision cases; none dispatched"
    );
    assert!(
        compaction_cases >= 1,
        "the fixture carries compaction-decision cases; none dispatched"
    );
}

#[test]
fn quota_gate_cases_pass_against_the_vendored_contract() {
    let contract = contract();
    let mut decision_cases = 0;
    let mut release_cases = 0;

    for case in contract
        .get("cases")
        .and_then(Value::as_array)
        .expect("an ordered cases array")
    {
        let id = case.get("id").and_then(Value::as_str).expect("case id");
        let operation = case
            .get("operation")
            .and_then(Value::as_str)
            .expect("case operation");
        let input = case.get("input").expect("case input");
        let expected = case.get("expected").expect("case expected");

        match operation {
            // decide(): block or forward, and which meter / reset. The
            // fixture's `now` is epoch milliseconds (ctp's Date.now());
            // meter resets are epoch seconds — the Rust port keeps both
            // units. Read per-arm: other operations carry other inputs.
            "quota-decision" => {
                let now_ms = input.get("now").and_then(Value::as_i64).expect("case now");
                let meters = Meters::over(input.get("meters").expect("case meters"));
                let decision =
                    quota::decide(Some(meters), &allowances_of(input.get("allowance")), now_ms);
                match expected.get("action").and_then(Value::as_str) {
                    Some("forward") => assert_eq!(
                        decision,
                        GateDecision::Forward,
                        "case {id}: expected forward"
                    ),
                    Some("block") => {
                        let meter = expected
                            .get("meter")
                            .and_then(Value::as_str)
                            .and_then(Meter::parse)
                            .expect("case {id}: expected meter");
                        let resets_at = expected.get("resetsAt").and_then(Value::as_i64);
                        assert_eq!(
                            decision,
                            GateDecision::Block { meter, resets_at },
                            "case {id}: expected block"
                        );
                    }
                    other => panic!("case {id}: unknown expected action {other:?}"),
                }
                decision_cases += 1;
            }
            // grant_for(): the allowance a release grants right now, then
            // decide() under that allowance — the fixture pins both the
            // grant and the resulting decision.
            "quota-release" => {
                let now_ms = input.get("now").and_then(Value::as_i64).expect("case now");
                let meters = Meters::over(input.get("meters").expect("case meters"));
                let grant = quota::grant_for(Some(meters), now_ms);
                let expected_allowance = expected.get("allowance").expect("case allowance");
                assert_eq!(
                    grant.five_hour,
                    expected_allowance.get("fiveHour").and_then(Value::as_i64),
                    "case {id}: fiveHour grant"
                );
                assert_eq!(
                    grant.seven_day,
                    expected_allowance.get("sevenDay").and_then(Value::as_i64),
                    "case {id}: sevenDay grant"
                );
                let decision = quota::decide(
                    Some(meters),
                    &allowances_of(Some(expected_allowance)),
                    now_ms,
                );
                match expected
                    .get("decision")
                    .and_then(|d| d.get("action"))
                    .and_then(Value::as_str)
                {
                    Some("forward") => assert_eq!(
                        decision,
                        GateDecision::Forward,
                        "case {id}: expected the granted release to forward"
                    ),
                    Some("block") => assert!(
                        matches!(decision, GateDecision::Block { .. }),
                        "case {id}: expected block"
                    ),
                    other => panic!("case {id}: unknown expected decision {other:?}"),
                }
                release_cases += 1;
            }
            // Other units' operations: not this harness's to dispatch.
            _ => continue,
        }
    }

    // The dispatch must have found its case sets — a renamed operation or
    // a restructured fixture must fail loudly here, not silently pass.
    assert!(
        decision_cases >= 1,
        "the fixture carries quota-decision cases; none dispatched"
    );
    assert!(
        release_cases >= 1,
        "the fixture carries quota-release cases; none dispatched"
    );
}
