//! `toker picker sync` end to end: a machine with no active routing revision
//! has nothing to publish, and sync must say so without touching claude's
//! settings. No network, no real settings: every path is a temp directory.

mod common;

use toker::config::Config;

fn test_dir(name: &str) -> common::TestDir {
    common::tempdir(&format!("toker-picker-sync-{name}-"))
}

fn config(dir: &std::path::Path) -> Config {
    let path = dir.join("toker.toml");
    std::fs::write(
        &path,
        format!(
            "db_path = \"{}\"\n[providers.anthropic_sub]\n",
            dir.join("toker.db").display()
        ),
    )
    .expect("write config");
    Config::load_from(&path).expect("config loads")
}

const SETTINGS: &str = r#"{"modelPicker": {"options": [{"model": "openrouter/kept/model"}]}}"#;

#[test]
fn a_sync_without_an_active_revision_fails_and_leaves_the_picker_alone() {
    let dir = test_dir("no-revision");
    let config = config(&dir);
    let settings = dir.join("settings.json");
    std::fs::write(&settings, SETTINGS).expect("write settings");

    let error = toker::picker::sync(&config, &settings, false).expect_err("nothing to publish");

    assert!(
        format!("{error:#}").contains("no active routing revision"),
        "{error:#}"
    );
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), SETTINGS);
}

#[test]
fn a_dry_run_without_a_ledger_fails_and_does_not_create_one() {
    let dir = test_dir("dry-run-no-ledger");
    let config = config(&dir);
    let settings = dir.join("settings.json");
    std::fs::write(&settings, SETTINGS).expect("write settings");

    let error = toker::picker::sync(&config, &settings, true).expect_err("nothing to publish");

    assert!(format!("{error:#}").contains("no active routing revision"));
    assert!(!config.db_path.exists(), "dry run leaves no ledger behind");
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), SETTINGS);
}
