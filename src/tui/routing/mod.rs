//! Routing policy page with draft-led controls.

mod client;
mod model;
mod view;

use client::{Client, Command};
use crossterm::event::KeyEvent;
use model::State;

/// Page-owned state, including the bounded control worker.
pub(super) struct Page {
    client: Client,
    state: State,
}

impl Page {
    pub(super) fn new(port: u16) -> Page {
        let client = Client::new(port);
        let mut page = Page {
            client,
            state: State::default(),
        };
        page.refresh();
        // Revisions are independent data: load them eagerly, but never make an
        // unavailable new endpoint stop the established read-only snapshot.
        page.send(Command::revisions());
        page
    }

    pub(super) fn refresh(&mut self) {
        if self.state.begin_refresh() {
            let _ = self.client.refresh();
        }
    }

    pub(super) fn poll(&mut self) {
        while let Some(reply) = self.client.poll() {
            self.state.receive(reply);
        }
    }

    pub(super) fn move_view(&mut self, delta: isize) {
        self.state.move_view(delta);
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        self.state.move_selection(delta);
    }

    pub(super) fn set_view(&mut self, view: usize) {
        let view = match view {
            0 => model::View::Presentations,
            1 => model::View::Routes,
            2 => model::View::Rewrites,
            _ => model::View::Status,
        };
        self.state.set_view(view);
    }

    /// Handle a key owned by the routing page.  Only this page owns its modal
    /// state, so closing a dialog never alters dashboard dialogs or controls.
    pub(super) fn key(&mut self, key: KeyEvent) -> bool {
        let active = self.state.modal().active();
        if let Some(command) = self.state.key(key) {
            self.send(command);
        }
        active
    }

    pub(super) fn modal_active(&self) -> bool {
        self.state.modal().active()
    }

    fn send(&mut self, command: Command) {
        if !self.client.send(command) {
            // The state deliberately retains its last snapshot and remains
            // usable for read-only navigation when a daemon is unavailable.
            self.state.receive(client::Reply::Command(
                Command::snapshot(),
                Err("routing command queue is busy".to_owned()),
            ));
        }
    }

    pub(super) fn render(&self, frame: &mut ratatui::Frame, legend: bool) -> view::Drawn {
        view::render(frame, &self.state, legend)
    }
}
