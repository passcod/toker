//! Backend providers.
//!
//! Plan: "Backend providers" — providers within a protocol share an adapter
//! (see [crate::proto]) and differ in auth, cost semantics (billed /
//! estimated / plan-equivalent), and meter parsing (e.g. anthropic sub's
//! `anthropic-ratelimit-*` headers).
