//! Tracing a request from the routing page: asking, the answer's frames, the
//! marks it leaves on the tables, and following it to the record that
//! caught the request (spec: UTUI#tracing-a-request).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};

use super::model::{Modal, State, Tab};
use super::testing::{self, CODEX, OR_MESSAGES, SUB, World};
use super::view::render;
use crate::tui::control::{Command, Reply, Response};

pub(super) fn loaded(world: World, tab: Tab) -> State {
    let mut state = State::default();
    let mut reply = |name: &'static str, body| {
        let mut command = Command::snapshot("routing-snapshot");
        command.name = name;
        state.receive(Reply::Command(command, Ok(Response { status: 200, body })));
    };
    reply("snapshot", testing::snapshot(world));
    reply("revisions", testing::revisions(world));
    state.set_tab(tab);
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

fn typed(state: &mut State, text: &str) {
    for character in text.chars() {
        press(state, KeyCode::Char(character));
    }
}

fn answer(state: &mut State, command: Command, status: u16, body: Value) {
    state.receive(Reply::Command(command, Ok(Response { status, body })));
}

/// The ledger's recent requests: one replayable, one from before the facts
/// were recorded.
fn recent() -> Value {
    json!({"toker": "routing-preview", "ok": true, "mode": "recent_requests", "requests": [
        {"id": 41, "ts_ms": 1_762_785_660_000_i64, "replayable": true, "not_replayable_reason": null,
         "requested_model": "anthropic_sub/claude-sonnet-5",
         "routed": {"binding_id": OR_MESSAGES, "effective_model": "anthropic/claude-sonnet-5"}},
        {"id": 7, "ts_ms": 1_762_700_000_000_i64, "replayable": false,
         "not_replayable_reason": "facts_not_recorded",
         "requested_model": "claude-opus-5",
         "routed": {"binding_id": SUB, "effective_model": "claude-opus-5"}},
    ]})
}

/// What the preview answers: the request, then the resolver's walk.
fn walked(request: Value, result: Value) -> Value {
    json!({"toker": "routing-preview", "ok": true, "mode": "resolve",
        "active_revision_id": 9, "against_revision_id": 9, "diagnostics": [],
        "request": request, "result": result})
}

fn facts(profile: Value, protocol: &str, model: &str) -> Value {
    json!({"profile": profile, "protocol": protocol, "model": model,
        "prompt_bound": 6883, "message_count": null, "tool_count": 0,
        "required_capabilities": {}, "asks_server_safeguards": false,
        "binding_affinity": null, "class": "inference"})
}

fn candidate(binding: &str, provider: &str, skipped: Option<&str>) -> Value {
    json!({"kind": "candidate", "candidate_id": "c", "crossed_from": null,
        "binding_id": binding, "provider": provider, "model": "m", "skipped": skipped,
        "server_safeguards": null})
}

/// The request of the design's trace frame: caught by a rule whose route
/// falls to its second candidate.
pub(super) fn fell_to_the_second() -> Value {
    walked(
        facts(
            json!("claude"),
            "anthropic_messages",
            "anthropic_sub/claude-sonnet-5",
        ),
        json!({
            "outcome": "selected", "route_id": "claude-sub", "candidate_id": "openrouter",
            "binding_id": OR_MESSAGES, "provider": "openrouter", "cost": "billed", "billed": true,
            "baseline_model": "claude-sonnet-5", "upgraded_model": null,
            "aliased_model": "anthropic/claude-sonnet-5",
            "effective_model": "anthropic/claude-sonnet-5",
            "provenance": {"kind": "rewrite", "scope": "protocol", "id": "anthropic-messages-2"},
            "trace": [
                {"kind": "lookup",
                 "provenance": {"kind": "rewrite", "scope": "protocol", "id": "anthropic-messages-2"}},
                candidate(SUB, "anthropic_sub", Some("no_credential")),
                candidate(OR_MESSAGES, "openrouter", None),
                {"kind": "alias", "from": "claude-sonnet-5", "to": "anthropic/claude-sonnet-5"},
            ],
        }),
    )
}

