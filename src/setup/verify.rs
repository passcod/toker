//! The post-install wiring check — the wizard's ordering-rule
//! enforcement point.
//!
//! Plan: "Setup wizard" step 5's ordering rule — "**bind the socket
//! first, then point clients at it**" (a frontend's env hot-reloads
//! into running sessions, so pointing a client at a listener that is
//! not up yet kills live sessions). The wizard installs/starts the
//! units, runs [`await_service_ready`] BEFORE touching any frontend
//! config, and only patches frontends on a service that answered.
//!
//! The check itself is the wiring check used throughout this project:
//! an **empty-body POST** to each usage path. toker forwards an
//! unparseable body verbatim (the IR parse failing is the passthrough
//! case, see `server::proxy` / `server::anthropic`), so the response
//! that comes back is the **upstream's own verdict on an
//! unauthenticated empty request** — typically its 401, the
//! no-key-configured case the providers document as "visibly verifying
//! the wiring". Any upstream verdict counts (401, 400, 404… a
//! signed-always backend like codex_sub answers something other than
//! 401 on its own paths): what is being proven is the whole chain —
//! listener up, route answering, upstream round-tripping.
//!
//! What does NOT count as an answer: toker's own synthetic failure
//! statuses (see [`TOKER_OWN_STATUSES`]) — the listener being up while
//! the upstream round-trip is broken is exactly the state the wizard
//! must not point clients into — and no response at all (connection
//! refused while the units are still starting, a hung exchange). Both
//! are retried on [`POLL`] cadence until `timeout`.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// The probe cadence while waiting for the service to answer.
const POLL: Duration = Duration::from_millis(250);

/// One probe's budget: an empty-body request that cannot complete
/// inside this is not an answer this instant — it is retried until
/// the caller's overall timeout runs out.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Statuses toker itself synthesises on a usage path (see
/// `server::proxy`): 502 — the upstream request failed; 413 — the
/// request body cap (unreachable for an empty body, classified
/// anyway). Seeing one of these means the listener answered but the
/// wiring did not round-trip — not an answer, keep polling.
const TOKER_OWN_STATUSES: &[u16] = &[502, 413];

/// Which frontend usage paths answered the wiring probe, and with
/// what upstream verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceReady {
    /// The verdict on the empty-body POST to `/v1/messages` (the
    /// anthropic path — typically 401), or `None` when it never
    /// answered within the timeout.
    pub messages: Option<u16>,
    /// The verdict on the empty-body POST to `/v1/chat/completions`
    /// (the openai-chat path), likewise.
    pub chat_completions: Option<u16>,
}

impl ServiceReady {
    /// Whether both frontend paths carry an upstream verdict — the
    /// state the wizard requires before patching any frontend.
    pub fn ready(&self) -> bool {
        self.messages.is_some() && self.chat_completions.is_some()
    }
}

