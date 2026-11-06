//! The operator's model map, as the config reads it, and the lexical splice
//! batch bodies take.
//!
//! `[providers.<id>.model_map]` is parsed and validated here, once, at
//! config load. Applying it is policy's job: revision one's generator
//! (`policy::legacy`) turns each map into a candidate-local alias policy, so
//! nothing at request time consults a `ModelMap`. What stays at request time
//! is [`rewrite_model_positions`], which writes the effective models policy
//! chose back into the documented model positions of a `count_tokens` or
//! batch body.
//!
//! Targets are deliberately opaque: a routing rule is not model capability or
//! pricing evidence.
//!
//! **Why a lexical tree, not a re-serialisation** (the predecessor's
//! rationale, kept verbatim): nodes retain source spans so model string
//! tokens are replaced without parsing and serialising unrelated numbers or
//! prompt/tool content. `JSON.parse` alone would round large integers, and
//! toker's IR re-serialisation is only byte-exact for canonical bodies. A
//! rewrite changes no unrelated JSON bytes.

use std::collections::BTreeMap;

use crate::catalog::windows::model_identity;
use crate::policy::family::family_of;

// ── the lexical JSON tree ────────────────────────────────────────────

/// One node of the span-retaining JSON tree: the grammar JSON.parse has
/// already accepted, with the byte spans a mapped model token replaces.
#[derive(Debug, Clone)]
enum Node {
    /// A string token: its full span, quotes included.
    String { start: usize, end: usize },
    /// An object: properties in source order, duplicates included (the
    /// duplicate check is the tree's other job).
    Object { properties: Vec<(String, Node)> },
    /// An array of items in source order.
    Array { items: Vec<Node> },
    /// Anything else — numbers, literals (the predecessor's node keeps
    /// their spans
    /// too; no documented model position is ever one, so the span would
    /// never be read).
    Primitive,
}

/// The scanner state: `at` is a byte offset into `bytes`.
struct Tree<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Tree<'a> {
    fn whitespace(&mut self) {
        while let Some(&byte) = self.bytes.get(self.at) {
            // JS \s semantics: JSON's own whitespace, plus the control
            // characters JS's \s also admits. Valid JSON never carries
            // either inside a value.
            if matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                self.at += 1;
            } else {
                break;
            }
        }
    }

    /// A string token: its span and its decoded value
    /// (escapes decoded, exactly as a JSON parser would).
    fn string(&mut self) -> Option<(usize, usize, String)> {
        let start = self.at;
        self.at += 1; // the opening quote
        while self.at < self.bytes.len() {
            match self.bytes[self.at] {
                b'\\' => self.at += 2,
                byte => {
                    self.at += 1;
                    if byte == b'"' {
                        break;
                    }
                }
            }
        }
        let end = self.at;
        if end > self.bytes.len() {
            return None;
        }
        let value = serde_json::from_slice::<serde_json::Value>(&self.bytes[start..end])
            .ok()?
            .as_str()?
            .to_owned();
        Some((start, end, value))
    }

    fn value(&mut self) -> Option<Node> {
        self.whitespace();
        let start = self.at;
        match *self.bytes.get(self.at)? {
            b'"' => {
                let (start, end, _) = self.string()?;
                Some(Node::String { start, end })
            }
            b'{' => {
                self.at += 1;
                let mut properties = Vec::new();
                self.whitespace();
                while *self.bytes.get(self.at)? != b'}' {
                    let (_start, _end, key) = self.string()?;
                    self.whitespace();
                    self.at += 1; // colon
                    let child = self.value()?;
                    properties.push((key, child));
                    self.whitespace();
                    if *self.bytes.get(self.at)? == b',' {
                        self.at += 1;
                        self.whitespace();
                    }
                }
                self.at += 1;
                Some(Node::Object { properties })
            }
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                self.whitespace();
                while *self.bytes.get(self.at)? != b']' {
                    items.push(self.value()?);
                    self.whitespace();
                    if *self.bytes.get(self.at)? == b',' {
                        self.at += 1;
                        self.whitespace();
                    }
                }
                self.at += 1;
                Some(Node::Array { items })
            }
            _ => {
                while let Some(&byte) = self.bytes.get(self.at) {
                    if matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b',' | b'}' | b']') {
                        break;
                    }
                    self.at += 1;
                }
                (self.at > start).then_some(Node::Primitive)
            }
        }
    }
}

/// The tree over already-valid JSON bytes, or `None` for bytes the
/// grammar rejects (callers validate first, the same way).
fn json_tree(bytes: &[u8]) -> Option<Node> {
    let mut tree = Tree { bytes, at: 0 };
    let root = tree.value()?;
    tree.whitespace();
    (tree.at == bytes.len()).then_some(root)
}

