//! The history tab: the revisions and their diffs in words, reactivating one,
//! tracing under one, and moving the running lanes onto the live revision
//! with the second confirmation that costs a cache
//! (spec: UTUI#history).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};

use super::model::{State, Tab};
use super::testing::{self, World};
use super::view::render;
use crate::tui::control::{Command, Reply, Response};

fn reply(state: &mut State, name: &'static str, status: u16, body: Value) {
    let mut command = Command::snapshot("routing-snapshot");
    command.name = name;
    state.receive(Reply::Command(command, Ok(Response { status, body })));
}

/// The page on the history tab, after a save, with two warm lanes left on
/// the revision before it.
pub(super) fn after_a_save() -> State {
    let mut state = State::default();
    reply(&mut state, "snapshot", 200, testing::snapshot(World::SAVED));
    reply(
        &mut state,
        "revisions",
        200,
        testing::revisions(World::SAVED),
    );
    state.set_tab(Tab::History);
    state
}

fn frame(state: &State, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| {
            render(frame, state, false);
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

fn press(state: &mut State, code: KeyCode) -> Option<Command> {
    state.key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn key(state: &mut State, key: char) -> Option<Command> {
    press(state, KeyCode::Char(key))
}

/// The snapshot with its lanes as `revisions` says.
fn with_lanes(revisions: Value) -> State {
    let mut snapshot = testing::snapshot(World::SAVED);
    snapshot["derived"]["earlier_lanes"] = testing::earlier_lanes(&revisions, 10);
    snapshot["derived"]["revisions"] = revisions;
    let mut state = State::default();
    reply(&mut state, "snapshot", 200, snapshot);
    reply(
        &mut state,
        "revisions",
        200,
        testing::revisions(World::SAVED),
    );
    state.set_tab(Tab::History);
    state
}

#[test]
fn the_revisions_are_listed_newest_first_with_what_changed_and_the_lanes() {
    let state = after_a_save();
    insta::assert_snapshot!(frame(&state, 120, 40));
}

#[test]
fn a_revisions_detail_is_its_diff_in_words() {
    let mut state = after_a_save();
    // Rev 8 moved a rule.
    state.move_selection(2);
    let text = frame(&state, 120, 40);
    assert!(text.contains("↕ rewrite anthropic_messages 2"), "{text}");
    assert!(text.contains("[a] reactivate"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn the_first_revision_is_the_setup_and_changed_nothing() {
    let mut state = after_a_save();
    state.page(true);
    let text = frame(&state, 120, 40);
    assert!(text.contains("the first revision"), "{text}");
    assert!(text.contains("setup: 7 routes"), "{text}");
}

#[test]
fn a_reactivation_is_made_against_the_live_revision_and_says_what_it_made() {
    let mut state = after_a_save();
    state.move_selection(1);
    let command = key(&mut state, 'a').expect("reactivating needs no second question");
    assert_eq!(command.name, "reactivate");
    assert_eq!(command.path, "reactivate");
    assert_eq!(
        command.body.clone().unwrap(),
        json!({"expected_revision_id": 10, "revision_id": 9}),
        "new and cold lanes as a save is: the body names no scope"
    );
    state.receive(Reply::Command(
        command,
        Ok(Response {
            status: 200,
            body: json!({"active_revision": {"revision_id": 11}}),
        }),
    ));
    assert_eq!(state.active_revision_id(), Some(11));
    assert_eq!(state.notice(), Some("reactivated rev 9 as rev 11"));
}

#[test]
fn the_live_revision_is_already_live() {
    let mut state = after_a_save();
    assert!(key(&mut state, 'a').is_none());
    assert_eq!(state.notice(), Some("rev 10 is already live"));
    assert!(frame(&state, 120, 40).contains("[a] already live"));
}

#[test]
fn a_reactivation_that_finds_another_revision_live_says_so_and_reads_again() {
    let mut state = after_a_save();
    state.move_selection(1);
    let command = key(&mut state, 'a').unwrap();
    state.receive(Reply::Command(
        command,
        Ok(Response {
            status: 409,
            body: json!({"error": "stale_revision", "live_revision_id": 11}),
        }),
    ));
    assert!(state.error().unwrap().contains("revision 11 is now live"));
    assert!(state.take_refresh_wanted());
    assert!(!state.in_flight(), "the page is usable again");
}

#[test]
fn c_traces_under_the_selected_revision_and_the_live_one_under_none() {
    let mut state = after_a_save();
    state.move_selection(1);
    let listing = key(&mut state, 'c').expect("recent requests are read");
    assert_eq!(listing.body.unwrap()["mode"], "recent_requests");
    reply(&mut state, "recent", 200, json!({"requests": []}));
    // The invented request, from its form.
    press(&mut state, KeyCode::Enter);
    let trace = press(&mut state, KeyCode::Enter).expect("traced");
    assert_eq!(
        trace.body.clone().unwrap()["against"],
        json!({"kind": "revision", "revision_id": 9})
    );
    // And the live revision, which the tables show, is the active one.
    let mut live = after_a_save();
    key(&mut live, 'c');
    reply(&mut live, "recent", 200, json!({"requests": []}));
    press(&mut live, KeyCode::Enter);
    let trace = press(&mut live, KeyCode::Enter).unwrap();
    assert_eq!(trace.body.unwrap()["against"], json!({"kind": "active"}));
}

#[test]
fn a_trace_under_an_earlier_revision_draws_its_stages_and_names_the_revision() {
    let mut state = after_a_save();
    state.move_selection(1);
    key(&mut state, 'c');
    reply(&mut state, "recent", 200, json!({"requests": []}));
    press(&mut state, KeyCode::Enter);
    let trace = press(&mut state, KeyCode::Enter).unwrap();
    state.receive(Reply::Command(
        trace,
        Ok(Response {
            status: 200,
            body: json!({"mode": "resolve", "request": {"protocol": "anthropic_messages", "profile": null, "model": "claude-opus-5"},
                "result": {"outcome": "unresolved", "code": "no_route"}}),
        }),
    ));
    let text = frame(&state, 120, 40);
    assert!(text.contains("under rev 9"), "{text}");
    assert!(
        text.contains("refused · no presentation, rewrite or default catches it"),
        "{text}"
    );
}

#[test]
fn m_arms_first_and_names_the_lanes_whose_cache_it_would_lose() {
    let mut state = after_a_save();
    assert!(
        key(&mut state, 'm').is_none(),
        "nothing is sent by the first press"
    );
    assert!(state.armed().is_some());
    let text = frame(&state, 120, 40);
    assert!(
        text.contains("2 lanes lose their cache: ses-hot, ses-crow"),
        "{text}"
    );
    assert!(
        text.contains("[m] again to move them to rev 10 · [esc] cancel"),
        "{text}"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn a_second_m_moves_the_lanes_with_the_consent_it_asked_for() {
    let mut state = after_a_save();
    key(&mut state, 'm');
    let command = key(&mut state, 'm').expect("confirmed");
    assert_eq!(command.name, "widen");
    assert_eq!(
        command.body.unwrap(),
        json!({"expected_revision_id": 10, "scope": "immediate", "force_cache_transition": true})
    );
    assert!(state.in_flight());
    reply(
        &mut state,
        "widen",
        200,
        json!({"revision_id": 10, "activation": {"scope": "immediate"}}),
    );
    assert_eq!(state.notice(), Some("moved running lanes onto rev 10"));
    assert!(
        state.take_refresh_wanted(),
        "where the lanes are is read again"
    );
    assert!(state.armed().is_none());
}

#[test]
fn a_confirmation_is_for_the_revision_it_named_not_one_that_went_live_since() {
    let mut state = after_a_save();
    key(&mut state, 'm');
    // A refresh lands between the two presses with another revision live.
    let mut snapshot = testing::snapshot(World::SAVED);
    snapshot["active_revision"]["revision_id"] = json!(11);
    reply(&mut state, "snapshot", 200, snapshot);
    let command = key(&mut state, 'm').expect("confirmed");
    assert_eq!(
        command.body.unwrap()["expected_revision_id"],
        10,
        "consent was given for rev 10's lanes, so rev 11 is refused as stale"
    );
}

/// The prompt after `m`, with `lanes` at risk on the earlier revision.
fn prompt_naming(lanes: Value) -> String {
    let mut snapshot = testing::snapshot(World::SAVED);
    let revisions = json!([
        {"revision_id": 9, "warm": 2, "cold": 0, "unknown": 0, "total": 2, "at_risk": lanes},
        {"revision_id": 10, "warm": 0, "cold": 1, "unknown": 0, "total": 1, "at_risk": []},
    ]);
    snapshot["derived"]["earlier_lanes"] = testing::earlier_lanes(&revisions, 10);
    snapshot["derived"]["revisions"] = revisions;
    let mut state = State::default();
    reply(&mut state, "snapshot", 200, snapshot);
    reply(
        &mut state,
        "revisions",
        200,
        testing::revisions(World::SAVED),
    );
    state.set_tab(Tab::History);
    key(&mut state, 'm');
    frame(&state, 160, 40)
}

#[test]
fn long_lane_names_are_cut_in_the_middle_so_their_ends_still_tell_them_apart() {
    let text = prompt_naming(json!([
        "session-with-a-long-prefix-one",
        "session-with-a-long-prefix-two"
    ]));
    assert!(text.contains("session…ix-one, session…ix-two"), "{text}");
}

#[test]
fn lanes_that_would_read_the_same_once_cut_are_named_in_full() {
    let text = prompt_naming(json!([
        "session-aaaaaaaaaa-with-a-shared-tail",
        "session-bbbbbbbbbb-with-a-shared-tail"
    ]));
    assert!(
        text.contains(
            "session-aaaaaaaaaa-with-a-shared-tail, session-bbbbbbbbbb-with-a-shared-tail"
        ),
        "{text}"
    );
}

#[test]
fn esc_or_any_other_key_cancels_the_arming_and_sends_nothing() {
    let mut state = after_a_save();
    key(&mut state, 'm');
    assert!(state.armed().is_some());
    assert!(press(&mut state, KeyCode::Esc).is_none());
    assert!(state.armed().is_none());
    assert!(key(&mut state, 'm').is_none(), "it arms again, afresh");
    state.move_selection(1);
    assert!(
        state.armed().is_none(),
        "a move of the selection cancels it"
    );
    assert!(
        key(&mut state, 'm').is_none(),
        "and a press after it only arms"
    );
}

#[test]
fn a_move_that_loses_no_cache_goes_at_once_without_consent() {
    let mut state = with_lanes(json!([
        {"revision_id": 9, "warm": 0, "cold": 3, "unknown": 0, "total": 3, "at_risk": []},
        {"revision_id": 10, "warm": 0, "cold": 1, "unknown": 0, "total": 1, "at_risk": []},
    ]));
    let command = key(&mut state, 'm').expect("nothing to confirm");
    assert_eq!(command.body.unwrap()["force_cache_transition"], false);
}

#[test]
fn with_no_lane_on_an_earlier_revision_there_is_nothing_to_move() {
    let mut state = with_lanes(json!([
        {"revision_id": 10, "warm": 1, "cold": 0, "unknown": 0, "total": 1, "at_risk": ["ses-a"]},
    ]));
    assert!(key(&mut state, 'm').is_none());
    assert!(state.armed().is_none());
    assert_eq!(
        state.notice(),
        Some("no lane is on an earlier revision than rev 10")
    );
}

#[test]
fn a_revision_that_already_moves_lanes_at_once_says_so() {
    let mut snapshot = testing::snapshot(World::SAVED);
    snapshot["activation"] = json!({"scope": "immediate", "forced_cache_transition": true});
    let mut state = State::default();
    reply(&mut state, "snapshot", 200, snapshot);
    reply(
        &mut state,
        "revisions",
        200,
        testing::revisions(World::SAVED),
    );
    state.set_tab(Tab::History);
    assert!(key(&mut state, 'm').is_none());
    assert!(
        state
            .notice()
            .unwrap()
            .contains("already moves running lanes")
    );
}

#[test]
fn the_daemon_asking_for_consent_the_page_did_not_foresee_arms_the_move() {
    // The lanes that would lose a cache are not all on earlier revisions, so
    // the page saw none at risk and asked for nothing.
    let mut state = with_lanes(json!([
        {"revision_id": 9, "warm": 0, "cold": 3, "unknown": 0, "total": 3, "at_risk": []},
    ]));
    let command = key(&mut state, 'm').expect("sent without consent");
    state.receive(Reply::Command(
        command,
        Ok(Response {
            status: 409,
            body: json!({"error": "forced_cache_transition_required",
                "lane_impacts": {"total": 5, "warm": 1, "cold": 3, "unknown": 1,
                    "unpinned": 1, "at_risk": 2}}),
        }),
    ));
    assert!(!state.in_flight());
    let armed = state.armed().expect("armed by the refusal");
    assert!(armed.lanes.is_empty(), "the daemon named none");
    assert_eq!(armed.count, 2);
    let text = frame(&state, 120, 40);
    assert!(text.contains("2 lanes lose their cache"), "{text}");
    let confirmed = key(&mut state, 'm').unwrap();
    assert_eq!(confirmed.body.unwrap()["force_cache_transition"], true);
}

#[test]
fn a_move_that_finds_another_revision_live_changes_nothing_and_reads_again() {
    let mut state = after_a_save();
    key(&mut state, 'm');
    let command = key(&mut state, 'm').unwrap();
    state.receive(Reply::Command(
        command,
        Ok(Response {
            status: 409,
            body: json!({"error": "stale_revision", "live_revision_id": 11}),
        }),
    ));
    assert!(
        state.armed().is_none(),
        "a new decision is asked for afresh"
    );
    assert!(state.error().unwrap().contains("revision 11 is now live"));
    assert!(state.take_refresh_wanted());
}

#[test]
fn the_keys_do_nothing_while_a_move_is_on_its_way() {
    let mut state = after_a_save();
    key(&mut state, 'm');
    key(&mut state, 'm');
    assert!(key(&mut state, 'a').is_none());
    assert!(key(&mut state, 'm').is_none());
}
