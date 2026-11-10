//! Gated, local configuration-control endpoints (spec: DCTRL#the-operations).
//!
//! The service is the only writer. Each operation resolves the configuration
//! it would leave behind *before* writing anything, so a value it cannot use
//! is refused with the stored one untouched, and then swaps the resolved
//! result into the running state in the same call: an edit takes effect
//! without a restart and with nothing watching the store for it
//! (spec: DCONF#editing).
//!
//! A credential value reaches toker only through [`credential`], and no
//! operation returns one (spec: SEC#a-credential-is-never-logged-or-displayed).

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use super::{MAX_CONTROL_BODY, control_token_ok, forbidden, json_content_type, store_error};
use crate::config::{Config, is_frontend_name};
use crate::routing::{CapabilityEvidence, DialectId};
use crate::server::Server;
use crate::store::{Configuration, ProviderConfigRow, SettingsConfigRow};

/// The providers toker knows. A row exists for each whether or not it is
/// enabled, so an edit naming anything else is a typo rather than a new
/// provider (spec: DCONF#in-the-store).
pub(crate) const KNOWN_PROVIDERS: &[&str] = &[
    "anthropic_api",
    "anthropic_sub",
    "codex_sub",
    "openai_api",
    "openrouter",
];

fn refused(setting: &str, why: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({
            "toker": "config-edit",
            "ok": false,
            "setting": setting,
            "error": why,
        })),
    )
        .into_response()
}

async fn read_body(request: Request) -> Result<Bytes, Box<Response>> {
    if !json_content_type(request.headers()) {
        return Err(Box::new(forbidden()));
    }
    axum::body::to_bytes(request.into_body(), MAX_CONTROL_BODY)
        .await
        .map_err(|_| Box::new(refused("body", "the request body is too large")))
}

/// `GET /_toker/config/snapshot` — everything the configuration pages show.
///
/// Credentials appear as which sources are set, never as values.
pub(crate) async fn snapshot(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "config-snapshot") || request.method() != Method::GET {
        return forbidden();
    }
    let stored = match server.store.load_configuration() {
        Ok(stored) => stored,
        Err(error) => return store_error(error),
    };
    let config = server.config();
    let marks = match server.store.load_safeguards_marks() {
        Ok(marks) => marks,
        Err(error) => return store_error(error),
    };
    // Each Messages binding's declared support for server safeguards and the
    // mark held against it: capability and temporary evidence, kept apart.
    let safeguards: Vec<_> = server
        .registry
        .bindings()
        .filter(|binding| {
            binding
                .dialect()
                .shares_replay_domain(DialectId::AnthropicMessages)
        })
        .map(|binding| {
            json!({
                "binding_id": binding.id(),
                "provider": binding.provider(),
                "declared": binding
                    .canonical_backend()
                    .map_or(CapabilityEvidence::Unknown, |backend| {
                        backend.capabilities().server_safeguards
                    })
                    .as_str(),
                "marked_ms": marks
                    .iter()
                    .find(|mark| &mark.binding_id == binding.id())
                    .map(|mark| mark.marked_ms),
            })
        })
        .collect();

    let largest_window = match largest_window(&server) {
        Ok(largest) => largest,
        Err(error) => return store_error(error),
    };

    let providers: Vec<_> = KNOWN_PROVIDERS
        .iter()
        .map(|name| {
            let row = stored.providers.iter().find(|row| &row.provider == name);
            let sources = sources_for(&config, name);
            json!({
                "provider": name,
                "enabled": row.is_some_and(|row| row.enabled),
                "upstream": row.and_then(|row| row.upstream.clone()),
                "credential": {
                    "env": row.and_then(|row| row.cred_env.clone()),
                    "env_set": sources.as_ref().is_some_and(|sources| sources.env_set),
                    "keyring": row.and_then(|row| row.cred_keyring).unwrap_or(false),
                    "literal_set": row.is_some_and(|row| row.cred_literal.is_some()),
                },
                "refresh_url": row.and_then(|row| row.refresh_url.clone()),
                "client_version": row.and_then(|row| row.client_version.clone()),
                "version_probe": row.and_then(|row| row.version_probe),
            })
        })
        .collect();

    (
        StatusCode::OK,
        axum::Json(json!({
            "toker": "config-snapshot",
            "ok": true,
            "providers": providers,
            "safeguards": safeguards,
            "settings": settings_body(&stored.settings),
            "notices": stored.notices,
            // What a minimum for the cold gate is checked against: the
            // largest window the enabled providers' models resolve, absent
            // where none does, so a form can refuse as it is typed what a
            // save would
            // (spec: GCOLD#only-a-request-that-could-be-the-re-read).
            "largest_context_window": largest_window,
            // Shown but not editable: settled before the store is opened
            // (spec: DCONF#before-the-store).
            "db_path": config.db_path.display().to_string(),
            "port": config.port,
        })),
    )
        .into_response()
}

