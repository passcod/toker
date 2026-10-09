//! Semantic middleware over the protocol-neutral request. Wire bytes enter
//! only where a legacy safety guard needs them, never as the mutation target.

use crate::ir::anthropic::Release;
use crate::ir::canonical::{
    CanonBlock, CanonMessage, CanonRole, CanonSystemPart, CanonicalExtension, CanonicalRequest,
    ToolResultContent,
};
use crate::middleware::cold::{self, RetargetOutcome};
use crate::middleware::model_map::{self, MapTarget, ModelMap};
use crate::routing::DialectId;
use serde_json::Value;

/// Apply a configured provider map to the one inference model position.
/// Batches and count-token bodies are administrative and keep their lexical
/// mapper. A same-identity match still returns its provenance for the row.
pub fn map_model(request: &mut CanonicalRequest, policy: Option<&ModelMap>) -> Option<MapTarget> {
    let matched = model_map::mapped_model(policy, request.model.as_deref()?)?.clone();
    request.model = Some(matched.target.clone());
    Some(matched)
}

/// Remove mid-conversation effort updates for a binding that rejects them.
/// The wire-side content shape decides whether an effort-only system message
/// disappears: string whitespace and an empty block array were distinct in
/// the old rule, and canonical text normalization must not guess between them.
pub fn strip_message_effort(request: &mut CanonicalRequest, source: &Value) -> bool {
    let source_messages = source.get("messages").and_then(Value::as_array);
    let mut changed = false;
    let mut kept = Vec::with_capacity(request.messages.len());
    for (index, mut message) in std::mem::take(&mut request.messages)
        .into_iter()
        .enumerate()
    {
        if message.role != CanonRole::System {
            kept.push(message);
            continue;
        }
        let before = message.extensions.len();
        message.extensions.retain(|extension| {
            !(extension.source() == DialectId::AnthropicMessages
                && extension.wire_name() == Some("output_config"))
        });
        if message.extensions.len() == before {
            kept.push(message);
            continue;
        }
        changed = true;
        let content = source_messages
            .and_then(|messages| messages.get(index))
            .and_then(|message| message.get("content"));
        let retain = match content {
            None | Some(Value::Null) => false,
            Some(Value::String(text)) => !text.trim().is_empty(),
            Some(Value::Array(blocks)) => !blocks.is_empty(),
            Some(_) => true,
        };
        if retain {
            kept.push(message);
        }
    }
    request.messages = kept;
    changed
}

