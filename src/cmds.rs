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
use anyhow::{Context as _, bail};

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
        &mut || jiff::Timestamp::now().as_millisecond(),
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
    // The predecessor's CTP_PING_CLAUDE and CTP_PING_MODEL, renamed: a
    // claude that is not on the unit's PATH, or a model to try.
    let claude = std::env::var("TOKER_PING_CLAUDE").unwrap_or_else(|_| "claude".to_owned());
    let model =
        std::env::var("TOKER_PING_MODEL").unwrap_or_else(|_| crate::timers::PING_MODEL.to_owned());
    let custom_headers = std::env::var("ANTHROPIC_CUSTOM_HEADERS").ok();
    let ping = crate::timers::PingConfig {
        db_path: &config.db_path,
        port: config.port,
        ping_header: &config.ping_header_name,
        claude: &claude,
        model: &model,
        custom_headers: custom_headers.as_deref(),
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
/// `toker promote` — the promote-model handover (the control
/// endpoint is the same one the predecessor's promote script used):
/// grant a served model the days (and optionally the prompt ceiling)
/// to become its family's rewrite target early. `days` local dates
/// ending today — the default of 7 clears the election bar at any log
/// age (the bar caps at 7), so promote is a deliberate override, not a
/// measurement.
pub fn promote(model: String, days: u32, max_prompt: Option<u64>) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(promote_run(model, days, max_prompt))
}

async fn promote_run(model: String, days: u32, max_prompt: Option<u64>) -> anyhow::Result<()> {
    let config = Config::load()?;
    let body = crate::middleware::models::promote_request_body(&model, days, max_prompt);
    let url = format!("http://127.0.0.1:{}/_toker/models/merge", config.port);
    let client = reqwest::Client::builder().build()?;
    let response = client
        .post(&url)
        .header("x-toker-control", "models-merge")
        .json(&body)
        .send()
        .await
        .with_context(|| format!("posting to {url} — is toker serving?"))?;
    let status = response.status();
    let reply: serde_json::Value = response.json().await.unwrap_or_default();
    if status.is_success() {
        let targets = reply
            .get("targets")
            .and_then(|targets| targets.as_object())
            .cloned()
            .unwrap_or_default();
        for (family, target) in targets {
            println!("{family} → {target}");
        }
        return Ok(());
    }
    if let Some("unseen") = reply.get("error").and_then(serde_json::Value::as_str) {
        let known = reply
            .get("known")
            .and_then(|known| known.as_array())
            .map(|known| {
                known
                    .iter()
                    .filter_map(|model| model.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        bail!(
            "{model:?} has never served — the merge only promotes models \
         the ledger has seen. Known: {}",
            known.join(", ")
        );
    }
    bail!("the merge refused: {} {}", status.as_u16(), reply)
}

pub fn wake_arm() -> anyhow::Result<()> {
    println!("{}", crate::timers::WAKE_ARM_NOTE);
    Ok(())
}
