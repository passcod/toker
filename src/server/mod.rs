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
//! - OpenAI-chat frontend (→ any registered Chat, Responses, or Messages
//!   binding):
//!   - `POST /v1/chat/completions` — the usage path: buffered, parsed to
//!     the IR, routed, recorded ([`proxy`]).
//!   - `GET /v1/models` — a local projection of the active revision's
//!     publication for the profile; nothing is forwarded or recorded.
//! - Anthropic frontend (→ any registered Messages/Codex binding):
//!   - `POST /v1/messages` — the anthropic usage path, fully recorded;
//!     `count_tokens` and `batches` run the same pipeline (they are not
//!     gated — gates arrive with the quota-gate unit) but carry no usage,
//!     so they record nothing in practice ([`anthropic`]).
//!   - The batch-result GETs (and cancel) — transparent forwarding to the
//!     binding the active revision's default route names, no recording.
//! - `GET /_toker/status`, `GET /_toker/session`,
//!   `POST /_toker/shutdown` — the control
//!   endpoints, gated by a custom header ([`control`]); `session` is the
//!   attribution plugin's query, `shutdown` is `toker restart`'s drain.
//!   Any other `/_toker/` path is a local 404.
//! - Every other path — transparent forwarding to the binding the active
//!   revision's default route names ([`anthropic::unmatched`]), as the
//!   predecessor forwarded everything but its control path.
//!
//! Every route above also answers under a `/f/<frontend>` prefix: the
//! router strips it before matching and keeps the profile
//! ([`FrontendProfile`]), which picks the frontend's gate-notice style
//! (`[notices]`). Setup writes the prefix into the frontends it patches;
//! an unprefixed request is an unknown frontend.
//!
//! A backend is enabled by its `[providers.X]` block's presence. A
//! protocol with no enabled backend answers its routes with a
//! not-configured error in that protocol's own error shape
//! ([`anthropic_not_configured`], [`openai_not_configured`]) and reaches
//! no upstream; that includes the unmatched-path fallback, which is the
//! anthropic default's. Policy chooses the binding for every request; the
//! route registry only turns that choice into a target.
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
//! fields (ts, duration) — never for bytes (spec: WCANON#rendering); serialisation
//! decisions never consult runtime state.

pub(crate) mod anthropic;
pub(crate) mod anthropic_target;
pub(crate) mod attempt;
pub(crate) mod cancel;
pub(crate) mod codex;
pub(crate) mod control;
pub(crate) mod inference;
pub(crate) mod openrouter_responses;
pub(crate) mod proxy;
pub(crate) mod quota_events;
pub(crate) mod quota_gate;
mod record;
mod record_anthropic;
pub(crate) mod routing_ops;

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use axum::routing::{any, get, post};

use crate::catalog::fetched::{self, FetchedCatalog, FetchedCatalogs};
use crate::catalog::offers;
use crate::config::Config;
use crate::middleware::awake::{self, AwakeState, LockSpawner};
use crate::middleware::lanes;
use crate::policy::{
    AvailabilitySnapshot, BindingAvailability, CatalogueAvailability, Evidence, LaneFacts,
    ProfileId, RequestClass, RequestFacts,
};
use crate::providers::{AnthropicApi, AnthropicSub, CodexSub, OpenAiApi, OpenRouter, Provider};
use crate::routing::{FrontendProfile, ModelTarget, ProtocolId, RouteRegistry};
use crate::secrets::{self, KEYRING_READ_TIMEOUT, OsKeyring, SecretStore};
use crate::store::{
    ActivatedPolicyRevision, BindingLearning, CredentialDigest, PolicyRevision,
    PublicationCasResult, PublicationKey, PublicationResultUpdate, PublicationState,
    RoutingPolicyState, RoutingPublication, Store,
};

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

#[derive(Debug, thiserror::Error)]
pub(crate) enum PolicyAttemptError {
    #[error(transparent)]
    Runtime(#[from] crate::policy::PolicyRuntimeError),
    #[error("policy revision {revision_id} could not route the request: {failure}")]
    Resolve {
        revision_id: i64,
        failure: crate::policy::ResolveFailure,
    },
    #[error("policy revision {revision_id} could not safely transition the lane: {reason:?}")]
    Transition {
        revision_id: i64,
        reason: crate::policy::TransitionDenied,
        /// What the resolver chose before the pin refused it. A lane pinned
        /// to a revision newer than the one running never got that far.
        route: Option<Box<DeniedRoute>>,
    },
    /// The caller built lane facts for a lane it had not loaded, or the other
    /// way round. That is a handler bug, and routing on half a lane would
    /// guess its cache temperature.
    #[error("the stored lane and the policy lane facts disagreed")]
    LaneFactsMismatch,
}

/// The route a resolved plan chose, kept on a [`PolicyAttemptError`] so the
/// ledger can say which route the lane's pin refused.
#[derive(Debug)]
pub(crate) struct DeniedRoute {
    route_id: crate::policy::RouteId,
    route_fingerprint: crate::policy::RouteFingerprint,
    provenance: crate::policy::RouteProvenance,
    trace: Vec<crate::policy::TraceEvent>,
}

impl DeniedRoute {
    fn of(plan: &crate::policy::AttemptPlan) -> Option<Box<Self>> {
        Some(Box::new(Self {
            route_id: plan.route_id.clone()?,
            route_fingerprint: plan.route_fingerprint.clone()?,
            provenance: plan.provenance.clone(),
            trace: plan.trace.clone(),
        }))
    }
}

impl PolicyAttemptError {
    /// Every candidate was skipped, so the route had nothing to send to.
    pub(crate) fn is_route_unavailable(&self) -> bool {
        matches!(
            self,
            Self::Resolve {
                failure: crate::policy::ResolveFailure::RouteUnavailable { .. },
                ..
            }
        )
    }

    /// The ledger provenance for a request the policy refused to route: no
    /// route matched, every candidate was skipped, or the lane's pin refused
    /// the move. Any other failure is a fault in the policy or the service,
    /// and gets no row.
    pub(crate) fn unroutable_provenance(
        &self,
        facts: &RequestFacts,
    ) -> Option<crate::store::Result<crate::store::PolicyRouteProvenance>> {
        use crate::store::{AttemptGroupId, PolicyRouteProvenance};
        let group = AttemptGroupId::fresh();
        let facts = crate::policy::RecordedFacts::of(facts);
        match self {
            Self::Resolve {
                revision_id,
                failure:
                    crate::policy::ResolveFailure::RouteUnavailable {
                        route_id,
                        route_fingerprint,
                        provenance,
                        trace,
                    },
            } => Some(PolicyRouteProvenance::from_route_unavailable(
                *revision_id,
                route_id.clone(),
                route_fingerprint.clone(),
                provenance,
                trace,
                group,
                facts,
            )),
            Self::Resolve {
                revision_id,
                failure: crate::policy::ResolveFailure::NoRoute,
            } => Some(PolicyRouteProvenance::from_no_route(
                *revision_id,
                group,
                facts,
            )),
            Self::Transition {
                revision_id,
                reason,
                route,
            } => Some(PolicyRouteProvenance::from_transition_denied(
                *revision_id,
                route.as_deref().map(|route| {
                    (
                        route.route_id.clone(),
                        route.route_fingerprint.clone(),
                        &route.provenance,
                        route.trace.as_slice(),
                    )
                }),
                *reason,
                group,
                facts,
            )),
            _ => None,
        }
    }

