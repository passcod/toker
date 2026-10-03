//! Route-scoped middleware chain.
//!
//! Plan: "Middleware" — frontend×backend routes enable toggles: recording +
//! costing, lane tracking + sleep lock, cold gate, compaction retarget,
//! force-newest model rewrite, model routing map, quota gate + release marker
//! stripping, ping tagging. Middleware transforms the IR; the backend adapter
//! serialises the result. There is no passthrough code path — passthrough is
//! what the IR produces when nothing transforms it.
//!
//! The decision middlewares, each a faithful ctp port:
//!
//! - [`quota`] — the quota gate (ctp limit.mjs): meters → block-or-forward,
//!   the release marker, the synthetic assistant turn a block is answered
//!   with;
//! - [`cold`] — the cold-cache gate and the compaction retarget (ctp
//!   cold.mjs plus the burn/fit pieces its outlook calls): the
//!   once-per-idle-spell notice, the quota outlook that can withhold it,
//!   and the only transform that changes model-visible prompt structure;
//! - [`lanes`] — the lane table (`sessionId|toolsHash`, ctp docs/internals/
//!   lanes.md's lane rule) with its TTL stickiness, ping tagging, prune
//!   policy, and restart reseed — the substrate both gates read;
//! - [`models`] — the learned model store (days served + maxPrompt, ctp
//!   models.mjs's family parsing and day-based newest election) and the
//!   in-memory recently-served map, plus the merge semantics the
//!   `/_toker/models/merge` control endpoint carries.

pub mod cold;
pub mod lanes;
pub mod models;
pub mod notice;
pub mod quota;
