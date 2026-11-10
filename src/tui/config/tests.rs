//! The configuration pages' frames and edit flows.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use reqwest::Method;
use serde_json::{Value, json};

use super::model::{Row, State, View};
use super::testing::{loaded, loaded_with_routing, ok, snapshot_body};
use super::view::render;
use crate::tui::control::{Command, Reply, Response};

/// The page at `width`×`height` as text, with the keys overlay over it when
/// `overlay` is set.
fn frame(state: &State, width: u16, height: u16, overlay: bool) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|f| {
            render(f, state, overlay);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(state: &mut State, code: KeyCode) -> Option<Command> {
    state.key(key(code))
}

fn type_in(state: &mut State, text: &str) {
    for c in text.chars() {
        press(state, KeyCode::Char(c));
    }
}

fn ctrl_s(state: &mut State) -> Option<Command> {
    state.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
}

/// Select `row` in the view the state is on.
fn on(state: &mut State, row: Row) {
    let at = state
        .rows()
        .iter()
        .position(|found| *found == row)
        .unwrap_or_else(|| panic!("{row:?} is not listed"));
    state.select(at);
}

fn settings_page() -> State {
    let mut state = loaded_with_routing();
    state.set_view(View::Settings);
    state
}

/// The service's reply to the edit that was last sent.
fn answered(state: &mut State, status: u16, body: Value) -> Option<Command> {
    state.receive(Reply::Command(
        Command {
            name: "edit",
            method: Method::PUT,
            path: "settings",
            control: "config-edit",
            body: None,
        },
        Ok(Response { status, body }),
    ))
}

fn on_provider(state: &mut State, name: &str) {
    let row = provider_row(state, name);
    on(state, row);
}

fn provider_row(state: &State, name: &str) -> Row {
    let snapshot = state.snapshot().expect("loaded");
    Row::Provider(
        snapshot
            .providers
            .iter()
            .position(|provider| provider.name == name)
            .expect("a provider"),
    )
}

// --- frames ------------------------------------------------------------------

/// The page opens on the provider that is on and cannot answer a request,
/// with its fix on the detail's last line (spec: UTUI#providers).
#[test]
fn the_providers_page_opens_on_the_problem_120x40() {
    insta::assert_snapshot!(frame(&loaded_with_routing(), 120, 40, false));
}

#[test]
fn the_providers_page_at_135x50() {
    insta::assert_snapshot!(frame(&loaded_with_routing(), 135, 50, false));
}

/// The narrow providers page sheds the endpoint and the count of bindings
/// before it clips anything (spec: UTUI#what-is-shed-first).
#[test]
fn the_providers_page_at_80x24() {
    insta::assert_snapshot!(frame(&loaded_with_routing(), 80, 24, false));
}

#[test]
fn the_settings_page_at_rest_120x40() {
    insta::assert_snapshot!(frame(&settings_page(), 120, 40, false));
}

#[test]
fn the_settings_page_at_80x24() {
    insta::assert_snapshot!(frame(&settings_page(), 80, 24, false));
}

/// A field refusing a value says why beside itself, and the save control
/// says what is in the way.
#[test]
fn a_field_refusing_a_value_says_so_beside_itself_120x40() {
    let mut state = settings_page();
    on(&mut state, Row::ColdMinTokens);
    press(&mut state, KeyCode::Char('e'));
    for _ in 0..4 {
        press(&mut state, KeyCode::Backspace);
    }
    type_in(&mut state, "2M");
    insta::assert_snapshot!(frame(&state, 120, 40, false));
}

#[test]
fn a_provider_form_opens_under_its_row_at_80x24() {
    let mut state = loaded_with_routing();
    press(&mut state, KeyCode::Char('u'));
    type_in(&mut state, "gateway.invalid");
    insta::assert_snapshot!(frame(&state, 80, 24, false));
}

#[test]
fn the_keys_overlay_120x40() {
    insta::assert_snapshot!(frame(&loaded_with_routing(), 120, 40, true));
}

#[test]
fn an_unreachable_service_says_so_rather_than_offering_values() {
    // The service is the only writer, so a form the operator can fill in
    // while it is down is a form whose edits go nowhere
    // (spec: UTUI#the-configuration-pages).
    let mut state = loaded_with_routing();
    state.command_refused("config-snapshot");
    insta::assert_snapshot!(frame(&state, 100, 24, false));
}

/// Too short for its rows boxed, the page sheds its border before any row,
/// then windows the list around the selection and counts the cut
/// (spec: UTUI#what-is-shed-first).
#[test]
fn a_short_page_goes_ruled_and_counts_what_it_cut() {
    let state = settings_page();
    let roomy = frame(&state, 100, 40, false);
    assert!(
        roomy.contains("\u{250c}SETTINGS"),
        "boxed while it fits:\n{roomy}"
    );
    let short = frame(&state, 100, 6, false);
    assert!(short.contains("SETTINGS \u{2500}"), "ruled:\n{short}");
    assert!(short.contains("more"), "the cut is counted:\n{short}");
}

/// A click names the row drawn where it lands, not one counted from the top
/// of a list that has scrolled.
#[test]
fn a_click_selects_the_row_drawn_there_once_the_list_has_scrolled() {
    let mut state = settings_page();
    let last = state.rows().len() - 1;
    state.select(last);
    let mut terminal = Terminal::new(TestBackend::new(100, 8)).expect("terminal");
    let mut drawn = super::view::Drawn::default();
    terminal
        .draw(|f| drawn = render(f, &state, false))
        .expect("draw");
    let first = drawn.rows.first().cloned().expect("a row was drawn");
    assert!(first.entry > 0, "the list scrolled: {:?}", drawn.rows);
    assert_eq!(
        crate::tui::chrome::entry_at(&drawn.rows, first.area.as_position()),
        Some(first.entry)
    );
}

/// The page's keys are in its overlay, every key it takes named there, and
/// there is no footer (spec: UTUI#finding-the-keys).
#[test]
fn the_overlay_names_every_key_the_page_takes() {
    let named = |token: &str| {
        [
            &super::view::EVERYWHERE[..],
            &super::view::ON_PROVIDERS,
            &super::view::ON_SETTINGS,
            &super::view::IN_A_FORM,
        ]
        .concat()
        .iter()
        .any(|(keys, _)| keys.split([' ', '/']).any(|piece| piece == token))
    };
    for key in 'a'..='z' {
        if super::owns_key(KeyCode::Char(key)) {
            assert!(named(&key.to_string()), "{key} is taken but not named");
        }
    }
    for key in ['1', '2'] {
        assert!(super::owns_key(KeyCode::Char(key)));
        assert!(named(&key.to_string()));
    }
    assert!(super::owns_key(KeyCode::Char(' ')) && named("space"));
    for token in ["enter", "ctrl+s", "esc"] {
        assert!(named(token), "{token} is not named");
    }
    let state = loaded_with_routing();
    assert!(
        !frame(&state, 100, 24, false).contains("quits"),
        "no footer"
    );
    let text = frame(&state, 100, 40, true);
    assert!(
        text.contains("KEYS") && text.contains("switch between"),
        "{text}"
    );
}

/// A refused value is said under the header, with the page still standing
/// below it (spec: UTUI#the-configuration-pages).
#[test]
fn a_message_sits_under_the_header_over_the_page() {
    let mut state = loaded_with_routing();
    state.set_error("quota_enabled: refused by the service");
    let text = frame(&state, 100, 24, false);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[1].contains("refused by the service"), "{text}");
    assert!(lines[2].contains("PROVIDERS"), "{text}");
}

