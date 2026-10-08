//! The openrouter backend (plan: "Backend providers") — OpenAI-chat
//! protocol, API-key auth, real billed `usage.cost` plus the serving
//! provider captured verbatim (ledger parity).

use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Url;

use super::Provider;

/// Anthropic's key header. Openrouter takes its key as a bearer only, so
/// this header on a request bound for openrouter is always another
/// provider's credential.
const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");

/// Whether the request's `Authorization` carries an Anthropic credential.
/// Claude's OAuth bearer (`sk-ant-oat…`) and Anthropic API keys
/// (`sk-ant-api…`) share the `sk-ant-` prefix, and no openrouter key does
/// (those are `sk-or-…`), so the prefix alone separates them. A denylist
/// rather than an `sk-or-` allowlist: pass-through-when-present exists
/// for whatever key the frontend brings, and the one credential known to
/// arrive here by mistake is claude's, which an anthropic-shaped client
/// sends on every path it calls, `/v1/models` included.
fn carries_anthropic_credential(headers: &HeaderMap) -> bool {
    headers.get_all(AUTHORIZATION).iter().any(|value| {
        value
            .to_str()
            .ok()
            .and_then(|value| value.split_whitespace().last())
            .is_some_and(|token| token.starts_with("sk-ant-"))
    })
}

/// OpenRouter: upstream `https://openrouter.ai/api/v1`, `Authorization:
/// Bearer <key>` injected only when the incoming request carries none.
pub struct OpenRouter {
    /// The upstream base including the `/v1` prefix (validated at config
    /// load).
    upstream: Url,
    /// The API key, resolved once at startup (env first, then the config
    /// literal — see [`crate::config::OpenRouterConfig::api_key`]). Never
    /// logged, never in the ledger (invariant 2).
    api_key: Option<String>,
}

impl OpenRouter {
    /// Build the provider. `api_key` is the already-resolved key.
    pub fn new(upstream: Url, api_key: Option<String>) -> OpenRouter {
        OpenRouter { upstream, api_key }
    }
}

impl Provider for OpenRouter {
    fn id(&self) -> &str {
        "openrouter"
    }

