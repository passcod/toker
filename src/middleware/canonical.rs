//! Semantic middleware over the protocol-neutral request. Wire bytes enter
//! only where a legacy safety guard needs them, never as the mutation target.

use crate::ir::anthropic::Release;
use crate::ir::canonical::{CanonBlock, CanonRole, CanonicalRequest};

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

    use super::{release_marker, strip_release};
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
}
