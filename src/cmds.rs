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
//! `restart` drives the running service's `/_toker/status` and
//! `/_toker/shutdown` to restart it without cutting a response.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::server::Server;
use crate::store::{CostKind, Store};
use anyhow::bail;

/// `serve`: config → store → server, with tracing on.
pub async fn serve() -> anyhow::Result<()> {
    init_tracing();
    let config = Config::load()?;
    let store = Arc::new(Store::open(&config.db_path)?);
    tracing::info!(
        "serving anthropic → {}, openai_chat → {} (db: {})",
        config
            .default_backend_anthropic
            .as_deref()
            .unwrap_or("not configured"),
        config
            .default_backend_openai_chat
            .as_deref()
            .unwrap_or("not configured"),
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
    use crate::secrets::OsKeyring;
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
        .block_on(
            Wizard::new(
                &mut prompt,
                &runner,
                &OsKeyring,
                &paths,
                &mut out,
                VERIFY_TIMEOUT,
            )
            .run(),
        )?;
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
        "default backend (anthropic): {}",
        config
            .default_backend_anthropic
            .as_deref()
            .unwrap_or("none — no anthropic backend is enabled")
    );
    println!(
        "default backend (openai_chat): {}",
        config
            .default_backend_openai_chat
            .as_deref()
            .unwrap_or("none — no openai_chat backend is enabled")
    );

    // Enabled backends only: a block's presence is what enables one, so
    // an absent block has nothing to report.
    let enabled = config.enabled_backends();
    if enabled.is_empty() {
        println!("backends: none enabled — add a [providers.<name>] block or run `toker setup`");
    }
    if let Some(sub) = &config.anthropic_sub {
        println!("anthropic_sub: {}", sub.upstream);
    }
    if let Some(api) = &config.anthropic_api {
        println!("anthropic_api: {}", api.upstream);
        println!(
            "anthropic_api key: {}",
            key_sources_line(&api.api_key_env, api.key_sources())
        );
    }
    if let Some(codex) = &config.codex_sub {
        println!(
            "codex_sub: {} (login {})",
            codex.upstream,
            codex.auth_path.display()
        );
    }
    if let Some(openrouter) = &config.openrouter {
        println!("openrouter: {}", openrouter.upstream);
        println!(
            "openrouter key: {}",
            key_sources_line(&openrouter.api_key_env, openrouter.key_sources())
        );
    }

    let rows = store.count_requests()?;
    println!("requests: {rows}");
    match store.requests_since(0, 1)?.pop() {
        Some(row) => println!("last request: {}", fmt_ts(row.ts_ms)),
        None => println!("last request: none"),
    }
    Ok(())
}

/// One api-key backend's key sources for `status`: which are set,
/// never a value (invariant 2).
///
/// The env verdict is this shell's environment, which the service does
/// not share (it sees the systemd user manager's), so the line says so.
/// The keyring is reported as configured, never read here: only the
/// service reads it.
fn key_sources_line(env: &str, sources: crate::config::KeySources) -> String {
    format!(
        "env {env} ({} in this shell; the service reads the systemd user environment), \
         keyring ({}), literal ({}); with none, the frontend's own key passes through",
        if sources.env_set { "set" } else { "unset" },
        if sources.keyring_configured {
            "configured"
        } else {
            "unused"
        },
        if sources.literal_set { "set" } else { "unset" },
    )
}

