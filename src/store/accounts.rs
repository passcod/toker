//! Learned account and credential-digest state.
//!
//! Credential values are transient request data. The store holds only their
//! SHA-256 digests, which may be unresolved while traffic is served and can be
//! retroactively attached to an account when an identity source answers.

use rusqlite::{Connection, OptionalExtension};

use super::{Error, Result, rows_of};

/// A SHA-256 credential digest in its lower-case hexadecimal storage form.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CredentialDigest(String);

impl CredentialDigest {
    pub fn new(value: impl Into<String>) -> std::result::Result<Self, String> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err("credential digest must be exactly 64 lowercase hexadecimal characters".into())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One account learned from a provider credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub number: i64,
    pub provider: String,
    pub upstream_id: Option<String>,
    pub email: Option<String>,
    pub named_by: AccountNameSource,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

/// The source that named an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountNameSource {
    Local,
    Upstream,
    Operator,
}

impl AccountNameSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Upstream => "upstream",
            Self::Operator => "operator",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "local" => Some(Self::Local),
            "upstream" => Some(Self::Upstream),
            "operator" => Some(Self::Operator),
            _ => None,
        }
    }
}

/// An account identity supplied by discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountIdentity {
    pub upstream_id: Option<String>,
    pub email: Option<String>,
    pub named_by: AccountNameSource,
}

impl AccountIdentity {
    fn validate(&self) -> Result<()> {
        if self.upstream_id.as_deref().is_none_or(str::is_empty)
            && self.email.as_deref().is_none_or(str::is_empty)
        {
            return Err(Error::InvalidAccountState(
                "an account identity needs an upstream id or email".into(),
            ));
        }
        Ok(())
    }
}

/// One credential observed on the machine, including a digest discovery has
/// not yet resolved to an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialAccount {
    pub digest: CredentialDigest,
    pub provider: String,
    pub account_number: Option<i64>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

pub(super) fn observe(
    conn: &mut Connection,
    provider: &str,
    digest: &CredentialDigest,
    at_ms: i64,
    identity: Option<&AccountIdentity>,
) -> Result<Option<i64>> {
    let tx = conn.transaction()?;
    let current = tx
        .query_row(
            "SELECT provider, account_number FROM credential_accounts WHERE digest = ?1",
            [digest.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .optional()?;
    let mut account_number = match current {
        Some((known_provider, number)) => {
            if known_provider != provider {
                return Err(Error::InvalidAccountState(format!(
                    "credential digest belongs to {known_provider}, not {provider}"
                )));
            }
            tx.execute(
                "UPDATE credential_accounts SET last_seen_ms = ?2 WHERE digest = ?1",
                (digest.as_str(), at_ms),
            )?;
            number
        }
        None => {
            tx.execute(
                "INSERT INTO credential_accounts
                    (digest, provider, account_number, first_seen_ms, last_seen_ms)
                 VALUES (?1, ?2, NULL, ?3, ?3)",
                (digest.as_str(), provider, at_ms),
            )?;
            None
        }
    };

    if let Some(identity) = identity {
        identity.validate()?;
        let number = account_for_identity(&tx, provider, identity, at_ms)?;
        tx.execute(
            "UPDATE credential_accounts
             SET account_number = ?2, last_seen_ms = ?3
             WHERE digest = ?1",
            (digest.as_str(), number, at_ms),
        )?;
        tx.execute(
            "UPDATE requests SET account_number = ?3
             WHERE provider = ?1 AND credential_digest = ?2 AND account_number IS NULL",
            (provider, digest.as_str(), number),
        )?;
        account_number = Some(number);
    }
    tx.commit()?;
    Ok(account_number)
}

fn account_for_identity(
    conn: &Connection,
    provider: &str,
    identity: &AccountIdentity,
    at_ms: i64,
) -> Result<i64> {
    let by_upstream = match identity.upstream_id.as_deref() {
        Some(id) => conn
            .query_row(
                "SELECT number FROM accounts WHERE provider = ?1 AND upstream_id = ?2",
                (provider, id),
                |row| row.get::<_, i64>(0),
            )
            .optional()?,
        None => None,
    };
    let by_email = match identity.email.as_deref() {
        Some(email) => conn
            .query_row(
                "SELECT number FROM accounts WHERE provider = ?1 AND email = ?2",
                (provider, email),
                |row| row.get::<_, i64>(0),
            )
            .optional()?,
        None => None,
    };
    if let (Some(left), Some(right)) = (by_upstream, by_email)
        && left != right
    {
        return Err(Error::InvalidAccountState(
            "upstream id and email name different accounts".into(),
        ));
    }
    if let Some(number) = by_upstream.or(by_email) {
        conn.execute(
            "UPDATE accounts
             SET upstream_id = COALESCE(?2, upstream_id),
                 email = COALESCE(?3, email),
                 last_seen_ms = MAX(last_seen_ms, ?4)
             WHERE number = ?1",
            (
                number,
                identity.upstream_id.as_deref(),
                identity.email.as_deref(),
                at_ms,
            ),
        )?;
        return Ok(number);
    }
    conn.execute(
        "INSERT INTO accounts
            (provider, upstream_id, email, named_by, first_seen_ms, last_seen_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        (
            provider,
            identity.upstream_id.as_deref(),
            identity.email.as_deref(),
            identity.named_by.as_str(),
            at_ms,
        ),
    )?;
    Ok(conn.last_insert_rowid())
}

pub(super) fn load_accounts(conn: &Connection) -> Result<Vec<Account>> {
    rows_of(
        conn,
        "SELECT number, provider, upstream_id, email, named_by, first_seen_ms, last_seen_ms
         FROM accounts ORDER BY number",
        [],
        |row| {
            let named_by: String = row.get("named_by")?;
            Ok(Account {
                number: row.get("number")?,
                provider: row.get("provider")?,
                upstream_id: row.get("upstream_id")?,
                email: row.get("email")?,
                named_by: AccountNameSource::parse(&named_by).ok_or(Error::UnknownDbValue {
                    column: "accounts.named_by",
                    value: named_by,
                })?,
                first_seen_ms: row.get("first_seen_ms")?,
                last_seen_ms: row.get("last_seen_ms")?,
            })
        },
    )
}

pub(super) fn load_credentials(conn: &Connection) -> Result<Vec<CredentialAccount>> {
    rows_of(
        conn,
        "SELECT digest, provider, account_number, first_seen_ms, last_seen_ms
         FROM credential_accounts ORDER BY first_seen_ms, digest",
        [],
        |row| {
            let digest: String = row.get("digest")?;
            Ok(CredentialAccount {
                digest: CredentialDigest::new(digest).map_err(Error::InvalidAccountState)?,
                provider: row.get("provider")?,
                account_number: row.get("account_number")?,
                first_seen_ms: row.get("first_seen_ms")?,
                last_seen_ms: row.get("last_seen_ms")?,
            })
        },
    )
}
