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

#[cfg(test)]
mod tests {
    use super::*;

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
