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
//!
//! Timeouts: none. Axum applies no default request or idle timeout, so
//! streams run as long as both ends keep the connection open — the plan's
//! `requestTimeout = 0`. The upstream client ([`Server::new`]) sets a
//! connect timeout only: no read timeout, on any path, because a read
//! timeout would kill live SSE streams; axum tears the connection down
//! itself when either end hangs up (the body stream's Drop aborts the
//! upstream, [proxy] implements it).
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

use anyhow::bail;
use axum::Router;
use axum::routing::{get, post};

use crate::config::Config;
use crate::middleware::awake::{self, AwakeState, LockSpawner};
use crate::middleware::lanes;
use crate::middleware::models::ModelStore;
use crate::providers::{AnthropicApi, AnthropicSub, CodexSub, OpenRouter, Provider};
use crate::store::Store;

use record::now_ms;

/// How many ledger rows the startup seed reads (ctp's 16 MiB log tail,
/// proxy.mjs:148, as a row count): enough to span several days of heavy
/// use, which is far more than the lane table or the served-model map look
/// back over (ctp's own `RECENT_MAX`).
const SEED_ROWS: u64 = 20_000;

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
    /// The shared upstream HTTP client. Connect timeout only — no read
    /// timeout, so streams live as long as their connections do (see the
    /// module docs).
    pub(crate) http: reqwest::Client,
    /// Phase 1's one openai-chat backend; a trait object because routing
    /// selects by `provider/model` prefix and later phases add providers
    /// to exactly this slot.
    pub(crate) openrouter: Arc<dyn Provider>,
    /// The anthropic subscription backend (the protocol default).
    pub(crate) anthropic_sub: Arc<dyn Provider>,
    /// The anthropic API backend.
    pub(crate) anthropic_api: Arc<dyn Provider>,
    /// The codex subscription backend, as routing sees it (the trait
    /// object: prefix routing and the protocol default resolve by id).
    pub(crate) codex_sub: Arc<dyn Provider>,
    /// The codex subscription backend, concretely — the translation
    /// branch needs [`crate::providers::codex::CodexSub`]'s own methods
    /// (auth-for-turn, the codex header block) that the trait does not
    /// carry. Same allocation as [`Server::codex_sub`].
    pub(crate) codex_turn: Arc<CodexSub>,
    /// In-flight usage-path requests (the anthropic `/v1/messages`
    /// non-ping ones and the openai chat completions — a running request
    /// holds the machine awake regardless of protocol): the sleep lock's
    /// other input besides the lane table (ctp `inFlight`,
    /// proxy.mjs:455).
    pub(crate) in_flight: Arc<AtomicUsize>,
    /// The idle-sleep lock's state (ctp's `sleepLock` + `awakeHeld`), or
    /// `None` when `awake` is off (ctp: `sleepLock = null` when
    /// `CTP_AWAKE=off` — never hold, never spawn).
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
        if config.default_backend_openai_chat != "openrouter" {
            bail!(
                "phase 1 wires only the openrouter backend, \
                 default_backend_openai_chat = {:?} is not available yet",
                config.default_backend_openai_chat
            );
        }
        if !matches!(
            config.default_backend_anthropic.as_str(),
            "anthropic_sub" | "anthropic_api" | "codex_sub"
        ) {
            bail!(
                "no anthropic backend named {:?} is wired",
                config.default_backend_anthropic
            );
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // Deliberately no read/overall timeout: streams must not be
            // killed by the clock (see the module docs).
            .build()?;
        let openrouter = Arc::new(OpenRouter::new(
            config.openrouter.upstream.clone(),
            config.openrouter.api_key(),
        ));
        let anthropic_sub = Arc::new(AnthropicSub::new(
            config.anthropic_sub.upstream.clone(),
            config.anthropic_sub.model_map.clone(),
        ));
        let anthropic_api = Arc::new(AnthropicApi::new(
            config.anthropic_api.upstream.clone(),
            config.anthropic_api.api_key(),
            config.anthropic_api.model_map.clone(),
        ));
        let codex_turn = Arc::new(CodexSub::new(
            config.codex_sub.upstream.clone(),
            config.codex_sub.originator.clone(),
            config.codex_sub.auth_path.clone(),
            config.codex_sub.refresh_url.clone(),
            config.codex_sub.model_map.clone(),
            config.codex_sub.client_version.clone(),
        )?);
        let codex_sub: Arc<dyn Provider> = codex_turn.clone();

        // ctp proxy.mjs:440-454: the lock exists only while the toggle is
        // on, and an unavailable platform says so once, at startup —
        // `CTP_AWAKE has no effect` there, `awake` here.
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

        // The startup seed (ctp `readLogTail` + `loadLanes` +
        // `loadModels`, proxy.mjs:150-269): the newest ledger rows, read
        // once, feed both state stores. Reseeding is idempotent, so every
        // Server::new — serve, tests, restarts — rebuilds the same state.
        let total = store.count_requests()?;
        let seed = store.requests_since(0, SEED_ROWS)?;
        // ctp `servedCoveredSince`: the tail read everything (no cut) →
        // the served map vouches from the beginning (`-Infinity` there);
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
            in_flight: Arc::new(AtomicUsize::new(0)),
            awake,
            started: Instant::now(),
        })
    }

    /// Resolve an anthropic backend by provider name — routing and the
    /// configured protocol default both resolve here (plan: Routing).
    pub(crate) fn anthropic_backend(&self, name: &str) -> Option<&Arc<dyn Provider>> {
        match name {
            "anthropic_sub" => Some(&self.anthropic_sub),
            "anthropic_api" => Some(&self.anthropic_api),
            "codex_sub" => Some(&self.codex_sub),
            _ => None,
        }
    }

    /// The configured default anthropic backend. Validated at startup.
    pub(crate) fn default_anthropic(&self) -> &Arc<dyn Provider> {
        self.anthropic_backend(&self.config.default_backend_anthropic)
            .expect("default_backend_anthropic is validated at startup")
    }

    // ── the idle-sleep lock (ctp evaluateAwake, proxy.mjs:465-482) ──

    /// Take or drop the sleep lock to match the lane table and the
    /// in-flight count, and write an `awake` row on every held/want flip.
    ///
    /// ctp wraps its whole body in a try/catch — "the lock is not worth
    /// a request" — so a panic here is caught and logged, never
    /// propagated to the request it rode in on. A store error just loses
    /// this one evaluation.
    pub(crate) fn evaluate_awake(&self) {
        let Some(awake) = &self.awake else {
            return; // ctp: `if (!sleepLock) return` — the toggle is off.
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

    /// A usage-path request is now in flight (ctp proxy.mjs:1129-1132:
    /// `inFlight++; … evaluateAwake()`): a lane's `at` moves only when a
    /// response finishes, and one long turn can outlast a 5-minute tier,
    /// so a request being served holds the machine awake, pings aside.
    pub(crate) fn begin_in_flight(&self) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.evaluate_awake();
        InFlightGuard {
            server: self.clone(),
        }
    }

    /// The other half of the guard's Drop (ctp: `res.on("close")` fires
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
        // A restart inside a live session takes the lock straight back
        // (ctp proxy.mjs:494).
        self.evaluate_awake();
        axum::serve(listener, self.router()).await?;
        Ok(())
    }

    /// The sleep lock's wall-clock re-evaluation on ctp's 60-second
    /// cadence (ctp proxy.mjs:487-491, `setInterval` + `unref`). Wall
    /// clock rather than a timeout aimed at the expiry: the interval
    /// runs on a monotonic clock that stops while the machine is
    /// suspended, so a release due at 18:30 would otherwise slip by
    /// however long the lid was shut — the decision reads the wall
    /// clock, so the first tick after a resume re-evaluates correctly.
    fn spawn_awake_timer(&self) {
        if self.awake.is_none() {
            return; // ctp: no sleepLock, no timer.
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

    /// The lane-table prune on ctp's flush cadence (proxy.mjs:380-394:
    /// `LANE_FLUSH_MS` 30 s, `unref`'d, never per request): a cheap SQL
    /// statement on a timer — the upserts themselves go straight into the
    /// store on every response, so unlike ctp nothing here carries state.
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
}

/// One in-flight request's hold on the sleep lock: increments on entry,
/// and Drop is the decrement plus the re-evaluation — ctp's
/// `res.on("close")`, which "fires however the exchange ends". A guard,
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
