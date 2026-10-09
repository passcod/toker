//! The anthropic backends (plan: "Backend providers") — two providers, one
//! upstream, one protocol ([crate::ir::anthropic]).
//!
//! - [`AnthropicSub`]: the subscription. Auth is pass-through-when-present:
//!   claude brings its own OAuth bearer, and toker has no stored sub token
//!   yet (a later credentials unit adds signing), so nothing is ever
//!   injected. The sub is also today's only **meter source** (plan: quota
//!   gate — "Anthropic sub is the only meter source today"): the
//!   `anthropic-ratelimit-*` response headers parse into the quota
//!   snapshot the gate will read.
//! - [`AnthropicApi`]: the plain API. Auth is `x-api-key` — NOT a bearer —
//!   injected only when the request carries neither `x-api-key` nor
//!   `authorization` of its own.
//!
//! [`parse_rate_limits`] is the faithful port of the predecessor's
//! meter-header parser. Every `anthropic-ratelimit-*`
//! header folds into the stable shape: util/reset per window (resets in
//! epoch seconds), the per-claim statuses, `status`, `claim` =
//! `representative-claim`, `overageInUse`, `fallbackPct` — and anything
//! unknown lands in `other`, kept, never dropped. (The predecessor's
//! first cut
//! cherry-picked and silently discarded the representative-claim and the
//! per-claim statuses, which turned out to be the signals that matter
//! most; the KNOWN set and the `other` catch-all are that lesson, ported.)
//!
//! Key spellings are the predecessor's camelCase verbatim — "the stable
//! shape, so
//! existing analysis keeps working" — including on the wire into the
//! ledger's `rate_limits` and `meters_state` JSON columns.

use axum::http::HeaderMap;
use axum::http::{HeaderValue, header};
use reqwest::Url;
use serde_json::{Map, Value, json};

use super::Provider;
use crate::routing::{BackendAdapterId, BackendBinding, Capabilities, DialectId, ProtocolId};

const BINDINGS: &[BackendBinding] = &[BackendBinding::canonical(
    ProtocolId::AnthropicMessages,
    DialectId::AnthropicMessages,
    BackendAdapterId::AnthropicMessages,
    Capabilities::MESSAGES,
)];

/// The header carrying the API key (Anthropic's API auth is `x-api-key`,
/// not a bearer).
const X_API_KEY: header::HeaderName = header::HeaderName::from_static("x-api-key");

/// The anthropic subscription backend: upstream `https://api.anthropic.com`,
/// claude's own OAuth passed through verbatim, quota meters from the
/// response headers.
pub struct AnthropicSub {
    upstream: Url,
    /// The operator's model routing map, when one is configured
    /// (`[providers.anthropic_sub.model_map]`) — the routing map this
    /// backend applies, ported from the predecessor's env-typed knob.
    model_map: Option<crate::middleware::model_map::ModelMap>,
}

impl AnthropicSub {
    /// Build the provider. `upstream` is the API root (no `/v1` prefix —
    /// frontend paths carry their own, unlike openrouter's base).
    pub fn new(
        upstream: Url,
        model_map: Option<crate::middleware::model_map::ModelMap>,
    ) -> AnthropicSub {
        AnthropicSub {
            upstream,
            model_map,
        }
    }
}

/// The anthropic API backend: same upstream, `x-api-key` auth, list-price
/// estimated cost (the row's cost kind; the pricing itself is
/// [`crate::catalog`]).
pub struct AnthropicApi {
    upstream: Url,
    /// The API key, resolved once at startup (env first, then the config
    /// literal — see [`crate::config::AnthropicApiConfig::api_key`]). Never
    /// logged, never in the ledger (invariant 2).
    api_key: Option<String>,
    /// The operator's model routing map (see [`AnthropicSub::model_map`]).
    model_map: Option<crate::middleware::model_map::ModelMap>,
}

impl AnthropicApi {
    /// Build the provider. `api_key` is the already-resolved key.
    pub fn new(
        upstream: Url,
        api_key: Option<String>,
        model_map: Option<crate::middleware::model_map::ModelMap>,
    ) -> AnthropicApi {
        AnthropicApi {
            upstream,
            api_key,
            model_map,
        }
    }
}

