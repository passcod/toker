//! Axum listener, routing, and streaming.
//!
//! Plan: "Server core" + "Deployment" — one loopback socket (all frontend
//! protocols plus `/_toker/*`), socket-activated via `listenfd` with a
//! direct bind fallback; buffered request bodies for gating, pass-through
//! response streams with a crash-proof SSE side-parser for
//! usage/model/cost.
//!
//! Routes:
//!
//! - OpenAI-chat frontend (phase 1, → openrouter):
//!   - `POST /v1/chat/completions` — the usage path: buffered, parsed to
//!     the IR, fidelity-checked, routed, recorded ([`proxy`]).
//!   - `GET /v1/models` — transparent forwarding, no recording.
//! - Anthropic frontend (phase 2, → anthropic sub/api backends):
//!   - `POST /v1/messages` — the anthropic usage path, fully recorded;
//!     `count_tokens` and `batches` run the same pipeline (they are not
//!     gated — gates arrive with the quota-gate unit) but carry no usage,
//!     so they record nothing in practice ([`anthropic`]).
//!   - The batch-result GETs (and cancel) — transparent forwarding like
//!     `/v1/models`.
//! - `GET /_toker/status`, `POST /_toker/models/merge`,
//!   `GET /_toker/session`, `POST /_toker/shutdown` — the control
//!   endpoints, gated by a custom header ([`control`]); `session` is the
//!   attribution plugin's query, `shutdown` is `toker restart`'s drain.
//!   Any other `/_toker/` path is a local 404.
//! - Every other path — transparent forwarding to the default anthropic
//!   backend ([`anthropic::unmatched`]), as the predecessor forwarded
//!   everything but its control path.
//!
//! Every route above also answers under a `/f/<frontend>` prefix: the
//! router strips it before matching and keeps the name
//! ([`FrontendName`]), which picks the frontend's gate-notice style
//! (`[notices]`). Setup writes the prefix into the frontends it patches;
//! an unprefixed request is an unknown frontend.
//!
//! A backend is enabled by its `[providers.X]` block's presence. A
//! protocol with no enabled backend answers its routes with a
//! not-configured error in that protocol's own error shape
//! ([`anthropic_not_configured`], [`openai_not_configured`]) and reaches
//! no upstream; that includes the unmatched-path fallback, which is the
//! anthropic default's. `GET /v1/models` is the openai backend's when
//! there is one and the anthropic default's otherwise.
//!
//! Timeouts: axum applies no default request or idle timeout on the
//! client side, so streams run as long as both ends keep the connection
//! open — the plan's `requestTimeout = 0`; axum tears the connection down
//! itself when the client hangs up (the body stream's Drop aborts the
//! upstream, [proxy] implements it). The upstream client
//! ([`Server::new`]) sets a connect timeout and an idle timeout
//! ([`UPSTREAM_IDLE_TIMEOUT`]): time *between* reads, never a total, so a
//! long healthy stream is left alone while a stalled one fails. It covers
//! the wait for the response headers too. Its expiry is an upstream
//! failure like any other: before a body, a 502; mid-body, the client's
//! response is aborted rather than ended (no row either way).
//!
//! [`RecordCtx`]-based recording notes the clock is used **only** for row
//! fields (ts, duration) — never for bytes (invariant 4); serialisation
//! decisions never consult runtime state.

pub(crate) mod anthropic;
pub(crate) mod codex;
pub(crate) mod control;
pub(crate) mod proxy;
pub(crate) mod quota_events;
mod record;
mod record_anthropic;

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use axum::routing::{get, post};

use crate::catalog::fetched::{self, FetchedCatalog, FetchedCatalogs};
use crate::config::Config;
use crate::middleware::awake::{self, AwakeState, LockSpawner};
use crate::middleware::lanes;
use crate::middleware::models::ModelStore;
use crate::providers::{AnthropicApi, AnthropicSub, CodexSub, OpenRouter, Provider};
use crate::secrets::{self, KEYRING_READ_TIMEOUT, OsKeyring, SecretStore};
use crate::store::Store;

use record::now_ms;

/// How many ledger rows the startup seed reads (the predecessor's
/// 16 MiB log tail, as a row count): enough to span several days of heavy
/// use, which is far more than the lane table or the served-model map look
/// back over.
const SEED_ROWS: u64 = 20_000;

/// How long after one borrowed anthropic catalogue fetch before another
/// may start. The borrow fires from the request path while the catalogue
/// is stale, so an endpoint that keeps failing would otherwise be called
/// on every turn.
const CATALOG_BORROW_RETRY_MS: i64 = 60 * 60 * 1000;

/// The `anthropic-version` the models listing is requested at, the API's
/// only stable version.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The beta flag a subscription bearer needs on the API's own paths; the
/// listing answered 200 with it when checked.
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";

/// How long the upstream may go silent — no response headers, no body
/// bytes — before toker gives up on it. Time between reads, never a
/// total: a whole-response deadline would kill long healthy SSE streams,
/// which is why there was once no read timeout at all. But with none, a
/// stalled upstream on a live connection keeps its [`InFlightGuard`]
/// forever, and with it the idle-sleep lock. 300 s is the predecessor's
/// read timeout: the API sends pings through a long turn, so five minutes
/// of silence is a dead stream, not a slow one.
pub const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// The upstream client: a connect timeout and the between-reads idle
/// timeout, never an overall one (see [`UPSTREAM_IDLE_TIMEOUT`]).
fn upstream_client(idle: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        // reqwest's read timeout resets after every successful read, and
        // also bounds the wait for the response headers: exactly the
        // between-chunks idle timeout, on every path that uses this
        // client, with no per-path wrapper to forget.
        .read_timeout(idle)
        .build()
}

