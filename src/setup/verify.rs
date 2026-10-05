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
//! The check is a list of [`Check`]s, each retried on [`POLL`] cadence
//! until it answers or `timeout` runs out:
//!
//! - [`Check::Usage`] — an **empty-body POST** to a usage path, through
//!   the exact prefix the frontend will be pointed at. toker forwards an
//!   unparseable body verbatim (the IR parse failing is the passthrough
//!   case, see `server::proxy` / `server::anthropic`), so the response
//!   that comes back is the **upstream's own verdict on an
//!   unauthenticated empty request** — typically its 401, the
//!   no-key-configured case the providers document as "visibly verifying
//!   the wiring". Any upstream verdict counts (401, 400, 404… a
//!   signed-always backend like codex_sub answers something other than
//!   401 on its own paths): what is being proven is the whole chain —
//!   listener up, route answering, upstream round-tripping.
//! - [`Check::Prefix`] — the frontend's `/f/<name>` prefix reaches
//!   toker's own status endpoint. An upstream verdict on a prefixed
//!   usage path cannot prove the prefix was stripped: a toker that
//!   predates prefixes forwards `/f/claude/v1/messages` upstream as it
//!   stands and relays the upstream's 404 — exactly what a frontend
//!   patched to that prefix would then get on every request. Only a
//!   toker that strips the prefix answers its own status there.
//!
//! What does NOT count as an answer: toker's own synthetic failure
//! statuses (see [`TOKER_OWN_STATUSES`]) — the listener being up while
//! the upstream round-trip is broken is exactly the state the wizard
//! must not point clients into — toker's not-configured answer (the
//! path's protocol has no backend), and no response at all (connection
//! refused while the units are still starting, a hung exchange).

use std::time::{Duration, Instant};

use anyhow::{Context, bail};

use crate::server::NOT_CONFIGURED_HEADER;

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

/// One thing the service must answer before any frontend is patched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// An empty-body POST to this usage path (prefix included) must carry
    /// an upstream verdict back.
    Usage(String),
    /// toker's own status endpoint must answer under `/f/<this name>`.
    Prefix(String),
}

impl Check {
    /// The path probed.
    pub fn path(&self) -> String {
        match self {
            Check::Usage(path) => path.clone(),
            Check::Prefix(name) => format!("/f/{name}/_toker/status"),
        }
    }

    /// How the check reads in the wizard's output.
    pub fn describe(&self) -> String {
        match self {
            Check::Usage(path) => format!("POST {path}"),
            Check::Prefix(name) => format!("the /f/{name} prefix"),
        }
    }
}

/// Every check that answered, with its verdict, in the order asked: the
/// upstream's status for a usage check, toker's 200 for a prefix check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceReady {
    pub answers: Vec<(Check, u16)>,
}