/// A property of an object node, last duplicate winning.
fn property_of<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
    let Node::Object { properties } = node else {
        return None;
    };
    properties
        .iter()
        .rev()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

// ── the policy ───────────────────────────────────────────────────────

/// One mapped target: the opaque upstream id and the canonical selector
/// that matched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapTarget {
    /// The opaque upstream model id — a routing rule is not capability or
    /// pricing evidence.
    pub target: String,
    /// The canonical selector that matched, `model:<id>` or
    /// `family:<name>`.
    pub selector: String,
}

/// A parsed, validated routing policy: exact identities first, families
/// behind them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelMap {
    exact: BTreeMap<String, MapTarget>,
    families: BTreeMap<String, MapTarget>,
}

impl ModelMap {
    /// The policy's (canonical selector, target) pairs — exact
    /// identities first, then families, each entry's selector the
    /// canonical `model:<id>` / `family:<name>` form — the shape the
    /// config's `[providers.<id>.model_map]` tables serialise back to
    /// (the write side of the wire mapping, see
    /// `crate::config::Config::to_file`). Deterministic, per invariant 4.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.exact
            .values()
            .chain(self.families.values())
            .map(|matched| (matched.selector.as_str(), matched.target.as_str()))
    }
}

/// Parse and validate one configured value; `Ok(None)` means disabled.
/// A present but invalid
/// policy is an error, never a silent empty map — a typo'd startup value
/// must fail loudly.
pub fn parse_model_map(raw: &str) -> anyhow::Result<Option<ModelMap>> {
    let invalid = |message: String| anyhow::anyhow!("invalid model map: {message}");
    if raw.trim().is_empty() || raw.trim() == "off" {
        return Ok(None);
    }

    // The tree runs after validation, in the same order the predecessor
    // ran it: JSON that
    // parses, then the raw-key duplicate check only the tree can see
    // (JSON.parse collapses duplicate keys silently, last winning).
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|error| invalid(format!("expected a JSON object ({error})")))?;
    let Some(object) = value.as_object() else {
        return Err(invalid(
            "expected a JSON object of selectors to model IDs".to_owned(),
        ));
    };
    let Some(Node::Object { properties }) = json_tree(raw.as_bytes()) else {
        return Err(invalid(
            "expected a JSON object of selectors to model IDs".to_owned(),
        ));
    };
    let raw_keys: Vec<&str> = properties
        .iter()
        .map(|(key, _): &(String, Node)| key.as_str())
        .collect();
    if let Some(repeated) = raw_keys
        .iter()
        .find(|key| raw_keys.iter().filter(|k| *k == *key).count() > 1)
    {
        return Err(invalid(format!(
            "duplicate canonical selector {}",
            serde_json::to_string(repeated).expect("a bare string serialises")
        )));
    }

    let mut map = ModelMap::default();
    for (raw_key, target_value) in object {
        let split = raw_key.find(':');
        let (kind, selector) = match split {
            None => (raw_key.as_str(), ""),
            Some(at) => (&raw_key[..at], raw_key[at + 1..].trim()),
        };
        if kind != "model" && kind != "family" {
            return Err(invalid(format!(
                "unknown selector type {}",
                serde_json::to_string(kind).expect("a bare string serialises")
            )));
        }
        if selector.is_empty() {
            return Err(invalid(format!("empty {kind} selector")));
        }
        let Some(target) = target_value
            .as_str()
            .filter(|target| !target.trim().is_empty())
        else {
            return Err(invalid(format!(
                "target for {} must be a non-empty string",
                serde_json::to_string(raw_key).expect("a bare string serialises")
            )));
        };
        let target = target.to_owned();

        if kind == "model" {
            let Some(canonical) = model_identity(selector) else {
                return Err(invalid(format!(
                    "empty canonical model selector {}",
                    serde_json::to_string(raw_key).expect("a bare string serialises")
                )));
            };
            let selector = format!("model:{canonical}");
            if map.exact.contains_key(&canonical) {
                return Err(invalid(format!(
                    "duplicate canonical selector {}",
                    serde_json::to_string(&selector).expect("a bare string serialises")
                )));
            }
            map.exact.insert(canonical, MapTarget { target, selector });
            continue;
        }

        // A family selector must name a family, never a model version
        // (a selector with no version segments).
        let Some(family) = family_of(selector).filter(|family| family.version.is_empty()) else {
            return Err(invalid(format!(
                "family selector {} must name a family, not a model version",
                serde_json::to_string(selector).expect("a bare string serialises")
            )));
        };
        let selector = format!("family:{}", family.name);
        if map.families.contains_key(&family.name) {
            return Err(invalid(format!(
                "duplicate canonical selector {}",
                serde_json::to_string(&selector).expect("a bare string serialises")
            )));
        }
        map.families
            .insert(family.name.clone(), MapTarget { target, selector });
    }
    Ok(Some(map))
}