/// Poll toker on `127.0.0.1:{port}` until both usage paths answer with
/// an upstream verdict (see the module docs), or `timeout` runs out.
/// `Ok` carries which paths answered — on success, both; the per-path
/// fields are the record the wizard displays. A timeout is an `Err`
/// whose message says what each path last did, because the caller
/// must NOT proceed to patching frontends (the ordering rule).
pub async fn await_service_ready(port: u16, timeout: Duration) -> anyhow::Result<ServiceReady> {
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .context("building the wiring-probe client")?;
    let deadline = Instant::now() + timeout;

    let mut messages: Option<u16> = None;
    let mut chat: Option<u16> = None;
    let mut messages_last: Option<Seen> = None;
    let mut chat_last: Option<Seen> = None;
    loop {
        if messages.is_none() {
            match probe(&client, &format!("{base}/v1/messages")).await {
                Probe::Answered(status) => messages = Some(status),
                Probe::TokerOwn(status) => messages_last = Some(Seen::TokerOwn(status)),
                Probe::Unreachable => messages_last = Some(Seen::Unreachable),
            }
        }
        if chat.is_none() {
            match probe(&client, &format!("{base}/v1/chat/completions")).await {
                Probe::Answered(status) => chat = Some(status),
                Probe::TokerOwn(status) => chat_last = Some(Seen::TokerOwn(status)),
                Probe::Unreachable => chat_last = Some(Seen::Unreachable),
            }
        }
        if messages.is_some() && chat.is_some() {
            return Ok(ServiceReady {
                messages,
                chat_completions: chat,
            });
        }
        if Instant::now() >= deadline {
            bail!(
                "toker at {base} did not answer every frontend path within {timeout:?} — \
                 POST /v1/messages: {}; POST /v1/chat/completions: {}. The frontends are \
                 NOT being patched: the ordering rule binds the socket and verifies it \
                 before pointing clients at it",
                describe(messages, messages_last),
                describe(chat, chat_last),
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// What one probe saw.
enum Probe {
    /// An upstream verdict passed back through toker — an answer.
    Answered(u16),
    /// A response, but one of toker's own synthetic failure statuses.
    TokerOwn(u16),
    /// No HTTP exchange completed at all (refused, reset, timed out).
    Unreachable,
}

/// The last thing an unanswered path did, for the timeout message.
enum Seen {
    TokerOwn(u16),
    Unreachable,
}

/// Probe one usage path with an empty-body POST (see the module docs).
async fn probe(client: &reqwest::Client, url: &str) -> Probe {
    match client.post(url).body(String::new()).send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            if TOKER_OWN_STATUSES.contains(&status) {
                Probe::TokerOwn(status)
            } else {
                Probe::Answered(status)
            }
        }
        Err(_) => Probe::Unreachable,
    }
}

/// One path's line for the timeout message.
fn describe(answered: Option<u16>, last: Option<Seen>) -> String {
    match answered {
        Some(status) => format!("answered {status} (the upstream's verdict)"),
        None => match last {
            Some(Seen::TokerOwn(status)) => {
                format!("toker's own {status} — the upstream did not round-trip")
            }
            _ => "no response".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::post;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A scratch axum server answering the two usage paths with fixed
    /// statuses — an upstream stand-in, never the real toker (scratch
    /// ports only; the machine's live listener is never probed).
    async fn scratch_server(
        messages: StatusCode,
        chat: StatusCode,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new()
            .route("/v1/messages", post(move || async move { messages }))
            .route("/v1/chat/completions", post(move || async move { chat }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scratch");
        let port = listener.local_addr().expect("local addr").port();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("the scratch server serves");
        });
        (port, handle)
    }

    /// A port with nothing on it (bound then dropped).
    fn dropped_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    }

    #[tokio::test]
    async fn a_ready_service_answers_both_paths_with_the_upstream_verdict() {
        let (port, _server) =
            scratch_server(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED).await;
        let ready = await_service_ready(port, Duration::from_secs(5))
            .await
            .expect("both paths answer");
        assert_eq!(
            ready,
            ServiceReady {
                messages: Some(401),
                chat_completions: Some(401)
            },
            "the classic wiring check: the unauthenticated upstream 401"
        );
        assert!(ready.ready());
    }

    #[tokio::test]
    async fn upstream_verdicts_other_than_401_count() {
        // A codex_sub-shaped route answers 404-ish on its own paths and
        // an empty chat body with a key configured draws a 400: any
        // upstream verdict proves the chain, not just the 401.
        let (port, _server) = scratch_server(StatusCode::NOT_FOUND, StatusCode::BAD_REQUEST).await;
        let ready = await_service_ready(port, Duration::from_secs(5))
            .await
            .expect("both paths answer");
        assert_eq!(ready.messages, Some(404));
        assert_eq!(ready.chat_completions, Some(400));
        assert!(ready.ready());
    }

    #[tokio::test]
    async fn a_toker_own_status_is_not_an_answer_and_times_out_with_partial_info() {
        let (port, _server) =
            scratch_server(StatusCode::UNAUTHORIZED, StatusCode::BAD_GATEWAY).await;
        let error = await_service_ready(port, Duration::from_millis(400))
            .await
            .expect_err("a 502 chat path must never count as answering");
        let message = format!("{error:#}");
        assert!(
            message.contains("/v1/messages") && message.contains("answered 401"),
            "the message says what DID answer: {message}"
        );
        assert!(
            message.contains("toker's own 502") && message.contains("did not round-trip"),
            "and what did not: {message}"
        );
        assert!(
            message.contains("NOT being patched"),
            "the ordering rule is in the refusal: {message}"
        );
    }

    #[tokio::test]
    async fn nothing_listening_times_out() {
        let port = dropped_port();
        let error = await_service_ready(port, Duration::from_millis(300))
            .await
            .expect_err("nothing is listening");
        let message = format!("{error:#}");
        assert!(
            message.contains(&format!("127.0.0.1:{port}"))
                && message.contains("/v1/messages")
                && message.contains("/v1/chat/completions"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_path_that_starts_refusing_but_answers_is_waited_out() {
        // The units-are-starting shape: the first probes see a toker
        // own status (or nothing), then the upstream verdict arrives —
        // the poll loop must keep retrying, not fail on first sight.
        let chat_seen = Arc::new(AtomicUsize::new(0));
        let chat_seen_probe = chat_seen.clone();
        let app = axum::Router::new()
            .route("/v1/messages", post(|| async { StatusCode::UNAUTHORIZED }))
            .route(
                "/v1/chat/completions",
                post(move || {
                    let n = chat_seen_probe.fetch_add(1, Ordering::SeqCst);
                    let status = if n < 2 {
                        StatusCode::BAD_GATEWAY
                    } else {
                        StatusCode::UNAUTHORIZED
                    };
                    async move { status }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scratch");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("scratch serves");
        });

        let ready = await_service_ready(port, Duration::from_secs(5))
            .await
            .expect("the chat path comes good");
        assert_eq!(ready.chat_completions, Some(401));
        assert!(ready.ready());
    }
}