/// The running proxy: config, ledger, upstream client, backend providers,
/// and the learned model store, cloned cheaply into every request handler.
#[derive(Clone)]
pub struct Server {
    pub(crate) store: Arc<Store>,
    pub(crate) config: Arc<Config>,
    /// The learned model store (days served + maxPrompt, the day-based
    /// family election) with its in-memory recently-served map, seeded
    /// from the ledger tail at startup.
    pub(crate) models: Arc<ModelStore>,
    /// The fetched models catalogues (one per source: openrouter's
    /// public listing, anthropic's presence list, the codex backend's
    /// own models endpoint — see [`crate::catalog::fetched`]),
    /// refreshed by the background task
    /// [`Server::spawn_catalog_refresh`] spawns at startup and every
    /// [`fetched::CACHE_TTL_MS`]. Held so the record paths and a
    /// future `/_toker` endpoint can read them without touching disk;
    /// the TUI reads the same data from the cache files, read-only.
    pub(crate) catalogs: Arc<std::sync::RwLock<FetchedCatalogs>>,
    /// The models-cache directory, set once the background refresh
    /// starts ([`Server::spawn_catalog_refresh`]). Until then, and so in
    /// every test that only drives the router, the borrowed anthropic
    /// fetch never runs and nothing touches the real cache.
    pub(crate) catalog_dir: Arc<std::sync::OnceLock<std::path::PathBuf>>,
    /// When the last borrowed anthropic catalogue fetch started (unix
    /// ms, 0 for never): the single-flight claim and the retry backoff
    /// of [`Server::borrow_catalog_credential`].
    pub(crate) catalog_borrowed_at: Arc<AtomicI64>,
    /// The shared upstream HTTP client. Connect and between-reads idle
    /// timeouts only — no overall deadline, so a stream lives as long as
    /// its upstream keeps talking (see [`UPSTREAM_IDLE_TIMEOUT`]).
    pub(crate) http: reqwest::Client,
    /// The one openai-chat backend; a trait object because routing
    /// selects by `provider/model` prefix and later phases add providers
    /// to exactly this slot. Every backend slot is `None` when its
    /// `[providers.X]` block is absent from the config: the routes that
    /// would reach it answer a not-configured error instead.
    pub(crate) openrouter: Option<Arc<dyn Provider>>,
    /// The anthropic subscription backend.
    pub(crate) anthropic_sub: Option<Arc<dyn Provider>>,
    /// The anthropic API backend.
    pub(crate) anthropic_api: Option<Arc<dyn Provider>>,
    /// The codex subscription backend, as routing sees it (the trait
    /// object: prefix routing and the protocol default resolve by id).
    pub(crate) codex_sub: Option<Arc<dyn Provider>>,
    /// The codex subscription backend, concretely — the translation
    /// branch needs [`crate::providers::codex::CodexSub`]'s own methods
    /// (auth-for-turn, the codex header block) that the trait does not
    /// carry. Same allocation as [`Server::codex_sub`].
    pub(crate) codex_turn: Option<Arc<CodexSub>>,
    /// In-flight usage-path requests (the anthropic `/v1/messages`
    /// non-ping ones and the openai chat completions — a running request
    /// holds the machine awake regardless of protocol): the sleep lock's
    /// other input besides the lane table.
    pub(crate) in_flight: Arc<AtomicUsize>,
    /// The idle-sleep lock's state, or
    /// `None` when `awake` is off (`awake = false` in config
    /// — never hold, never spawn).
    pub(crate) awake: Option<Arc<Mutex<AwakeState>>>,
    /// Process start, for `/_toker/status` uptime.
    pub(crate) started: Instant,
    /// This process's identity in `/_toker/status`, a random id drawn
    /// once at construction. `toker restart` tells the new instance from
    /// the old by it: uptime alone cannot, since an instance asked to
    /// shut down a second after it started reads the same as its
    /// successor.
    pub(crate) instance: Arc<str>,
    /// Process start on the wall clock (ms since the epoch), for status.
    pub(crate) started_ms: i64,
    /// Fired by `POST /_toker/shutdown`: [`Server::serve_listener`]
    /// stops accepting, drains every open connection, and returns. A
    /// `Notify` keeps the permit when nothing waits yet, so a request
    /// that lands before the listener awaits is not lost.
    pub(crate) shutdown: Arc<tokio::sync::Notify>,
    /// The console quota events' per-backend latches (see
    /// [`quota_events`]).
    pub(crate) quota_events: Arc<quota_events::QuotaEvents>,
}

impl Server {
    /// Build the server: resolve the provider credentials once, validate
    /// the routing table, build the upstream client, and arm the
    /// idle-sleep lock over this host's platform command.
    pub fn new(config: Config, store: Arc<Store>) -> anyhow::Result<Server> {
        Self::with_awake_spawner(config, store, Box::new(awake::ProcessSpawner))
    }

    /// [`Server::new`], with the sleep-lock spawner injected. The real
    /// spawner takes a REAL idle-sleep lock via systemd-inhibit /
    /// gnome-session-inhibit, so tests inject a fake through here and
    /// never arm a real inhibitor on the host they run on.
    pub fn with_awake_spawner(
        config: Config,
        store: Arc<Store>,
        spawner: Box<dyn LockSpawner>,
    ) -> anyhow::Result<Server> {
        Self::with_seams(config, store, spawner, Arc::new(OsKeyring))
    }

    /// [`Server::with_awake_spawner`], with the keyring injected too: the
    /// stored API keys are read from it here, once, in the service — the
    /// only process that ever reads the keyring. Tests pass a
    /// [`crate::secrets::MemoryStore`] and never touch the real one.
    pub fn with_seams(
        config: Config,
        store: Arc<Store>,
        spawner: Box<dyn LockSpawner>,
        secrets: Arc<dyn SecretStore>,
    ) -> anyhow::Result<Server> {
        // The config's own validation, again: a Config built by hand (the
        // tests, any embedder) must not reach routing with a default that
        // names a disabled backend — `default_anthropic` relies on it.
        config.validate()?;
        let http = upstream_client(UPSTREAM_IDLE_TIMEOUT)?;
        let openrouter = config.openrouter.as_ref().map(|openrouter| {
            Arc::new(OpenRouter::new(
                openrouter.upstream.clone(),
                openrouter.api_key(|| {
                    secrets::read_key(secrets.clone(), "openrouter", KEYRING_READ_TIMEOUT)
                }),
            )) as Arc<dyn Provider>
        });
        let anthropic_sub = config.anthropic_sub.as_ref().map(|sub| {
            Arc::new(AnthropicSub::new(
                sub.upstream.clone(),
                sub.model_map.clone(),
            )) as Arc<dyn Provider>
        });
        let anthropic_api = config.anthropic_api.as_ref().map(|api| {
            Arc::new(AnthropicApi::new(
                api.upstream.clone(),
                api.api_key(|| {
                    secrets::read_key(secrets.clone(), "anthropic_api", KEYRING_READ_TIMEOUT)
                }),
                api.model_map.clone(),
            )) as Arc<dyn Provider>
        });
        let codex_turn = match &config.codex_sub {
            Some(codex) => Some(Arc::new(CodexSub::new(
                codex.upstream.clone(),
                codex.originator.clone(),
                codex.auth_path.clone(),
                codex.refresh_url.clone(),
                codex.model_map.clone(),
                codex.client_version.clone(),
                codex.version_probe,
            )?)),
            None => None,
        };
        let codex_sub = codex_turn.clone().map(|codex| codex as Arc<dyn Provider>);

        // The lock exists only while the toggle is
        // on, and an unavailable platform says so once, at startup —
        // "`awake` has no effect" there, `awake` here.
        let awake = config.awake.then(|| {
            let state = AwakeState::new(
                awake::platform_command(awake::INHIBIT_WHO, awake::INHIBIT_WHY),
                spawner,
            );
            if !state.available() {
                tracing::warn!("no idle-sleep lock on this platform; `awake` has no effect");
            }
            Arc::new(Mutex::new(state))
        });

        // The startup seed: the newest ledger rows, read
        // once, feed both state stores (lanes and served-models).
        // Reseeding is idempotent, so every
        // Server::new — serve, tests, restarts — rebuilds the same state.
        let total = store.count_requests()?;
        let seed = store.requests_since(0, SEED_ROWS)?;
        // How far the served map vouches: the tail read everything (no
        // cut) →
        // the served map vouches from the beginning;
        // else only from the oldest row the tail kept.
        let covered =
            (total as u64 > SEED_ROWS).then(|| seed.first().map_or_else(now_ms, |row| row.ts_ms));
        lanes::reseed(&store, &seed)?;
        let models = Arc::new(ModelStore::seeded(store.clone(), &seed, covered));
        Ok(Server {
            store,
            config: Arc::new(config),
            http,
            openrouter,
            anthropic_sub,
            anthropic_api,
            codex_sub,
            codex_turn,
            models,
            catalogs: Arc::new(std::sync::RwLock::new(FetchedCatalogs::default())),
            catalog_dir: Arc::default(),
            catalog_borrowed_at: Arc::default(),
            in_flight: Arc::new(AtomicUsize::new(0)),
            awake,
            started: Instant::now(),
            instance: Arc::from(uuid::Uuid::new_v4().simple().to_string()),
            started_ms: now_ms(),
            shutdown: Arc::default(),
            quota_events: Arc::default(),
        })
    }

