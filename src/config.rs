//! Runtime configuration (`toker.toml`) load and save.
//!
//! Plan: "Setup wizard" — no config files written by hand unless wanted;
//! `toker.toml` exists for hand-editing later, patched atomically via
//! `toml_edit` (formatting-preserving) by setup.

/// Placeholder for the `toker.toml` model: backend auth, per-frontend-protocol
/// defaults, route-level middleware toggles, wake/sleep/ping options.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Config {
    /// Placeholder; grows into the `toker.toml` schema.
    pub placeholder: (),
}