// ── the rewrite ──────────────────────────────────────────────────────

/// Write policy's effective models into the documented Anthropic request
/// model positions: the top-level `model` on `/v1/messages/count_tokens`
/// (and `/v1/messages`), and `requests[].params.model` on
/// `/v1/messages/batches`, one replacement per position in order.
///
/// `None` for an unsupported method or path, a body that is not valid JSON,
/// or a replacement list that does not match the positions found; the caller
/// then refuses rather than send a half-rewritten body. Model string tokens
/// are replaced lexically, so bytes outside those positions survive even
/// when a parsed value could not be represented exactly.
pub fn rewrite_model_positions(
    body: &[u8],
    method: &str,
    url: &str,
    replacements_by_index: &[String],
) -> Option<Vec<u8>> {
    if method != "POST" {
        return None;
    }
    let path = url.split('?').next().unwrap_or(url);
    let top_level = path == "/v1/messages" || path == "/v1/messages/count_tokens";
    let batch = path == "/v1/messages/batches";
    if !top_level && !batch {
        return None;
    }
    std::str::from_utf8(body).ok()?;
    serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let root = json_tree(body)?;
    let mut positions = Vec::new();
    if top_level {
        positions.extend(
            property_of(&root, "model").filter(|model| matches!(model, Node::String { .. })),
        );
    } else if let Some(Node::Array { items }) = property_of(&root, "requests") {
        for request in items {
            if let Some(params) = property_of(request, "params")
                && let Some(model) = property_of(params, "model")
                && matches!(model, Node::String { .. })
            {
                positions.push(model);
            }
        }
    }
    if positions.len() != replacements_by_index.len() {
        return None;
    }
    let mut rewritten = body.to_vec();
    for (position, replacement) in positions.iter().zip(replacements_by_index).rev() {
        let Node::String { start, end } = position else {
            unreachable!("only string model positions were retained")
        };
        let token = serde_json::to_string(&serde_json::Value::String(replacement.clone())).ok()?;
        rewritten.splice(*start..*end, token.bytes());
    }
    Some(rewritten)
}

#[cfg(test)]
mod tests {
    use super::{parse_model_map, rewrite_model_positions};

