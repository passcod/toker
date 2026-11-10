//! Builders the state tests and the frame tests both draw on, so a frame is
//! asserted against the same snapshot the behaviour is.

use serde_json::{Value, json};

use super::model::State;
use crate::tui::control::{Command, Reply, Response};

/// The configuration snapshot of a machine with five providers, one of them
/// on and with no credential, which is what the page opens on.
pub(super) fn snapshot_body() -> Value {
    json!({
        "providers": [
            {
                "provider": "anthropic_api",
                "enabled": false,
                "upstream": null,
                "credential": {"env": null, "env_set": false, "keyring": false,
                               "literal_set": false}
            },
            {
                "provider": "anthropic_sub",
                "enabled": true,
                "upstream": null,
                "credential": {"env": "TOKER_ANTHROPIC_SUB_TOKEN", "env_set": false,
                               "keyring": false, "literal_set": false}
            },
            {
                "provider": "codex_sub",
                "enabled": false,
                "upstream": "http://gateway.invalid",
                "refresh_url": "http://login.invalid/token",
                "credential": {"env": null, "env_set": false, "keyring": false,
                               "literal_set": false}
            },
            {
                "provider": "openai_api",
                "enabled": false,
                "upstream": null,
                "credential": {"env": null, "env_set": false, "keyring": false,
                               "literal_set": false}
            },
            {
                "provider": "openrouter",
                "enabled": true,
                "upstream": null,
                "credential": {"env": null, "env_set": false, "keyring": true,
                               "literal_set": false}
            }
        ],
        "safeguards": [
            {"binding_id": "anthropic_api.anthropic_messages",
             "provider": "anthropic_api", "declared": "supported", "marked_ms": null},
            {"binding_id": "anthropic_sub.anthropic_messages",
             "provider": "anthropic_sub", "declared": "supported", "marked_ms": null},
            {"binding_id": "openrouter.anthropic_messages",
             "provider": "openrouter", "declared": "unknown", "marked_ms": 1700000000000u64}
        ],
        "settings": {
            "quota_enabled": true,
            "cold_enabled": true,
            "cold_outlook": true,
            "cold_min_tokens": 175000,
            "awake": true,
            "ping_header": "x-toker-ping",
            "notice_default": null,
            "session_headers": ["x-toker-session", "x-claude-code-session-id", "x-session-id"],
            "transcript_roots": []
        },
        "notices": {},
        "largest_context_window": 1000000,
        "db_path": "/home/example/.local/share/toker/toker.db",
        "port": 18123
    })
}

/// The routing snapshot's derived provider facts for the same machine.
pub(super) fn routing_body() -> Value {
    let binding = |id: &str, cost: &str| json!({"binding_id": id, "cost_class": cost});
    json!({
        "derived": {"active": {"providers": [
            {
                "provider": "anthropic_api",
                "bindings": [binding("anthropic_api.anthropic_messages", "billed")],
                "presentations": {"count": 0, "ids": []},
                "rewrites": {"count": 3, "by_protocol":
                    {"anthropic_messages": 2, "openai_chat": 1}},
                "defaults": {"count": 0, "scopes": []},
                "routes": {"count": 1, "ids": ["claude-api"]}
            },
            {
                "provider": "anthropic_sub",
                "bindings": [binding("anthropic_sub.anthropic_messages", "subscription")],
                "presentations": {"count": 0, "ids": []},
                "rewrites": {"count": 4, "by_protocol":
                    {"anthropic_messages": 2, "openai_chat": 1, "openai_responses": 1}},
                "defaults": {"count": 2, "scopes":
                    ["anthropic_messages", "unprefixed/anthropic_messages"]},
                "routes": {"count": 1, "ids": ["claude-sub"]}
            },
            {
                "provider": "codex_sub",
                "bindings": [binding("codex_sub.openai_responses", "subscription")],
                "presentations": {"count": 0, "ids": []},
                "rewrites": {"count": 3, "by_protocol":
                    {"anthropic_messages": 1, "openai_chat": 1, "openai_responses": 1}},
                "defaults": {"count": 0, "scopes": []},
                "routes": {"count": 1, "ids": ["codex"]}
            },
            {
                "provider": "openai_api",
                "bindings": [binding("openai_api.openai_chat", "billed")],
                "presentations": {"count": 0, "ids": []},
                "rewrites": {"count": 1, "by_protocol": {"openai_chat": 1}},
                "defaults": {"count": 0, "scopes": []},
                "routes": {"count": 1, "ids": ["openai-chat"]}
            },
            {
                "provider": "openrouter",
                "bindings": [
                    binding("openrouter.anthropic_messages", "billed"),
                    binding("openrouter.openai_chat", "billed"),
                    binding("openrouter.openai_responses", "billed"),
                ],
                "presentations": {"count": 0, "ids": []},
                "rewrites": {"count": 10, "by_protocol":
                    {"anthropic_messages": 4, "openai_chat": 3, "openai_responses": 3}},
                "defaults": {"count": 4, "scopes": [
                    "anthropic_messages", "unprefixed/anthropic_messages",
                    "openai_chat", "openai_responses"]},
                "routes": {"count": 5, "ids": [
                    "claude-sub", "openrouter-messages", "openrouter-chat",
                    "openrouter-responses", "claude-api"]}
            }
        ]}}
    })
}

pub(super) fn ok(body: Value) -> Result<Response, String> {
    Ok(Response { status: 200, body })
}

/// The page having read the configuration, and not routing.
pub(crate) fn loaded() -> State {
    let mut state = State::default();
    let reply = Reply::Command(Command::snapshot("config-snapshot"), ok(snapshot_body()));
    assert!(state.receive(reply).is_none());
    state
}

/// The page having read both, as it does on arriving.
pub(crate) fn loaded_with_routing() -> State {
    let mut state = loaded();
    state.receive_routing(Reply::Command(
        Command::snapshot("routing-snapshot"),
        ok(routing_body()),
    ));
    state
}