/// The endpoint mapping both anthropic providers share: the frontend path
/// (query included) is appended to the base whole. Unlike openrouter's
/// base, the anthropic base has no `/v1` prefix to strip — the frontend's
/// `/v1/messages…` paths are already the upstream's paths.
fn endpoint_of(upstream: &Url, path: &str) -> Url {
    let base = upstream.as_str().trim_end_matches('/');
    let url = format!("{base}{path}");
    // Infallible: the base parsed as a URL at config load, and the pieces
    // above are valid path/query fragments of one.
    Url::parse(&url).expect("validated upstream base makes every endpoint valid")
}

impl Provider for AnthropicSub {
    fn id(&self) -> &str {
        "anthropic_sub"
    }

    fn bindings(&self) -> &'static [BackendBinding] {
        BINDINGS
    }

    fn model_map(&self) -> Option<&crate::middleware::model_map::ModelMap> {
        self.model_map.as_ref()
    }

    fn endpoint(&self, path: &str) -> Url {
        endpoint_of(&self.upstream, path)
    }

    // credential_present: the default — the sub's credential is the OAuth
    // bearer, so `authorization` present means pass-through.

    /// Nothing to inject: toker holds no sub token yet, so a request
    /// without its own bearer goes up unauthenticated and anthropic's 401
    /// body passes through — visibly verifying the wiring (the
    /// keyring/signing unit replaces this).
    fn inject_auth(&self, _outgoing: &mut HeaderMap) {}

    fn prepare_protocol_headers(&self, outgoing: &mut HeaderMap) {
        outgoing
            .entry("anthropic-version")
            .or_insert(HeaderValue::from_static("2023-06-01"));
    }

    /// The sub is the meter source: its quota snapshot is the parsed
    /// `anthropic-ratelimit-*` headers.
    fn meters(&self, headers: &HeaderMap) -> Option<Value> {
        parse_rate_limits(headers)
    }

    fn is_meter_source(&self) -> bool {
        true
    }
}

impl Provider for AnthropicApi {
    fn id(&self) -> &str {
        "anthropic_api"
    }

    fn bindings(&self) -> &'static [BackendBinding] {
        BINDINGS
    }

    fn model_map(&self) -> Option<&crate::middleware::model_map::ModelMap> {
        self.model_map.as_ref()
    }

    fn endpoint(&self, path: &str) -> Url {
        endpoint_of(&self.upstream, path)
    }

    /// The API's credential lives in `x-api-key`, so a request is already
    /// credentialed when it carries that — OR a bearer (some clients send
    /// `authorization` against the API too, and pass-through-when-present
    /// keeps it either way).
    fn credential_present(&self, incoming: &HeaderMap) -> bool {
        incoming.contains_key(&X_API_KEY) || incoming.contains_key(header::AUTHORIZATION)
    }

    fn strip_foreign_credentials_for(&self, outgoing: &mut HeaderMap, frontend: ProtocolId) {
        if frontend != ProtocolId::AnthropicMessages {
            outgoing.remove(header::AUTHORIZATION);
            outgoing.remove(&X_API_KEY);
        }
    }

    fn prepare_protocol_headers(&self, outgoing: &mut HeaderMap) {
        outgoing
            .entry("anthropic-version")
            .or_insert(HeaderValue::from_static("2023-06-01"));
    }

    fn inject_auth(&self, outgoing: &mut HeaderMap) {
        let Some(key) = &self.api_key else {
            // No key: forward unauthenticated; anthropic's 401 body passes
            // through and names the problem (see the trait docs).
            return;
        };
        // A key with invalid header bytes injects nothing — the upstream
        // 401s visibly instead of the proxy panicking over a credential.
        if let Ok(value) = HeaderValue::from_str(key) {
            outgoing.insert(&X_API_KEY, value);
        }
    }

    // meters: the default `None` — the plain API's `anthropic-ratelimit-*`
    // headers are requests/tokens-per-minute limits, not quota meters, and
    // the plan names the sub as today's only meter source.
}

