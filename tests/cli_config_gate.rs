//! The configuration file gate as processes: every command but setup
//! refuses while a file is there, and the refusal is one the supervisor
//! will not retry.

mod common;

use std::process::Command;

fn scratch(name: &str) -> common::TestDir {
    common::tempdir(&format!("toker-config-gate-{name}-"))
}

fn toker(config: &std::path::Path, data: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_toker"));
    // Nothing here may read the real configuration or store.
    command
        .env("TOKER_CONFIG", config)
        .env("XDG_DATA_HOME", data)
        .env_remove("TOKER_DB");
    command
}

#[test]
fn every_command_but_setup_refuses_while_a_configuration_file_exists() {
    let dir = scratch("present");
    let config = dir.join("toker.toml");
    let data = dir.join("data");
    std::fs::write(&config, "port = 19999\n[providers.openrouter]\n").expect("seed the file");

    // `setup` is left out deliberately: it is the one command that may run,
    // because it is what moves the machine off the file.
    // A mistyped flag still fails as a mistyped flag: clap parses before
    // the gate, so these are spelled as the CLI takes them.
    for verb in [
        &["status"][..],
        &["export"],
        &["tui"],
        &["serve"],
        &["restart"],
        &["picker", "sync"],
        &["promote", "--model", "some-model"],
    ] {
        let output = toker(&config, &data)
            .args(verb)
            .output()
            .expect("run the command");
        let verb = verb.join(" ");
        assert_eq!(
            output.status.code(),
            Some(toker::config::EXIT_CONFIG_FILE_PRESENT),
            "{verb} must refuse while the file is there"
        );
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(
            said.contains("toker setup"),
            "{verb} must say what moves the machine off it: {said}"
        );
        assert!(
            said.contains(&config.display().to_string()),
            "{verb} must name the file: {said}"
        );
    }
}

#[test]
fn the_refusal_is_a_status_of_its_own_so_the_supervisor_can_stop_retrying() {
    // Nothing can change until the operator runs setup, so a supervisor
    // that retried would only fill the journal (spec: DLIFE#starting).
    // The service unit names this status in RestartPreventExitStatus.
    let dir = scratch("status");
    let config = dir.join("toker.toml");
    std::fs::write(&config, "port = 19999\n").expect("seed the file");

    let refused = toker(&config, &dir.join("data"))
        .arg("serve")
        .output()
        .expect("run serve");
    assert_eq!(
        refused.status.code(),
        Some(toker::config::EXIT_CONFIG_FILE_PRESENT)
    );
    assert_ne!(
        toker::config::EXIT_CONFIG_FILE_PRESENT,
        0,
        "a refusal is not a success"
    );
    assert!(
        toker::setup::wizard::service_unit(
            std::path::Path::new("/opt/toker/toker"),
            std::path::Path::new("/state"),
            &[],
        )
        .contains(&format!(
            "RestartPreventExitStatus={}",
            toker::config::EXIT_CONFIG_FILE_PRESENT
        )),
        "the unit names the status it must not retry"
    );
}

#[test]
fn a_command_runs_once_the_file_is_gone() {
    let dir = scratch("absent");
    let config = dir.join("toker.toml");
    let data = dir.join("data");

    // No file: the machine is across, so the store answers for it.
    let output = toker(&config, &data)
        .arg("status")
        .output()
        .expect("run status");
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(said.contains("port:"), "status reports the machine: {said}");
}
