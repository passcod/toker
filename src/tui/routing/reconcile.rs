//! Claude settings reconciliation after an activation.
//!
//! The daemon has already switched policy by the time this runs, so the sync
//! lives on its own short-lived thread and reports back by message: a failed
//! or slow settings write can only ever produce a note on the page, never undo
//! or delay the activation (the picker records the failure in the publication
//! row itself).

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use crate::config::Config;

/// What the page needs to reconcile Claude's settings with the active policy.
#[derive(Debug, Clone)]
pub struct SettingsSync {
    pub config: Config,
    pub settings: PathBuf,
}

/// Runs reconciliations off the terminal thread and collects their outcomes.
pub(super) struct Reconciler {
    sync: Option<SettingsSync>,
    tx: Sender<String>,
    rx: Receiver<String>,
}

impl Reconciler {
    pub(super) fn new(sync: Option<SettingsSync>) -> Reconciler {
        let (tx, rx) = channel();
        Reconciler { sync, tx, rx }
    }

    /// Start a reconciliation, returning the note to show while it runs, or
    /// `None` when this terminal has no settings context.
    pub(super) fn start(&self) -> Option<String> {
        let sync = self.sync.clone()?;
        let tx = self.tx.clone();
        let spawned = thread::Builder::new()
            .name("toker-routing-settings".to_owned())
            .spawn(move || {
                let _ = tx.send(run(&sync));
            });
        Some(match spawned {
            Ok(_) => "Claude settings: reconciling".to_owned(),
            Err(error) => failure(&error.to_string()),
        })
    }

    pub(super) fn poll(&self) -> Option<String> {
        self.rx.try_recv().ok()
    }
}

fn run(sync: &SettingsSync) -> String {
    // A panic must not leave the page claiming it is still reconciling.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::picker::sync(&sync.config, &sync.settings, false)
    }));
    match result {
        Ok(Ok(report)) if report.changed => "Claude settings: updated".to_owned(),
        Ok(Ok(_)) => "Claude settings: already current".to_owned(),
        Ok(Err(error)) => failure(&format!("{error:#}")),
        Err(_) => failure("reconciliation panicked"),
    }
}

fn failure(detail: &str) -> String {
    format!(
        "Claude settings not reconciled ({detail}); the activation stands, run `toker picker sync` to retry"
    )
}

#[cfg(test)]
mod tests {
    use super::{Reconciler, SettingsSync, run};
    use crate::config::Config;

    fn sync(name: &str) -> (crate::test_support::TestDir, SettingsSync) {
        let dir = crate::test_support::tempdir(&format!("toker-reconcile-{name}-"));
        let path = dir.join("toker.toml");
        std::fs::write(&path, "[providers.anthropic_sub]\n").unwrap();
        let mut config = Config::load_from(&path).unwrap();
        config.db_path = dir.join("toker.db");
        let settings = dir.join("settings.json");
        std::fs::write(&settings, "{}").unwrap();
        // The sync publishes the active policy, so one must be active.
        let store = crate::store::Store::open(&config.db_path).unwrap();
        let registry = crate::routing::RouteRegistry::configured_bindings(
            crate::picker::configured_bindings(&config),
        );
        let cache = crate::catalog::fetched::cache_dir().ok();
        let catalogs = crate::picker::configured_catalogs(&config, cache.as_deref());
        let inventory = crate::catalog::offers::inventory(&registry, &catalogs);
        crate::policy::legacy::establish_revision_one(
            &store,
            &config,
            &registry,
            &inventory,
            crate::catalog::fetched::snapshot_generation(&catalogs),
            1,
        )
        .unwrap();
        (dir, SettingsSync { config, settings })
    }

    #[test]
    fn a_terminal_without_settings_context_starts_nothing() {
        assert_eq!(Reconciler::new(None).start(), None);
    }

    #[test]
    fn a_successful_sync_is_reported_as_settled() {
        let (_dir, sync) = sync("ok");
        let note = run(&sync);
        assert!(note.starts_with("Claude settings: "), "{note}");
        assert!(!note.contains("not reconciled"), "{note}");
    }

    #[test]
    fn a_failed_sync_says_the_activation_stands() {
        let (dir, mut sync) = sync("fail");
        // A ledger below a regular file cannot be opened.
        sync.config.db_path = dir.join("settings.json").join("toker.db");
        let note = run(&sync);
        assert!(note.starts_with("Claude settings not reconciled"), "{note}");
        assert!(note.contains("the activation stands"), "{note}");
    }

    #[test]
    fn the_outcome_arrives_on_the_poll_channel() {
        let (_dir, sync) = sync("poll");
        let reconciler = Reconciler::new(Some(sync));
        assert_eq!(
            reconciler.start().as_deref(),
            Some("Claude settings: reconciling")
        );
        let note = std::iter::repeat_with(|| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            reconciler.poll()
        })
        .take(250)
        .flatten()
        .next()
        .expect("the reconciliation reports back");
        assert!(note.starts_with("Claude settings: "), "{note}");
    }
}