/// Poll toker on `127.0.0.1:{port}` until every check answers (see the
/// module docs), or `timeout` runs out. `Ok` carries each check's
/// verdict, the record the wizard displays. A timeout is an `Err` whose
/// message says what each check last did, because the caller must NOT
/// proceed to patching frontends (the ordering rule).
pub async fn await_service_ready(
    port: u16,
    checks: &[Check],
    timeout: Duration,
) -> anyhow::Result<ServiceReady> {
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .context("building the wiring-probe client")?;
    let deadline = Instant::now() + timeout;

    let mut answered: Vec<Option<u16>> = vec![None; checks.len()];
    let mut last: Vec<Option<Seen>> = vec![None; checks.len()];
    loop {
        for (index, check) in checks.iter().enumerate() {
            if answered[index].is_some() {
                continue;
            }
            match probe(&client, &base, check).await {
                Probe::Answered(status) => answered[index] = Some(status),
                Probe::Not(seen) => last[index] = Some(seen),
            }
        }
        if answered.iter().all(Option::is_some) {
            return Ok(ServiceReady {
                answers: checks
                    .iter()
                    .cloned()
                    .zip(answered.into_iter().flatten())
                    .collect(),
            });
        }
        if Instant::now() >= deadline {
            let lines: Vec<String> = checks
                .iter()
                .enumerate()
                .map(|(index, check)| {
                    format!(
                        "{}: {}",
                        check.describe(),
                        describe(answered[index], &last[index])
                    )
                })
                .collect();
            bail!(
                "toker at {base} did not answer every check within {timeout:?} — {}. The \
                 frontends are NOT being patched: the ordering rule binds the socket and \
                 verifies it before pointing clients at it",
                lines.join("; "),
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// What one probe saw.
enum Probe {
    /// The answer the check wants.
    Answered(u16),
    /// Anything else.
    Not(Seen),
}

/// The last thing an unanswered check did, for the timeout message.
#[derive(Clone)]
enum Seen {
    TokerOwn(u16),
    NotConfigured(String),
    NoPrefix(u16),
    Unreachable,
}

/// Probe one check (see the module docs).
async fn probe(client: &reqwest::Client, base: &str, check: &Check) -> Probe {
    let url = format!("{base}{}", check.path());
    let request = match check {
        Check::Usage(_) => client.post(&url).body(String::new()),
        // The control header names the operation, as the endpoint
        // requires; without it the answer is a 403, not a status.
        Check::Prefix(_) => client.get(&url).header("x-toker-control", "status"),
    };
    let Ok(response) = request.send().await else {
        return Probe::Not(Seen::Unreachable);
    };
    let status = response.status().as_u16();
    match check {
        Check::Usage(_) => {
            if let Some(protocol) = response.headers().get(NOT_CONFIGURED_HEADER) {
                let protocol = protocol.to_str().unwrap_or("?").to_owned();
                Probe::Not(Seen::NotConfigured(protocol))
            } else if TOKER_OWN_STATUSES.contains(&status) {
                Probe::Not(Seen::TokerOwn(status))
            } else {
                Probe::Answered(status)
            }
        }
        Check::Prefix(_) if status == 200 => Probe::Answered(status),
        Check::Prefix(_) => Probe::Not(Seen::NoPrefix(status)),
    }
}

/// One check's line for the timeout message.
fn describe(answered: Option<u16>, last: &Option<Seen>) -> String {
    match answered {
        Some(status) => format!("answered {status}"),
        None => match last {
            Some(Seen::TokerOwn(status)) => {
                format!("toker's own {status} — the upstream did not round-trip")
            }
            Some(Seen::NotConfigured(protocol)) => {
                format!("toker has no {protocol} backend configured")
            }
            Some(Seen::NoPrefix(status)) => format!(
                "answered {status}, not toker's status — the running toker does not strip \
                 the prefix (an older build still serving? restart toker.service)"
            ),
            _ => "no response".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A scratch axum server answering the usage paths, prefixed or not,
    /// with fixed statuses, and the prefixed status endpoint when
    /// `strips` — an upstream stand-in, never the real toker (scratch
    /// ports only; the machine's live listener is never probed).
    async fn scratch_server(
        messages: StatusCode,
        chat: StatusCode,
        strips: bool,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let status = if strips {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        };
        let app = axum::Router::new()
            .route("/v1/messages", post(move || async move { messages }))
            .route(
                "/f/{name}/v1/messages",
                post(move || async move { messages }),
            )
            .route("/v1/chat/completions", post(move || async move { chat }))
            .route(
                "/f/{name}/_toker/status",
                get(move || async move { status }),
            );
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

    fn both() -> Vec<Check> {
        vec![
            Check::Usage("/v1/messages".to_owned()),
            Check::Usage("/v1/chat/completions".to_owned()),
        ]
    }

    #[tokio::test]
    async fn a_ready_service_answers_every_check_with_the_upstream_verdict() {
        let (port, _server) =
            scratch_server(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED, true).await;
        let checks = vec![
            Check::Prefix("claude".to_owned()),
            Check::Usage("/f/claude/v1/messages".to_owned()),
            Check::Usage("/v1/chat/completions".to_owned()),
        ];
        let ready = await_service_ready(port, &checks, Duration::from_secs(5))
            .await
            .expect("every check answers");
        assert_eq!(
            ready.answers,
            vec![
                (checks[0].clone(), 200),
                (checks[1].clone(), 401),
                (checks[2].clone(), 401),
            ],
            "the classic wiring check: the unauthenticated upstream 401"
        );
    }

    #[tokio::test]
    async fn upstream_verdicts_other_than_401_count() {
        // A codex_sub-shaped route answers 404-ish on its own paths and
        // an empty chat body with a key configured draws a 400: any
        // upstream verdict proves the chain, not just the 401.
        let (port, _server) =
            scratch_server(StatusCode::NOT_FOUND, StatusCode::BAD_REQUEST, true).await;
        let ready = await_service_ready(port, &both(), Duration::from_secs(5))
            .await
            .expect("both paths answer");
        assert_eq!(ready.answers[0].1, 404);
        assert_eq!(ready.answers[1].1, 400);
    }

    #[tokio::test]
    async fn a_toker_that_does_not_strip_the_prefix_is_not_ready() {
        // The usage path through the prefix answers (an old toker relays
        // the upstream's verdict on the unstripped path), but the
        // prefix check sees no toker status: not ready, and the message
        // says why.
        let (port, _server) =
            scratch_server(StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED, false).await;
        let checks = vec![
            Check::Prefix("claude".to_owned()),
            Check::Usage("/f/claude/v1/messages".to_owned()),
        ];
        let error = await_service_ready(port, &checks, Duration::from_millis(400))
            .await
            .expect_err("an unstripped prefix must never count");
        let message = format!("{error:#}");
        assert!(
            message.contains("the /f/claude prefix") && message.contains("does not strip"),
            "{message}"
        );
        assert!(message.contains("NOT being patched"), "{message}");
    }

    #[tokio::test]
    async fn a_not_configured_answer_is_not_an_answer() {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    StatusCode::NOT_FOUND,
                    [(NOT_CONFIGURED_HEADER, "openai_chat")],
                    "{}",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scratch");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("scratch serves");
        });
        let error = await_service_ready(
            port,
            &[Check::Usage("/v1/chat/completions".to_owned())],
            Duration::from_millis(400),
        )
        .await
        .expect_err("toker's own not-configured answer proves no upstream");
        assert!(
            format!("{error:#}").contains("no openai_chat backend configured"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn a_toker_own_status_is_not_an_answer_and_times_out_with_partial_info() {
        let (port, _server) =
            scratch_server(StatusCode::UNAUTHORIZED, StatusCode::BAD_GATEWAY, true).await;
        let error = await_service_ready(port, &both(), Duration::from_millis(400))
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
        let error = await_service_ready(port, &both(), Duration::from_millis(300))
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

        let ready = await_service_ready(port, &both(), Duration::from_secs(5))
            .await
            .expect("the chat path comes good");
        assert_eq!(ready.answers[1].1, 401);
    }
}
