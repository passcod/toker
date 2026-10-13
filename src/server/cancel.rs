//! Precommit client-cancellation accounting.
//!
//! An inference handler can be dropped after an actual upstream send but before
//! it returns a frontend response. The future then cannot return a cancellation
//! outcome, so an armed guard schedules the attempt's terminal row from `Drop`.
//! SQLite work runs on Tokio's blocking pool, never in the dropping async task.

use axum::http::StatusCode;

use super::record::{RecordCtx, record_error};
use super::record_anthropic::{AnthropicRecordCtx, record_anthropic_error, record_codex_error};

enum CancellationRecord {
    Chat(RecordCtx),
    Anthropic(AnthropicRecordCtx),
    Responses {
        ctx: AnthropicRecordCtx,
        frontend_protocol: &'static str,
    },
}

pub(crate) struct PrecommitCancellationGuard {
    record: Option<CancellationRecord>,
}

impl PrecommitCancellationGuard {
    pub(crate) fn chat(record: Option<&RecordCtx>) -> Self {
        Self {
            record: record.cloned().map(CancellationRecord::Chat),
        }
    }

    pub(crate) fn anthropic(record: Option<&AnthropicRecordCtx>) -> Self {
        Self {
            record: record.cloned().map(CancellationRecord::Anthropic),
        }
    }

    pub(crate) fn responses(
        record: Option<&AnthropicRecordCtx>,
        frontend_protocol: &'static str,
    ) -> Self {
        Self {
            record: record.cloned().map(|ctx| CancellationRecord::Responses {
                ctx,
                frontend_protocol,
            }),
        }
    }

    pub(crate) fn update_anthropic(&mut self, record: Option<&AnthropicRecordCtx>) {
        self.record = record.cloned().map(CancellationRecord::Anthropic);
    }

    pub(crate) fn disarm(&mut self) {
        self.record = None;
    }
}

impl Drop for PrecommitCancellationGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::error!("precommit cancellation row lost outside a Tokio runtime");
            return;
        };
        handle.spawn_blocking(move || match record {
            CancellationRecord::Chat(ctx) => record_error(
                &ctx,
                StatusCode::BAD_GATEWAY.as_u16(),
                Some("client_cancelled".to_owned()),
                None,
            ),
            CancellationRecord::Anthropic(ctx) => record_anthropic_error(
                &ctx,
                StatusCode::BAD_GATEWAY.as_u16(),
                Some("client_cancelled".to_owned()),
                None,
                None,
                None,
            ),
            CancellationRecord::Responses {
                ctx,
                frontend_protocol,
            } => record_codex_error(
                &ctx,
                StatusCode::BAD_GATEWAY.as_u16(),
                "client_cancelled",
                "client disconnected before response commitment",
                None,
                frontend_protocol,
            ),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use crate::config::{Config, OpenRouterConfig};
    use crate::middleware::awake::ProcessSpawner;
    use crate::policy::BindingId;
    use crate::providers::{OpenRouter, Provider};
    use crate::server::Server;
    use crate::store::{AttemptReason, RowKind, Store};

    use super::*;

    #[tokio::test]
    async fn an_armed_guard_records_cancellation_off_the_dropping_task() {
        let dir = crate::test_support::tempdir("cancel-guard-");
        let path = dir.join("toker.toml");
        std::fs::write(&path, "[providers.openrouter]\n").expect("config");
        let mut config = Config::load_from(&path).expect("load config");
        config.db_path = ":memory:".into();
        config.openrouter = Some(OpenRouterConfig::default());
        config.default_backend_openai_chat = Some("openrouter".to_owned());
        let store = Arc::new(Store::open(&config.db_path).expect("store"));
        let server = Server::with_seams(
            config,
            store.clone(),
            Box::new(ProcessSpawner),
            Arc::new(crate::secrets::MemoryStore::new()),
        )
        .expect("server");
        let backend: Arc<dyn Provider> = Arc::new(OpenRouter::new(
            "http://127.0.0.1:9/v1".parse().expect("url"),
            None,
        ));
        let ctx = RecordCtx {
            server,
            backend,
            binding_id: BindingId::new("openrouter.openrouter_chat_completions").expect("binding"),
            attempt: Some(crate::server::attempt::initial_provenance()),
            policy_routing: None,
            started: Instant::now(),
            session_id: None,
            ping: false,
            requested_model: Some("example/model".to_owned()),
            effective_model: Some("example/model".to_owned()),
            shape: None,
            system_messages: None,
            frontend: None,
        };

        drop(PrecommitCancellationGuard::chat(Some(&ctx)));
        for _ in 0..100 {
            let rows = store.requests_since(0, 10).expect("rows");
            if let Some(row) = rows.first() {
                assert_eq!(row.kind, Some(RowKind::Error));
                assert_eq!(row.error_type.as_deref(), Some("client_cancelled"));
                assert_eq!(
                    row.attempt.as_ref().map(|attempt| attempt.reason),
                    Some(AttemptReason::Initial)
                );
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("cancellation row did not land");
    }
}
