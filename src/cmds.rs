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

use std::path::PathBuf;
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
    tracing::info!(db = %config.db_path.display(), "serving");
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

    let routing_state = store.load_routing_policy_state()?;
    println!("port: {}", config.port);
    println!("db: {}", config.db_path.display());
    println!(
        "routing: {}",
        routing_state.active_revision_id.map_or_else(
            || "no active revision yet (the service creates revision one from this config)"
                .to_owned(),
            |revision| format!("active revision {revision}")
        )
    );
    println!(
        "session headers: {}",
        config.session_header_names.join(", ")
    );
    // Before the first start, the config's defaults are what revision one
    // will be generated from; afterwards the revision owns routing.
    if routing_state.active_revision_id.is_none() {
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
    }

    // Enabled backends only: a block's presence is what enables one, so
    // an absent block has nothing to report.
    let enabled = config.enabled_backends();
    if enabled.is_empty() {
        println!("backends: none enabled — add a [providers.<name>] block or run `toker setup`");
    }
    if let Some(sub) = &config.anthropic_sub {
        println!("anthropic_sub: {}", sub.upstream);
        println!(
            "anthropic_sub OAuth token: {}; Claude login: {}",
            key_sources_line(&sub.oauth_token_env, sub.oauth_sources()),
            if sub.claude_credentials_path.is_some() {
                "configured"
            } else {
                "disabled"
            },
        );
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

    let rows = store.count_logical_requests()?;
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

/// `picker sync`: Claude Code's `/model` rows, rebuilt from the active routing
/// policy (see [`crate::picker`]). A dry run prints the rows instead of
/// writing them.
pub fn picker_sync(dry_run: bool) -> anyhow::Result<()> {
    let config = Config::load()?;
    let settings = crate::setup::wizard::Paths::real()?.claude_settings;
    let report = crate::picker::sync(&config, &settings, dry_run)?;
    if dry_run {
        for row in &report.rows {
            println!("{}  {}  (as {})", row.model, row.label, row.behaves_as);
        }
    } else {
        println!(
            "picker: {} Claude /model rows in {} ({})",
            report.rows.len(),
            settings.display(),
            if report.changed {
                "updated"
            } else {
                "unchanged"
            }
        );
    }
    Ok(())
}

/// `tui`: the ratatui dashboard over the ledger (plan: TUI). `--db`
/// overrides the path; otherwise `TOKER_DB` and the config default apply
/// (Config::load already layered env over file).
pub fn tui(window_mins: u64, db: Option<PathBuf>) -> anyhow::Result<()> {
    let config = Config::load()?;
    // Without a resolvable home the routing page still edits and activates;
    // it just cannot follow an activation with the settings sync.
    let settings = crate::setup::wizard::Paths::real()
        .ok()
        .map(|paths| crate::tui::SettingsSync {
            config: config.clone(),
            settings: paths.claude_settings,
        });
    let db_path = db.unwrap_or(config.db_path);
    crate::tui::run(
        &db_path,
        window_mins,
        &config.transcript_roots,
        &config.gates,
        config.port,
        settings,
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

/// `toker promote`: mark an already learned binding/model pair as explicitly
/// eligible for a policy-managed upgrade. Observed days, empirical prompt
/// ceilings, and declared context remain provider evidence and are never
/// fabricated by this command.
pub fn promote(
    model: String,
    binding: Option<crate::policy::BindingId>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let config = Config::load()?;
    promote_run(
        &config,
        &PromoteOpts {
            model,
            binding,
            dry_run,
        },
        &mut std::io::stdout(),
    )
}

/// What `toker promote` was asked to do.
#[derive(Debug, Clone)]
pub struct PromoteOpts {
    pub model: String,
    pub binding: Option<crate::policy::BindingId>,
    /// Print the evidence mutation and change nothing.
    pub dry_run: bool,
}

/// Apply one binding-local promotion directly to the configured ledger. The
/// promotable families come from the active routing revision. This writes
/// only the binding's own learning, never the legacy `models` table.
pub fn promote_run(
    config: &Config,
    opts: &PromoteOpts,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    use crate::catalog::offers::OfferInventory;
    use crate::routing::RouteRegistry;

    if !config.db_path.exists() {
        bail!(
            "no ledger at {}: toker writes it once it has served a request",
            config.db_path.display()
        );
    }
    let model = opts.model.as_str();
    if model.trim().is_empty() {
        bail!("the model identity is blank");
    }
    let store = Store::open(&config.db_path)?;
    let matching = store.load_binding_learnings_for_model(model)?;
    let binding = match (&opts.binding, matching.as_slice()) {
        (Some(binding), _) => binding.clone(),
        (None, [entry]) => entry.binding_id.clone(),
        (None, []) => bail!(binding_unseen_message(model, &[])),
        (None, entries) => {
            let bindings = entries
                .iter()
                .map(|entry| entry.binding_id.as_str())
                .collect::<Vec<_>>();
            bail!(
                "{model} is learned under multiple bindings; pass --binding with one of: {}",
                bindings.join(", ")
            )
        }
    };
    // The active revision, as the service compiled it: a credential-free view
    // of the enabled bindings is all the classifiers need.
    let topology = RouteRegistry::configured_bindings(crate::picker::configured_bindings(config));
    if topology.binding(&binding).is_none() {
        bail!("{binding} is not a declared backend binding");
    }
    let entry = store
        .load_binding_learning(&binding, model)?
        .ok_or_else(|| anyhow::anyhow!(binding_unseen_message(model, &[binding.as_str()])))?;
    let revision_id = store
        .load_routing_policy_state()?
        .active_revision_id
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no active routing revision in {}: start the service once, or run `toker setup`",
                config.db_path.display()
            )
        })?;
    let revision = store
        .load_policy_revision(revision_id)?
        .ok_or_else(|| anyhow::anyhow!("active policy revision {revision_id} is missing"))?;
    let policy = crate::policy::runtime::compile_stored_revision(
        &revision,
        &topology,
        &OfferInventory::default(),
    )
    .into_policy()
    .map_err(|diagnostics| {
        anyhow::anyhow!(
            "active policy revision {revision_id} has {} diagnostic(s)",
            diagnostics.len()
        )
    })?;
    let mut applicable = policy
        .upgrades()
        .filter(|upgrade| upgrade.policy().enabled && upgrade.policy().allow_manual_promotion)
        .filter_map(|upgrade| {
            let classifier = policy.classifier(&upgrade.policy().classifier_id)?;
            (classifier.binding_id() == &binding)
                .then(|| classifier.classify(model).ok())
                .flatten()
                .flatten()
        });
    let classified = applicable
        .next()
        .ok_or_else(|| anyhow::anyhow!("{model} is not manually promotable under {binding}"))?;
    if applicable.next().is_some() {
        bail!("{model} matches multiple manual-promotion policies under {binding}");
    }

    writeln!(out, "{model}")?;
    writeln!(out, "  binding      {binding}")?;
    writeln!(out, "  family       {}", classified.family)?;
    writeln!(
        out,
        "  promotion    {}",
        if entry.explicitly_promoted {
            "already explicit"
        } else {
            "set explicit eligibility"
        }
    )?;
    if opts.dry_run {
        writeln!(out, "\n  --dry-run, nothing written.")?;
        return Ok(());
    }
    if store
        .promote_binding_learning(&binding, model, jiff::Timestamp::now().as_millisecond())?
        .is_none()
    {
        bail!("{model} disappeared from {binding} before promotion; nothing was written");
    }
    let outcome = if entry.explicitly_promoted {
        "was already present"
    } else {
        "written"
    };
    writeln!(
        out,
        "\n  binding-scoped promotion {outcome} in {}",
        config.db_path.display()
    )?;
    Ok(())
}