/// The panel title is upper case and runs into its rule, as the dashboard's
/// and the routing page's do.
#[test]
fn the_panel_title_matches_the_other_pages() {
    let text = frame(&loaded_with_routing(), 100, 40, false);
    assert!(text.contains("\u{250c}PROVIDERS\u{2500}"), "{text}");
    assert!(text.contains("\u{250c}PROVIDER\u{2500}"), "{text}");
    let text = frame(&settings_page(), 100, 40, false);
    assert!(text.contains("\u{250c}SETTINGS\u{2500}"), "{text}");
}

/// Routing not being read is not routing having nothing to say: the column
/// is a dash, never a count of nothing.
#[test]
fn an_unread_routing_snapshot_reads_as_a_dash_not_as_nothing() {
    let text = frame(&loaded(), 120, 40, false);
    assert!(!text.contains("nothing"), "{text}");
    assert!(text.contains("not read from the service"), "{text}");
}

#[test]
fn a_provider_nothing_reaches_says_so() {
    let mut state = loaded();
    state.receive_routing(Reply::Command(
        Command::snapshot("routing-snapshot"),
        ok(json!({"derived": {"active": {"providers": []}}})),
    ));
    assert!(frame(&state, 120, 40, false).contains("nothing"));
}

// --- the rows and what opens the page ----------------------------------------