/// Which credential sources are actually set for an enabled provider.
/// `None` where the provider is off, or where toker holds no key for it:
/// codex signs with the login it shares with the codex CLI.
fn sources_for(config: &Config, provider: &str) -> Option<crate::config::KeySources> {
    match provider {
        "openrouter" => config.openrouter.as_ref().map(|block| block.key_sources()),
        "openai_api" => config.openai_api.as_ref().map(|block| block.key_sources()),
        "anthropic_api" => config
            .anthropic_api
            .as_ref()
            .map(|block| block.key_sources()),
        "anthropic_sub" => config
            .anthropic_sub
            .as_ref()
            .map(|block| block.oauth_sources()),
        _ => None,
    }
}

fn settings_body(settings: &SettingsConfigRow) -> serde_json::Value {
    json!({
        "session_headers": settings.session_headers,
        "ping_header": settings.ping_header,
        "awake": settings.awake,
        "transcript_roots": settings.transcript_roots,
        "quota_enabled": settings.quota_enabled,
        "cold_enabled": settings.cold_enabled,
        "cold_outlook": settings.cold_outlook,
        "cold_min_tokens": settings.cold_min_tokens,
        "cold_idle_min": settings.cold_idle_min,
        "notice_default": settings.notice_default,
    })
}

/// One provider's row, whole. The caller sends back what the snapshot gave
/// it with its change applied, so there is no "absent means unchanged"
/// ambiguity to resolve. Credentials are deliberately absent: they move
/// only through [`credential`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderEdit {
    provider: String,
    enabled: bool,
    #[serde(default)]
    upstream: Option<String>,
    #[serde(default)]
    refresh_url: Option<String>,
    #[serde(default)]
    client_version: Option<String>,
    #[serde(default)]
    version_probe: Option<bool>,
}

/// The settings row, whole.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsEdit {
    #[serde(default)]
    session_headers: Option<Vec<String>>,
    #[serde(default)]
    ping_header: Option<String>,
    #[serde(default)]
    awake: Option<bool>,
    #[serde(default)]
    transcript_roots: Option<Vec<String>>,
    #[serde(default)]
    quota_enabled: Option<bool>,
    #[serde(default)]
    cold_enabled: Option<bool>,
    #[serde(default)]
    cold_outlook: Option<bool>,
    #[serde(default)]
    cold_min_tokens: Option<u64>,
    #[serde(default)]
    cold_idle_min: Option<f64>,
    #[serde(default)]
    notice_default: Option<String>,
}

/// A profile's notice style; a null style returns it to the default.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoticeEdit {
    profile: String,
    #[serde(default)]
    style: Option<String>,
}

/// Where a provider's credential comes from, and the literal where that is
/// one of the sources. The only operation that carries a credential value.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialEdit {
    provider: String,
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    keyring: Option<bool>,
    /// Absent leaves the stored literal alone; null clears it.
    #[serde(default, deserialize_with = "deserialize_some")]
    literal: Option<Option<String>>,
}

/// Distinguish an absent field from one explicitly null, which plain
/// `Option` cannot: clearing a credential and leaving it alone must not be
/// the same request.
fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Apply `change` to the stored configuration, refusing it whole if what it
/// would leave behind cannot be resolved.
fn commit<T>(
    server: &Server,
    change: impl FnOnce(&mut Configuration) -> Result<T, (String, String)>,
    write: impl FnOnce(&T) -> crate::store::Result<()>,
) -> Response {
    let mut candidate = match server.store.load_configuration() {
        Ok(stored) => stored,
        Err(error) => return store_error(error),
    };
    let minimum_before = candidate.settings.cold_min_tokens;
    let written = match change(&mut candidate) {
        Ok(written) => written,
        Err((setting, why)) => return refused(&setting, &why),
    };
    // Only a minimum this edit sets is checked: one already stored is not
    // the reason an unrelated edit, such as turning a provider off, should
    // be refused.
    if candidate.settings.cold_min_tokens != minimum_before
        && let Some(minimum) = candidate.settings.cold_min_tokens
        && let Err(response) = check_cold_minimum(server, minimum)
    {
        return *response;
    }
    // Resolve before writing: a value toker cannot use is refused with the
    // stored one untouched (spec: DCTRL#the-operations).
    let resolved = match Config::resolve(&candidate) {
        Ok(resolved) => resolved,
        Err(error) => return refused("configuration", &format!("{error:#}")),
    };
    if let Err(error) = write(&written) {
        return store_error(error);
    }
    server.replace_config(resolved);
    (
        StatusCode::OK,
        axum::Json(json!({ "toker": "config-edit", "ok": true })),
    )
        .into_response()
}

