//! Configuration rows: everything that was once `toker.toml`, held beside
//! the ledger and the policy revisions (spec: DCONF#in-the-store).
//!
//! These rows are deliberately dumb. A nullable column is "not set", which
//! resolves to the built-in default at read time in [`crate::config`], so a
//! default that moves moves for every machine that never set it. Freezing
//! today's default into a row at write time would silently pin it.
//!
//! A fresh machine has no rows at all: every default, every provider off.

use std::collections::BTreeMap;

use rusqlite::Connection;

use super::{Error, Result, row_of, rows_of};

/// One provider's configuration. Present whether or not the provider is
/// enabled, so switching it off keeps the endpoint and credential source it
/// was configured with (spec: DCONF#in-the-store).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderConfigRow {
    pub provider: String,
    pub enabled: bool,
    /// The endpoint; `None` is the provider's built-in one.
    pub upstream: Option<String>,
    /// The environment variable the credential may come from; `None` is the
    /// provider's built-in variable name.
    pub cred_env: Option<String>,
    /// Whether to look in the keyring.
    pub cred_keyring: Option<bool>,
    /// The credential itself, where the operator gave one directly.
    pub cred_literal: Option<String>,
    /// Codex only: the OAuth refresh endpoint.
    pub refresh_url: Option<String>,
    /// Codex only: the pinned client version, `None` resolving as the
    /// provider's own rules describe.
    pub client_version: Option<String>,
    /// Codex only: whether to probe for the ecosystem's latest release.
    pub version_probe: Option<bool>,
}

/// Everything that is neither per-provider nor per-profile. Every field is
/// "not set" by default, which is every built-in default.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SettingsConfigRow {
    pub session_headers: Option<Vec<String>>,
    pub ping_header: Option<String>,
    pub awake: Option<bool>,
    pub transcript_roots: Option<Vec<String>>,
    pub quota_enabled: Option<bool>,
    pub cold_enabled: Option<bool>,
    pub cold_outlook: Option<bool>,
    pub cold_min_tokens: Option<u64>,
    pub cold_idle_min: Option<f64>,
    pub notice_default: Option<String>,
}

/// Configuration whole, as the store holds it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Configuration {
    pub providers: Vec<ProviderConfigRow>,
    pub settings: SettingsConfigRow,
    /// Per-profile notice styles, by profile name.
    pub notices: BTreeMap<String, String>,
}

fn json_list(text: Option<String>) -> Result<Option<Vec<String>>> {
    match text {
        None => Ok(None),
        Some(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| Error::InvalidRoutingState(format!("configuration list: {error}"))),
    }
}

fn list_json(list: Option<&Vec<String>>) -> Result<Option<String>> {
    match list {
        None => Ok(None),
        Some(list) => serde_json::to_string(list)
            .map(Some)
            .map_err(|error| Error::InvalidRoutingState(format!("configuration list: {error}"))),
    }
}