    /// Announce what one response's meter snapshot says about its
    /// backend's quota (see [`quota_events`]). Call it before the
    /// snapshot is saved: the first reading after a restart seeds its
    /// latch from the one the store still holds. Never fails and never
    /// panics out (invariant 3): a lost announcement is the worst case.
    pub(crate) fn note_quota(&self, backend: &str, snapshot: &serde_json::Value) {
        let announced = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let prior = || match self.store.load_meters(backend) {
                Ok(stored) => stored.map(|stored| stored.snapshot),
                Err(error) => {
                    tracing::error!(%error, "meter snapshot load for the quota latch failed");
                    None
                }
            };
            for event in self
                .quota_events
                .note(backend, snapshot, prior, record::now_ms())
            {
                quota_events::emit(backend, &event);
            }
        }));
        if announced.is_err() {
            tracing::error!("quota event check panicked");
        }
    }

    /// Replace the upstream idle timeout ([`UPSTREAM_IDLE_TIMEOUT`] by
    /// default). For tests, which cannot wait five minutes to see a
    /// stalled upstream abort.
    pub fn set_upstream_idle_timeout(&mut self, idle: Duration) -> anyhow::Result<()> {
        self.http = upstream_client(idle)?;
        Ok(())
    }

    /// Resolve an anthropic backend by provider name — routing and the
    /// configured protocol default both resolve here (plan: Routing).
    /// `None` for an unknown name and for a known backend whose block is
    /// absent from the config.
    pub(crate) fn anthropic_backend(&self, name: &str) -> Option<&Arc<dyn Provider>> {
        match name {
            "anthropic_sub" => self.anthropic_sub.as_ref(),
            "anthropic_api" => self.anthropic_api.as_ref(),
            "codex_sub" => self.codex_sub.as_ref(),
            _ => None,
        }
    }

    /// The configured default anthropic backend; `None` exactly when no
    /// anthropic backend is enabled (validated at startup: a default
    /// always names an enabled backend, and one is inferred whenever any
    /// is enabled).
    pub(crate) fn default_anthropic(&self) -> Option<&Arc<dyn Provider>> {
        self.anthropic_backend(self.config.default_backend_anthropic.as_deref()?)
    }

    // ── the idle-sleep lock ──────────────────────────────────────────

    /// Take or drop the sleep lock to match the lane table and the
    /// in-flight count, and write an `awake` row on every held/want flip.
    ///
    /// The whole body runs under a catch — "the lock is not worth
    /// a request" — so a panic here is caught and logged, never
    /// propagated to the request it rode in on. A store error just loses
    /// this one evaluation.
    pub(crate) fn evaluate_awake(&self) {
        let Some(awake) = &self.awake else {
            return; // The toggle is off: no lock, nothing to evaluate.
        };
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| self.evaluate_awake_inner(awake)));
    }

    fn evaluate_awake_inner(&self, awake: &Arc<Mutex<AwakeState>>) {
        let lanes = match self.store.load_lanes() {
            Ok(lanes) => lanes,
            Err(error) => {
                tracing::error!(%error, "awake: lane table load failed");
                return;
            }
        };
        let in_flight = self.in_flight.load(Ordering::SeqCst) as u64;
        let now = now_ms();
        let decision = awake::decide_awake(&lanes, in_flight, now);
        // A poisoned lock recovers: the state is held/backoff bookkeeping
        // only, and a panic inside an evaluation must not disable the
        // sleep lock for the rest of the process's life.
        let transition = {
            let mut state = match awake.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.evaluate(&decision, now)
        };
        if let Some(transition) = transition {
            record::record_awake(self, &transition, now);
        }
    }

    /// A usage-path request is now in flight:
    /// a lane's `updated_ms` moves only when a
    /// response finishes, and one long turn can outlast a 5-minute tier,
    /// so a request being served holds the machine awake, pings aside.
    pub(crate) fn begin_in_flight(&self) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.evaluate_awake();
        InFlightGuard {
            server: self.clone(),
        }
    }

    /// The other half of the guard's Drop (the close event fires
    /// however the exchange ends — completion, error, hangup).
    fn end_in_flight(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.evaluate_awake();
    }

    /// The full route table, behind the frontend-prefix strip: a request
    /// to `/f/<frontend>/<path>` is routed as `/<path>`, with the
    /// frontend's name riding along as a [`FrontendName`] extension (see
    /// [`strip_frontend_prefix`]).
    pub fn router(&self) -> Router {
        // The strip runs before routing because it wraps a router whose
        // only route is the fallback: an outer layer sees the request
        // first, and the inner router matches the rewritten path.
        Router::new()
            .fallback_service(self.routes())
            .layer(axum::middleware::map_request(strip_frontend_prefix))
    }

    /// The route table itself, matched on unprefixed paths.
    fn routes(&self) -> Router {
        Router::new()
            .route("/v1/chat/completions", post(proxy::chat_completions))
            .route("/v1/responses", post(codex::responses))
            .route("/v1/models", get(proxy::models))
            .route("/v1/messages", post(anthropic::messages))
            .route("/v1/messages/count_tokens", post(anthropic::count_tokens))
            .route(
                "/v1/messages/batches",
                post(anthropic::batches_create).get(anthropic::batches_list),
            )
            .route("/v1/messages/batches/{id}", get(anthropic::batches_get))
            .route(
                "/v1/messages/batches/{id}/results",
                get(anthropic::batches_results),
            )
            .route(
                "/v1/messages/batches/{id}/cancel",
                post(anthropic::batches_cancel),
            )
            .route("/_toker/status", get(control::status))
            .route("/_toker/session", get(control::session))
            .route("/_toker/models/merge", post(control::models_merge))
            .route("/_toker/shutdown", post(control::shutdown))
            // Everything else passes through to the default anthropic
            // backend, as the predecessor forwarded every path but its
            // control path. A known path with the wrong method still
            // answers axum's 405: the fallback catches unmatched paths
            // only, and every listed route is one whose methods are known.
            .fallback(anthropic::unmatched)
            .with_state(self.clone())
    }

    /// Listen and serve: a socket-activated listener (`LISTEN_FDS`, via
    /// [`listenfd`]) when systemd handed us one, else a direct loopback
    /// bind for dev and tests. Returns once a `POST /_toker/shutdown`
    /// has drained (see [`Server::serve_listener`]), and the process then
    /// exits 0; under systemd, `Restart=always` starts the next instance.
    pub async fn serve(self) -> anyhow::Result<()> {
        let mut listenfd = listenfd::ListenFd::from_env();
        let listener = match listenfd.take_tcp_listener(0)? {
            Some(std_listener) => {
                std_listener.set_nonblocking(true)?;
                tokio::net::TcpListener::from_std(std_listener)?
            }
            None => tokio::net::TcpListener::bind(("127.0.0.1", self.config.port)).await?,
        };
        let address = listener.local_addr()?;
        tracing::info!("toker listening on http://{address}");
        self.spawn_state_prune();
        self.spawn_awake_timer();
        self.spawn_catalog_refresh();
        // A restart inside a live session takes the lock straight back.
        self.evaluate_awake();
        self.serve_listener(listener).await
    }

    /// Serve the router on `listener` until a shutdown request, then
    /// drain: stop accepting, let every response under way finish (SSE
    /// streams included), close idle keep-alive connections, and return.
    ///
    /// The drain has no deadline of its own, because the point of the
    /// shutdown is never to cut a stream. It is bounded anyway: a stalled
    /// upstream fails after [`UPSTREAM_IDLE_TIMEOUT`] like any other, and
    /// a client that hangs up ends its exchange.
    ///
    /// Dropping the listener closes this process's descriptor only. Under
    /// socket activation systemd keeps its own, so connections that arrive
    /// during the drain queue in the kernel for the next instance; on a
    /// direct bind the port closes and they are refused.
    ///
    /// The background tasks [`Server::serve`] spawns are not stopped here:
    /// they end with the runtime, when the process exits.
    pub async fn serve_listener(self, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
        let shutdown = self.shutdown.clone();
        axum::serve(listener, self.router())
            .with_graceful_shutdown(async move { shutdown.notified().await })
            .await?;
        tracing::info!("drained; exiting");
        self.release_awake_for_exit();
        Ok(())
    }

    /// Ask [`Server::serve_listener`] to drain and return.
    pub(crate) fn begin_shutdown(&self) {
        tracing::info!(
            in_flight = self.in_flight.load(Ordering::SeqCst),
            "shutdown requested; draining"
        );
        self.shutdown.notify_one();
    }

    /// This instance's id, as `/_toker/status` reports it.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Drop the sleep lock for good, writing no row. The lock is the
    /// inhibitor child, which would die with this process anyway; killing
    /// it here means a drained exit never leaves one behind, even for the
    /// moment before the PID watch notices. No row, because a release row
    /// says the sessions went quiet, and at exit they need not have: the
    /// next instance reseeds the lanes and takes the lock straight back.
    /// After this the timer and any late evaluation leave the lock alone.
    fn release_awake_for_exit(&self) {
        let Some(awake) = &self.awake else {
            return;
        };
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut state = match awake.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.shut_down();
        }));
    }

    /// The sleep lock's wall-clock re-evaluation on a 60-second
    /// cadence. Wall
    /// clock rather than a timeout aimed at the expiry: the interval
    /// runs on a monotonic clock that stops while the machine is
    /// suspended, so a release due at 18:30 would otherwise slip by
    /// however long the lid was shut — the decision reads the wall
    /// clock, so the first tick after a resume re-evaluates correctly.
    fn spawn_awake_timer(&self) {
        if self.awake.is_none() {
            return; // No lock, no timer.
        }
        let server = self.clone();
        tokio::spawn(async move {
            let tick = Duration::from_millis(awake::AWAKE_TICK_MS);
            let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + tick, tick);
            loop {
                timer.tick().await;
                server.evaluate_awake();
            }
        });
    }

    /// The state-table prunes on the lanes' 30-second flush cadence
    /// (never per request): cheap SQL statements on a timer — the
    /// upserts themselves go straight into the store on every response,
    /// so nothing here carries state. The first tick fires at startup,
    /// so a restart clears what accumulated while the service was down.
    ///
    /// Allowances go once their window's reset has passed (ctp pruned
    /// its `allowances.json` on every load and save to the same rule).
    /// The gate reads one session's rows by the primary key, so the
    /// table's size never lands on the request path; the prune keeps the
    /// table, and the TUI's whole-table read, from growing without end.
    fn spawn_state_prune(&self) {
        let store = self.store.clone();
        tokio::spawn(async move {
            let mut timer =
                tokio::time::interval(std::time::Duration::from_millis(lanes::LANE_FLUSH_MS));
            loop {
                timer.tick().await;
                let now = record::now_ms();
                // Either prune is never worth a request (invariant 3
                // spirit): a failure retries on the next tick.
                match store.prune_lanes(now, lanes::LANE_MAX, lanes::LANE_MAX_AGE_MS) {
                    Ok(0) => {}
                    Ok(count) => {
                        tracing::debug!("lane prune removed {count} lanes");
                    }
                    Err(error) => {
                        tracing::error!(%error, "lane prune failed");
                    }
                }
                match store.prune_allowances(now) {
                    Ok(0) => {}
                    Ok(count) => {
                        tracing::debug!("allowance prune removed {count} allowances");
                    }
                    Err(error) => {
                        tracing::error!(%error, "allowance prune failed");
                    }
                }
            }
        });
    }

    // ── the fetched models catalogues ────────────────────────────────

    /// Install (or replace) one source's fetched catalogue in memory —
    /// the same write the background refresh makes per cycle, exposed so
    /// embedders and tests can seed a catalogue without touching the
    /// disk cache. The source must be one of [`fetched::SOURCES`] to be
    /// reachable through the provider-mapped lookups.
    pub fn install_catalog(&self, source: &'static str, catalog: FetchedCatalog) {
        self.catalogs
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .set(source, catalog);
    }

    /// The models-catalogue sources as this server is configured
    /// (see [`crate::catalog::fetched`]): openrouter's public listing,
    /// anthropic's listing, and the codex backend's own models
    /// endpoint. Built per refresh cycle so the codex credentials and
    /// client version are read fresh, never cached here.
    ///
    /// Only the enabled backends' sources: a disabled backend's catalogue
    /// is never fetched (for codex that would read a login the operator
    /// did not hand to toker).
    fn catalog_sources(&self) -> Vec<fetched::CatalogSource> {
        let mut sources = Vec::new();
        if let Some(openrouter) = &self.openrouter {
            sources.push(fetched::CatalogSource {
                provider: "openrouter",
                // The upstream base already includes `/v1`, so the
                // frontend's own models path is the endpoint (and the
                // public listing needs no credential).
                url: openrouter.endpoint("/v1/models"),
                headers: HeaderMap::new(),
                fetch: true,
            });
        }
        if let Some(anthropic) = self.anthropic_sub.as_ref().or(self.anthropic_api.as_ref()) {
            // The stored API key when there is one. Without it toker
            // holds no anthropic credential, and the source reads the
            // cache only: the borrowed fetch fills it.
            let source = match self.stored_anthropic_key() {
                Some(key) => anthropic_catalog_source(anthropic.as_ref(), key, true),
                None => anthropic_catalog_source(anthropic.as_ref(), HeaderMap::new(), false),
            };
            sources.push(source);
        }
        if let Some(codex) = &self.codex_turn {
            sources.push(fetched::CatalogSource {
                provider: "codex_sub",
                // The codex CLI's own request shape: the version the
                // handshake speaks rides as the client_version query.
                // The stored login as-is, no refresh attempt — a stale
                // token simply fails into the fallback.
                url: codex.endpoint(&format!(
                    "/models?client_version={}",
                    codex.client_version()
                )),
                headers: codex
                    .auth()
                    .and_then(|auth| auth.access_token().and_then(fetched::bearer_header))
                    .map(|value| [(header::AUTHORIZATION, value)].into_iter().collect())
                    .unwrap_or_default(),
                fetch: true,
            });
        }
        sources
    }

    /// Refresh all three fetched catalogues into
    /// [`Server::catalogs`], one write-lock swap per cycle. Each
    /// source goes through [`fetched::refresh`] (cache → fetch →
    /// stale fallback, internally); a source that still errors keeps
    /// its previous entry (an empty swap would throw away good data
    /// over bookkeeping). Failures log at debug and retry next cycle.
    async fn refresh_catalogs(&self) {
        let Some(dir) = self.catalog_dir.get() else {
            return;
        };
        let previous = self
            .catalogs
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut next = FetchedCatalogs::default();
        for source in self.catalog_sources() {
            match fetched::refresh(&source, dir, &self.http, record::now_ms()).await {
                Ok(catalog) => {
                    tracing::debug!(
                        provider = source.provider,
                        "models catalogue refreshed: {} models",
                        catalog.models.len()
                    );
                    next.set(source.provider, catalog);
                }
                Err(error) => {
                    tracing::debug!(
                        provider = source.provider,
                        %error,
                        "models catalogue refresh failed; keeping the previous entry"
                    );
                    if let Some(previous) = previous.get(source.provider) {
                        next.set(source.provider, previous.clone());
                    }
                }
            }
        }
        *self
            .catalogs
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
    }

    /// The models-catalogue refresh: all three sources at startup, then
    /// every [`fetched::CACHE_TTL_MS`] — the timers' pattern (the
    /// interval's first tick fires immediately). Nothing here can
    /// delay or fail serving: the task runs beside the listener, each
    /// fetch is bounded by [`fetched::FETCH_TIMEOUT`], and every
    /// failure is the refresh chain's own fallback plus a debug log.
    fn spawn_catalog_refresh(&self) {
        match fetched::cache_dir() {
            Ok(dir) => {
                let _ = self.catalog_dir.set(dir);
            }
            Err(_) => {
                tracing::debug!("no data home; models catalogues disabled");
                return;
            }
        }
        let server = self.clone();
        tokio::spawn(async move {
            let tick = Duration::from_millis(fetched::CACHE_TTL_MS as u64);
            let mut timer = tokio::time::interval(tick);
            loop {
                timer.tick().await;
                server.refresh_catalogs().await;
            }
        });
    }

    /// The `anthropic_api` backend's stored key as the listing's
    /// headers, marked sensitive; `None` without that backend or a key.
    fn stored_anthropic_key(&self) -> Option<HeaderMap> {
        let api = self.anthropic_api.as_ref()?;
        let mut headers = HeaderMap::new();
        api.inject_auth(&mut headers);
        if headers.is_empty() {
            return None;
        }
        for value in headers.values_mut() {
            value.set_sensitive(true);
        }
        Some(headers)
    }

    /// Fetch the anthropic models listing with the bearer of a request on
    /// its way to `anthropic_sub`, when toker holds no anthropic key of
    /// its own (see the fetched module's auth notes). Called from the
    /// usage path before the request goes upstream; it only ever spawns,
    /// so the request never waits on it and nothing here can fail it.
    ///
    /// It does nothing unless every condition holds: the background
    /// refresh is running, the backend is the subscription, no API key
    /// is stored, the request carries a bearer, the catalogue in memory
    /// is empty or older than [`fetched::CACHE_TTL_MS`], and no borrowed
    /// fetch started within [`CATALOG_BORROW_RETRY_MS`]. The bearer is
    /// moved into that one GET and dropped with it.
    pub(crate) fn borrow_catalog_credential(&self, backend: &dyn Provider, incoming: &HeaderMap) {
        let Some(dir) = self.catalog_dir.get().cloned() else {
            return;
        };
        if backend.id() != "anthropic_sub" || self.stored_anthropic_key().is_some() {
            return;
        }
        let Some(authorization) = incoming.get(header::AUTHORIZATION) else {
            return;
        };
        let now = now_ms();
        let fresh = self
            .catalogs
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get("anthropic")
            .is_some_and(|catalog| {
                // An empty catalogue is stamped with the time of the
                // failure that produced it: absence, never freshness.
                !catalog.models.is_empty()
                    && now.saturating_sub(catalog.fetched_at_ms) < fetched::CACHE_TTL_MS
            });
        if fresh {
            return;
        }
        let last = self.catalog_borrowed_at.load(Ordering::SeqCst);
        if now.saturating_sub(last) < CATALOG_BORROW_RETRY_MS
            || self
                .catalog_borrowed_at
                .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return;
        }
        let mut authorization = authorization.clone();
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, authorization);
        headers.insert(
            HeaderName::from_static("anthropic-beta"),
            HeaderValue::from_static(ANTHROPIC_OAUTH_BETA),
        );
        let source = anthropic_catalog_source(backend, headers, true);
        let server = self.clone();
        tokio::spawn(async move {
            match fetched::refresh(&source, &dir, &server.http, now_ms()).await {
                Ok(catalog) if !catalog.models.is_empty() => {
                    tracing::debug!(
                        "anthropic models catalogue refreshed on a borrowed credential: {} models",
                        catalog.models.len()
                    );
                    server.install_catalog("anthropic", catalog);
                }
                Ok(_) => {
                    tracing::debug!("borrowed anthropic models fetch left the catalogue empty");
                }
                Err(error) => {
                    tracing::debug!(%error, "borrowed anthropic models fetch failed");
                }
            }
        });
    }
}

