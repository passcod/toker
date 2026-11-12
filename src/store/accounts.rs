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
        // Only when the binding actually changes. Rebinding sweeps the
        // ledger for rows this digest left unattributed, which on a real
        // store is every row from before accounts were recorded: ~96,700
        // on the machine this was written against, measured at ~55ms. A
        // digest that already resolves to this account has nothing to
        // sweep, and it is the overwhelmingly common case — once per
        // credential against once per request.
        if account_number != Some(number) {
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
    // Matched by upstream id, the thing an account actually is: the address
    // beside it may have changed, so take the one offered.
    if let Some(number) = by_upstream {
        conn.execute(
            "UPDATE accounts
             SET email = COALESCE(?2, email), last_seen_ms = MAX(last_seen_ms, ?3)
             WHERE number = ?1",
            (number, identity.email.as_deref(), at_ms),
        )?;
        return Ok(number);
    }
    // Matched by address alone, so the account is one held by its address
    // until an id arrives. Filling an absent id is that arrival; replacing
    // one already held would be two accounts conflated under one address,
    // which is the sighting to refuse rather than resolve.
    if let Some(number) = by_email {
        let held: Option<String> = conn.query_row(
            "SELECT upstream_id FROM accounts WHERE number = ?1",
            [number],
            |row| row.get(0),
        )?;
        if let (Some(held), Some(offered)) = (held.as_deref(), identity.upstream_id.as_deref())
            && held != offered
        {
            return Err(Error::InvalidAccountState(format!(
                "account {number} is held by upstream id {held}, not {offered}"
            )));
        }
        conn.execute(
            "UPDATE accounts
             SET upstream_id = COALESCE(upstream_id, ?2),
                 last_seen_ms = MAX(last_seen_ms, ?3)
             WHERE number = ?1",
            (number, identity.upstream_id.as_deref(), at_ms),
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

/// Open (or find) the account `identity` names, without a credential to
/// hang it on. This is how history that predates accounts is given one on a
/// machine that has never seen a login for it
/// (spec: ACCT#moving-a-machine-onto-accounts).
pub(super) fn open_account(
    conn: &mut Connection,
    provider: &str,
    identity: &AccountIdentity,
    at_ms: i64,
) -> Result<i64> {
    identity.validate()?;
    let tx = conn.transaction()?;
    let number = account_for_identity(&tx, provider, identity, at_ms)?;
    tx.commit()?;
    Ok(number)
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

/// How many of a provider's rows predate accounts: carrying neither an
/// account nor the digest of the credential that served them
/// (spec: ACCT#moving-a-machine-onto-accounts).
///
/// A row carrying a digest but no account is not one of these. Its account is
/// simply not resolved yet, and resolving the digest backfills it.
///
/// Neither is a row the proxy wrote about a request it never sent: a block,
/// a release, a cold notice and the like carry no credential by nature, so
/// counting them made every gate event written after accounts began read as
/// history from before, and every command refused to run over it. Only a
/// real API measurement (`kind IS NULL`) was ever served on an account.
pub(super) fn unattributed_row_count(conn: &Connection, provider: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM requests
         WHERE provider = ?1 AND account_number IS NULL AND credential_digest IS NULL
           AND kind IS NULL",
        [provider],
        |row| row.get(0),
    )?)
}

/// Why a history cannot be attributed to one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixedHistory {
    /// When the row that contradicted the window was logged.
    pub at_ms: i64,
    /// The reset the window open at that moment reported.
    pub open_reset_s: i64,
    /// The reset that row reported instead.
    pub row_reset_s: i64,
}

/// Whether a provider's pre-account rows carry the meter windows of a
/// single account (spec: ACCT#moving-a-machine-onto-accounts).
///
/// Two accounts' windows overlap in time and show up as two series, so
/// the evidence is a series that goes back to a window it had already
/// left while a later one is still open. One account's window only ever
/// moves forward: it rolls to the next five hours, or is re-anchored
/// forward in ten-minute steps while traffic keeps arriving.
///
/// Only live windows are read. A reading whose reset had already passed
/// when it was served names no open window at all — it is a stale copy,
/// which the proxy writes onto its own blocked and error rows — and one
/// more than five hours ahead cannot be a five-hour window's. Both were
/// found by running an earlier, stricter form of this check against a
/// copy of a real store, where they made a one-account history read as
/// two.
pub(super) fn unattributed_window_conflict(
    conn: &Connection,
    provider: &str,
) -> Result<Option<MixedHistory>> {
    const WINDOW_MS: i64 = 5 * 60 * 60 * 1000;
    let mut statement = conn.prepare(
        "SELECT ts_ms, CAST(json_extract(rate_limits, '$.reset5h') AS INTEGER) AS reset5h
         FROM requests
         WHERE provider = ?1 AND account_number IS NULL AND credential_digest IS NULL
           AND kind IS NULL AND rate_limits IS NOT NULL
           AND json_type(rate_limits, '$.reset5h') IN ('integer', 'real')
         ORDER BY ts_ms, id",
    )?;
    let mut rows = statement.query([provider])?;
    // The furthest window any earlier row named. The walk compares
    // against this rather than the row before, so a single step back is
    // caught however many rows the series ran forward first.
    let mut furthest: Option<i64> = None;
    while let Some(row) = rows.next()? {
        let at_ms: i64 = row.get("ts_ms")?;
        let reset_s: i64 = row.get("reset5h")?;
        let reset_ms = reset_s.saturating_mul(1000);
        if reset_ms <= at_ms || reset_ms > at_ms.saturating_add(WINDOW_MS) {
            continue;
        }
        if let Some(open_reset_s) = furthest
            && reset_s < open_reset_s
            && at_ms < open_reset_s.saturating_mul(1000)
        {
            return Ok(Some(MixedHistory {
                at_ms,
                open_reset_s,
                row_reset_s: reset_s,
            }));
        }
        furthest = Some(furthest.map_or(reset_s, |open| open.max(reset_s)));
    }
    Ok(None)
}

/// Write `account` onto every one of a provider's pre-account rows, in one
/// transaction (spec: ACCT#moving-a-machine-onto-accounts). Returns how many
/// rows moved, so an interrupted move repeats harmlessly: a second run finds
/// none left and reports zero.
pub(super) fn attribute_unattributed_rows(
    conn: &mut Connection,
    provider: &str,
    account: i64,
) -> Result<usize> {
    let tx = conn.transaction()?;
    let moved = tx.execute(
        "UPDATE requests SET account_number = ?2
         WHERE provider = ?1 AND account_number IS NULL AND credential_digest IS NULL
           AND kind IS NULL",
        rusqlite::params![provider, account],
    )?;
    tx.commit()?;
    Ok(moved)
}

/// How many of a provider's rows carry `digest`. The requests held
/// against a credential no source has named yet: they move to the
/// account the moment one does.
pub(super) fn rows_for_digest(conn: &Connection, provider: &str, digest: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM requests WHERE provider = ?1 AND credential_digest = ?2",
        rusqlite::params![provider, digest],
        |row| row.get(0),
    )?)
}

/// The account `digest` is bound to, if any source has named it.
pub(super) fn account_for_digest(
    conn: &Connection,
    digest: &CredentialDigest,
) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT account_number FROM credential_accounts WHERE digest = ?1",
            [digest.as_str()],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten())
}