/// The cold-compaction rewrite over canonical nodes. Work on a clone and
/// publish only after every system message has a safe preceding user host;
/// no partial prompt rewrite can escape on a shape this middleware cannot
/// interpret. Cache controls are removed recursively, including opaque
/// extension values, just as the previous whole-JSON traversal did.
pub fn retarget_compaction(
    request: &mut CanonicalRequest,
    target: Option<&str>,
    cold_lane: bool,
    map: Option<&ModelMap>,
) -> Option<RetargetOutcome> {
    if request.messages.is_empty() {
        return None;
    }
    let from = request.model.clone();
    let cheaper = cold::cheaper_of(from.as_deref(), target, map);
    if cheaper.is_none() && !cold_lane {
        return None;
    }
    let mut candidate = request.clone();
    let mut messages: Vec<CanonMessage> = Vec::with_capacity(candidate.messages.len());
    let mut merged = 0u64;
    for message in std::mem::take(&mut candidate.messages) {
        if message.role != CanonRole::System {
            messages.push(message);
            continue;
        }
        let host = messages.last_mut()?;
        if host.role != CanonRole::User {
            return None;
        }
        let joined = message
            .blocks
            .iter()
            .map(|block| match block.semantic() {
                CanonBlock::Text(text) => text.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = joined.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}');
        if text.is_empty() {
            return None;
        }
        host.blocks
            .push(CanonBlock::Text(format!("{} {text}", cold::MERGED_SYSTEM)));
        merged += 1;
    }
    candidate.messages = messages;
    candidate.model = cheaper.or_else(|| from.clone());
    let stripped = strip_cache_controls(&mut candidate);
    let to = candidate.model.clone();
    *request = candidate;
    Some(RetargetOutcome {
        from,
        to,
        merged,
        stripped,
    })
}

fn strip_cache_controls(request: &mut CanonicalRequest) -> u64 {
    let mut count = strip_extensions(&mut request.extensions);
    for part in &mut request.system {
        count += match part {
            CanonSystemPart::Text { extensions, .. } => strip_extensions(extensions),
            CanonSystemPart::Opaque(extension) => strip_cache_value(extension.value_mut()),
        };
    }
    for message in &mut request.messages {
        count += strip_extensions(&mut message.extensions);
        for block in &mut message.blocks {
            count += strip_block_cache_controls(block);
        }
    }
    for tool in &mut request.tools {
        count += strip_cache_value(&mut tool.parameters);
        count += strip_extensions(&mut tool.extensions);
    }
    count
}

fn strip_block_cache_controls(block: &mut CanonBlock) -> u64 {
    match block {
        CanonBlock::Annotated {
            block, extensions, ..
        } => strip_extensions(extensions) + strip_block_cache_controls(block),
        CanonBlock::ToolUse { input, .. } => strip_cache_value(input),
        CanonBlock::ToolResult { content, .. } => match content {
            ToolResultContent::String(_) => 0,
            ToolResultContent::Blocks(blocks) => {
                blocks.iter_mut().map(strip_block_cache_controls).sum()
            }
        },
        _ => 0,
    }
}

fn strip_extensions(extensions: &mut Vec<CanonicalExtension>) -> u64 {
    let before = extensions.len();
    extensions.retain(|extension| extension.wire_name() != Some("cache_control"));
    let mut count = (before - extensions.len()) as u64;
    for extension in extensions {
        count += strip_cache_value(extension.value_mut());
    }
    count
}

fn strip_cache_value(value: &mut Value) -> u64 {
    match value {
        Value::Array(values) => values.iter_mut().map(strip_cache_value).sum(),
        Value::Object(object) => {
            let mut count = u64::from(object.remove("cache_control").is_some());
            count += object.values_mut().map(strip_cache_value).sum::<u64>();
            count
        }
        _ => 0,
    }
}

/// A release belongs to the last user message's first text block. A tool
/// continuation carrying an earlier marker cannot grant a second allowance.
pub fn release_marker(request: &CanonicalRequest) -> Option<Release> {
    let text = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == CanonRole::User)?
        .blocks
        .iter()
        .find_map(|block| match block.semantic() {
            CanonBlock::Text(text) => Some(text.as_str()),
            _ => None,
        })?;
    [Release::Overage, Release::Plan]
        .into_iter()
        .find(|release| text.starts_with(release.marker()))
}

/// Strip each marker only if all mutation sites agree with the raw scan.
/// This retains the old ambiguity guard while the actual edit is canonical:
/// an echoed marker or an empty-after-strip text leaves the turn untouched.
pub fn strip_release(request: &mut CanonicalRequest, original_bytes: &[u8]) -> bool {
    let mut changed = false;
    for release in [Release::Overage, Release::Plan] {
        let marker = release.marker();
        let targets = request
            .messages
            .iter()
            .filter(|message| message.role == CanonRole::User)
            .flat_map(|message| &message.blocks)
            .filter_map(|block| match block.semantic() {
                CanonBlock::Text(text) => Some(text),
                _ => None,
            })
            .filter(|text| strippable(text, marker))
            .count();
        if targets == 0 {
            continue;
        }
        let needle = format!("\"{marker}");
        let hits = original_bytes
            .windows(needle.len())
            .filter(|window| *window == needle.as_bytes())
            .count();
        if hits != targets {
            continue;
        }
        for message in &mut request.messages {
            if message.role != CanonRole::User {
                continue;
            }
            for block in &mut message.blocks {
                if let Some(text) = semantic_text_mut(block)
                    && strippable(text, marker)
                {
                    *text = text[marker.len()..].to_owned();
                    changed = true;
                }
            }
        }
    }
    changed
}

fn semantic_text_mut(block: &mut CanonBlock) -> Option<&mut String> {
    match block {
        CanonBlock::Text(text) => Some(text),
        CanonBlock::Annotated { block, .. } => semantic_text_mut(block),
        _ => None,
    }
}

