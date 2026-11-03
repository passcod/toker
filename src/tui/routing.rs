//! Read and restore the daemon-owned live routing configuration.
//!
//! The terminal UI does not read or write policy rows directly. The running
//! daemon is the authority because applying a revision also swaps the policy
//! snapshot used by requests already arriving at the listener.

use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
struct SnapshotReply {
    ok: bool,
    active: Option<Active>,
}

#[derive(Debug, Clone, Deserialize)]
struct Active {
    revision: Revision,
    #[serde(default)]
    document: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
struct RevisionsReply {
    ok: bool,
    revisions: Vec<Revision>,
}

#[derive(Debug, Clone, Deserialize)]
struct Revision {
    revision_id: i64,
    parent_revision_id: Option<i64>,
    description: Option<String>,
    source: String,
    #[serde(default)]
    fingerprint: String,
}

#[derive(Debug, Default)]
pub(crate) struct State {
    snapshot: Option<SnapshotReply>,
    revisions: Vec<Revision>,
    selected: usize,
    restore_armed: Option<i64>,
    message: Option<String>,
}

impl State {
    pub(crate) fn refresh(&mut self, port: u16) {
        let message = self.message.take();
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                self.message = Some(format!("routing runtime: {error}"));
                return;
            }
        };
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                self.message = Some(format!("routing client: {error}"));
                return;
            }
        };
        let base = format!("http://127.0.0.1:{port}/_toker/routing");
        let (snapshot, revisions) = runtime.block_on(async {
            tokio::join!(
                async {
                    client
                        .get(format!("{base}/snapshot"))
                        .header("x-toker-control", "routing-snapshot")
                        .send()
                        .await?
                        .error_for_status()?
                        .json::<SnapshotReply>()
                        .await
                },
                async {
                    client
                        .get(format!("{base}/revisions"))
                        .header("x-toker-control", "routing-revisions")
                        .send()
                        .await?
                        .error_for_status()?
                        .json::<RevisionsReply>()
                        .await
                }
            )
        });
        match (snapshot, revisions) {
            (Ok(snapshot), Ok(revisions)) if snapshot.ok && revisions.ok => {
                self.snapshot = Some(snapshot);
                self.revisions = revisions.revisions;
                self.selected = self.selected.min(self.revisions.len().saturating_sub(1));
                self.message = message;
            }
            (Err(error), _) | (_, Err(error)) => self.message = Some(format!("routing: {error}")),
            _ => self.message = Some("routing service rejected its reply".to_owned()),
        }
    }

    pub(crate) fn active_document(&self) -> Option<Vec<u8>> {
        self.snapshot
            .as_ref()?
            .active
            .as_ref()
            .and_then(|active| serde_json::to_vec_pretty(&active.document).ok())
    }

    pub(crate) fn apply_document(&mut self, port: u16, document: &[u8]) {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                self.message = Some(format!("routing runtime: {error}"));
                return;
            }
        };
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                self.message = Some(format!("routing client: {error}"));
                return;
            }
        };
        let response = runtime.block_on(
            client
                .post(format!("http://127.0.0.1:{port}/_toker/routing/apply"))
                .header("x-toker-control", "routing-apply")
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(document.to_owned())
                .send(),
        );
        match response {
            Ok(response) if response.status().is_success() => {
                self.message = Some("applied routing edit".to_owned());
                self.refresh(port);
            }
            Ok(response) => self.message = Some(format!("apply: {}", response.status())),
            Err(error) => self.message = Some(format!("apply: {error}")),
        }
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.revisions.len().saturating_sub(1));
        self.restore_armed = None;
    }

    /// The first press names the irreversible-looking action; the second sends
    /// it. Restore itself is still forward-only in the ledger, but this keeps a
    /// stray key from changing live routing.
    pub(crate) fn restore_selected(&mut self, port: u16) {
        let Some(revision) = self.revisions.get(self.selected) else {
            return;
        };
        if self.restore_armed != Some(revision.revision_id) {
            self.restore_armed = Some(revision.revision_id);
            self.message = Some(format!(
                "press Enter again to restore r{}",
                revision.revision_id
            ));
            return;
        }
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                self.message = Some(format!("routing runtime: {error}"));
                return;
            }
        };
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                self.message = Some(format!("routing client: {error}"));
                return;
            }
        };
        let response = runtime.block_on(
            client
                .post(format!("http://127.0.0.1:{port}/_toker/routing/restore"))
                .header("x-toker-control", "routing-restore")
                .json(&serde_json::json!({"revision_id": revision.revision_id}))
                .send(),
        );
        self.restore_armed = None;
        match response {
            Ok(response) if response.status().is_success() => {
                self.message = Some(format!("restored r{}", revision.revision_id));
                self.refresh(port);
            }
            Ok(response) => self.message = Some(format!("restore: {}", response.status())),
            Err(error) => self.message = Some(format!("restore: {error}")),
        }
    }
}

pub(crate) fn render(frame: &mut Frame, area: Rect, state: &State) {
    let [summary, history] =
        Layout::vertical([Constraint::Length(8), Constraint::Min(4)]).areas(area);
    let block = Block::default().title(" ROUTING ").borders(Borders::ALL);
    let text = match &state.snapshot {
        Some(SnapshotReply {
            active: Some(active),
            ..
        }) => {
            let routes = active
                .document
                .get("routes")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            let presentations = active
                .document
                .get("presentations")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            vec![
                Line::from(vec![
                    Span::styled("Active ", Style::new().add_modifier(Modifier::DIM)),
                    Span::styled(
                        format!("r{}", active.revision.revision_id),
                        Style::new().bold(),
                    ),
                ]),
                Line::from(
                    active
                        .revision
                        .description
                        .as_deref()
                        .unwrap_or("No description")
                        .to_owned(),
                ),
                Line::from(format!(
                    "{routes} routes · {presentations} presented models"
                )),
                Line::from(Span::styled(
                    "Tab dashboard · e edit/apply · ↑↓ history · Enter restore",
                    Style::new().add_modifier(Modifier::DIM),
                )),
            ]
        }
        _ => vec![Line::from("Routing service unavailable")],
    };
    frame.render_widget(Paragraph::new(text).block(block), summary);

    let items = state.revisions.iter().enumerate().map(|(index, revision)| {
        let marker = if index == state.selected { ">" } else { " " };
        let active = state
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.active.as_ref())
            .is_some_and(|active| active.revision.revision_id == revision.revision_id);
        let description = revision.description.as_deref().unwrap_or("No description");
        let parent = revision
            .parent_revision_id
            .map(|parent| format!(" ←r{parent}"))
            .unwrap_or_default();
        let fingerprint = revision.fingerprint.get(..8).unwrap_or("-");
        let label = format!(
            "{marker} r{}{}{}  {}  [{} {fingerprint}]",
            revision.revision_id,
            if active { " active" } else { "" },
            parent,
            description,
            revision.source
        );
        ListItem::new(label)
    });
    frame.render_widget(
        List::new(items)
            .block(Block::default().title(" HISTORY ").borders(Borders::ALL))
            .highlight_style(Style::new().bold()),
        history,
    );
    if let Some(message) = &state.message {
        let area = Rect::new(
            area.x + 2,
            area.bottom().saturating_sub(2),
            area.width.saturating_sub(4),
            1,
        );
        frame.render_widget(
            Paragraph::new(message.as_str()).alignment(Alignment::Center),
            area,
        );
    }
}
