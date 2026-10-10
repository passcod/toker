//! The direct OpenAI API backend: Chat Completions, bearer API-key auth,
//! provider model discovery, and estimated list-price accounting.

use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Url;

use super::Provider;
use crate::routing::{BackendAdapterId, BackendBinding, Capabilities, DialectId, ProtocolId};

const BINDINGS: &[BackendBinding] = &[BackendBinding::canonical(
    ProtocolId::OpenAiChat,
    DialectId::OpenAiChatCompletions,
    BackendAdapterId::OpenAiChatCompletions,
    Capabilities::CHAT,
)];

const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");

fn carries_foreign_bearer(headers: &HeaderMap) -> bool {
    headers.get_all(AUTHORIZATION).iter().any(|value| {
        value
            .to_str()
            .ok()
            .and_then(|value| value.split_whitespace().last())
            .is_some_and(|token| token.starts_with("sk-ant-") || token.starts_with("sk-or-"))
    })
}

pub struct OpenAiApi {
    upstream: Url,
    api_key: Option<String>,
}

impl OpenAiApi {
    pub fn new(upstream: Url, api_key: Option<String>) -> OpenAiApi {
        OpenAiApi { upstream, api_key }
    }
}

impl Provider for OpenAiApi {
    fn id(&self) -> &str {
        "openai_api"
    }

    fn bindings(&self) -> &'static [BackendBinding] {
        BINDINGS
    }

    fn endpoint(&self, path: &str) -> Url {
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
        Url::parse(&url).expect("validated upstream base makes every endpoint valid")
    }

    fn credential_present(&self, incoming: &HeaderMap) -> bool {
        incoming.contains_key(AUTHORIZATION) && !carries_foreign_bearer(incoming)
    }

    fn strip_foreign_credentials(&self, outgoing: &mut HeaderMap) {
        outgoing.remove(&X_API_KEY);
        if carries_foreign_bearer(outgoing) {
            outgoing.remove(AUTHORIZATION);
        }
    }

    fn inject_auth(&self, outgoing: &mut HeaderMap) {
        let Some(key) = &self.api_key else {
            return;
        };
        if let Ok(value) = HeaderValue::from_str(&format!("Bearer {key}")) {
            outgoing.insert(AUTHORIZATION, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenAiApi, Provider};
    use crate::routing::{BackendAdapterId, DialectId, ProtocolId};
    use axum::http::header::AUTHORIZATION;
    use axum::http::{HeaderMap, HeaderValue};

    fn provider() -> OpenAiApi {
        OpenAiApi::new(
            "https://api.openai.com/v1".parse().expect("upstream"),
            Some("sk-proj-test".to_owned()),
        )
    }

    #[test]
    fn binds_only_the_chat_completions_wire() {
        let provider = provider();
        assert!(provider.supports_protocol(ProtocolId::OpenAiChat));
        assert!(!provider.supports_protocol(ProtocolId::AnthropicMessages));
        assert!(!provider.supports_protocol(ProtocolId::OpenAiResponses));
        let binding = provider.bindings()[0];
        assert_eq!(binding.dialect(), DialectId::OpenAiChatCompletions);
        assert_eq!(
            binding.canonical_backend().expect("canonical").adapter(),
            BackendAdapterId::OpenAiChatCompletions
        );
    }

    #[test]
    fn maps_frontend_paths_onto_the_v1_base() {
        let provider = provider();
        assert_eq!(
            provider.endpoint("/v1/chat/completions?x=1").as_str(),
            "https://api.openai.com/v1/chat/completions?x=1"
        );
        assert_eq!(
            provider.endpoint("/v1/models").as_str(),
            "https://api.openai.com/v1/models"
        );
    }

    #[test]
    fn replaces_known_foreign_credentials_but_keeps_native_bearers() {
        let provider = provider();
        for foreign in ["Bearer sk-ant-oat-test", "Bearer sk-or-test"] {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, HeaderValue::from_str(foreign).unwrap());
            provider.strip_foreign_credentials(&mut headers);
            assert!(!provider.credential_present(&headers));
            provider.inject_auth(&mut headers);
            assert_eq!(headers[AUTHORIZATION], "Bearer sk-proj-test");
        }

        let mut native = HeaderMap::new();
        native.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer supplied-openai-key"),
        );
        provider.strip_foreign_credentials(&mut native);
        assert!(provider.credential_present(&native));
        assert_eq!(native[AUTHORIZATION], "Bearer supplied-openai-key");
    }
}
