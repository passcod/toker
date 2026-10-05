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
//!   `GET /_toker/session` — the control endpoints, gated by a custom
//!   header ([`control`]); `session` is the attribution plugin's query.
//!   Any other `/_toker/` path is a local 404.
//! - Every other path — transparent forwarding to the default anthropic
//!   backend ([`anthropic::unmatched`]), as the predecessor forwarded
//!   everything but its control path.
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
mod record;
mod record_anthropic;

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::{get, post};

use crate::catalog::fetched::{self, FetchedCatalog, FetchedCatalogs};
use crate::config::Config;
use crate::middleware::awake::{self, AwakeState, LockSpawner};
use crate::middleware::lanes;
use crate::middleware::models::ModelStore;
use crate::providers::{AnthropicApi, AnthropicSub, CodexSub, OpenRouter, Provider};
use crate::store::Store;

use record::now_ms;

/// How many ledger rows the startup seed reads (the predecessor's
/// 16 MiB log tail, as a row count): enough to span several days of heavy
/// use, which is far more than the lane table or the served-model map look
/// back over.
const SEED_ROWS: u64 = 20_000;

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
        // The config's own validation, again: a Config built by hand (the
        // tests, any embedder) must not reach routing with a default that
        // names a disabled backend — `default_anthropic` relies on it.
        config.validate()?;
        let http = upstream_client(UPSTREAM_IDLE_TIMEOUT)?;
        let openrouter = config.openrouter.as_ref().map(|openrouter| {
            Arc::new(OpenRouter::new(
                openrouter.upstream.clone(),
                openrouter.api_key(),
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
                api.api_key(),
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
            in_flight: Arc::new(AtomicUsize::new(0)),
            awake,
            started: Instant::now(),
        })
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

    /// The full route table.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/chat/completions", post(proxy::chat_completions))
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
    /// bind for dev and tests.
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
        self.spawn_lane_prune();
        self.spawn_awake_timer();
        self.spawn_catalog_refresh();
        // A restart inside a live session takes the lock straight back.
        self.evaluate_awake();
        axum::serve(listener, self.router()).await?;
        Ok(())
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

    /// The lane-table prune on a 30-second flush cadence
    /// (never per request): a cheap SQL
    /// statement on a timer — the upserts themselves go straight into the
    /// store on every response, so nothing here carries state.
    fn spawn_lane_prune(&self) {
        let store = self.store.clone();
        tokio::spawn(async move {
            let mut timer =
                tokio::time::interval(std::time::Duration::from_millis(lanes::LANE_FLUSH_MS));
            loop {
                timer.tick().await;
                let now = record::now_ms();
                match store.prune_lanes(now, lanes::LANE_MAX, lanes::LANE_MAX_AGE_MS) {
                    Ok(0) => {}
                    Ok(count) => {
                        tracing::debug!("lane prune removed {count} lanes");
                    }
                    Err(error) => {
                        // The prune is never worth a request (invariant 6
                        // spirit): it retries on the next tick.
                        tracing::error!(%error, "lane prune failed");
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
    /// anthropic's presence list, and the codex backend's own models
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
                bearer: None,
            });
        }
        if let Some(anthropic) = self.anthropic_sub.as_ref().or(self.anthropic_api.as_ref()) {
            sources.push(fetched::CatalogSource {
                provider: "anthropic",
                // Deliberately uncredentialed (see the fetched module's
                // docs): the 401 falls back to the hand-verified
                // windows, which cover claude.
                url: anthropic.endpoint("/v1/models"),
                bearer: None,
            });
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
                bearer: codex
                    .auth()
                    .and_then(|auth| auth.access_token().map(str::to_owned)),
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
        let Ok(dir) = fetched::cache_dir() else {
            tracing::debug!("no data home; models catalogues disabled");
            return;
        };
        let previous = self
            .catalogs
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut next = FetchedCatalogs::default();
        for source in self.catalog_sources() {
            match fetched::refresh(&source, &dir, &self.http, record::now_ms()).await {
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

#[cfg(test)]
mod tests {
    use super::Server;
    use crate::config::Config;
    use crate::store::Store;
    use std::sync::Arc;

    /// A server over a scratch config whose providers point at
    /// unreachable localhost ports (nothing contacts them at
    /// construction; the codex version probe is disabled too, and the
    /// background catalog task only spawns in `serve`, which tests
    /// never run). The scratch `auth_path` keeps the codex version at
    /// the built-in floor and the bearer absent — hermetic, never the
    /// real `~/.codex`.
    fn server(dir: &std::path::Path) -> Server {
        std::fs::write(
            dir.join("toker.toml"),
            format!(
                r#"
awake = false
db_path = ":memory:"

[providers.openrouter]
upstream = "http://localhost:9/v1"

[providers.anthropic_sub]
upstream = "http://localhost:10"

[providers.codex_sub]
upstream = "http://localhost:11/backend-api/codex"
auth_path = {auth_path:?}
version_probe = false
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
    /// anthropic's presence list (deliberately uncredentialed), and
    /// the codex backend's own endpoint with the client_version query
    /// and the stored bearer — none in the scratch setup.
    #[test]
    fn catalog_sources_point_at_the_configured_models_endpoints() {
        let dir = crate::setup::test_dir("catalog-sources");
        let sources = server(&dir).catalog_sources();
        assert_eq!(sources.len(), 3);

        assert_eq!(sources[0].provider, "openrouter");
        assert_eq!(sources[0].url.as_str(), "http://localhost:9/v1/models");
        assert_eq!(sources[0].bearer, None, "the openrouter listing is public");

        assert_eq!(sources[1].provider, "anthropic");
        assert_eq!(sources[1].url.as_str(), "http://localhost:10/v1/models");
        assert_eq!(
            sources[1].bearer, None,
            "anthropic is called WITHOUT credentials — the 401 falls back to the hand-verified windows"
        );

        assert_eq!(sources[2].provider, "codex_sub");
        assert_eq!(
            sources[2].url.as_str(),
            format!(
                "http://localhost:11/backend-api/codex/models?client_version={}",
                crate::providers::codex::DEFAULT_CLIENT_VERSION
            ),
            "the codex CLI's own request shape: the version the handshake speaks rides as the query"
        );
        assert_eq!(
            sources[2].bearer, None,
            "no login in the scratch dir → no bearer (the request goes up cleanly and fails into the fallback)"
        );
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
