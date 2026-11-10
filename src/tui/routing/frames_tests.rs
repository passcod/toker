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