pub(super) fn load_configuration(conn: &Connection) -> Result<Configuration> {
    let providers = rows_of(
        conn,
        "SELECT provider, enabled, upstream, cred_env, cred_keyring, cred_literal,
                refresh_url, client_version, version_probe
           FROM config_providers ORDER BY provider",
        [],
        |row| {
            Ok(ProviderConfigRow {
                provider: row.get(0)?,
                enabled: row.get::<_, i64>(1)? != 0,
                upstream: row.get(2)?,
                cred_env: row.get(3)?,
                cred_keyring: row.get::<_, Option<i64>>(4)?.map(|flag| flag != 0),
                cred_literal: row.get(5)?,
                refresh_url: row.get(6)?,
                client_version: row.get(7)?,
                version_probe: row.get::<_, Option<i64>>(8)?.map(|flag| flag != 0),
            })
        },
    )?;

    let settings = row_of(
        conn,
        "SELECT session_headers, ping_header, awake, transcript_roots, quota_enabled,
                cold_enabled, cold_outlook, cold_min_tokens, cold_idle_min, notice_default
           FROM config_settings WHERE id = 1",
        [],
        |row| {
            Ok(SettingsConfigRow {
                session_headers: json_list(row.get(0)?)?,
                ping_header: row.get(1)?,
                awake: row.get::<_, Option<i64>>(2)?.map(|flag| flag != 0),
                transcript_roots: json_list(row.get(3)?)?,
                quota_enabled: row.get::<_, Option<i64>>(4)?.map(|flag| flag != 0),
                cold_enabled: row.get::<_, Option<i64>>(5)?.map(|flag| flag != 0),
                cold_outlook: row.get::<_, Option<i64>>(6)?.map(|flag| flag != 0),
                cold_min_tokens: row.get::<_, Option<i64>>(7)?.map(|count| count as u64),
                cold_idle_min: row.get(8)?,
                notice_default: row.get(9)?,
            })
        },
    )?
    .unwrap_or_default();

    let notices = rows_of(
        conn,
        "SELECT profile, style FROM config_notices ORDER BY profile",
        [],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?
    .into_iter()
    .collect();

    Ok(Configuration {
        providers,
        settings,
        notices,
    })
}

/// Write one provider's row, replacing any already there.
pub(super) fn put_provider(conn: &Connection, row: &ProviderConfigRow) -> Result<()> {
    conn.execute(
        "INSERT INTO config_providers
             (provider, enabled, upstream, cred_env, cred_keyring, cred_literal,
              refresh_url, client_version, version_probe)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(provider) DO UPDATE SET
             enabled = excluded.enabled, upstream = excluded.upstream,
             cred_env = excluded.cred_env, cred_keyring = excluded.cred_keyring,
             cred_literal = excluded.cred_literal, refresh_url = excluded.refresh_url,
             client_version = excluded.client_version,
             version_probe = excluded.version_probe",
        rusqlite::params![
            row.provider,
            row.enabled as i64,
            row.upstream,
            row.cred_env,
            row.cred_keyring.map(|flag| flag as i64),
            row.cred_literal,
            row.refresh_url,
            row.client_version,
            row.version_probe.map(|flag| flag as i64),
        ],
    )?;
    Ok(())
}

/// Write the settings row, replacing any already there.
pub(super) fn put_settings(conn: &Connection, row: &SettingsConfigRow) -> Result<()> {
    conn.execute(
        "INSERT INTO config_settings
             (id, session_headers, ping_header, awake, transcript_roots, quota_enabled,
              cold_enabled, cold_outlook, cold_min_tokens, cold_idle_min, notice_default)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET
             session_headers = excluded.session_headers, ping_header = excluded.ping_header,
             awake = excluded.awake, transcript_roots = excluded.transcript_roots,
             quota_enabled = excluded.quota_enabled, cold_enabled = excluded.cold_enabled,
             cold_outlook = excluded.cold_outlook, cold_min_tokens = excluded.cold_min_tokens,
             cold_idle_min = excluded.cold_idle_min, notice_default = excluded.notice_default",
        rusqlite::params![
            list_json(row.session_headers.as_ref())?,
            row.ping_header,
            row.awake.map(|flag| flag as i64),
            list_json(row.transcript_roots.as_ref())?,
            row.quota_enabled.map(|flag| flag as i64),
            row.cold_enabled.map(|flag| flag as i64),
            row.cold_outlook.map(|flag| flag as i64),
            row.cold_min_tokens.map(|count| count as i64),
            row.cold_idle_min,
            row.notice_default,
        ],
    )?;
    Ok(())
}

/// Set one profile's notice style.
pub(super) fn put_notice(conn: &Connection, profile: &str, style: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO config_notices (profile, style) VALUES (?1, ?2)
         ON CONFLICT(profile) DO UPDATE SET style = excluded.style",
        rusqlite::params![profile, style],
    )?;
    Ok(())
}

/// Drop one profile's notice style, returning it to the default.
pub(super) fn delete_notice(conn: &Connection, profile: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM config_notices WHERE profile = ?1",
        rusqlite::params![profile],
    )?;
    Ok(())
}