/// The largest context window any model the enabled bindings offer resolves
/// to, as RMODEL specifies a window is resolved: the hand-verified
/// catalogue, the provider's fetched listing, a stored declaration.
fn largest_window(server: &Server) -> crate::store::Result<Option<u64>> {
    let catalogs = server
        .catalogs
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    // The inventory holds the offers of configured bindings only, so a
    // provider that is off contributes nothing.
    let inventory = crate::catalog::offers::inventory(&server.registry, &catalogs);
    let declared: std::collections::HashMap<_, _> = server
        .store
        .load_models()?
        .into_iter()
        .filter_map(|entry| Some((entry.model_id, entry.context_window_json?)))
        .collect();
    Ok(inventory
        .iter()
        .filter_map(|offer| {
            let declared = crate::catalog::model_identity(&offer.backend_model_id)
                .and_then(|identity| declared.get(&identity));
            crate::catalog::resolve_context_window(
                &offer.backend_model_id,
                None,
                offer.context_window,
                declared,
                None,
            )
            .tokens()
        })
        .max())
}

/// Refuse a cold-gate minimum no request could reach, since the gate would
/// be switched off without saying so; accept any where no window resolves.
// spec: GCOLD#only-a-request-that-could-be-the-re-read
fn check_cold_minimum(server: &Server, minimum: u64) -> Result<(), Box<Response>> {
    let largest = largest_window(server).map_err(|error| Box::new(store_error(error)))?;
    match largest {
        Some(largest) if minimum > largest => Err(Box::new(refused(
            "cold_min_tokens",
            &format!(
                "{minimum} tokens is larger than every context window the enabled \
                 providers' models have; the largest is {largest} tokens, so the gate \
                 could never fire. Set it to {largest} or less"
            ),
        ))),
        _ => Ok(()),
    }
}

