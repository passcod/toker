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
    /// Grant a served model the days to become its family's rewrite
    /// target early (the promote-model handover).
    Promote {
        /// The exact model identity, as the ledger recorded it.
        #[arg(long = "model")]
        model: String,
        /// How many days the model should hold after the grant, drawn
        /// from days the ledger already holds, newest first.
        #[arg(long = "days", default_value_t = 7)]
        days: u32,
        /// A prompt ceiling to grant alongside the days, in tokens.
        #[arg(long = "max-prompt")]
        max_prompt: Option<u64>,
    },
    /// Run the ratatui dashboard (plan: TUI).
    Tui {
        /// Dashboard window length in minutes. Default 60: the
        /// anthropic cache's longest TTL is one hour, so the window
        /// that answers "is my cache still warm" is the last hour
        /// (openai-family caches are shorter — the window still shows
        /// them, just with more history attached).
        #[arg(
            long = "window-mins",
            default_value_t = 60,
            value_parser = clap::value_parser!(u64).range(1..=1440)
        )]
        window_mins: u64,
        /// Ledger path; TOKER_DB env or the config default when omitted.
        #[arg(long = "db", value_name = "PATH")]
        db: Option<PathBuf>,
    },
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

    /// A documented no-op: wake is owned by the systemd system timer
    /// (plan: Sleep lock, wake, ping). The predecessor's one-shot
    /// `pmset` arm was macOS-only.
    #[command(hide = true)]
    WakeArm,
    /// Hold the idle-sleep lock for a span (the hold timers' verb; the
    /// unit passes 15m). Fractional minutes accepted; parsed strictly.
    #[command(hide = true)]
    Hold {
        /// The span in minutes: a number, fractional OK, an optional
        /// trailing `m`.
        #[arg(long = "for", value_name = "MINUTES")]
        for_: String,
    },
    /// Open a ping quota window with one tiny client request (the ping
    /// timers' verb); refuses a slot more than 10 minutes past its
    /// fire time (plan: Sleep lock, wake, ping).
    #[command(hide = true)]
    PingWindow {
        /// The slot whose window this ping opens, `hh:mm`.
        #[arg(long = "slot", value_name = "HH:MM")]
        slot: String,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(cmds::serve()),
        Command::Promote {
            model,
            days,
            max_prompt,
        } => cmds::promote(model, days, max_prompt),
        Command::Tui { window_mins, db } => cmds::tui(window_mins, db),
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
        Command::Hold { for_ } => cmds::hold(for_),
        Command::PingWindow { slot } => cmds::ping_window(slot),
    }
}

fn not_implemented(what: &str) -> anyhow::Result<()> {
    eprintln!("not implemented yet: {what}");
    Ok(())
}