    fn endpoint(&self, path: &str) -> Url {
        // The frontend path carries the OpenAI `/v1` prefix; the upstream
        // base already includes `/v1`, so the prefix is stripped and the
        // rest is appended. String construction, not `Url::join`: RFC 3986
        // join against a base not ending in `/` drops its last segment
        // (`…/api/v1` + `chat/completions` would give `…/api/chat/completions`).
        let (path, query) = match path.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (path, None),
        };
        let path = path.strip_prefix("/v1").unwrap_or(path);
        let base = self.upstream.as_str().trim_end_matches('/');
        let mut url = format!("{base}{path}");
        if let Some(query) = query {
            url.push('?');
            url.push_str(query);
        }
        // Infallible: the base parsed as a URL at config load, and the
        // pieces above are valid path/query fragments of one.
        Url::parse(&url).expect("validated upstream base makes every endpoint valid")
    }

    /// A bearer counts as the frontend's own openrouter credential unless
    /// it is an Anthropic one, which [`Provider::strip_foreign_credentials`]
    /// drops: the stored key then takes its place, as if the request had
    /// carried none.
    fn credential_present(&self, incoming: &HeaderMap) -> bool {
        incoming.contains_key(AUTHORIZATION) && !carries_anthropic_credential(incoming)
    }

    /// Anthropic credentials never reach openrouter.ai: `/v1/models` is
    /// served here for every frontend, and an anthropic-shaped client
    /// calling it sends its anthropic key or claude's OAuth bearer, which
    /// pass-through-when-present would otherwise forward verbatim.
    fn strip_foreign_credentials(&self, outgoing: &mut HeaderMap) {
        outgoing.remove(&X_API_KEY);
        if carries_anthropic_credential(outgoing) {
            outgoing.remove(AUTHORIZATION);
        }
    }

    /// Only Anthropic's own models behind openrouter understand the field;
    /// the rest (glm, deepseek, ...) answer `400 Mid-conversation reasoning
    /// effort (configuration_update) is not supported`.
    fn accepts_message_effort(&self, model: &str) -> bool {
        model.starts_with("anthropic/")
    }

    fn inject_auth(&self, outgoing: &mut HeaderMap) {
        let Some(key) = &self.api_key else {
            // No key: forward unauthenticated; openrouter's 401 body passes
            // through and names the problem (see the trait docs).
            return;
        };
        let value = format!("Bearer {key}");
        // A key with invalid header bytes injects nothing — the upstream
        // 401s visibly instead of the proxy panicking over a credential.
        if let Ok(value) = HeaderValue::from_str(&value) {
            outgoing.insert(AUTHORIZATION, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Provider;
    use super::OpenRouter;
    use axum::http::header::AUTHORIZATION;
    use axum::http::{HeaderMap, HeaderValue};

    fn provider() -> OpenRouter {
        OpenRouter::new(
            "https://openrouter.ai/api/v1"
                .parse()
                .expect("upstream url"),
            Some("sk-test-key".to_owned()),
        )
    }

    #[test]
    fn only_anthropic_models_accept_message_effort() {
        let provider = provider();
        assert!(provider.accepts_message_effort("anthropic/claude-sonnet-5.5"));
        assert!(!provider.accepts_message_effort("z-ai/glm-5.3"));
    }

    #[test]
    fn frontend_paths_map_onto_the_v1_base() {
        let provider = provider();
        assert_eq!(
            provider.endpoint("/v1/chat/completions").as_str(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            provider.endpoint("/v1/models").as_str(),
            "https://openrouter.ai/api/v1/models"
        );
        assert_eq!(
            provider.endpoint("/v1/models?owned_by=z-ai").as_str(),
            "https://openrouter.ai/api/v1/models?owned_by=z-ai"
        );
    }

    #[test]
    fn base_trailing_slash_and_unknown_prefixes_do_not_mangle_the_url() {
        let provider = provider();
        assert_eq!(
            provider.endpoint("/other/chat").as_str(),
            "https://openrouter.ai/api/v1/other/chat",
            "a path without /v1 is appended as-is"
        );
        let slashed = OpenRouter::new(
            "http://localhost:9/v1/".parse().expect("upstream url"),
            None,
        );
        assert_eq!(
            slashed.endpoint("/v1/chat/completions").as_str(),
            "http://localhost:9/v1/chat/completions",
            "a trailing slash on the base never doubles up"
        );
    }

    #[test]
    fn auth_is_bearer_and_absent_keys_inject_nothing() {
        let provider = provider();
        let mut headers = HeaderMap::new();
        provider.inject_auth(&mut headers);
        assert_eq!(
            headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()),
            Some("Bearer sk-test-key")
        );

        let keyless = OpenRouter::new(
            "https://openrouter.ai/api/v1"
                .parse()
                .expect("upstream url"),
            None,
        );
        let mut headers = HeaderMap::new();
        keyless.inject_auth(&mut headers);
        assert!(headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn anthropic_credentials_are_foreign_and_never_count_as_present() {
        let provider = provider();
        for bearer in [
            "Bearer sk-ant-oat01-claude-oauth",
            "Bearer sk-ant-api03-key",
            "bearer  sk-ant-api03-key",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, HeaderValue::from_static(bearer));
            headers.insert("x-api-key", HeaderValue::from_static("sk-ant-api03-key"));
            assert!(
                !provider.credential_present(&headers),
                "{bearer:?} is not an openrouter credential"
            );
            provider.strip_foreign_credentials(&mut headers);
            assert!(headers.get(AUTHORIZATION).is_none(), "{bearer:?} dropped");
            assert!(headers.get("x-api-key").is_none(), "x-api-key dropped");
        }

        // The frontend's own openrouter key stays, verbatim.
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer sk-or-v1-own"),
        );
        assert!(provider.credential_present(&headers));
        provider.strip_foreign_credentials(&mut headers);
        assert_eq!(
            headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()),
            Some("Bearer sk-or-v1-own")
        );
    }
}