fn strippable(text: &str, marker: &str) -> bool {
    text.strip_prefix(marker).is_some_and(|rest| {
        !rest
            .trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
            .is_empty()
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{release_marker, retarget_compaction, strip_message_effort, strip_release};
    use crate::ir::Request;
    use crate::ir::anthropic::{Release, SENTINEL};
    use crate::routing::DialectId;
    use crate::translate::{from_anthropic, render_anthropic};

    #[test]
    fn release_and_strip_match_the_wire_view_on_safe_and_ambiguous_turns() {
        for value in [
            json!({"model":"m","max_tokens":64,"messages":[{"role":"user","content":[{"type":"text","text":format!("{SENTINEL} continue") }]}]}),
            json!({"model":"m","max_tokens":64,"messages":[{"role":"user","content":[{"type":"text","text":format!("{SENTINEL} continue")}]},{"role":"assistant","content":[{"type":"text","text":format!("{SENTINEL} echoed") }]}]}),
            json!({"model":"m","max_tokens":64,"messages":[{"role":"user","content":[{"type":"text","text":SENTINEL}]}]}),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            let mut legacy = Request::parse(&bytes).unwrap();
            let mut canonical = from_anthropic(legacy.value()).unwrap();
            assert_eq!(
                release_marker(&canonical),
                legacy.anthropic().release_marker()
            );
            let changed = strip_release(&mut canonical, &bytes);
            legacy.anthropic_mut().strip_release();
            let rendered = render_anthropic(&canonical, DialectId::AnthropicMessages)
                .unwrap()
                .value;
            assert_eq!(rendered["messages"], legacy.value()["messages"]);
            assert_eq!(changed, rendered["messages"] != value["messages"]);
        }
        assert_eq!(Release::Overage.marker(), SENTINEL);
    }

    #[test]
    fn effort_strip_matches_the_wire_rule_for_empty_and_text_system_messages() {
        let value = json!({
            "model":"m", "max_tokens":64,
            "messages":[
                {"role":"user","content":"start"},
                {"role":"system","output_config":{"effort":"low"}},
                {"role":"system","output_config":{"effort":"high"},"content":"  "},
                {"role":"system","output_config":{"effort":"high"},"content":[]},
                {"role":"system","output_config":{"effort":"high"},"content":"keep"},
                {"role":"assistant","content":"answer"}
            ]
        });
        let bytes = serde_json::to_vec(&value).unwrap();
        let mut legacy = Request::parse(&bytes).unwrap();
        let mut canonical = from_anthropic(&value).unwrap();
        assert!(strip_message_effort(&mut canonical, &value));
        assert!(legacy.anthropic_mut().strip_message_effort());
        let rendered = render_anthropic(&canonical, DialectId::AnthropicMessages)
            .unwrap()
            .value;
        assert_eq!(
            from_anthropic(&rendered).unwrap().messages,
            from_anthropic(legacy.value()).unwrap().messages,
        );
    }

    #[test]
    fn cold_retarget_matches_legacy_semantics_and_declines_unsafe_merge() {
        for value in [
            json!({
                "model":"m", "max_tokens":64,
                "system":[{"type":"text","text":"preface","cache_control":{"type":"ephemeral"}}],
                "tools":[{"name":"echo","description":"","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}],
                "messages":[
                    {"role":"user","content":[{"type":"text","text":"start","cache_control":{"type":"ephemeral"}}]},
                    {"role":"system","content":"new instruction"},
                    {"role":"assistant","content":[{"type":"text","text":"done"}]}
                ]
            }),
            json!({
                "model":"m", "max_tokens":64,
                "messages":[{"role":"assistant","content":"start"},{"role":"system","content":"unsafe"}]
            }),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            let mut legacy = Request::parse(&bytes).unwrap();
            let mut canonical = from_anthropic(&value).unwrap();
            let legacy_outcome =
                crate::middleware::cold::retarget_compaction(&mut legacy, None, true, None);
            let before = canonical.clone();
            let canonical_outcome = retarget_compaction(&mut canonical, None, true, None);
            assert_eq!(canonical_outcome, legacy_outcome);
            if legacy_outcome.is_some() {
                let rendered = render_anthropic(&canonical, DialectId::AnthropicMessages)
                    .unwrap()
                    .value;
                assert_eq!(
                    from_anthropic(&rendered).unwrap(),
                    from_anthropic(legacy.value()).unwrap(),
                );
            } else {
                assert_eq!(
                    canonical, before,
                    "a refused retarget leaves the input untouched"
                );
            }
        }
    }
}
