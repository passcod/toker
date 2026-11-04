//! Bounded, asynchronous access to the daemon's routing control surface.
//!
//! The worker owns the HTTP client.  Terminal input only ever fills a tiny
//! queue, so an unavailable or restarting daemon cannot stall redraws.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::Duration;

use reqwest::Method;
use serde_json::Value;

const REQUEST_QUEUE: usize = 4;
const REPLY_QUEUE: usize = 8;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// A named control operation, kept with its reply so state can distinguish an
/// old snapshot from a draft edit or confirmation action.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Command {
    pub(super) name: &'static str,
    pub(super) method: Method,
    pub(super) path: &'static str,
    pub(super) control: &'static str,
    pub(super) body: Option<Value>,
}

impl Command {
    pub(super) fn snapshot() -> Self {
        Self {
            name: "snapshot",
            method: Method::GET,
            path: "snapshot",
            control: "routing-snapshot",
            body: None,
        }
    }

    pub(super) fn revisions() -> Self {
        Self {
            name: "revisions",
            method: Method::GET,
            path: "revisions",
            control: "routing-revisions",
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
pub(super) struct Response {
    pub(super) status: u16,
    pub(super) body: Value,
}

/// A reply from the routing control endpoint.
#[derive(Debug)]
pub(super) enum Reply {
    Command(Command, Result<Response, String>),
}

/// The terminal-side half of the routing control worker.
pub(super) struct Client {
    requests: SyncSender<Request>,
    replies: Receiver<Reply>,
}

impl Client {
    /// Start a worker against the configured loopback listener.
    pub(super) fn new(port: u16) -> Client {
        let (requests, request_rx) = sync_channel(REQUEST_QUEUE);
        let (reply_tx, replies) = sync_channel(REPLY_QUEUE);
        thread::Builder::new()
            .name("toker-routing-control".to_owned())
            .spawn(move || worker(base_endpoint(port), request_rx, reply_tx))
            .expect("spawn routing control worker");
        Client { requests, replies }
    }

    /// Ask for a fresh snapshot without waiting for either queue.
    pub(super) fn refresh(&self) -> bool {
        self.send(Command::snapshot())
    }

    /// Submit an operation without waiting for either queue.
    pub(super) fn send(&self, command: Command) -> bool {
        match self.requests.try_send(Request::Run(command)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Drain one reply, if the worker has one ready.
    pub(super) fn poll(&self) -> Option<Reply> {
        self.replies.try_recv().ok()
    }
}

fn base_endpoint(port: u16) -> String {
    format!("http://127.0.0.1:{port}/_toker/routing")
}

fn worker(base: String, requests: Receiver<Request>, replies: SyncSender<Reply>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let command = Command::snapshot();
            let _ = replies.try_send(Reply::Command(
                command,
                Err(format!("routing worker unavailable ({error})")),
            ));
            return;
        }
    };
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            let command = Command::snapshot();
            let _ = replies.try_send(Reply::Command(
                command,
                Err(format!("routing client unavailable ({error})")),
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
        .header("x-toker-control", command.control);
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
    fn control_requests_are_pinned_to_loopback_and_their_verb() {
        assert_eq!(
            base_endpoint(18123),
            "http://127.0.0.1:18123/_toker/routing"
        );
        assert_eq!(Command::snapshot().control, "routing-snapshot");
        assert_eq!(Command::revisions().path, "revisions");
    }
}
