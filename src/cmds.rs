//! Subcommand implementations, wired from the CLI surface in main.rs.
//!
//! The timer verbs serve the systemd wake/hold/ping units (plan: "Sleep
//! lock, wake, ping") and are registered as hidden subcommands so they are
//! reachable by the units but not part of the everyday CLI surface. The
//! verbs' logic lives in [`crate::timers`] behind seams (the spawner, the
//! claude client, the readback pacing); everything here is wiring — this
//! is the only place those seams meet the real world, exactly like
//! `setup` is the wizard's.
//! `serve`, `setup`, `status`, and `tui` are the wired commands: config →
//! store → server; the interactive wizard; the resolved-config/ledger
//! summary (plan: Credentials — status reports which key sources are in
//! use, never the values); and the ratatui dashboard (plan: TUI).

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use crate::server::Server;
use crate::store::{CostKind, Store};

/// `serve`: config → store → server, with tracing on.
pub async fn serve() -> anyhow::Result<()> {
    init_tracing();
    let config = Config::load()?;
    let store = Arc::new(Store::open(&config.db_path)?);
    tracing::info!(
        "serving openai_chat → {} , anthropic → {} (db: {})",
        config.openrouter.upstream,
        config.anthropic_sub.upstream,
        config.db_path.display()
    );
    Server::new(config, store)?.serve().await
}

/// `setup`: the interactive wizard (plan: "Setup wizard") — the ONLY
/// place the wizard's seams meet the real world: the inquire prompt,
/// the process systemctl runner, and this user's HOME-derived paths
/// ([`crate::setup::wizard::Paths::real`]). Everything with logic runs
/// scripted in [`crate::setup::wizard`]'s tests; everything here is
/// wiring. The wizard prints its own state summary and report.
pub fn setup() -> anyhow::Result<()> {
    use crate::setup::wizard::{InquirePrompt, Paths, ProcessRunner, VERIFY_TIMEOUT, Wizard};

    use anyhow::Context as _;

    let paths = Paths::real()?;
    let runner = ProcessRunner::new(paths.units_dir.clone());
    let mut prompt = InquirePrompt;
    let mut out = std::io::stdout();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the setup wizard's runtime")?
        .block_on(Wizard::new(&mut prompt, &runner, &paths, &mut out, VERIFY_TIMEOUT).run())?;
    Ok(())
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
    println!(
        "default backend (anthropic): {}",
        config.default_backend_anthropic
    );

    println!("openrouter: {}", config.openrouter.upstream);
    let sources = config.openrouter.key_sources();
    println!(
        "openrouter api key: env {} ({}), literal ({})",
        config.openrouter.api_key_env,
        if sources.env_set { "set" } else { "unset" },
        if sources.literal_set { "set" } else { "unset" },
    );

    println!("anthropic sub: {}", config.anthropic_sub.upstream);
    println!("anthropic api: {}", config.anthropic_api.upstream);
    let sources = config.anthropic_api.key_sources();
    println!(
        "anthropic api key: env {} ({}), literal ({})",
        config.anthropic_api.api_key_env,
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
    crate::tui::run(&db_path, window_mins, &config.transcript_roots)
}

/// `import`: ingest the predecessor proxy's usage.jsonl into the ledger
/// (plan: Storage).
/// `--db` overrides the path; otherwise `TOKER_DB` and the config default
/// apply (Config::load already layered env over file).
pub fn import(
    from: PathBuf,
    db: Option<PathBuf>,
    cost_kind: CostKind,
    force: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let config = Config::load()?;
    let db = db.unwrap_or(config.db_path);
    crate::import::run(crate::import::ImportOpts {
        from,
        db,
        cost_kind,
        force,
        dry_run,
    })
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

/// `hold --for=<mins>` (the hold timers' verb): hold the idle-sleep
/// lock for the span, then release — independently of the daemon's own
/// lock. The span parses strictly (minutes, fractional OK, optional
/// trailing `m`).
pub fn hold(for_arg: String) -> anyhow::Result<()> {
    let Some(minutes) = crate::timers::parse_hold_minutes(&for_arg) else {
        anyhow::bail!(
            "--for must be minutes — a number, fractional OK, optionally suffixed m — got {for_arg:?}"
        );
    };
    let mut spawner = crate::middleware::awake::ProcessSpawner;
    let mut out = std::io::stdout();
    crate::timers::hold_lock(
        &mut out,
        minutes,
        crate::middleware::awake::platform_command(
            crate::middleware::awake::INHIBIT_WHO,
            crate::timers::HOLD_WHY,
        ),
        &mut spawner,
        &mut |span| std::thread::sleep(span),
    )
}

/// `ping-window --slot=hh:mm` (the ping timers' verb): open a fresh
/// quota window with one tiny client request, then confirm the ping
/// row landed on the ledger. The lateness guard refuses a slot more
/// than 10 minutes past its fire time, with the reason and a non-zero
/// exit.
pub fn ping_window(slot: String) -> anyhow::Result<()> {
    let config = Config::load()?;
    let ping = crate::timers::PingConfig {
        db_path: &config.db_path,
        port: config.port,
        ping_header: &config.ping_header_name,
    };
    let mut out = std::io::stdout();
    let now = jiff::Timestamp::now().as_millisecond();
    crate::timers::ping_window(
        &mut out,
        &ping,
        &slot,
        now,
        &jiff::tz::TimeZone::system(),
        &crate::timers::ProcessCommandRunner,
        &mut |span| std::thread::sleep(span),
    )
}

/// `wake-arm`: a documented no-op — on Linux, wake is owned by the
/// systemd system timer (`WakeSystem=true`), which `toker setup`
/// installs and enables. The predecessor's one-shot `pmset schedule
/// wake` was macOS-only and has no counterpart here; the wizard never
/// installs anything for this verb.
pub fn wake_arm() -> anyhow::Result<()> {
    println!("{}", crate::timers::WAKE_ARM_NOTE);
    Ok(())
}