fn served_first() -> Value {
    walked(
        facts(
            Value::Null,
            "anthropic_messages",
            "openrouter/anthropic/claude-sonnet-5",
        ),
        json!({
            "outcome": "selected", "route_id": "openrouter-messages", "candidate_id": "openrouter",
            "binding_id": OR_MESSAGES, "provider": "openrouter", "cost": "billed", "billed": true,
            "baseline_model": "anthropic/claude-sonnet-5", "upgraded_model": null,
            "aliased_model": null, "effective_model": "anthropic/claude-sonnet-5",
            "provenance": {"kind": "rewrite", "scope": "protocol", "id": "anthropic-messages-4"},
            "trace": [
                {"kind": "lookup",
                 "provenance": {"kind": "rewrite", "scope": "protocol", "id": "anthropic-messages-4"}},
                candidate(OR_MESSAGES, "openrouter", None),
            ],
        }),
    )
}

fn refused() -> Value {
    walked(
        facts(Value::Null, "openai_responses", "mystery-model"),
        json!({"outcome": "unresolved", "code": "no_route"}),
    )
}

fn nothing_can_serve() -> Value {
    walked(
        facts(Value::Null, "openai_responses", "codex_sub/gpt-5.6-sol"),
        json!({
            "outcome": "unavailable", "route_id": "codex",
            "provenance": {"kind": "rewrite", "scope": "protocol", "id": "openai-responses-1"},
            "trace": [
                {"kind": "lookup",
                 "provenance": {"kind": "rewrite", "scope": "protocol", "id": "openai-responses-1"}},
                candidate(CODEX, "codex_sub", Some("provider_off")),
            ],
        }),
    )
}

/// Ask for a trace of the first (invented) choice and answer the form with
/// `walk`.
pub(super) fn trace_invented(state: &mut State, walk: Value) -> Command {
    let listing = press(state, KeyCode::Char('t')).expect("the recent requests are asked for");
    assert_eq!(listing.name, "recent");
    answer(state, listing, 200, recent());
    assert!(press(state, KeyCode::Enter).is_none(), "the form opens");
    let command = press(state, KeyCode::Enter).expect("the form is submitted");
    assert_eq!(command.name, "trace");
    answer(state, command.clone(), 200, walk);
    command
}

#[test]
fn t_reads_the_recent_requests_and_offers_an_invented_one_first() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    let listing = press(&mut state, KeyCode::Char('t')).unwrap();
    assert_eq!(listing.path, "preview");
    assert_eq!(
        listing.body.as_ref().unwrap()["mode"],
        "recent_requests",
        "the same preview the editor uses"
    );
    assert!(matches!(state.modal(), Modal::Trace(_)));
    insta::assert_snapshot!(frame(&state, 120, 40));
    answer(&mut state, listing, 200, recent());
    insta::assert_snapshot!("the_recent_requests_are_listed", frame(&state, 120, 40));
}

#[test]
fn an_invented_request_is_asked_for_in_a_small_form_and_sent_as_described() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    // The form starts from the rule selected: its protocol and a name it
    // catches.
    state.move_selection(1);
    let listing = press(&mut state, KeyCode::Char('t')).unwrap();
    answer(&mut state, listing, 200, recent());
    press(&mut state, KeyCode::Enter);
    insta::assert_snapshot!("the_invented_request_form", frame(&state, 120, 40));
    let command = press(&mut state, KeyCode::Enter).unwrap();
    let body = command.body.unwrap();
    assert_eq!(body["mode"], "resolve");
    assert_eq!(body["against"], json!({"kind": "active"}));
    assert_eq!(body["request"]["kind"], "invented");
    assert_eq!(body["request"]["protocol"], "anthropic_messages");
    assert_eq!(body["request"]["model"], "anthropic_sub/claude-sonnet-5");
    // Nothing the operator left blank is described to the resolver.
    assert!(body["request"].get("profile").is_none());
    assert!(body["request"].get("prompt_bound").is_none());
    assert!(body["request"].get("asks_server_safeguards").is_none());
}

#[test]
fn a_recent_request_is_traced_by_its_ledger_row() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    let listing = press(&mut state, KeyCode::Char('t')).unwrap();
    answer(&mut state, listing, 200, recent());
    press(&mut state, KeyCode::Down);
    let command = press(&mut state, KeyCode::Enter).unwrap();
    assert_eq!(
        command.body.unwrap()["request"],
        json!({"kind": "recorded", "request_id": 41})
    );
}

#[test]
fn a_request_that_fell_to_a_later_candidate_is_marked_in_the_table() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    trace_invented(&mut state, fell_to_the_second());
    insta::assert_snapshot!(frame(&state, 120, 40));
}

#[test]
fn a_request_the_first_candidate_served() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    trace_invented(&mut state, served_first());
    insta::assert_snapshot!(frame(&state, 120, 40));
}

