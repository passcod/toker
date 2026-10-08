//! Backend providers.
//!
//! Plan: "Backend providers" — providers within a protocol share an adapter
//! (see [crate::proto]) and differ in auth, cost semantics (billed /
//! estimated / plan-equivalent), and meter parsing (e.g. anthropic sub's
//! `anthropic-ratelimit-*` headers). Phase 1 wired [OpenRouter] only; the
//! anthropic unit added the two anthropic providers
//! ([`AnthropicSub`], [`AnthropicApi`]); the codex unit adds
//! [`CodexSub`] (the OpenAI-Responses backend, client + auth first —
//! its routing lands with the responses frontend), and later phases add
//! more to exactly the slots the router holds.

mod anthropic;
pub mod codex;
mod openrouter;

pub use anthropic::{AnthropicApi, AnthropicSub, parse_rate_limits};
pub use codex::CodexSub;
pub use openrouter::OpenRouter;

use axum::http::{HeaderMap, header};
use reqwest::Url;
use serde_json::Value;

/// A backend provider: identity, upstream endpoint mapping, credential
/// injection, and (when the provider is a meter source) meter parsing.
/// Same-protocol providers (openrouter, openai api, lunaroute) all satisfy
/// this today; cross-protocol backends add their adapter in later phases.
pub trait Provider: Send + Sync {
    /// The stable provider id — the ledger's `provider` column and the
    /// backend half of `frontend:backend` routes.
    fn id(&self) -> &str;

    /// The upstream URL for an incoming frontend path (query included when
    /// the request carries one). E.g. `/v1/chat/completions` →
    /// `https://openrouter.ai/api/v1/chat/completions` — the mapping from
    /// frontend paths to this provider's upstream shape lives here.
    fn endpoint(&self, path: &str) -> Url;

    /// Whether the incoming request already carries this provider's
    /// credential — pass-through-when-present (plan: Credentials): a
    /// frontend that brings its own credential keeps it, verbatim. The
    /// server consults this before [`Provider::inject_auth`].
    ///
    /// The default checks `authorization`; a provider whose credential
    /// lives in another header (anthropic api's `x-api-key`) overrides —
    /// accepting more than one header as "already credentialed" rather
    /// than trying to out-rank what the frontend brought.
    fn credential_present(&self, incoming: &HeaderMap) -> bool {
        incoming.contains_key(header::AUTHORIZATION)
    }

    /// Inject this provider's credential headers into the outgoing
    /// request. The server calls this **only** when
    /// [`Provider::credential_present`] says the incoming request carries no
    /// credential of its own.
    ///
    /// A provider with no resolved key injects nothing, and the upstream's
    /// 401 body passes through unchanged — that passthrough is the visible
    /// verification of the wiring (ledger-proxy lesson).
    fn inject_auth(&self, outgoing: &mut HeaderMap);

    /// Remove from the outgoing headers any credential that belongs to a
    /// different provider and must never reach this one. The server calls
    /// this on every request, before the pass-through-when-present check,
    /// so a frontend's credential for one backend cannot ride a route to
    /// another. The default removes nothing: a provider that only ever
    /// sees its own protocol's frontend has no foreign credential to meet.
    fn strip_foreign_credentials(&self, outgoing: &mut HeaderMap) {
        let _ = outgoing;
    }

    /// The operator's model routing map for this backend, when one is
    /// configured (`[providers.<id>.model_map]`, the same
    /// env-typed pattern the predecessor used):
    /// the final routing stage consults it, and force-newest
    /// previews through it (recency reads the identity a request would be
    /// mapped to, never the asked model). `None` — the default — for
    /// providers without a map.
    fn model_map(&self) -> Option<&crate::middleware::model_map::ModelMap> {
        None
    }

    /// Whether `model` on this backend accepts a mid-conversation effort
    /// change (`output_config` on a `role: "system"` message). Where it does
    /// not, the server strips the field (invariant 6) rather than let the
    /// upstream 400 every turn of the session. `true` by default: the field
    /// is Anthropic's own.
    fn accepts_message_effort(&self, model: &str) -> bool {
        let _ = model;
        true
    }

    /// Parse this provider's meter snapshot from one upstream response's
    /// headers, when this provider is a **meter source** (plan: quota
    /// gate — "Anthropic sub is the only meter source today. The meter
    /// interface is open so codex sub's usage limits can become one").
    /// `None` — the default — for providers without meters: the snapshot
    /// the gate reads must never be overwritten by a backend that has no
    /// quota to report.
    fn meters(&self, headers: &HeaderMap) -> Option<Value> {
        let _ = headers;
        None
    }

    /// Whether this provider's quota is a rate-limit window — the
    /// providers that override [`Provider::meters`]. Where it is not, a
    /// re-read is billed rather than metered, and the cold notice must not
    /// say otherwise.
    fn is_meter_source(&self) -> bool {
        false
    }
}
