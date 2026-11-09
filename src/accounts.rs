//! Local sources that can name an observed credential.
//!
//! This module never exposes or retains a token. It reads a bounded local
//! account file, immediately reduces each token to a digest, and returns only
//! digest-to-email identity facts for the store to merge.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::providers::credential_digest;
use crate::store::{AccountIdentity, AccountNameSource, CredentialDigest};

/// Refuse oversized account files rather than loading an unbounded local
/// secret into the service. The local source is small metadata, not a ledger.
const MAX_ACCOUNT_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Deserialize)]
struct LocalAccountsFile {
    #[serde(default)]
    accounts: Vec<LocalAccount>,
}

#[derive(Debug, Deserialize)]
struct LocalAccount {
    email: Option<String>,
    token: String,
}

/// The local multi-account store, under the operator's home.
pub fn local_account_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| {
            std::path::PathBuf::from(home)
                .join(".workhorse")
                .join("claude-accounts.json")
        })
}

/// The identity facts a local multi-account file names. Malformed entries are
/// skipped individually; a missing, unreadable or malformed file answers no
/// identities, never an empty destructive replacement.
pub fn local_account_identities(
    path: &Path,
) -> std::io::Result<BTreeMap<CredentialDigest, AccountIdentity>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() > MAX_ACCOUNT_FILE_BYTES {
        return Ok(BTreeMap::new());
    }
    let bytes = std::fs::read(path)?;
    let Ok(file) = serde_json::from_slice::<LocalAccountsFile>(&bytes) else {
        return Ok(BTreeMap::new());
    };
    Ok(file
        .accounts
        .into_iter()
        .filter_map(|account| {
            let email = account.email?.trim().to_owned();
            (!email.is_empty()).then(|| {
                (
                    credential_digest(&account.token),
                    AccountIdentity {
                        upstream_id: None,
                        email: Some(email),
                        named_by: AccountNameSource::Local,
                    },
                )
            })
        })
        .collect())
}

/// The provider whose accounts Toker can tell apart. The others reach one
/// upstream identity per configured credential, so there is nothing to move.
pub const ACCOUNTED_PROVIDER: &str = "anthropic_sub";

/// The exit status a refusal over unattributed rows leaves. Distinct so the
/// service unit can name it and the supervisor stop retrying: nothing can
/// change until the operator runs setup.
pub const EXIT_ROWS_UNATTRIBUTED: i32 = 79;

/// Refuse to run while the store holds rows from before accounts were
/// recorded (spec: ACCT#moving-a-machine-onto-accounts).
///
/// Every command except setup goes through here, the service included.
/// Refusing is what makes the move safe to interrupt: nothing adds to the
/// history while the move is outstanding, so running setup again repairs it.
///
/// A store that cannot be opened or read is not a refusal. The command is
/// about to fail on its own terms, with a better account of why than this
/// could give.
pub fn refuse_while_rows_are_unattributed() -> anyhow::Result<()> {
    let Ok(path) = crate::config::store_path() else {
        return Ok(());
    };
    let Ok(store) = crate::store::Store::open(&path) else {
        return Ok(());
    };
    let Ok(rows) = store.unattributed_row_count(ACCOUNTED_PROVIDER) else {
        return Ok(());
    };
    if rows == 0 {
        return Ok(());
    }
    eprintln!(
        "toker: {rows} rows were served before toker recorded which account they \
         were on.\n\
         Run `toker setup` to attribute them; every other command waits until then."
    );
    std::process::exit(EXIT_ROWS_UNATTRIBUTED);
}

/// How long a miss waits before the file is read again. A credential the
/// file genuinely does not name would otherwise re-read it on every
/// request that carries it.
const RELOAD_AFTER_MS: i64 = 5_000;

/// The local identity source, re-read when it cannot answer.
///
/// Another program rewrites this file whenever it refreshes a token, and
/// a refreshed credential presents a digest toker has not seen
/// (spec: ACCT#coming-to-know-an-account). Read once at startup, the map
/// would stop answering at the first refresh and every request after it
/// would be attributed to a digest with no account until a restart.
///
/// The read is a bounded local file and happens only on a digest the map
/// cannot answer, which is once per refresh rather than once per request.
/// Token values do not survive it.
pub struct LocalAccounts {
    path: Option<std::path::PathBuf>,
    known: std::sync::RwLock<Known>,
}

#[derive(Default)]
struct Known {
    identities: BTreeMap<CredentialDigest, AccountIdentity>,
    read_at_ms: i64,
}

impl std::fmt::Debug for LocalAccounts {
    /// Deliberately only the count: the addresses are the operator's, and
    /// a debug line is not where they belong.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let known = self.known.read().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("LocalAccounts")
            .field("identities", &known.identities.len())
            .finish()
    }
}

impl LocalAccounts {
    /// Read the source at the operator's own path, where there is one.
    pub fn at_home(now_ms: i64) -> LocalAccounts {
        Self::at(local_account_path(), now_ms)
    }

    pub fn at(path: Option<std::path::PathBuf>, now_ms: i64) -> LocalAccounts {
        let accounts = LocalAccounts {
            path,
            known: std::sync::RwLock::new(Known::default()),
        };
        accounts.reload(now_ms);
        accounts
    }

