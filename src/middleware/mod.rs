//! Route-scoped middleware chain.
//!
//! Plan: "Middleware" — frontend×backend routes enable toggles: recording +
//! costing, lane tracking + sleep lock, cold gate, compaction retarget,
//! force-newest model rewrite, model routing map, quota gate + release marker
//! stripping, ping tagging. Middleware transforms the IR; the backend adapter
//! serialises the result. There is no passthrough code path — passthrough is
//! what the IR produces when nothing transforms it.
//!
//! This unit lands the substrate the decision middlewares read:
//!
//! - [`lanes`] — the lane table (`sessionId|toolsHash`, ctp docs/internals/
//!   lanes.md's lane rule) with its TTL stickiness, ping tagging, prune
//!   policy, and restart reseed;
//! - [`models`] — the learned model store (days served + maxPrompt, ctp
//!   models.mjs's family parsing and day-based newest election) and the
//!   in-memory recently-served map, plus the merge semantics the
//!   `/_toker/models/merge` control endpoint carries.
//!
//! The cold gate and the force-newest rewrite build on these next; nothing
//! here decides a request yet.

pub mod lanes;
pub mod models;
pub mod notice;
pub mod quota;
