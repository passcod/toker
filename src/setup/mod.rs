//! `toker setup` — the wizard's library half plus the wizard itself
//! (plan: "Setup wizard", phase 5).
//!
//! `toker setup` is idempotent — re-run it to change anything — and its
//! steps are fixed in [`plan`], which is also the documentation of WHY
//! the order is what it is (the ordering rule). The pieces:
//!
//! - [`atomic`] — the write primitive every config patch shares:
//!   temp-write in the same directory, fsync, re-parse, compare, rename
//!   (the opencode.json lesson), symlink-resolving, refusing to touch
//!   anything it does not understand.
//! - [`config_writer`] — `toker.toml`: read-merge, change, rewrite.
//! - [`patchers`] — one pure-file patch per frontend (claude user +
//!   Workhorse settings, opencode, the generic shell rc), plus the
//!   [`patchers::Frontend`] enum the wizard composes.
//! - [`verify`] — [`verify::await_service_ready`], the wiring check
//!   the wizard runs after the units are up and before any frontend is
//!   pointed at toker: an empty-body POST through each frontend's own
//!   prefix, and the prefix reaching toker's own status.
//! - [`wizard`] — the interactive flow: the questions, the systemd
//!   unit installation, and the report, all behind testable seams
//!   ([`wizard::Prompt`], [`wizard::SystemRunner`], [`wizard::Paths`])
//!   so the whole wizard runs as a script in tests. `cmds::setup` is
//!   the only place the seams meet the real world.

pub mod atomic;
pub mod config_writer;
pub mod patchers;
pub mod plugin;
pub mod verify;
pub mod wizard;

/// One `toker setup` step, in execution order (see [`plan`]). The
/// skeleton the interactive unit fills; every step is re-runnable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Pick backends and authenticate each (paste key / reuse CLI
    /// tokens / keyring with fallback). First because nothing after it
    /// can be verified without a backend wired.
    ChooseBackends,
    /// Write `toker.toml`
    /// ([`config_writer::write_config`]) — the resolved config with
    /// the wizard's choices applied, atomically.
    WriteConfig,
    /// Install and start the systemd units (`toker.socket` first — the
    /// enabled unit — then `toker.service`). **This is where the
    /// socket binds.**
    InstallUnits,
    /// Verify the live service end-to-end
    /// ([`verify::await_service_ready`]): the usage path of every
    /// enabled protocol must answer with the upstream's verdict, through
    /// the very `/f/<frontend>` prefix each frontend will be pointed at,
    /// and that prefix must reach toker itself. **The ordering rule's
    /// enforcement point** — no frontend is patched until this passes,
    /// because a frontend's settings hot-reload into its live sessions.
    VerifyService,
    /// Point the frontends at toker ([`patchers`]): claude (user +
    /// Workhorse repo settings — both required, directory-scoped
    /// settings win), opencode, and/or the shell rc. Last, so the
    /// clients are only ever pointed at a listener that is provably up.
    PatchFrontends,
    /// The wizard's terminal step: report what was done and what to
    /// re-run to change it.
    Done,
}

impl Step {
    /// A short human label, for the wizard's display.
    pub fn label(self) -> &'static str {
        match self {
            Step::ChooseBackends => "choose backends",
            Step::WriteConfig => "write toker.toml",
            Step::InstallUnits => "install + start the units",
            Step::VerifyService => "verify the service answers",
            Step::PatchFrontends => "wire the frontends",
            Step::Done => "done",
        }
    }
}

/// The wizard's step order. Why it is THIS order:
///
/// **Bind the socket first, then point clients at it** (the plan's
/// ordering rule, on the measured failure: a frontend's env
/// hot-reloads into *running* sessions, so pointing a client at a
/// listener that is not up yet kills live sessions). So
/// [`Step::InstallUnits`] binds the socket, [`Step::VerifyService`]
/// proves the whole wiring answers, and only [`Step::PatchFrontends`]
/// rewrites client configs — a frontend patched before the socket
/// answers is a frontend broken by setup.
///
/// **Every step is re-runnable** (the plan's idempotence rule):
/// `toker setup` is re-enterable at any point and converges. Each
/// config write is an atomic merge-in-place ([`atomic`],
/// [`config_writer`]), each frontend patch is an update-in-place that
/// never stacks duplicates ([`patchers`]), and the unit install is
/// declarative — re-running a step applies "the same intent" again,
/// not "the same edit" again.
///
/// The plan's wizard list, in plan order, maps onto these steps as:
/// backends → defaults → toggles ([`Step::ChooseBackends`] +
/// [`Step::WriteConfig`]), units ([`Step::InstallUnits`]), verify
/// ([`Step::VerifyService`]), wire frontends ([`Step::PatchFrontends`]).
pub fn plan() -> &'static [Step] {
    &[
        Step::ChooseBackends,
        Step::WriteConfig,
        Step::InstallUnits,
        Step::VerifyService,
        Step::PatchFrontends,
        Step::Done,
    ]
}

/// A fresh scratch directory under /tmp/opencode, unique per call —
/// every test in this module tree writes here, never into the real
/// home (the machine's live configs are already correctly wired and
/// stay untouched).
#[cfg(test)]
pub(crate) fn test_dir(name: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::path::PathBuf::from("/tmp/opencode").join(format!(
        "setup-{}-{}-{}",
        std::process::id(),
        name,
        n
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

#[cfg(test)]
mod tests {
    use super::{Step, plan};

    #[test]
    fn the_plan_is_pinned_and_the_ordering_rule_holds() {
        let steps = plan();
        assert_eq!(
            steps,
            &[
                Step::ChooseBackends,
                Step::WriteConfig,
                Step::InstallUnits,
                Step::VerifyService,
                Step::PatchFrontends,
                Step::Done,
            ],
            "the wizard's order is part of its contract"
        );
        let verify = steps
            .iter()
            .position(|step| *step == Step::VerifyService)
            .expect("the plan verifies");
        let patch = steps
            .iter()
            .position(|step| *step == Step::PatchFrontends)
            .expect("the plan patches frontends");
        let install = steps
            .iter()
            .position(|step| *step == Step::InstallUnits)
            .expect("the plan installs units");
        assert!(
            install < verify && verify < patch,
            "bind the socket, verify it answers, THEN point clients at it"
        );
        assert_eq!(*steps.last().expect("non-empty"), Step::Done);
    }

    #[test]
    fn every_step_labels_uniquely() {
        let mut labels: Vec<&str> = plan().iter().map(|step| step.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), plan().len(), "each step has its own label");
        assert!(labels.iter().all(|label| !label.is_empty()));
    }
}