    /// The revision has no default route for the request's protocol, so no
    /// backend was ever a candidate.
    pub(crate) fn is_no_default_route(&self) -> bool {
        matches!(
            self,
            Self::Resolve {
                failure: crate::policy::ResolveFailure::NoRoute,
                ..
            }
        )
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct PolicyAttempt {
    pub(crate) plan: crate::policy::AttemptPlan,
    pub(crate) pin_transition: crate::policy::LaneTransitionDecision,
    continuation: PolicyContinuation,
}

#[allow(dead_code)]
#[derive(Clone)]
struct PolicyContinuation {
    snapshot: Arc<crate::policy::PolicySnapshot>,
    learning: crate::policy::LearningSnapshot,
    availability: crate::policy::AvailabilitySnapshot,
    circuits: crate::policy::CircuitSnapshot,
    lane_state: crate::policy::LaneState,
    activation_scope: crate::policy::ActivationScope,
    request_native_credentials: std::collections::BTreeSet<crate::policy::BindingId>,
    accounts: BTreeMap<String, i64>,
}

struct ResolvedPolicySnapshot {
    plan: crate::policy::AttemptPlan,
    snapshot: Arc<crate::policy::PolicySnapshot>,
    learning: crate::policy::LearningSnapshot,
    availability: crate::policy::AvailabilitySnapshot,
    circuits: crate::policy::CircuitSnapshot,
}

/// The injectable edges of a [`Server`]: what a test replaces so it touches
/// nothing of the host it runs on.
pub struct Seams {
    /// Takes the idle-sleep lock. The real one runs systemd-inhibit or the
    /// like, so tests never hand it to a server with `awake` on.
    pub spawner: Box<dyn LockSpawner>,
    /// Where stored API keys are read, once, at startup.
    pub secrets: Arc<dyn SecretStore>,
    /// The model catalogues the server starts with. `None` loads the host's
    /// cache, which only the service wants: a test's inventory would then
    /// depend on whichever machine runs it.
    pub catalogs: Option<FetchedCatalogs>,
}

impl Seams {
    /// The service's own: process spawner, OS keyring, cached catalogues.
    pub fn host() -> Seams {
        Seams {
            spawner: Box::new(awake::ProcessSpawner),
            secrets: Arc::new(OsKeyring),
            catalogs: None,
        }
    }

    /// Touches nothing of the host: no sleep lock, an empty in-memory
    /// keyring, no catalogues. Add what a test needs with the `with_*`
    /// methods.
    pub fn isolated() -> Seams {
        Seams {
            spawner: Box::new(awake::RefusingSpawner),
            secrets: Arc::new(secrets::MemoryStore::new()),
            catalogs: Some(FetchedCatalogs::default()),
        }
    }

    pub fn with_spawner(mut self, spawner: Box<dyn LockSpawner>) -> Seams {
        self.spawner = spawner;
        self
    }

    pub fn with_secrets(mut self, secrets: Arc<dyn SecretStore>) -> Seams {
        self.secrets = secrets;
        self
    }

    pub fn with_catalogs(mut self, catalogs: FetchedCatalogs) -> Seams {
        self.catalogs = Some(catalogs);
        self
    }
}

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

/// The live configuration, swapped under a write lock when an edit lands
/// and read as an immutable snapshot by everything else.
///
/// An edit takes effect without a restart because the service is the only
/// writer: it performs the row change and this swap in the same operation
/// (spec: DCONF#editing). Shared across every `Server` clone, so a handler
/// cloned before an edit still reads the configuration after it.
#[derive(Debug)]
pub(crate) struct ConfigCell(std::sync::RwLock<Arc<Config>>);

impl ConfigCell {
    fn new(config: Config) -> ConfigCell {
        ConfigCell(std::sync::RwLock::new(Arc::new(config)))
    }

    fn get(&self) -> Arc<Config> {
        Arc::clone(
            &self
                .0
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    fn replace(&self, config: Config) {
        *self
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(config);
    }
}

/// The running proxy: config, ledger, upstream client, backend providers,
/// and the learned model store, cloned cheaply into every request handler.
#[derive(Clone)]
pub struct Server {
    pub(crate) store: Arc<Store>,
    config: Arc<ConfigCell>,
    /// Binding-attributed observations and recently-sent targets: the
    /// learning evidence policy resolves against.
    pub(crate) binding_learning: Arc<crate::policy::BindingLearningView>,
    /// The fetched models catalogues (one per source: openrouter's
    /// public listing, anthropic's presence list, the codex backend's
    /// own models endpoint — see [`crate::catalog::fetched`]), projected
    /// into frontend catalogues by [`crate::catalog::offers`], and
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
    /// The local multi-account identity source, keyed only by credential
    /// digest. Re-read when it cannot answer, because another program
    /// rewrites it on every token refresh. Token values do not survive it.
    pub(crate) local_accounts: Arc<crate::accounts::LocalAccounts>,
    /// The enabled provider bindings and implemented frontend-to-backend
    /// routes. Policy picks the binding; handlers turn it into a concrete
    /// model target here.
    pub(crate) registry: RouteRegistry,
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
    /// Fired by a `POST /_toker/shutdown` that says `force`: the drain
    /// stops waiting for connections. [`Server::serve_listener`] returns
    /// at once and the process exits through its normal path, which
    /// cancels what was still running (dropping it, so destructors run).
    pub(crate) force: Arc<tokio::sync::Notify>,
    /// The console quota events' per-backend latches (see
    /// [`quota_events`]).
    pub(crate) quota_events: Arc<quota_events::QuotaEvents>,
    /// Serialises catalogue replacement with a control-plane activation, so a
    /// persisted desired inventory generation always matches the snapshot the
    /// new runtime policy was compiled against.
    routing_control: Arc<Mutex<()>>,
    /// The active policy and the live evidence it resolves against, captured
    /// atomically.
    pub(crate) policy_runtime: Arc<crate::policy::PolicyRuntime>,
    /// Bindings that refused the server safeguards ask (spec:
    /// WSAFE#a-rejection-mark), mirrored from the store so the resolver and
    /// the renderer read them without I/O.
    safeguards_marks: Arc<std::sync::RwLock<std::collections::BTreeSet<crate::policy::BindingId>>>,
}

impl Server {
    /// Build the production server: this host's keyring, idle-sleep lock and
    /// cached model catalogues. Tests never come through here; they use
    /// [`Server::with_seams`] with [`Seams::isolated`], so the host's state
    /// cannot leak into a fixture.
    pub fn new(config: Config, store: Arc<Store>) -> anyhow::Result<Server> {
        Self::with_seams(config, store, Seams::host())
    }

    /// Build the server: resolve the provider credentials once, validate
    /// the routing table, build the upstream client, arm the idle-sleep
    /// lock, and take the active routing revision, establishing revision one
    /// from the config's routing fields when the ledger has none. Every
    /// server routes by policy; there is no other mode.
    pub fn with_seams(config: Config, store: Arc<Store>, seams: Seams) -> anyhow::Result<Server> {
        let Seams {
            spawner,
            secrets,
            catalogs: initial_catalogs,
        } = seams;
        // The legacy routing fields are only input to establishing revision
        // one, which validates them itself.
        config.validate_retained()?;
        let http = upstream_client(UPSTREAM_IDLE_TIMEOUT)?;
        let local_accounts = crate::accounts::LocalAccounts::at_home(now_ms());
        let openrouter = config.openrouter.as_ref().map(|openrouter| {
            Arc::new(OpenRouter::new(
                openrouter.upstream.clone(),
                openrouter.api_key(|| {
                    secrets::read_key(secrets.clone(), "openrouter", KEYRING_READ_TIMEOUT)
                }),
            )) as Arc<dyn Provider>
        });
        let openai_api = config.openai_api.as_ref().map(|api| {
            Arc::new(OpenAiApi::new(
                api.upstream.clone(),
                api.api_key(|| {
                    secrets::read_key(secrets.clone(), "openai_api", KEYRING_READ_TIMEOUT)
                }),
            )) as Arc<dyn Provider>
        });
        let anthropic_sub = config.anthropic_sub.as_ref().map(|sub| {
            Arc::new(AnthropicSub::new(
                sub.upstream.clone(),
                sub.oauth_token(|| {
                    secrets::read_key(secrets.clone(), "anthropic_sub", KEYRING_READ_TIMEOUT)
                }),
                sub.claude_credentials_path.clone(),
            )) as Arc<dyn Provider>
        });
        let anthropic_api = config.anthropic_api.as_ref().map(|api| {
            Arc::new(AnthropicApi::new(
                api.upstream.clone(),
                api.api_key(|| {
                    secrets::read_key(secrets.clone(), "anthropic_api", KEYRING_READ_TIMEOUT)
                }),
            )) as Arc<dyn Provider>
        });
        let codex_turn = match &config.codex_sub {
            Some(codex) => Some(Arc::new(CodexSub::new(
                codex.upstream.clone(),
                codex.originator.clone(),
                codex.auth_path.clone(),
                codex.refresh_url.clone(),
                codex.client_version.clone(),
                codex.version_probe,
            )?)),
            None => None,
        };
        let codex_sub = codex_turn.clone().map(|codex| codex as Arc<dyn Provider>);
        let providers = [
            openrouter.clone(),
            openai_api.clone(),
            anthropic_sub.clone(),
            anthropic_api.clone(),
            codex_sub.clone(),
        ]
        .into_iter()
        .flatten();
        let registry = RouteRegistry::new(providers);

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

        // The startup seed: the newest ledger rows, read once, feed both
        // state stores (lanes and binding learning). Reseeding is
        // idempotent, so every Server::new (serve, tests, restarts)
        // rebuilds the same state.
        let seed = store.requests_since(0, SEED_ROWS)?;
        lanes::reseed(&store, &seed)?;
        let binding_learning = Arc::new(crate::policy::BindingLearningView::seeded(
            store.clone(),
            &seed,
        ));
        let (initial_catalogs, initial_catalog_dir) = match initial_catalogs {
            Some(catalogs) => (catalogs, None),
            None => {
                let cache_dir = fetched::cache_dir().ok();
                let catalogs = cache_dir
                    .as_deref()
                    .map(fetched::load_cached)
                    .unwrap_or_default();
                (catalogs, cache_dir)
            }
        };
        let inventory = offers::inventory(&registry, &initial_catalogs);
        let availability = Arc::new(availability_for(&registry, &initial_catalogs));
        let publication_generation = fetched::snapshot_generation(&initial_catalogs);
        // Every start goes through the first-revision transaction, which
        // retires generated revisions before the pointer is read and loads
        // the active revision where there is one.
        let (revision, policy) = crate::policy::legacy::establish_revision_one(
            &store,
            &config,
            &registry,
            &inventory,
            publication_generation,
            now_ms(),
            // The service never imports: it refuses to start while a
            // configuration file exists, so setup is the only importer.
            None,
        )?;
        let policy_runtime = Arc::new(crate::policy::PolicyRuntime::new(
            store.clone(),
            registry.clone(),
            &revision,
            Arc::new(policy),
            Arc::new(inventory),
            availability,
        ));
        // A widening lies beside the revision it widened, so a restart reads
        // it back from the state row.
        let state = store.load_routing_policy_state()?;
        if let Some(widening) = state.widened {
            policy_runtime.widen_active(
                revision.revision_id,
                widening.scope,
                widening.forced_cache_transition,
            );
        }
        let safeguards_marks = store
            .load_safeguards_marks()?
            .into_iter()
            .map(|mark| mark.binding_id)
            .collect();
        let catalog_dir = Arc::new(std::sync::OnceLock::new());
        if let Some(initial_catalog_dir) = initial_catalog_dir {
            let _ = catalog_dir.set(initial_catalog_dir);
        }
        Ok(Server {
            store,
            config: Arc::new(ConfigCell::new(config)),
            http,
            local_accounts: Arc::new(local_accounts),
            registry,
            codex_turn,
            binding_learning,
            catalogs: Arc::new(std::sync::RwLock::new(initial_catalogs)),
            catalog_dir,
            catalog_borrowed_at: Arc::default(),
            in_flight: Arc::new(AtomicUsize::new(0)),
            awake,
            started: Instant::now(),
            instance: Arc::from(uuid::Uuid::new_v4().simple().to_string()),
            started_ms: now_ms(),
            shutdown: Arc::default(),
            force: Arc::default(),
            quota_events: Arc::default(),
            routing_control: Arc::default(),
            policy_runtime,
            safeguards_marks: Arc::new(std::sync::RwLock::new(safeguards_marks)),
        })
    }

    /// The bindings holding a server safeguards rejection mark.
    pub(crate) fn safeguards_marks(&self) -> std::collections::BTreeSet<crate::policy::BindingId> {
        self.safeguards_marks
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Remember that a binding refused the ask. A store failure keeps the
    /// mark for this process and is logged: the worst case is one more refused
    /// send after a restart.
    pub(crate) fn mark_safeguards_rejected(&self, binding_id: &crate::policy::BindingId) {
        if let Err(error) = self.store.set_safeguards_mark(binding_id, now_ms()) {
            tracing::error!(%error, "server safeguards mark could not be stored");
        }
        self.safeguards_marks
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(binding_id.clone());
    }

    /// Clear the marks held against `bindings`; how many were held.
    pub(crate) fn clear_safeguards_marks<'a>(
        &self,
        bindings: impl IntoIterator<Item = &'a crate::policy::BindingId>,
    ) -> usize {
        let bindings: Vec<_> = bindings.into_iter().collect();
        if let Err(error) = self.store.clear_safeguards_marks(&bindings) {
            tracing::error!(%error, "server safeguards marks could not be cleared in the store");
        }
        let mut marks = self
            .safeguards_marks
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        bindings
            .into_iter()
            .filter(|binding| marks.remove(*binding))
            .count()
    }

    /// Clear every mark held against a provider's bindings, as a change of its
    /// credential does.
    pub(crate) fn clear_safeguards_marks_for_provider(&self, provider: &str) -> usize {
        let bindings: Vec<_> = self
            .registry
            .bindings()
            .filter(|binding| binding.provider() == provider)
            .map(|binding| binding.id().clone())
            .collect();
        self.clear_safeguards_marks(bindings.iter())
    }

    /// Mark one selected binding and effective target immediately before an
    /// actual inference transport starts.
    /// Resolve an outbound credential from the startup's local account facts,
    /// then record and backfill the durable digest binding. A failure loses
    /// attribution only: requests are already in flight and must continue.
    pub(crate) fn observe_credential(
        &self,
        provider: &str,
        digest: &CredentialDigest,
    ) -> Option<i64> {
        let now = now_ms();
        let identity = self.local_accounts.get(digest, now);
        match self
            .store
            .observe_credential(provider, digest, now, identity.as_ref())
        {
            Ok(account_number) => account_number,
            Err(error) => {
                tracing::error!(%error, "credential account observation failed");
                None
            }
        }
    }

    pub(crate) fn note_binding_sent(
        &self,
        binding_id: &crate::policy::BindingId,
        effective_model: Option<&str>,
    ) {
        self.binding_learning
            .note_sent(binding_id, effective_model, now_ms());
    }

    /// Merge a completed provider-reported observation. Store errors are
    /// visible in logs and never escape into traffic handling.
    pub(crate) fn note_binding_observed(
        &self,
        binding_id: &crate::policy::BindingId,
        reported_model: Option<&str>,
        at_ms: i64,
        prompt: Option<u64>,
    ) {
        let Some(reported_model) = reported_model else {
            return;
        };
        let tz = jiff::tz::TimeZone::system();
        if let Err(error) =
            self.binding_learning
                .note_observed(binding_id, reported_model, at_ms, prompt, &tz)
        {
            tracing::error!(%error, "binding model observation update failed");
            return;
        }
        // The resolver reads only the published snapshot (no I/O on the
        // request path), so a response that taught something is where it
        // gets republished; otherwise learning and recency would stay frozen
        // at their startup values until a promote or a restart.
        if let Err(error) =
            crate::policy::LearningView::refresh_snapshot(self.binding_learning.as_ref())
        {
            tracing::error!(%error, "binding learning snapshot refresh failed");
        }
    }

    /// Announce what one response's meter snapshot says about its
    /// backend's quota (see [`quota_events`]). Call it before the
    /// snapshot is saved: the first reading after a restart seeds its
    /// latch from the one the store still holds. Never fails and never
    /// panics out (spec: GATE#only-a-gate-stops-a-session): a lost announcement is the worst case.
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

    /// A Messages target standing in for a request that policy has not
    /// routed yet: the context a body that never parses, or a gate that runs
    /// before the route is known, needs. Any binding that serves Messages
    /// will do, since the placeholder carries no routing decision.
    pub(crate) fn pending_messages_target(&self) -> Option<ModelTarget> {
        self.registry.bindings().find_map(|binding| {
            self.registry
                .resolve_binding(ProtocolId::AnthropicMessages, binding.id(), None, "pending")
                .ok()
        })
    }

    /// Capture the active runtime policy and all evidence derived from one
    /// catalogue snapshot. Control handlers use this to compile, persist and
    /// swap a revision without mixing inventory generations.
    /// The configuration as it stands, an immutable snapshot a request can
    /// hold for its whole life without blocking an edit.
    pub fn config(&self) -> Arc<Config> {
        self.config.get()
    }

    /// Adopt an edited configuration. Called by the control surface inside
    /// the operation that wrote the row (spec: DCONF#editing).
    pub(crate) fn replace_config(&self, config: Config) {
        self.config.replace(config);
    }

    pub(crate) fn routing_snapshot(&self) -> anyhow::Result<RoutingControlSnapshot> {
        let _control = self
            .routing_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.routing_snapshot_locked()
    }

    fn routing_snapshot_locked(&self) -> anyhow::Result<RoutingControlSnapshot> {
        let basis = self.control_basis_locked()?;
        let routing_ops::ControlBasis {
            active_revision: revision,
            state,
            inventory,
            availability,
            inventory_generation,
        } = basis;
        let now = now_ms();
        let lane_impacts = self.routing_lane_impacts_locked(&state, now)?;
        let publications = self.store.load_publications()?;
        let active_policy = compile_stored_policy(&revision, &self.registry, &inventory).ok();
        let (pinned, revision_lanes, at_risk) = self.pinned_lanes(now)?;
        let hit_counts = self.store.routing_hit_counts(now)?;
        let learning = crate::policy::LearningView::snapshot(self.binding_learning.as_ref());
        let circuits = self
            .policy_runtime
            .open_circuits(&self.policy_runtime.capture(), now);
        let derived = control::derived::build(&control::derived::Inputs {
            availability: &availability,
            inventory: &inventory,
            learning: &learning,
            circuits: &circuits,
            pinned: &pinned,
            publications: &publications,
            active: active_policy.as_ref(),
            document: Some(revision.document.document()),
            hits: &hit_counts,
            revision_lanes: &revision_lanes,
            revision_at_risk: &at_risk,
        });
        Ok(RoutingControlSnapshot {
            active_revision: revision,
            state,
            inventory_generation,
            inventory,
            lane_impacts,
            publications,
            derived,
            hit_counts,
        })
    }

    fn routing_lane_impacts_locked(
        &self,
        _state: &RoutingPolicyState,
        now: i64,
    ) -> Result<RoutingLaneImpacts, crate::store::Error> {
        let mut impacts = RoutingLaneImpacts::default();
        // A ledger that cannot be read is no evidence of zero warm lanes: the
        // consent gate must refuse to proceed, not read an empty impact.
        let lanes = self.store.load_lanes()?;
        for lane in lanes {
            impacts.total += 1;
            let Some(pin) = lane.pin.as_ref() else {
                // An older, unpinned lane may still retain a cache. It has no
                // policy identity to prove safe movement, so never call it
                // cold for an immediate activation.
                impacts.unpinned += 1;
                impacts.unknown += 1;
                continue;
            };
            // The active binding's cache domain is the only place the TTL is
            // known. A still-live pinned lane with a binding no longer in the
            // current topology is conservatively warm for activation consent:
            // unknown never proves cold.
            match cache_temperature(&lane, self.registry.binding(&pin.binding_id), None, now) {
                crate::policy::CacheTemperature::Warm => impacts.warm += 1,
                crate::policy::CacheTemperature::Cold => impacts.cold += 1,
                crate::policy::CacheTemperature::Unknown => {
                    // No cache-domain TTL is not evidence that the lane is
                    // cold, so an immediate activation must be forced.
                    impacts.unknown += 1;
                }
            }
        }
        Ok(impacts)
    }

    /// Pinned lanes grouped by the route they are pinned to and, separately,
    /// by the revision, each by cache temperature as activation consent reads
    /// it.
    #[allow(clippy::type_complexity)]
    fn pinned_lanes(
        &self,
        now: i64,
    ) -> Result<
        (
            std::collections::BTreeMap<crate::policy::RouteId, RoutingLaneImpacts>,
            std::collections::BTreeMap<i64, RoutingLaneImpacts>,
            std::collections::BTreeMap<i64, Vec<String>>,
        ),
        crate::store::Error,
    > {
        let mut by_route: std::collections::BTreeMap<crate::policy::RouteId, RoutingLaneImpacts> =
            std::collections::BTreeMap::new();
        let mut by_revision: std::collections::BTreeMap<i64, RoutingLaneImpacts> =
            std::collections::BTreeMap::new();
        let mut at_risk: std::collections::BTreeMap<i64, Vec<String>> =
            std::collections::BTreeMap::new();
        for lane in self.store.load_lanes()? {
            let Some(pin) = lane.pin.as_ref() else {
                continue;
            };
            let temperature =
                cache_temperature(&lane, self.registry.binding(&pin.binding_id), None, now);
            // Moving a lane onto another revision costs the cache of one that
            // is warm, or not provably cold. Its name is its session's, or
            // its key where it has none.
            if temperature != crate::policy::CacheTemperature::Cold {
                let names = at_risk.entry(pin.policy_revision_id).or_default();
                if names.len() < AT_RISK_LANES_NAMED {
                    names.push(lane.session_id.clone().unwrap_or_else(|| lane.key.clone()));
                }
            }
            for impacts in [
                by_route.entry(pin.policy_route_id.clone()).or_default(),
                by_revision.entry(pin.policy_revision_id).or_default(),
            ] {
                impacts.total += 1;
                match temperature {
                    crate::policy::CacheTemperature::Warm => impacts.warm += 1,
                    crate::policy::CacheTemperature::Cold => impacts.cold += 1,
                    crate::policy::CacheTemperature::Unknown => impacts.unknown += 1,
                }
            }
        }
        Ok((by_route, by_revision, at_risk))
    }

    pub(crate) fn routing_revisions(&self) -> Result<Vec<PolicyRevision>, crate::store::Error> {
        self.store.load_policy_revisions()
    }

    pub(crate) fn promote_routing_learning(
        &self,
        binding_id: &crate::policy::BindingId,
        model_id: &str,
    ) -> Result<Option<BindingLearning>, crate::store::Error> {
        let learning = self
            .store
            .promote_binding_learning(binding_id, model_id, now_ms())?;
        if learning.is_some() {
            crate::policy::LearningView::refresh_snapshot(self.binding_learning.as_ref())?;
        }
        Ok(learning)
    }

    pub(crate) fn record_routing_publication(
        &self,
        key: &PublicationKey,
        expected_revision_id: i64,
        expected_inventory_generation: i64,
        state: PublicationState,
        detail_code: Option<crate::policy::StableId>,
    ) -> Result<PublicationCasResult, crate::store::Error> {
        self.store.compare_and_swap_publication_result(
            key,
            expected_revision_id,
            expected_inventory_generation,
            &PublicationResultUpdate {
                state,
                applied_identities: None,
                detail_code,
                updated_ms: now_ms(),
            },
        )
    }

    pub(crate) fn active_publication(
        &self,
        profile: Option<&str>,
        protocol: crate::policy::FrontendProtocol,
    ) -> crate::catalog::offers::PolicyPublication {
        let snapshot = self.policy_runtime.capture();
        let profile = profile.and_then(|profile| crate::policy::ProfileId::new(profile).ok());
        crate::catalog::offers::render_policy(
            snapshot.policy(),
            snapshot.inventory(),
            profile.as_ref(),
            protocol,
        )
    }

    pub(crate) fn resolve_policy_first_candidate(
        &self,
        facts: &RequestFacts,
        credentials: &crate::policy::CredentialSnapshot,
    ) -> Result<crate::policy::AttemptPlan, PolicyAttemptError> {
        let runtime = &self.policy_runtime;
        let snapshot = runtime.capture();
        let revision_id = snapshot.revision_id();
        let availability = snapshot
            .availability()
            .at(facts.now_ms, crate::catalog::fetched::CACHE_TTL_MS)
            .with_credentials(credentials)
            .with_safeguards_marks(&self.safeguards_marks());
        let learning = crate::policy::LearningView::snapshot(self.binding_learning.as_ref());
        let circuits = runtime.open_circuits(&snapshot, facts.now_ms);
        crate::policy::resolve_first_candidate(
            snapshot.policy(),
            snapshot.inventory(),
            &learning,
            &availability,
            &circuits,
            facts,
        )
        .map(|plan| plan.with_policy_revision(revision_id))
        .map_err(|failure| PolicyAttemptError::Resolve {
            revision_id,
            failure,
        })
    }

    /// The binding a model-free administrative request is forwarded to, under
    /// the active revision.
    pub(crate) fn resolve_policy_model_free(
        &self,
        facts: &RequestFacts,
        credentials: &crate::policy::CredentialSnapshot,
    ) -> Result<crate::policy::BindingId, PolicyAttemptError> {
        let snapshot = self.policy_runtime.capture();
        let revision_id = snapshot.revision_id();
        let availability = snapshot
            .availability()
            .at(facts.now_ms, crate::catalog::fetched::CACHE_TTL_MS)
            .with_credentials(credentials)
            .with_safeguards_marks(&self.safeguards_marks());
        crate::policy::resolve_model_free(snapshot.policy(), &availability, facts).map_err(
            |failure| PolicyAttemptError::Resolve {
                revision_id,
                failure,
            },
        )
    }

    pub(crate) fn resolve_policy_first_candidates(
        &self,
        facts: &[RequestFacts],
        credentials: &crate::policy::CredentialSnapshot,
    ) -> Result<Vec<crate::policy::AttemptPlan>, PolicyAttemptError> {
        let runtime = &self.policy_runtime;
        let snapshot = runtime.capture();
        let revision_id = snapshot.revision_id();
        let learning = crate::policy::LearningView::snapshot(self.binding_learning.as_ref());
        let mut plans = Vec::with_capacity(facts.len());
        for facts in facts {
            let availability = snapshot
                .availability()
                .at(facts.now_ms, crate::catalog::fetched::CACHE_TTL_MS)
                .with_credentials(credentials)
                .with_safeguards_marks(&self.safeguards_marks());
            let circuits = runtime.open_circuits(&snapshot, facts.now_ms);
            let plan = crate::policy::resolve_first_candidate(
                snapshot.policy(),
                snapshot.inventory(),
                &learning,
                &availability,
                &circuits,
                facts,
            )
            .map(|plan| plan.with_policy_revision(revision_id))
            .map_err(|failure| PolicyAttemptError::Resolve {
                revision_id,
                failure,
            })?;
            plans.push(plan);
        }
        Ok(plans)
    }

    /// Resolve one inference attempt under the active policy revision, or
    /// the revision its lane is pinned to: every request resolves or fails
    /// with a [`PolicyAttemptError`].
    pub(crate) fn resolve_policy_attempt(
        &self,
        facts: &RequestFacts,
        credentials: &crate::policy::CredentialSnapshot,
        stored_lane: Option<&crate::store::Lane>,
        lane: Option<&LaneFacts>,
    ) -> Result<PolicyAttempt, PolicyAttemptError> {
        let runtime = &self.policy_runtime;
        let current = runtime.capture();
        let evidence = current.evidence();
        let current_revision_id = current.revision_id();
        let scope = current.activation_scope();
        if stored_lane.is_some() != lane.is_some() {
            return Err(crate::server::PolicyAttemptError::LaneFactsMismatch);
        }
        let temperature = lane
            .map(|lane| lane.temperature)
            .unwrap_or(crate::policy::CacheTemperature::Unknown);
        let lane_state = crate::policy::LaneState::from_stored(stored_lane, temperature);
        let pinned_revision = stored_lane
            .and_then(|lane| lane.pin.as_ref())
            .map(|pin| pin.policy_revision_id);
        // A lane with no pin predates policy routing and resolves under the
        // active revision like a new lane, whatever the activation scope:
        // its cache cannot be attributed to a binding, so there is nothing a
        // pinned revision could preserve. The one thing kept from it is a
        // proven-cold temperature, which the compaction route needs.
        let lane = lane.filter(|lane| {
            pinned_revision.is_some() || lane.temperature == crate::policy::CacheTemperature::Cold
        });

        let use_active = match (pinned_revision, scope) {
            (None, _) => true,
            (Some(revision), _) if revision > current_revision_id => {
                return Err(PolicyAttemptError::Transition {
                    revision_id: revision,
                    reason: crate::policy::TransitionDenied::StaleRevision,
                    route: None,
                });
            }
            (Some(revision), _) if revision == current_revision_id => true,
            (Some(_), crate::policy::ActivationScope::NewLanes) => false,
            (Some(_), crate::policy::ActivationScope::NewAndColdLanes) => {
                temperature == crate::policy::CacheTemperature::Cold
            }
            (Some(_), crate::policy::ActivationScope::Immediate) => true,
        };
        let snapshot = if use_active {
            current
        } else {
            runtime.capture_revision_with_evidence(
                pinned_revision.expect("a non-active selection is pinned"),
                &evidence,
            )?
        };
        let forced_immediate = use_active
            && scope == crate::policy::ActivationScope::Immediate
            && snapshot.forced_cache_transition();
        let mut resolved = match self.resolve_policy_snapshot(
            runtime,
            &snapshot,
            facts,
            credentials,
            lane,
            forced_immediate,
        ) {
            Ok(resolved) => resolved,
            Err(_)
                if use_active
                    && scope == crate::policy::ActivationScope::Immediate
                    && !forced_immediate
                    && pinned_revision.is_some_and(|revision| revision != current_revision_id) =>
            {
                let revision_id = pinned_revision.expect("the guarded case is pinned");
                let pinned = runtime.capture_revision_with_evidence(revision_id, &evidence)?;
                self.resolve_policy_snapshot(runtime, &pinned, facts, credentials, lane, false)?
            }
            Err(error) => return Err(error),
        };
        Self::attach_plan_account(&mut resolved.plan, &facts.accounts);
        let mut decision = policy_transition(
            &lane_state,
            &resolved.plan,
            scope,
            resolved.snapshot.forced_cache_transition(),
        );

        if use_active
            && scope == crate::policy::ActivationScope::Immediate
            && matches!(
                decision,
                crate::policy::LaneTransitionDecision::Denied(
                    crate::policy::TransitionDenied::WarmCacheLoss
                )
            )
            && let Some(revision_id) = pinned_revision
            && revision_id != current_revision_id
        {
            let pinned = runtime.capture_revision_with_evidence(revision_id, &evidence)?;
            resolved =
                self.resolve_policy_snapshot(runtime, &pinned, facts, credentials, lane, false)?;
            Self::attach_plan_account(&mut resolved.plan, &facts.accounts);
            decision = policy_transition(&lane_state, &resolved.plan, scope, false);
        }
        if let crate::policy::LaneTransitionDecision::Denied(reason) = decision {
            return Err(PolicyAttemptError::Transition {
                revision_id: resolved
                    .plan
                    .policy_revision
                    .expect("a resolved plan carries its revision"),
                reason,
                route: DeniedRoute::of(&resolved.plan),
            });
        }
        let continuation = PolicyContinuation {
            snapshot: resolved.snapshot,
            learning: resolved.learning,
            availability: resolved.availability,
            circuits: resolved.circuits,
            lane_state,
            activation_scope: scope,
            request_native_credentials: credentials
                .iter()
                .filter(|(_, evidence)| matches!(evidence, Evidence::Positive("passing_native")))
                .map(|(binding, _)| binding.clone())
                .collect(),
            accounts: facts.accounts.clone(),
        };
        Ok(PolicyAttempt {
            plan: resolved.plan,
            pin_transition: decision,
            continuation,
        })
    }

    pub(crate) fn open_policy_circuit(
        &self,
        attempt: &PolicyAttempt,
        failure: crate::policy::FailureClass,
        now_ms: i64,
    ) {
        let runtime = &self.policy_runtime;
        if failure == crate::policy::FailureClass::Authentication
            && attempt
                .continuation
                .request_native_credentials
                .contains(&attempt.plan.binding_id)
        {
            return;
        }
        let Some(revision_id) = attempt.plan.policy_revision else {
            return;
        };
        let (Some(route_id), Some(candidate_id)) = (
            attempt.plan.route_id.as_ref(),
            attempt.plan.candidate_id.as_ref(),
        ) else {
            return;
        };
        let Some(duration) = attempt
            .continuation
            .snapshot
            .policy()
            .route(route_id)
            .and_then(|route| {
                route
                    .candidates()
                    .iter()
                    .find(|candidate| candidate.id() == candidate_id)
            })
            .and_then(|candidate| candidate.health_circuit_seconds(failure))
        else {
            return;
        };
        let duration_ms = i64::try_from(duration.saturating_mul(1_000)).unwrap_or(i64::MAX);
        runtime.open_circuit(
            &attempt.continuation.snapshot,
            crate::policy::HealthCircuitKey {
                policy_revision_id: revision_id,
                route_id: route_id.clone(),
                candidate_id: candidate_id.clone(),
                binding_id: attempt.plan.binding_id.clone(),
                model_id: attempt.plan.effective_model.clone(),
                failure,
            },
            now_ms.saturating_add(duration_ms),
        );
    }

    pub(crate) fn clear_policy_target_circuits(&self, attempt: &PolicyAttempt) {
        let (Some(route_id), Some(candidate_id)) = (
            attempt.plan.route_id.as_ref(),
            attempt.plan.candidate_id.as_ref(),
        ) else {
            return;
        };
        self.policy_runtime.clear_target_circuits(
            &attempt.continuation.snapshot,
            route_id,
            candidate_id,
            &attempt.plan.binding_id,
            &attempt.plan.effective_model,
        );
    }

    pub(crate) fn continue_policy_attempt(
        &self,
        failed: &PolicyAttempt,
        failure: crate::policy::FailureClass,
    ) -> Result<Option<PolicyAttempt>, PolicyAttemptError> {
        let revision_id = failed
            .plan
            .policy_revision
            .expect("a resolved plan carries its revision");
        let plan = crate::policy::resolve_after_failure(
            failed.continuation.snapshot.policy(),
            failed.continuation.snapshot.inventory(),
            &failed.continuation.learning,
            &failed.continuation.availability,
            &failed.continuation.circuits,
            &failed.plan,
            failure,
        )
        .map_err(|failure| PolicyAttemptError::Resolve {
            revision_id,
            failure,
        })?;
        let Some(mut plan) = plan else {
            return Ok(None);
        };
        // The request's own credential for the provider it fell back to
        // names the account, or none does.
        Self::attach_plan_account(&mut plan, &failed.continuation.accounts);
        let decision = policy_transition(
            &failed.continuation.lane_state,
            &plan,
            failed.continuation.activation_scope,
            failed.continuation.snapshot.forced_cache_transition(),
        );
        if let crate::policy::LaneTransitionDecision::Denied(reason) = decision {
            return Err(PolicyAttemptError::Transition {
                revision_id,
                reason,
                route: DeniedRoute::of(&plan),
            });
        }
        Ok(Some(PolicyAttempt {
            plan,
            pin_transition: decision,
            continuation: failed.continuation.clone(),
        }))
    }

    fn resolve_policy_snapshot(
        &self,
        runtime: &crate::policy::PolicyRuntime,
        snapshot: &Arc<crate::policy::PolicySnapshot>,
        facts: &RequestFacts,
        credentials: &crate::policy::CredentialSnapshot,
        lane: Option<&LaneFacts>,
        activation_allows_cache_loss: bool,
    ) -> Result<ResolvedPolicySnapshot, PolicyAttemptError> {
        let revision_id = snapshot.revision_id();
        let mut lane = lane.cloned();
        if let Some(pin) = lane.as_mut().and_then(|lane| lane.pinned.as_mut()) {
            pin.policy_fingerprint = Some(snapshot.policy().fingerprint().clone());
        }
        let availability = snapshot
            .availability()
            .at(facts.now_ms, crate::catalog::fetched::CACHE_TTL_MS)
            .with_credentials(credentials)
            .with_safeguards_marks(&self.safeguards_marks());
        let learning = crate::policy::LearningView::snapshot(self.binding_learning.as_ref());
        let circuits = runtime.open_circuits(snapshot, facts.now_ms);
        let plan = crate::policy::resolve::resolve_with_activation_cache_consent(
            snapshot.policy(),
            snapshot.inventory(),
            &learning,
            lane.as_ref(),
            &availability,
            &circuits,
            facts,
            activation_allows_cache_loss,
        )
        .map(|plan| plan.with_policy_revision(revision_id))
        .map_err(|failure| PolicyAttemptError::Resolve {
            revision_id,
            failure,
        })?;
        Ok(ResolvedPolicySnapshot {
            plan,
            snapshot: snapshot.clone(),
            learning,
            availability,
            circuits,
        })
    }

    pub(crate) fn credential_snapshot(
        &self,
        headers: &HeaderMap,
        frontend: ProtocolId,
        now_ms: i64,
    ) -> crate::policy::CredentialSnapshot {
        self.registry
            .bindings()
            .filter_map(|binding| {
                let provider = self.registry.provider(binding.provider())?;
                let state = provider.credential_availability(headers, frontend, now_ms / 1000);
                let evidence = match state {
                    crate::providers::CredentialAvailability::KnownMissingOrExpired => {
                        Evidence::Negative(state.reason())
                    }
                    crate::providers::CredentialAvailability::UnknownUntilAttempt => {
                        Evidence::Unknown
                    }
                    crate::providers::CredentialAvailability::PassingNative
                    | crate::providers::CredentialAvailability::Held
                    | crate::providers::CredentialAvailability::Refreshable => {
                        Evidence::Positive(state.reason())
                    }
                };
                Some((binding.id().clone(), evidence))
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn request_facts(
        &self,
        protocol: ProtocolId,
        profile: Option<&str>,
        requested_model: Option<String>,
        lane_reference: Option<String>,
        prompt_bound: u64,
        message_count: Option<u64>,
        tool_count: u64,
        required_capabilities: crate::policy::CapabilityFlags,
        binding_affinity: Option<crate::policy::BindingId>,
        class: RequestClass,
    ) -> RequestFacts {
        RequestFacts {
            profile: profile
                .and_then(|name| ProfileId::new(name).ok())
                .or_else(|| ProfileId::new("unprefixed").ok()),
            protocol: protocol.into(),
            requested_model,
            lane_reference,
            prompt_bound,
            message_count,
            tool_count,
            required_capabilities,
            asks_server_safeguards: false,
            binding_affinity,
            class,
            compaction_evidence: None,
            accounts: Default::default(),
            now_ms: now_ms(),
        }
    }

    /// The account each provider's credential names for a request carrying
    /// `headers`, read off the credential the request will be sent with
    /// rather than guessed from earlier traffic
    /// (spec: ACCT#routing-does-not-select-an-account).
    ///
    /// Only the providers that have accounts are asked. A credential no
    /// source has named yet has no entry, which every reader takes to mean
    /// there is no account to speak of.
    pub(crate) fn request_accounts(
        &self,
        headers: &HeaderMap,
        frontend: ProtocolId,
    ) -> std::collections::BTreeMap<String, i64> {
        let provider = crate::accounts::ACCOUNTED_PROVIDER;
        let Some(backend) = self.registry.provider(provider) else {
            return Default::default();
        };
        let credential = quota_gate::request_credential(self, backend.as_ref(), headers, frontend);
        credential
            .account_number
            .map(|account| (provider.to_owned(), account))
            .into_iter()
            .collect()
    }

    /// The account a plan serves on is the one the request's credential names
    /// for the plan's provider, or none. Routing does not select an account
    /// (spec: ACCT#routing-does-not-select-an-account), so this is evidence the
    /// request holds rather than a routing input.
    ///
    /// None leaves the plan unpinnable on an account, which costs one re-pin;
    /// a guess would attribute a cache to an account this request may not be
    /// on, and a wrong account reads the lane cold and rebuilds its whole
    /// prefix.
    fn attach_plan_account(
        plan: &mut crate::policy::AttemptPlan,
        accounts: &BTreeMap<String, i64>,
    ) {
        plan.account_number = accounts.get(&plan.provider).copied();
    }

    pub(crate) fn lane_facts(
        &self,
        lane: Option<&crate::store::Lane>,
        binding: Option<&crate::routing::BackendBinding>,
        accounts: &BTreeMap<String, i64>,
        now: i64,
        compaction: bool,
    ) -> Option<LaneFacts> {
        let lane = lane?;
        let compaction_cold = compaction
            && crate::middleware::cold::lane_is_cold(
                Some(lane),
                self.config().gates.cold_min_tokens,
                self.config()
                    .gates
                    .cold_idle_min
                    .map(|minutes| (minutes * 60_000.0) as i64),
                now,
            );
        let temperature = if compaction_cold {
            // Proof from the lane's own record, the same the compaction
            // transform uses. Once the lane's recorded TTL has expired, no
            // binding-local cache remains to preserve; a policy pin is not
            // needed to make that positive cold fact safe.
            crate::policy::CacheTemperature::Cold
        } else if compaction {
            // A lane that is not cold by its own record can disprove
            // eligibility without proving which binding owns a still-live
            // cache.
            crate::policy::CacheTemperature::Unknown
        } else {
            // The account asked about is the one this request names, which is
            // what decides whether the cache the pin names is
            // reachable at all. Only worth a store read when the pin names an
            // account to compare it against: a lane pinned without one, which
            // is every lane on a provider whose accounts Toker cannot tell
            // apart, has nothing this could answer.
            let account = lane
                .pin
                .as_ref()
                .and_then(|pin| pin.account_number)
                .and(binding)
                .and_then(|binding| accounts.get(binding.provider()).copied());
            cache_temperature(lane, binding, account, now)
        };
        Some(LaneFacts {
            temperature,
            pinned: lane.pin.as_ref().map(|pin| crate::policy::ProvisionalPin {
                policy_revision_id: Some(pin.policy_revision_id),
                // Filled once the pin's immutable revision is loaded.
                policy_fingerprint: None,
                route_fingerprint: pin.route_fingerprint.clone(),
                candidate_id: pin.candidate_id.clone(),
                binding_id: pin.binding_id.clone(),
                baseline_model: pin.baseline_model.clone(),
                effective_model: pin.effective_model.clone(),
            }),
            forced_from: lane.forced_from.clone(),
            forced_to: lane.forced_to.clone(),
        })
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
    /// frontend's profile riding along as a [`FrontendProfile`] extension (see
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
            .route("/_toker/shutdown", post(control::shutdown))
            .route("/_toker/config/snapshot", any(control::config::snapshot))
            .route("/_toker/config/provider", any(control::config::provider))
            .route("/_toker/config/settings", any(control::config::settings))
            .route("/_toker/config/notice", any(control::config::notice))
            .route(
                "/_toker/config/credential",
                any(control::config::credential),
            )
            .route(
                "/_toker/safeguards/mark/clear",
                any(control::config::clear_safeguards_mark),
            )
            .route("/_toker/routing/snapshot", any(control::routing::snapshot))
            .route(
                "/_toker/routing/revisions",
                any(control::routing::revisions),
            )
            .route("/_toker/routing/validate", any(control::routing::validate))
            .route("/_toker/routing/preview", any(control::routing::preview))
            .route("/_toker/routing/save", any(control::routing::save))
            .route("/_toker/routing/widen", any(control::routing::widen))
            .route(
                "/_toker/routing/reactivate",
                any(control::routing::reactivate),
            )
            .route("/_toker/routing/promote", any(control::routing::promote))
            .route(
                "/_toker/routing/publication",
                any(control::routing::publication),
            )
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
            None => tokio::net::TcpListener::bind(("127.0.0.1", self.config().port)).await?,
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
    /// a client that hangs up ends its exchange. A shutdown request that
    /// says `force` is the caller's deadline: this returns without
    /// waiting, and what was still streaming is cut when the process
    /// exits.
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
        let serving = axum::serve(listener, self.router())
            .with_graceful_shutdown(async move { shutdown.notified().await });
        tokio::select! {
            result = serving => {
                result?;
                tracing::info!("drained; exiting");
            }
            // The serve future is dropped here, closing the listener.
            // Connections already accepted are tasks of the runtime and
            // go with it when the process exits; nothing is killed
            // before that, so every destructor still runs.
            () = self.force.notified() => {
                tracing::warn!(
                    in_flight = self.in_flight.load(Ordering::SeqCst),
                    "forced shutdown; not waiting for connections"
                );
            }
        }
        self.release_awake_for_exit();
        Ok(())
    }

    /// Ask [`Server::serve_listener`] to drain and return, or with
    /// `force` to return without waiting for the connections under way.
    pub(crate) fn begin_shutdown(&self, force: bool) {
        tracing::info!(
            in_flight = self.in_flight.load(Ordering::SeqCst),
            force,
            "shutdown requested; draining"
        );
        self.shutdown.notify_one();
        if force {
            self.force.notify_one();
        }
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
                // Either prune is never worth a request (spec: STORE#not-recorded-is-not-zero
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
        let mut next = self
            .catalogs
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        next.set(source, catalog);
        self.replace_catalogs(next);
    }

    /// Replace catalogue state and every immutable projection derived from it as
    /// one update. In-flight observations retain the old snapshot.
    fn replace_catalogs(&self, next: FetchedCatalogs) {
        let _control = self
            .routing_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let inventory = offers::inventory(&self.registry, &next);
        let availability = availability_for(&self.registry, &next);
        let publication_generation = fetched::snapshot_generation(&next);
        let mut catalogs = self
            .catalogs
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed_catalogue_bindings =
            changed_catalogue_bindings(&self.registry, &catalogs, &next);
        // A refresh clears the mark even where the listing came back the
        // same: the next ask then probes the upstream again.
        self.clear_safeguards_marks(&refreshed_catalogue_bindings(
            &self.registry,
            &catalogs,
            &next,
        ));
        let runtime = &self.policy_runtime;
        // Model-list readers retain this catalogue guard through their
        // comparison, so they can observe only the old pair or new pair.
        let revision_id = runtime.capture().revision_id();
        let revision = match self.store.load_policy_revision(revision_id) {
            Ok(Some(revision)) => revision,
            Ok(None) => {
                tracing::error!(revision_id, "active policy refresh revision is missing");
                return;
            }
            Err(error) => {
                tracing::error!(%error, revision_id, "active policy refresh load failed");
                return;
            }
        };
        match crate::policy::runtime::compile_stored_revision(&revision, &self.registry, &inventory)
            .into_policy()
        {
            Ok(policy) => {
                let generation = publication_generation;
                if let Err(error) = crate::store::publication_targets().and_then(|targets| {
                    self.store
                        .set_publications_desired(
                            &targets,
                            revision.revision_id,
                            generation,
                            now_ms(),
                        )
                        .map(|_| ())
                }) {
                    tracing::error!(%error, revision_id, "active publication handoff failed");
                    return;
                }
                // The recompiled revision keeps the scope it was widened to.
                let activation = match self.store.load_routing_policy_state() {
                    Ok(state) => state.effective_activation(&revision),
                    Err(error) => {
                        tracing::error!(%error, revision_id, "active policy refresh state load failed");
                        return;
                    }
                };
                runtime.replace_active_as(
                    &revision,
                    activation,
                    Arc::new(policy),
                    Arc::new(inventory),
                    Arc::new(availability),
                    &changed_catalogue_bindings,
                );
                *catalogs = next;
            }
            Err(_) => tracing::error!(
                revision_id = revision.revision_id,
                "active policy refresh compilation failed"
            ),
        }
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
        if let Some(openrouter) = self.registry.provider("openrouter") {
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
        if let Some(openai) = self.registry.provider("openai_api") {
            let mut headers = HeaderMap::new();
            openai.inject_auth(&mut headers);
            for value in headers.values_mut() {
                value.set_sensitive(true);
            }
            sources.push(fetched::CatalogSource {
                provider: "openai_api",
                url: openai.endpoint("/v1/models"),
                fetch: !headers.is_empty(),
                headers,
            });
        }
        if let Some(anthropic) = self
            .registry
            .provider("anthropic_sub")
            .or_else(|| self.registry.provider("anthropic_api"))
        {
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
                Ok(catalog) if !catalog.models.is_empty() => {
                    tracing::debug!(
                        provider = source.provider,
                        "models catalogue refreshed: {} models",
                        catalog.models.len()
                    );
                    next.set(source.provider, catalog);
                }
                Ok(_) => {
                    // An absent cache plus an unavailable listing is unknown,
                    // not an empty provider catalogue. Do not publish a
                    // process-local empty entry the picker cannot reproduce.
                    if let Some(previous) = previous.get(source.provider) {
                        next.set(source.provider, previous.clone());
                    }
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
        self.replace_catalogs(next);
    }

    /// The models-catalogue refresh: all three sources at startup, then
    /// every [`fetched::CACHE_TTL_MS`] — the timers' pattern (the
    /// interval's first tick fires immediately). Nothing here can
    /// delay or fail serving: the task runs beside the listener, each
    /// fetch is bounded by [`fetched::FETCH_TIMEOUT`], and every
    /// failure is the refresh chain's own fallback plus a debug log.
    fn spawn_catalog_refresh(&self) {
        let dir = match self
            .catalog_dir
            .get()
            .cloned()
            .or_else(|| fetched::cache_dir().ok())
        {
            Some(dir) => dir,
            None => {
                tracing::debug!("no data home; models catalogues disabled");
                return;
            }
        };
        if self.catalog_dir.get().is_none() {
            self.replace_catalogs(fetched::load_cached(&dir));
            let _ = self.catalog_dir.set(dir);
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
        let api = self.registry.provider("anthropic_api")?;
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

fn changed_catalogue_bindings(
    registry: &RouteRegistry,
    previous: &FetchedCatalogs,
    next: &FetchedCatalogs,
) -> std::collections::BTreeSet<crate::policy::BindingId> {
    registry
        .bindings()
        .filter(|binding| {
            let Some(source) = FetchedCatalogs::source_of(binding.provider()) else {
                return false;
            };
            effective_catalogue(previous, source) != effective_catalogue(next, source)
        })
        .map(|binding| binding.id().clone())
        .collect()
}

/// The bindings whose catalogue source was fetched again, whether or not its
/// listing changed.
fn refreshed_catalogue_bindings(
    registry: &RouteRegistry,
    previous: &FetchedCatalogs,
    next: &FetchedCatalogs,
) -> Vec<crate::policy::BindingId> {
    registry
        .bindings()
        .filter(|binding| {
            let Some(source) = FetchedCatalogs::source_of(binding.provider()) else {
                return false;
            };
            effective_catalogue(previous, source).map(|catalog| catalog.fetched_at_ms)
                != effective_catalogue(next, source).map(|catalog| catalog.fetched_at_ms)
        })
        .map(|binding| binding.id().clone())
        .collect()
}

fn effective_catalogue<'a>(
    catalogs: &'a FetchedCatalogs,
    source: &str,
) -> Option<&'a FetchedCatalog> {
    catalogs
        .get(source)
        .filter(|catalog| !catalog.models.is_empty())
}

/// A catalogue-consistent input for one routing control operation.
pub(crate) struct RoutingControlSnapshot {
    pub(crate) active_revision: PolicyRevision,
    pub(crate) state: RoutingPolicyState,
    pub(crate) inventory_generation: i64,
    pub(crate) inventory: crate::catalog::offers::OfferInventory,
    pub(crate) lane_impacts: RoutingLaneImpacts,
    pub(crate) publications: Vec<RoutingPublication>,
    /// What the routing page shows beyond the documents; see
    /// `control::derived`.
    pub(crate) derived: serde_json::Value,
    /// The last 24 hours' requests by the record that caught them.
    pub(crate) hit_counts: crate::store::HitCounts,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoutingLaneImpacts {
    pub(crate) total: u64,
    pub(crate) unpinned: u64,
    pub(crate) warm: u64,
    pub(crate) cold: u64,
    pub(crate) unknown: u64,
}

#[derive(Debug)]
pub(crate) struct RoutingActivation {
    pub(crate) activation: ActivatedPolicyRevision,
    pub(crate) lane_impacts: RoutingLaneImpacts,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RoutingControlError {
    #[error(transparent)]
    Store(#[from] crate::store::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
    #[error("routing policy compilation failed")]
    Compilation(Vec<crate::policy::Diagnostic>),
    #[error("routing policy revision {0} does not exist")]
    MissingRevision(i64),
    /// The operation was made against a revision that is no longer the
    /// active one.
    #[error("the active routing revision moved to {live_revision_id}")]
    StaleRevision { live_revision_id: i64 },
    #[error("routing edit is invalid")]
    Edit(#[from] crate::policy::PolicyEditError),
    #[error("the activation scope is already as wide as that")]
    NotAWidening {
        scope: crate::policy::ActivationScope,
        forced_cache_transition: bool,
    },
    #[error("request {0} is not in the ledger as a routed request")]
    RequestNotFound(i64),
    #[error("an immediate activation needs explicit warm-cache consent")]
    ForcedTransitionRequired { impacts: RoutingLaneImpacts },
}

/// How many lanes a revision's entry names as holding a cache that moving
/// them would lose; the rest are counted.
const AT_RISK_LANES_NAMED: usize = 20;

/// An immediate activation can strand a warm cache, so it needs explicit
/// consent whenever any lane is warm or not provably cold.
fn require_cache_consent(
    impacts: &RoutingLaneImpacts,
    scope: crate::policy::ActivationScope,
    forced_cache_transition: bool,
) -> Result<(), RoutingControlError> {
    if scope == crate::policy::ActivationScope::Immediate
        && impacts.warm + impacts.unknown > 0
        && !forced_cache_transition
    {
        return Err(RoutingControlError::ForcedTransitionRequired { impacts: *impacts });
    }
    Ok(())
}

/// A preview's result: where a request would go and how the resolver got
/// there, in stable names a terminal can read without parsing Rust's `Debug`.
///
/// The cost is read from the same availability the resolver was given, so a
/// binding whose cost is not known says so rather than reading as unbilled.
fn preview_result_body(
    result: Result<crate::policy::AttemptPlan, crate::policy::ResolveFailure>,
    availability: &AvailabilitySnapshot,
) -> serde_json::Value {
    let trace = |events: Vec<crate::policy::TraceEvent>| -> Vec<serde_json::Value> {
        events
            .into_iter()
            .map(|event| preview_trace_event(event, availability))
            .collect()
    };
    match result {
        Ok(plan) => {
            let (cost, billed) = match availability
                .get(&plan.binding_id)
                .map(|live| live.cost_class)
            {
                Some(crate::policy::CostClass::Billed) => ("billed", Some(true)),
                Some(crate::policy::CostClass::Subscription) => ("subscription", Some(false)),
                _ => ("unknown", None),
            };
            serde_json::json!({
                "outcome": "selected",
                "policy_revision": plan.policy_revision,
                "route_id": plan.route_id,
                "route_fingerprint": plan.route_fingerprint.map(|value| value.to_string()),
                "provenance": routing_ops::provenance_body(&plan.provenance),
                "candidate_id": plan.candidate_id,
                "binding_id": plan.binding_id,
                "provider": plan.provider,
                "cost": cost,
                "billed": billed,
                "baseline_model": plan.baseline_model,
                "upgraded_model": plan.upgraded_model,
                "aliased_model": plan.aliased_model,
                "effective_model": plan.effective_model,
                "cache_transition": plan.cache_transition.map(|value| format!("{:?}", value)),
                "server_safeguards": {
                    "asked": plan.safeguards.asked,
                    "effective": plan.safeguards.effective.map(crate::routing::CapabilityEvidence::as_str),
                    "marked": plan.safeguards.marked,
                    "skipped_candidate": plan.safeguards.skipped_candidate,
                    "walked_again": plan.safeguards.walked_again,
                },
                "trace": trace(plan.trace),
            })
        }
        Err(crate::policy::ResolveFailure::RouteUnavailable {
            route_id,
            route_fingerprint,
            provenance,
            trace: events,
        }) => serde_json::json!({
            "outcome": "unavailable",
            "route_id": route_id,
            "route_fingerprint": route_fingerprint.to_string(),
            "provenance": routing_ops::provenance_body(&provenance),
            "trace": trace(events),
        }),
        Err(error) => serde_json::json!({
            "outcome": "unresolved",
            "code": preview_failure_code(&error),
        }),
    }
}

/// One step of the resolver's walk. A candidate that was passed over says
/// why by the code the reach uses; one that was not has `skipped` null.
fn preview_trace_event(
    event: crate::policy::TraceEvent,
    availability: &AvailabilitySnapshot,
) -> serde_json::Value {
    match event {
        crate::policy::TraceEvent::Lookup(provenance) => serde_json::json!({
            "kind": "lookup",
            "provenance": routing_ops::provenance_body(&provenance),
        }),
        crate::policy::TraceEvent::Candidate(candidate) => serde_json::json!({
            "kind": "candidate",
            "candidate_id": candidate.candidate_id,
            "crossed_from": candidate.crossed_from,
            "server_safeguards": availability
                .get(&candidate.binding_id)
                .map(|live| live.capabilities.server_safeguards.as_str()),
            "binding_id": candidate.binding_id,
            "provider": availability
                .get(&candidate.binding_id)
                .map(|live| live.provider.as_str()),
            "model": candidate.model,
            "skipped": candidate.result.err().map(|reason| reason.code()),
        }),
        crate::policy::TraceEvent::Upgrade { from, to } => serde_json::json!({
            "kind": "upgrade",
            "from": from,
            "to": to,
        }),
        crate::policy::TraceEvent::Alias { from, to } => serde_json::json!({
            "kind": "alias",
            "from": from,
            "to": to,
        }),
        crate::policy::TraceEvent::SafeguardsWalkedAgain => {
            serde_json::json!({"kind": "safeguards_walked_again"})
        }
    }
}

fn preview_failure_code(error: &crate::policy::ResolveFailure) -> &'static str {
    match error {
        crate::policy::ResolveFailure::NoRoute => "no_route",
        crate::policy::ResolveFailure::MissingRoute => "missing_route",
        crate::policy::ResolveFailure::InvalidArgument => "invalid_argument",
        crate::policy::ResolveFailure::InvalidTemplate => "invalid_template",
        crate::policy::ResolveFailure::LearningUnavailable => "learning_unavailable",
        crate::policy::ResolveFailure::RouteUnavailable { .. } => "route_unavailable",
    }
}

fn compile_control(
    document: &crate::policy::ValidatedPolicyDocument,
    registry: &RouteRegistry,
    inventory: &crate::catalog::offers::OfferInventory,
) -> crate::policy::Compilation {
    crate::policy::compile(document.clone(), registry, inventory)
}

fn compile_control_policy(
    document: &crate::policy::ValidatedPolicyDocument,
    registry: &RouteRegistry,
    inventory: &crate::catalog::offers::OfferInventory,
) -> Result<crate::policy::CompiledPolicy, RoutingControlError> {
    compile_control(document, registry, inventory)
        .into_policy()
        .map_err(RoutingControlError::Compilation)
}

fn compile_stored_policy(
    revision: &crate::store::PolicyRevision,
    registry: &RouteRegistry,
    inventory: &crate::catalog::offers::OfferInventory,
) -> anyhow::Result<crate::policy::CompiledPolicy> {
    let compilation =
        crate::policy::runtime::compile_stored_revision(revision, registry, inventory);
    compilation.into_policy().map_err(|diagnostics| {
        anyhow::anyhow!(
            "active policy revision {} has {} compile diagnostics",
            revision.revision_id,
            diagnostics.len()
        )
    })
}

fn availability_for(registry: &RouteRegistry, catalogs: &FetchedCatalogs) -> AvailabilitySnapshot {
    let bindings = registry
        .bindings()
        .map(|binding| {
            let configured = registry.binding_is_configured(binding);
            let catalogue = FetchedCatalogs::source_of(binding.provider())
                .and_then(|source| catalogs.get(source))
                .filter(|catalog| !catalog.models.is_empty())
                .map_or(CatalogueAvailability::Absent, |catalog| {
                    CatalogueAvailability::Fresh {
                        fetched_at_ms: catalog.fetched_at_ms,
                    }
                });
            let canonical = binding.canonical_backend();
            (
                binding.id().clone(),
                BindingAvailability {
                    provider: binding.provider().to_owned(),
                    configured: if configured {
                        Evidence::Positive("configured")
                    } else {
                        Evidence::Negative("unconfigured")
                    },
                    enabled: if configured {
                        Evidence::Positive("enabled")
                    } else {
                        Evidence::Negative("disabled")
                    },
                    credential: if configured {
                        Evidence::Unknown
                    } else {
                        Evidence::Negative("provider-unconfigured")
                    },
                    catalogue,
                    capabilities: canonical
                        .map_or(crate::routing::Capabilities::UNKNOWN, |canonical| {
                            canonical.capabilities()
                        }),
                    safeguards_marked: false,
                    cost_class: binding.cost_class(),
                },
            )
        })
        .collect();
    AvailabilitySnapshot::new(bindings)
}

fn policy_transition(
    lane: &crate::policy::LaneState,
    plan: &crate::policy::AttemptPlan,
    scope: crate::policy::ActivationScope,
    forced_cache_transition: bool,
) -> crate::policy::LaneTransitionDecision {
    let prospective = plan
        .pin_identity()
        .expect("an inference plan has complete pin identity");
    let same_revision = matches!(
        lane,
        crate::policy::LaneState::Existing {
            pin: Some(current), ..
        } if current.policy_revision_id == prospective.policy_revision_id
    );
    let fell_back = plan.fallback_cache_transition.is_some();
    let cause = match lane {
        _ if plan.compaction && same_revision => crate::policy::TransitionCause::Compaction {
            lane: plan.lane_transition,
            cache: plan
                .cache_transition
                .unwrap_or(crate::policy::CacheTransitionPolicy::PreserveWarm),
        },
        crate::policy::LaneState::Existing {
            pin: Some(current), ..
        } if current.policy_revision_id == prospective.policy_revision_id
            && current.policy_route_id == prospective.policy_route_id
            && current.route_fingerprint == prospective.route_fingerprint
            && current.candidate_id == prospective.candidate_id
            && current.binding_id == prospective.binding_id
            && current.baseline_model == prospective.baseline_model =>
        {
            crate::policy::TransitionCause::Upgrade {
                lane: plan.lane_transition,
            }
        }
        _ if fell_back && same_revision => crate::policy::TransitionCause::Fallback {
            cache: plan
                .fallback_cache_transition
                .expect("a fallback plan retains its crossed-edge consent"),
        },
        // The client asked for something else: another route or candidate, or
        // another model on a candidate that takes the client's own spelling.
        // That choice carries the user's consent to lose the cache it leaves
        // behind. Everything else that differs within a revision (an offer
        // selector's winner moving, say) is the policy's doing and keeps the
        // route's own cache rule.
        crate::policy::LaneState::Existing {
            pin: Some(current), ..
        } if same_revision
            && (current.route_fingerprint != prospective.route_fingerprint
                || current.candidate_id != prospective.candidate_id
                || (plan.baseline_from_request
                    && current.baseline_model != prospective.baseline_model)) =>
        {
            crate::policy::TransitionCause::ExplicitUserChange {
                cache: crate::policy::CacheTransitionPolicy::AllowCacheLoss,
            }
        }
        _ if same_revision => crate::policy::TransitionCause::ExplicitUserChange {
            cache: plan
                .cache_transition
                .unwrap_or(crate::policy::CacheTransitionPolicy::PreserveWarm),
        },
        _ => crate::policy::TransitionCause::Activation {
            scope,
            forced_cache_transition,
        },
    };
    crate::policy::decide_transition(lane, prospective, cause)
}

/// Cache temperature for one lane, as the table in SLANE#cache-temperature
/// gives it. `account` is the account being asked about, and is `None` where
/// the caller asks about no account in particular, such as activation consent
/// over every lane at once.
fn cache_temperature(
    lane: &crate::store::Lane,
    binding: Option<&crate::routing::BackendBinding>,
    account: Option<i64>,
    now_ms: i64,
) -> crate::policy::CacheTemperature {
    use crate::policy::CacheTemperature;

    // The one row of the table answering cold from knowledge rather than from
    // a clock: the cache was written under another account and cannot be read
    // under this one, so unknown here would hand every consumer a warm verdict.
    if let (Some(account), Some(pinned)) = (
        account,
        lane.pin.as_ref().and_then(|pin| pin.account_number),
    ) && pinned != account
    {
        return CacheTemperature::Cold;
    }
    let Some(binding) = binding else {
        return CacheTemperature::Unknown;
    };
    if lane
        .pin
        .as_ref()
        .is_none_or(|pin| pin.binding_id != *binding.id())
    {
        return CacheTemperature::Unknown;
    }
    // Anthropic and codex bindings declare no cache domain, so the TTL their
    // cache runs on is the one the lane itself recorded from the usage the
    // provider reported, read the way the cold gate reads it. A lane that has
    // recorded none stays unknown: absent evidence is never cold.
    let ttl_ms = match binding.cache_domain() {
        Some(domain) => domain.ttl_ms(),
        None if lane.ttl.is_some() => crate::middleware::cold::ttl_of(lane),
        None => return CacheTemperature::Unknown,
    };
    let idle_ms = now_ms.saturating_sub(lane.updated_ms);
    if now_ms < lane.updated_ms {
        CacheTemperature::Unknown
    } else if idle_ms >= ttl_ms {
        CacheTemperature::Cold
    } else {
        CacheTemperature::Warm
    }
}

pub(crate) fn frontend_of(extensions: &axum::http::Extensions) -> Option<&str> {
    extensions
        .get::<FrontendProfile>()
        .map(FrontendProfile::name)
}

/// The resolved frontend profile, for ambiguous frontend-owned surfaces such
/// as `/v1/models`. Inference handlers continue to trust their matched path.
pub(crate) fn frontend_profile_of(extensions: &axum::http::Extensions) -> Option<&FrontendProfile> {
    extensions.get::<FrontendProfile>()
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
/// it came (spec: STORE#not-recorded-is-not-zero — accounting never breaks a session).
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
    request
        .extensions_mut()
        .insert(FrontendProfile::named(name));
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
    mark_not_configured(
        proxy::error_response(
            wire,
            axum::http::StatusCode::NOT_FOUND,
            proxy::WireError {
                anthropic_type: "not_found_error",
                openai_type: "invalid_request_error",
                openai_code: Some("backend_not_configured"),
            },
            &not_configured_message(protocol),
        ),
        protocol,
    )
}

/// Tag an answer as toker's own "this protocol has no backend", for the
/// setup check to tell from an upstream's verdict. The active policy has its
/// own answers for the same condition (no default route for the protocol),
/// each in the status and shape of its path, so they carry the tag too.
pub(crate) fn mark_not_configured(
    mut response: axum::response::Response,
    protocol: &'static str,
) -> axum::response::Response {
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
    use super::{Seams, Server};
    use crate::config::Config;
    use crate::routing::ProtocolId;
    use crate::store::Store;
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use axum::response::IntoResponse;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Arc, Mutex};

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
        Server::with_seams(config, store, Seams::isolated()).expect("server builds")
    }

    #[test]
    fn authoritative_startup_activates_once_and_ignores_later_legacy_routing_changes() {
        let dir = crate::setup::test_dir("authoritative-startup");
        let path = dir.join("toker.toml");
        std::fs::write(
            &path,
            r#"
awake = false
default_backend_openai_chat = "openrouter"

[providers.openrouter]
upstream = "http://127.0.0.1:9/v1"
api_key = "sk-test"
"#,
        )
        .unwrap();
        let mut config = Config::load_from(&path).unwrap();
        config.db_path = dir.join("toker.db");
        let store = Arc::new(Store::open(&config.db_path).unwrap());
        let first = Server::with_seams(config.clone(), store.clone(), Seams::isolated()).unwrap();
        let first_revision = first.policy_runtime.capture().revision_id();
        assert_eq!(
            store
                .load_routing_policy_state()
                .unwrap()
                .active_revision_id,
            Some(first_revision)
        );
        assert_eq!(store.load_policy_revisions().unwrap().len(), 1);
        let publications = store.load_publications().unwrap();
        assert_eq!(publications.len(), 3);
        assert_eq!(
            publications
                .iter()
                .filter(|row| row.state == crate::store::PublicationState::Applied)
                .count(),
            2
        );

        config.gates.force_newest = !config.gates.force_newest;
        let restarted = Server::with_seams(config, store.clone(), Seams::isolated()).unwrap();
        assert_eq!(
            restarted.policy_runtime.capture().revision_id(),
            first_revision
        );
        assert_eq!(store.load_policy_revisions().unwrap().len(), 1);
    }

    #[test]
    fn a_widened_scope_governs_the_live_revision_across_a_refresh_and_a_restart() {
        let dir = crate::setup::test_dir("widened-scope-survives");
        let path = dir.join("toker.toml");
        std::fs::write(
            &path,
            r#"
awake = false
default_backend_openai_chat = "openrouter"

[providers.openrouter]
upstream = "http://127.0.0.1:9/v1"
api_key = "sk-test"
"#,
        )
        .unwrap();
        let mut config = Config::load_from(&path).unwrap();
        config.db_path = dir.join("toker.db");
        let store = Arc::new(Store::open(&config.db_path).unwrap());
        let server = Server::with_seams(config.clone(), store.clone(), Seams::isolated()).unwrap();
        let scope = |server: &Server| {
            let snapshot = server.policy_runtime.capture();
            (
                snapshot.activation_scope(),
                snapshot.forced_cache_transition(),
            )
        };
        assert_eq!(
            scope(&server),
            (crate::policy::ActivationScope::NewLanes, false)
        );

        server
            .widen_routing_activation(1, crate::policy::ActivationScope::NewAndColdLanes, false)
            .expect("widens");
        let widened = (crate::policy::ActivationScope::NewAndColdLanes, false);
        assert_eq!(scope(&server), widened);

        // Recompiling the live revision, as a catalogue refresh does, keeps
        // the scope it was widened to: the widening lies beside the revision.
        server.replace_catalogs(crate::catalog::fetched::FetchedCatalogs::default());
        assert_eq!(scope(&server), widened);

        // So does a restart.
        let restarted = Server::with_seams(config, store.clone(), Seams::isolated()).unwrap();
        assert_eq!(scope(&restarted), widened);

        // Widened again to immediate, with consent, and then saved over: the
        // new revision starts from its own scope, with the widening cleared.
        restarted
            .widen_routing_activation(1, crate::policy::ActivationScope::Immediate, true)
            .expect("widens again");
        assert_eq!(
            scope(&restarted),
            (crate::policy::ActivationScope::Immediate, true)
        );
        let edit = crate::policy::PolicyEdit {
            operations: vec![crate::policy::PolicyEditOperation::SetDescription {
                description: Some("saved".to_owned()),
            }],
        };
        restarted.save_routing_edit(1, &edit, None).expect("saves");
        assert_eq!(
            scope(&restarted),
            (crate::policy::ActivationScope::NewAndColdLanes, false)
        );
        assert_eq!(store.load_routing_policy_state().unwrap().widened, None);
    }

    #[test]
    fn authoritative_startup_rejects_a_corrupt_active_document() {
        let dir = crate::setup::test_dir("authoritative-corrupt-revision");
        let path = dir.join("toker.toml");
        std::fs::write(&path, "[providers.openrouter]\napi_key = 'sk-test'\n").unwrap();
        let mut config = Config::load_from(&path).unwrap();
        config.db_path = dir.join("toker.db");
        let store = Arc::new(Store::open(&config.db_path).unwrap());
        Server::with_seams(config.clone(), store.clone(), Seams::isolated()).unwrap();
        store
            .execute_for_test(
                "PRAGMA ignore_check_constraints = ON;
                 UPDATE routing_policy_revisions SET document_json = '{' WHERE revision_id = 1;",
            )
            .unwrap();
        let error = Server::with_seams(config, store, Seams::isolated())
            .err()
            .expect("startup fails");
        assert!(
            error.to_string().contains("invalid stored routing state")
                || error.to_string().contains("invalid policy document"),
            "{error:#}"
        );
    }

    #[test]
    fn authoritative_startup_rejects_a_missing_active_revision() {
        let dir = crate::setup::test_dir("authoritative-missing-revision");
        let server = server(&dir);
        server
            .store
            .execute_for_test(
                "PRAGMA foreign_keys = OFF;
                 UPDATE routing_policy_state SET active_revision_id = 999 WHERE id = 1;",
            )
            .unwrap();
        let config = (*server.config()).clone();
        let store = server.store.clone();
        drop(server);
        let error = Server::with_seams(config, store, Seams::isolated())
            .err()
            .expect("startup fails");
        // The first-revision transaction loads the pointer's revision, so it
        // is the one that reports the pointer naming nothing.
        assert!(
            format!("{error:#}").contains("active policy revision 999 does not exist"),
            "{error:#}"
        );
    }

    const CHAT_WITHOUT_USAGE: u8 = 1;
    const CHAT_FAILURE: u8 = 2;
    const CHAT_RATE_LIMIT: u8 = 3;
    const CHAT_AUTH_FAILURE: u8 = 4;

    #[derive(Default)]
    struct ChatUpstream {
        seen: Mutex<Vec<serde_json::Value>>,
        response_mode: AtomicU8,
    }

    type ChatBodies = Arc<ChatUpstream>;

    async fn anthropic_chat_upstream(
        axum::extract::State(upstream): axum::extract::State<ChatBodies>,
        request: axum::extract::Request,
    ) -> axum::response::Response {
        let body = match axum::body::to_bytes(request.into_body(), 1 << 20).await {
            Ok(body) => body,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        };
        let value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        };
        upstream.seen.lock().unwrap().push(value);
        (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "id": "msg-policy",
                "type": "message",
                "role": "assistant",
                "model": "backup-model",
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 1}
            })),
        )
            .into_response()
    }

    async fn chat_upstream(
        axum::extract::State(upstream): axum::extract::State<ChatBodies>,
        request: axum::extract::Request,
    ) -> axum::response::Response {
        let body = match axum::body::to_bytes(request.into_body(), 1 << 20).await {
            Ok(body) => body,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        };
        let value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        };
        upstream.seen.lock().unwrap().push(value);
        let response_mode = upstream.response_mode.load(Ordering::Relaxed);
        if response_mode == CHAT_FAILURE {
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "error": {"type": "upstream_error", "message": "test failure"}
                })),
            )
                .into_response();
        }
        if response_mode == CHAT_RATE_LIMIT {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(serde_json::json!({
                    "error": {"type": "rate_limit_error", "message": "test limit"}
                })),
            )
                .into_response();
        }
        if response_mode == CHAT_AUTH_FAILURE {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({
                    "error": {"type": "authentication_error", "message": "test credential"}
                })),
            )
                .into_response();
        }
        let mut response = serde_json::json!({
            "id": "chatcmpl-policy",
            "model": "policy-effective",
            "object": "chat.completion",
            "created": 1,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        });
        if response_mode == CHAT_WITHOUT_USAGE {
            response.as_object_mut().unwrap().remove("usage");
        }
        (StatusCode::OK, axum::Json(response)).into_response()
    }

