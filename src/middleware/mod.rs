//! Route-scoped middleware chain.
//!
//! Recording and costing, lane tracking and the sleep lock, the cold gate and
//! compaction retarget, the quota gate with release marker stripping, ping
//! tagging. Middleware transforms the canonical request; the backend adapter
//! serialises the result. There is no passthrough code path: passthrough is
//! what the canonical form produces when nothing transforms it. Which model a
//! request goes to is not decided here: policy (`crate::policy`) owns routing,
//! aliases and force-newest upgrades.
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
//! - [`model_map`] — the config's `model_map` tables, parsed once at load for
//!   revision one's generator, and the lexical splice that writes policy's
//!   effective models into `count_tokens` and batch bodies;
//! - [`system_change`] — capture-time system-prompt change localisation:
//!   the lane's previous row from the ledger, the change bounded to a
//!   block, an 8 KiB step or a tail window, and the rule that keeps the
//!   ladders only where the prompt changed or the lane began.

pub mod awake;
pub mod canonical;
pub mod cold;
pub mod lanes;
pub mod model_map;
pub mod notice;
pub mod quota;
pub mod system_change;