#[test]
fn a_request_nothing_catches_is_refused_and_falls_past_every_rule() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    trace_invented(&mut state, refused());
    insta::assert_snapshot!(frame(&state, 120, 40));
}

#[test]
fn a_route_whose_candidates_are_all_unavailable_is_refused() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    trace_invented(&mut state, nothing_can_serve());
    insta::assert_snapshot!(frame(&state, 120, 40));
}

#[test]
fn a_recent_request_that_cannot_be_replayed_says_so_and_is_not_traced() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    let listing = press(&mut state, KeyCode::Char('t')).unwrap();
    answer(&mut state, listing, 200, recent());
    press(&mut state, KeyCode::Down);
    press(&mut state, KeyCode::Down);
    let command = press(&mut state, KeyCode::Enter).unwrap();
    answer(
        &mut state,
        command,
        200,
        json!({"toker": "routing-preview", "ok": true, "mode": "resolve",
            "active_revision_id": 9, "against_revision_id": 9, "diagnostics": [],
            "request": null,
            "result": {"outcome": "not_replayable", "reason": "facts_not_recorded"}}),
    );
    let text = frame(&state, 120, 40);
    assert!(text.contains("not replayable"), "{text}");
    assert!(!text.contains('>'), "nothing is marked: {text}");
    insta::assert_snapshot!(text);
}

#[test]
fn the_marks_are_where_the_trace_went_on_every_lookup_tab() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    trace_invented(&mut state, fell_to_the_second());
    let active = state.active().unwrap().clone();
    let trace = state.trace().unwrap();
    let live = state.active_revision_id();
    let rewrites = trace.marks(&active, Tab::Rewrites, live);
    // Rule 1 was passed, rule 2 caught it, and what is after it was not
    // looked at.
    assert_eq!(rewrites.get(&0), Some(&'↓'));
    assert_eq!(rewrites.get(&1), Some(&'>'));
    assert_eq!(rewrites.len(), 2);
    // Every presentation of the request's scope was looked at and missed.
    let presentations = trace.marks(&active, Tab::Presentations, live);
    assert!(!presentations.is_empty());
    assert!(presentations.values().all(|mark| *mark == '↓'));
    // The route it walked.
    let routes = trace.marks(&active, Tab::Routes, live);
    assert_eq!(routes.values().collect::<Vec<_>>(), [&'>']);
    // It was caught before the defaults, which are not marked.
    assert!(trace.marks(&active, Tab::Defaults, live).is_empty());
}

#[test]
fn a_default_that_caught_it_is_marked_with_every_rule_it_fell_past() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    let default = walked(
        facts(Value::Null, "anthropic_messages", "claude-sonnet-5"),
        json!({
            "outcome": "selected", "route_id": "claude-sub", "candidate_id": "openrouter",
            "binding_id": OR_MESSAGES, "provider": "openrouter", "cost": "billed", "billed": true,
            "baseline_model": "claude-sonnet-5", "upgraded_model": null, "aliased_model": null,
            "effective_model": "claude-sonnet-5",
            "provenance": {"kind": "default", "scope": "protocol", "id": null},
            "trace": [{"kind": "lookup",
                "provenance": {"kind": "default", "scope": "protocol", "id": null}}],
        }),
    );
    trace_invented(&mut state, default);
    let active = state.active().unwrap().clone();
    let trace = state.trace().unwrap();
    let rewrites = trace.marks(&active, Tab::Rewrites, state.active_revision_id());
    assert_eq!(rewrites.get(&0), Some(&'↓'));
    assert_eq!(rewrites.get(&4), Some(&'↓'));
    assert_eq!(rewrites.get(&5), Some(&'>'), "the else row of the group");
    let defaults = trace.marks(&active, Tab::Defaults, state.active_revision_id());
    assert_eq!(defaults.get(&0), Some(&'>'));
}

#[test]
fn a_trace_is_on_every_tab_and_leaves_with_esc() {
    for tab in Tab::ALL {
        let mut state = loaded(World::AT_REST, tab);
        trace_invented(&mut state, fell_to_the_second());
        let text = frame(&state, 120, 40);
        assert!(
            text.contains("goes to  openrouter.openrouter_messages"),
            "{tab:?}\n{text}"
        );
        press(&mut state, KeyCode::Esc);
        assert!(state.trace().is_none(), "{tab:?}");
    }
}

#[test]
fn a_trace_over_a_route_page_closes_before_the_page_does() {
    let mut state = loaded(World::AT_REST, Tab::Routes);
    press(&mut state, KeyCode::Enter);
    trace_invented(&mut state, fell_to_the_second());
    insta::assert_snapshot!(frame(&state, 120, 40));
    press(&mut state, KeyCode::Esc);
    assert!(state.trace().is_none());
    assert_eq!(state.mode(), super::model::Mode::Route, "the page stays");
}

