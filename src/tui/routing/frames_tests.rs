//! The routing page's frames from the design that no other test owns: a
//! fresh install, the keys overlay, and the narrow terminals
//! (spec: UTUI#the-routing-page, UTUI#finding-the-keys,
//! UTUI#what-is-shed-first).

use ratatui::{Terminal, backend::TestBackend};

use super::model::{State, Tab};
use super::testing::{self, World};
use super::view::render;
use crate::tui::control::{Command, Reply, Response};

/// A page that has read the snapshot and revisions of `world`, on `tab`.
fn page(world: World, tab: Tab) -> State {
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

/// The page at `width`×`height` as text, with the keys overlay over it when
/// `overlay` is set.
fn frame(state: &State, width: u16, height: u16, overlay: bool) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| {
            render(frame, state, overlay);
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

/// A fresh install's rewrites: every protocol drawn with the else row that
/// refuses, and the control that would add a rule pointing at routes.
#[test]
fn a_fresh_install_draws_every_protocol_refused() {
    let state = page(World::EMPTY, Tab::Rewrites);
    insta::assert_snapshot!(frame(&state, 120, 40, false));
}

/// The overlay at 120×44: the keys grouped by where they apply beside the
/// marks, under a header whose buttons stay in view.
#[test]
fn the_keys_overlay_groups_the_keys_beside_the_marks_120x44() {
    let state = page(World::AT_REST, Tab::Rewrites);
    insta::assert_snapshot!(frame(&state, 120, 44, true));
}

/// A page that has drawn `state` with the overlay open: its first row.
fn header_row(state: &State, width: u16, overlay: bool) -> String {
    frame(state, width, 40, overlay)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// The overlay opens under the header, so the buttons that go to a page, and
/// the one that closes the overlay again, stay where they were
/// (spec: UTUI#finding-the-keys).
#[test]
fn the_overlay_leaves_the_header_showing() {
    let state = page(World::AT_REST, Tab::Rewrites);
    for width in [80, 100, 120, 200] {
        assert_eq!(
            header_row(&state, width, true),
            header_row(&state, width, false),
            "at {width}"
        );
    }
}

/// Every row of the overlay, keys and marks.
fn overlay_rows() -> Vec<(&'static str, &'static str)> {
    use super::keys;
    [
        &keys::ANYWHERE[..],
        &keys::IN_HISTORY,
        &keys::IN_A_TRACE,
        &keys::IN_AN_EDIT,
        &keys::ROWS,
        &keys::READINGS,
        &keys::PRESENTATIONS,
        &keys::REVISIONS,
    ]
    .concat()
}

/// Every key the page acts on is named in the overlay, so a key that was
/// added cannot stay undocumented (spec: UTUI#finding-the-keys).
#[test]
fn the_overlay_names_every_key_the_page_takes() {
    use crossterm::event::KeyCode;
    let rows = overlay_rows();
    let named = |key: &str| {
        rows.iter()
            .any(|(keys, _)| keys.split([' ', '/']).any(|token| token == key))
    };
    for key in 'a'..='z' {
        assert!(
            !super::owns_key(KeyCode::Char(key)) || named(&key.to_string()),
            "{key} is taken but not named"
        );
    }
    // The keys the dashboard's loop routes to the page, and a form's own.
    for key in [
        "pgup", "pgdn", "enter", "esc", "tab", "q", "r", "h", "j", "k", "l", "1–6", "space",
        "alt+↑", "ctrl+d", "ctrl+s",
    ] {
        assert!(named(key), "{key} is not named");
    }
}

/// Every word and glyph the overlay explains is one the page draws, and
/// every glyph the page draws is explained: the overlay is the operator's
/// dictionary, and an entry with nothing behind it is a claim about a screen
/// that does not exist (spec: UTUI#marks).
#[test]
fn the_overlay_explains_every_mark_the_page_draws_and_no_other() {
    use super::keys;
    use super::{history_tests, trace_tests};
    let mut frames: Vec<String> = Vec::new();
    for (world, tab) in [
        (World::PROFILED, Tab::Presentations),
        (World::PROFILED, Tab::Rewrites),
        (World::PROFILED, Tab::Defaults),
        (World::SAVED, Tab::Rewrites),
        (World::AT_REST, Tab::Routes),
        (World::AT_REST, Tab::Models),
        (World::EMPTY, Tab::Rewrites),
    ] {
        let mut state = page(world, tab);
        frames.push(frame(&state, 120, 40, false));
        // A short table cut, and its selection reached.
        state.move_selection(1);
        frames.push(frame(&state, 120, 12, false));
        // A narrow header, with its tabs scrolled.
        frames.push(frame(&state, 70, 24, false));
    }
    let mut route = page(World::AT_REST, Tab::Routes);
    route.open();
    frames.push(frame(&route, 120, 40, false));
    let mut traced = trace_tests::loaded(World::AT_REST, Tab::Rewrites);
    trace_tests::trace_invented(&mut traced, trace_tests::fell_to_the_second());
    frames.push(frame(&traced, 120, 40, false));
    frames.push(frame(&history_tests::after_a_save(), 120, 40, false));
    let drawn = frames.join("\n");

    // What the overlay says it explains, and where on the page to see it.
    let probes: [(&str, &[&str]); 34] = [
        (">", &["> caught"]),
        ("↓", &["↓ fell past"]),
        ("┌─ │ └─", &["┌─ ", "└─ else"]),
        ("else", &["else  default"]),
        ("tries X next", &["tries anthropic_messages next"]),
        ("${model}", &["sends     ${model}"]),
        ("as asked", &["as asked"]),
        ("ok", &["  ok  "]),
        ("no cred", &["no cred"]),
        ("off", &["  off  "]),
        ("billed fallback", &["billed fallback", "no cred · billed "]),
        ("refused", &["refused"]),
        ("refused · no route", &["refused · no route"]),
        ("- ≥N", &["  - ", "≥1"]),
        (
            "… N more",
            &["more record", "record above", "records above"],
        ),
        ("slug", &["slug openrouter"]),
        ("${model_id}", &["${model_id}"]),
        ("as", &["  as  "]),
        ("published", &["published"]),
        ("unprefixed", &["unprefixed/"]),
        ("+ ~ - ↕", &["+ rewrite"]),
        ("live", &["10 live"]),
        ("warm · cold", &["2 warm", "8 cold"]),
        ("[m] reversed", &["[m] move running lanes"]),
        ("N lanes on rev M", &["2 lanes on rev 9"]),
        ("claude pending", &["claude pending"]),
        ("no policy yet", &["no policy yet"]),
        ("→ ·", &["→ 2 rewrites", "· 5 models"]),
        ("‹ ›", &["‹"]),
        // The rest are read off the vocabulary rather than a frame.
        ("unbilled fallback", &[]),
        ("fallback, cost unknown", &[]),
        ("none available", &[]),
        ("billed · unbilled", &[]),
        ("cost unknown", &[]),
    ];
    let mut explained: Vec<&str> = [
        &keys::ROWS[..],
        &keys::READINGS,
        &keys::PRESENTATIONS,
        &keys::REVISIONS,
    ]
    .concat()
    .into_iter()
    .map(|(mark, _)| mark)
    .collect();
    for (mark, seen) in probes {
        let at = explained.iter().position(|have| *have == mark);
        assert!(at.is_some(), "{mark:?} is not explained in the overlay");
        explained.remove(at.unwrap_or_default());
        if !seen.is_empty() {
            assert!(
                seen.iter().any(|probe| drawn.contains(probe)),
                "{mark:?} is explained but none of {seen:?} is drawn"
            );
        }
    }
    assert!(
        explained.is_empty(),
        "marks in the overlay with no probe here: {explained:?}"
    );

    // Every glyph drawn that is not a letter, a border or a plain ASCII
    // character is one of the overlay's.
    let said: String = overlay_rows()
        .iter()
        .flat_map(|(key, meaning)| [*key, *meaning])
        .collect();
    for glyph in drawn.chars().filter(|glyph| {
        !glyph.is_ascii()
            && !glyph.is_alphanumeric()
            && !glyph.is_whitespace()
            && !('\u{2500}'..='\u{257f}').contains(glyph)
    }) {
        assert!(said.contains(glyph), "{glyph:?} is drawn but not explained");
    }
}

/// Each clause the vocabulary writes for a reach, in full or in its fewest
/// words, is a mark the overlay explains, spelled as the page spells it.
#[test]
fn every_clause_a_reach_is_written_in_is_explained_as_written() {
    use super::keys;
    use super::words::reach_text_in;
    use serde_json::json;
    let marks: Vec<&str> = keys::READINGS.iter().map(|(mark, _)| *mark).collect();
    let later = |billed: serde_json::Value| {
        json!({"outcome": "served_later", "serving": {"billed": billed},
            "skipped": [{"reason": "no_credential", "provider": "p"}]})
    };
    for reach in [
        later(json!(true)),
        later(json!(false)),
        later(json!(null)),
        json!({"outcome": "refused_unavailable", "skipped": []}),
    ] {
        let full = reach_text_in(&reach, false);
        for clause in full.split(" · ") {
            assert!(
                marks.contains(&clause),
                "{clause:?} of {full:?} is not explained as written"
            );
        }
        // The brief forms are said together, in one entry.
        let brief = reach_text_in(&reach, true);
        for clause in brief.split(" · ") {
            assert!(
                marks.contains(&clause)
                    || marks
                        .iter()
                        .any(|mark| mark.split(" · ").any(|word| word == clause)),
                "{clause:?} of {brief:?} is not explained as written"
            );
        }
    }
}

/// The reach words an overlay explains are the ones the vocabulary writes.
#[test]
fn the_reach_words_the_overlay_explains_are_the_ones_the_page_writes() {
    use super::words::reach_text_in;
    use serde_json::json;
    let later = |billed| {
        json!({"outcome": "served_later", "serving": {"billed": billed},
            "skipped": [{"reason": "no_credential", "provider": "p"}]})
    };
    let said = |reach| reach_text_in(&reach, false);
    assert_eq!(said(json!({"outcome": "served_first"})), "ok");
    assert_eq!(said(later(true)), "no cred · billed fallback");
    assert_eq!(said(later(false)), "no cred · unbilled fallback");
    assert!(
        said(json!({"outcome": "refused_edge", "edge": {"setting": "billed_boundary"}}))
            .starts_with("refused")
    );
    assert_eq!(
        said(json!({"outcome": "refused_unavailable",
            "skipped": [{"reason": "provider_off"}, {"reason": "no_credential"}]})),
        "off · no cred"
    );
    assert_eq!(
        said(json!({"outcome": "refused_no_route", "cause": "no_default"})),
        "refused · no route"
    );
}

/// The rewrites at 100×24: the columns shared by every row of a group are
/// stated in its head, the tabs give way to their numbers, and the detail
/// is cut to what is left (spec: UTUI#what-is-shed-first).
#[test]
fn rewrites_100x24() {
    let state = page(World::AT_REST, Tab::Rewrites);
    insta::assert_snapshot!(frame(&state, 100, 24, false));
}

/// The same at 80×24.
#[test]
fn rewrites_80x24() {
    let state = page(World::AT_REST, Tab::Rewrites);
    insta::assert_snapshot!(frame(&state, 80, 24, false));
}

/// A route page at 100×24, the candidates, what handles them and what
/// matches into them all in view.
#[test]
fn the_route_page_100x24() {
    let mut state = page(World::AT_REST, Tab::Routes);
    state.open();
    insta::assert_snapshot!(frame(&state, 100, 24, false));
}

/// Every page draws in any room a terminal can be, with the overlay open and
/// over a policy with nothing in it: a frame too small for a table is cut,
/// never a panic (spec: UTUI#what-is-shed-first).
#[test]
fn every_frame_draws_in_any_size() {
    for world in [World::EMPTY, World::PROFILED] {
        for tab in Tab::ALL {
            let mut state = page(world, tab);
            for width in [1, 3, 10, 40, 70, 80, 100, 120, 200] {
                for height in [1, 2, 3, 4, 6, 10, 16, 24, 40] {
                    frame(&state, width, height, false);
                    frame(&state, width, height, true);
                }
            }
            if tab == Tab::Routes {
                state.open();
                for (width, height) in [(80, 24), (100, 24), (60, 12), (30, 5)] {
                    frame(&state, width, height, false);
                }
            }
        }
    }
}