#[test]
fn the_snapshot_becomes_the_rows_the_page_lists() {
    let state = loaded();
    assert!(state.reachable());
    assert_eq!(
        state.rows(),
        (0..5).map(Row::Provider).collect::<Vec<_>>(),
        "one row a provider; their detail is under the table"
    );
    let settings = settings_page();
    let rows = settings.rows();
    assert_eq!(rows.first(), Some(&Row::Quota));
    assert!(rows.contains(&Row::SessionHeaders));
    assert!(rows.contains(&Row::TranscriptRoots));
    assert!(rows.contains(&Row::Notice(0)));
}

/// A provider that is on and holds no credential is the one to look at
/// (spec: UTUI#providers).
#[test]
fn the_page_opens_on_a_provider_that_is_on_and_not_configured() {
    let state = loaded();
    assert_eq!(state.row(), Some(provider_row(&state, "anthropic_sub")));
}

#[test]
fn it_opens_on_the_first_provider_when_none_is_in_trouble() {
    let mut body = snapshot_body();
    body["providers"][1]["credential"]["keyring"] = json!(true);
    let mut state = State::default();
    state.receive(Reply::Command(
        Command::snapshot("config-snapshot"),
        ok(body),
    ));
    assert_eq!(state.selected(), 0);
}

#[test]
fn a_refresh_leaves_the_selection_and_arriving_opens_on_the_problem_again() {
    let mut state = loaded();
    state.select(4);
    state.receive(Reply::Command(
        Command::snapshot("config-snapshot"),
        ok(snapshot_body()),
    ));
    assert_eq!(state.selected(), 4, "an edit's re-read keeps the cursor");
    state.reopen();
    state.receive(Reply::Command(
        Command::snapshot("config-snapshot"),
        ok(snapshot_body()),
    ));
    assert_eq!(state.row(), Some(provider_row(&state, "anthropic_sub")));
}

#[test]
fn a_provider_with_no_credential_reads_as_not_configured() {
    let state = loaded();
    let providers = &state.snapshot().expect("loaded").providers;
    assert!(providers[1].is_not_configured());
    assert!(providers[4].has_credential(), "the keyring is a source");
    // Off is not the same as not configured: nothing is asked of it.
    assert!(!providers[0].is_not_configured());
    // Codex signs with the login it shares with the codex CLI.
    assert!(!providers[2].holds_a_credential());
}

#[test]
fn what_routing_depends_on_is_counted_per_provider() {
    let state = loaded_with_routing();
    let found = state.dependents("openrouter").expect("read");
    assert_eq!(found.rewrites, 10);
    assert_eq!(found.defaults.len(), 4);
    assert_eq!(found.routes.len(), 5);
    assert_eq!(found.bindings.len(), 3);
    assert!(
        loaded().dependents("openrouter").is_none(),
        "unread is not nothing"
    );
}

// --- toggles and the whole row -------------------------------------------------

#[test]
fn space_toggles_a_provider_sending_its_whole_row_without_a_credential() {
    let mut state = loaded();
    let command = press(&mut state, KeyCode::Char(' ')).expect("a request");
    assert_eq!(command.path, "provider");
    assert_eq!(command.control, "config-edit");
    let body = command.body.expect("a body");
    assert_eq!(body["provider"], json!("anthropic_sub"));
    assert_eq!(body["enabled"], json!(false), "the toggle flips it");
    // The credential moves only through its own operation, so the edit path
    // never carries one in either direction (spec: DCTRL).
    assert!(body.get("cred_literal").is_none());
    assert!(body.get("credential").is_none());
}

#[test]
fn an_edit_carries_the_fields_the_page_does_not_edit() {
    // The service writes the row it is given whole, so a field left out of
    // the body is cleared in the store.
    let mut state = loaded();
    state.select(2);
    let provider = press(&mut state, KeyCode::Char(' '))
        .expect("a request")
        .body
        .expect("a body");
    assert_eq!(provider["refresh_url"], json!("http://login.invalid/token"));
    assert_eq!(provider["upstream"], json!("http://gateway.invalid"));

    let mut state = settings_page();
    let settings = press(&mut state, KeyCode::Char(' '))
        .expect("a request")
        .body
        .expect("a body");
    assert_eq!(
        settings["session_headers"],
        json!([
            "x-toker-session",
            "x-claude-code-session-id",
            "x-session-id"
        ])
    );
    assert_eq!(settings["transcript_roots"], json!([]));
    assert_eq!(settings["ping_header"], json!("x-toker-ping"));
}