#[test]
fn enter_follows_a_trace_to_the_record_that_caught_it() {
    let mut state = loaded(World::AT_REST, Tab::Routes);
    trace_invented(&mut state, fell_to_the_second());
    press(&mut state, KeyCode::Enter);
    assert_eq!(state.tab(), Tab::Rewrites);
    assert_eq!(state.selected_in(Tab::Rewrites), 1);
    // Once there it opens the route, as it does with no trace.
    press(&mut state, KeyCode::Enter);
    assert_eq!(state.tab(), Tab::Routes);
}

#[test]
fn a_trace_that_is_refused_says_why_in_its_panel() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    let listing = press(&mut state, KeyCode::Char('t')).unwrap();
    answer(&mut state, listing, 200, recent());
    press(&mut state, KeyCode::Enter);
    let command = press(&mut state, KeyCode::Enter).unwrap();
    answer(
        &mut state,
        command,
        422,
        json!({"toker": "routing-preview", "ok": false, "error": "invalid_policy"}),
    );
    let text = frame(&state, 120, 40);
    assert!(text.contains("invalid_policy"), "{text}");
}

#[test]
fn only_the_answer_to_the_question_asked_now_is_kept() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    let first = {
        let listing = press(&mut state, KeyCode::Char('t')).unwrap();
        answer(&mut state, listing, 200, recent());
        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Enter).unwrap()
    };
    // Another is asked before the first is answered.
    let listing = press(&mut state, KeyCode::Char('t')).unwrap();
    answer(&mut state, listing, 200, recent());
    press(&mut state, KeyCode::Down);
    let second = press(&mut state, KeyCode::Enter).unwrap();
    answer(&mut state, first, 200, served_first());
    assert!(
        frame(&state, 120, 40).contains("tracing…"),
        "the first answer is not the second's"
    );
    answer(&mut state, second, 200, fell_to_the_second());
    assert!(frame(&state, 120, 40).contains("anthropic_sub/claude-sonnet-5"));
}

#[test]
fn typing_in_the_form_changes_what_is_traced() {
    // No record is selected on the models tab, so the form starts empty.
    let mut state = loaded(World::AT_REST, Tab::Models);
    let listing = press(&mut state, KeyCode::Char('t')).unwrap();
    answer(&mut state, listing, 200, recent());
    press(&mut state, KeyCode::Enter);
    // profile, then protocol, then the model.
    typed(&mut state, "claude");
    press(&mut state, KeyCode::Down);
    press(&mut state, KeyCode::Down);
    typed(&mut state, "claude-opus-5");
    let command = press(&mut state, KeyCode::Enter).unwrap();
    let request = &command.body.unwrap()["request"];
    assert_eq!(request["profile"], "claude");
    assert_eq!(request["model"], "claude-opus-5");
}

#[test]
fn a_busy_queue_is_said_and_leaves_the_page_usable() {
    let mut state = loaded(World::AT_REST, Tab::Rewrites);
    press(&mut state, KeyCode::Char('t'));
    state.command_refused("recent");
    assert!(state.error().unwrap().contains("recent"));
    press(&mut state, KeyCode::Esc);
    assert!(!state.capturing());
}

/// Every reason the service can give has words of its own. A reason without
/// them once fell through to "its facts were not recorded", naming an absence
/// that was not the one that had happened.
#[test]
fn every_not_replayable_reason_has_words_of_its_own() {
    use std::collections::BTreeSet;

    use crate::policy::facts::NotReplayable;

    let reasons = [
        NotReplayable::FactsNotRecorded,
        NotReplayable::CompactionEvidenceNotRecorded,
        NotReplayable::LaneNotRecorded,
    ];
    // Exhaustive, so a reason added without words fails to compile here.
    for reason in reasons {
        match reason {
            NotReplayable::FactsNotRecorded
            | NotReplayable::CompactionEvidenceNotRecorded
            | NotReplayable::LaneNotRecorded => {}
        }
    }
    let fallback = super::trace::not_replayable_words("no_such_reason");
    let mut seen = BTreeSet::new();
    for reason in reasons {
        let words = super::trace::not_replayable_words(reason.code());
        assert_ne!(words, fallback, "{} falls through", reason.code());
        assert!(seen.insert(words), "{} shares its words", reason.code());
    }
}
