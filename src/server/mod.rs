//! Axum listener, routing, and streaming.
//!
//! Plan: "Server core" + "Deployment" — one loopback socket (all frontend
//! protocols plus `/_toker/*`), socket-activated via `listenfd` with a
//! direct bind fallback; buffered request bodies for gating, pass-through
//! response streams with a crash-proof SSE side-parser for
//! usage/model/cost.
//!
//! Routes (phase 1, the openai_chat frontend → openrouter backend):
//!
//! - `POST /v1/chat/completions` — the usage path: buffered, parsed to the
//!   IR, fidelity-checked, routed, recorded ([`proxy`]).
//! - `GET /v1/models` — transparent forwarding, no recording (not a usage
//!   path).
//! - `GET /_toker/status`, `POST /_toker/models/merge` — the control
//!   endpoint, gated by a custom header ([`control`]).
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

pub(crate) mod control;
pub(crate) mod proxy;
mod record;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::bail;
use axum::Router;
use axum::routing::{get, post};

use crate::config::Config;
use crate::providers::{OpenRouter, Provider};
use crate::store::Store;

/// The running proxy: config, ledger, upstream client, and the backend
/// providers, cloned cheaply into every request handler.
#[derive(Clone)]
pub struct Server {
    pub(crate) store: Arc<Store>,
    pub(crate) config: Arc<Config>,
    /// The shared upstream HTTP client. Connect timeout only — no read
    /// timeout, so streams live as long as their connections do (see the
    /// module docs).
    pub(crate) http: reqwest::Client,
    /// Phase 1's one backend; a trait object because routing selects by
    /// `provider/model` prefix and later phases add providers to exactly
    /// this slot.
    pub(crate) openrouter: Arc<dyn Provider>,
    /// Process start, for `/_toker/status` uptime.
    pub(crate) started: Instant,
}

impl Server {
    /// Build the server: resolve the provider credential once, validate
    /// the phase-1 routing table, build the upstream client.
    pub fn new(config: Config, store: Arc<Store>) -> anyhow::Result<Server> {
        if config.default_backend_openai_chat != "openrouter" {
            bail!(
                "phase 1 wires only the openrouter backend, \
                 default_backend_openai_chat = {:?} is not available yet",
                config.default_backend_openai_chat
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
        Ok(Server {
            store,
            config: Arc::new(config),
            http,
            openrouter,
            started: Instant::now(),
        })
    }

    /// The full route table.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/chat/completions", post(proxy::chat_completions))
            .route("/v1/models", get(proxy::models))
            .route("/_toker/status", get(control::status))
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
        axum::serve(listener, self.router()).await?;
        Ok(())
    }
}