#[test]
fn nothing_is_applied_locally_until_the_service_says_so() {
    let mut state = loaded();
    press(&mut state, KeyCode::Char(' '));
    assert!(
        state.snapshot().expect("loaded").providers[1].enabled,
        "the page still shows what the service last said"
    );
    // A successful edit is followed by a fresh read, which is what the page
    // then believes.
    let next = answered(&mut state, 200, Value::Null).expect("a refresh follows");
    assert_eq!(next.name, "snapshot");
}

#[test]
fn an_unset_setting_toggles_off_its_default_rather_than_to_it() {
    let mut body = snapshot_body();
    body["settings"]["quota_enabled"] = Value::Null;
    let mut state = State::default();
    state.receive(Reply::Command(
        Command::snapshot("config-snapshot"),
        ok(body),
    ));
    state.set_view(View::Settings);
    // quota_enabled is unset, so it follows the built-in default (on);
    // toggling it must turn it off, not set it to what it already is.
    let command = press(&mut state, KeyCode::Char(' ')).expect("a request");
    assert_eq!(command.body.expect("a body")["quota_enabled"], json!(false));
}

#[test]
fn an_unset_value_says_what_it_falls_back_to() {
    let mut body = snapshot_body();
    for name in ["ping_header", "session_headers", "cold_min_tokens", "awake"] {
        body["settings"][name] = Value::Null;
    }
    let mut state = State::default();
    state.receive(Reply::Command(
        Command::snapshot("config-snapshot"),
        ok(body),
    ));
    state.set_view(View::Settings);
    let text = frame(&state, 120, 40, false);
    for want in [
        "x-toker-ping  built-in",
        "x-toker-session · x-claude-code-session-id · x-session-id  built-in",
        "175k  built-in",
        "on  built-in",
        "follows lane TTL",
        "block  built-in",
        "gfm  follows default",
    ] {
        assert!(text.contains(want), "{want:?} missing:\n{text}");
    }
}

#[test]
fn the_port_and_the_store_are_shown_but_not_offered() {
    let mut state = settings_page();
    let rows = state.rows();
    for row in [Row::Port, Row::Store] {
        assert!(rows.contains(&row));
        assert!(!row.is_editable(), "{row:?} is settled before the store");
        on(&mut state, row);
        assert!(press(&mut state, KeyCode::Char(' ')).is_none());
        assert!(press(&mut state, KeyCode::Char('e')).is_none());
        assert!(press(&mut state, KeyCode::Enter).is_none());
        assert!(state.editing().is_none());
    }
    let text = frame(&state, 120, 40, false);
    assert!(text.contains("18123") && text.contains("set by toker setup"));
    assert!(text.contains("toker.db") && text.contains("set in toker.toml"));
}

#[test]
fn the_pages_report_an_unanswering_service_rather_than_offering_edits() {
    let mut state = loaded();
    assert!(state.reachable());
    state.receive(Reply::Command(
        Command::snapshot("config-snapshot"),
        Err("connection refused".to_owned()),
    ));
    assert!(!state.reachable());
    assert_eq!(state.error(), Some("connection refused"));
    // And nothing can be acted on while it is down: an edit would go
    // nowhere (spec: UTUI#the-configuration-pages).
    assert!(press(&mut state, KeyCode::Char(' ')).is_none());
    assert!(press(&mut state, KeyCode::Char('e')).is_none());
    assert!(press(&mut state, KeyCode::Char('u')).is_none());
    assert!(state.editing().is_none());
}

#[test]
fn a_form_open_when_the_service_goes_quiet_is_closed() {
    let mut state = loaded();
    press(&mut state, KeyCode::Char('u'));
    assert!(state.editing().is_some());
    state.command_refused("edit");
    assert!(state.editing().is_none());
}

// --- forms ---------------------------------------------------------------------

#[test]
fn editing_an_endpoint_sends_what_was_typed_and_an_empty_entry_clears_it() {
    let mut state = loaded();
    assert!(press(&mut state, KeyCode::Char('u')).is_none());
    type_in(&mut state, "http://mine.invalid");
    let command = ctrl_s(&mut state).expect("a request");
    assert_eq!(command.path, "provider");
    assert_eq!(
        command.body.expect("a body")["upstream"],
        json!("http://mine.invalid")
    );
    // The form stays until the service has taken it.
    assert!(state.editing().is_some());
    answered(&mut state, 200, json!({"ok": true}));
    assert!(state.editing().is_none(), "taken, so closed");

    // The form opens on what is stored, so it can be changed, and an empty
    // entry clears the value, returning it to the built-in one.
    state.select(2);
    press(&mut state, KeyCode::Char('u'));
    assert_eq!(
        state.editing().expect("a form").buffer,
        "http://gateway.invalid"
    );
    for _ in 0.."http://gateway.invalid".len() {
        press(&mut state, KeyCode::Backspace);
    }
    let command = press(&mut state, KeyCode::Enter).expect("a request");
    assert_eq!(command.body.expect("a body")["upstream"], Value::Null);
}

