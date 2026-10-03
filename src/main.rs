//! toker — unified local proxy toolsuite: the binary/CLI surface.
//!
//! Design: docs/plans/toker-toolsuite.md. Three layers: frontend protocol
//! adapters ([`toker::proto`]) → middleware chain ([`toker::middleware`]) →
//! backend providers ([`toker::providers`]), with [`toker::ir`] as the
//! canonical request model they share, [`toker::store`] as the SQLite
//! ledger, [`toker::server`] as the listener, and [`toker::tui`] as the
//! dashboard. The modules live in the `toker` library crate (src/lib.rs).

use toker::cmds;

use clap::{Parser, Subcommand};

/// toker: local LLM proxy, ledger, and dashboard.
#[derive(Debug, Parser)]
#[command(name = "toker", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Subcommands. The timer verbs are hidden from everyday use — they are
/// invoked by the systemd wake/hold/ping timer units (see [cmds]).
#[derive(Debug, Subcommand)]
enum Command {
    /// Run the proxy server (plan: Server core).
    Serve,
    /// Run the ratatui dashboard (plan: TUI).
    Tui,
    /// Print ledger-derived reports (plan: `toker report`).
    Report,
    /// Run the interactive setup wizard (plan: Setup wizard).
    Setup,
    /// Show configuration and backend status (plan: Credentials).
    Status,
    /// Ingest ctp's usage.jsonl into the ledger (plan: Storage).
    Import,
    /// Emit the ledger as JSONL for greppability (plan: Storage).
    Export,

    /// Arm the wake timer's quota window (plan: Sleep lock, wake, ping).
    #[command(hide = true)]
    WakeArm,
    /// Extend the user hold timer, 15 m (plan: Sleep lock, wake, ping).
    #[command(hide = true)]
    Hold,
    /// Open a ping quota window, lateness guard >10 min
    /// (plan: Sleep lock, wake, ping).
    #[command(hide = true)]
    PingWindow,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(cmds::serve()),
        Command::Tui => not_implemented("tui"),
        Command::Report => not_implemented("report"),
        Command::Setup => not_implemented("setup"),
        Command::Status => cmds::status(),
        Command::Import => not_implemented("import"),
        Command::Export => not_implemented("export"),
        Command::WakeArm => cmds::wake_arm(),
        Command::Hold => cmds::hold(),
        Command::PingWindow => cmds::ping_window(),
    }
}

fn not_implemented(what: &str) -> anyhow::Result<()> {
    eprintln!("not implemented yet: {what}");
    Ok(())
}