/// `PUT /_toker/config/provider` — one provider's row.
pub(crate) async fn provider(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "config-edit") || request.method() != Method::PUT {
        return forbidden();
    }
    let body = match read_body(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let edit: ProviderEdit = match serde_json::from_slice(&body) {
        Ok(edit) => edit,
        Err(error) => return refused("provider", &error.to_string()),
    };

    commit(
        &server,
        |candidate| {
            if !KNOWN_PROVIDERS.contains(&edit.provider.as_str()) {
                return Err((
                    "provider".to_owned(),
                    format!("{} is not a provider toker knows", edit.provider),
                ));
            }
            let existing = candidate
                .providers
                .iter()
                .find(|row| row.provider == edit.provider)
                .cloned()
                .unwrap_or_else(|| ProviderConfigRow {
                    provider: edit.provider.clone(),
                    ..ProviderConfigRow::default()
                });
            // The credential stays exactly as it was: it moves only through
            // the credential operation.
            let row = ProviderConfigRow {
                enabled: edit.enabled,
                upstream: edit.upstream.clone(),
                refresh_url: edit.refresh_url.clone(),
                client_version: edit.client_version.clone(),
                version_probe: edit.version_probe,
                ..existing
            };
            candidate
                .providers
                .retain(|row| row.provider != edit.provider);
            candidate.providers.push(row.clone());
            candidate
                .providers
                .sort_by(|a, b| a.provider.cmp(&b.provider));
            Ok(row)
        },
        |row| server.store.put_config_provider(row),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SafeguardsMarkClear {
    binding_id: crate::policy::BindingId,
}

/// `POST /_toker/safeguards/mark/clear` — forget that a binding refused the
/// server safeguards ask, so the next ask probes it again.
pub(crate) async fn clear_safeguards_mark(
    State(server): State<Server>,
    request: Request,
) -> Response {
    if !control_token_ok(request.headers(), "safeguards-mark-clear")
        || request.method() != Method::POST
    {
        return forbidden();
    }
    let body = match read_body(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let clear: SafeguardsMarkClear = match serde_json::from_slice(&body) {
        Ok(clear) => clear,
        Err(_) => return refused("safeguards", "the request does not parse"),
    };
    let cleared = server.clear_safeguards_marks([&clear.binding_id]);
    (
        StatusCode::OK,
        axum::Json(json!({
            "toker": "safeguards-mark-clear",
            "ok": true,
            "cleared": cleared > 0,
        })),
    )
        .into_response()
}

/// `PUT /_toker/config/settings` — the settings row.
pub(crate) async fn settings(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "config-edit") || request.method() != Method::PUT {
        return forbidden();
    }
    let body = match read_body(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let edit: SettingsEdit = match serde_json::from_slice(&body) {
        Ok(edit) => edit,
        Err(error) => return refused("settings", &error.to_string()),
    };

    let row = SettingsConfigRow {
        session_headers: edit.session_headers,
        ping_header: edit.ping_header,
        awake: edit.awake,
        transcript_roots: edit.transcript_roots,
        quota_enabled: edit.quota_enabled,
        cold_enabled: edit.cold_enabled,
        cold_outlook: edit.cold_outlook,
        cold_min_tokens: edit.cold_min_tokens,
        cold_idle_min: edit.cold_idle_min,
        notice_default: edit.notice_default,
    };
    let stored = row.clone();
    commit(
        &server,
        move |candidate| {
            candidate.settings = row;
            Ok(())
        },
        |()| server.store.put_config_settings(&stored),
    )
}

/// `PUT /_toker/config/notice` — one profile's notice style.
pub(crate) async fn notice(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "config-edit") || request.method() != Method::PUT {
        return forbidden();
    }
    let body = match read_body(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let edit: NoticeEdit = match serde_json::from_slice(&body) {
        Ok(edit) => edit,
        Err(error) => return refused("notice", &error.to_string()),
    };

    let (profile, style) = (edit.profile.clone(), edit.style.clone());
    commit(
        &server,
        |candidate| {
            // The router accepts exactly these, so a style named outside
            // them could never be read (spec: FPROF).
            if !is_frontend_name(&edit.profile) {
                return Err((
                    "notice".to_owned(),
                    format!(
                        "{} is not a profile name a request could carry",
                        edit.profile
                    ),
                ));
            }
            match &edit.style {
                Some(style) => {
                    candidate
                        .notices
                        .insert(edit.profile.clone(), style.clone());
                }
                None => {
                    candidate.notices.remove(&edit.profile);
                }
            }
            Ok(())
        },
        |()| match &style {
            Some(style) => server.store.put_config_notice(&profile, style),
            None => server.store.delete_config_notice(&profile),
        },
    )
}

/// `PUT /_toker/config/credential` — where a provider's credential comes
/// from, and the literal where that is one of the sources.
pub(crate) async fn credential(State(server): State<Server>, request: Request) -> Response {
    if !control_token_ok(request.headers(), "config-credential") || request.method() != Method::PUT
    {
        return forbidden();
    }
    let body = match read_body(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let edit: CredentialEdit = match serde_json::from_slice(&body) {
        Ok(edit) => edit,
        // The body may carry a credential, so the parser's message, which
        // can quote what it was reading, must not reach the reply.
        Err(_) => return refused("credential", "the credential request does not parse"),
    };

    let response = commit(
        &server,
        |candidate| {
            if !KNOWN_PROVIDERS.contains(&edit.provider.as_str()) {
                return Err((
                    "credential".to_owned(),
                    format!("{} is not a provider toker knows", edit.provider),
                ));
            }
            let existing = candidate
                .providers
                .iter()
                .find(|row| row.provider == edit.provider)
                .cloned()
                .unwrap_or_else(|| ProviderConfigRow {
                    provider: edit.provider.clone(),
                    ..ProviderConfigRow::default()
                });
            let row = ProviderConfigRow {
                cred_env: edit.env.clone(),
                cred_keyring: edit.keyring,
                cred_literal: match &edit.literal {
                    Some(literal) => literal.clone(),
                    None => existing.cred_literal.clone(),
                },
                ..existing
            };
            candidate
                .providers
                .retain(|row| row.provider != edit.provider);
            candidate.providers.push(row.clone());
            candidate
                .providers
                .sort_by(|a, b| a.provider.cmp(&b.provider));
            Ok(row)
        },
        |row| server.store.put_config_provider(row),
    );
    // A new credential may be enrolled where the old one was not, so the
    // provider's refused asks are worth trying again (spec: WSAFE#a-rejection-mark).
    if response.status() == StatusCode::OK {
        server.clear_safeguards_marks_for_provider(&edit.provider);
    }
    response
}
