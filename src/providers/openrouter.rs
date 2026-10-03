//! The openrouter backend (plan: "Backend providers") — OpenAI-chat
//! protocol, API-key auth, real billed `usage.cost` plus the serving
//! provider captured verbatim (ledger parity).

use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue};
use reqwest::Url;

use super::Provider;

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
    use axum::http::HeaderMap;
    use axum::http::header::AUTHORIZATION;

    fn provider() -> OpenRouter {
        OpenRouter::new(
            "https://openrouter.ai/api/v1"
                .parse()
                .expect("upstream url"),
            Some("sk-test-key".to_owned()),
        )
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
}