#[test]
fn an_endpoint_that_is_not_an_address_is_refused_before_it_is_sent() {
    let mut state = loaded();
    press(&mut state, KeyCode::Char('u'));
    type_in(&mut state, "gateway.invalid");
    assert!(ctrl_s(&mut state).is_none(), "a refused form is not saved");
    assert!(press(&mut state, KeyCode::Enter).is_none());
    let text = frame(&state, 120, 40, false);
    assert!(text.contains("not an address"), "{text}");
    assert!(text.contains("fix value to save"), "{text}");
}

#[test]
fn a_credential_goes_through_its_own_operation_and_is_never_echoed() {
    let mut state = loaded();
    press(&mut state, KeyCode::Char('e'));
    let editing = state.editing().expect("a form");
    assert!(editing.secret, "a credential entry is masked");
    assert!(editing.buffer.is_empty(), "a key is never read back");
    type_in(&mut state, "sk-secret");
    let text = frame(&state, 120, 40, false);
    assert!(!text.contains("sk-secret"), "never shown:\n{text}");
    assert!(text.contains("·········"), "masked:\n{text}");
    let command = ctrl_s(&mut state).expect("a request");
    assert_eq!(command.path, "credential");
    assert_eq!(command.control, "config-credential");
    let body = command.body.expect("a body");
    assert_eq!(body["literal"], json!("sk-secret"));
    assert_eq!(body["provider"], json!("anthropic_sub"));
    assert_eq!(body["env"], json!("TOKER_ANTHROPIC_SUB_TOKEN"));
}

#[test]
fn codex_holds_no_credential_to_enter_but_has_its_handshake_settings() {
    let mut state = loaded();
    state.select(2);
    press(&mut state, KeyCode::Char('e'));
    assert!(state.editing().is_none(), "it signs with the codex login");

    press(&mut state, KeyCode::Char('v'));
    type_in(&mut state, "9.9.9");
    let body = ctrl_s(&mut state).expect("a request").body.expect("a body");
    assert_eq!(body["client_version"], json!("9.9.9"));
    assert_eq!(body["refresh_url"], json!("http://login.invalid/token"));
    answered(&mut state, 200, Value::Null);

    press(&mut state, KeyCode::Char('f'));
    for _ in 0.."http://login.invalid/token".len() {
        press(&mut state, KeyCode::Backspace);
    }
    type_in(&mut state, "http://login.example/oauth");
    let body = ctrl_s(&mut state).expect("a request").body.expect("a body");
    assert_eq!(body["refresh_url"], json!("http://login.example/oauth"));
    answered(&mut state, 200, Value::Null);

    let body = press(&mut state, KeyCode::Char('p'))
        .expect("a request")
        .body
        .expect("a body");
    assert_eq!(body["version_probe"], json!(false), "on until switched off");

    // Another provider has none of them.
    state.select(1);
    assert!(press(&mut state, KeyCode::Char('v')).is_none());
    assert!(press(&mut state, KeyCode::Char('p')).is_none());
    assert!(state.editing().is_none());
}

/// The token shorthand is read as the dashboard writes it, so what is typed
/// is what is stored.
#[test]
fn a_token_count_accepts_the_shorthand() {
    for (typed, tokens) in [
        ("500k", 500_000u64),
        ("1M", 1_000_000),
        ("0.5M", 500_000),
        ("200000", 200_000),
        ("200,000", 200_000),
    ] {
        let mut state = settings_page();
        on(&mut state, Row::ColdMinTokens);
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(state.editing().expect("a form").buffer, "175k");
        for _ in 0..4 {
            press(&mut state, KeyCode::Backspace);
        }
        type_in(&mut state, typed);
        let command = ctrl_s(&mut state).unwrap_or_else(|| panic!("{typed} is saveable"));
        assert_eq!(
            command.body.expect("a body")["cold_min_tokens"],
            json!(tokens),
            "{typed}"
        );
    }
}

