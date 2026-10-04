//! The model routing map (plan: Middleware — "Model routing map":
//! deliberate, once-per-change byte edits): an optional operator routing
//! policy mapping the models clients ask for onto the identities a
//! compatibility upstream will actually receive.
//!
//! A faithful port of ctp's `model-map.mjs`, measured in production —
//! ported, not improved. Targets are deliberately opaque: a routing rule
//! is not model capability or pricing evidence. Parsing and matching live
//! here so the caller only decides when the final rewrite runs.
//!
//! **Status:** the pure decision interface, ported and pinned by the
//! vendored `route-identity` parity case (ctp `fixtures/node-reference-v1.
//! json` via `tests/node_reference_parity.rs`). Config and the server's
//! final routing stage are the model-routing unit's; until it lands,
//! [`crate::middleware::force_newest`] consults nothing here (ctp previews
//! the map before its served-recency lookup — with no map configured that
//! preview is the identity, which is what the force decision assumes).
//!
//! **Why a lexical tree, not a re-serialisation** (ctp model-map.mjs:11-13,
//! kept verbatim): nodes retain source spans so mapped model string tokens
//! are replaced without parsing and serialising unrelated numbers or
//! prompt/tool content — `JSON.parse` alone would round large integers,
//! and toker's IR re-serialisation is only byte-exact for canonical
//! bodies. A mapping changes no unrelated JSON bytes.

use std::collections::BTreeMap;

use crate::catalog::windows::model_identity;
use crate::middleware::models::family_of;

// ── the lexical JSON tree (ctp jsonTree, model-map.mjs:14-66) ─────────────

/// One node of the span-retaining JSON tree: the grammar JSON.parse has
/// already accepted, with the byte spans a mapped model token replaces.
#[derive(Debug, Clone)]
enum Node {
    /// A string token: its full span (quotes included) and decoded value.
    String {
        start: usize,
        end: usize,
        value: String,
    },
    /// An object: properties in source order, duplicates included (the
    /// duplicate check is the tree's other job).
    Object { properties: Vec<(String, Node)> },
    /// An array of items in source order.
    Array { items: Vec<Node> },
    /// Anything else — numbers, literals (ctp's node keeps their spans
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
            // ctp's /\s/: JSON's own whitespace, plus the control
            // characters JS's \s also admits. Valid JSON never carries
            // either inside a value.
            if matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                self.at += 1;
            } else {
                break;
            }
        }
    }

    /// A string token: its span and its decoded value (ctp's
    /// `JSON.parse(text.slice(start, end))` on the token — escapes and
    /// all).
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
                let (start, end, value) = self.string()?;
                Some(Node::String { start, end, value })
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
/// grammar rejects (ctp calls the tree only after `JSON.parse` has
/// accepted the text; callers here validate first the same way).
fn json_tree(bytes: &[u8]) -> Option<Node> {
    let mut tree = Tree { bytes, at: 0 };
    let root = tree.value()?;
    tree.whitespace();
    (tree.at == bytes.len()).then_some(root)
}

/// A property of an object node, last duplicate winning (ctp's
/// `findLast`).
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

// ── the policy (ctp parseModelMap / mappedModel / previewMappedModel) ─────

/// One mapped target: the opaque upstream id and the canonical selector
/// that matched it (ctp's `{ target, selector }` map values).
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
/// behind them (ctp's `{ exact, families }`).
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

/// Parse and validate one configured value; `Ok(None)` means disabled
/// (ctp `parseModelMap`, model-map.mjs:74-129). A present but invalid
/// policy is an error, never a silent empty map — a typo'd startup value
/// must fail loudly.
pub fn parse_model_map(raw: &str) -> anyhow::Result<Option<ModelMap>> {
    let invalid = |message: String| anyhow::anyhow!("invalid model map: {message}");
    if raw.trim().is_empty() || raw.trim() == "off" {
        return Ok(None);
    }

    // The tree runs after validation, exactly as ctp orders it: JSON that
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
        // (ctp: `familyOf(selector)` with no version segments).
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

/// Select an opaque upstream target, exact identity ahead of family (ctp
/// `mappedModel`, model-map.mjs:132-139). `None` when there is no policy
/// or no match.
pub fn mapped_model<'a>(policy: Option<&'a ModelMap>, model: &str) -> Option<&'a MapTarget> {
    let policy = policy?;
    if model.is_empty() {
        return None;
    }
    if let Some(id) = model_identity(model)
        && let Some(exact) = policy.exact.get(&id)
    {
        return Some(exact);
    }
    let family = family_of(model)?.name;
    policy.families.get(&family)
}

