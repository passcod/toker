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
