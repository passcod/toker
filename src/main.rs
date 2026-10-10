//! toker — unified local proxy toolsuite: the binary/CLI surface.
//!
//! The implemented design is documented under docs/internals/. Three layers:
//! frontend protocol adapters ([`toker::proto`]) → middleware chain
//! ([`toker::middleware`]) → backend providers ([`toker::providers`]), with
//! [`toker::ir`] as the canonical request model they share, [`toker::store`] as the SQLite
//! ledger, [`toker::server`] as the listener, and [`toker::tui`] as the
//! dashboard. The modules live in the `toker` library crate (src/lib.rs).

use toker::cmds;
use toker::export;
use toker::store::{CostKind, KindFilter, RequestFilter};

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
        /// from days the ledger already holds, newest first. Default: the
        /// fewest that clear the election's bar.
        #[arg(long = "days")]
        days: Option<u32>,
        /// The prompt ceiling to grant alongside the days, in tokens.
        /// Default: the family's best observed. It only ever raises the
        /// ceiling, and never declares context capacity.
        #[arg(long = "max-prompt")]
        max_prompt: Option<u64>,
        /// Print what would be granted; change nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Restart the running service without cutting a response if it can
    /// be helped: wait for a moment with nothing in flight, ask it to
    /// drain and exit, and wait for systemd to start the new binary. If
    /// that does not happen in time (no quiet moment, no answer, a drain
    /// that does not end), tell it to stop without waiting for connections;
    /// it still exits cleanly, and clients retry a cut stream. Ctrl-C
    /// while waiting leaves it untouched.
    Restart {
        /// Stop without waiting for connections if no quiet moment comes
        /// within this span, a whole number with a unit (90s, 10m, 1h). Default 30s.
        #[arg(long = "max-wait", value_name = "SPAN", value_parser = cmds::wait_arg)]
        max_wait: Option<std::time::Duration>,
        /// Do not wait for a quiet moment. The old instance still gets 30s
        /// to finish every response under way before it is told to stop
        /// without waiting; new connections wait for the next instance
        /// meanwhile.
        #[arg(long = "now", conflicts_with = "max_wait")]
        now: bool,
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
    /// Claude Code's `/model` rows for openrouter models.
    Picker {
        #[command(subcommand)]
        command: PickerCommand,
    },
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
    /// Emit the ledger's rows as JSONL on stdout, oldest first, one
    /// object per row keyed by column name; NULL columns are omitted.
    Export {
        /// Ledger path; TOKER_DB env or the config default when omitted.
        #[arg(long = "db", value_name = "PATH")]
        db: Option<PathBuf>,
        /// Only rows at or after this time: RFC 3339
        /// (2026-10-05T09:00:00Z) or a span before now (90m, 2h, 3d, 1w).
        #[arg(long = "since", value_name = "TIME", value_parser = export::instant_arg)]
        since: Option<i64>,
        /// Only rows before this time (exclusive); same forms as --since.
        #[arg(long = "until", value_name = "TIME", value_parser = export::instant_arg)]
        until: Option<i64>,
        /// Only rows whose session id starts with this.
        #[arg(long = "session", value_name = "PREFIX")]
        session: Option<String>,
        /// Which rows: all (default), measurement (API measurements),
        /// proxy (every proxy-written row), or one kind (blocked,
        /// released, cold, cold-quiet, awake, error, fidelity-drift).
        #[arg(long = "kind", value_name = "KIND", default_value = "all", value_parser = export::kind_arg)]
        kind: KindFilter,
    },
    /// Print a PROOF line, once each, when a served response proves a
    /// context window: an exact 1M window holding a prompt over 200k,
    /// or a prompt over a provider-declared ceiling. One pass per run;
    /// meant to be re-run by a monitor.
    WatchContextWindow {
        /// Ledger path; TOKER_DB env or the config default when omitted.
        #[arg(long = "db", value_name = "PATH")]
        db: Option<PathBuf>,
        /// Only rows at or after this time (RFC 3339, or a span before
        /// now: 90m, 2h, 3d, 1w). Default: when the state file's first
        /// pass ran, or now on a first pass.
        #[arg(long = "since", value_name = "TIME", value_parser = export::instant_arg)]
        since: Option<i64>,
        /// The seen-set file. Default: watch-context-window.json beside
        /// the ledger.
        #[arg(long = "state", value_name = "PATH")]
        state: Option<PathBuf>,
        /// Only sessions whose id starts with one of these.
        #[arg(value_name = "SESSION_PREFIX")]
        sessions: Vec<String>,
    },

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

/// `toker picker` verbs.
#[derive(Debug, Subcommand)]
enum PickerCommand {
    /// Fetch openrouter's models listing, apply the picker rules, and
    /// write the rows into `~/.claude/settings.json` (setup and the daily
    /// picker timer run this).
    Sync {
        /// Print the rows; change nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Print the built-in picker rules as toker.toml text, to copy and
    /// edit.
    Defaults,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let served = runtime.block_on(cmds::serve());
            // A forced shutdown returns with connections still open:
            // dropping the runtime cancels them, running their destructors,
            // and waits on any blocking work. Bound that wait so a stuck
            // one cannot hold a restart up.
            runtime.shutdown_timeout(std::time::Duration::from_secs(5));
            served
        }
        Command::Promote {
            model,
            days,
            max_prompt,
            dry_run,
        } => cmds::promote(model, days, max_prompt, dry_run),
        Command::Restart { max_wait, now } => cmds::restart(max_wait, now),
        Command::Tui { window_mins, db } => cmds::tui(window_mins, db),
        Command::Setup => cmds::setup(),
        Command::Status => cmds::status(),
        Command::Picker { command } => match command {
            PickerCommand::Sync { dry_run } => cmds::picker_sync(dry_run),
            PickerCommand::Defaults => {
                print!("{}", toker::picker::defaults_toml());
                Ok(())
            }
        },
        Command::Import {
            from,
            db,
            cost_kind,
            force,
            dry_run,
        } => cmds::import(from, db, cost_kind, force, dry_run),
        Command::Export {
            db,
            since,
            until,
            session,
            kind,
        } => cmds::export(
            db,
            RequestFilter {
                since_ms: since,
                until_ms: until,
                session_prefix: session,
                kind,
            },
        ),
        Command::WatchContextWindow {
            db,
            since,
            state,
            sessions,
        } => cmds::watch_context_window(db, since, state, sessions),
        Command::WakeArm => cmds::wake_arm(),
        Command::Hold { for_ } => cmds::hold(for_),
        Command::PingWindow { slot } => cmds::ping_window(slot),
    }
}