/// Side-effect-free effective identity for pre-flight decisions (ctp
/// `previewMappedModel`, model-map.mjs:143-144): the mapped target when
/// one matches, the model unchanged otherwise.
pub fn preview_mapped_model<'a>(policy: Option<&'a ModelMap>, model: &'a str) -> Option<&'a str> {
    mapped_model(policy, model)
        .map(|matched| matched.target.as_str())
        .or(Some(model))
}

// ── the rewrite (ctp rewriteMappedModels, model-map.mjs:163-237) ─────────

/// One matched model position (ctp's `models` entries): where the model
/// token sat, what it was, and what the upstream will receive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedPosition {
    /// The batch request index (`requests[].params.model`), or `None` for
    /// the top-level position.
    pub request_index: Option<usize>,
    /// The model as the client asked for it.
    pub pre_map_model: String,
    /// The model the upstream receives.
    pub effective_model: String,
    /// Whether a selector matched this position.
    pub matched: bool,
    /// The canonical selector that matched, when one did.
    pub selector: Option<String>,
}

/// The result of applying (or not applying) the map to one request body
/// (ctp `rewriteMappedModels`'s return): the possibly-rewritten body, and
/// the routing identity contract the row records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedRewrite {
    /// The body to send upstream: the original bytes when nothing
    /// changed, the spliced bytes when a model token was replaced.
    pub body: Vec<u8>,
    /// Whether any bytes changed (a matched no-op target keeps the bytes
    /// and still reports the effective model).
    pub changed: bool,
    /// Whether any selector matched at all (distinct from `changed`).
    pub mapped: bool,
    /// The top-level position's asked model, when there was one.
    pub pre_map_model: Option<String>,
    /// The top-level position's effective model, when there was one.
    pub effective_model: Option<String>,
    /// Every matched position, in order.
    pub models: Vec<MappedPosition>,
}

/// The unchanged result (ctp's `unchanged` helper).
fn unchanged(body: &[u8]) -> MappedRewrite {
    MappedRewrite {
        body: body.to_vec(),
        changed: false,
        mapped: false,
        pre_map_model: None,
        effective_model: None,
        models: Vec::new(),
    }
}