    fn body(value: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&value).expect("serialise test body")
    }

    #[test]
    fn explicit_batch_replacements_splice_only_model_tokens() {
        let input = br#"{ "requests" : [
            {"params":{"model":"first","temperature":0.7}},
            {"params":{"model":"second"}}
        ], "large": 9007199254740993 }"#;
        let rewritten = rewrite_model_positions(
            input,
            "POST",
            "/v1/messages/batches",
            &["one".to_owned(), "two/two".to_owned()],
        )
        .expect("documented positions");
        let expected = String::from_utf8(input.to_vec())
            .unwrap()
            .replacen(r#""first""#, r#""one""#, 1)
            .replacen(r#""second""#, r#""two/two""#, 1);
        assert_eq!(rewritten, expected.as_bytes());
        assert!(rewrite_model_positions(input, "POST", "/v1/messages/batches", &[]).is_none());
    }

    #[test]
    fn disabled_values_parse_to_no_map() {
        for raw in ["", "   ", "off"] {
            assert!(parse_model_map(raw).expect("disabled parses").is_none());
        }
    }

    #[test]
    fn entries_list_exact_selectors_before_family_selectors_in_canonical_form() {
        for raw in [
            r#"{"family:opus":"family-target","model:claude-opus-4-5-20251101":"exact-target"}"#,
            r#"{"model:claude-opus-4-5-20251101":"exact-target","family:opus":"family-target"}"#,
        ] {
            let map = parse_model_map(raw).expect("parses").expect("enabled");
            assert_eq!(
                map.entries().collect::<Vec<_>>(),
                vec![
                    ("model:claude-opus-4-5", "exact-target"),
                    ("family:opus", "family-target"),
                ],
                "a published snapshot id folds to its dateless selector"
            );
        }
    }

    #[test]
    fn invalid_policies_fail_validation_clearly() {
        // The invalid set the predecessor's tests pinned, message for
        // message.
        for (raw, message) in [
            ("{", "expected a JSON object"),
            ("[]", "expected a JSON object"),
            ("null", "expected a JSON object"),
            (r#"{"route:opus":"x"}"#, "unknown selector type"),
            (r#"{"model:":"x"}"#, "empty model selector"),
            (r#"{"family:":"x"}"#, "empty family selector"),
            (r#"{"family:opus":""}"#, "non-empty string"),
            (r#"{"family:opus":4}"#, "non-empty string"),
            (r#"{"family:opus-4":"x"}"#, "must name a family"),
            (
                r#"{"family:opus":"x","family:opus":"y"}"#,
                "duplicate canonical selector",
            ),
            (
                r#"{"model:claude-opus-4-5":"x","model:claude-opus-4-5-20251101":"y"}"#,
                "duplicate canonical selector",
            ),
            (
                r#"{"family:opus":"x","family:CLAUDE-OPUS":"y"}"#,
                "duplicate canonical selector",
            ),
        ] {
            let error = parse_model_map(raw).expect_err(raw);
            assert!(
                error.to_string().contains(message),
                "{raw:?}: expected {message:?}, got {error}"
            );
        }
    }

    #[test]
    fn a_replacement_changes_no_unrelated_json_bytes() {
        // The lexical splice, not a re-serialisation: the large integer
        // keeps its exact digits and the escape stays escaped.
        let raw = r#"{ "model" : "claude-haiku-4-5", "tool_use":{"input":{"record_id":9007199254740993}}, "escaped":"\u0061" }"#;
        let rewritten = rewrite_model_positions(
            raw.as_bytes(),
            "POST",
            "/v1/messages/count_tokens",
            &["gpt-5.6-luna".to_owned()],
        )
        .expect("documented position");
        let expected = raw.replace("\"claude-haiku-4-5\"", "\"gpt-5.6-luna\"");
        assert_eq!(String::from_utf8(rewritten).expect("utf-8"), expected);
    }

    #[test]
    fn only_the_documented_model_positions_are_visited() {
        let input = body(serde_json::json!({
            "model": "claude-haiku-4-5",
            "messages": [{"role": "user", "content": {"model": "claude-opus-5"}}],
            "tools": [{"name": "lookup", "input_schema": {"model": "claude-opus-5"}}],
        }));
        let rewritten = rewrite_model_positions(
            &input,
            "POST",
            "/v1/messages/count_tokens",
            &["gpt-5.6-luna".to_owned()],
        )
        .expect("documented position");
        let rewritten: serde_json::Value = serde_json::from_slice(&rewritten).expect("parses");
        assert_eq!(rewritten["model"], "gpt-5.6-luna");
        assert_eq!(
            rewritten["messages"][0]["content"]["model"],
            "claude-opus-5"
        );
        assert_eq!(
            rewritten["tools"][0]["input_schema"]["model"],
            "claude-opus-5"
        );

        let batch = body(serde_json::json!({
            "model": "claude-haiku-4-5",
            "requests": [
                {"custom_id": "a", "params": {"model": "claude-haiku-4-5", "messages": []}},
                {"custom_id": "b", "params": {"model": "claude-opus-5", "metadata": {"model": "claude-haiku-4-5"}}},
            ],
        }));
        let rewritten = rewrite_model_positions(
            &batch,
            "POST",
            "/v1/messages/batches",
            &["one".to_owned(), "two".to_owned()],
        )
        .expect("documented positions");
        let rewritten: serde_json::Value = serde_json::from_slice(&rewritten).expect("parses");
        assert_eq!(
            rewritten["model"], "claude-haiku-4-5",
            "the top level is not a batch position"
        );
        assert_eq!(rewritten["requests"][0]["params"]["model"], "one");
        assert_eq!(rewritten["requests"][1]["params"]["model"], "two");
        assert_eq!(
            rewritten["requests"][1]["params"]["metadata"]["model"],
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn malformed_and_unsupported_requests_are_refused() {
        let replacement = ["x".to_owned()];
        let cases: Vec<(Vec<u8>, &str, &str)> = vec![
            (b"not json".to_vec(), "POST", "/v1/messages"),
            (
                body(serde_json::json!({"model": "claude-haiku-4-5"})),
                "GET",
                "/v1/messages",
            ),
            (
                body(serde_json::json!({"model": "claude-haiku-4-5"})),
                "POST",
                "/v1/complete",
            ),
            (
                body(serde_json::json!({"requests": "wrong"})),
                "POST",
                "/v1/messages/batches",
            ),
            (b"5".to_vec(), "POST", "/v1/messages"),
        ];
        for (input, method, url) in cases {
            assert!(
                rewrite_model_positions(&input, method, url, &replacement).is_none(),
                "{method} {url}"
            );
        }
    }

    #[test]
    fn invalid_utf8_is_refused_rather_than_spliced() {
        let mut input = br#"{"model":"claude-haiku-4-5","metadata":""#.to_vec();
        input.push(0xff);
        input.extend_from_slice(br#""}"#);
        assert!(
            rewrite_model_positions(&input, "POST", "/v1/messages", &["x".to_owned()]).is_none()
        );
    }
}
