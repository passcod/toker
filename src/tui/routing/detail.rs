//! What a preview result says beyond its raw body.

use serde_json::Value;

use super::data::{array, text};

/// What a preview that asked for server safeguards says about them: each
/// candidate the walk looked at with its binding's effective support, the
/// ones skipped for lacking it, and whether the route was walked again
/// without the requirement. Only the three support words and ids are shown,
/// never anything the request carried.
///
/// Empty where the body is not a preview result with a walk in it.
pub(super) fn preview_safeguards(body: &Value) -> Vec<String> {
    let result = body.get("result").unwrap_or(body);
    let trace = array(result, "trace");
    let mut lines = Vec::new();
    for event in trace {
        match text(event, "kind") {
            "candidate" => {
                let support = event
                    .get("server_safeguards")
                    .and_then(Value::as_str)
                    .unwrap_or("unreported");
                let skipped = match event.pointer("/result/Err").and_then(Value::as_str) {
                    Some("LacksServerSafeguards") => {
                        "  skipped: lacks server safeguards".to_owned()
                    }
                    Some(other) => format!("  skipped: {other}"),
                    None => String::new(),
                };
                lines.push(format!(
                    "  {}  {}  {support}{skipped}",
                    text(event, "candidate_id"),
                    text(event, "binding_id"),
                ));
            }
            "safeguards_walked_again" => {
                lines.push("  walked again without the requirement".to_owned());
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        return lines;
    }
    lines.insert(0, "SERVER SAFEGUARDS asked for".to_owned());
    if let Some(serving) = result.get("server_safeguards")
        && let Some(effective) = serving.get("effective").and_then(Value::as_str)
    {
        let marked = if serving.get("marked") == Some(&Value::Bool(true)) {
            ", rejection mark held"
        } else {
            ""
        };
        lines.push(format!("  serving binding: {effective}{marked}"));
    }
    lines.push(String::new());
    lines
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::preview_safeguards;

    fn walked_preview() -> serde_json::Value {
        json!({"toker": "routing-preview", "ok": true, "result": {
            "outcome": "selected",
            "server_safeguards": {"asked": true, "effective": "unknown", "marked": false,
                                  "skipped_candidate": true, "walked_again": true},
            "trace": [
                {"kind": "lookup", "provenance": "ProtocolDefault"},
                {"kind": "candidate", "candidate_id": "sub", "binding_id": "codex_sub.codex_responses",
                 "server_safeguards": "unsupported", "result": {"Err": "LacksServerSafeguards"}},
                {"kind": "candidate", "candidate_id": "billed", "binding_id": "openrouter.anthropic_messages",
                 "server_safeguards": "unknown", "result": {"Err": "HealthCircuitOpen"}},
                {"kind": "safeguards_walked_again"},
                {"kind": "candidate", "candidate_id": "sub", "binding_id": "codex_sub.codex_responses",
                 "server_safeguards": "unsupported", "result": {"Ok": null}},
            ],
        }})
    }

    #[test]
    fn a_preview_lists_each_candidate_s_support_and_the_second_walk() {
        let text = preview_safeguards(&walked_preview()).join("\n");
        insta::assert_snapshot!("preview_safeguards_walked_again", text);
    }

    #[test]
    fn a_preview_with_no_walk_in_it_has_no_safeguards_section() {
        assert!(preview_safeguards(&json!({"kind": "validation", "valid": true})).is_empty());
        assert!(
            preview_safeguards(&json!({"result": {"outcome": "unresolved", "code": "no_route"}}))
                .is_empty()
        );
    }
}
