//! The pre-account rows gate as processes: every command but setup
//! refuses while the ledger holds rows served before toker recorded
//! which account they were on, and the refusal is one the supervisor
//! will not retry (spec: ACCT#moving-a-machine-onto-accounts).

mod common;

use std::process::Command;

use toker::store::{RequestRow, Store};

fn scratch(name: &str) -> common::TestDir {
    common::tempdir(&format!("toker-accounts-gate-{name}-"))
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

/// The store the binary will open, seeded with one row from before
/// accounts: no account, and no digest to resolve into one later.
fn seed_unattributed(data: &std::path::Path) -> std::path::PathBuf {
    let db = data.join("toker").join("toker.db");
    std::fs::create_dir_all(db.parent().expect("the state dir")).expect("create the state dir");
    let store = Store::open(&db).expect("open the store");
    let row = RequestRow {
        ts_ms: jiff::Timestamp::now().as_millisecond(),
        provider: Some("anthropic_sub".to_owned()),
        ..Default::default()
    };
    store.record_request(&row).expect("seed the ledger");
    db
}

#[test]
fn every_command_but_setup_refuses_while_rows_predate_accounts() {
    let dir = scratch("present");
    let config = dir.join("toker.toml");
    let data = dir.join("data");
    seed_unattributed(&data);

    // `setup` is left out deliberately: it is the one command that may
    // run, because it is what attributes the rows.
    for verb in [
        &["status"][..],
        &["export"],
        &["tui"],
        &["serve"],
        &["restart"],
    ] {
        let output = toker(&config, &data)
            .args(verb)
            .output()
            .expect("run the command");
        let verb = verb.join(" ");
        assert_eq!(
            output.status.code(),
            Some(toker::accounts::EXIT_ROWS_UNATTRIBUTED),
            "{verb} must refuse while such rows remain"
        );
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(
            said.contains("toker setup"),
            "{verb} must say what attributes them: {said}"
        );
        assert!(
            said.contains("account"),
            "{verb} must say what is missing: {said}"
        );
    }
}

#[test]
fn the_refusal_is_a_status_of_its_own_so_the_supervisor_can_stop_retrying() {
    assert_ne!(
        toker::accounts::EXIT_ROWS_UNATTRIBUTED,
        0,
        "a refusal is not a success"
    );
    assert_ne!(
        toker::accounts::EXIT_ROWS_UNATTRIBUTED,
        toker::config::EXIT_CONFIG_FILE_PRESENT,
        "the two refusals are told apart by their status"
    );
    assert!(
        toker::setup::wizard::service_unit(
            std::path::Path::new("/opt/toker/toker"),
            std::path::Path::new("/state/toker.db"),
            &[],
        )
        .contains(&format!(
            "RestartPreventExitStatus={} {}",
            toker::config::EXIT_CONFIG_FILE_PRESENT,
            toker::accounts::EXIT_ROWS_UNATTRIBUTED
        )),
        "the unit names both statuses it must not retry"
    );
}

#[test]
fn a_command_runs_once_the_rows_are_attributed() {
    let dir = scratch("absent");
    let config = dir.join("toker.toml");
    let data = dir.join("data");
    let db = seed_unattributed(&data);

    {
        let store = Store::open(&db).expect("open the store");
        let digest = toker::providers::credential_digest("gate-credential");
        let account = store
            .observe_credential(
                "anthropic_sub",
                &digest,
                jiff::Timestamp::now().as_millisecond(),
                Some(&toker::store::AccountIdentity {
                    upstream_id: None,
                    email: Some("fixture@example.test".to_owned()),
                    named_by: toker::store::AccountNameSource::Local,
                }),
            )
            .expect("observe the credential")
            .expect("the identity names an account");
        assert_eq!(
            store
                .attribute_unattributed_rows("anthropic_sub", account)
                .expect("attribute"),
            1
        );
    }

    let output = toker(&config, &data)
        .arg("status")
        .output()
        .expect("run status");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
}