/// The meter-header keys lifted into the stable shape's named fields.
/// Everything else the API sends lands in
/// `other` rather than being dropped. Note `reset` is known but not
/// lifted — the predecessor never surfaced it either; it is simply
/// excluded from
/// `other`, exactly as ported.
const KNOWN_LIMIT_KEYS: &[&str] = &[
    "5h-utilization",
    "5h-reset",
    "5h-status",
    "7d-utilization",
    "7d-reset",
    "7d-status",
    "overage-utilization",
    "overage-reset",
    "overage-status",
    "status",
    "reset",
    "representative-claim",
    "fallback-percentage",
    "overage-in-use",
];

/// Parse the quota meter snapshot from one response's headers
/// (a faithful port; see the module docs).
///
/// `None` when the response carries no `anthropic-ratelimit-*` headers at
/// all. Every key the parser lifts is present in the returned object,
/// `null` when
/// the response did not carry it; unknown keys are preserved in `other`
/// with the `anthropic-ratelimit-unified-` family prefix stripped (the
/// same strip the lookup applies, so non-unified unknowns keep
/// their full header names).
pub fn parse_rate_limits(headers: &HeaderMap) -> Option<Value> {
    // The first occurrence of a repeated header name wins (Node's Headers
    // would have merged them; a merge production never showed).
    let mut all: Vec<(String, String)> = Vec::new();
    for (name, value) in headers {
        let name = name.as_str();
        if !name.starts_with("anthropic-ratelimit") {
            continue;
        }
        let Ok(value) = value.to_str() else {
            continue; // non-UTF-8: unobservable, skipped
        };
        let key = match name.strip_prefix("anthropic-ratelimit-unified") {
            Some(rest) => rest.strip_prefix('-').unwrap_or(rest),
            None => name,
        };
        if !all.iter().any(|(existing, _)| existing == key) {
            all.push((key.to_owned(), value.to_owned()));
        }
    }
    if all.is_empty() {
        return None;
    }

    // The raw header string parsed as a
    // JSON number keeps its literal verbatim (epoch-second resets stay
    // integers, utilisations keep their precision), and anything
    // non-numeric is `NaN`, which JSON-serialises as `null`.
    let get = |key: &str| all.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    let num = |key: &str| {
        get(key)
            .and_then(|v| serde_json::from_str::<serde_json::Number>(v.trim()).ok())
            .map(Value::Number)
    };
    let text = |key: &str| get(key).map(|v| Value::String(v.to_owned()));

    let other: Map<String, Value> = all
        .iter()
        .filter(|(k, _)| !KNOWN_LIMIT_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();

    Some(json!({
        // Stable shape, so existing analysis keeps working.
        "util5h": num("5h-utilization"),
        "reset5h": num("5h-reset"),
        "util7d": num("7d-utilization"),
        "reset7d": num("7d-reset"),
        "utilOverage": num("overage-utilization"),
        "resetOverage": num("overage-reset"),
        "status": text("status"),
        // Per-claim state, and which claim is currently binding. `claim` is
        // the field that should move when a window fills and spend shifts
        // to overage.
        "status5h": text("5h-status"),
        "status7d": text("7d-status"),
        "statusOverage": text("overage-status"),
        "claim": text("representative-claim"),
        // Whether this request is drawing on overage rather than plan
        // quota (exactly the literal `true`: a bool, false when absent).
        "overageInUse": get("overage-in-use") == Some("true"),
        "fallbackPct": num("fallback-percentage"),
        "other": Value::Object(other),
    }))
}

#[cfg(test)]
mod tests {
    use super::super::Provider;
    use super::{AnthropicApi, AnthropicSub, parse_rate_limits};
    use crate::routing::{BackendAdapterId, Capabilities, ProtocolId};
    use axum::http::{HeaderMap, HeaderValue, header};
    use serde_json::json;

    fn sub() -> AnthropicSub {
        AnthropicSub::new(
            "https://api.anthropic.com".parse().expect("upstream url"),
            None,
        )
    }

    fn api() -> AnthropicApi {
        AnthropicApi::new(
            "https://api.anthropic.com".parse().expect("upstream url"),
            Some("sk-ant-test-key".to_owned()),
            None,
        )
    }

    #[test]
    fn both_anthropic_providers_bind_only_messages() {
        for provider in [&sub() as &dyn Provider, &api() as &dyn Provider] {
            assert!(provider.supports_protocol(ProtocolId::AnthropicMessages));
            assert!(!provider.supports_protocol(ProtocolId::OpenAiChat));
            assert!(!provider.supports_protocol(ProtocolId::OpenAiResponses));
            let canonical = provider.bindings()[0]
                .canonical_backend()
                .expect("Messages adapter is complete");
            assert_eq!(canonical.adapter(), BackendAdapterId::AnthropicMessages);
            assert_eq!(canonical.capabilities(), Capabilities::MESSAGES);
        }
    }

    /// A realistic full meter set: one header per named field, plus an
    /// unknown experiment header proving `other` preservation, plus the
    /// known-but-unlifted `reset`.
    fn metered(map: &mut HeaderMap) {
        for (name, value) in [
            ("anthropic-ratelimit-unified-5h-utilization", "0.4127"),
            ("anthropic-ratelimit-unified-5h-reset", "1769500800"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.2214"),
            ("anthropic-ratelimit-unified-7d-reset", "1769846400"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-overage-utilization", "0.0004"),
            ("anthropic-ratelimit-unified-overage-reset", "1769500800"),
            ("anthropic-ratelimit-unified-overage-status", "allowed"),
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-representative-claim", "5h"),
            ("anthropic-ratelimit-unified-overage-in-use", "false"),
            ("anthropic-ratelimit-unified-fallback-percentage", "12.5"),
            ("anthropic-ratelimit-unified-reset", "1769500800"),
            ("anthropic-ratelimit-experiment-thing", "42"),
        ] {
            map.insert(
                name.parse::<axum::http::HeaderName>().expect("header name"),
                HeaderValue::from_static(value),
            );
        }
    }

    #[test]
    fn frontend_paths_map_onto_the_api_root() {
        let providers: [&dyn Provider; 2] = [&sub(), &api()];
        for provider in providers {
            assert_eq!(
                provider.endpoint("/v1/messages").as_str(),
                "https://api.anthropic.com/v1/messages"
            );
            assert_eq!(
                provider.endpoint("/v1/messages/count_tokens").as_str(),
                "https://api.anthropic.com/v1/messages/count_tokens"
            );
            assert_eq!(
                provider
                    .endpoint("/v1/messages/batches?limit=10&after=3")
                    .as_str(),
                "https://api.anthropic.com/v1/messages/batches?limit=10&after=3"
            );
        }
        let slashed = AnthropicSub::new("http://localhost:9/".parse().expect("upstream url"), None);
        assert_eq!(
            slashed.endpoint("/v1/messages").as_str(),
            "http://localhost:9/v1/messages",
            "a trailing slash on the base never doubles up"
        );
    }

    #[test]
    fn the_sub_passes_credentials_through_and_injects_nothing() {
        let provider = sub();
        assert_eq!(provider.id(), "anthropic_sub");

        let mut incoming = HeaderMap::new();
        assert!(!provider.credential_present(&incoming));
        incoming.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer oauth"),
        );
        assert!(
            provider.credential_present(&incoming),
            "claude's bearer is the sub credential: pass-through"
        );

        let mut outgoing = HeaderMap::new();
        provider.inject_auth(&mut outgoing);
        assert!(
            outgoing.is_empty(),
            "no stored sub token yet — nothing injected"
        );
    }

    #[test]
    fn the_api_injects_x_api_key_only_when_neither_auth_header_is_present() {
        let provider = api();
        assert_eq!(provider.id(), "anthropic_api");

        let neither = HeaderMap::new();
        assert!(!provider.credential_present(&neither));
        let mut outgoing = HeaderMap::new();
        provider.inject_auth(&mut outgoing);
        assert_eq!(
            outgoing.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("sk-ant-test-key"),
            "no credential of its own → the stored key is injected"
        );
        assert!(
            !outgoing.contains_key(header::AUTHORIZATION),
            "anthropic API auth is x-api-key, never a bearer"
        );

        let mut bearer = HeaderMap::new();
        bearer.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer own"),
        );
        assert!(
            provider.credential_present(&bearer),
            "a request-bearing authorization also blocks injection"
        );

        let mut keyed = HeaderMap::new();
        keyed.insert(
            axum::http::HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("client-own"),
        );
        assert!(
            provider.credential_present(&keyed),
            "x-api-key present is pass-through, not injection"
        );

        let keyless = AnthropicApi::new(
            "https://api.anthropic.com".parse().expect("upstream url"),
            None,
            None,
        );
        let mut outgoing = HeaderMap::new();
        keyless.inject_auth(&mut outgoing);
        assert!(outgoing.is_empty(), "no key → nothing injected");
    }

    #[test]
    fn only_the_sub_is_a_meter_source() {
        let mut headers = HeaderMap::new();
        metered(&mut headers);
        assert!(
            sub().meters(&headers).is_some(),
            "the sub's quota headers parse into a snapshot"
        );
        assert!(
            api().meters(&headers).is_none(),
            "the API's RPM limits are not quota meters (plan: the sub is the only meter source)"
        );
        assert!(sub().is_meter_source());
        assert!(!api().is_meter_source());

        let bare = HeaderMap::new();
        assert_eq!(sub().meters(&bare), None);
    }

    #[test]
    fn the_full_meter_set_parses_to_the_stable_wire_shape() {
        let mut headers = HeaderMap::new();
        metered(&mut headers);
        assert_eq!(
            parse_rate_limits(&headers),
            Some(json!({
                "util5h": 0.4127,
                "reset5h": 1769500800,
                "util7d": 0.2214,
                "reset7d": 1769846400,
                "utilOverage": 0.0004,
                "resetOverage": 1769500800,
                "status": "allowed",
                "status5h": "allowed",
                "status7d": "allowed",
                "statusOverage": "allowed",
                "claim": "5h",
                "overageInUse": false,
                "fallbackPct": 12.5,
                // Unknown keys are kept, never dropped — under the full
                // header name, because the unified-family strip never
                // matched. `reset` is known-but-unlifted, so it is not
                // here either (parser parity).
                "other": {"anthropic-ratelimit-experiment-thing": "42"},
            })),
            "epoch resets stay integers and unknown keys land in other"
        );
    }

    #[test]
    fn absent_fields_read_null_and_absent_headers_read_none() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization"
                .parse::<axum::http::HeaderName>()
                .expect("name"),
            HeaderValue::from_static("0.9"),
        );
        assert_eq!(
            parse_rate_limits(&headers),
            Some(json!({
                "util5h": 0.9,
                "reset5h": null, "util7d": null, "reset7d": null,
                "utilOverage": null, "resetOverage": null, "status": null,
                "status5h": null, "status7d": null, "statusOverage": null,
                "claim": null, "overageInUse": false, "fallbackPct": null,
                "other": {},
            })),
            "every named key present, null when the response did not carry it"
        );

        assert_eq!(parse_rate_limits(&HeaderMap::new()), None);
        let mut unrelated = HeaderMap::new();
        unrelated.insert(header::RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(parse_rate_limits(&unrelated), None);
    }

    #[test]
    fn non_numeric_values_read_null_and_overage_needs_the_exact_literal() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("anthropic-ratelimit-unified-5h-utilization", "pretty high"),
            ("anthropic-ratelimit-unified-overage-in-use", "TRUE"),
            ("anthropic-ratelimit-unified-fallback-percentage", ""),
        ] {
            headers.insert(
                name.parse::<axum::http::HeaderName>().expect("name"),
                HeaderValue::from_static(value),
            );
        }
        let parsed = parse_rate_limits(&headers).expect("ratelimit headers present");
        assert_eq!(parsed["util5h"], serde_json::Value::Null, "NaN → null");
        assert_eq!(parsed["fallbackPct"], serde_json::Value::Null);
        // The overage flag is exact and case-sensitive.
        assert_eq!(
            parsed["overageInUse"],
            json!(false),
            "\"TRUE\" is not \"true\""
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-overage-in-use"
                .parse::<axum::http::HeaderName>()
                .expect("name"),
            HeaderValue::from_static("true"),
        );
        assert_eq!(
            parse_rate_limits(&headers).expect("parsed")["overageInUse"],
            json!(true)
        );
    }
}
