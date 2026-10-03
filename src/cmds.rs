//! Subcommand implementations, wired from the CLI surface in main.rs.
//!
//! The timer verbs serve the systemd wake/hold/ping units (plan: "Sleep
//! lock, wake, ping") and are registered as hidden subcommands so they are
//! reachable by the units but not part of the everyday CLI surface.
//! `serve`, `status`, and `tui` are the wired commands: config → store →
//! server; the resolved-config/ledger summary (plan: Credentials — status
//! reports which key sources are in use, never the values); and the
//! ratatui dashboard (plan: TUI).

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use crate::server::Server;
use crate::store::Store;

/// `serve`: config → store → server, with tracing on.
pub async fn serve() -> anyhow::Result<()> {
    init_tracing();
    let config = Config::load()?;
    let store = Arc::new(Store::open(&config.db_path)?);
    tracing::info!(
        "serving openai_chat → {} (db: {})",
        config.openrouter.upstream,
        config.db_path.display()
    );
    Server::new(config, store)?.serve().await
}

/// `status`: the resolved config (sans secrets) plus ledger summary.
pub fn status() -> anyhow::Result<()> {
    let config = Config::load()?;
    let store = Store::open(&config.db_path)?;

    println!("port: {}", config.port);
    println!("db: {}", config.db_path.display());
    println!(
        "session headers: {}",
        config.session_header_names.join(", ")
    );
    println!(
        "default backend (openai_chat): {}",
        config.default_backend_openai_chat
    );

    println!("openrouter: {}", config.openrouter.upstream);
    let sources = config.openrouter.key_sources();
    println!(
        "openrouter api key: env {} ({}), literal ({})",
        config.openrouter.api_key_env,
        if sources.env_set { "set" } else { "unset" },
        if sources.literal_set { "set" } else { "unset" },
    );

    let rows = store.count_requests()?;
    println!("requests: {rows}");
    match store.requests_since(0, 1)?.pop() {
        Some(row) => println!("last request: {}", fmt_ts(row.ts_ms)),
        None => println!("last request: none"),
    }
    Ok(())
}

/// `tui`: the ratatui dashboard over the ledger (plan: TUI). `--db`
/// overrides the path; otherwise `TOKER_DB` and the config default apply
/// (Config::load already layered env over file).
pub fn tui(window_mins: u64, db: Option<PathBuf>) -> anyhow::Result<()> {
    let config = Config::load()?;
    let db_path = db.unwrap_or(config.db_path);
    crate::tui::run(&db_path, window_mins)
}

/// A local-clock rendering of a row ts (the system zone; UTC-shaped on
/// any failure — the timestamp is never hidden by a formatting error).
fn fmt_ts(ts_ms: i64) -> String {
    match jiff::Timestamp::from_millisecond(ts_ms) {
        Ok(ts) => ts.to_zoned(jiff::tz::TimeZone::system()).to_string(),
        Err(_) => format!("{ts_ms} (ms since epoch)"),
    }
}

/// Tracing with env-filter, defaulting to info.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// `wake-arm`: arm the wake timer's quota window.
pub fn wake_arm() -> anyhow::Result<()> {
    eprintln!("not implemented yet: wake-arm");
    Ok(())
}

/// `hold`: extend the user hold timer by 15 minutes.
pub fn hold() -> anyhow::Result<()> {
    eprintln!("not implemented yet: hold");
    Ok(())
}

/// `ping-window`: open a ping quota window via `claude -p`.
pub fn ping_window() -> anyhow::Result<()> {
    eprintln!("not implemented yet: ping-window");
    Ok(())
}
