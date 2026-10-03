//! Backend providers.
//!
//! Plan: "Backend providers" — providers within a protocol share an adapter
//! (see [crate::proto]) and differ in auth, cost semantics (billed /
//! estimated / plan-equivalent), and meter parsing (e.g. anthropic sub's
//! `anthropic-ratelimit-*` headers). Phase 1 wires exactly one
//! ([`openrouter`]); the trait below stays deliberately small and grows one
//! method at a time as the anthropic/codex units need it.

mod openrouter;

pub use openrouter::OpenRouter;

use axum::http::HeaderMap;
use reqwest::Url;

/// A backend provider: identity, upstream endpoint mapping, and credential
/// injection. Same-protocol providers (openrouter, openai api, lunaroute)
/// all satisfy this today; cross-protocol backends add their adapter in
/// later phases.
pub trait Provider: Send + Sync {
    /// The stable provider id — the ledger's `provider` column and the
    /// backend half of `frontend:backend` routes.
    fn id(&self) -> &str;

    /// The upstream URL for an incoming frontend path (query included when
    /// the request carries one). E.g. `/v1/chat/completions` →
    /// `https://openrouter.ai/api/v1/chat/completions` — the mapping from
    /// frontend paths to this provider's upstream shape lives here.
    fn endpoint(&self, path: &str) -> Url;

    /// Inject this provider's credential headers into the outgoing
    /// request. The server calls this **only** when the incoming request
    /// carries no Authorization header of its own — pass-through-when-
    /// present (plan: Credentials): a frontend that brings its own
    /// credential keeps it, verbatim.
    ///
    /// A provider with no resolved key injects nothing, and the upstream's
    /// 401 body passes through unchanged — that passthrough is the visible
    /// verification of the wiring (ledger-proxy lesson).
    fn inject_auth(&self, outgoing: &mut HeaderMap);
}