/// The frontend a request came through: the name in its base URL's
/// `/f/<frontend>` prefix, which setup writes into each frontend it
/// patches (`/f/claude`, `/f/workhorse`). Absent for an unprefixed
/// request — an unknown frontend. It selects the gate notices' style
/// ([`crate::config::NoticesConfig`]) and nothing else: routing ignores
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrontendName(pub(crate) String);

/// The frontend name a request carries, if it came through a prefix.
pub(crate) fn frontend_of(extensions: &axum::http::Extensions) -> Option<&str> {
    extensions
        .get::<FrontendName>()
        .map(|frontend| frontend.0.as_str())
}

/// Split `/f/<frontend>/<rest>` into the frontend name and `/<rest>`
/// (`/` when nothing follows the name). `None` for any other path, and
/// for a name outside [`crate::config::is_frontend_name`] — such a path
/// is routed as it is, like any unknown path.
fn split_frontend(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/f/")?;
    let (name, rest) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest, "/"),
    };
    crate::config::is_frontend_name(name).then_some((name, rest))
}

/// The router's first step: strip a `/f/<frontend>` prefix off the
/// request path (the query rides along untouched) and record the name.
/// Never fails a request: a URI that would not rebuild is passed on as
/// it came (invariant 3 — accounting never breaks a session).
async fn strip_frontend_prefix(mut request: axum::extract::Request) -> axum::extract::Request {
    let Some((name, rest)) = split_frontend(request.uri().path()) else {
        return request;
    };
    let name = name.to_owned();
    let path_and_query = match request.uri().query() {
        Some(query) => format!("{rest}?{query}"),
        None => rest.to_owned(),
    };
    let mut parts = request.uri().clone().into_parts();
    let Ok(path_and_query) = path_and_query.parse() else {
        return request;
    };
    parts.path_and_query = Some(path_and_query);
    let Ok(uri) = axum::http::Uri::from_parts(parts) else {
        return request;
    };
    *request.uri_mut() = uri;
    request.extensions_mut().insert(FrontendName(name));
    request
}

