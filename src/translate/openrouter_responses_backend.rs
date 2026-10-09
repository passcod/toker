//! OpenRouter's live-verified Responses subset. The wire resembles Codex's
//! Responses dialect, but a Codex-owned extension is not automatically an
//! OpenRouter capability. Reuse the typed item renderer only after removing
//! unverified opaque fields, with content-free losses for every omission.

use serde_json::{Value, json};

use crate::ir::canonical::{CanonBlock, CanonicalExtension, CanonicalRequest};
use crate::routing::DialectId;

use super::{Rendered, TranslateError, TranslationLoss, TranslationLossReason, TranslationReport};

pub fn render_openrouter_responses(
    canonical: &CanonicalRequest,
    had_prompt_cache_key: bool,
) -> Result<Rendered<Value>, TranslateError> {
    let mut safe = canonical.clone();
    let mut report = TranslationReport::default();
    if had_prompt_cache_key {
        report.push(TranslationLoss::new(
            "$.prompt_cache_key",
            TranslationLossReason::UnsupportedByBinding,
            1,
        ));
    }
    let mut max_output_tokens = safe.sampling.max_tokens.take();
    for extension in std::mem::take(&mut safe.extensions) {
        match (extension.wire_path(), extension.wire_name()) {
            ("$.max_output_tokens", Some("max_output_tokens")) => {
                let Some(limit) = extension.value().as_u64().filter(|limit| *limit >= 16) else {
                    return Err(TranslateError::Malformed {
                        reason: "max_output_tokens must be an integer of at least 16".to_owned(),
                    });
                };
                max_output_tokens = Some(limit);
            }
            ("$.store", Some("store")) if extension.value() == &Value::Bool(false) => {}
            ("$.store", Some("store")) => {
                return Err(TranslateError::Malformed {
                    reason: "OpenRouter Responses binding has not verified store:true".to_owned(),
                });
            }
            ("$.tools", Some("tools")) => {
                return Err(TranslateError::Malformed {
                    reason: "OpenRouter Responses binding cannot safely omit unknown tools"
                        .to_owned(),
                });
            }
            _ => report_loss(&extension, &mut report),
        }
    }
    for part in &mut safe.system {
        if let crate::ir::canonical::CanonSystemPart::Text { extensions, .. } = part {
            strip_extensions(extensions, &mut report);
        }
    }
    for message in &mut safe.messages {
        if message.blocks.is_empty() && !message.extensions.is_empty() {
            return Err(TranslateError::UnsupportedBlock {
                kind: "provider-owned Responses input item".to_owned(),
            });
        }
        strip_extensions(&mut message.extensions, &mut report);
        for block in &mut message.blocks {
            strip_block(block, &mut report)?;
        }
    }
    for tool in &mut safe.tools {
        tool.extensions.retain(|extension| {
            if extension.wire_name() == Some("strict")
                && extension.source() == DialectId::CodexResponses
                && extension.value().is_boolean()
            {
                true
            } else {
                report_loss(extension, &mut report);
                false
            }
        });
    }

    if safe.thinking.take().is_some() {
        report.push(TranslationLoss::new(
            "thinking",
            TranslationLossReason::UnsupportedByBinding,
            1,
        ));
    }
    let rendered = super::codex_backend::render_codex(&safe, "")?;
    for loss in rendered.report.losses() {
        report.push(loss.clone());
    }
    let mut value =
        serde_json::to_value(rendered.value).expect("a typed Responses request always serialises");
    let body = value
        .as_object_mut()
        .expect("a Responses request is an object");
    body.remove("prompt_cache_key");
    body.remove("parallel_tool_calls");
    if let Some(limit) = max_output_tokens {
        body.insert("max_output_tokens".to_owned(), json!(limit));
    }
    body.remove("include");
    body.remove("reasoning");
    // The binding was verified with stateless streamed turns. The frontend
    // can still ask for JSON; the handler aggregates the backend stream.
    body.insert("stream".to_owned(), Value::Bool(true));
    body.insert("store".to_owned(), Value::Bool(false));
    Ok(Rendered { value, report })
}

fn strip_block(
    block: &mut CanonBlock,
    report: &mut TranslationReport,
) -> Result<(), TranslateError> {
    match block {
        CanonBlock::Image { .. } => Err(TranslateError::UnsupportedBlock {
            kind: "image on unverified OpenRouter Responses binding".to_owned(),
        }),
        CanonBlock::Annotated {
            block, extensions, ..
        } => {
            strip_extensions(extensions, report);
            strip_block(block, report)
        }
        _ => Ok(()),
    }
}

fn strip_extensions(extensions: &mut Vec<CanonicalExtension>, report: &mut TranslationReport) {
    for extension in extensions.drain(..) {
        report_loss(&extension, report);
    }
}

fn report_loss(extension: &CanonicalExtension, report: &mut TranslationReport) {
    report.push(TranslationLoss::new(
        extension.wire_path(),
        TranslationLossReason::IncompatibleExtensionDialect,
        1,
    ));
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::render_openrouter_responses;
    use crate::translate::from_openai_responses;

    fn render(value: Value) -> super::Rendered<Value> {
        let canonical = from_openai_responses(&value).expect("valid Responses input");
        render_openrouter_responses(&canonical, value.get("prompt_cache_key").is_some())
            .expect("supported subset")
    }

    #[test]
    fn renders_only_verified_request_fields_and_reports_opaque_omissions() {
        let rendered = render(json!({
            "model":"openai/gpt-4.1-mini",
            "input":"hello",
            "max_output_tokens":64,
            "store":false,
            "include":["reasoning.encrypted_content"],
            "prompt_cache_key":"session-secret",
            "stream":false
        }));
        assert_eq!(rendered.value["max_output_tokens"], 64);
        assert_eq!(rendered.value["stream"], true);
        assert_eq!(rendered.value["store"], false);
        for name in [
            "include",
            "prompt_cache_key",
            "parallel_tool_calls",
            "reasoning",
        ] {
            assert!(rendered.value.get(name).is_none(), "{name} must not leak");
        }
        let paths: Vec<_> = rendered
            .report
            .losses()
            .iter()
            .map(|loss| loss.path())
            .collect();
        assert!(paths.contains(&"$.include"), "{paths:?}");
        assert!(paths.contains(&"$.prompt_cache_key"), "{paths:?}");
    }

    #[test]
    fn rejects_store_true_and_unknown_tools_before_upstream() {
        for value in [
            json!({"model":"x", "input":"hi", "store":true}),
            json!({"model":"x", "input":"hi", "tools":[{"type":"web_search_preview"}]}),
        ] {
            let canonical = from_openai_responses(&value).expect("opaque input stays canonical");
            assert!(render_openrouter_responses(&canonical, false).is_err());
        }
    }

    #[test]
    fn appending_a_turn_preserves_the_rendered_input_prefix() {
        let first = json!({
            "model":"openai/gpt-4.1-mini",
            "input":[{"role":"user","content":"one"}]
        });
        let mut second = first.clone();
        second["input"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"assistant","content":"two"}));
        let first = render(first).value["input"].as_array().unwrap().clone();
        let second = render(second).value["input"].as_array().unwrap().clone();
        assert_eq!(&second[..first.len()], first);
    }
}
