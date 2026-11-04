//! Bounded, asynchronous access to the daemon's routing control surface.
//!
//! The TUI must keep handling terminal input while the daemon restarts or a
//! control request stalls.  The worker owns the HTTP client; the terminal
//! thread only `try_send`s one refresh and drains replies with `try_recv`.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::Duration;

use serde_json::Value;

const REQUEST_QUEUE: usize = 1;
const REPLY_QUEUE: usize = 2;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
enum Request {
    Snapshot,
}

/// A reply from the routing control endpoint.
#[derive(Debug)]
pub(super) enum Reply {
    Snapshot(Result<Value, String>),
}

/// The terminal-side half of the routing control worker.
///
/// Its queues are deliberately tiny.  A refresh already waiting or a reply the
/// UI has not yet consumed means another would be stale, so it is dropped
/// rather than making key handling wait.
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
            .spawn(move || worker(endpoint(port), request_rx, reply_tx))
            .expect("spawn routing control worker");
        Client { requests, replies }
    }

    /// Ask for the newest snapshot, without waiting for either queue.
    pub(super) fn refresh(&self) {
        match self.requests.try_send(Request::Snapshot) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Drain one reply, if the worker has one ready.
    pub(super) fn poll(&self) -> Option<Reply> {
        self.replies.try_recv().ok()
    }
}

fn endpoint(port: u16) -> String {
    format!("http://127.0.0.1:{port}/_toker/routing/snapshot")
}

fn worker(endpoint: String, requests: Receiver<Request>, replies: SyncSender<Reply>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = replies.try_send(Reply::Snapshot(Err(format!(
                "routing worker unavailable ({error})"
            ))));
            return;
        }
    };
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            let _ = replies.try_send(Reply::Snapshot(Err(format!(
                "routing client unavailable ({error})"
            ))));
            return;
        }
    };
    while let Ok(Request::Snapshot) = requests.recv() {
        let reply = runtime.block_on(snapshot(&client, &endpoint));
        // A later reply is redundant if the UI has not read this one yet.
        let _ = replies.try_send(Reply::Snapshot(reply));
    }
}

async fn snapshot(client: &reqwest::Client, endpoint: &str) -> Result<Value, String> {
    let response = client
        .get(endpoint)
        .header("x-toker-control", "routing-snapshot")
        .send()
        .await
        .map_err(|error| format!("routing service unavailable ({error})"))?;
    if !response.status().is_success() {
        return Err(format!(
            "routing service returned HTTP {}",
            response.status()
        ));
    }
    response
        .json()
        .await
        .map_err(|error| format!("routing snapshot was not JSON ({error})"))
}

#[cfg(test)]
mod tests {
    use super::endpoint;

    #[test]
    fn the_snapshot_request_is_pinned_to_loopback_and_its_control_verb() {
        assert_eq!(
            endpoint(18123),
            "http://127.0.0.1:18123/_toker/routing/snapshot"
        );
    }
}