fn binding_unseen_message(model: &str, bindings: &[&str]) -> String {
    if bindings.is_empty() {
        format!(
            "{model} has not been learned under any backend binding; use it once, then promote it"
        )
    } else {
        format!(
            "{model} has not been learned under {}; use it once through that binding, then promote it",
            bindings.join(", ")
        )
    }
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

/// How long the graceful restart may look for a quiet moment before
/// giving up on it and telling the service to stop without waiting.
pub const DEFAULT_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// What `toker restart` was asked to do.
#[derive(Debug, Clone, Default)]
pub struct RestartOpts {
    /// How long to wait for a quiet moment before forcing the stop
    /// ([`DEFAULT_MAX_WAIT`] when unset).
    pub max_wait: Option<std::time::Duration>,
    /// Skip the wait. The shutdown still drains: nothing is cut, but the
    /// new instance starts only after the responses under way finish, or
    /// until `up_timeout` runs out and the stop is forced.
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
    /// `POST /_toker/shutdown` for the named instance; with `force`, not
    /// waiting for connections under way.
    fn shutdown(
        &self,
        instance: &str,
        force: bool,
    ) -> impl Future<Output = Result<ControlReply, ControlError>>;
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

    fn shutdown(
        &self,
        instance: &str,
        force: bool,
    ) -> impl Future<Output = Result<ControlReply, ControlError>> {
        let mut body = serde_json::json!({ "instance": instance });
        if force {
            body["force"] = serde_json::json!(true);
        }
        self.send(
            self.client
                .post(format!("{}/_toker/shutdown", self.base))
                .header("x-toker-control", "shutdown")
                .json(&body),
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
    // `None` is a service that did not answer in time: busy, not gone.
    // That is the case the forced stop is for, so it is not an error
    // here the way a refused connection or another server is.
    let read = |result: Result<ControlReply, ControlError>| -> anyhow::Result<Option<Reading>> {
        let reply = match result {
            Ok(reply) => reply,
            // With the socket unit active, the connection itself starts
            // the service; refused means neither is up. Starting them is
            // systemd's job, not this command's.
            Err(ControlError::Down) => bail!(
                "nothing is listening on {address}: toker is not running. Nothing was \
                 restarted; `systemctl --user start toker.socket` starts it."
            ),
            Err(ControlError::Timeout) => return Ok(None),
            Err(ControlError::Failed(error)) => {
                bail!("asking toker on {address} for its status failed: {error}")
            }
        };
        match reading(&reply) {
            Ok(reading) => Ok(Some(reading)),
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

    let max_wait = opts.max_wait.unwrap_or(DEFAULT_MAX_WAIT);
    let started = tokio::time::Instant::now();
    let mut current = read(control.status().await)?;
    // The last instance that answered: what the next one is told apart from.
    let mut old = current.as_ref().map(|reading| reading.instance.clone());
    let mut quiet = 0;
    let mut shown = None;
    let mut silent = false;
    // Why the graceful restart was given up on, once it is.
    let mut hard: Option<String> = None;
    loop {
        if opts.now {
            if let Some(current) = &current
                && current.in_flight > 0
            {
                writeln!(
                    out,
                    "not waiting (--now): {} in flight will finish before the old instance exits",
                    responses(current.in_flight)
                )?;
            }
            break;
        }
        match &current {
            Some(current) if current.in_flight == 0 => {
                quiet += 1;
                if quiet >= QUIET_POLLS {
                    break;
                }
            }
            Some(current) => {
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
            None => {
                quiet = 0;
                if !silent {
                    writeln!(
                        out,
                        "toker is not answering its status request (Ctrl-C leaves toker untouched)"
                    )?;
                }
                silent = true;
            }
        }
        if started.elapsed() >= max_wait {
            hard = Some(match &current {
                Some(current) => format!(
                    "no quiet moment within {}: {} in flight at the last look",
                    span(max_wait),
                    responses(current.in_flight)
                ),
                None => format!("toker did not answer within {}", span(max_wait)),
            });
            break;
        }
        tokio::time::sleep(pacing.poll).await;
        current = read(control.status().await)?;
        if let Some(current) = &current {
            old = Some(current.instance.clone());
        }
    }

    if hard.is_none() {
        match &current {
            None => hard = Some("toker did not answer its status request".to_owned()),
            Some(current) => match control.shutdown(&current.instance, false).await {
                Ok(reply) if reply.status == 202 => {
                    writeln!(
                        out,
                        "toker {} is draining and will exit; waiting for systemd to start the next one",
                        short(&current.instance)
                    )?;
                }
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
                Err(ControlError::Timeout) => {
                    hard = Some(format!(
                        "the shutdown request got no answer within {}",
                        span(pacing.request_timeout)
                    ));
                }
                Err(ControlError::Failed(error)) => bail!(
                    "the shutdown request to {address} failed ({error}): the old instance may or \
                     may not be draining."
                ),
            },
        }
    }

    // The forced stop: the same request again, saying not to wait for
    // connections. The daemon still exits through its normal path, so
    // destructors run and the ledger and sleep lock are tidied; only the
    // responses under way are cut, and the harness retries those. It names
    // the last instance that answered, so without one there is nothing to
    // address, and a daemon that answers nothing is for systemd to deal
    // with.
    let force = async |reason: &str, out: &mut dyn std::io::Write| -> anyhow::Result<()> {
        let Some(instance) = &old else {
            bail!(
                "{reason}, and toker never answered, so there is no instance to tell to stop. \
                 Nothing was restarted; `systemctl --user restart toker.service` restarts it."
            );
        };
        writeln!(
            out,
            "{reason}; stopping toker without waiting for connections, systemd starts the next one"
        )?;
        match control.shutdown(instance, true).await {
            Ok(reply) if reply.status == 202 => Ok(()),
            // Gone between the two requests: on its way out already.
            Err(ControlError::Down) => Ok(()),
            Ok(reply) if reply.status == 409 => bail!(
                "toker on {address} is no longer the instance that was asked to stop (another \
                 restart?). Run the command again."
            ),
            Ok(reply) => bail!(
                "toker on {address} refused the forced shutdown ({}). Nothing more was done.",
                reply.status
            ),
            Err(ControlError::Timeout) => bail!(
                "the forced shutdown request to {address} got no answer within {}. \
                 `systemctl --user restart toker.service` restarts it from outside.",
                span(pacing.request_timeout)
            ),
            Err(ControlError::Failed(error)) => bail!(
                "the forced shutdown request to {address} failed ({error}). \
                 `systemctl --user restart toker.service` restarts it from outside."
            ),
        }
    };
    let mut forced = false;
    if let Some(reason) = &hard {
        force(reason, out).await?;
        forced = true;
    }

    // Until the deadline anything goes: refused while nothing listens,
    // a timeout while the connection queues, the old instance itself if
    // the drain has not closed its listener yet. Only a new id ends it.
    let mut deadline = tokio::time::Instant::now() + pacing.up_timeout;
    loop {
        tokio::time::sleep(pacing.poll).await;
        if let Ok(reply) = control.status().await
            && let Ok(reading) = reading(&reply)
            && old.as_deref() != Some(reading.instance.as_str())
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
            if forced {
                bail!(
                    "no new toker answered on {address} within {} of the forced shutdown. Under \
                     systemd `Restart=always` starts the next one once the old one has exited \
                     (see `systemctl --user status toker.service`). A hand-run `toker serve` is \
                     not restarted.",
                    span(pacing.up_timeout)
                );
            }
            // The drain is not done: a response that will not end. That
            // is the second timeout, and it ends the same way.
            force(
                &format!(
                    "the old instance was still draining {} after the shutdown",
                    span(pacing.up_timeout)
                ),
                out,
            )
            .await?;
            forced = true;
            deadline = tokio::time::Instant::now() + pacing.up_timeout;
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
    use std::collections::BTreeSet;

    use super::{PromoteOpts, promote_run};
    use crate::config::Config;
    use crate::policy::BindingId;
    use crate::store::{BindingLearningUpdate, ModelEntry, Store};

    fn config(name: &str) -> (crate::test_support::TestDir, Config) {
        let dir = crate::test_support::tempdir(&format!("toker-promote-{name}-"));
        let path = dir.join("toker.toml");
        std::fs::write(&path, "[providers.anthropic_sub]\n").unwrap();
        let mut config = Config::load_from(&path).unwrap();
        config.db_path = dir.join("toker.db");
        (dir, config)
    }

    /// Give the ledger the active revision a started service would have
    /// created from this config.
    fn activate(config: &Config) {
        let store = Store::open(&config.db_path).unwrap();
        let topology = crate::routing::RouteRegistry::configured_bindings(
            crate::picker::configured_bindings(config),
        );
        crate::policy::legacy::establish_revision_one(
            &store,
            config,
            &topology,
            &crate::catalog::offers::OfferInventory::default(),
            0,
            1,
        )
        .unwrap();
    }

    fn binding(value: &str) -> BindingId {
        value.parse().unwrap()
    }

    fn learn(store: &Store, binding_id: &BindingId, model_id: &str) {
        store
            .merge_binding_learning(&BindingLearningUpdate {
                binding_id: binding_id.clone(),
                model_id: model_id.to_owned(),
                days: BTreeSet::from(["2026-10-01".to_owned()]),
                max_prompt: Some(123),
                explicitly_promoted: false,
                updated_ms: 1,
            })
            .unwrap();
    }

    fn opts(model: &str, binding_id: Option<&str>, dry_run: bool) -> PromoteOpts {
        PromoteOpts {
            model: model.to_owned(),
            binding: binding_id.map(binding),
            dry_run,
        }
    }

    #[test]
    fn unique_binding_is_inferred_and_only_promotion_state_changes() {
        let (_dir, config) = config("unique");
        activate(&config);
        let store = Store::open(&config.db_path).unwrap();
        let selected = binding("anthropic_sub.anthropic_messages");
        learn(&store, &selected, "claude-opus-5");
        store
            .upsert_model(&ModelEntry {
                model_id: "claude-opus-5".to_owned(),
                days_json: Some(serde_json::json!(["2026-10-01"])),
                max_prompt: Some(999),
                context_window_json: None,
            })
            .unwrap();

        let mut out = Vec::new();
        promote_run(&config, &opts("claude-opus-5", None, false), &mut out).unwrap();
        let report = String::from_utf8(out).unwrap();
        assert!(report.contains(selected.as_str()), "{report}");
        assert!(report.contains("family       opus"), "{report}");
        assert!(!report.contains("legacy"), "{report}");
        let learned = store
            .load_binding_learning(&selected, "claude-opus-5")
            .unwrap()
            .unwrap();
        assert!(learned.explicitly_promoted);
        assert_eq!(learned.days, BTreeSet::from(["2026-10-01".to_owned()]));
        assert_eq!(learned.max_prompt, Some(123));
        assert_eq!(
            store
                .load_model("claude-opus-5")
                .unwrap()
                .unwrap()
                .max_prompt,
            Some(999),
            "binding promotion never dual-writes legacy learning",
        );
    }

    #[test]
    fn ambiguous_model_requires_binding_and_explicit_binding_is_isolated() {
        let (_dir, config) = config("ambiguous");
        activate(&config);
        let store = Store::open(&config.db_path).unwrap();
        let first = binding("anthropic_api.anthropic_messages");
        let second = binding("anthropic_sub.anthropic_messages");
        learn(&store, &first, "claude-opus-5");
        learn(&store, &second, "claude-opus-5");

        let error = promote_run(
            &config,
            &opts("claude-opus-5", None, false),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("multiple bindings"), "{error}");
        assert!(error.contains(first.as_str()), "{error}");
        assert!(error.contains(second.as_str()), "{error}");

        promote_run(
            &config,
            &opts("claude-opus-5", Some(second.as_str()), false),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            !store
                .load_binding_learning(&first, "claude-opus-5")
                .unwrap()
                .unwrap()
                .explicitly_promoted
        );
        assert!(
            store
                .load_binding_learning(&second, "claude-opus-5")
                .unwrap()
                .unwrap()
                .explicitly_promoted
        );
    }

    #[test]
    fn dry_run_and_refusals_write_nothing() {
        let (_dir, config) = config("refusals");
        activate(&config);
        let store = Store::open(&config.db_path).unwrap();
        let selected = binding("anthropic_sub.anthropic_messages");
        learn(&store, &selected, "claude-opus-5");

        let mut out = Vec::new();
        promote_run(&config, &opts("claude-opus-5", None, true), &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("--dry-run"));
        assert!(
            !store
                .load_binding_learning(&selected, "claude-opus-5")
                .unwrap()
                .unwrap()
                .explicitly_promoted
        );

        let wrong = binding("anthropic_api.anthropic_messages");
        let error = promote_run(
            &config,
            &opts("claude-opus-5", Some(wrong.as_str()), false),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(wrong.as_str()), "{error}");
        assert!(
            promote_run(&config, &opts("opaque-model", None, false), &mut Vec::new(),)
                .unwrap_err()
                .to_string()
                .contains("not been learned")
        );
    }

    #[test]
    fn a_ledger_with_no_active_revision_has_nothing_to_promote_under() {
        let (_dir, config) = config("no-revision");
        let store = Store::open(&config.db_path).unwrap();
        learn(
            &store,
            &binding("anthropic_sub.anthropic_messages"),
            "claude-opus-5",
        );
        let error = promote_run(
            &config,
            &opts("claude-opus-5", None, false),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("no active routing revision"),
            "{error}"
        );
        assert!(
            !store
                .load_binding_learning(
                    &binding("anthropic_sub.anthropic_messages"),
                    "claude-opus-5"
                )
                .unwrap()
                .unwrap()
                .explicitly_promoted
        );
    }

    #[test]
    fn missing_ledger_is_never_created() {
        let (_dir, config) = config("missing");
        let error =
            promote_run(&config, &opts("claude-opus-5", None, true), &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("no ledger"), "{error}");
        assert!(!config.db_path.exists());
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
        forced: Mutex<Vec<String>>,
        /// Whether a forced shutdown brings the successor up: after it,
        /// status answers as the new instance.
        revives: bool,
    }

    impl Script {
        fn new(statuses: Vec<Result<ControlReply, ControlError>>) -> Script {
            Script {
                statuses: Mutex::new(statuses.into()),
                status_calls: Mutex::new(0),
                shutdowns: Mutex::new(Vec::new()),
                forced: Mutex::new(Vec::new()),
                revives: false,
            }
        }

        fn reviving(mut self) -> Script {
            self.revives = true;
            self
        }

        fn forced(&self) -> Vec<String> {
            self.forced.lock().unwrap().clone()
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

        async fn shutdown(
            &self,
            instance: &str,
            force: bool,
        ) -> Result<ControlReply, ControlError> {
            if force {
                self.forced.lock().unwrap().push(instance.to_owned());
                if self.revives {
                    *self.statuses.lock().unwrap() = VecDeque::from([status(0, NEW)]);
                }
            } else {
                self.shutdowns.lock().unwrap().push(instance.to_owned());
            }
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
    async fn max_wait_expiring_forces_the_stop_instead_of_waiting_on() {
        // Busy until the wait runs out; the successor answers once forced.
        let script = Script::new(vec![status(1, OLD), status(1, OLD), status(0, NEW)]).reviving();
        let (result, out) = run(
            &script,
            RestartOpts {
                max_wait: Some(Duration::from_millis(1)),
                now: false,
            },
        )
        .await;
        result.expect("restarts, forced");
        assert!(
            script.shutdowns().is_empty(),
            "no graceful drain was asked for"
        );
        assert_eq!(script.forced(), vec![OLD.to_owned()]);
        insta::assert_snapshot!(out);
    }

    #[tokio::test]
    async fn a_status_that_times_out_is_waited_on_then_forced_not_an_error() {
        // The service answered once, then went quiet: the instance it
        // gave is what the forced stop names.
        let script = Script::new(vec![
            status(1, OLD),
            Err(ControlError::Timeout),
            Err(ControlError::Timeout),
        ])
        .reviving();
        let (result, out) = run(
            &script,
            RestartOpts {
                max_wait: Some(Duration::from_millis(1)),
                now: false,
            },
        )
        .await;
        result.expect("restarts, forced");
        assert!(script.shutdowns().is_empty());
        assert_eq!(script.forced(), vec![OLD.to_owned()]);
        insta::assert_snapshot!(out);
    }

    #[tokio::test]
    async fn a_toker_that_never_answers_has_no_instance_to_force_and_says_so() {
        let script = Script::new(vec![Err(ControlError::Timeout)]);
        let (result, out) = run(
            &script,
            RestartOpts {
                max_wait: Some(Duration::from_millis(1)),
                now: false,
            },
        )
        .await;
        let error = result.expect_err("nothing to address");
        assert!(script.shutdowns().is_empty() && script.forced().is_empty());
        insta::assert_snapshot!(format!("{out}---\n{error:#}"));
    }

    #[tokio::test]
    async fn a_drain_that_never_ends_is_forced_after_the_up_timeout() {
        let mut statuses: Vec<_> = (0..QUIET_POLLS).map(|_| status(0, OLD)).collect();
        // The old instance keeps answering through the whole drain
        // budget; only once forced does the successor show.
        statuses.push(status(1, OLD));
        let script = Script::new(statuses).reviving();
        let (result, out) = run(&script, RestartOpts::default()).await;
        result.expect("restarts, forced after the drain times out");
        assert_eq!(script.shutdowns(), vec![OLD.to_owned()]);
        assert_eq!(script.forced(), vec![OLD.to_owned()]);
        insta::assert_snapshot!(out);
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
        assert_eq!(script.forced().len(), 1, "forced once, not in a loop");
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
