//! Route-scoped middleware chain.
//!
//! Plan: "Middleware" — frontend×backend routes enable toggles: recording +
//! costing, lane tracking + sleep lock, cold gate, compaction retarget,
//! force-newest model rewrite, model routing map, quota gate + release marker
//! stripping, ping tagging. Middleware transforms the IR; the backend adapter
//! serialises the result. There is no passthrough code path — passthrough is
//! what the IR produces when nothing transforms it.
//!
//! The decision middlewares, each a faithful port of the predecessor proxy:
//!
//! - [`quota`] — the quota gate: meters → block-or-forward,
//!   the release marker, the synthetic assistant turn a block is answered
//!   with;
//! - [`cold`] — the cold-cache gate and the compaction retarget (plus
//!   the burn/fit pieces its outlook calls): the
//!   once-per-idle-spell notice, the quota outlook that can withhold it,
//!   and the only transform that changes model-visible prompt structure;
//! - [`lanes`] — the lane table (`sessionId|toolsHash`, the lane rule
//!   as the predecessor's internal docs stated it) with its TTL stickiness, ping tagging, prune
//!   policy, and restart reseed — the substrate both gates read;
//! - [`awake`] — the idle-sleep lock: live lanes + in-flight want-to-hold, the
//!   platform command table (GNOME → `gnome-session-inhibit`, other
//!   Linux → `systemd-inhibit --what=idle --mode=block`), the detached
//!   PID-watching child, the 5-minute retry backoff, and the held/want
//!   flip bookkeeping — Linux v1, idle-only on purpose;
//! - [`models`] — the learned model store (days served + maxPrompt — the
//!   family parsing and day-based newest election) and the
//!   in-memory recently-served map, plus the merge semantics the
//!   `/_toker/models/merge` control endpoint carries;
//! - [`force_newest`] — the force-newest model rewrite (the
//!   target decision plus the block that sequences it): move a request onto its family's learned
//!   newest, only where no cache can be lost, never down, sticky once
//!   moved;
//! - [`model_map`] — the model routing map: opaque
//!   operator routing policy, the pure parse/match/rewrite interface the
//!   `route-identity` parity case pins (config and server wiring are the
//!   model-routing unit's);
//! - [`system_change`] — capture-time system-prompt change localisation:
//!   the lane's previous row from the ledger, the change bounded to a
//!   block, an 8 KiB step or a tail window, and the rule that keeps the
//!   ladders only where the prompt changed or the lane began.

pub mod awake;
pub mod canonical;
pub mod cold;
pub mod force_newest;
pub mod lanes;
pub mod model_map;
pub mod models;
pub mod notice;
pub mod quota;
pub mod system_change;