/// Write a whole configuration. Used by the import, inside the transaction
/// that also activates revision one, so a machine moving off its file does
/// both or neither (spec: RMIG#migrating-the-file-and-the-store-together).
///
/// Replaces rather than merges: the file is the whole of what the machine
/// was configured with, so a row it does not mention is a row that should
/// not be there.
pub(super) fn write_configuration(conn: &Connection, config: &Configuration) -> Result<()> {
    conn.execute("DELETE FROM config_providers", [])?;
    conn.execute("DELETE FROM config_notices", [])?;
    for provider in &config.providers {
        put_provider(conn, provider)?;
    }
    put_settings(conn, &config.settings)?;
    for (profile, style) in &config.notices {
        put_notice(conn, profile, style)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::Store;
    use super::*;

    fn store() -> Store {
        Store::open(":memory:").expect("open in-memory store")
    }

    fn provider(name: &str) -> ProviderConfigRow {
        ProviderConfigRow {
            provider: name.to_owned(),
            enabled: true,
            upstream: Some("https://example.invalid/v1".to_owned()),
            cred_env: Some("EXAMPLE_KEY".to_owned()),
            cred_keyring: Some(true),
            cred_literal: Some("sk-not-a-real-key".to_owned()),
            ..ProviderConfigRow::default()
        }
    }

    #[test]
    fn a_fresh_store_is_every_default_and_no_provider() {
        let configuration = store().load_configuration().expect("load");
        assert_eq!(configuration, Configuration::default());
        assert!(configuration.providers.is_empty());
        // Not "all the defaults written out": nothing is set, so nothing
        // pins today's default.
        assert_eq!(configuration.settings.cold_min_tokens, None);
        assert_eq!(configuration.settings.awake, None);
    }

    #[test]
    fn a_provider_row_round_trips() {
        let store = store();
        store
            .put_config_provider(&provider("openrouter"))
            .expect("put");
        let loaded = store.load_configuration().expect("load");
        assert_eq!(loaded.providers, vec![provider("openrouter")]);
    }

    #[test]
    fn disabling_a_provider_keeps_its_endpoint_and_credential_source() {
        let store = store();
        store
            .put_config_provider(&provider("openrouter"))
            .expect("put");
        let off = ProviderConfigRow {
            enabled: false,
            ..provider("openrouter")
        };
        store.put_config_provider(&off).expect("put off");

        let loaded = store.load_configuration().expect("load");
        let row = &loaded.providers[0];
        assert!(!row.enabled);
        assert_eq!(row.upstream.as_deref(), Some("https://example.invalid/v1"));
        assert_eq!(row.cred_env.as_deref(), Some("EXAMPLE_KEY"));
        assert_eq!(row.cred_keyring, Some(true));
    }

    #[test]
    fn settings_round_trip_including_lists_and_fractions() {
        let store = store();
        let settings = SettingsConfigRow {
            session_headers: Some(vec!["x-one".to_owned(), "x-two".to_owned()]),
            ping_header: Some("x-ping".to_owned()),
            awake: Some(false),
            transcript_roots: Some(vec!["~/elsewhere".to_owned()]),
            quota_enabled: Some(false),
            cold_enabled: Some(true),
            cold_outlook: Some(false),
            cold_min_tokens: Some(123_456),
            cold_idle_min: Some(12.5),
            notice_default: Some("plain".to_owned()),
        };
        store.put_config_settings(&settings).expect("put");
        assert_eq!(store.load_configuration().expect("load").settings, settings);
    }

    #[test]
    fn a_notice_style_is_set_and_cleared_per_profile() {
        let store = store();
        store.put_config_notice("claude", "block").expect("put");
        store.put_config_notice("workhorse", "toker").expect("put");
        let loaded = store.load_configuration().expect("load");
        assert_eq!(
            loaded.notices.get("claude").map(String::as_str),
            Some("block")
        );
        assert_eq!(loaded.notices.len(), 2);

        store.delete_config_notice("claude").expect("delete");
        let loaded = store.load_configuration().expect("load");
        assert!(!loaded.notices.contains_key("claude"));
        assert_eq!(loaded.notices.len(), 1);
    }

    #[test]
    fn a_profile_name_the_router_could_never_match_is_refused() {
        let store = store();
        // The router accepts lowercase, digits, '-' and '_' and nothing
        // else (spec: FPROF), so a row outside that could never be read.
        for bad in ["Claude", "with space", "dots.here", ""] {
            assert!(
                store.put_config_notice(bad, "gfm").is_err(),
                "{bad:?} should be refused"
            );
        }
        assert!(store.put_config_notice("ok-1_x", "gfm").is_ok());
    }

    #[test]
    fn an_unknown_notice_style_is_refused() {
        assert!(store().put_config_notice("claude", "sparkle").is_err());
    }

    #[test]
    fn writing_a_configuration_replaces_rather_than_merges() {
        let store = store();
        store
            .put_config_provider(&provider("openrouter"))
            .expect("put");
        store.put_config_notice("claude", "block").expect("put");

        let replacement = Configuration {
            providers: vec![provider("codex_sub")],
            settings: SettingsConfigRow {
                ping_header: Some("x-ping".to_owned()),
                ..SettingsConfigRow::default()
            },
            notices: [("workhorse".to_owned(), "toker".to_owned())]
                .into_iter()
                .collect(),
        };
        {
            let conn = store.conn().expect("conn");
            write_configuration(&conn, &replacement).expect("write");
        }

        // The file is the whole of what the machine was configured with, so
        // a provider it does not name should not survive the import.
        let loaded = store.load_configuration().expect("load");
        assert_eq!(loaded, replacement);
    }
}