    async fn serve(server: Server) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("toker binds");
        let address = listener.local_addr().expect("toker address");
        tokio::spawn(async move {
            axum::serve(listener, server.router())
                .await
                .expect("toker serves")
        });
        address
    }

    /// Revision one's document for this config, from a throwaway server: the
    /// route registry is the only thing it takes from one.
    fn revision_one_document(config: &Config) -> crate::policy::PolicyDocument {
        let scratch = Server::with_seams(
            config.clone(),
            Arc::new(Store::open(":memory:").expect("scratch store")),
            Seams::isolated(),
        )
        .expect("scratch server builds");
        crate::policy::legacy::document(&scratch.config(), &scratch.registry)
            .expect("legacy document")
    }

    /// A server whose revision one is this edited document rather than the
    /// one its config generates. Activated before the server is built, so the
    /// constructor loads it instead of establishing its own.
    fn server_under_revision_one(
        config: Config,
        document: crate::policy::PolicyDocument,
    ) -> (Server, Arc<Store>) {
        let document = document.validate().expect("policy validates");
        let store = Arc::new(Store::open(":memory:").expect("scratch store"));
        store
            .activate_revision_one(&document, 2, 0, &[])
            .expect("activate policy fixture");
        let server =
            Server::with_seams(config, store.clone(), Seams::isolated()).expect("server builds");
        (server, store)
    }

    async fn policy_chat_fallback_server() -> (Server, ChatBodies, ChatBodies) {
        policy_chat_fallback_server_with_openrouter_key(true).await
    }

    /// Without a stored OpenRouter key, a credential on the request is the
    /// one that signs; with one, it replaces whatever the request carried.
    async fn policy_chat_fallback_server_with_openrouter_key(
        stored_key: bool,
    ) -> (Server, ChatBodies, ChatBodies) {
        let openrouter_key = if stored_key {
            "api_key = \"sk-or-test\""
        } else {
            ""
        };
        let primary: ChatBodies = Arc::default();
        primary.response_mode.store(CHAT_FAILURE, Ordering::Relaxed);
        let backup: ChatBodies = Arc::default();
        let primary_upstream = axum::Router::new()
            .route("/v1/chat/completions", axum::routing::post(chat_upstream))
            .with_state(primary.clone());
        let primary_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("primary mock binds");
        let primary_address = primary_listener.local_addr().expect("primary mock address");
        tokio::spawn(async move {
            axum::serve(primary_listener, primary_upstream)
                .await
                .expect("primary mock serves")
        });
        let backup_upstream = axum::Router::new()
            .route("/v1/messages", axum::routing::post(anthropic_chat_upstream))
            .with_state(backup.clone());
        let backup_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("backup mock binds");
        let backup_address = backup_listener.local_addr().expect("backup mock address");
        tokio::spawn(async move {
            axum::serve(backup_listener, backup_upstream)
                .await
                .expect("backup mock serves")
        });

        let dir = crate::setup::test_dir("active-chat-fallback-policy");
        std::fs::write(
            dir.join("toker.toml"),
            format!(
                r#"
awake = false
db_path = ":memory:"
default_backend_openai_chat = "openrouter"

[providers.openrouter]
upstream = "http://{primary_address}/v1"
api_key_env = "TOKER_TEST_NO_SUCH_KEY_VAR"
{openrouter_key}

[providers.anthropic_api]
upstream = "http://{backup_address}"
api_key_env = "TOKER_TEST_NO_SUCH_KEY_VAR"
api_key = "sk-ant-test"
"#,
            ),
        )
        .expect("write config");
        let config = Config::load_from(&dir.join("toker.toml")).expect("config loads");
        let mut document = revision_one_document(&config);
        let route_id =
            crate::policy::RouteId::new("route-openrouter-openrouter_chat_completions").unwrap();
        for default in &mut document.defaults {
            if matches!(
                &default.scope,
                crate::policy::FrontendScope::Profile {
                    protocol: crate::policy::FrontendProtocol::OpenaiChat,
                    ..
                } | crate::policy::FrontendScope::Protocol {
                    protocol: crate::policy::FrontendProtocol::OpenaiChat,
                }
            ) {
                default.target.route_id = route_id.clone();
            }
        }
        let route = document
            .routes
            .iter_mut()
            .find(|route| route.id == route_id)
            .expect("OpenRouter route");
        route.chain.first.model = crate::policy::CandidateModel::Exact {
            model_id: "primary-model".to_owned(),
        };
        route.chain.first.health_circuits.extend([
            crate::policy::HealthCircuit {
                failure: crate::policy::FailureClass::ServerError,
                duration_seconds: 30,
            },
            crate::policy::HealthCircuit {
                failure: crate::policy::FailureClass::Authentication,
                duration_seconds: 30,
            },
        ]);
        route.chain.steps.push(crate::policy::RouteStep {
            edge: crate::policy::CandidateEdge {
                preflight: crate::policy::FallbackPermission::Enabled,
                post_send: vec![crate::policy::FailureClass::ServerError],
                billed_boundary: crate::policy::BoundaryConsent::Deny,
                ambiguous_transport: crate::policy::BoundaryConsent::Deny,
                cache_transition: crate::policy::CacheTransitionPolicy::AllowCacheLoss,
            },
            candidate: crate::policy::Candidate {
                id: crate::policy::CandidateId::new("backup").unwrap(),
                binding_id: crate::routing::ANTHROPIC_API_MESSAGES.id().clone(),
                model: crate::policy::CandidateModel::Exact {
                    model_id: "backup-model".to_owned(),
                },
                upgrade_policy_id: None,
                alias_policy: None,
                required_capabilities: Default::default(),
                enabled: true,
                health_circuits: vec![],
            },
        });
        let (server, _store) = server_under_revision_one(config, document);
        (server, primary, backup)
    }

    async fn policy_chat_server() -> (Server, ChatBodies) {
        let seen: ChatBodies = Arc::default();
        let upstream = axum::Router::new()
            .route("/v1/chat/completions", axum::routing::post(chat_upstream))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("mock binds");
        let address = listener.local_addr().expect("mock address");
        tokio::spawn(async move { axum::serve(listener, upstream).await.expect("mock serves") });

        let dir = crate::setup::test_dir("active-chat-policy");
        std::fs::write(
            dir.join("toker.toml"),
            format!(
                r#"
awake = false
db_path = ":memory:"
default_backend_openai_chat = "openrouter"

[providers.openrouter]
upstream = "http://127.0.0.1:9/v1"
api_key_env = "TOKER_TEST_NO_SUCH_KEY_VAR"
api_key = "sk-or-test"

[providers.openai_api]
upstream = "http://{address}/v1"
api_key_env = "TOKER_TEST_NO_SUCH_KEY_VAR"
api_key = "sk-proj-test"
"#,
            ),
        )
        .expect("write config");
        let config = Config::load_from(&dir.join("toker.toml")).expect("config loads");
        let mut document = revision_one_document(&config);
        let openai_route =
            crate::policy::RouteId::new("route-openai_api-openai_chat_completions").unwrap();
        for default in &mut document.defaults {
            if matches!(
                &default.scope,
                crate::policy::FrontendScope::Profile {
                    protocol: crate::policy::FrontendProtocol::OpenaiChat,
                    ..
                } | crate::policy::FrontendScope::Protocol {
                    protocol: crate::policy::FrontendProtocol::OpenaiChat,
                }
            ) {
                default.target.route_id = openai_route.clone();
            }
        }
        document
            .routes
            .iter_mut()
            .find(|route| route.id == openai_route)
            .expect("OpenAI route")
            .chain
            .first
            .model = crate::policy::CandidateModel::Exact {
            model_id: "policy-effective".to_owned(),
        };
        let (server, _store) = server_under_revision_one(config, document);
        (server, seen)
    }

    fn activate_chat_revision(
        server: &Server,
        scope: crate::policy::ActivationScope,
        forced_cache_transition: bool,
        model: &str,
    ) -> i64 {
        let mut document = crate::policy::legacy::document(&server.config(), &server.registry)
            .expect("legacy document");
        let route_id =
            crate::policy::RouteId::new("route-openai_api-openai_chat_completions").unwrap();
        for default in &mut document.defaults {
            if matches!(
                &default.scope,
                crate::policy::FrontendScope::Profile {
                    protocol: crate::policy::FrontendProtocol::OpenaiChat,
                    ..
                } | crate::policy::FrontendScope::Protocol {
                    protocol: crate::policy::FrontendProtocol::OpenaiChat,
                }
            ) {
                default.target.route_id = route_id.clone();
            }
        }
        document
            .routes
            .iter_mut()
            .find(|route| route.id == route_id)
            .expect("OpenAI route")
            .chain
            .first
            .model = crate::policy::CandidateModel::Exact {
            model_id: model.to_owned(),
        };
        let document = document.validate().expect("policy validates");
        let parent_revision_id = server.policy_runtime.capture().revision_id();
        let revision = server
            .store
            .insert_policy_revision(&crate::store::NewPolicyRevision {
                created_ms: 3,
                activated_ms: 4,
                description: Some("replacement Chat test"),
                activation_scope: scope,
                forced_cache_transition,
                source: crate::store::PolicyRevisionSource::Legacy,
                parent_revision_id: Some(parent_revision_id),
                document: &document,
            })
            .expect("insert revision");
        let catalogs = crate::catalog::FetchedCatalogs::default();
        let inventory = crate::catalog::offers::inventory(&server.registry, &catalogs);
        let policy = crate::policy::compile(document, &server.registry, &inventory)
            .into_policy()
            .expect("policy compiles");
        server.policy_runtime.replace_active(
            &revision,
            Arc::new(policy),
            Arc::new(inventory),
            Arc::new(super::availability_for(&server.registry, &catalogs)),
            &std::collections::BTreeSet::new(),
        );
        revision.revision_id
    }

    fn lane_facts(
        lane: &crate::store::Lane,
        temperature: crate::policy::CacheTemperature,
    ) -> crate::policy::LaneFacts {
        crate::policy::LaneFacts {
            temperature,
            pinned: lane.pin.as_ref().map(|pin| crate::policy::ProvisionalPin {
                policy_revision_id: Some(pin.policy_revision_id),
                policy_fingerprint: None,
                route_fingerprint: pin.route_fingerprint.clone(),
                candidate_id: pin.candidate_id.clone(),
                binding_id: pin.binding_id.clone(),
                baseline_model: pin.baseline_model.clone(),
                effective_model: pin.effective_model.clone(),
            }),
            forced_from: lane.forced_from.clone(),
            forced_to: lane.forced_to.clone(),
        }
    }

    #[test]
    fn policy_continuation_uses_the_captured_revision_and_evidence() {
        let (server, _) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(policy_chat_server());
        let runtime = &server.policy_runtime;
        let snapshot = runtime.capture();
        let route_id =
            crate::policy::RouteId::new("route-openai_api-openai_chat_completions").unwrap();
        let mut document = crate::policy::legacy::document(&server.config(), &server.registry)
            .expect("legacy document");
        for default in &mut document.defaults {
            if matches!(
                &default.scope,
                crate::policy::FrontendScope::Profile {
                    protocol: crate::policy::FrontendProtocol::OpenaiChat,
                    ..
                } | crate::policy::FrontendScope::Protocol {
                    protocol: crate::policy::FrontendProtocol::OpenaiChat,
                }
            ) {
                default.target.route_id = route_id.clone();
            }
        }
        let route = document
            .routes
            .iter_mut()
            .find(|route| route.id == route_id)
            .expect("OpenAI route");
        route.chain.first.model = crate::policy::CandidateModel::Exact {
            model_id: "first-model".to_owned(),
        };
        route
            .chain
            .first
            .health_circuits
            .push(crate::policy::HealthCircuit {
                failure: crate::policy::FailureClass::Overload,
                duration_seconds: 10,
            });
        let mut backup = route.chain.first.clone();
        backup.id = crate::policy::CandidateId::new("backup").unwrap();
        backup.model = crate::policy::CandidateModel::Exact {
            model_id: "backup-model".to_owned(),
        };
        route.chain.steps.push(crate::policy::RouteStep {
            edge: crate::policy::CandidateEdge {
                preflight: crate::policy::FallbackPermission::Enabled,
                post_send: vec![crate::policy::FailureClass::Overload],
                billed_boundary: crate::policy::BoundaryConsent::Deny,
                ambiguous_transport: crate::policy::BoundaryConsent::Deny,
                cache_transition: crate::policy::CacheTransitionPolicy::AllowCacheLoss,
            },
            candidate: backup,
        });
        let document = document.validate().expect("policy validates");
        let revision = server
            .store
            .insert_policy_revision(&crate::store::NewPolicyRevision {
                created_ms: 3,
                activated_ms: 4,
                description: Some("fallback continuation test"),
                activation_scope: crate::policy::ActivationScope::NewLanes,
                forced_cache_transition: false,
                source: crate::store::PolicyRevisionSource::Legacy,
                parent_revision_id: Some(snapshot.revision_id()),
                document: &document,
            })
            .expect("insert revision");
        let inventory = snapshot.inventory().clone();
        let policy = crate::policy::compile(document, &server.registry, &inventory)
            .into_policy()
            .expect("policy compiles");
        runtime.replace_active(
            &revision,
            Arc::new(policy),
            inventory,
            snapshot.availability().clone(),
            &std::collections::BTreeSet::new(),
        );
        let facts = server.request_facts(
            ProtocolId::OpenAiChat,
            None,
            Some("asked".to_owned()),
            None,
            1,
            Some(1),
            0,
            Default::default(),
            None,
            crate::policy::RequestClass::Inference,
        );
        let first = server
            .resolve_policy_attempt(
                &facts,
                &crate::policy::CredentialSnapshot::new(),
                None,
                None,
            )
            .expect("initial resolution");
        assert_eq!(first.plan.effective_model, "first-model");

        activate_chat_revision(
            &server,
            crate::policy::ActivationScope::NewLanes,
            false,
            "new-active-model",
        );
        let next = server
            .continue_policy_attempt(&first, crate::policy::FailureClass::Overload)
            .expect("continuation resolves")
            .expect("fallback is allowed");

        assert_eq!(next.plan.policy_revision, Some(revision.revision_id));
        assert_eq!(next.plan.candidate_id.as_ref().unwrap().as_str(), "backup");
        assert_eq!(next.plan.effective_model, "backup-model");
    }

    fn unpinned_lane() -> crate::store::Lane {
        crate::store::Lane {
            key: "lane".to_owned(),
            session_id: Some("session".to_owned()),
            tools_hash: Some("tools".to_owned()),
            updated_ms: 0,
            prompt_tokens: None,
            ttl: None,
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
            pin_generation: 3,
            pin: None,
        }
    }

    fn chat_facts(server: &Server) -> crate::policy::RequestFacts {
        server.request_facts(
            ProtocolId::OpenAiChat,
            None,
            Some("policy-alias".to_owned()),
            Some("lane".to_owned()),
            1,
            Some(1),
            0,
            Default::default(),
            None,
            crate::policy::RequestClass::Inference,
        )
    }

    #[test]
    fn an_existing_unpinned_lane_resolves_under_the_active_revision_like_a_new_lane() {
        let (server, _) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(policy_chat_server());
        // The server's revision activates for new lanes only. A lane with no
        // pin has no revision to be preserved on: it takes the active
        // revision, and is pinned at the lane's own generation.
        let facts = chat_facts(&server);
        let stored_lane = unpinned_lane();
        for temperature in [
            crate::policy::CacheTemperature::Warm,
            crate::policy::CacheTemperature::Unknown,
            crate::policy::CacheTemperature::Cold,
        ] {
            let lane = crate::policy::LaneFacts {
                temperature,
                pinned: None,
                forced_from: None,
                forced_to: None,
            };
            let resolved = server
                .resolve_policy_attempt(
                    &facts,
                    &crate::policy::CredentialSnapshot::new(),
                    Some(&stored_lane),
                    Some(&lane),
                )
                .expect("an unpinned lane resolves");
            assert_eq!(resolved.plan.policy_revision, Some(1), "{temperature:?}");
            assert_eq!(resolved.plan.effective_model, "policy-effective");
            assert!(
                matches!(
                    resolved.pin_transition,
                    crate::policy::LaneTransitionDecision::FirstPin {
                        expected_generation: 3,
                        source: crate::policy::TransitionSource::NewLane,
                        ..
                    }
                ),
                "{temperature:?}: {:?}",
                resolved.pin_transition
            );
        }
    }

    #[test]
    fn lane_facts_that_disagree_with_the_stored_lane_are_an_error_not_a_legacy_fallback() {
        let (server, _) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(policy_chat_server());
        let facts = chat_facts(&server);
        let stored_lane = unpinned_lane();
        let lane = crate::policy::LaneFacts {
            temperature: crate::policy::CacheTemperature::Unknown,
            pinned: None,
            forced_from: None,
            forced_to: None,
        };
        let credentials = crate::policy::CredentialSnapshot::new();
        assert!(matches!(
            server.resolve_policy_attempt(&facts, &credentials, Some(&stored_lane), None),
            Err(crate::server::PolicyAttemptError::LaneFactsMismatch)
        ));
        assert!(matches!(
            server.resolve_policy_attempt(&facts, &credentials, None, Some(&lane)),
            Err(crate::server::PolicyAttemptError::LaneFactsMismatch)
        ));
    }

    #[test]
    fn activation_scope_arbitrates_between_pinned_and_active_revisions() {
        let (server, _) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(policy_chat_server());
        let facts = server.request_facts(
            ProtocolId::OpenAiChat,
            None,
            Some("policy-alias".to_owned()),
            Some("lane".to_owned()),
            1,
            Some(1),
            0,
            Default::default(),
            None,
            crate::policy::RequestClass::Inference,
        );
        let first = server
            .resolve_policy_attempt(
                &facts,
                &crate::policy::CredentialSnapshot::new(),
                None,
                None,
            )
            .expect("first resolution");
        let mut lane = crate::store::Lane {
            key: "lane".to_owned(),
            session_id: Some("session".to_owned()),
            tools_hash: Some("tools".to_owned()),
            updated_ms: 0,
            prompt_tokens: None,
            ttl: None,
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
            pin_generation: 1,
            pin: first.plan.pin_identity().map(crate::store::LanePin::from),
        };

        let active_revision = activate_chat_revision(
            &server,
            crate::policy::ActivationScope::NewAndColdLanes,
            false,
            "policy-next",
        );
        for temperature in [
            crate::policy::CacheTemperature::Warm,
            crate::policy::CacheTemperature::Unknown,
        ] {
            let resolved = server
                .resolve_policy_attempt(
                    &facts,
                    &crate::policy::CredentialSnapshot::new(),
                    Some(&lane),
                    Some(&lane_facts(&lane, temperature)),
                )
                .expect("pinned resolution");
            assert_eq!(resolved.plan.policy_revision, Some(1));
            assert!(matches!(
                resolved.pin_transition,
                crate::policy::LaneTransitionDecision::Keep { .. }
            ));
        }
        let resolved = server
            .resolve_policy_attempt(
                &facts,
                &crate::policy::CredentialSnapshot::new(),
                Some(&lane),
                Some(&lane_facts(&lane, crate::policy::CacheTemperature::Cold)),
            )
            .expect("cold activation resolution");
        assert_eq!(resolved.plan.policy_revision, Some(active_revision));
        assert_eq!(resolved.plan.effective_model, "policy-next");
        assert!(matches!(
            resolved.pin_transition,
            crate::policy::LaneTransitionDecision::Replace {
                expected_generation: 1,
                source: crate::policy::TransitionSource::Activation,
                ..
            }
        ));

        lane.pin = resolved
            .plan
            .pin_identity()
            .map(crate::store::LanePin::from);
        lane.pin_generation = 2;
        let _unforced_revision = activate_chat_revision(
            &server,
            crate::policy::ActivationScope::Immediate,
            false,
            "policy-immediate",
        );
        let preserved = server
            .resolve_policy_attempt(
                &facts,
                &crate::policy::CredentialSnapshot::new(),
                Some(&lane),
                Some(&lane_facts(&lane, crate::policy::CacheTemperature::Warm)),
            )
            .expect("warm immediate resolution");
        assert_eq!(preserved.plan.policy_revision, Some(active_revision));
        assert!(matches!(
            preserved.pin_transition,
            crate::policy::LaneTransitionDecision::Keep { .. }
        ));

        let newest_revision = activate_chat_revision(
            &server,
            crate::policy::ActivationScope::Immediate,
            true,
            "policy-forced",
        );
        let moved = server
            .resolve_policy_attempt(
                &facts,
                &crate::policy::CredentialSnapshot::new(),
                Some(&lane),
                Some(&lane_facts(&lane, crate::policy::CacheTemperature::Unknown)),
            )
            .expect("forced immediate resolution");
        assert_eq!(moved.plan.policy_revision, Some(newest_revision));
        assert_eq!(moved.plan.effective_model, "policy-forced");
        assert!(matches!(
            moved.pin_transition,
            crate::policy::LaneTransitionDecision::Replace {
                expected_generation: 2,
                source: crate::policy::TransitionSource::Activation,
                ..
            }
        ));
    }

    #[test]
    fn a_refused_pin_move_is_recorded_with_the_route_it_refused_and_the_lineage() {
        let (server, _) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(policy_chat_server());
        let facts = server.request_facts(
            ProtocolId::OpenAiChat,
            None,
            Some("policy-alias".to_owned()),
            None,
            1,
            Some(1),
            0,
            Default::default(),
            None,
            crate::policy::RequestClass::Inference,
        );
        let resolved = server
            .resolve_policy_attempt(
                &facts,
                &crate::policy::CredentialSnapshot::new(),
                None,
                None,
            )
            .expect("resolution");
        let lineage = crate::store::SessionLineage::codex_guardian("guardian:parent");
        for (route, expect_route) in [
            (super::DeniedRoute::of(&resolved.plan), true),
            (None, false),
        ] {
            let error = super::PolicyAttemptError::Transition {
                revision_id: 1,
                reason: crate::policy::TransitionDenied::WarmCacheLoss,
                route,
            };
            super::record::record_unroutable(
                &server,
                std::time::Instant::now(),
                None,
                "openai_chat",
                Some("guardian:parent"),
                Some("policy-alias"),
                lineage.clone(),
                &error,
                &facts,
            );
            let rows = server.store.requests_since(0, 10).unwrap();
            let row = rows.last().expect("a row per refusal");
            assert_eq!(row.kind, Some(crate::store::RowKind::RouteUnavailable));
            assert_eq!(row.lineage, lineage);
            let routing = row.policy_routing.as_ref().expect("provenance");
            assert_eq!(
                routing.facts,
                Some(crate::policy::RecordedFacts::of(&facts)),
                "a refusal records the facts it was resolved from"
            );
            assert_eq!(routing.route.is_some(), expect_route);
            assert!(matches!(
                routing.trace.events.last(),
                Some(crate::store::StoredRoutingTraceEvent::TransitionDenied {
                    reason: crate::store::StoredTransitionDenied::WarmCacheLoss
                })
            ));
        }
        // Faults in the policy or the service are not refusals and write nothing.
        let before = server.store.requests_since(0, 10).unwrap().len();
        super::record::record_unroutable(
            &server,
            std::time::Instant::now(),
            None,
            "openai_chat",
            None,
            None,
            None,
            &super::PolicyAttemptError::Resolve {
                revision_id: 1,
                failure: crate::policy::ResolveFailure::InvalidTemplate,
            },
            &facts,
        );
        assert_eq!(server.store.requests_since(0, 10).unwrap().len(), before);
    }

    #[test]
    fn an_old_first_pin_completion_cannot_cross_the_activation_fence() {
        let (server, _) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(policy_chat_server());
        let facts = server.request_facts(
            ProtocolId::OpenAiChat,
            None,
            Some("policy-alias".to_owned()),
            Some("fenced|tools".to_owned()),
            1,
            Some(1),
            0,
            Default::default(),
            None,
            crate::policy::RequestClass::Inference,
        );
        let old = server
            .resolve_policy_attempt(
                &facts,
                &crate::policy::CredentialSnapshot::new(),
                None,
                None,
            )
            .expect("old resolution");
        activate_chat_revision(
            &server,
            crate::policy::ActivationScope::NewLanes,
            false,
            "policy-next",
        );

        super::record::persist_lane_pin(
            &server,
            Some("fenced"),
            Some("tools"),
            false,
            Some(&old.pin_transition),
            super::record::now_ms(),
        );
        assert_eq!(server.store.load_lane("fenced|tools").expect("lane"), None);
    }

    #[tokio::test]
    async fn immediate_activation_preserves_or_replaces_the_live_chat_lane() {
        let (server, upstream) = policy_chat_server().await;
        let store = server.store.clone();
        let address = serve(server.clone()).await;
        let send = |session: &'static str| async move {
            let response = reqwest::Client::new()
                .post(format!("http://{address}/v1/chat/completions"))
                .header("x-toker-session", session)
                .json(&serde_json::json!({
                    "model": "policy-alias",
                    "messages": [{"role": "user", "content": "hi"}],
                    "stream": false
                }))
                .send()
                .await
                .expect("request succeeds");
            assert_eq!(response.status(), StatusCode::OK);
        };

        send("activation-session").await;
        let initial = store
            .load_lanes()
            .expect("lanes")
            .into_iter()
            .find(|lane| lane.session_id.as_deref() == Some("activation-session"))
            .expect("first request pinned the lane");
        assert_eq!(initial.pin_generation, 1);

        let unforced = activate_chat_revision(
            &server,
            crate::policy::ActivationScope::Immediate,
            false,
            "policy-immediate",
        );
        send("activation-session").await;
        let preserved = store.load_lane(&initial.key).expect("lane").expect("lane");
        assert_eq!(preserved.pin_generation, 1);
        assert_eq!(
            preserved.pin.as_ref().map(|pin| pin.policy_revision_id),
            Some(1)
        );
        assert_eq!(
            upstream.seen.lock().unwrap().last().unwrap()["model"],
            "policy-effective",
            "unforced immediate activation preserves an unknown-temperature cache"
        );

        let forced = activate_chat_revision(
            &server,
            crate::policy::ActivationScope::Immediate,
            true,
            "policy-forced",
        );
        send("activation-session").await;
        let moved = store.load_lane(&initial.key).expect("lane").expect("lane");
        assert_eq!(moved.pin_generation, 2);
        assert_eq!(
            moved.pin.as_ref().map(|pin| pin.policy_revision_id),
            Some(forced)
        );
        assert_eq!(
            upstream.seen.lock().unwrap().last().unwrap()["model"],
            "policy-forced"
        );
        assert!(forced > unforced);
    }

    #[tokio::test]
    async fn a_warm_lane_with_no_pin_is_routed_and_pinned_by_the_active_revision() {
        // The revision activates for new lanes only, and the lane has just
        // been used, but a lane with no pin has no revision to stay on.
        let (server, upstream) = policy_chat_server().await;
        let store = server.store.clone();
        let address = serve(server).await;
        let send = || async {
            let response = reqwest::Client::new()
                .post(format!("http://{address}/v1/chat/completions"))
                .header("x-toker-session", "unpinned-session")
                .json(&serde_json::json!({
                    "model": "policy-alias",
                    "messages": [{"role": "user", "content": "hi"}],
                    "stream": false
                }))
                .send()
                .await
                .expect("request succeeds");
            assert_eq!(response.status(), StatusCode::OK);
        };

        send().await;
        let pinned = store
            .load_lanes()
            .expect("lanes")
            .into_iter()
            .find(|lane| lane.session_id.as_deref() == Some("unpinned-session"))
            .expect("first request pinned the lane");
        // Strip the pin, leaving the lane as an older version recorded it.
        store
            .compare_and_swap_lane_pin(
                &pinned.key,
                pinned.pin_generation,
                pinned.pin.as_ref(),
                None,
            )
            .expect("unpin");
        assert_eq!(store.load_lane(&pinned.key).unwrap().unwrap().pin, None);

        send().await;
        let repinned = store.load_lane(&pinned.key).expect("lane").expect("lane");
        assert_eq!(
            repinned.pin.as_ref().map(|pin| pin.policy_revision_id),
            Some(1),
            "the unpinned lane took the active revision"
        );
        assert_eq!(repinned.pin_generation, pinned.pin_generation + 2);
        assert_eq!(
            upstream.seen.lock().unwrap().last().unwrap()["model"],
            "policy-effective",
            "and was routed by policy, not by the legacy default"
        );
    }

    #[tokio::test]
    async fn successful_policy_chat_without_usage_still_pins_its_lane() {
        let (server, upstream) = policy_chat_server().await;
        upstream
            .response_mode
            .store(CHAT_WITHOUT_USAGE, Ordering::Relaxed);
        let store = server.store.clone();
        let address = serve(server).await;

        let response = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .header("x-toker-session", "policy-no-usage")
            .json(&serde_json::json!({
                "model": "policy-alias",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 16,
                "stream": false
            }))
            .send()
            .await
            .expect("request succeeds");
        assert_eq!(response.status(), StatusCode::OK);
        let lane = store
            .load_lanes()
            .expect("lanes")
            .into_iter()
            .find(|lane| lane.session_id.as_deref() == Some("policy-no-usage"))
            .expect("successful policy request created a lane");
        assert_eq!(lane.pin_generation, 1);
        assert_eq!(
            lane.pin.as_ref().map(|pin| pin.binding_id.as_str()),
            Some("openai_api.openai_chat_completions")
        );
        assert!(
            store.requests_since(0, 10).expect("ledger rows").is_empty(),
            "usage-less success preserves the existing no-measurement behavior"
        );
    }

    #[tokio::test]
    async fn provider_state_on_an_unpinned_lane_resolves_and_pins_it() {
        let (server, _upstream) = policy_chat_server().await;
        let store = server.store.clone();
        let address = serve(server).await;

        // A lane with no pin has no recorded producer for this state, and a
        // refusal could never recover: the client replays the same history on
        // every retry, and only a served response pins the lane.
        let response = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .header("x-toker-session", "policy-unpinned-state")
            .json(&serde_json::json!({
                "model": "policy-alias",
                "messages": [
                    {"role": "user", "content": "hi"},
                    {
                        "role": "assistant",
                        "content": "hello",
                        "reasoning_details": [{"type": "reasoning.encrypted", "data": "x"}]
                    },
                    {"role": "user", "content": "again"}
                ],
                "max_tokens": 16,
                "stream": false
            }))
            .send()
            .await
            .expect("request succeeds");
        assert_eq!(response.status(), StatusCode::OK);
        let lane = store
            .load_lanes()
            .expect("lanes")
            .into_iter()
            .find(|lane| lane.session_id.as_deref() == Some("policy-unpinned-state"))
            .expect("the served request created a lane");
        assert!(lane.pin.is_some(), "the response pins the lane");
    }

    #[tokio::test]
    async fn failed_policy_chat_does_not_pin_its_lane() {
        let (server, upstream) = policy_chat_server().await;
        upstream
            .response_mode
            .store(CHAT_FAILURE, Ordering::Relaxed);
        let store = server.store.clone();
        let address = serve(server).await;

        let response = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .header("x-toker-session", "policy-failure")
            .json(&serde_json::json!({
                "model": "policy-alias",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 16,
                "stream": false
            }))
            .send()
            .await
            .expect("request completes");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(
            store
                .load_lanes()
                .expect("lanes")
                .into_iter()
                .all(|lane| lane.session_id.as_deref() != Some("policy-failure")),
            "a terminal provider failure cannot select the lane pin"
        );
        let rows = store.requests_since(0, 10).expect("ledger rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, Some(crate::store::RowKind::Error));
    }

    #[test]
    fn translated_chat_successes_persist_the_pending_lane_pin() {
        use crate::policy::{
            BindingId, CandidateId, LaneState, PinIdentity, RouteFingerprint, RouteId,
            TransitionCause, decide_transition,
        };
        use crate::server::record::RecordCtx;
        use crate::server::record_anthropic::{
            record_anthropic_measurement, record_responses_measurement,
        };

        let dir = crate::setup::test_dir("translated-chat-pin");
        let server = server(&dir);
        // Pins name the active revision: one the server did not activate
        // would not be the one its lanes are written under.
        let revision_id = server.policy_runtime.capture().revision_id();
        let make_record = |session: &str, binding: &str, model: &str| {
            let binding_id = BindingId::new(binding).expect("binding");
            let target = server
                .registry
                .resolve_binding(ProtocolId::OpenAiChat, &binding_id, Some(model), model)
                .expect("configured translated Chat target");
            let pin = PinIdentity {
                policy_revision_id: revision_id,
                policy_route_id: RouteId::new(format!("route-{session}")).expect("route"),
                route_fingerprint: "11"
                    .repeat(32)
                    .parse::<RouteFingerprint>()
                    .expect("fingerprint"),
                candidate_id: CandidateId::new(format!("candidate-{session}")).expect("candidate"),
                binding_id: binding_id.clone(),
                account_number: None,
                baseline_model: model.to_owned(),
                effective_model: model.to_owned(),
            };
            RecordCtx {
                server: server.clone(),
                backend: target.provider().clone(),
                binding_id,
                attempt: None,
                policy_routing: None,
                pin_transition: Some(decide_transition(
                    &LaneState::Absent,
                    pin,
                    TransitionCause::Activation {
                        scope: crate::policy::ActivationScope::NewLanes,
                        forced_cache_transition: false,
                    },
                )),
                started: std::time::Instant::now(),
                session_id: Some(session.to_owned()),
                ping: false,
                requested_model: Some(model.to_owned()),
                effective_model: Some(model.to_owned()),
                shape: Some(crate::ir::openai_chat::Shape {
                    req_bytes: 1,
                    req_messages: Some(1),
                    req_tools: 0,
                    tools_hash: format!("tools-{session}"),
                    system_chars: 0,
                    system_hash: String::new(),
                    system_blocks: vec![],
                    summarising: false,
                }),
                system_messages: None,
                frontend: None,
                policy_attempt: None,
            }
        };

        let anthropic = make_record(
            "anthropic-chat",
            "anthropic_sub.anthropic_messages",
            "claude-sonnet-5",
        );
        let anthropic_backend = anthropic.backend.clone();
        let anthropic = super::proxy::chat_cross_record(anthropic, anthropic_backend);
        record_anthropic_measurement(&anthropic, None, Some("claude-sonnet-5"), None, 200);

        let codex = make_record("codex-chat", "codex_sub.codex_responses", "gpt-5.6-sol");
        let codex_backend = codex.backend.clone();
        let codex = super::proxy::chat_cross_record(codex, codex_backend);
        let mut parser = crate::providers::codex::ResponsesSse::new();
        let mut capture = crate::providers::codex::TurnCapture::new();
        for event in parser.feed(
            br#"data: {"type":"response.completed","response":{"id":"response_test","model":"gpt-5.6-sol","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2},"end_turn":true}}

"#,
        ) {
            capture.observe(&event);
        }
        assert!(capture.turn_ended());
        record_responses_measurement(&codex, &capture, None, 200, "openai_chat", None);

        for (session, binding) in [
            ("anthropic-chat", "anthropic_sub.anthropic_messages"),
            ("codex-chat", "codex_sub.codex_responses"),
        ] {
            let lane = server
                .store
                .load_lanes()
                .expect("lanes")
                .into_iter()
                .find(|lane| lane.session_id.as_deref() == Some(session))
                .expect("translated success created a lane");
            assert_eq!(lane.pin_generation, 1);
            assert_eq!(
                lane.pin.as_ref().map(|pin| pin.binding_id.as_str()),
                Some(binding)
            );
        }
    }

    #[tokio::test]
    async fn active_chat_policy_falls_back_before_commitment() {
        let (server, primary, backup) = policy_chat_fallback_server().await;
        let store = server.store.clone();
        let runtime = server.policy_runtime.clone();
        let snapshot = runtime.capture();
        let route = snapshot
            .policy()
            .route(
                &crate::policy::RouteId::new("route-openrouter-openrouter_chat_completions")
                    .unwrap(),
            )
            .unwrap();
        let primary_candidate = &route.candidates()[0];
        let primary_circuit = crate::policy::HealthCircuitKey {
            policy_revision_id: snapshot.revision_id(),
            route_id: route.id().clone(),
            candidate_id: primary_candidate.id().clone(),
            binding_id: primary_candidate.binding_id().clone(),
            model_id: "primary-model".to_owned(),
            failure: crate::policy::FailureClass::ServerError,
        };
        let address = serve(server).await;
        let response = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .header("x-toker-session", "fallback-session")
            .json(&serde_json::json!({
                "model": "asked-model",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 16,
                "stream": false
            }))
            .send()
            .await
            .expect("request completes");
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = response.json().await.expect("JSON response");
        assert_eq!(body["model"], "asked-model");
        assert_eq!(primary.seen.lock().unwrap()[0]["model"], "primary-model");
        assert_eq!(backup.seen.lock().unwrap()[0]["model"], "backup-model");
        assert!(
            runtime.circuit_is_open(&snapshot, &primary_circuit, super::now_ms()),
            "a configured classified failure opens the exact failed target circuit"
        );

        let rows = store.requests_since(0, 10).expect("ledger rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(store.count_logical_requests().expect("logical count"), 1);
        let first = rows[0].attempt.as_ref().expect("first attempt");
        let second = rows[1].attempt.as_ref().expect("fallback attempt");
        assert_eq!(first.attempt_group_id, second.attempt_group_id);
        assert_eq!(first.attempt_index.get(), 0);
        assert_eq!(first.reason, crate::store::AttemptReason::Initial);
        assert_eq!(second.attempt_index.get(), 1);
        assert_eq!(second.reason, crate::store::AttemptReason::ServerError);
        assert_eq!(rows[0].provider.as_deref(), Some("openrouter"));
        assert_eq!(rows[1].provider.as_deref(), Some("anthropic_api"));
        assert_eq!(
            rows[1]
                .policy_routing
                .as_ref()
                .and_then(|routing| match &routing.outcome {
                    crate::store::PolicyRoutingOutcome::Attempt { candidate_id, .. } => {
                        Some(candidate_id.as_str())
                    }
                    crate::store::PolicyRoutingOutcome::RouteUnavailable { .. } => None,
                }),
            Some("backup")
        );
        let lane = store
            .load_lanes()
            .expect("lanes")
            .into_iter()
            .find(|lane| lane.session_id.as_deref() == Some("fallback-session"))
            .expect("successful fallback pinned the lane");
        assert_eq!(
            lane.pin.as_ref().map(|pin| pin.binding_id.as_str()),
            Some("anthropic_api.anthropic_messages")
        );
        assert_eq!(
            lane.pin.as_ref().map(|pin| pin.effective_model.as_str()),
            Some("backup-model")
        );
    }

    #[tokio::test]
    async fn request_native_auth_failure_does_not_open_a_shared_circuit() {
        let (server, primary, backup) =
            policy_chat_fallback_server_with_openrouter_key(false).await;
        primary
            .response_mode
            .store(CHAT_AUTH_FAILURE, Ordering::Relaxed);
        let runtime = server.policy_runtime.clone();
        let snapshot = runtime.capture();
        let route = snapshot
            .policy()
            .route(
                &crate::policy::RouteId::new("route-openrouter-openrouter_chat_completions")
                    .unwrap(),
            )
            .unwrap();
        let primary_candidate = &route.candidates()[0];
        let auth_circuit = crate::policy::HealthCircuitKey {
            policy_revision_id: snapshot.revision_id(),
            route_id: route.id().clone(),
            candidate_id: primary_candidate.id().clone(),
            binding_id: primary_candidate.binding_id().clone(),
            model_id: "primary-model".to_owned(),
            failure: crate::policy::FailureClass::Authentication,
        };
        let address = serve(server).await;
        let response = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .header(axum::http::header::AUTHORIZATION, "Bearer request-key")
            .json(&serde_json::json!({
                "model": "asked-model",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 16,
                "stream": false
            }))
            .send()
            .await
            .expect("request completes");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(primary.seen.lock().unwrap().len(), 1);
        assert!(backup.seen.lock().unwrap().is_empty());
        assert!(
            !runtime.circuit_is_open(&snapshot, &auth_circuit, super::now_ms()),
            "a request-supplied credential must not poison shared availability"
        );
    }

    #[tokio::test]
    async fn active_chat_policy_does_not_fall_back_for_an_unlisted_failure() {
        let (server, primary, backup) = policy_chat_fallback_server().await;
        primary
            .response_mode
            .store(CHAT_RATE_LIMIT, Ordering::Relaxed);
        let store = server.store.clone();
        let address = serve(server).await;
        let response = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": "asked-model",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 16,
                "stream": false
            }))
            .send()
            .await
            .expect("request completes");

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(primary.seen.lock().unwrap().len(), 1);
        assert!(backup.seen.lock().unwrap().is_empty());
        let rows = store.requests_since(0, 10).expect("ledger rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].attempt.as_ref().map(|attempt| attempt.reason),
            Some(crate::store::AttemptReason::Initial)
        );
    }

    #[test]
    fn a_catalogue_refresh_and_a_credential_change_each_clear_a_safeguards_mark() {
        let dir = crate::setup::test_dir("safeguards-mark-clears");
        let server = server(&dir);
        let binding = crate::policy::BindingId::new("openrouter.openrouter_messages").unwrap();
        let listing = |fetched_at_ms| crate::catalog::FetchedCatalog {
            fetched_at_ms,
            models: vec![crate::catalog::FetchedModel {
                id: "some-model".to_owned(),
                context_window: None,
                chronology: None,
                raw: serde_json::Value::Null,
            }],
        };
        server.install_catalog("openrouter", listing(1));

        server.mark_safeguards_rejected(&binding);
        assert!(server.safeguards_marks().contains(&binding));
        assert_eq!(server.store.load_safeguards_marks().unwrap().len(), 1);

        // The same listing fetched again still clears it.
        server.install_catalog("openrouter", listing(2));
        assert!(server.safeguards_marks().is_empty());
        assert!(server.store.load_safeguards_marks().unwrap().is_empty());

        server.mark_safeguards_rejected(&binding);
        assert_eq!(
            server.clear_safeguards_marks_for_provider("anthropic_sub"),
            0
        );
        assert!(server.safeguards_marks().contains(&binding));
        assert_eq!(server.clear_safeguards_marks_for_provider("openrouter"), 1);
        assert!(server.safeguards_marks().is_empty());
    }

    #[tokio::test]
    async fn active_chat_policy_selects_target_and_records_provenance() {
        let (server, seen) = policy_chat_server().await;
        server.install_catalog(
            "openrouter",
            crate::catalog::FetchedCatalog {
                fetched_at_ms: super::now_ms(),
                models: vec![crate::catalog::FetchedModel {
                    id: "legacy-only".to_owned(),
                    context_window: None,
                    chronology: None,
                    raw: serde_json::Value::Null,
                }],
            },
        );
        assert_eq!(
            server.policy_runtime.capture().revision_id(),
            1,
            "catalogue refresh preserves active routing authority"
        );
        let store = server.store.clone();
        let address = serve(server).await;
        let response = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .header("x-toker-session", "policy-session")
            .json(&serde_json::json!({
                "model": "policy-alias",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 16,
                "stream": false
            }))
            .send()
            .await
            .expect("request succeeds");
        let status = response.status();
        let response_bytes = response.bytes().await.expect("response body");
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&response_bytes)
        );
        let body: serde_json::Value =
            serde_json::from_slice(&response_bytes).expect("JSON response");
        assert_eq!(body["model"], "policy-alias");
        assert_eq!(
            seen.seen.lock().unwrap()[0]["model"],
            "policy-effective",
            "the active policy selected both binding and model despite the conflicting legacy default"
        );
        let rows = store.requests_since(0, 10).expect("ledger rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider.as_deref(), Some("openai_api"));
        assert_eq!(
            rows[0]
                .binding_id
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("openai_api.openai_chat_completions")
        );
        let routing = rows[0]
            .policy_routing
            .as_ref()
            .expect("authoritative provenance");
        assert_eq!(routing.policy_revision_id, 1);
        assert_eq!(
            routing.route.as_ref().unwrap().route_id.as_str(),
            "route-openai_api-openai_chat_completions"
        );
        assert!(rows[0].attempt.is_some());
        let lane = store
            .load_lanes()
            .expect("lanes")
            .into_iter()
            .find(|lane| lane.session_id.as_deref() == Some("policy-session"))
            .expect("successful policy request created a lane");
        let pin = lane.pin.expect("successful policy request pinned its lane");
        assert_eq!(pin.policy_revision_id, 1);
        assert_eq!(
            pin.binding_id.as_str(),
            "openai_api.openai_chat_completions"
        );
        assert_eq!(pin.effective_model, "policy-effective");
        assert_eq!(lane.pin_generation, 1);
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
        let sub = server
            .registry
            .provider("anthropic_sub")
            .cloned()
            .expect("sub");

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
        let sub = server
            .registry
            .provider("anthropic_sub")
            .cloned()
            .expect("sub");

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
        let sub = server
            .registry
            .provider("anthropic_sub")
            .cloned()
            .expect("sub");

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
        let api = keyed
            .registry
            .provider("anthropic_api")
            .cloned()
            .expect("api");
        keyed.borrow_catalog_credential(api.as_ref(), &bearer("t"));
        let keyed_sub = keyed
            .registry
            .provider("anthropic_sub")
            .cloned()
            .expect("sub");
        keyed.borrow_catalog_credential(keyed_sub.as_ref(), &bearer("t"));

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "{:?}",
            seen.lock().unwrap()
        );
        assert_eq!(server.catalog_borrowed_at.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn compaction_lane_uses_positive_cold_proof_without_a_pin() {
        use crate::policy::CacheTemperature;
        use crate::store::Lane;

        let dir = crate::setup::test_dir("compaction-lane-cold");
        let server = server(&dir);
        let binding = server
            .registry
            .bindings()
            .find(|binding| binding.id_str() == "anthropic_sub.anthropic_messages")
            .unwrap();
        let now = 4_000_000;
        let mut lane = Lane {
            key: "lane".to_owned(),
            session_id: Some("session".to_owned()),
            tools_hash: Some("tools".to_owned()),
            updated_ms: 0,
            prompt_tokens: Some(200_000),
            ttl: Some(3_600_000),
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
            pin_generation: 0,
            pin: None,
        };

        assert_eq!(
            server
                .lane_facts(Some(&lane), Some(binding), &Default::default(), now, true)
                .unwrap()
                .temperature,
            CacheTemperature::Cold
        );
        lane.updated_ms = now - 1_000;
        assert_eq!(
            server
                .lane_facts(Some(&lane), Some(binding), &Default::default(), now, true)
                .unwrap()
                .temperature,
            CacheTemperature::Unknown,
            "a positive warm identity still needs a matching policy pin"
        );
        lane.prompt_tokens = Some(1);
        lane.updated_ms = 0;
        assert_eq!(
            server
                .lane_facts(Some(&lane), Some(binding), &Default::default(), now, true)
                .unwrap()
                .temperature,
            CacheTemperature::Unknown,
            "an idle lane below the compaction token floor is ineligible"
        );
    }

    #[test]
    fn cache_temperature_requires_matching_verified_binding_evidence() {
        use crate::policy::CacheTemperature;
        use crate::store::{Lane, LanePin};

        let dir = crate::setup::test_dir("cache-temperature");
        let server = server(&dir);
        let binding = server
            .registry
            .bindings()
            .find(|binding| binding.id_str() == "openrouter.openrouter_chat_completions")
            .unwrap();
        let now = 20 * 60 * 1000;
        let mut lane = Lane {
            key: "lane".to_owned(),
            session_id: Some("session".to_owned()),
            tools_hash: Some("tools".to_owned()),
            updated_ms: now - 5 * 60 * 1000,
            prompt_tokens: None,
            ttl: None,
            ping: None,
            noticed_at: None,
            forced_from: None,
            forced_to: None,
            pin_generation: 0,
            pin: None,
        };
        assert_eq!(
            super::cache_temperature(&lane, Some(binding), None, now),
            CacheTemperature::Unknown,
            "an unattributed legacy lane cannot borrow a binding TTL"
        );
        lane.pin = Some(LanePin {
            policy_revision_id: 1,
            policy_route_id: crate::policy::RouteId::new("route").unwrap(),
            route_fingerprint: "11".repeat(32).parse().unwrap(),
            candidate_id: crate::policy::CandidateId::new("candidate").unwrap(),
            binding_id: binding.id().clone(),
            account_number: Some(1),
            baseline_model: "model".to_owned(),
            effective_model: "model".to_owned(),
        });
        assert_eq!(
            super::cache_temperature(&lane, Some(binding), None, now),
            CacheTemperature::Warm
        );
        assert_eq!(
            super::cache_temperature(&lane, Some(binding), Some(1), now),
            CacheTemperature::Warm,
            "the account asked about is the one the lane is pinned to"
        );
        assert_eq!(
            super::cache_temperature(&lane, Some(binding), Some(2), now),
            CacheTemperature::Cold,
            "the cache was written under another account, so it cannot be read"
        );
        lane.updated_ms = now - 10 * 60 * 1000;
        assert_eq!(
            super::cache_temperature(&lane, Some(binding), None, now),
            CacheTemperature::Cold
        );
        lane.updated_ms = now + 1;
        assert_eq!(
            super::cache_temperature(&lane, Some(binding), None, now),
            CacheTemperature::Unknown
        );
        let no_ttl = server
            .registry
            .bindings()
            .find(|binding| binding.id_str() == "anthropic_sub.anthropic_messages")
            .unwrap();
        lane.pin.as_mut().unwrap().binding_id = no_ttl.id().clone();
        lane.updated_ms = now - 5 * 60 * 1000;
        assert_eq!(
            super::cache_temperature(&lane, Some(no_ttl), None, now),
            CacheTemperature::Unknown,
            "a binding with no cache domain and a lane that recorded no TTL \
             has no clock, and an unknown clock is never cold"
        );
        // Anthropic bindings declare no cache domain; the lane's recorded TTL
        // is the clock, as it is for the cold gate.
        lane.ttl = Some(300_000);
        assert_eq!(
            super::cache_temperature(&lane, Some(no_ttl), None, now),
            CacheTemperature::Cold,
            "idle for exactly the recorded five-minute TTL"
        );
        lane.updated_ms = now - 60 * 1000;
        assert_eq!(
            super::cache_temperature(&lane, Some(no_ttl), None, now),
            CacheTemperature::Warm
        );
        lane.ttl = Some(3_600_000);
        lane.updated_ms = now - 20 * 60 * 1000;
        assert_eq!(
            super::cache_temperature(&lane, Some(no_ttl), None, now),
            CacheTemperature::Warm,
            "the one-hour TTL outlasts twenty idle minutes"
        );
        lane.pin = None;
        assert_eq!(
            super::cache_temperature(&lane, Some(no_ttl), None, now),
            CacheTemperature::Unknown,
            "an unattributed lane cannot borrow a binding's TTL"
        );
    }

    fn messages_facts(server: &Server, model: &str) -> crate::policy::RequestFacts {
        server.request_facts(
            ProtocolId::AnthropicMessages,
            None,
            Some(model.to_owned()),
            Some("lane".to_owned()),
            1,
            Some(3),
            0,
            Default::default(),
            None,
            crate::policy::RequestClass::Inference,
        )
    }

    #[test]
    fn a_model_switch_on_a_warm_pinned_lane_is_the_users_own_consent_to_lose_the_cache() {
        use crate::policy::{CacheTemperature, LaneTransitionDecision, TransitionSource};

        let dir = crate::setup::test_dir("explicit-model-switch");
        let server = server(&dir);
        let credentials = crate::policy::CredentialSnapshot::new();
        let resolve = |stored: Option<&crate::store::Lane>, model: &str| {
            let lane = stored.map(|stored| lane_facts(stored, CacheTemperature::Warm));
            server.resolve_policy_attempt(
                &messages_facts(&server, model),
                &credentials,
                stored,
                lane.as_ref(),
            )
        };

        let first = resolve(None, "claude-opus-4-8").expect("a new lane resolves");
        let LaneTransitionDecision::FirstPin { new_pin, .. } = first.pin_transition else {
            panic!("a new lane is pinned: {:?}", first.pin_transition);
        };
        let mut stored = unpinned_lane();
        stored.pin_generation = 1;
        stored.pin = Some(new_pin.into());

        // Asking for the same model again changes nothing, warm or not.
        let again = resolve(Some(&stored), "claude-opus-4-8").expect("the same model resolves");
        assert!(
            matches!(again.pin_transition, LaneTransitionDecision::Keep { .. }),
            "{:?}",
            again.pin_transition
        );

        // Asking for another model is the user's choice: the warm cache it
        // leaves behind is theirs to give up.
        let switched = resolve(Some(&stored), "claude-sonnet-5").expect("a model switch resolves");
        assert!(
            matches!(
                &switched.pin_transition,
                LaneTransitionDecision::Replace {
                    new_pin,
                    source: TransitionSource::ExplicitUserChange,
                    ..
                } if new_pin.effective_model == "claude-sonnet-5"
            ),
            "{:?}",
            switched.pin_transition
        );
    }

    fn one_model_catalogue(fetched_at_ms: i64) -> crate::catalog::FetchedCatalog {
        crate::catalog::FetchedCatalog {
            fetched_at_ms,
            models: vec![crate::catalog::FetchedModel {
                id: "model".to_owned(),
                context_window: None,
                chronology: None,
                raw: serde_json::Value::Null,
            }],
        }
    }

    #[test]
    fn catalogue_changes_fan_out_only_to_bindings_for_that_source() {
        let dir = crate::setup::test_dir("catalogue-binding-generations");
        let server = server(&dir);
        let empty = crate::catalog::FetchedCatalogs::default();
        let mut openrouter = crate::catalog::FetchedCatalogs::default();
        openrouter.set("openrouter", one_model_catalogue(10));
        let changed = super::changed_catalogue_bindings(&server.registry, &empty, &openrouter);
        assert_eq!(
            changed,
            server
                .registry
                .bindings()
                .filter(|binding| binding.provider() == "openrouter")
                .map(|binding| binding.id().clone())
                .collect()
        );

        let mut anthropic = openrouter.clone();
        anthropic.set("anthropic", one_model_catalogue(10));
        let changed = super::changed_catalogue_bindings(&server.registry, &openrouter, &anthropic);
        assert_eq!(
            changed,
            server
                .registry
                .bindings()
                .filter(|binding| matches!(binding.provider(), "anthropic_sub" | "anthropic_api"))
                .map(|binding| binding.id().clone())
                .collect()
        );
    }

    #[test]
    fn empty_catalogues_are_equivalent_to_absence_for_generations() {
        let dir = crate::setup::test_dir("catalogue-empty-generations");
        let server = server(&dir);
        let missing = crate::catalog::FetchedCatalogs::default();
        let mut empty = crate::catalog::FetchedCatalogs::default();
        empty.set(
            "openrouter",
            crate::catalog::FetchedCatalog {
                fetched_at_ms: 10,
                models: vec![],
            },
        );
        let mut newer_empty = crate::catalog::FetchedCatalogs::default();
        newer_empty.set(
            "openrouter",
            crate::catalog::FetchedCatalog {
                fetched_at_ms: 20,
                models: vec![],
            },
        );
        assert!(super::changed_catalogue_bindings(&server.registry, &missing, &empty).is_empty());
        assert!(
            super::changed_catalogue_bindings(&server.registry, &empty, &newer_empty).is_empty()
        );

        let mut present = crate::catalog::FetchedCatalogs::default();
        present.set("openrouter", one_model_catalogue(20));
        assert!(!super::changed_catalogue_bindings(&server.registry, &empty, &present).is_empty());
        assert!(
            !super::changed_catalogue_bindings(&server.registry, &present, &missing).is_empty()
        );
    }

    #[test]
    fn empty_incident_catalogue_is_absent_but_nonempty_catalogue_is_fresh() {
        let dir = crate::setup::test_dir("catalogue-availability");
        let server = server(&dir);
        let binding = "openrouter.openrouter_chat_completions"
            .parse::<crate::policy::BindingId>()
            .unwrap();
        let mut catalogs = crate::catalog::FetchedCatalogs::default();
        catalogs.set(
            "openrouter",
            crate::catalog::FetchedCatalog {
                fetched_at_ms: 10,
                models: vec![],
            },
        );
        assert_eq!(
            super::availability_for(&server.registry, &catalogs)
                .get(&binding)
                .unwrap()
                .catalogue,
            crate::policy::CatalogueAvailability::Absent
        );
        catalogs.set("openrouter", one_model_catalogue(20));
        assert_eq!(
            super::availability_for(&server.registry, &catalogs)
                .get(&binding)
                .unwrap()
                .catalogue,
            crate::policy::CatalogueAvailability::Fresh { fetched_at_ms: 20 }
        );
    }

    /// The response may name another identity than the one toker sent (a
    /// host alias, an upstream substitution), and what the binding learns
    /// must follow what actually served.
    #[test]
    fn a_recorded_response_teaches_the_binding_the_model_it_named() {
        use crate::server::record_anthropic::{AnthropicRecordCtx, record_anthropic_measurement};
        let dir = crate::setup::test_dir("served-mark");
        let server = server(&dir);
        let target = server
            .registry
            .resolve_binding_model_free(
                ProtocolId::AnthropicMessages,
                crate::routing::ANTHROPIC_SUB_MESSAGES.id(),
            )
            .expect("enabled");
        let ctx = AnthropicRecordCtx {
            server: server.clone(),
            started: std::time::Instant::now(),
            path: "/v1/messages",
            session_id: None,
            lineage: None,
            requested_model: Some("claude-opus-5".to_owned()),
            effective_model: Some("claude-opus-5".to_owned()),
            backend: target.provider().clone(),
            credential_digest: None,
            account_number: None,
            binding_id: target.binding().id().clone(),
            attempt: None,
            policy_routing: None,
            pin_transition: None,
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
            translation_report: None,
            safeguards: Default::default(),
            policy_attempt: None,
        };
        let mut observer = crate::observe::AnthropicObserver::new();
        observer.observe_json(
            br#"{"type":"message","model":"claude-sonnet-5","usage":{"input_tokens":3,"output_tokens":1}}"#,
        );
        use crate::policy::LearningView;
        let learned = |model: &str| {
            server
                .binding_learning
                .learned(&ctx.binding_id, model)
                .expect("learning readable")
        };
        assert!(learned("claude-sonnet-5").is_none());
        let (reported_model, capture) = observer.finish_parts();
        record_anthropic_measurement(&ctx, capture.as_ref(), reported_model.as_deref(), None, 200);
        let rows = server.store.requests_since(0, 10).expect("ledger rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].binding_id, Some(ctx.binding_id.clone()));
        assert_eq!(rows[0].policy_routing, None);
        assert_eq!(rows[0].lineage, None);
        assert!(
            learned("claude-sonnet-5").is_some(),
            "the served identity is learned from the response"
        );
        assert!(
            learned("claude-opus-5").is_none(),
            "the sent identity is not what the response teaches"
        );
        // The resolver reads the published snapshot, not the store.
        let snapshot = server.binding_learning.snapshot();
        assert!(
            snapshot
                .learned(&ctx.binding_id, "claude-sonnet-5")
                .expect("snapshot readable")
                .is_some(),
            "a learned model reaches the snapshot the resolver reads"
        );
    }
}
