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
    use crate::policy::{
        BindingId, CandidateId, LaneState, LaneTransitionDecision, PinIdentity, RouteFingerprint,
        RouteId, TransitionCause, decide_transition,
    };
    use crate::providers::{OpenRouter, Provider};
    use crate::server::Server;
    use crate::store::{AttemptReason, RowKind, Store};

    use super::*;

    fn chat_record(
        prefix: &str,
        session_id: Option<&str>,
        pin_transition: Option<LaneTransitionDecision>,
    ) -> (Arc<Store>, RecordCtx) {
        let dir = crate::test_support::tempdir(prefix);
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
        let shape = session_id.map(|_| crate::ir::openai_chat::Shape {
            req_bytes: 1,
            req_messages: Some(1),
            req_tools: 0,
            tools_hash: "tools".to_owned(),
            system_chars: 0,
            system_hash: String::new(),
            system_blocks: vec![],
            summarising: false,
        });
        let ctx = RecordCtx {
            server,
            backend,
            binding_id: BindingId::new("openrouter.openrouter_chat_completions").expect("binding"),
            attempt: Some(crate::server::attempt::initial_provenance()),
            policy_routing: None,
            pin_transition,
            started: Instant::now(),
            session_id: session_id.map(str::to_owned),
            ping: false,
            requested_model: Some("example/model".to_owned()),
            effective_model: Some("example/model".to_owned()),
            shape,
            system_messages: None,
            frontend: None,
        };
        (store, ctx)
    }

    #[tokio::test]
    async fn an_armed_guard_records_cancellation_off_the_dropping_task() {
        let (store, ctx) = chat_record("cancel-guard-", None, None);

        drop(PrecommitCancellationGuard::chat(Some(&ctx)));
        for _ in 0..200 {
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
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("cancellation row did not land");
    }

    #[tokio::test]
    async fn cancellation_records_the_attempt_without_pinning_the_lane() {
        let pin = PinIdentity {
            policy_revision_id: 1,
            policy_route_id: RouteId::new("route").expect("route"),
            route_fingerprint: "11"
                .repeat(32)
                .parse::<RouteFingerprint>()
                .expect("fingerprint"),
            candidate_id: CandidateId::new("candidate").expect("candidate"),
            binding_id: BindingId::new("openrouter.openrouter_chat_completions").expect("binding"),
            baseline_model: "example/model".to_owned(),
            effective_model: "example/model".to_owned(),
        };
        let pin_transition = decide_transition(
            &LaneState::Absent,
            pin,
            TransitionCause::Activation {
                scope: crate::policy::ActivationScope::NewLanes,
                forced_cache_transition: false,
            },
        );
        let (store, ctx) = chat_record(
            "cancel-pin-",
            Some("cancelled-session"),
            Some(pin_transition),
        );

        drop(PrecommitCancellationGuard::chat(Some(&ctx)));
        for _ in 0..200 {
            if !store.requests_since(0, 10).expect("rows").is_empty() {
                assert!(
                    store.load_lanes().expect("lanes").is_empty(),
                    "client cancellation cannot select the lane pin"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("cancellation row did not land");
    }
}
