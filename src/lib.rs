//! toker — unified local proxy toolsuite: the library crate.
//!
//! Design: docs/plans/toker-toolsuite.md. Three layers: frontend protocol
//! adapters ([proto]) → middleware chain ([middleware]) → backend providers
//! ([providers]), with [ir] as the canonical request model they share,
//! [store] as the SQLite ledger, [server] as the listener, [tui] as the
//! dashboard, [setup] as the wizard's library half (the tested
//! modules the interactive `toker setup` composes), [timers] as the
//! wake/hold/ping subsystem (the hidden verbs the wizard-installed
//! systemd units invoke), and [release] as the quota gate's grant and
//! revoke writes, shared by the markers and the TUI. The binary
//! (src/main.rs) is the CLI surface over these modules.
//!
//! The lib/bin split exists so each unit can land its full public API before
//! the next unit wires it in: `pub` items here are the crate's public
//! surface and stay lint-clean without dead-code suppression. Scaffold
//! placeholders that have not been implemented yet are doc-only; their
//! units replace them wholesale.

pub mod catalog;
pub mod cmds;
pub mod config;
pub mod export;
pub mod import;
pub mod ir;
pub mod middleware;
pub mod observe;
pub mod picker;
pub mod proto;
pub mod providers;
pub mod release;
pub mod secrets;
pub mod server;
pub mod setup;
pub mod store;
pub mod timers;
pub mod translate;
pub mod tui;
pub mod watch;
