//! API keys handed to toker, kept in the OS keyring.
//!
//! The operator puts a key there; the setup wizard only asks whether an
//! entry exists, and the service reads it once at startup. Both sides go
//! through [`SecretStore`] so no test ever touches the real keyring: the
//! wizard's tests and the server's inject a map-backed store instead.
//!
//! spec: SEC#a-credential-is-never-logged-or-displayed (credentials): a key read here only ever becomes the
//! provider's injected credential. It is never logged, and the errors
//! below name the account, never the value.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context;

/// The keyring service every toker entry lives under; the account is the
/// provider's name (`openrouter`, `anthropic_api`).
pub const KEYRING_SERVICE: &str = "toker";

/// How long the service waits for one keyring read. The Secret Service
/// may prompt to unlock a locked collection, and a socket-activated
/// service has nobody to answer: past this the read is "no key", so a
/// locked keyring costs the key, never the listener.
pub const KEYRING_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The keyring seam.
pub trait SecretStore: Send + Sync {
    /// The stored secret for `account`, `Ok(None)` when there is no entry.
    fn get(&self, account: &str) -> anyhow::Result<Option<String>>;

    /// Whether `account` has an entry, found without reading its secret.
    fn exists(&self, account: &str) -> anyhow::Result<bool>;
}

/// The real keyring: the platform's own store (the Secret Service on
/// Linux, Keychain on macOS) through the `keyring` crate.
pub struct OsKeyring;

impl SecretStore for OsKeyring {
    fn get(&self, account: &str) -> anyhow::Result<Option<String>> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, account)
            .with_context(|| format!("opening the keyring entry {KEYRING_SERVICE}/{account}"))?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(anyhow::Error::new(error).context(format!(
                "reading the keyring entry {KEYRING_SERVICE}/{account}"
            ))),
        }
    }

    fn exists(&self, account: &str) -> anyhow::Result<bool> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, account)
            .with_context(|| format!("opening the keyring entry {KEYRING_SERVICE}/{account}"))?;
        // Finding the credential searches the store by the entry's
        // attributes and hands back a handle to it, without fetching the
        // secret: setup asks only whether a key is there, and must not read
        // it (spec: USETUP#writing). Several matching entries still mean
        // one exists.
        match entry.inner.get_credential() {
            Ok(_) | Err(keyring::Error::Ambiguous(_)) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(anyhow::Error::new(error).context(format!(
                "looking up the keyring entry {KEYRING_SERVICE}/{account}"
            ))),
        }
    }
}

/// Read one key in the service, bounded by `timeout`. Every failure —
/// no secret service, a locked collection nobody unlocks, a missing
/// entry — reads as no key, with one warning naming the account: the
/// provider then goes upstream unauthenticated (or with the frontend's
/// own credential), and the upstream's 401 says so visibly.
pub fn read_key(
    store: Arc<dyn SecretStore>,
    account: &'static str,
    timeout: Duration,
) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    // A thread rather than the caller's: a read blocked on an unlock
    // prompt must not hold startup, and an abandoned thread blocked in
    // D-Bus costs nothing but itself.
    let spawned = std::thread::Builder::new()
        .name("toker-keyring".to_owned())
        .spawn(move || {
            let _ = tx.send(store.get(account));
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, account, "the keyring read could not start; no stored key");
        return None;
    }
    match rx.recv_timeout(timeout) {
        Ok(Ok(Some(secret))) => Some(secret),
        Ok(Ok(None)) => {
            tracing::warn!(
                account,
                "the config says the key is in the keyring, but there is no entry; no stored key"
            );
            None
        }
        Ok(Err(error)) => {
            tracing::warn!(error = %format!("{error:#}"), account, "keyring read failed; no stored key");
            None
        }
        Err(_) => {
            tracing::warn!(
                account,
                "the keyring did not answer within {timeout:?} (locked?); no stored key"
            );
            None
        }
    }
}

/// A map-backed [`SecretStore`] for tests: never the real keyring. `None`
/// for the map stands for "no secret service": every call fails.
pub struct MemoryStore {
    entries: std::sync::Mutex<Option<std::collections::BTreeMap<String, String>>>,
    reads: AtomicUsize,
}

impl MemoryStore {
    /// A working store with no entries.
    pub fn new() -> MemoryStore {
        MemoryStore {
            entries: std::sync::Mutex::new(Some(Default::default())),
            reads: AtomicUsize::new(0),
        }
    }

    /// A store standing for a machine with no secret service.
    pub fn unavailable() -> MemoryStore {
        MemoryStore {
            entries: std::sync::Mutex::new(None),
            reads: AtomicUsize::new(0),
        }
    }

    /// How many times a secret was asked for, so a test can show that a
    /// caller only checking for an entry never read one.
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    /// Seed `secret` for `account`, standing for a key the operator put
    /// in the keyring. Does nothing on a store with no secret service.
    pub fn insert(&self, account: &str, secret: &str) {
        if let Some(entries) = &mut *self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            entries.insert(account.to_owned(), secret.to_owned());
        }
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore::new()
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, account: &str) -> anyhow::Result<Option<String>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        match &*self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            Some(entries) => Ok(entries.get(account).cloned()),
            None => anyhow::bail!("no secret service"),
        }
    }

    fn exists(&self, account: &str) -> anyhow::Result<bool> {
        match &*self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            Some(entries) => Ok(entries.contains_key(account)),
            None => anyhow::bail!("no secret service"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store whose reads never return: a locked collection waiting on
    /// an unlock prompt nobody answers.
    struct Hangs;

    impl SecretStore for Hangs {
        fn get(&self, _account: &str) -> anyhow::Result<Option<String>> {
            std::thread::sleep(Duration::from_secs(60));
            Ok(Some("late".to_owned()))
        }

        fn exists(&self, _account: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    #[test]
    fn every_keyring_failure_reads_as_no_key() {
        let store = Arc::new(MemoryStore::new());
        store.insert("openrouter", "sk-test");
        assert_eq!(
            read_key(store.clone(), "openrouter", KEYRING_READ_TIMEOUT).as_deref(),
            Some("sk-test")
        );
        assert_eq!(
            read_key(store, "anthropic_api", KEYRING_READ_TIMEOUT),
            None,
            "no entry"
        );
        assert_eq!(
            read_key(
                Arc::new(MemoryStore::unavailable()),
                "openrouter",
                KEYRING_READ_TIMEOUT
            ),
            None,
            "no secret service"
        );
        let started = std::time::Instant::now();
        assert_eq!(
            read_key(Arc::new(Hangs), "openrouter", Duration::from_millis(50)),
            None,
            "a read that never answers"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounded by the timeout"
        );
    }
}
