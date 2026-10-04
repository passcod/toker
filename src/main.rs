//! toker — unified local proxy toolsuite: the binary/CLI surface.
//!
//! Design: docs/plans/toker-toolsuite.md. Three layers: frontend protocol
//! adapters ([`toker::proto`]) → middleware chain ([`toker::middleware`]) →
//! backend providers ([`toker::providers`]), with [`toker::ir`] as the
//! canonical request model they share, [`toker::store`] as the SQLite
//! ledger, [`toker::server`] as the listener, and [`toker::tui`] as the
//! dashboard. The modules live in the `toker` library crate (src/lib.rs).

use toker::cmds;
use toker::store::CostKind;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
    Tui {
        /// Dashboard window length in minutes.
        #[arg(
            long = "window-mins",
            default_value_t = 30,
            value_parser = clap::value_parser!(u64).range(1..=1440)
        )]
        window_mins: u64,
        /// Ledger path; TOKER_DB env or the config default when omitted.
        #[arg(long = "db", value_name = "PATH")]
        db: Option<PathBuf>,
    },
    /// Print ledger-derived reports (plan: `toker report`).
    Report,
    /// Run the interactive setup wizard (plan: Setup wizard).
    Setup,
    /// Show configuration and backend status (plan: Credentials).
    Status,
    /// Ingest the predecessor proxy's usage.jsonl into the ledger (plan:
    /// Storage).
    Import {
        /// The predecessor proxy's usage.jsonl to import.
        #[arg(long = "from", value_name = "PATH")]
        from: PathBuf,
        /// Ledger path; TOKER_DB env or the config default when omitted.
        #[arg(long = "db", value_name = "PATH")]
        db: Option<PathBuf>,
        /// Cost semantics for imported costUsd (plan: Storage); the
        /// predecessor priced
        /// at list rates on a subscription, so plan-equivalent is the
        /// default and api-era logs want `estimated`.
        #[arg(long = "cost-kind", default_value = "plan_equivalent")]
        cost_kind: CostKind,
        /// Re-import the whole file even though a checkpoint says otherwise
        /// (the ledger is insert-only, so the old rows stay: duplicates).
        #[arg(long)]
        force: bool,
        /// Parse and report; insert nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
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
        Command::Tui { window_mins, db } => cmds::tui(window_mins, db),
        Command::Report => not_implemented("report"),
        Command::Setup => cmds::setup(),
        Command::Status => cmds::status(),
        Command::Import {
            from,
            db,
            cost_kind,
            force,
            dry_run,
        } => cmds::import(from, db, cost_kind, force, dry_run),
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