#[test]
fn a_token_count_larger_than_any_window_is_refused_as_it_is_typed() {
    let mut state = settings_page();
    on(&mut state, Row::ColdMinTokens);
    press(&mut state, KeyCode::Char('e'));
    for _ in 0..4 {
        press(&mut state, KeyCode::Backspace);
    }
    type_in(&mut state, "2M");
    assert!(ctrl_s(&mut state).is_none(), "nothing is sent");
    assert!(frame(&state, 120, 40, false).contains("more than any configured model's context, 1M"),);
    // Fixed as it is typed, it can be saved.
    press(&mut state, KeyCode::Backspace);
    type_in(&mut state, "00k");
    assert!(ctrl_s(&mut state).is_some());
}

#[test]
fn a_setting_that_is_not_a_number_is_refused_before_it_is_sent() {
    let mut state = settings_page();
    on(&mut state, Row::ColdMinTokens);
    press(&mut state, KeyCode::Char('e'));
    for _ in 0..4 {
        press(&mut state, KeyCode::Backspace);
    }
    type_in(&mut state, "lots");
    assert!(ctrl_s(&mut state).is_none());
    let text = frame(&state, 120, 40, false);
    assert!(text.contains("is not a token count"), "{text}");
    assert!(state.editing().is_some(), "the form is kept as typed");
}

/// What the service refuses on save shows beside the field with what was
/// typed still in it, and the header is left alone
/// (spec: UTUI#the-configuration-pages).
#[test]
fn the_services_refusal_is_kept_beside_the_field() {
    let mut state = settings_page();
    on(&mut state, Row::ColdIdleMin);
    press(&mut state, KeyCode::Char('e'));
    type_in(&mut state, "45");
    assert!(ctrl_s(&mut state).is_some());
    let next = answered(
        &mut state,
        400,
        json!({"setting": "configuration", "error": "an idle floor must be under the TTL"}),
    );
    assert!(next.is_none(), "a refusal is not followed by a read");
    let editing = state.editing().expect("the form is kept");
    assert_eq!(editing.buffer, "45");
    assert_eq!(
        editing.refusal.as_deref(),
        Some("configuration: an idle floor must be under the TTL")
    );
    assert!(state.error().is_none(), "said beside the field, not above");
    let text = frame(&state, 120, 40, false);
    assert!(
        text.contains("an idle floor must be under the TTL"),
        "{text}"
    );
    // The same value cannot be sent again until it changes.
    assert!(ctrl_s(&mut state).is_none());
    // And the stored value is as it was.
    assert!(
        state
            .snapshot()
            .expect("loaded")
            .settings
            .cold_idle_min
            .is_none()
    );
    // Editing the value lifts the refusal.
    press(&mut state, KeyCode::Backspace);
    assert!(state.editing().expect("a form").refusal.is_none());
    assert!(ctrl_s(&mut state).is_some());
}

#[test]
fn escape_cancels_a_form_and_sends_nothing() {
    let mut state = settings_page();
    on(&mut state, Row::PingHeader);
    press(&mut state, KeyCode::Char('e'));
    type_in(&mut state, "x");
    assert!(press(&mut state, KeyCode::Esc).is_none());
    assert!(state.editing().is_none());
}

#[test]
fn a_header_list_is_typed_comma_separated_and_sent_as_a_list() {
    let mut state = settings_page();
    on(&mut state, Row::SessionHeaders);
    press(&mut state, KeyCode::Char('e'));
    assert_eq!(
        state.editing().expect("a form").buffer,
        "x-toker-session, x-claude-code-session-id, x-session-id"
    );
    for _ in 0..state.editing().expect("a form").buffer.len() {
        press(&mut state, KeyCode::Backspace);
    }
    type_in(&mut state, "X-Mine, x-other");
    let body = ctrl_s(&mut state).expect("a request").body.expect("a body");
    assert_eq!(body["session_headers"], json!(["x-mine", "x-other"]));
    // The rest of the row is carried.
    assert_eq!(body["ping_header"], json!("x-toker-ping"));
}

#[test]
fn a_header_with_a_space_in_it_is_refused() {
    let mut state = settings_page();
    on(&mut state, Row::PingHeader);
    press(&mut state, KeyCode::Char('e'));
    type_in(&mut state, " two words");
    assert!(ctrl_s(&mut state).is_none());
}

