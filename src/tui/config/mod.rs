//! The configuration pages: providers, and everything else.
//!
//! Configuration lives in the store and is edited through the service, so
//! this page is a view of what the service says plus the requests that
//! change it (spec: UTUI#the-configuration-pages).

mod model;
mod view;

use crossterm::event::{KeyCode, KeyEvent};

use crate::tui::control::{Client, Command};
use model::State;

/// The keys the configuration pages act on themselves. The dashboard loop
/// forwards exactly these, so a key the page handles but this list lacks is
/// silently dead.
pub(super) fn owns_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Char('e' | ' ' | '1' | '2') | KeyCode::Enter | KeyCode::Backspace | KeyCode::Esc
    )
}

pub(super) struct Page {
    client: Client,
    state: State,
    drawn: std::cell::RefCell<view::Drawn>,
}

impl Page {
    pub(super) fn new(port: u16) -> Page {
        let mut page = Page {
            client: Client::new(port, "config", "config-snapshot"),
            state: State::default(),
            drawn: Default::default(),
        };
        page.refresh();
        page
    }

    pub(super) fn refresh(&mut self) {
        if !self.client.refresh() {
            self.state.command_refused("snapshot");
        }
    }

    pub(super) fn poll(&mut self) {
        while let Some(reply) = self.client.poll() {
            if let Some(next) = self.state.receive(reply) {
                self.send(next);
            }
        }
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        self.state.move_selection(delta);
    }

    /// Handle a key owned by this page. Returns whether a text entry was
    /// open, so the loop knows the key was swallowed by it.
    pub(super) fn key(&mut self, key: KeyEvent) -> bool {
        let editing = self.state.editing().is_some();
        if let Some(command) = self.state.key(key) {
            self.send(command);
        }
        editing
    }

    pub(super) fn editing(&self) -> bool {
        self.state.editing().is_some()
    }

    fn send(&mut self, command: Command) {
        let name = command.name;
        if !self.client.send(command) {
            self.state.command_refused(name);
        }
    }

    pub(super) fn render(&self, frame: &mut ratatui::Frame) {
        let drawn = view::render(frame, &self.state);
        self.drawn.replace(drawn);
    }

    pub(super) fn click(&mut self, at: ratatui::layout::Position) {
        let drawn = self.drawn.borrow().clone();
        if drawn.entries.contains(at) && at.y > drawn.entries.y {
            self.state.select(usize::from(at.y - drawn.entries.y - 1));
        }
    }

    pub(super) fn wheel(&mut self, delta: i32) {
        self.state.move_selection(delta as isize);
    }
}
