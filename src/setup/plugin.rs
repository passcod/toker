//! The bundled opencode plugin (`plugins/opencode/toker-cost/`):
//! the repo files are canonical, embedded at build time so an installed
//! binary carries its own copy — `cargo run`, `cargo install`, any
//! working dir, all install the same bytes. A test pins the embedded
//! copies to the on-disk files so the two can never drift.

use anyhow::{Context, Result};

/// The plugin's file set, as (filename, bytes).
pub(crate) const PLUGIN_FILES: &[(&str, &str)] = &[
    (
        "package.json",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/opencode/toker-cost/package.json"
        )),
    ),
    (
        "index.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/opencode/toker-cost/index.ts"
        )),
    ),
    (
        "tui.tsx",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/opencode/toker-cost/tui.tsx"
        )),
    ),
];

/// The install dir for the plugin: `<plugins>/toker-cost/`.
pub(crate) fn plugin_target_dir(plugins_dir: &std::path::Path) -> std::path::PathBuf {
    plugins_dir.join("toker-cost")
}

/// The plugin's install state under `plugins_dir`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginState {
    /// Nothing (or an empty dir) is there.
    Absent,
    /// Every file present, byte-identical to the embedded set.
    Installed,
    /// A dir with files, but not ours as-shipped (a hand edit or a
    /// different toker version) — reinstall is an overwrite offer.
    Different,
}

/// Read the install state: every embedded file present byte-identical
/// is `Installed`; a target dir holding anything else is `Different`
/// (a hand edit or another version — the caller asks before touching
/// it); no dir is `Absent`.
pub(crate) fn plugin_state(plugins_dir: &std::path::Path) -> PluginState {
    let dir = plugin_target_dir(plugins_dir);
    if !dir.is_dir() {
        return PluginState::Absent;
    }
    let mut all_installed = true;
    for (name, bytes) in PLUGIN_FILES {
        match std::fs::read(dir.join(name)) {
            Ok(disk) if disk == bytes.as_bytes() => {}
            _ => all_installed = false,
        }
    }
    if all_installed {
        return PluginState::Installed;
    }
    PluginState::Different
}

/// Install the embedded plugin: create the target dir, write every
/// file. Overwrites whatever is there (the caller checked the state
/// and asked). Plain writes — the dir is toker's to own once accepted;
/// a partially failed install is reported as the error it is.
pub(crate) fn install_plugin(plugins_dir: &std::path::Path) -> Result<()> {
    let dir = plugin_target_dir(plugins_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, bytes) in PLUGIN_FILES {
        std::fs::write(dir.join(name), bytes)
            .with_context(|| format!("installing {}", dir.join(name).display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_copies_are_the_repo_files() {
        // The two can never drift: the embed is a build-time copy, so
        // editing the repo file without rebuilding shows here as a
        // mismatch the moment the test binary is rebuilt.
        for (name, bytes) in PLUGIN_FILES {
            let disk = std::fs::read_to_string(format!(
                "{}/plugins/opencode/toker-cost/{name}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .expect("the repo file exists");
            assert_eq!(*bytes, disk, "{name} embedded ≠ on disk");
        }
    }

    #[test]
    fn the_state_machine_reads_absent_installed_and_different() {
        let root = crate::setup::test_dir("plugin-state");
        let plugins = root.join("plugins");
        assert_eq!(plugin_state(&plugins), PluginState::Absent);

        install_plugin(&plugins).expect("install");
        assert_eq!(plugin_state(&plugins), PluginState::Installed);

        std::fs::write(
            plugin_target_dir(&plugins).join("package.json"),
            "{\"name\":\"edited\"}",
        )
        .expect("hand edit");
        assert_eq!(plugin_state(&plugins), PluginState::Different);

        install_plugin(&plugins).expect("reinstall overwrites");
        assert_eq!(plugin_state(&plugins), PluginState::Installed);
    }
}