#[test]
fn transcript_roots_are_edited_as_comma_separated_paths() {
    let mut state = settings_page();
    on(&mut state, Row::TranscriptRoots);
    press(&mut state, KeyCode::Char('e'));
    type_in(&mut state, "/harness/.claude, ~/other");
    let body = ctrl_s(&mut state).expect("a request").body.expect("a body");
    assert_eq!(
        body["transcript_roots"],
        json!(["/harness/.claude", "~/other"])
    );

    let mut state = settings_page();
    on(&mut state, Row::TranscriptRoots);
    press(&mut state, KeyCode::Char('e'));
    type_in(&mut state, "relative");
    assert!(ctrl_s(&mut state).is_none(), "a relative path is refused");
}

#[test]
fn an_empty_entry_puts_a_list_back_to_unset() {
    let mut state = settings_page();
    on(&mut state, Row::SessionHeaders);
    press(&mut state, KeyCode::Char('e'));
    for _ in 0..state.editing().expect("a form").buffer.len() {
        press(&mut state, KeyCode::Backspace);
    }
    let body = ctrl_s(&mut state).expect("a request").body.expect("a body");
    assert_eq!(body["session_headers"], Value::Null);
}

#[test]
fn a_notice_style_is_set_for_one_profile_through_its_own_operation() {
    let mut state = settings_page();
    let at = state
        .snapshot()
        .expect("loaded")
        .profiles()
        .iter()
        .position(|profile| profile == "opencode")
        .expect("a profile");
    on(&mut state, Row::Notice(at));
    press(&mut state, KeyCode::Enter);
    type_in(&mut state, "fancy");
    assert!(
        ctrl_s(&mut state).is_none(),
        "not a style the service renders"
    );
    for _ in 0..5 {
        press(&mut state, KeyCode::Backspace);
    }
    type_in(&mut state, "Plain");
    let command = ctrl_s(&mut state).expect("a request");
    assert_eq!(command.path, "notice");
    assert_eq!(
        command.body.expect("a body"),
        json!({"profile": "opencode", "style": "plain"})
    );
}

#[test]
fn the_default_notice_style_is_part_of_the_settings_row() {
    let mut state = settings_page();
    on(&mut state, Row::NoticeDefault);
    press(&mut state, KeyCode::Char('e'));
    type_in(&mut state, "toker");
    let body = ctrl_s(&mut state).expect("a request").body.expect("a body");
    assert_eq!(body["notice_default"], json!("toker"));
}

#[test]
fn a_toggle_is_not_a_form() {
    let mut state = settings_page();
    on(&mut state, Row::Cold);
    assert!(press(&mut state, KeyCode::Char('e')).is_none());
    assert!(state.editing().is_none());
    assert!(
        press(&mut state, KeyCode::Enter).is_some(),
        "enter switches it"
    );
}

#[test]
fn a_refusal_of_a_toggle_is_said_above_the_page() {
    let mut state = settings_page();
    press(&mut state, KeyCode::Char(' '));
    answered(
        &mut state,
        400,
        json!({"setting": "configuration", "error": "refused"}),
    );
    assert_eq!(state.error(), Some("configuration: refused"));
}

#[test]
fn a_refusal_names_the_setting_and_changes_nothing() {
    let mut state = loaded();
    let before = state.snapshot().cloned();
    let next = answered(
        &mut state,
        400,
        json!({"setting": "provider", "error": "not a url at all"}),
    );
    assert!(next.is_none(), "a refusal is not followed by a read");
    assert_eq!(state.error(), Some("provider: not a url at all"));
    assert_eq!(state.snapshot().cloned(), before);
}

// --- safeguards ------------------------------------------------------------------

fn clear_reply(status: u16) -> Reply {
    Reply::Command(
        Command {
            name: "safeguards-clear",
            method: Method::POST,
            path: "/safeguards/mark/clear",
            control: "safeguards-mark-clear",
            body: None,
        },
        Ok(Response {
            status,
            body: json!({"toker": "safeguards-mark-clear", "ok": true, "cleared": true}),
        }),
    )
}

#[test]
fn the_detail_says_each_messages_bindings_support_and_mark() {
    let mut state = loaded_with_routing();
    let text = frame(&state, 120, 40, false);
    assert!(
        text.contains("anthropic_sub.anthropic_messages · subscription · safeguards supported"),
        "{text}"
    );
    on_provider(&mut state, "openrouter");
    let text = frame(&state, 120, 40, false);
    assert!(
        text.contains(
            "openrouter.anthropic_messages · billed · safeguards unknown, marked unsupported"
        ),
        "{text}"
    );
    // A binding that does not speak Messages has no support to declare.
    assert!(
        text.contains("openrouter.openai_chat · billed\n")
            || text.contains("openrouter.openai_chat · billed ")
    );
    assert!(text.contains("[c] clear safeguards mark"), "{text}");
}