/// The header toker sets on its own not-configured answers, so the setup
/// wizard's wiring probe can tell "toker answered for a protocol with no
/// backend" from an upstream's verdict (see [`crate::setup::verify`]).
pub const NOT_CONFIGURED_HEADER: &str = "x-toker-not-configured";

/// The message both not-configured answers carry: what is missing and
/// where it goes.
fn not_configured_message(protocol: &str) -> String {
    format!(
        "toker has no {protocol} backend configured: add a [providers.<name>] block to \
         toker.toml, or run `toker setup`"
    )
}

/// The anthropic routes' answer when no anthropic backend is enabled: an
/// anthropic-shaped error, so the client shows the message instead of
/// failing to parse one. 404 rather than a 5xx, which the clients retry
/// with backoff: no retry can configure a backend.
pub(crate) fn anthropic_not_configured() -> axum::response::Response {
    not_configured(proxy::ErrorWire::Anthropic, "anthropic")
}

/// The openai routes' answer when no openai-chat backend is enabled: the
/// openai error shape, for the same reasons as
/// [`anthropic_not_configured`].
pub(crate) fn openai_not_configured() -> axum::response::Response {
    not_configured(proxy::ErrorWire::Openai, "openai_chat")
}

pub(crate) fn responses_not_configured() -> axum::response::Response {
    not_configured(proxy::ErrorWire::Openai, "openai_responses")
}

