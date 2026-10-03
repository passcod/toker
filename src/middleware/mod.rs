//! Route-scoped middleware chain.
//!
//! Plan: "Middleware" — frontend×backend routes enable toggles: recording +
//! costing, lane tracking + sleep lock, cold gate, compaction retarget,
//! force-newest model rewrite, model routing map, quota gate + release marker
//! stripping, ping tagging. Middleware transforms the IR; the backend adapter
//! serialises the result.