/// `tui`: the ratatui dashboard over the ledger (plan: TUI). `--db`
/// overrides the path; otherwise `TOKER_DB` and the config default apply
/// (Config::load already layered env over file).
pub fn tui(window_mins: u64, db: Option<PathBuf>) -> anyhow::Result<()> {
    let config = Config::load()?;
    let db_path = db.unwrap_or(config.db_path);
    crate::tui::run(
        &db_path,
        window_mins,
        &config.transcript_roots,
        &config.gates,
    )
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

/// The ledger a reading verb opens: `--db` when given, else `TOKER_DB`
/// or the config default (the config is read only when it is needed, so
/// an explicit path works without one).
fn ledger_path(db: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match db {
        Some(db) => Ok(db),
        None => Ok(Config::load()?.db_path),
    }
}

/// `export`: the ledger's rows as JSONL on stdout (see [`crate::export`]
/// for the format). The ledger is opened read-only, so a wrong path
/// fails rather than creating one. A reader that goes away (`| head`)
/// ends the export quietly with success: it got what it asked for.
pub fn export(db: Option<PathBuf>, filter: crate::store::RequestFilter) -> anyhow::Result<()> {
    let store = Store::open_read_only(ledger_path(db)?)?;
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    match crate::export::write_jsonl(&store, &filter, &mut out) {
        Ok(_) => Ok(()),
        Err(error) if crate::export::is_broken_pipe(&error) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// `watch-context-window`: one pass of the context-window watch (see
/// [`crate::watch`]), printing each new proof once. `since_ms` wins
/// when given; otherwise the state file's recorded start, else now,
/// which is then recorded, so a monitor that re-runs the verb without a
/// `--since` keeps one window. The state file defaults to the state dir
/// beside the ledger. It is written only after the lines are printed,
/// so a proof whose line never reached the reader prints again next
/// pass.
pub fn watch_context_window(
    db: Option<PathBuf>,
    since_ms: Option<i64>,
    state_path: Option<PathBuf>,
    prefixes: Vec<String>,
) -> anyhow::Result<()> {
    use std::io::Write;
    let db = ledger_path(db)?;
    let state_path = state_path.unwrap_or_else(|| crate::watch::default_state_path(&db));
    let store = Store::open_read_only(&db)?;
    let mut state = crate::watch::WatchState::load(&state_path)?;
    let since_ms = since_ms
        .or(state.since_ms)
        .unwrap_or_else(|| jiff::Timestamp::now().as_millisecond());
    state.since_ms.get_or_insert(since_ms);
    // The daemon's models caches, read and never fetched: a missing
    // cache costs the fetched ceilings, not the pass.
    let catalogs = crate::catalog::fetched::cache_dir()
        .map(|dir| crate::catalog::fetched::load_cached(&dir))
        .unwrap_or_default();
    let windows = crate::watch::Windows::from_store(&store, &catalogs)?;
    let lines = crate::watch::pass(&store, &windows, since_ms, &prefixes, &mut state)?;
    let mut out = std::io::stdout().lock();
    for line in &lines {
        match writeln!(out, "{line}") {
            Ok(()) => {}
            // The reader left: leave the state alone so these print again.
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    state.save(&state_path)
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

/// `toker promote` — the promote-model handover (the control
/// endpoint is the same one the predecessor's promote script used):
/// grant a served model the days (and the prompt ceiling) to become its
/// family's rewrite target early. The days are drawn only from days the
/// ledger already holds, newest first; by default just enough to clear
/// the election's bar (see [`crate::middleware::models::plan_promotion`]).
pub fn promote(
    model: String,
    days: Option<u32>,
    max_prompt: Option<u64>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let config = Config::load()?;
    let opts = PromoteOpts {
        model,
        days: days.map(|days| days as usize),
        max_prompt: max_prompt.map(|max_prompt| i64::try_from(max_prompt).unwrap_or(i64::MAX)),
        dry_run,
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(promote_run(
            &config.db_path,
            config.port,
            &opts,
            &mut std::io::stdout(),
        ))
}

/// What `toker promote` was asked to do.
#[derive(Debug, Clone)]
pub struct PromoteOpts {
    pub model: String,
    /// Days the model should hold after the grant; `None` for the fewest
    /// that clear the bar.
    pub days: Option<usize>,
    /// An explicit prompt ceiling; `None` for the family's best.
    pub max_prompt: Option<i64>,
    /// Print the plan and change nothing.
    pub dry_run: bool,
}

/// The promote verb over an explicit ledger and port, writing its report
/// to `out`: plan from the ledger, then hand the grant to the server on
/// `port`, or, when nothing listens there, apply the same merge to the
/// ledger directly.
pub async fn promote_run(
    db: &Path,
    port: u16,
    opts: &PromoteOpts,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    use crate::middleware::models::{
        MergeOutcome, PromotionRefusal, merge_learned, plan_promotion,
    };

    // Opening a missing ledger would create an empty one, and a promotion
    // can only grant from what a ledger holds.
    if !db.exists() {
        bail!(
            "no ledger at {}: toker writes it once it has served a request",
            db.display()
        );
    }
    let store = Store::open(db)?;
    let entries = store.load_models()?;
    let model = &opts.model;
    let plan = match plan_promotion(&entries, model, opts.days, opts.max_prompt) {
        Ok(plan) => plan,
        Err(PromotionRefusal::Unseen { known }) => bail!(unseen_message(model, &known)),
        Err(PromotionRefusal::Already {
            model_id,
            days,
            active_days,
            needed,
        }) => {
            writeln!(
                out,
                "{model_id} already qualifies: seen on {days} day(s), more than \
                 {needed} of {active_days} active — nothing to do."
            )?;
            return Ok(());
        }
    };
    render_plan(&plan, out)?;
    if opts.dry_run {
        writeln!(out, "\n  --dry-run, nothing written.")?;
        return Ok(());
    }

    let url = format!("http://127.0.0.1:{port}/_toker/models/merge");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let sent = client
        .post(&url)
        .header("x-toker-control", "models-merge")
        .json(&plan.request_body())
        .send()
        .await;
    let response = match sent {
        Ok(response) => response,
        // Nothing listening: apply the same merge, with the same
        // validation, to the ledger itself. The server reads the learned
        // store per decision and never caches it, so the write is seen
        // by whichever server next opens this ledger.
        Err(error) if error.is_connect() => {
            return match merge_learned(&store, &plan.incoming())? {
                MergeOutcome::Merged { entry, target } => {
                    writeln!(
                        out,
                        "\n  nothing is listening on 127.0.0.1:{port}, so the grant was \
                         written to {} directly",
                        db.display()
                    )?;
                    report_target(out, &entry.model_id, target.as_deref())?;
                    Ok(())
                }
                MergeOutcome::Unseen => {
                    let known: Vec<String> =
                        entries.into_iter().map(|entry| entry.model_id).collect();
                    bail!(unseen_message(model, &known))
                }
                MergeOutcome::InventedDays(days) => bail!(invented_message(&days)),
            };
        }
        // Sent, but no answer: the server may or may not have applied it.
        Err(error) => bail!(
            "posting to {url} failed ({error}); the grant may or may not have \
             landed. Run the same promote again: it says \"already qualifies\" if it did."
        ),
    };
    let status = response.status();
    let reply: serde_json::Value = response.json().await.unwrap_or_default();
    if reply.get("toker").and_then(serde_json::Value::as_str) != Some("models-merge") {
        bail!(
            "something is listening on 127.0.0.1:{port} but did not take the merge \
             ({}), so it is not this toker. Nothing was written.",
            status.as_u16()
        );
    }
    if status.is_success() {
        writeln!(out, "\n  merged into the running server")?;
        let targets = reply
            .get("targets")
            .and_then(|targets| targets.as_object())
            .cloned()
            .unwrap_or_default();
        for (_family, target) in targets {
            report_target(out, &plan.model_id, target.as_str())?;
        }
        return Ok(());
    }
    let strings = |key: &str| -> Vec<String> {
        reply
            .get(key)
            .and_then(|values| values.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    match reply.get("error").and_then(serde_json::Value::as_str) {
        // The server's view is the one that counts: it answered, and has
        // never served the model.
        Some("unseen") => bail!(unseen_message(model, &strings("known"))),
        Some("invented days") => bail!(invented_message(&strings("days"))),
        _ => bail!("the merge refused: {} {}", status.as_u16(), reply),
    }
}

/// What the family's election names now: the effect, not the intent.
fn report_target(
    out: &mut impl std::io::Write,
    model_id: &str,
    target: Option<&str>,
) -> std::io::Result<()> {
    let family = crate::middleware::models::family_of(model_id)
        .map(|family| family.name)
        .unwrap_or_else(|| model_id.to_owned());
    writeln!(
        out,
        "  {family} now rewrites to {}",
        target.unwrap_or("nothing")
    )
}

/// The refusal for days the ledger holds for no model. The plan draws only
/// from held days, so this means the ledger changed between the plan and
/// the merge, or the server reads a different ledger from this one.
fn invented_message(days: &[String]) -> String {
    format!(
        "the merge refused days the ledger holds for no model: {}. Nothing was \
         written; is the server reading a different ledger?",
        days.join(", ")
    )
}

/// The refusal for a model the ledger has never served — almost always a
/// typo, and promoting an id the API rejects would redirect a whole
/// family's traffic to a dead end.
fn unseen_message(model: &str, known: &[String]) -> String {
    format!(
        "{model} has never been served through toker. Promoting a model id \
         the API will reject would redirect a whole family's traffic to a \
         dead end; use it once, then promote it.\n  known: {}",
        known.join(", ")
    )
}

/// A token count with thousands separators, as the predecessor printed
/// them (`en-US`).
pub(crate) fn thousands(count: i64) -> String {
    let digits = count.unsigned_abs().to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if count < 0 {
        out.insert(0, '-');
    }
    out
}

/// The plan, as the predecessor's promote script reported it: the bar and
/// its denominator, the days before and after, whether the model now
/// clears the bar, and what the family's election will name.
fn render_plan(
    plan: &crate::middleware::models::PromotionPlan,
    out: &mut impl std::io::Write,
) -> std::io::Result<()> {
    writeln!(out, "{}", plan.model_id)?;
    writeln!(
        out,
        "  bar          seen on more than {} of {} active day(s)",
        plan.needed, plan.active_days
    )?;
    writeln!(
        out,
        "  days         {} → {}  (+{} granted from days the ledger already holds)",
        plan.days_before,
        plan.days.len(),
        plan.granted.len()
    )?;
    if plan.qualifies() {
        writeln!(out, "  qualifies    yes")?;
    } else {
        writeln!(
            out,
            "  qualifies    no: {} day(s) is not more than {}",
            plan.days.len(),
            plan.needed
        )?;
    }
    if plan.raises_ceiling() {
        writeln!(
            out,
            "  rewrite-safe {} → {} tokens",
            thousands(plan.max_prompt_before.unwrap_or(0)),
            thousands(plan.max_prompt.unwrap_or(0))
        )?;
        if plan.ceiling_explicit {
            writeln!(out, "               set by --max-prompt.")?;
        } else {
            writeln!(
                out,
                "               raised to the family's best empirical bound, assuming a newer"
            )?;
            writeln!(
                out,
                "               version holds at least as much. --max-prompt=N to set it yourself."
            )?;
        }
        writeln!(
            out,
            "               declared context capacity is separate and is not widened."
        )?;
    }
    let target = plan.target.as_deref().unwrap_or("nothing");
    let held = if plan.target.as_deref() == Some(plan.model_id.as_str()) {
        ""
    } else if plan.target.is_none() && plan.qualifies() {
        "   ← it belongs to no family, so no election names it"
    } else if plan.qualifies() {
        "   ← a newer version still holds the slot"
    } else {
        "   ← below the bar, so the slot is not this model's"
    };
    writeln!(out, "  target       {target}{held}")
}

// ── toker restart ──────────────────────────────────────────────────────

/// How many idle readings in a row, a poll apart, make a quiet moment.
/// `in_flight` counts only exchanges already under way, and an agent's
/// next request follows its tool calls after a short gap, so one idle
/// reading can land between two requests of a busy turn. Three readings a
/// poll apart outlast the usual gap; a request that still slips in is not
/// cut, the drain lets it finish, it only delays the new instance.
pub const QUIET_POLLS: u32 = 3;

/// The restart's timings: the poll cadence, how long one control request
/// may take, and how long the new instance may take to answer.
#[derive(Debug, Clone)]
pub struct Pacing {
    pub poll: std::time::Duration,
    /// Per request. Under socket activation a status request sent while
    /// the old instance drains waits in the socket's queue until the next
    /// instance accepts it, so this is generous.
    pub request_timeout: std::time::Duration,
    /// From the shutdown request to the new instance answering.
    pub up_timeout: std::time::Duration,
}

impl Pacing {
    /// The real cadence: a poll a second, and 30 s for systemd to bring
    /// the next instance up (`RestartSec=1`, plus the startup seed).
    pub const REAL: Pacing = Pacing {
        poll: std::time::Duration::from_secs(1),
        request_timeout: std::time::Duration::from_secs(10),
        up_timeout: std::time::Duration::from_secs(30),
    };
}

/// What `toker restart` was asked to do.
#[derive(Debug, Clone, Default)]
pub struct RestartOpts {
    /// Give up, restarting nothing, if no quiet moment comes in this long.
    pub max_wait: Option<std::time::Duration>,
    /// Skip the wait. The shutdown still drains: nothing is cut, but the
    /// new instance starts only after the responses under way finish.
    pub now: bool,
}

/// Why a control request got no HTTP answer.
#[derive(Debug, Clone)]
pub enum ControlError {
    /// Nothing accepted the connection.
    Down,
    /// Connected, but no answer within the request timeout.
    Timeout,
    /// Anything else, described.
    Failed(String),
}

/// A control endpoint's answer: the status code, and the body when it was
/// JSON.
#[derive(Debug, Clone)]
pub struct ControlReply {
    pub status: u16,
    pub body: Option<serde_json::Value>,
}

/// The two control requests `toker restart` makes. A seam so the wait
/// loop runs in tests against a script; [`HttpControl`] is the real one.
pub trait Control {
    /// `GET /_toker/status`.
    fn status(&self) -> impl Future<Output = Result<ControlReply, ControlError>>;
    /// `POST /_toker/shutdown` for the named instance.
    fn shutdown(&self, instance: &str) -> impl Future<Output = Result<ControlReply, ControlError>>;
}

/// [`Control`] over HTTP to the toker on a loopback port.
pub struct HttpControl {
    base: String,
    client: reqwest::Client,
}

impl HttpControl {
    pub fn new(port: u16, timeout: std::time::Duration) -> reqwest::Result<HttpControl> {
        Ok(HttpControl {
            base: format!("http://127.0.0.1:{port}"),
            client: reqwest::Client::builder()
                .timeout(timeout)
                // A fresh connection per request. A pooled one to the old
                // instance would outlive its listener, and a status sent
                // on it after the shutdown would ask the wrong process.
                .pool_max_idle_per_host(0)
                .build()?,
        })
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<ControlReply, ControlError> {
        let response = request.send().await.map_err(classify)?;
        let status = response.status().as_u16();
        let body = response.json().await.ok();
        Ok(ControlReply { status, body })
    }
}

fn classify(error: reqwest::Error) -> ControlError {
    if error.is_connect() {
        ControlError::Down
    } else if error.is_timeout() {
        ControlError::Timeout
    } else {
        ControlError::Failed(error.to_string())
    }
}

impl Control for HttpControl {
    fn status(&self) -> impl Future<Output = Result<ControlReply, ControlError>> {
        self.send(
            self.client
                .get(format!("{}/_toker/status", self.base))
                .header("x-toker-control", "status"),
        )
    }

    fn shutdown(&self, instance: &str) -> impl Future<Output = Result<ControlReply, ControlError>> {
        self.send(
            self.client
                .post(format!("{}/_toker/shutdown", self.base))
                .header("x-toker-control", "shutdown")
                .json(&serde_json::json!({ "instance": instance })),
        )
    }
}

/// What one status reading says.
struct Reading {
    in_flight: u64,
    instance: String,
    version: Option<String>,
    uptime_s: Option<u64>,
}

/// Why a status answer is not a reading.
enum Unreadable {
    /// Not toker's status at all.
    NotToker(u16),
    /// toker's status, from a build without `in_flight` and `instance`,
    /// which also has no shutdown endpoint.
    Predates,
}

fn reading(reply: &ControlReply) -> Result<Reading, Unreadable> {
    use serde_json::Value;
    let body = match &reply.body {
        Some(body) if reply.status == 200 && body.get("requests").is_some() => body,
        _ => return Err(Unreadable::NotToker(reply.status)),
    };
    let in_flight = body.get("in_flight").and_then(Value::as_u64);
    let instance = body.get("instance").and_then(Value::as_str);
    let (Some(in_flight), Some(instance)) = (in_flight, instance) else {
        return Err(Unreadable::Predates);
    };
    Ok(Reading {
        in_flight,
        instance: instance.to_owned(),
        version: body
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned),
        uptime_s: body.get("uptime_s").and_then(Value::as_u64),
    })
}

/// The first eight characters of an instance id, enough to tell two apart.
fn short(instance: &str) -> &str {
    instance.get(..8).unwrap_or(instance)
}

fn responses(count: u64) -> String {
    match count {
        1 => "1 response".to_owned(),
        count => format!("{count} responses"),
    }
}

/// A duration as `--max-wait` takes it.
fn span(duration: std::time::Duration) -> String {
    let millis = duration.as_millis();
    if !millis.is_multiple_of(1000) {
        format!("{millis}ms")
    } else if millis.is_multiple_of(3_600_000) {
        format!("{}h", millis / 3_600_000)
    } else if millis.is_multiple_of(60_000) {
        format!("{}m", millis / 60_000)
    } else {
        format!("{}s", millis / 1000)
    }
}

/// Parse `--max-wait`: a whole, positive number with one unit, `s`, `m`
/// or `h` (`90s`, `10m`, `1h`). Anything else is an error, so a typo never
/// turns into waiting forever or not at all.
pub fn wait_arg(text: &str) -> Result<std::time::Duration, String> {
    let expected = "expected a whole number with a unit: 90s, 10m, 1h";
    let unit = match text.chars().last() {
        Some('s') => 1,
        Some('m') => 60,
        Some('h') => 3_600,
        _ => return Err(format!("{text:?} is not a span: {expected}")),
    };
    let digits = &text[..text.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{text:?} is not a span: {expected}"));
    }
    match digits.parse::<u64>().ok().and_then(|n| n.checked_mul(unit)) {
        Some(0) => Err(format!("{text:?} is no wait at all: use --now to skip it")),
        Some(seconds) => Ok(std::time::Duration::from_secs(seconds)),
        None => Err(format!("{text:?} is too long")),
    }
}

/// `toker restart`: wait for the running toker to go quiet, ask it to
/// drain and exit, and wait for systemd to start the next one.
pub fn restart(max_wait: Option<std::time::Duration>, now: bool) -> anyhow::Result<()> {
    let config = Config::load()?;
    let pacing = Pacing::REAL;
    let control = HttpControl::new(config.port, pacing.request_timeout)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(restart_run(
            &control,
            config.port,
            &RestartOpts { max_wait, now },
            &pacing,
            &mut std::io::stdout(),
        ))
}

/// The restart over an explicit [`Control`], writing its report to `out`.
/// `port` is for the messages only. Ctrl-C at any point before the
/// shutdown request leaves toker exactly as it was; after it, the drain
/// and the restart go on without this command.
pub async fn restart_run(
    control: &impl Control,
    port: u16,
    opts: &RestartOpts,
    pacing: &Pacing,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let address = format!("127.0.0.1:{port}");
    let read = |result: Result<ControlReply, ControlError>| -> anyhow::Result<Reading> {
        let reply = match result {
            Ok(reply) => reply,
            // With the socket unit active, the connection itself starts
            // the service; refused means neither is up. Starting them is
            // systemd's job, not this command's.
            Err(ControlError::Down) => bail!(
                "nothing is listening on {address}: toker is not running. Nothing was \
                 restarted; `systemctl --user start toker.socket` starts it."
            ),
            Err(ControlError::Timeout) => bail!(
                "toker on {address} did not answer within {}. Nothing was restarted.",
                span(pacing.request_timeout)
            ),
            Err(ControlError::Failed(error)) => {
                bail!("asking toker on {address} for its status failed: {error}")
            }
        };
        match reading(&reply) {
            Ok(reading) => Ok(reading),
            Err(Unreadable::NotToker(status)) => bail!(
                "something on {address} answered the status request with {status}, so it \
                 is not toker. Nothing was restarted."
            ),
            Err(Unreadable::Predates) => bail!(
                "the toker on {address} predates `toker restart`: it cannot drain on \
                 request. Restart it once with `systemctl --user restart toker.service`, \
                 which cuts any response under way, and use `toker restart` from then on."
            ),
        }
    };

    let started = tokio::time::Instant::now();
    let mut current = read(control.status().await)?;
    let mut quiet = 0;
    let mut shown = None;
    loop {
        if opts.now {
            if current.in_flight > 0 {
                writeln!(
                    out,
                    "not waiting (--now): {} in flight will finish before the old instance exits",
                    responses(current.in_flight)
                )?;
            }
            break;
        }
        if current.in_flight == 0 {
            quiet += 1;
            if quiet >= QUIET_POLLS {
                break;
            }
        } else {
            quiet = 0;
            if shown.is_none() {
                writeln!(
                    out,
                    "waiting for {} in flight to finish (Ctrl-C leaves toker untouched)",
                    responses(current.in_flight)
                )?;
            } else if shown != Some(current.in_flight) {
                writeln!(out, "  {} in flight", current.in_flight)?;
            }
            shown = Some(current.in_flight);
        }
        if let Some(max_wait) = opts.max_wait
            && started.elapsed() >= max_wait
        {
            bail!(
                "no quiet moment within {} (--max-wait): {} in flight at the last look. \
                 toker was not restarted.",
                span(max_wait),
                responses(current.in_flight)
            );
        }
        tokio::time::sleep(pacing.poll).await;
        current = read(control.status().await)?;
    }

    let old = current.instance;
    match control.shutdown(&old).await {
        Ok(reply) if reply.status == 202 => {}
        Ok(reply) if reply.status == 409 => bail!(
            "toker on {address} is no longer the instance that was idle (another restart?). \
             Nothing was restarted by this command; run it again."
        ),
        Ok(reply) => bail!(
            "toker on {address} refused the shutdown request ({}). Nothing was restarted.",
            reply.status
        ),
        Err(ControlError::Down) => bail!(
            "toker on {address} went away before the shutdown request reached it; \
             systemd may be restarting it already."
        ),
        Err(ControlError::Timeout) => bail!(
            "the shutdown request to {address} got no answer within {}: the old instance \
             may or may not be draining.",
            span(pacing.request_timeout)
        ),
        Err(ControlError::Failed(error)) => bail!(
            "the shutdown request to {address} failed ({error}): the old instance may or \
             may not be draining."
        ),
    }
    writeln!(
        out,
        "toker {} is draining and will exit; waiting for systemd to start the next one",
        short(&old)
    )?;

    // Until the deadline anything goes: refused while nothing listens,
    // a timeout while the connection queues, the old instance itself if
    // the drain has not closed its listener yet. Only a new id ends it.
    let deadline = tokio::time::Instant::now() + pacing.up_timeout;
    loop {
        tokio::time::sleep(pacing.poll).await;
        if let Ok(reply) = control.status().await
            && let Ok(reading) = reading(&reply)
            && reading.instance != old
        {
            writeln!(
                out,
                "toker {} is up: version {}, up {}s",
                short(&reading.instance),
                reading.version.as_deref().unwrap_or("unknown"),
                reading
                    .uptime_s
                    .map_or_else(|| "?".to_owned(), |uptime| uptime.to_string()),
            )?;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "no new toker answered on {address} within {} of the shutdown. The old \
                 instance may still be draining a long response; under systemd \
                 `Restart=always` starts the next one once it exits (see `systemctl --user \
                 status toker.service`). A hand-run `toker serve` is not restarted.",
                span(pacing.up_timeout)
            );
        }
    }
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

#[cfg(test)]
mod tests {
    use super::{PromoteOpts, promote_run, render_plan};
    use crate::middleware::models::plan_promotion;
    use crate::store::{ModelEntry, Store};
    use serde_json::json;
    use std::path::PathBuf;

    fn entry(model_id: &str, days: &[&str], max_prompt: Option<i64>) -> ModelEntry {
        ModelEntry {
            model_id: model_id.to_owned(),
            days_json: Some(json!(days)),
            max_prompt,
            context_window_json: None,
        }
    }

    fn rendered(entries: &[ModelEntry], model: &str, days: Option<usize>) -> String {
        let plan = plan_promotion(entries, model, days, None).expect("plans");
        let mut out = Vec::new();
        render_plan(&plan, &mut out).expect("render");
        String::from_utf8(out).expect("utf-8")
    }

    const NINE: [&str; 9] = [
        "2026-09-20",
        "2026-09-21",
        "2026-09-22",
        "2026-09-23",
        "2026-09-24",
        "2026-09-25",
        "2026-09-26",
        "2026-09-27",
        "2026-09-28",
    ];

    #[test]
    fn the_report_names_the_bar_the_days_and_the_effect() {
        let entries = vec![
            entry("claude-opus-5", &NINE, Some(150_000)),
            entry("claude-opus-5-5", &["2026-09-28"], Some(150_000)),
        ];
        let report = rendered(&entries, "claude-opus-5-5", None);
        assert!(
            !report.contains('\u{2190}'),
            "no slot note when the model takes it: {report}"
        );
        insta::assert_snapshot!("qualifies", report);

        // Too few days asked for: it says so, and who keeps the slot.
        let report = rendered(&entries, "claude-opus-5-5", Some(4));
        insta::assert_snapshot!("too_few_days", report);

        // The ceiling: raised to the family's best by default, and said so.
        let entries = vec![
            entry("claude-opus-5", &NINE, Some(480_000)),
            entry("claude-opus-5-5", &["2026-09-28"], Some(4_000)),
        ];
        let report = rendered(&entries, "claude-opus-5-5", None);
        assert!(report.contains("480,000"), "{report}");
        insta::assert_snapshot!("ceiling_raised", report);

        // A newer version holds the slot.
        let entries = vec![
            entry("claude-opus-5-5", &NINE, Some(150_000)),
            entry("claude-opus-5", &["2026-09-28"], Some(150_000)),
        ];
        let report = rendered(&entries, "claude-opus-5", None);
        insta::assert_snapshot!("newer_holds_slot", report);
    }

    /// A fresh ledger in a directory of its own, holding the given
    /// learned entries.
    fn ledger(name: &str, entries: &[ModelEntry]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("toker-promote-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = dir.join("toker.db");
        let store = Store::open(&db).expect("open ledger");
        for entry in entries {
            store.upsert_model(entry).expect("upsert");
        }
        db
    }

    /// A loopback port nothing listens on: bound, then released.
    fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    }

    fn opts(model: &str, dry_run: bool) -> PromoteOpts {
        PromoteOpts {
            model: model.to_owned(),
            days: None,
            max_prompt: None,
            dry_run,
        }
    }

    fn promoting_entries() -> Vec<ModelEntry> {
        vec![
            entry("claude-opus-5", &NINE, Some(480_000)),
            entry("claude-opus-5-5", &["2026-09-28"], Some(4_000)),
        ]
    }

    #[tokio::test]
    async fn with_nothing_listening_the_grant_is_written_to_the_ledger() {
        let db = ledger("offline", &promoting_entries());
        let mut out = Vec::new();
        promote_run(
            &db,
            closed_port(),
            &opts("claude-opus-5-5", false),
            &mut out,
        )
        .await
        .expect("promotes offline");
        let report = String::from_utf8(out).expect("utf-8");
        assert!(report.contains("written to"), "{report}");
        assert!(
            report.contains("opus now rewrites to claude-opus-5-5"),
            "{report}"
        );

        let store = Store::open(&db).expect("reopen");
        assert_eq!(
            store.load_model("claude-opus-5-5").expect("load"),
            Some(entry(
                "claude-opus-5-5",
                &[
                    "2026-09-24",
                    "2026-09-25",
                    "2026-09-26",
                    "2026-09-27",
                    "2026-09-28"
                ],
                Some(480_000)
            )),
            "five held days and the family's ceiling"
        );
        assert_eq!(
            crate::middleware::models::newest_in_family(
                &store.load_models().expect("load"),
                "opus"
            ),
            Some("claude-opus-5-5".to_owned())
        );

        // Run again: it now qualifies, so there is nothing to do.
        let mut out = Vec::new();
        promote_run(
            &db,
            closed_port(),
            &opts("claude-opus-5-5", false),
            &mut out,
        )
        .await
        .expect("already");
        let report = String::from_utf8(out).expect("utf-8");
        assert!(report.contains("already qualifies"), "{report}");

        // An unseen model is refused offline too, naming what is known.
        let error = promote_run(
            &db,
            closed_port(),
            &opts("claude-opus-6", false),
            &mut Vec::new(),
        )
        .await
        .expect_err("unseen");
        assert!(error.to_string().contains("claude-opus-5-5"), "{error}");
    }

    #[tokio::test]
    async fn a_dry_run_reports_the_plan_and_touches_nothing() {
        let db = ledger("dry-run", &promoting_entries());
        // A listener that would see any attempt to reach the server.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("addr").port();

        let mut out = Vec::new();
        promote_run(&db, port, &opts("claude-opus-5-5", true), &mut out)
            .await
            .expect("dry run");
        let report = String::from_utf8(out).expect("utf-8");
        assert!(report.contains("--dry-run"), "{report}");
        insta::assert_snapshot!(report);
        assert!(
            listener.accept().is_err(),
            "a dry run never contacts the server"
        );
        assert_eq!(
            Store::open(&db)
                .expect("reopen")
                .load_model("claude-opus-5-5")
                .expect("load"),
            Some(entry("claude-opus-5-5", &["2026-09-28"], Some(4_000))),
            "the ledger is untouched"
        );
    }

    #[tokio::test]
    async fn a_listener_that_is_not_toker_gets_nothing_written() {
        let db = ledger("not-toker", &promoting_entries());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0; 64 * 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                    )
                    .await;
            }
        });
        let error = promote_run(&db, port, &opts("claude-opus-5-5", false), &mut Vec::new())
            .await
            .expect_err("not toker");
        assert!(error.to_string().contains("Nothing was written"), "{error}");
        assert_eq!(
            Store::open(&db)
                .expect("reopen")
                .load_model("claude-opus-5-5")
                .expect("load"),
            Some(entry("claude-opus-5-5", &["2026-09-28"], Some(4_000)))
        );
    }

    #[tokio::test]
    async fn a_missing_ledger_is_never_created() {
        let dir =
            std::env::temp_dir().join(format!("toker-promote-{}-missing", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = dir.join("toker.db");
        let error = promote_run(
            &db,
            closed_port(),
            &opts("claude-opus-5", true),
            &mut Vec::new(),
        )
        .await
        .expect_err("no ledger");
        assert!(error.to_string().contains("no ledger"), "{error}");
        assert!(!db.exists());
    }
}

#[cfg(test)]
mod restart_tests {
    use super::{
        Control, ControlError, ControlReply, Pacing, QUIET_POLLS, RestartOpts, restart_run,
        wait_arg,
    };
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    const OLD: &str = "0ld1d0ld00000000000000000000000a";
    const NEW: &str = "n3wn3wn300000000000000000000000b";

    /// A scripted control: status answers in order, the last repeating
    /// forever; every shutdown request recorded and accepted.
    struct Script {
        statuses: Mutex<VecDeque<Result<ControlReply, ControlError>>>,
        status_calls: Mutex<usize>,
        shutdowns: Mutex<Vec<String>>,
    }

    impl Script {
        fn new(statuses: Vec<Result<ControlReply, ControlError>>) -> Script {
            Script {
                statuses: Mutex::new(statuses.into()),
                status_calls: Mutex::new(0),
                shutdowns: Mutex::new(Vec::new()),
            }
        }

        fn shutdowns(&self) -> Vec<String> {
            self.shutdowns.lock().unwrap().clone()
        }
    }

    impl Control for Script {
        async fn status(&self) -> Result<ControlReply, ControlError> {
            *self.status_calls.lock().unwrap() += 1;
            let mut statuses = self.statuses.lock().unwrap();
            if statuses.len() > 1 {
                statuses.pop_front().expect("non-empty")
            } else {
                statuses.front().cloned().expect("a script has an answer")
            }
        }

        async fn shutdown(&self, instance: &str) -> Result<ControlReply, ControlError> {
            self.shutdowns.lock().unwrap().push(instance.to_owned());
            Ok(ControlReply {
                status: 202,
                body: Some(json!({"toker": "shutdown", "ok": true})),
            })
        }
    }

    fn status(in_flight: u64, instance: &str) -> Result<ControlReply, ControlError> {
        Ok(ControlReply {
            status: 200,
            body: Some(json!({
                "requests": 12,
                "in_flight": in_flight,
                "instance": instance,
                "version": "0.1.0",
                "uptime_s": if instance == NEW { 0 } else { 5_000 },
            })),
        })
    }

    fn pacing() -> Pacing {
        Pacing {
            poll: Duration::from_millis(2),
            request_timeout: Duration::from_secs(10),
            up_timeout: Duration::from_millis(100),
        }
    }

    async fn run(script: &Script, opts: RestartOpts) -> (anyhow::Result<()>, String) {
        let mut out = Vec::new();
        let result = restart_run(script, 20000, &opts, &pacing(), &mut out).await;
        (result, String::from_utf8(out).expect("utf-8"))
    }

    #[tokio::test]
    async fn an_idle_toker_is_restarted_after_the_quiet_polls_and_says_little() {
        let mut statuses: Vec<_> = (0..QUIET_POLLS).map(|_| status(0, OLD)).collect();
        // After the shutdown: the old instance a moment longer, nothing
        // listening, then the successor.
        statuses.extend([status(0, OLD), Err(ControlError::Down), status(0, NEW)]);
        let script = Script::new(statuses);
        let (result, out) = run(&script, RestartOpts::default()).await;
        result.expect("restarts");
        assert_eq!(script.shutdowns(), vec![OLD.to_owned()]);
        assert_eq!(
            *script.status_calls.lock().unwrap(),
            QUIET_POLLS as usize + 3
        );
        insta::assert_snapshot!(out);
    }

    #[tokio::test]
    async fn a_busy_toker_is_waited_for_and_an_idle_reading_between_requests_is_not_enough() {
        let script = Script::new(vec![
            status(2, OLD),
            status(2, OLD),
            status(1, OLD),
            // One idle reading between two requests resets nothing but
            // itself: the count of quiet readings starts over.
            status(0, OLD),
            status(1, OLD),
            status(0, OLD),
            status(0, OLD),
            status(0, OLD),
            status(0, NEW),
        ]);
        let (result, out) = run(&script, RestartOpts::default()).await;
        result.expect("restarts");
        assert_eq!(script.shutdowns(), vec![OLD.to_owned()]);
        assert_eq!(*script.status_calls.lock().unwrap(), 9);
        insta::assert_snapshot!(out);
    }

    #[tokio::test]
    async fn max_wait_expiring_restarts_nothing() {
        let script = Script::new(vec![status(1, OLD)]);
        let (result, out) = run(
            &script,
            RestartOpts {
                max_wait: Some(Duration::from_millis(30)),
                now: false,
            },
        )
        .await;
        let error = result.expect_err("gives up");
        assert!(script.shutdowns().is_empty(), "no shutdown was sent");
        insta::assert_snapshot!(format!("{out}---\n{error:#}"));
    }

    #[tokio::test]
    async fn now_skips_the_wait_but_says_what_is_still_running() {
        let script = Script::new(vec![status(2, OLD), status(0, NEW)]);
        let (result, out) = run(
            &script,
            RestartOpts {
                max_wait: None,
                now: true,
            },
        )
        .await;
        result.expect("restarts");
        assert_eq!(script.shutdowns(), vec![OLD.to_owned()]);
        insta::assert_snapshot!(out);
    }

    #[tokio::test]
    async fn a_service_that_is_not_running_is_reported_and_left_alone() {
        let script = Script::new(vec![Err(ControlError::Down)]);
        let (result, out) = run(&script, RestartOpts::default()).await;
        let error = result.expect_err("nothing to restart");
        assert!(script.shutdowns().is_empty());
        assert_eq!(out, "");
        insta::assert_snapshot!(format!("{error:#}"));
    }

    #[tokio::test]
    async fn a_toker_without_the_endpoint_or_something_else_is_refused() {
        // A toker from before `instance` was in status: no shutdown to ask.
        let script = Script::new(vec![Ok(ControlReply {
            status: 200,
            body: Some(json!({"requests": 3, "uptime_s": 9})),
        })]);
        let error = run(&script, RestartOpts::default())
            .await
            .0
            .expect_err("predates");
        assert!(script.shutdowns().is_empty());
        insta::assert_snapshot!("predates", format!("{error:#}"));

        // Some other server on the port.
        let script = Script::new(vec![Ok(ControlReply {
            status: 404,
            body: None,
        })]);
        let error = run(&script, RestartOpts::default())
            .await
            .0
            .expect_err("not toker");
        assert!(script.shutdowns().is_empty());
        insta::assert_snapshot!("not_toker", format!("{error:#}"));
    }

    #[tokio::test]
    async fn a_successor_that_never_answers_is_a_clear_failure() {
        let mut statuses: Vec<_> = (0..QUIET_POLLS).map(|_| status(0, OLD)).collect();
        statuses.push(Err(ControlError::Down));
        let script = Script::new(statuses);
        let (result, out) = run(&script, RestartOpts::default()).await;
        let error = result.expect_err("never up");
        assert_eq!(script.shutdowns(), vec![OLD.to_owned()]);
        insta::assert_snapshot!(format!("{out}---\n{error:#}"));
    }

    #[test]
    fn max_wait_parses_strictly() {
        assert_eq!(wait_arg("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(wait_arg("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(wait_arg("1h"), Ok(Duration::from_secs(3_600)));
        for bad in [
            "", "10", "m", "1.5m", "-1m", "10 m", "10min", "1d", "0s", "+5m",
        ] {
            assert!(wait_arg(bad).is_err(), "{bad:?} must not parse");
        }
    }
}