fn not_configured(wire: proxy::ErrorWire, protocol: &'static str) -> axum::response::Response {
    let mut response = proxy::error_response(
        wire,
        axum::http::StatusCode::NOT_FOUND,
        proxy::WireError {
            anthropic_type: "not_found_error",
            openai_type: "invalid_request_error",
            openai_code: Some("backend_not_configured"),
        },
        &not_configured_message(protocol),
    );
    response.headers_mut().insert(
        NOT_CONFIGURED_HEADER,
        axum::http::HeaderValue::from_static(protocol),
    );
    response
}

/// One in-flight request's hold on the sleep lock: increments on entry,
/// and Drop is the decrement plus the re-evaluation — the close
/// event, which "fires however the exchange ends". A guard,
/// so no early return and no error path can leak the count: for a
/// streamed response the guard rides the body stream (it drops when axum
/// drops the body — client hangup or natural completion); for everything
/// else it drops when the handler's work is done.
pub(crate) struct InFlightGuard {
    server: Server,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.server.end_in_flight();
    }
}

/// The anthropic models source: the listing at the API's largest page
/// (no account sees anywhere near 1000 models, so one page is the whole
/// listing), the version header, and `credential` — the stored key, a
/// borrowed bearer, or nothing for a cache-only source.
fn anthropic_catalog_source(
    anthropic: &dyn Provider,
    credential: HeaderMap,
    fetch: bool,
) -> fetched::CatalogSource {
    let mut headers = credential;
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    fetched::CatalogSource {
        provider: "anthropic",
        url: anthropic.endpoint("/v1/models?limit=1000"),
        headers,
        fetch,
    }
}