#[test]
fn a_service_that_reports_no_safeguards_leaves_the_support_out() {
    let mut body = snapshot_body();
    body.as_object_mut().expect("object").remove("safeguards");
    let mut state = State::default();
    state.receive(Reply::Command(
        Command::snapshot("config-snapshot"),
        ok(body),
    ));
    assert!(state.snapshot().expect("loaded").safeguards.is_empty());
    assert!(!frame(&state, 120, 40, false).contains("safeguards"));
}

#[test]
fn c_clears_the_marks_of_the_focused_providers_bindings() {
    let mut state = loaded();
    on_provider(&mut state, "openrouter");
    let command = press(&mut state, KeyCode::Char('c')).expect("a request");
    assert_eq!(command.name, "safeguards-clear");
    assert_eq!(command.method, Method::POST);
    assert_eq!(command.path, "/safeguards/mark/clear");
    assert_eq!(command.control, "safeguards-mark-clear");
    assert_eq!(
        command.body.expect("a body"),
        json!({"binding_id": "openrouter.anthropic_messages"})
    );
    // Nothing is applied locally: the mark shows until the service says.
    assert!(
        state.snapshot().expect("loaded").safeguards[2]
            .marked_ms
            .is_some()
    );
    let next = state.receive(clear_reply(200)).expect("a refresh follows");
    assert_eq!(next.name, "snapshot");
}

#[test]
fn c_on_a_provider_with_no_mark_says_so_and_sends_nothing() {
    let mut state = loaded();
    assert!(press(&mut state, KeyCode::Char('c')).is_none());
    assert_eq!(
        state.error(),
        Some("anthropic_sub: no rejection mark is held")
    );
    // Neither does a provider with no Messages binding at all.
    on_provider(&mut state, "codex_sub");
    assert!(press(&mut state, KeyCode::Char('c')).is_none());
    assert_eq!(state.error(), Some("codex_sub: no rejection mark is held"));
}

#[test]
fn marks_on_several_bindings_clear_one_request_after_another() {
    let mut state = State::default();
    let mut body = snapshot_body();
    body["safeguards"] = json!([
        {"binding_id": "openrouter.a", "provider": "openrouter",
         "declared": "unknown", "marked_ms": 1},
        {"binding_id": "openrouter.b", "provider": "openrouter",
         "declared": "unknown", "marked_ms": 2},
    ]);
    state.receive(Reply::Command(
        Command::snapshot("config-snapshot"),
        ok(body),
    ));
    on_provider(&mut state, "openrouter");
    let first = press(&mut state, KeyCode::Char('c')).expect("a request");
    assert_eq!(
        first.body.expect("a body")["binding_id"],
        json!("openrouter.a")
    );
    let second = state.receive(clear_reply(200)).expect("the next one");
    assert_eq!(second.name, "safeguards-clear");
    assert_eq!(
        second.body.expect("a body")["binding_id"],
        json!("openrouter.b")
    );
    let refresh = state.receive(clear_reply(200)).expect("then the read");
    assert_eq!(refresh.name, "snapshot");
}

#[test]
fn a_refused_clear_stops_the_chain_and_is_reported() {
    let mut state = loaded();
    on_provider(&mut state, "openrouter");
    press(&mut state, KeyCode::Char('c'));
    let mut reply = clear_reply(400);
    if let Reply::Command(_, Ok(response)) = &mut reply {
        response.body = json!({"setting": "safeguards", "error": "the request does not parse"});
    }
    assert!(state.receive(reply).is_none());
    assert_eq!(
        state.error(),
        Some("safeguards: the request does not parse")
    );
}

// --- moving about ------------------------------------------------------------------

#[test]
fn enter_on_a_provider_asks_for_the_first_route_that_reaches_it() {
    let mut state = loaded_with_routing();
    assert!(press(&mut state, KeyCode::Enter).is_none());
    assert_eq!(state.take_goto().as_deref(), Some("claude-sub"));
    assert_eq!(state.take_goto(), None, "once");
    // Not where routing was not read: there is no route to name.
    let mut state = loaded();
    press(&mut state, KeyCode::Enter);
    assert_eq!(state.take_goto(), None);
}

#[test]
fn switching_views_puts_the_selection_back_at_the_top() {
    let mut state = loaded();
    state.move_selection(2);
    assert_eq!(state.selected(), 3);
    press(&mut state, KeyCode::Char('2'));
    assert_eq!(state.view(), View::Settings);
    assert_eq!(state.selected(), 0);
}
