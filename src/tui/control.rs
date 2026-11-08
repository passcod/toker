//! Bounded, asynchronous access to one of the daemon's control namespaces.
//!
//! The worker owns the HTTP client.  Terminal input only ever fills a tiny
//! queue, so an unavailable or restarting daemon cannot stall redraws.
//!
//! One worker serves one namespace (`routing`, `config`), named when the
//! client is built, so a page's commands can only reach its own endpoints.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::Duration;

use reqwest::Method;
use serde_json::Value;

const REQUEST_QUEUE: usize = 4;
const REPLY_QUEUE: usize = 8;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Writes (an activation compiles and persists a revision) get longer than
/// reads: a timeout on a write that then lands would report a failure for a
/// change the daemon made, and skip the settings sync that follows it.
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);

/// A named control operation, kept with its reply so state can distinguish an
/// old snapshot from a draft edit or confirmation action.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Command {
    pub(crate) name: &'static str,
    pub(crate) method: Method,
    pub(crate) path: &'static str,
    pub(crate) control: &'static str,
    pub(crate) body: Option<Value>,
}

impl Command {
    /// A namespace's snapshot read. `control` is the operation header the
    /// endpoint expects, which differs per namespace.
    pub(crate) fn snapshot(control: &'static str) -> Self {
        Self {
            name: "snapshot",
            method: Method::GET,
            path: "snapshot",
            control,
            body: None,
        }
    }
}

#[derive(Debug)]
enum Request {
    Run(Command),
}

/// HTTP status and JSON body returned by a routing control operation.
#[derive(Debug)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: Value,
}

/// A reply from the routing control endpoint.
#[derive(Debug)]
pub(crate) enum Reply {
    Command(Command, Result<Response, String>),
}

/// Whether a reply is a successful activation or reactivation, after which
/// the user's Claude settings should follow the new active policy.
/// The terminal-side half of one namespace's control worker.
pub(crate) struct Client {
    requests: SyncSender<Request>,
    replies: Receiver<Reply>,
    /// The operation header this namespace's snapshot read expects.
    snapshot: &'static str,
}

impl Client {
    /// Start a worker against the configured loopback listener.
    pub(crate) fn new(port: u16, namespace: &'static str, snapshot: &'static str) -> Client {
        let (requests, request_rx) = sync_channel(REQUEST_QUEUE);
        let (reply_tx, replies) = sync_channel(REPLY_QUEUE);
        thread::Builder::new()
            .name(format!("toker-{namespace}-control"))
            .spawn(move || {
                worker(
                    base_endpoint(port, namespace),
                    snapshot,
                    request_rx,
                    reply_tx,
                )
            })
            .expect("spawn the control worker");
        Client {
            requests,
            replies,
            snapshot,
        }
    }

    /// Ask for a fresh snapshot without waiting for either queue.
    pub(crate) fn refresh(&self) -> bool {
        self.send(Command::snapshot(self.snapshot))
    }

    /// Submit an operation without waiting for either queue.
    pub(crate) fn send(&self, command: Command) -> bool {
        match self.requests.try_send(Request::Run(command)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Drain one reply, if the worker has one ready.
    pub(crate) fn poll(&self) -> Option<Reply> {
        self.replies.try_recv().ok()
    }
}

fn base_endpoint(port: u16, namespace: &str) -> String {
    format!("http://127.0.0.1:{port}/_toker/{namespace}")
}

fn worker(
    base: String,
    snapshot: &'static str,
    requests: Receiver<Request>,
    replies: SyncSender<Reply>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = replies.try_send(Reply::Command(
                Command::snapshot(snapshot),
                Err(format!("the control worker is unavailable ({error})")),
            ));
            return;
        }
    };
    let client = match reqwest::Client::builder().build() {
        Ok(client) => client,
        Err(error) => {
            let _ = replies.try_send(Reply::Command(
                Command::snapshot(snapshot),
                Err(format!("the control client is unavailable ({error})")),
            ));
            return;
        }
    };
    while let Ok(Request::Run(command)) = requests.recv() {
        let result = runtime.block_on(request(&client, &base, &command));
        let _ = replies.try_send(Reply::Command(command, result));
    }
}

async fn request(
    client: &reqwest::Client,
    base: &str,
    command: &Command,
) -> Result<Response, String> {
    let mut request = client
        .request(command.method.clone(), format!("{base}/{}", command.path))
        .header("x-toker-control", command.control)
        .timeout(if command.method == Method::GET {
            REQUEST_TIMEOUT
        } else {
            WRITE_TIMEOUT
        });
    if let Some(body) = &command.body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("routing service unavailable ({error})"))?;
    let status = response.status().as_u16();
    let body = response.json().await.map_err(|error| {
        format!(
            "routing {name} reply was not JSON ({error})",
            name = command.name
        )
    })?;
    Ok(Response { status, body })
}

#[cfg(test)]
mod tests {
    use super::{Command, base_endpoint};

    #[test]
    fn control_requests_are_pinned_to_loopback_and_their_namespace() {
        assert_eq!(
            base_endpoint(18123, "routing"),
            "http://127.0.0.1:18123/_toker/routing"
        );
        assert_eq!(
            base_endpoint(18123, "config"),
            "http://127.0.0.1:18123/_toker/config"
        );
        assert_eq!(
            Command::snapshot("config-snapshot").control,
            "config-snapshot"
        );
    }
}