    /// The identity this digest belongs to. A miss re-reads the file,
    /// at most once every [`RELOAD_AFTER_MS`].
    pub fn get(&self, digest: &CredentialDigest, now_ms: i64) -> Option<AccountIdentity> {
        {
            let known = self.known.read().unwrap_or_else(|e| e.into_inner());
            if let Some(identity) = known.identities.get(digest) {
                return Some(identity.clone());
            }
            if now_ms.saturating_sub(known.read_at_ms) < RELOAD_AFTER_MS {
                return None;
            }
        }
        self.reload(now_ms);
        let known = self.known.read().unwrap_or_else(|e| e.into_inner());
        known.identities.get(digest).cloned()
    }

    fn reload(&self, now_ms: i64) {
        let identities = match &self.path {
            None => BTreeMap::new(),
            Some(path) => match local_account_identities(path) {
                Ok(identities) => identities,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "local account identities unavailable");
                    BTreeMap::new()
                }
            },
        };
        let mut known = self.known.write().unwrap_or_else(|e| e.into_inner());
        // The stamp moves even on a failed read, so a missing file is not
        // re-opened on every request that misses.
        known.read_at_ms = now_ms;
        // A read that came back empty does not erase what was known: the
        // file is rewritten in place by another program, so an empty read
        // is as likely to be a half-written file as an emptied one.
        if !identities.is_empty() {
            known.identities = identities;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_source(path: &Path, entries: &[(&str, &str)]) {
        let accounts: Vec<String> = entries
            .iter()
            .map(|(email, token)| {
                format!(r#"{{"id":"{email}","email":"{email}","token":"{token}"}}"#)
            })
            .collect();
        std::fs::write(
            path,
            format!(r#"{{"active":"one","accounts":[{}]}}"#, accounts.join(",")),
        )
        .expect("write account source");
    }

    #[test]
    fn a_refreshed_credential_is_resolved_without_a_restart() {
        let dir = crate::test_support::tempdir("account-reload-");
        let path = dir.join("claude-accounts.json");
        write_source(&path, &[("one@example.test", "token-one")]);

        let now = 1_000_000;
        let accounts = LocalAccounts::at(Some(path.clone()), now);
        assert_eq!(
            accounts
                .get(&credential_digest("token-one"), now)
                .and_then(|identity| identity.email),
            Some("one@example.test".to_owned())
        );

        // Another program refreshes the token, so the same login now
        // presents a digest the startup read never saw.
        write_source(&path, &[("one@example.test", "token-refreshed")]);
        assert_eq!(
            accounts.get(&credential_digest("token-refreshed"), now),
            None,
            "a miss within the reload interval does not re-read"
        );
        assert_eq!(
            accounts
                .get(&credential_digest("token-refreshed"), now + RELOAD_AFTER_MS)
                .and_then(|identity| identity.email),
            Some("one@example.test".to_owned()),
            "the refreshed credential resolves to the same login"
        );
    }

    #[test]
    fn an_unreadable_source_keeps_what_was_known() {
        let dir = crate::test_support::tempdir("account-half-written-");
        let path = dir.join("claude-accounts.json");
        write_source(&path, &[("one@example.test", "token-one")]);
        let now = 1_000_000;
        let accounts = LocalAccounts::at(Some(path.clone()), now);

        // The file is rewritten in place, so a read can land on a
        // half-written one. That must not erase a login toker already
        // knows the name of.
        std::fs::write(&path, "{\"accounts\":").expect("truncate the source");
        let later = now + RELOAD_AFTER_MS;
        assert_eq!(
            accounts
                .get(&credential_digest("token-one"), later)
                .and_then(|identity| identity.email),
            Some("one@example.test".to_owned())
        );
        assert!(
            !format!("{accounts:?}").contains("example.test"),
            "a debug line is not where the operator's addresses belong"
        );
    }

    #[test]
    fn source_reduces_tokens_to_digest_email_pairs() {
        let dir = crate::test_support::tempdir("account-source-");
        let path = dir.join("claude-accounts.json");
        std::fs::write(
            &path,
            r#"{"active":"one","accounts":[{"id":"one","email":"one@example.test","token":"token-one"},{"id":"two","email":"two@example.test","token":"token-two"}]}"#,
        )
        .expect("write account source");
        let identities = local_account_identities(&path).expect("read source");
        assert_eq!(identities.len(), 2);
        assert_eq!(
            identities
                .get(&credential_digest("token-one"))
                .and_then(|identity| identity.email.as_deref()),
            Some("one@example.test")
        );
        assert!(
            !format!("{identities:?}").contains("token-one"),
            "tokens never survive parsing"
        );
    }

    #[test]
    fn malformed_source_is_no_identity_source() {
        let dir = crate::test_support::tempdir("account-source-malformed-");
        let path = dir.join("claude-accounts.json");
        std::fs::write(&path, "not json").expect("write source");
        assert!(
            local_account_identities(&path)
                .expect("read source")
                .is_empty()
        );
    }
}