#[cfg(test)]
mod tests {
    use super::Server;
    use crate::config::Config;
    use crate::store::Store;
    use axum::http::{HeaderMap, HeaderValue, header};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    /// A server over a scratch config whose providers point at
    /// unreachable localhost ports (nothing contacts them at
    /// construction; the codex version probe is disabled too, and the
    /// background catalog task only spawns in `serve`, which tests
    /// never run). The scratch `auth_path` keeps the codex version at
    /// the built-in floor and the bearer absent — hermetic, never the
    /// real `~/.codex`.
    fn server(dir: &std::path::Path) -> Server {
        server_with(dir, "http://localhost:10", "")
    }

    /// [`server`], with the anthropic upstream and extra config blocks
    /// given.
    fn server_with(dir: &std::path::Path, anthropic: &str, extra: &str) -> Server {
        std::fs::write(
            dir.join("toker.toml"),
            format!(
                r#"
awake = false
db_path = ":memory:"

[providers.openrouter]
upstream = "http://localhost:9/v1"

[providers.anthropic_sub]
upstream = "{anthropic}"

[providers.codex_sub]
upstream = "http://localhost:11/backend-api/codex"
auth_path = {auth_path:?}
version_probe = false
{extra}
"#,
                auth_path = dir.join("auth.json"),
            ),
        )
        .expect("write config");
        let config = Config::load_from(&dir.join("toker.toml")).expect("config loads");
        let store = Arc::new(Store::open(":memory:").expect("scratch store"));
        Server::new(config, store).expect("server builds")
    }

    /// The three sources the background task refreshes: the openrouter
    /// models path off the `/v1` base (public, unauthenticated),
    /// anthropic's listing (cache-only: no API key is stored), and
    /// the codex backend's own endpoint with the client_version query
    /// and the stored bearer — none in the scratch setup.
    #[test]
    fn catalog_sources_point_at_the_configured_models_endpoints() {
        let dir = crate::setup::test_dir("catalog-sources");
        let sources = server(&dir).catalog_sources();
        assert_eq!(sources.len(), 3);

        assert_eq!(sources[0].provider, "openrouter");
        assert_eq!(sources[0].url.as_str(), "http://localhost:9/v1/models");
        assert!(
            sources[0].headers.is_empty(),
            "the openrouter listing is public"
        );

        assert_eq!(sources[1].provider, "anthropic");
        assert_eq!(
            sources[1].url.as_str(),
            "http://localhost:10/v1/models?limit=1000"
        );
        assert!(
            !sources[1].fetch,
            "no stored key: the daily refresh reads the cache, never a certain 401"
        );
        assert!(!sources[1].headers.contains_key(header::AUTHORIZATION));
        assert!(!sources[1].headers.contains_key("x-api-key"));

        assert_eq!(sources[2].provider, "codex_sub");
        assert_eq!(
            sources[2].url.as_str(),
            format!(
                "http://localhost:11/backend-api/codex/models?client_version={}",
                crate::providers::codex::DEFAULT_CLIENT_VERSION
            ),
            "the codex CLI's own request shape: the version the handshake speaks rides as the query"
        );
        assert!(
            sources[2].headers.is_empty(),
            "no login in the scratch dir → no bearer (the request goes up cleanly and fails into the fallback)"
        );
    }

    /// With an `anthropic_api` key stored, the daily refresh fetches the
    /// anthropic listing with it, as `x-api-key` marked sensitive.
    #[test]
    fn a_stored_api_key_credentials_the_daily_anthropic_fetch() {
        let dir = crate::setup::test_dir("catalog-sources-key");
        let server = server_with(
            &dir,
            "http://localhost:10",
            r#"
[providers.anthropic_api]
upstream = "http://localhost:10"
api_key_env = "TOKER_TEST_NO_SUCH_KEY_VAR"
api_key = "ak-literal-test"
"#,
        );
        let sources = server.catalog_sources();
        let anthropic = sources
            .iter()
            .find(|source| source.provider == "anthropic")
            .expect("the anthropic source");
        assert!(anthropic.fetch);
        let key = anthropic.headers.get("x-api-key").expect("the stored key");
        assert_eq!(key, "ak-literal-test");
        assert!(key.is_sensitive(), "a Debug of the source never prints it");
        assert_eq!(anthropic.headers["anthropic-version"], "2023-06-01");
        assert!(!anthropic.headers.contains_key(header::AUTHORIZATION));
    }

    /// One recorded models request's headers, by name.
    #[derive(Debug, Clone, PartialEq)]
    struct Seen {
        uri: String,
        authorization: Option<String>,
        beta: Option<String>,
        version: Option<String>,
    }

