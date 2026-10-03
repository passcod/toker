//! Hand-verified provider catalogues: list prices and context-window
//! ceilings.
//!
//! Plan: Model catalogues — "hand-verified per-provider table with a
//! freshness date" and "hand-verified catalogue per provider (exact
//! normalised identities)". Both are ports of the user's Node code in
//! claude-token-proxy, which encodes weeks of production measurement, and
//! both are pure lookup tables with no I/O.
//!
//! - [`pricing`]: ctp's `pricing.mjs` — per-Mtok rates stored explicitly
//!   (cache-read is never derived as a ratio of input: some models read at
//!   0.025×), fast-mode repricing, the US-geo 1.1× multiplier, and the
//!   web-search per-call price. ctp's plan/overage constants are
//!   deliberately NOT ported (plan: "Legacy ctp tooling … the plan/overage
//!   constants in `pricing.mjs` die with ctp").
//! - [`windows`]: ctp's `models.mjs` context-window catalogue — exact
//!   identities, fixed/beta/declared capabilities, dated phases for
//!   historical rows, and the reconciliation rules that decide
//!   exact/declared/unknown.
//!
//! Freshness discipline (ctp `PRICING_VERIFIED_ON`): each table carries the
//! date it was last verified against the provider's published figures.
//! Staleness is a human duty — re-verify and move the date — but the date
//! must exist, and a test pins that it does.
//!
//! Unknown stays unknown everywhere (invariant 3): a model missing from a
//! table returns `None`/`Unknown`, never a guess. The caller records the
//! absence (a NULL cost plus a one-time warning) rather than inventing a
//! number.

pub mod pricing;
pub mod windows;

pub use pricing::{
    CostBuckets, Pricing, Rates, US_GEO_MULTIPLIER, VERIFIED_ON as PRICING_VERIFIED_ON,
    WEB_SEARCH_USD_PER_REQUEST, normalise_model_id, price,
};
pub use windows::{
    CONTEXT_1M_BETA, ContextWindow, DeclaredWindow, VERIFIED_ON as WINDOWS_VERIFIED_ON,
    WindowRange, model_identity, resolve_context_window,
};