/// Apply the map only to documented Anthropic request model positions
/// (ctp `rewriteMappedModels`, model-map.mjs:163-237): the top-level
/// `model` on `/v1/messages` and `/v1/messages/count_tokens`, and
/// `requests[].params.model` on `/v1/messages/batches`.
///
/// Disabled, unsupported, malformed, and unmatched requests retain the
/// exact original bytes. Matching model string tokens are replaced
/// lexically, so bytes outside those documented positions survive even
/// when a parsed value could not be represented exactly — a mapping
/// changes no unrelated JSON bytes.
pub fn rewrite_mapped_models(
    policy: Option<&ModelMap>,
    body: &[u8],
    method: &str,
    url: &str,
) -> MappedRewrite {
    if policy.is_none() || method != "POST" {
        return unchanged(body);
    }
    let path = url.split('?').next().unwrap_or(url);
    let top_level = path == "/v1/messages" || path == "/v1/messages/count_tokens";
    let batch = path == "/v1/messages/batches";
    if !top_level && !batch {
        return unchanged(body);
    }

    // Validate before the span parser relies on the grammar, and reject
    // invalid UTF-8 byte-exactly (a lossy decode would splice garbage).
    let Ok(text) = std::str::from_utf8(body) else {
        return unchanged(body);
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        return unchanged(body);
    };
    if !parsed.is_object() {
        return unchanged(body);
    }
    let Some(root) = json_tree(body) else {
        return unchanged(body);
    };

    // The documented model positions, in order (ctp model-map.mjs:181-195).
    #[derive(Clone, Copy)]
    struct Position<'a> {
        model: &'a Node,
        request_index: Option<usize>,
    }
    let mut positions: Vec<Position<'_>> = Vec::new();
    if top_level {
        if let Some(model) = property_of(&root, "model")
            && matches!(model, Node::String { .. })
        {
            positions.push(Position {
                model,
                request_index: None,
            });
        }
    } else if let Some(Node::Array { items }) = property_of(&root, "requests") {
        for (request_index, request) in items.iter().enumerate() {
            if let Some(params) = property_of(request, "params")
                && let Some(model) = property_of(params, "model")
                && matches!(model, Node::String { .. })
            {
                positions.push(Position {
                    model,
                    request_index: Some(request_index),
                });
            }
        }
    }

    let mut models = Vec::new();
    let mut replacements = Vec::new();
    let mut any_matched = false;
    for position in &positions {
        let Node::String {
            start,
            end,
            value: pre_map_model,
        } = position.model
        else {
            continue; // the grammar put something else where a model goes
        };
        let matched = mapped_model(policy, pre_map_model);
        let effective_model = matched
            .map(|matched| matched.target.clone())
            .unwrap_or_else(|| pre_map_model.clone());
        let was_matched = matched.is_some();
        let changed = was_matched && effective_model != *pre_map_model;
        models.push(MappedPosition {
            request_index: position.request_index,
            pre_map_model: pre_map_model.clone(),
            effective_model: effective_model.clone(),
            matched: was_matched,
            selector: matched.map(|matched| matched.selector.clone()),
        });
        any_matched |= was_matched;
        if changed {
            // ctp's `JSON.stringify(effectiveModel)`: the token with its
            // quotes, minimally escaped.
            let token = serde_json::to_string(&serde_json::Value::String(effective_model))
                .expect("a bare string serialises");
            replacements.push((*start, *end, token));
        }
    }

    // Splice from the end so earlier spans never shift.
    let changed = !replacements.is_empty();
    let mut rewritten = body.to_vec();
    for (start, end, token) in replacements.iter().rev() {
        rewritten.splice(start..end, token.bytes());
    }

    let single = if top_level { models.first() } else { None };
    MappedRewrite {
        body: if changed { rewritten } else { body.to_vec() },
        changed,
        mapped: any_matched,
        pre_map_model: single.map(|single| single.pre_map_model.clone()),
        effective_model: single.map(|single| single.effective_model.clone()),
        models,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MapTarget, mapped_model, parse_model_map, preview_mapped_model, rewrite_mapped_models,
    };

    /// ctp test/model-map.mjs's policy.
    fn policy() -> Option<super::ModelMap> {
        parse_model_map(
            r#"{
                "family:haiku": "gpt-5.6-luna",
                "family:opus": "gpt-5.6-sol",
                "model:claude-opus-4-5": "special-opus"
            }"#,
        )
        .expect("the test policy parses")
    }

    fn body(value: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&value).expect("serialise test body")
    }

    #[test]
    fn disabled_mapping_preserves_the_exact_request_bytes() {
        let input = body(serde_json::json!({"model": "claude-haiku-4-5", "messages": []}));
        for raw in ["", "   ", "off"] {
            let disabled = parse_model_map(raw).expect("disabled parses");
            let result = rewrite_mapped_models(disabled.as_ref(), &input, "POST", "/v1/messages");
            assert_eq!(result.body, input, "raw {raw:?}");
            assert!(!result.mapped);
            assert!(!result.changed);
        }
    }

    #[test]
    fn configured_but_unmatched_requests_preserve_the_exact_bytes() {
        // A nested model value is NOT a documented position, so a body
        // the map does not match keeps every byte — including its spacing.
        let input =
            br#"{ "model" : "claude-sonnet-5", "nested":{"model":"claude-haiku-4-5"} }"#.to_vec();
        let result =
            rewrite_mapped_models(policy().as_ref(), &input, "POST", "/v1/messages?beta=1");
        assert_eq!(result.body, input);
        assert_eq!(result.pre_map_model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(result.effective_model.as_deref(), Some("claude-sonnet-5"));
        assert!(!result.mapped);
    }

    #[test]
    fn exact_selectors_beat_family_selectors_regardless_of_json_key_order() {
        let first = parse_model_map(
            r#"{"family:opus":"family-target","model:claude-opus-4-5":"exact-target"}"#,
        )
        .expect("parses");
        let last = parse_model_map(
            r#"{"model:claude-opus-4-5":"exact-target","family:opus":"family-target"}"#,
        )
        .expect("parses");
        for map in [first, last] {
            let map = map.expect("enabled");
            // A published snapshot id folds to the exact selector's identity.
            assert_eq!(
                mapped_model(Some(&map), "claude-opus-4-5-20251101"),
                Some(&MapTarget {
                    selector: "model:claude-opus-4-5".to_owned(),
                    target: "exact-target".to_owned(),
                })
            );
            assert_eq!(
                preview_mapped_model(Some(&map), "claude-opus-5"),
                Some("family-target")
            );
        }
    }

    #[test]
    fn invalid_policies_fail_validation_clearly() {
        // ctp test/model-map.mjs's invalid set, message for message.
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
    fn canonical_matching_covers_bracket_variants_and_only_published_snapshots() {
        let map = policy().expect("enabled");
        assert_eq!(
            preview_mapped_model(Some(&map), "claude-haiku-4-5[1m]"),
            Some("gpt-5.6-luna")
        );
        assert_eq!(
            preview_mapped_model(Some(&map), "claude-opus-4-5-20251101"),
            Some("special-opus")
        );
        // An unpublished snapshot has no family: never matched, never
        // guessed into one.
        assert_eq!(
            preview_mapped_model(Some(&map), "claude-opus-5-20990101"),
            Some("claude-opus-5-20990101")
        );
        assert_eq!(
            preview_mapped_model(Some(&map), "claude-opus-9"),
            Some("gpt-5.6-sol")
        );
        // No policy: the model unchanged, and no identity for a blank.
        assert_eq!(
            preview_mapped_model(None, "claude-opus-9"),
            Some("claude-opus-9")
        );
        assert_eq!(mapped_model(Some(&map), ""), None);
    }

    #[test]
    fn messages_and_count_tokens_rewrite_only_the_top_level_model() {
        for url in ["/v1/messages", "/v1/messages/count_tokens"] {
            let input = body(serde_json::json!({
                "model": "claude-haiku-4-5",
                "messages": [{"role": "user", "content": {"model": "claude-opus-5"}}],
                "tools": [{"name": "lookup", "input_schema": {"model": "claude-opus-5"}}],
            }));
            let result = rewrite_mapped_models(policy().as_ref(), &input, "POST", url);
            assert!(result.changed, "{url}");
            assert_eq!(result.pre_map_model.as_deref(), Some("claude-haiku-4-5"));
            assert_eq!(result.effective_model.as_deref(), Some("gpt-5.6-luna"));
            let rewritten: serde_json::Value =
                serde_json::from_slice(&result.body).expect("spliced body parses");
            assert_eq!(rewritten["model"], "gpt-5.6-luna");
            // Nested model values are not documented positions.
            assert_eq!(
                rewritten["messages"][0]["content"]["model"],
                "claude-opus-5"
            );
            assert_eq!(
                rewritten["tools"][0]["input_schema"]["model"],
                "claude-opus-5"
            );
        }
    }

    #[test]
    fn a_mapped_model_changes_no_unrelated_json_bytes() {
        // The lexical splice, not a re-serialisation: the large integer
        // keeps its exact digits and the escape stays escaped.
        let raw = r#"{ "model" : "claude-haiku-4-5", "tool_use":{"input":{"record_id":9007199254740993}}, "escaped":"\u0061" }"#;
        let input = raw.as_bytes().to_vec();
        let result = rewrite_mapped_models(policy().as_ref(), &input, "POST", "/v1/messages");
        let expected = raw.replace("\"claude-haiku-4-5\"", "\"gpt-5.6-luna\"");
        let text = String::from_utf8(result.body).expect("utf-8");
        assert_eq!(text, expected);
        assert!(
            text.contains("9007199254740993"),
            "the integer keeps its digits"
        );
        assert!(
            text.contains(r#""escaped":"\u0061""#),
            "the escape stays escaped"
        );
    }

    #[test]
    fn batch_rewriting_visits_only_requests_params_model() {
        let input = body(serde_json::json!({
            "model": "claude-haiku-4-5",
            "requests": [
                {"custom_id": "a", "params": {"model": "claude-haiku-4-5", "messages": []}},
                {"custom_id": "b", "params": {"model": "claude-sonnet-5", "messages": [{"model": "claude-opus-5"}]}},
                {"custom_id": "c", "params": {"model": "claude-opus-5", "metadata": {"model": "claude-haiku-4-5"}}},
            ],
        }));
        let result =
            rewrite_mapped_models(policy().as_ref(), &input, "POST", "/v1/messages/batches");
        assert!(result.changed);
        assert_eq!(result.pre_map_model, None, "a batch has no single position");
        assert_eq!(result.effective_model, None);
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| (
                    m.request_index,
                    m.pre_map_model.as_str(),
                    m.effective_model.as_str(),
                    m.matched
                ))
                .collect::<Vec<_>>(),
            vec![
                (Some(0), "claude-haiku-4-5", "gpt-5.6-luna", true),
                (Some(1), "claude-sonnet-5", "claude-sonnet-5", false),
                (Some(2), "claude-opus-5", "gpt-5.6-sol", true),
            ]
        );
        let rewritten: serde_json::Value =
            serde_json::from_slice(&result.body).expect("spliced body parses");
        assert_eq!(
            rewritten["model"], "claude-haiku-4-5",
            "the top level is not a batch position"
        );
        assert_eq!(rewritten["requests"][0]["params"]["model"], "gpt-5.6-luna");
        assert_eq!(
            rewritten["requests"][1]["params"]["model"],
            "claude-sonnet-5"
        );
        assert_eq!(
            rewritten["requests"][1]["params"]["messages"][0]["model"],
            "claude-opus-5"
        );
        assert_eq!(rewritten["requests"][2]["params"]["model"], "gpt-5.6-sol");
        assert_eq!(
            rewritten["requests"][2]["params"]["metadata"]["model"],
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn malformed_and_unsupported_requests_are_forwarded_byte_for_byte() {
        let map = policy();
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
            let result = rewrite_mapped_models(map.as_ref(), &input, method, url);
            assert_eq!(result.body, input, "{method} {url}");
            assert!(!result.changed, "{method} {url}");
        }
    }

    #[test]
    fn invalid_utf8_remains_byte_exact_even_when_its_model_would_match() {
        let mut input = br#"{"model":"claude-haiku-4-5","metadata":""#.to_vec();
        input.push(0xff);
        input.extend_from_slice(br#""}"#);
        let result = rewrite_mapped_models(policy().as_ref(), &input, "POST", "/v1/messages");
        assert_eq!(result.body, input);
        assert!(!result.changed);
        assert!(!result.mapped);
    }

    #[test]
    fn a_matching_no_op_target_preserves_bytes_but_reports_the_effective_model() {
        let map = parse_model_map(r#"{"family:haiku":"claude-haiku-4-5"}"#)
            .expect("parses")
            .expect("enabled");
        let input = br#"{ "model": "claude-haiku-4-5", "messages": [] }"#.to_vec();
        let result = rewrite_mapped_models(Some(&map), &input, "POST", "/v1/messages");
        assert_eq!(result.body, input, "a no-op target splices nothing");
        assert!(result.mapped);
        assert!(!result.changed);
        assert_eq!(result.effective_model.as_deref(), Some("claude-haiku-4-5"));
    }
}