    /// An anthropic upstream that answers `/v1/models` with `status` and a
    /// one-model listing, recording each request.
    async fn models_upstream(
        status: axum::http::StatusCode,
    ) -> (Arc<std::sync::Mutex<Vec<Seen>>>, String) {
        use axum::extract::{Request, State};
        use axum::response::IntoResponse;
        type Log = Arc<std::sync::Mutex<Vec<Seen>>>;
        async fn models(
            State((seen, status)): State<(Log, axum::http::StatusCode)>,
            request: Request,
        ) -> axum::response::Response {
            let named = |name: &str| {
                request
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            seen.lock().unwrap().push(Seen {
                uri: request.uri().to_string(),
                authorization: named("authorization"),
                beta: named("anthropic-beta"),
                version: named("anthropic-version"),
            });
            let body = serde_json::json!({"data": [{
                "type": "model",
                "id": "claude-sonnet-9",
                "max_input_tokens": 1_000_000
            }]});
            (status, axum::Json(body)).into_response()
        }
        let seen: Log = Arc::default();
        let app = axum::Router::new()
            .route("/v1/models", axum::routing::get(models))
            .with_state((seen.clone(), status));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("mock binds");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("mock serves") });
        (seen, format!("http://{addr}"))
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).expect("header"),
        );
        headers
    }

    /// Wait for the spawned borrowed fetch to settle: either the
    /// catalogue lands or `requests` requests have been seen and a beat
    /// has passed for the install.
    async fn settle(server: &Server, seen: &Arc<std::sync::Mutex<Vec<Seen>>>, requests: usize) {
        for _ in 0..200 {
            let installed = server
                .catalogs
                .read()
                .unwrap()
                .get("anthropic")
                .is_some_and(|catalog| !catalog.models.is_empty());
            if installed || seen.lock().unwrap().len() >= requests {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// A subscription request's bearer fetches a missing anthropic
    /// listing once: with the version and oauth beta headers, into the
    /// in-memory catalogue and the cache file. Once the catalogue is
    /// fresh, later requests fetch nothing.
    #[tokio::test]
    async fn a_subscription_bearer_fills_a_missing_anthropic_catalogue_once() {
        let dir = crate::setup::test_dir("catalog-borrow");
        let (seen, upstream) = models_upstream(axum::http::StatusCode::OK).await;
        let server = server_with(&dir, &upstream, "");
        let cache = dir.join("models-cache");
        server.catalog_dir.set(cache.clone()).expect("unset");
        let sub = server.anthropic_sub.clone().expect("sub");

        server.borrow_catalog_credential(sub.as_ref(), &bearer("sk-ant-oat01-test"));
        settle(&server, &seen, 1).await;
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![Seen {
                uri: "/v1/models?limit=1000".to_owned(),
                authorization: Some("Bearer sk-ant-oat01-test".to_owned()),
                beta: Some("oauth-2025-04-20".to_owned()),
                version: Some("2023-06-01".to_owned()),
            }]
        );
        assert_eq!(
            server
                .catalogs
                .read()
                .unwrap()
                .context_window_of("anthropic_sub", "claude-sonnet-9"),
            Some(1_000_000)
        );
        let cached = std::fs::read_to_string(cache.join("anthropic.json")).expect("persisted");
        assert!(
            !cached.contains("sk-ant-oat01-test"),
            "the cache holds the listing, never the credential"
        );

        // Fresh now: the next request borrows nothing, even past the
        // retry backoff.
        server.catalog_borrowed_at.store(0, Ordering::SeqCst);
        server.borrow_catalog_credential(sub.as_ref(), &bearer("sk-ant-oat01-test"));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// A failing listing is retried no sooner than the backoff, however
    /// many requests pass through meanwhile.
    #[tokio::test]
    async fn a_failed_borrowed_fetch_waits_out_the_backoff() {
        let dir = crate::setup::test_dir("catalog-borrow-401");
        let (seen, upstream) = models_upstream(axum::http::StatusCode::UNAUTHORIZED).await;
        let server = server_with(&dir, &upstream, "");
        server
            .catalog_dir
            .set(dir.join("models-cache"))
            .expect("unset");
        let sub = server.anthropic_sub.clone().expect("sub");

        for _ in 0..5 {
            server.borrow_catalog_credential(sub.as_ref(), &bearer("t"));
        }
        settle(&server, &seen, 1).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(seen.lock().unwrap().len(), 1, "one attempt per backoff");

        // Past the backoff, the next request tries again.
        server.catalog_borrowed_at.store(
            super::now_ms() - super::CATALOG_BORROW_RETRY_MS,
            Ordering::SeqCst,
        );
        server.borrow_catalog_credential(sub.as_ref(), &bearer("t"));
        settle(&server, &seen, 2).await;
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    /// Nothing is borrowed before the refresh task sets the cache dir
    /// (every router-only test), for a request with no bearer, for the
    /// API backend, or when an API key is stored.
    #[tokio::test]
    async fn the_borrow_stays_quiet_when_it_should() {
        let dir = crate::setup::test_dir("catalog-borrow-quiet");
        let (seen, upstream) = models_upstream(axum::http::StatusCode::OK).await;
        let server = server_with(&dir, &upstream, "");
        let sub = server.anthropic_sub.clone().expect("sub");

        server.borrow_catalog_credential(sub.as_ref(), &bearer("t"));
        server
            .catalog_dir
            .set(dir.join("models-cache"))
            .expect("unset");
        server.borrow_catalog_credential(sub.as_ref(), &HeaderMap::new());

        let keyed_dir = crate::setup::test_dir("catalog-borrow-keyed");
        let keyed = server_with(
            &keyed_dir,
            &upstream,
            &format!(
                r#"
[providers.anthropic_api]
upstream = "{upstream}"
api_key_env = "TOKER_TEST_NO_SUCH_KEY_VAR"
api_key = "ak-literal-test"
"#
            ),
        );
        keyed
            .catalog_dir
            .set(keyed_dir.join("models-cache"))
            .expect("unset");
        let api = keyed.anthropic_api.clone().expect("api");
        keyed.borrow_catalog_credential(api.as_ref(), &bearer("t"));
        let keyed_sub = keyed.anthropic_sub.clone().expect("sub");
        keyed.borrow_catalog_credential(keyed_sub.as_ref(), &bearer("t"));

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "{:?}",
            seen.lock().unwrap()
        );
        assert_eq!(server.catalog_borrowed_at.load(Ordering::SeqCst), 0);
    }

    /// The mark before sending names what toker sent; the response may name
    /// another identity (a host alias, an upstream substitution), and
    /// recency must follow what actually served — as the predecessor's
    /// every-row mark did.
    #[test]
    fn a_recorded_response_marks_the_model_it_named_as_served() {
        use crate::server::record_anthropic::{AnthropicRecordCtx, record_anthropic_measurement};
        let dir = crate::setup::test_dir("served-mark");
        let server = server(&dir);
        let ctx = AnthropicRecordCtx {
            server: server.clone(),
            started: std::time::Instant::now(),
            path: "/v1/messages",
            session_id: None,
            requested_model: Some("claude-opus-5".to_owned()),
            effective_model: Some("claude-opus-5".to_owned()),
            drift: None,
            backend: server.default_anthropic().expect("enabled").clone(),
            betas: None,
            shape: None,
            ping: false,
            downgraded_from: None,
            downgraded_to: None,
            cache_stripped: None,
            system_merged: None,
            forced_from: None,
            forced_to: None,
            model_mappings: None,
            frontend: None,
        };
        let mut observer = crate::observe::AnthropicObserver::new();
        observer.observe_json(
            br#"{"type":"message","model":"claude-sonnet-5","usage":{"input_tokens":3,"output_tokens":1}}"#,
        );
        assert_eq!(server.models.last_served("claude-sonnet-5"), None);
        record_anthropic_measurement(&ctx, observer.finish().as_ref(), None, 200);
        assert!(
            server.models.last_served("claude-sonnet-5").is_some(),
            "the served identity is marked from the response"
        );
        assert_eq!(
            server.models.last_served("claude-opus-5"),
            None,
            "the sent identity is not what the row marks"
        );
    }
}
